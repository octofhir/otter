//! Interpreter inline-cache records for property bytecodes.
//!
//! This module keeps IC state out of the bytecode format. Caches are
//! interpreter-local, keyed by compiled function id plus bytecode pc, and only
//! guard ordinary object data slots for now.
//!
//! # Contents
//! - [`LoadPropertyIc`] — monomorphic own-data-slot load cache.
//! - [`StorePropertyIc`] — monomorphic own-data-slot store cache.
//! - [`PropertyIcEntry`] — per-site cache state and miss policy.
//! - [`PropertyIcStats`] — aggregate IC counters for diagnostics/tests.
//!
//! # Invariants
//! - ICs are performance hints only; every miss falls back to ordinary
//!   ECMAScript property semantics.
//! - Proxies, accessors, symbols, computed keys, and prototype hits are not
//!   cached by this first slice.
//! - Cache guards include both shape identity and atom id.
//!
//! # See also
//! - [`crate::object`]
//! - [`crate::property_dispatch`]

use crate::object::{AtomOwnPropertyHit, ShapeId};
use crate::property_atom::AtomizedPropertyKey;

const MONOMORPHIC_MISS_DISABLE_THRESHOLD: u8 = 4;

/// Aggregate inline-cache counters for named property bytecodes.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct PropertyIcStats {
    /// Guarded `LoadProperty` fast-path hits.
    pub load_hits: u64,
    /// `LoadProperty` object receivers that missed or had no IC entry.
    pub load_misses: u64,
    /// `LoadProperty` IC entries installed or replaced.
    pub load_installs: u64,
    /// `LoadProperty` sites disabled after repeated guard misses.
    pub load_disables: u64,
    /// Guarded `StoreProperty` fast-path hits.
    pub store_hits: u64,
    /// `StoreProperty` ordinary object receivers that missed or had no IC entry.
    pub store_misses: u64,
    /// `StoreProperty` IC entries installed or replaced.
    pub store_installs: u64,
    /// `StoreProperty` sites disabled after repeated guard misses.
    pub store_disables: u64,
}

/// Per-site monomorphic IC state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PropertyIcEntry<T> {
    /// Site has not installed an IC yet.
    Empty,
    /// Site guards one receiver shape/key pair and tracks guard misses.
    Monomorphic {
        /// Cached IC record.
        ic: T,
        /// Guard misses since install.
        misses: u8,
    },
    /// Site was too unstable for a monomorphic IC.
    Disabled,
}

impl<T> Default for PropertyIcEntry<T> {
    fn default() -> Self {
        Self::Empty
    }
}

impl<T: Copy> PropertyIcEntry<T> {
    /// Cached monomorphic record, if the site is active.
    #[must_use]
    pub(crate) const fn cached(self) -> Option<T> {
        match self {
            Self::Monomorphic { ic, .. } => Some(ic),
            Self::Empty | Self::Disabled => None,
        }
    }

    /// `true` when this site currently has a monomorphic cache.
    #[must_use]
    #[allow(dead_code)]
    pub(crate) const fn is_monomorphic(self) -> bool {
        matches!(self, Self::Monomorphic { .. })
    }

    /// `true` when this site should not reinstall a monomorphic IC.
    #[must_use]
    pub(crate) const fn is_disabled(self) -> bool {
        matches!(self, Self::Disabled)
    }

    /// Install or replace the monomorphic record, resetting miss count.
    pub(crate) fn install(&mut self, ic: T) {
        if !self.is_disabled() {
            *self = Self::Monomorphic { ic, misses: 0 };
        }
    }

    /// Record one guard miss. Returns `true` when this miss disabled the site.
    pub(crate) fn record_guard_miss(&mut self) -> bool {
        let Self::Monomorphic { misses, .. } = self else {
            return false;
        };
        *misses = misses.saturating_add(1);
        if *misses < MONOMORPHIC_MISS_DISABLE_THRESHOLD {
            return false;
        }
        *self = Self::Disabled;
        true
    }
}

/// Monomorphic `LoadProperty` cache for ordinary data slots.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LoadPropertyIc {
    /// Receiver owns the data slot.
    OwnData {
        /// Guarded receiver slot metadata.
        hit: AtomOwnPropertyHit,
    },
    /// Receiver's direct prototype owns the data slot.
    DirectPrototypeData {
        /// Receiver shape observed before walking to `[[Prototype]]`.
        receiver_shape_id: ShapeId,
        /// Guarded direct-prototype slot metadata.
        hit: AtomOwnPropertyHit,
    },
}

impl LoadPropertyIc {
    /// Create an own-data IC from an atom-aware object lookup hit.
    #[must_use]
    pub(crate) const fn own_data(hit: AtomOwnPropertyHit) -> Self {
        Self::OwnData { hit }
    }

    /// Create a direct-prototype data IC.
    #[must_use]
    pub(crate) const fn direct_prototype_data(
        receiver_shape_id: ShapeId,
        hit: AtomOwnPropertyHit,
    ) -> Self {
        Self::DirectPrototypeData {
            receiver_shape_id,
            hit,
        }
    }

    /// `true` when this own-data IC applies to the current receiver/key pair.
    #[must_use]
    pub(crate) fn matches_own_data(
        self,
        shape_id: ShapeId,
        key: AtomizedPropertyKey<'_>,
    ) -> Option<AtomOwnPropertyHit> {
        let Self::OwnData { hit } = self else {
            return None;
        };
        (hit.shape_id == shape_id && hit.atom_id == key.atom().id()).then_some(hit)
    }

    /// Direct-prototype IC metadata when the receiver shape/key guard matches.
    #[must_use]
    pub(crate) fn direct_prototype_hit(
        self,
        receiver_shape_id: ShapeId,
        key: AtomizedPropertyKey<'_>,
    ) -> Option<AtomOwnPropertyHit> {
        let Self::DirectPrototypeData {
            receiver_shape_id: cached_receiver_shape_id,
            hit,
        } = self
        else {
            return None;
        };
        (cached_receiver_shape_id == receiver_shape_id && hit.atom_id == key.atom().id())
            .then_some(hit)
    }
}

/// Monomorphic `StoreProperty` cache for an ordinary own writable data slot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct StorePropertyIc {
    /// Guarded slot metadata.
    pub(crate) hit: AtomOwnPropertyHit,
}

impl StorePropertyIc {
    /// Create an IC from an atom-aware object lookup hit.
    #[must_use]
    pub(crate) const fn from_hit(hit: AtomOwnPropertyHit) -> Self {
        Self { hit }
    }

    /// `true` when this IC applies to the current receiver/key pair.
    #[must_use]
    pub(crate) fn matches(self, shape_id: ShapeId, key: AtomizedPropertyKey<'_>) -> bool {
        self.hit.shape_id == shape_id && self.hit.atom_id == key.atom().id()
    }
}

#[cfg(test)]
mod tests {
    use super::PropertyIcEntry;

    #[test]
    fn monomorphic_entry_disables_after_repeated_guard_misses() {
        let mut entry = PropertyIcEntry::Empty;
        entry.install(7_u8);

        assert_eq!(entry.cached(), Some(7));
        assert!(!entry.is_disabled());
        assert!(!entry.record_guard_miss());
        assert!(!entry.record_guard_miss());
        assert!(!entry.record_guard_miss());

        assert!(entry.record_guard_miss());
        assert!(entry.is_disabled());
        assert_eq!(entry.cached(), None);

        entry.install(8);
        assert!(entry.is_disabled());
        assert_eq!(entry.cached(), None);
    }
}
