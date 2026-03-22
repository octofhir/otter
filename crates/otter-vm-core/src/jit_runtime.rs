use std::sync::OnceLock;

use otter_vm_bytecode::Function;
use otter_vm_bytecode::function::BailoutAction;
use otter_vm_jit::runtime_helpers::RuntimeHelpers;
use otter_vm_jit::{BAILOUT_SENTINEL, BailoutReason};

use crate::jit_helpers::{self, JitContext};
use crate::jit_stubs::call_jit_entry;
use crate::value::Value;
use crate::interpreter::Interpreter;

static RUNTIME_HELPERS: OnceLock<RuntimeHelpers> = OnceLock::new();

pub(crate) fn runtime_helpers() -> &'static RuntimeHelpers {
    RUNTIME_HELPERS.get_or_init(jit_helpers::build_runtime_helpers)
}

/// State for on-stack replacement: the interpreter's full frame snapshot
/// to be loaded by JIT code at a loop header entry point.
pub(crate) struct OsrState {
    /// Bytecode PC of the loop header to enter.
    pub entry_pc: u32,
    /// All local variable values from the interpreter frame.
    pub locals: Vec<Value>,
    /// All register values from the interpreter frame.
    pub registers: Vec<Value>,
}

/// Result of attempting JIT execution at the otter-vm-core level.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct DeoptValueSlot {
    pub index: u16,
    pub value: Value,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct JitResumeState {
    pub bailout_pc: u32,
    pub bailout_reason: otter_vm_jit::BailoutReason,
    pub locals: Vec<DeoptValueSlot>,
    pub registers: Vec<DeoptValueSlot>,
}

pub(crate) enum JitCallResult {
    /// JIT code ran successfully.
    Ok(Value),
    /// JIT code bailed out with captured frame state — resume at deopt PC.
    BailoutResume(JitResumeState),
    /// JIT code bailed out — restart function from PC 0.
    BailoutRestart,
    /// No JIT code available for this function.
    NotCompiled,
    /// JIT code bailed out and the function should be recompiled.
    NeedsRecompilation,
}

fn decode_raw_deopt_value(bits: i64) -> Value {
    unsafe { Value::from_raw_bits_unchecked(bits as u64).unwrap_or_else(Value::undefined) }
}

fn decode_dense_slots(raw: &[i64]) -> Vec<DeoptValueSlot> {
    raw.iter()
        .enumerate()
        .map(|(index, &bits)| DeoptValueSlot {
            index: index as u16,
            value: decode_raw_deopt_value(bits),
        })
        .collect()
}

fn decode_sparse_slots(raw: &[i64], live_indices: &[u16]) -> Vec<DeoptValueSlot> {
    live_indices
        .iter()
        .filter_map(|&index| {
            raw.get(index as usize).map(|&bits| DeoptValueSlot {
                index,
                value: decode_raw_deopt_value(bits),
            })
        })
        .collect()
}

fn map_exec_result(
    exec_result: otter_vm_exec::JitExecResult,
    module_id: u64,
    function_index: u32,
    deopt_locals: &[i64],
    deopt_regs: &[i64],
) -> JitCallResult {
    match exec_result {
        otter_vm_exec::JitExecResult::Ok(bits) => {
            if let Some(value) = Value::from_jit_bits(bits as u64) {
                JitCallResult::Ok(value)
            } else {
                JitCallResult::BailoutRestart
            }
        }
        otter_vm_exec::JitExecResult::Bailout(snapshot) => {
            if snapshot.resume_mode == otter_vm_exec::DeoptResumeMode::ResumeAtPc
                && let Some(pc) = snapshot.bailout_pc
            {
                let (locals, registers) = if let Some(metadata) =
                    otter_vm_exec::deopt_metadata_snapshot(module_id, function_index)
                {
                    if let Some(site) = metadata.site(pc) {
                        (
                            decode_sparse_slots(deopt_locals, &site.live_locals),
                            decode_sparse_slots(deopt_regs, &site.live_registers),
                        )
                    } else {
                        (
                            decode_dense_slots(deopt_locals),
                            decode_dense_slots(deopt_regs),
                        )
                    }
                } else {
                    (
                        decode_dense_slots(deopt_locals),
                        decode_dense_slots(deopt_regs),
                    )
                };
                return JitCallResult::BailoutResume(JitResumeState {
                    bailout_pc: pc,
                    bailout_reason: snapshot.reason,
                    locals,
                    registers,
                });
            }
            JitCallResult::BailoutRestart
        }
        otter_vm_exec::JitExecResult::NeedsRecompilation(_) => JitCallResult::NeedsRecompilation,
        otter_vm_exec::JitExecResult::NotCompiled => JitCallResult::NotCompiled,
    }
}

pub(crate) fn try_execute_jit(
    module_id: u64,
    function_index: u32,
    function: &Function,
    args: &[Value],
    proto_epoch: u64,
    interpreter: *const crate::interpreter::Interpreter,
    vm_ctx: *mut crate::context::VmContext,
    constants: *const otter_vm_bytecode::ConstantPool,
    upvalues: &[crate::value::UpvalueCell],
    osr: Option<OsrState>,
) -> JitCallResult {
    let this_raw = if vm_ctx.is_null() {
        Value::undefined().to_jit_bits()
    } else {
        let vm = unsafe { &*vm_ctx };
        let pending = vm.pending_this_to_trace().cloned();
        let this_val = pending.unwrap_or_else(Value::undefined);
        if !function.flags.is_strict && (this_val.is_undefined() || this_val.is_null()) {
            Value::object(vm.global()).to_jit_bits()
        } else {
            this_val.to_jit_bits()
        }
    };

    const INLINE_DEOPT_SLOTS: usize = 32;
    let local_count = function.local_count as usize;
    let reg_count = function.register_count as usize;
    let mut inline_locals = [0_i64; INLINE_DEOPT_SLOTS];
    let mut inline_regs = [0_i64; INLINE_DEOPT_SLOTS];
    let mut heap_locals: Vec<i64>;
    let mut heap_regs: Vec<i64>;
    let deopt_locals: &mut [i64] = if local_count <= INLINE_DEOPT_SLOTS {
        &mut inline_locals[..local_count]
    } else {
        heap_locals = vec![0_i64; local_count];
        &mut heap_locals
    };
    let deopt_regs: &mut [i64] = if reg_count <= INLINE_DEOPT_SLOTS {
        &mut inline_regs[..reg_count]
    } else {
        heap_regs = vec![0_i64; reg_count];
        &mut heap_regs
    };

    let osr_entry_pc: i64 = if let Some(ref state) = osr {
        for (i, val) in state.locals.iter().enumerate() {
            if i < local_count {
                deopt_locals[i] = val.to_jit_bits();
            }
        }
        for (i, val) in state.registers.iter().enumerate() {
            if i < reg_count {
                deopt_regs[i] = val.to_jit_bits();
            }
        }
        state.entry_pc as i64
    } else {
        -1
    };

    let fv_len = function.feedback_vector.read().len();
    if fv_len > 0 && function.jit_ic_probes.is_empty() {
        function.jit_ic_probes.init(fv_len);
        let fv = function.feedback_vector.read();
        for (i, ic) in fv.iter().enumerate() {
            if let otter_vm_bytecode::function::InlineCacheState::Monomorphic {
                shape_id,
                depth: 0,
                offset,
                ..
            } = &ic.ic_state
            {
                if let Some(probe) = function.jit_ic_probes.get_mut(i) {
                    probe.set_mono_inline(*shape_id, *offset);
                }
            }
        }
    }

    let jit_ctx = JitContext {
        function_ptr: function as *const Function,
        proto_epoch,
        interpreter,
        vm_ctx,
        constants,
        upvalues_ptr: if upvalues.is_empty() {
            std::ptr::null()
        } else {
            upvalues.as_ptr()
        },
        upvalue_count: upvalues.len() as u32,
        this_raw,
        callee_raw: if vm_ctx.is_null() {
            Value::undefined().to_jit_bits()
        } else {
            let vm = unsafe { &*vm_ctx };
            vm.pending_callee_to_trace()
                .cloned()
                .unwrap_or_else(Value::undefined)
                .to_jit_bits()
        },
        home_object_raw: if vm_ctx.is_null() {
            Value::null().to_jit_bits()
        } else {
            let vm = unsafe { &*vm_ctx };
            vm.pending_home_object_to_trace()
                .map(|obj| Value::object(*obj).to_jit_bits())
                .unwrap_or_else(|| Value::null().to_jit_bits())
        },
        secondary_result: 0,
        bailout_reason: BailoutReason::Unknown.code(),
        bailout_pc: -1,
        deopt_locals_ptr: if local_count > 0 {
            deopt_locals.as_mut_ptr()
        } else {
            std::ptr::null_mut()
        },
        deopt_locals_count: local_count as u32,
        deopt_regs_ptr: if reg_count > 0 {
            deopt_regs.as_mut_ptr()
        } else {
            std::ptr::null_mut()
        },
        deopt_regs_count: reg_count as u32,
        osr_entry_pc,
        tier_up_budget: otter_vm_jit::runtime_helpers::JIT_TIER_UP_BUDGET_DEFAULT,
        ic_probes_ptr: if function.jit_ic_probes.is_empty() {
            std::ptr::null()
        } else {
            function.jit_ic_probes.as_ptr()
        },
        ic_probes_count: function.jit_ic_probes.len() as u32,
        interrupt_flag_ptr: if vm_ctx.is_null() {
            std::ptr::null()
        } else {
            unsafe { (*vm_ctx).interrupt_flag_raw_ptr() }
        },
    };

    let ctx_ptr = &jit_ctx as *const JitContext as *mut u8;
    let argc = args.len() as u32;

    let exec_result = if args.len() <= 8 {
        let mut inline = [0_i64; 8];
        for (idx, arg) in args.iter().enumerate() {
            inline[idx] = arg.to_jit_bits();
        }
        otter_vm_exec::try_execute_jit_raw(
            module_id,
            function_index,
            function,
            argc,
            inline.as_ptr(),
            ctx_ptr,
        )
    } else {
        let mut arg_bits = Vec::with_capacity(args.len());
        for arg in args {
            arg_bits.push(arg.to_jit_bits());
        }
        otter_vm_exec::try_execute_jit_raw(
            module_id,
            function_index,
            function,
            argc,
            arg_bits.as_ptr(),
            ctx_ptr,
        )
    };

    let result = map_exec_result(
        exec_result,
        module_id,
        function_index,
        deopt_locals,
        deopt_regs,
    );
    if matches!(result, JitCallResult::NotCompiled)
        && function.is_hot_function()
        && !function.is_deoptimized()
        && otter_vm_exec::pending_count() > 0
    {
        otter_vm_exec::compile_one_pending_request(runtime_helpers());
    }
    result
}

pub(crate) fn try_execute_jit_from_raw_args(
    module_id: u64,
    function_index: u32,
    function: &Function,
    argc: u32,
    args_ptr: *const i64,
    this_raw: i64,
    callee_raw: i64,
    home_object_raw: i64,
    proto_epoch: u64,
    interpreter: *const crate::interpreter::Interpreter,
    vm_ctx: *mut crate::context::VmContext,
    constants: *const otter_vm_bytecode::ConstantPool,
    upvalues: &[crate::value::UpvalueCell],
) -> JitCallResult {
    let jit_ctx = JitContext {
        function_ptr: function as *const Function,
        proto_epoch,
        interpreter,
        vm_ctx,
        constants,
        upvalues_ptr: if upvalues.is_empty() {
            std::ptr::null()
        } else {
            upvalues.as_ptr()
        },
        upvalue_count: upvalues.len() as u32,
        this_raw,
        callee_raw,
        home_object_raw,
        secondary_result: 0,
        bailout_reason: BailoutReason::Unknown.code(),
        bailout_pc: -1,
        deopt_locals_ptr: std::ptr::null_mut(),
        deopt_locals_count: 0,
        deopt_regs_ptr: std::ptr::null_mut(),
        deopt_regs_count: 0,
        osr_entry_pc: -1,
        tier_up_budget: otter_vm_jit::runtime_helpers::JIT_TIER_UP_BUDGET_DEFAULT,
        ic_probes_ptr: if function.jit_ic_probes.is_empty() {
            std::ptr::null()
        } else {
            function.jit_ic_probes.as_ptr()
        },
        ic_probes_count: function.jit_ic_probes.len() as u32,
        interrupt_flag_ptr: if vm_ctx.is_null() {
            std::ptr::null()
        } else {
            unsafe { (*vm_ctx).interrupt_flag_raw_ptr() }
        },
    };

    let ctx_ptr = &jit_ctx as *const JitContext as *mut u8;
    let ptr = function.jit_entry_ptr();
    if ptr != 0 {
        let outcome = unsafe { call_jit_entry(ctx_ptr.cast::<JitContext>(), args_ptr, argc, ptr) };
        let result = outcome.result;
        if result != BAILOUT_SENTINEL {
            return Value::from_jit_bits(result as u64)
                .map(JitCallResult::Ok)
                .unwrap_or(JitCallResult::BailoutRestart);
        }

        let action = function.record_bailout(otter_vm_exec::jit_deopt_threshold());
        if matches!(
            action,
            BailoutAction::Recompile | BailoutAction::PermanentDeopt
        ) {
            otter_vm_exec::invalidate_jit_code(module_id, function_index, function);
        }
        return match action {
            BailoutAction::Recompile => JitCallResult::NeedsRecompilation,
            BailoutAction::Continue | BailoutAction::PermanentDeopt => {
                JitCallResult::BailoutRestart
            }
        };
    }

    let exec_result = otter_vm_exec::try_execute_jit_raw(
        module_id,
        function_index,
        function,
        argc,
        args_ptr,
        ctx_ptr,
    );

    map_exec_result(exec_result, module_id, function_index, &[], &[])
}

#[unsafe(no_mangle)]
pub extern "C" fn baseline_get_local(ctx: *mut crate::context::VmContext, idx: u32) -> i64 {
    let ctx_ref = unsafe { &*ctx };
    let val = ctx_ref.read_local_unchecked(idx as u16);
    unsafe { std::mem::transmute(val) }
}

#[unsafe(no_mangle)]
pub extern "C" fn baseline_set_local(ctx: *mut crate::context::VmContext, idx: u32, val_raw: i64) {
    let ctx_mut = unsafe { &mut *ctx };
    let val: crate::value::Value = unsafe { std::mem::transmute(val_raw) };
    ctx_mut.write_local_unchecked(idx as u16, val);
}

#[unsafe(no_mangle)]
pub extern "C" fn baseline_is_truthy(val_raw: i64) -> u32 {
    let val: crate::value::Value = unsafe { std::mem::transmute(val_raw) };
    if val.to_boolean() { 1 } else { 0 }
}

#[unsafe(no_mangle)]
pub extern "C" fn baseline_get_prop_const(
    ctx: *mut crate::context::VmContext,
    function_ptr: *const otter_vm_bytecode::Function,
    obj_raw: i64,
    const_idx: u32,
    ic_index: u32,
) -> i64 {
    let ctx_ref = unsafe { &*ctx };
    let function = unsafe { &*function_ptr };
    let obj_val: crate::value::Value = unsafe { std::mem::transmute(obj_raw) };

    if let Some(obj_ref) = obj_val.as_object() {
        if !obj_ref.is_dictionary_mode() {
            let obj_shape_id = obj_ref.shape_id();

            if let Some(probe) = function.jit_ic_probes.get_mut(ic_index as usize) {
                if probe.shape_id == obj_shape_id {
                    if let Some(val) = obj_ref.get_by_offset(probe.offset as usize) {
                        return unsafe { std::mem::transmute(val) };
                    }
                }
            }

            let mut feedback = function.feedback_vector.write();
            if let Some(ic) = feedback.get_mut(ic_index as usize) {
                use otter_vm_bytecode::function::InlineCacheState;

                if ic.proto_epoch_matches(ctx_ref.cached_proto_epoch) {
                    match &ic.ic_state {
                        InlineCacheState::Monomorphic { shape_id, depth: 0, offset, .. } => {
                            if *shape_id == obj_shape_id {
                                if let Some(probe) = function.jit_ic_probes.get_mut(ic_index as usize) {
                                    probe.set_mono_inline(*shape_id, *offset);
                                }
                                if let Some(val) = obj_ref.get_by_offset(*offset as usize) {
                                    ic.record_ic_hit();
                                    return unsafe { std::mem::transmute(val) };
                                }
                            }
                        }
                        InlineCacheState::Polymorphic { count, entries } => {
                            for i in 0..(*count as usize) {
                                let (shape_id, _proto_id, depth, offset) = entries[i];
                                if shape_id == obj_shape_id && depth == 0 {
                                    if let Some(val) = obj_ref.get_by_offset(offset as usize) {
                                        ic.record_ic_poly_hit();
                                        return unsafe { std::mem::transmute(val) };
                                    }
                                }
                            }
                        }
                        _ => {}
                    }
                }
                ic.record_ic_miss();
            }
        }

        let module = if let Some(frame) = ctx_ref.current_frame() {
            ctx_ref.get_module(frame.module_id)
        } else {
            ctx_ref.get_module(0)
        };

        if let Some(name_const) = module.constants.get(const_idx) {
            if let Some(name_str) = name_const.as_string() {
                let key = crate::object::PropertyKey::String(crate::string::JsString::intern_utf16(name_str));
                let result = obj_ref.get(&key).unwrap_or(crate::value::Value::undefined());

                if !obj_ref.is_dictionary_mode() {
                    let mut current_obj = Some(obj_ref.clone());
                    let mut depth = 0;
                    let mut found_offset = None;
                    let mut found_shape = 0;

                    while let Some(cur) = current_obj.take() {
                        if cur.is_dictionary_mode() { break; }
                        if let Some(offset) = cur.shape_get_offset(&key) {
                            found_offset = Some(offset);
                            found_shape = cur.shape_id();
                            break;
                        }
                        if let Some(proto) = cur.prototype().as_object() {
                            current_obj = Some(proto);
                            depth += 1;
                        } else { break; }
                    }

                    if let Some(offset) = found_offset {
                        let feedback = function.feedback_vector.write();
                        if let Some(ic) = feedback.get_mut(ic_index as usize) {
                            use otter_vm_bytecode::function::InlineCacheState;
                            let shape_ptr = obj_ref.shape_id();
                            let proto_shape_id = if depth > 0 { found_shape } else { 0 };
                            let current_epoch = ctx_ref.cached_proto_epoch;

                            match &mut ic.ic_state {
                                InlineCacheState::Uninitialized => {
                                    ic.ic_state = InlineCacheState::Monomorphic {
                                        shape_id: shape_ptr,
                                        proto_shape_id,
                                        depth,
                                        offset: offset as u32,
                                    };
                                    ic.proto_epoch = current_epoch;
                                }
                                InlineCacheState::Monomorphic { shape_id: old_shape, proto_shape_id: old_proto, depth: old_depth, offset: old_offset } => {
                                    if *old_shape != shape_ptr {
                                        let mut entries = [(0u64, 0u64, 0u8, 0u32); 4];
                                        entries[0] = (*old_shape, *old_proto, *old_depth, *old_offset);
                                        entries[1] = (shape_ptr, proto_shape_id, depth, offset as u32);
                                        ic.ic_state = InlineCacheState::Polymorphic { count: 2, entries };
                                        ic.proto_epoch = current_epoch;
                                    }
                                }
                                InlineCacheState::Polymorphic { count, entries } => {
                                    let mut found = false;
                                    for i in 0..(*count as usize) {
                                        if entries[i].0 == shape_ptr { found = true; break; }
                                    }
                                    if !found {
                                        if (*count as usize) < 4 {
                                            entries[*count as usize] = (shape_ptr, proto_shape_id, depth, offset as u32);
                                            *count += 1;
                                            ic.proto_epoch = current_epoch;
                                        } else {
                                            ic.ic_state = InlineCacheState::Megamorphic;
                                        }
                                    }
                                }
                                _ => {}
                            }
                        }
                    }
                }

                return unsafe { std::mem::transmute(result) };
            }
        }
    }

    unsafe { std::mem::transmute(crate::value::Value::undefined()) }
}

#[unsafe(no_mangle)]
pub extern "C" fn baseline_set_prop_const(
    ctx: *mut crate::context::VmContext,
    function_ptr: *const otter_vm_bytecode::Function,
    obj_raw: i64,
    const_idx: u32,
    val_raw: i64,
    ic_index: u32,
) {
    let ctx_mut = unsafe { &mut *ctx };
    let function = unsafe { &*function_ptr };
    let obj_val: crate::value::Value = unsafe { std::mem::transmute(obj_raw) };
    let val: crate::value::Value = unsafe { std::mem::transmute(val_raw) };

    if let Some(obj_ref) = obj_val.as_object() {
        if !obj_ref.is_dictionary_mode() {
            let obj_shape_id = obj_ref.shape_id();

            if let Some(probe) = function.jit_ic_probes.get_mut(ic_index as usize) {
                if probe.shape_id == obj_shape_id {
                    let _ = obj_ref.set_by_offset(probe.offset as usize, val);
                    return;
                }
            }

            let mut feedback = function.feedback_vector.write();
            if let Some(ic) = feedback.get_mut(ic_index as usize) {
                use otter_vm_bytecode::function::InlineCacheState;

                if let InlineCacheState::Monomorphic { shape_id, depth: 0, offset, .. } = &ic.ic_state {
                    if *shape_id == obj_shape_id {
                        if let Some(probe) = function.jit_ic_probes.get_mut(ic_index as usize) {
                            probe.set_mono_inline(*shape_id, *offset);
                        }
                        let _ = obj_ref.set_by_offset(*offset as usize, val);
                        ic.record_ic_hit();
                        return;
                    }
                }
                ic.record_ic_miss();
            }
        }

        let module = if let Some(frame) = ctx_mut.current_frame() {
            ctx_mut.get_module(frame.module_id)
        } else {
            ctx_mut.get_module(0)
        };

        if let Some(name_const) = module.constants.get(const_idx) {
            if let Some(name_str) = name_const.as_string() {
                let key = crate::object::PropertyKey::String(crate::string::JsString::intern_utf16(name_str));
                let _ = obj_ref.set(key, val);

                if !obj_ref.is_dictionary_mode() {
                    let feedback = function.feedback_vector.write();
                    if let Some(ic) = feedback.get_mut(ic_index as usize) {
                        use otter_vm_bytecode::function::InlineCacheState;
                        let shape_id = obj_ref.shape_id();
                        if let Some(offset) = obj_ref.shape_get_offset(&key) {
                            match &mut ic.ic_state {
                                InlineCacheState::Uninitialized => {
                                    ic.ic_state = InlineCacheState::Monomorphic {
                                        shape_id,
                                        proto_shape_id: 0,
                                        depth: 0,
                                        offset: offset as u32,
                                    };
                                }
                                InlineCacheState::Monomorphic { shape_id: old_shape, .. } if *old_shape != shape_id => {
                                    ic.ic_state = InlineCacheState::Megamorphic;
                                }
                                _ => {}
                            }
                        } else {
                            ic.ic_state = InlineCacheState::Megamorphic;
                        }
                    }
                }
            }
        }
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn baseline_call(
    ctx: *mut crate::context::VmContext,
    function_ptr: *const otter_vm_bytecode::Function,
    callee_raw: i64,
    _argc: u32,
    ic_index: u32,
) -> i64 {
    let _ctx_ref = unsafe { &mut *ctx };
    let function = unsafe { &*function_ptr };
    let callee_val: crate::value::Value = unsafe { std::mem::transmute(callee_raw) };

    // 1. Check JIT IC Probe (Monomorphic fast path)
    if let Some(probe) = function.jit_ic_probes.get_mut(ic_index as usize) {
        if probe.state == otter_vm_bytecode::function::JitIcProbe::STATE_CALL_MONO {
            if callee_val.function_id() == probe.func_id {
                // Monomorphic hit!
            }
        }
    }

    // 2. Resolve target and update IC
    if let Some(closure) = callee_val.as_function() {
        let mut feedback = function.feedback_vector.write();
        if let Some(ic) = feedback.get_mut(ic_index as usize) {
            use otter_vm_bytecode::function::InlineCacheState;
            let func_id = callee_val.function_id();
            let jit_entry = 0; // TODO: Get jit entry if compiled

            match &mut ic.ic_state {
                InlineCacheState::Uninitialized => {
                    ic.ic_state = InlineCacheState::MonoCall { func_id, jit_entry };
                    if let Some(probe) = function.jit_ic_probes.get_mut(ic_index as usize) {
                        probe.set_call_mono(func_id, jit_entry);
                    }
                }
                InlineCacheState::MonoCall { func_id: old_id, .. } if *old_id != func_id => {
                    let mut entries = [(0u64, 0u64); 4];
                    entries[0] = (*old_id, 0);
                    entries[1] = (func_id, jit_entry);
                    ic.ic_state = InlineCacheState::PolyCall { count: 2, entries };
                    if let Some(probe) = function.jit_ic_probes.get_mut(ic_index as usize) {
                        probe.set_other();
                    }
                }
                _ => {}
            }
        }
    }

    // Return callee value for the JIT to know what to call
    unsafe { std::mem::transmute(callee_val) }
}

#[unsafe(no_mangle)]
pub extern "C" fn baseline_call_method(
    ctx: *mut crate::context::VmContext,
    function_ptr: *const otter_vm_bytecode::Function,
    obj_raw: i64,
    const_idx: u32,
    _argc: u32,
    ic_index: u32,
) -> i64 {
    let ctx_ref = unsafe { &mut *ctx };
    let function = unsafe { &*function_ptr };
    let obj_val: crate::value::Value = unsafe { std::mem::transmute(obj_raw) };

    // 1. Resolve method using IC (similar to GetPropConst)
    if let Some(obj_ref) = obj_val.as_object() {
        if !obj_ref.is_dictionary_mode() {
            let obj_shape_id = obj_ref.shape_id();

            // Try monomorphic probe
            if let Some(probe) = function.jit_ic_probes.get_mut(ic_index as usize) {
                if probe.state == otter_vm_bytecode::function::JitIcProbe::STATE_MONO_INLINE && probe.shape_id == obj_shape_id {
                    if let Some(val) = obj_ref.get_by_offset(probe.offset as usize) {
                        return unsafe { std::mem::transmute(val) };
                    }
                }
            }
        }
    }

    // 2. Slow path: full lookup and IC update
    let module = if let Some(frame) = ctx_ref.current_frame() {
        ctx_ref.get_module(frame.module_id)
    } else {
        ctx_ref.get_module(0)
    };

    if let Some(name_const) = module.constants.get(const_idx) {
        if let Some(name_str) = name_const.as_string() {
            let key = crate::object::PropertyKey::String(crate::string::JsString::intern_utf16(name_str));
            let result = if let Some(obj_ref) = obj_val.as_object() {
                 obj_ref.get(&key).unwrap_or(Value::undefined())
            } else {
                Value::undefined()
            };

            // Update IC
            if let Some(obj_ref) = obj_val.as_object() {
                if !obj_ref.is_dictionary_mode() {
                    if let Some(offset) = obj_ref.shape_get_offset(&key) {
                        let feedback = function.feedback_vector.write();
                        if let Some(ic) = feedback.get_mut(ic_index as usize) {
                            use otter_vm_bytecode::function::InlineCacheState;
                            let shape_id = obj_ref.shape_id();
                            match &mut ic.ic_state {
                                InlineCacheState::Uninitialized => {
                                    ic.ic_state = InlineCacheState::Monomorphic {
                                        shape_id,
                                        proto_shape_id: 0,
                                        depth: 0,
                                        offset: offset as u32,
                                    };
                                    if let Some(probe) = function.jit_ic_probes.get_mut(ic_index as usize) {
                                        probe.set_mono_inline(shape_id, offset as u32);
                                    }
                                }
                                _ => {}
                            }
                        }
                    }
                }
            }

            return unsafe { std::mem::transmute(result) };
        }
    }

    unsafe { std::mem::transmute(crate::value::Value::undefined()) }
}

#[unsafe(no_mangle)]
pub extern "C" fn baseline_arith_add(
    ctx_raw: *mut u8,
    lhs_raw: i64,
    rhs_raw: i64,
    ic_index: u32,
) -> i64 {
    let ctx = unsafe { &*(ctx_raw as *const JitContext) };
    let function = unsafe { &*ctx.function_ptr };
    let lhs = Value::from_jit_bits(lhs_raw as u64).unwrap_or(Value::undefined());
    let rhs = Value::from_jit_bits(rhs_raw as u64).unwrap_or(Value::undefined());

    let vm_ctx = unsafe { &mut *ctx.vm_ctx };
    Interpreter::update_arithmetic_ic_on_function(
        vm_ctx,
        function,
        ic_index as u16,
        &lhs,
        &rhs,
        None,
    );

    let interp = unsafe { &*ctx.interpreter };
    interp
        .op_add(vm_ctx, &lhs, &rhs)
        .map(|v| v.to_jit_bits() as i64)
        .unwrap_or(BAILOUT_SENTINEL)
}

#[unsafe(no_mangle)]
pub extern "C" fn baseline_arith_sub(
    ctx_raw: *mut u8,
    lhs_raw: i64,
    rhs_raw: i64,
    ic_index: u32,
) -> i64 {
    let ctx = unsafe { &*(ctx_raw as *const JitContext) };
    let function = unsafe { &*ctx.function_ptr };
    let lhs = Value::from_jit_bits(lhs_raw as u64).unwrap_or(Value::undefined());
    let rhs = Value::from_jit_bits(rhs_raw as u64).unwrap_or(Value::undefined());

    let vm_ctx = unsafe { &mut *ctx.vm_ctx };
    Interpreter::update_arithmetic_ic_on_function(
        vm_ctx,
        function,
        ic_index as u16,
        &lhs,
        &rhs,
        None,
    );

    let interp = unsafe { &*ctx.interpreter };
    interp
        .op_sub(vm_ctx, &lhs, &rhs)
        .map(|v| v.to_jit_bits() as i64)
        .unwrap_or(BAILOUT_SENTINEL)
}

#[unsafe(no_mangle)]
pub extern "C" fn baseline_arith_mul(
    ctx_raw: *mut u8,
    lhs_raw: i64,
    rhs_raw: i64,
    ic_index: u32,
) -> i64 {
    let ctx = unsafe { &*(ctx_raw as *const JitContext) };
    let function = unsafe { &*ctx.function_ptr };
    let lhs = Value::from_jit_bits(lhs_raw as u64).unwrap_or(Value::undefined());
    let rhs = Value::from_jit_bits(rhs_raw as u64).unwrap_or(Value::undefined());

    let vm_ctx = unsafe { &mut *ctx.vm_ctx };
    Interpreter::update_arithmetic_ic_on_function(
        vm_ctx,
        function,
        ic_index as u16,
        &lhs,
        &rhs,
        None,
    );

    let interp = unsafe { &*ctx.interpreter };
    interp
        .op_mul(vm_ctx, &lhs, &rhs)
        .map(|v| v.to_jit_bits() as i64)
        .unwrap_or(BAILOUT_SENTINEL)
}

#[unsafe(no_mangle)]
pub extern "C" fn baseline_arith_div(
    ctx_raw: *mut u8,
    lhs_raw: i64,
    rhs_raw: i64,
    ic_index: u32,
) -> i64 {
    let ctx = unsafe { &*(ctx_raw as *const JitContext) };
    let function = unsafe { &*ctx.function_ptr };
    let lhs = Value::from_jit_bits(lhs_raw as u64).unwrap_or(Value::undefined());
    let rhs = Value::from_jit_bits(rhs_raw as u64).unwrap_or(Value::undefined());

    let vm_ctx = unsafe { &mut *ctx.vm_ctx };
    Interpreter::update_arithmetic_ic_on_function(
        vm_ctx,
        function,
        ic_index as u16,
        &lhs,
        &rhs,
        None,
    );

    let interp = unsafe { &*ctx.interpreter };
    interp
        .op_div(vm_ctx, &lhs, &rhs)
        .map(|v| v.to_jit_bits() as i64)
        .unwrap_or(BAILOUT_SENTINEL)
}
