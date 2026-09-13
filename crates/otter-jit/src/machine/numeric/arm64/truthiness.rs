//! Allocation-driven tagged truthiness without calls or managed temporaries.
//!
//! # Contents
//! - Immediate Number/Boolean/nullish decisions and non-primitive cell hits.
//!
//! # Invariants
//! - x15/x16 are reserved emitter scratch registers; outputs are defined only
//!   after the input's final read and may therefore reuse its allocation.
//! - Primitive cells and native functions take the explicit leaf sibling;
//!   string length, BigInt zero and HTMLDDA retain canonical VM semantics.
//! - The VM purifies NaN payloads before boxing. Cage loads read only body tags.
//!
//! # See also
//! - `machine::truthiness` — explicit CFG and result joins before allocation.

#![allow(clippy::useless_conversion)]

use super::emit_load_symbolic_u64;
use crate::artifact::relocation::{RelocationCapture, RelocationTarget};
use crate::template::arm64::values::{CellTest, emit_cell_test, emit_load_u64};
use dynasmrt::aarch64::Assembler;
use dynasmrt::{DynasmApi, DynasmLabelApi, dynasm};
use otter_vm::{JitCompileSnapshot, Value};

pub(super) fn emit(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    view: &JitCompileSnapshot,
    source: u8,
    result: u8,
    hit: u8,
) {
    let int = ops.new_dynamic_label();
    let double = ops.new_dynamic_label();
    let truthy = ops.new_dynamic_label();
    let falsy = ops.new_dynamic_label();
    let miss = ops.new_dynamic_label();
    let done = ops.new_dynamic_label();
    emit_load_u64(ops, 16, otter_vm::value::tag::NUMBER_TAG);
    dynasm!(ops ; .arch aarch64
        ; and x15, X(source), x16 ; cmp x15, x16 ; b.eq =>int
        ; cbnz x15, =>double
    );
    for (value, target) in [
        (Value::boolean(true), truthy),
        (Value::boolean(false), falsy),
        (Value::null(), falsy),
        (Value::undefined(), falsy),
    ] {
        emit_load_u64(ops, 16, value.to_bits());
        dynasm!(ops ; .arch aarch64 ; cmp X(source), x16 ; b.eq =>target);
    }
    emit_cell_test(ops, source, 16, CellTest::IsNotCell, miss);
    if view.cage_base == 0 {
        dynasm!(ops ; .arch aarch64 ; b =>miss);
    } else {
        emit_load_symbolic_u64(
            ops,
            relocations,
            16,
            view.cage_base as u64,
            RelocationTarget::GcCageBase,
        );
        dynasm!(ops ; .arch aarch64 ; mov w15, W(source) ; add x16, x16, x15 ; ldrb w15, [x16]);
        for tag in view
            .primitive_cell_type_tags
            .into_iter()
            .chain([view.collection_layout.native_function_type_tag])
        {
            dynasm!(ops ; .arch aarch64 ; cmp w15, u32::from(tag) ; b.eq =>miss);
        }
        dynasm!(ops ; .arch aarch64 ; b =>truthy);
    }
    dynasm!(ops ; .arch aarch64 ; =>int ; cbz W(source), =>falsy ; b =>truthy ; =>double);
    emit_load_u64(ops, 16, otter_vm::value::tag::DOUBLE_ENCODE_OFFSET);
    dynasm!(ops ; .arch aarch64 ; sub x15, X(source), x16 ; cbz x15, =>falsy);
    emit_load_u64(ops, 16, (-0.0_f64).to_bits());
    dynasm!(ops ; .arch aarch64 ; cmp x15, x16 ; b.eq =>falsy);
    emit_load_u64(ops, 16, otter_vm::value::tag::CANONICAL_NAN);
    dynasm!(ops ; .arch aarch64
        ; cmp x15, x16 ; b.eq =>falsy
        ; =>truthy ; mov W(result), #1 ; mov W(hit), #1 ; b =>done
        ; =>falsy ; mov W(result), wzr ; mov W(hit), #1 ; b =>done
        ; =>miss ; mov W(result), wzr ; mov W(hit), wzr
        ; =>done
    );
}
