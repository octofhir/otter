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
//! - An attempted site without an attached program is distinct from an
//!   unexecuted site; compiler admission never infers execution from IC size.
//! - Proxies, accessors, symbols, computed keys, dictionary
//!   objects and opaque lookup state are not cached. Ordinary inherited data
//!   uses a chain validity cell.
//! - Cache guards include both shape identity and atom id.
//! - Native snapshots require resident shapes (provisional constructor
//!   lineages included) before selecting immutable inline or suffix fields.
//!   Dictionary-only ids cannot stand in for layouts.
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
    /// Site has never reached property semantic dispatch.
    #[default]
    Empty,
    /// Site reached semantic dispatch but has no representable cache program.
    /// A later cacheable receiver may still attach a program.
    Uncacheable,
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
    /// Record semantic dispatch before receiver classification or user code.
    /// Returns whether this was the site's first attempt.
    pub(crate) fn record_attempt(&mut self) -> bool {
        if matches!(self, Self::Empty) {
            *self = Self::Uncacheable;
            true
        } else {
            false
        }
    }

    /// Whether property semantic dispatch has been attempted at this site.
    #[must_use]
    pub(crate) const fn attempted(&self) -> bool {
        !matches!(self, Self::Empty)
    }

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

    /// Number of installed PIC entries (zero for cold, uncacheable and
    /// megamorphic sites).
    #[must_use]
    pub(crate) fn entry_count(&self) -> usize {
        match self {
            Self::Polymorphic { entries, .. } => entries.len(),
            Self::Empty | Self::Uncacheable | Self::Megamorphic => 0,
        }
    }

    /// Borrow the cached PIC entries in install order. Empty slice for
    /// cold, uncacheable and megamorphic sites.
    #[must_use]
    pub(crate) fn entries(&self) -> &[T] {
        match self {
            Self::Polymorphic { entries, .. } => entries.as_slice(),
            Self::Empty | Self::Uncacheable | Self::Megamorphic => &[],
        }
    }

    /// Install a new IC entry. No-op when the site is megamorphic.
    /// Appends to the PIC when capacity remains; on overflow the site
    /// transitions to `Megamorphic` instead of evicting.
    pub(crate) fn install(&mut self, ic: T) {
        match self {
            Self::Megamorphic => {}
            Self::Empty | Self::Uncacheable => {
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

    /// Actual shaped append, rather than raw construction's dictionary store.
    /// Build the actual immutable append; the dictionary-only test helper
    /// intentionally cannot supply this ordinary IC premise.
    fn shaped_data_fixture(
        object: object::JsObject,
        heap: &mut otter_gc::GcHeap,
        name: &str,
        value: Value,
    ) {
        let atom = match name {
            "own" => 6,
            "x" => 7,
            "y" => 8,
            "z" => 9,
            _ => panic!("fixture atom name"),
        };
        object::append_shaped_data_for_fixture(
            object,
            heap,
            AtomizedPropertyKey::new(PropertyAtom::new(AtomId::from_global(atom)), name),
            value,
        );
    }

    /// Install the real mapped-arguments lookup producer on an existing old
    /// fixture object. Its context is rooted by the production installer while
    /// sidecar/state preparation allocates; no raw header latch is fabricated.
    fn install_mapped_lookup(obj: &mut object::JsObject, heap: &mut otter_gc::GcHeap) {
        // SAFETY: the heap and its handle stack outlive this fixture call;
        // the scoped receiver cannot escape and is read after allocation.
        let scope = unsafe { otter_gc::HandleScope::from_ptr(heap.handle_stack_ptr()) };
        let receiver = scope.local(*obj);
        let context = crate::context::alloc_context_with_roots(
            heap,
            crate::context::ContextShape {
                scope_function_id: 0,
                scope_index: 0,
                slot_count: 1,
                has_extension: false,
            },
            Value::undefined(),
            |_| false,
            &mut |_| {},
        )
        .expect("mapped parameter context");
        assert!(crate::context::write_slot(
            heap,
            context,
            0,
            Value::boolean(true)
        ));
        *obj = receiver.get();
        object::install_mapped_arguments(
            obj,
            heap,
            object::MappedArguments {
                context,
                entries: vec![object::MappedArgumentEntry {
                    key: "virtual".into(),
                    slot: 0,
                }],
            },
        )
        .expect("actual mapped lookup installation");
        assert!(object::state(*obj, heap).is_opaque());
        assert_ne!(
            object::state(*obj, heap).bits() & object::ShapeState::MAPPED_ARGUMENTS_MASK,
            0
        );
    }

    #[test]
    fn attempted_uncacheable_site_is_distinct_from_cold_and_can_later_attach() {
        let mut entry = PropertyIcEntry::Empty;
        assert!(!entry.attempted());
        assert!(entry.record_attempt());
        assert!(matches!(entry, PropertyIcEntry::Uncacheable));
        assert!(entry.attempted());
        assert_eq!(entry.entry_count(), 0);
        assert!(!entry.record_attempt());

        entry.install(7_u8);
        assert_eq!(entry.entries(), &[7]);
        assert!(entry.attempted());
        assert!(!entry.record_attempt());
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
    fn direct_prototype_load_ic_rejects_dictionary_prototype() {
        let mut heap = fresh_heap();
        // SAFETY: this scope belongs to the stationary heap and ends before
        // it; every old fixture cell stays traced through collecting metadata.
        let fixture_roots = unsafe { otter_gc::HandleScope::from_ptr(heap.handle_stack_ptr()) };
        let mut proto = object::alloc_object_old_for_fixture(&mut heap).unwrap();
        let _proto_root = fixture_roots.local(proto);
        shaped_data_fixture(proto, &mut heap, "x", Value::boolean(true));
        shaped_data_fixture(proto, &mut heap, "y", Value::null());
        shaped_data_fixture(proto, &mut heap, "z", Value::null());
        let mut receiver = object::alloc_object_old_for_fixture(&mut heap).unwrap();
        let _receiver_root = fixture_roots.local(receiver);
        assert!(
            object::set_prototype(&mut receiver, &mut heap, Some(proto))
                .expect("fixture prototype transition")
        );
        let resolved =
            crate::cache_ir::resolve_atom_data_slot(receiver, &heap, key("x")).expect("load ic");
        let ic = crate::cache_ir::CacheStub::from_resolved_load(
            object::shape_id(receiver, &heap),
            &resolved,
        );
        assert_eq!(resolved.value, Value::boolean(true));

        assert!(object::delete(&mut proto, &mut heap, "y").expect("non-final property deletion"));
        assert!(
            object::is_dictionary(proto, &heap),
            "actual dictionary holder"
        );
        assert_eq!(
            object::get_own(proto, &heap, "x"),
            Some(Value::boolean(true))
        );

        assert_eq!(ic.run_load(receiver, &heap, key("x")), None);
    }

    #[test]
    fn direct_prototype_store_transition_rejects_dictionary_prototype() {
        let mut heap = fresh_heap();
        // SAFETY: this scope belongs to the stationary heap and ends before
        // it; every old fixture cell stays traced through collecting metadata.
        let fixture_roots = unsafe { otter_gc::HandleScope::from_ptr(heap.handle_stack_ptr()) };
        let mut proto = object::alloc_object_old_for_fixture(&mut heap).unwrap();
        let _proto_root = fixture_roots.local(proto);
        shaped_data_fixture(proto, &mut heap, "x", Value::boolean(true));
        shaped_data_fixture(proto, &mut heap, "y", Value::null());
        shaped_data_fixture(proto, &mut heap, "z", Value::null());
        let mut first = object::alloc_object_old_for_fixture(&mut heap).unwrap();
        let _first_root = fixture_roots.local(first);
        assert!(
            object::set_prototype(&mut first, &mut heap, Some(proto))
                .expect("fixture prototype transition")
        );
        let transition = object::capture_store_property_transition(
            first,
            &mut heap,
            key("x"),
            &Value::boolean(false),
        )
        .expect("store transition");
        let ic = crate::cache_ir::CacheStub::store_transition(transition);
        let mut second = object::alloc_object_old_for_fixture(&mut heap).unwrap();
        let _second_root = fixture_roots.local(second);
        assert!(
            object::set_prototype(&mut second, &mut heap, Some(proto))
                .expect("fixture prototype transition")
        );

        assert!(object::delete(&mut proto, &mut heap, "y").expect("non-final property deletion"));
        assert!(
            object::is_dictionary(proto, &heap),
            "actual dictionary holder"
        );
        assert_eq!(
            object::get_own(proto, &heap, "x"),
            Some(Value::boolean(true))
        );

        assert_eq!(
            ic.run_store(second, &mut heap, key("x"), &Value::null())
                .expect("store allocation"),
            None
        );
        assert_eq!(object::get_own(second, &heap, "x"), None);
    }

    #[test]
    fn deep_prototype_proof_is_shared_and_retires_on_shadowing() {
        let mut heap = fresh_heap();
        // SAFETY: this scope belongs to the stationary heap and ends before
        // it; every old fixture cell stays traced through collecting metadata.
        let fixture_roots = unsafe { otter_gc::HandleScope::from_ptr(heap.handle_stack_ptr()) };
        let holder = object::alloc_object_old_for_fixture(&mut heap).unwrap();
        let _holder_root = fixture_roots.local(holder);
        shaped_data_fixture(holder, &mut heap, "x", Value::boolean(true));
        let mut first = holder;
        let mut middle = holder;
        for depth in 0..12 {
            let mut next = object::alloc_object_old_for_fixture(&mut heap).unwrap();
            let _next_root = fixture_roots.local(next);
            assert!(
                object::set_prototype(&mut next, &mut heap, Some(first))
                    .expect("fixture prototype transition")
            );
            first = next;
            if depth == 5 {
                middle = next;
            }
        }
        let mut receiver = object::alloc_object_old_for_fixture(&mut heap).unwrap();
        let _receiver_root = fixture_roots.local(receiver);
        let mut sibling = object::alloc_object_old_for_fixture(&mut heap).unwrap();
        let _sibling_root = fixture_roots.local(sibling);
        assert!(
            object::set_prototype(&mut receiver, &mut heap, Some(first))
                .expect("fixture prototype transition")
        );
        assert!(
            object::set_prototype(&mut sibling, &mut heap, Some(first))
                .expect("fixture prototype transition")
        );
        let resolved = crate::cache_ir::resolve_atom_data_slot(receiver, &heap, key("x")).unwrap();
        let shared = crate::cache_ir::resolve_atom_data_slot(sibling, &heap, key("x")).unwrap();
        let old = resolved.validity.as_ref().unwrap();
        assert!(std::sync::Arc::ptr_eq(
            old,
            shared.validity.as_ref().unwrap()
        ));
        let ic = crate::cache_ir::CacheStub::from_resolved_load(
            object::shape_id(receiver, &heap),
            &resolved,
        );
        assert_eq!(
            ic.run_load(receiver, &heap, key("x")),
            Some(Value::boolean(true))
        );
        assert!(
            object::define_own_property_in_place(
                &mut middle,
                &mut heap,
                "x",
                crate::object::PropertyDescriptor::data(Value::boolean(false), true, true, true)
            )
            .expect("fixture property allocation")
        );
        assert!(!old.is_valid());
        assert_eq!(ic.run_load(receiver, &heap, key("x")), None);
        // The proof can be rebuilt even when the mutated link has dictionary storage.
        let rebuilt = object::prototype_validity::chain_validity(first, &heap).unwrap();
        assert!(rebuilt.is_valid());
        assert_ne!(old.address(), rebuilt.address());
        assert_eq!(
            object::get(receiver, &heap, "x"),
            Some(Value::boolean(false))
        );
    }

    #[test]
    fn prototype_mutations_retire_dependents_but_preserve_unrelated_chains() {
        let mut heap = fresh_heap();
        // SAFETY: this scope belongs to the stationary heap and ends before
        // it; every old fixture cell stays traced through collecting metadata.
        let fixture_roots = unsafe { otter_gc::HandleScope::from_ptr(heap.handle_stack_ptr()) };
        let mut prototype = object::alloc_object_old_for_fixture(&mut heap).unwrap();
        let _prototype_root = fixture_roots.local(prototype);
        assert!(
            object::ordinary_set_data_property(
                &mut prototype,
                &mut heap,
                "x",
                Value::boolean(true)
            )
            .expect("fixture assignment allocation")
        );
        let mut receiver = object::alloc_object_old_for_fixture(&mut heap).unwrap();
        let _receiver_root = fixture_roots.local(receiver);
        assert!(
            object::set_prototype(&mut receiver, &mut heap, Some(prototype))
                .expect("fixture prototype transition")
        );
        let unrelated = object::alloc_object_old_for_fixture(&mut heap).unwrap();
        let _unrelated_root = fixture_roots.local(unrelated);
        let mut unrelated_receiver = object::alloc_object_old_for_fixture(&mut heap).unwrap();
        let _unrelated_receiver_root = fixture_roots.local(unrelated_receiver);
        assert!(
            object::set_prototype(&mut unrelated_receiver, &mut heap, Some(unrelated))
                .expect("fixture prototype transition")
        );
        let stable = object::prototype_validity::chain_validity(unrelated, &heap).unwrap();
        for mutation in 0..4 {
            let proof = object::prototype_validity::chain_validity(prototype, &heap).unwrap();
            match mutation {
                0 => {
                    assert!(
                        object::ordinary_set_data_property(
                            &mut prototype,
                            &mut heap,
                            "x",
                            Value::boolean(false)
                        )
                        .expect("fixture assignment allocation")
                    );
                }
                1 => {
                    assert!(
                        object::define_own_property(
                            prototype,
                            &mut heap,
                            "x",
                            PropertyDescriptor::data(Value::boolean(false), false, true, true)
                        )
                        .expect("descriptor fixture allocation")
                    );
                }
                2 => {
                    assert!(
                        object::delete(&mut prototype, &mut heap, "x")
                            .expect("fixture property deletion")
                    );
                }
                _ => {
                    assert!(
                        object::set_prototype(&mut prototype, &mut heap, Some(unrelated))
                            .expect("fixture prototype transition")
                    );
                }
            }
            assert!(!proof.is_valid(), "mutation {mutation}");
            assert!(stable.is_valid(), "unrelated chain on mutation {mutation}");
        }
    }

    #[test]
    fn existing_own_store_candidate_rejects_non_writable_data() {
        let mut heap = fresh_heap();
        // SAFETY: this scope belongs to the stationary heap and ends before
        // it; every old fixture cell stays traced through collecting metadata.
        let fixture_roots = unsafe { otter_gc::HandleScope::from_ptr(heap.handle_stack_ptr()) };
        let obj = object::alloc_object_old_for_fixture(&mut heap).unwrap();
        let _obj_root = fixture_roots.local(obj);
        assert!(
            object::define_own_property(
                obj,
                &mut heap,
                "x",
                PropertyDescriptor::data(Value::boolean(true), false, true, true),
            )
            .expect("descriptor fixture allocation")
        );

        assert!(crate::cache_ir::CacheStub::install_store_existing(obj, &heap, key("x")).is_none());
    }

    #[test]
    fn opaque_lookup_state_rejects_ordinary_slot_attachment_and_replay() {
        let mut heap = fresh_heap();
        // SAFETY: this scope belongs to the stationary heap and ends before
        // it; every old fixture cell stays traced through collecting metadata.
        let fixture_roots = unsafe { otter_gc::HandleScope::from_ptr(heap.handle_stack_ptr()) };
        let mut obj = object::alloc_object_old_for_fixture(&mut heap).unwrap();
        let _obj_root = fixture_roots.local(obj);
        shaped_data_fixture(obj, &mut heap, "x", Value::boolean(true));
        assert!(object::supports_fast_property_ic(obj, &heap));
        let resolved = crate::cache_ir::resolve_atom_data_slot(obj, &heap, key("x")).unwrap();
        let load =
            crate::cache_ir::CacheStub::from_resolved_load(object::shape_id(obj, &heap), &resolved);
        let store =
            crate::cache_ir::CacheStub::install_store_existing(obj, &heap, key("x")).unwrap();

        // The actual mapped-arguments producer changes immutable shape state;
        // the retained ordinary cache cannot authorize that new lookup model.
        let before = object::keyed_shape(obj, &heap);
        install_mapped_lookup(&mut obj, &mut heap);
        assert_ne!(object::keyed_shape(obj, &heap), before);

        assert!(!object::supports_fast_property_ic(obj, &heap));
        assert!(crate::cache_ir::resolve_atom_data_slot(obj, &heap, key("x")).is_none());
        assert!(crate::cache_ir::CacheStub::install_store_existing(obj, &heap, key("x")).is_none());
        assert_eq!(load.run_load(obj, &heap, key("x")), None);
        assert_eq!(
            store
                .run_store(obj, &mut heap, key("x"), &Value::boolean(false))
                .expect("store probe"),
            None
        );
        assert_eq!(object::get_own(obj, &heap, "x"), Some(Value::boolean(true)));
    }

    #[test]
    fn ordinary_store_transition_replay_rejects_opaque_receiver() {
        let mut heap = fresh_heap();
        // SAFETY: this scope belongs to the stationary heap and ends before
        // it; every old fixture cell stays traced through collecting metadata.
        let fixture_roots = unsafe { otter_gc::HandleScope::from_ptr(heap.handle_stack_ptr()) };
        let first = object::alloc_object_old_for_fixture(&mut heap).unwrap();
        let _first_root = fixture_roots.local(first);
        let mut second = object::alloc_object_old_for_fixture(&mut heap).unwrap();
        let _second_root = fixture_roots.local(second);
        let transition = object::capture_store_property_transition(
            first,
            &mut heap,
            key("x"),
            &Value::boolean(true),
        )
        .expect("ordinary store transition");
        assert_eq!(object::shape_id(second, &heap), transition.from_shape_id);
        let before = object::keyed_shape(second, &heap);
        install_mapped_lookup(&mut second, &mut heap);
        assert_ne!(object::keyed_shape(second, &heap), before);

        assert_eq!(
            object::replay_store_property_transition(
                second,
                &mut heap,
                key("x"),
                transition.from_shape_id,
                transition.atom_id,
                transition.to_shape_id,
                || transition.to_shape.get(),
                &transition.kind,
                transition.slot,
                &Value::boolean(false),
            )
            .expect("store transition probe"),
            None
        );
        assert_eq!(object::get_own(second, &heap, "x"), None);
    }

    fn proof_for_test(
        cell: &std::sync::Arc<object::prototype_validity::PrototypeValidity>,
    ) -> Option<crate::jit::JitPrototypeValidity> {
        cell.is_valid().then_some(crate::jit::JitPrototypeValidity {
            address: cell.address(),
            identity: cell.identity(),
        })
    }

    #[test]
    fn cache_ir_snapshot_preserves_own_load_and_store_programs() {
        let mut interpreter = crate::Interpreter::new().expect("fixture interpreter bootstrap");
        let mut obj = object::alloc_object_old_for_fixture(interpreter.gc_heap_mut()).unwrap();
        interpreter
            .create_data_property(&mut obj, "x", Value::boolean(true))
            .unwrap();
        let heap = interpreter.gc_heap();
        let handle = object::keyed_shape(obj, heap);
        assert!(!handle.is_null());
        let shape_id = heap.read_payload(handle, object::ShapeBody::id);
        let atom = heap.read_payload(handle, object::ShapeBody::transition_atom);
        let property = AtomizedPropertyKey::new(PropertyAtom::new(atom), "x");
        let shape = handle.offset();
        let resolved = crate::cache_ir::resolve_atom_data_slot(obj, heap, property).unwrap();

        let load = crate::cache_ir::CacheStub::from_resolved_load(shape_id, &resolved)
            .snapshot_for_jit(|id| (id == shape_id).then_some(shape), proof_for_test)
            .expect("complete own-load snapshot");
        assert_eq!(
            load.ops.as_ref(),
            &[
                JitCacheIrOp::GuardShape { object: 0, shape },
                JitCacheIrOp::GuardAtomSlot {
                    object: 0,
                    atom: atom.raw(),
                    field: crate::object::FieldLocation::inline(0),
                    writable: false,
                },
                JitCacheIrOp::LoadField {
                    object: 0,
                    field: crate::object::FieldLocation::inline(0),
                },
            ]
        );

        let store = crate::cache_ir::CacheStub::install_store_existing(obj, heap, property)
            .unwrap()
            .snapshot_for_jit(|id| (id == shape_id).then_some(shape), proof_for_test)
            .expect("complete own-store snapshot");
        assert_eq!(
            store.ops.as_ref(),
            &[
                JitCacheIrOp::GuardShape { object: 0, shape },
                JitCacheIrOp::GuardAtomSlot {
                    object: 0,
                    atom: atom.raw(),
                    field: crate::object::FieldLocation::inline(0),
                    writable: true,
                },
                JitCacheIrOp::StoreField {
                    object: 0,
                    field: crate::object::FieldLocation::inline(0),
                },
            ]
        );
    }

    #[test]
    fn cache_ir_snapshot_preserves_prototype_program_order() {
        let mut interpreter = crate::Interpreter::new().expect("fixture interpreter bootstrap");
        let mut proto = object::alloc_object_old_for_fixture(interpreter.gc_heap_mut()).unwrap();
        interpreter
            .create_data_property(&mut proto, "x", Value::boolean(true))
            .unwrap();
        let mut receiver = object::alloc_object_old_for_fixture(interpreter.gc_heap_mut()).unwrap();
        assert!(
            object::set_prototype(&mut receiver, interpreter.gc_heap_mut(), Some(proto))
                .expect("fixture prototype transition")
        );
        let heap = interpreter.gc_heap();
        let receiver_handle = object::keyed_shape(receiver, heap);
        let holder_root = object::cached_instance_root(proto, heap).unwrap();
        assert!(!receiver_handle.is_null());
        assert_eq!(holder_root, receiver_handle);
        let receiver_id = heap.read_payload(receiver_handle, object::ShapeBody::id);
        let receiver_shape = receiver_handle.offset();
        let holder_id = heap.read_payload(holder_root, object::ShapeBody::id);
        let holder_shape = holder_root.offset();
        assert_eq!(holder_id, receiver_id);
        let atom = heap.read_payload(
            object::keyed_shape(proto, heap),
            object::ShapeBody::transition_atom,
        );
        let property = AtomizedPropertyKey::new(PropertyAtom::new(atom), "x");
        let resolved = crate::cache_ir::resolve_atom_data_slot(receiver, heap, property).unwrap();
        let hit_shape = resolved.hit.shape.offset();
        let hit_id = resolved.hit.shape_id;
        assert!(!resolved.hit.shape.is_null());
        assert_ne!(
            hit_id, receiver_id,
            "the holder data layout is a distinct real child shape"
        );
        assert!(
            crate::cache_ir::CacheStub::from_resolved_load(receiver_id, &resolved)
                .snapshot_for_jit(
                    |id| match id {
                        id if id == receiver_id => Some(receiver_shape),
                        id if id == holder_id => Some(holder_shape),
                        _ => None,
                    },
                    proof_for_test,
                )
                .is_none(),
            "a valid chain does not authorize an unresolved native holder layout"
        );
        let program = crate::cache_ir::CacheStub::from_resolved_load(receiver_id, &resolved)
            .snapshot_for_jit(
                |id| match id {
                    id if id == receiver_id => Some(receiver_shape),
                    id if id == holder_id => Some(holder_shape),
                    id if id == hit_id => Some(hit_shape),
                    _ => None,
                },
                proof_for_test,
            )
            .expect("complete prototype-load snapshot");
        assert_eq!(
            program.ops.as_ref(),
            &[
                JitCacheIrOp::GuardShape {
                    object: 0,
                    shape: receiver_shape,
                },
                JitCacheIrOp::GuardPrototypeValidity {
                    validity: proof_for_test(resolved.validity.as_ref().unwrap()).unwrap()
                },
                JitCacheIrOp::LoadPrototypeHolder {
                    root: holder_shape,
                    result: 1
                },
                JitCacheIrOp::LoadField {
                    object: 1,
                    field: crate::object::FieldLocation::inline(0),
                },
            ]
        );
    }

    #[test]
    fn unresolved_transition_rejects_the_complete_cache_ir_program() {
        let mut heap = fresh_heap();
        // SAFETY: this scope belongs to the stationary heap and ends before
        // it; every old fixture cell stays traced through collecting metadata.
        let fixture_roots = unsafe { otter_gc::HandleScope::from_ptr(heap.handle_stack_ptr()) };
        let first = object::alloc_object_old_for_fixture(&mut heap).unwrap();
        let _first_root = fixture_roots.local(first);
        let transition = object::capture_store_property_transition(
            first,
            &mut heap,
            key("x"),
            &Value::boolean(false),
        )
        .expect("store transition");
        let stub = crate::cache_ir::CacheStub::store_transition(transition);
        assert!(stub.snapshot_for_jit(|_| None, proof_for_test).is_none());
    }

    #[test]
    fn shape_transition_snapshot_preserves_every_pre_effect_guard_and_publication() {
        let mut interpreter = crate::Interpreter::new().expect("fixture interpreter bootstrap");
        let mut obj = object::alloc_object_old_for_fixture(interpreter.gc_heap_mut()).unwrap();
        interpreter
            .create_data_property(&mut obj, "prefix", Value::boolean(false))
            .unwrap();
        let from_handle = object::keyed_shape(obj, interpreter.gc_heap());
        let mut value = Value::boolean(true);
        let to_handle = interpreter
            .shape_child_rooting_object_value(from_handle, "x", &mut obj, &mut value)
            .unwrap();
        let atom = interpreter
            .gc_heap()
            .read_payload(to_handle, object::ShapeBody::transition_atom);
        let property = AtomizedPropertyKey::new(PropertyAtom::new(atom), "x");
        let transition = object::capture_store_property_transition_with_shape(
            obj,
            interpreter.gc_heap_mut(),
            property,
            &value,
            to_handle,
        )
        .expect("resident own-add transition");
        assert_eq!(transition.slot, 1);
        assert!(matches!(
            transition.kind,
            object::StorePropertyTransitionKind::OwnAdd
        ));
        assert_eq!(transition.to_shape.get(), to_handle);
        let from = transition.from_shape_id;
        let to = transition.to_shape_id;
        let from_shape = from_handle.offset();
        let to_shape = to_handle.offset();
        let program = crate::cache_ir::CacheStub::store_transition(transition)
            .snapshot_for_jit(
                |id| match id {
                    id if id == from => Some(from_shape),
                    id if id == to => Some(to_shape),
                    _ => None,
                },
                proof_for_test,
            )
            .expect("complete add-transition snapshot");
        assert_eq!(
            program.ops.as_ref(),
            &[
                JitCacheIrOp::GuardShape {
                    object: 0,
                    shape: from_shape,
                },
                JitCacheIrOp::GuardPrototypeNull { object: 0 },
                JitCacheIrOp::GuardExtensible {
                    object: 0,
                    field: crate::object::FieldLocation::inline(1),
                },
                JitCacheIrOp::StoreField {
                    object: 0,
                    field: crate::object::FieldLocation::inline(1),
                },
                JitCacheIrOp::PublishShape {
                    object: 0,
                    shape: to_shape,
                },
            ]
        );
    }
}
