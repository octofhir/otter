//! AArch64 machine handlers over one named-property IC slot, shared by the
//! Template IC routines and Graph generic property nodes.
//!
//! # Contents
//! - [`emit_select_entry`] — the source receiver selects a current storage
//!   shape through the shared checked projection key.
//! - [`emit_slot_load`] — own field, prototype field under its chain proof,
//!   nonexistent key.
//! - [`emit_slot_store`] — own-field store and add-transition (write, shape
//!   publication), returning the actual storage parent and optional child
//!   shape for the caller's barriers.
//!
//! # Invariants
//! - Callers name every register; the emitters additionally clobber only
//!   `x16`/`x17`. The receiver, value and slot registers survive every miss.
//! - Handler checks precede the first effect; a miss has no effects and
//!   allocates nothing. A megamorphic slot branches to its own label so the
//!   caller can probe the isolate's shared action table.
//! - Feedback is read from the slot at run time; nothing is baked.
//! - An append clears the source shape's stability byte before publishing
//!   its child; overwrite stores preserve the shape's stability.
//! - Stores observe their defining field owner after complete publication.
//!   Membership misses call a noncollecting leaf that preserves live registers.
//! - Stores retain Value-valid words and observe physical numeric categories;
//!   every loaded Double becomes canonical before it enters Tagged state.
//!
//! # See also
//! - `otter_vm::property_ic` owns the slot layout and handler kinds.
//! - `crate::arm64::property_actions` owns the shared-table probe.

use dynasmrt::{DynamicLabel, DynasmApi, DynasmLabelApi, aarch64::Assembler, dynasm};
use otter_vm::JitCompileSnapshot;
use otter_vm::jit::{
    PROPERTY_IC_LAYOUT as IC, PropertyIcHandlerKind as Kind,
    PropertyStorageProjection as Projection,
};
use otter_vm::object::FieldLocation;

use crate::entry::{
    OBJECT_BODY_TYPE_TAG, THREAD_OFFSET, VALUE_UNDEFINED, VM_THREAD_ACTIVE_REALM_CELL_OFFSET,
};
use crate::template::arm64::values::{CellTest, emit_cell_test, emit_load_u64};

fn distinct(registers: &[u8]) -> bool {
    registers
        .iter()
        .enumerate()
        .all(|(i, r)| *r < 31 && !matches!(*r, 16 | 17) && !registers[..i].contains(r))
}

/// Read a tagged live named-storage object without changing the source receiver.
/// The nullable compressed edge is reloaded on every access; no payload pointer
/// is retained across a helper or collection.
pub(crate) fn emit_array_expando(
    ops: &mut Assembler,
    layout: otter_vm::jit::JitArrayLayout,
    receiver: u8,
    destination: u8,
    miss: DynamicLabel,
) {
    emit_cell_test(ops, receiver, CellTest::IsNotCell, miss);
    dynasm!(ops ; .arch aarch64
        ; cbz X(receiver), =>miss
        ; ldrb w16, [X(receiver)]
        ; cmp w16, u32::from(layout.type_tag) ; b.ne =>miss
        ; and x17, X(receiver), #0xffff_ffff_0000_0000
        ; ldr W(destination), [X(receiver), layout.expando_byte]
        ; cbz W(destination), =>miss
        ; add X(destination), x17, X(destination));
}

/// Select the entry of the receiver's map in the slot at `X(slot)`:
/// `X(entry)` addresses it on fall-through. An ordinary object's map is its
/// shape. With `start` given, an Array selects the live expando's shape with
/// `ArrayExpando`, and a closure or native function whose bag carries its own
/// `[[Prototype]]` (bag present, no override, default realm active), or a
/// class constructor's statics in the default realm, selects that shape with
/// `LookupStart`. `X(start)` receives the storage object,
/// including the receiver itself for an ordinary object. A megamorphic slot
/// branches to `megamorphic`, anything else to
/// `miss`. Clobbers `X(entry)`, `X(scratch)`, `x16`, `x17`.
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
    let projected = ops.new_dynamic_label();
    emit_cell_test(ops, receiver, CellTest::IsNotCell, miss);
    dynasm!(ops ; .arch aarch64
        ; cbz X(receiver), =>miss
        ; ldrb w16, [X(receiver)]
        ; cmp w16, OBJECT_BODY_TYPE_TAG
    );
    match start {
        None => dynasm!(ops ; .arch aarch64
            ; b.ne =>miss
            ; ldr w17, [X(receiver), view.object_shape_byte]
        ),
        // An ordinary object is its own storage and falls through to the scan;
        // every projected receiver kind selects out of line.
        Some(start) => dynasm!(ops ; .arch aarch64
            ; b.ne =>projected
            ; ldr w17, [X(receiver), view.object_shape_byte]
            ; mov X(start), X(receiver)
        ),
    }
    dynasm!(ops ; .arch aarch64
        ; =>keyed
        ; ldr w16, [X(slot), IC.state_byte]
        ; tbnz w16, IC.megamorphic_bit.trailing_zeros(), =>megamorphic
        ; and w16, w16, #IC.count_mask
        ; cbz w16, =>miss
        ; add XSP(entry), XSP(slot), #IC.entries_byte
        ; =>scan
        ; ldr W(scratch), [X(entry), IC.entry_shape_byte]
        ; cmp W(scratch), w17
        ; b.eq =>found
        ; subs w16, w16, #1
        ; b.eq =>miss
        ; add XSP(entry), XSP(entry), #IC.entry_bytes
        ; b =>scan
    );
    if let Some(start) = start {
        let array = ops.new_dynamic_label();
        let native = ops.new_dynamic_label();
        let class = ops.new_dynamic_label();
        let bag = ops.new_dynamic_label();
        let layout = view.closure_call_layout;
        let native_layout = view.native_call_layout;
        let qualifying = u32::from(
            otter_vm::closure::CLOSURE_LOOKUP_OWN_PROPS
                | otter_vm::closure::CLOSURE_LOOKUP_PROTO_OVERRIDE,
        );
        dynasm!(ops ; .arch aarch64
            ; =>projected
            ; cmp w16, u32::from(view.array_layout.type_tag)
            ; b.eq =>array
            ; cmp w16, u32::from(otter_vm::native_function::NATIVE_FUNCTION_BODY_TYPE_TAG)
            ; b.eq =>native
            ; cmp w16, u32::from(view.class_constructor_layout.type_tag)
            ; b.eq =>class
            ; cmp w16, u32::from(otter_vm::closure::JS_CLOSURE_BODY_TYPE_TAG)
            ; b.ne =>miss
            ; ldrb w16, [X(receiver), otter_vm::closure::CLOSURE_NAMED_LOOKUP_BYTE]
            ; and w16, w16, #qualifying
            ; cmp w16, u32::from(otter_vm::closure::CLOSURE_LOOKUP_OWN_PROPS)
            ; b.ne =>miss
            ; ldr w16, [X(receiver), layout.rare_byte]
            ; cbz w16, =>miss
            ; and x17, X(receiver), #0xffff_ffff_0000_0000
            ; add x16, x17, x16
            ; ldr W(start), [x16, layout.own_props_byte]
            ; b =>bag
            // A class's statics carry its [[Prototype]] chain.
            ; =>class
            ; ldr W(start), [X(receiver), view.class_constructor_layout.statics_byte]
            ; b =>bag
            ; =>native
            ; ldrb w16, [X(receiver), native_layout.flags_byte]
            ; tst w16, u32::from(native_layout.prototype_override_flag)
            ; b.ne =>miss
            ; ldr W(start), [X(receiver), native_layout.own_props_byte]
            // The bag of either function kind: present, in the default
            // realm, keyed by its shape.
            ; =>bag
            ; cbz W(start), =>miss
            ; ldr x16, [x20, THREAD_OFFSET]
            ; ldr x16, [x16, VM_THREAD_ACTIVE_REALM_CELL_OFFSET]
            ; cbz x16, =>miss
            ; ldr w16, [x16]
            ; cbnz w16, =>miss
            ; and x17, X(receiver), #0xffff_ffff_0000_0000
            ; add X(start), x17, X(start)
            ; ldr w17, [X(start), view.object_shape_byte]
            ; orr w17, w17, #Projection::LookupStart as u32
            ; b =>keyed
            ; =>array
            ; ldr W(start), [X(receiver), view.array_layout.expando_byte]
            ; cbz W(start), =>miss
            ; and x17, X(receiver), #0xffff_ffff_0000_0000
            ; add X(start), x17, X(start)
            ; ldr w17, [X(start), view.object_shape_byte]
            ; orr w17, w17, #Projection::ArrayExpando as u32
            ; b =>keyed
        );
    }
    dynasm!(ops ; .arch aarch64 ; =>found);
}

/// Branch to `miss` unless the entry at `X(entry)` carries no proof (when
/// `required` is false) or a valid one. Clobbers `x16`.
fn emit_entry_proof(ops: &mut Assembler, entry: u8, required: bool, miss: DynamicLabel) {
    let held = ops.new_dynamic_label();
    dynasm!(ops ; .arch aarch64 ; ldr x16, [X(entry), IC.entry_validity_byte]);
    if required {
        dynasm!(ops ; .arch aarch64 ; cbz x16, =>miss);
    } else {
        dynasm!(ops ; .arch aarch64 ; cbz x16, =>held);
    }
    // Proofs retire on the mutator, which also runs this code: a plain load
    // sees every retirement before it.
    dynasm!(ops ; .arch aarch64
        ; ldr w16, [x16]
        ; cbz w16, =>miss
        ; =>held
    );
}

/// `x16` = first word of the bank the field key in `W(index)` selects on the
/// object at `X(holder)`; `X(index)` becomes the bank-relative word index.
/// A missing overflow slab branches to `miss`. Clobbers `x17`.
fn emit_field_bank(
    ops: &mut Assembler,
    view: &JitCompileSnapshot,
    holder: u8,
    index: u8,
    miss: DynamicLabel,
) {
    let inline = ops.new_dynamic_label();
    let ready = ops.new_dynamic_label();
    let layout = view.field_layout;
    dynasm!(ops ; .arch aarch64
        ; tbnz W(index), 31, =>inline
        ; and WSP(index), W(index), #FieldLocation::INDEX_MASK
        ; ldr w16, [X(holder), layout.slab_handle_byte]
        ; cbz w16, =>miss
        ; and x17, X(holder), #0xffff_ffff_0000_0000
        ; add x16, x17, x16
        ; add x16, x16, #layout.slab_words_byte
        ; b =>ready
        ; =>inline
        ; and WSP(index), W(index), #FieldLocation::INDEX_MASK
        ; add x16, XSP(holder), #layout.inline_values_byte
        ; =>ready
    );
}

/// Complete a load through the slot at `X(slot)`: on a hit `X(destination)`
/// holds the value and control jumps to `done`. `destination` may alias the
/// receiver; it is written only after every check.
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
    dynasm!(ops ; .arch aarch64
        ; ldrb w16, [X(entry), IC.entry_kind_byte]
        ; cmp w16, Kind::OwnField as u32
        ; b.eq =>own
        ; cmp w16, Kind::PrototypeField as u32
        ; b.eq =>prototype
        ; cmp w16, Kind::NonExistent as u32
        ; b.ne =>miss
    );
    emit_entry_proof(ops, entry, false, miss);
    emit_load_u64(ops, destination, VALUE_UNDEFINED);
    dynasm!(ops ; .arch aarch64 ; b =>done ; =>prototype);
    emit_entry_proof(ops, entry, true, miss);
    dynasm!(ops ; .arch aarch64
        ; and x17, X(receiver), #0xffff_ffff_0000_0000
        ; ldr W(holder), [X(entry), IC.entry_aux_byte]
        ; add X(holder), x17, X(holder)
        ; ldr W(holder), [X(holder), view.shape_prototype_byte]
        ; cbz W(holder), =>miss
        ; add X(holder), x17, X(holder)
        ; b =>field
        // An own field lives on the lookup-start object `selection` left in
        // the holder register.
        ; =>own
        ; =>field
        ; ldr W(index), [X(entry), IC.entry_field_byte]
    );
    emit_field_bank(ops, view, holder, index, miss);
    dynasm!(ops ; .arch aarch64
        ; ldr X(destination), [x16, X(index), lsl #3]
    );

    dynasm!(ops ; .arch aarch64 ; b =>done);
}

/// Complete a store of `X(value)` through the slot at `X(slot)`: on a hit
/// the field is written, an append has published its target shape, and
/// control jumps to `stored` with `W(child)` holding the compressed child
/// shape (zero for an overwrite), and `slot` naming the actual storage owner.
/// Misses preserve the source receiver and slot; the caller owns both barriers.
#[allow(clippy::too_many_arguments)]
pub(crate) fn emit_slot_store(
    ops: &mut Assembler,
    relocations: &mut crate::artifact::relocation::RelocationCapture,
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
    let inline = ops.new_dynamic_label();
    let publish = ops.new_dynamic_label();
    let layout = view.field_layout;
    emit_select_entry(
        ops,
        view,
        receiver,
        slot,
        [entry, index],
        Some(child),
        megamorphic,
        miss,
    );
    dynasm!(ops ; .arch aarch64
        ; ldrb w16, [X(entry), IC.entry_kind_byte]
        ; cmp w16, Kind::StoreField as u32
        ; b.eq =>own
        ; cmp w16, Kind::StoreTransition as u32
        ; b.ne =>miss
        ; ldr w16, [X(entry), IC.entry_shape_byte]
        ; tst w16, #Projection::KEY_MASK
        ; b.ne =>miss
    );
    // Append: the chain proof (absent for a `null` prototype, which the
    // receiver shape fixes), a resident target layout and storage, then the
    // write and the target shape's publication.
    emit_entry_proof(ops, entry, false, miss);
    // Code subscribed to the shape the receiver leaves: the runtime store
    // fires it, before anything is written.
    dynasm!(ops ; .arch aarch64
        ; and x17, X(receiver), #0xffff_ffff_0000_0000
        ; ldr w16, [X(receiver), view.object_shape_byte]
        ; add x16, x17, x16
        ; ldrb w16, [x16, view.shape_watched_byte]
        ; tbnz w16, otter_vm::code_dependencies::ShapeFact::Stable.bit(), =>miss
        ; ldr W(child), [X(entry), IC.entry_aux_byte]
        ; cbz W(child), =>miss
    );
    dynasm!(ops ; .arch aarch64
        ; ldr W(index), [X(entry), IC.entry_field_byte]
        ; tbnz W(index), 31, =>inline
        ; and WSP(index), W(index), #FieldLocation::INDEX_MASK
        ; ldr w16, [X(receiver), layout.slab_handle_byte]
        ; cbz w16, =>miss
        ; and x17, X(receiver), #0xffff_ffff_0000_0000
        ; add x16, x17, x16
        ; ldr w17, [x16, layout.slab_capacity_byte]
        ; cmp W(index), w17
        ; b.hs =>miss
        ; add x16, x16, #layout.slab_words_byte
        ; str X(value), [x16, X(index), lsl #3]
        ; b =>publish
        ; =>inline
        ; and WSP(index), W(index), #FieldLocation::INDEX_MASK
        ; add x16, XSP(receiver), #layout.inline_values_byte
        ; str X(value), [x16, X(index), lsl #3]
        ; =>publish
        ; and x17, X(receiver), #0xffff_ffff_0000_0000
        ; ldr w16, [X(receiver), view.object_shape_byte]
        ; add x16, x17, x16
        ; strb wzr, [x16, view.shape_stable_byte]
        ; str W(child), [X(receiver), view.object_shape_byte]
        ; ldr W(entry), [X(entry), IC.entry_owner_byte]
    );
    super::field_type::emit_observe(ops, relocations, view, entry, value);
    dynasm!(ops ; .arch aarch64
        ; mov X(slot), X(receiver)
        ; b =>stored
        ; =>own
        ; ldr W(index), [X(entry), IC.entry_field_byte]
    );
    emit_field_bank(ops, view, child, index, miss);
    dynasm!(ops ; .arch aarch64
        ; str X(value), [x16, X(index), lsl #3]
        ; ldr W(entry), [X(entry), IC.entry_owner_byte]
    );
    super::field_type::emit_observe(ops, relocations, view, entry, value);
    dynasm!(ops ; .arch aarch64
        ; mov X(slot), X(child)
        ; mov W(child), wzr
        ; b =>stored
    );
}

#[cfg(test)]
#[path = "property_ic_tests.rs"]
mod tests;
