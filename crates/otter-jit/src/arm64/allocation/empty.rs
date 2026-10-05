//! Empty literal initialization on the shared linear allocation buffer.
//!
//! # Contents
//! - VM-owned object/array layouts and one parameterized fit emitter.
//!
//! # Invariants
//! Realm and fit misses precede all writes. The candidate is a declared
//! temporary, never an early result register. Every payload word is initialized
//! before publication; no helper call or collection occurs on a fit.
//!
//! # See also
//! `otter_vm::jit::JitLiteralAllocationPlans` and `graph::arm64::allocation`.

use super::{emit_bump_probe, emit_publish};
use crate::allocation::{EmptyLiteralLayout, LabRegisters};
use crate::entry::{THREAD_OFFSET, VALUE_UNDEFINED, VM_THREAD_ACTIVE_REALM_CELL_OFFSET};
use crate::template::arm64::values::emit_load_u64;
use dynasmrt::{DynamicLabel, DynasmApi, DynasmLabelApi, aarch64::Assembler, dynasm};

/// Carve the exact source-realm literal, leaving its value in candidate.
/// Clobbers only the declared recipe registers and reserved x16/x17.
pub(crate) fn emit_empty_literal(
    ops: &mut Assembler,
    context: u8,
    realm_id: u32,
    layout: EmptyLiteralLayout,
    regs: LabRegisters,
    slow: DynamicLabel,
) {
    let scratch = regs.scratch;
    if matches!(layout, EmptyLiteralLayout::Array(_)) && realm_id != 0 {
        dynasm!(ops ; .arch aarch64 ; b =>slow);
        return;
    }
    emit_source_realm_guard(ops, context, realm_id, scratch, slow);
    let cell_bytes = layout.bytes();
    let header_word = layout.header();
    emit_load_u64(ops, regs.size, u64::from(cell_bytes));
    emit_bump_probe(ops, context, regs, slow);
    emit_initialize_empty(ops, layout, regs);
    emit_publish(ops, context, header_word as u8, regs);
}

pub(super) fn emit_source_realm_guard(
    ops: &mut Assembler,
    context: u8,
    realm_id: u32,
    scratch: u8,
    slow: DynamicLabel,
) {
    dynasm!(ops
        ; .arch aarch64
        ; ldr X(scratch), [X(context), THREAD_OFFSET]
        ; ldr X(scratch), [X(scratch), VM_THREAD_ACTIVE_REALM_CELL_OFFSET]
        ; ldr W(scratch), [X(scratch)]
    );
    emit_load_u64(ops, 16, u64::from(realm_id));
    dynasm!(ops ; .arch aarch64 ; cmp W(scratch), w16 ; b.ne =>slow);
}

#[cfg(test)]
#[path = "empty_tests.rs"]
mod tests;

/// Initialize one already reserved cell; no guard, call, probe or publication.
pub(super) fn emit_initialize_empty(
    ops: &mut Assembler,
    layout: EmptyLiteralLayout,
    regs: LabRegisters,
) {
    let candidate = regs.candidate;
    let cell_bytes = layout.bytes();
    let header_word = layout.header();
    emit_load_u64(ops, 16, header_word);
    dynasm!(ops ; .arch aarch64 ; str x16, [X(candidate)]);
    match layout {
        EmptyLiteralLayout::Object(plan) => {
            for byte in (8..plan.initial_value_bytes[0]).step_by(8) {
                dynasm!(ops ; .arch aarch64 ; str xzr, [X(candidate), byte]);
            }
            emit_load_u64(ops, 16, u64::from(plan.shape));
            dynasm!(ops ; .arch aarch64 ; str w16, [X(candidate), plan.shape_byte]);
            emit_load_u64(ops, 16, VALUE_UNDEFINED);
            for byte in plan.initial_value_bytes {
                dynasm!(ops ; .arch aarch64 ; str x16, [X(candidate), byte]);
            }
        }
        EmptyLiteralLayout::Array(_) => {
            for byte in (8..cell_bytes).step_by(8) {
                dynasm!(ops ; .arch aarch64 ; str xzr, [X(candidate), byte]);
            }
        }
    }
}
