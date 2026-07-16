//! Isolate feedback ownership and high-level inline-cache operations.
//!
//! # Contents
//! - Dense global property-site directory installation.
//! - Executable property and builtin-method IC banks.
//! - Intent-level IC accounting, installation, snapshots, and tracing views.
//! - Narrow lookup helpers for lock-free CodeBlock feedback slots.
//! - Single-writer bounded method-target distributions.
//!
//! # Invariants
//! - All mutable executable IC state is owned by the isolate and reached
//!   through this directory; the interpreter exposes no parallel IC vectors.
//! - CodeBlock property/call summaries contain atomics and stable numeric ids
//!   only. GC-bearing executable recipes never cross that boundary.
//! - A site id maps to exactly one canonical instruction for the lifetime of
//!   the isolate.
//! - Direct-call linkage caches remain separate from observational feedback.
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

/// Stable property-load recipe retained by a compiled direct-method cache.
/// The recipe carries shape/slot identity only and reloads the callable from
/// the current receiver on every invocation.
#[derive(Clone)]
pub(crate) enum MethodLoadHit {
    Own(crate::object::AtomOwnPropertyHit),
    DirectPrototype {
        receiver_shape_id: crate::object::ShapeId,
        prototype_hit: crate::object::AtomOwnPropertyHit,
    },
}

impl MethodLoadHit {
    #[must_use]
    pub(crate) fn receiver_shape_id(&self) -> crate::object::ShapeId {
        match self {
            Self::Own(hit) => hit.shape_id,
            Self::DirectPrototype {
                receiver_shape_id, ..
            } => *receiver_shape_id,
        }
    }

    /// Re-read the current callable under the cached receiver/prototype guards.
    pub(crate) fn reload(
        &self,
        recv: crate::object::JsObject,
        heap: &otter_gc::GcHeap,
    ) -> Option<crate::Value> {
        match *self {
            Self::Own(slot) => crate::object::load_plain_shaped_own_data_slot_hit(recv, heap, slot),
            Self::DirectPrototype {
                receiver_shape_id,
                prototype_hit,
            } => {
                if crate::object::shape_id(recv, heap) != receiver_shape_id {
                    return None;
                }
                let prototype = crate::object::prototype(recv, heap)?;
                crate::object::load_plain_shaped_own_data_slot_hit(prototype, heap, prototype_hit)
            }
        }
    }
}

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
    has_ics: Vec<ExecutablePropertyIc>,
    method_ics: Vec<Option<MethodCallIc>>,
    property_stats: PropertyIcStats,
}

impl FeedbackDirectory {
    fn install_context(&mut self, context: &ExecutionContext) {
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
        if self.has_ics.len() < site_count {
            self.has_ics.resize(site_count, PropertyIcEntry::Empty);
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
            PropertyIcKind::Has => &self.has_ics,
        }
    }

    fn property_bank_with_stats_mut(
        &mut self,
        kind: PropertyIcKind,
    ) -> (&mut [ExecutablePropertyIc], &mut PropertyIcStats) {
        match kind {
            PropertyIcKind::Load => (&mut self.load_ics, &mut self.property_stats),
            PropertyIcKind::Store => (&mut self.store_ics, &mut self.property_stats),
            PropertyIcKind::Has => (&mut self.has_ics, &mut self.property_stats),
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
    ) -> bool {
        self.property_stubs(site, PropertyIcKind::Store)
            .is_some_and(|stubs| {
                stubs
                    .iter()
                    .any(|stub| stub.run_store(obj, heap, key, value).is_some())
            })
    }

    /// Probe a named `in` site and return whether an installed recipe matched.
    pub(crate) fn probe_has(
        &self,
        site: usize,
        obj: crate::object::JsObject,
        heap: &otter_gc::GcHeap,
        key: crate::JsString,
    ) -> bool {
        self.property_stubs(site, PropertyIcKind::Has)
            .is_some_and(|stubs| {
                stubs
                    .iter()
                    .any(|stub| stub.run_has(obj, heap, key).is_some())
            })
    }

    /// Resolve a load recipe suitable for a compiled direct-method cache.
    /// Only own-data and direct-prototype data slots qualify.
    pub(crate) fn method_load_hit(
        &self,
        site: usize,
        obj: crate::object::JsObject,
        heap: &otter_gc::GcHeap,
        key: crate::property_atom::AtomizedPropertyKey<'_>,
        method: crate::Value,
    ) -> Option<MethodLoadHit> {
        for stub in self.property_stubs(site, PropertyIcKind::Load)? {
            if let Some(hit) = stub.own_data_hit()
                && crate::object::load_own_data_slot_atom(obj, heap, key, hit) == Some(method)
                && crate::object::load_plain_shaped_own_data_slot_hit(obj, heap, hit)
                    == Some(method)
            {
                return Some(MethodLoadHit::Own(hit));
            }
            if let Some((receiver_shape_id, prototype_hit)) = stub.direct_prototype_load()
                && crate::object::shape_id(obj, heap) == receiver_shape_id
                && let Some(prototype) = crate::object::prototype(obj, heap)
                && crate::object::load_own_data_slot_atom(prototype, heap, key, prototype_hit)
                    == Some(method)
                && crate::object::load_plain_shaped_own_data_slot_hit(
                    prototype,
                    heap,
                    prototype_hit,
                ) == Some(method)
            {
                return Some(MethodLoadHit::DirectPrototype {
                    receiver_shape_id,
                    prototype_hit,
                });
            }
        }
        None
    }

    /// Encode one monomorphic own-data load recipe for the WhiskerIC cell.
    pub(crate) fn whisker_load_cell_fill(
        &self,
        site: usize,
        obj: crate::object::JsObject,
        heap: &otter_gc::GcHeap,
        key: crate::property_atom::AtomizedPropertyKey<'_>,
    ) -> u64 {
        let recv_shape = crate::object::shape(obj, heap).offset();
        if recv_shape == 0 {
            return 0;
        }
        let Some(stubs) = self.property_stubs(site, PropertyIcKind::Load) else {
            return 0;
        };
        for stub in stubs {
            if let Some(hit) = stub.own_data_hit()
                && hit.shape.offset() == recv_shape
                && hit.atom_id == key.atom().id()
                && crate::object::load_own_data_slot_atom(obj, heap, key, hit).is_some()
            {
                let value_byte = u32::from(hit.slot)
                    * std::mem::size_of::<crate::value::compressed::CompressedValue>() as u32;
                return (u64::from(value_byte) << 32) | u64::from(hit.shape.offset());
            }
        }
        0
    }

    /// Encode one monomorphic existing-own-data store recipe for WhiskerIC.
    pub(crate) fn whisker_store_cell_fill(&self, site: usize, recv_shape: u32) -> u64 {
        if recv_shape == 0 {
            return 0;
        }
        let Some(stubs) = self.property_stubs(site, PropertyIcKind::Store) else {
            return 0;
        };
        for stub in stubs {
            if let Some(hit) = stub.store_own_data_hit()
                && hit.shape.offset() == recv_shape
            {
                let value_byte = u32::from(hit.slot)
                    * std::mem::size_of::<crate::value::compressed::CompressedValue>() as u32;
                return (u64::from(value_byte) << 32) | u64::from(hit.shape.offset());
            }
        }
        0
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
        if let Some(entry) = bank.get(site) {
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

    pub(crate) fn disable_property(&mut self, site: usize, kind: PropertyIcKind) {
        let (bank, stats) = self.property_bank_with_stats_mut(kind);
        if let Some(entry) = bank.get_mut(site) {
            entry.disable_with_stats(stats, kind);
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

    #[must_use]
    pub(crate) fn has_method_ic(&self, site: usize) -> bool {
        self.method_ic(site).is_some()
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
        let mut out =
            Vec::with_capacity(self.load_ics.len() + self.store_ics.len() + self.has_ics.len());
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
        for (index, entry) in self.has_ics.iter().enumerate() {
            out.push(crate::inspect::IcSiteSnapshot {
                site_index: index as u32,
                kind: crate::inspect::IcSiteKind::Has,
                state: crate::inspect::snapshot_has_state(entry),
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

    fn record_method_target(&mut self, site: usize, method_fid: u32, method_site: MethodSite) {
        if !self
            .address(site)
            .is_some_and(FeedbackSlotAddress::is_method)
        {
            return;
        }
        if let Some(targets) = self.method_targets.get_mut(site) {
            record_method_distribution(targets, method_fid, method_site);
        }
    }
}

/// Apply mono -> bounded-poly -> megamorphic transitions to isolate-owned
/// method feedback. This state never crosses the Send/Sync CodeBlock boundary.
fn record_method_distribution(
    feedback: &mut Option<MethodCallFeedback>,
    method_fid: u32,
    site: MethodSite,
) {
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
            }
        }
        Some(MethodCallFeedback::Poly(targets)) => {
            if let Some(existing) = targets
                .iter_mut()
                .find(|target| target.matches(method_fid, &site))
            {
                existing.hits = existing.hits.saturating_add(1);
            } else if targets.len() < MAX_POLY_METHOD_TARGETS {
                targets.push(new_target);
            } else {
                *feedback = Some(MethodCallFeedback::Megamorphic);
            }
        }
        Some(MethodCallFeedback::Megamorphic) => {}
    }
}

impl Interpreter {
    pub(crate) fn ensure_property_ic_capacity(&mut self, context: &ExecutionContext) {
        let site_count = context.property_ic_site_end();
        self.feedback_directory.install_context(context);
        if self.jit_direct_method_cache.len() < site_count {
            self.jit_direct_method_cache
                .resize_with(site_count, Vec::new);
        }
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
                otter_bytecode::Op::HasProperty => crate::property_ic::PropertyIcKind::Has,
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

    pub(crate) fn record_method_target_feedback(
        &mut self,
        site: usize,
        method_fid: u32,
        method_site: MethodSite,
    ) {
        self.feedback_directory
            .record_method_target(site, method_fid, method_site);
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
        }
    }

    #[test]
    fn isolate_method_distribution_transitions_to_bounded_poly_then_mega() {
        let mut feedback = None;
        for raw in 1..=MAX_POLY_METHOD_TARGETS as u64 {
            record_method_distribution(&mut feedback, raw as u32, method_site(raw));
        }
        let Some(MethodCallFeedback::Poly(targets)) = &feedback else {
            panic!("bounded method distribution must be polymorphic");
        };
        assert_eq!(targets.len(), MAX_POLY_METHOD_TARGETS);

        record_method_distribution(
            &mut feedback,
            MAX_POLY_METHOD_TARGETS as u32,
            method_site(MAX_POLY_METHOD_TARGETS as u64),
        );
        let Some(MethodCallFeedback::Poly(targets)) = &feedback else {
            panic!("repeated method target must remain polymorphic");
        };
        assert_eq!(targets.last().map(|target| target.hits), Some(2));

        record_method_distribution(&mut feedback, 99, method_site(99));
        assert!(matches!(feedback, Some(MethodCallFeedback::Megamorphic)));
    }

    #[test]
    fn codeblock_feedback_is_send_sync_without_interior_locks() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<crate::feedback::FeedbackVector>();
    }
}
