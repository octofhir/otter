//! Shape-selected object storage geometry shared by x86-64 native tiers.
//!
//! # Contents
//! - Explicit-register field-bank and compressed prototype materialization.
//! - Fused own-field loads and stores without a movable SSA storage pointer.
//!
//! # Invariants
//! - Receivers name decompressed GC headers; VM layouts include the header.
//! - A proved shape fixes the bank and index, including the persistent prefix.
//! - Suffix handles are reread for each access and never retained across calls.
//! - Symbol addresses keep their full-width artifact relocation identity.
//!
//! # See also
//! - `otter_vm::object::FieldLocation` owns bank and slot-to-byte conversion.
//! - [`super::values`] owns immediate and symbolic address materialization.

use dynasmrt::{DynasmApi, dynasm, x64::Assembler};
use otter_vm::{
    JitCompileSnapshot,
    object::{FieldLayout, FieldLocation},
};

use super::values::{emit_load_symbol_u64, emit_load_u64};
use crate::artifact::relocation::{RelocationCapture, RelocationTarget};

/// Materialize the first word of a shape-proven bank in `destination`.
pub(crate) fn emit_field_base(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    view: &JitCompileSnapshot,
    header: u8,
    destination: u8,
    scratch: u8,
    field: FieldLocation,
) {
    debug_assert_ne!(destination, scratch);
    if field.is_inline() {
        dynasm!(ops ; .arch x64 ; lea Rq(destination), [Rq(header) + view.field_layout.inline_values_byte as i32]);
    } else {
        dynasm!(ops ; .arch x64 ; mov Rd(scratch), [Rq(header) + view.field_layout.slab_handle_byte as i32]);
        emit_load_symbol_u64(
            ops,
            relocations,
            destination,
            view.cage_base as u64,
            RelocationTarget::GcCageBase,
        );
        dynasm!(ops ; .arch x64
            ; add Rq(destination), Rq(scratch)
            ; add Rq(destination), view.field_layout.slab_words_byte as i32);
    }
}

/// Load an object's compressed prototype. `destination` may alias `object`.
pub(crate) fn emit_load_prototype(
    ops: &mut Assembler,
    view: &JitCompileSnapshot,
    destination: u8,
    object: u8,
    cage: u8,
) {
    debug_assert_ne!(destination, cage);
    dynasm!(ops ; .arch x64
        ; mov Rd(destination), [Rq(object) + view.object_shape_byte as i32]
        ; mov Rd(destination), [Rq(cage) + Rq(destination) + view.shape_prototype_byte as i32]);
}

/// Load or store one shape-owned field, clobbering only r10/r11 and a load result.
pub(crate) fn emit_own_field(
    ops: &mut Assembler,
    layout: FieldLayout,
    object: u8,
    value: u8,
    field: FieldLocation,
    store: bool,
) {
    let (base, offset) = if field.is_inline() {
        (object, u64::from(layout.inline_byte(field)))
    } else {
        dynasm!(ops ; .arch x64 ; mov r11d, [Rq(object) + layout.slab_handle_byte as i32]);
        emit_load_u64(ops, 10, 0xffff_ffff_0000_0000);
        dynasm!(ops ; .arch x64 ; and r10, Rq(object) ; add r11, r10);
        (
            11,
            u64::from(layout.slab_words_byte) + u64::from(field.byte_offset()),
        )
    };
    if let Ok(offset) = i32::try_from(offset) {
        if store {
            dynasm!(ops ; .arch x64 ; mov [Rq(base) + offset], Rq(value));
        } else {
            dynasm!(ops ; .arch x64 ; mov Rq(value), [Rq(base) + offset]);
        }
    } else {
        emit_load_u64(ops, 10, offset);
        if store {
            dynasm!(ops ; .arch x64 ; mov [Rq(base) + r10], Rq(value));
        } else {
            dynasm!(ops ; .arch x64 ; mov Rq(value), [Rq(base) + r10]);
        }
    }
}
