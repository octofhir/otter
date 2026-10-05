//! AArch64 machine handlers over one named-property IC slot, shared by the
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
//!   `x16`/`x17`. The receiver, value and slot registers survive every miss.
//! - Handler checks precede the first effect; a miss has no effects and
//!   allocates nothing. A megamorphic slot branches to its own label so the
//!   caller can probe the isolate's shared action table.
//! - Feedback is read from the slot at run time; nothing is baked.
//!
//! # See also
//! - `otter_vm::property_ic` owns the slot layout and handler kinds.
//! - `crate::arm64::property_actions` owns the shared-table probe.

use dynasmrt::{DynamicLabel, DynasmApi, DynasmLabelApi, aarch64::Assembler, dynasm};
use otter_vm::JitCompileSnapshot;
use otter_vm::jit::{
    PROPERTY_IC_FUNCTION_RECEIVER_KEY_BIT as FUNCTION_RECEIVER_KEY_BIT, PROPERTY_IC_LAYOUT as IC,
    PropertyIcHandlerKind as Kind,
};

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

/// Select the entry of the receiver's map in the slot at `X(slot)`:
/// `X(entry)` addresses it on fall-through. An ordinary object's map is its
/// shape. With `start` given, a closure receiver whose property bag carries
/// its own `[[Prototype]]` (bag present, no override, default realm active)
/// selects by its bag's shape keyed with the function bit, and `X(start)`
/// becomes the lookup-start object (the receiver itself for an ordinary
/// object). A megamorphic slot branches to `megamorphic`, anything else to
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
    emit_cell_test(ops, receiver, 16, CellTest::IsNotCell, miss);
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
        Some(start) => {
            let function = ops.new_dynamic_label();
            let layout = view.closure_call_layout;
            let qualifying = u32::from(
                otter_vm::closure::CLOSURE_LOOKUP_OWN_PROPS
                    | otter_vm::closure::CLOSURE_LOOKUP_PROTO_OVERRIDE,
            );
            dynasm!(ops ; .arch aarch64
                ; b.ne =>function
                ; ldr w17, [X(receiver), view.object_shape_byte]
                ; mov X(start), X(receiver)
                ; b =>keyed
                ; =>function
                ; cmp w16, u32::from(otter_vm::closure::JS_CLOSURE_BODY_TYPE_TAG)
                ; b.ne =>miss
                ; ldrb w16, [X(receiver), otter_vm::closure::CLOSURE_NAMED_LOOKUP_BYTE]
                ; and w16, w16, #qualifying
                ; cmp w16, u32::from(otter_vm::closure::CLOSURE_LOOKUP_OWN_PROPS)
                ; b.ne =>miss
                ; ldr x16, [x20, THREAD_OFFSET]
                ; ldr x16, [x16, VM_THREAD_ACTIVE_REALM_CELL_OFFSET]
                ; cbz x16, =>miss
                ; ldr w16, [x16]
                ; cbnz w16, =>miss
                ; ldr w16, [X(receiver), layout.rare_byte]
                ; cbz w16, =>miss
                ; and x17, X(receiver), #0xffff_ffff_0000_0000
                ; add x16, x17, x16
                ; ldr W(start), [x16, layout.own_props_byte]
                ; cbz W(start), =>miss
                ; add X(start), x17, X(start)
                ; ldr w17, [X(start), view.object_shape_byte]
                ; orr w17, w17, #FUNCTION_RECEIVER_KEY_BIT
            );
        }
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
        ; =>found
    );
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
    dynasm!(ops ; .arch aarch64
        ; ldar w16, [x16]
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
        ; ldr w16, [X(holder), layout.slab_handle_byte]
        ; cbz w16, =>miss
        ; and x17, X(holder), #0xffff_ffff_0000_0000
        ; add x16, x17, x16
        ; add x16, x16, #layout.slab_words_byte
        ; b =>ready
        ; =>inline
        ; and WSP(index), W(index), #0x7fff_ffff
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
        ; b =>done
    );
}

/// Complete a store of `X(value)` through the slot at `X(slot)`: on a hit
/// the field is written, an append has published its target shape, and
/// control jumps to `stored` with `W(child)` holding the compressed child
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
    let inline = ops.new_dynamic_label();
    let publish = ops.new_dynamic_label();
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
    dynasm!(ops ; .arch aarch64
        ; ldrb w16, [X(entry), IC.entry_kind_byte]
        ; cmp w16, Kind::StoreField as u32
        ; b.eq =>own
        ; cmp w16, Kind::StoreTransition as u32
        ; b.ne =>miss
    );
    // Append: the chain proof (absent for a `null` prototype, which the
    // receiver shape fixes), a resident target layout and storage, then the
    // write and the target shape's publication.
    emit_entry_proof(ops, entry, false, miss);
    dynasm!(ops ; .arch aarch64
        ; ldr W(child), [X(entry), IC.entry_aux_byte]
        ; cbz W(child), =>miss
        ; ldr W(index), [X(entry), IC.entry_field_byte]
        ; tbnz W(index), 31, =>inline
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
        ; and WSP(index), W(index), #0x7fff_ffff
        ; add x16, XSP(receiver), #layout.inline_values_byte
        ; str X(value), [x16, X(index), lsl #3]
        ; =>publish
        ; str W(child), [X(receiver), view.object_shape_byte]
        ; b =>stored
        ; =>own
        ; ldr W(index), [X(entry), IC.entry_field_byte]
    );
    emit_field_bank(ops, view, receiver, index, miss);
    dynasm!(ops ; .arch aarch64
        ; str X(value), [x16, X(index), lsl #3]
        ; mov W(child), wzr
        ; b =>stored
    );
}
