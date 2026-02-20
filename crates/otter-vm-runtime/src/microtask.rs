//! Microtask queue for ECMAScript Promise callbacks.
//!
//! ## Drain Points (ES2026 Spec Compliance)
//!
//! Microtasks MUST be drained at the following synchronization points:
//!
//! 1. **After synchronous script execution** (`eval_sync`, `eval_in_context`)
//! 2. **After module evaluation** (if not suspended for top-level await)
//! 3. **After each timer callback** (setTimeout, setInterval)
//! 4. **After each immediate callback** (setImmediate)
//! 5. **After each HTTP/WebSocket event handler**
//! 6. **Before event loop timer phase** (highest priority)
//!
//! ## Ordering Guarantees
//!
//! - FIFO: First queued, first executed
//! - All pending microtasks drained until queue is empty
//! - New microtasks enqueued during drain are also executed
//!
//! ## Error Handling
//!
//! - Errors in microtasks are captured and the first error is returned
//! - Remaining microtasks continue to execute even after an error
//! - Only the first error is returned to the caller

use parking_lot::Mutex;
use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use otter_vm_core::promise::JsPromiseJob;
use otter_vm_core::value::Value;

/// Microtask callback type (Rust closures)
pub type Microtask = Box<dyn FnOnce() + Send>;

/// Shared sequencer for microtask ordering across queues
#[derive(Clone, Default)]
pub struct MicrotaskSequencer {
    counter: Arc<AtomicU64>,
}

impl MicrotaskSequencer {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn next(&self) -> u64 {
        self.counter.fetch_add(1, Ordering::Relaxed)
    }
}

/// A job that calls a JavaScript function
///
/// This represents a promise callback that needs to be executed by the interpreter.
/// Unlike Rust microtasks, these require VM context to execute.
#[derive(Clone)]
pub struct JsCallbackJob {
    /// Arguments to pass to the function
    pub args: Vec<Value>,
    /// Promise reaction job metadata
    pub job: JsPromiseJob,
}

/// Queue of microtasks (Rust closures)
pub struct MicrotaskQueue {
    queue: Mutex<VecDeque<(u64, Microtask)>>,
    len: AtomicUsize,
    sequencer: MicrotaskSequencer,
}

/// Queue of JS callback jobs (JavaScript functions)
///
/// This is separate from the Rust microtask queue because JS callbacks
/// need to be executed by the interpreter, which requires VM context.
pub struct JsJobQueue {
    queue: Mutex<VecDeque<(u64, JsCallbackJob)>>,
    len: AtomicUsize,
    sequencer: MicrotaskSequencer,
}

impl MicrotaskQueue {
    /// Create new empty queue
    pub fn new() -> Self {
        Self::with_sequencer(MicrotaskSequencer::new())
    }

    /// Create new queue with a shared sequencer
    pub fn with_sequencer(sequencer: MicrotaskSequencer) -> Self {
        Self {
            queue: Mutex::new(VecDeque::new()),
            len: AtomicUsize::new(0),
            sequencer,
        }
    }

    /// Add a microtask to the queue
    pub fn enqueue<F>(&self, task: F)
    where
        F: FnOnce() + Send + 'static,
    {
        let seq = self.sequencer.next();
        self.queue.lock().push_back((seq, Box::new(task)));
        self.len.fetch_add(1, Ordering::Relaxed);
    }

    /// Take the next microtask
    pub fn dequeue(&self) -> Option<Microtask> {
        let task = self.queue.lock().pop_front().map(|(_, task)| task);
        if task.is_some() {
            self.len.fetch_sub(1, Ordering::Relaxed);
        }
        task
    }

    /// Drain all currently queued microtasks in FIFO order.
    pub fn drain_all(&self, out: &mut Vec<Microtask>) -> usize {
        let mut queue = self.queue.lock();
        let drained = queue.len();
        out.reserve(drained);
        while let Some((_, task)) = queue.pop_front() {
            out.push(task);
        }
        if drained != 0 {
            self.len.fetch_sub(drained, Ordering::Relaxed);
        }
        drained
    }

    /// Peek the next microtask sequence number
    pub fn peek_seq(&self) -> Option<u64> {
        self.queue.lock().front().map(|(seq, _)| *seq)
    }

    /// Check if queue is empty
    pub fn is_empty(&self) -> bool {
        self.len.load(Ordering::Relaxed) == 0
    }

    /// Clear all pending microtasks
    pub fn clear(&self) {
        let mut queue = self.queue.lock();
        let len = queue.len();
        queue.clear();
        self.len.fetch_sub(len, Ordering::Relaxed);
    }
}

impl Default for MicrotaskQueue {
    fn default() -> Self {
        Self::new()
    }
}

impl JsJobQueue {
    /// Create new empty JS job queue
    pub fn new() -> Self {
        Self::with_sequencer(MicrotaskSequencer::new())
    }

    /// Create new JS job queue with a shared sequencer
    pub fn with_sequencer(sequencer: MicrotaskSequencer) -> Self {
        Self {
            queue: Mutex::new(VecDeque::new()),
            len: AtomicUsize::new(0),
            sequencer,
        }
    }

    /// Enqueue a JS callback job
    pub fn enqueue(&self, job: JsPromiseJob, args: Vec<Value>) {
        let seq = self.sequencer.next();
        self.queue
            .lock()
            .push_back((seq, JsCallbackJob { job, args }));
        self.len.fetch_add(1, Ordering::Relaxed);
    }

    /// Dequeue the next JS callback job
    pub fn dequeue(&self) -> Option<JsCallbackJob> {
        let job = self.queue.lock().pop_front().map(|(_, job)| job);
        if job.is_some() {
            self.len.fetch_sub(1, Ordering::Relaxed);
        }
        job
    }

    /// Dequeue up to `max` jobs in FIFO order in a single lock acquisition.
    pub fn dequeue_batch(&self, max: usize, out: &mut Vec<JsCallbackJob>) -> usize {
        if max == 0 {
            return 0;
        }
        let mut queue = self.queue.lock();
        let to_take = max.min(queue.len());
        out.reserve(to_take);
        for _ in 0..to_take {
            if let Some((_, job)) = queue.pop_front() {
                out.push(job);
            } else {
                break;
            }
        }
        if to_take != 0 {
            self.len.fetch_sub(to_take, Ordering::Relaxed);
        }
        to_take
    }

    /// Peek the next JS job sequence number
    pub fn peek_seq(&self) -> Option<u64> {
        self.queue.lock().front().map(|(seq, _)| *seq)
    }

    /// Check if queue is empty
    pub fn is_empty(&self) -> bool {
        self.len.load(Ordering::Relaxed) == 0
    }

    /// Clear all pending JS jobs
    pub fn clear(&self) {
        let mut queue = self.queue.lock();
        let len = queue.len();
        queue.clear();
        self.len.fetch_sub(len, Ordering::Relaxed);
    }

    /// Trace GC roots held by queued JS callback jobs
    pub fn trace_roots(&self, tracer: &mut dyn FnMut(*const otter_vm_core::gc::GcHeader)) {
        let queue = self.queue.lock();
        for job in queue.iter() {
            job.1.job.callback.trace(tracer);
            job.1.job.this_arg.trace(tracer);
            if let Some(promise) = &job.1.job.result_promise {
                promise.trace_roots(tracer);
            }
            for arg in job.1.args.iter() {
                arg.trace(tracer);
            }
        }
    }
}

impl Default for JsJobQueue {
    fn default() -> Self {
        Self::new()
    }
}

/// A `process.nextTick()` callback job.
///
/// In Node.js, nextTick callbacks fire before promise reactions (microtasks).
/// This queue is drained entirely before the interleaved microtask/JS-job loop.
#[derive(Clone)]
pub struct NextTickJob {
    /// The JavaScript callback function.
    pub callback: Value,
    /// Additional arguments to pass to the callback.
    pub args: Vec<Value>,
}

/// Queue for `process.nextTick()` callbacks.
///
/// Semantics: All pending nextTick callbacks are drained before ANY promise
/// microtask in the same drain cycle, matching Node.js behavior.
pub struct NextTickQueue {
    queue: Mutex<VecDeque<NextTickJob>>,
    len: AtomicUsize,
}

impl NextTickQueue {
    /// Create a new empty nextTick queue.
    pub fn new() -> Self {
        Self {
            queue: Mutex::new(VecDeque::new()),
            len: AtomicUsize::new(0),
        }
    }

    /// Enqueue a nextTick callback.
    pub fn enqueue(&self, callback: Value, args: Vec<Value>) {
        self.queue.lock().push_back(NextTickJob { callback, args });
        self.len.fetch_add(1, Ordering::Relaxed);
    }

    /// Dequeue the next nextTick callback.
    pub fn dequeue(&self) -> Option<NextTickJob> {
        let job = self.queue.lock().pop_front();
        if job.is_some() {
            self.len.fetch_sub(1, Ordering::Relaxed);
        }
        job
    }

    /// Dequeue up to `max` nextTick jobs in FIFO order in a single lock acquisition.
    pub fn dequeue_batch(&self, max: usize, out: &mut Vec<NextTickJob>) -> usize {
        if max == 0 {
            return 0;
        }
        let mut queue = self.queue.lock();
        let to_take = max.min(queue.len());
        out.reserve(to_take);
        for _ in 0..to_take {
            if let Some(job) = queue.pop_front() {
                out.push(job);
            } else {
                break;
            }
        }
        if to_take != 0 {
            self.len.fetch_sub(to_take, Ordering::Relaxed);
        }
        to_take
    }

    /// Check if queue is empty.
    pub fn is_empty(&self) -> bool {
        self.len.load(Ordering::Relaxed) == 0
    }

    /// Clear all pending nextTick jobs.
    pub fn clear(&self) {
        let mut queue = self.queue.lock();
        let len = queue.len();
        queue.clear();
        self.len.fetch_sub(len, Ordering::Relaxed);
    }

    /// Trace GC roots held by queued nextTick jobs.
    pub fn trace_roots(&self, tracer: &mut dyn FnMut(*const otter_vm_core::gc::GcHeader)) {
        let queue = self.queue.lock();
        for job in queue.iter() {
            job.callback.trace(tracer);
            for arg in &job.args {
                arg.trace(tracer);
            }
        }
    }
}

impl Default for NextTickQueue {
    fn default() -> Self {
        Self::new()
    }
}

/// Wrapper to implement the trait from otter-vm-core
pub struct JsJobQueueWrapper {
    queue: Arc<JsJobQueue>,
}

impl JsJobQueueWrapper {
    pub fn new(queue: Arc<JsJobQueue>) -> Arc<Self> {
        Arc::new(Self { queue })
    }
}

impl otter_vm_core::context::JsJobQueueTrait for JsJobQueueWrapper {
    fn enqueue(&self, job: JsPromiseJob, args: Vec<Value>) {
        self.queue.enqueue(job, args);
    }
}

impl otter_vm_core::context::ExternalRootSet for JsJobQueueWrapper {
    fn trace_roots(&self, tracer: &mut dyn FnMut(*const otter_vm_core::gc::GcHeader)) {
        self.queue.trace_roots(tracer);
    }
}

/// Wrapper for NextTickQueue to implement ExternalRootSet for GC tracing.
pub struct NextTickQueueWrapper {
    queue: Arc<NextTickQueue>,
}

impl NextTickQueueWrapper {
    /// Create a new wrapper.
    pub fn new(queue: Arc<NextTickQueue>) -> Arc<Self> {
        Arc::new(Self { queue })
    }
}

impl otter_vm_core::context::ExternalRootSet for NextTickQueueWrapper {
    fn trace_roots(&self, tracer: &mut dyn FnMut(*const otter_vm_core::gc::GcHeader)) {
        self.queue.trace_roots(tracer);
    }
}
