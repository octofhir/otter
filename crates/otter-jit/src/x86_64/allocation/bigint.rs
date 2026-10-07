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
//! - `crate::arm64::allocation` — the peer AArch64 carve.

use super::*;
use otter_vm::jit::{
    JIT_BIGINT_DIGIT_BYTE, JIT_BIGINT_NEGATIVE_BYTE, JIT_BIGINT_SHAPE_BYTE,
    JIT_BIGINT64_CELL_BYTES, JIT_YOUNG_BIGINT64_HEADER_WORD,
};

/// Replace the BigInt value in `Rq(value)` by its `i64`; any other value, or
/// one outside `i64`, jumps to `slow`. Clobbers `Rq(scratch)`.
pub(crate) fn emit_unbox_bigint64(ops: &mut Assembler, value: u8, scratch: u8, slow: DynamicLabel) {
    let zero = ops.new_dynamic_label();
    let positive = ops.new_dynamic_label();
    let done = ops.new_dynamic_label();
    dynasm!(ops
        ; .arch x64
        ; test Rq(value), Rq(value)
        ; jz =>slow
        ; mov Rq(scratch), QWORD otter_vm::value::tag::NOT_CELL_MASK as i64
        ; test Rq(value), Rq(scratch)
        ; jnz =>slow
        ; cmp BYTE [Rq(value)], otter_vm::bigint::BIG_INT_BODY_TYPE_TAG as i8
        ; jne =>slow
        ; mov Rd(scratch), [Rq(value) + JIT_BIGINT_SHAPE_BYTE as i32]
        ; test Rd(scratch), Rd(scratch)
        ; jz =>zero
        ; cmp Rd(scratch), 1
        ; jne =>slow
        ; movzx Rd(scratch), BYTE [Rq(value) + JIT_BIGINT_NEGATIVE_BYTE as i32]
        ; mov Rq(value), [Rq(value) + JIT_BIGINT_DIGIT_BYTE as i32]
        ; test Rd(scratch), Rd(scratch)
        ; jz =>positive
        // -|d| fits exactly when it comes out negative (|d| <= 2^63).
        ; neg Rq(value)
        ; jns =>slow
        ; jmp =>done
        ; =>positive
        ; test Rq(value), Rq(value)
        ; js =>slow
        ; jmp =>done
        ; =>zero
        ; xor Rd(value), Rd(value)
        ; =>done
    );
}

/// Carve the BigInt of the `i64` in `Rq(value)` into `Rq(regs.candidate)`.
/// A full buffer jumps to `slow` before any write. Clobbers the LAB
/// registers and `Rq(tmp)`; `Rq(value)` survives.
pub(crate) fn emit_bigint64(
    ops: &mut Assembler,
    context: u8,
    value: u8,
    tmp: u8,
    regs: LabRegisters,
    slow: DynamicLabel,
) {
    dynasm!(ops ; .arch x64 ; mov Rq(regs.size), JIT_BIGINT64_CELL_BYTES as i32);
    emit_bump_probe(ops, context, regs, slow);
    dynasm!(ops
        ; .arch x64
        ; mov Rq(regs.scratch), QWORD JIT_YOUNG_BIGINT64_HEADER_WORD as i64
        ; mov [Rq(regs.candidate)], Rq(regs.scratch)
        // Shape word: digit count (0 for zero, else 1), sign byte at bit 32.
        ; xor Rd(regs.scratch), Rd(regs.scratch)
        ; test Rq(value), Rq(value)
        ; setne Rb(regs.scratch)
        ; mov Rq(tmp), Rq(value)
        ; shr Rq(tmp), 63
        ; shl Rq(tmp), 32
        ; or Rq(regs.scratch), Rq(tmp)
        ; mov [Rq(regs.candidate) + JIT_BIGINT_SHAPE_BYTE as i32], Rq(regs.scratch)
        // |value|; `i64::MIN` stays 2^63 as an unsigned digit.
        ; mov Rq(tmp), Rq(value)
        ; neg Rq(tmp)
        ; cmovs Rq(tmp), Rq(value)
        ; mov [Rq(regs.candidate) + JIT_BIGINT_DIGIT_BYTE as i32], Rq(tmp)
    );
    emit_publish(ops, context, otter_vm::bigint::BIG_INT_BODY_TYPE_TAG, regs);
}
