//! Interpreter inline-cache records for property bytecodes.
//!
//! This module keeps IC state out of the bytecode format. Caches are
//! interpreter-local, keyed by compiled function id plus bytecode pc, and only
//! guard ordinary object data slots for now.
//!
//! # Contents
//! - [`LoadPropertyIc`] — monomorphic own/direct-prototype data-slot load cache.
//! - [`StorePropertyIc`] — monomorphic existing-own-slot and guarded
//!   store-transition cache.
//! - [`HasPropertyIc`] — monomorphic own/direct-prototype presence cache.
//! - [`PropertyIcEntry`] — per-site cache state and miss policy.
//! - [`PropertyIcStats`] — aggregate IC counters for diagnostics/tests.
//!
//! # Invariants
//! - ICs are performance hints only; every miss falls back to ordinary
//!   ECMAScript property semantics.
//! - Proxies, accessors, symbols, computed keys, dictionary-compatible
//!   objects, and deep prototype hits are not cached.
//! - Cache guards include both shape identity and atom id.
//! - Store transition guard semantics live in [`crate::object`]'s
//!   shape-transition layer; this module stores only the frozen IC record.
//!
//! # See also
//! - [`crate::object`]
//! - [`crate::property_dispatch`]

use crate::object::{
    self, AtomOwnPropertyHit, OwnPropertySlotHit, ShapeId, StorePropertyTransition,
    StorePropertyTransitionKind,
};
use crate::property_atom::AtomizedPropertyKey;
use crate::{JsObject, JsString, Value};
use otter_gc::raw::SlotVisitor;

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

/// Property opcode family for shared IC lifecycle accounting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PropertyIcKind {
    /// `LoadProperty` site.
    Load,
    /// `StoreProperty` site.
    Store,
    /// `HasProperty` site.
    Has,
}

impl PropertyIcStats {
    /// Record a guarded IC hit.
    pub(crate) fn record_hit(&mut self, kind: PropertyIcKind) {
        match kind {
            PropertyIcKind::Load => self.load_hits += 1,
            PropertyIcKind::Store => self.store_hits += 1,
            PropertyIcKind::Has => self.has_hits += 1,
        }
    }

    /// Record an IC miss or absent active entry.
    fn record_miss(&mut self, kind: PropertyIcKind) {
        match kind {
            PropertyIcKind::Load => self.load_misses += 1,
            PropertyIcKind::Store => self.store_misses += 1,
            PropertyIcKind::Has => self.has_misses += 1,
        }
    }

    /// Record a new monomorphic IC install.
    fn record_install(&mut self, kind: PropertyIcKind) {
        match kind {
            PropertyIcKind::Load => self.load_installs += 1,
            PropertyIcKind::Store => self.store_installs += 1,
            PropertyIcKind::Has => self.has_installs += 1,
        }
    }

    /// Record a site disable.
    fn record_disable(&mut self, kind: PropertyIcKind) {
        match kind {
            PropertyIcKind::Load => self.load_disables += 1,
            PropertyIcKind::Store => self.store_disables += 1,
            PropertyIcKind::Has => self.has_disables += 1,
        }
    }
}

/// Per-site monomorphic IC state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum PropertyIcEntry<T> {
    /// Site has not installed an IC yet.
    #[default]
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

impl<T> PropertyIcEntry<T> {
    /// `true` when this site currently has a monomorphic cache.
    #[must_use]
    #[cfg(test)]
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

    /// Record a guard miss and update opcode-family counters.
    pub(crate) fn record_guard_miss_with_stats(
        &mut self,
        stats: &mut PropertyIcStats,
        kind: PropertyIcKind,
    ) {
        stats.record_miss(kind);
        if self.record_guard_miss() {
            stats.record_disable(kind);
        }
    }

    /// Record a miss when the site has no active monomorphic entry yet.
    pub(crate) fn record_uncached_miss_with_stats(
        &self,
        stats: &mut PropertyIcStats,
        kind: PropertyIcKind,
    ) {
        if !self.is_disabled() {
            stats.record_miss(kind);
        }
    }

    /// Install or replace this site's monomorphic entry and update counters.
    pub(crate) fn install_with_stats(
        &mut self,
        stats: &mut PropertyIcStats,
        kind: PropertyIcKind,
        ic: T,
    ) {
        if !self.is_disabled() {
            self.install(ic);
            stats.record_install(kind);
        }
    }

    /// Disable this site and update counters if it was not already disabled.
    pub(crate) fn disable_with_stats(&mut self, stats: &mut PropertyIcStats, kind: PropertyIcKind) {
        if !self.is_disabled() {
            self.disable();
            stats.record_disable(kind);
        }
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

impl PropertyIcEntry<StorePropertyIc> {
    pub(crate) fn trace_roots(&self, visitor: &mut SlotVisitor<'_>) {
        if let Self::Monomorphic { ic, .. } = self {
            ic.trace_roots(visitor);
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

    /// Replay this IC against an ordinary object receiver.
    #[must_use]
    pub(crate) fn probe(
        &self,
        obj: JsObject,
        heap: &otter_gc::GcHeap,
        key: &JsString,
    ) -> Option<()> {
        let receiver_shape_id = object::shape_id(obj, heap);
        if let Some(hit) = self.own_hit(receiver_shape_id, key)
            && object::has_own_slot(obj, heap, hit)
        {
            return Some(());
        }
        if let Some(hit) = self.direct_prototype_hit(receiver_shape_id, key)
            && let Some(proto) = object::prototype(obj, heap)
            && object::supports_fast_property_ic(proto, heap)
            && object::has_own_slot(proto, heap, hit)
        {
            return Some(());
        }
        None
    }

    /// Build a presence IC candidate for the current receiver/key pair.
    #[must_use]
    pub(crate) fn install_candidate(
        obj: JsObject,
        heap: &otter_gc::GcHeap,
        key: &JsString,
    ) -> Option<Self> {
        if !object::supports_fast_property_ic(obj, heap) {
            return None;
        }
        let key_name = key.to_lossy_string();
        let receiver_shape_id = object::shape_id(obj, heap);
        let (own_hit, own_lookup) = object::lookup_own_slot(obj, heap, &key_name);
        if let (Some(hit), object::PropertyLookup::Data { .. }) = (own_hit, own_lookup) {
            return Some(Self::own_data(key.clone(), hit));
        }
        let proto = object::prototype(obj, heap)?;
        if !object::supports_fast_property_ic(proto, heap) {
            return None;
        }
        let (proto_hit, proto_lookup) = object::lookup_own_slot(proto, heap, &key_name);
        if let (Some(hit), object::PropertyLookup::Data { .. }) = (proto_hit, proto_lookup) {
            return Some(Self::direct_prototype_data(
                receiver_shape_id,
                key.clone(),
                hit,
            ));
        }
        None
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

    /// Replay this IC against an ordinary object receiver.
    #[must_use]
    pub(crate) fn load(
        self,
        obj: JsObject,
        heap: &otter_gc::GcHeap,
        key: AtomizedPropertyKey<'_>,
    ) -> Option<Value> {
        let receiver_shape_id = object::shape_id(obj, heap);
        if let Some(hit) = self.matches_own_data(receiver_shape_id, key)
            && let Some(value) = object::load_own_data_slot_atom(obj, heap, key, hit)
        {
            return Some(value);
        }
        if let Some(hit) = self.direct_prototype_hit(receiver_shape_id, key)
            && let Some(proto) = object::prototype(obj, heap)
            && object::supports_fast_property_ic(proto, heap)
        {
            return object::load_own_data_slot_atom(proto, heap, key, hit);
        }
        None
    }

    /// Build a load IC candidate for the current receiver/key pair.
    #[must_use]
    pub(crate) fn install_candidate(
        obj: JsObject,
        heap: &otter_gc::GcHeap,
        key: AtomizedPropertyKey<'_>,
    ) -> Option<(Self, Value)> {
        if !object::supports_fast_property_ic(obj, heap) {
            return None;
        }
        let receiver_shape_id = object::shape_id(obj, heap);
        let atom_lookup = object::lookup_own_atom(obj, heap, key);
        if let (Some(hit), object::PropertyLookup::Data { value, flags: _ }) =
            (atom_lookup.hit, atom_lookup.lookup)
        {
            return Some((Self::own_data(hit), value));
        }
        if atom_lookup.hit.is_some() {
            return None;
        }
        let proto = object::prototype(obj, heap)?;
        if !object::supports_fast_property_ic(proto, heap) {
            return None;
        }
        let proto_lookup = object::lookup_own_atom(proto, heap, key);
        if let (Some(hit), object::PropertyLookup::Data { value, flags: _ }) =
            (proto_lookup.hit, proto_lookup.lookup)
        {
            return Some((Self::direct_prototype_data(receiver_shape_id, hit), value));
        }
        None
    }
}

/// Monomorphic `StoreProperty` cache for ordinary data-slot writes.
#[derive(Debug, Clone)]
pub(crate) enum StorePropertyIc {
    /// Receiver already owns the writable data slot.
    ExistingOwnDataStore {
        /// Guarded slot metadata.
        hit: AtomOwnPropertyHit,
    },
    /// Receiver can add one data slot with a `null` prototype.
    OwnAddTransition {
        /// Guarded hidden-class transition.
        transition: StorePropertyTransition,
    },
    /// Receiver can add one data slot when its direct prototype still misses
    /// the key and has no deeper chain.
    DirectPrototypeMissingTransition {
        /// Guarded hidden-class transition.
        transition: StorePropertyTransition,
    },
    /// Receiver can add one data slot when its direct prototype still owns a
    /// writable data property for this key.
    DirectPrototypeWritableDataTransition {
        /// Guarded hidden-class transition.
        transition: StorePropertyTransition,
    },
}

impl StorePropertyIc {
    /// Create an IC from an atom-aware object lookup hit.
    #[must_use]
    pub(crate) const fn own_data(hit: AtomOwnPropertyHit) -> Self {
        Self::ExistingOwnDataStore { hit }
    }

    /// Create an IC from a guarded store-property transition.
    #[must_use]
    pub(crate) fn transition(transition: StorePropertyTransition) -> Self {
        match &transition.kind {
            StorePropertyTransitionKind::OwnAdd => Self::OwnAddTransition { transition },
            StorePropertyTransitionKind::DirectPrototypeMissing { .. } => {
                Self::DirectPrototypeMissingTransition { transition }
            }
            StorePropertyTransitionKind::DirectPrototypeWritableData { .. } => {
                Self::DirectPrototypeWritableDataTransition { transition }
            }
        }
    }

    pub(crate) fn trace_roots(&self, visitor: &mut SlotVisitor<'_>) {
        match self {
            Self::ExistingOwnDataStore { .. } => {}
            Self::OwnAddTransition { transition }
            | Self::DirectPrototypeMissingTransition { transition }
            | Self::DirectPrototypeWritableDataTransition { transition } => {
                transition.trace_roots(visitor);
            }
        }
    }

    /// Own-data IC metadata when the receiver shape/key guard matches.
    #[must_use]
    pub(crate) fn own_data_hit(
        &self,
        shape_id: ShapeId,
        key: AtomizedPropertyKey<'_>,
    ) -> Option<AtomOwnPropertyHit> {
        let Self::ExistingOwnDataStore { hit } = self else {
            return None;
        };
        (hit.shape_id == shape_id && hit.atom_id == key.atom().id()).then_some(*hit)
    }

    /// Add-transition metadata when the receiver shape/key guard matches.
    #[must_use]
    pub(crate) fn store_transition(
        &self,
        shape_id: ShapeId,
        key: AtomizedPropertyKey<'_>,
    ) -> Option<&StorePropertyTransition> {
        let transition = match self {
            Self::OwnAddTransition { transition }
            | Self::DirectPrototypeMissingTransition { transition }
            | Self::DirectPrototypeWritableDataTransition { transition } => transition,
            Self::ExistingOwnDataStore { .. } => return None,
        };
        (transition.from_shape_id == shape_id && transition.atom_id == key.atom().id())
            .then_some(transition)
    }

    /// Replay this IC against an ordinary object receiver.
    pub(crate) fn store(
        &self,
        obj: JsObject,
        heap: &mut otter_gc::GcHeap,
        key: AtomizedPropertyKey<'_>,
        value: &Value,
    ) -> Option<()> {
        if !object::supports_fast_property_ic(obj, heap) {
            return None;
        }
        let shape_id = object::shape_id(obj, heap);
        if let Some(hit) = self.own_data_hit(shape_id, key) {
            return object::store_own_data_slot_atom(obj, heap, key, hit, value);
        }
        if let Some(transition) = self.store_transition(shape_id, key) {
            return object::replay_store_property_transition(obj, heap, key, transition, value);
        }
        None
    }

    /// Build an existing-own-data store IC candidate for the current receiver.
    ///
    /// Add-transition capture lives in the object shape-transition layer
    /// because it performs the write while creating replay metadata.
    #[must_use]
    pub(crate) fn existing_own_data_install_candidate(
        obj: JsObject,
        heap: &otter_gc::GcHeap,
        key: AtomizedPropertyKey<'_>,
    ) -> Option<Self> {
        if !object::supports_fast_property_ic(obj, heap) {
            return None;
        }
        let atom_lookup = object::lookup_own_atom(obj, heap, key);
        let (Some(hit), object::PropertyLookup::Data { flags, value: _ }) =
            (atom_lookup.hit, atom_lookup.lookup)
        else {
            return None;
        };
        flags.writable().then_some(Self::own_data(hit))
    }
}

#[cfg(test)]
mod tests {
    use super::{
        HasPropertyIc, LoadPropertyIc, PropertyIcEntry, PropertyIcKind, PropertyIcStats,
        StorePropertyIc,
    };
    use crate::object::{self, PropertyDescriptor};
    use crate::property_atom::{AtomId, AtomizedPropertyKey, PropertyAtom};
    use crate::{JsString, Value};

    fn fresh_heap() -> otter_gc::GcHeap {
        otter_gc::GcHeap::new().expect("init heap")
    }

    fn key<'a>(name: &'a str) -> AtomizedPropertyKey<'a> {
        AtomizedPropertyKey::new(PropertyAtom::new(AtomId::from_constant_index(7)), name)
    }

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

    #[test]
    fn entry_lifecycle_updates_opcode_family_stats() {
        let mut stats = PropertyIcStats::default();
        let mut entry = PropertyIcEntry::Empty;

        entry.record_uncached_miss_with_stats(&mut stats, PropertyIcKind::Load);
        assert_eq!(stats.load_misses, 1);

        entry.install_with_stats(&mut stats, PropertyIcKind::Load, 7_u8);
        assert_eq!(stats.load_installs, 1);
        stats.record_hit(PropertyIcKind::Load);
        assert_eq!(stats.load_hits, 1);

        entry.record_guard_miss_with_stats(&mut stats, PropertyIcKind::Load);
        entry.record_guard_miss_with_stats(&mut stats, PropertyIcKind::Load);
        entry.record_guard_miss_with_stats(&mut stats, PropertyIcKind::Load);
        assert_eq!(stats.load_misses, 4);
        assert_eq!(stats.load_disables, 0);

        entry.record_guard_miss_with_stats(&mut stats, PropertyIcKind::Load);
        assert!(entry.is_disabled());
        assert_eq!(stats.load_misses, 5);
        assert_eq!(stats.load_disables, 1);

        entry.install_with_stats(&mut stats, PropertyIcKind::Load, 8_u8);
        entry.disable_with_stats(&mut stats, PropertyIcKind::Load);
        assert_eq!(stats.load_installs, 1);
        assert_eq!(stats.load_disables, 1);
    }

    #[test]
    fn direct_prototype_load_ic_rejects_dictionary_compatible_prototype() {
        let mut heap = fresh_heap();
        let proto = object::alloc_object_old_for_fixture(&mut heap).unwrap();
        object::set(proto, &mut heap, "x", Value::Boolean(true));
        object::set(proto, &mut heap, "y", Value::Null);
        let receiver = object::alloc_object_old_for_fixture(&mut heap).unwrap();
        object::set_prototype(receiver, &mut heap, Some(proto));
        let (ic, value) =
            LoadPropertyIc::install_candidate(receiver, &heap, key("x")).expect("load ic");
        assert_eq!(value, Value::Boolean(true));

        assert!(object::delete(proto, &mut heap, "y"));

        assert_eq!(ic.load(receiver, &heap, key("x")), None);
    }

    #[test]
    fn direct_prototype_has_ic_rejects_dictionary_compatible_prototype() {
        let mut heap = fresh_heap();
        let proto = object::alloc_object_old_for_fixture(&mut heap).unwrap();
        object::set(proto, &mut heap, "x", Value::Boolean(true));
        object::set(proto, &mut heap, "y", Value::Null);
        let receiver = object::alloc_object_old_for_fixture(&mut heap).unwrap();
        object::set_prototype(receiver, &mut heap, Some(proto));
        let string_heap = crate::StringHeap::with_cap(1024);
        let key_string = JsString::from_str("x", &string_heap).expect("string");
        let ic = HasPropertyIc::install_candidate(receiver, &heap, &key_string).expect("has ic");

        assert!(object::delete(proto, &mut heap, "y"));

        assert_eq!(ic.probe(receiver, &heap, &key_string), None);
    }

    #[test]
    fn direct_prototype_store_transition_rejects_dictionary_compatible_prototype() {
        let mut heap = fresh_heap();
        let proto = object::alloc_object_old_for_fixture(&mut heap).unwrap();
        object::set(proto, &mut heap, "x", Value::Boolean(true));
        object::set(proto, &mut heap, "y", Value::Null);
        let first = object::alloc_object_old_for_fixture(&mut heap).unwrap();
        object::set_prototype(first, &mut heap, Some(proto));
        let transition = object::capture_store_property_transition(
            first,
            &mut heap,
            key("x"),
            &Value::Boolean(false),
        )
        .expect("store transition");
        let ic = StorePropertyIc::transition(transition);
        let second = object::alloc_object_old_for_fixture(&mut heap).unwrap();
        object::set_prototype(second, &mut heap, Some(proto));

        assert!(object::delete(proto, &mut heap, "y"));

        assert_eq!(ic.store(second, &mut heap, key("x"), &Value::Null), None);
        assert_eq!(object::get_own(second, &heap, "x"), None);
    }

    #[test]
    fn existing_own_store_candidate_rejects_non_writable_data() {
        let mut heap = fresh_heap();
        let obj = object::alloc_object_old_for_fixture(&mut heap).unwrap();
        assert!(object::define_own_property(
            obj,
            &mut heap,
            "x",
            PropertyDescriptor::data(Value::Boolean(true), false, true, true),
        ));

        assert!(
            StorePropertyIc::existing_own_data_install_candidate(obj, &heap, key("x")).is_none()
        );
    }
}
