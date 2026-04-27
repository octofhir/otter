//! `Promise` value, modelled as a trait + concrete impl.
//!
//! # Why a trait
//!
//! Foundation today ships exactly one promise implementation
//! ([`PurePromise`]) that owns its own state machine. Phase F
//! brings host-bridged promises (Tokio futures, fetch responses,
//! file I/O) which want to expose **the same surface** to JS but
//! resolve through a different mechanism — a future completing on
//! a worker thread should look identical to user code.
//!
//! Mirrors the [`crate::AsyncRuntime`] trait shape introduced in
//! task 33. The legacy `crates/otter-vm/src/promise.rs` was a
//! single concrete struct; the new design extracts the spec
//! surface into a trait so we never need to retrofit again.
//!
//! # State machine — ES2024 §27.2
//!
//! ```text
//! Pending ──→ Fulfilled(value)
//!         └─→ Rejected(reason)
//! ```
//!
//! Once settled, a promise's state is immutable. Reactions queued
//! before settlement run as microtasks at settlement time;
//! reactions queued after settlement enqueue immediately into the
//! microtask queue.
//!
//! # Contents
//! - [`JsPromise`] — the trait. Embedders implement this for
//!   host-bridged promises in Phase F.
//! - [`PromiseState`] — `Pending` / `Fulfilled(Value)` /
//!   `Rejected(Value)`.
//! - [`PromiseReaction`] — one `then` registration.
//! - [`PromiseCapability`] — `{ promise, resolve, reject }` triple.
//! - [`PurePromise`] — concrete spec-faithful impl.
//! - [`PromiseSettleJobs`] — what `fulfill` / `reject` return so
//!   the caller can enqueue the reactions on the microtask queue.
//!
//! # Invariants
//! - `fulfill` / `reject` on an already-settled promise is a
//!   no-op (per spec §27.2.1.4 / §27.2.1.7).
//! - Reactions FIFO within the fulfill-bucket and reject-bucket.
//! - `is_handled` flips to `true` the first time a reaction is
//!   registered; embedders observe this for "unhandled rejection"
//!   reporting (Phase F).
//! - Foundation foundation slice uses `Rc<RefCell<...>>` for
//!   shared mutability. Task 56 will replace the cell with a
//!   `&mut`-owned slot via the broader RefCell-removal effort;
//!   the [`JsPromise`] trait surface stays the same.
//!
//! # See also
//! - [`docs/new-engine/tasks/34-promise-value.md`](
//!     ../../../docs/new-engine/tasks/34-promise-value.md
//!   )
//! - [`docs/new-engine/tasks/33-microtask-queue.md`](
//!     ../../../docs/new-engine/tasks/33-microtask-queue.md
//!   )

use std::cell::RefCell;
use std::rc::Rc;

use crate::Value;
use crate::microtask::Microtask;

/// One of three terminal states. Once `Fulfilled` or `Rejected`,
/// the promise never transitions again.
#[derive(Debug, Clone, PartialEq)]
pub enum PromiseState {
    /// No settlement decided yet. New reactions append to the
    /// matching bucket.
    Pending,
    /// Settled with a fulfillment value (`then`-handled).
    Fulfilled(Value),
    /// Settled with a rejection reason (`catch`-handled).
    Rejected(Value),
}

impl PromiseState {
    /// `true` when this state is not [`Self::Pending`].
    #[must_use]
    pub fn is_settled(&self) -> bool {
        !matches!(self, Self::Pending)
    }
}

/// One reaction registered via `.then` / `.catch` / `.finally`.
#[derive(Debug, Clone)]
pub struct PromiseReaction {
    /// Downstream capability this reaction settles into.
    pub capability: PromiseCapability,
    /// JS callback for this kind. `None` means identity (fulfill)
    /// or rethrow (reject), per spec.
    pub handler: Option<Value>,
    /// Which side of `then` this reaction handles.
    pub kind: ReactionKind,
}

/// Whether a reaction handles fulfillment or rejection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReactionKind {
    /// Wired through `on_fulfilled` of a `then`.
    Fulfill,
    /// Wired through `on_rejected` of a `then` / `catch`.
    Reject,
}

/// `{ promise, resolve, reject }` — the triple created by
/// `NewPromiseCapability` (spec §27.2.1.5). All three slots are
/// `Value`s so they survive cloning across the dispatcher;
/// `resolve` / `reject` are always [`Value::NativeFunction`].
#[derive(Debug, Clone)]
pub struct PromiseCapability {
    /// The promise this capability settles.
    pub promise: Value,
    /// Native function: `resolve(v)` settles `promise` as fulfilled.
    pub resolve: Value,
    /// Native function: `reject(reason)` settles `promise` as rejected.
    pub reject: Value,
}

/// Output of [`JsPromise::fulfill`] / [`JsPromise::reject`].
///
/// The caller (typically the runtime's settlement path) is
/// responsible for pushing each entry onto the microtask queue.
/// Returning the jobs out of the trait means the trait stays
/// queue-agnostic — host-bridged impls can reuse the data
/// structure.
#[derive(Debug, Default)]
pub struct PromiseSettleJobs {
    /// Microtasks to enqueue, in FIFO order.
    pub jobs: Vec<Microtask>,
}

/// Output of [`JsPromise::perform_then`]. Either we attached a
/// pending reaction (no immediate work) or the promise was
/// already settled and one job needs to be enqueued.
#[derive(Debug, Default)]
pub struct PromiseThenOutcome {
    /// Set when the promise was already settled and the reaction
    /// runs in the next microtask cycle. `None` when the reaction
    /// was queued for future settlement.
    pub immediate_job: Option<Microtask>,
}

/// Spec-faithful contract every promise impl must satisfy.
///
/// Implementations:
/// - **`PurePromise`** — pure JS, foundation default.
/// - *(Phase F)* `BridgedFuturePromise` — wraps a Rust `Future`
///   and settles via the [`crate::AsyncRuntime`] when the future
///   resolves.
pub trait JsPromise: std::fmt::Debug {
    /// Snapshot of the current state. Returned by value so callers
    /// can avoid borrow-conflict with `fulfill` / `reject`.
    fn state(&self) -> PromiseState;

    /// Mark the promise fulfilled. No-op if already settled.
    /// Returns the microtask jobs that should drain on the next
    /// generation.
    fn fulfill(&self, value: Value) -> PromiseSettleJobs;

    /// Mark the promise rejected. No-op if already settled.
    fn reject(&self, reason: Value) -> PromiseSettleJobs;

    /// `PerformPromiseThen` (§27.2.5.4). Registers handlers
    /// against the matching capability.
    fn perform_then(
        &self,
        on_fulfilled: Option<Value>,
        on_rejected: Option<Value>,
        capability: PromiseCapability,
    ) -> PromiseThenOutcome;

    /// `true` once any reaction has been attached (used by
    /// "unhandled rejection" detection).
    fn is_handled(&self) -> bool;

    /// Identity comparison for `===`. Two handles are equal iff
    /// they share the same underlying state cell.
    fn ptr_eq(&self, other: &dyn JsPromise) -> bool;

    /// Downcast helper — every impl must also be `'static`. The
    /// foundation single-impl world doesn't strictly need this
    /// yet, but ptr-eq across multiple impls relies on it and the
    /// trait surface stays stable for Phase F additions.
    fn as_any(&self) -> &dyn std::any::Any;
}

/// Concrete spec-faithful promise. Holds its state behind an `Rc`
/// so multiple `Value` clones share one underlying body. The
/// inner `RefCell` is the foundation-era escape hatch — task 56
/// will replace it with a `&mut`-owned slot.
#[derive(Debug, Clone)]
pub struct PurePromise {
    inner: Rc<RefCell<PurePromiseBody>>,
}

#[derive(Debug)]
struct PurePromiseBody {
    state: PromiseState,
    fulfill_reactions: Vec<PromiseReaction>,
    reject_reactions: Vec<PromiseReaction>,
    is_handled: bool,
}

impl PurePromise {
    /// Construct a fresh pending promise.
    #[must_use]
    pub fn pending() -> Self {
        Self {
            inner: Rc::new(RefCell::new(PurePromiseBody {
                state: PromiseState::Pending,
                fulfill_reactions: Vec::new(),
                reject_reactions: Vec::new(),
                is_handled: false,
            })),
        }
    }

    /// Construct a promise pre-settled to fulfilled. Used by
    /// `Promise.resolve(v)` when `v` is not itself a thenable.
    #[must_use]
    pub fn fulfilled(value: Value) -> Self {
        Self {
            inner: Rc::new(RefCell::new(PurePromiseBody {
                state: PromiseState::Fulfilled(value),
                fulfill_reactions: Vec::new(),
                reject_reactions: Vec::new(),
                is_handled: false,
            })),
        }
    }

    /// Construct a promise pre-settled to rejected. Used by
    /// `Promise.reject(reason)`.
    #[must_use]
    pub fn rejected(reason: Value) -> Self {
        Self {
            inner: Rc::new(RefCell::new(PurePromiseBody {
                state: PromiseState::Rejected(reason),
                fulfill_reactions: Vec::new(),
                reject_reactions: Vec::new(),
                is_handled: false,
            })),
        }
    }
}

impl JsPromise for PurePromise {
    fn state(&self) -> PromiseState {
        self.inner.borrow().state.clone()
    }

    fn fulfill(&self, value: Value) -> PromiseSettleJobs {
        let reactions: Vec<PromiseReaction> = {
            let mut body = self.inner.borrow_mut();
            if body.state.is_settled() {
                return PromiseSettleJobs::default();
            }
            body.state = PromiseState::Fulfilled(value.clone());
            let taken = std::mem::take(&mut body.fulfill_reactions);
            body.reject_reactions.clear();
            taken
        };
        let jobs = reactions
            .into_iter()
            .map(|r| reaction_to_microtask(r, value.clone()))
            .collect();
        PromiseSettleJobs { jobs }
    }

    fn reject(&self, reason: Value) -> PromiseSettleJobs {
        let reactions: Vec<PromiseReaction> = {
            let mut body = self.inner.borrow_mut();
            if body.state.is_settled() {
                return PromiseSettleJobs::default();
            }
            body.state = PromiseState::Rejected(reason.clone());
            let taken = std::mem::take(&mut body.reject_reactions);
            body.fulfill_reactions.clear();
            taken
        };
        let jobs = reactions
            .into_iter()
            .map(|r| reaction_to_microtask(r, reason.clone()))
            .collect();
        PromiseSettleJobs { jobs }
    }

    fn perform_then(
        &self,
        on_fulfilled: Option<Value>,
        on_rejected: Option<Value>,
        capability: PromiseCapability,
    ) -> PromiseThenOutcome {
        let mut body = self.inner.borrow_mut();
        body.is_handled = true;
        match body.state.clone() {
            PromiseState::Pending => {
                body.fulfill_reactions.push(PromiseReaction {
                    capability: capability.clone(),
                    handler: on_fulfilled,
                    kind: ReactionKind::Fulfill,
                });
                body.reject_reactions.push(PromiseReaction {
                    capability,
                    handler: on_rejected,
                    kind: ReactionKind::Reject,
                });
                PromiseThenOutcome::default()
            }
            PromiseState::Fulfilled(value) => {
                let reaction = PromiseReaction {
                    capability,
                    handler: on_fulfilled,
                    kind: ReactionKind::Fulfill,
                };
                PromiseThenOutcome {
                    immediate_job: Some(reaction_to_microtask(reaction, value)),
                }
            }
            PromiseState::Rejected(reason) => {
                let reaction = PromiseReaction {
                    capability,
                    handler: on_rejected,
                    kind: ReactionKind::Reject,
                };
                PromiseThenOutcome {
                    immediate_job: Some(reaction_to_microtask(reaction, reason)),
                }
            }
        }
    }

    fn is_handled(&self) -> bool {
        self.inner.borrow().is_handled
    }

    fn ptr_eq(&self, other: &dyn JsPromise) -> bool {
        match other.as_any().downcast_ref::<PurePromise>() {
            Some(p) => Rc::ptr_eq(&self.inner, &p.inner),
            None => false,
        }
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

/// Concrete promise handle held by [`crate::Value::Promise`].
///
/// **Why a tagged enum, not `Rc<dyn JsPromise>`:** trait-object
/// dispatch costs a vtable indirection on every method call.
/// Real engines (V8 / JSC / SpiderMonkey) keep promises as a
/// concrete cell type with a tag. We do the same so the hot path
/// is direct dispatch.
///
/// **Why `Rc` at all:** foundation has no GC. Task 57 replaces
/// every `Rc` in `crates-next/*` with a `Gc<>` handle from a
/// tracing collector — that migration includes this type.
#[derive(Debug, Clone)]
pub struct JsPromiseHandle {
    inner: PromiseRepr,
}

#[derive(Debug, Clone)]
enum PromiseRepr {
    /// Foundation default — pure-JS spec-faithful promise.
    Pure(PurePromise),
    // Phase F additions:
    // /// Future-bridged: settles when an `AsyncRuntime` future completes.
    // BridgedFuture(BridgedFuturePromise),
}

impl JsPromiseHandle {
    /// Wrap a [`PurePromise`] as the value-level handle.
    #[must_use]
    pub fn from_pure(p: PurePromise) -> Self {
        Self {
            inner: PromiseRepr::Pure(p),
        }
    }

    /// Convenience: pending pure promise.
    #[must_use]
    pub fn pending() -> Self {
        Self::from_pure(PurePromise::pending())
    }

    /// Convenience: pre-fulfilled pure promise.
    #[must_use]
    pub fn fulfilled(value: Value) -> Self {
        Self::from_pure(PurePromise::fulfilled(value))
    }

    /// Convenience: pre-rejected pure promise.
    #[must_use]
    pub fn rejected(reason: Value) -> Self {
        Self::from_pure(PurePromise::rejected(reason))
    }

    /// Borrow the underlying [`PurePromise`] when the handle is
    /// the foundation default. `None` for future bridged variants
    /// (Phase F).
    #[must_use]
    pub fn as_pure(&self) -> Option<&PurePromise> {
        match &self.inner {
            PromiseRepr::Pure(p) => Some(p),
        }
    }
}

impl JsPromise for JsPromiseHandle {
    fn state(&self) -> PromiseState {
        match &self.inner {
            PromiseRepr::Pure(p) => p.state(),
        }
    }

    fn fulfill(&self, value: Value) -> PromiseSettleJobs {
        match &self.inner {
            PromiseRepr::Pure(p) => p.fulfill(value),
        }
    }

    fn reject(&self, reason: Value) -> PromiseSettleJobs {
        match &self.inner {
            PromiseRepr::Pure(p) => p.reject(reason),
        }
    }

    fn perform_then(
        &self,
        on_fulfilled: Option<Value>,
        on_rejected: Option<Value>,
        capability: PromiseCapability,
    ) -> PromiseThenOutcome {
        match &self.inner {
            PromiseRepr::Pure(p) => p.perform_then(on_fulfilled, on_rejected, capability),
        }
    }

    fn is_handled(&self) -> bool {
        match &self.inner {
            PromiseRepr::Pure(p) => p.is_handled(),
        }
    }

    fn ptr_eq(&self, other: &dyn JsPromise) -> bool {
        // Compare via the concrete inner repr; only Pure today.
        match &self.inner {
            PromiseRepr::Pure(p) => p.ptr_eq(other),
        }
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

/// Convert a stored reaction + the settling value into a
/// [`Microtask`] ready for the queue.
///
/// When a handler is present, we set `result_capability` so the
/// runtime resolves the downstream promise with the handler's
/// return value (spec §27.2.5.4 step 11). When the handler is
/// missing (fall-through), the callee IS the capability's
/// resolve/reject directly so no extra wiring is needed.
fn reaction_to_microtask(reaction: PromiseReaction, value: Value) -> Microtask {
    use crate::microtask::MicrotaskCapability;
    use smallvec::smallvec;
    let (callee, result_capability) = match reaction.handler {
        Some(h) => (
            h,
            Some(MicrotaskCapability {
                resolve: reaction.capability.resolve.clone(),
                reject: reaction.capability.reject.clone(),
            }),
        ),
        None => match reaction.kind {
            ReactionKind::Fulfill => (reaction.capability.resolve.clone(), None),
            ReactionKind::Reject => (reaction.capability.reject.clone(), None),
        },
    };
    Microtask {
        callee,
        this_value: Value::Undefined,
        args: smallvec![value],
        result_capability,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::number::NumberValue;

    fn n(v: i32) -> Value {
        Value::Number(NumberValue::from_i32(v))
    }

    fn cap_for(p: &PurePromise) -> PromiseCapability {
        // Foundation tests use sentinel resolve/reject — the
        // dispatcher never actually invokes them in unit tests.
        PromiseCapability {
            promise: Value::Undefined,
            resolve: Value::Undefined,
            reject: Value::Undefined,
        }
        .with_promise(Value::Undefined) // identity helper for clarity
        .with_pp(p.clone())
    }

    impl PromiseCapability {
        fn with_promise(self, _p: Value) -> Self {
            self
        }
        fn with_pp(mut self, _p: PurePromise) -> Self {
            self.promise = Value::Undefined;
            self
        }
    }

    #[test]
    fn pending_promise_starts_pending() {
        let p = PurePromise::pending();
        assert!(matches!(p.state(), PromiseState::Pending));
        assert!(!p.is_handled());
    }

    #[test]
    fn fulfilled_promise_settles_and_rejects_no_op() {
        let p = PurePromise::pending();
        let jobs = p.fulfill(n(7));
        assert!(matches!(p.state(), PromiseState::Fulfilled(_)));
        assert!(jobs.jobs.is_empty()); // no pending reactions
        // Subsequent reject is a no-op.
        let no_jobs = p.reject(n(99));
        assert!(no_jobs.jobs.is_empty());
        assert!(matches!(p.state(), PromiseState::Fulfilled(_)));
    }

    #[test]
    fn rejected_promise_settles_and_fulfill_no_op() {
        let p = PurePromise::pending();
        p.reject(n(7));
        assert!(matches!(p.state(), PromiseState::Rejected(_)));
        p.fulfill(n(99));
        assert!(matches!(p.state(), PromiseState::Rejected(_)));
    }

    #[test]
    fn perform_then_on_pending_attaches_no_immediate_job() {
        let p = PurePromise::pending();
        let outcome = p.perform_then(None, None, cap_for(&p));
        assert!(outcome.immediate_job.is_none());
        assert!(p.is_handled());
    }

    #[test]
    fn perform_then_on_settled_returns_immediate_job() {
        let p = PurePromise::fulfilled(n(42));
        let outcome = p.perform_then(None, None, cap_for(&p));
        assert!(outcome.immediate_job.is_some());
    }

    #[test]
    fn fulfill_drains_pending_reactions_into_jobs() {
        let p = PurePromise::pending();
        // Attach two reactions while pending.
        p.perform_then(None, None, cap_for(&p));
        p.perform_then(None, None, cap_for(&p));
        let jobs = p.fulfill(n(11));
        // Two fulfill reactions → two jobs.
        assert_eq!(jobs.jobs.len(), 2);
    }

    #[test]
    fn ptr_eq_uses_handle_identity() {
        let p = PurePromise::pending();
        let clone = p.clone();
        assert!(p.ptr_eq(&clone as &dyn JsPromise));
        let other = PurePromise::pending();
        assert!(!p.ptr_eq(&other as &dyn JsPromise));
    }
}
