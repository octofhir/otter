//! Tri-color marking of the old generation: the stop-the-world drain and the
//! concurrent marker that runs on the heap's helper threads beside the
//! mutator.
//!
//! # Algorithm
//!
//! 1. Roots → shade each old target gray and enqueue it.
//! 2. Drain: pop a gray header, trace its slots, shade each white old child
//!    gray and enqueue it, paint the parent black. Repeat until empty.
//! 3. After the drain every reachable old object is black; whatever is still
//!    white is garbage.
//!
//! Only old and large cells are marked. Young cells never enter a worklist:
//! the collection that finishes a cycle scavenges first and then visits
//! every surviving young cell as a root of the old generation.
//!
//! A concurrent cycle runs the drain on the helper threads while the mutator
//! runs. The insertion barrier shades every old white child the mutator
//! stores; old cells allocated during the cycle are born black. A visit
//! claims its cell black before reading it, so a store no barrier saw finds
//! it black and re-grays it for the final pause. A body type that did not opt
//! in to concurrent visits ([`crate::Traceable::CONCURRENT_MARKING`]) is set
//! aside, still gray, for the final pause, which also re-scans the roots and
//! drains everything left.
//!
//! # Contents
//!
//! - [`MarkingState`] — the mutator's worklist, the `is_marking` flag the
//!   barrier checks, the stop-the-world drains and the concurrent cycle.
//!
//! # Invariants
//!
//! - `is_marking == true` only between [`MarkingState::start_cycle`] and
//!   [`MarkingState::finish_cycle`]; the insertion barrier is live then.
//! - A worklist holds only gray old or large headers. Shading is a
//!   compare-and-swap from white to gray, so exactly one thread enqueues and
//!   traces each object.
//! - Concurrent visits read slots with relaxed atomic loads and never write
//!   a body. A layout-locked type is visited under its cell's layout lock.
//! - Helpers run the concurrent drain only between
//!   [`MarkingState::start_concurrent`] and [`MarkingState::stop_concurrent`];
//!   no other job uses the helpers in between, and the mutator stops them
//!   before any collection.
//! - Each traced object's size is added to its page's live bytes atomically;
//!   the cycle's total is merged once a drain ends.
//!
//! # See also
//!
//! - [`crate::barrier`] — the insertion half of the write barrier.
//! - [`crate::heap::GcHeap`] — when cycles start and how they finish.

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};
use std::sync::{Condvar, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use crate::compressed::{RawGc, cage_base};
use crate::gc_threads::{GcThreads, WorkPool};
use crate::header::GcHeader;
use crate::header::MarkColor;
use crate::page::{Page, PageHeader, SpaceKind};
use crate::trace::{ConcurrentTraceFn, TraceTable};

/// Worklist length past which a marking thread hands half to idle threads.
const SHARE_THRESHOLD: usize = 128;
/// Mutator worklist length at which the barrier publishes it to the
/// concurrent marker.
const PUBLISH_THRESHOLD: usize = 512;
/// Objects a concurrent marker visits between two looks at its stop flag.
const STOP_POLL: usize = 256;
/// How long an idle concurrent marker waits before looking again.
const IDLE_WAIT: Duration = Duration::from_millis(1);

/// Worklist + book-keeping for a marking cycle.
pub struct MarkingState {
    worklist: Vec<*mut GcHeader>,
    /// Bytes covered by black objects discovered so far this
    /// cycle. The sweeper consults this for live-set accounting.
    pub live_bytes: usize,
    /// `true` while a marking cycle is active: the insertion barrier shades.
    is_marking: bool,
    /// The concurrent drain, while helpers run it.
    concurrent: Option<Box<Concurrent>>,
    /// Old cells the final pause visits: black ones the mutator changed
    /// without a barrier, and those born during the cycle whose type the
    /// concurrent marker does not visit.
    final_visits: Vec<*mut GcHeader>,
    /// The mutator runs inside an active cycle: it is neither in the pause
    /// that started it nor in the one finishing it.
    mutator_phase: bool,
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
            worklist: Vec::new(),
            live_bytes: 0,
            is_marking: false,
            concurrent: None,
            final_visits: Vec::new(),
            mutator_phase: false,
        }
    }

    /// Whether the mutator runs inside an active marking cycle.
    #[inline]
    #[must_use]
    pub fn mutator_phase(&self) -> bool {
        self.mutator_phase
    }

    /// Mark the mutator's part of the cycle begun or ended.
    pub(crate) fn set_mutator_phase(&mut self, running: bool) {
        self.mutator_phase = running;
    }

    /// After the mutator changed `header`'s body without a barrier: a black
    /// old cell is re-grayed for the final pause to visit again.
    ///
    /// # Safety
    ///
    /// `header` is a live cell's header.
    #[inline]
    pub unsafe fn regray_changed(&mut self, header: *mut GcHeader) {
        // Pairs with the fence after a concurrent visit claims a header.
        std::sync::atomic::fence(Ordering::SeqCst);
        // SAFETY: per docstring.
        unsafe {
            if !(*header).is_young() && (*header).try_regray() {
                self.final_visits.push(header);
            }
        }
    }

    /// Settle the color of an old cell just born while a cycle is active:
    /// black, counted live now, since no visit follows. Its allocator then
    /// barriers every edge it was initialized with, and later stores pass
    /// the barrier or re-gray it.
    ///
    /// # Safety
    ///
    /// `header` is a live old cell the caller just wrote.
    #[inline]
    pub(crate) unsafe fn record_old_birth(&mut self, header: *mut GcHeader) {
        if self.is_marking {
            // SAFETY: per docstring.
            unsafe {
                (*header).set_mark_color(MarkColor::Black);
                self.live_bytes += account_live(header);
            }
        }
    }

    /// [`Self::record_old_birth`] for a cell carved from the old buffer,
    /// whose page already counts the whole buffer live.
    ///
    /// # Safety
    ///
    /// As [`Self::record_old_birth`].
    #[inline]
    pub(crate) unsafe fn record_old_lab_birth(&mut self, header: *mut GcHeader) {
        if self.is_marking {
            // SAFETY: per docstring.
            unsafe { (*header).set_mark_color(MarkColor::Black) };
        }
    }

    /// Count a free extent about to serve old cells born black toward its
    /// page's live bytes.
    ///
    /// # Safety
    ///
    /// `header` is the free filler covering the extent, inside a live page.
    pub(crate) unsafe fn count_live_extent(&mut self, header: *mut GcHeader) {
        // SAFETY: per docstring.
        unsafe { account_live(header) };
    }

    /// Move the cells queued for the final pause onto the worklist.
    pub(crate) fn take_final_visits(&mut self) {
        self.worklist.append(&mut self.final_visits);
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

    /// Shade `header` gray and enqueue it when it is a white old cell.
    /// Young cells are left alone: the finishing collection visits them.
    ///
    /// # Safety
    ///
    /// `header` must point to a live, valid `GcHeader`.
    #[inline]
    pub unsafe fn shade_gray(&mut self, header: *mut GcHeader) {
        // SAFETY: caller guarantees liveness of header.
        unsafe {
            let h = &*header;
            if h.is_young() || !h.try_shade_gray() {
                return;
            }
        }
        self.worklist.push(header);
        if self.worklist.len() >= PUBLISH_THRESHOLD {
            self.publish();
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
            let header = cage_base().add(raw as usize) as *mut GcHeader;
            self.shade_gray(header);
        }
    }

    /// Enqueue a header the caller already painted gray.
    ///
    /// # Safety
    ///
    /// `header` is a live old cell that was just painted gray, by the caller.
    pub(crate) unsafe fn push_gray(&mut self, header: *mut GcHeader) {
        self.worklist.push(header);
    }

    /// Begin a marking cycle.
    pub fn start_cycle(&mut self) {
        debug_assert!(!self.is_marking);
        self.is_marking = true;
        self.live_bytes = 0;
        debug_assert!(self.worklist.is_empty());
    }

    /// End the marking cycle.
    pub fn finish_cycle(&mut self) {
        debug_assert!(self.concurrent.is_none());
        self.is_marking = false;
        self.mutator_phase = false;
        debug_assert!(self.worklist.is_empty() && self.final_visits.is_empty());
    }

    /// Drain the worklist completely under the supplied trace
    /// table. Used by the STW old-gen mark phase.
    ///
    /// # Safety
    ///
    /// The mutator is stopped and no concurrent drain runs; every header on
    /// the worklist is alive.
    pub unsafe fn drain_full(&mut self, trace_table: &TraceTable) {
        // SAFETY: caller-side invariant from STW pause.
        unsafe {
            while let Some(header) = self.worklist.pop() {
                self.live_bytes += mark_one(header, trace_table, &mut self.worklist);
            }
        }
    }

    /// Drain at most `budget` headers and return how many were
    /// processed.
    ///
    /// # Safety
    ///
    /// Same as [`Self::drain_full`].
    pub unsafe fn drain_with_budget(&mut self, budget: usize, trace_table: &TraceTable) -> usize {
        let mut done = 0;
        // SAFETY: see drain_full.
        unsafe {
            while done < budget {
                let Some(header) = self.worklist.pop() else {
                    break;
                };
                self.live_bytes += mark_one(header, trace_table, &mut self.worklist);
                done += 1;
            }
        }
        done
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
            pool: WorkPool::new(),
            live_bytes: AtomicUsize::new(0),
        };
        // SAFETY: `mark_worker` keeps workers apart through the pool and the
        // atomic shading; `job` outlives the call.
        unsafe { threads.run(mark_worker, (&raw const job).cast()) };
        self.live_bytes += job.live_bytes.into_inner();
    }

    /// Number of gray headers waiting on the mutator's worklist.
    pub fn pending(&self) -> usize {
        self.worklist.len()
    }

    /// Whether helpers are running the concurrent drain.
    #[must_use]
    pub fn is_concurrent(&self) -> bool {
        self.concurrent.is_some()
    }

    /// Start the concurrent drain of the current worklist on the helpers;
    /// `false` when there are none, and the caller then drains it in a pause.
    ///
    /// # Safety
    ///
    /// A marking cycle is active, the worklist holds gray old headers, and
    /// the caller stops the drain with [`Self::stop_concurrent`] before any
    /// collection, heap walk or drop of the heap.
    pub unsafe fn start_concurrent(
        &mut self,
        trace_table: &TraceTable,
        threads: &mut GcThreads,
    ) -> bool {
        debug_assert!(self.is_marking && self.concurrent.is_none());
        let concurrent = Box::new(Concurrent {
            entries: trace_table.concurrent_entries(),
            pool: Mutex::new(Vec::new()),
            wake: Condvar::new(),
            set_aside: Mutex::new(Vec::new()),
            stop: AtomicBool::new(false),
            idle: AtomicUsize::new(0),
            helpers: AtomicUsize::new(0),
            live_bytes: AtomicUsize::new(0),
        });
        let context = (&raw const *concurrent).cast::<()>();
        // SAFETY: the box stays alive in `self.concurrent` until
        // `stop_concurrent` joins every helper.
        let helpers = unsafe { threads.start_background(concurrent_worker, context) };
        if helpers == 0 {
            return false;
        }
        concurrent.helpers.store(helpers, Ordering::Relaxed);
        self.concurrent = Some(concurrent);
        self.publish();
        true
    }

    /// Hand the mutator's worklist to the concurrent drain.
    pub fn publish(&mut self) {
        let Some(concurrent) = &self.concurrent else {
            return;
        };
        if self.worklist.is_empty() {
            return;
        }
        let chunk: Vec<usize> = self.worklist.drain(..).map(|h| h as usize).collect();
        concurrent.lock_pool().push(chunk);
        concurrent.wake.notify_one();
    }

    /// Whether the concurrent drain ran out of work: every helper waits, and
    /// neither the shared pool nor the mutator's worklist holds anything.
    #[must_use]
    pub fn concurrent_done(&self) -> bool {
        self.concurrent.as_ref().is_some_and(|concurrent| {
            self.worklist.is_empty()
                && concurrent.idle.load(Ordering::Acquire)
                    == concurrent.helpers.load(Ordering::Relaxed)
                && concurrent.lock_pool().is_empty()
        })
    }

    /// Stop the concurrent drain and take back whatever it did not finish,
    /// the bodies it set aside included, onto the worklist.
    pub fn stop_concurrent(&mut self, threads: &mut GcThreads) {
        let Some(concurrent) = self.concurrent.take() else {
            return;
        };
        concurrent.stop.store(true, Ordering::Release);
        concurrent.wake.notify_all();
        threads.join_background();
        for chunk in concurrent.lock_pool().drain(..) {
            self.worklist
                .extend(chunk.into_iter().map(|h| h as *mut GcHeader));
        }
        self.worklist.extend(
            concurrent
                .set_aside
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .drain(..)
                .map(|h| h as *mut GcHeader),
        );
        self.live_bytes += concurrent.live_bytes.load(Ordering::Relaxed);
    }
}

impl Drop for MarkingState {
    fn drop(&mut self) {
        debug_assert!(
            self.concurrent.is_none(),
            "the heap stops its concurrent marker before dropping"
        );
    }
}

/// One concurrent drain, shared by the helpers running it and the mutator.
struct Concurrent {
    /// Each type's concurrent visit and whether it takes the layout lock,
    /// as registered when the drain started.
    entries: [Option<(ConcurrentTraceFn, bool)>; 256],
    /// Chunks of gray headers waiting for a helper.
    pool: Mutex<Vec<Vec<usize>>>,
    wake: Condvar,
    /// Gray bodies the final pause visits.
    set_aside: Mutex<Vec<usize>>,
    stop: AtomicBool,
    /// Helpers waiting for work.
    idle: AtomicUsize,
    helpers: AtomicUsize,
    live_bytes: AtomicUsize,
}

impl Concurrent {
    fn lock_pool(&self) -> MutexGuard<'_, Vec<Vec<usize>>> {
        self.pool.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn stopping(&self) -> bool {
        self.stop.load(Ordering::Acquire)
    }
}

/// One helper of a concurrent drain: visit gray headers until stopped,
/// waiting for the mutator to publish more while there are none.
///
/// # Safety
///
/// `context` is the live [`Concurrent`] of an active marking cycle.
unsafe fn concurrent_worker(context: *const (), _index: usize) {
    // SAFETY: per docstring.
    let job = unsafe { &*context.cast::<Concurrent>() };
    let mut local: Vec<usize> = Vec::new();
    let mut set_aside: Vec<usize> = Vec::new();
    let mut live = 0usize;
    'drain: loop {
        let mut visited = 0usize;
        while let Some(header) = local.pop() {
            // SAFETY: entries are gray headers of live old objects.
            live += unsafe {
                visit_concurrent(header as *mut GcHeader, job, &mut local, &mut set_aside)
            };
            if local.len() > SHARE_THRESHOLD && job.idle.load(Ordering::Relaxed) != 0 {
                let half = local.len() / 2;
                job.lock_pool().push(local.drain(..half).collect());
                job.wake.notify_one();
            }
            visited += 1;
            if visited.is_multiple_of(STOP_POLL) && job.stopping() {
                break 'drain;
            }
        }
        let mut pool = job.lock_pool();
        loop {
            if job.stopping() {
                break 'drain;
            }
            if let Some(chunk) = pool.pop() {
                local = chunk;
                break;
            }
            job.idle.fetch_add(1, Ordering::AcqRel);
            pool = job
                .wake
                .wait_timeout(pool, IDLE_WAIT)
                .unwrap_or_else(PoisonError::into_inner)
                .0;
            job.idle.fetch_sub(1, Ordering::AcqRel);
        }
    }
    if !local.is_empty() {
        job.lock_pool().push(local);
    }
    if !set_aside.is_empty() {
        job.set_aside
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .append(&mut set_aside);
    }
    job.live_bytes.fetch_add(live, Ordering::Relaxed);
}

/// Visit one gray header concurrently, or set it aside for the final pause
/// when its type did not opt in. Returns the bytes it marked live.
///
/// # Safety
///
/// `header` is a gray header of a live old object only this thread visits.
unsafe fn visit_concurrent(
    header: *mut GcHeader,
    job: &Concurrent,
    local: &mut Vec<usize>,
    set_aside: &mut Vec<usize>,
) -> usize {
    // SAFETY: per docstring.
    unsafe {
        let h = &*header;
        let Some((trace, locked)) = job.entries[usize::from(h.type_tag())] else {
            set_aside.push(header as usize);
            return 0;
        };
        let Some(first) = h.try_begin_visit() else {
            return 0;
        };
        // Pairs with the fence of a mutator change the barrier does not
        // see: either this visit reads the new word, or the mutator finds
        // the header black and re-grays it.
        std::sync::atomic::fence(Ordering::SeqCst);
        let base = cage_base();
        let mut visitor = |slot: *mut RawGc| {
            let raw = AtomicU32::from_ptr(slot.cast::<u32>()).load(Ordering::Relaxed);
            if raw == 0 {
                return;
            }
            let child = base.add(raw as usize) as *mut GcHeader;
            // The page, not the header, tells a young cell apart: a nursery
            // cell's header may still be in the mutator's hands.
            if !matches!(
                Page::space_of(child as *const u8),
                SpaceKind::NewFrom | SpaceKind::NewTo | SpaceKind::NewLarge
            ) && (*child).try_shade_gray()
            {
                local.push(child as usize);
            }
        };
        let visited = if locked {
            h.lock_layout();
            let visited = trace(header, &mut visitor);
            h.unlock_layout();
            visited
        } else {
            trace(header, &mut visitor)
        };
        if !visited {
            // Its layout is in flux: wait for the final pause, in the color
            // that pause expects.
            h.set_mark_color(if first {
                MarkColor::Gray
            } else {
                MarkColor::Regray
            });
            set_aside.push(header as usize);
            return 0;
        }
        if first { account_live(header) } else { 0 }
    }
}

/// Add `header`'s size to its page's live bytes and return it.
///
/// # Safety
///
/// `header` is a live object header.
unsafe fn account_live(header: *mut GcHeader) -> usize {
    // SAFETY: per docstring.
    unsafe {
        let size = (*header).size_bytes() as usize;
        let page = Page::page_base_of(header as *const u8).cast::<PageHeader>();
        AtomicUsize::from_ptr(std::ptr::addr_of_mut!((*page).live_bytes))
            .fetch_add(size, Ordering::Relaxed);
        size
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
    let mut local: Vec<*mut GcHeader> = if index == 0 {
        std::mem::take(&mut *job.initial.lock().unwrap_or_else(PoisonError::into_inner))
            .into_iter()
            .map(|h| h as *mut GcHeader)
            .collect()
    } else {
        job.pool.join();
        Vec::new()
    };
    let mut live = 0usize;
    loop {
        while let Some(header) = local.pop() {
            // SAFETY: entries are gray headers of live objects.
            live += unsafe { mark_one(header, &*job.trace_table, &mut local) };
            if local.len() > SHARE_THRESHOLD && job.pool.has_idle() {
                let half = local.len() / 2;
                job.pool
                    .share(local.drain(..half).map(|h| h as usize).collect());
            }
        }
        match job.pool.take(|| true) {
            Some(chunk) => local = chunk.into_iter().map(|h| h as *mut GcHeader).collect(),
            None => break,
        }
    }
    job.live_bytes.fetch_add(live, Ordering::Relaxed);
}

/// Trace one gray header with the mutator stopped, shade its white old
/// children onto `local`, paint it black and return its size.
///
/// # Safety
///
/// `header` is a gray header of a live object only this thread traces.
unsafe fn mark_one(
    header: *mut GcHeader,
    trace_table: &TraceTable,
    local: &mut Vec<*mut GcHeader>,
) -> usize {
    // SAFETY: per docstring.
    unsafe {
        let Some(first) = (*header).try_begin_visit() else {
            return 0;
        };
        let base = cage_base();
        let mut visitor = |slot: *mut RawGc| {
            let raw = (*slot).0;
            if raw == 0 {
                return;
            }
            let child = base.add(raw as usize) as *mut GcHeader;
            if !(*child).is_young() && (*child).try_shade_gray() {
                local.push(child);
            }
        };
        trace_table.trace(header, &mut visitor);
        if first { account_live(header) } else { 0 }
    }
}
