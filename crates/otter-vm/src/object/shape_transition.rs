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
//! - [`LowerableStoreTransition`] — allocation-free subset whose complete
//!   guards can be emitted by the native property cell.
//! - [`capture_store_property_transition`] — apply a resolved `[[Set]]` data
//!   write and return replay metadata when the path is IC-compatible.
//! - [`replay_store_property_transition`] — validate guards and add the cached
//!   own data slot on a fresh matching receiver.
//!
//! # Invariants
//! - Replay only applies to fast-shape ordinary objects.
//! - Replay guards run before sidecar or slab growth, so a failed probe is
//!   allocation-free and cannot relocate a caller-owned receiver handle.
//! - Allocation failure after a matched transition propagates as OOM; it is
//!   never converted into a miss that could reuse a stale receiver.
//! - Prototype guards are complete here: null prototype, a prototype chain on
//!   which every object is a fast-shape ordinary object missing the key
//!   (each link's shape is recorded and revalidated, as SpiderMonkey's
//!   CacheIR guards every prototype shape for an add-slot store), or a
//!   direct prototype with a writable data slot.
//! - Descriptor changes that do not alter shape are still guarded: inherited
//!   writable-data replay rechecks the direct prototype slot's writability.
//! - Native lowering is narrower than VM replay: it admits a null receiver
//!   prototype, a missing-key chain of at most two prototypes, or direct
//!   inherited writable data with an unmodified descriptor and no sidecar.
//!   Generated guards bound-check the appended slot against live storage.
//!   Deeper missing-key chains remain on the runtime stub.
//! - Accessors, proxies, string wrapper objects, deep prototype hits,
//!   non-writable inherited data, and dictionary-compatible objects remain
//!   fallback paths.
//!
//! # See also
//! - [`crate::property_ic`]
//! - [`crate::property_dispatch`]

use super::{
    AtomOwnPropertyHit, JsObject, ObjectBody, ObjectPrototype, PropertyLookup, ShapeHandle,
    ShapeId, SlotMeta, has_writable_own_data_slot_atom, lookup_own_atom, prototype_value, shape_id,
};
use crate::Value;
use crate::property_atom::{AtomId, AtomizedPropertyKey};
use otter_gc::raw::{RawGc, SlotVisitor};
use smallvec::SmallVec;
use std::cell::Cell;

/// Longest missing-key prototype chain a transition records. A deeper chain
/// keeps the store on ordinary `[[Set]]`; every recorded link costs one shape
/// read per replay.
const MAX_TRANSITION_CHAIN: usize = 8;

/// Shape ids of each prototype on a missing-key chain, nearest first.
pub(crate) type PrototypeChainShapes = SmallVec<[ShapeId; 4]>;

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

/// Complete immutable metadata for one add-property transition that generated
/// code may execute without calling the runtime.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct LowerableStoreTransition {
    /// Parent shape guarded before the append.
    pub(crate) from_shape: ShapeHandle,
    /// Direct-prototype shape for the selected proof, or null when the
    /// receiver itself has a null prototype.
    pub(crate) prototype_shape: ShapeHandle,
    /// Shape of the direct prototype's own prototype when the missing-key
    /// chain is two links long, or null when the direct prototype is the
    /// chain's end (or the receiver has no prototype). Writable-data proofs
    /// leave this null and impose no constraint on later links.
    pub(crate) prototype_chain_shape: ShapeHandle,
    /// Native proof that permits creating the receiver's own property.
    pub(crate) prototype_guard: crate::jit::JitPropertyIcPrototypeGuard,
    /// Canonical child shape published after the value append.
    pub(crate) to_shape: ShapeHandle,
    /// New inline own-slot index.
    pub(crate) slot: u16,
}

impl StorePropertyTransition {
    pub(crate) fn trace_roots(&self, visitor: &mut SlotVisitor<'_>) {
        if !self.to_shape.get().is_null() {
            let p = self.to_shape.as_ptr() as *mut RawGc;
            visitor(p);
        }
    }

    /// Revalidate the committed transition as the allocation-free subset a
    /// native property cell can replay.
    ///
    /// `obj` is the receiver immediately after this transition committed. Its
    /// child shape lets us recover the immutable parent handle without storing
    /// another GC root in the cache record. The generated program will guard
    /// the parent shape, the exact pre-append slot count, extensibility, and the
    /// same prototype topology before performing any write.
    pub(crate) fn lowerable_add(
        &self,
        obj: JsObject,
        heap: &otter_gc::GcHeap,
        key: AtomizedPropertyKey<'_>,
    ) -> Option<LowerableStoreTransition> {
        if self.atom_id != key.atom().id() {
            return None;
        }

        let to_shape = self.to_shape.get();
        let to_shape_offset = to_shape.offset();
        if to_shape.is_null()
            || to_shape_offset == 0
            || object_shape(obj, heap) != to_shape
            || !super::is_extensible(obj, heap)
            || heap.read_payload(obj, ObjectBody::slab_len) != usize::from(self.slot) + 1
        {
            return None;
        }
        let (to_shape_id, from_shape, transition_atom, property_count, own_offset) = heap
            .read_payload(to_shape, |body| {
                (
                    body.id(),
                    body.parent(),
                    body.transition_atom(),
                    body.property_count(),
                    body.own_offset(),
                )
            });
        if from_shape.is_null() || from_shape.offset() == 0 {
            return None;
        }
        let (from_shape_id, from_property_count) =
            heap.read_payload(from_shape, |body| (body.id(), body.property_count()));
        if to_shape_id == ShapeId::UNASSIGNED
            || to_shape_id != self.to_shape_id
            || transition_atom != self.atom_id
            || property_count != u32::from(self.slot) + 1
            || own_offset != u32::from(self.slot)
            || from_shape_id != self.from_shape_id
            || from_property_count != u32::from(self.slot)
            || self.from_shape_id == ShapeId::UNASSIGNED
        {
            return None;
        }

        use crate::jit::JitPropertyIcPrototypeGuard::{Missing, WritableData};
        let (prototype_shape, prototype_chain_shape, prototype_guard) = match &self.kind {
            StorePropertyTransitionKind::OwnAdd => {
                if prototype_value(obj, heap).is_some() {
                    return None;
                }
                (ShapeHandle::null(), ShapeHandle::null(), Missing)
            }
            StorePropertyTransitionKind::PrototypeChainMissing { chain } => {
                if chain.len() > 2 {
                    return None;
                }
                let mut shapes = [ShapeHandle::null(); 2];
                let mut proto = super::prototype(obj, heap)?;
                for (index, expected) in chain.iter().enumerate() {
                    if index > 0 {
                        proto = super::prototype(proto, heap)?;
                    }
                    if !super::supports_fast_property_ic(proto, heap)
                        || shape_id(proto, heap) != *expected
                        || *expected == ShapeId::UNASSIGNED
                    {
                        return None;
                    }
                    let lookup = lookup_own_atom(proto, heap, key);
                    if lookup.hit.is_some() || !matches!(lookup.lookup, PropertyLookup::Absent) {
                        return None;
                    }
                    let shape = object_shape(proto, heap);
                    if shape.is_null() || shape.offset() == 0 {
                        return None;
                    }
                    shapes[index] = shape;
                }
                if prototype_value(proto, heap).is_some() {
                    return None;
                }
                (shapes[0], shapes[1], Missing)
            }
            StorePropertyTransitionKind::DirectPrototypeWritableData { prototype_hit } => {
                let proto = super::prototype(obj, heap)?;
                if !super::supports_fast_property_ic(proto, heap)
                    || !has_writable_own_data_slot_atom(proto, heap, self.atom_id, *prototype_hit)
                    || !heap.read_payload(proto, |body| {
                        !body.slot_attrs_overridden && body.exotic.handle.is_null()
                    })
                {
                    return None;
                }
                let shape = object_shape(proto, heap);
                if shape.is_null() || shape.offset() == 0 {
                    return None;
                }
                (shape, ShapeHandle::null(), WritableData)
            }
        };

        Some(LowerableStoreTransition {
            from_shape,
            prototype_shape,
            prototype_chain_shape,
            prototype_guard,
            to_shape,
            slot: self.slot,
        })
    }
}

#[inline]
fn object_shape(obj: JsObject, heap: &otter_gc::GcHeap) -> ShapeHandle {
    super::shape(obj, heap)
}

/// Explicit StoreProperty transition categories.
#[derive(Debug, Clone)]
pub(crate) enum StorePropertyTransitionKind {
    /// Existing receiver had `null` prototype and added a new own data slot.
    OwnAdd,
    /// Every prototype on the receiver's chain was a fast-shape ordinary
    /// object without an own key when the transition was installed; `chain`
    /// holds their shape ids nearest first, and the last one's prototype was
    /// null.
    PrototypeChainMissing {
        /// Prototype shape ids observed at install time, nearest first.
        chain: PrototypeChainShapes,
    },
    /// Receiver's direct prototype had a writable ordinary data property for
    /// this key. Setting through it creates an own data property on receiver.
    DirectPrototypeWritableData {
        /// Direct-prototype data slot metadata.
        prototype_hit: AtomOwnPropertyHit,
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
            || !body.extensible
            || !transition_kind_matches_receiver_body(body, &kind)
        {
            return None;
        }
        if existing_offset.is_some() {
            return None;
        }
        let to_shape_id = super::next_shape_id();
        body.dictionary_shape_id = to_shape_id;
        if let Some(table) = dict_table {
            body.exotic_mut().dictionary_keys = table;
        }
        if let Some(table) = slot_meta_table {
            body.exotic_mut().slots = table;
        }
        super::dict_push_key(body, key.name().to_owned());
        body.shape = super::ShapeHandle::null();
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
    obj: JsObject,
    heap: &mut otter_gc::GcHeap,
    key: AtomizedPropertyKey<'_>,
    value: &Value,
    next_shape: ShapeHandle,
) -> Option<StorePropertyTransition> {
    let mut obj = obj;
    let (to_shape_id, to_shape_count) =
        heap.read_payload(next_shape, |s| (s.id(), s.property_count()));
    // The appended slot's flat index is the new shape's last offset.
    let index = to_shape_count as usize - 1;
    let slot = u16::try_from(index).ok()?;
    // Reserve before taking the payload borrow. The incoming value is not yet
    // in a traced object, so keep its direct word in the pending-root slice.
    let mut stored = *value;
    super::reserve_slot_capacity(&mut obj, heap, index + 1, std::slice::from_mut(&mut stored))
        .ok()?;
    let kind = transition_kind(obj, heap, key)?;
    let from_shape_id = super::shape_id(obj, heap);
    let existing_offset =
        heap.read_payload(obj, |body| super::body_offset_of_atom(heap, body, key));
    let transition = heap.with_payload(obj, |body| {
        if !is_fast_shape_body(body)
            || !body.extensible
            || !transition_kind_matches_receiver_body(body, &kind)
        {
            return None;
        }
        if existing_offset.is_some() {
            return None;
        }
        body.shape = next_shape;
        body.push_slot(index, SlotMeta::data_default(), stored);
        Some(StorePropertyTransition {
            from_shape_id,
            atom_id: key.atom().id(),
            to_shape_id,
            to_shape: Cell::new(next_shape),
            kind,
            slot,
        })
    })?;
    super::record_slot_write(heap, obj, stored);
    heap.record_write(obj, &next_shape);
    Some(transition)
}

/// Replay a cached add-property transition for a fresh matching receiver.
///
/// Returns `Ok(Some(()))` only after the property was added. Any shape/key,
/// prototype, extensibility, or dictionary-mode mismatch falls back to ordinary
/// `[[Set]]`. Allocation failure after a matched guard propagates to the VM
/// without probing another recipe or replaying the operation.
pub(crate) fn replay_store_property_transition(
    obj: JsObject,
    heap: &mut otter_gc::GcHeap,
    key: AtomizedPropertyKey<'_>,
    transition: &StorePropertyTransition,
    value: &Value,
) -> Result<Option<()>, otter_gc::OutOfMemory> {
    if !transition_kind_matches(obj, heap, transition) {
        return Ok(None);
    }
    let current_shape_id = super::shape_id(obj, heap);
    let to_shape = transition.to_shape.get();
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
        if !is_fast_shape_body(body)
            || current_shape_id != transition.from_shape_id
            || key.atom().id() != transition.atom_id
            || !transition_kind_matches_receiver_body(body, &transition.kind)
            || !body.extensible
        {
            return false;
        }
        #[cfg(debug_assertions)]
        debug_assert_eq!(existing_offset, None);
        #[cfg(debug_assertions)]
        debug_assert_eq!(
            current_count,
            usize::from(transition.slot),
            "replay count diverged from slot offset"
        );
        true
    });
    if !guard_matches {
        return Ok(None);
    }

    // Only a confirmed hit may allocate for sidecar/slab growth. On a miss the
    // caller retains its original raw handle, so allocating before guard
    // validation would leave fallback holding a forwarded cell.
    let mut obj = obj;
    let mut stored = *value;
    let mut slot_meta_table = None;
    let mut dict_table = None;
    if to_shape.is_null() {
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
            usize::from(transition.slot) + 1,
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
        usize::from(transition.slot) + 1,
        std::slice::from_mut(&mut stored),
    )?;
    heap.with_payload(obj, |body| {
        let offset = usize::from(transition.slot);
        if to_shape.is_null() {
            body.dictionary_shape_id = transition.to_shape_id;
            if let Some(table) = dict_table {
                body.exotic_mut().dictionary_keys = table;
            }
            if let Some(table) = slot_meta_table {
                body.exotic_mut().slots = table;
            }
            super::dict_push_key(body, key.name().to_owned());
            body.shape = super::ShapeHandle::null();
        } else {
            debug_assert_eq!(to_shape_id, Some(transition.to_shape_id));
            body.shape = to_shape;
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
        if is_fast_shape_body(body) {
            Some(body.prototype())
        } else {
            None
        }
    })? {
        ObjectPrototype::Null => Some(StorePropertyTransitionKind::OwnAdd),
        ObjectPrototype::Object(first) => {
            let mut chain = PrototypeChainShapes::new();
            let mut proto = first;
            loop {
                if !super::supports_fast_property_ic(proto, heap) {
                    return None;
                }
                let lookup = lookup_own_atom(proto, heap, key);
                match lookup.lookup {
                    PropertyLookup::Absent => {
                        if chain.len() == MAX_TRANSITION_CHAIN {
                            return None;
                        }
                        chain.push(shape_id(proto, heap));
                    }
                    PropertyLookup::Data { flags, .. } if flags.writable() && chain.is_empty() => {
                        return lookup.hit.map(|prototype_hit| {
                            StorePropertyTransitionKind::DirectPrototypeWritableData {
                                prototype_hit,
                            }
                        });
                    }
                    PropertyLookup::Data { .. } | PropertyLookup::Accessor { .. } => return None,
                }
                if prototype_value(proto, heap).is_none() {
                    return Some(StorePropertyTransitionKind::PrototypeChainMissing { chain });
                }
                // A non-object prototype (a Proxy or an exotic value) ends
                // the fast chain without proving the key absent.
                proto = super::prototype(proto, heap)?;
            }
        }
        ObjectPrototype::Value(_) | ObjectPrototype::Proxy(_) => None,
    }
}

fn transition_kind_matches(
    obj: JsObject,
    heap: &otter_gc::GcHeap,
    transition: &StorePropertyTransition,
) -> bool {
    let prototype = heap.read_payload(obj, |body| {
        if is_fast_shape_body(body) {
            Some(body.prototype())
        } else {
            None
        }
    });
    match (&transition.kind, prototype) {
        (StorePropertyTransitionKind::OwnAdd, Some(ObjectPrototype::Null)) => true,
        (
            StorePropertyTransitionKind::PrototypeChainMissing { chain },
            Some(ObjectPrototype::Object(first)),
        ) => {
            let mut proto = first;
            for (index, expected) in chain.iter().enumerate() {
                if index > 0 {
                    let Some(next) = super::prototype(proto, heap) else {
                        return false;
                    };
                    proto = next;
                }
                if !super::supports_fast_property_ic(proto, heap)
                    || shape_id(proto, heap) != *expected
                {
                    return false;
                }
            }
            prototype_value(proto, heap).is_none()
        }
        (
            StorePropertyTransitionKind::DirectPrototypeWritableData { prototype_hit },
            Some(ObjectPrototype::Object(proto)),
        ) => {
            super::supports_fast_property_ic(proto, heap)
                && has_writable_own_data_slot_atom(proto, heap, transition.atom_id, *prototype_hit)
        }
        _ => false,
    }
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
                    | StorePropertyTransitionKind::DirectPrototypeWritableData { .. },
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
        let mut interp = crate::Interpreter::with_string_heap_cap(4 * 1024 * 1024);
        let owner = otter_gc::ExtraRoots::new(&interp);
        let _owner = interp.gc_heap_mut().register_extra_roots(owner);
        let mut first = interp
            .alloc_runtime_rooted_object_with_roots(&[], &[])
            .expect("first receiver");
        let mut second = interp
            .alloc_runtime_rooted_object_with_roots(&[], &[])
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
        let result = crate::cache_ir::CacheStub::store_transition(transition).run_store(
            second,
            interp.gc_heap_mut(),
            key,
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
