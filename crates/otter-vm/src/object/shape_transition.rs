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
//! - [`super::Shape`]
//! - [`crate::property_ic`]
//! - [`crate::property_dispatch`]

use std::rc::Rc;

use super::{
    AtomOwnPropertyHit, JsObject, ObjectBody, ObjectPrototype, PropertyLookup, PropertySlot, Shape,
    ShapeId, has_writable_own_data_slot_atom, lookup_own_atom, prototype_value, shape_id,
};
use crate::Value;
use crate::property_atom::{AtomId, AtomizedPropertyKey};

/// Atom-aware hidden-class transition for adding one ordinary own data slot.
#[derive(Debug, Clone)]
pub(crate) struct StorePropertyTransition {
    /// Shape observed before adding the property.
    pub(crate) from_shape_id: ShapeId,
    /// Atomized named-property key from the executable context.
    pub(crate) atom_id: AtomId,
    /// Shared child shape reached by adding the property.
    pub(crate) to_shape: Rc<Shape>,
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
pub(crate) fn capture_store_property_transition(
    obj: JsObject,
    heap: &mut otter_gc::GcHeap,
    key: AtomizedPropertyKey<'_>,
    value: &Value,
) -> Option<StorePropertyTransition> {
    let kind = transition_kind(obj, heap, key)?;
    let transition = heap.with_payload(obj, |body| {
        if !is_fast_shape_body(body)
            || !body.extensible
            || !transition_kind_matches_receiver_body(body, &kind)
        {
            return None;
        }
        if body.shape.offset_of(key.name()).is_some() {
            return None;
        }
        let slot = u16::try_from(body.slots.len()).ok()?;
        let from_shape_id = body.shape.id();
        let to_shape = Shape::add_property(&body.shape, key.name());
        body.shape = Rc::clone(&to_shape);
        body.slots.push(PropertySlot::data_default(value.clone()));
        Some(StorePropertyTransition {
            from_shape_id,
            atom_id: key.atom().id(),
            to_shape,
            kind,
            slot,
        })
    })?;
    heap.record_write(obj, value);
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
    let success = heap.with_payload(obj, |body| {
        if !is_fast_shape_body(body)
            || body.shape.id() != transition.from_shape_id
            || key.atom().id() != transition.atom_id
            || !transition_kind_matches_receiver_body(body, &transition.kind)
            || !body.extensible
        {
            return false;
        }
        let offset = usize::from(transition.slot);
        debug_assert_eq!(body.shape.offset_of(key.name()), None);
        debug_assert_eq!(
            transition.to_shape.offset_of(key.name()),
            Some(transition.slot)
        );
        if body.slots.len() != offset {
            return false;
        }
        body.shape = Rc::clone(&transition.to_shape);
        body.slots.push(PropertySlot::data_default(value.clone()));
        true
    });
    if !success {
        return None;
    }
    heap.record_write(obj, value);
    Some(())
}

fn transition_kind(
    obj: JsObject,
    heap: &otter_gc::GcHeap,
    key: AtomizedPropertyKey<'_>,
) -> Option<StorePropertyTransitionKind> {
    match heap.read_payload(obj, |body| {
        if is_fast_shape_body(body) {
            Some(body.prototype.clone())
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
            Some(body.prototype.clone())
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
    match (kind, &body.prototype) {
        (StorePropertyTransitionKind::OwnAdd, ObjectPrototype::Null) => true,
        (
            StorePropertyTransitionKind::DirectPrototypeMissing { .. }
            | StorePropertyTransitionKind::DirectPrototypeWritableData { .. },
            ObjectPrototype::Object(_),
        ) => true,
        _ => false,
    }
}

fn is_fast_shape_body(body: &ObjectBody) -> bool {
    super::shape_cache::supports_fast_property_ic(body)
}
