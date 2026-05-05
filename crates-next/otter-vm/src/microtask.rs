//! Microtask queue with a sync hot path and a cross-thread async
//! inbox.
//!
//! # Why this shape
//!
//! Other engines pick one of two designs:
//!
//! - **Single deque under interior mutability** (common JS engine design) —
//!   single-thread by language spec; interior cell is
//!   not a problem because no other thread can touch it. Cheap, but
//!   leaves no door open for cross-thread async-runtime callbacks.
//! - **MPSC channel into the runtime thread** (Tokio, Deno bridge
//!   layer) — every enqueue goes through a lock-free channel so
//!   any thread can post. Strictly more flexible, slightly more
//!   per-task overhead than a plain deque.
//!
//! We do **both**, split by axis:
//!
//! - **Sync hot path:** plain `VecDeque<Microtask>` mutated through
//!   `&mut self` from inside the dispatch loop. No `RefCell`, no
//!   `UnsafeCell`, no atomics — just a field write. This is the
//!   path `Op::QueueMicrotask` takes 100% of the time today.
//! - **Cross-thread async path:** an optional
//!   [`crossbeam_channel::Receiver`] populated by an
//!   [`AsyncRuntime`] impl from any thread. The drain calls
//!   `try_recv()` between generations to fold async-side work into
//!   the same FIFO order as sync-side enqueues.
//!
//! The async slot is wired in shape but **left unpopulated by the
//! foundation slice** — task 35 (`async`/`await`) plugs in the
//! Tokio-backed [`AsyncRuntime`] impl. The trait skeleton is here
//! today so the queue's API is stable across Phase F: no rework
//! when async lands, just plug in the impl.
//!
//! # Drain semantics
//!
//! - **Swap-and-drain** with `mem::take`: tasks enqueued *during* a
//!   drain go on the next generation. Each generation runs to
//!   completion before the next. This matches reused-buffer engine
//!   patterns while skipping the interior-mutability cost.
//! - **Reentrant `drain_depth`**: nested `drain_microtasks()` calls
//!   from inside a microtask are no-ops — the outermost drain
//!   absorbs all pending work.
//! - **Iteration budget**: a hard cap (`MAX_DRAIN_ITERS`) prevents
//!   `queueMicrotask(fn) inside fn` from livelocking the host.
//!   Hitting it surfaces as [`MicrotaskError::Runaway`].
//! - **Exception policy**: foundation propagates the **first**
//!   error out of the drain. Once the `Promise` constructor lands
//!   in task 34, this flips to spec semantics ("rejected promise,
//!   continue draining").
//!
//! # Contents
//! - [`Microtask`] — task record (callee + this + inline args).
//! - [`MicrotaskQueue`] — sync deque + optional async inbox + state.
//! - [`AsyncRuntime`] — trait the embedder implements (Tokio in
//!   task 35).
//! - [`MicrotaskError`] — drain-time failure modes.
//!
//! # See also
//! - [`docs/new-engine/tasks/33-microtask-queue.md`](
//!     ../../../docs/new-engine/tasks/33-microtask-queue.md
//!   )

use std::collections::VecDeque;

use crossbeam_channel::Receiver;
use smallvec::SmallVec;

use crate::{Frame, Value};

/// Hard cap on tasks drained per single drain call. Past this we
/// return [`MicrotaskError::Runaway`] so a misbehaving JS program
/// that recursively schedules microtasks cannot stall the host.
pub const MAX_DRAIN_ITERS: u32 = 1_000_000;

/// One queued microtask.
///
/// Args use a 4-element inline `SmallVec` so the typical
/// `Promise.resolve().then(fn)` (1 arg) and `queueMicrotask(fn)`
/// (0 args) shapes never allocate.
///
/// The default [`MicrotaskKind::Call`] dispatch invokes `callee`
/// with `args`. The [`MicrotaskKind::AsyncResume`] kind is the
/// async-await suspension point's settlement vehicle: when the
/// awaited promise settles, the runtime parks the suspended frame
/// onto a fresh microtask of this kind so the drain re-pushes it
/// onto a one-deep stack and runs `dispatch_loop` from where the
/// `Op::Await` left off.
#[derive(Debug)]
pub struct Microtask {
    /// Function value to invoke. Must satisfy `is_callable` for
    /// [`MicrotaskKind::Call`] tasks; ignored entirely for the
    /// async-resume kind.
    pub callee: Value,
    /// `this` binding for the call. Spec microtasks have
    /// `undefined`; embedder-injected callbacks may bind otherwise.
    pub this_value: Value,
    /// Arguments. 0–4 inline; 5+ heap. For
    /// [`MicrotaskKind::AsyncResume`] the slot at index 0 carries
    /// the resolved value (fulfilment) or rejection reason.
    pub args: SmallVec<[Value; 4]>,
    /// Optional `{resolve, reject}` capability to settle with the
    /// task's outcome. Promise reaction jobs use this so the
    /// handler's return value flows into the next promise in the
    /// chain (and a thrown error rejects it). `None` for plain
    /// `queueMicrotask(fn)` callbacks.
    pub result_capability: Option<MicrotaskCapability>,
    /// What flavour of work this task represents. Defaults to
    /// [`MicrotaskKind::Call`] (the `queueMicrotask(fn, args...)`
    /// shape). `Op::Await` enqueues [`MicrotaskKind::AsyncResume`]
    /// to wake a parked async frame.
    pub kind: MicrotaskKind,
}

/// What a queued microtask actually does when the drain reaches
/// it. The default `Call` is `queueMicrotask(callee, args...)` — a
/// plain top-level invocation with optional reaction-mode
/// settlement. `AsyncResume` is the await-suspension settlement
/// vehicle: instead of calling `callee`, the drain re-pushes the
/// parked frame, writes the resolution value into the await's
/// destination register (or throws into the frame on rejection),
/// and runs `dispatch_loop` until the frame settles its result
/// promise.
#[derive(Debug)]
#[non_exhaustive]
pub enum MicrotaskKind {
    /// `queueMicrotask(callee, args...)`. Default for both plain
    /// `queueMicrotask` calls and promise-reaction handlers.
    Call,
    /// Host cleanup callback queued after a `FinalizationRegistry`
    /// cell's target was found dead by GC. Dispatch is identical to
    /// [`Self::Call`], but the distinct kind keeps finalization jobs
    /// visible to tests and future host scheduling policy.
    FinalizationCallback,
    /// Resume a parked async frame. `frame` was popped off the
    /// active stack at the matching `Op::Await`; the drain rebuilds
    /// a fresh stack containing only this frame and continues
    /// execution from the next pc.
    AsyncResume {
        /// Frame the drain re-pushes. Boxed so the `Microtask`
        /// stays small in the common-case `Call` enqueue path.
        frame: Box<Frame>,
        /// Register inside `frame` that receives the awaited
        /// value on the fulfilled path.
        await_dst: u16,
        /// `true` when the awaited promise fulfilled. `false`
        /// rejects: the resume path immediately unwinds with
        /// `args[0]` as the thrown value.
        fulfilled: bool,
    },
    /// Resume a parked async-generator body — see ECMA-262 §27.6
    /// for the spec semantics. Same shape as [`Self::AsyncResume`]
    /// but also carries the owning generator handle so the drain
    /// can settle its `pending_request` on completion.
    AsyncGenResume {
        /// Frame the drain re-pushes.
        frame: Box<Frame>,
        /// Register inside `frame` that receives the awaited
        /// value on the fulfilled path.
        await_dst: u16,
        /// `true` when the awaited promise fulfilled.
        fulfilled: bool,
        /// Owning async-generator handle whose `pending_request`
        /// the drain settles on yield / completion / throw.
        owner: crate::generator::JsGenerator,
    },
}

/// `{resolve, reject}` pair the runtime uses to settle a
/// reaction's downstream promise. Defined here (not in
/// `crate::promise`) to keep the microtask layer dependency-free
/// of the promise module.
#[derive(Debug, Clone)]
pub struct MicrotaskCapability {
    /// Native callable: `resolve(v)` settles the downstream as fulfilled.
    pub resolve: Value,
    /// Native callable: `reject(reason)` settles the downstream as rejected.
    pub reject: Value,
}

/// Failure modes for a drain.
#[derive(Debug, Clone, thiserror::Error)]
#[non_exhaustive]
pub enum MicrotaskError {
    /// The drain hit [`MAX_DRAIN_ITERS`] before the queue emptied.
    /// A real program almost never trips this — it indicates a
    /// `queueMicrotask` recursion bug.
    #[error("microtask drain exceeded {limit} iterations")]
    Runaway {
        /// The cap that was reached.
        limit: u32,
    },
}

/// Async-runtime contract. The interpreter never owns a runtime
/// itself; the embedder wires one in. Foundation slice ships only
/// the trait — the Tokio impl arrives with task 35.
///
/// # Why a trait, not a concrete struct
///
/// 1. Most embedders already have their own Tokio runtime; they want
///    the VM to
///    plug into theirs, not start a second one.
/// 2. Tests can mock the trait without standing up Tokio.
/// 3. Different platforms (wasm32-wasi, no-std hosts) substitute
///    different impls without touching the interpreter.
pub trait AsyncRuntime: Send + Sync {
    /// Cloneable handle for posting microtasks back from any
    /// thread. The interpreter wires the matching `Receiver` into
    /// [`MicrotaskQueue::async_inbox`].
    fn microtask_sender(&self) -> crossbeam_channel::Sender<Microtask>;

    /// Spawn a future onto the underlying runtime. Foundation
    /// guarantee: the future must complete (or post a microtask)
    /// before the next drain returns; long-running futures belong
    /// to the embedder's `Runtime`, not the microtask path.
    fn spawn(&self, fut: futures_core_local::BoxFuture<'static, ()>);

    /// Park the runtime thread until at least one microtask is
    /// available on `recv`. Used by long-running event loops; the
    /// foundation `drain_microtasks` does not park.
    fn park_until_microtask(&self, recv: &Receiver<Microtask>);
}

// Foundation-local `BoxFuture` placeholder. Once task 35 introduces
// a real `futures` dep we'll replace this re-export module with the
// real `futures::future::BoxFuture`. The shim keeps the trait
// signature stable today so embedders can implement against it.
pub mod futures_core_local {
    //! Placeholder for `futures::future::BoxFuture<'a, T>`. Task 35
    //! swaps this for the real type.
    use std::future::Future;
    use std::pin::Pin;

    /// Boxed dynamic future. Mirrors `futures::future::BoxFuture`.
    pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;
}

/// Sync deque + optional async inbox + drain bookkeeping.
///
/// Owned by [`crate::Interpreter`] as a plain field — every method
/// that touches the queue takes `&mut self`. No `RefCell`, no
/// `UnsafeCell`. Cross-thread enqueues go through `async_inbox`.
#[derive(Debug, Default)]
pub struct MicrotaskQueue {
    /// Sync side: pushed by `Op::QueueMicrotask` and host-side
    /// `enqueue` calls running on the interpreter thread.
    pending: VecDeque<Microtask>,
    /// Cross-thread async inbox. `None` until task 35 wires up the
    /// `AsyncRuntime` impl. When `Some`, drain pulls from this on
    /// every generation boundary.
    async_inbox: Option<Receiver<Microtask>>,
    /// Reentrant-drain depth. Only the outermost drain finalises;
    /// nested calls return immediately (no-op) so a microtask body
    /// can call `drain_microtasks` itself without recursing.
    drain_depth: u32,
    /// Generation counter. Incremented at every `mem::take` swap.
    /// Exposed via [`Self::generation`] for embedder telemetry.
    generation: u64,
    /// Persistent high-water mark. The drain reuses the swapped
    /// buffer between generations so steady-state allocation is
    /// zero once the queue's seen its peak size.
    high_water: usize,
}

impl MicrotaskQueue {
    /// Construct an empty queue with no async inbox wired.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Wire a cross-thread receiver into the queue. Idempotent —
    /// replaces any prior receiver.
    pub fn set_async_inbox(&mut self, recv: Receiver<Microtask>) {
        self.async_inbox = Some(recv);
    }

    /// `true` when the sync side has tasks pending.
    #[must_use]
    pub fn has_pending_sync(&self) -> bool {
        !self.pending.is_empty()
    }

    /// Generation counter — increments once per swap during drain.
    /// Useful for embedder observers (e.g. checkpoint markers).
    #[must_use]
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// Enqueue from the interpreter thread. Cheap: one
    /// `VecDeque::push_back` plus a high-water update.
    pub fn enqueue(&mut self, task: Microtask) {
        self.pending.push_back(task);
        if self.pending.len() > self.high_water {
            self.high_water = self.pending.len();
        }
    }

    /// Drain bookkeeping — see module docstring. Returns the
    /// generation snapshot the caller is now responsible for
    /// running; pulls from `async_inbox` so the snapshot includes
    /// any cross-thread tasks posted up to this moment.
    ///
    /// Returns `None` when this is a reentrant call (drain already
    /// in progress on an outer frame); the caller should yield
    /// without iterating in that case.
    pub fn begin_drain(&mut self) -> Option<DrainBatch> {
        if self.drain_depth > 0 {
            return None;
        }
        self.drain_depth += 1;
        // Pull cross-thread tasks into the sync deque before
        // swapping. `try_recv` returns immediately when empty —
        // never blocks the interpreter thread.
        if let Some(recv) = &self.async_inbox {
            while let Ok(task) = recv.try_recv() {
                self.pending.push_back(task);
                if self.pending.len() > self.high_water {
                    self.high_water = self.pending.len();
                }
            }
        }
        self.generation += 1;
        // Take ownership of the current generation. Tasks enqueued
        // during the drain go on the fresh deque (returned to
        // `pending` by `mem::take`), which the caller's outer loop
        // picks up on the next iteration.
        let batch = std::mem::take(&mut self.pending);
        Some(DrainBatch {
            tasks: batch,
            generation: self.generation,
        })
    }

    /// End-of-drain bookkeeping. Decrements `drain_depth` and
    /// drops the temporary deque. Caller is required to invoke
    /// this once for every successful [`Self::begin_drain`].
    pub fn end_drain(&mut self) {
        debug_assert!(self.drain_depth > 0);
        self.drain_depth = self.drain_depth.saturating_sub(1);
    }

    /// `true` if either the sync side has work or the async inbox
    /// has unread tasks. The async-inbox check is non-blocking
    /// (`is_empty` peek).
    #[must_use]
    pub fn has_any_pending(&self) -> bool {
        if !self.pending.is_empty() {
            return true;
        }
        match &self.async_inbox {
            Some(recv) => !recv.is_empty(),
            None => false,
        }
    }

    /// Hot-path testing helper — clear the queue without going
    /// through a drain. Foundation tests use this to reset state
    /// between assertions.
    #[doc(hidden)]
    pub fn clear_for_tests(&mut self) {
        self.pending.clear();
        self.drain_depth = 0;
    }
}

impl Microtask {
    /// Trace every GC-bearing value slot held by this queued task.
    pub(crate) fn trace_gc_slots(&self, visitor: &mut dyn FnMut(*mut otter_gc::RawGc)) {
        self.callee.trace_value_slots(visitor);
        self.this_value.trace_value_slots(visitor);
        for arg in &self.args {
            arg.trace_value_slots(visitor);
        }
        if let Some(capability) = &self.result_capability {
            capability.resolve.trace_value_slots(visitor);
            capability.reject.trace_value_slots(visitor);
        }
        match &self.kind {
            MicrotaskKind::Call | MicrotaskKind::FinalizationCallback => {}
            MicrotaskKind::AsyncResume { frame, .. } => frame.trace_frame_slots(visitor),
            MicrotaskKind::AsyncGenResume { frame, owner, .. } => {
                frame.trace_frame_slots(visitor);
                owner.trace_value_slots(visitor);
            }
        }
    }
}

impl MicrotaskQueue {
    /// Trace every queued task on the sync side. The cross-thread
    /// inbox is drained into this deque before execution; it is not
    /// an isolate-local root until folded into `pending`.
    pub(crate) fn trace_gc_slots(&self, visitor: &mut dyn FnMut(*mut otter_gc::RawGc)) {
        for task in &self.pending {
            task.trace_gc_slots(visitor);
        }
    }
}

/// One generation of work removed from the queue by
/// [`MicrotaskQueue::begin_drain`]. The driver iterates this batch,
/// then asks `begin_drain` again until it returns an empty batch
/// (no more sync work) **and** `has_any_pending()` is false.
#[derive(Debug)]
pub struct DrainBatch {
    /// Tasks in FIFO order — the caller pops from the front.
    pub tasks: VecDeque<Microtask>,
    /// Generation number assigned to this batch.
    pub generation: u64,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::number::NumberValue;

    fn task_for(n: i32) -> Microtask {
        Microtask {
            callee: Value::Number(NumberValue::from_i32(n)),
            this_value: Value::Undefined,
            args: SmallVec::new(),
            result_capability: None,
            kind: MicrotaskKind::Call,
        }
    }

    #[test]
    fn enqueue_then_drain_preserves_order() {
        let mut q = MicrotaskQueue::new();
        q.enqueue(task_for(1));
        q.enqueue(task_for(2));
        q.enqueue(task_for(3));
        let batch = q.begin_drain().unwrap();
        assert_eq!(batch.tasks.len(), 3);
        let order: Vec<i32> = batch
            .tasks
            .iter()
            .map(|t| t.callee.as_number().unwrap().as_smi().unwrap())
            .collect();
        assert_eq!(order, vec![1, 2, 3]);
        q.end_drain();
        assert!(!q.has_pending_sync());
    }

    #[test]
    fn nested_begin_drain_returns_none() {
        let mut q = MicrotaskQueue::new();
        q.enqueue(task_for(1));
        let _outer = q.begin_drain().unwrap();
        // Reentrant drain: returns None until end_drain is called.
        assert!(q.begin_drain().is_none());
        q.end_drain();
    }

    #[test]
    fn enqueue_during_drain_lands_in_next_batch() {
        let mut q = MicrotaskQueue::new();
        q.enqueue(task_for(1));
        let batch = q.begin_drain().unwrap();
        // Simulate the driver running a task that pushes another.
        q.enqueue(task_for(2));
        assert_eq!(batch.tasks.len(), 1);
        // The fresh push lands on the next generation.
        q.end_drain();
        let next = q.begin_drain().unwrap();
        assert_eq!(next.tasks.len(), 1);
        assert_eq!(
            next.tasks[0].callee.as_number().unwrap().as_smi().unwrap(),
            2
        );
    }

    #[test]
    fn generation_increments_per_swap() {
        let mut q = MicrotaskQueue::new();
        assert_eq!(q.generation(), 0);
        q.enqueue(task_for(1));
        let _ = q.begin_drain().unwrap();
        assert_eq!(q.generation(), 1);
        q.end_drain();
        let _ = q.begin_drain().unwrap();
        assert_eq!(q.generation(), 2);
        q.end_drain();
    }

    #[test]
    fn async_inbox_folds_into_drain() {
        let (tx, rx) = crossbeam_channel::unbounded();
        let mut q = MicrotaskQueue::new();
        q.set_async_inbox(rx);
        // Cross-thread post (from this thread, but the API is
        // identical to a Tokio-task post).
        tx.send(task_for(42)).unwrap();
        assert!(q.has_any_pending());
        let batch = q.begin_drain().unwrap();
        assert_eq!(batch.tasks.len(), 1);
        assert_eq!(
            batch.tasks[0].callee.as_number().unwrap().as_smi().unwrap(),
            42
        );
        q.end_drain();
    }
}
