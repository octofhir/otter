//! Own data reads from the live shape directory.
//!
//! # Contents
//! - [`emit_load`] resolves the atom through a shape's complete key directory.
//!
//! # Invariants
//! - No cache residency, allocation, call, safepoint or retained raw pointer.
//! - Shared directories are bounded by the receiver shape's immutable prefix.
//! - Accessors, absent properties and exotic lookup leave before the read.
//! - Inputs survive every miss; assigned GP temporaries and x16/x17 are scratch.
//!
//! # See also
//! - `otter_vm::object::ShapeLookupLayout` owns the physical contract.
use crate::template::arm64::values::emit_load_u64;
use dynasmrt::{DynamicLabel, DynasmApi, DynasmLabelApi, aarch64::Assembler, dynasm};
use otter_vm::{JitCompileSnapshot, object::ShapeState};

pub(crate) fn emit_load(
    ops: &mut Assembler,
    view: &JitCompileSnapshot,
    atom: u32,
    receiver: u8,
    [shape, slot, entry, mask]: [u8; 4],
    destination: u8,
    miss: DynamicLabel,
) {
    let l = view.shape_lookup;
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
    super::property_actions::bank_of_shape(ops, view, receiver, shape, slot, mask, miss);
    dynasm!(ops ; .arch aarch64
            ; ldr X(destination), [X(mask), X(slot), lsl #3]
    );
    let done = ops.new_dynamic_label();

    dynasm!(ops ; .arch aarch64 ; =>done);
}
