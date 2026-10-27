//! Eligibility derived from immutable hidden-class semantics.
//!
//! # Contents
//! - `supports_fast_property_ic` is the runtime ordinary named-slot predicate.
//!
//! # Invariants
//! - Storage and lookup facts have one owner: `ShapeBody::state`.
//! - Provisional ordinary shapes may train runtime ICs; native preparation uses
//!   the separate `ShapeState::is_provisional` allocation-root predicate.
//! - Delete changes actual layout identity rather than storing a permanent latch.
//!
//! # See also
//! - `super::shape_state` owns the only state byte.
//! - `crate::property_ic` consumes ordinary runtime lookup facts.

use super::{ObjectBody, ShapeState};

#[must_use]
pub(super) fn supports_fast_property_ic(body: &ObjectBody) -> bool {
    let state = body.state();
    !state.is_dictionary() && !state.is_opaque()
}

/// Whether `body`'s own named slots are ordinary hidden-class storage: a
/// non-ordinary `[[Prototype]]` (a function value) decides only the keys the
/// object lacks.
pub(super) fn supports_fast_own_property_ic(body: &ObjectBody) -> bool {
    let state = body.state();
    let facts = ShapeState::OPAQUE_LOOKUP_MASK & !ShapeState::OPAQUE_PROTOTYPE_MASK;
    !state.is_dictionary() && state.bits() & facts == 0
}
