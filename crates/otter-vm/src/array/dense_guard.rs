//! Present own dense-slot eligibility and receiver-family classification.
//!
//! # Contents
//! - The cached body byte consumed by both native element backends.
//! - Sidecar admission for state that leaves indexed slots ordinary, and
//!   conservative rejection of everything that reaches them.
//!
//! # Invariants
//! - A clear exotic bit proves default own data semantics only for a present
//!   slot. It never permits a hole to bypass a prototype lookup.
//! - The shared bit marks copy-on-write storage: reads only test the exotic
//!   bit, while every in-place write requires the whole byte to be zero.
//! - Managed named state never aliases an index, so the expando leaves the
//!   byte zero. A RegExp match
//!   result (`index`, `input`, `groups`) keeps native element access.
//! - Mutable sidecar exposure invalidates the byte before a write; completed
//!   mutations and traced relocation recompute it from the current sidecar.
//! - Holey numeric access still requires no sidecar, because its hole result
//!   may use the realm prototype protector to answer `undefined`.
//!
//! # See also
//! - [`super`] owns mutation and collector-rewritten element caches.
//! - [`crate::jit::JitElementAccess`] owns the typed native guard offsets.

use super::{ArrayBody, ArrayExoticSlots, DenseElementKind, JsArray};
use crate::jit::JitElementFamily;

/// Own-guard bit: an indexed slot may lack default own data semantics.
/// Every native element access requires it clear.
pub(crate) const DENSE_OWN_GUARD_EXOTIC: u8 = 1;
/// Own-guard bit: the dense storage is a literal site's shared boilerplate.
/// Native reads ignore it; native writes require the whole byte clear.
pub(crate) const DENSE_OWN_GUARD_SHARED: u8 = 2;

impl ArrayExoticSlots {
    /// Whether every present dense slot still has default own data
    /// semantics: no sparse or JSON-source state, extensible, and no accessor
    /// or descriptor on an index key.
    fn leaves_dense_slots_ordinary(&self) -> bool {
        self.sparse_elements.is_none()
            && self.source_bytes.is_none()
            && self.extensible.0
            && self
                .accessors
                .as_ref()
                .is_none_or(|accessors| accessors.is_empty())
            && self
                .property_flags
                .as_ref()
                .is_none_or(|flags| flags.is_empty())
        // `prototype_override` cannot affect a proved present own element.
        // The managed expando never aliases an index.
        // `dirty` is bookkeeping, not a property or descriptor.
    }
}

impl ArrayBody {
    pub(super) fn current_dense_own_guard(&self) -> u8 {
        let exotic = !self
            .exotic()
            .is_none_or(ArrayExoticSlots::leaves_dense_slots_ordinary);
        let shared = super::elements::is_shared(self.slab);
        u8::from(exotic) * DENSE_OWN_GUARD_EXOTIC + u8::from(shared) * DENSE_OWN_GUARD_SHARED
    }

    pub(crate) fn refresh_dense_own_guard(&self) {
        self.dense_own_guard.set(self.current_dense_own_guard());
    }
}

/// The immutable native family this receiver can actually satisfy.
/// Exotic indexed semantics stay generic instead of repeatedly failing a
/// guard that their own feedback selected.
pub(crate) fn element_family(array: JsArray, heap: &otter_gc::GcHeap) -> JitElementFamily {
    heap.read_payload(array, |body| {
        debug_assert_eq!(body.dense_own_guard.get(), body.current_dense_own_guard());
        // Shared storage reads like any present dense prefix; a store at the
        // same site copies it out before writing.
        if body.dense_own_guard.get() & DENSE_OWN_GUARD_EXOTIC != 0 {
            return JitElementFamily::Generic;
        }
        match body.dense_kind() {
            DenseElementKind::PackedDouble => JitElementFamily::DenseFloat64,
            DenseElementKind::Tagged => JitElementFamily::DenseTagged,
            DenseElementKind::HoleyDouble if body.exotic.is_null() => {
                JitElementFamily::DenseHoleyFloat64
            }
            DenseElementKind::HoleyDouble => JitElementFamily::Generic,
            DenseElementKind::Empty => JitElementFamily::Unseen,
        }
    })
}

#[cfg(test)]
mod tests;
