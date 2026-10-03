//! Generated JavaScript calls on AArch64.
//!
//! # Contents
//! - [`emit_push_arguments`] / [`emit_pop_arguments`] — the actual span on
//!   the caller's stack.
//! - [`emit_call`] — the call ABI registers and the call itself: the current
//!   generation of a proven bytecode target through its `FunctionEntryCell`,
//!   or the generic entry that classifies any other callee.
//! - [`emit_staged_call`] / [`emit_enter_staged`] — a call whose span, or
//!   whole request, a runtime staging entry wrote.
//! - [`emit_tail_branch`] — the entry branch of a proper tail call.
//!
//! # Invariants
//! - The span is pushed before the call ABI registers are set and popped
//!   after the completion returns; a proven target's span is padded with
//!   `undefined` to its formal count.
//! - Callee activation, receiver binding, constructor completion and callee
//!   deoptimization belong to the callee. A caller consumes only `Success`,
//!   `Throw` (exception in `x0`) or `Fatal` (error parked in the context).
//! - Emitters clobber `x0`–`x17` and preserve every callee-saved register.
//!
//! # See also
//! - [`crate::call_linkage`] — the architecture-neutral call contract.
//! - [`super::activation`] — the callee side of the same ABI.

use dynasmrt::{DynasmApi, aarch64::Assembler, dynasm};
use otter_vm::native_abi as abi;

use crate::{
    artifact::relocation::{RelocationCapture, RelocationTarget},
    call_linkage::pushed_argument_bytes,
    entry::{
        PENDING_CALL_OFFSET, REQUEST_CALLEE_OFFSET, REQUEST_ENTRY_OFFSET, REQUEST_FLAGS_OFFSET,
        REQUEST_NEW_TARGET_OFFSET, REQUEST_RECEIVER_OFFSET, REQUEST_REGISTER_SEED_OFFSET,
        TransitionTable, Unsupported, VALUE_UNDEFINED,
    },
    template::arm64::values::emit_load_u64,
};

/// Reserve and fill a fixed actual span below `sp`.
///
/// `source(ops, index, scratch, sp_bias)` returns the register holding actual
/// `index`, loading it into `scratch` when it lives elsewhere; `sp_bias` is
/// the reservation already below the caller's frame-relative homes. Returns
/// the reserved byte count to release after the call returns.
pub(crate) fn emit_push_arguments<Source>(
    ops: &mut Assembler,
    count: usize,
    mut source: Source,
) -> Result<u32, Unsupported>
where
    Source: FnMut(&mut Assembler, usize, u8, u32) -> Result<u8, Unsupported>,
{
    let bytes = pushed_argument_bytes(count)?;
    // A one- or two-word span is read before the reservation and stored
    // with one pre-indexed store that also reserves it.
    if bytes == 16 {
        let first = source(ops, 0, 15, 0)?;
        if count == 2 {
            let second = source(ops, 1, 16, 0)?;
            dynasm!(ops ; .arch aarch64 ; stp X(first), X(second), [sp, -16]!);
        } else {
            dynasm!(ops ; .arch aarch64 ; str X(first), [sp, -16]!);
        }
        return Ok(bytes);
    }
    if bytes != 0 {
        dynasm!(ops ; .arch aarch64 ; sub sp, sp, bytes);
    }
    let mut index = 0;
    while index < count {
        let first = source(ops, index, 15, bytes)?;
        let offset = u32::try_from(index * 8)
            .map_err(|_| Unsupported::OperandShape("call actual offset"))?;
        if index + 1 < count {
            let second = source(ops, index + 1, 16, bytes)?;
            dynasm!(ops ; .arch aarch64 ; stp X(first), X(second), [sp, (offset) as i32]);
            index += 2;
        } else {
            dynasm!(ops ; .arch aarch64 ; str X(first), [sp, offset]);
            index += 1;
        }
    }
    Ok(bytes)
}

/// Release a span reserved by [`emit_push_arguments`].
pub(crate) fn emit_pop_arguments(ops: &mut Assembler, bytes: u32) {
    if bytes != 0 {
        dynasm!(ops ; .arch aarch64 ; add sp, sp, bytes);
    }
}

pub(crate) use crate::call_linkage::CallTarget;

/// Set the call ABI registers from `callee`, `receiver` (`undefined` when
/// absent) and `new_target` (present exactly for `[[Construct]]`), then call
/// `target` with `count` actuals at `sp`. Returns with the completion in
/// `x0`/`x1`.
///
/// Each value register is either its own ABI register (`x1` callee, `x2`
/// receiver, `x3` `new.target`) or lies outside `x0`–`x4`, `x8` and `x16`;
/// `new.target` may also name `x1`, the callee already in place.
#[allow(clippy::too_many_arguments)]
pub(crate) fn emit_call(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    table: &TransitionTable,
    context: u8,
    callee: u8,
    receiver: Option<u8>,
    new_target: Option<u8>,
    count: u32,
    target: CallTarget,
) {
    debug_assert!(
        [
            (callee, 1),
            (receiver.unwrap_or(2), 2),
            (new_target.unwrap_or(3), 3)
        ]
        .iter()
        .all(|&(register, abi)| register == abi
            || (abi == 3 && register == 1)
            || (register > 4 && register != 8 && register != 16))
    );
    dynasm!(ops ; .arch aarch64 ; mov x0, X(context));
    if callee != 1 {
        dynasm!(ops ; .arch aarch64 ; mov x1, X(callee));
    }
    match receiver {
        Some(2) => {}
        Some(receiver) => dynasm!(ops ; .arch aarch64 ; mov x2, X(receiver)),
        None => dynasm!(ops ; .arch aarch64 ; movz x2, VALUE_UNDEFINED as u32),
    }
    match new_target {
        Some(3) => {}
        Some(new_target) => dynasm!(ops ; .arch aarch64 ; mov x3, X(new_target)),
        None => dynasm!(ops ; .arch aarch64 ; movz x3, VALUE_UNDEFINED as u32),
    }
    emit_load_u64(ops, 4, u64::from(count));
    match target {
        CallTarget::Known {
            entry_cell,
            function_id,
        } => {
            let start = ops.offset().0;
            emit_load_u64(ops, 8, entry_cell);
            relocations.record_mov_wide(
                start,
                ops.offset().0,
                8,
                RelocationTarget::FunctionEntryCell { function_id },
            );
            dynasm!(ops
                ; .arch aarch64
                ; ldr x8, [x8]
                ; ldr x16, [x8]
                ; blr x16
            );
        }
        CallTarget::Generic => {
            let start = ops.offset().0;
            emit_load_u64(ops, 16, table.entry(abi::STUB_JIT_CALL_GENERIC));
            relocations.record_mov_wide(
                start,
                ops.offset().0,
                16,
                RelocationTarget::runtime_stub(abi::STUB_JIT_CALL_GENERIC),
            );
            dynasm!(ops ; .arch aarch64 ; blr x16);
        }
    }
}

/// Branch to `target` with the call ABI registers already set: the tail of
/// a proper tail call, whose callee returns to the retired record's caller.
/// Clobbers `x8` and `x16`.
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
            emit_load_u64(ops, 8, entry_cell);
            relocations.record_mov_wide(
                start,
                ops.offset().0,
                8,
                RelocationTarget::FunctionEntryCell { function_id },
            );
            dynasm!(ops
                ; .arch aarch64
                ; ldr x8, [x8]
                ; ldr x16, [x8]
                ; br x16
            );
        }
        CallTarget::Generic => {
            let start = ops.offset().0;
            emit_load_u64(ops, 16, table.entry(abi::STUB_JIT_CALL_GENERIC));
            relocations.record_mov_wide(
                start,
                ops.offset().0,
                16,
                RelocationTarget::runtime_stub(abi::STUB_JIT_CALL_GENERIC),
            );
            dynasm!(ops ; .arch aarch64 ; br x16);
        }
    }
}

/// Enter the trampoline with the complete request a staging entry wrote.
/// Returns the completion in `x0`/`x1`.
pub(crate) fn emit_enter_staged(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    table: &TransitionTable,
    context: u8,
) {
    dynasm!(ops ; .arch aarch64 ; mov x0, X(context));
    let start = ops.offset().0;
    emit_load_u64(ops, 16, table.entry(abi::STUB_JIT_CALL));
    relocations.record_mov_wide(
        start,
        ops.offset().0,
        16,
        RelocationTarget::runtime_stub(abi::STUB_JIT_CALL),
    );
    dynasm!(ops ; .arch aarch64 ; blr x16);
}

/// Write the context's request around a span a staging entry already wrote,
/// then enter the trampoline. Returns the completion in `x0`/`x1`.
///
/// The value registers must lie outside `x0` and `x9`–`x11`.
pub(crate) fn emit_staged_call(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    table: &TransitionTable,
    context: u8,
    callee: u8,
    receiver: Option<u8>,
    new_target: Option<u8>,
) {
    debug_assert!(
        ![
            callee,
            receiver.unwrap_or(callee),
            new_target.unwrap_or(callee)
        ]
        .iter()
        .any(|register| (9..=11).contains(register) || *register == 0)
    );
    let flags = if new_target.is_some() {
        u32::from(abi::NativeFrameFlags::CONSTRUCT)
    } else {
        0
    };
    dynasm!(ops
        ; .arch aarch64
        ; mov x9, X(context)
        ; add x9, x9, PENDING_CALL_OFFSET
        ; str xzr, [x9, REQUEST_ENTRY_OFFSET]
        ; movz w10, flags
        ; strb w10, [x9, REQUEST_FLAGS_OFFSET]
        ; str X(callee), [x9, REQUEST_CALLEE_OFFSET]
        ; movz x11, VALUE_UNDEFINED as u32
    );
    match receiver {
        Some(receiver) => {
            dynasm!(ops ; .arch aarch64 ; str X(receiver), [x9, REQUEST_RECEIVER_OFFSET])
        }
        None => dynasm!(ops ; .arch aarch64 ; str x11, [x9, REQUEST_RECEIVER_OFFSET]),
    }
    match new_target {
        Some(new_target) => {
            dynasm!(ops ; .arch aarch64 ; str X(new_target), [x9, REQUEST_NEW_TARGET_OFFSET])
        }
        None => dynasm!(ops ; .arch aarch64 ; str x11, [x9, REQUEST_NEW_TARGET_OFFSET]),
    }
    // No restored registers; the completion returns through the native ABI.
    dynasm!(ops
        ; .arch aarch64
        ; orr x10, xzr, 0xffff_ffff_0000_0000
        ; str x10, [x9, REQUEST_REGISTER_SEED_OFFSET]
        ; str xzr, [x9, REQUEST_REGISTER_SEED_OFFSET + 8]
    );
    emit_enter_staged(ops, relocations, table, context);
}
