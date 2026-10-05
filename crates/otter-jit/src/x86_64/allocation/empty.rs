//! Empty literal initialization on the shared x86 linear allocation buffer.
//!
//! # Contents
//! - Exact VM-owned object/array shells and the source-realm fit guard.
//!
//! # Invariants
//! - Realm and limit misses precede every heap write.
//! - The candidate is a declared temporary, never an early result register.
//! - All payload words are initialized before publication; a fit cannot collect.
//! - Additional-realm arrays use the committed allocator for their sidecar.
//!
//! # See also
//! - [`super`] owns the one target LAB probe and publication encoder.
//! - [`crate::allocation`] owns target-neutral initialization recipes.

use super::{emit_bump_probe, emit_publish};
use crate::allocation::{EmptyLiteralLayout, LabRegisters};
use crate::entry::{THREAD_OFFSET, VALUE_UNDEFINED, VM_THREAD_ACTIVE_REALM_CELL_OFFSET};
use crate::x86_64::values::emit_load_u64;
use dynasmrt::{DynamicLabel, DynasmApi, DynasmLabelApi, dynasm, x64::Assembler};

/// Carve the exact source-realm literal, leaving its value in candidate.
/// Clobbers only recipe temporaries, r10/r11 and arithmetic flags.
pub(crate) fn emit_empty_literal(
    ops: &mut Assembler,
    context: u8,
    realm_id: u32,
    layout: EmptyLiteralLayout,
    regs: LabRegisters,
    slow: DynamicLabel,
) {
    debug_assert_eq!(regs.size, 11);
    debug_assert!(
        [regs.buffer, regs.candidate, regs.end, regs.scratch]
            .iter()
            .all(|register| ![10, 11].contains(register))
    );
    if matches!(layout, EmptyLiteralLayout::Array(_)) && realm_id != 0 {
        dynasm!(ops ; .arch x64 ; jmp =>slow);
        return;
    }
    emit_source_realm_guard(ops, context, realm_id, regs.scratch, slow);
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
    dynasm!(ops ; .arch x64
        ; mov Rq(scratch), [Rq(context) + THREAD_OFFSET as i32]
        ; mov Rq(scratch), [Rq(scratch) + VM_THREAD_ACTIVE_REALM_CELL_OFFSET as i32]
        ; cmp DWORD [Rq(scratch)], realm_id as i32
        ; jne =>slow
    );
}

/// Initialize one already reserved cell; no guard, call, probe or publication.
pub(super) fn emit_initialize_empty(
    ops: &mut Assembler,
    layout: EmptyLiteralLayout,
    regs: LabRegisters,
) {
    let cell_bytes = layout.bytes();
    let header_word = layout.header();
    emit_load_u64(ops, 10, header_word);
    dynasm!(ops ; .arch x64 ; mov [Rq(regs.candidate)], r10);
    match layout {
        EmptyLiteralLayout::Object(plan) => {
            for byte in (8..plan.initial_value_bytes[0]).step_by(8) {
                dynasm!(ops ; .arch x64 ; mov QWORD [Rq(regs.candidate) + byte as i32], 0);
            }
            dynasm!(ops ; .arch x64 ; mov DWORD [Rq(regs.candidate) + plan.shape_byte as i32], plan.shape as i32);
            emit_load_u64(ops, 10, VALUE_UNDEFINED);
            for byte in plan.initial_value_bytes {
                dynasm!(ops ; .arch x64 ; mov [Rq(regs.candidate) + byte as i32], r10);
            }
        }
        EmptyLiteralLayout::Array(_) => {
            for byte in (8..cell_bytes).step_by(8) {
                dynasm!(ops ; .arch x64 ; mov QWORD [Rq(regs.candidate) + byte as i32], 0);
            }
        }
    }
}
