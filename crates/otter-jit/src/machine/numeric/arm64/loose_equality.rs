//! No-call proofs for tagged ECMAScript loose equality.
//!
//! # Contents
//! - Identity, homogeneous Number, nullish-pair and ordinary object-pair hits.
//! - A pure miss for coercion, different primitive cells and uncertain values.
//!
//! # Invariants
//! - No allocation, VM transition, deopt or user code occurs inside the probe.
//! - Same-pointer objects never coerce, including revoked Proxies and HTMLDDA.
//!   A mixed nullish/object pair stays canonical for HTMLDDA handling.
//! - NaN is purified before boxing; signed-zero Number pairs compare equal.
//! - x15/x16 are reserved scratch; outputs follow the last input read.
//!
//! # See also
//! - `machine::committed_probe` — cold call, catch edge and SSA result join.

#![allow(clippy::useless_conversion)]

use super::emit_load_symbolic_u64;
use crate::artifact::relocation::{RelocationCapture, RelocationTarget};
use crate::template::arm64::values::{CellTest, emit_cell_test, emit_load_u64};
use dynasmrt::aarch64::Assembler;
use dynasmrt::{DynasmApi, DynasmLabelApi, dynasm};
use otter_vm::{JitCompileSnapshot, Value, value::tag};

pub(super) fn emit(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    view: &JitCompileSnapshot,
    inputs: [u8; 2],
    outputs: [u8; 2],
    equal: bool,
) {
    let [left, right] = inputs;
    let [result, hit] = outputs;
    let identical = ops.new_dynamic_label();
    let integer = ops.new_dynamic_label();
    let double = ops.new_dynamic_label();
    let nullish = ops.new_dynamic_label();
    let yes = ops.new_dynamic_label();
    let no = ops.new_dynamic_label();
    let miss = ops.new_dynamic_label();
    let done = ops.new_dynamic_label();
    dynasm!(ops ; .arch aarch64 ; cmp X(left), X(right) ; b.eq =>identical);
    emit_load_u64(ops, 16, tag::NUMBER_TAG);
    dynasm!(ops ; .arch aarch64
        ; and x15, X(left), x16 ; cmp x15, x16 ; b.eq =>integer
        ; cbnz x15, =>double
    );
    for value in [Value::null(), Value::undefined()] {
        emit_load_u64(ops, 16, value.to_bits());
        dynasm!(ops ; .arch aarch64 ; cmp X(left), x16 ; b.eq =>nullish);
    }
    emit_cell_test(ops, left, 16, CellTest::IsNotCell, miss);
    emit_cell_test(ops, right, 16, CellTest::IsNotCell, miss);
    if view.cage_base == 0 {
        dynasm!(ops ; .arch aarch64 ; b =>miss);
    } else {
        for source in [left, right] {
            emit_load_symbolic_u64(
                ops,
                relocations,
                16,
                view.cage_base as u64,
                RelocationTarget::GcCageBase,
            );
            dynasm!(ops ; .arch aarch64
                ; mov w15, W(source) ; add x16, x16, x15 ; ldrb w15, [x16]
            );
            for primitive_tag in view.primitive_cell_type_tags {
                dynasm!(ops ; .arch aarch64 ; cmp w15, u32::from(primitive_tag) ; b.eq =>miss);
            }
        }
        // Both are non-primitive JS cells and their identities already differ.
        dynasm!(ops ; .arch aarch64 ; b =>no);
    }
    dynasm!(ops ; .arch aarch64 ; =>integer);
    emit_load_u64(ops, 16, tag::NUMBER_TAG);
    dynasm!(ops ; .arch aarch64
        ; and x15, X(right), x16 ; cmp x15, x16 ; b.eq =>no ; b =>miss
        ; =>double
        ; and x15, X(right), x16 ; cbz x15, =>miss ; cmp x15, x16 ; b.eq =>miss
    );
    // Distinct boxed doubles differ except the two signed-zero encodings.
    emit_load_u64(ops, 16, tag::DOUBLE_ENCODE_OFFSET);
    dynasm!(ops ; .arch aarch64
        ; and x15, X(left), #0x7fffffffffffffff ; cmp x15, x16 ; b.ne =>no
        ; and x15, X(right), #0x7fffffffffffffff ; cmp x15, x16 ; b.eq =>yes ; b =>no
        ; =>nullish
    );
    for value in [Value::null(), Value::undefined()] {
        emit_load_u64(ops, 16, value.to_bits());
        dynasm!(ops ; .arch aarch64 ; cmp X(right), x16 ; b.eq =>yes);
    }
    dynasm!(ops ; .arch aarch64 ; b =>miss ; =>identical);
    emit_load_u64(
        ops,
        16,
        Value::number(otter_vm::NumberValue::Double(f64::NAN)).to_bits(),
    );
    dynasm!(ops ; .arch aarch64 ; cmp X(left), x16 ; b.eq =>no ; =>yes);
    emit_load_u64(ops, result, Value::boolean(equal).to_bits());
    dynasm!(ops ; .arch aarch64 ; mov W(hit), #1 ; b =>done ; =>no);
    emit_load_u64(ops, result, Value::boolean(!equal).to_bits());
    dynasm!(ops ; .arch aarch64 ; mov W(hit), #1 ; b =>done ; =>miss);
    emit_load_u64(ops, result, Value::boolean(false).to_bits());
    dynasm!(ops ; .arch aarch64 ; mov W(hit), wzr ; =>done);
}
