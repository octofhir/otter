//! The collecting thread's side of old-space sweeping: one cycle per full
//! collection, finished in its pause or behind it on the helper threads.
//!
//! A full collection's pause ends once the old pages are closed to
//! allocation and their walk is handed to an [`sweep::OldSweep`]. A
//! collection that must release memory, or an old space too small to
//! share, walks every page before the pause ends. Otherwise the helpers'
//! background job walks them while the mutator runs; the mutator applies
//! the pages that finished whenever it refills old allocation space, walks
//! one more page itself when it needs room, and the cycle completes when
//! the last page is applied or something needs the whole heap swept.
//!
//! # Contents
//! - `GcHeap::start_old_sweep` — the pause's hand-off.
//! - `GcHeap::drain_old_sweep` / `GcHeap::help_old_sweep` — applying
//!   finished pages, and walking one, from allocation slow paths.
//! - `GcHeap::complete_old_sweep` and [`GcHeap::finish_sweeping`] — the
//!   cycle's end: every page applied, dead pages reaped, statistics,
//!   external releases and allocation limits committed.
//!
//! # Invariants
//! - At most one cycle exists. Every collection, marking start, heap walk
//!   and the heap's drop completes it first, so the walks never meet a
//!   collector or the next cycle's marks.
//! - A finished page is applied in one step: its dead cells' reclamation
//!   hooks run, its header takes the walk's cursor and bytes, and only then
//!   are its free ranges listed. A cycle that releases memory lists all
//!   ranges after discarding their unused payload.
//! - The background job holds the helpers: nothing else posts a job to
//!   them until the cycle completes, which joins it.
//!
//! # See also
//! - [`super::sweep`] — the page walk.
//! - [`super::allocation_limits`] — the limits a completed sweep sets.

use std::time::Instant;

use super::sweep::{self, OldSweep, SweepTotals};
use super::{FullGcGoal, GcHeap};
use crate::header::GcHeader;

/// Old pages from which a throughput collection sweeps them behind its
/// pause, on the helper threads.
const CONCURRENT_SWEEP_MIN_PAGES: usize = 8;
/// Old pages from which a sweep finished in a pause runs on the helper
/// threads too.
const PARALLEL_SWEEP_MIN_PAGES: usize = 32;

/// One full collection's sweep of its old pages.
pub(super) struct SweepCycle {
    sweep: Box<OldSweep>,
    /// The helpers run the walk as their background job.
    background: bool,
    /// Pages applied so far.
    applied: usize,
    /// Bases of the applied pages without survivors.
    dead: Vec<usize>,
    /// Free ranges held back until the cycle's end, when it releases memory.
    deferred_ranges: Option<Vec<(u32, usize)>>,
    goal: FullGcGoal,
    /// What the pause's own walks counted.
    totals: SweepTotals,
    pause_start: Instant,
    pause_end: Instant,
    /// The old generation's size before the collection, when the allocation
    /// limits follow this sweep.
    limits: Option<u64>,
}

impl GcHeap {
    /// Hand the old pages to a new sweep cycle at the end of a full
    /// collection's pause, and finish it here unless the helpers can take
    /// it behind the pause.
    pub(super) fn start_old_sweep(
        &mut self,
        totals: SweepTotals,
        goal: FullGcGoal,
        pause_start: Instant,
        limits: Option<u64>,
    ) {
        debug_assert!(self.sweeping.is_none());
        let sweep = OldSweep::new(self.old_space.pages(), &self.trace_table);
        self.old_space.begin_sweep();
        let behind = goal == FullGcGoal::Throughput
            && sweep.page_count() >= CONCURRENT_SWEEP_MIN_PAGES
            && self.gc_threads.workers() > 1;
        let mut cycle = SweepCycle {
            sweep,
            background: false,
            applied: 0,
            dead: Vec::new(),
            deferred_ranges: (goal == FullGcGoal::ReleaseMemory).then(Vec::new),
            goal,
            totals,
            pause_start,
            pause_end: Instant::now(),
            limits,
        };
        if behind {
            // SAFETY: the cycle's walk owns its pages until they are applied;
            // the boxed cycle stays in place until `complete_old_sweep`
            // joins the job.
            let helpers = unsafe {
                self.gc_threads
                    .start_background(sweep::sweep_background, (&raw const *cycle.sweep).cast())
            };
            cycle.background = helpers != 0;
        }
        self.sweeping = Some(cycle);
        if !behind {
            self.complete_old_sweep();
        }
    }

    /// Apply the pages that finished since the last call; complete the
    /// cycle once every page is applied.
    pub(super) fn drain_old_sweep(&mut self) {
        let Some(cycle) = self.sweeping.as_ref() else {
            return;
        };
        if cycle.sweep.finished() == cycle.applied {
            return;
        }
        let mut cycle = self.sweeping.take().expect("a sweep cycle");
        self.apply_swept(&mut cycle);
        let done = cycle.applied == cycle.sweep.page_count();
        self.sweeping = Some(cycle);
        if done {
            self.complete_old_sweep();
        }
    }

    /// Before old space takes a fresh page: apply what finished, and while
    /// that leaves nothing to reuse, walk one more unclaimed page on this
    /// thread.
    pub(super) fn help_old_sweep(&mut self) {
        self.drain_old_sweep();
        while let Some(cycle) = self.sweeping.as_ref() {
            if self.old_space.has_reusable_space() || cycle.sweep.all_claimed() {
                return;
            }
            // SAFETY: the cycle's pages are still unapplied, so the walk's
            // conditions hold for every page it can claim.
            unsafe { cycle.sweep.sweep_pages(1) };
            self.drain_old_sweep();
        }
    }

    /// Complete any sweep in progress: walk every page still unclaimed,
    /// wait for the helpers, and apply and commit everything. A heap walk
    /// or a host inspection of the heap calls this first.
    pub fn finish_sweeping(&mut self) {
        self.complete_old_sweep();
    }

    /// See [`Self::finish_sweeping`].
    pub(super) fn complete_old_sweep(&mut self) {
        let Some(mut cycle) = self.sweeping.take() else {
            return;
        };
        let context: *const () = (&raw const *cycle.sweep).cast();
        // SAFETY: as in `help_old_sweep`; the cycle outlives either job.
        unsafe {
            if cycle.background {
                cycle.sweep.sweep_pages(usize::MAX);
                self.gc_threads.join_background();
            } else if cycle.sweep.page_count() >= PARALLEL_SWEEP_MIN_PAGES
                && self.gc_threads.workers() > 1
            {
                self.gc_threads.run(sweep::sweep_background, context);
            } else {
                cycle.sweep.sweep_pages(usize::MAX);
            }
        }
        self.apply_swept(&mut cycle);
        debug_assert_eq!(cycle.applied, cycle.sweep.page_count());
        let old = cycle.sweep.into_totals();
        cycle.totals.absorb(&old);
        let _ = self.old_space.reap_swept_pages(&cycle.dead);
        // The walks whitened every surviving cell.
        self.marks_white = true;
        // All dead-cell and backing-store finalizers have completed. This is
        // the sole ledger commit shared by full, split and incremental sweep.
        self.drain_shared_external_releases();
        if self.max_heap_bytes != 0 {
            self.tracked_bytes = cycle.totals.retained.saturating_add(self.reserved_bytes);
        }
        if let Some(ranges) = cycle.deferred_ranges.take() {
            if cycle.goal == FullGcGoal::ReleaseMemory {
                // Every weak/finalizer/trace consumer is finished and marker
                // workers are stopped. Throughput requests retain resident
                // payload for reuse; release requests preserve its virtual
                // ownership.
                // SAFETY: the pause retired both buffers before walking the
                // heap; every reclamation hook ran and dead pages were reaped.
                unsafe { self.new_space.discard_unused_payload() };
                // Preserve every FREE header/link prefix before publication.
                // SAFETY: as above; the old free list holds none of `ranges`.
                unsafe { self.old_space.discard_swept_unused_payload(&ranges) };
            }
            for (offset, bytes) in ranges {
                self.old_space.push_free_range(offset, bytes);
            }
        }
        let reclaimed = cycle.totals.reclaimed;
        self.stats.last_full_reclaimed = reclaimed;
        self.stats.total_full_reclaimed = self.stats.total_full_reclaimed.saturating_add(reclaimed);
        self.gc_stats.last_gc_reclaimed_bytes = reclaimed;
        // Commit the per-tag counters the walks gathered, so a full
        // collection pays for at most one heap walk.
        for tag in 0..crate::stats::TYPE_TAG_COUNT {
            let row = &mut self.gc_stats.by_type[tag];
            row.live_bytes = cycle.totals.live_bytes[tag];
            row.live_count = cycle.totals.live_count[tag];
        }
        if let Some(before) = cycle.limits {
            self.recompute_allocation_limits(cycle.pause_start, cycle.pause_end, before);
        }
    }

    /// Apply every page that finished: run its dead cells' hooks, give its
    /// header the walk's result, then list its free ranges.
    fn apply_swept(&mut self, cycle: &mut SweepCycle) {
        for page in cycle.sweep.take_swept() {
            for &(header, tag) in &page.reclaim {
                let h = header as *mut GcHeader;
                // SAFETY: a dead cell of a page no allocation reached yet;
                // its payload is intact and its hooks run once.
                unsafe {
                    if let Some(release_fn) = self.trace_table.get_host_release(tag) {
                        release_fn(h, &mut self.host_refs);
                    }
                    if let Some(finalize_fn) = self.trace_table.get_finalize(tag) {
                        finalize_fn(h);
                    }
                    if let Some(drop_fn) = self.trace_table.get_drop(tag) {
                        drop_fn(h);
                    }
                }
            }
            cycle.applied += 1;
            if page.dead {
                self.old_space.apply_dead_page(page.base);
                cycle.dead.push(page.base);
                continue;
            }
            self.old_space
                .apply_swept_page(page.base, page.bump_cursor, page.allocated);
            match &mut cycle.deferred_ranges {
                Some(deferred) => deferred.extend(page.free_ranges),
                None => {
                    for (offset, bytes) in page.free_ranges {
                        self.old_space.push_free_range(offset, bytes);
                    }
                }
            }
        }
    }

    /// Whether a sweep cycle is in progress.
    #[must_use]
    pub fn is_sweeping(&self) -> bool {
        self.sweeping.is_some()
    }
}

#[cfg(test)]
#[path = "sweeping_tests.rs"]
mod tests;
