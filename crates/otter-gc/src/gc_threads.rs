//! Helper threads one heap runs collection work on.
//!
//! # Contents
//! - [`GcThreads`] — the heap's helper threads and [`GcThreads::run`], which
//!   runs one job on every helper and on the calling thread at once.
//!
//! # Invariants
//! - The threads belong to one heap: they start at its first parallel job and
//!   are stopped and joined when it drops. No state is shared between heaps.
//! - A job runs only while the collecting thread is inside
//!   [`GcThreads::run`]; `run` returns after every helper finished it, so a
//!   job may borrow the caller's stack.
//! - A panic on any thread inside a job aborts the process: the heap is
//!   partially collected and cannot be resumed or unwound safely.
//!
//! # See also
//! - [`crate::scavenger`] — the parallel copying collection that uses them.

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
        let helpers = self.threads.len();
        if helpers != 0 {
            let mut state = self.shared.lock();
            state.generation += 1;
            state.job = Some(Job { work, context });
            state.running = helpers;
            drop(state);
            self.shared.start.notify_all();
        }
        abort_on_panic(|| {
            // SAFETY: forwarded from the caller.
            unsafe { work(context, 0) }
        });
        if helpers != 0 {
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
