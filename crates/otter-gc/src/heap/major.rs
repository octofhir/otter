//! Full and incremental old-generation collection orchestration.
//!
//! # Contents
//! - Growth triggers, concurrent marking and finishing pauses.
//! - Ephemeron/weak pruning, sweeping and post-major budgets.
//!
//! # Invariants
//! - Finishing cycles mark roots, resolve weak owners, then reclaim dead bodies.
//! - Growth collection cannot reenter itself through its finishing scavenge.
//!
//! # See also
//! - [`super::sweeping`].

use super::{GcHeap, RootSlotVisitor, allocation_limits, sweep};
use crate::collection_goal::FullGcGoal;
use crate::compressed::{RawGc, cage_base};
use crate::header::GcHeader;
use crate::marking::MarkingState;
use crate::observation::{GcPauseKind, GcPauseOutcome, GcPauseTrigger};
use crate::oom::OutOfMemory;
use std::time::Instant;

/// Old-generation size from which a stop-the-world mark drain runs on the
/// helper threads too.
const PARALLEL_MARK_MIN_OLD_BYTES: u64 = 8 * 1024 * 1024;
/// Smallest overshoot of the old-generation limit that stops the world for
/// a mark-sweep rather than letting a concurrent cycle run on.
const LIMIT_OVERSHOOT_MARGIN_FOR_SMALL_HEAPS: u64 = 32 * 1024 * 1024;
impl GcHeap {
    /// Run an explicit full GC and release unused physical payload.
    ///
    /// `external_visit` yields every external root slot.
    pub fn collect_full(
        &mut self,
        external_visit: &mut RootSlotVisitor<'_>,
    ) -> Result<(), OutOfMemory> {
        self.collect_full_observed(
            external_visit,
            GcPauseTrigger::Explicit,
            FullGcGoal::ReleaseMemory,
        )
    }

    /// Collect with the owning request's physical-memory goal and exact trigger
    /// provenance. Both goals use the same collector and canonical root visitors;
    /// capture state and trigger labels never choose the goal.
    pub fn collect_full_observed(
        &mut self,
        external_visit: &mut RootSlotVisitor<'_>,
        trigger: GcPauseTrigger,
        goal: FullGcGoal,
    ) -> Result<(), OutOfMemory> {
        self.with_gc_pause_result(GcPauseKind::Full, trigger, |heap| {
            heap.collect_full_work(external_visit, goal)
        })
    }

    pub(super) fn collect_full_work(
        &mut self,
        external_visit: &mut RootSlotVisitor<'_>,
        goal: FullGcGoal,
    ) -> Result<(), OutOfMemory> {
        self.complete_old_sweep();
        let pause_start = Instant::now();
        let old_generation_before = self.old_generation_consumed_bytes();
        // A cycle already marking, concurrently or between incremental
        // steps, finishes; otherwise the whole mark runs in this pause.
        if self.marking.is_marking() {
            self.finish_incremental_mark_phase(external_visit)?;
        } else {
            self.mark_phase(external_visit)?;
        }
        self.run_post_mark_processing();
        self.sweep_phase_with_pause_start(pause_start, goal, Some(old_generation_before));
        Ok(())
    }

    /// Growth-ratio major-GC trigger for cap-less heaps.
    ///
    /// Called from the allocation slow path when no hard cap is set.
    /// Runs a full GC once old+LOS occupancy reaches the recomputed
    /// growth threshold (or the cage soft-cap backstop), then sets the
    /// next threshold from the post-GC live size. This is what keeps a
    /// cap-less heap from growing old space without bound — without it,
    /// `collect_full` is only ever reached via the cap path. No-op when
    /// a hard cap is configured (that path drives full GCs itself) or
    /// while a major GC is already in flight.
    pub(super) fn maybe_major_gc(
        &mut self,
        external_visit: &mut RootSlotVisitor<'_>,
    ) -> Result<(), OutOfMemory> {
        if self.always_allocate_depth != 0 || self.in_major_gc {
            return Ok(());
        }
        if self.sweeping.is_some() {
            // The last cycle's sweep is still settling the old generation's
            // size; no cycle starts before it completes.
            self.drain_old_sweep();
            if self.sweeping.is_some() {
                return Ok(());
            }
        }
        if self.marking.is_concurrent() {
            // Helpers are marking: hand them what the barrier shaded, and
            // finish the cycle once they ran dry, the old generation
            // overshot its limit by the margin the cycle may run into, or
            // external bytes reached theirs.
            self.marking.publish();
            if !self.marking.concurrent_done()
                && !self.limit_overshot_by_large_margin()
                && !self.external_limit_reached()
            {
                return Ok(());
            }
        } else {
            // The old-generation limit is soft: reaching it starts a
            // concurrent cycle, and only an overshoot past the margin stops
            // the world for one. The external limit stays hard.
            let due = self.major_gc_due();
            if (due || self.concurrent_marking_due())
                && !self.external_limit_reached()
                && self.concurrent_marking_possible()
                && !self.limit_overshot_by_large_margin()
            {
                self.with_gc_pause_result(
                    GcPauseKind::MarkStart,
                    GcPauseTrigger::GrowthBudget,
                    |heap| heap.start_concurrent_marking_work(external_visit),
                )?;
                return Ok(());
            }
            if !due {
                return Ok(());
            }
        }
        self.with_gc_pause_result(GcPauseKind::Full, GcPauseTrigger::GrowthBudget, |heap| {
            heap.collect_major_work(external_visit)
        })
    }

    /// Whether a concurrent marking cycle should start now: the old
    /// generation reached its start occupancy and a cycle can start.
    pub(super) fn concurrent_marking_due(&mut self) -> bool {
        self.old_generation_consumed_bytes() >= self.concurrent_marking_start
            && self.concurrent_marking_possible()
    }

    /// Whether a concurrent marking cycle can start: none runs and helper
    /// threads exist.
    pub(super) fn concurrent_marking_possible(&mut self) -> bool {
        !self.marking.is_marking()
            && self.machine_allocation_allowed()
            && self.gc_threads.workers() > 1
    }

    /// Whether the old generation passed its limit by more than a running
    /// concurrent cycle may let it: half the limit, at least
    /// [`LIMIT_OVERSHOOT_MARGIN_FOR_SMALL_HEAPS`], and at most half the room
    /// left under the cage ceiling.
    pub(super) fn limit_overshot_by_large_margin(&self) -> bool {
        let consumed = self.old_generation_consumed_bytes();
        let limit = self.old_generation_limit;
        if consumed < limit {
            return false;
        }
        let ceiling = allocation_limits::cage_hardcap_bytes();
        let margin = (limit / 2)
            .max(LIMIT_OVERSHOOT_MARGIN_FOR_SMALL_HEAPS)
            .min(ceiling.saturating_sub(limit) / 2);
        consumed - limit >= margin
    }

    /// Whether external bytes reached their limit, which a mark-sweep
    /// answers at once.
    pub(super) fn external_limit_reached(&self) -> bool {
        self.reserved_bytes >= self.external_limit
    }

    /// Begin a concurrent marking cycle: shade the roots and hand the drain
    /// to the helper threads, which run it beside the mutator until the
    /// collection that finishes the cycle stops them.
    pub(super) fn start_concurrent_marking_work(
        &mut self,
        external_visit: &mut RootSlotVisitor<'_>,
    ) -> Result<(), OutOfMemory> {
        // Much of the old generation may be reachable only through young
        // cells, which the concurrent marker never enters: a scavenge leaves
        // the nursery holding only live ones, whose old targets are roots.
        // Old cells are born black through the canonical allocator while the
        // cycle runs, so the old buffer stays retired.
        self.collect_minor_internal(external_visit)?;
        self.prepare_mark_cycle();
        self.marking.start_cycle();
        self.shade_young_edges();
        self.shade_roots(external_visit);
        // SAFETY: the cycle is active, the worklist holds gray old headers,
        // and every collection, heap walk and the heap's drop stop the
        // drain first.
        unsafe {
            self.marking
                .start_concurrent(&self.trace_table, &mut self.gc_threads)
        };
        self.marking.set_mutator_phase(true);
        Ok(())
    }

    pub(super) fn collect_major_work(
        &mut self,
        external_visit: &mut RootSlotVisitor<'_>,
    ) -> Result<(), OutOfMemory> {
        if self.always_allocate_depth != 0 || !(self.major_gc_due() || self.marking.is_concurrent())
        {
            return Ok(());
        }
        self.in_major_gc = true;
        let collect_result = self.collect_full_observed(
            external_visit,
            GcPauseTrigger::GrowthBudget,
            FullGcGoal::Throughput,
        );
        self.in_major_gc = false;
        collect_result
    }

    pub(super) fn major_gc_due(&self) -> bool {
        // Runs regardless of any byte cap: the default cap equals the
        // cage size, so it can never fire before the cage physically
        // exhausts (pages carry overhead the byte-cap doesn't see).
        // This growth trigger is what actually bounds old-space.
        if self.in_major_gc {
            return false;
        }
        // The bytes old cells occupy, not the pages they span: swept holes
        // are reusable space, so a fragmented generation is measured by
        // what it holds. The limit is itself clamped below the cage ceiling,
        // so a large live set settles just under it instead of collecting
        // on every allocation.
        self.old_generation_consumed_bytes() >= self.old_generation_limit
            || self.reserved_bytes >= self.external_limit
    }

    /// Begin a full-GC mark phase and drain the ordinary root graph.
    ///
    /// Ephemeron users may call [`Self::mark_additional`] after this
    /// method and before [`Self::sweep_phase`] to run a weak-table
    /// fixpoint.
    ///
    /// This convenience wrapper performs the entire mark phase under
    /// one STW pause; the incremental driver
    /// ([`Self::start_incremental_mark_phase`] /
    /// [`Self::incremental_mark_step`] /
    /// [`Self::finish_incremental_mark_phase`]) splits the same work
    /// across multiple safepoints so the mutator can run between
    /// drain steps. Both paths leave the heap in identical states.
    pub fn mark_phase(
        &mut self,
        external_visit: &mut RootSlotVisitor<'_>,
    ) -> Result<(), OutOfMemory> {
        self.with_gc_pause_result(GcPauseKind::SplitMark, GcPauseTrigger::Explicit, |heap| {
            heap.mark_phase_work(external_visit)
        })
    }

    pub(super) fn mark_phase_work(
        &mut self,
        external_visit: &mut RootSlotVisitor<'_>,
    ) -> Result<(), OutOfMemory> {
        self.start_incremental_mark_phase(external_visit)?;
        // The whole mark runs in this pause: no mutator runs inside it.
        self.marking.set_mutator_phase(false);
        // SAFETY: STW pause; all pushed headers alive.
        unsafe { self.drain_marking_stopped() };
        Ok(())
    }

    /// Drain the marking worklist under a stop-the-world pause, on the
    /// helper threads as well once the old generation is large enough to
    /// share.
    ///
    /// # Safety
    ///
    /// STW pause; every header on the worklist is alive.
    pub(super) unsafe fn drain_marking_stopped(&mut self) {
        // SAFETY: forwarded from the caller; trace callbacks are
        // object-local.
        unsafe {
            if self.old_generation_page_bytes() >= PARALLEL_MARK_MIN_OLD_BYTES {
                self.marking
                    .drain_parallel(&self.trace_table, &mut self.gc_threads);
            } else {
                self.marking.drain_full(&self.trace_table);
            }
        }
    }

    /// Begin an incremental old-gen mark cycle.
    ///
    /// Performs the parts of [`Self::mark_phase`] that can only run
    /// under an STW pause — pre-scavenge, mark-bit reset, root scan
    /// — then returns with `is_marking == true`. The mutator may
    /// resume between this call and the matching
    /// [`Self::finish_incremental_mark_phase`]:
    ///
    /// - Pointer stores route through [`Self::write_barrier`] /
    ///   [`Self::record_write`]; the **insertion** half of the
    ///   barrier shades any white child the mutator publishes
    ///   (Dijkstra invariant).
    /// - New old-gen allocations are stamped black at birth so the
    ///   mark cycle never traces freshly-published children.
    ///
    /// Drain progress is made by repeatedly calling
    /// [`Self::incremental_mark_step`]. Once the mutator is ready
    /// to finish, [`Self::finish_incremental_mark_phase`] re-scans
    /// roots (catching any updates the barrier could not observe —
    /// e.g. handles popped & re-pushed mid-cycle) and drains to
    /// completion.
    ///
    /// Uses a Dijkstra-style insertion barrier with a black-on-allocation
    /// new-object policy so the mutator can run between mark steps without
    /// losing a freshly published edge.
    pub fn start_incremental_mark_phase(
        &mut self,
        external_visit: &mut RootSlotVisitor<'_>,
    ) -> Result<(), OutOfMemory> {
        self.with_gc_pause_result(GcPauseKind::SplitMark, GcPauseTrigger::Explicit, |heap| {
            heap.start_incremental_mark_phase_work(external_visit)
        })
    }

    pub(super) fn start_incremental_mark_phase_work(
        &mut self,
        external_visit: &mut RootSlotVisitor<'_>,
    ) -> Result<(), OutOfMemory> {
        // Scavenge first: the nursery then holds only survivors, which mark
        // the old generation as roots.
        self.collect_minor_internal(external_visit)?;
        self.prepare_mark_cycle();
        self.marking.start_cycle();
        self.shade_young_edges();
        self.shade_roots(external_visit);
        self.marking.set_mutator_phase(true);
        Ok(())
    }

    /// Reset the old generation's live counters and make every old cell
    /// white for a new marking cycle.
    pub(super) fn prepare_mark_cycle(&mut self) {
        self.old_space.reset_live_bytes();
        self.large_space.reset_live_bytes();
        // Every header must be white before we begin. The previous cycle's
        // sweep whitened its survivors and nothing shades outside a cycle,
        // so the whole-heap reset runs only after an unswept cycle or a
        // restored image.
        if !self.marks_white {
            // SAFETY: STW pause + we own the spaces.
            unsafe {
                for page in self.old_space.pages() {
                    page.for_each_object(|h, _| {
                        (*h).clear_mark();
                    });
                }
                for page in self.large_space.pages() {
                    page.for_each_object(|h, _| {
                        (*h).clear_mark();
                    });
                }
            }
        }
        self.marks_white = false;
    }

    /// Shade every old cell a young cell points at: roots of the old
    /// generation's marking, which never enters young cells. Right after a
    /// scavenge the nursery holds only the cells it kept alive.
    pub(super) fn shade_young_edges(&mut self) {
        let marking: *mut MarkingState = &mut self.marking;
        let trace_table = &self.trace_table;
        for page in self.new_space.from_pages() {
            // SAFETY: STW pause with the nursery buffer published; every
            // from-space cell is initialized, with a registered type.
            unsafe {
                page.for_each_object(|h, _| {
                    if (*h).type_tag() != crate::header::FREE_TAG {
                        trace_table.trace(h, &mut |slot| (*marking).shade_from_slot(slot));
                    }
                });
            }
        }
    }

    /// Drain at most `budget` gray headers from the marking
    /// worklist. Returns the number of headers processed (always
    /// `<= budget`); a return of `0` signals the worklist is
    /// currently empty, though the insertion barrier may push more
    /// before [`Self::finish_incremental_mark_phase`] is called.
    ///
    /// Calling this outside an active mark cycle is a no-op.
    #[must_use]
    pub fn incremental_mark_step(&mut self, budget: usize) -> usize {
        if !self.marking.is_marking() {
            return 0;
        }
        self.with_gc_pause(GcPauseKind::SplitStep, GcPauseTrigger::Explicit, |heap| {
            (
                heap.incremental_mark_step_work(budget),
                GcPauseOutcome::Completed,
            )
        })
    }

    pub(super) fn incremental_mark_step_work(&mut self, budget: usize) -> usize {
        if !self.marking.is_marking() {
            return 0;
        }
        self.prepare_collection_observations();
        // SAFETY: every header on the worklist was pushed while
        // alive. New old-gen allocations during the cycle are
        // black-at-birth so they never enter the worklist; the
        // mutator cannot drop a live old-gen object out from under
        // a gray header (sweep is gated on the matching
        // `finish_incremental_mark_phase` returning).
        unsafe { self.marking.drain_with_budget(budget, &self.trace_table) }
    }

    /// Re-scan roots and drain the worklist to completion.
    ///
    /// Pairs with [`Self::start_incremental_mark_phase`]. After
    /// this returns the heap is in the same state as the end of
    /// [`Self::mark_phase`]: `is_marking == true` (cleared inside
    /// [`Self::sweep_phase`]), every reachable old-gen object is
    /// black, and the worklist is empty.
    ///
    /// # Errors
    ///
    /// Same as the scavenge it begins with.
    pub fn finish_incremental_mark_phase(
        &mut self,
        external_visit: &mut RootSlotVisitor<'_>,
    ) -> Result<(), OutOfMemory> {
        self.with_gc_pause_result(GcPauseKind::SplitMark, GcPauseTrigger::Explicit, |heap| {
            heap.finish_incremental_mark_phase_work(external_visit)
        })
    }

    pub(super) fn finish_incremental_mark_phase_work(
        &mut self,
        external_visit: &mut RootSlotVisitor<'_>,
    ) -> Result<(), OutOfMemory> {
        self.marking.set_mutator_phase(false);
        self.marking.stop_concurrent(&mut self.gc_threads);
        // The scavenge grays what it promotes; the nursery then holds only
        // survivors, which are roots of the old generation.
        self.collect_minor_internal(external_visit)?;
        self.shade_young_edges();
        self.prepare_collection_observations();
        // Re-shade roots — the mutator may have rewritten handles
        // or globals between mark steps. The barrier covered every
        // *new* white-child publication, but a slot whose pointer
        // changed twice (new white → another white) only retains
        // its final value, so we walk the live root set again to
        // pick up any white target the barrier already shaded gray
        // (idempotent) plus any pointer the mutator stored straight
        // into a root slot (where no barrier fires).
        self.shade_roots(external_visit);
        self.marking.take_final_visits();
        // SAFETY: STW pause covers the final drain.
        unsafe { self.drain_marking_stopped() };
        if crate::scavenger::gc_verify_enabled() {
            self.verify_marking_closed();
        }
        Ok(())
    }

    /// Mark additional raw objects during an active mark phase and
    /// drain their transitive closure.
    ///
    /// Returns `true` when at least one previously-white object was
    /// discovered.
    #[doc(hidden)]
    pub fn mark_additional(&mut self, additions: impl IntoIterator<Item = RawGc>) -> bool {
        self.with_gc_pause(GcPauseKind::SplitMark, GcPauseTrigger::Explicit, |heap| {
            (
                heap.mark_additional_work(additions),
                GcPauseOutcome::Completed,
            )
        })
    }

    pub(super) fn mark_additional_work(
        &mut self,
        additions: impl IntoIterator<Item = RawGc>,
    ) -> bool {
        let mut discovered = false;
        for raw in additions {
            if raw.is_null() || self.is_marked(raw) {
                continue;
            }
            let mut slot = raw;
            // SAFETY: `raw` came from a live heap payload inspected
            // during the STW ephemeron phase.
            unsafe {
                self.marking.shade_from_slot(&mut slot as *mut RawGc);
            }
            discovered = true;
        }
        if discovered {
            // SAFETY: STW pause; all pushed headers are heap objects.
            unsafe {
                self.marking.drain_full(&self.trace_table);
            }
        }
        discovered
    }

    /// Returns true iff `raw` is marked in the current full-GC cycle.
    #[doc(hidden)]
    #[must_use]
    pub fn is_marked(&self, raw: RawGc) -> bool {
        raw_is_marked(raw)
    }

    /// Prune the ephemeron registry to tables that survived the
    /// current mark phase. Must run before sweep frees dead tables.
    pub fn prune_ephemeron_registry_to_marked(&mut self) {
        self.ephemerons.retain_marked(raw_is_marked);
    }

    /// Prune weak-reference/finalization registries to handles
    /// that survived the current mark phase. Must run before sweep
    /// frees dead bodies.
    pub fn prune_weak_finalization_registry_to_marked(&mut self) {
        self.weak_finalization.retain_marked(raw_is_marked);
    }

    /// Finish an explicit split full-GC cycle and release unused physical payload.
    pub fn sweep_phase(&mut self) {
        self.with_gc_pause(GcPauseKind::SplitSweep, GcPauseTrigger::Explicit, |heap| {
            heap.sweep_phase_with_pause_start(Instant::now(), FullGcGoal::ReleaseMemory, None);
            ((), GcPauseOutcome::Completed)
        });
    }

    /// Sweep after a completed mark. With `limits` (the old generation's
    /// size before the collection), the allocation limits follow the
    /// completed sweep.
    pub(super) fn sweep_phase_with_pause_start(
        &mut self,
        pause_start: Instant,
        goal: FullGcGoal,
        limits: Option<u64>,
    ) {
        self.retire_machine_labs();
        self.prepare_collection_observations();
        // Heap-owned weak state is always present, even when a split sweep
        // has no temporary mutator/root-source registration. Mark bits are
        // final and no old-space cell has been reclaimed yet.
        if let Some(data) = &self.isolate_data {
            data.sweep_weak(self);
        }
        // This is also the split mark/sweep entry. Resolve observations here
        // so even a caller without an embedder post-mark pass cannot leave a
        // freed old offset in an owner cache. Full collections have already
        // completed the ephemeron fixpoint before reaching this boundary.
        for (index, source) in self.extra_roots.iter().enumerate() {
            if !self.extra_roots[..index]
                .iter()
                .any(|prior| prior.same_source(source))
            {
                source.process_weak(&mut |raw| raw_is_marked(raw).then_some(raw));
            }
        }
        self.prune_ephemeron_registry_to_marked();
        self.prune_weak_finalization_registry_to_marked();
        self.weak_cleanup_due |= self.weak_finalization.has_finalization_registries();

        // White large cells are reclaimed and wholly dead large pages reaped
        // here; old pages are swept by an [`sweep::OldSweep`] cycle, in this
        // pause or behind it. The finishing scavenge has already copied
        // nursery survivors into the active from-space. Preserve its
        // initialized prefix, including cells whose old parent became
        // unreachable only in this full mark; the next scavenge can reclaim
        // those cells. Only inactive payload and unused active tails may be
        // discarded.
        //
        // The same walks accumulate per-tag live counts for
        // [`crate::stats::GcStats`] reconciliation, so a full collection
        // pays for one heap walk.
        let track_cap = self.max_heap_bytes != 0;
        let mut totals = sweep::SweepTotals::default();
        // Prune the remembered set to parents that survived this full GC.
        // The embedded pre-mark scavenge (`start_incremental_mark_phase`)
        // repopulated the buffer with the live old→young edges among
        // survivors, but mark-sweep may then free some of those parents as
        // unreachable. A dead parent left in the buffer would dangle once
        // `reap_dead_pages` frees its page and the cage offset is reused, so
        // drop every entry whose header did not survive the mark (and clear
        // its now-meaningless remembered bit). Runs before the sweep below
        // whitens the survivors, while every header still shows its color.
        self.remembered_parents.retain(|raw| {
            // SAFETY: STW pause; `raw` is an in-cage old/large parent offset
            // recorded by the barrier; its header is live until the reap below.
            let header = unsafe { cage_base().add(raw.0 as usize) as *const GcHeader };
            if unsafe { (*header).is_marked() } {
                true
            } else {
                unsafe { (*header).clear_remembered() };
                false
            }
        });
        // SAFETY: STW pause.
        unsafe {
            for page in self.large_space.pages() {
                page.for_each_object(|h, _| {
                    let tag = (*h).type_tag() as usize;
                    let size = (*h).size_bytes() as usize;
                    if !(*h).is_marked() {
                        // Old/large space is non-moving and reaps only
                        // whole dead pages, so a dropped corpse stays in
                        // the bump range and is re-walked by later
                        // sweeps. Drop exactly once — a second
                        // `drop_in_place` would double-free any buffer
                        // the payload owns (e.g. a string `Vec<u16>`).
                        if !(*h).is_swept() {
                            let tag_u8 = (*h).type_tag();
                            if let Some(finalize_fn) = self.trace_table.get_finalize(tag_u8) {
                                finalize_fn(h);
                            }
                            if let Some(drop_fn) = self.trace_table.get_drop(tag_u8) {
                                drop_fn(h);
                            }
                            (*h).set_swept();
                            totals.reclaimed += size;
                        }
                    } else {
                        (*h).clear_mark();
                        totals.live_count[tag] = totals.live_count[tag].wrapping_add(1);
                        totals.live_bytes[tag] = totals.live_bytes[tag].wrapping_add(size);
                    }
                });
                if track_cap && page.header().live_bytes != 0 {
                    totals.retained = totals
                        .retained
                        .saturating_add(page.header().allocated_bytes as u64);
                }
            }
            // Young from-space is recycled at next scavenge but
            // any survivor still in to-space (after the
            // pre-sweep scavenge flip) belongs to the live set.
            // After the flip, those survivors live in
            // `from_pages()` of the new orientation.
            for page in self.new_space.from_pages() {
                // These physical survivor cells remain in the nursery until
                // the next scavenge, regardless of per-type mark statistics.
                if track_cap {
                    totals.retained = totals
                        .retained
                        .saturating_add(page.header().allocated_bytes as u64);
                }
                // Every nursery cell is a survivor of the finishing scavenge.
                page.for_each_object(|h, _| {
                    if (*h).type_tag() != crate::header::FREE_TAG {
                        let tag = (*h).type_tag() as usize;
                        let size = (*h).size_bytes() as usize;
                        totals.live_count[tag] = totals.live_count[tag].wrapping_add(1);
                        totals.live_bytes[tag] = totals.live_bytes[tag].wrapping_add(size);
                    }
                });
            }
        }
        let _ = self.large_space.reap_dead_pages();
        self.marking.finish_cycle();
        self.gc_stats.gc_cycles = self.gc_stats.gc_cycles.saturating_add(1);
        let elapsed = pause_start.elapsed();
        self.gc_stats.last_gc_pause_ms = elapsed.as_secs_f32() * 1000.0;
        self.gc_stats.full_pause_ns_total = self
            .gc_stats
            .full_pause_ns_total
            .saturating_add(elapsed.as_nanos().min(u128::from(u64::MAX)) as u64);
        self.start_old_sweep(totals, goal, pause_start, limits);
    }
}

/// Header-only mark check for registry pruning: reads the mark bit
/// straight off the object header without touching heap state.
/// Whether `raw` survives the old generation's marking: marked, or young,
/// since a finishing collection's nursery holds only scavenge survivors.
fn raw_is_marked(raw: RawGc) -> bool {
    if raw.is_null() {
        return false;
    }
    // SAFETY: registries only hold heap-issued raw handles.
    unsafe {
        let header = &*raw.as_header_ptr();
        header.is_young() || header.is_marked()
    }
}
