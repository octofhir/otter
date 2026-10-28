//! Store-property shape transitions and IC replay guards.
//!
//! This module is the internal contract between ordinary object storage and
//! monomorphic property inline caches. It owns the rules for when a named
//! `StoreProperty` may be replayed as a shape transition and which receiver /
//! prototype facts must be revalidated before replay.
//!
//! # Contents
//! - [`StorePropertyTransition`] — frozen replay record for one own-slot add.
//! - [`StorePropertyTransitionKind`] — explicit transition categories cached by
//!   StoreProperty ICs.
//! - [`capture_store_property_transition`] — apply a resolved `[[Set]]` data
//!   write and return replay metadata when the path is IC-compatible.
//! - [`replay_store_property_transition`] — validate guards and add the cached
//!   own data slot on a fresh matching receiver.
//! - [`shape_store`] — what a named store does to any object of one exact
//!   shape, from the shape, its interned children and the live chain alone.
//!
//! # Invariants
//! - Replay only applies to fast-shape ordinary objects.
//! - Replay guards run before sidecar or slab growth, so a failed probe is
//!   allocation-free and cannot relocate a caller-owned receiver handle.
//! - Capture and replay publish Value-valid words without changing layout for
//!   numeric representation; every structural guard precedes allocation or writes.
//! - Allocation failure after a matched transition propagates as OOM; it is
//!   never converted into a miss that could reuse a stale receiver.
//! - Receiver shapes fix prototype identity. A shared validity cell proves
//!   inherited absence or writable data; prototype mutation invalidates it
//!   before publication. A miss never traverses the old chain.
//! - Generated stores into watched prototypes enter the mutation boundary
//!   before committing, so they cannot bypass invalidation.
//! - Accessors, proxies, string wrapper objects, non-writable inherited data,
//!   and dictionary-compatible objects remain fallback paths. Writable
//!   inherited data at any depth is shadowed under the chain's proof.
//!
//! # See also
//! - [`crate::property_ic`]
//! - [`crate::property_dispatch`]

use super::prototype_validity::{PrototypeValidity, chain_validity};
use super::{
    JsObject, ObjectBody, ObjectPrototype, PropertyLookup, ShapeHandle, ShapeId, SlotMeta,
    lookup_own_atom, prototype_value_in,
};
use crate::Value;
use crate::property_atom::{AtomId, AtomizedPropertyKey};
use crate::rooting::RootScopeExt;
use std::cell::Cell;
use std::sync::Arc;

/// Atom-aware hidden-class transition for adding one ordinary own data slot.
#[derive(Debug, Clone)]
pub(crate) struct StorePropertyTransition {
    /// Shape observed before adding the property.
    pub(crate) from_shape_id: ShapeId,
    /// Atomized named-property key from the executable context.
    pub(crate) atom_id: AtomId,
    /// Child shape id reached by adding the property.
    pub(crate) to_shape_id: ShapeId,
    /// GC-managed child shape reached by adding the property.
    pub(crate) to_shape: Cell<ShapeHandle>,
    /// Transition category and replay guard.
    pub(crate) kind: StorePropertyTransitionKind,
    /// Slot offset added by this transition.
    pub(crate) slot: u16,
}

/// Explicit StoreProperty transition categories.
#[derive(Debug, Clone)]
pub(crate) enum StorePropertyTransitionKind {
    /// Existing receiver had `null` prototype and added a new own data slot.
    OwnAdd,
    /// An ordinary prototype chain had no inherited property of this name.
    PrototypeChainMissing {
        /// Shared proof invalidated by any mutation along the chain.
        validity: Arc<PrototypeValidity>,
    },
    /// A link of the ordinary prototype chain had writable data of this name;
    /// the appended own property shadows it.
    PrototypeWritableData {
        /// Shared proof includes descriptor and value mutations.
        validity: Arc<PrototypeValidity>,
    },
}

/// Apply a resolved data assignment and capture replay metadata.
///
/// Callers must only use this after ordinary `[[Set]]` selected
/// [`PropertyLookup`]-compatible data assignment. The helper deliberately
/// refuses unsupported object/prototype shapes instead of approximating.
#[cfg(test)]
pub(crate) fn capture_store_property_transition(
    obj: JsObject,
    heap: &mut otter_gc::GcHeap,
    key: AtomizedPropertyKey<'_>,
    value: &Value,
) -> Option<StorePropertyTransition> {
    // Symbol-keyed slots leave the hidden class through the ordinary
    // dictionary conversion, not this replay.
    if key.symbol_handle().is_some() || super::shape_body::has_symbol_keys(super::shape(obj, heap))
    {
        return None;
    }
    let mut obj = obj;
    let mut stored = *value;
    // A store can demote this object to dictionary mode, and demotion
    // writes through the sidecar. Reserved here, outside every payload
    // borrow, because creating it allocates.
    super::ensure_exotic_with_pending_values(&mut obj, heap, std::slice::from_mut(&mut stored))
        .ok()?;
    let index = heap.read_payload(obj, |body| super::body_property_count(heap, body));
    let slot = u16::try_from(index).ok()?;
    // Growing the slab can collect. Keep the incoming `Value` in the pending
    // root slice so its embedded GC offset is rewritten if that happens.
    super::reserve_slot_capacity(&mut obj, heap, index + 1, std::slice::from_mut(&mut stored))
        .ok()?;
    let kind = transition_kind(obj, heap, key)?;
    let from_shape_id = super::shape_id(obj, heap);
    let existing_offset =
        heap.read_payload(obj, |body| super::body_offset_of_atom(heap, body, key));
    // The demotion below materializes per-slot metadata; the table is
    // built here, outside the borrow, exactly as the runtime demote
    // paths do.
    let slot_metas = super::slot_metas_for_shape_transition(heap, obj, existing_offset);
    let slot_meta_table = super::slot_meta_table_for_install(
        &mut obj,
        heap,
        &slot_metas,
        index + 1,
        std::slice::from_mut(&mut stored),
    )
    .ok()?;
    let dictionary_keys = super::dictionary_keys_for_shape_transition(heap, obj, existing_offset);
    let dict_table = super::dict_keys_table_for_install(
        &mut obj,
        heap,
        &dictionary_keys,
        key.name(),
        std::slice::from_mut(&mut stored),
    )
    .ok()?;
    let transition = heap.with_payload(obj, |body| {
        if !is_fast_shape_body(body)
            || !body.extensible()
            || !transition_kind_matches_receiver_body(body, &kind)
        {
            return None;
        }
        if existing_offset.is_some() {
            return None;
        }
        let to_shape_id = super::next_shape_id();
        // A fast body enters dictionary mode: every slot-layout proof moves.
        body.enter_dictionary_mode_as(to_shape_id, true);
        if let Some(table) = dict_table {
            body.exotic_mut().dictionary_keys = table;
        }
        if let Some(table) = slot_meta_table {
            body.exotic_mut().slots = table;
        }
        super::dict_push_key(body, key.name().to_owned());
        body.push_slot(index, SlotMeta::data_default(), stored);
        Some(StorePropertyTransition {
            from_shape_id,
            atom_id: key.atom().id(),
            to_shape_id,
            to_shape: Cell::new(ShapeHandle::null()),
            kind,
            slot,
        })
    })?;
    super::record_slot_write(heap, obj, stored);
    Some(transition)
}

pub(crate) fn capture_store_property_transition_with_shape(
    obj: &mut JsObject,
    heap: &mut otter_gc::GcHeap,
    key: AtomizedPropertyKey<'_>,
    value: &mut Value,
    mut next_shape: ShapeHandle,
) -> Result<Option<StorePropertyTransition>, otter_gc::OutOfMemory> {
    let to_shape_count = heap.read_payload(next_shape, |s| s.property_count());
    // The appended slot's flat index is the new shape's last offset.
    let index = to_shape_count as usize - 1;
    let Ok(slot) = u16::try_from(index) else {
        return Ok(None);
    };
    if super::shape_body::parent_of(next_shape) != super::shape(*obj, heap) {
        return Ok(None);
    }
    let Some(kind) = transition_kind(*obj, heap, key) else {
        return Ok(None);
    };
    let from_shape_id = super::shape_id(*obj, heap);
    let existing_offset =
        heap.read_payload(*obj, |body| super::body_offset_of_atom(heap, body, key));
    if existing_offset.is_some() || !heap.read_payload(*obj, |body| body.extensible()) {
        return Ok(None);
    }
    let mut roots = otter_gc::RootScope::new(heap);
    // SAFETY: the actual receiver, canonical incoming word and proposed child
    // stay stationary through preparation and reservation, then publish without
    // another allocation. The immutable child is the exact publication target.
    unsafe {
        roots.add_object(obj);
        roots.add_value(value);
        roots.add_raw_slot((&mut next_shape as *mut ShapeHandle).cast());
    }
    super::reserve_slot_capacity(obj, heap, index + 1, std::slice::from_mut(value))?;
    let to_shape_id = heap.read_payload(next_shape, |s| s.id());
    let physical = *value;
    let transition = heap.with_payload(*obj, |body| {
        if !is_fast_shape_body(body)
            || !body.extensible()
            || !transition_kind_matches_receiver_body(body, &kind)
        {
            return None;
        }
        if existing_offset.is_some() {
            return None;
        }
        body.invalidate_prototype_proofs();
        body.adopt_shape(next_shape);
        body.push_slot(index, SlotMeta::data_default(), physical);
        Some(StorePropertyTransition {
            from_shape_id,
            atom_id: key.atom().id(),
            to_shape_id,
            to_shape: Cell::new(next_shape),
            kind,
            slot,
        })
    });
    let Some(transition) = transition else {
        return Ok(None);
    };
    super::record_slot_write(heap, *obj, physical);
    heap.record_write(*obj, &next_shape);
    Ok(Some(transition))
}

/// Replay a cached add-property transition for a fresh matching receiver.
///
/// Returns `Ok(Some(()))` only after the property was added. Any shape/key,
/// prototype, extensibility, or dictionary-mode mismatch falls back to ordinary
/// `[[Set]]`. Allocation failure after a matched guard propagates to the VM
/// without probing another recipe or replaying the operation. The child reader
/// names its actual traced owner; no copied child word is reused after GC.
#[allow(clippy::too_many_arguments)]
pub(crate) fn replay_store_property_transition(
    obj: JsObject,
    heap: &mut otter_gc::GcHeap,
    key: AtomizedPropertyKey<'_>,
    from_shape_id: ShapeId,
    atom_id: AtomId,
    target_shape_id: ShapeId,
    read_target: impl Fn() -> ShapeHandle,
    kind: &StorePropertyTransitionKind,
    slot: u16,
    value: &Value,
) -> Result<Option<()>, otter_gc::OutOfMemory> {
    if !transition_kind_matches(obj, heap, kind) {
        return Ok(None);
    }
    let current_shape_id = super::shape_id(obj, heap);
    let to_shape = read_target();
    let to_shape_id = if to_shape.is_null() {
        None
    } else {
        Some(heap.read_payload(to_shape, super::shape_body::ShapeBody::id))
    };
    // The shape-id guard below already pins both invariants the asserts
    // check: a receiver whose current shape equals `from_shape_id` has
    // exactly that shape's own keys (so the appended key is absent) and
    // exactly that shape's count of own properties (the appended slot's
    // offset). Verifying either costs a shape-chain walk, so both are
    // confirmed in debug builds only.
    #[cfg(debug_assertions)]
    let existing_offset =
        heap.read_payload(obj, |body| super::body_offset_of_atom(heap, body, key));
    #[cfg(debug_assertions)]
    let current_count = heap.read_payload(obj, |body| super::body_property_count(heap, body));
    let guard_matches = heap.read_payload(obj, |body| {
        let matches = is_fast_shape_body(body)
            && current_shape_id == from_shape_id
            && key.atom().id() == atom_id
            && transition_kind_matches_receiver_body(body, kind)
            && body.extensible();
        #[cfg(debug_assertions)]
        if matches {
            debug_assert_eq!(existing_offset, None);
            debug_assert_eq!(
                current_count,
                usize::from(slot),
                "replay count diverged from slot offset"
            );
        }
        matches
    });
    if !guard_matches {
        return Ok(None);
    }

    // The retained direct child preserves the complete immutable physical
    // prefix. Every incoming Value is a legal field word without preparation.
    if !to_shape.is_null() && super::shape_body::parent_of(to_shape) != super::shape(obj, heap) {
        return Ok(None);
    }
    let stored = *value;

    // Only a confirmed hit may allocate for sidecar/slab growth. On a miss the
    // caller retains its original raw handle, so allocating before guard
    // validation would leave fallback holding a forwarded cell.
    let mut obj = obj;
    let mut stored = stored;
    let mut slot_meta_table = None;
    let mut dict_table = None;
    if to_shape.is_null() {
        // Symbol-keyed slots leave the hidden class through the ordinary
        // dictionary conversion, not this replay.
        if key.symbol_handle().is_some()
            || super::shape_body::has_symbol_keys(super::shape(obj, heap))
        {
            return Ok(None);
        }
        // The dictionary arm below appends through `dict_push_key`,
        // which writes the sidecar and materializes per-slot metadata;
        // the shape-append arm never touches either and allocates none.
        super::ensure_exotic_with_pending_values(
            &mut obj,
            heap,
            std::slice::from_mut(&mut stored),
        )?;
        let slot_metas = super::slot_metas_for_shape_transition(heap, obj, None);
        slot_meta_table = super::slot_meta_table_for_install(
            &mut obj,
            heap,
            &slot_metas,
            usize::from(slot) + 1,
            std::slice::from_mut(&mut stored),
        )?;
        let dictionary_keys = super::dictionary_keys_for_shape_transition(heap, obj, None);
        dict_table = super::dict_keys_table_for_install(
            &mut obj,
            heap,
            &dictionary_keys,
            key.name(),
            std::slice::from_mut(&mut stored),
        )?;
    }
    // Reserve before taking the payload borrow; the slot stores `Value`
    // directly and performs no numeric box allocation.
    super::reserve_slot_capacity(
        &mut obj,
        heap,
        usize::from(slot) + 1,
        std::slice::from_mut(&mut stored),
    )?;
    // Read the actual retained child word after every collecting reservation.
    let to_shape = read_target();
    heap.with_payload(obj, |body| {
        let offset = usize::from(slot);
        if to_shape.is_null() {
            let advance_layout = !body.is_dictionary();
            body.enter_dictionary_mode_as(target_shape_id, advance_layout);
            if let Some(table) = dict_table {
                body.exotic_mut().dictionary_keys = table;
            }
            if let Some(table) = slot_meta_table {
                body.exotic_mut().slots = table;
            }
            super::dict_push_key(body, key.name().to_owned());
        } else {
            debug_assert_eq!(to_shape_id, Some(target_shape_id));
            body.invalidate_prototype_proofs();
            body.adopt_shape(to_shape);
        }
        body.push_slot(offset, SlotMeta::data_default(), stored);
    });
    super::record_slot_write(heap, obj, stored);
    if !to_shape.is_null() {
        heap.record_write(obj, &to_shape);
    }
    Ok(Some(()))
}

fn transition_kind(
    obj: JsObject,
    heap: &otter_gc::GcHeap,
    key: AtomizedPropertyKey<'_>,
) -> Option<StorePropertyTransitionKind> {
    match heap.read_payload(obj, |body| {
        is_fast_shape_body(body).then(|| body.prototype())
    })? {
        ObjectPrototype::Null => Some(StorePropertyTransitionKind::OwnAdd),
        ObjectPrototype::Object(first) => chain_append_kind(heap, first, key),
        ObjectPrototype::Value(_) | ObjectPrototype::Proxy(_) => None,
    }
}

/// The proof an own append of `key` under the ordinary prototype chain
/// starting at `first` needs: the key is absent from the whole chain, or its
/// first holder has writable data the append shadows.
fn chain_append_kind(
    heap: &impl crate::heap_read::HeapRead,
    first: JsObject,
    key: AtomizedPropertyKey<'_>,
) -> Option<StorePropertyTransitionKind> {
    let mut proto = first;
    for _ in 0..super::PROTO_CHAIN_HARD_CAP {
        if !super::supports_fast_property_ic(proto, heap) {
            return None;
        }
        match lookup_own_atom(proto, heap, key).lookup {
            PropertyLookup::Absent => {}
            PropertyLookup::Data { flags, .. } if flags.writable() => {
                return Some(StorePropertyTransitionKind::PrototypeWritableData {
                    validity: chain_validity(first, heap)?,
                });
            }
            PropertyLookup::Data { .. } | PropertyLookup::Accessor { .. } => return None,
        }
        if prototype_value_in(proto, heap).is_none() {
            return Some(StorePropertyTransitionKind::PrototypeChainMissing {
                validity: chain_validity(first, heap)?,
            });
        }
        proto = super::prototype_in(proto, heap)?;
    }
    None
}

/// What a named `[[Set]]` of `key` does to any extensible ordinary object of
/// the exact `shape`, as far as the shape, its interned children and the live
/// prototype chain fix it.
#[derive(Debug, Clone)]
pub(crate) enum ShapeStore {
    /// Overwrite the existing own writable data slot.
    Own(super::FieldLocation),
    /// Append the slot at `field` and publish the interned `child`, under the
    /// chain proof `kind` names.
    Append {
        child: ShapeHandle,
        field: super::FieldLocation,
        kind: StorePropertyTransitionKind,
    },
}

/// [`ShapeStore`] of `key` on objects of `shape`. `None` for a non-ordinary
/// shape, an accessor or read-only own property, a key never appended to
/// this shape, an append outside its inline geometry, or a chain whose
/// holder forbids the append. Reads only; never allocates.
pub(crate) fn shape_store(
    heap: &impl crate::heap_read::HeapRead,
    shape: ShapeHandle,
    key: AtomizedPropertyKey<'_>,
) -> Option<ShapeStore> {
    use super::shape_body;
    let atom = key.atom().id();
    if shape.is_null()
        || atom == AtomId::NONE
        || shape_body::state_of(shape) != super::ShapeState::ORDINARY
    {
        return None;
    }
    if let Some(slot) = shape_body::shape_slot_of_atom(heap, shape, atom) {
        return (!slot.is_accessor && slot.flags.writable()).then(|| {
            ShapeStore::Own(super::FieldLocation::for_slot(
                slot.offset,
                shape_body::inline_capacity_of(shape),
            ))
        });
    }
    let child = super::shape_interner::ShapeInterner::data_child(heap, shape, atom)?;
    let field = shape_body::own_field_of(child);
    if !shape_body::append_geometry_matches(shape, child, field) {
        return None;
    }
    let kind = match shape_body::prototype_of(shape) {
        shape_body::ShapePrototype::Null => StorePropertyTransitionKind::OwnAdd,
        shape_body::ShapePrototype::Object(first) => chain_append_kind(heap, first, key)?,
        shape_body::ShapePrototype::Value(_) => return None,
    };
    Some(ShapeStore::Append { child, field, kind })
}

fn transition_kind_matches(
    obj: JsObject,
    heap: &otter_gc::GcHeap,
    kind: &StorePropertyTransitionKind,
) -> bool {
    heap.read_payload(obj, |body| {
        is_fast_shape_body(body)
            && transition_kind_matches_receiver_body(body, kind)
            && match kind {
                StorePropertyTransitionKind::OwnAdd => true,
                StorePropertyTransitionKind::PrototypeChainMissing { validity }
                | StorePropertyTransitionKind::PrototypeWritableData { validity } => {
                    validity.is_valid()
                }
            }
    })
}

fn transition_kind_matches_receiver_body(
    body: &ObjectBody,
    kind: &StorePropertyTransitionKind,
) -> bool {
    matches!(
        (kind, &body.prototype()),
        (StorePropertyTransitionKind::OwnAdd, ObjectPrototype::Null)
            | (
                StorePropertyTransitionKind::PrototypeChainMissing { .. }
                    | StorePropertyTransitionKind::PrototypeWritableData { .. },
                ObjectPrototype::Object(_),
            )
    )
}

fn is_fast_shape_body(body: &ObjectBody) -> bool {
    super::shape_cache::supports_fast_property_ic(body)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rooting::RootScopeExt;

    #[test]
    fn matched_transition_allocation_failure_is_not_a_cache_miss() {
        let mut interp = crate::Interpreter::with_string_heap_cap(4 * 1024 * 1024)
            .expect("fixture interpreter bootstrap");
        let owner = otter_gc::ExtraRoots::new(&interp);
        let _owner = interp.gc_heap_mut().register_extra_roots(owner);
        let mut first = interp
            .alloc_runtime_rooted_object_with_roots(&mut [], &mut [])
            .expect("first receiver");
        let mut second = interp
            .alloc_runtime_rooted_object_with_roots(&mut [], &mut [])
            .expect("second receiver");
        let mut roots = otter_gc::RootScope::new(interp.gc_heap_mut());
        // SAFETY: both receiver slots precede the scope and stay stationary
        // through transition capture and the allocation-triggered full GC.
        unsafe {
            roots.add_object(&mut first);
            roots.add_object(&mut second);
        }
        let key = AtomizedPropertyKey::new(
            crate::property_atom::PropertyAtom::new(AtomId::from_global(7)),
            "x",
        );
        let transition = capture_store_property_transition(
            first,
            interp.gc_heap_mut(),
            key,
            &Value::boolean(true),
        )
        .expect("capture an allocating dictionary transition");
        assert!(transition.to_shape.get().is_null());
        assert_eq!(
            super::super::shape_id(second, interp.gc_heap()),
            transition.from_shape_id
        );
        // A bootstrap allocation may survive its first collection. Settle
        // the live set before filling the cap so emergency GC cannot simply
        // reclaim unrelated bootstrap garbage and make the request succeed.
        let mut settled = false;
        for round in 0..8 {
            let before = interp.gc_heap().stats().allocated_bytes;
            interp
                .gc_heap_mut()
                .collect_full(&mut |_| {})
                .expect("settle live heap");
            if round > 0 && before == interp.gc_heap().stats().allocated_bytes {
                settled = true;
                break;
            }
        }
        assert!(settled, "bootstrap live set must stabilize");
        let stats = interp.gc_heap().stats();
        // Admission recomputes allocated + reserved after emergency GC;
        // explicit collection leaves the conservative tracked counter intact.
        let remaining = stats.max_heap_bytes - stats.allocated_bytes as u64 - stats.reserved_bytes;
        interp
            .gc_heap_mut()
            .reserve_bytes(remaining)
            .expect("fill cap");
        let before = (
            interp.gc_heap().tracked_bytes(),
            interp.gc_heap().gc_cycle_counts(),
        );
        let result = replay_store_property_transition(
            second,
            interp.gc_heap_mut(),
            key,
            transition.from_shape_id,
            transition.atom_id,
            transition.to_shape_id,
            || transition.to_shape.get(),
            &transition.kind,
            transition.slot,
            &Value::null(),
        );
        assert!(
            matches!(result, Err(otter_gc::OutOfMemory::HeapCapExceeded { .. })),
            "{result:?}: before={before:?}, after={:?}, cycles={:?}",
            interp.gc_heap().tracked_bytes(),
            interp.gc_heap().gc_cycle_counts()
        );
        assert_ne!(
            interp.gc_heap().gc_cycle_counts(),
            before.1,
            "allocation must attempt GC before refusal"
        );
        assert_eq!(super::super::get_own(second, interp.gc_heap(), "x"), None);
        interp.gc_heap_mut().release_bytes(remaining);
    }
}
