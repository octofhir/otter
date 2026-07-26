//! AArch64 lowering of one cache program, shared by every tier that probes a
//! property site.
//!
//! # Contents
//! - [`emit_way_walk`] — match the receiver shape against the cell's ways.
//! - [`emit_resolve_holder`] — perform the guarded prototype hop a matched way
//!   asks for.
//! - [`emit_refuse_prototype_hop`] — send hop programs to the stub at sites
//!   that cannot execute them.
//!
//! # Invariants
//! - Both tiers emit property probes from here. A cache program has exactly one
//!   machine lowering, so a tier cannot disagree with the interpreter about
//!   what a site caches.
//! - Way stride is [`WHISKER_IC_WAY_BYTES`], asserted against the cell's own
//!   layout where the cell is defined.
//! - Register contract on entry: `x15` holds the cell address, `w14` the
//!   receiver shape handle, `x13` the receiver's `GcHeader`. On a matched way
//!   `w17` holds the slot byte offset and `w7` the guarded holder shape; after
//!   [`emit_resolve_holder`], `x13` is the holder's header and `w14` its shape.
//! - The hop reads the receiver's `[[Prototype]]` at run time. The receiver
//!   shape cannot stand in for it: the prototype lives in the object body, so
//!   `setPrototypeOf` changes it while the shape stays put.

use dynasmrt::{DynamicLabel, DynasmApi, DynasmLabelApi, aarch64::Assembler, dynasm};
use otter_vm::JitCompileSnapshot;

use super::values::emit_load_symbol_u64;
use crate::artifact::relocation::{RelocationCapture, RelocationTarget};
use crate::entry::{IC_WAYS, OBJECT_BODY_TYPE_TAG, WHISKER_IC_WAY_BYTES};

/// Match `w14` against the cell's ways, branching to `miss` when none hold.
///
/// On a match `w7` carries the way's holder shape and `w17` its slot byte
/// offset, and control falls through to `matched`.
pub(crate) fn emit_way_walk(ops: &mut Assembler, matched: DynamicLabel, miss: DynamicLabel) {
    for way in 0..IC_WAYS as u32 {
        let shape_off = way * WHISKER_IC_WAY_BYTES;
        let holder_off = shape_off + 4;
        let value_byte_off = shape_off + 8;
        let next = ops.new_dynamic_label();
        dynasm!(ops
            ; .arch aarch64
            ; ldr w16, [x15, shape_off]
            ; cmp w14, w16
            ; b.ne =>next
            ; ldr w7, [x15, holder_off]
            ; ldr w17, [x15, value_byte_off]
            ; b =>matched
            ; =>next
        );
    }
    dynasm!(ops ; .arch aarch64 ; b =>miss ; =>matched);
}

/// Resolve the object that owns the slot.
///
/// A way with holder shape `0` owns its slot on the receiver and this is a
/// single branch. Otherwise the receiver's `[[Prototype]]` is loaded and its
/// shape guarded before the caller touches the slab.
pub(crate) fn emit_resolve_holder(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    view: &JitCompileSnapshot,
    miss: DynamicLabel,
) {
    let shape_byte = view.object_shape_byte;
    let resolved = ops.new_dynamic_label();
    dynasm!(ops
        ; .arch aarch64
        ; cbz w7, =>resolved
        ; ldr w12, [x13, view.jit_proto_byte]
        ; cbz w12, =>miss
    );
    emit_load_symbol_u64(
        ops,
        relocations,
        13,
        view.cage_base as u64,
        RelocationTarget::GcCageBase,
    );
    dynasm!(ops
        ; .arch aarch64
        ; add x13, x13, x12          // x13 = prototype GcHeader ptr
        ; ldrb w14, [x13]
        ; cmp w14, OBJECT_BODY_TYPE_TAG
        ; b.ne =>miss
        ; ldr w14, [x13, shape_byte] // holder shape handle
        ; cmp w14, w7
        ; b.ne =>miss
        ; =>resolved
    );
}

/// Send a matched way to `miss` when it asks for a hop this site cannot run.
///
/// Stores write through the holder, which the store sequences do not resolve;
/// such a program stays on the stub rather than writing the wrong object.
pub(crate) fn emit_refuse_prototype_hop(ops: &mut Assembler, miss: DynamicLabel) {
    dynasm!(ops ; .arch aarch64 ; cbnz w7, =>miss);
}
