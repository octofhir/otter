//! CodeBlock-owned inline-cache records for named property loads and stores.
//!
//! This module keeps IC state out of the bytecode format. Each executable
//! property instruction owns exactly one cache state in its CodeBlock feedback
//! slot.
//! Each site holds the census-derived polymorphic population plus a
//! megamorphic terminal state for sites whose receiver shape diversity
//! exceeds that population.
//!
//! # Contents
//! - [`PropertyIcEntry`] — per-site cache state and miss policy.
//! - [`PropertyIcStats`] — aggregate IC counters for diagnostics/tests.
//!
//! # Invariants
//! - ICs are performance hints only; every miss falls back to ordinary
//!   ECMAScript property semantics.
//! - Proxies, accessors, symbols, computed keys, dictionary-compatible
//!   objects, and deep prototype hits are not cached.
//! - Cache guards include both shape identity and atom id.
//! - PIC capacity is derived from the checked-in tier census. A new shape that
//!   cannot fit transitions directly to [`PropertyIcEntry::Megamorphic`];
//!   there is no independent guard-miss budget or re-probation state.
//! - Store transition guard semantics live in [`crate::object`]'s
//!   shape-transition layer; this module stores only the frozen IC record.
//!
//! # See also
//! - [`crate::object`]
//! - [`crate::property_dispatch`]

use otter_gc::raw::SlotVisitor;
use smallvec::SmallVec;

use crate::tier_policy::PROFILED_PROPERTY_PIC_CAPACITY;

/// Aggregate inline-cache counters for named property loads and stores.
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

/// Property opcode family for shared IC lifecycle accounting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PropertyIcKind {
    /// `LoadProperty` site.
    Load,
    /// `StoreProperty` site.
    Store,
}

#[cfg(test)]
impl PropertyIcStats {
    /// Record a guarded IC hit.
    pub(crate) fn record_hit(&mut self, kind: PropertyIcKind) {
        match kind {
            PropertyIcKind::Load => self.load_hits += 1,
            PropertyIcKind::Store => self.store_hits += 1,
        }
    }

    /// Record an IC miss or absent active entry.
    fn record_miss(&mut self, kind: PropertyIcKind) {
        match kind {
            PropertyIcKind::Load => self.load_misses += 1,
            PropertyIcKind::Store => self.store_misses += 1,
        }
    }

    /// Record a new monomorphic IC install.
    fn record_install(&mut self, kind: PropertyIcKind) {
        match kind {
            PropertyIcKind::Load => self.load_installs += 1,
            PropertyIcKind::Store => self.store_installs += 1,
        }
    }

    /// Record a site disable.
    fn record_disable(&mut self, kind: PropertyIcKind) {
        match kind {
            PropertyIcKind::Load => self.load_disables += 1,
            PropertyIcKind::Store => self.store_disables += 1,
        }
    }
}

/// Per-site polymorphic inline cache state.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) enum PropertyIcEntry<T> {
    /// Site has not installed any IC yet.
    #[default]
    Empty,
    /// Site holds the profiled guarded program population in install order.
    Polymorphic {
        /// Cached IC records, in install order. Capped at
        /// [`PROFILED_PROPERTY_PIC_CAPACITY`].
        entries: SmallVec<[T; PROFILED_PROPERTY_PIC_CAPACITY]>,
    },
    /// Site saw more shape diversity than the PIC could absorb. This is a
    /// terminal state for the owning CodeBlock.
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
                *self = Self::Polymorphic { entries };
            }
            Self::Polymorphic { entries } => {
                if entries.len() < PROFILED_PROPERTY_PIC_CAPACITY {
                    entries.push(ic);
                } else {
                    *self = Self::Megamorphic;
                }
            }
        }
    }

    /// Permanently bypass this site for the owning CodeBlock lifetime.
    #[cfg(test)]
    pub(crate) fn disable(&mut self) {
        *self = Self::Megamorphic;
    }

    /// Record a guard miss and update opcode-family counters. Shape diversity,
    /// not a separate miss counter, owns the megamorphic transition.
    #[cfg(test)]
    pub(crate) fn record_guard_miss_with_stats(
        &mut self,
        stats: &mut PropertyIcStats,
        kind: PropertyIcKind,
    ) {
        stats.record_miss(kind);
    }

    /// Record a miss when the site has no PIC entries yet. Megamorphic sites
    /// remain terminal and have already stopped participating in miss policy.
    #[cfg(test)]
    pub(crate) fn record_uncached_miss_with_stats(
        &mut self,
        stats: &mut PropertyIcStats,
        kind: PropertyIcKind,
    ) {
        if self.is_megamorphic() {
            return;
        }
        stats.record_miss(kind);
    }

    /// Append a new entry to the site's PIC and update counters. No-op
    /// when the site is already megamorphic.
    #[cfg(test)]
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
            if entries.len() >= PROFILED_PROPERTY_PIC_CAPACITY);
        self.install(ic);
        if became_megamorphic {
            stats.record_disable(kind);
        } else {
            stats.record_install(kind);
        }
    }

    /// Disable this site and update counters if it was not already megamorphic.
    #[cfg(test)]
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
    use crate::{Value, jit::JitCacheIrOp};

    fn fresh_heap() -> otter_gc::GcHeap {
        otter_gc::GcHeap::new().expect("init heap")
    }

    fn key<'a>(name: &'a str) -> AtomizedPropertyKey<'a> {
        AtomizedPropertyKey::new(PropertyAtom::new(AtomId::from_global(7)), name)
    }

    #[test]
    fn pic_grows_until_capacity_then_transitions_to_megamorphic() {
        let mut entry = PropertyIcEntry::Empty;
        // Install the profiled population without exceeding it.
        // missing — the PIC fills but the site stays polymorphic.
        for i in 0..super::PROFILED_PROPERTY_PIC_CAPACITY as u8 {
            entry.install(i);
        }
        assert!(entry.is_polymorphic());
        assert_eq!(entry.entry_count(), super::PROFILED_PROPERTY_PIC_CAPACITY);
        assert_eq!(entry.entries(), &[0_u8, 1, 2, 3]);

        // A fifth distinct shape cannot fit and transitions immediately.
        entry.install(4);
        assert!(entry.is_megamorphic());
        assert_eq!(entry.entries(), &[] as &[u8]);

        // Megamorphic is sticky: install is a no-op.
        entry.install(99);
        assert!(entry.is_megamorphic());
        assert_eq!(entry.entries(), &[] as &[u8]);
        assert!(entry.is_megamorphic(), "megamorphic never re-probates");
    }

    #[test]
    fn install_into_full_pic_transitions_to_megamorphic() {
        let mut entry = PropertyIcEntry::Empty;
        for i in 0..super::PROFILED_PROPERTY_PIC_CAPACITY as u8 {
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
        assert_eq!(entry.entry_count(), super::PROFILED_PROPERTY_PIC_CAPACITY);

        // A distinct program beyond the measured population flips directly.
        entry.install_with_stats(&mut stats, PropertyIcKind::Load, 11_u8);
        assert!(entry.is_megamorphic());
        assert_eq!(stats.load_disables, 1);

        // Install past Megamorphic stays a no-op and does not update
        // install / disable counters.
        let installs_before = stats.load_installs;
        let disables_before = stats.load_disables;
        entry.install_with_stats(&mut stats, PropertyIcKind::Load, 12_u8);
        entry.disable_with_stats(&mut stats, PropertyIcKind::Load);
        assert_eq!(stats.load_installs, installs_before);
        assert_eq!(stats.load_disables, disables_before);
    }

    #[test]
    fn direct_prototype_load_ic_rejects_dictionary_compatible_prototype() {
        let mut heap = fresh_heap();
        let mut proto = object::alloc_object_old_for_fixture(&mut heap).unwrap();
        object::set(&mut proto, &mut heap, "x", Value::boolean(true));
        object::set(&mut proto, &mut heap, "y", Value::null());
        let receiver = object::alloc_object_old_for_fixture(&mut heap).unwrap();
        object::set_prototype(receiver, &mut heap, Some(proto));
        let resolved =
            crate::cache_ir::resolve_atom_data_slot(receiver, &heap, key("x")).expect("load ic");
        let ic = crate::cache_ir::CacheStub::from_resolved_load(
            object::shape_id(receiver, &heap),
            &resolved,
        );
        assert_eq!(resolved.value, Value::boolean(true));

        assert!(object::delete(proto, &mut heap, "y"));

        assert_eq!(ic.run_load(receiver, &heap, key("x")), None);
    }

    #[test]
    fn direct_prototype_store_transition_rejects_dictionary_compatible_prototype() {
        let mut heap = fresh_heap();
        let mut proto = object::alloc_object_old_for_fixture(&mut heap).unwrap();
        object::set(&mut proto, &mut heap, "x", Value::boolean(true));
        object::set(&mut proto, &mut heap, "y", Value::null());
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

        assert_eq!(
            ic.run_store(second, &mut heap, key("x"), &Value::null())
                .expect("store allocation"),
            None
        );
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

    #[test]
    fn cache_ir_snapshot_preserves_own_load_and_store_programs() {
        let mut heap = fresh_heap();
        let mut obj = object::alloc_object_old_for_fixture(&mut heap).unwrap();
        object::set(&mut obj, &mut heap, "x", Value::boolean(true));
        let shape_id = object::shape_id(obj, &heap);
        let shape = 101;
        let resolved = crate::cache_ir::resolve_atom_data_slot(obj, &heap, key("x")).unwrap();

        let load = crate::cache_ir::CacheStub::from_resolved_load(shape_id, &resolved)
            .snapshot_for_jit(|id| (id == shape_id).then_some(shape))
            .expect("complete own-load snapshot");
        assert_eq!(
            load.ops.as_ref(),
            &[
                JitCacheIrOp::GuardShape { object: 0, shape },
                JitCacheIrOp::GuardAtomSlot {
                    object: 0,
                    atom: 7,
                    value_byte: 0,
                    writable: false,
                },
                JitCacheIrOp::LoadField {
                    object: 0,
                    value_byte: 0,
                },
            ]
        );

        let store = crate::cache_ir::CacheStub::install_store_existing(obj, &heap, key("x"))
            .unwrap()
            .snapshot_for_jit(|id| (id == shape_id).then_some(shape))
            .expect("complete own-store snapshot");
        assert_eq!(
            store.ops.as_ref(),
            &[
                JitCacheIrOp::GuardShape { object: 0, shape },
                JitCacheIrOp::GuardAtomSlot {
                    object: 0,
                    atom: 7,
                    value_byte: 0,
                    writable: true,
                },
                JitCacheIrOp::StoreField {
                    object: 0,
                    value_byte: 0,
                },
            ]
        );
    }

    #[test]
    fn cache_ir_snapshot_preserves_prototype_program_order() {
        let mut heap = fresh_heap();
        let mut proto = object::alloc_object_old_for_fixture(&mut heap).unwrap();
        object::set(&mut proto, &mut heap, "x", Value::boolean(true));
        let receiver = object::alloc_object_old_for_fixture(&mut heap).unwrap();
        object::set_prototype(receiver, &mut heap, Some(proto));
        let receiver_id = object::shape_id(receiver, &heap);
        let receiver_shape = 101;
        let holder_id = object::shape_id(proto, &heap);
        let holder_shape = 202;
        let resolved = crate::cache_ir::resolve_atom_data_slot(receiver, &heap, key("x")).unwrap();
        let program = crate::cache_ir::CacheStub::from_resolved_load(receiver_id, &resolved)
            .snapshot_for_jit(|id| match id {
                id if id == receiver_id => Some(receiver_shape),
                id if id == holder_id => Some(holder_shape),
                _ => None,
            })
            .expect("complete prototype-load snapshot");
        assert_eq!(
            program.ops.as_ref(),
            &[
                JitCacheIrOp::GuardShape {
                    object: 0,
                    shape: receiver_shape,
                },
                JitCacheIrOp::LoadPrototype {
                    object: 0,
                    result: 1,
                },
                JitCacheIrOp::GuardShape {
                    object: 1,
                    shape: holder_shape,
                },
                JitCacheIrOp::GuardAtomSlot {
                    object: 1,
                    atom: 7,
                    value_byte: 0,
                    writable: false,
                },
                JitCacheIrOp::LoadField {
                    object: 1,
                    value_byte: 0,
                },
            ]
        );
    }

    #[test]
    fn unresolved_transition_rejects_the_complete_cache_ir_program() {
        let mut heap = fresh_heap();
        let first = object::alloc_object_old_for_fixture(&mut heap).unwrap();
        let transition = object::capture_store_property_transition(
            first,
            &mut heap,
            key("x"),
            &Value::boolean(false),
        )
        .expect("store transition");
        let stub = crate::cache_ir::CacheStub::store_transition(transition);
        assert!(stub.snapshot_for_jit(|_| None).is_none());
    }

    #[test]
    fn shape_transition_snapshot_preserves_every_pre_effect_guard_and_publication() {
        let from = object::ShapeId::from_raw(41);
        let to = object::ShapeId::from_raw(42);
        let transition = object::StorePropertyTransition {
            from_shape_id: from,
            atom_id: AtomId::from_global(7),
            to_shape_id: to,
            to_shape: std::cell::Cell::new(object::ShapeHandle::null()),
            kind: object::StorePropertyTransitionKind::OwnAdd,
            slot: 1,
        };
        let program = crate::cache_ir::CacheStub::store_transition(transition)
            .snapshot_for_jit(|id| match id {
                id if id == from => Some(101),
                id if id == to => Some(202),
                _ => None,
            })
            .expect("complete add-transition snapshot");
        assert_eq!(
            program.ops.as_ref(),
            &[
                JitCacheIrOp::GuardShape {
                    object: 0,
                    shape: 101,
                },
                JitCacheIrOp::GuardPrototypeNull { object: 0 },
                JitCacheIrOp::GuardExtensible {
                    object: 0,
                    value_byte: 8,
                },
                JitCacheIrOp::StoreField {
                    object: 0,
                    value_byte: 8,
                },
                JitCacheIrOp::PublishShape {
                    object: 0,
                    shape: 202,
                    new_len: 2,
                    initialize_inline: false,
                },
            ]
        );
    }
}
