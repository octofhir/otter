//! Generated BigInt arithmetic over one-digit operands (V8's BigInt64
//! fast path).
//!
//! # Contents
//! - [`emit_unbox_bigint64`]: a BigInt whose value fits `i64` read into a
//!   register.
//! - [`emit_bigint64`]: a signed 64-bit result carved as a one-digit BigInt.
//!
//! # Invariants
//! - Every miss precedes every effect: an operand outside `i64`, a
//!   non-BigInt or a full buffer branches with the inputs untouched.
//! - The carve writes every word of the cell before publishing it, so the
//!   body is canonical: length 0 for zero, a clear sign for non-negatives.
//!
//! # See also
//! - `otter_vm::bigint::gc_body` owns the body layout.
//! - `otter_vm::bigint::ops` owns the general operators the miss reaches.

use super::*;
use otter_vm::jit::{
    JIT_BIGINT_DIGIT_BYTE, JIT_BIGINT_NEGATIVE_BYTE, JIT_BIGINT_SHAPE_BYTE,
    JIT_BIGINT64_CELL_BYTES, JIT_YOUNG_BIGINT64_HEADER_WORD,
};

/// Replace the BigInt value in `X(value)` by its `i64`; any other value, or
/// one outside `i64`, branches to `slow`. Clobbers `X(scratch)`.
pub(crate) fn emit_unbox_bigint64(ops: &mut Assembler, value: u8, scratch: u8, slow: DynamicLabel) {
    let zero = ops.new_dynamic_label();
    let positive = ops.new_dynamic_label();
    let done = ops.new_dynamic_label();
    dynasm!(ops ; .arch aarch64 ; cbz X(value), =>slow);
    emit_cell_test(ops, value, CellTest::IsNotCell, slow);
    dynasm!(ops
        ; .arch aarch64
        ; ldrb W(scratch), [X(value)]
        ; cmp WSP(scratch), u32::from(otter_vm::bigint::BIG_INT_BODY_TYPE_TAG)
        ; b.ne =>slow
        ; ldr W(scratch), [X(value), JIT_BIGINT_SHAPE_BYTE]
        ; cbz W(scratch), =>zero
        ; cmp WSP(scratch), 1
        ; b.ne =>slow
        ; ldrb W(scratch), [X(value), JIT_BIGINT_NEGATIVE_BYTE]
        ; ldr X(value), [X(value), JIT_BIGINT_DIGIT_BYTE]
        ; cbz W(scratch), =>positive
        // -|d| fits exactly when it comes out negative (|d| <= 2^63).
        ; neg X(value), X(value)
        ; tbz X(value), 63, =>slow
        ; b =>done
        ; =>positive
        ; tbnz X(value), 63, =>slow
        ; b =>done
        ; =>zero
        ; mov X(value), xzr
        ; =>done
    );
}

/// Carve the BigInt of the `i64` in `X(value)` into `X(regs.candidate)`.
/// A full buffer branches to `slow` before any write. Clobbers the LAB
/// registers and `X(tmp)`; `X(value)` survives.
pub(crate) fn emit_bigint64(
    ops: &mut Assembler,
    context: u8,
    value: u8,
    tmp: u8,
    regs: LabRegisters,
    slow: DynamicLabel,
) {
    emit_load_u64(ops, regs.size, u64::from(JIT_BIGINT64_CELL_BYTES));
    emit_bump_probe(ops, context, regs, slow);
    emit_load_u64(ops, regs.scratch, JIT_YOUNG_BIGINT64_HEADER_WORD);
    dynasm!(ops
        ; .arch aarch64
        ; str X(regs.scratch), [X(regs.candidate)]
        // Shape word: digit count (0 for zero, else 1), sign byte at bit 32.
        ; cmp XSP(value), 0
        ; cset W(regs.scratch), ne
        ; lsr X(tmp), X(value), 63
        ; orr X(regs.scratch), X(regs.scratch), X(tmp), lsl 32
        ; str X(regs.scratch), [X(regs.candidate), JIT_BIGINT_SHAPE_BYTE]
        ; cmp XSP(value), 0
        ; cneg X(tmp), X(value), mi
        ; str X(tmp), [X(regs.candidate), JIT_BIGINT_DIGIT_BYTE]
    );
    emit_publish(ops, context, otter_vm::bigint::BIG_INT_BODY_TYPE_TAG, regs);
}
