//! Isolate feedback ownership and high-level inline-cache operations.
//!
//! # Contents
//! - Dense global property-site directory installation.
//! - Executable property and builtin-method IC banks.
//! - Intent-level IC accounting, installation, snapshots, and tracing views.
//! - Narrow lookup helpers for lock-free CodeBlock feedback slots.
//! - Single-writer bounded method-target distributions.
//! - Range purge for tombstoned code chunks.
//!
//! # Invariants
//! - All mutable executable IC state is owned by the isolate and reached
//!   through this directory; the interpreter exposes no parallel IC vectors.
//! - CodeBlock property/call summaries contain atomics and stable numeric ids
//!   only. GC-bearing executable recipes never cross that boundary.
//! - A site id maps to exactly one canonical instruction for the lifetime of
//!   a live chunk. Tombstoned ranges remain reserved and hold no slot address
//!   or executable IC state.
//! - Generated-call plans remain separate from observational feedback.
//! - Store banks stop on allocation failure; only allocation-free guard misses
//!   may advance to another recipe with the same receiver handle.
//!
//! # See also
//! - [`crate::feedback::FeedbackVector`]
//! - [`crate::executable::FeedbackSlotAddress`]

use crate::executable::FeedbackSlotAddress;
use crate::feedback::PropertyFeedbackState;
use crate::method_ops::MethodCallIc;
use crate::property_ic::{PropertyIcEntry, PropertyIcKind, PropertyIcStats};
use crate::{
    ExecutionContext, Interpreter, JitCollectionMethodIcStats, MAX_POLY_METHOD_TARGETS,
    MethodCallFeedback, MethodSite, PolyMethodTarget,
};
use smallvec::SmallVec;

type ExecutablePropertyIc = PropertyIcEntry<crate::cache_ir::CacheStub>;

/// Isolate-local facade mapping global executable site ids to canonical typed
/// feedback slots and executable IC state. Atomic publication, GC-bearing
/// recipes, accounting, and opcode-selected storage do not escape this
/// boundary; every mutation is performed by the isolate VM thread.
#[derive(Default)]
pub(crate) struct FeedbackDirectory {
    slots: Vec<Option<FeedbackSlotAddress>>,
    method_targets: Vec<Option<MethodCallFeedback>>,
    load_ics: Vec<ExecutablePropertyIc>,
    store_ics: Vec<ExecutablePropertyIc>,
    method_ics: Vec<Option<MethodCallIc>>,
    property_stats: PropertyIcStats,
    /// Chunks whose slot addresses are already installed, keyed by executable
    /// address. A chunk's sites are immutable once linked, so installation
    /// happens once per chunk, not once per dispatch entry, and the membership
    /// probe stays O(1) however many chunks an isolate has linked. Held
    /// weakly: the allocation behind a `Weak` is never reused while the `Weak`
    /// lives, so an address cannot alias a later chunk until its entry is
    /// purged with the tombstoned range.
    installed_chunks:
        rustc_hash::FxHashMap<usize, std::sync::Weak<crate::executable::ExecutableModule>>,
}

impl FeedbackDirectory {
    pub(crate) fn evict_site_range(&mut self, start: u32, end: u32) {
        let start = start as usize;
        let end = (end as usize).min(self.slots.len());
        for site in start..end {
            self.slots[site] = None;
            self.method_targets[site] = None;
            self.load_ics[site] = PropertyIcEntry::Empty;
            self.store_ics[site] = PropertyIcEntry::Empty;
            self.method_ics[site] = None;
        }
        self.installed_chunks
            .retain(|_, chunk| chunk.strong_count() != 0);
    }

    fn install_context(&mut self, context: &ExecutionContext) {
        let executable = context.executable_module();
        let identity = std::sync::Arc::as_ptr(executable) as usize;
        match self.installed_chunks.entry(identity) {
            std::collections::hash_map::Entry::Occupied(installed)
                if installed.get().strong_count() != 0 =>
            {
                return;
            }
            std::collections::hash_map::Entry::Occupied(mut stale) => {
                stale.insert(std::sync::Arc::downgrade(executable));
            }
            std::collections::hash_map::Entry::Vacant(vacant) => {
                vacant.insert(std::sync::Arc::downgrade(executable));
            }
        }
        let site_count = context.property_ic_site_end();
        if self.slots.len() < site_count {
            self.slots.resize_with(site_count, || None);
        }
        if self.method_targets.len() < site_count {
            self.method_targets.resize_with(site_count, || None);
        }
        if self.load_ics.len() < site_count {
            self.load_ics.resize(site_count, PropertyIcEntry::Empty);
        }
        if self.store_ics.len() < site_count {
            self.store_ics.resize(site_count, PropertyIcEntry::Empty);
        }
        if self.method_ics.len() < site_count {
            self.method_ics.resize(site_count, None);
        }
        for (site, address) in context.feedback_slot_addresses() {
            if let Some(slot) = self.slots.get_mut(site) {
                slot.get_or_insert(address);
            }
        }
    }

    fn address(&self, site: usize) -> Option<&FeedbackSlotAddress> {
        self.slots.get(site)?.as_ref()
    }

    fn property_bank(&self, kind: PropertyIcKind) -> &[ExecutablePropertyIc] {
        match kind {
            PropertyIcKind::Load => &self.load_ics,
            PropertyIcKind::Store => &self.store_ics,
        }
    }

    fn property_bank_with_stats_mut(
        &mut self,
        kind: PropertyIcKind,
    ) -> (&mut [ExecutablePropertyIc], &mut PropertyIcStats) {
        match kind {
            PropertyIcKind::Load => (&mut self.load_ics, &mut self.property_stats),
            PropertyIcKind::Store => (&mut self.store_ics, &mut self.property_stats),
        }
    }

    fn property_stubs(
        &self,
        site: usize,
        kind: PropertyIcKind,
    ) -> Option<&[crate::cache_ir::CacheStub]> {
        self.property_bank(kind)
            .get(site)
            .map(PropertyIcEntry::entries)
    }

    /// Probe a named load site. Miss accounting and installation remain
    /// explicit operations so semantic slow paths can decide when a failed
    /// probe is cache-representable.
    pub(crate) fn probe_load(
        &self,
        site: usize,
        obj: crate::object::JsObject,
        heap: &otter_gc::GcHeap,
        key: crate::property_atom::AtomizedPropertyKey<'_>,
    ) -> Option<crate::Value> {
        self.property_stubs(site, PropertyIcKind::Load)?
            .iter()
            .find_map(|stub| stub.run_load(obj, heap, key))
    }

    /// Probe a named store site and execute the first matching recipe.
    pub(crate) fn probe_store(
        &self,
        site: usize,
        obj: crate::object::JsObject,
        heap: &mut otter_gc::GcHeap,
        key: crate::property_atom::AtomizedPropertyKey<'_>,
        value: &crate::Value,
    ) -> Result<bool, otter_gc::OutOfMemory> {
        let Some(stubs) = self.property_stubs(site, PropertyIcKind::Store) else {
            return Ok(false);
        };
        for stub in stubs {
            if stub.run_store(obj, heap, key, value)?.is_some() {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// Every settled own-slot program installed at a site, in install order.
    ///
    /// This is the site's cache as a compile-time declaration: one shape and
    /// one slot per installed program. Generated code turns it into a guard
    /// chain and never loads the runtime cell.
    pub(crate) fn settled_property_slots(
        &self,
        site: usize,
        kind: PropertyIcKind,
    ) -> Option<Vec<(crate::object::ShapeId, u32, u16)>> {
        let stubs = self.property_stubs(site, kind)?;
        let settled: Vec<_> = stubs
            .iter()
            .filter_map(crate::cache_ir::CacheStub::settled_own_slot)
            .collect();
        (!settled.is_empty() && settled.len() == stubs.len()).then_some(settled)
    }

    /// Receiver shape, holder shape and slot for a load site every one of whose
    /// programs reaches its slot through the receiver's prototype.
    pub(crate) fn settled_prototype_slots(
        &self,
        site: usize,
    ) -> Option<Vec<(crate::object::ShapeId, crate::object::ShapeId, u32, u16)>> {
        let stubs = self.property_stubs(site, PropertyIcKind::Load)?;
        let settled: Vec<_> = stubs
            .iter()
            .filter_map(crate::cache_ir::CacheStub::settled_prototype_slot)
            .collect();
        (!settled.is_empty() && settled.len() == stubs.len()).then_some(settled)
    }

    /// Lower this load site's cache program to the way generated code runs.
    ///
    /// Whatever the stub's op sequence is — own data, or a guarded hop to the
    /// receiver's prototype — the lowering walks it. A shape the site has never
    /// seen, or a program with an op that has no inline form yet, yields `None`
    /// and leaves the access on the stub.
    pub(crate) fn whisker_load_cell_fill(
        &self,
        site: usize,
        obj: crate::object::JsObject,
        heap: &otter_gc::GcHeap,
        key: crate::property_atom::AtomizedPropertyKey<'_>,
    ) -> Option<crate::jit::JitPropertyIcWay> {
        self.property_stubs(site, PropertyIcKind::Load)?
            .iter()
            .find_map(|stub| stub.lower_jit_way(obj, heap, key))
    }

    /// Lower this store site's cache program the same way.
    pub(crate) fn whisker_store_cell_fill(
        &self,
        site: usize,
        obj: crate::object::JsObject,
        heap: &otter_gc::GcHeap,
        key: crate::property_atom::AtomizedPropertyKey<'_>,
    ) -> Option<crate::jit::JitPropertyIcWay> {
        self.property_stubs(site, PropertyIcKind::Store)?
            .iter()
            .find_map(|stub| stub.lower_jit_way(obj, heap, key))
    }

    #[must_use]
    pub(crate) fn property_entry_count(&self, site: usize, kind: PropertyIcKind) -> Option<usize> {
        self.property_bank(kind)
            .get(site)
            .map(PropertyIcEntry::entry_count)
    }

    #[must_use]
    pub(crate) fn property_is_megamorphic(
        &self,
        site: usize,
        kind: PropertyIcKind,
    ) -> Option<bool> {
        self.property_bank(kind)
            .get(site)
            .map(PropertyIcEntry::is_megamorphic)
    }

    pub(crate) fn record_property_hit(&mut self, kind: PropertyIcKind) {
        self.property_stats.record_hit(kind);
    }

    /// Record a failed guarded probe and return the site's resulting terminal
    /// state. `None` means the site was not installed in this isolate.
    pub(crate) fn record_property_guard_miss(
        &mut self,
        site: usize,
        kind: PropertyIcKind,
    ) -> Option<bool> {
        let (bank, stats) = self.property_bank_with_stats_mut(kind);
        let entry = bank.get_mut(site)?;
        entry.record_guard_miss_with_stats(stats, kind);
        Some(entry.is_megamorphic())
    }

    pub(crate) fn record_property_uncached_miss(&mut self, site: usize, kind: PropertyIcKind) {
        let (bank, stats) = self.property_bank_with_stats_mut(kind);
        if let Some(entry) = bank.get_mut(site) {
            entry.record_uncached_miss_with_stats(stats, kind);
        }
    }

    pub(crate) fn install_property_stub(
        &mut self,
        site: usize,
        kind: PropertyIcKind,
        stub: crate::cache_ir::CacheStub,
    ) {
        let (bank, stats) = self.property_bank_with_stats_mut(kind);
        if let Some(entry) = bank.get_mut(site) {
            entry.install_with_stats(stats, kind, stub);
        }
    }

    #[must_use]
    pub(crate) const fn property_stats(&self) -> PropertyIcStats {
        self.property_stats
    }

    #[cfg(test)]
    pub(crate) fn polymorphic_property_count(&self, kind: PropertyIcKind) -> usize {
        self.property_bank(kind)
            .iter()
            .filter(|entry| entry.is_polymorphic())
            .count()
    }

    /// GC root view for store transition stubs. This is deliberately the only
    /// raw-bank view: the collector needs to rewrite cached shape handles.
    pub(crate) fn store_ics_for_trace(&self) -> &[ExecutablePropertyIc] {
        &self.store_ics
    }

    #[must_use]
    pub(crate) fn method_ic(&self, site: usize) -> Option<MethodCallIc> {
        self.method_ics.get(site).copied().flatten()
    }

    /// The monomorphic own-data hit recorded by the property-load IC at `site`,
    /// if the site holds exactly one stub resolving to an own data property.
    /// Lets a load or method call take a shape-guarded direct slab read instead
    /// of walking the stub list and re-comparing the atom. Returns `None` when
    /// the site is polymorphic or the property lives on the prototype.
    #[must_use]
    pub(crate) fn mono_load_own_data_hit(
        &self,
        site: usize,
    ) -> Option<crate::object::AtomOwnPropertyHit> {
        let stubs = self.property_stubs(site, PropertyIcKind::Load)?;
        if stubs.len() != 1 {
            return None;
        }
        stubs.iter().find_map(|stub| stub.own_data_hit())
    }

    pub(crate) fn install_method_ic(&mut self, site: usize, ic: MethodCallIc) -> bool {
        let Some(slot) = self.method_ics.get_mut(site) else {
            return false;
        };
        *slot = Some(ic);
        true
    }

    pub(crate) fn clear_method_ic(&mut self, site: usize) {
        if let Some(slot) = self.method_ics.get_mut(site) {
            *slot = None;
        }
    }

    #[must_use]
    pub(crate) fn collection_method_stats(&self) -> JitCollectionMethodIcStats {
        let mut stats = JitCollectionMethodIcStats {
            slots: self.method_ics.len() as u64,
            ..JitCollectionMethodIcStats::default()
        };
        for slot in &self.method_ics {
            if let Some(MethodCallIc::Collection(ic)) = slot {
                stats.collection_slots = stats.collection_slots.saturating_add(1);
                if ic.leaf_stub_id.is_some() {
                    stats.leaf_stub_slots = stats.leaf_stub_slots.saturating_add(1);
                }
                if ic.alloc_stub_id.is_some() {
                    stats.alloc_stub_slots = stats.alloc_stub_slots.saturating_add(1);
                }
            } else {
                stats.empty_slots = stats.empty_slots.saturating_add(1);
            }
        }
        stats
    }

    #[must_use]
    pub(crate) fn ic_snapshot(&self) -> Vec<crate::inspect::IcSiteSnapshot> {
        let mut out = Vec::with_capacity(self.load_ics.len() + self.store_ics.len());
        for (index, entry) in self.load_ics.iter().enumerate() {
            out.push(crate::inspect::IcSiteSnapshot {
                site_index: index as u32,
                kind: crate::inspect::IcSiteKind::Load,
                state: crate::inspect::snapshot_load_state(entry),
            });
        }
        for (index, entry) in self.store_ics.iter().enumerate() {
            out.push(crate::inspect::IcSiteSnapshot {
                site_index: index as u32,
                kind: crate::inspect::IcSiteKind::Store,
                state: crate::inspect::snapshot_store_state(entry),
            });
        }
        out
    }

    fn publish_property(&self, site: usize, kind: PropertyIcKind) {
        let Some(entry) = self.property_bank(kind).get(site) else {
            return;
        };
        if let Some(slot) = self
            .address(site)
            .and_then(|address| address.property(kind))
        {
            slot.publish(entry);
        }
    }

    fn property_state(&self, site: usize, kind: PropertyIcKind) -> Option<PropertyFeedbackState> {
        self.address(site)?.property(kind).map(|slot| slot.state())
    }

    fn method_targets(&self, site: usize) -> Option<MethodCallFeedback> {
        self.address(site)?.is_method().then_some(())?;
        self.method_targets.get(site)?.clone()
    }

    fn method_targets_saturated(&self, site: usize) -> bool {
        self.address(site)
            .is_some_and(FeedbackSlotAddress::is_method)
            && matches!(
                self.method_targets.get(site),
                Some(Some(MethodCallFeedback::Megamorphic))
            )
    }

    fn record_method_native_leaf(
        &mut self,
        site: usize,
        stub_id: crate::native_abi::RuntimeStubId,
        method_site: MethodSite,
    ) -> bool {
        if !self
            .address(site)
            .is_some_and(FeedbackSlotAddress::is_method)
        {
            return false;
        }
        let Some(feedback) = self.method_targets.get_mut(site) else {
            return false;
        };
        record_method_native_leaf_distribution(feedback, stub_id, method_site)
    }

    fn record_method_target(
        &mut self,
        site: usize,
        method_fid: u32,
        method_site: MethodSite,
    ) -> bool {
        if !self
            .address(site)
            .is_some_and(FeedbackSlotAddress::is_method)
        {
            return false;
        }
        if let Some(targets) = self.method_targets.get_mut(site) {
            return record_method_distribution(targets, method_fid, method_site);
        }
        false
    }
}

/// Apply declared-native method transitions to isolate-owned method feedback.
/// A bytecode/native mixture cannot share one direct guard chain and therefore
/// saturates the site instead of retaining an incomplete immutable chain.
fn record_method_native_leaf_distribution(
    feedback: &mut Option<MethodCallFeedback>,
    stub_id: crate::native_abi::RuntimeStubId,
    method_site: MethodSite,
) -> bool {
    match feedback {
        None => {
            *feedback = Some(MethodCallFeedback::MonoNativeLeaf {
                stub_id,
                recv_shape: method_site.recv_shape,
                proto_chain: method_site.proto_chain,
                method_value_byte: method_site.method_value_byte,
                recv_shape_offset: method_site.recv_shape_offset,
                holder_shape_offset: method_site.holder_shape_offset,
            });
            true
        }
        Some(MethodCallFeedback::MonoNativeLeaf {
            stub_id: seen_stub,
            recv_shape,
            proto_chain,
            method_value_byte,
            ..
        }) => {
            if *seen_stub != stub_id
                || *recv_shape != method_site.recv_shape
                || !proto_chain.same(&method_site.proto_chain)
                || *method_value_byte != method_site.method_value_byte
            {
                *feedback = Some(MethodCallFeedback::Megamorphic);
                true
            } else {
                false
            }
        }
        Some(MethodCallFeedback::Megamorphic) => false,
        Some(_) => {
            *feedback = Some(MethodCallFeedback::Megamorphic);
            true
        }
    }
}

/// Apply mono -> bounded-poly -> megamorphic transitions to isolate-owned
/// method feedback. This state never crosses the Send/Sync CodeBlock boundary.
fn record_method_distribution(
    feedback: &mut Option<MethodCallFeedback>,
    method_fid: u32,
    site: MethodSite,
) -> bool {
    let new_target = PolyMethodTarget {
        method_fid,
        recv_shape: site.recv_shape,
        proto_chain: site.proto_chain,
        method_value_byte: site.method_value_byte,
        hits: 1,
    };
    match feedback {
        None => {
            *feedback = Some(MethodCallFeedback::Mono {
                method_fid,
                recv_shape: site.recv_shape,
                proto_chain: site.proto_chain,
                method_value_byte: site.method_value_byte,
            });
            true
        }
        Some(MethodCallFeedback::Mono {
            method_fid: seen_fid,
            recv_shape: seen_shape,
            proto_chain: seen_proto_chain,
            method_value_byte: seen_value_byte,
        }) => {
            let same = *seen_fid == method_fid
                && *seen_shape == site.recv_shape
                && seen_proto_chain.same(&site.proto_chain)
                && *seen_value_byte == site.method_value_byte;
            if !same {
                let prior = PolyMethodTarget {
                    method_fid: *seen_fid,
                    recv_shape: *seen_shape,
                    proto_chain: *seen_proto_chain,
                    method_value_byte: *seen_value_byte,
                    hits: 1,
                };
                let mut targets: SmallVec<[PolyMethodTarget; MAX_POLY_METHOD_TARGETS]> =
                    SmallVec::new();
                targets.push(prior);
                targets.push(new_target);
                *feedback = Some(MethodCallFeedback::Poly(Box::new(targets)));
                true
            } else {
                false
            }
        }
        Some(MethodCallFeedback::Poly(targets)) => {
            if let Some(existing) = targets
                .iter_mut()
                .find(|target| target.matches(method_fid, &site))
            {
                existing.hits = existing.hits.saturating_add(1);
                false
            } else if targets.len() < MAX_POLY_METHOD_TARGETS {
                targets.push(new_target);
                true
            } else {
                *feedback = Some(MethodCallFeedback::Megamorphic);
                true
            }
        }
        Some(MethodCallFeedback::Megamorphic) => false,
        // A site that already resolved to a declared native leaf cannot also
        // carry a bytecode inline chain: the two need different guard
        // lowerings, so the second shape gives up rather than mixing them.
        Some(MethodCallFeedback::MonoNativeLeaf { .. }) => {
            *feedback = Some(MethodCallFeedback::Megamorphic);
            true
        }
    }
}

impl Interpreter {
    pub(crate) fn ensure_property_ic_capacity(&mut self, context: &ExecutionContext) {
        self.feedback_directory.install_context(context);
    }

    /// Publish a stable compile/profile summary after mutating a runtime IC.
    pub(crate) fn publish_property_feedback(
        &self,
        site: usize,
        kind: crate::property_ic::PropertyIcKind,
    ) {
        self.feedback_directory.publish_property(site, kind);
    }

    #[must_use]
    pub(crate) fn property_feedback_state(
        &self,
        site: usize,
        kind: PropertyIcKind,
    ) -> Option<PropertyFeedbackState> {
        self.feedback_directory.property_state(site, kind)
    }

    /// Refresh stable property summaries at the compile boundary. Runtime IC
    /// probes stay lock-free; the CodeBlock atomic slot receives a stable
    /// numeric snapshot only when a tier is about to consume it.
    pub(crate) fn publish_property_feedback_for_view(&self, view: &crate::jit::JitCompileSnapshot) {
        for instruction in &view.instructions {
            let kind = match instruction.op(&view.code_block) {
                otter_bytecode::Op::LoadProperty => crate::property_ic::PropertyIcKind::Load,
                otter_bytecode::Op::StoreProperty => crate::property_ic::PropertyIcKind::Store,
                _ => continue,
            };
            if let Some(site) = instruction.property_ic_site(&view.code_block) {
                self.publish_property_feedback(site, kind);
            }
        }
    }

    #[must_use]
    pub(crate) fn method_target_feedback(&self, site: usize) -> Option<MethodCallFeedback> {
        self.feedback_directory.method_targets(site)
    }

    #[must_use]
    pub(crate) fn method_target_feedback_saturated(&self, site: usize) -> bool {
        self.feedback_directory.method_targets_saturated(site)
    }

    /// Record that one `Op::CallMethodValue` site resolved to a declared native
    /// leaf entry, with the receiver layout captured before the call.
    ///
    /// A site that has already observed a bytecode target becomes megamorphic:
    /// mixing both kinds would need a guard chain no consumer builds, and the
    /// transition must retire any previously baked bytecode-only chain.
    pub(crate) fn record_method_native_leaf_feedback(
        &mut self,
        site: usize,
        stub_id: crate::native_abi::RuntimeStubId,
        method_site: MethodSite,
    ) -> bool {
        self.feedback_directory
            .record_method_native_leaf(site, stub_id, method_site)
    }

    pub(crate) fn record_method_target_feedback(
        &mut self,
        site: usize,
        method_fid: u32,
        method_site: MethodSite,
    ) -> bool {
        self.feedback_directory
            .record_method_target(site, method_fid, method_site)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{MethodProtoChain, object::ShapeId};

    fn method_site(raw: u64) -> MethodSite {
        MethodSite {
            recv_shape: ShapeId::for_test(raw),
            proto_chain: MethodProtoChain::own(),
            method_value_byte: raw as u32 * 8,
            recv_shape_offset: raw as u32,
            holder_shape_offset: 0,
        }
    }

    #[test]
    fn isolate_method_distribution_transitions_to_bounded_poly_then_mega() {
        let mut feedback = None;
        for raw in 1..=MAX_POLY_METHOD_TARGETS as u64 {
            assert!(record_method_distribution(
                &mut feedback,
                raw as u32,
                method_site(raw)
            ));
        }
        let Some(MethodCallFeedback::Poly(targets)) = &feedback else {
            panic!("bounded method distribution must be polymorphic");
        };
        assert_eq!(targets.len(), MAX_POLY_METHOD_TARGETS);

        assert!(!record_method_distribution(
            &mut feedback,
            MAX_POLY_METHOD_TARGETS as u32,
            method_site(MAX_POLY_METHOD_TARGETS as u64),
        ));
        let Some(MethodCallFeedback::Poly(targets)) = &feedback else {
            panic!("repeated method target must remain polymorphic");
        };
        assert_eq!(targets.last().map(|target| target.hits), Some(2));

        assert!(record_method_distribution(
            &mut feedback,
            99,
            method_site(99)
        ));
        assert!(matches!(feedback, Some(MethodCallFeedback::Megamorphic)));
        assert!(!record_method_distribution(
            &mut feedback,
            100,
            method_site(100)
        ));
    }

    #[test]
    fn native_and_bytecode_method_targets_are_material_transitions() {
        let native_site = method_site(1);
        let mut feedback = None;
        assert!(record_method_native_leaf_distribution(
            &mut feedback,
            7,
            native_site
        ));
        assert!(!record_method_native_leaf_distribution(
            &mut feedback,
            7,
            native_site
        ));
        assert!(record_method_distribution(
            &mut feedback,
            42,
            method_site(2)
        ));
        assert!(matches!(feedback, Some(MethodCallFeedback::Megamorphic)));

        let mut feedback = None;
        assert!(record_method_distribution(
            &mut feedback,
            42,
            method_site(1)
        ));
        assert!(record_method_native_leaf_distribution(
            &mut feedback,
            7,
            method_site(2)
        ));
        assert!(matches!(feedback, Some(MethodCallFeedback::Megamorphic)));
        assert!(!record_method_native_leaf_distribution(
            &mut feedback,
            7,
            method_site(2)
        ));
    }

    #[test]
    fn codeblock_feedback_is_send_sync_without_interior_locks() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<crate::feedback::FeedbackVector>();
    }
}
