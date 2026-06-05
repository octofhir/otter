//! Microtask queue for isolate-local promise and finalization jobs.
//!
//! # Why this shape
//!
//! This queue deliberately stays isolate-local. `Microtask` records
//! carry [`crate::Value`] and parked [`crate::Frame`] state, both of
//! which can contain GC handles. Host async work must therefore post
//! owned runtime messages through `otter-runtime` and re-enter the
//! isolate before enqueueing a `Microtask`; there is no public
//! cross-thread `Sender<Microtask>` surface.
//!
//! The hot path is a plain `VecDeque<Microtask>` mutated through
//! `&mut self` from inside the dispatch loop. No `RefCell`, no
//! `UnsafeCell`, no atomics — just a field write. This is the path
//! `Op::QueueMicrotask` takes 100% of the time today.
//!
//! # Drain semantics
//!
//! - **Swap-and-drain** into the queue-owned `in_flight` deque:
//!   tasks enqueued *during* a drain go on the next generation, and
//!   each generation runs to completion before the next. Waiting
//!   tasks stay owned by the queue — never by a driver-local batch —
//!   so the GC root walk sees them while their predecessors execute
//!   (parked async frames in the queue carry raw register slots a
//!   scavenge must rewrite).
//! - **Reentrant `drain_depth`**: nested `drain_microtasks()` calls
//!   from inside a microtask are no-ops — the outermost drain
//!   absorbs all pending work.
//! - **Iteration budget**: a hard cap (`MAX_DRAIN_ITERS`) prevents
//!   `queueMicrotask(fn) inside fn` from livelocking the host.
//!   Hitting it surfaces as [`MicrotaskError::Runaway`].
//! - **Exception policy**: foundation propagates the **first**
//!   error out of the drain. Promise reactions use spec-style rejection
//!   scheduling when they are queued through the promise machinery.
//!
//! # Contents
//! - [`Microtask`] — task record (callee + this + inline args).
//! - [`MicrotaskQueue`] — sync deque + drain state.
//! - [`MicrotaskError`] — drain-time failure modes.
//!
//! # See also
//! - [Event loop](../../../docs/book/src/engine/event-loop.md)

use std::collections::VecDeque;

use smallvec::SmallVec;

use crate::execution_context::ExecutionContext;
use crate::{Frame, Value};
use otter_gc::raw::RawGc;

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
    /// Execution context that owns the queued callable / parked
    /// frame. Host-driven settlement can happen after another
    /// script has run, so the microtask carries its dispatch
    /// context.
    pub context: Option<ExecutionContext>,
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
        /// Detached cold record extracted from the interpreter pool
        /// at suspend time. The drain re-attaches it before pushing
        /// the frame back onto the stack. `None` when the parked
        /// frame had no cold state.
        cold: Option<Box<crate::cold_frame::ColdFrame>>,
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
    /// can settle queued requests on completion.
    AsyncGenResume {
        /// Frame the drain re-pushes.
        frame: Box<Frame>,
        /// Detached cold record, see [`Self::AsyncResume::cold`].
        cold: Option<Box<crate::cold_frame::ColdFrame>>,
        /// Register inside `frame` that receives the awaited
        /// value on the fulfilled path.
        await_dst: u16,
        /// `true` when the awaited promise fulfilled.
        fulfilled: bool,
        /// Owning async-generator handle whose request queue the
        /// drain settles on yield / completion / throw.
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

/// Sync deque + optional async inbox + drain bookkeeping.
///
/// Owned by [`crate::Interpreter`] as a plain field — every method
/// that touches the queue takes `&mut self`. No `RefCell`, no
/// `UnsafeCell`.
#[derive(Debug, Default)]
pub struct MicrotaskQueue {
    /// Sync side: pushed by `Op::QueueMicrotask` and host-side
    /// `enqueue` calls running on the interpreter thread.
    pending: VecDeque<Microtask>,
    /// Tasks of the generation currently being drained. Owned by the
    /// queue — not handed to the driver by value — so tasks waiting
    /// behind the one being executed stay visible to the GC root
    /// walk. A parked async frame in here holds raw `Value` register
    /// slots; if a scavenge during task `k` cannot see task `k+1`,
    /// the later frame resumes over freed/moved objects.
    in_flight: VecDeque<Microtask>,
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

    /// Drain bookkeeping — see module docstring. Moves the current
    /// generation into the queue-owned `in_flight` deque and returns
    /// its task count. The driver pulls tasks one at a time through
    /// [`Self::next_in_flight`]; tasks still waiting stay traced by
    /// [`Self::trace_gc_slots`] while their predecessors execute.
    ///
    /// Returns `None` when this is a reentrant call (drain already
    /// in progress on an outer frame); the caller should yield
    /// without iterating in that case.
    pub fn begin_drain(&mut self) -> Option<usize> {
        if self.drain_depth > 0 {
            return None;
        }
        self.drain_depth += 1;
        self.generation += 1;
        debug_assert!(self.in_flight.is_empty());
        // Swap the current generation into `in_flight`. Tasks
        // enqueued during the drain go on `pending`, which the
        // caller's outer loop picks up on the next iteration.
        std::mem::swap(&mut self.pending, &mut self.in_flight);
        Some(self.in_flight.len())
    }

    /// Pop the next task of the in-flight generation, if any.
    pub fn next_in_flight(&mut self) -> Option<Microtask> {
        self.in_flight.pop_front()
    }

    /// End-of-drain bookkeeping. Decrements `drain_depth` and
    /// returns any unexecuted in-flight tasks (an erroring drain
    /// stops mid-generation) to the front of `pending` in their
    /// original order so a follow-up drain resumes where this one
    /// stopped. Caller is required to invoke this once for every
    /// successful [`Self::begin_drain`].
    pub fn end_drain(&mut self) {
        debug_assert!(self.drain_depth > 0);
        while let Some(task) = self.in_flight.pop_back() {
            self.pending.push_front(task);
        }
        self.drain_depth = self.drain_depth.saturating_sub(1);
    }

    /// `true` if the sync side has work.
    #[must_use]
    pub fn has_any_pending(&self) -> bool {
        !self.pending.is_empty() || !self.in_flight.is_empty()
    }

    /// Hot-path testing helper — clear the queue without going
    /// through a drain. Foundation tests use this to reset state
    /// between assertions.
    #[doc(hidden)]
    pub fn clear_for_tests(&mut self) {
        self.pending.clear();
        self.in_flight.clear();
        self.drain_depth = 0;
    }
}

impl Microtask {
    /// Trace every GC-bearing value slot held by this queued task.
    pub(crate) fn trace_gc_slots(&self, visitor: &mut dyn FnMut(*mut RawGc)) {
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
            MicrotaskKind::AsyncResume { frame, cold, .. } => {
                frame.trace_frame_slots(visitor);
                if let Some(c) = cold {
                    c.trace_cold_slots(visitor);
                }
            }
            MicrotaskKind::AsyncGenResume {
                frame, cold, owner, ..
            } => {
                frame.trace_frame_slots(visitor);
                if let Some(c) = cold {
                    c.trace_cold_slots(visitor);
                }
                owner.trace_value_slots(visitor);
            }
        }
    }
}

impl MicrotaskQueue {
    /// Trace every queued isolate-local task — both the pending
    /// generation and the in-flight one a drain is executing.
    pub(crate) fn trace_gc_slots(&self, visitor: &mut dyn FnMut(*mut RawGc)) {
        for task in &self.pending {
            task.trace_gc_slots(visitor);
        }
        for task in &self.in_flight {
            task.trace_gc_slots(visitor);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn task_for(n: i32) -> Microtask {
        Microtask {
            callee: Value::number_i32(n),
            this_value: Value::undefined(),
            args: SmallVec::new(),
            context: None,
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
        let batch_len = q.begin_drain().unwrap();
        assert_eq!(batch_len, 3);
        let mut order: Vec<i32> = Vec::new();
        while let Some(task) = q.next_in_flight() {
            order.push(task.callee.as_number().unwrap().as_smi().unwrap());
        }
        assert_eq!(order, vec![1, 2, 3]);
        q.end_drain();
        assert!(!q.has_pending_sync());
    }

    #[test]
    fn end_drain_returns_unexecuted_tasks_to_pending_in_order() {
        let mut q = MicrotaskQueue::new();
        q.enqueue(task_for(1));
        q.enqueue(task_for(2));
        q.enqueue(task_for(3));
        let _ = q.begin_drain().unwrap();
        // Driver executes task 1, then aborts the drain (error path).
        let first = q.next_in_flight().unwrap();
        assert_eq!(first.callee.as_number().unwrap().as_smi().unwrap(), 1);
        q.end_drain();
        // Tasks 2 and 3 must survive, in order, for the next drain.
        let next_len = q.begin_drain().unwrap();
        assert_eq!(next_len, 2);
        let mut order: Vec<i32> = Vec::new();
        while let Some(task) = q.next_in_flight() {
            order.push(task.callee.as_number().unwrap().as_smi().unwrap());
        }
        assert_eq!(order, vec![2, 3]);
        q.end_drain();
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
        let batch_len = q.begin_drain().unwrap();
        assert_eq!(batch_len, 1);
        // Simulate the driver running a task that pushes another.
        let _ = q.next_in_flight().unwrap();
        q.enqueue(task_for(2));
        // The fresh push lands on the next generation.
        q.end_drain();
        let next_len = q.begin_drain().unwrap();
        assert_eq!(next_len, 1);
        let next = q.next_in_flight().unwrap();
        assert_eq!(next.callee.as_number().unwrap().as_smi().unwrap(), 2);
        q.end_drain();
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
    fn microtask_records_stay_isolate_local() {
        static_assertions::assert_not_impl_any!(Microtask: Send, Sync);
    }
}
