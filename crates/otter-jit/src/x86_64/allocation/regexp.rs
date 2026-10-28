//! Young birth of a regex literal from its baked image.
//!
//! # Contents
//! - [`emit_regexp_literal`] — the realm guard, one bump, the header and the
//!   image words, then publication.
//!
//! # Invariants
//! The image is the complete payload of a fresh body over the site's old
//! program cell, so the cell needs no barrier and nothing is read from the
//! heap. A realm or room miss precedes every write. Only r10 is clobbered
//! beside the declared temporaries.
//!
//! # See also
//! `otter_vm::jit::JitRegExpLiteralPlan` owns the image.

use super::empty::emit_source_realm_guard;
use super::{emit_bump_probe, emit_publish};
use crate::allocation::LabRegisters;
use crate::x86_64::values::emit_load_u64;
use dynasmrt::{DynamicLabel, DynasmApi, dynasm, x64::Assembler};
use otter_vm::jit::JitRegExpLiteralPlan;

pub(crate) fn emit_regexp_literal(
    ops: &mut Assembler,
    context: u8,
    realm_id: u32,
    plan: &JitRegExpLiteralPlan,
    regs: LabRegisters,
    slow: DynamicLabel,
) {
    debug_assert_eq!(regs.size, 11);
    emit_source_realm_guard(ops, context, realm_id, regs.scratch, slow);
    emit_load_u64(ops, regs.size, u64::from(plan.cell_bytes));
    emit_bump_probe(ops, context, regs, slow);
    let candidate = regs.candidate;
    emit_load_u64(ops, 10, plan.header_word);
    dynasm!(ops ; .arch x64 ; mov [Rq(candidate)], r10);
    for (index, &word) in plan.image.iter().enumerate() {
        let byte = (8 + 8 * index) as i32;
        if word == 0 {
            dynasm!(ops ; .arch x64 ; mov QWORD [Rq(candidate) + byte], 0);
        } else {
            emit_load_u64(ops, 10, word);
            dynasm!(ops ; .arch x64 ; mov [Rq(candidate) + byte], r10);
        }
    }
    emit_publish(ops, regs);
}
