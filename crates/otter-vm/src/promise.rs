//! Promise intrinsic — ES2024 §27.2.
//!
//! Promises are GC-managed heap objects. When a promise settles, it enqueues
//! [`PromiseJob`]s into the microtask queue for each pending reaction.
//!
//! # State machine
//!
//! ```text
//! Pending ──→ Fulfilled(value)
//!         └─→ Rejected(reason)
//! ```
//!
//! Once settled, a promise's state never changes (immutable after settlement).
//!
//! # Integration
//!
//! - **Interpreter**: `Opcode::Await` creates a promise reaction that, on
//!   settlement, resumes the suspended async frame.
//! - **Microtask queue**: Settlement enqueues `PromiseJob`s via
//!   `MicrotaskQueue::enqueue_promise_job()`.
//! - **Event loop**: Timer/IO completions resolve promises, which enqueue
//!   microtasks, which are drained after each macrotask.

use otter_gc::typed::{Handle as GcHandle, Traceable};

use crate::microtask::{PromiseJob, PromiseJobKind};
use crate::object::ObjectHandle;
use crate::value::RegisterValue;

/// The three-state lifecycle of a Promise.
#[derive(Debug, Clone, PartialEq)]
pub enum PromiseState {
    /// Not yet settled. Reactions accumulate.
    Pending,
    /// Settled with a fulfillment value.
    Fulfilled(RegisterValue),
    /// Settled with a rejection reason.
    Rejected(RegisterValue),
}

/// A single reaction registered via `.then()`, `.catch()`, or `.finally()`.
#[derive(Debug, Clone, PartialEq)]
pub struct PromiseReaction {
    /// The downstream promise that depends on this reaction's result.
    /// Created by `.then()` — the promise it returns.
    pub capability: PromiseCapability,
    /// The JS callback function. `None` means identity (fulfill) or thrower (reject).
    pub handler: Option<ObjectHandle>,
    /// Whether this reaction handles fulfillment or rejection.
    pub kind: ReactionKind,
}

/// Fulfill or reject.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReactionKind {
    Fulfill,
    Reject,
}

/// A { promise, resolve, reject } triple from `NewPromiseCapability`.
///
/// `resolve` and `reject` are ObjectHandles to native resolve/reject functions
/// that, when called, settle the `promise`.
#[derive(Debug, Clone, PartialEq)]
pub struct PromiseCapability {
    /// The promise object.
    pub promise: ObjectHandle,
    /// The resolve function (native, settles `promise` as fulfilled).
    pub resolve: ObjectHandle,
    /// The reject function (native, settles `promise` as rejected).
    pub reject: ObjectHandle,
}

/// The Promise heap object stored in the GC heap.
///
/// Each JS Promise value points to one of these. The `state` field drives
/// the entire resolution lifecycle.
#[derive(Debug, Clone, PartialEq)]
pub struct JsPromise {
    /// Current state.
    pub state: PromiseState,
    /// Pending fulfill reactions (cleared on settlement).
    pub fulfill_reactions: Vec<PromiseReaction>,
    /// Pending reject reactions (cleared on settlement).
    pub reject_reactions: Vec<PromiseReaction>,
    /// Whether any `.then()`/`.catch()` has been registered.
    /// Used for unhandled rejection detection.
    pub is_handled: bool,
    /// The resolve function handle (for chaining — the native function that
    /// can settle this promise from outside).
    pub resolve_function: Option<ObjectHandle>,
    /// The reject function handle.
    pub reject_function: Option<ObjectHandle>,
}

impl JsPromise {
    /// Creates a new pending promise.
    pub fn new() -> Self {
        Self {
            state: PromiseState::Pending,
            fulfill_reactions: Vec::new(),
            reject_reactions: Vec::new(),
            is_handled: false,
            resolve_function: None,
            reject_function: None,
        }
    }

    /// Returns `true` if the promise is still pending.
    pub fn is_pending(&self) -> bool {
        matches!(self.state, PromiseState::Pending)
    }

    /// Returns `true` if the promise is fulfilled.
    pub fn is_fulfilled(&self) -> bool {
        matches!(self.state, PromiseState::Fulfilled(_))
    }

    /// Returns `true` if the promise is rejected.
    pub fn is_rejected(&self) -> bool {
        matches!(self.state, PromiseState::Rejected(_))
    }

    /// Returns the fulfillment value if fulfilled.
    pub fn fulfilled_value(&self) -> Option<RegisterValue> {
        match &self.state {
            PromiseState::Fulfilled(v) => Some(*v),
            _ => None,
        }
    }

    /// Returns the rejection reason if rejected.
    pub fn rejected_reason(&self) -> Option<RegisterValue> {
        match &self.state {
            PromiseState::Rejected(v) => Some(*v),
            _ => None,
        }
    }

    /// ES2024 §27.2.1.4 FulfillPromise.
    ///
    /// Transitions from Pending to Fulfilled and returns the promise jobs
    /// to enqueue (one per pending fulfill reaction).
    ///
    /// Returns `None` if already settled (no-op per spec).
    pub fn fulfill(&mut self, value: RegisterValue) -> Option<Vec<PromiseJob>> {
        if !self.is_pending() {
            return None;
        }

        self.state = PromiseState::Fulfilled(value);
        let reactions = std::mem::take(&mut self.fulfill_reactions);
        self.reject_reactions.clear();

        let jobs = reactions
            .into_iter()
            .map(|reaction| PromiseJob {
                callback: reaction.handler.unwrap_or(reaction.capability.resolve),
                this_value: RegisterValue::undefined(),
                argument: value,
                result_promise: Some(reaction.capability.promise),
                kind: PromiseJobKind::Fulfill,
            })
            .collect();

        Some(jobs)
    }

    /// ES2024 §27.2.1.7 RejectPromise.
    ///
    /// Transitions from Pending to Rejected and returns the promise jobs
    /// to enqueue (one per pending reject reaction).
    pub fn reject(&mut self, reason: RegisterValue) -> Option<Vec<PromiseJob>> {
        if !self.is_pending() {
            return None;
        }

        self.state = PromiseState::Rejected(reason);
        let reactions = std::mem::take(&mut self.reject_reactions);
        self.fulfill_reactions.clear();

        let jobs = reactions
            .into_iter()
            .map(|reaction| PromiseJob {
                callback: reaction.handler.unwrap_or(reaction.capability.reject),
                this_value: RegisterValue::undefined(),
                argument: reason,
                result_promise: Some(reaction.capability.promise),
                kind: PromiseJobKind::Reject,
            })
            .collect();

        Some(jobs)
    }

    /// ES2024 §27.2.5.4 PerformPromiseThen.
    ///
    /// Registers fulfill and/or reject reactions. If the promise is already
    /// settled, returns an immediate job to enqueue.
    pub fn then(
        &mut self,
        on_fulfill: Option<ObjectHandle>,
        on_reject: Option<ObjectHandle>,
        capability: PromiseCapability,
    ) -> Option<PromiseJob> {
        self.is_handled = true;

        match &self.state {
            PromiseState::Pending => {
                self.fulfill_reactions.push(PromiseReaction {
                    capability: capability.clone(),
                    handler: on_fulfill,
                    kind: ReactionKind::Fulfill,
                });
                self.reject_reactions.push(PromiseReaction {
                    capability,
                    handler: on_reject,
                    kind: ReactionKind::Reject,
                });
                None // No immediate job
            }
            PromiseState::Fulfilled(value) => {
                let value = *value;
                Some(PromiseJob {
                    callback: on_fulfill.unwrap_or(capability.resolve),
                    this_value: RegisterValue::undefined(),
                    argument: value,
                    result_promise: Some(capability.promise),
                    kind: PromiseJobKind::Fulfill,
                })
            }
            PromiseState::Rejected(reason) => {
                let reason = *reason;
                Some(PromiseJob {
                    callback: on_reject.unwrap_or(capability.reject),
                    this_value: RegisterValue::undefined(),
                    argument: reason,
                    result_promise: Some(capability.promise),
                    kind: PromiseJobKind::Reject,
                })
            }
        }
    }
}

impl Default for JsPromise {
    fn default() -> Self {
        Self::new()
    }
}

/// GC tracing for JsPromise — reports all ObjectHandle references.
impl Traceable for JsPromise {
    fn trace_handles(&self, visitor: &mut dyn FnMut(GcHandle)) {
        // Trace reactions.
        for reaction in &self.fulfill_reactions {
            visitor(GcHandle(reaction.capability.promise.0));
            visitor(GcHandle(reaction.capability.resolve.0));
            visitor(GcHandle(reaction.capability.reject.0));
            if let Some(h) = reaction.handler {
                visitor(GcHandle(h.0));
            }
        }
        for reaction in &self.reject_reactions {
            visitor(GcHandle(reaction.capability.promise.0));
            visitor(GcHandle(reaction.capability.resolve.0));
            visitor(GcHandle(reaction.capability.reject.0));
            if let Some(h) = reaction.handler {
                visitor(GcHandle(h.0));
            }
        }
        // Trace resolved/rejected values if they contain object handles.
        match &self.state {
            PromiseState::Fulfilled(v) | PromiseState::Rejected(v) => {
                if let Some(h) = v.as_object_handle() {
                    visitor(GcHandle(h));
                }
            }
            PromiseState::Pending => {}
        }
        if let Some(h) = self.resolve_function {
            visitor(GcHandle(h.0));
        }
        if let Some(h) = self.reject_function {
            visitor(GcHandle(h.0));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dummy_capability() -> PromiseCapability {
        PromiseCapability {
            promise: ObjectHandle(100),
            resolve: ObjectHandle(101),
            reject: ObjectHandle(102),
        }
    }

    #[test]
    fn new_promise_is_pending() {
        let p = JsPromise::new();
        assert!(p.is_pending());
        assert!(!p.is_fulfilled());
        assert!(!p.is_rejected());
    }

    #[test]
    fn fulfill_transitions_to_fulfilled() {
        let mut p = JsPromise::new();
        let jobs = p.fulfill(RegisterValue::from_i32(42));
        assert!(jobs.is_some());
        assert!(p.is_fulfilled());
        assert_eq!(p.fulfilled_value(), Some(RegisterValue::from_i32(42)));
    }

    #[test]
    fn reject_transitions_to_rejected() {
        let mut p = JsPromise::new();
        let jobs = p.reject(RegisterValue::from_i32(-1));
        assert!(jobs.is_some());
        assert!(p.is_rejected());
        assert_eq!(p.rejected_reason(), Some(RegisterValue::from_i32(-1)));
    }

    #[test]
    fn double_fulfill_is_noop() {
        let mut p = JsPromise::new();
        p.fulfill(RegisterValue::from_i32(1));
        let jobs = p.fulfill(RegisterValue::from_i32(2));
        assert!(jobs.is_none()); // Second fulfill ignored
        assert_eq!(p.fulfilled_value(), Some(RegisterValue::from_i32(1)));
    }

    #[test]
    fn double_reject_is_noop() {
        let mut p = JsPromise::new();
        p.reject(RegisterValue::from_i32(1));
        let jobs = p.reject(RegisterValue::from_i32(2));
        assert!(jobs.is_none());
    }

    #[test]
    fn fulfill_after_reject_is_noop() {
        let mut p = JsPromise::new();
        p.reject(RegisterValue::from_i32(1));
        let jobs = p.fulfill(RegisterValue::from_i32(2));
        assert!(jobs.is_none());
        assert!(p.is_rejected());
    }

    #[test]
    fn then_on_pending_queues_reactions() {
        let mut p = JsPromise::new();
        let handler = ObjectHandle(50);
        let cap = dummy_capability();

        let immediate = p.then(Some(handler), None, cap);
        assert!(immediate.is_none()); // Pending — no immediate job.
        assert_eq!(p.fulfill_reactions.len(), 1);
        assert_eq!(p.reject_reactions.len(), 1);
        assert!(p.is_handled);
    }

    #[test]
    fn then_on_fulfilled_returns_immediate_job() {
        let mut p = JsPromise::new();
        p.fulfill(RegisterValue::from_i32(99));

        let handler = ObjectHandle(60);
        let cap = dummy_capability();
        let job = p.then(Some(handler), None, cap);

        assert!(job.is_some());
        let job = job.unwrap();
        assert_eq!(job.callback, handler);
        assert_eq!(job.argument, RegisterValue::from_i32(99));
        assert_eq!(job.kind, PromiseJobKind::Fulfill);
    }

    #[test]
    fn then_on_rejected_returns_reject_job() {
        let mut p = JsPromise::new();
        p.reject(RegisterValue::from_i32(-5));

        let handler = ObjectHandle(70);
        let cap = dummy_capability();
        let job = p.then(None, Some(handler), cap);

        assert!(job.is_some());
        let job = job.unwrap();
        assert_eq!(job.callback, handler);
        assert_eq!(job.argument, RegisterValue::from_i32(-5));
        assert_eq!(job.kind, PromiseJobKind::Reject);
    }

    #[test]
    fn fulfill_triggers_reactions() {
        let mut p = JsPromise::new();

        // Register two reactions.
        let h1 = ObjectHandle(10);
        let h2 = ObjectHandle(20);
        p.then(Some(h1), None, dummy_capability());
        p.then(Some(h2), None, PromiseCapability {
            promise: ObjectHandle(200),
            resolve: ObjectHandle(201),
            reject: ObjectHandle(202),
        });

        assert_eq!(p.fulfill_reactions.len(), 2);

        // Fulfill — should produce 2 jobs.
        let jobs = p.fulfill(RegisterValue::from_i32(7)).unwrap();
        assert_eq!(jobs.len(), 2);
        assert_eq!(jobs[0].callback, h1);
        assert_eq!(jobs[1].callback, h2);

        // Reactions cleared after settlement.
        assert!(p.fulfill_reactions.is_empty());
        assert!(p.reject_reactions.is_empty());
    }

    #[test]
    fn traceable_reports_handles() {
        let mut p = JsPromise::new();
        p.resolve_function = Some(ObjectHandle(5));
        p.then(Some(ObjectHandle(10)), Some(ObjectHandle(11)), dummy_capability());

        let mut handles = Vec::new();
        p.trace_handles(&mut |h| handles.push(h.0));

        // Should include: capability (100, 101, 102), handler (10), reject handler (11),
        // and resolve_function (5). Exact count depends on fulfill + reject reaction duplication.
        assert!(!handles.is_empty());
        assert!(handles.contains(&5)); // resolve_function
        assert!(handles.contains(&10)); // fulfill handler
        assert!(handles.contains(&100)); // capability.promise
    }
}
