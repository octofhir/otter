//! Shared AArch64 probes of the isolate's one property-action table.
//!
//! # Contents
//! - One ordinary receiver/key decode and bounded set scan for both tiers.
//! - Independent live data loads, writable own stores and add-own stores.
//! - Current shape-selected inline/slab bank resolution before any effect.
//!
//! # Invariants
//! Four caller-declared GP temporaries and reserved x16/x17 are the only
//! scratch. Inputs remain unchanged through every pre-effect miss. Neither
//! probe allocates, collects or calls; no interior address escapes to a cold
//! transition. Load and store facts share a key, never a holder or slot. The
//! store writes its full value and optional child shape after every guard,
//! then leaves the compressed child (zero for overwrite) in the slot temporary.
//! The tier runs its existing child/value barriers without a replay edge.
//!
//! # See also
//! - `otter_vm::property_cache` owns the table, proofs and traced target.
//! - `crate::template::arm64::properties` owns the Template committed miss.
//! - `crate::graph::arm64` owns SSA homes, source identity and barriers.

use crate::{
    artifact::relocation::{RelocationCapture, RelocationTarget},
    entry::OBJECT_BODY_TYPE_TAG,
    template::arm64::values::{emit_load_symbol_u64, emit_load_u64},
};
use dynasmrt::{DynamicLabel, DynasmApi, DynasmLabelApi, aarch64::Assembler, dynasm};
use otter_vm::{
    JitCompileSnapshot,
    jit::{JitPropertyActionCache, PropertyLoadAction, PropertyStoreAction},
    object::ShapeState,
};

/// The probed property key: a compile-time atom, or one held in a register by
/// a shared per-code-object probe that serves many sites.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AtomOperand {
    Immediate(u32),
    Register(u8),
}

fn layout(view: &JitCompileSnapshot) -> Option<JitPropertyActionCache> {
    view.property_action_cache.filter(|cache| {
        cache.table_addr != 0
            && cache.entry_bytes != 0
            && cache.entry_bytes < 4096
            && cache.ways != 0
            && cache.ways <= u32::from(u16::MAX)
            && cache.hash_shift < 64
    })
}
fn registers(inputs: &[u8], temps: [u8; 4]) {
    debug_assert!(inputs.iter().all(|r| *r < 31 && !matches!(*r, 16 | 17)));
    debug_assert!(
        temps
            .iter()
            .all(|r| *r < 31 && !matches!(*r, 16 | 17) && !inputs.contains(r))
    );
    for (i, r) in temps.iter().enumerate() {
        debug_assert!(!temps[..i].contains(r));
    }
}

/// Decode the receiver and select the exact key once. The temporaries become
/// its shape address, immutable ID, matched way and exhausted scan budget.
#[allow(clippy::too_many_arguments)]
fn key(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    view: &JitCompileSnapshot,
    cache: JitPropertyActionCache,
    atom: AtomOperand,
    receiver: u8,
    [holder, slot, entry, base]: [u8; 4],
    store: bool,
    miss: DynamicLabel,
) {
    emit_load_u64(ops, 16, otter_vm::value::tag::NOT_CELL_MASK);
    dynasm!(ops ; .arch aarch64
        ; tst X(receiver), x16 ; b.ne =>miss ; cbz X(receiver), =>miss
        ; ldrb w16, [X(receiver)] ; cmp w16, #OBJECT_BODY_TYPE_TAG ; b.ne =>miss
        ; and x16, X(receiver), #0xffff_ffff_0000_0000
        ; ldr W(holder), [X(receiver), view.object_shape_byte] ; cbz W(holder), =>miss
        ; add X(holder), x16, X(holder) ; ldrb w17, [X(holder), view.shape_state_byte]);
    let mask = ShapeState::DICTIONARY_MASK
        | ShapeState::OPAQUE_LOOKUP_MASK
        | if store { ShapeState::PROTOTYPE_MASK } else { 0 };
    emit_load_u64(ops, 16, u64::from(mask));
    dynasm!(ops ; .arch aarch64 ; tst w17, w16 ; b.ne =>miss
        ; ldr X(slot), [X(holder), cache.shape_id_byte]);
    emit_load_u64(ops, 17, cache.hash_shape_multiplier);
    dynasm!(ops ; .arch aarch64 ; mul X(entry), X(slot), x17);
    match atom {
        AtomOperand::Immediate(atom) => emit_load_u64(
            ops,
            17,
            u64::from(atom).wrapping_mul(cache.hash_atom_multiplier),
        ),
        AtomOperand::Register(atom) => {
            emit_load_u64(ops, 17, cache.hash_atom_multiplier);
            dynasm!(ops ; .arch aarch64 ; mov W(base), W(atom) ; mul x17, X(base), x17);
        }
    }
    dynasm!(ops ; .arch aarch64 ; eor X(entry), X(entry), x17 ; lsr X(entry), X(entry), #u32::from(cache.hash_shift));
    emit_load_u64(ops, 17, u64::from(cache.set_mask));
    dynasm!(ops ; .arch aarch64 ; and X(entry), X(entry), x17);
    emit_load_u64(
        ops,
        17,
        u64::from(cache.entry_bytes) * u64::from(cache.ways),
    );
    dynasm!(ops ; .arch aarch64 ; mul X(entry), X(entry), x17);
    emit_load_symbol_u64(
        ops,
        relocations,
        17,
        cache.table_addr as u64,
        RelocationTarget::PropertyActionCacheTable,
    );
    dynasm!(ops ; .arch aarch64 ; add X(entry), X(entry), x17);
    emit_load_u64(ops, base, u64::from(cache.ways));
    let way = ops.new_dynamic_label();
    let next = ops.new_dynamic_label();
    let found = ops.new_dynamic_label();
    dynasm!(ops ; .arch aarch64 ; =>way
        ; ldr x16, [X(entry), cache.receiver_shape_id_byte] ; cmp x16, X(slot) ; b.ne =>next
        ; ldr w16, [X(entry), cache.atom_byte]);
    match atom {
        AtomOperand::Immediate(atom) => {
            emit_load_u64(ops, 17, u64::from(atom));
            dynasm!(ops ; .arch aarch64 ; cmp w16, w17);
        }
        AtomOperand::Register(atom) => dynasm!(ops ; .arch aarch64 ; cmp w16, W(atom)),
    }
    dynasm!(ops ; .arch aarch64 ; b.eq =>found
        ; =>next ; add XSP(entry), XSP(entry), #cache.entry_bytes
        ; subs W(base), WSP(base), #1 ; b.ne =>way ; b =>miss ; =>found);
}

/// Resolve a flat slot into a bank-relative index and current bank address.
fn bank(
    ops: &mut Assembler,
    view: &JitCompileSnapshot,
    holder: u8,
    slot: u8,
    base: u8,
    miss: DynamicLabel,
) {
    let inline = ops.new_dynamic_label();
    let ready = ops.new_dynamic_label();
    dynasm!(ops ; .arch aarch64
        ; and x16, X(holder), #0xffff_ffff_0000_0000
        ; ldr w17, [X(holder), view.object_shape_byte] ; add x17, x16, x17
        ; ldrb w17, [x17, view.shape_inline_capacity_byte]
        ; cmp W(slot), w17 ; b.lo =>inline ; sub W(slot), W(slot), w17
        ; ldr W(base), [X(holder), view.field_layout.slab_handle_byte] ; cbz W(base), =>miss
        ; add X(base), x16, X(base) ; ldr w16, [X(base), view.field_layout.slab_capacity_byte]
        ; cmp W(slot), w16 ; b.hs =>miss
        ; add XSP(base), XSP(base), #view.field_layout.slab_words_byte ; b =>ready
        ; =>inline ; add XSP(base), XSP(holder), #view.field_layout.inline_values_byte ; =>ready);
}

/// Complete one own/inherited data load or branch before effects. The result
/// may alias the receiver and is written only after every guard succeeds.
#[allow(clippy::too_many_arguments)]
pub(crate) fn emit_load(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    view: &JitCompileSnapshot,
    atom: AtomOperand,
    receiver: u8,
    temps @ [holder, slot, entry, base]: [u8; 4],
    destination: u8,
    miss: DynamicLabel,
) {
    registers(&[receiver], temps);
    if let AtomOperand::Register(atom) = atom {
        debug_assert!(atom != receiver && !temps.contains(&atom) && !matches!(atom, 16 | 17));
    }
    let Some(cache) = layout(view) else {
        dynasm!(ops ; .arch aarch64 ; b =>miss);
        return;
    };
    key(
        ops,
        relocations,
        view,
        cache,
        atom,
        receiver,
        temps,
        false,
        miss,
    );
    let own = ops.new_dynamic_label();
    let held = ops.new_dynamic_label();
    dynasm!(ops ; .arch aarch64
        ; ldrb w16, [X(entry), cache.load_action_byte]
        ; cmp w16, #PropertyLoadAction::OwnData as u32 ; b.eq =>own
        ; cmp w16, #PropertyLoadAction::InheritedData as u32 ; b.ne =>miss
        ; ldr x16, [X(entry), cache.load_validity_byte] ; cbz x16, =>miss
        ; ldar w16, [x16] ; cbz w16, =>miss
        ; and x17, X(receiver), #0xffff_ffff_0000_0000
        ; ldr w16, [X(entry), cache.holder_root_byte] ; cbz w16, =>miss
        ; add x16, x17, x16
        ; ldr W(holder), [x16, view.shape_prototype_byte] ; cbz W(holder), =>miss
        ; add X(holder), x17, X(holder)
        ; ldrb w16, [X(holder)] ; cmp w16, #OBJECT_BODY_TYPE_TAG ; b.ne =>miss
        ; b =>held
        ; =>own ; mov X(holder), X(receiver)
        ; =>held ; ldr W(base), [X(holder), view.object_shape_byte]
        ; ldr w16, [X(entry), cache.holder_shape_byte] ; cbz w16, =>miss
        ; cmp W(base), w16 ; b.ne =>miss
        ; and x17, X(holder), #0xffff_ffff_0000_0000
        ; add X(base), x17, X(base)
        ; ldrb w16, [X(base), view.shape_state_byte]);
    emit_load_u64(
        ops,
        17,
        u64::from(ShapeState::DICTIONARY_MASK | ShapeState::OPAQUE_LOOKUP_MASK),
    );
    dynasm!(ops ; .arch aarch64 ; tst w16, w17 ; b.ne =>miss
        ; ldr x16, [X(base), cache.shape_id_byte]
        ; ldr x17, [X(entry), cache.holder_shape_id_byte] ; cmp x16, x17 ; b.ne =>miss
        ; ldrh W(slot), [X(entry), cache.load_slot_byte]);
    bank(ops, view, holder, slot, base, miss);
    dynasm!(ops ; .arch aarch64 ; ldr X(destination), [X(base), X(slot), lsl #otter_vm::object::FieldLocation::INDEX_SHIFT]);
}

/// Complete an own writable store or no-allocation append. W(slot) is the
/// child shape, zero for overwrite. The caller owns both required barriers;
/// no branch to miss follows the first store.
#[allow(clippy::too_many_arguments)]
pub(crate) fn emit_store(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    view: &JitCompileSnapshot,
    atom: AtomOperand,
    [receiver, value]: [u8; 2],
    temps @ [holder, slot, entry, base]: [u8; 4],
    miss: DynamicLabel,
) {
    registers(&[receiver, value], temps);
    if let AtomOperand::Register(atom) = atom {
        debug_assert!(
            atom != receiver && atom != value && !temps.contains(&atom) && !matches!(atom, 16 | 17)
        );
    }
    let Some(cache) = layout(view) else {
        dynasm!(ops ; .arch aarch64 ; b =>miss);
        return;
    };
    key(
        ops,
        relocations,
        view,
        cache,
        atom,
        receiver,
        temps,
        true,
        miss,
    );
    let own = ops.new_dynamic_label();
    let chain = ops.new_dynamic_label();
    let done = ops.new_dynamic_label();
    dynasm!(ops ; .arch aarch64
        ; ldrb w16, [X(entry), cache.store_action_byte]
        ; cmp w16, #PropertyStoreAction::OwnWritable as u32 ; b.eq =>own
        ; cmp w16, #PropertyStoreAction::AddOwn as u32 ; b.ne =>miss
        ; ldr w16, [X(entry), cache.target_shape_byte] ; cbz w16, =>miss
        ; ldr x16, [X(entry), cache.store_validity_byte] ; cbz x16, =>chain
        ; ldar w16, [x16] ; cbz w16, =>miss
        ; =>chain ; ldrb w16, [X(holder), view.shape_state_byte]
        ; tst w16, #u32::from(ShapeState::EXTENSIBLE_MASK) ; b.eq =>miss
        ; and x17, X(receiver), #0xffff_ffff_0000_0000
        ; ldr W(holder), [X(entry), cache.target_shape_byte] ; add X(holder), x17, X(holder)
        ; ldr x16, [X(holder), cache.shape_id_byte]
        ; ldr x17, [X(entry), cache.target_shape_id_byte] ; cmp x16, x17 ; b.ne =>miss
        ; ldrh W(slot), [X(entry), cache.store_slot_byte]);
    bank(ops, view, receiver, slot, base, miss);
    dynasm!(ops ; .arch aarch64
        ; str X(value), [X(base), X(slot), lsl #otter_vm::object::FieldLocation::INDEX_SHIFT]
        ; ldr W(slot), [X(entry), cache.target_shape_byte]
        ; str W(slot), [X(receiver), view.object_shape_byte] ; b =>done
        ; =>own ; ldrh W(slot), [X(entry), cache.store_slot_byte]);
    bank(ops, view, receiver, slot, base, miss);
    dynasm!(ops ; .arch aarch64
        ; str X(value), [X(base), X(slot), lsl #otter_vm::object::FieldLocation::INDEX_SHIFT]
        ; mov W(slot), wzr ; =>done);
}

#[cfg(test)]
mod tests;
