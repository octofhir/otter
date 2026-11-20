//! Compiler worker threads and their shared request and result queues.
//!
//! # Contents
//! - [`CompilerPool`] starts workers on demand, dispatches work and returns results.
//!
//! # Invariants
//! - An existing worker can drain the queue if an additional worker cannot start.
//! - Queued jobs release their resources outside the lock. Dropping the pool joins its workers.
//!
//! # See also
//! - [`super::OptimizingCompileQueue`] holds the isolate's pending owners.

use super::{CompileJob, FinishedCompile, cancel_queued_job, compile};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};

/// Stack reserved for each compiler thread: the same depth the isolate thread
/// gives a compilation that runs on it.
const COMPILER_THREAD_STACK_BYTES: usize = 16 * 1024 * 1024;

/// A compiler panic, carried to the isolate thread.
type CompilerPanic = Box<dyn std::any::Any + Send>;

#[derive(Default)]
struct PoolQueue {
    requests: VecDeque<CompileJob>,
    results: Vec<Result<FinishedCompile, CompilerPanic>>,
    stopping: bool,
}

struct PoolShared {
    queue: Mutex<PoolQueue>,
    wake: Condvar,
    /// Set while `queue.results` holds anything; read without the lock.
    finished: AtomicBool,
}

impl PoolShared {
    fn lock(&self) -> MutexGuard<'_, PoolQueue> {
        self.queue.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

pub(super) struct CompilerPool {
    shared: Arc<PoolShared>,
    threads: Vec<std::thread::JoinHandle<()>>,
    #[cfg(test)]
    pub(super) fail_next_spawn: bool,
}

impl CompilerPool {
    pub(super) fn new() -> Self {
        Self {
            shared: Arc::new(PoolShared {
                queue: Mutex::default(),
                wake: Condvar::new(),
                finished: AtomicBool::new(false),
            }),
            threads: Vec::new(),
            #[cfg(test)]
            fail_next_spawn: false,
        }
    }

    pub(super) fn ensure_worker(&mut self, desired: usize) -> bool {
        self.threads.len() >= desired || self.spawn_thread().is_ok() || !self.threads.is_empty()
    }

    pub(super) fn enqueue(&self, job: CompileJob) {
        self.shared.lock().requests.push_back(job);
        self.shared.wake.notify_one();
    }

    pub(super) fn has_finished(&self) -> bool {
        self.shared.finished.load(Ordering::Acquire)
    }

    pub(super) fn take_results(&self) -> Vec<Result<FinishedCompile, CompilerPanic>> {
        let mut queue = self.shared.lock();
        self.shared.finished.store(false, Ordering::Release);
        std::mem::take(&mut queue.results)
    }

    pub(super) fn acknowledge_queued_cancellations(&self) {
        let cancelled = {
            let mut queue = self.shared.lock();
            let requests = std::mem::take(&mut queue.requests);
            let mut cancelled = Vec::new();
            for job in requests {
                if job.cancellation.is_cancelled() {
                    cancelled.push(job);
                } else {
                    queue.requests.push_back(job);
                }
            }
            cancelled
        };
        if cancelled.is_empty() {
            return;
        }
        let finished = cancelled
            .into_iter()
            .map(cancel_queued_job)
            .map(Ok)
            .collect::<Vec<_>>();
        self.shared.lock().results.extend(finished);
        self.shared.finished.store(true, Ordering::Release);
    }

    /// Start one more compiler thread.
    fn spawn_thread(&mut self) -> std::io::Result<()> {
        #[cfg(test)]
        if std::mem::take(&mut self.fail_next_spawn) {
            return Err(std::io::Error::other(
                "injected compiler thread spawn failure",
            ));
        }
        let shared = Arc::clone(&self.shared);
        let thread = std::thread::Builder::new()
            .name(format!("otter-jit-compiler-{}", self.threads.len()))
            .stack_size(COMPILER_THREAD_STACK_BYTES)
            .spawn(move || compile_requests(&shared))?;
        self.threads.push(thread);
        Ok(())
    }
}

impl Drop for CompilerPool {
    fn drop(&mut self) {
        let requests = {
            let mut queue = self.shared.lock();
            queue.stopping = true;
            std::mem::take(&mut queue.requests)
        };
        // Snapshot and hook destructors must never run under the queue lock.
        drop(requests);
        self.shared.wake.notify_all();
        for thread in self.threads.drain(..) {
            let _ = thread.join();
        }
    }
}

/// One compiler thread: take requests until the pool stops.
fn compile_requests(shared: &PoolShared) {
    loop {
        let job = {
            let mut queue = shared.lock();
            loop {
                if queue.stopping {
                    return;
                }
                if let Some(job) = queue.requests.pop_front() {
                    break job;
                }
                queue = shared
                    .wake
                    .wait(queue)
                    .unwrap_or_else(PoisonError::into_inner);
            }
        };
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| compile(job)));
        shared.lock().results.push(result);
        shared.finished.store(true, Ordering::Release);
    }
}
