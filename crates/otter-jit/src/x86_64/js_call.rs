//! Generated JavaScript calls on x86-64.
//!
//! # Contents
//! - [`emit_push_arguments`] / [`emit_pop_arguments`] — the actual span on
//!   the caller's stack.
//! - [`emit_call`] — the remaining call ABI registers and the call: the current
//!   generation of a proven bytecode target through its `FunctionEntryCell`,
//!   a proved NativeFunction through its selected Host convention, or the
//!   generic entry that classifies any other callee.
//! - [`emit_staged_call`] / [`emit_enter_staged`] — a call whose span, or
//!   whole request, a runtime staging entry wrote.
//! - [`emit_tail_branch`] — the entry jump of a proper tail call.
//! - [`emit_cached_identity`] — a call site's identity proof behind its cache
//!   of the last callee it proved.
//!
//! # Invariants
//! - Call ABI: `rdi` context, `rsi` callee, `rdx` receiver, `rcx`
//!   `new.target` (`undefined` exactly for `[[Call]]`), `r8` actual count,
//!   `r9` the entered generation; the span sits above the return address,
//!   containing exactly the actuals; the callee initializes missing formals. The
//!   completion returns in `rax`/`rdx`.
//! - Call emitters return the exact offset immediately after CALL, before
//!   platform result reloads, argument cleanup or completion handling.
//! - The caller keeps `rsp` 16-byte aligned across the span and the call.
//! - Emitters clobber every caller-saved register and preserve `rbx`, `rbp`
//!   and `r12`–`r15`.
//! - A fully staged request enters the C trampoline through the platform C
//!   boundary. Known, generic and tail JavaScript targets use the private ABI.
//!
//! # See also
//! - [`crate::call_linkage`] — the architecture-neutral call contract.
//! - `crate::arm64::js_call` — the AArch64 caller.

use dynasmrt::{AssemblyOffset, DynamicLabel, DynasmApi, DynasmLabelApi, dynasm, x64::Assembler};
use otter_vm::native_abi as abi;

use crate::{
    artifact::relocation::{RelocationCapture, RelocationTarget},
    call_linkage::pushed_argument_bytes,
    entry::{
        PENDING_CALL_OFFSET, REQUEST_CALLEE_OFFSET, REQUEST_ENTRY_OFFSET, REQUEST_FLAGS_OFFSET,
        REQUEST_NEW_TARGET_OFFSET, REQUEST_RECEIVER_OFFSET, REQUEST_REGISTER_SEED_OFFSET,
        TransitionTable, Unsupported, VALUE_UNDEFINED,
    },
};

pub(crate) use crate::call_linkage::CallTarget;

/// Prove a full tagged NativeFunction cell before reading its header.
/// Only R10/R11 and flags are clobbered; the callee must exclude those scratch.
pub(crate) fn emit_native_kind_guard(ops: &mut Assembler, value: u8, miss: DynamicLabel) {
    assert!(
        value != 10 && value != 11,
        "native callee cannot alias guard scratch"
    );
    dynasm!(ops ; .arch x64
        ; mov r10, QWORD otter_vm::value::tag::NOT_CELL_MASK as i64
        ; test Rq(value), r10 ; jnz =>miss ; test Rq(value), Rq(value) ; jz =>miss
        ; cmp BYTE [Rq(value)], otter_vm::native_function::NATIVE_FUNCTION_BODY_TYPE_TAG as i8
        ; jne =>miss
    );
}

/// Clear the sole canonical incoming construction ticket. No callable,
/// receiver, argument, flags or target register is clobbered.
pub(crate) fn emit_clear_construct_ticket(ops: &mut Assembler, context: u8) {
    dynasm!(ops ; .arch x64
        ; mov QWORD [Rq(context) + (PENDING_CALL_OFFSET + abi::REQUEST_SUPER_ORIGIN_OFFSET) as i32], 0
        ; mov DWORD [Rq(context) + (PENDING_CALL_OFFSET + abi::REQUEST_CONSTRUCT_LAYOUT_OFFSET) as i32], 0
        ; mov QWORD [Rq(context) + (PENDING_CALL_OFFSET + abi::REQUEST_CONSTRUCT_RECEIVER_OFFSET) as i32], VALUE_UNDEFINED as i32
    );
}

/// Prove the callee in `r9` is `plan`'s function or branch to `bail`.
///
/// The site's identity cell holds the last callee proved here, so a repeated
/// callee costs one compare; any other value runs `proof`, which branches to
/// `bail` on failure, and is cached on success. Clobbers `r10` besides what
/// `proof` clobbers.
pub(crate) fn emit_cached_identity(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    plan: otter_vm::jit::JitDirectCallPlan,
    call_pc: u32,
    bail: DynamicLabel,
    proof: impl FnOnce(&mut Assembler, DynamicLabel),
) {
    if plan.callee_cell == 0 {
        proof(ops, bail);
        return;
    }
    let cell = RelocationTarget::CalleeIdentityCell {
        function_id: plan.function_id,
        call_pc,
    };
    let proven = ops.new_dynamic_label();
    let start = ops.offset().0;
    dynasm!(ops ; .arch x64 ; mov r10, QWORD plan.callee_cell as i64);
    relocations.record_x86_imm64(start, ops.offset().0, 10, cell.clone());
    dynasm!(ops ; .arch x64 ; cmp r9, [r10] ; je =>proven);
    proof(ops, bail);
    let start = ops.offset().0;
    dynasm!(ops ; .arch x64 ; mov r10, QWORD plan.callee_cell as i64);
    relocations.record_x86_imm64(start, ops.offset().0, 10, cell);
    dynasm!(ops ; .arch x64 ; mov [r10], r9 ; =>proven);
}

/// Reserve and fill a fixed actual span below `rsp`.
///
/// `load(ops, index, register, rsp_bias)` loads actual `index` into
/// `register`; `rsp_bias` is the reservation already below the caller's
/// frame-relative homes. Returns the reserved byte count.
pub(crate) fn emit_push_arguments<Load>(
    ops: &mut Assembler,
    count: usize,
    scratch: u8,
    mut load: Load,
) -> Result<u32, Unsupported>
where
    Load: FnMut(&mut Assembler, usize, u8, u32) -> Result<(), Unsupported>,
{
    let bytes = pushed_argument_bytes(count)?;
    if bytes != 0 {
        dynasm!(ops ; .arch x64 ; sub rsp, bytes as i32);
    }
    for index in 0..count {
        load(ops, index, scratch, bytes)?;
        let offset = i32::try_from(index * 8)
            .map_err(|_| Unsupported::OperandShape("call actual offset"))?;
        dynasm!(ops ; .arch x64 ; mov [rsp + offset], Rq(scratch));
    }
    Ok(bytes)
}

/// Release a span reserved by [`emit_push_arguments`].
pub(crate) fn emit_pop_arguments(ops: &mut Assembler, bytes: u32) {
    if bytes != 0 {
        dynasm!(ops ; .arch x64 ; add rsp, bytes as i32);
    }
}

/// Complete the call ABI registers around a callee already in `rsi`, a
/// receiver in `rdx` when `receiver` (`undefined` otherwise) and a
/// `new.target` in `rcx` exactly for `[[Construct]]`, then call `target`
/// with `count` actuals at `rsp`. Returns with the completion in `rax`/`rdx`.
#[allow(clippy::too_many_arguments)]
pub(crate) fn emit_call(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    table: &TransitionTable,
    context: u8,
    receiver: bool,
    new_target: bool,
    count: u32,
    target: CallTarget,
) -> AssemblyOffset {
    if !receiver {
        dynasm!(ops ; .arch x64 ; mov edx, VALUE_UNDEFINED as i32);
    }
    // Every request consumer clears the construction fields, so an ordinary
    // call has nothing to clear.
    if !new_target {
        dynasm!(ops ; .arch x64 ; mov ecx, VALUE_UNDEFINED as i32);
    }
    dynasm!(ops
        ; .arch x64
        ; mov rdi, Rq(context)
        ; mov r8d, count as i32
    );
    emit_target_call(ops, relocations, table, target)
}

fn emit_target_call(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    table: &TransitionTable,
    target: CallTarget,
) -> AssemblyOffset {
    match target {
        CallTarget::Known {
            entry_cell,
            function_id,
        } => {
            let start = ops.offset().0;
            dynasm!(ops ; .arch x64 ; mov r11, QWORD entry_cell as i64);
            relocations.record_x86_imm64(
                start,
                ops.offset().0,
                11,
                RelocationTarget::FunctionEntryCell { function_id },
            );
            dynasm!(ops
                ; .arch x64
                ; mov r9, [r11]
                ; call QWORD [r9]
            );
        }
        CallTarget::Generic | CallTarget::Native => {
            let stub = match target {
                CallTarget::Native => abi::STUB_JIT_CALL_NATIVE,
                _ => abi::STUB_JIT_CALL_GENERIC,
            };
            let start = ops.offset().0;
            dynasm!(ops ; .arch x64 ; mov r11, QWORD table.entry(stub) as i64);
            relocations.record_x86_imm64(
                start,
                ops.offset().0,
                11,
                RelocationTarget::runtime_stub(stub),
            );
            dynasm!(ops ; .arch x64 ; call r11);
        }
    }
    ops.offset()
}

/// Jump to `target` with the call ABI registers already set and `rsp` on
/// the return address: the tail of a proper tail call, whose callee returns
/// to the retired record's caller. Clobbers `r9` and `r11`.
pub(crate) fn emit_tail_branch(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    table: &TransitionTable,
    target: CallTarget,
) {
    match target {
        CallTarget::Known {
            entry_cell,
            function_id,
        } => {
            let start = ops.offset().0;
            dynasm!(ops ; .arch x64 ; mov r11, QWORD entry_cell as i64);
            relocations.record_x86_imm64(
                start,
                ops.offset().0,
                11,
                RelocationTarget::FunctionEntryCell { function_id },
            );
            dynasm!(ops
                ; .arch x64
                ; mov r9, [r11]
                ; jmp QWORD [r9]
            );
        }
        CallTarget::Generic | CallTarget::Native => {
            let stub = match target {
                CallTarget::Native => abi::STUB_JIT_CALL_NATIVE,
                _ => abi::STUB_JIT_CALL_GENERIC,
            };
            let start = ops.offset().0;
            dynasm!(ops ; .arch x64 ; mov r11, QWORD table.entry(stub) as i64);
            relocations.record_x86_imm64(
                start,
                ops.offset().0,
                11,
                RelocationTarget::runtime_stub(stub),
            );
            dynasm!(ops ; .arch x64 ; jmp r11);
        }
    }
}

/// Publish the generated caller's genuine return coordinate and enter the
/// trampoline with the complete staged request. Existing nonzero, tier-transfer
/// and tail-call associations remain owned by their original source extent.
/// Returns the completion in `rax`/`rdx`.
pub(crate) fn emit_enter_staged(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    table: &TransitionTable,
    context: u8,
) -> AssemblyOffset {
    let return_to_caller = ops.new_dynamic_label();
    let origin_ready = ops.new_dynamic_label();
    let no_generated_caller = ops.new_dynamic_label();
    dynasm!(ops ; .arch x64
        ; mov rdi, Rq(context)
        ; lea r10, [rdi + PENDING_CALL_OFFSET as i32]
        ; cmp QWORD [r10 + abi::REQUEST_CALLER_RETURN_PC_OFFSET as i32], 0
        ; jne =>origin_ready
        ; test BYTE [r10 + REQUEST_FLAGS_OFFSET as i32], (abi::NativeFrameFlags::TIER_ENTRY | abi::NativeFrameFlags::TAIL_CALL) as i8
        ; jnz =>origin_ready
        ; mov r11, [rdi + crate::entry::NATIVE_FRAME_OFFSET as i32]
        ; mov [r10 + abi::REQUEST_CALLER_OFFSET as i32], r11
        ; test r11, r11
        ; jz =>no_generated_caller
        ; cmp DWORD [r11 + abi::NATIVE_FRAME_CODE_OBJECT_ID_OFFSET as i32], 0
        ; je =>no_generated_caller
        ; lea r11, [=>return_to_caller]
        ; mov [r10 + abi::REQUEST_CALLER_RETURN_PC_OFFSET as i32], r11
        ; jmp =>origin_ready
        ; =>no_generated_caller
        ; mov QWORD [r10 + abi::REQUEST_CALLER_RETURN_PC_OFFSET as i32], 0
        ; =>origin_ready
    );
    let start = ops.offset().0;
    dynasm!(ops ; .arch x64 ; mov r11, QWORD table.entry(abi::STUB_JIT_CALL) as i64);
    relocations.record_x86_imm64(
        start,
        ops.offset().0,
        11,
        RelocationTarget::runtime_stub(abi::STUB_JIT_CALL),
    );
    super::call_abi::emit_staged_call(ops, return_to_caller)
}

/// Write the context's request around a span a staging entry already wrote,
/// with the callee in `rsi`, a receiver in `rdx` when `receiver` and a
/// `new.target` in `rcx` exactly for `[[Construct]]`; then enter the
/// trampoline. Returns the completion in `rax`/`rdx`.
pub(crate) fn emit_staged_call(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    table: &TransitionTable,
    context: u8,
    receiver: bool,
    new_target: bool,
) -> AssemblyOffset {
    emit_clear_construct_ticket(ops, context);
    let flags = if new_target {
        abi::NativeFrameFlags::CONSTRUCT
    } else {
        0
    };
    dynasm!(ops
        ; .arch x64
        ; lea r10, [Rq(context) + PENDING_CALL_OFFSET as i32]
        ; mov QWORD [r10 + REQUEST_ENTRY_OFFSET as i32], 0
        ; mov BYTE [r10 + REQUEST_FLAGS_OFFSET as i32], flags as i8
        ; mov [r10 + REQUEST_CALLEE_OFFSET as i32], rsi
        ; mov r11d, VALUE_UNDEFINED as i32
    );
    if receiver {
        dynasm!(ops ; .arch x64 ; mov [r10 + REQUEST_RECEIVER_OFFSET as i32], rdx);
    } else {
        dynasm!(ops ; .arch x64 ; mov [r10 + REQUEST_RECEIVER_OFFSET as i32], r11);
    }
    if new_target {
        dynasm!(ops ; .arch x64 ; mov [r10 + REQUEST_NEW_TARGET_OFFSET as i32], rcx);
    } else {
        dynasm!(ops ; .arch x64 ; mov [r10 + REQUEST_NEW_TARGET_OFFSET as i32], r11);
    }
    // No restored registers; the completion returns through the native ABI.
    dynasm!(ops
        ; .arch x64
        ; mov rax, QWORD -4294967296
        ; mov [r10 + REQUEST_REGISTER_SEED_OFFSET as i32], rax
        ; mov QWORD [r10 + REQUEST_REGISTER_SEED_OFFSET as i32 + 8], 0
    );
    emit_enter_staged(ops, relocations, table, context)
}

/// Forward with the already prepared frame argument count in r8d.
pub(crate) fn emit_forwarded_call(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    table: &TransitionTable,
    context: u8,
    target: CallTarget,
) -> AssemblyOffset {
    emit_clear_construct_ticket(ops, context);
    dynasm!(ops ; .arch x64 ; mov rdi, Rq(context) ; mov ecx, VALUE_UNDEFINED as i32);
    emit_target_call(ops, relocations, table, target)
}

/// Copy an activation's exact actual span and replace mapped formals.
/// Stack-limit refusal precedes writes and page probes precede RSP movement.
/// Only old_sp/source, R10/R11 and the final r8d count are changed.
pub(crate) fn emit_push_forwarded(
    ops: &mut Assembler,
    frame: u8,
    [old_sp, source]: [u8; 2],
    overflow: DynamicLabel,
    bindings: &[(u16, crate::call_linkage::ForwardedBinding)],
) {
    use crate::call_linkage::ForwardedBinding;
    let probe = ops.new_dynamic_label();
    let probed = ops.new_dynamic_label();
    dynasm!(ops ; .arch x64
        ; mov r10d, [Rq(frame) + abi::NATIVE_FRAME_ARGUMENT_COUNT_OFFSET as i32]
        ; add r10d, 1 ; and r10d, -2 ; shl r10, 3
        ; mov Rq(old_sp), rsp ; neg r10 ; add r10, Rq(old_sp)
        ; cmp r10, [r15 + crate::entry::NATIVE_STACK_LIMIT_OFFSET as i32] ; jb =>overflow
        ; mov r11, Rq(old_sp) ; =>probe ; sub r11, 4096
        ; cmp r11, r10 ; jbe =>probed ; mov QWORD [r11], 0 ; jmp =>probe
        ; =>probed ; mov rsp, r10
        ; mov Rq(source), [Rq(frame) + abi::NATIVE_FRAME_ACTUALS_OFFSET as i32]
        ; mov r11d, [Rq(frame) + abi::NATIVE_FRAME_ARGUMENT_COUNT_OFFSET as i32]
    );
    let copy = ops.new_dynamic_label();
    let copied = ops.new_dynamic_label();
    dynasm!(ops ; .arch x64 ; =>copy ; test r11d, r11d ; jz =>copied
        ; dec r11d ; mov r10, [Rq(source) + r11 * 8] ; mov [rsp + r11 * 8], r10
        ; jmp =>copy ; =>copied
    );
    for &(index, binding) in bindings {
        let skip = ops.new_dynamic_label();
        dynasm!(ops ; .arch x64
            ; cmp DWORD [Rq(frame) + abi::NATIVE_FRAME_ARGUMENT_COUNT_OFFSET as i32], i32::from(index)
            ; jbe =>skip
        );
        let value = match binding {
            ForwardedBinding::Register(register) => register,
            ForwardedBinding::Load { base, offset } => {
                dynasm!(ops ; .arch x64 ; mov r10, [Rq(base) + offset as i32]);
                10
            }
            ForwardedBinding::ContextSlot {
                base,
                offset,
                slot_byte,
            } => {
                dynasm!(ops ; .arch x64
                    ; mov r10, [Rq(base) + offset as i32]
                    ; mov r10, [r10 + slot_byte as i32]
                );
                10
            }
            ForwardedBinding::Immediate(bits) => {
                super::values::emit_load_u64(ops, 10, bits);
                10
            }
        };
        dynasm!(ops ; .arch x64 ; mov [rsp + i32::from(index) * 8], Rq(value) ; =>skip);
    }
    dynasm!(ops ; .arch x64 ; mov r8d, [Rq(frame) + abi::NATIVE_FRAME_ARGUMENT_COUNT_OFFSET as i32]);
}

/// Release the span by the caller's unchanged actual count, preserving result.
pub(crate) fn emit_pop_forwarded(ops: &mut Assembler, frame: u8) {
    dynasm!(ops ; .arch x64
        ; mov r10d, [Rq(frame) + abi::NATIVE_FRAME_ARGUMENT_COUNT_OFFSET as i32]
        ; add r10d, 1 ; and r10d, -2 ; shl r10, 3 ; add rsp, r10
    );
}
