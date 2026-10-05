//! Resolution of cacheable named-property accesses.
//!
//! Every property IC handler and the isolate's shared `(shape, atom)` action
//! table cache one of the answers computed here: an own or inherited ordinary
//! data slot with one shared chain proof, or a key absent from the receiver
//! and its whole ordinary chain.
//!
//! # Contents
//!
//! - [`ResolvedDataSlot`] / [`resolve_atom_data_slot`] — where a named data
//!   property lives and what it holds.
//! - [`resolve_absent_atom`] — a key no link of an ordinary chain owns.
//!
//! # Invariants
//!
//! - Resolution neither allocates nor runs JavaScript.
//! - Accessors, proxies, opaque lookup state and dictionary holders are not
//!   resolved; their callers complete the ordinary `[[Get]]`/`[[Set]]`.
//! - An inherited or absent answer carries the chain proof of its first
//!   prototype. Any mutation of a link (including adding the key) invalidates
//!   it, and an invalid proof is never revived.
//!
//! # See also
//! [`object::prototype_validity`] owns the shared inherited-property proofs;
//! [`crate::property_ic`] owns the per-site handlers.

use crate::object::prototype_validity::{PrototypeValidity, chain_validity};
use std::sync::Arc;

use crate::object::{self, AtomOwnPropertyHit, ShapeId};
use crate::property_atom::AtomizedPropertyKey;
use crate::{JsObject, Value};

/// Where a named data property lives relative to the receiver, and what it
/// currently holds.
#[derive(Debug, Clone)]
pub(crate) struct ResolvedDataSlot {
    pub(crate) validity: Option<Arc<PrototypeValidity>>,
    pub(crate) holder_root: object::ShapeHandle,
    pub(crate) holder_root_id: ShapeId,
    /// Holder category: `0` for own data, `1` for inherited data at any depth.
    pub(crate) hops: u8,
    /// The holder's own-slot hit, guarded by its shape.
    pub(crate) hit: AtomOwnPropertyHit,
    /// The value the slot holds right now.
    pub(crate) value: Value,
    /// Whether the resolved data descriptor accepts an ordinary assignment.
    pub(crate) is_writable: bool,
}

/// Resolve own or inherited ordinary data with one shared chain proof.
/// Both per-site stubs and the shared property table consume this answer.
/// Accessors, absent properties and opaque lookup state return `None`.
#[must_use]
pub(crate) fn resolve_atom_data_slot(
    obj: JsObject,
    heap: &otter_gc::GcHeap,
    key: AtomizedPropertyKey<'_>,
) -> Option<ResolvedDataSlot> {
    if !object::supports_fast_property_ic(obj, heap) {
        return None;
    }
    let own = object::lookup_own_atom(obj, heap, key);
    if let (Some(hit), object::PropertyLookup::Data { value, flags }) = (own.hit, own.lookup) {
        return Some(ResolvedDataSlot {
            hops: 0,
            validity: None,
            holder_root: object::ShapeHandle::null(),
            holder_root_id: ShapeId::UNASSIGNED,
            hit,
            value,
            is_writable: flags.writable(),
        });
    }
    if own.hit.is_some() || object::state(obj, heap).is_opaque() {
        return None;
    }
    let first = object::prototype(obj, heap)?;
    let validity = chain_validity(first, heap)?;
    let mut proto = first;
    for _ in 0..object::PROTO_CHAIN_HARD_CAP {
        if !object::supports_fast_property_ic(proto, heap) {
            return None;
        }
        let inherited = object::lookup_own_atom(proto, heap, key);
        match (inherited.hit, inherited.lookup) {
            (Some(hit), object::PropertyLookup::Data { value, flags }) => {
                let holder_root = object::cached_instance_root(proto, heap)?;
                return Some(ResolvedDataSlot {
                    hops: 1,
                    hit,
                    value,
                    is_writable: flags.writable(),
                    validity: Some(validity),
                    holder_root,
                    holder_root_id: heap
                        .read_payload(holder_root, object::shape_body::ShapeBody::id),
                });
            }
            (_, object::PropertyLookup::Absent) => proto = object::prototype(proto, heap)?,
            _ => return None,
        }
    }
    None
}

/// Resolve a key the receiver and its whole ordinary prototype chain lack.
///
/// `Some(None)` when the receiver's prototype is `null` (its shape alone
/// proves the absence), `Some(Some(proof))` when every link of an ordinary
/// chain lacks the key, `None` when the key exists anywhere or the chain is
/// not provable.
#[must_use]
pub(crate) fn resolve_absent_atom(
    obj: JsObject,
    heap: &otter_gc::GcHeap,
    key: AtomizedPropertyKey<'_>,
) -> Option<Option<Arc<PrototypeValidity>>> {
    if !object::supports_fast_property_ic(obj, heap) {
        return None;
    }
    if !matches!(
        object::lookup_own_atom(obj, heap, key).lookup,
        object::PropertyLookup::Absent
    ) {
        return None;
    }
    if object::prototype_value(obj, heap).is_none() {
        return Some(None);
    }
    let first = object::prototype(obj, heap)?;
    let validity = chain_validity(first, heap)?;
    let mut proto = first;
    for _ in 0..object::PROTO_CHAIN_HARD_CAP {
        if !object::supports_fast_property_ic(proto, heap)
            || !matches!(
                object::lookup_own_atom(proto, heap, key).lookup,
                object::PropertyLookup::Absent
            )
        {
            return None;
        }
        if object::prototype_value(proto, heap).is_none() {
            return Some(Some(validity));
        }
        proto = object::prototype(proto, heap)?;
    }
    None
}
