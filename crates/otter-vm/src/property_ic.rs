//! Interpreter inline-cache records for property bytecodes.
//!
//! This module keeps IC state out of the bytecode format. Caches are
//! interpreter-local, keyed by compiled function id plus bytecode pc, and only
//! guard ordinary object data slots for now.
//!
//! # Contents
//! - [`LoadPropertyIc`] — monomorphic own/direct-prototype data-slot load cache.
//! - [`StorePropertyIc`] — monomorphic own-data-slot and add-transition store cache.
//! - [`HasPropertyIc`] — monomorphic own/direct-prototype presence cache.
//! - [`PropertyIcEntry`] — per-site cache state and miss policy.
//! - [`PropertyIcStats`] — aggregate IC counters for diagnostics/tests.
//!
//! # Invariants
//! - ICs are performance hints only; every miss falls back to ordinary
//!   ECMAScript property semantics.
//! - Proxies, accessors, symbols, computed keys, and deep prototype hits are
//!   not cached.
//! - Cache guards include both shape identity and atom id.
//!
//! # See also
//! - [`crate::object`]
//! - [`crate::property_dispatch`]

use crate::JsString;
use crate::object::{AddOwnPropertyTransition, AtomOwnPropertyHit, OwnPropertySlotHit, ShapeId};
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
    /// Guarded `HasProperty` fast-path hits.
    pub has_hits: u64,
    /// `HasProperty` ordinary object receivers that missed or had no IC entry.
    pub has_misses: u64,
    /// `HasProperty` IC entries installed or replaced.
    pub has_installs: u64,
    /// `HasProperty` sites disabled after repeated guard misses.
    pub has_disables: u64,
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

impl<T> PropertyIcEntry<T> {
    /// `true` when this site currently has a monomorphic cache.
    #[must_use]
    #[allow(dead_code)]
    pub(crate) const fn is_monomorphic(&self) -> bool {
        matches!(self, Self::Monomorphic { .. })
    }

    /// `true` when this site should not reinstall a monomorphic IC.
    #[must_use]
    pub(crate) const fn is_disabled(&self) -> bool {
        matches!(self, Self::Disabled)
    }

    /// Install or replace the monomorphic record, resetting miss count.
    pub(crate) fn install(&mut self, ic: T) {
        if !self.is_disabled() {
            *self = Self::Monomorphic { ic, misses: 0 };
        }
    }

    /// Permanently disable this site for the interpreter lifetime.
    pub(crate) fn disable(&mut self) {
        *self = Self::Disabled;
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

impl<T: Clone> PropertyIcEntry<T> {
    /// Cached monomorphic record, if the site is active.
    #[must_use]
    pub(crate) fn cached(&self) -> Option<T> {
        match self {
            Self::Monomorphic { ic, .. } => Some(ic.clone()),
            Self::Empty | Self::Disabled => None,
        }
    }
}

impl<T> PropertyIcEntry<T> {
    /// Borrow the cached monomorphic record without cloning it.
    #[must_use]
    pub(crate) fn cached_ref(&self) -> Option<&T> {
        match self {
            Self::Monomorphic { ic, .. } => Some(ic),
            Self::Empty | Self::Disabled => None,
        }
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

/// Monomorphic `HasProperty` cache for ordinary data-property presence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum HasPropertyIc {
    /// Receiver owns the data slot.
    OwnData {
        /// Guarded key as loaded by the `in` bytecode.
        key: JsString,
        /// Guarded receiver slot metadata.
        hit: OwnPropertySlotHit,
    },
    /// Receiver's direct prototype owns the data slot.
    DirectPrototypeData {
        /// Receiver shape observed before walking to `[[Prototype]]`.
        receiver_shape_id: ShapeId,
        /// Guarded key as loaded by the `in` bytecode.
        key: JsString,
        /// Guarded direct-prototype slot metadata.
        hit: OwnPropertySlotHit,
    },
}

impl HasPropertyIc {
    /// Create an own-data presence IC.
    #[must_use]
    pub(crate) fn own_data(key: JsString, hit: OwnPropertySlotHit) -> Self {
        Self::OwnData { key, hit }
    }

    /// Create a direct-prototype data presence IC.
    #[must_use]
    pub(crate) fn direct_prototype_data(
        receiver_shape_id: ShapeId,
        key: JsString,
        hit: OwnPropertySlotHit,
    ) -> Self {
        Self::DirectPrototypeData {
            receiver_shape_id,
            key,
            hit,
        }
    }

    /// Own-data IC metadata when the receiver shape/key guard matches.
    #[must_use]
    pub(crate) fn own_hit(
        &self,
        shape_id: ShapeId,
        key_value: &JsString,
    ) -> Option<OwnPropertySlotHit> {
        let Self::OwnData { key, hit } = self else {
            return None;
        };
        (hit.shape_id == shape_id && key.equals(key_value)).then_some(*hit)
    }

    /// Direct-prototype IC metadata when the receiver shape/key guard matches.
    #[must_use]
    pub(crate) fn direct_prototype_hit(
        &self,
        receiver_shape_id: ShapeId,
        key_value: &JsString,
    ) -> Option<OwnPropertySlotHit> {
        let Self::DirectPrototypeData {
            receiver_shape_id: cached_receiver_shape_id,
            key,
            hit,
        } = self
        else {
            return None;
        };
        (*cached_receiver_shape_id == receiver_shape_id && key.equals(key_value)).then_some(*hit)
    }
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

/// Monomorphic `StoreProperty` cache for ordinary data-slot writes.
#[derive(Debug, Clone)]
pub(crate) enum StorePropertyIc {
    /// Receiver already owns the writable data slot.
    OwnData {
        /// Guarded slot metadata.
        hit: AtomOwnPropertyHit,
    },
    /// Receiver is a fresh null-prototype object that can add one data slot.
    AddOwnData {
        /// Guarded hidden-class transition.
        transition: AddOwnPropertyTransition,
    },
}

impl StorePropertyIc {
    /// Create an IC from an atom-aware object lookup hit.
    #[must_use]
    pub(crate) const fn own_data(hit: AtomOwnPropertyHit) -> Self {
        Self::OwnData { hit }
    }

    /// Create an IC from an add-property transition.
    #[must_use]
    pub(crate) const fn add_own_data(transition: AddOwnPropertyTransition) -> Self {
        Self::AddOwnData { transition }
    }

    /// Own-data IC metadata when the receiver shape/key guard matches.
    #[must_use]
    pub(crate) fn own_data_hit(
        &self,
        shape_id: ShapeId,
        key: AtomizedPropertyKey<'_>,
    ) -> Option<AtomOwnPropertyHit> {
        let Self::OwnData { hit } = self else {
            return None;
        };
        (hit.shape_id == shape_id && hit.atom_id == key.atom().id()).then_some(*hit)
    }

    /// Add-transition metadata when the receiver shape/key guard matches.
    #[must_use]
    pub(crate) fn add_own_data_transition(
        &self,
        shape_id: ShapeId,
        key: AtomizedPropertyKey<'_>,
    ) -> Option<&AddOwnPropertyTransition> {
        let Self::AddOwnData { transition } = self else {
            return None;
        };
        (transition.from_shape_id == shape_id && transition.atom_id == key.atom().id())
            .then_some(transition)
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
