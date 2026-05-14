//! Fast-shape eligibility state for property inline caches.
//!
//! This module owns the small internal contract that says whether an ordinary
//! object may participate in hidden-class based IC assumptions. It does not
//! implement dictionary storage; it only gives shape-mutating operations a
//! single hook for leaving the fast-shape contract.
//!
//! # Contents
//! - [`ShapeCacheMode`] — current fast-shape eligibility state.
//! - [`ShapeCacheInvalidation`] — mutation reasons that leave fast mode.
//! - [`supports_fast_property_ic`] — common receiver/prototype IC eligibility
//!   predicate.
//! - [`invalidate_fast_shape_assumptions`] — transition to the
//!   dictionary-compatible contract.
//!
//! # Invariants
//! - Only [`ShapeCacheMode::Fast`] objects may install or replay hidden-class
//!   IC entries.
//! - String exotic wrapper objects are never fast-shape IC receivers even when
//!   their hidden-class state is otherwise fast.
//! - Delete invalidates fast-shape assumptions immediately; today's storage
//!   still rebuilds a shape, but future dictionary mode can reuse this hook.
//!
//! # See also
//! - [`super::Shape`]
//! - [`crate::property_ic`]

use super::ObjectBody;

/// Internal eligibility mode for hidden-class based IC assumptions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ShapeCacheMode {
    /// Object is represented by append-only shape transitions.
    Fast,
    /// Object has taken a mutation such as delete that future dictionary mode
    /// may represent without ordinary append-only transitions.
    DictionaryCompatible,
}

/// Reason an object leaves fast-shape IC eligibility.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ShapeCacheInvalidation {
    /// String-keyed own property deletion breaks append-only shape history.
    DeleteOwnProperty,
}

/// Common IC eligibility predicate for ordinary object receivers/prototypes.
#[must_use]
pub(super) const fn supports_fast_property_ic(body: &ObjectBody) -> bool {
    matches!(body.shape_cache_mode, ShapeCacheMode::Fast) && body.string_data.is_none()
}

/// Mark the object as no longer valid for hidden-class IC assumptions.
pub(super) fn invalidate_fast_shape_assumptions(
    body: &mut ObjectBody,
    reason: ShapeCacheInvalidation,
) {
    match reason {
        ShapeCacheInvalidation::DeleteOwnProperty => {
            body.shape_cache_mode = ShapeCacheMode::DictionaryCompatible;
        }
    }
}
