//! No-call Number fast path for committed generic binary operators.
//!
//! # Contents
//! - [`emit`] completes two Int32 operands in general registers, else decodes
//!   two tagged Numbers into v30/v31, applies one IEEE double operation, and
//!   boxes the result or selects a Boolean immediate.
//!
//! # Invariants
//! - No allocation, VM transition, deopt or user code occurs in the probe.
//!   A cell, immediate, `%` or `**` misses to the committed cold sibling.
//! - Both inputs are fully read before either output is defined, so outputs
//!   may share registers with inputs.
//! - The Int32 path defers overflow, a zero product and division to the
//!   double path, so it only produces results the double path would box to
//!   the same Int32.
//! - JavaScript Number arithmetic is IEEE double arithmetic, so converting an
//!   Int32 operand exactly and boxing through the ordinary `BoxNumber`
//!   canonicalization yields the interpreter's value.
//! - Relational conditions are false for an unordered (NaN) comparison.
//! - x15/x16/x17 and v30/v31 lie outside the scalar allocation file.
//!
//! # See also
//! - `machine::committed_probe` — cold call, catch edge and SSA result join.
//! - `otter_vm::BinaryOperator` — the committed operator family.

use super::{DOUBLE_OFFSET_HI16, NUMBER_TAG_HI16, emit_box_number};
use crate::template::arm64::values::emit_load_u64;
use dynasmrt::aarch64::Assembler;
use dynasmrt::{DynamicLabel, DynasmApi, DynasmLabelApi, dynasm};
use otter_vm::{Value, native_abi::BinaryOperator};

pub(super) fn emit(
    ops: &mut Assembler,
    operator: BinaryOperator,
    inputs: [u8; 2],
    outputs: [u8; 2],
) {
    let [left, right] = inputs;
    let [result, hit] = outputs;
    let miss = ops.new_dynamic_label();
    let done = ops.new_dynamic_label();
    let double = ops.new_dynamic_label();
    integer(ops, operator, inputs, outputs, double, done);
    dynasm!(ops ; .arch aarch64 ; =>double);
    decode(ops, left, 30, miss);
    decode(ops, right, 31, miss);
    match operator {
        BinaryOperator::Add | BinaryOperator::Sub | BinaryOperator::Mul | BinaryOperator::Div => {
            match operator {
                BinaryOperator::Add => dynasm!(ops ; .arch aarch64 ; fadd d30, d30, d31),
                BinaryOperator::Sub => dynasm!(ops ; .arch aarch64 ; fsub d30, d30, d31),
                BinaryOperator::Mul => dynasm!(ops ; .arch aarch64 ; fmul d30, d30, d31),
                _ => dynasm!(ops ; .arch aarch64 ; fdiv d30, d30, d31),
            }
            emit_box_number(ops, 30, result);
        }
        BinaryOperator::LessThan
        | BinaryOperator::LessEq
        | BinaryOperator::GreaterThan
        | BinaryOperator::GreaterEq => {
            let truthy = ops.new_dynamic_label();
            let chosen = ops.new_dynamic_label();
            dynasm!(ops ; .arch aarch64 ; fcmp d30, d31);
            match operator {
                BinaryOperator::LessThan => dynasm!(ops ; .arch aarch64 ; b.mi =>truthy),
                BinaryOperator::LessEq => dynasm!(ops ; .arch aarch64 ; b.ls =>truthy),
                BinaryOperator::GreaterThan => dynasm!(ops ; .arch aarch64 ; b.gt =>truthy),
                _ => dynasm!(ops ; .arch aarch64 ; b.ge =>truthy),
            }
            emit_load_u64(ops, result, Value::boolean(false).to_bits());
            dynasm!(ops ; .arch aarch64 ; b =>chosen ; =>truthy);
            emit_load_u64(ops, result, Value::boolean(true).to_bits());
            dynasm!(ops ; .arch aarch64 ; =>chosen);
        }
        BinaryOperator::Rem | BinaryOperator::Pow => {
            dynasm!(ops ; .arch aarch64 ; b =>miss);
        }
    }
    dynasm!(ops ; .arch aarch64 ; movz W(hit), 1 ; b =>done ; =>miss);
    emit_load_u64(ops, result, Value::undefined().to_bits());
    dynasm!(ops ; .arch aarch64 ; movz W(hit), 0 ; =>done);
}

/// Complete two Int32 operands in general registers, the dominant shape.
/// Overflow, a zero product (whose sign needs the double path), division, and
/// any non-Int32 operand branch to `double`.
fn integer(
    ops: &mut Assembler,
    operator: BinaryOperator,
    inputs: [u8; 2],
    outputs: [u8; 2],
    double: DynamicLabel,
    done: DynamicLabel,
) {
    let [left, right] = inputs;
    let [result, hit] = outputs;
    if matches!(
        operator,
        BinaryOperator::Div | BinaryOperator::Rem | BinaryOperator::Pow
    ) {
        return;
    }
    dynasm!(ops
        ; .arch aarch64
        ; movz x16, NUMBER_TAG_HI16, lsl #48
        ; and x15, X(left), x16
        ; cmp x15, x16
        ; b.ne =>double
        ; and x15, X(right), x16
        ; cmp x15, x16
        ; b.ne =>double
    );
    match operator {
        BinaryOperator::Add | BinaryOperator::Sub | BinaryOperator::Mul => {
            match operator {
                BinaryOperator::Add => {
                    dynasm!(ops ; .arch aarch64 ; adds w15, W(left), W(right) ; b.vs =>double);
                }
                BinaryOperator::Sub => {
                    dynasm!(ops ; .arch aarch64 ; subs w15, W(left), W(right) ; b.vs =>double);
                }
                _ => dynasm!(ops
                    ; .arch aarch64
                    ; smull x15, W(left), W(right)
                    ; cmp x15, w15, sxtw
                    ; b.ne =>double
                    ; cbz w15, =>double
                    ; mov w15, w15
                ),
            }
            // A 32-bit result register write already cleared the upper half.
            dynasm!(ops ; .arch aarch64 ; orr X(result), x15, x16);
        }
        _ => {
            let truthy = ops.new_dynamic_label();
            let chosen = ops.new_dynamic_label();
            dynasm!(ops ; .arch aarch64 ; cmp W(left), W(right));
            match operator {
                BinaryOperator::LessThan => dynasm!(ops ; .arch aarch64 ; b.lt =>truthy),
                BinaryOperator::LessEq => dynasm!(ops ; .arch aarch64 ; b.le =>truthy),
                BinaryOperator::GreaterThan => dynasm!(ops ; .arch aarch64 ; b.gt =>truthy),
                _ => dynasm!(ops ; .arch aarch64 ; b.ge =>truthy),
            }
            emit_load_u64(ops, result, Value::boolean(false).to_bits());
            dynasm!(ops ; .arch aarch64 ; b =>chosen ; =>truthy);
            emit_load_u64(ops, result, Value::boolean(true).to_bits());
            dynasm!(ops ; .arch aarch64 ; =>chosen);
        }
    }
    dynasm!(ops ; .arch aarch64 ; movz W(hit), 1 ; b =>done);
}

/// Decode the tagged Number in `source` into `D(target)`, or branch to `miss`.
fn decode(ops: &mut Assembler, source: u8, target: u8, miss: DynamicLabel) {
    let integer = ops.new_dynamic_label();
    let decoded = ops.new_dynamic_label();
    dynasm!(ops
        ; .arch aarch64
        ; movz x16, NUMBER_TAG_HI16, lsl #48
        ; and x15, X(source), x16
        ; cmp x15, x16
        ; b.eq =>integer
        ; cbz x15, =>miss
        ; movz x16, DOUBLE_OFFSET_HI16, lsl #48
        ; sub x15, X(source), x16
        ; fmov D(target), x15
        ; b =>decoded
        ; =>integer
        ; scvtf D(target), W(source)
        ; =>decoded
    );
}
