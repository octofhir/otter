//! One x86-64 probe of the VM's authoritative named-property action cache.
//!
//! # Contents
//! - One full-width shape/atom set walk shared by Template and Graph.
//! - Independent own/inherited/absent loads and writable/append store actions.
//! - Live persistent-prefix/suffix addressing and pre-effect guards.
//!
//! # Invariants
//! - A probe preserves receiver/value and clobbers four assigned GPs, r10/r11
//!   and flags only; a successful load additionally writes its destination.
//! - The VM owns the table, action discriminants, immutable shapes and retained
//!   validity words. Native probes neither allocate nor clone a proof.
//! - Every store check finishes before the first write. Append commits slot
//!   then child shape; callers perform the existing child/value edge barriers.
//! - Slots and the suffix handle are reread from current geometry. No storage
//!   pointer or table proof survives a call, collection or committed miss.
//! - Unknown, unresolvable and runtime-only actions take the one cold source
//!   owner. Successful native hits never reenter, deopt or replay source work.
//!
//! # See also
//! - `otter_vm::jit::JitPropertyActionCache` owns the physical layout DTO.
//! - [`super::fields`] owns static shape-selected field operations.
//! - `crate::graph::x86_64::properties` and `crate::template::x86_64` supply
//!   their current value homes, committed miss and barrier owners.

use dynasmrt::{DynamicLabel, DynasmApi, DynasmLabelApi, dynasm, x64::Assembler};
use otter_vm::{
    JitCompileSnapshot,
    jit::{JitPropertyActionCache, PropertyLoadAction, PropertyStoreAction},
    object::ShapeState,
    value::tag,
};

use super::values::{emit_load_symbol_u64, emit_load_u64};
use crate::artifact::relocation::{PropertySourceAccess, RelocationCapture, RelocationTarget};

/// The probed property key: a compile-time atom, or one held in a register
/// by a shared per-code-object IC routine that serves many sites.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AtomOperand {
    Immediate(u32),
    Register(u8),
}

/// Probe a key once, then consume only the requested independent action.
///
/// `temps[1]` contains the compressed child on `appended`; the caller emits its
/// canonical child barrier before joining its ordinary value-barrier path.
/// `destination` may alias the receiver on a load, after every holder guard.
#[allow(clippy::too_many_arguments)]
pub(crate) fn emit_action_probe(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    view: &JitCompileSnapshot,
    cache: Option<JitPropertyActionCache>,
    atom: Option<AtomOperand>,
    access: PropertySourceAccess,
    receiver: u8,
    value: Option<u8>,
    [shape, identity, entry, base]: [u8; 4],
    destination: Option<u8>,
    miss: DynamicLabel,
    done: DynamicLabel,
    appended: DynamicLabel,
) {
    let temps = [shape, identity, entry, base];
    debug_assert!(temps.iter().all(|r| *r != receiver && *r != 10 && *r != 11));
    debug_assert!(
        temps
            .iter()
            .enumerate()
            .all(|(i, r)| !temps[..i].contains(r))
    );
    if let Some(value) = value {
        debug_assert!(temps.iter().all(|r| *r != value));
        debug_assert!(value != 10 && value != 11);
    }
    debug_assert!(receiver != 10 && receiver != 11);
    if let Some(AtomOperand::Register(atom)) = atom {
        debug_assert!(
            atom != receiver && !temps.contains(&atom) && atom != 10 && atom != 11,
            "the atom register survives the probe"
        );
        debug_assert!(value != Some(atom));
    }
    let Some(cache) = cache.filter(|cache| {
        cache.table_addr != 0
            && cache.entry_bytes != 0
            && cache.entry_bytes < 4096
            && cache.ways != 0
            && cache.ways <= u32::from(u16::MAX)
            && cache.hash_shift < 64
    }) else {
        dynasm!(ops ; .arch x64 ; jmp =>miss);
        return;
    };
    let Some(atom) = atom else {
        dynasm!(ops ; .arch x64 ; jmp =>miss);
        return;
    };
    let store = matches!(access, PropertySourceAccess::Store);
    let excluded = ShapeState::DICTIONARY_MASK
        | ShapeState::OPAQUE_LOOKUP_MASK
        | if store { ShapeState::PROTOTYPE_MASK } else { 0 };
    emit_load_u64(ops, 10, tag::NOT_CELL_MASK);
    dynasm!(ops ; .arch x64
        ; test Rq(receiver), r10 ; jnz =>miss
        ; test Rq(receiver), Rq(receiver) ; jz =>miss
        ; cmp BYTE [Rq(receiver)], crate::entry::OBJECT_BODY_TYPE_TAG as i8 ; jne =>miss);
    emit_load_u64(ops, 10, 0xffff_ffff_0000_0000);
    dynasm!(ops ; .arch x64
        ; and r10, Rq(receiver)
        ; mov Rd(shape), [Rq(receiver) + view.object_shape_byte as i32]
        ; test Rd(shape), Rd(shape) ; jz =>miss
        ; add Rq(shape), r10
        ; test BYTE [Rq(shape) + view.shape_state_byte as i32], excluded as i8 ; jnz =>miss
        ; mov Rq(identity), [Rq(shape) + cache.shape_id_byte as i32]);

    emit_load_u64(ops, 11, cache.hash_shape_multiplier);
    dynasm!(ops ; .arch x64 ; mov Rq(entry), Rq(identity) ; imul Rq(entry), r11);
    match atom {
        AtomOperand::Immediate(atom) => emit_load_u64(
            ops,
            11,
            u64::from(atom).wrapping_mul(cache.hash_atom_multiplier),
        ),
        AtomOperand::Register(atom) => {
            // The register holds the zero-extended 32-bit atom.
            emit_load_u64(ops, 11, cache.hash_atom_multiplier);
            dynasm!(ops ; .arch x64 ; mov Rd(atom), Rd(atom) ; imul r11, Rq(atom));
        }
    }
    dynasm!(ops ; .arch x64 ; xor Rq(entry), r11 ; shr Rq(entry), cache.hash_shift as i8);
    emit_load_u64(ops, 11, u64::from(cache.set_mask));
    dynasm!(ops ; .arch x64 ; and Rq(entry), r11);
    emit_load_u64(
        ops,
        11,
        u64::from(cache.entry_bytes) * u64::from(cache.ways),
    );
    dynasm!(ops ; .arch x64 ; imul Rq(entry), r11);
    emit_load_symbol_u64(
        ops,
        relocations,
        11,
        cache.table_addr as u64,
        RelocationTarget::PropertyActionCacheTable,
    );
    dynasm!(ops ; .arch x64 ; add Rq(entry), r11);
    let way = ops.new_dynamic_label();
    let next = ops.new_dynamic_label();
    let found = ops.new_dynamic_label();
    dynasm!(ops ; .arch x64
        ; mov Rd(base), cache.ways as i32
        ; =>way
        ; cmp Rq(identity), [Rq(entry) + cache.receiver_shape_id_byte as i32] ; jne =>next);
    match atom {
        AtomOperand::Immediate(atom) => dynasm!(ops ; .arch x64
            ; cmp DWORD [Rq(entry) + cache.atom_byte as i32], atom as i32),
        AtomOperand::Register(atom) => dynasm!(ops ; .arch x64
            ; cmp [Rq(entry) + cache.atom_byte as i32], Rd(atom)),
    }
    dynasm!(ops ; .arch x64
        ; je =>found
        ; =>next
        ; add Rq(entry), cache.entry_bytes as i32
        ; dec Rd(base) ; jnz =>way ; jmp =>miss
        ; =>found);

    match access {
        PropertySourceAccess::Load => {
            let destination = destination.expect("property load destination");
            let own = ops.new_dynamic_label();
            let absent = ops.new_dynamic_label();
            let undefined = ops.new_dynamic_label();
            dynasm!(ops ; .arch x64
                ; cmp BYTE [Rq(entry) + cache.load_action_byte as i32], PropertyLoadAction::OwnData as i8 ; je =>own
                ; cmp BYTE [Rq(entry) + cache.load_action_byte as i32], PropertyLoadAction::NonExistent as i8 ; je =>absent
                ; cmp BYTE [Rq(entry) + cache.load_action_byte as i32], PropertyLoadAction::InheritedData as i8 ; jne =>miss
                ; mov r10, [Rq(entry) + cache.load_validity_byte as i32]
                ; test r10, r10 ; jz =>miss
                ; cmp DWORD [r10], 0 ; je =>miss
                ; mov Rd(shape), [Rq(entry) + cache.holder_root_byte as i32]
                ; test Rd(shape), Rd(shape) ; jz =>miss);
            emit_load_u64(ops, 11, 0xffff_ffff_0000_0000);
            dynasm!(ops ; .arch x64
                ; and r11, Rq(receiver) ; add Rq(shape), r11
                ; mov Rd(shape), [Rq(shape) + view.shape_prototype_byte as i32]
                ; test Rd(shape), Rd(shape) ; jz =>miss
                ; add Rq(shape), r11
                ; cmp BYTE [Rq(shape)], crate::entry::OBJECT_BODY_TYPE_TAG as i8 ; jne =>miss
                ; mov r10d, [Rq(shape) + view.object_shape_byte as i32]
                ; test r10d, r10d ; jz =>miss
                ; cmp r10d, [Rq(entry) + cache.holder_shape_byte as i32] ; jne =>miss);
            emit_load_u64(ops, 11, 0xffff_ffff_0000_0000);
            dynasm!(ops ; .arch x64
                ; and r11, Rq(shape) ; add r10, r11
                ; test BYTE [r10 + view.shape_state_byte as i32], (ShapeState::DICTIONARY_MASK | ShapeState::OPAQUE_LOOKUP_MASK) as i8 ; jnz =>miss
                ; mov r10, [r10 + cache.shape_id_byte as i32]
                ; cmp r10, [Rq(entry) + cache.holder_shape_id_byte as i32] ; jne =>miss
                ; movzx Rd(identity), WORD [Rq(entry) + cache.load_slot_byte as i32]);
            emit_slot_storage(ops, view, shape, identity, base, miss);
            dynasm!(ops ; .arch x64
                ; mov Rq(destination), [Rq(base) + Rq(identity) * 8]
                ; jmp =>done
                // The matched key already proved the receiver's exact shape,
                // whose address the key decode left in `shape`.
                ; =>own
                ; movzx Rd(identity), WORD [Rq(entry) + cache.load_slot_byte as i32]);
            emit_slot_storage_of_shape(ops, view, receiver, shape, identity, base, miss);
            dynasm!(ops ; .arch x64
                ; mov Rq(destination), [Rq(base) + Rq(identity) * 8]
                ; jmp =>done
                // Absent from the whole chain: the proof (none for a `null`
                // prototype the receiver shape fixes) authorizes `undefined`.
                ; =>absent
                ; mov r10, [Rq(entry) + cache.load_validity_byte as i32]
                ; test r10, r10 ; jz =>undefined
                ; cmp DWORD [r10], 0 ; je =>miss
                ; =>undefined);
            emit_load_u64(ops, destination, crate::entry::VALUE_UNDEFINED);
            dynasm!(ops ; .arch x64 ; jmp =>done);
        }
        PropertySourceAccess::Store => {
            let value = value.expect("property store value");
            let own = ops.new_dynamic_label();
            let guards_done = ops.new_dynamic_label();
            dynasm!(ops ; .arch x64
                ; cmp BYTE [Rq(entry) + cache.store_action_byte as i32], PropertyStoreAction::OwnWritable as i8 ; je =>own
                ; cmp BYTE [Rq(entry) + cache.store_action_byte as i32], PropertyStoreAction::AddOwn as i8 ; jne =>miss
                ; test BYTE [Rq(shape) + view.shape_state_byte as i32], ShapeState::EXTENSIBLE_MASK as i8 ; jz =>miss
                ; mov r10, [Rq(entry) + cache.store_validity_byte as i32]
                // A zero proof belongs only to canonical OwnAdd: the exact
                // immutable receiver shape key already fixes its null chain.
                ; test r10, r10 ; jz =>guards_done
                ; cmp DWORD [r10], 0 ; je =>miss
                ; =>guards_done
                ; mov r10d, [Rq(entry) + cache.target_shape_byte as i32]
                ; test r10d, r10d ; jz =>miss);
            emit_load_u64(ops, 11, 0xffff_ffff_0000_0000);
            dynasm!(ops ; .arch x64
                ; and r11, Rq(receiver) ; add r10, r11
                ; mov r10, [r10 + cache.shape_id_byte as i32]
                ; cmp r10, [Rq(entry) + cache.target_shape_id_byte as i32] ; jne =>miss
                ; movzx Rd(identity), WORD [Rq(entry) + cache.store_slot_byte as i32]);
            emit_slot_storage(ops, view, receiver, identity, base, miss);
            dynasm!(ops ; .arch x64
                ; mov [Rq(base) + Rq(identity) * 8], Rq(value)
                ; mov Rd(identity), [Rq(entry) + cache.target_shape_byte as i32]
                ; mov [Rq(receiver) + view.object_shape_byte as i32], Rd(identity)
                ; jmp =>appended
                ; =>own
                ; movzx Rd(identity), WORD [Rq(entry) + cache.store_slot_byte as i32]);
            emit_slot_storage_of_shape(ops, view, receiver, shape, identity, base, miss);
            dynasm!(ops ; .arch x64
                ; mov [Rq(base) + Rq(identity) * 8], Rq(value)
                ; jmp =>done);
        }
    }
}

/// `slot` becomes relative to the currently selected live bank.
fn emit_slot_storage(
    ops: &mut Assembler,
    view: &JitCompileSnapshot,
    holder: u8,
    slot: u8,
    base: u8,
    miss: DynamicLabel,
) {
    emit_load_u64(ops, 10, 0xffff_ffff_0000_0000);
    dynasm!(ops ; .arch x64
        ; and r10, Rq(holder)
        ; mov r11d, [Rq(holder) + view.object_shape_byte as i32] ; add r11, r10);
    emit_slot_storage_of_shape(ops, view, holder, 11, slot, base, miss);
}

/// [`emit_slot_storage`] of `holder` whose current shape's address is already
/// in `shape`.
fn emit_slot_storage_of_shape(
    ops: &mut Assembler,
    view: &JitCompileSnapshot,
    holder: u8,
    shape: u8,
    slot: u8,
    base: u8,
    miss: DynamicLabel,
) {
    let inline = ops.new_dynamic_label();
    let ready = ops.new_dynamic_label();
    dynasm!(ops ; .arch x64
        ; movzx r11d, BYTE [Rq(shape) + view.shape_inline_capacity_byte as i32]
        ; cmp Rd(slot), r11d ; jb =>inline ; sub Rd(slot), r11d
        ; mov Rd(base), [Rq(holder) + view.field_layout.slab_handle_byte as i32]
        ; test Rd(base), Rd(base) ; jz =>miss);
    emit_load_u64(ops, 10, 0xffff_ffff_0000_0000);
    dynasm!(ops ; .arch x64
        ; and r10, Rq(holder) ; add Rq(base), r10
        ; cmp Rd(slot), [Rq(base) + view.field_layout.slab_capacity_byte as i32] ; jae =>miss
        ; add Rq(base), view.field_layout.slab_words_byte as i32 ; jmp =>ready
        ; =>inline ; lea Rq(base), [Rq(holder) + view.field_layout.inline_values_byte as i32]
        ; =>ready);
}

#[cfg(test)]
mod tests;
