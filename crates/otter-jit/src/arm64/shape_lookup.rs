//! Own data reads from the live shape directory.
//!
//! # Contents
//! - [`emit_load`] resolves the atom through a shape's complete key directory.
//!
//! # Invariants
//! - No allocation, call, safepoint or retained raw pointer. The one cache
//!   is the named site's own ([`otter_vm::jit::PROPERTY_IC_LAYOUT`] site
//!   cache): keyed by never reused shape identities, filled here only with
//!   an own data slot the directory resolved, so a hit is the same read.
//! - Shared directories are bounded by the receiver shape's immutable prefix.
//! - Accessors, absent properties and exotic lookup leave before the read.
//! - Inputs survive every miss; assigned GP temporaries and x16/x17 are scratch.
//!
//! # See also
//! - `otter_vm::object::ShapeLookupLayout` owns the physical contract.
use crate::artifact::relocation::{RelocationCapture, RelocationTarget};
use crate::template::arm64::values::{emit_load_symbol_u64, emit_load_u64};
use dynasmrt::{DynamicLabel, DynasmApi, DynasmLabelApi, aarch64::Assembler, dynasm};
use otter_vm::{JitCompileSnapshot, jit::slot_word, object::ShapeState};

#[allow(clippy::too_many_arguments)]
pub(crate) fn emit_load(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    view: &JitCompileSnapshot,
    atom: u32,
    receiver: u8,
    [shape, slot, entry, mask]: [u8; 4],
    destination: u8,
    site: Option<(u64, RelocationTarget)>,
    miss: DynamicLabel,
) {
    use otter_vm::jit::PROPERTY_IC_LAYOUT as IC;
    let l = view.shape_lookup;
    let directory = ops.new_dynamic_label();
    let done = ops.new_dynamic_label();
    if let Some((address, target)) = site.clone() {
        // The site's own cache: one identity compare, the slot resolved.
        emit_load_symbol_u64(ops, relocations, 17, address, target);
        dynasm!(ops ; .arch aarch64
            ; tst X(receiver), #0xfffe_0000_0000_0007 ; b.ne =>miss ; cbz X(receiver), =>miss
            ; ldrb w16, [X(receiver)] ; cmp w16, #crate::entry::OBJECT_BODY_TYPE_TAG ; b.ne =>miss
            ; ldr X(entry), [x17, IC.site_cache_byte] ; cbz X(entry), =>directory
            ; ldr W(shape), [X(receiver), view.object_shape_byte] ; cbz W(shape), =>miss
            ; and x17, X(receiver), #0xffff_ffff_0000_0000 ; add X(shape), x17, X(shape)
            ; ldr X(slot), [X(shape), view.shape_id_byte]
            ; and XSP(mask), X(slot), #u64::from(IC.site_cache_entries - 1)
            ; add X(entry), X(entry), X(mask), lsl #4
            ; ldp x16, X(mask), [X(entry)]
            ; cmp x16, X(slot) ; b.ne =>directory
            ; tbnz W(mask), SLOT_WORD_SLAB_BIT, =>directory
            ; and x16, X(mask), #u64::from(slot_word::OFFSET_MASK)
            ; ldr X(destination), [X(receiver), x16]
            ; b =>done);
    }
    dynasm!(ops ; .arch aarch64 ; =>directory);
    let chain = ops.new_dynamic_label();
    let walk = ops.new_dynamic_label();
    let chain_found = ops.new_dynamic_label();
    let probe = ops.new_dynamic_label();
    let next = ops.new_dynamic_label();
    let read = ops.new_dynamic_label();
    dynasm!(ops ; .arch aarch64
        ; tst X(receiver), #0xfffe_0000_0000_0007 ; b.ne =>miss ; cbz X(receiver), =>miss
        ; ldrb w16, [X(receiver)] ; cmp w16, #crate::entry::OBJECT_BODY_TYPE_TAG ; b.ne =>miss
        ; and x17, X(receiver), #0xffff_ffff_0000_0000
        ; ldr W(shape), [X(receiver), view.object_shape_byte] ; cbz W(shape), =>miss
        ; add X(shape), x17, X(shape)
        ; ldrb w16, [X(shape), view.shape_state_byte]);
    emit_load_u64(
        ops,
        17,
        u64::from(ShapeState::DICTIONARY_MASK | ShapeState::OPAQUE_LOOKUP_MASK),
    );
    dynasm!(ops ; .arch aarch64
        ; tst w16, w17 ; b.ne =>miss
        ; and x17, X(receiver), #0xffff_ffff_0000_0000
        ; ldr W(entry), [X(shape), l.keys_byte] ; cbz W(entry), =>chain
        ; add X(entry), x17, X(entry)
        ; ldr W(mask), [X(entry), l.capacity_byte]);
    emit_load_u64(ops, 16, u64::from(l.entry_bytes));
    dynasm!(ops ; .arch aarch64
        ; madd X(entry), X(mask), x16, X(entry)
        ; add XSP(entry), XSP(entry), #l.entries_byte
        ; lsl W(mask), W(mask), #1 ; sub WSP(mask), WSP(mask), #1);
    emit_load_u64(
        ops,
        slot,
        u64::from(otter_vm::object::ShapeLookupLayout::atom_hash(atom)),
    );
    dynasm!(ops ; .arch aarch64
        ; and W(slot), W(slot), W(mask)
        ; =>probe ; ldr x16, [X(entry), X(slot), lsl #3] ; cbz x16, =>miss);
    emit_load_u64(ops, 17, u64::from(atom) << 32);
    dynasm!(ops ; .arch aarch64
        ; eor x16, x16, x17 ; lsr x17, x16, #32 ; cbnz x17, =>next
        ; tbnz x16, #16, =>miss
        ; ubfx W(slot), w16, #0, #16 ; sub WSP(slot), WSP(slot), #1
        ; ldr w17, [X(shape), view.shape_property_count_byte]
        ; cmp W(slot), w17 ; b.hs =>miss
        ; b =>read
        ; =>next ; add WSP(slot), WSP(slot), #1 ; and W(slot), W(slot), W(mask) ; b =>probe
        ; =>chain ; mov X(entry), X(shape)
        ; =>walk ; ldr W(slot), [X(entry), view.shape_property_count_byte] ; cbz W(slot), =>miss
        ; ldr w16, [X(entry), l.atom_byte]);
    emit_load_u64(ops, 17, u64::from(atom));
    dynasm!(ops ; .arch aarch64
        ; cmp w16, w17 ; b.eq =>chain_found
        ; and x17, X(receiver), #0xffff_ffff_0000_0000
        ; ldr W(entry), [X(entry), l.parent_byte] ; cbz W(entry), =>miss
        ; add X(entry), x17, X(entry) ; b =>walk
        ; =>chain_found ; ldrb w16, [X(entry), l.accessor_byte] ; cbnz w16, =>miss
        ; ldr W(entry), [X(entry), l.field_byte]
        ; sub WSP(slot), WSP(slot), #1
        ; =>read);
    if let Some((address, target)) = site {
        // Resolve the slot into its inline-bank word and leave it in the
        // site's cache; a slab slot or an offset too wide is read as before.
        let banked = ops.new_dynamic_label();
        dynasm!(ops ; .arch aarch64
            ; ldrb w16, [X(shape), view.shape_inline_capacity_byte]
            ; cmp W(slot), w16 ; b.hs =>banked
            ; cmp WSP(slot), #MAX_CACHED_INLINE_SLOT ; b.hi =>banked
            ; lsl W(entry), W(slot), #3 ; add WSP(entry), WSP(entry), #view.field_layout.inline_values_byte);
        emit_load_symbol_u64(ops, relocations, 17, address, target);
        dynasm!(ops ; .arch aarch64
            ; ldr x16, [x17, IC.site_cache_byte] ; cbz x16, >unfilled
            ; ldr x17, [X(shape), view.shape_id_byte]
            ; and XSP(mask), x17, #u64::from(IC.site_cache_entries - 1)
            ; add x16, x16, X(mask), lsl #4
            ; stp x17, X(entry), [x16]
            ; unfilled:
            ; ldr X(destination), [X(receiver), X(entry)]
            ; b =>done
            ; =>banked);
    }
    super::property_actions::bank_of_shape(ops, view, receiver, shape, slot, mask, miss);
    dynasm!(ops ; .arch aarch64
            ; ldr X(destination), [X(mask), X(slot), lsl #3]
            ; =>done);
}

/// The widest inline slot whose resolved offset a site cache word holds.
const MAX_CACHED_INLINE_SLOT: u32 = 4095;
const SLOT_WORD_SLAB_BIT: u32 = slot_word::SLAB.trailing_zeros();
