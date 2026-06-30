//! Interpreter inline-cache records for property bytecodes.
//!
//! This module keeps IC state out of the bytecode format. Caches are
//! interpreter-local, keyed by compiled function id plus bytecode pc.
//! Each site holds up to four polymorphic entries (a fixed PIC) plus
//! a megamorphic terminal state for sites whose receiver shape churn
//! exceeds the PIC capacity.
//!
//! # Contents
//!   transition cache.
//! - [`PropertyIcEntry`] — per-site cache state and miss policy.
//! - [`PropertyIcStats`] — aggregate IC counters for diagnostics/tests.
//!
//! # Invariants
//! - ICs are performance hints only; every miss falls back to ordinary
//!   ECMAScript property semantics.
//! - Proxies, accessors, symbols, computed keys, dictionary-compatible
//!   objects, and deep prototype hits are not cached.
//! - Cache guards include both shape identity and atom id.
//! - PIC capacity is fixed at [`MAX_PIC_ENTRIES`]; a probe through all
//!   entries that still misses contributes to the shared `misses`
//!   counter on the site. Once `misses` reaches
//!   [`PIC_GUARD_MISS_THRESHOLD`] **and** the PIC is full, the site
//!   transitions to [`PropertyIcEntry::Megamorphic`] and is never
//!   re-populated for the interpreter lifetime.
//! - Store transition guard semantics live in [`crate::object`]'s
//!   shape-transition layer; this module stores only the frozen IC record.
//!
//! # See also
//! - [`crate::object`]
//! - [`crate::property_dispatch`]

use otter_gc::raw::SlotVisitor;
use smallvec::SmallVec;

/// Maximum polymorphic entries stored per site before the site
/// transitions to [`PropertyIcEntry::Megamorphic`]. Four matches the
/// shape mix in real-world JS object factories (V8 Ignition, Boa,
/// JSC) without ballooning per-site memory.
pub(crate) const MAX_PIC_ENTRIES: usize = 4;

/// Miss budget across all PIC entries before a full PIC transitions
/// to megamorphic. Aligns with the pre-PIC monomorphic disable
/// threshold so single-shape micro-benchmarks behave identically.
const PIC_GUARD_MISS_THRESHOLD: u8 = 4;

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

/// Per-site polymorphic inline cache state.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) enum PropertyIcEntry<T> {
    /// Site has not installed any IC yet.
    #[default]
    Empty,
    /// Site holds 1..=[`MAX_PIC_ENTRIES`] guarded IC records. Probe
    /// walks them in install order. `misses` counts probes whose
    /// receiver shape did not match any entry; when it reaches the
    /// threshold and the PIC is full, the site transitions to
    /// [`Self::Megamorphic`].
    Polymorphic {
        /// Cached IC records, in install order. Capped at
        /// [`MAX_PIC_ENTRIES`].
        entries: SmallVec<[T; MAX_PIC_ENTRIES]>,
        /// Guard misses since install across all entries.
        misses: u8,
    },
    /// Site saw more shape diversity than the PIC could absorb and is
    /// permanently bypassed for the interpreter lifetime.
    Megamorphic,
}

impl<T> PropertyIcEntry<T> {
    /// `true` when this site currently holds at least one PIC entry.
    #[must_use]
    #[cfg(test)]
    pub(crate) fn is_polymorphic(&self) -> bool {
        matches!(self, Self::Polymorphic { .. })
    }

    /// `true` when this site should not install any further IC entries.
    #[must_use]
    pub(crate) const fn is_megamorphic(&self) -> bool {
        matches!(self, Self::Megamorphic)
    }

    /// Number of installed PIC entries (0 for `Empty` / `Megamorphic`).
    #[must_use]
    pub(crate) fn entry_count(&self) -> usize {
        match self {
            Self::Polymorphic { entries, .. } => entries.len(),
            Self::Empty | Self::Megamorphic => 0,
        }
    }

    /// Borrow the cached PIC entries in install order. Empty slice for
    /// `Empty` / `Megamorphic` sites.
    #[must_use]
    pub(crate) fn entries(&self) -> &[T] {
        match self {
            Self::Polymorphic { entries, .. } => entries.as_slice(),
            Self::Empty | Self::Megamorphic => &[],
        }
    }

    /// Install a new IC entry. No-op when the site is megamorphic.
    /// Appends to the PIC when capacity remains; on overflow the site
    /// transitions to `Megamorphic` instead of evicting.
    pub(crate) fn install(&mut self, ic: T) {
        match self {
            Self::Megamorphic => {}
            Self::Empty => {
                let mut entries = SmallVec::new();
                entries.push(ic);
                *self = Self::Polymorphic { entries, misses: 0 };
            }
            Self::Polymorphic { entries, misses } => {
                if entries.len() < MAX_PIC_ENTRIES {
                    entries.push(ic);
                    *misses = 0;
                } else {
                    *self = Self::Megamorphic;
                }
            }
        }
    }

    /// Permanently bypass this site for the interpreter lifetime.
    pub(crate) fn disable(&mut self) {
        *self = Self::Megamorphic;
    }

    /// Record one guard miss. Returns `true` when this miss promoted
    /// the site to `Megamorphic` (PIC was full and miss budget tipped
    /// over).
    pub(crate) fn record_guard_miss(&mut self) -> bool {
        let Self::Polymorphic { entries, misses } = self else {
            return false;
        };
        *misses = misses.saturating_add(1);
        if *misses < PIC_GUARD_MISS_THRESHOLD || entries.len() < MAX_PIC_ENTRIES {
            return false;
        }
        *self = Self::Megamorphic;
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

    /// Record a miss when the site has no PIC entries yet (Empty).
    /// Megamorphic sites do not contribute further miss counts since
    /// they no longer try the IC fast path.
    pub(crate) fn record_uncached_miss_with_stats(
        &self,
        stats: &mut PropertyIcStats,
        kind: PropertyIcKind,
    ) {
        if !self.is_megamorphic() {
            stats.record_miss(kind);
        }
    }

    /// Append a new entry to the site's PIC and update counters. No-op
    /// when the site is already megamorphic.
    pub(crate) fn install_with_stats(
        &mut self,
        stats: &mut PropertyIcStats,
        kind: PropertyIcKind,
        ic: T,
    ) {
        if self.is_megamorphic() {
            return;
        }
        let became_megamorphic = matches!(self, Self::Polymorphic { entries, .. }
            if entries.len() >= MAX_PIC_ENTRIES);
        self.install(ic);
        if became_megamorphic {
            stats.record_disable(kind);
        } else {
            stats.record_install(kind);
        }
    }

    /// Disable this site and update counters if it was not already megamorphic.
    pub(crate) fn disable_with_stats(&mut self, stats: &mut PropertyIcStats, kind: PropertyIcKind) {
        if !self.is_megamorphic() {
            self.disable();
            stats.record_disable(kind);
        }
    }
}

impl PropertyIcEntry<crate::cache_ir::CacheStub> {
    pub(crate) fn trace_roots(&self, visitor: &mut SlotVisitor<'_>) {
        if let Self::Polymorphic { entries, .. } = self {
            for ic in entries {
                ic.trace_roots(visitor);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{PropertyIcEntry, PropertyIcKind, PropertyIcStats};
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
    fn pic_grows_until_capacity_then_transitions_to_megamorphic() {
        let mut entry = PropertyIcEntry::Empty;
        // Install MAX_PIC_ENTRIES distinct entries without ever
        // missing — the PIC fills but the site stays polymorphic.
        for i in 0..super::MAX_PIC_ENTRIES as u8 {
            entry.install(i);
        }
        assert!(entry.is_polymorphic());
        assert_eq!(entry.entry_count(), super::MAX_PIC_ENTRIES);
        assert_eq!(entry.entries(), &[0_u8, 1, 2, 3]);

        // The PIC is full but only a fresh guard miss budgets toward
        // promotion. The first three misses leave the site
        // polymorphic.
        assert!(!entry.record_guard_miss());
        assert!(!entry.record_guard_miss());
        assert!(!entry.record_guard_miss());

        // Fourth miss tips the budget once the PIC is full → Megamorphic.
        assert!(entry.record_guard_miss());
        assert!(entry.is_megamorphic());
        assert_eq!(entry.entries(), &[] as &[u8]);

        // Megamorphic is sticky: install is a no-op.
        entry.install(99);
        assert!(entry.is_megamorphic());
        assert_eq!(entry.entries(), &[] as &[u8]);
    }

    #[test]
    fn install_into_full_pic_transitions_to_megamorphic() {
        let mut entry = PropertyIcEntry::Empty;
        for i in 0..super::MAX_PIC_ENTRIES as u8 {
            entry.install(i);
        }
        // Direct install past capacity (no preceding miss) still
        // promotes — the PIC simply has no room.
        entry.install(99);
        assert!(entry.is_megamorphic());
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

        // Single PIC entry — three misses in a row don't yet promote
        // because the PIC isn't full.
        entry.record_guard_miss_with_stats(&mut stats, PropertyIcKind::Load);
        entry.record_guard_miss_with_stats(&mut stats, PropertyIcKind::Load);
        entry.record_guard_miss_with_stats(&mut stats, PropertyIcKind::Load);
        assert_eq!(stats.load_misses, 4);
        assert_eq!(stats.load_disables, 0);
        assert!(entry.is_polymorphic());

        // Fill the PIC. Each install bumps `load_installs` but does
        // not affect `load_disables`.
        entry.install_with_stats(&mut stats, PropertyIcKind::Load, 8_u8);
        entry.install_with_stats(&mut stats, PropertyIcKind::Load, 9_u8);
        entry.install_with_stats(&mut stats, PropertyIcKind::Load, 10_u8);
        assert_eq!(stats.load_installs, 4);
        assert_eq!(stats.load_disables, 0);
        assert_eq!(entry.entry_count(), super::MAX_PIC_ENTRIES);

        // Now PIC is full — guard misses accumulate until threshold,
        // then the site flips to Megamorphic.
        for _ in 0..super::PIC_GUARD_MISS_THRESHOLD {
            entry.record_guard_miss_with_stats(&mut stats, PropertyIcKind::Load);
        }
        assert!(entry.is_megamorphic());
        assert_eq!(stats.load_disables, 1);

        // Install past Megamorphic stays a no-op and does not update
        // install / disable counters.
        let installs_before = stats.load_installs;
        let disables_before = stats.load_disables;
        entry.install_with_stats(&mut stats, PropertyIcKind::Load, 11_u8);
        entry.disable_with_stats(&mut stats, PropertyIcKind::Load);
        assert_eq!(stats.load_installs, installs_before);
        assert_eq!(stats.load_disables, disables_before);
    }

    #[test]
    fn direct_prototype_load_ic_rejects_dictionary_compatible_prototype() {
        let mut heap = fresh_heap();
        let proto = object::alloc_object_old_for_fixture(&mut heap).unwrap();
        object::set(proto, &mut heap, "x", Value::boolean(true));
        object::set(proto, &mut heap, "y", Value::null());
        let receiver = object::alloc_object_old_for_fixture(&mut heap).unwrap();
        object::set_prototype(receiver, &mut heap, Some(proto));
        let (ic, value) =
            crate::cache_ir::CacheStub::install_load(receiver, &heap, key("x")).expect("load ic");
        assert_eq!(value, Value::boolean(true));

        assert!(object::delete(proto, &mut heap, "y"));

        assert_eq!(ic.run_load(receiver, &heap, key("x")), None);
    }

    #[test]
    fn direct_prototype_has_ic_rejects_dictionary_compatible_prototype() {
        let mut heap = fresh_heap();
        let proto = object::alloc_object_old_for_fixture(&mut heap).unwrap();
        object::set(proto, &mut heap, "x", Value::boolean(true));
        object::set(proto, &mut heap, "y", Value::null());
        let receiver = object::alloc_object_old_for_fixture(&mut heap).unwrap();
        object::set_prototype(receiver, &mut heap, Some(proto));
        let key_string = JsString::from_str("x", &mut heap).expect("string");
        let ic =
            crate::cache_ir::CacheStub::install_has(receiver, &heap, key_string).expect("has ic");

        assert!(object::delete(proto, &mut heap, "y"));

        assert_eq!(ic.run_has(receiver, &heap, key_string), None);
    }

    #[test]
    fn direct_prototype_store_transition_rejects_dictionary_compatible_prototype() {
        let mut heap = fresh_heap();
        let proto = object::alloc_object_old_for_fixture(&mut heap).unwrap();
        object::set(proto, &mut heap, "x", Value::boolean(true));
        object::set(proto, &mut heap, "y", Value::null());
        let first = object::alloc_object_old_for_fixture(&mut heap).unwrap();
        object::set_prototype(first, &mut heap, Some(proto));
        let transition = object::capture_store_property_transition(
            first,
            &mut heap,
            key("x"),
            &Value::boolean(false),
        )
        .expect("store transition");
        let ic = crate::cache_ir::CacheStub::store_transition(transition);
        let second = object::alloc_object_old_for_fixture(&mut heap).unwrap();
        object::set_prototype(second, &mut heap, Some(proto));

        assert!(object::delete(proto, &mut heap, "y"));

        assert_eq!(ic.run_store(second, &mut heap, key("x"), &Value::null()), None);
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
            PropertyDescriptor::data(Value::boolean(true), false, true, true),
        ));

        assert!(crate::cache_ir::CacheStub::install_store_existing(obj, &heap, key("x")).is_none());
    }
}
