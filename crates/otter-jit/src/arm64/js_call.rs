//! Generated JavaScript calls on AArch64.
//!
//! # Contents
//! - [`emit_push_arguments`] / [`emit_pop_arguments`] — the actual span on
//!   the caller's stack.
//! - [`emit_call`] — the call ABI registers and the call itself: the current
//!   generation of a proven bytecode target through its `FunctionEntryCell`,
//!   a proved NativeFunction through its selected Host convention, or the
//!   generic entry that classifies any other callee.
//! - [`emit_staged_call`] / [`emit_enter_staged`] — a call whose span, or
//!   whole request, a runtime staging entry wrote.
//! - [`emit_tail_branch`] — the entry branch of a proper tail call.
//! - [`emit_push_forwarded`] / [`emit_pop_forwarded`] — the span of a
//!   forwarded call: the activation's own actual arguments, its mapped
//!   formals read from where they live now.
//!
//! # Invariants
//! - The span is pushed before the call ABI registers are set and popped
//!   after the completion returns. It contains only actual arguments;
//!   the callee owns every missing formal's `undefined` value.
//! - Callee activation, receiver binding, constructor completion and callee
//!   deoptimization belong to the callee. A caller consumes only `Success`,
//!   `Throw` (exception in `x0`) or `Fatal` (error parked in the context).
//! - Emitters return the exact offset immediately after BLR, before any cleanup.
//! - Emitters clobber `x0`–`x17` and preserve every callee-saved register.
//!
//! # See also
//! - [`crate::call_linkage`] — the architecture-neutral call contract.
//! - [`super::activation`] — the callee side of the same ABI.

use dynasmrt::{
    AssemblyOffset, DynamicLabel, DynasmApi, DynasmLabelApi, aarch64::Assembler, dynasm,
};
use otter_vm::native_abi as abi;

use crate::{
    artifact::relocation::{RelocationCapture, RelocationTarget},
    call_linkage::pushed_argument_bytes,
    entry::{
        NATIVE_STACK_LIMIT_OFFSET, PENDING_CALL_OFFSET, REQUEST_CALLEE_OFFSET,
        REQUEST_ENTRY_OFFSET, REQUEST_FLAGS_OFFSET, REQUEST_NEW_TARGET_OFFSET,
        REQUEST_RECEIVER_OFFSET, REQUEST_REGISTER_SEED_OFFSET, TransitionTable, Unsupported,
        VALUE_UNDEFINED,
    },
    template::arm64::values::emit_load_u64,
};

/// Prove a full tagged NativeFunction cell before reading its header.
/// Only x16 and flags are clobbered; the callee excludes reserved scratch.
/// x17 retains the original guarded-method callable on the Generic miss path.
pub(crate) fn emit_native_kind_guard(ops: &mut Assembler, value: u8, miss: DynamicLabel) {
    assert!(
        value != 16 && value != 17,
        "native callee cannot alias guard scratch"
    );
    emit_load_u64(ops, 16, otter_vm::value::tag::NOT_CELL_MASK);
    dynasm!(ops ; .arch aarch64
        ; tst X(value), x16 ; b.ne =>miss ; cbz X(value), =>miss
        ; ldrb w16, [X(value)]
        ; cmp w16, u32::from(otter_vm::native_function::NATIVE_FUNCTION_BODY_TYPE_TAG)
        ; b.ne =>miss
    );
}

/// Clear the sole canonical incoming construction ticket. Only x17 is
/// scratch; no call operand or the x16 target is touched.
pub(crate) fn emit_clear_construct_ticket(ops: &mut Assembler, context: u8) {
    dynasm!(ops ; .arch aarch64
        ; str xzr, [X(context), PENDING_CALL_OFFSET + abi::REQUEST_SUPER_ORIGIN_OFFSET]
        ; str wzr, [X(context), PENDING_CALL_OFFSET + abi::REQUEST_CONSTRUCT_LAYOUT_OFFSET]
        ; movz x17, VALUE_UNDEFINED as u32
        ; str x17, [X(context), PENDING_CALL_OFFSET + abi::REQUEST_CONSTRUCT_RECEIVER_OFFSET]
    );
}

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

/// `x16` = the bytes of the frame's actual span, rounded up to 16 bytes.
fn emit_forwarded_bytes(ops: &mut Assembler, frame: u8) {
    dynasm!(ops
        ; .arch aarch64
        ; ldr w16, [X(frame), abi::NATIVE_FRAME_ARGUMENT_COUNT_OFFSET]
    );
    dynasm!(ops
        ; .arch aarch64
        ; add w16, w16, 1
        ; and w16, w16, 0xffff_fffe
        ; lsl x16, x16, 3
    );
}

/// Reserve and fill the span of a forwarded call below `sp`: the actual
/// arguments of the activation whose record is in `frame`, each
/// `(argument index, binding)` actual
/// below the argument count replaced by the binding's current value; then
/// `x4` holds the argument count. A span past the stack limit branches to
/// `overflow` before `sp` moves. `old_sp` keeps the stack pointer the span
/// moved, `source` is clobbered, and so are `x16`/`x17`.
pub(crate) fn emit_push_forwarded(
    ops: &mut Assembler,
    frame: u8,
    [old_sp, source]: [u8; 2],
    overflow: DynamicLabel,
    bindings: &[(u16, ForwardedBinding)],
) {
    emit_forwarded_bytes(ops, frame);
    let probe = ops.new_dynamic_label();
    let probed = ops.new_dynamic_label();
    dynasm!(ops
        ; .arch aarch64
        ; mov x17, sp
        ; mov X(old_sp), x17
        ; sub x16, X(old_sp), x16
        ; ldr x17, [x20, NATIVE_STACK_LIMIT_OFFSET]
        ; cmp x16, x17
        ; b.lo =>overflow
        // Touch each page from the old stack pointer down before `sp`
        // passes it.
        ; mov x17, X(old_sp)
        ; =>probe
        ; sub x17, x17, 1, lsl 12
        ; cmp x17, x16
        ; b.ls =>probed
        ; str xzr, [x17]
        ; b =>probe
        ; =>probed
        ; mov sp, x16
    );
    let copy = ops.new_dynamic_label();
    let copied = ops.new_dynamic_label();
    dynasm!(ops
        ; .arch aarch64
        ; ldr X(source), [X(frame), abi::NATIVE_FRAME_ACTUALS_OFFSET]
        ; ldr w17, [X(frame), abi::NATIVE_FRAME_ARGUMENT_COUNT_OFFSET]
        ; =>copy
        ; cbz w17, =>copied
        ; sub w17, w17, 1
        ; ldr x16, [X(source), x17, lsl #3]
        ; str x16, [sp, x17, lsl #3]
        ; b =>copy
        ; =>copied
    );
    for &(index, binding) in bindings {
        let skip = ops.new_dynamic_label();
        emit_load_u64(ops, 16, u64::from(index));
        dynasm!(ops
            ; .arch aarch64
            ; ldr w17, [X(frame), abi::NATIVE_FRAME_ARGUMENT_COUNT_OFFSET]
            ; cmp w17, w16
            ; b.ls =>skip
        );
        let value = match binding {
            ForwardedBinding::Register(register) => register,
            ForwardedBinding::Load { base, offset } => {
                emit_load_u64(ops, 17, u64::from(offset));
                dynasm!(ops ; .arch aarch64 ; ldr x16, [X(base), x17]);
                16
            }
            ForwardedBinding::ContextSlot {
                base,
                offset,
                slot_byte,
            } => {
                emit_load_u64(ops, 17, u64::from(offset));
                dynasm!(ops ; .arch aarch64 ; ldr x16, [X(base), x17]);
                emit_load_u64(ops, 17, u64::from(slot_byte));
                dynasm!(ops ; .arch aarch64 ; ldr x16, [x16, x17]);
                16
            }
            ForwardedBinding::Immediate(bits) => {
                emit_load_u64(ops, 16, bits);
                16
            }
        };
        emit_load_u64(ops, 17, u64::from(index));
        dynasm!(ops
            ; .arch aarch64
            ; str X(value), [sp, x17, lsl #3]
            ; =>skip
        );
    }
    dynasm!(ops
        ; .arch aarch64
        ; ldr w4, [X(frame), abi::NATIVE_FRAME_ARGUMENT_COUNT_OFFSET]
    );
}

/// Release a span [`emit_push_forwarded`] reserved, sized again from the
/// unchanged argument count of `frame`. Clobbers `x16`/`x17`.
pub(crate) fn emit_pop_forwarded(ops: &mut Assembler, frame: u8) {
    emit_forwarded_bytes(ops, frame);
    dynasm!(ops ; .arch aarch64 ; add sp, sp, x16);
}

pub(crate) use crate::call_linkage::CallTarget;
use crate::call_linkage::ForwardedBinding;

/// Set the call ABI registers from `callee`, `receiver` (`undefined` when
/// absent) and `new_target` (present exactly for `[[Construct]]`), then call
/// `target` with `count` actuals at `sp`, or with the count the caller
/// already put in `x4` when `count` is `None`. Returns with the completion
/// in `x0`/`x1`.
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
    count: Option<u32>,
    target: CallTarget,
) -> AssemblyOffset {
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
        None => {
            emit_clear_construct_ticket(ops, context);
            dynasm!(ops ; .arch aarch64 ; movz x3, VALUE_UNDEFINED as u32);
        }
    }
    if let Some(count) = count {
        emit_load_u64(ops, 4, u64::from(count));
    }
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
        CallTarget::Generic | CallTarget::Native => {
            let stub = match target {
                CallTarget::Native => abi::STUB_JIT_CALL_NATIVE,
                _ => abi::STUB_JIT_CALL_GENERIC,
            };
            let start = ops.offset().0;
            emit_load_u64(ops, 16, table.entry(stub));
            relocations.record_mov_wide(
                start,
                ops.offset().0,
                16,
                RelocationTarget::runtime_stub(stub),
            );
            dynasm!(ops ; .arch aarch64 ; blr x16);
        }
    }
    ops.offset()
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
        CallTarget::Generic | CallTarget::Native => {
            let stub = match target {
                CallTarget::Native => abi::STUB_JIT_CALL_NATIVE,
                _ => abi::STUB_JIT_CALL_GENERIC,
            };
            let start = ops.offset().0;
            emit_load_u64(ops, 16, table.entry(stub));
            relocations.record_mov_wide(
                start,
                ops.offset().0,
                16,
                RelocationTarget::runtime_stub(stub),
            );
            dynasm!(ops ; .arch aarch64 ; br x16);
        }
    }
}

/// Publish the generated caller's genuine return coordinate and enter the
/// trampoline with the complete staged request. Existing nonzero, tier-transfer
/// and tail-call associations remain owned by their original source extent.
/// Returns the completion in `x0`/`x1`.
pub(crate) fn emit_enter_staged(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    table: &TransitionTable,
    context: u8,
) -> AssemblyOffset {
    let return_to_caller = ops.new_dynamic_label();
    let origin_ready = ops.new_dynamic_label();
    let no_generated_caller = ops.new_dynamic_label();
    dynasm!(ops ; .arch aarch64
        ; mov x0, X(context)
        ; add x16, x0, PENDING_CALL_OFFSET
        ; ldr x17, [x16, abi::REQUEST_CALLER_RETURN_PC_OFFSET]
        ; cbnz x17, =>origin_ready
        ; ldrb w17, [x16, REQUEST_FLAGS_OFFSET]
        ; tst w17, u32::from(abi::NativeFrameFlags::TIER_ENTRY | abi::NativeFrameFlags::TAIL_CALL)
        ; b.ne =>origin_ready
        ; ldr x17, [x0, crate::entry::NATIVE_FRAME_OFFSET]
        ; str x17, [x16, abi::REQUEST_CALLER_OFFSET]
        ; cbz x17, =>no_generated_caller
        ; ldr w17, [x17, abi::NATIVE_FRAME_CODE_OBJECT_ID_OFFSET]
        ; cbz w17, =>no_generated_caller
        ; adr x17, =>return_to_caller
        ; str x17, [x16, abi::REQUEST_CALLER_RETURN_PC_OFFSET]
        ; b =>origin_ready
        ; =>no_generated_caller
        ; str xzr, [x16, abi::REQUEST_CALLER_RETURN_PC_OFFSET]
        ; =>origin_ready
    );
    let start = ops.offset().0;
    emit_load_u64(ops, 16, table.entry(abi::STUB_JIT_CALL));
    relocations.record_mov_wide(
        start,
        ops.offset().0,
        16,
        RelocationTarget::runtime_stub(abi::STUB_JIT_CALL),
    );
    dynasm!(ops ; .arch aarch64 ; blr x16 ; =>return_to_caller);
    ops.offset()
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
) -> AssemblyOffset {
    emit_clear_construct_ticket(ops, context);
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
    emit_enter_staged(ops, relocations, table, context)
}
