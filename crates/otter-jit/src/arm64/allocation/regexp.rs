//! Young birth of a regex literal from its baked image.
//!
//! # Contents
//! - [`emit_regexp_literal`] — the realm guard, one bump, the header and the
//!   image words, then publication.
//!
//! # Invariants
//! The image is the complete payload of a fresh body over the site's old
//! program cell, so the cell needs no barrier and nothing is read from the
//! heap. A realm or room miss precedes every write.
//!
//! # See also
//! `otter_vm::jit::JitRegExpLiteralPlan` owns the image.

use super::empty::emit_source_realm_guard;
use super::{emit_bump_probe, emit_publish};
use crate::allocation::LabRegisters;
use crate::template::arm64::values::emit_load_u64;
use dynasmrt::{DynamicLabel, DynasmApi, aarch64::Assembler, dynasm};
use otter_vm::jit::JitRegExpLiteralPlan;

pub(crate) fn emit_regexp_literal(
    ops: &mut Assembler,
    context: u8,
    realm_id: u32,
    plan: &JitRegExpLiteralPlan,
    regs: LabRegisters,
    slow: DynamicLabel,
) {
    emit_source_realm_guard(ops, context, realm_id, regs.scratch, slow);
    emit_load_u64(ops, regs.size, u64::from(plan.cell_bytes));
    emit_bump_probe(ops, context, regs, slow);
    let candidate = regs.candidate;
    emit_load_u64(ops, 16, plan.header_word);
    dynasm!(ops ; .arch aarch64 ; str x16, [X(candidate)]);
    for (index, &word) in plan.image.iter().enumerate() {
        let byte = 8 + 8 * index as u32;
        if word == 0 {
            dynasm!(ops ; .arch aarch64 ; str xzr, [X(candidate), byte]);
        } else {
            emit_load_u64(ops, 16, word);
            dynasm!(ops ; .arch aarch64 ; str x16, [X(candidate), byte]);
        }
    }
    emit_publish(ops, regs);
}
