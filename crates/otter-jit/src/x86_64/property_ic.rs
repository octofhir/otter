//! x86-64 machine handlers over one named-property IC slot, shared by the
//! Template IC routines and Graph generic property nodes.
//!
//! # Contents
//! - [`emit_select_entry`] — V8 `TryMonomorphicCase` / `HandlePolymorphicCase`:
//!   the receiver's map selects its slot entry — an ordinary object's shape,
//!   or a closure's property-bag shape keyed with the function bit.
//! - [`emit_slot_load`] — own field, prototype field under its chain proof,
//!   nonexistent key.
//! - [`emit_slot_store`] — own-field store and add-transition (write, shape
//!   publication), leaving the published child shape for the caller's barrier.
//!
//! # Invariants
//! - Callers name every register; the emitters additionally clobber only
//!   `r10`/`r11` and flags. The receiver, value and slot registers survive
//!   every miss.
//! - Handler checks precede the first effect; a miss has no effects and
//!   allocates nothing. A megamorphic slot branches to its own label so the
//!   caller can probe the isolate's shared action table.
//! - Feedback is read from the slot at run time; nothing is baked.
//!
//! # See also
//! - `otter_vm::property_ic` owns the slot layout and handler kinds.
//! - `crate::x86_64::property_actions` owns the shared-table probe.

use dynasmrt::{DynamicLabel, DynasmApi, DynasmLabelApi, dynasm, x64::Assembler};
use otter_vm::JitCompileSnapshot;
use otter_vm::jit::{
    PROPERTY_IC_FUNCTION_RECEIVER_KEY_BIT as FUNCTION_RECEIVER_KEY_BIT, PROPERTY_IC_LAYOUT as IC,
    PropertyIcHandlerKind as Kind,
};

use super::values::emit_load_u64;
use crate::entry::{
    OBJECT_BODY_TYPE_TAG, THREAD_OFFSET, VALUE_UNDEFINED, VM_THREAD_ACTIVE_REALM_CELL_OFFSET,
};

const CAGE_MASK: u64 = 0xffff_ffff_0000_0000;

fn distinct(registers: &[u8]) -> bool {
    registers
        .iter()
        .enumerate()
        .all(|(i, r)| *r < 16 && !matches!(*r, 4 | 10 | 11) && !registers[..i].contains(r))
}

/// Select the entry of the receiver's map in the slot at `Rq(slot)`:
/// `Rq(entry)` addresses it on fall-through. An ordinary object's map is its
/// shape. With `start` given, a closure receiver whose property bag carries
/// its own `[[Prototype]]` (bag present, no override, default realm active)
/// selects by its bag's shape keyed with the function bit, and `Rq(start)`
/// becomes the lookup-start object (the receiver itself for an ordinary
/// object). A megamorphic slot jumps to `megamorphic`, anything else to
/// `miss`. Clobbers `Rq(entry)`, `Rq(scratch)`, `r10`, `r11`.
#[allow(clippy::too_many_arguments)]
pub(crate) fn emit_select_entry(
    ops: &mut Assembler,
    view: &JitCompileSnapshot,
    receiver: u8,
    slot: u8,
    [entry, scratch]: [u8; 2],
    start: Option<u8>,
    megamorphic: DynamicLabel,
    miss: DynamicLabel,
) {
    debug_assert!(distinct(&[receiver, slot, entry, scratch]));
    debug_assert!(start.is_none_or(|start| distinct(&[receiver, slot, entry, scratch, start])));
    let scan = ops.new_dynamic_label();
    let found = ops.new_dynamic_label();
    let keyed = ops.new_dynamic_label();
    emit_load_u64(ops, 10, otter_vm::value::tag::NOT_CELL_MASK);
    dynasm!(ops ; .arch x64
        ; test Rq(receiver), r10 ; jnz =>miss
        ; test Rq(receiver), Rq(receiver) ; jz =>miss
        ; cmp BYTE [Rq(receiver)], OBJECT_BODY_TYPE_TAG as i8);
    match start {
        None => dynasm!(ops ; .arch x64
            ; jne =>miss
            ; mov r11d, [Rq(receiver) + view.object_shape_byte as i32]),
        Some(start) => {
            let function = ops.new_dynamic_label();
            let layout = view.closure_call_layout;
            let qualifying = i32::from(
                otter_vm::closure::CLOSURE_LOOKUP_OWN_PROPS
                    | otter_vm::closure::CLOSURE_LOOKUP_PROTO_OVERRIDE,
            );
            dynasm!(ops ; .arch x64
                ; jne =>function
                ; mov r11d, [Rq(receiver) + view.object_shape_byte as i32]
                ; mov Rq(start), Rq(receiver)
                ; jmp =>keyed
                ; =>function
                ; cmp BYTE [Rq(receiver)], otter_vm::closure::JS_CLOSURE_BODY_TYPE_TAG as i8
                ; jne =>miss
                ; movzx r10d, BYTE [Rq(receiver) + otter_vm::closure::CLOSURE_NAMED_LOOKUP_BYTE as i32]
                ; and r10d, qualifying
                ; cmp r10d, i32::from(otter_vm::closure::CLOSURE_LOOKUP_OWN_PROPS)
                ; jne =>miss
                ; mov r10, [r15 + THREAD_OFFSET as i32]
                ; mov r10, [r10 + VM_THREAD_ACTIVE_REALM_CELL_OFFSET as i32]
                ; test r10, r10 ; jz =>miss
                ; cmp DWORD [r10], 0 ; jne =>miss
                ; mov r10d, [Rq(receiver) + layout.rare_byte as i32]
                ; test r10d, r10d ; jz =>miss);
            emit_load_u64(ops, 11, CAGE_MASK);
            dynasm!(ops ; .arch x64
                ; and r11, Rq(receiver) ; add r10, r11
                ; mov Rd(start), [r10 + layout.own_props_byte as i32]
                ; test Rd(start), Rd(start) ; jz =>miss
                ; add Rq(start), r11
                ; mov r11d, [Rq(start) + view.object_shape_byte as i32]
                ; or r11d, FUNCTION_RECEIVER_KEY_BIT as i32);
        }
    }
    dynasm!(ops ; .arch x64
        ; =>keyed
        ; mov Rd(scratch), [Rq(slot) + IC.state_byte as i32]
        ; test Rd(scratch), IC.megamorphic_bit as i32 ; jnz =>megamorphic
        ; and Rd(scratch), IC.count_mask as i32 ; jz =>miss
        ; lea Rq(entry), [Rq(slot) + IC.entries_byte as i32]
        ; =>scan
        ; cmp r11d, [Rq(entry) + IC.entry_shape_byte as i32] ; je =>found
        ; dec Rd(scratch) ; jz =>miss
        ; add Rq(entry), IC.entry_bytes as i32 ; jmp =>scan
        ; =>found
    );
}

/// Jump to `miss` unless the entry at `Rq(entry)` carries no proof (when
/// `required` is false) or a valid one. Clobbers `r10`.
fn emit_entry_proof(ops: &mut Assembler, entry: u8, required: bool, miss: DynamicLabel) {
    let held = ops.new_dynamic_label();
    dynasm!(ops ; .arch x64
        ; mov r10, [Rq(entry) + IC.entry_validity_byte as i32]
        ; test r10, r10);
    if required {
        dynasm!(ops ; .arch x64 ; jz =>miss);
    } else {
        dynasm!(ops ; .arch x64 ; jz =>held);
    }
    dynasm!(ops ; .arch x64 ; cmp DWORD [r10], 0 ; je =>miss ; =>held);
}

/// `r10` = first word of the slab the field key in `Rd(index)` selects on
/// the object at `Rq(holder)` and `Rq(index)` its bank-relative index, or
/// `Rq(index)` the inline index when the key selects the in-object bank
/// (then control continues at `inline`). A missing slab jumps to `miss`.
/// Clobbers `r11`.
fn emit_field_bank(
    ops: &mut Assembler,
    view: &JitCompileSnapshot,
    holder: u8,
    index: u8,
    inline: DynamicLabel,
    miss: DynamicLabel,
) {
    let layout = view.field_layout;
    dynasm!(ops ; .arch x64
        ; test Rd(index), Rd(index) ; js =>inline
        ; mov r10d, [Rq(holder) + layout.slab_handle_byte as i32]
        ; test r10d, r10d ; jz =>miss);
    emit_load_u64(ops, 11, CAGE_MASK);
    dynasm!(ops ; .arch x64 ; and r11, Rq(holder) ; add r10, r11);
}

/// Complete a load through the slot at `Rq(slot)`: on a hit
/// `Rq(destination)` holds the value and control jumps to `done`.
/// `destination` may alias the receiver; it is written after every check.
#[allow(clippy::too_many_arguments)]
pub(crate) fn emit_slot_load(
    ops: &mut Assembler,
    view: &JitCompileSnapshot,
    receiver: u8,
    slot: u8,
    [entry, index, holder]: [u8; 3],
    destination: u8,
    megamorphic: DynamicLabel,
    miss: DynamicLabel,
    done: DynamicLabel,
) {
    debug_assert!(distinct(&[receiver, slot, entry, index, holder]));
    debug_assert!(destination != slot && destination != entry && destination != holder);
    let own = ops.new_dynamic_label();
    let prototype = ops.new_dynamic_label();
    let field = ops.new_dynamic_label();
    let inline = ops.new_dynamic_label();
    let layout = view.field_layout;
    emit_select_entry(
        ops,
        view,
        receiver,
        slot,
        [entry, index],
        Some(holder),
        megamorphic,
        miss,
    );
    dynasm!(ops ; .arch x64
        ; movzx r10d, BYTE [Rq(entry) + IC.entry_kind_byte as i32]
        ; cmp r10d, Kind::OwnField as i32 ; je =>own
        ; cmp r10d, Kind::PrototypeField as i32 ; je =>prototype
        ; cmp r10d, Kind::NonExistent as i32 ; jne =>miss);
    emit_entry_proof(ops, entry, false, miss);
    emit_load_u64(ops, destination, VALUE_UNDEFINED);
    dynasm!(ops ; .arch x64 ; jmp =>done ; =>prototype);
    emit_entry_proof(ops, entry, true, miss);
    emit_load_u64(ops, 11, CAGE_MASK);
    dynasm!(ops ; .arch x64
        ; and r11, Rq(receiver)
        ; mov Rd(holder), [Rq(entry) + IC.entry_aux_byte as i32] ; add Rq(holder), r11
        ; mov Rd(holder), [Rq(holder) + view.shape_prototype_byte as i32]
        ; test Rd(holder), Rd(holder) ; jz =>miss ; add Rq(holder), r11
        ; jmp =>field
        // An own field lives on the lookup-start object the selection left
        // in the holder register.
        ; =>own
        ; =>field
        ; mov Rd(index), [Rq(entry) + IC.entry_field_byte as i32]);
    emit_field_bank(ops, view, holder, index, inline, miss);
    dynasm!(ops ; .arch x64
        ; mov Rq(destination), [r10 + Rq(index) * 8 + layout.slab_words_byte as i32]
        ; jmp =>done
        ; =>inline
        ; and Rd(index), 0x7fff_ffff
        ; mov Rq(destination), [Rq(holder) + Rq(index) * 8 + layout.inline_values_byte as i32]
        ; jmp =>done);
}

/// Complete a store of `Rq(value)` through the slot at `Rq(slot)`: on a hit
/// the field is written, an append has published its target shape, and
/// control jumps to `stored` with `Rd(child)` holding the compressed child
/// shape (zero for an overwrite). The caller owns both barriers.
#[allow(clippy::too_many_arguments)]
pub(crate) fn emit_slot_store(
    ops: &mut Assembler,
    view: &JitCompileSnapshot,
    receiver: u8,
    value: u8,
    slot: u8,
    [entry, index, child]: [u8; 3],
    megamorphic: DynamicLabel,
    miss: DynamicLabel,
    stored: DynamicLabel,
) {
    debug_assert!(distinct(&[receiver, value, slot, entry, index, child]));
    let own = ops.new_dynamic_label();
    let append_inline = ops.new_dynamic_label();
    let publish = ops.new_dynamic_label();
    let own_inline = ops.new_dynamic_label();
    let overwritten = ops.new_dynamic_label();
    let layout = view.field_layout;
    emit_select_entry(
        ops,
        view,
        receiver,
        slot,
        [entry, index],
        None,
        megamorphic,
        miss,
    );
    dynasm!(ops ; .arch x64
        ; movzx r10d, BYTE [Rq(entry) + IC.entry_kind_byte as i32]
        ; cmp r10d, Kind::StoreField as i32 ; je =>own
        ; cmp r10d, Kind::StoreTransition as i32 ; jne =>miss);
    // Append: the chain proof (absent for a `null` prototype, which the
    // receiver shape fixes), a resident target layout and storage, then the
    // write and the target shape's publication.
    emit_entry_proof(ops, entry, false, miss);
    dynasm!(ops ; .arch x64
        ; mov Rd(child), [Rq(entry) + IC.entry_aux_byte as i32]
        ; test Rd(child), Rd(child) ; jz =>miss
        ; mov Rd(index), [Rq(entry) + IC.entry_field_byte as i32]);
    emit_field_bank(ops, view, receiver, index, append_inline, miss);
    dynasm!(ops ; .arch x64
        ; cmp Rd(index), [r10 + layout.slab_capacity_byte as i32] ; jae =>miss
        ; mov [r10 + Rq(index) * 8 + layout.slab_words_byte as i32], Rq(value)
        ; jmp =>publish
        ; =>append_inline
        ; and Rd(index), 0x7fff_ffff
        ; mov [Rq(receiver) + Rq(index) * 8 + layout.inline_values_byte as i32], Rq(value)
        ; =>publish
        ; mov [Rq(receiver) + view.object_shape_byte as i32], Rd(child)
        ; jmp =>stored
        ; =>own
        ; mov Rd(index), [Rq(entry) + IC.entry_field_byte as i32]);
    emit_field_bank(ops, view, receiver, index, own_inline, miss);
    dynasm!(ops ; .arch x64
        ; mov [r10 + Rq(index) * 8 + layout.slab_words_byte as i32], Rq(value)
        ; jmp =>overwritten
        ; =>own_inline
        ; and Rd(index), 0x7fff_ffff
        ; mov [Rq(receiver) + Rq(index) * 8 + layout.inline_values_byte as i32], Rq(value)
        ; =>overwritten
        ; xor Rd(child), Rd(child)
        ; jmp =>stored);
}
