//! Tri-color marking worklist for STW old-gen collection.
//!
//! # Algorithm
//!
//! 1. Roots → enqueue each header onto the worklist, paint gray.
//! 2. Drain the worklist: pop a gray header, trace its slots,
//!    paint each unmarked child gray and enqueue it, paint the
//!    parent black. Repeat until empty.
//! 3. After drain, every reachable object is black; everything
//!    still white is garbage.
//!
//! A stop-the-world drain of a large heap runs on the heap's helper threads
//! together with the collecting thread: each keeps its own worklist, hands
//! half of a long one to idle threads, and shades a child by a
//! compare-and-swap from white to gray, so exactly one thread traces each
//! object.
//!
//! # Contents
//!
//! - [`MarkingState`] — the worklist + the `is_marking` flag the
//!   barrier checks.
//!
//! # Invariants
//!
//! - `is_marking == true` only between [`MarkingState::start_cycle`]
//!   and [`MarkingState::finish_cycle`]. The Phase-1 STW path
//!   never observes the flag set; Phase 2 lights the insertion
//!   barrier when this flag is true.
//! - The worklist contains only gray headers. Black and white
//!   never appear in the queue.
//! - A parallel drain adds each traced object's size to its page's live
//!   bytes atomically, and merges the cycle's live bytes once at the end.
//!   Trace callbacks are object-local; marking only reads slots.
//! - `drain_with_budget` is dormant in Phase 1 (the STW path uses
//!   `drain_full`); the API exists so Phase 2 / task 86 can
//!   adopt incremental marking without an audit sweep.
//!
//! # See also
//!
//! - GC architecture plan §2.3 (MarkingState row), §5 (barriers).

use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, PoisonError};

use crate::compressed::{RawGc, cage_base};
use crate::gc_threads::{GcThreads, WorkPool};
use crate::header::{GcHeader, MarkColor};
use crate::page::{Page, PageHeader};
use crate::trace::TraceTable;

/// Worklist length past which a marking thread hands half to idle threads.
const SHARE_THRESHOLD: usize = 128;

/// Worklist + book-keeping for a marking cycle.
pub struct MarkingState {
    worklist: VecDeque<*mut GcHeader>,
    /// Bytes covered by black objects discovered so far this
    /// cycle. The sweeper consults this for live-set accounting.
    pub live_bytes: usize,
    /// `true` while a marking cycle is active. Phase-1 keeps it
    /// false everywhere except inside `drain_full`; Phase 2 (task
    /// 86) flips it on at cycle start and off at cycle finish so
    /// the insertion barrier wakes up.
    is_marking: bool,
}

impl Default for MarkingState {
    fn default() -> Self {
        Self::new()
    }
}

impl MarkingState {
    /// Empty state.
    pub fn new() -> Self {
        Self {
            worklist: VecDeque::new(),
            live_bytes: 0,
            is_marking: false,
        }
    }

    /// True while a marking cycle is in progress.
    #[inline]
    pub fn is_marking(&self) -> bool {
        self.is_marking
    }

    /// Address of the marking flag byte.
    ///
    /// Compiled code tests the insertion barrier's precondition inline, so it
    /// needs the one byte rather than the state that owns it. The address is
    /// stable for the heap's lifetime.
    #[must_use]
    pub fn marking_flag_addr(&self) -> *const u8 {
        std::ptr::from_ref(&self.is_marking).cast::<u8>()
    }

    /// Push `header` onto the worklist if it is currently white.
    /// Idempotent — re-marking the same header is a no-op.
    ///
    /// # Safety
    ///
    /// `header` must point to a live, valid `GcHeader`.
    pub unsafe fn shade_gray(&mut self, header: *mut GcHeader) {
        // SAFETY: caller guarantees liveness of header.
        unsafe {
            let h = &*header;
            if matches!(h.mark_color(), MarkColor::White) {
                h.set_mark_color(MarkColor::Gray);
                self.worklist.push_back(header);
            }
        }
    }

    /// Mark the header reached from a [`RawGc`] slot, no-op if
    /// the slot is null.
    ///
    /// # Safety
    ///
    /// `slot` must address a valid initialised `RawGc` pointing
    /// inside the cage (or null).
    pub unsafe fn shade_from_slot(&mut self, slot: *mut RawGc) {
        // SAFETY: by precondition slot is dereferenceable and
        // (if non-null) points at a live cage object.
        unsafe {
            let raw = (*slot).0;
            if raw == 0 {
                return;
            }
            // SAFETY: raw is a valid in-cage offset by trace
            // table contract.
            let header = cage_base().add(raw as usize) as *mut GcHeader;
            self.shade_gray(header);
        }
    }

    /// Begin a marking cycle (Phase 2 hook).
    pub fn start_cycle(&mut self) {
        debug_assert!(!self.is_marking);
        self.is_marking = true;
        self.live_bytes = 0;
        debug_assert!(self.worklist.is_empty());
    }

    /// End the marking cycle.
    pub fn finish_cycle(&mut self) {
        self.is_marking = false;
        debug_assert!(self.worklist.is_empty());
    }

    /// Drain the worklist completely under the supplied trace
    /// table. Used by the STW old-gen mark phase.
    ///
    /// # Safety
    ///
    /// Every header pushed via `shade_gray` must outlive the
    /// drain (Phase-1 STW guarantees this — the mutator is
    /// paused).
    pub unsafe fn drain_full(&mut self, trace_table: &TraceTable) {
        // SAFETY: caller-side invariant from STW pause.
        unsafe {
            while let Some(header) = self.worklist.pop_front() {
                self.process_one(header, trace_table);
            }
        }
    }

    /// Drain at most `budget` headers and return how many were
    /// processed. Phase-1 caller never invokes this — it exists
    /// so the Phase-2 incremental driver (task 86) can reuse the
    /// same marking surface.
    ///
    /// # Safety
    ///
    /// Same as [`drain_full`].
    pub unsafe fn drain_with_budget(&mut self, budget: usize, trace_table: &TraceTable) -> usize {
        let mut done = 0;
        // SAFETY: see drain_full.
        unsafe {
            while done < budget {
                let Some(header) = self.worklist.pop_front() else {
                    break;
                };
                self.process_one(header, trace_table);
                done += 1;
            }
        }
        done
    }

    /// Process one gray header → traced + painted black.
    ///
    /// # Safety
    ///
    /// `header` must be valid and was placed on the worklist
    /// while gray.
    unsafe fn process_one(&mut self, header: *mut GcHeader, trace_table: &TraceTable) {
        // SAFETY: header is alive (worklist invariant).
        unsafe {
            let h = &*header;
            // Account for live bytes BEFORE we paint black so a
            // re-entry trace can read the size off the header.
            self.live_bytes += h.size_bytes() as usize;
            // Mark the owning page's live-bytes counter so the
            // sweeper knows the page is non-empty.
            let page_header = Page::header_of_mut(header as *const u8);
            page_header.live_bytes += h.size_bytes() as usize;
            // Trace children.
            let mut visitor = |slot: *mut RawGc| {
                let raw = (*slot).0;
                if raw == 0 {
                    return;
                }
                // SAFETY: raw is a valid in-cage offset.
                let child = cage_base().add(raw as usize) as *mut GcHeader;
                let child_h = &*child;
                if matches!(child_h.mark_color(), MarkColor::White) {
                    child_h.set_mark_color(MarkColor::Gray);
                    self.worklist.push_back(child);
                }
            };
            trace_table.trace(header, &mut visitor);
            h.set_mark_color(MarkColor::Black);
        }
    }

    /// Drain the worklist completely on the heap's helper threads and the
    /// calling thread. Used by stop-the-world mark phases on large heaps.
    ///
    /// # Safety
    ///
    /// Same as [`Self::drain_full`]; the trace table's callbacks must be
    /// object-local.
    pub unsafe fn drain_parallel(&mut self, trace_table: &TraceTable, threads: &mut GcThreads) {
        let workers = threads.workers();
        if workers <= 1 {
            // SAFETY: forwarded from the caller.
            unsafe { self.drain_full(trace_table) };
            return;
        }
        let job = ParallelMark {
            trace_table,
            initial: Mutex::new(self.worklist.drain(..).map(|h| h as usize).collect()),
            pool: WorkPool::new(workers),
            live_bytes: AtomicUsize::new(0),
        };
        // SAFETY: `mark_worker` keeps workers apart through the pool and the
        // atomic shading; `job` outlives the call.
        unsafe { threads.run(mark_worker, (&raw const job).cast()) };
        self.live_bytes += job.live_bytes.into_inner();
    }

    /// Number of gray headers waiting on the worklist.
    pub fn pending(&self) -> usize {
        self.worklist.len()
    }
}

/// One parallel drain.
struct ParallelMark {
    trace_table: *const TraceTable,
    /// The collecting thread's worklist, taken by worker 0.
    initial: Mutex<Vec<usize>>,
    pool: WorkPool<usize>,
    live_bytes: AtomicUsize,
}

// SAFETY: the trace table is only read; everything else is locked or atomic.
unsafe impl Sync for ParallelMark {}

/// One worker of a parallel drain.
///
/// # Safety
///
/// `context` is a live [`ParallelMark`] whose worklist holds gray headers of
/// live objects; the mutator is stopped.
unsafe fn mark_worker(context: *const (), index: usize) {
    // SAFETY: per docstring.
    let job = unsafe { &*context.cast::<ParallelMark>() };
    let mut local: Vec<usize> = if index == 0 {
        std::mem::take(&mut *job.initial.lock().unwrap_or_else(PoisonError::into_inner))
    } else {
        Vec::new()
    };
    let mut live = 0usize;
    loop {
        while let Some(header) = local.pop() {
            // SAFETY: entries are gray headers of live objects.
            live += unsafe { mark_one(header as *mut GcHeader, &*job.trace_table, &mut local) };
            if local.len() > SHARE_THRESHOLD && job.pool.has_idle() {
                let half = local.len() / 2;
                job.pool.share(local.drain(..half).collect());
            }
        }
        match job.pool.take(|| true) {
            Some(chunk) => local = chunk,
            None => break,
        }
    }
    job.live_bytes.fetch_add(live, Ordering::Relaxed);
}

/// Trace one gray header, shade its white children onto `local`, paint it
/// black and return its size.
///
/// # Safety
///
/// `header` is a gray header of a live object only this thread traces.
unsafe fn mark_one(
    header: *mut GcHeader,
    trace_table: &TraceTable,
    local: &mut Vec<usize>,
) -> usize {
    // SAFETY: per docstring.
    unsafe {
        let h = &*header;
        let size = h.size_bytes() as usize;
        let page = Page::page_base_of(header as *const u8).cast::<PageHeader>();
        AtomicUsize::from_ptr(std::ptr::addr_of_mut!((*page).live_bytes))
            .fetch_add(size, Ordering::Relaxed);
        let base = cage_base();
        let mut visitor = |slot: *mut RawGc| {
            let raw = (*slot).0;
            if raw == 0 {
                return;
            }
            let child = base.add(raw as usize) as *mut GcHeader;
            if (*child).try_shade_gray() {
                local.push(child as usize);
            }
        };
        trace_table.trace(header, &mut visitor);
        h.set_mark_color(MarkColor::Black);
        size
    }
}
