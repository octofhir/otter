//! ECMA-262 promise state for the new-engine VM.
//!
//! Promises are heap-shared JavaScript objects with a small state
//! machine and reaction queues. The active engine stores pure promise
//! state in old-space GC bodies; all reads and writes take an explicit
//! [`otter_gc::GcHeap`] so there is no hidden thread-local heap lookup.
//!
//! # Contents
//!
//! - [`JsPromise`] — promise operation contract.
//! - [`JsPromiseHandle`] / [`PurePromise`] — value-level handle and
//!   GC-managed pure promise body.
//! - [`PromiseState`] — pending / fulfilled / rejected.
//! - [`PromiseReaction`] — queued reaction payloads, including
//!   explicit parked-frame resume payloads for async functions.
//! - [`PromiseCapability`] — `{ promise, resolve, reject }`.
//!
//! # Invariants
//!
//! - Settlement is one-shot: fulfilling or rejecting an already
//!   settled promise is a no-op.
//! - Reactions are FIFO within the selected fulfill/reject bucket.
//! - Promise bodies trace their settled value, queued reactions,
//!   reaction capabilities, and parked-frame payloads.
//! - Storing a GC-bearing value into a promise body fires a write
//!   barrier against the owning body.
//!
//! # See also
//!
//! - <https://tc39.es/ecma262/#sec-promise-objects>
//! - [GC architecture plan §4.2](../../../docs/new-engine/gc-architecture.md)
//! - [Task 82](../../../docs/new-engine/tasks/82-migrate-promise-iterator-generator.md)

use crate::Value;
use crate::microtask::{Microtask, MicrotaskKind};

/// Reserved [`otter_gc::Traceable::TYPE_TAG`] for [`PurePromiseBody`].
pub const PURE_PROMISE_BODY_TYPE_TAG: u8 = 0x19;

/// One of three terminal states. Once `Fulfilled` or `Rejected`,
/// the promise never transitions again.
#[derive(Debug, Clone, PartialEq)]
pub enum PromiseState {
    /// No settlement decided yet.
    Pending,
    /// Settled with a fulfillment value.
    Fulfilled(Value),
    /// Settled with a rejection reason.
    Rejected(Value),
}

impl PromiseState {
    /// `true` when this state is not [`Self::Pending`].
    #[must_use]
    pub fn is_settled(&self) -> bool {
        !matches!(self, Self::Pending)
    }

    fn trace_value_slots(&self, visitor: &mut otter_gc::SlotVisitor<'_>) {
        match self {
            Self::Fulfilled(value) | Self::Rejected(value) => value.trace_value_slots(visitor),
            Self::Pending => {}
        }
    }
}

/// One reaction registered via `.then` / `.catch` / `.finally`.
#[derive(Debug, Clone)]
pub struct PromiseReaction {
    /// Downstream capability this reaction settles into.
    pub capability: PromiseCapability,
    /// Work to perform when this reaction runs.
    pub handler: PromiseReactionHandler,
    /// Which side of `then` this reaction handles.
    pub kind: ReactionKind,
}

impl PromiseReaction {
    fn trace_value_slots(&self, visitor: &mut otter_gc::SlotVisitor<'_>) {
        self.capability.promise.trace_value_slots(visitor);
        self.capability.resolve.trace_value_slots(visitor);
        self.capability.reject.trace_value_slots(visitor);
        self.handler.trace_value_slots(visitor);
    }
}

/// Payload stored in a promise reaction.
#[derive(Debug, Clone)]
pub enum PromiseReactionHandler {
    /// JS callback for this kind. `None` means identity (fulfill)
    /// or rethrow (reject), per spec.
    Call(Option<Value>),
    /// Resume a parked async function frame when the awaited promise
    /// settles.
    AsyncResume {
        /// GC-managed parked frame shared by the fulfill/reject
        /// reaction pair.
        parked: crate::generator::ParkedFrame,
        /// Register inside the parked frame that receives the
        /// settlement value.
        await_dst: u16,
        /// `true` for the fulfilment reaction.
        fulfilled: bool,
    },
    /// Resume a parked async-generator frame.
    AsyncGenResume {
        /// GC-managed parked frame shared by the fulfill/reject pair.
        parked: crate::generator::ParkedFrame,
        /// Register inside the parked frame that receives the
        /// settlement value.
        await_dst: u16,
        /// Owning generator.
        owner: crate::generator::JsGenerator,
        /// `true` for the fulfilment reaction.
        fulfilled: bool,
    },
}

impl PromiseReactionHandler {
    fn trace_value_slots(&self, visitor: &mut otter_gc::SlotVisitor<'_>) {
        match self {
            Self::Call(Some(handler)) => handler.trace_value_slots(visitor),
            Self::Call(None) => {}
            Self::AsyncResume { parked, .. } => {
                let p = parked as *const crate::generator::ParkedFrame as *mut otter_gc::RawGc;
                visitor(p);
            }
            Self::AsyncGenResume { parked, owner, .. } => {
                let p = parked as *const crate::generator::ParkedFrame as *mut otter_gc::RawGc;
                visitor(p);
                owner.trace_value_slots(visitor);
            }
        }
    }

    fn gc_children(&self) -> impl Iterator<Item = otter_gc::RawGc> + '_ {
        let mut out: smallvec::SmallVec<[otter_gc::RawGc; 4]> = smallvec::SmallVec::new();
        match self {
            Self::Call(Some(handler)) => {
                if let Some(raw) = handler.as_gc_raw() {
                    out.push(raw);
                }
            }
            Self::Call(None) => {}
            Self::AsyncResume { parked, .. } => out.push(parked.raw()),
            Self::AsyncGenResume { parked, owner, .. } => {
                out.push(parked.raw());
                out.push(owner.raw());
            }
        }
        out.into_iter()
    }
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
/// `NewPromiseCapability` (spec §27.2.1.5).
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
#[derive(Debug, Default)]
pub struct PromiseSettleJobs {
    /// Microtasks to enqueue, in FIFO order.
    pub jobs: Vec<Microtask>,
}

/// Output of [`JsPromise::perform_then`].
#[derive(Debug, Default)]
pub struct PromiseThenOutcome {
    /// Set when the promise was already settled and the reaction runs
    /// in the next microtask cycle.
    pub immediate_job: Option<Microtask>,
}

#[derive(Debug, Default)]
struct ThenOutcomeInternal {
    immediate_reaction: Option<(PromiseReaction, Value)>,
    stored: smallvec::SmallVec<[PromiseReaction; 2]>,
}

/// Spec-faithful contract every promise impl must satisfy.
pub trait JsPromise: std::fmt::Debug {
    /// Snapshot the current state.
    fn state(&self, heap: &otter_gc::GcHeap) -> PromiseState;

    /// Mark fulfilled. No-op if already settled.
    fn fulfill(&self, heap: &mut otter_gc::GcHeap, value: Value) -> PromiseSettleJobs;

    /// Mark rejected. No-op if already settled.
    fn reject(&self, heap: &mut otter_gc::GcHeap, reason: Value) -> PromiseSettleJobs;

    /// `PerformPromiseThen` (§27.2.5.4).
    fn perform_then(
        &self,
        heap: &mut otter_gc::GcHeap,
        on_fulfilled: Option<Value>,
        on_rejected: Option<Value>,
        capability: PromiseCapability,
    ) -> PromiseThenOutcome;

    /// `true` once any reaction has been attached.
    fn is_handled(&self, heap: &otter_gc::GcHeap) -> bool;

    /// Identity comparison for `===`.
    fn ptr_eq(&self, other: &dyn JsPromise) -> bool;

    /// Downcast helper.
    fn as_any(&self) -> &dyn std::any::Any;
}

/// Concrete spec-faithful promise handle.
#[derive(Debug, Clone, Copy)]
pub struct PurePromise {
    inner: otter_gc::Gc<PurePromiseBody>,
}

/// GC-allocated pure promise body.
#[derive(Debug)]
pub struct PurePromiseBody {
    state: PromiseState,
    fulfill_reactions: Vec<PromiseReaction>,
    reject_reactions: Vec<PromiseReaction>,
    is_handled: bool,
}

impl otter_gc::SafeTraceable for PurePromiseBody {
    const TYPE_TAG: u8 = PURE_PROMISE_BODY_TYPE_TAG;

    fn trace_slots_safe(&self, visitor: &mut otter_gc::SlotVisitor<'_>) {
        self.state.trace_value_slots(visitor);
        for reaction in self
            .fulfill_reactions
            .iter()
            .chain(self.reject_reactions.iter())
        {
            reaction.trace_value_slots(visitor);
        }
    }
}

impl PurePromise {
    /// Construct a fresh pending promise.
    pub fn pending(heap: &mut otter_gc::GcHeap) -> Result<Self, otter_gc::OutOfMemory> {
        Ok(Self {
            inner: heap.alloc_old(PurePromiseBody {
                state: PromiseState::Pending,
                fulfill_reactions: Vec::new(),
                reject_reactions: Vec::new(),
                is_handled: false,
            })?,
        })
    }

    /// Construct a pre-fulfilled promise.
    pub fn fulfilled(
        heap: &mut otter_gc::GcHeap,
        value: Value,
    ) -> Result<Self, otter_gc::OutOfMemory> {
        Ok(Self {
            inner: heap.alloc_old(PurePromiseBody {
                state: PromiseState::Fulfilled(value),
                fulfill_reactions: Vec::new(),
                reject_reactions: Vec::new(),
                is_handled: false,
            })?,
        })
    }

    /// Construct a pre-rejected promise.
    pub fn rejected(
        heap: &mut otter_gc::GcHeap,
        reason: Value,
    ) -> Result<Self, otter_gc::OutOfMemory> {
        Ok(Self {
            inner: heap.alloc_old(PurePromiseBody {
                state: PromiseState::Rejected(reason),
                fulfill_reactions: Vec::new(),
                reject_reactions: Vec::new(),
                is_handled: false,
            })?,
        })
    }

    /// Raw handle used by root tracing.
    #[must_use]
    pub fn raw(&self) -> otter_gc::RawGc {
        self.inner.raw()
    }

    /// Stable identity token.
    #[must_use]
    pub fn identity_addr(&self) -> *const () {
        self.inner.as_header_ptr() as *const ()
    }

    /// Attach explicit parked-frame reactions for `await`.
    pub fn perform_async_resume_then(
        &self,
        heap: &mut otter_gc::GcHeap,
        parked: crate::generator::ParkedFrame,
        await_dst: u16,
        capability: PromiseCapability,
        owner: Option<crate::generator::JsGenerator>,
    ) -> PromiseThenOutcome {
        let outcome = self.perform_then_internal(heap, capability, |kind| {
            let fulfilled = kind == ReactionKind::Fulfill;
            match owner {
                Some(owner) => PromiseReactionHandler::AsyncGenResume {
                    parked,
                    await_dst,
                    owner,
                    fulfilled,
                },
                None => PromiseReactionHandler::AsyncResume {
                    parked,
                    await_dst,
                    fulfilled,
                },
            }
        });
        self.finish_then_outcome(heap, outcome)
    }

    fn perform_then_internal(
        &self,
        heap: &mut otter_gc::GcHeap,
        capability: PromiseCapability,
        mut handler_for: impl FnMut(ReactionKind) -> PromiseReactionHandler,
    ) -> ThenOutcomeInternal {
        heap.with_payload(self.inner, |body| {
            body.is_handled = true;
            match body.state.clone() {
                PromiseState::Pending => {
                    let fulfill = PromiseReaction {
                        capability: capability.clone(),
                        handler: handler_for(ReactionKind::Fulfill),
                        kind: ReactionKind::Fulfill,
                    };
                    let reject = PromiseReaction {
                        capability,
                        handler: handler_for(ReactionKind::Reject),
                        kind: ReactionKind::Reject,
                    };
                    body.fulfill_reactions.push(fulfill.clone());
                    body.reject_reactions.push(reject.clone());
                    ThenOutcomeInternal {
                        immediate_reaction: None,
                        stored: smallvec::smallvec![fulfill, reject],
                    }
                }
                PromiseState::Fulfilled(value) => {
                    let reaction = PromiseReaction {
                        capability,
                        handler: handler_for(ReactionKind::Fulfill),
                        kind: ReactionKind::Fulfill,
                    };
                    ThenOutcomeInternal {
                        immediate_reaction: Some((reaction, value)),
                        stored: smallvec::SmallVec::new(),
                    }
                }
                PromiseState::Rejected(reason) => {
                    let reaction = PromiseReaction {
                        capability,
                        handler: handler_for(ReactionKind::Reject),
                        kind: ReactionKind::Reject,
                    };
                    ThenOutcomeInternal {
                        immediate_reaction: Some((reaction, reason)),
                        stored: smallvec::SmallVec::new(),
                    }
                }
            }
        })
    }

    fn finish_then_outcome(
        &self,
        heap: &mut otter_gc::GcHeap,
        outcome: ThenOutcomeInternal,
    ) -> PromiseThenOutcome {
        for reaction in &outcome.stored {
            record_reaction_barriers(self.inner, heap, reaction);
        }
        PromiseThenOutcome {
            immediate_job: outcome
                .immediate_reaction
                .and_then(|(reaction, value)| reaction_to_microtask(heap, reaction, value)),
        }
    }
}

impl JsPromise for PurePromise {
    fn state(&self, heap: &otter_gc::GcHeap) -> PromiseState {
        heap.read_payload(self.inner, |body| body.state.clone())
    }

    fn fulfill(&self, heap: &mut otter_gc::GcHeap, value: Value) -> PromiseSettleJobs {
        let child_raw = value.as_gc_raw();
        let reactions: Vec<PromiseReaction> = heap.with_payload(self.inner, |body| {
            if body.state.is_settled() {
                return Vec::new();
            }
            body.state = PromiseState::Fulfilled(value.clone());
            let taken = std::mem::take(&mut body.fulfill_reactions);
            body.reject_reactions.clear();
            taken
        });
        if let Some(child) = child_raw {
            heap.write_barrier_raw(self.inner, promise_payload_slot(self.inner), child);
        }
        PromiseSettleJobs {
            jobs: reactions
                .into_iter()
                .filter_map(|r| reaction_to_microtask(heap, r, value.clone()))
                .collect(),
        }
    }

    fn reject(&self, heap: &mut otter_gc::GcHeap, reason: Value) -> PromiseSettleJobs {
        let child_raw = reason.as_gc_raw();
        let reactions: Vec<PromiseReaction> = heap.with_payload(self.inner, |body| {
            if body.state.is_settled() {
                return Vec::new();
            }
            body.state = PromiseState::Rejected(reason.clone());
            let taken = std::mem::take(&mut body.reject_reactions);
            body.fulfill_reactions.clear();
            taken
        });
        if let Some(child) = child_raw {
            heap.write_barrier_raw(self.inner, promise_payload_slot(self.inner), child);
        }
        PromiseSettleJobs {
            jobs: reactions
                .into_iter()
                .filter_map(|r| reaction_to_microtask(heap, r, reason.clone()))
                .collect(),
        }
    }

    fn perform_then(
        &self,
        heap: &mut otter_gc::GcHeap,
        on_fulfilled: Option<Value>,
        on_rejected: Option<Value>,
        capability: PromiseCapability,
    ) -> PromiseThenOutcome {
        let outcome = self.perform_then_internal(heap, capability, |kind| match kind {
            ReactionKind::Fulfill => PromiseReactionHandler::Call(on_fulfilled.clone()),
            ReactionKind::Reject => PromiseReactionHandler::Call(on_rejected.clone()),
        });
        self.finish_then_outcome(heap, outcome)
    }

    fn is_handled(&self, heap: &otter_gc::GcHeap) -> bool {
        heap.read_payload(self.inner, |body| body.is_handled)
    }

    fn ptr_eq(&self, other: &dyn JsPromise) -> bool {
        if let Some(other) = other.as_any().downcast_ref::<PurePromise>() {
            return self.inner == other.inner;
        }
        if let Some(other) = other.as_any().downcast_ref::<JsPromiseHandle>() {
            return other.as_pure().is_some_and(|p| p.inner == self.inner);
        }
        false
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

/// Concrete promise handle held by [`crate::Value::Promise`].
#[derive(Debug, Clone, Copy)]
pub struct JsPromiseHandle {
    inner: PromiseRepr,
}

#[derive(Debug, Clone, Copy)]
enum PromiseRepr {
    /// Pure-JS spec-faithful promise.
    Pure(PurePromise),
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
    pub fn pending(heap: &mut otter_gc::GcHeap) -> Result<Self, otter_gc::OutOfMemory> {
        Ok(Self::from_pure(PurePromise::pending(heap)?))
    }

    /// Convenience: pre-fulfilled pure promise.
    pub fn fulfilled(
        heap: &mut otter_gc::GcHeap,
        value: Value,
    ) -> Result<Self, otter_gc::OutOfMemory> {
        Ok(Self::from_pure(PurePromise::fulfilled(heap, value)?))
    }

    /// Convenience: pre-rejected pure promise.
    pub fn rejected(
        heap: &mut otter_gc::GcHeap,
        reason: Value,
    ) -> Result<Self, otter_gc::OutOfMemory> {
        Ok(Self::from_pure(PurePromise::rejected(heap, reason)?))
    }

    /// Borrow the underlying pure promise.
    #[must_use]
    pub fn as_pure(&self) -> Option<PurePromise> {
        match self.inner {
            PromiseRepr::Pure(p) => Some(p),
        }
    }

    /// Raw handle used by root tracing.
    #[must_use]
    pub fn raw(&self) -> otter_gc::RawGc {
        match self.inner {
            PromiseRepr::Pure(p) => p.raw(),
        }
    }

    /// Stable identity token.
    #[must_use]
    pub fn identity_addr(&self) -> *const () {
        match self.inner {
            PromiseRepr::Pure(p) => p.identity_addr(),
        }
    }

    /// Trace this handle as a root slot.
    pub(crate) fn trace_value_slots(&self, visitor: &mut otter_gc::SlotVisitor<'_>) {
        let p = self as *const JsPromiseHandle as *mut otter_gc::RawGc;
        visitor(p);
    }

    /// Attach explicit parked-frame reactions for `await`.
    pub fn perform_async_resume_then(
        &self,
        heap: &mut otter_gc::GcHeap,
        parked: crate::generator::ParkedFrame,
        await_dst: u16,
        capability: PromiseCapability,
        owner: Option<crate::generator::JsGenerator>,
    ) -> PromiseThenOutcome {
        match self.inner {
            PromiseRepr::Pure(p) => {
                p.perform_async_resume_then(heap, parked, await_dst, capability, owner)
            }
        }
    }
}

impl JsPromise for JsPromiseHandle {
    fn state(&self, heap: &otter_gc::GcHeap) -> PromiseState {
        match self.inner {
            PromiseRepr::Pure(p) => p.state(heap),
        }
    }

    fn fulfill(&self, heap: &mut otter_gc::GcHeap, value: Value) -> PromiseSettleJobs {
        match self.inner {
            PromiseRepr::Pure(p) => p.fulfill(heap, value),
        }
    }

    fn reject(&self, heap: &mut otter_gc::GcHeap, reason: Value) -> PromiseSettleJobs {
        match self.inner {
            PromiseRepr::Pure(p) => p.reject(heap, reason),
        }
    }

    fn perform_then(
        &self,
        heap: &mut otter_gc::GcHeap,
        on_fulfilled: Option<Value>,
        on_rejected: Option<Value>,
        capability: PromiseCapability,
    ) -> PromiseThenOutcome {
        match self.inner {
            PromiseRepr::Pure(p) => p.perform_then(heap, on_fulfilled, on_rejected, capability),
        }
    }

    fn is_handled(&self, heap: &otter_gc::GcHeap) -> bool {
        match self.inner {
            PromiseRepr::Pure(p) => p.is_handled(heap),
        }
    }

    fn ptr_eq(&self, other: &dyn JsPromise) -> bool {
        if let Some(other) = other.as_any().downcast_ref::<JsPromiseHandle>() {
            return self.raw() == other.raw();
        }
        match self.inner {
            PromiseRepr::Pure(p) => p.ptr_eq(other),
        }
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

fn reaction_to_microtask(
    heap: &mut otter_gc::GcHeap,
    reaction: PromiseReaction,
    value: Value,
) -> Option<Microtask> {
    use crate::microtask::MicrotaskCapability;
    use smallvec::smallvec;

    match reaction.handler {
        PromiseReactionHandler::Call(handler) => {
            let (callee, result_capability) = match handler {
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
            Some(Microtask {
                callee,
                this_value: Value::Undefined,
                args: smallvec![value],
                result_capability,
                kind: MicrotaskKind::Call,
            })
        }
        PromiseReactionHandler::AsyncResume {
            parked,
            await_dst,
            fulfilled,
        } => {
            let frame = crate::generator::take_parked_frame(parked, heap)?;
            Some(Microtask {
                callee: Value::Undefined,
                this_value: Value::Undefined,
                args: smallvec![value],
                result_capability: None,
                kind: MicrotaskKind::AsyncResume {
                    frame,
                    await_dst,
                    fulfilled,
                },
            })
        }
        PromiseReactionHandler::AsyncGenResume {
            parked,
            await_dst,
            owner,
            fulfilled,
        } => {
            let frame = crate::generator::take_parked_frame(parked, heap)?;
            Some(Microtask {
                callee: Value::Undefined,
                this_value: Value::Undefined,
                args: smallvec![value],
                result_capability: None,
                kind: MicrotaskKind::AsyncGenResume {
                    frame,
                    await_dst,
                    fulfilled,
                    owner,
                },
            })
        }
    }
}

fn record_reaction_barriers(
    parent: otter_gc::Gc<PurePromiseBody>,
    heap: &mut otter_gc::GcHeap,
    reaction: &PromiseReaction,
) {
    let slot = promise_payload_slot(parent);
    for child in [
        reaction.capability.promise.as_gc_raw(),
        reaction.capability.resolve.as_gc_raw(),
        reaction.capability.reject.as_gc_raw(),
    ]
    .into_iter()
    .flatten()
    .chain(reaction.handler.gc_children())
    {
        heap.write_barrier_raw(parent, slot, child);
    }
}

fn promise_payload_slot(promise: otter_gc::Gc<PurePromiseBody>) -> *mut otter_gc::RawGc {
    let body_base = promise.as_header_ptr() as *mut u8;
    body_base.wrapping_add(std::mem::size_of::<otter_gc::GcHeader>()) as *mut otter_gc::RawGc
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::number::NumberValue;

    fn n(v: i32) -> Value {
        Value::Number(NumberValue::from_i32(v))
    }

    fn cap_for(heap: &mut otter_gc::GcHeap) -> PromiseCapability {
        let p = JsPromiseHandle::pending(heap).expect("cap promise");
        PromiseCapability {
            promise: Value::Promise(p),
            resolve: Value::Undefined,
            reject: Value::Undefined,
        }
    }

    #[test]
    fn pending_promise_starts_pending() {
        let mut heap = otter_gc::GcHeap::new().expect("heap");
        let p = PurePromise::pending(&mut heap).expect("promise");
        assert!(matches!(p.state(&heap), PromiseState::Pending));
        assert!(!p.is_handled(&heap));
    }

    #[test]
    fn fulfilled_promise_settles_and_rejects_no_op() {
        let mut heap = otter_gc::GcHeap::new().expect("heap");
        let p = PurePromise::pending(&mut heap).expect("promise");
        let jobs = p.fulfill(&mut heap, n(7));
        assert!(matches!(p.state(&heap), PromiseState::Fulfilled(_)));
        assert!(jobs.jobs.is_empty());
        let no_jobs = p.reject(&mut heap, n(99));
        assert!(no_jobs.jobs.is_empty());
        assert!(matches!(p.state(&heap), PromiseState::Fulfilled(_)));
    }

    #[test]
    fn rejected_promise_settles_and_fulfill_no_op() {
        let mut heap = otter_gc::GcHeap::new().expect("heap");
        let p = PurePromise::pending(&mut heap).expect("promise");
        p.reject(&mut heap, n(7));
        assert!(matches!(p.state(&heap), PromiseState::Rejected(_)));
        p.fulfill(&mut heap, n(99));
        assert!(matches!(p.state(&heap), PromiseState::Rejected(_)));
    }

    #[test]
    fn perform_then_on_pending_attaches_no_immediate_job() {
        let mut heap = otter_gc::GcHeap::new().expect("heap");
        let p = PurePromise::pending(&mut heap).expect("promise");
        let cap = cap_for(&mut heap);
        let outcome = p.perform_then(&mut heap, None, None, cap);
        assert!(outcome.immediate_job.is_none());
        assert!(p.is_handled(&heap));
    }

    #[test]
    fn perform_then_on_settled_returns_immediate_job() {
        let mut heap = otter_gc::GcHeap::new().expect("heap");
        let p = PurePromise::fulfilled(&mut heap, n(42)).expect("promise");
        let cap = cap_for(&mut heap);
        let outcome = p.perform_then(&mut heap, None, None, cap);
        assert!(outcome.immediate_job.is_some());
    }

    #[test]
    fn fulfill_drains_pending_reactions_into_jobs() {
        let mut heap = otter_gc::GcHeap::new().expect("heap");
        let p = PurePromise::pending(&mut heap).expect("promise");
        let cap = cap_for(&mut heap);
        p.perform_then(&mut heap, None, None, cap);
        let cap = cap_for(&mut heap);
        p.perform_then(&mut heap, None, None, cap);
        let jobs = p.fulfill(&mut heap, n(11));
        assert_eq!(jobs.jobs.len(), 2);
    }

    #[test]
    fn ptr_eq_uses_handle_identity() {
        let mut heap = otter_gc::GcHeap::new().expect("heap");
        let p = PurePromise::pending(&mut heap).expect("promise");
        let clone = p;
        assert!(p.ptr_eq(&clone as &dyn JsPromise));
        let other = PurePromise::pending(&mut heap).expect("promise");
        assert!(!p.ptr_eq(&other as &dyn JsPromise));
    }
}
