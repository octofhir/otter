//! One LAB probe, complete shell initialization and one group publication.
//!
//! # Contents
//! - Exact source-realm and all candidate limit guards before writes.
//! - Shared empty-cell payload writer and exact per-member type accounting.
//!
//! # Invariants
//! This helper cannot allocate, collect or call. Every member is initialized
//! before the single top store, and the candidate retains the tagged first cell.
//! No per-member probe/publication or alternate header writer exists.
//!
//! # See also
//! - `super::empty` owns the existing full payload initializer.

use super::empty::{emit_initialize_empty, emit_source_realm_guard};
use super::*;
use crate::allocation::EmptyLiteralLayout;
use crate::x86_64::values::emit_load_u64;

pub(crate) fn emit_fixed_group(
    ops: &mut Assembler,
    context: u8,
    realm: u32,
    members: &[(u32, EmptyLiteralLayout)],
    bytes: u32,
    r: LabRegisters,
    slow: DynamicLabel,
) {
    debug_assert!(members.len() >= 2 && members.len() <= 8);
    emit_source_realm_guard(ops, context, realm, r.scratch, slow);
    if realm != 0
        && members
            .iter()
            .any(|(_, p)| matches!(p, EmptyLiteralLayout::Array(_)))
    {
        dynasm!(ops ; .arch x64 ; jmp =>slow);
        return;
    }
    emit_load_u64(ops, r.size, u64::from(bytes));
    emit_bump_probe(ops, context, r, slow);
    // Move candidate along source-order cells, then recover its original value.
    let mut cursor = 0;
    for &(byte, layout) in members {
        dynasm!(ops ; .arch x64 ; add Rq(r.candidate),(byte-cursor) as i32);
        emit_initialize_empty(ops, layout, r);
        cursor = byte;
    }
    dynasm!(ops ; .arch x64 ; sub Rq(r.candidate),cursor as i32
        ; mov [Rq(r.buffer)+LAB_TOP_OFFSET as i32],Rq(r.end));
    for &(_, layout) in members {
        emit_load_u64(ops, r.size, u64::from(layout.bytes()));
        emit_count_allocation(ops, context, layout.header() as u8, r.size, r.buffer);
    }
}
