//! No-call x86-64 Number fast path for committed generic binary operators.
//!
//! # Contents
//! - [`emit`] completes two Int32 operands in general registers, else decodes
//!   two tagged Numbers into xmm14/xmm15, applies one IEEE double operation,
//!   and boxes the result or selects a Boolean immediate.
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
//! - `r11` and `xmm15` are reserved scratch; `xmm14` is the declared
//!   `NumberProbe` clobber.
//!
//! # See also
//! - `machine::committed_probe` — cold call, catch edge and SSA result join.
//! - `otter_vm::BinaryOperator` — the committed operator family.

use super::*;
use otter_vm::native_abi::BinaryOperator;

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
    dynasm!(ops ; .arch x64 ; =>double);
    decode_number(ops, left, 14, miss);
    decode_right(ops, right, miss);
    match operator {
        BinaryOperator::Add | BinaryOperator::Sub | BinaryOperator::Mul | BinaryOperator::Div => {
            match operator {
                BinaryOperator::Add => dynasm!(ops ; .arch x64 ; addsd xmm14, xmm15),
                BinaryOperator::Sub => dynasm!(ops ; .arch x64 ; subsd xmm14, xmm15),
                BinaryOperator::Mul => dynasm!(ops ; .arch x64 ; mulsd xmm14, xmm15),
                _ => dynasm!(ops ; .arch x64 ; divsd xmm14, xmm15),
            }
            box_number(ops, 14, result);
        }
        BinaryOperator::LessThan
        | BinaryOperator::LessEq
        | BinaryOperator::GreaterThan
        | BinaryOperator::GreaterEq => {
            let truthy = ops.new_dynamic_label();
            let chosen = ops.new_dynamic_label();
            // `ucomisd a, b` then `ja`/`jae` is false for an unordered pair.
            match operator {
                BinaryOperator::LessThan => {
                    dynasm!(ops ; .arch x64 ; ucomisd xmm15, xmm14 ; ja =>truthy);
                }
                BinaryOperator::LessEq => {
                    dynasm!(ops ; .arch x64 ; ucomisd xmm15, xmm14 ; jae =>truthy);
                }
                BinaryOperator::GreaterThan => {
                    dynasm!(ops ; .arch x64 ; ucomisd xmm14, xmm15 ; ja =>truthy);
                }
                _ => dynasm!(ops ; .arch x64 ; ucomisd xmm14, xmm15 ; jae =>truthy),
            }
            load64(ops, result, Value::boolean(false).to_bits());
            dynasm!(ops ; .arch x64 ; jmp =>chosen ; =>truthy);
            load64(ops, result, Value::boolean(true).to_bits());
            dynasm!(ops ; .arch x64 ; =>chosen);
        }
        BinaryOperator::Rem | BinaryOperator::Pow => {
            dynasm!(ops ; .arch x64 ; jmp =>miss);
        }
    }
    dynasm!(ops ; .arch x64 ; mov Rd(hit), 1 ; jmp =>done ; =>miss);
    load64(ops, result, Value::undefined().to_bits());
    dynasm!(ops ; .arch x64 ; xor Rd(hit), Rd(hit) ; =>done);
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
    for source in [left, right] {
        dynasm!(ops
            ; .arch x64
            ; mov r11, Rq(source)
            ; shr r11, 48
            ; cmp r11w, NUMBER_TAG_HI16 as i16
            ; jne =>double
        );
    }
    match operator {
        BinaryOperator::Add | BinaryOperator::Sub | BinaryOperator::Mul => {
            dynasm!(ops ; .arch x64 ; mov r11d, Rd(left));
            match operator {
                BinaryOperator::Add => dynasm!(ops ; .arch x64 ; add r11d, Rd(right) ; jo =>double),
                BinaryOperator::Sub => dynasm!(ops ; .arch x64 ; sub r11d, Rd(right) ; jo =>double),
                _ => dynasm!(ops
                    ; .arch x64
                    ; imul r11d, Rd(right)
                    ; jo =>double
                    ; test r11d, r11d
                    ; jz =>double
                ),
            }
            box_int32(ops, 11, result);
        }
        _ => {
            let truthy = ops.new_dynamic_label();
            let chosen = ops.new_dynamic_label();
            dynasm!(ops ; .arch x64 ; cmp Rd(left), Rd(right));
            match operator {
                BinaryOperator::LessThan => dynasm!(ops ; .arch x64 ; jl =>truthy),
                BinaryOperator::LessEq => dynasm!(ops ; .arch x64 ; jle =>truthy),
                BinaryOperator::GreaterThan => dynasm!(ops ; .arch x64 ; jg =>truthy),
                _ => dynasm!(ops ; .arch x64 ; jge =>truthy),
            }
            load64(ops, result, Value::boolean(false).to_bits());
            dynasm!(ops ; .arch x64 ; jmp =>chosen ; =>truthy);
            load64(ops, result, Value::boolean(true).to_bits());
            dynasm!(ops ; .arch x64 ; =>chosen);
        }
    }
    dynasm!(ops ; .arch x64 ; mov Rd(hit), 1 ; jmp =>done);
}

/// Decode the tagged Number in `source` into xmm15, or branch to `miss`.
/// `decode_number` itself borrows xmm15, so the right operand is decoded
/// here through `r11` alone.
fn decode_right(ops: &mut Assembler, source: u8, miss: DynamicLabel) {
    let double = ops.new_dynamic_label();
    let done = ops.new_dynamic_label();
    dynasm!(ops
        ; .arch x64
        ; mov r11, Rq(source)
        ; shr r11, 48
        ; cmp r11w, NUMBER_TAG_HI16 as i16
        ; jne =>double
        ; cvtsi2sd xmm15, Rd(source)
        ; jmp =>done
        ; =>double
        ; test r11w, NUMBER_TAG_HI16 as i16
        ; jz =>miss
    );
    load64(ops, 11, DOUBLE_OFFSET.wrapping_neg());
    dynasm!(ops
        ; .arch x64
        ; add r11, Rq(source)
        ; movq xmm15, r11
        ; =>done
    );
}
