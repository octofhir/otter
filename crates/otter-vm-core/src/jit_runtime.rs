use std::sync::OnceLock;

use otter_vm_bytecode::function::BailoutAction;
use otter_vm_bytecode::Function;
use otter_vm_jit::{BAILOUT_SENTINEL, BailoutReason};
use otter_vm_jit::runtime_helpers::RuntimeHelpers;

use crate::jit_helpers::{self, JitContext};
use crate::value::Value;

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
///
/// Unlike `otter_vm_exec::JitExecResult`, this carries VM-level `Value` types
/// and deopt frame state needed for precise interpreter resume.
pub(crate) enum JitCallResult {
    /// JIT code ran successfully.
    Ok(Value),
    /// JIT code bailed out with captured frame state — resume at deopt PC.
    BailoutResume {
        bailout_pc: u32,
        locals: Vec<Value>,
        registers: Vec<Value>,
    },
    /// JIT code bailed out — restart function from PC 0.
    BailoutRestart,
    /// No JIT code available for this function.
    NotCompiled,
    /// JIT code bailed out and the function should be recompiled.
    NeedsRecompilation,
}

fn map_exec_result(
    exec_result: otter_vm_exec::JitExecResult,
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
            if snapshot.resume_mode == otter_vm_exec::DeoptResumeMode::ResumeAtPc {
                if let Some(pc) = snapshot.bailout_pc {
                    // SAFETY: We are in the JIT execution scope — no GC has occurred
                    // between the JIT writing these bits and us reading them. The raw
                    // pointers in pointer-tagged values still point to live GC objects.
                    let locals: Vec<Value> = deopt_locals
                        .iter()
                        .map(|&bits| unsafe {
                            Value::from_raw_bits_unchecked(bits as u64)
                                .unwrap_or_else(Value::undefined)
                        })
                        .collect();
                    let registers: Vec<Value> = deopt_regs
                        .iter()
                        .map(|&bits| unsafe {
                            Value::from_raw_bits_unchecked(bits as u64)
                                .unwrap_or_else(Value::undefined)
                        })
                        .collect();
                    return JitCallResult::BailoutResume {
                        bailout_pc: pc,
                        locals,
                        registers,
                    };
                }
            }
            JitCallResult::BailoutRestart
        }
        otter_vm_exec::JitExecResult::NeedsRecompilation(_) => JitCallResult::NeedsRecompilation,
        otter_vm_exec::JitExecResult::NotCompiled => JitCallResult::NotCompiled,
    }
}

/// Try to execute JIT-compiled code for a function.
///
/// Builds the per-call `JitContext` (VM pointers + snapshots) and delegates
/// machine-code dispatch/deopt accounting to `otter-vm-exec`.
///
/// On bailout with a mapped deopt site, captures locals and registers from the
/// JIT-side deopt buffer for precise interpreter resume.
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

    // Allocate deopt state buffers for precise resume / OSR input.
    let local_count = function.local_count as usize;
    let reg_count = function.register_count as usize;
    let mut deopt_locals = vec![0_i64; local_count];
    let mut deopt_regs = vec![0_i64; reg_count];

    // For OSR entry, pre-fill deopt buffers with the interpreter's frame state.
    // The JIT prologue will load these instead of reading from argv.
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
                .map(|obj| Value::object(obj.clone()).to_jit_bits())
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

    map_exec_result(exec_result, &deopt_locals, &deopt_regs)
}

/// Try to execute JIT-compiled code with raw NaN-boxed argument bits.
///
/// Used by JIT runtime helpers to avoid rebuilding `Value` slices when the call
/// can stay fully on the JIT path.
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
        // Helper-only nested call path: no precise deopt resume required.
        // Keep buffers null/empty to avoid per-call allocations.
        deopt_locals_ptr: std::ptr::null_mut(),
        deopt_locals_count: 0,
        deopt_regs_ptr: std::ptr::null_mut(),
        deopt_regs_count: 0,
        osr_entry_pc: -1,
    };

    let ctx_ptr = &jit_ctx as *const JitContext as *mut u8;
    // Hot nested-call fast path: execute directly via cached entry pointer.
    // This avoids per-call JIT runtime mutex/drain overhead on call-heavy code.
    let ptr = function.jit_entry_ptr();
    if ptr != 0 {
        // SAFETY: `ptr` is a JIT entry pointer produced by the compiler with
        // signature `extern "C" fn(*mut u8, *const i64, u32) -> i64`.
        let code: extern "C" fn(*mut u8, *const i64, u32) -> i64 =
            unsafe { std::mem::transmute(ptr) };
        let result = code(ctx_ptr, args_ptr, argc);
        if result != BAILOUT_SENTINEL {
            return Value::from_jit_bits(result as u64)
                .map(JitCallResult::Ok)
                .unwrap_or(JitCallResult::BailoutRestart);
        }

        let action = function.record_bailout(otter_vm_exec::jit_deopt_threshold());
        if matches!(action, BailoutAction::Recompile | BailoutAction::PermanentDeopt) {
            otter_vm_exec::invalidate_jit_code(module_id, function_index);
        }
        return match action {
            BailoutAction::Recompile => JitCallResult::NeedsRecompilation,
            BailoutAction::Continue | BailoutAction::PermanentDeopt => JitCallResult::BailoutRestart,
        };
    }

    let exec_result =
        otter_vm_exec::try_execute_jit_raw(module_id, function_index, function, argc, args_ptr, ctx_ptr);

    map_exec_result(exec_result, &[], &[])
}
