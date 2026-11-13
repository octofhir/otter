//! Minor collection, rooted evacuation and young large-body lifecycle.
//!
//! # Contents
//! - Combined roots, weak resolution and scavenge coordination.
//! - Nursery growth, promotion accounting and young-large admission.
//!
//! # Invariants
//! - Helpers and compiler readers stop before nursery movement.
//! - All root sources, weak hooks and remembered parents keep their order.
//!
//! # See also
//! - [`crate::scavenger`].

use super::roots::visit_extra_roots_deduped;
use super::{EmptyRoots, GcHeap, RootSlotVisitor};
use crate::compressed::RawGc;
use crate::extra_roots::{ExtraRoots, RootSet};
use crate::frame_roots::FrameRootProviders;
use crate::handle::{GlobalHandleTable, HandleStack};
use crate::observation::{GcPauseKind, GcPauseTrigger};
use crate::oom::OutOfMemory;
use std::time::Instant;

impl GcHeap {
    /// Run a minor GC (Cheney scavenge).
    pub fn collect_minor(&mut self, _roots: EmptyRoots) -> Result<(), OutOfMemory> {
        let empty: fn(&mut dyn FnMut(*mut RawGc)) = |_| {};
        self.collect_minor_internal(&mut |v| empty(v))
    }

    /// A young large-object page for `aligned` bytes, when the nursery
    /// admits it: the young large cells already allocated plus this one stay
    /// within the nursery's capacity, a scavenge first making room when they
    /// would not. `None` places the body in the old large object space: a
    /// body larger than the nursery itself, or an allocation inside a
    /// collection.
    pub(super) fn admit_young_large(
        &mut self,
        aligned: usize,
        allocation_roots: &mut RootSlotVisitor<'_>,
    ) -> Result<Option<u32>, OutOfMemory> {
        let capacity = self.new_space.capacity_bytes();
        if self.always_allocate_depth != 0 || aligned > capacity {
            return Ok(None);
        }
        if self.young_large_space.allocated_bytes() + aligned > capacity {
            self.collect_minor_observed(allocation_roots, GcPauseTrigger::NurseryCapacity)?;
        }
        Ok(Some(self.young_large_space.alloc(aligned)?))
    }

    /// After a scavenge: the young large cells it promoted move with their
    /// pages to the old large object space, and every other young large
    /// page is released after its body's reclamation hooks run. Returns the
    /// released bytes.
    pub(super) fn finish_young_large(&mut self) -> usize {
        let trace_table = &self.trace_table;
        let host_refs = &mut self.host_refs;
        self.young_large_space
            .promote_or_release(&mut self.large_space, |page| {
                let mut promoted = false;
                // SAFETY: STW after the scavenge; the page holds one
                // initialized cell of a registered type.
                unsafe {
                    page.for_each_object(|header, _| {
                        if !(*header).is_young() {
                            promoted = true;
                            return;
                        }
                        let tag = (*header).type_tag();
                        if let Some(release) = trace_table.get_host_release(tag) {
                            release(header, host_refs);
                        }
                        if let Some(finalize) = trace_table.get_finalize(tag) {
                            finalize(header);
                        }
                        if let Some(drop_fn) = trace_table.get_drop(tag) {
                            drop_fn(header);
                        }
                    });
                }
                promoted
            })
    }

    /// Run a minor GC with caller-supplied root visitor.
    pub fn collect_minor_with_roots(
        &mut self,
        external_visit: &mut RootSlotVisitor<'_>,
    ) -> Result<(), OutOfMemory> {
        self.collect_minor_internal(external_visit)
    }

    pub(super) fn collect_minor_internal(
        &mut self,
        external_visit: &mut RootSlotVisitor<'_>,
    ) -> Result<(), OutOfMemory> {
        self.collect_minor_observed(external_visit, GcPauseTrigger::Explicit)
    }

    pub(super) fn collect_minor_observed(
        &mut self,
        external_visit: &mut RootSlotVisitor<'_>,
        trigger: GcPauseTrigger,
    ) -> Result<(), OutOfMemory> {
        self.with_gc_pause_result(GcPauseKind::Minor, trigger, |heap| {
            heap.collect_minor_work(external_visit)
        })
    }

    pub(super) fn collect_minor_work(
        &mut self,
        external_visit: &mut RootSlotVisitor<'_>,
    ) -> Result<(), OutOfMemory> {
        // The scavenge needs the helpers and promotes into old space.
        self.complete_old_sweep();
        // A concurrent marker never reads young cells, but the scavenge needs
        // the helpers; it resumes once the nursery is evacuated.
        let resume_marking = self.marking.is_concurrent();
        self.marking.stop_concurrent(&mut self.gc_threads);
        let marking = self.marking.is_marking();
        let mut promoted_gray = Vec::new();
        self.retire_machine_labs();
        self.prepare_collection_observations();
        // Combine the caller's external_visit with the heap's
        // own handle-stack and global-handles walk.
        let handle_stack: *const HandleStack = &*self.handle_stack;
        let global_handles: *const GlobalHandleTable = &*self.global_handles;
        let extra_roots: *const Vec<ExtraRoots> = &self.extra_roots;
        let frame_root_providers: *const FrameRootProviders = &self.frame_root_providers;
        let embedder_roots: *const Self = self;
        let mut combined = move |visitor: &mut dyn FnMut(*mut RawGc)| {
            // SAFETY: STW pause; raw pointers reconstituted to
            // shared references.
            unsafe {
                (*handle_stack).visit_slots(visitor);
                (*global_handles).visit_slots(visitor);
                (*embedder_roots).trace_embedder_roots(visitor);
            }
            external_visit(visitor);
            // SAFETY: STW pause; the heap owns the registration stack for the
            // duration of this root walk.
            visit_extra_roots_deduped(unsafe { &*extra_roots }, RootSet::Young, visitor);
            // SAFETY: STW pause; the heap owns the registry for the duration of
            // this root walk.
            unsafe { (*frame_root_providers).trace(visitor) };
        };
        let ephemeron_registry_slots = self.ephemerons.handle_slots();
        let weak_registry_slots = self.weak_finalization.handle_slots();
        let mut process_weak = |resolve: &mut dyn FnMut(RawGc) -> Option<RawGc>| {
            // SAFETY: the registration stack is unchanged during this pause;
            // no weak owner may allocate or change registrations.
            let sources = unsafe { &*extra_roots };
            for (index, source) in sources.iter().enumerate() {
                if !sources[..index]
                    .iter()
                    .any(|prior| prior.same_source(source))
                {
                    source.process_weak(resolve);
                }
            }
        };
        if crate::scavenger::gc_verify_enabled() {
            self.verify_remembered_complete();
        }
        // SAFETY: STW pause for the duration of the call;
        // every type tag in from-space is registered.
        let scavenge_start = Instant::now();
        let nursery_at_maximum = self.new_space.at_maximum();
        let mut stats = unsafe {
            crate::scavenger::scavenge(
                &mut self.new_space,
                &mut self.old_space,
                &self.trace_table,
                &[],
                &mut combined,
                &ephemeron_registry_slots,
                &weak_registry_slots,
                &mut process_weak,
                &mut self.remembered_parents,
                &mut self.host_refs,
                &mut self.allocation_sites,
                &mut self.gc_threads,
                marking.then_some(&mut promoted_gray),
            )
        }?;
        for raw in promoted_gray {
            // SAFETY: the scavenge promoted this cell gray just now.
            unsafe { self.marking.push_gray(raw.as_header_ptr()) };
        }
        let released_large = self.finish_young_large();
        stats.reclaimed_bytes += released_large;
        // The nursery grows once the scavenges since its last growth kept
        // a whole capacity alive: its objects outlive the space they get.
        self.survived_since_last_expansion +=
            stats.copied_bytes + stats.promoted_bytes + stats.promoted_large_bytes;
        if !self.new_space.at_maximum()
            && self.survived_since_last_expansion > self.new_space.capacity_bytes()
        {
            self.new_space.grow_capacity();
            self.survived_since_last_expansion = 0;
        }
        self.allocation_sites.digest(nursery_at_maximum);
        stats.minor_pause_ns = scavenge_start.elapsed().as_nanos() as u64;
        self.ephemerons.retain_non_null();
        self.weak_finalization.retain_non_null();
        self.weak_cleanup_due |= self.weak_finalization.has_finalization_registries();
        if self.max_heap_bytes != 0 {
            // Successful evacuation preserves copied/promoted bytes; only
            // doomed physical nursery cells disappear. Failed preflight never
            // reaches this commit. Payload token drops are drained separately.
            self.tracked_bytes = self
                .tracked_bytes
                .saturating_sub(stats.reclaimed_bytes as u64);
            self.drain_shared_external_releases();
        }
        self.stats.last_scavenge = stats;
        self.gc_stats.record_minor(&stats);
        self.seed_initial_old_generation_limit(&stats);
        // Per-tag counters drift between scavenges (young
        // allocations counted at alloc-time but never
        // decremented when the scavenger reclaims them); the
        // drift is corrected on the next full GC via
        // [`Self::reconcile_live_counts`]. Scavenge keeps the
        // hot path tight by not walking the live set just for
        // stats — see `bench_scavenge_4mb` for the cost of
        // adding a walk here.
        if resume_marking {
            // SAFETY: the cycle is still active; the scavenge is complete.
            unsafe {
                self.marking
                    .start_concurrent(&self.trace_table, &mut self.gc_threads)
            };
        }
        Ok(())
    }
}
