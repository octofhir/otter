//! Template JavaScript calls on x86-64.
//!
//! # Contents
//! - [`emit_call`] — one call or construct: the actual span, the call ABI
//!   registers and the call into a proven target's current generation or the
//!   generic entry, then completion routing.
//! - [`emit_method_call`] — bounded native method proofs and current-generation
//!   calls, with committed resolution after the whole guard chain misses.
//! - [`emit_spread_call_op`] — calls whose span a runtime staging entry writes.
//! - [`emit_tail_call`] — a proper tail call.
//!
//! # Invariants
//! - `r15` holds the context, `r14` the published frame and `r13` its
//!   register window; the stack stays 16-byte aligned at every call.
//! - The callee owns its activation, receiver binding, constructor
//!   completion and deoptimization. A completion commits once: success
//!   stores the destination, a throw reaches `throw_value` with the exception
//!   in `rax`, a parked error reaches `threw`.
//! - Proven method hits never run the resolver or generic callable classifier.
//!   A guard miss commits resolution and call effects once.
//! - Staging and resolution entries use the platform C ABI; generated
//!   JavaScript targets keep the private call registers and actual span.
//! - Proper tail teardown restores the supplied physical frame kind, including
//!   the optimizing tier's additional nonvolatile register save.
//!
//! # See also
//! - [`crate::x86_64::js_call`] — the shared call ABI emitters.

use dynasmrt::{DynamicLabel, DynasmApi, DynasmLabelApi, dynasm, x64::Assembler};
use otter_vm::{JitCompileSnapshot, native_abi as abi};

use super::{emit_load_reg, emit_store_reg};
use crate::x86_64::{
    frame,
    values::{emit_load_runtime_stub, emit_load_symbol_u64},
};
use crate::{
    artifact::relocation::RelocationCapture,
    entry::{NATIVE_FRAME_NEW_TARGET_OFFSET, Unsupported, VALUE_UNDEFINED},
    x86_64::js_call::{
        CallTarget, emit_call as emit_js_call, emit_enter_staged, emit_pop_arguments,
        emit_push_arguments, emit_staged_call,
    },
};

/// `new.target` of a call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum CallNewTarget {
    /// `[[Call]]`.
    None,
    /// `new C(...)`: the constructor itself.
    Callee,
    /// `super(...)`: the frame's `new.target`, or the parent outside a
    /// construct.
    Super,
}

/// Deliver a completion in `rax`/`rdx`: success to `dst`, a throw to
/// `throw_value`, a parked error to `threw`. A callee that retired itself
/// for a tail call it staged returns `Continue`; that call is entered in its
/// place and its completion delivered the same way. A callee's `Fatal` is
/// final and reaches `fatal` without another projection.
pub(super) fn emit_completion(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    table: &crate::entry::TransitionTable,
    return_sites: &mut crate::return_sites::ReturnSiteRecorder<'_>,
    dst: u16,
    throw_value: DynamicLabel,
    threw: DynamicLabel,
    fatal: DynamicLabel,
) -> Result<(), Unsupported> {
    let completion = ops.new_dynamic_label();
    let error = ops.new_dynamic_label();
    let done = ops.new_dynamic_label();
    dynasm!(ops
        ; .arch x64
        ; =>completion
        ; test rdx, rdx
        ; jz =>done
        ; cmp edx, abi::NativeResultStatus::Continue as i32
        ; jne =>error
    );
    return_sites.record(emit_enter_staged(ops, relocations, table, 15))?;
    dynasm!(ops
        ; .arch x64
        ; jmp =>completion
        ; =>error
    );
    super::emit_cold_call_source(ops, return_sites.logical_pc, return_sites.safepoint_id);
    dynasm!(ops
        ; .arch x64
        ; cmp edx, abi::NativeResultStatus::Throw as i32
        ; je =>throw_value
        ; cmp edx, abi::NativeResultStatus::Fatal as i32
        ; je =>fatal
        ; jmp =>threw
        ; =>done
    );
    emit_store_reg(ops, 0, dst);
    Ok(())
}

/// `rcx = new.target` for a callee in `rsi`.
fn emit_new_target(ops: &mut Assembler, new_target: CallNewTarget) {
    // The opcode owner distinguishes Super from ordinary construction before
    // entering either Known or generic linkage. No callee classification guess.
    match new_target {
        CallNewTarget::Super => dynasm!(ops ; .arch x64
            ; mov [r15 + (crate::entry::PENDING_CALL_OFFSET + abi::REQUEST_SUPER_ORIGIN_OFFSET) as i32], r14),
        CallNewTarget::Callee => dynasm!(ops ; .arch x64
            ; mov QWORD [r15 + (crate::entry::PENDING_CALL_OFFSET + abi::REQUEST_SUPER_ORIGIN_OFFSET) as i32], 0),
        CallNewTarget::None => {}
    }
    match new_target {
        CallNewTarget::None => {}
        CallNewTarget::Callee => dynasm!(ops ; .arch x64 ; mov rcx, rsi),
        CallNewTarget::Super => {
            let ready = ops.new_dynamic_label();
            dynasm!(ops
                ; .arch x64
                ; mov rcx, [r14 + NATIVE_FRAME_NEW_TARGET_OFFSET as i32]
                ; cmp rcx, VALUE_UNDEFINED as i32
                ; jne =>ready
                ; mov rcx, rsi
                ; =>ready
            );
        }
    }
}

/// Branch to `miss` unless the callable in `r9` is the bytecode function
/// `function_id` without runtime call setup. Clobbers r8, r10 and r11.
fn emit_identity_guard(
    ops: &mut Assembler,
    view: &JitCompileSnapshot,
    function_id: u32,
    miss: DynamicLabel,
) {
    let guarded = ops.new_dynamic_label();
    let layout = view.closure_call_layout;
    dynasm!(ops
        ; .arch x64
        ; mov r10, QWORD otter_vm::value::tag::box_function_id(function_id) as i64
        ; cmp r9, r10
        ; je =>guarded
        ; test r9, r9
        ; jz =>miss
        ; mov r11, QWORD otter_vm::value::tag::NOT_CELL_MASK as i64
        ; test r9, r11
        ; jnz =>miss
        ; cmp BYTE [r9], otter_vm::closure::JS_CLOSURE_BODY_TYPE_TAG as i8
        ; jne =>miss
    );
    if layout.runtime_setup_flags != 0 {
        dynasm!(ops
            ; .arch x64
            ; test DWORD [r9 + layout.flags_byte as i32], layout.runtime_setup_flags as i32
            ; jnz =>miss
        );
    }
    dynasm!(ops
        ; .arch x64
        ; cmp DWORD [r9 + layout.function_id_byte as i32], function_id as i32
        ; jne =>miss
        ; =>guarded
    );
}

/// Enter one proven or generic target with its callable in `r9`.
#[allow(clippy::too_many_arguments)]
fn emit_target_call(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    table: &crate::entry::TransitionTable,
    return_sites: &mut crate::return_sites::ReturnSiteRecorder<'_>,
    receiver: Option<u16>,
    new_target: CallNewTarget,
    arguments: &[u16],
    target: CallTarget,
    dst: u16,
    throw_value: DynamicLabel,
    threw: DynamicLabel,
    fatal: DynamicLabel,
) -> Result<(), Unsupported> {
    let count = u32::try_from(arguments.len())
        .map_err(|_| Unsupported::OperandShape("x86-64 call actual count"))?;
    let bytes = emit_push_arguments(ops, arguments.len(), 0, |ops, index, register, _| {
        emit_load_reg(ops, register, arguments[index]);
        Ok(())
    })?;
    dynasm!(ops ; .arch x64 ; mov rsi, r9);
    if let Some(receiver) = receiver {
        emit_load_reg(ops, 2, receiver);
    }
    emit_new_target(ops, new_target);
    return_sites.record(emit_js_call(
        ops,
        relocations,
        table,
        15,
        receiver.is_some(),
        new_target != CallNewTarget::None,
        count,
        target,
    ))?;
    emit_pop_arguments(ops, bytes);
    emit_completion(
        ops,
        relocations,
        table,
        return_sites,
        dst,
        throw_value,
        threw,
        fatal,
    )?;
    Ok(())
}

/// Call `callee` (a register, or `r9` when `None`) with `arguments` and
/// deliver the completion to `dst`. A proven target `known` is entered
/// through its current generation after its identity guard; every other
/// callee through the generic entry.
#[allow(clippy::too_many_arguments)]
pub(super) fn emit_call(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    table: &crate::entry::TransitionTable,
    return_sites: &mut crate::return_sites::ReturnSiteRecorder<'_>,
    view: &JitCompileSnapshot,
    callee: Option<u16>,
    receiver: Option<u16>,
    new_target: CallNewTarget,
    arguments: &[u16],
    known: Option<otter_vm::jit::JitDirectCallPlan>,
    call_pc: u32,
    dst: u16,
    throw_value: DynamicLabel,
    threw: DynamicLabel,
    fatal: DynamicLabel,
) -> Result<(), Unsupported> {
    let done = ops.new_dynamic_label();
    // `[[Construct]]` enters a proven target directly only when it has the
    // internal method; classification throws otherwise.
    let known = known.filter(|plan| {
        new_target == CallNewTarget::None || plan.call_flags & abi::FUNCTION_CALL_CONSTRUCTIBLE != 0
    });
    if let Some(callee) = callee {
        emit_load_reg(ops, 9, callee);
    }
    let targets = known
        .map(|plan| {
            (
                Some(plan),
                CallTarget::Known {
                    entry_cell: plan.entry_cell,
                    function_id: plan.function_id,
                },
            )
        })
        .into_iter()
        .chain(
            matches!(
                view.native_calls
                    .get(&view.instructions[call_pc as usize].byte_pc),
                Some(otter_vm::JitNativeCall::Native)
            )
            .then_some((None, CallTarget::Native)),
        )
        .chain([(None, CallTarget::Generic)]);
    for (plan, target) in targets {
        let next = ops.new_dynamic_label();
        if matches!(target, CallTarget::Native) {
            crate::x86_64::js_call::emit_native_kind_guard(ops, 9, next);
        }
        if let Some(plan) = plan {
            crate::x86_64::js_call::emit_cached_identity(
                ops,
                relocations,
                plan,
                call_pc,
                next,
                |ops, bail| emit_identity_guard(ops, view, plan.function_id, bail),
            );
        }
        emit_target_call(
            ops,
            relocations,
            table,
            return_sites,
            receiver,
            new_target,
            arguments,
            target,
            dst,
            throw_value,
            threw,
            fatal,
        )?;
        dynasm!(ops ; .arch x64 ; jmp =>done ; =>next);
    }
    dynasm!(ops ; .arch x64 ; =>done);
    Ok(())
}

/// Emit `return callee(args…)` from a strict tail position (§15.10.3); see
/// the AArch64 emitter for the protocol. A called record whose span holds
/// the callee's actuals hands its place to the callee in place; actuals that
/// outgrow the span are staged and the record retires with `Continue`; a
/// constructing record calls ordinarily into `dst`; a tier-entered frame
/// and a pending interrupt reach `leave`.
#[allow(clippy::too_many_arguments)]
pub(super) fn emit_tail_call(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    table: &crate::entry::TransitionTable,
    return_sites: &mut crate::return_sites::ReturnSiteRecorder<'_>,
    view: &JitCompileSnapshot,
    frame_kind: abi::NativeFrameKind,
    callee: u16,
    arguments: &[u16],
    known: Option<otter_vm::jit::JitDirectCallPlan>,
    call_pc: u32,
    dst: u16,
    leave: DynamicLabel,
    throw_value: DynamicLabel,
    threw: DynamicLabel,
    fatal: DynamicLabel,
) -> Result<(), Unsupported> {
    let ordinary = ops.new_dynamic_label();
    let outgrown = ops.new_dynamic_label();
    let count = u32::try_from(arguments.len())
        .map_err(|_| Unsupported::OperandShape("x86-64 tail call actual count"))?;
    frame::emit_tail_admission(ops, leave, ordinary);
    emit_load_reg(ops, 9, callee);
    let targets = known
        .map(|plan| {
            (
                Some(plan),
                CallTarget::Known {
                    entry_cell: plan.entry_cell,
                    function_id: plan.function_id,
                },
            )
        })
        .into_iter()
        .chain(
            matches!(
                view.native_calls
                    .get(&view.instructions[call_pc as usize].byte_pc),
                Some(otter_vm::JitNativeCall::Native)
            )
            .then_some((None, CallTarget::Native)),
        )
        .chain([(None, CallTarget::Generic)]);
    for (plan, target) in targets {
        let next = ops.new_dynamic_label();
        if matches!(target, CallTarget::Native) {
            crate::x86_64::js_call::emit_native_kind_guard(ops, 9, next);
        }
        if let Some(plan) = plan {
            crate::x86_64::js_call::emit_cached_identity(
                ops,
                relocations,
                plan,
                call_pc,
                next,
                |ops, bail| emit_identity_guard(ops, view, plan.function_id, bail),
            );
        }
        let bytes = crate::call_linkage::pushed_argument_bytes(arguments.len())?;
        frame::emit_tail_span_check(ops, bytes / 8, outgrown);
        emit_push_arguments(ops, arguments.len(), 0, |ops, index, register, _| {
            emit_load_reg(ops, register, arguments[index]);
            Ok(())
        })?;
        dynasm!(ops ; .arch x64 ; mov rsi, r9);
        frame::emit_tail_transfer(ops, relocations, table, bytes, count, target, frame_kind);
        dynasm!(ops ; .arch x64 ; =>next);
    }
    // The generic target's span check fell through only when it fits, so
    // control never reaches here from a transfer.
    let words = std::iter::once(callee)
        .chain(arguments.iter().copied())
        .collect::<Vec<_>>();
    let bytes = crate::call_linkage::pushed_argument_bytes(words.len())?;
    let staged = ops.new_dynamic_label();
    dynasm!(ops ; .arch x64 ; =>outgrown ; sub rsp, bytes as i32);
    for (index, &word) in words.iter().enumerate() {
        emit_load_reg(ops, 0, word);
        dynasm!(ops ; .arch x64 ; mov [rsp + (index * 8) as i32], rax);
    }
    dynasm!(ops
        ; .arch x64
        ; mov rdi, r15
        ; mov rsi, rsp
        ; mov edx, words.len() as i32
    );
    super::emit_cold_call_source(ops, return_sites.logical_pc, return_sites.safepoint_id);
    emit_load_runtime_stub(
        ops,
        relocations,
        table.entry(abi::STUB_JIT_STAGE_TAIL_CALL),
        abi::STUB_JIT_STAGE_TAIL_CALL,
    );
    crate::x86_64::call_abi::emit_runtime_call(ops, abi::STUB_JIT_STAGE_TAIL_CALL);
    dynasm!(ops
        ; .arch x64
        ; add rsp, bytes as i32
        ; test rdx, rdx
        ; jz =>staged
        ; cmp edx, abi::NativeResultStatus::Throw as i32
        ; je =>throw_value
        ; jmp =>fatal
        ; =>staged
    );
    frame::emit_tail_return(ops, frame_kind);
    dynasm!(ops ; .arch x64 ; =>ordinary);
    emit_call(
        ops,
        relocations,
        table,
        return_sites,
        view,
        Some(callee),
        None,
        CallNewTarget::None,
        arguments,
        known,
        call_pc,
        dst,
        throw_value,
        threw,
        fatal,
    )
}

/// Resolve the method of `receiver` through the shared caches, then call it
/// with `receiver` as `this`.
#[allow(clippy::too_many_arguments)]
pub(super) fn emit_method_call(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    table: &crate::entry::TransitionTable,
    return_sites: &mut crate::return_sites::ReturnSiteRecorder<'_>,
    view: &JitCompileSnapshot,
    shared_property: &mut super::shared_property::SharedPropertyProbes,
    mut direct_call_events: Option<&mut crate::template::DirectCallEvents>,
    mut code_map: Option<&mut crate::artifact::CodeMapCapture>,
    logical_pc: u32,
    byte_pc: u32,
    receiver: u16,
    arguments: &[u16],
    dst: u16,
    throw_value: DynamicLabel,
    threw: DynamicLabel,
    fatal: DynamicLabel,
) -> Result<(), Unsupported> {
    let done = ops.new_dynamic_label();
    if let Some(targets) = view.direct_methods.get(&byte_pc) {
        for target in targets {
            let start = ops.offset().0;
            let next = ops.new_dynamic_label();
            emit_load_reg(ops, 8, receiver);
            crate::x86_64::method_guard::emit(ops, relocations, view, &target.guard, next)?;
            let plan = target.callee.plan;
            emit_target_call(
                ops,
                relocations,
                table,
                return_sites,
                Some(receiver),
                CallNewTarget::None,
                arguments,
                CallTarget::Known {
                    entry_cell: plan.entry_cell,
                    function_id: plan.function_id,
                },
                dst,
                throw_value,
                threw,
                fatal,
            )?;
            crate::template::record_generated_direct_call(
                direct_call_events.as_deref_mut(),
                otter_vm::JitDirectCallKind::Method,
                logical_pc,
                byte_pc,
                &target.callee,
                target.target_index,
                target.target_count,
            );
            if let Some(map) = code_map.as_deref_mut() {
                map.record(crate::artifact::CodeRegion::call_structural(
                    "callTrampoline",
                    start,
                    ops.offset().0,
                    view.code_block.id,
                    logical_pc,
                    byte_pc,
                    Some(plan.function_id),
                ));
            }
            dynasm!(ops ; .arch x64 ; jmp =>done ; =>next);
        }
    }
    let resolved = ops.new_dynamic_label();
    // Every other receiver runs the site's load IC (its handlers, then the
    // isolate's shared action table as V8's megamorphic stub cache); a
    // closure or native hit is called generically. A miss resolves through
    // the committed method resolution, which also records call feedback and
    // updates the slot.
    let resolve = ops.new_dynamic_label();
    if let Some(ic_slot) = view
        .property_accesses
        .get(&byte_pc)
        .map(|access| access.ic_slot)
        .filter(|&slot| slot != 0 && view.cage_base != 0)
    {
        use super::shared_property::{LOAD_RECEIVER, LOAD_SLOT};
        let routine = shared_property.method_label(ops);
        emit_load_reg(ops, LOAD_RECEIVER, receiver);
        emit_load_symbol_u64(
            ops,
            relocations,
            LOAD_SLOT,
            ic_slot,
            crate::artifact::relocation::RelocationTarget::PropertyIcSlot {
                function_id: view.code_block.id,
                byte_pc,
            },
        );
        dynasm!(ops ; .arch x64
            ; call =>routine
            ; test rdx, rdx
            ; jz =>resolved);
    }
    dynasm!(ops ; .arch x64 ; =>resolve);
    emit_load_reg(ops, 0, receiver);
    dynasm!(ops
        ; .arch x64
        ; sub rsp, 16
        ; mov [rsp], rax
        ; mov rdi, r15
        ; mov rsi, rsp
        ; mov edx, 1
    );
    super::emit_cold_call_source(ops, return_sites.logical_pc, return_sites.safepoint_id);
    emit_load_runtime_stub(
        ops,
        relocations,
        table.entry(abi::STUB_JIT_RESOLVE_METHOD),
        abi::STUB_JIT_RESOLVE_METHOD,
    );
    crate::x86_64::call_abi::emit_runtime_call(ops, abi::STUB_JIT_RESOLVE_METHOD);
    dynasm!(ops
        ; .arch x64
        ; add rsp, 16
        ; test rdx, rdx
        ; jz =>resolved
        ; cmp edx, abi::NativeResultStatus::Throw as i32
        ; je =>throw_value
        ; jmp =>threw
        ; =>resolved
        ; mov r9, rax
    );
    emit_call(
        ops,
        relocations,
        table,
        return_sites,
        view,
        None,
        Some(receiver),
        CallNewTarget::None,
        arguments,
        None,
        logical_pc,
        dst,
        throw_value,
        threw,
        fatal,
    )?;
    dynasm!(ops ; .arch x64 ; =>done);
    Ok(())
}

/// `CallSpread`, `NewSpread` and `SuperConstructSpread`: stage the dense
/// spread array's elements as the request's span, then call.
#[allow(clippy::too_many_arguments)]
pub(super) fn emit_spread_call_op(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    table: &crate::entry::TransitionTable,
    return_sites: &mut crate::return_sites::ReturnSiteRecorder<'_>,
    opcode: u8,
    arg0: u64,
    arg1: u64,
    arg2: u64,
    throw_value: DynamicLabel,
    threw: DynamicLabel,
    fatal: DynamicLabel,
) -> Result<(), Unsupported> {
    let lane = |packed: u64, index: usize| ((packed >> (index * 16)) & 0xffff) as u16;
    let (dst, callee, receiver, array, new_target) =
        if opcode == otter_bytecode::Op::CallSpread as u8 {
            (
                lane(arg0, 0),
                lane(arg0, 1),
                Some(lane(arg0, 2)),
                lane(arg0, 3),
                CallNewTarget::None,
            )
        } else if opcode == otter_bytecode::Op::NewSpread as u8 {
            (
                arg0 as u16,
                arg1 as u16,
                None,
                arg2 as u16,
                CallNewTarget::Callee,
            )
        } else if opcode == otter_bytecode::Op::SuperConstructSpread as u8 {
            (
                arg0 as u16,
                arg1 as u16,
                None,
                arg2 as u16,
                CallNewTarget::Super,
            )
        } else {
            return Err(Unsupported::OperandShape("x86-64 spread call opcode"));
        };
    let staged = ops.new_dynamic_label();
    emit_load_reg(ops, 6, array);
    dynasm!(ops ; .arch x64 ; mov rdi, r15);
    super::emit_cold_call_source(ops, return_sites.logical_pc, return_sites.safepoint_id);
    emit_load_runtime_stub(
        ops,
        relocations,
        table.entry(abi::STUB_JIT_STAGE_SPREAD),
        abi::STUB_JIT_STAGE_SPREAD,
    );
    crate::x86_64::call_abi::emit_runtime_call(ops, abi::STUB_JIT_STAGE_SPREAD);
    dynasm!(ops
        ; .arch x64
        ; test rdx, rdx
        ; jz =>staged
        ; cmp edx, abi::NativeResultStatus::Throw as i32
        ; je =>throw_value
        ; jmp =>threw
        ; =>staged
    );
    emit_load_reg(ops, 6, callee);
    if let Some(receiver) = receiver {
        emit_load_reg(ops, 2, receiver);
    }
    emit_new_target(ops, new_target);
    return_sites.record(emit_staged_call(
        ops,
        relocations,
        table,
        15,
        receiver.is_some(),
        new_target != CallNewTarget::None,
    ))?;
    emit_completion(
        ops,
        relocations,
        table,
        return_sites,
        dst,
        throw_value,
        threw,
        fatal,
    )?;
    Ok(())
}
