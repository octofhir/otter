//! Isolate feedback ownership and high-level inline-cache operations.
//!
//! # Contents
//! - Dense global method-site directory installation.
//! - Executable builtin-method IC banks.
//! - Single-writer bounded method-target distributions.
//! - Range purge for tombstoned code chunks.
//!
//! # Invariants
//! - Property IC programs live only in their owning CodeBlock feedback slot.
//!   This directory retains method-only state keyed by the existing global
//!   site identity.
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
use crate::method_ops::MethodCallIc;
use crate::{
    ExecutionContext, Interpreter, JitCollectionMethodIcStats, MAX_POLY_METHOD_TARGETS,
    MethodCallFeedback, MethodSite, PolyMethodTarget,
};
use smallvec::SmallVec;

/// Isolate-local method-feedback directory. Property programs and counters are
/// owned directly by CodeBlock slots and never enter this structure.
#[derive(Default)]
pub(crate) struct MethodFeedbackDirectory {
    slots: Vec<Option<FeedbackSlotAddress>>,
    method_targets: Vec<Option<MethodCallFeedback>>,
    method_ics: Vec<Option<MethodCallIc>>,
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

impl MethodFeedbackDirectory {
    pub(crate) fn evict_site_range(&mut self, start: u32, end: u32) {
        let start = start as usize;
        let end = (end as usize).min(self.slots.len());
        for site in start..end {
            self.slots[site] = None;
            self.method_targets[site] = None;
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

    #[must_use]
    pub(crate) fn method_ic(&self, site: usize) -> Option<MethodCallIc> {
        self.method_ics.get(site).copied().flatten()
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
    pub(crate) fn ensure_method_feedback_context(&mut self, context: &ExecutionContext) {
        self.method_feedback.install_context(context);
    }

    #[must_use]
    pub(crate) fn method_target_feedback(&self, site: usize) -> Option<MethodCallFeedback> {
        self.method_feedback.method_targets(site)
    }

    #[must_use]
    pub(crate) fn method_target_feedback_saturated(&self, site: usize) -> bool {
        self.method_feedback.method_targets_saturated(site)
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
        self.method_feedback
            .record_method_native_leaf(site, stub_id, method_site)
    }

    pub(crate) fn record_method_target_feedback(
        &mut self,
        site: usize,
        method_fid: u32,
        method_site: MethodSite,
    ) -> bool {
        self.method_feedback
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
