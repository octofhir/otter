//! VM-internal microtask queue — ES2024 §9.4 Jobs and Job Queues.
//!
//! The microtask queue is per-isolate (owned by [`RuntimeState`]) and drained
//! at every spec-defined checkpoint:
//!
//! - After top-level script/module evaluation
//! - After each macrotask (timer callback, I/O completion)
//! - After each `await` resume
//!
//! # Queue priority (matching Node.js/Bun)
//!
//! 1. **nextTick** — `process.nextTick()` callbacks (highest priority)
//! 2. **promise_jobs** — Promise reaction callbacks (`HostEnqueuePromiseJob`)
//! 3. **microtasks** — `queueMicrotask()` callbacks
//!
//! All queues drain completely in each checkpoint. If draining enqueues new
//! jobs (e.g., a `.then()` handler enqueues another `.then()`), they are
//! processed in the same drain cycle — this matches the ES spec requirement
//! that all promise jobs run before control returns to the event loop.

use std::collections::VecDeque;

use crate::object::ObjectHandle;
use crate::value::RegisterValue;

/// A promise reaction job enqueued via `HostEnqueuePromiseJob`.
///
/// Contains the callback handle, the value to pass, and the result promise
/// that should be resolved/rejected with the callback's return value.
#[derive(Debug, Clone)]
pub struct PromiseJob {
    /// The JS callback function to invoke.
    pub callback: ObjectHandle,
    /// The `this` binding for the callback (usually undefined for promise reactions).
    pub this_value: RegisterValue,
    /// The argument to pass to the callback (the settled value).
    pub argument: RegisterValue,
    /// The promise whose resolution depends on this job's result.
    /// `None` for terminal `.then()` handlers with no downstream chain.
    pub result_promise: Option<ObjectHandle>,
    /// Whether this is a fulfill or reject reaction.
    pub kind: PromiseJobKind,
}

/// Whether the promise job is a fulfill or reject handler.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PromiseJobKind {
    Fulfill,
    Reject,
}

/// A generic microtask (from `queueMicrotask()` or `process.nextTick()`).
#[derive(Debug, Clone)]
pub struct MicrotaskJob {
    /// The JS callback function to invoke.
    pub callback: ObjectHandle,
    /// The `this` binding.
    pub this_value: RegisterValue,
    /// Arguments to pass (usually none for `queueMicrotask`, may have args for `nextTick`).
    pub args: Vec<RegisterValue>,
}

/// VM-internal microtask queue with three priority levels.
///
/// Drain order per checkpoint: ALL nextTick → ALL promise_jobs → ALL microtasks.
/// Repeat until all three queues are empty (handles cascading enqueues).
pub struct MicrotaskQueue {
    /// `process.nextTick()` callbacks — highest priority.
    next_tick: VecDeque<MicrotaskJob>,
    /// Promise reaction callbacks — `HostEnqueuePromiseJob`.
    promise_jobs: VecDeque<PromiseJob>,
    /// `queueMicrotask()` callbacks — lowest microtask priority.
    microtasks: VecDeque<MicrotaskJob>,
}

impl MicrotaskQueue {
    /// Creates an empty microtask queue.
    pub fn new() -> Self {
        Self {
            next_tick: VecDeque::new(),
            promise_jobs: VecDeque::new(),
            microtasks: VecDeque::new(),
        }
    }

    /// Enqueues a promise reaction job (from `HostEnqueuePromiseJob`).
    pub fn enqueue_promise_job(&mut self, job: PromiseJob) {
        self.promise_jobs.push_back(job);
    }

    /// Enqueues a `process.nextTick()` callback.
    pub fn enqueue_next_tick(&mut self, job: MicrotaskJob) {
        self.next_tick.push_back(job);
    }

    /// Enqueues a `queueMicrotask()` callback.
    pub fn enqueue_microtask(&mut self, job: MicrotaskJob) {
        self.microtasks.push_back(job);
    }

    /// Whether all queues are empty.
    pub fn is_empty(&self) -> bool {
        self.next_tick.is_empty()
            && self.promise_jobs.is_empty()
            && self.microtasks.is_empty()
    }

    /// Total number of pending jobs across all queues.
    pub fn len(&self) -> usize {
        self.next_tick.len() + self.promise_jobs.len() + self.microtasks.len()
    }

    /// Takes the next nextTick job, if any.
    pub fn pop_next_tick(&mut self) -> Option<MicrotaskJob> {
        self.next_tick.pop_front()
    }

    /// Takes the next promise job, if any.
    pub fn pop_promise_job(&mut self) -> Option<PromiseJob> {
        self.promise_jobs.pop_front()
    }

    /// Takes the next queueMicrotask job, if any.
    pub fn pop_microtask(&mut self) -> Option<MicrotaskJob> {
        self.microtasks.pop_front()
    }

    /// Number of pending nextTick jobs.
    pub fn next_tick_count(&self) -> usize {
        self.next_tick.len()
    }

    /// Number of pending promise jobs.
    pub fn promise_job_count(&self) -> usize {
        self.promise_jobs.len()
    }

    /// Number of pending queueMicrotask jobs.
    pub fn microtask_count(&self) -> usize {
        self.microtasks.len()
    }

    /// Clears all queues. Used during teardown or error recovery.
    pub fn clear(&mut self) {
        self.next_tick.clear();
        self.promise_jobs.clear();
        self.microtasks.clear();
    }
}

impl Default for MicrotaskQueue {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::object::ObjectHandle;
    use crate::value::RegisterValue;

    fn make_promise_job(handle_id: u32) -> PromiseJob {
        PromiseJob {
            callback: ObjectHandle(handle_id),
            this_value: RegisterValue::undefined(),
            argument: RegisterValue::from_i32(handle_id as i32),
            result_promise: None,
            kind: PromiseJobKind::Fulfill,
        }
    }

    fn make_microtask(handle_id: u32) -> MicrotaskJob {
        MicrotaskJob {
            callback: ObjectHandle(handle_id),
            this_value: RegisterValue::undefined(),
            args: vec![],
        }
    }

    #[test]
    fn empty_queue() {
        let queue = MicrotaskQueue::new();
        assert!(queue.is_empty());
        assert_eq!(queue.len(), 0);
    }

    #[test]
    fn enqueue_and_dequeue_promise_jobs() {
        let mut queue = MicrotaskQueue::new();
        queue.enqueue_promise_job(make_promise_job(1));
        queue.enqueue_promise_job(make_promise_job(2));

        assert_eq!(queue.promise_job_count(), 2);
        assert!(!queue.is_empty());

        let j1 = queue.pop_promise_job().unwrap();
        assert_eq!(j1.callback, ObjectHandle(1));
        let j2 = queue.pop_promise_job().unwrap();
        assert_eq!(j2.callback, ObjectHandle(2));
        assert!(queue.pop_promise_job().is_none());
    }

    #[test]
    fn enqueue_and_dequeue_next_tick() {
        let mut queue = MicrotaskQueue::new();
        queue.enqueue_next_tick(make_microtask(10));

        assert_eq!(queue.next_tick_count(), 1);
        let job = queue.pop_next_tick().unwrap();
        assert_eq!(job.callback, ObjectHandle(10));
    }

    #[test]
    fn enqueue_and_dequeue_microtasks() {
        let mut queue = MicrotaskQueue::new();
        queue.enqueue_microtask(make_microtask(20));

        assert_eq!(queue.microtask_count(), 1);
        let job = queue.pop_microtask().unwrap();
        assert_eq!(job.callback, ObjectHandle(20));
    }

    #[test]
    fn len_counts_all_queues() {
        let mut queue = MicrotaskQueue::new();
        queue.enqueue_next_tick(make_microtask(1));
        queue.enqueue_promise_job(make_promise_job(2));
        queue.enqueue_microtask(make_microtask(3));

        assert_eq!(queue.len(), 3);
    }

    #[test]
    fn clear_empties_all() {
        let mut queue = MicrotaskQueue::new();
        queue.enqueue_next_tick(make_microtask(1));
        queue.enqueue_promise_job(make_promise_job(2));
        queue.enqueue_microtask(make_microtask(3));

        queue.clear();
        assert!(queue.is_empty());
        assert_eq!(queue.len(), 0);
    }

    #[test]
    fn fifo_ordering_within_each_queue() {
        let mut queue = MicrotaskQueue::new();
        queue.enqueue_promise_job(make_promise_job(1));
        queue.enqueue_promise_job(make_promise_job(2));
        queue.enqueue_promise_job(make_promise_job(3));

        assert_eq!(queue.pop_promise_job().unwrap().callback, ObjectHandle(1));
        assert_eq!(queue.pop_promise_job().unwrap().callback, ObjectHandle(2));
        assert_eq!(queue.pop_promise_job().unwrap().callback, ObjectHandle(3));
    }
}
