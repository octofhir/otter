//! Present own dense-slot eligibility and receiver-family classification.
//!
//! # Contents
//! - The cached body byte consumed by both native element backends.
//! - Prototype-only sidecar admission and conservative exotic rejection.
//!
//! # Invariants
//! - A zero byte proves default own data semantics only for a present slot.
//!   It never permits a hole to bypass a prototype lookup.
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

impl ArrayExoticSlots {
    fn prototype_only(&self) -> bool {
        self.sparse_elements.is_none()
            && self.named_properties.is_none()
            && self.named_key_order.is_none()
            && self.accessors.is_none()
            && self.property_flags.is_none()
            && self.symbol_properties.is_null()
            && self.source_bytes.is_none()
            && self.extensible.0
        // `prototype_override` cannot affect a proved present own element.
        // `dirty` is bookkeeping, not a property or descriptor.
    }
}

impl ArrayBody {
    pub(super) fn current_dense_own_guard(&self) -> u8 {
        u8::from(!self.exotic().is_none_or(ArrayExoticSlots::prototype_only))
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
        if body.dense_own_guard.get() != 0 {
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
