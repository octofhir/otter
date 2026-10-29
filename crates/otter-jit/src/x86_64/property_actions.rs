//! One x86-64 probe of the VM's authoritative named-property action cache.
//!
//! # Contents
//! - One full-width shape/atom set walk shared by Template and Graph.
//! - Independent own/inherited/absent loads and writable/append store actions.
//! - Live persistent-prefix/suffix addressing and pre-effect guards.
//!
//! # Invariants
//! - A probe preserves receiver/value and clobbers only explicit GP
//!   temporaries, r10/r11 and flags; a load additionally writes its destination.
//! - The VM owns the table, action discriminants, immutable shapes and retained
//!   validity words. Native probes neither allocate nor clone a proof.
//! - Every store check finishes before the first write. Append commits slot
//!   then clears the source shape's stability byte and publishes the child
//!   shape; callers perform the existing child/value edge barriers. Defining
//!   field summaries are observed after publication, with a noncollecting leaf
//!   used only on an inline membership miss.
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
    jit::{JitPropertyActionCache, PropertyLoadAction, PropertyStoreAction, slot_word},
    value::tag,
};

use super::values::{emit_load_symbol_u64, emit_load_u64};
use crate::artifact::relocation::{PropertySourceAccess, RelocationCapture, RelocationTarget};

/// The bytes of one set as `odd << shift` with `odd` one of 1, 3, 5 or 9, so
/// a set index scales into the table with one `lea` and a shift.
fn set_stride(cache: JitPropertyActionCache) -> Option<(u32, u32)> {
    let stride = u64::from(cache.entry_bytes) * u64::from(cache.ways);
    let shift = stride.trailing_zeros();
    matches!(stride >> shift, 1 | 3 | 5 | 9).then_some(((stride >> shift) as u32, shift))
}

/// `Rq(entry)` = the first entry of the set the key `(Rq(identity),
/// Rd(atom))` hashes to: one multiply of the shape identity XOR the shifted
/// atom, whose top bits are the set index, scaled by `lea` and a shift.
/// Clobbers `r11`.
fn emit_set_address(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    cache: JitPropertyActionCache,
    [identity, atom]: [u8; 2],
    entry: u8,
) {
    let (odd, shift) = set_stride(cache).expect("validated set stride");
    if cache.set_mask == 0 {
        dynasm!(ops ; .arch x64 ; xor Rd(entry), Rd(entry));
    } else {
        // The atom register's upper half is not read: the shift drops it.
        dynasm!(ops ; .arch x64
            ; mov Rq(entry), Rq(atom)
            ; shl Rq(entry), cache.hash_atom_shift as i8
            ; xor Rq(entry), Rq(identity));
        emit_load_u64(ops, 11, cache.hash_multiplier);
        dynasm!(ops ; .arch x64
            ; imul Rq(entry), r11
            ; shr Rq(entry), cache.hash_shift as i8);
        match odd {
            3 => dynasm!(ops ; .arch x64 ; lea Rq(entry), [Rq(entry) + Rq(entry) * 2]),
            5 => dynasm!(ops ; .arch x64 ; lea Rq(entry), [Rq(entry) + Rq(entry) * 4]),
            9 => dynasm!(ops ; .arch x64 ; lea Rq(entry), [Rq(entry) + Rq(entry) * 8]),
            _ => {}
        }
        if shift != 0 {
            dynasm!(ops ; .arch x64 ; shl Rq(entry), shift as i8);
        }
    }
    emit_load_symbol_u64(
        ops,
        relocations,
        11,
        cache.table_addr as u64,
        RelocationTarget::PropertyActionCacheTable,
    );
    dynasm!(ops ; .arch x64 ; add Rq(entry), r11);
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
    atom: u8,
    access: PropertySourceAccess,
    receiver: u8,
    value: Option<u8>,
    [shape, identity, entry, base]: [u8; 4],
    destination: Option<u8>,
    own_only: bool,
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
    debug_assert!(
        atom != receiver && !temps.contains(&atom) && atom != 10 && atom != 11,
        "the atom register survives the probe"
    );
    debug_assert!(value != Some(atom));
    let Some(cache) = cache.filter(|cache| {
        cache.table_addr != 0
            && cache.entry_bytes != 0
            && cache.entry_bytes < 4096
            && cache.ways != 0
            && cache.ways <= u32::from(u16::MAX)
            && (cache.set_mask + 1).is_power_of_two()
            && u32::from(cache.hash_shift) + (cache.set_mask + 1).trailing_zeros() == 64
            && cache.hash_atom_shift < 64
            && set_stride(*cache).is_some()
    }) else {
        dynasm!(ops ; .arch x64 ; jmp =>miss);
        return;
    };
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
    );
    // The table records loads only for shaped receivers with ordinary
    // lookup and stores only for shapes whose stores stay in place: a
    // dictionary, opaque or prototype receiver's shape id keys no such fact,
    // and shape state is immutable, so neither probe tests state.
    dynasm!(ops ; .arch x64
        ; mov Rq(identity), [Rq(shape) + cache.shape_id_byte as i32]);

    emit_set_address(ops, relocations, cache, [identity, atom], entry);
    let way = ops.new_dynamic_label();
    let next = ops.new_dynamic_label();
    let found = ops.new_dynamic_label();
    dynasm!(ops ; .arch x64
        ; mov Rd(base), cache.ways as i32
        ; =>way
        ; cmp Rq(identity), [Rq(entry) + cache.receiver_shape_id_byte as i32] ; jne =>next);
    dynasm!(ops ; .arch x64 ; cmp [Rq(entry) + cache.atom_byte as i32], Rd(atom));
    dynasm!(ops ; .arch x64
        ; je =>found
        ; =>next
        ; add Rq(entry), cache.entry_bytes as i32
        ; dec Rd(base) ; jnz =>way ; jmp =>miss
        ; =>found);
    // A projected storage object answers its own slots only: its chain is
    // not the source receiver's.
    if own_only {
        match access {
            PropertySourceAccess::Load => dynasm!(ops ; .arch x64
                ; cmp BYTE [Rq(entry) + cache.load_action_byte as i32], PropertyLoadAction::OwnData as i8 ; jne =>miss),
            PropertySourceAccess::Store => dynasm!(ops ; .arch x64
                ; cmp BYTE [Rq(entry) + cache.store_action_byte as i32], PropertyStoreAction::OwnWritable as i8 ; jne =>miss),
        }
    }

    match access {
        PropertySourceAccess::Load => {
            let destination = destination.expect("property load destination");
            let own = ops.new_dynamic_label();
            let absent = ops.new_dynamic_label();
            let undefined = ops.new_dynamic_label();
            let canonical = ops.new_dynamic_label();
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
                // The recorded holder was shaped with ordinary lookup, so its
                // never reused shape id proves its state as well as its layout.
                ; mov r10, [r10 + cache.shape_id_byte as i32]
                ; cmp r10, [Rq(entry) + cache.holder_shape_id_byte as i32] ; jne =>miss
                ; mov Rd(identity), [Rq(entry) + cache.load_word_byte as i32]);
            emit_word_load(ops, view, shape, identity, base, destination, miss);
            dynasm!(ops ; .arch x64
                ; jmp =>canonical
                // The matched key already proved the receiver's exact shape.
                ; =>own
                ; mov Rd(identity), [Rq(entry) + cache.load_word_byte as i32]);
            emit_word_load(ops, view, receiver, identity, base, destination, miss);
            dynasm!(ops ; .arch x64
                ; jmp =>canonical
                // Absent from the whole chain: the proof (none for a `null`
                // prototype the receiver shape fixes) authorizes `undefined`.
                ; =>absent
                ; mov r10, [Rq(entry) + cache.load_validity_byte as i32]
                ; test r10, r10 ; jz =>undefined
                ; cmp DWORD [r10], 0 ; je =>miss
                ; =>undefined);
            emit_load_u64(ops, destination, crate::entry::VALUE_UNDEFINED);
            dynasm!(ops ; .arch x64
                ; jmp =>done
                ; =>canonical
            );

            dynasm!(ops ; .arch x64 ; jmp =>done);
        }
        PropertySourceAccess::Store => {
            let value = value.expect("property store value");
            let own = ops.new_dynamic_label();
            let guards_done = ops.new_dynamic_label();
            dynasm!(ops ; .arch x64
                ; cmp BYTE [Rq(entry) + cache.store_action_byte as i32], PropertyStoreAction::OwnWritable as i8 ; je =>own
                ; cmp BYTE [Rq(entry) + cache.store_action_byte as i32], PropertyStoreAction::AddOwn as i8 ; jne =>miss
                // The recorded source shape was extensible, which its
                // immutable state keeps.
                // Code subscribed to the shape the receiver leaves: the
                // runtime store fires it, before anything is written.
                ; test BYTE [Rq(shape) + view.shape_watched_byte as i32], otter_vm::code_dependencies::ShapeFact::Stable as i8 ; jnz =>miss
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
                ; mov Rq(base), r10
                ; mov r10, [Rq(base) + cache.shape_id_byte as i32]
                ; cmp r10, [Rq(entry) + cache.target_shape_id_byte as i32] ; jne =>miss
                ; mov Rd(identity), [Rq(entry) + cache.target_shape_byte as i32]);
            dynasm!(ops ; .arch x64 ; movzx Rd(identity), WORD [Rq(entry) + cache.store_slot_byte as i32]);
            emit_slot_storage(ops, view, receiver, identity, base, miss);
            dynasm!(ops ; .arch x64
                ; mov [Rq(base) + Rq(identity) * 8], Rq(value)
                ; mov BYTE [Rq(shape) + view.shape_stable_byte as i32], 0
                ; mov Rd(identity), [Rq(entry) + cache.target_shape_byte as i32]
                ; mov [Rq(receiver) + view.object_shape_byte as i32], Rd(identity)
                ; mov Rd(entry), [Rq(entry) + cache.store_owner_byte as i32]
            );
            super::field_type::emit_observe(ops, relocations, view, entry, value);
            // An overwrite of a value its field already covers needs no
            // observation; an uncovered one is observed only where the
            // summary tracks value shapes, and otherwise completes in the
            // runtime.
            let observe = ops.new_dynamic_label();
            dynasm!(ops ; .arch x64 ; jmp =>appended
                ; =>own
                ; mov Rd(identity), [Rq(entry) + cache.store_word_byte as i32]
                ; test Rd(identity), Rd(identity) ; jz =>miss);
            emit_covered(ops, value, identity, observe, miss);
            emit_word_store(ops, view, receiver, value, identity, base, miss);
            dynasm!(ops ; .arch x64 ; jmp =>done ; =>observe);
            emit_word_store(ops, view, receiver, value, identity, base, miss);
            dynasm!(ops ; .arch x64
                ; mov Rd(entry), [Rq(entry) + cache.store_owner_byte as i32]);
            super::field_type::emit_observe(ops, relocations, view, entry, value);
            dynasm!(ops ; .arch x64
                ; jmp =>done);
        }
    }
}

/// `Rq(destination)` = the slot of `holder` that `Rd(word)` resolves
/// ([`otter_vm::jit::slot_word`]); a slab slot beyond the object's current
/// slab goes to `miss`, before the destination is written.
fn emit_word_load(
    ops: &mut Assembler,
    view: &JitCompileSnapshot,
    holder: u8,
    word: u8,
    base: u8,
    destination: u8,
    miss: DynamicLabel,
) {
    let slab = ops.new_dynamic_label();
    let done = ops.new_dynamic_label();
    dynasm!(ops ; .arch x64
        ; test Rd(word), Rd(word) ; jz =>miss
        ; test Rd(word), slot_word::SLAB as i32 ; jnz =>slab
        ; movzx r10d, Rw(word)
        ; mov Rq(destination), [Rq(holder) + r10]
        ; jmp =>done
        ; =>slab);
    emit_word_slab(ops, view, holder, word, base, miss);
    dynasm!(ops ; .arch x64 ; mov Rq(destination), [Rq(base) + Rq(word)] ; =>done);
}

/// `Rq(base)` = `holder`'s slab words and `Rq(word)` the byte offset of the
/// slab slot `Rd(word)` resolves, inside the slab's capacity, else `miss`.
fn emit_word_slab(
    ops: &mut Assembler,
    view: &JitCompileSnapshot,
    holder: u8,
    word: u8,
    base: u8,
    miss: DynamicLabel,
) {
    dynasm!(ops ; .arch x64
        ; and Rd(word), slot_word::OFFSET_MASK as i32
        ; mov Rd(base), [Rq(holder) + view.field_layout.slab_handle_byte as i32]
        ; test Rd(base), Rd(base) ; jz =>miss);
    emit_load_u64(ops, 10, 0xffff_ffff_0000_0000);
    dynasm!(ops ; .arch x64
        ; and r10, Rq(holder) ; add Rq(base), r10
        ; mov r10d, [Rq(base) + view.field_layout.slab_capacity_byte as i32]
        ; shl r10d, 3 ; cmp Rd(word), r10d ; jae =>miss
        ; add Rq(base), view.field_layout.slab_words_byte as i32);
}

/// Fall through when `Rd(word)` covers `Rq(value)`'s class, so the store
/// needs no observation; else go to `observe` when the summary tracks value
/// shapes, or to `miss`.
fn emit_covered(
    ops: &mut Assembler,
    value: u8,
    word: u8,
    observe: DynamicLabel,
    miss: DynamicLabel,
) {
    let covered = ops.new_dynamic_label();
    let uncovered = ops.new_dynamic_label();
    let number = ops.new_dynamic_label();
    let other = ops.new_dynamic_label();
    let undefined = ops.new_dynamic_label();
    dynasm!(ops ; .arch x64
        ; test Rd(word), slot_word::ANY as i32 ; jnz =>covered
        ; mov r10, Rq(value) ; sar r10, tag::NUMBER_TAG.trailing_zeros() as i8
        ; cmp r10, -1 ; jne =>number
        ; test Rd(word), slot_word::INT32 as i32 ; jnz =>covered
        ; jmp =>uncovered
        ; =>number);
    emit_load_u64(ops, 10, tag::NUMBER_TAG);
    dynasm!(ops ; .arch x64
        ; test Rq(value), r10 ; jz =>other
        ; test Rd(word), slot_word::DOUBLE as i32 ; jnz =>covered
        ; jmp =>uncovered
        ; =>other);
    emit_load_u64(ops, 10, crate::entry::VALUE_NULL);
    dynasm!(ops ; .arch x64
        ; cmp Rq(value), r10 ; jne =>undefined
        ; test Rd(word), slot_word::NULL as i32 ; jnz =>covered
        ; jmp =>uncovered
        ; =>undefined);
    emit_load_u64(ops, 10, crate::entry::VALUE_UNDEFINED);
    dynasm!(ops ; .arch x64
        ; cmp Rq(value), r10 ; jne =>uncovered
        ; test Rd(word), slot_word::UNDEFINED as i32 ; jnz =>covered
        ; =>uncovered
        ; test Rd(word), slot_word::OBSERVE as i32 ; jnz =>observe
        ; jmp =>miss
        ; =>covered);
}

/// Store `Rq(value)` into the slot of `receiver` that `Rd(word)` resolves;
/// a slab slot beyond the current slab goes to `miss` before anything is
/// written.
fn emit_word_store(
    ops: &mut Assembler,
    view: &JitCompileSnapshot,
    receiver: u8,
    value: u8,
    word: u8,
    base: u8,
    miss: DynamicLabel,
) {
    let slab = ops.new_dynamic_label();
    let done = ops.new_dynamic_label();
    dynasm!(ops ; .arch x64
        ; test Rd(word), slot_word::SLAB as i32 ; jnz =>slab
        ; movzx r10d, Rw(word)
        ; mov [Rq(receiver) + r10], Rq(value)
        ; jmp =>done
        ; =>slab);
    emit_word_slab(ops, view, receiver, word, base, miss);
    dynasm!(ops ; .arch x64 ; mov [Rq(base) + Rq(word)], Rq(value) ; =>done);
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
pub(super) fn emit_slot_storage_of_shape(
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
