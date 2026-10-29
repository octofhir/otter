//! Shared AArch64 probes of the isolate's one property-action table.
//!
//! # Contents
//! - One ordinary receiver/key decode and bounded set scan for both tiers.
//! - Independent live data loads, absent-key loads, writable own stores and
//!   add-own stores.
//! - Current shape-selected inline/slab bank resolution before any effect.
//!
//! # Invariants
//! Caller-declared GP temporaries and reserved x16/x17 are the only
//! scratch. Inputs remain unchanged through every pre-effect miss. Neither
//! probe allocates or collects; field-summary membership misses use a
//! noncollecting metadata leaf. No storage address escapes to a cold transition. Load and store facts share a key, never a holder or slot. The
//! store writes its full value and optional child shape after every guard,
//! then leaves the compressed child (zero for overwrite) in the slot temporary.
//! An append clears the source shape's stability byte before publishing its child.
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
    jit::slot_word,
    value::tag,
};

const SLOT_WORD_SLAB_BIT: u32 = slot_word::SLAB.trailing_zeros();

/// `X(register)` = `value`: one move when that encodes it, else one load of
/// its literal-pool word where the code object has a pool.
fn load_constant(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    register: u8,
    value: u64,
) {
    if crate::template::arm64::values::load_u64_instructions(value) == 1
        || !relocations.emit_constant_load(ops, register, value)
    {
        emit_load_u64(ops, register, value);
    }
}

fn layout(view: &JitCompileSnapshot) -> Option<JitPropertyActionCache> {
    view.property_action_cache.filter(|cache| {
        cache.table_addr != 0
            && cache.entry_bytes != 0
            && cache.entry_bytes < 4096
            && cache.ways != 0
            && cache.ways <= u32::from(u16::MAX)
            && (cache.set_mask + 1).is_power_of_two()
            && u32::from(cache.hash_shift) + (cache.set_mask + 1).trailing_zeros() == 64
            && cache.hash_atom_shift < 64
            && set_stride(*cache).is_some()
    })
}

/// The bytes of one set as `odd << shift` with `odd` one of 1, 3, 5 or 9, so
/// a set index scales into the table with shifted adds alone.
fn set_stride(cache: JitPropertyActionCache) -> Option<(u32, u32)> {
    let stride = u64::from(cache.entry_bytes) * u64::from(cache.ways);
    let shift = stride.trailing_zeros();
    matches!(stride >> shift, 1 | 3 | 5 | 9).then_some(((stride >> shift) as u32, shift))
}

/// `X(entry)` = the first entry of the set the key `(X(shape_id), W(atom))`
/// hashes to: one multiply of the shape identity XOR the shifted atom, whose
/// top bits are the set index, scaled by shifted adds. Clobbers `x17`.
fn emit_set_address(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    cache: JitPropertyActionCache,
    [shape_id, atom]: [u8; 2],
    entry: u8,
) {
    let (odd, shift) = set_stride(cache).expect("validated set stride");
    if cache.set_mask == 0 {
        dynasm!(ops ; .arch aarch64 ; mov X(entry), xzr);
    } else {
        load_constant(ops, relocations, 17, cache.hash_multiplier);
        dynasm!(ops ; .arch aarch64
            ; eor X(entry), X(shape_id), X(atom), lsl u32::from(cache.hash_atom_shift)
            ; mul X(entry), X(entry), x17
            ; lsr X(entry), X(entry), u32::from(cache.hash_shift));
        match odd {
            3 => dynasm!(ops ; .arch aarch64 ; add X(entry), X(entry), X(entry), lsl 1),
            5 => dynasm!(ops ; .arch aarch64 ; add X(entry), X(entry), X(entry), lsl 2),
            9 => dynasm!(ops ; .arch aarch64 ; add X(entry), X(entry), X(entry), lsl 3),
            _ => {}
        }
    }
    emit_load_symbol_u64(
        ops,
        relocations,
        17,
        cache.table_addr as u64,
        RelocationTarget::PropertyActionCacheTable,
    );
    dynasm!(ops ; .arch aarch64 ; add X(entry), x17, X(entry), lsl shift);
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
    atom: u8,
    receiver: u8,
    [holder, slot, entry, base]: [u8; 4],
    miss: DynamicLabel,
) {
    dynasm!(ops ; .arch aarch64
        ; tst X(receiver), #0xfffe_0000_0000_0007 ; b.ne =>miss ; cbz X(receiver), =>miss
        ; ldrb w16, [X(receiver)] ; cmp w16, #OBJECT_BODY_TYPE_TAG ; b.ne =>miss
        ; and x16, X(receiver), #0xffff_ffff_0000_0000
        ; ldr W(holder), [X(receiver), view.object_shape_byte] ; cbz W(holder), =>miss
        ; add X(holder), x16, X(holder));
    // The table records loads only for shaped receivers with ordinary
    // lookup and stores only for shapes whose stores stay in place: a
    // dictionary, opaque or prototype receiver's shape id keys no such fact,
    // and shape state is immutable, so neither probe tests state.
    dynasm!(ops ; .arch aarch64 ; ldr X(slot), [X(holder), cache.shape_id_byte]);
    emit_set_address(ops, relocations, cache, [slot, atom], entry);
    let found = ops.new_dynamic_label();
    let paired = cache.atom_byte == cache.receiver_shape_id_byte + 8
        && cache.receiver_shape_id_byte.is_multiple_of(8)
        && cache.ways <= 8
        && (cache.ways - 1) * cache.entry_bytes + cache.receiver_shape_id_byte <= 504;
    if paired {
        // Every way of the set is one pair load of its key and a compare
        // chain; a hit beyond the first way rebases the entry address.
        let mut rebase = Vec::new();
        for way in 0..cache.ways {
            let offset = way * cache.entry_bytes + cache.receiver_shape_id_byte;
            let hit = if way == 0 {
                found
            } else {
                let label = ops.new_dynamic_label();
                rebase.push((label, way * cache.entry_bytes));
                label
            };
            dynasm!(ops ; .arch aarch64
                ; ldp x16, x17, [X(entry), offset as i32]
                ; cmp x16, X(slot)
                ; ccmp w17, W(atom), #0, eq
                ; b.eq =>hit);
        }
        dynasm!(ops ; .arch aarch64 ; b =>miss);
        for (label, bytes) in rebase {
            dynasm!(ops ; .arch aarch64 ; =>label ; add XSP(entry), XSP(entry), bytes ; b =>found);
        }
        dynasm!(ops ; .arch aarch64 ; =>found);
        return;
    }
    emit_load_u64(ops, base, u64::from(cache.ways));
    let way = ops.new_dynamic_label();
    let next = ops.new_dynamic_label();
    dynasm!(ops ; .arch aarch64 ; =>way
        ; ldr x16, [X(entry), cache.receiver_shape_id_byte] ; cmp x16, X(slot) ; b.ne =>next
        ; ldr w16, [X(entry), cache.atom_byte]);
    dynasm!(ops ; .arch aarch64 ; cmp w16, W(atom));
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
    dynasm!(ops ; .arch aarch64
        ; and x16, X(holder), #0xffff_ffff_0000_0000
        ; ldr w17, [X(holder), view.object_shape_byte] ; add x17, x16, x17);
    bank_of_shape(ops, view, holder, 17, slot, base, miss);
}

/// [`bank`] of `holder` whose current shape's address is already in `shape`.
pub(super) fn bank_of_shape(
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
    dynasm!(ops ; .arch aarch64
        ; ldrb w17, [X(shape), view.shape_inline_capacity_byte]
        ; cmp W(slot), w17 ; b.lo =>inline ; sub W(slot), W(slot), w17
        ; and x16, X(holder), #0xffff_ffff_0000_0000
        ; ldr W(base), [X(holder), view.field_layout.slab_handle_byte] ; cbz W(base), =>miss
        ; add X(base), x16, X(base) ; ldr w16, [X(base), view.field_layout.slab_capacity_byte]
        ; cmp W(slot), w16 ; b.hs =>miss
        ; add XSP(base), XSP(base), #view.field_layout.slab_words_byte ; b =>ready
        ; =>inline ; add XSP(base), XSP(holder), #view.field_layout.inline_values_byte ; =>ready);
}

/// Select the site cache entry of `X(receiver)`'s shape, or go to
/// `fallback`: `X(word)` becomes the entry's slot word, `X(identity)` the
/// shape identity. The site's slot address is in `X(site)`; nothing but the
/// temporaries, `x16` and `x17` is written.
fn emit_site_entry(
    ops: &mut Assembler,
    view: &JitCompileSnapshot,
    cache: JitPropertyActionCache,
    receiver: u8,
    site: u8,
    [identity, table, word]: [u8; 3],
    fallback: DynamicLabel,
) {
    use otter_vm::jit::PROPERTY_IC_LAYOUT as IC;
    debug_assert_eq!(IC.site_cache_entry_bytes, 16);
    dynasm!(ops ; .arch aarch64
        ; tst X(receiver), #0xfffe_0000_0000_0007 ; b.ne =>fallback ; cbz X(receiver), =>fallback
        ; ldrb w16, [X(receiver)] ; cmp w16, #OBJECT_BODY_TYPE_TAG ; b.ne =>fallback
        ; ldr w16, [X(receiver), view.object_shape_byte] ; cbz w16, =>fallback
        ; and x17, X(receiver), #0xffff_ffff_0000_0000 ; add x16, x17, x16
        ; ldr X(identity), [x16, cache.shape_id_byte]
        ; ldr X(table), [X(site), IC.site_cache_byte] ; cbz X(table), =>fallback
        ; and XSP(word), X(identity), #u64::from(IC.site_cache_entries - 1)
        ; add X(table), X(table), X(word), lsl #4
        ; ldp x17, X(word), [X(table)]
        ; cmp x17, X(identity) ; b.ne =>fallback);
}

/// `X(destination)` = the named property of `X(receiver)` from the cache of
/// the site whose slot address is in `X(site)`, inline-bank own data only;
/// anything else goes to `fallback` with nothing written but the
/// temporaries, `x16` and `x17`.
pub(crate) fn emit_site_cache_load(
    ops: &mut Assembler,
    view: &JitCompileSnapshot,
    receiver: u8,
    site: u8,
    temps @ [_, _, word]: [u8; 3],
    destination: u8,
    fallback: DynamicLabel,
) {
    let Some(cache) = layout(view) else {
        dynasm!(ops ; .arch aarch64 ; b =>fallback);
        return;
    };
    emit_site_entry(ops, view, cache, receiver, site, temps, fallback);
    dynasm!(ops ; .arch aarch64
        ; tbnz W(word), SLOT_WORD_SLAB_BIT, =>fallback
        ; and x16, X(word), #u64::from(slot_word::OFFSET_MASK)
        ; ldr X(destination), [X(receiver), x16]);
}

/// `X(destination)` = `X(receiver)[X(key)]` from the cache of the keyed
/// site whose slot address is in `X(site)` (zero for none), for a string
/// or symbol key with a recorded atom and an inline-bank own data slot;
/// anything else goes to `fallback` with nothing written but the
/// temporaries, `x16` and `x17`.
#[allow(clippy::too_many_arguments)]
pub(crate) fn emit_keyed_site_cache_load(
    ops: &mut Assembler,
    view: &JitCompileSnapshot,
    [receiver, key]: [u8; 2],
    site: u8,
    [atom, identity, table, word]: [u8; 4],
    destination: u8,
    fallback: DynamicLabel,
) {
    use otter_vm::jit::PROPERTY_IC_LAYOUT as IC;
    let Some(cache) = layout(view) else {
        dynasm!(ops ; .arch aarch64 ; b =>fallback);
        return;
    };
    let symbol = ops.new_dynamic_label();
    let ready = ops.new_dynamic_label();
    dynasm!(ops ; .arch aarch64
        ; cbz X(site), =>fallback
        ; ldr X(table), [X(site), IC.site_cache_byte] ; cbz X(table), =>fallback
        // The key is a string or a symbol: its recorded atom, or a decline.
        ; ldrb w16, [X(key)]
        ; cmp w16, u32::from(view.string_layout.string_type_tag) ; b.ne =>symbol
        ; ldr W(atom), [X(key), view.string_layout.atom_byte] ; cbz W(atom), =>fallback
        ; sub WSP(atom), WSP(atom), #1 ; b =>ready
        ; =>symbol
        ; ldr W(atom), [X(key), otter_vm::symbol::SYMBOL_ATOM_BYTE]
        ; cmn WSP(atom), #1 ; b.eq =>fallback
        ; =>ready
        ; tst X(receiver), #0xfffe_0000_0000_0007 ; b.ne =>fallback ; cbz X(receiver), =>fallback
        ; ldrb w16, [X(receiver)] ; cmp w16, #OBJECT_BODY_TYPE_TAG ; b.ne =>fallback
        ; ldr w16, [X(receiver), view.object_shape_byte] ; cbz w16, =>fallback
        ; and x17, X(receiver), #0xffff_ffff_0000_0000 ; add x16, x17, x16
        ; ldr X(identity), [x16, cache.shape_id_byte]
        ; eor X(word), X(identity), X(atom)
        ; and XSP(word), X(word), #u64::from(IC.site_cache_entries - 1)
        ; add X(table), X(table), X(word), lsl #4
        ; ldp x17, X(word), [X(table)]
        ; cmp x17, X(identity) ; b.ne =>fallback
        ; cmp X(atom), X(word), lsr #32 ; b.ne =>fallback
        ; tbnz W(word), SLOT_WORD_SLAB_BIT, =>fallback
        ; and x16, X(word), #u64::from(slot_word::OFFSET_MASK)
        ; ldr X(destination), [X(receiver), x16]);
}

/// Store `X(value)` into the named property of `X(receiver)` through the
/// cache of the site whose slot address is in `X(site)`: an inline-bank own
/// overwrite of a value its field already covers. Anything else goes to
/// `fallback` before anything is written; the caller owns the value
/// barrier.
pub(crate) fn emit_site_cache_store(
    ops: &mut Assembler,
    view: &JitCompileSnapshot,
    [receiver, value]: [u8; 2],
    site: u8,
    temps @ [_, _, word]: [u8; 3],
    fallback: DynamicLabel,
) {
    let Some(cache) = layout(view) else {
        dynasm!(ops ; .arch aarch64 ; b =>fallback);
        return;
    };
    emit_site_entry(ops, view, cache, receiver, site, temps, fallback);
    emit_covered(ops, value, word, fallback, fallback);
    dynasm!(ops ; .arch aarch64
        ; tbnz W(word), SLOT_WORD_SLAB_BIT, =>fallback
        ; and x16, X(word), #u64::from(slot_word::OFFSET_MASK)
        ; str X(value), [X(receiver), x16]);
}

/// The site whose cache a shared-table hit fills.
#[derive(Clone, Copy)]
pub(crate) enum SiteFill {
    /// A named site whose slot address the register holds.
    Named(u8),
    /// A keyed site whose slot address (zero for none) and key atom the
    /// registers hold.
    Keyed {
        /// The slot address, or zero.
        site: u8,
        /// The key's atom.
        atom: u8,
    },
}

/// Copy the matched row's receiver shape identity and `W(word)` into the
/// cache of the site `fill` names, when it has one. Writes `X(scratch)`,
/// `x16` and `x17`.
fn emit_site_fill(
    ops: &mut Assembler,
    cache: JitPropertyActionCache,
    fill: SiteFill,
    entry: u8,
    word: u8,
    scratch: u8,
) {
    use otter_vm::jit::PROPERTY_IC_LAYOUT as IC;
    debug_assert_eq!(IC.site_cache_entry_bytes, 16);
    let mask = u64::from(IC.site_cache_entries - 1);
    match fill {
        SiteFill::Named(site) => dynasm!(ops ; .arch aarch64
            ; ldr x16, [X(site), IC.site_cache_byte] ; cbz x16, >skip
            ; ldr x17, [X(entry), cache.receiver_shape_id_byte]
            ; and XSP(scratch), x17, #mask
            ; add x16, x16, X(scratch), lsl #4
            ; stp x17, X(word), [x16]
            ; skip:),
        SiteFill::Keyed { site, atom } => dynasm!(ops ; .arch aarch64
            ; cbz X(site), >skip
            ; ldr x16, [X(site), IC.site_cache_byte] ; cbz x16, >skip
            ; ldr x17, [X(entry), cache.receiver_shape_id_byte]
            ; eor X(scratch), x17, X(atom)
            ; and XSP(scratch), X(scratch), #mask
            ; add x16, x16, X(scratch), lsl #4
            ; orr X(scratch), X(word), X(atom), lsl #32
            ; stp x17, X(scratch), [x16]
            ; skip:),
    }
}

/// `X(destination)` = the slot of `holder` that `W(word)` resolves
/// ([`otter_vm::property_cache::slot_word`]); a slab slot beyond the
/// object's current slab goes to `miss`, before the destination is written.
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
    dynasm!(ops ; .arch aarch64
        ; cbz W(word), =>miss
        ; tbnz W(word), SLOT_WORD_SLAB_BIT, =>slab
        ; and x16, X(word), #u64::from(slot_word::OFFSET_MASK)
        ; ldr X(destination), [X(holder), x16]
        ; b =>done
        ; =>slab);
    emit_word_slab(ops, view, holder, word, base, miss);
    dynasm!(ops ; .arch aarch64 ; ldr X(destination), [X(base), X(word)] ; =>done);
}

/// `X(base)` = `holder`'s slab words and `X(word)` the byte offset of the
/// slab slot `W(word)` resolves, inside the slab's capacity, else `miss`.
fn emit_word_slab(
    ops: &mut Assembler,
    view: &JitCompileSnapshot,
    holder: u8,
    word: u8,
    base: u8,
    miss: DynamicLabel,
) {
    dynasm!(ops ; .arch aarch64
        ; and WSP(word), W(word), #slot_word::OFFSET_MASK
        ; and x16, X(holder), #0xffff_ffff_0000_0000
        ; ldr W(base), [X(holder), view.field_layout.slab_handle_byte] ; cbz W(base), =>miss
        ; add X(base), x16, X(base) ; ldr w16, [X(base), view.field_layout.slab_capacity_byte]
        ; cmp W(word), w16, lsl #3 ; b.hs =>miss
        ; add XSP(base), XSP(base), #view.field_layout.slab_words_byte);
}

/// Fall through when `W(word)` covers `X(value)`'s class, so the store
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
    dynasm!(ops ; .arch aarch64
        ; tbnz W(word), slot_word::ANY.trailing_zeros(), =>covered
        ; asr x16, X(value), tag::NUMBER_TAG.trailing_zeros()
        ; cmn x16, 1 ; b.ne =>number
        ; tbnz W(word), slot_word::INT32.trailing_zeros(), =>covered
        ; b =>uncovered
        ; =>number
        ; tst X(value), tag::NUMBER_TAG ; b.eq =>other
        ; tbnz W(word), slot_word::DOUBLE.trailing_zeros(), =>covered
        ; b =>uncovered
        ; =>other);
    emit_load_u64(ops, 16, crate::entry::VALUE_NULL);
    dynasm!(ops ; .arch aarch64
        ; cmp X(value), x16 ; b.ne >undefined
        ; tbnz W(word), slot_word::NULL.trailing_zeros(), =>covered
        ; b =>uncovered
        ; undefined:);
    emit_load_u64(ops, 16, crate::entry::VALUE_UNDEFINED);
    dynasm!(ops ; .arch aarch64
        ; cmp X(value), x16 ; b.ne =>uncovered
        ; tbnz W(word), slot_word::UNDEFINED.trailing_zeros(), =>covered
        ; =>uncovered
        ; tbnz W(word), slot_word::OBSERVE.trailing_zeros(), =>observe
        ; b =>miss
        ; =>covered);
}

/// Store `X(value)` into the slot of `receiver` that `W(word)` resolves; a
/// slab slot beyond the current slab goes to `miss` before anything is
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
    dynasm!(ops ; .arch aarch64
        ; tbnz W(word), SLOT_WORD_SLAB_BIT, =>slab
        ; and x16, X(word), #u64::from(slot_word::OFFSET_MASK)
        ; str X(value), [X(receiver), x16]
        ; b =>done
        ; =>slab);
    emit_word_slab(ops, view, receiver, word, base, miss);
    dynasm!(ops ; .arch aarch64 ; str X(value), [X(base), X(word)] ; =>done);
}

/// Complete one own/inherited data load or branch before effects. The result
/// may alias the receiver and is written only after every guard succeeds.
/// `own_only` accepts an own data slot alone: a projected storage object's
/// chain is not its source receiver's.
#[allow(clippy::too_many_arguments)]
pub(crate) fn emit_load(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    view: &JitCompileSnapshot,
    atom: u8,
    receiver: u8,
    temps: [u8; 4],
    destination: u8,
    own_only: bool,
    miss: DynamicLabel,
) {
    emit_site_load(
        ops,
        relocations,
        view,
        atom,
        receiver,
        temps,
        destination,
        own_only,
        None,
        miss,
    );
}

/// [`emit_load`] for a named site whose slot address `site` holds: an own
/// hit is also copied into the site's cache ([`emit_site_fill`]).
#[allow(clippy::too_many_arguments)]
pub(crate) fn emit_site_load(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    view: &JitCompileSnapshot,
    atom: u8,
    receiver: u8,
    temps @ [holder, slot, entry, base]: [u8; 4],
    destination: u8,
    own_only: bool,
    site: Option<SiteFill>,
    miss: DynamicLabel,
) {
    registers(&[receiver], temps);
    debug_assert!(atom != receiver && !temps.contains(&atom) && !matches!(atom, 16 | 17));
    let Some(cache) = layout(view) else {
        dynasm!(ops ; .arch aarch64 ; b =>miss);
        return;
    };
    key(ops, relocations, view, cache, atom, receiver, temps, miss);
    let own = ops.new_dynamic_label();
    let absent = ops.new_dynamic_label();
    let undefined = ops.new_dynamic_label();
    let canonical = ops.new_dynamic_label();
    let done = ops.new_dynamic_label();
    if own_only {
        // A projected storage object answers its own slots only: its chain
        // is not the source receiver's.
        dynasm!(ops ; .arch aarch64
            ; ldrb w16, [X(entry), cache.load_action_byte]
            ; cmp w16, #PropertyLoadAction::OwnData as u32 ; b.ne =>miss
            ; ldr W(slot), [X(entry), cache.load_word_byte]);
        emit_word_load(ops, view, receiver, slot, base, destination, miss);
        return;
    }
    dynasm!(ops ; .arch aarch64
        ; ldrb w16, [X(entry), cache.load_action_byte]
        ; cmp w16, #PropertyLoadAction::OwnData as u32 ; b.eq =>own
        ; cmp w16, #PropertyLoadAction::NonExistent as u32 ; b.eq =>absent
        ; cmp w16, #PropertyLoadAction::InheritedData as u32 ; b.ne =>miss
        ; ldr x16, [X(entry), cache.load_validity_byte] ; cbz x16, =>miss
        ; ldr w16, [x16] ; cbz w16, =>miss
        ; and x17, X(receiver), #0xffff_ffff_0000_0000
        ; ldr w16, [X(entry), cache.holder_root_byte] ; cbz w16, =>miss
        ; add x16, x17, x16
        ; ldr W(holder), [x16, view.shape_prototype_byte] ; cbz W(holder), =>miss
        ; add X(holder), x17, X(holder)
        ; ldrb w16, [X(holder)] ; cmp w16, #OBJECT_BODY_TYPE_TAG ; b.ne =>miss
        ; ldr W(base), [X(holder), view.object_shape_byte]
        ; ldr w16, [X(entry), cache.holder_shape_byte] ; cbz w16, =>miss
        ; cmp W(base), w16 ; b.ne =>miss
        ; and x17, X(holder), #0xffff_ffff_0000_0000
        ; add X(base), x17, X(base)
        // The recorded holder was shaped with ordinary lookup, so its never
        // reused shape id proves the holder's state as well as its layout.
        ; ldr x16, [X(base), cache.shape_id_byte]
        ; ldr x17, [X(entry), cache.holder_shape_id_byte] ; cmp x16, x17 ; b.ne =>miss
        ; ldr W(slot), [X(entry), cache.load_word_byte]);
    emit_word_load(ops, view, holder, slot, base, destination, miss);
    dynasm!(ops ; .arch aarch64
        ; b =>canonical
        // The matched key already proved the receiver's exact shape.
        ; =>own ; ldr W(slot), [X(entry), cache.load_word_byte]);
    if let Some(site) = site {
        emit_site_fill(ops, cache, site, entry, slot, base);
    }
    emit_word_load(ops, view, receiver, slot, base, destination, miss);
    dynasm!(ops ; .arch aarch64
        ; b =>canonical
        // Absent from the whole chain: the proof (none for a `null`
        // prototype the receiver shape fixes) authorizes `undefined`.
        ; =>absent
        ; ldr x16, [X(entry), cache.load_validity_byte] ; cbz x16, =>undefined
        ; ldr w16, [x16] ; cbz w16, =>miss
        ; =>undefined);
    emit_load_u64(ops, destination, crate::entry::VALUE_UNDEFINED);
    dynasm!(ops ; .arch aarch64
        ; b =>done
        ; =>canonical
    );

    dynasm!(ops ; .arch aarch64 ; =>done);
}

/// Complete an own writable store or no-allocation append. W(slot) is the
/// child shape, zero for overwrite. The caller owns both required barriers;
/// no branch to miss follows the first store. `own_only` accepts the
/// overwrite alone, for a projected storage object.
#[allow(clippy::too_many_arguments)]
pub(crate) fn emit_store(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    view: &JitCompileSnapshot,
    atom: u8,
    inputs: [u8; 2],
    temps: [u8; 4],
    own_only: bool,
    miss: DynamicLabel,
) {
    emit_site_store(
        ops,
        relocations,
        view,
        atom,
        inputs,
        temps,
        own_only,
        None,
        miss,
    );
}

/// [`emit_store`] for a named site whose slot address `site` holds: an own
/// overwrite's row is also copied into the site's cache
/// ([`emit_site_fill`]).
#[allow(clippy::too_many_arguments)]
pub(crate) fn emit_site_store(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    view: &JitCompileSnapshot,
    atom: u8,
    [receiver, value]: [u8; 2],
    temps @ [holder, slot, entry, base]: [u8; 4],
    own_only: bool,
    site: Option<SiteFill>,
    miss: DynamicLabel,
) {
    registers(&[receiver, value], temps);
    debug_assert!(
        atom != receiver && atom != value && !temps.contains(&atom) && !matches!(atom, 16 | 17)
    );
    let Some(cache) = layout(view) else {
        dynasm!(ops ; .arch aarch64 ; b =>miss);
        return;
    };
    key(ops, relocations, view, cache, atom, receiver, temps, miss);
    let own = ops.new_dynamic_label();
    let chain = ops.new_dynamic_label();
    let done = ops.new_dynamic_label();
    if own_only {
        dynasm!(ops ; .arch aarch64
            ; ldrb w16, [X(entry), cache.store_action_byte]
            ; cmp w16, #PropertyStoreAction::OwnWritable as u32 ; b.ne =>miss);
    }
    dynasm!(ops ; .arch aarch64
        ; ldrb w16, [X(entry), cache.store_action_byte]
        ; cmp w16, #PropertyStoreAction::OwnWritable as u32 ; b.eq =>own
        ; cmp w16, #PropertyStoreAction::AddOwn as u32 ; b.ne =>miss
        ; ldr w16, [X(entry), cache.target_shape_byte] ; cbz w16, =>miss
        ; ldr x16, [X(entry), cache.store_validity_byte] ; cbz x16, =>chain
        ; ldr w16, [x16] ; cbz w16, =>miss
        // The recorded source shape was extensible, which its immutable
        // state keeps.
        ; =>chain
        // Code subscribed to the shape the receiver leaves: the runtime
        // store fires it, before anything is written.
        ; ldrb w16, [X(holder), view.shape_watched_byte] ; tbnz w16, otter_vm::code_dependencies::ShapeFact::Stable.bit(), =>miss
        ; and x17, X(receiver), #0xffff_ffff_0000_0000
        ; ldr W(base), [X(entry), cache.target_shape_byte] ; add X(base), x17, X(base)
        ; ldr x16, [X(base), cache.shape_id_byte]
        ; ldr x17, [X(entry), cache.target_shape_id_byte] ; cmp x16, x17 ; b.ne =>miss
        ; ldr W(slot), [X(entry), cache.target_shape_byte]);
    dynasm!(ops ; .arch aarch64 ; ldrh W(slot), [X(entry), cache.store_slot_byte]);
    bank(ops, view, receiver, slot, base, miss);
    dynasm!(ops ; .arch aarch64
        ; str X(value), [X(base), X(slot), lsl #otter_vm::object::FieldLocation::INDEX_SHIFT]
        ; strb wzr, [X(holder), view.shape_stable_byte]
        ; ldr W(slot), [X(entry), cache.target_shape_byte]
        ; str W(slot), [X(receiver), view.object_shape_byte]
        ; ldr W(entry), [X(entry), cache.store_owner_byte]
    );
    super::field_type::emit_observe(ops, relocations, view, entry, value);
    // An overwrite of a value its field already covers needs no
    // observation; an uncovered one is observed only where the summary
    // tracks value shapes, and otherwise completes in the runtime.
    let observe = ops.new_dynamic_label();
    let overwritten = ops.new_dynamic_label();
    dynasm!(ops ; .arch aarch64 ; b =>done
        ; =>own ; ldr W(slot), [X(entry), cache.store_word_byte] ; cbz W(slot), =>miss);
    if let Some(site) = site {
        emit_site_fill(ops, cache, site, entry, slot, base);
    }
    emit_covered(ops, value, slot, observe, miss);
    emit_word_store(ops, view, receiver, value, slot, base, miss);
    dynasm!(ops ; .arch aarch64 ; b =>overwritten ; =>observe);
    emit_word_store(ops, view, receiver, value, slot, base, miss);
    dynasm!(ops ; .arch aarch64 ; ldr W(entry), [X(entry), cache.store_owner_byte]);
    super::field_type::emit_observe(ops, relocations, view, entry, value);
    dynasm!(ops ; .arch aarch64
        ; =>overwritten
        ; mov W(slot), wzr ; =>done);
}

#[cfg(test)]
mod tests;
