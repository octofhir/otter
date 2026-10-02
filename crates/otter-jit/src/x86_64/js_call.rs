//! Generated JavaScript calls on x86-64.
//!
//! # Contents
//! - [`emit_push_arguments`] / [`emit_pop_arguments`] — the actual span on
//!   the caller's stack.
//! - [`emit_call`] — the remaining call ABI registers and the call: the current
//!   generation of a proven bytecode target through its `FunctionEntryCell`,
//!   or the generic entry that classifies any other callee.
//! - [`emit_staged_call`] / [`emit_enter_staged`] — a call whose span, or
//!   whole request, a runtime staging entry wrote.
//! - [`emit_cached_identity`] — a call site's identity proof behind its cache
//!   of the last callee it proved.
//!
//! # Invariants
//! - Call ABI: `rdi` context, `rsi` callee, `rdx` receiver, `rcx`
//!   `new.target` (`undefined` exactly for `[[Call]]`), `r8` actual count,
//!   `r9` the entered generation; the span sits above the return address,
//!   padded with `undefined` to a proven target's formal count. The
//!   completion returns in `rax`/`rdx`.
//! - The caller keeps `rsp` 16-byte aligned across the span and the call.
//! - Emitters clobber every caller-saved register and preserve `rbx`, `rbp`
//!   and `r12`–`r15`.
//!
//! # See also
//! - [`crate::call_linkage`] — the architecture-neutral call contract.
//! - `crate::arm64::js_call` — the AArch64 caller.

use dynasmrt::{DynamicLabel, DynasmApi, DynasmLabelApi, dynasm, x64::Assembler};
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
) {
    if !receiver {
        dynasm!(ops ; .arch x64 ; mov edx, VALUE_UNDEFINED as i32);
    }
    if !new_target {
        dynasm!(ops ; .arch x64 ; mov ecx, VALUE_UNDEFINED as i32);
    }
    dynasm!(ops
        ; .arch x64
        ; mov rdi, Rq(context)
        ; mov r8d, count as i32
    );
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
        CallTarget::Generic => {
            let start = ops.offset().0;
            dynasm!(ops ; .arch x64 ; mov r11, QWORD table.entry(abi::STUB_JIT_CALL_GENERIC) as i64);
            relocations.record_x86_imm64(
                start,
                ops.offset().0,
                11,
                RelocationTarget::runtime_stub(abi::STUB_JIT_CALL_GENERIC),
            );
            dynasm!(ops ; .arch x64 ; call r11);
        }
    }
}

/// Enter the trampoline with the complete request a staging entry wrote.
/// Returns the completion in `rax`/`rdx`.
pub(crate) fn emit_enter_staged(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    table: &TransitionTable,
    context: u8,
) {
    dynasm!(ops ; .arch x64 ; mov rdi, Rq(context));
    let start = ops.offset().0;
    dynasm!(ops ; .arch x64 ; mov r11, QWORD table.entry(abi::STUB_JIT_CALL) as i64);
    relocations.record_x86_imm64(
        start,
        ops.offset().0,
        11,
        RelocationTarget::runtime_stub(abi::STUB_JIT_CALL),
    );
    dynasm!(ops ; .arch x64 ; call r11);
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
) {
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
    emit_enter_staged(ops, relocations, table, context);
}
