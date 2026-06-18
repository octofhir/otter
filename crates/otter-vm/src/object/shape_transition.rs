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
//!
//! # Invariants
//! - Replay only applies to fast-shape ordinary objects.
//! - Prototype guards are complete here: null prototype, direct prototype with
//!   missing key and no deeper chain, or direct prototype with a writable data
//!   slot.
//! - Descriptor changes that do not alter shape are still guarded: inherited
//!   writable-data replay rechecks the direct prototype slot's writability.
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
    pub(crate) to_shape: ShapeHandle,
    /// Transition category and replay guard.
    pub(crate) kind: StorePropertyTransitionKind,
    /// Slot offset added by this transition.
    pub(crate) slot: u16,
}

impl StorePropertyTransition {
    pub(crate) fn trace_roots(&self, visitor: &mut SlotVisitor<'_>) {
        if !self.to_shape.is_null() {
            let p = &self.to_shape as *const ShapeHandle as *mut RawGc;
            visitor(p);
        }
    }
}

/// Explicit StoreProperty transition categories.
#[derive(Debug, Clone)]
pub(crate) enum StorePropertyTransitionKind {
    /// Existing receiver had `null` prototype and added a new own data slot.
    OwnAdd,
    /// Receiver's direct prototype had this shape, no own key, and no further
    /// prototype chain when the transition was installed.
    DirectPrototypeMissing {
        /// Direct prototype shape observed at install time.
        prototype_shape_id: ShapeId,
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
    let kind = transition_kind(obj, heap, key)?;
    let from_shape_id = super::shape_id(obj, heap);
    let existing_offset =
        heap.read_payload(obj, |body| super::body_offset_of(heap, body, key.name()));
    let index = heap.read_payload(obj, |body| super::body_property_count(heap, body));
    let slot = u16::try_from(index).ok()?;
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
        super::dict_push_key(body, key.name().to_owned());
        body.shape = super::ShapeHandle::null();
        body.push_slot(index, SlotMeta::data_default(), *value);
        Some(StorePropertyTransition {
            from_shape_id,
            atom_id: key.atom().id(),
            to_shape_id,
            to_shape: ShapeHandle::null(),
            kind,
            slot,
        })
    })?;
    heap.record_write(obj, value);
    Some(transition)
}

pub(crate) fn capture_store_property_transition_with_shape(
    obj: JsObject,
    heap: &mut otter_gc::GcHeap,
    key: AtomizedPropertyKey<'_>,
    value: &Value,
    next_shape: ShapeHandle,
) -> Option<StorePropertyTransition> {
    let kind = transition_kind(obj, heap, key)?;
    let from_shape_id = super::shape_id(obj, heap);
    let (to_shape_id, to_shape_count) =
        heap.read_payload(next_shape, |s| (s.id(), s.property_count()));
    let existing_offset =
        heap.read_payload(obj, |body| super::body_offset_of(heap, body, key.name()));
    // The appended slot's flat index is the new shape's last offset.
    let index = to_shape_count as usize - 1;
    let slot = u16::try_from(index).ok()?;
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
        body.push_slot(index, SlotMeta::data_default(), *value);
        Some(StorePropertyTransition {
            from_shape_id,
            atom_id: key.atom().id(),
            to_shape_id,
            to_shape: next_shape,
            kind,
            slot,
        })
    })?;
    heap.record_write(obj, value);
    heap.record_write(obj, &next_shape);
    Some(transition)
}

/// Replay a cached add-property transition for a fresh matching receiver.
///
/// Returns `Some(())` only after the property was added. Any shape/key,
/// prototype, extensibility, or dictionary-mode mismatch falls back to ordinary
/// `[[Set]]`.
pub(crate) fn replay_store_property_transition(
    obj: JsObject,
    heap: &mut otter_gc::GcHeap,
    key: AtomizedPropertyKey<'_>,
    transition: &StorePropertyTransition,
    value: &Value,
) -> Option<()> {
    if !transition_kind_matches(obj, heap, transition) {
        return None;
    }
    let current_shape_id = super::shape_id(obj, heap);
    let to_shape_id = if transition.to_shape.is_null() {
        None
    } else {
        Some(heap.read_payload(transition.to_shape, super::shape_body::ShapeBody::id))
    };
    let existing_offset =
        heap.read_payload(obj, |body| super::body_offset_of(heap, body, key.name()));
    // The shape-id guard below already pins the property count: a receiver whose
    // current shape equals `from_shape_id` has exactly that shape's count of own
    // properties, which is the appended slot's offset. Verifying it costs a
    // shape walk, so confirm the invariant in debug builds only.
    #[cfg(debug_assertions)]
    let current_count = heap.read_payload(obj, |body| super::body_property_count(heap, body));
    let success = heap.with_payload(obj, |body| {
        if !is_fast_shape_body(body)
            || current_shape_id != transition.from_shape_id
            || key.atom().id() != transition.atom_id
            || !transition_kind_matches_receiver_body(body, &transition.kind)
            || !body.extensible
        {
            return false;
        }
        let offset = usize::from(transition.slot);
        debug_assert_eq!(existing_offset, None);
        #[cfg(debug_assertions)]
        debug_assert_eq!(
            current_count, offset,
            "replay count diverged from slot offset"
        );
        if transition.to_shape.is_null() {
            body.dictionary_shape_id = transition.to_shape_id;
            super::dict_push_key(body, key.name().to_owned());
            body.shape = super::ShapeHandle::null();
        } else {
            debug_assert_eq!(to_shape_id, Some(transition.to_shape_id));
            body.shape = transition.to_shape;
        }
        body.push_slot(offset, SlotMeta::data_default(), *value);
        true
    });
    if !success {
        return None;
    }
    heap.record_write(obj, value);
    if !transition.to_shape.is_null() {
        heap.record_write(obj, &transition.to_shape);
    }
    Some(())
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
        ObjectPrototype::Object(proto) => {
            if !super::supports_fast_property_ic(proto, heap) {
                return None;
            }
            let lookup = lookup_own_atom(proto, heap, key);
            match lookup.lookup {
                PropertyLookup::Absent => prototype_value(proto, heap).is_none().then(|| {
                    StorePropertyTransitionKind::DirectPrototypeMissing {
                        prototype_shape_id: shape_id(proto, heap),
                    }
                }),
                PropertyLookup::Data { flags, .. } if flags.writable() => {
                    lookup.hit.map(|prototype_hit| {
                        StorePropertyTransitionKind::DirectPrototypeWritableData { prototype_hit }
                    })
                }
                PropertyLookup::Data { .. } | PropertyLookup::Accessor { .. } => None,
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
            StorePropertyTransitionKind::DirectPrototypeMissing { prototype_shape_id },
            Some(ObjectPrototype::Object(proto)),
        ) => {
            super::supports_fast_property_ic(proto, heap)
                && prototype_value(proto, heap).is_none()
                && shape_id(proto, heap) == *prototype_shape_id
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
                StorePropertyTransitionKind::DirectPrototypeMissing { .. }
                    | StorePropertyTransitionKind::DirectPrototypeWritableData { .. },
                ObjectPrototype::Object(_),
            )
    )
}

fn is_fast_shape_body(body: &ObjectBody) -> bool {
    super::shape_cache::supports_fast_property_ic(body)
}
