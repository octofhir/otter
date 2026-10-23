//! Own data reads from the VM's complete shape directory.
//!
//! # Contents
//! - [`emit_load`] probes the shape prefix, then reads its current storage bank.
//!
//! # Invariants
//! - No cache, call, allocation or safepoint; inputs survive every miss.
//! - A bucket outside the receiver's prefix cannot authorize a read.
//! - Other descriptors and exotic receivers leave before the operation.
//! - Only assigned GP temporaries, r10/r11 and destination are clobbered.
//!
//! # See also
//! - `crate::arm64::shape_lookup` implements the same VM-owned contract.
use super::values::emit_load_u64;
use dynasmrt::{DynamicLabel, DynasmApi, DynasmLabelApi, dynasm, x64::Assembler};
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
    emit_load_u64(ops, 10, 0xfffe_0000_0000_0007);
    dynasm!(ops ; .arch x64
        ; test Rq(receiver), r10 ; jnz =>miss ; test Rq(receiver), Rq(receiver) ; jz =>miss
        ; cmp BYTE [Rq(receiver)], crate::entry::OBJECT_BODY_TYPE_TAG as i8 ; jne =>miss);
    emit_load_u64(ops, 11, 0xffff_ffff_0000_0000);
    dynasm!(ops ; .arch x64
        ; and r11, Rq(receiver)
        ; mov Rd(shape), [Rq(receiver) + view.object_shape_byte as i32]
        ; test Rd(shape), Rd(shape) ; jz =>miss ; add Rq(shape), r11
        ; test BYTE [Rq(shape) + view.shape_state_byte as i32], (ShapeState::DICTIONARY_MASK | ShapeState::OPAQUE_LOOKUP_MASK) as i8 ; jnz =>miss
        ; mov Rd(entry), [Rq(shape) + l.keys_byte as i32]
        ; test Rd(entry), Rd(entry) ; jz =>chain ; add Rq(entry), r11
        ; mov Rd(mask), [Rq(entry) + l.capacity_byte as i32]
        ; imul r10, Rq(mask), l.entry_bytes as i32
        ; lea Rq(entry), [Rq(entry) + r10 + l.entries_byte as i32]
        ; shl Rd(mask), 1 ; dec Rd(mask)
        ; mov Rd(slot), otter_vm::object::ShapeLookupLayout::atom_hash(atom) as i32
        ; and Rd(slot), Rd(mask)
        ; =>probe ; mov r10, [Rq(entry) + Rq(slot) * 8]
        ; test r10, r10 ; jz =>miss
        ; mov r11, r10 ; shr r11, 32 ; cmp r11d, atom as i32 ; jne =>next
        ; test r10d, 1 << 16 ; jnz =>miss
        ; movzx Rd(slot), r10w ; dec Rd(slot)
        ; cmp Rd(slot), [Rq(shape) + view.shape_property_count_byte as i32] ; jae =>miss
        ; jmp =>read
        ; =>next ; inc Rd(slot) ; and Rd(slot), Rd(mask) ; jmp =>probe
        ; =>chain ; mov Rq(entry), Rq(shape)
        ; =>walk ; mov Rd(slot), [Rq(entry) + view.shape_property_count_byte as i32]
        ; test Rd(slot), Rd(slot) ; jz =>miss
        ; cmp DWORD [Rq(entry) + l.atom_byte as i32], atom as i32 ; je =>chain_found);
    emit_load_u64(ops, 11, 0xffff_ffff_0000_0000);
    dynasm!(ops ; .arch x64
        ; and r11, Rq(receiver)
        ; mov Rd(entry), [Rq(entry) + l.parent_byte as i32]
        ; test Rd(entry), Rd(entry) ; jz =>miss ; add Rq(entry), r11 ; jmp =>walk
        ; =>chain_found ; cmp BYTE [Rq(entry) + l.accessor_byte as i32], 0 ; jne =>miss
        ; mov Rd(entry), [Rq(entry) + l.field_byte as i32]
        ; dec Rd(slot)
        ; =>read);
    super::property_actions::emit_slot_storage_of_shape(
        ops, view, receiver, shape, slot, mask, miss,
    );
    dynasm!(ops ; .arch x64 ; mov Rq(destination), [Rq(mask) + Rq(slot) * 8]);
    let done = ops.new_dynamic_label();

    dynasm!(ops ; .arch x64 ; =>done);
}
