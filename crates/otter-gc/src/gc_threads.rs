//! Helper threads one heap runs collection work on.
//!
//! # Contents
//! - [`GcThreads`] — the heap's helper threads and [`GcThreads::run`], which
//!   runs one job on every helper and on the calling thread at once, or
//!   [`GcThreads::start_background`] / [`GcThreads::join_background`], which
//!   run one job on the helpers alone while the calling thread continues.
//! - [`WorkPool`] — chunks of work the workers of one job hand each other,
//!   and the termination test of a job that ends when all of them run dry.
//!
//! # Invariants
//! - The threads belong to one heap: they start at its first parallel job and
//!   are stopped and joined when it drops. No state is shared between heaps.
//! - A job runs only while the collecting thread is inside
//!   [`GcThreads::run`]; `run` returns after every helper finished it, so a
//!   job may borrow the caller's stack. A background job runs from
//!   `start_background` until `join_background` returns; its context must
//!   outlive that, and no other job starts in between.
//! - A panic on any thread inside a job aborts the process: the heap is
//!   partially collected and cannot be resumed or unwound safely.
//! - [`WorkPool::take`] reports the end of a job only when every worker is
//!   waiting in it with nothing shared and no other source of work left, so
//!   no worker can still produce a chunk.
//!
//! # See also
//! - [`crate::scavenger`] — the parallel copying collection that uses them.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};

/// Stack reserved for each helper: collection work is iterative.
const HELPER_STACK_BYTES: usize = 512 * 1024;

/// Most helpers a heap starts; the collecting thread is one more worker.
const MAX_HELPERS: usize = 3;

/// One job: a function every worker runs with its index and the job's context.
#[derive(Clone, Copy)]
struct Job {
    work: unsafe fn(*const (), usize),
    context: *const (),
}

// SAFETY: a job's context outlives every call made through it, because
// `GcThreads::run` returns only after all helpers have finished the job.
unsafe impl Send for Job {}

#[derive(Default)]
struct State {
    /// Incremented once per job.
    generation: u64,
    job: Option<Job>,
    /// Helpers still running the current job.
    running: usize,
    stopping: bool,
}

#[derive(Default)]
struct Shared {
    state: Mutex<State>,
    start: Condvar,
    done: Condvar,
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// The heap's collection helper threads.
#[derive(Default)]
pub struct GcThreads {
    shared: Arc<Shared>,
    threads: Vec<std::thread::JoinHandle<()>>,
    started: bool,
    /// A background job runs on the helpers.
    background: bool,
}

impl GcThreads {
    /// Workers a job runs on: the helpers plus the calling thread. Starts the
    /// helpers on first use.
    pub(crate) fn workers(&mut self) -> usize {
        if !self.started {
            self.started = true;
            let helpers = std::thread::available_parallelism()
                .map_or(0, |cores| cores.get().saturating_sub(1))
                .min(MAX_HELPERS);
            for index in 1..=helpers {
                let shared = Arc::clone(&self.shared);
                if let Ok(thread) = std::thread::Builder::new()
                    .name(format!("otter-gc-{index}"))
                    .stack_size(HELPER_STACK_BYTES)
                    .spawn(move || help(&shared, index))
                {
                    self.threads.push(thread);
                }
            }
        }
        self.threads.len() + 1
    }

    /// Run `work(context, index)` on every worker, the calling thread as
    /// index 0, and return when all of them have finished.
    ///
    /// # Safety
    /// `work` must be safe to run concurrently with itself for distinct
    /// indices below [`Self::workers`] over the same `context`, which must
    /// stay valid until this returns.
    pub(crate) unsafe fn run(&mut self, work: unsafe fn(*const (), usize), context: *const ()) {
        debug_assert!(!self.background, "a background job holds the helpers");
        let helpers = self.threads.len();
        if helpers != 0 {
            self.post(Job { work, context }, helpers);
        }
        abort_on_panic(|| {
            // SAFETY: forwarded from the caller.
            unsafe { work(context, 0) }
        });
        if helpers != 0 {
            self.wait_helpers();
        }
    }

    /// Run `work(context, index)` on every helper, indices from 1, and return
    /// at once; the number of helpers running it, zero when there are none.
    ///
    /// # Safety
    /// As [`Self::run`] for the helpers' indices; `context` must stay valid
    /// until [`Self::join_background`] returns, and `work` must return once
    /// the caller asks it to through `context`.
    pub(crate) unsafe fn start_background(
        &mut self,
        work: unsafe fn(*const (), usize),
        context: *const (),
    ) -> usize {
        debug_assert!(!self.background, "one background job at a time");
        self.workers();
        let helpers = self.threads.len();
        if helpers != 0 {
            self.post(Job { work, context }, helpers);
            self.background = true;
        }
        helpers
    }

    /// Wait until every helper returned from the background job.
    pub(crate) fn join_background(&mut self) {
        if std::mem::take(&mut self.background) {
            self.wait_helpers();
        }
    }

    fn post(&self, job: Job, helpers: usize) {
        let mut state = self.shared.lock();
        state.generation += 1;
        state.job = Some(job);
        state.running = helpers;
        drop(state);
        self.shared.start.notify_all();
    }

    fn wait_helpers(&self) {
        let mut state = self.shared.lock();
        while state.running != 0 {
            state = self
                .shared
                .done
                .wait(state)
                .unwrap_or_else(PoisonError::into_inner);
        }
        state.job = None;
    }
}

impl Drop for GcThreads {
    fn drop(&mut self) {
        self.shared.lock().stopping = true;
        self.shared.start.notify_all();
        for thread in self.threads.drain(..) {
            let _ = thread.join();
        }
    }
}

/// One helper: run each new job once until the heap stops.
fn help(shared: &Shared, index: usize) {
    let mut seen = 0;
    loop {
        let job = {
            let mut state = shared.lock();
            loop {
                if state.stopping {
                    return;
                }
                if state.generation != seen
                    && let Some(job) = state.job
                {
                    seen = state.generation;
                    break job;
                }
                state = shared
                    .start
                    .wait(state)
                    .unwrap_or_else(PoisonError::into_inner);
            }
        };
        abort_on_panic(|| {
            // SAFETY: `GcThreads::run` keeps the context alive until this
            // helper reports completion below.
            unsafe { (job.work)(job.context, index) }
        });
        let mut state = shared.lock();
        state.running -= 1;
        if state.running == 0 {
            shared.done.notify_all();
        }
    }
}

fn abort_on_panic(work: impl FnOnce()) {
    if std::panic::catch_unwind(std::panic::AssertUnwindSafe(work)).is_err() {
        std::process::abort();
    }
}

/// Chunks of work the workers of one parallel job hand each other.
pub(crate) struct WorkPool<T> {
    state: Mutex<PoolState<T>>,
    wake: Condvar,
    /// Workers waiting in [`Self::take`]; read without the lock as a hint.
    idle: AtomicUsize,
    workers: usize,
}

struct PoolState<T> {
    chunks: Vec<Vec<T>>,
    idle: usize,
    done: bool,
}

impl<T> WorkPool<T> {
    /// A pool shared by `workers` workers.
    pub(crate) fn new(workers: usize) -> Self {
        Self {
            state: Mutex::new(PoolState {
                chunks: Vec::new(),
                idle: 0,
                done: false,
            }),
            wake: Condvar::new(),
            idle: AtomicUsize::new(0),
            workers,
        }
    }

    fn lock(&self) -> MutexGuard<'_, PoolState<T>> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Whether some worker waits for work.
    #[inline]
    pub(crate) fn has_idle(&self) -> bool {
        self.idle.load(Ordering::Relaxed) != 0
    }

    /// Hand `chunk` to a waiting worker.
    pub(crate) fn share(&self, chunk: Vec<T>) {
        self.lock().chunks.push(chunk);
        self.wake.notify_one();
    }

    /// Take a shared chunk, waiting while another worker may still share
    /// one. `None` once every worker waits here with nothing shared and
    /// `exhausted` reports no other source of work.
    pub(crate) fn take(&self, exhausted: impl Fn() -> bool) -> Option<Vec<T>> {
        let mut state = self.lock();
        loop {
            if let Some(chunk) = state.chunks.pop() {
                return Some(chunk);
            }
            if state.done {
                return None;
            }
            if state.idle + 1 == self.workers && exhausted() {
                state.done = true;
                self.wake.notify_all();
                return None;
            }
            state.idle += 1;
            self.idle.store(state.idle, Ordering::Relaxed);
            state = self
                .wake
                .wait(state)
                .unwrap_or_else(PoisonError::into_inner);
            state.idle -= 1;
            self.idle.store(state.idle, Ordering::Relaxed);
        }
    }
}
