//! ECMA-262 promise state for the new-engine VM.
//!
//! Promises are heap-shared JavaScript objects with a small state
//! machine and reaction queues. Active VM/runtime paths can allocate pure
//! promise state through root-aware young-generation helpers; legacy external
//! helpers still take an explicit [`otter_gc::GcHeap`] so there is no hidden
//! thread-local heap lookup.
//!
//! # Contents
//!
//! - [`JsPromise`] — promise operation contract.
//! - [`JsPromiseHandle`] / [`PurePromise`] — value-level handle and
//!   GC-managed pure promise body.
//! - [`PromiseState`] — pending / fulfilled / rejected.
//! - [`PromiseReaction`] — queued reaction payloads, including
//!   explicit parked-frame resume payloads for async functions.
//! - [`PromiseCapability`] — `{ promise, resolve, reject }`
//!   plus the VM context that owns later settlement work.
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
//! - [GC API](../../../docs/book/src/engine/gc-api.md)

use crate::Value;
use crate::execution_context::ExecutionContext;
use crate::microtask::{Microtask, MicrotaskKind};
use otter_gc::heap::RootSlotVisitor;
use otter_gc::raw::{RawGc, SlotVisitor};

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

    fn trace_value_slots(&self, visitor: &mut SlotVisitor<'_>) {
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
    /// Execution context that registered this reaction.
    pub context: Option<ExecutionContext>,
}

impl PromiseReaction {
    fn trace_value_slots(&self, visitor: &mut SlotVisitor<'_>) {
        self.capability.promise.trace_value_slots(visitor);
        self.capability.resolve.trace_value_slots(visitor);
        self.capability.reject.trace_value_slots(visitor);
        self.handler.trace_value_slots(visitor);
    }
}

impl otter_gc::GcStore for PromiseReaction {
    fn visit_gc_edges(&self, visitor: &mut dyn FnMut(otter_gc::GcEdge)) {
        self.capability.visit_gc_edges(visitor);
        self.handler.visit_gc_edges(visitor);
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
    fn trace_value_slots(&self, visitor: &mut SlotVisitor<'_>) {
        match self {
            Self::Call(Some(handler)) => handler.trace_value_slots(visitor),
            Self::Call(None) => {}
            Self::AsyncResume { parked, .. } => {
                let p = parked as *const crate::generator::ParkedFrame as *mut RawGc;
                visitor(p);
            }
            Self::AsyncGenResume { parked, owner, .. } => {
                let p = parked as *const crate::generator::ParkedFrame as *mut RawGc;
                visitor(p);
                owner.trace_value_slots(visitor);
            }
        }
    }
}

impl otter_gc::GcStore for PromiseReactionHandler {
    fn visit_gc_edges(&self, visitor: &mut dyn FnMut(otter_gc::GcEdge)) {
        match self {
            Self::Call(Some(handler)) => {
                handler.visit_gc_edges(visitor);
            }
            Self::Call(None) => {}
            Self::AsyncResume { parked, .. } => {
                if let Some(edge) = otter_gc::GcEdge::from_gc(*parked) {
                    visitor(edge);
                }
            }
            Self::AsyncGenResume { parked, owner, .. } => {
                if let Some(edge) = otter_gc::GcEdge::from_gc(*parked) {
                    visitor(edge);
                }
                if let Some(edge) = otter_gc::GcEdge::from_raw(owner.raw()) {
                    visitor(edge);
                }
            }
        }
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
    /// VM context captured when the capability was created.
    pub context: Option<ExecutionContext>,
}

impl otter_gc::GcStore for PromiseCapability {
    fn visit_gc_edges(&self, visitor: &mut dyn FnMut(otter_gc::GcEdge)) {
        self.promise.visit_gc_edges(visitor);
        self.resolve.visit_gc_edges(visitor);
        self.reject.visit_gc_edges(visitor);
    }
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

    /// `PerformPromiseThen` with explicit context ownership for the
    /// queued reaction job.
    fn perform_then_with_context(
        &self,
        heap: &mut otter_gc::GcHeap,
        on_fulfilled: Option<Value>,
        on_rejected: Option<Value>,
        capability: PromiseCapability,
        context: Option<ExecutionContext>,
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
    /// Lazy expando bag for non-spec own properties such as
    /// user-installed `then` / `constructor` overrides per the
    /// Promise resolution-callback observation tests.
    expando: Option<crate::object::JsObject>,
    /// Per-instance `[[Prototype]]` override for Promise subclass
    /// construction.
    prototype_override: Option<Value>,
}

impl otter_gc::SafeTraceable for PurePromiseBody {
    const TYPE_TAG: u8 = PURE_PROMISE_BODY_TYPE_TAG;

    fn trace_slots_safe(&self, visitor: &mut SlotVisitor<'_>) {
        self.state.trace_value_slots(visitor);
        for reaction in self
            .fulfill_reactions
            .iter()
            .chain(self.reject_reactions.iter())
        {
            reaction.trace_value_slots(visitor);
        }
        if let Some(expando) = &self.expando {
            Value::object(*expando).trace_value_slots(visitor);
        }
        if let Some(proto) = &self.prototype_override {
            proto.trace_value_slots(visitor);
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
                expando: None,
                prototype_override: None,
            })?,
        })
    }

    /// Construct a fresh pending promise while exposing caller-owned roots.
    pub(crate) fn pending_with_roots(
        heap: &mut otter_gc::GcHeap,
        external_visit: &mut RootSlotVisitor<'_>,
    ) -> Result<Self, otter_gc::OutOfMemory> {
        Ok(Self {
            inner: heap.alloc_with_roots(
                PurePromiseBody {
                    state: PromiseState::Pending,
                    fulfill_reactions: Vec::new(),
                    reject_reactions: Vec::new(),
                    is_handled: false,
                    expando: None,
                    prototype_override: None,
                },
                external_visit,
            )?,
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
                expando: None,
                prototype_override: None,
            })?,
        })
    }

    /// Construct a pre-fulfilled promise while exposing caller-owned roots.
    pub(crate) fn fulfilled_with_roots(
        heap: &mut otter_gc::GcHeap,
        value: Value,
        external_visit: &mut RootSlotVisitor<'_>,
    ) -> Result<Self, otter_gc::OutOfMemory> {
        Ok(Self {
            inner: heap.alloc_with_roots(
                PurePromiseBody {
                    state: PromiseState::Fulfilled(value),
                    fulfill_reactions: Vec::new(),
                    reject_reactions: Vec::new(),
                    is_handled: false,
                    expando: None,
                    prototype_override: None,
                },
                external_visit,
            )?,
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
                expando: None,
                prototype_override: None,
            })?,
        })
    }

    /// Construct a pre-rejected promise while exposing caller-owned roots.
    pub(crate) fn rejected_with_roots(
        heap: &mut otter_gc::GcHeap,
        reason: Value,
        external_visit: &mut RootSlotVisitor<'_>,
    ) -> Result<Self, otter_gc::OutOfMemory> {
        Ok(Self {
            inner: heap.alloc_with_roots(
                PurePromiseBody {
                    state: PromiseState::Rejected(reason),
                    fulfill_reactions: Vec::new(),
                    reject_reactions: Vec::new(),
                    is_handled: false,
                    expando: None,
                    prototype_override: None,
                },
                external_visit,
            )?,
        })
    }

    /// Raw handle used by root tracing.
    #[must_use]
    pub(crate) fn raw(&self) -> RawGc {
        self.inner.raw()
    }

    /// Reinterpret a body handle as the public [`PurePromise`]
    /// wrapper. Used by [`crate::value::Value::as_promise`] after a
    /// `GcHeader::type_tag` check has confirmed the body is a
    /// [`PurePromiseBody`].
    #[inline]
    #[must_use]
    pub fn from_gc(inner: otter_gc::Gc<PurePromiseBody>) -> Self {
        Self { inner }
    }

    /// Stable identity token.
    #[must_use]
    pub fn identity_addr(&self) -> *const () {
        self.inner.as_header_ptr() as *const ()
    }

    /// Read the lazy expando bag, if one was created.
    #[must_use]
    pub fn expando(&self, heap: &otter_gc::GcHeap) -> Option<crate::object::JsObject> {
        heap.read_payload(self.inner, |body| body.expando)
    }

    /// Install (or replace) the lazy expando bag.
    pub fn set_expando(&self, heap: &mut otter_gc::GcHeap, expando: crate::object::JsObject) {
        let barrier = Value::object(expando);
        heap.with_payload(self.inner, |body| {
            body.expando = Some(expando);
        });
        heap.record_write(self.inner, &barrier);
    }

    #[must_use]
    pub(crate) fn prototype_override(&self, heap: &otter_gc::GcHeap) -> Option<Value> {
        heap.read_payload(self.inner, |body| body.prototype_override)
    }

    pub(crate) fn set_prototype_override(&self, heap: &mut otter_gc::GcHeap, proto: Option<Value>) {
        let barrier_value = proto;
        heap.with_payload(self.inner, |body| {
            body.prototype_override = proto;
        });
        if let Some(value) = &barrier_value {
            heap.record_write(self.inner, value);
        }
    }

    #[cfg(test)]
    pub(crate) fn debug_fulfill_reactions(&self, heap: &otter_gc::GcHeap) -> Vec<PromiseReaction> {
        heap.read_payload(self.inner, |body| body.fulfill_reactions.clone())
    }

    /// Attach explicit parked-frame reactions for `await`.
    pub fn perform_async_resume_then(
        &self,
        heap: &mut otter_gc::GcHeap,
        parked: crate::generator::ParkedFrame,
        await_dst: u16,
        capability: PromiseCapability,
        owner: Option<crate::generator::JsGenerator>,
        context: Option<ExecutionContext>,
    ) -> PromiseThenOutcome {
        let outcome = self.perform_then_internal(heap, capability, context, |kind| {
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
        context: Option<ExecutionContext>,
        mut handler_for: impl FnMut(ReactionKind) -> PromiseReactionHandler,
    ) -> ThenOutcomeInternal {
        let reaction_context = context.or_else(|| capability.context.clone());
        heap.with_payload(self.inner, |body| {
            body.is_handled = true;
            match body.state.clone() {
                PromiseState::Pending => {
                    let fulfill = PromiseReaction {
                        capability: capability.clone(),
                        handler: handler_for(ReactionKind::Fulfill),
                        kind: ReactionKind::Fulfill,
                        context: reaction_context.clone(),
                    };
                    let reject = PromiseReaction {
                        capability,
                        handler: handler_for(ReactionKind::Reject),
                        kind: ReactionKind::Reject,
                        context: reaction_context,
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
                        context: reaction_context,
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
                        context: reaction_context,
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
        let barrier_value = value;
        let reactions: Vec<PromiseReaction> = heap.with_payload(self.inner, |body| {
            if body.state.is_settled() {
                return Vec::new();
            }
            body.state = PromiseState::Fulfilled(value);
            let taken = std::mem::take(&mut body.fulfill_reactions);
            body.reject_reactions.clear();
            taken
        });
        heap.record_write(self.inner, &barrier_value);
        PromiseSettleJobs {
            jobs: reactions
                .into_iter()
                .filter_map(|r| reaction_to_microtask(heap, r, value))
                .collect(),
        }
    }

    fn reject(&self, heap: &mut otter_gc::GcHeap, reason: Value) -> PromiseSettleJobs {
        let barrier_reason = reason;
        let reactions: Vec<PromiseReaction> = heap.with_payload(self.inner, |body| {
            if body.state.is_settled() {
                return Vec::new();
            }
            body.state = PromiseState::Rejected(reason);
            let taken = std::mem::take(&mut body.reject_reactions);
            body.fulfill_reactions.clear();
            taken
        });
        heap.record_write(self.inner, &barrier_reason);
        PromiseSettleJobs {
            jobs: reactions
                .into_iter()
                .filter_map(|r| reaction_to_microtask(heap, r, reason))
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
        self.perform_then_with_context(heap, on_fulfilled, on_rejected, capability, None)
    }

    fn perform_then_with_context(
        &self,
        heap: &mut otter_gc::GcHeap,
        on_fulfilled: Option<Value>,
        on_rejected: Option<Value>,
        capability: PromiseCapability,
        context: Option<ExecutionContext>,
    ) -> PromiseThenOutcome {
        let outcome = self.perform_then_internal(heap, capability, context, |kind| match kind {
            ReactionKind::Fulfill => PromiseReactionHandler::Call(on_fulfilled),
            ReactionKind::Reject => PromiseReactionHandler::Call(on_rejected),
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

    /// Read the lazy expando bag — only populated by user
    /// installations such as `promise.then = fn`.
    #[must_use]
    pub fn expando(&self, heap: &otter_gc::GcHeap) -> Option<crate::object::JsObject> {
        match &self.inner {
            PromiseRepr::Pure(p) => p.expando(heap),
        }
    }

    /// Install / replace the lazy expando bag.
    pub fn set_expando(&self, heap: &mut otter_gc::GcHeap, expando: crate::object::JsObject) {
        match &self.inner {
            PromiseRepr::Pure(p) => p.set_expando(heap, expando),
        }
    }

    #[must_use]
    pub(crate) fn prototype_override(&self, heap: &otter_gc::GcHeap) -> Option<Value> {
        match &self.inner {
            PromiseRepr::Pure(p) => p.prototype_override(heap),
        }
    }

    pub(crate) fn set_prototype_override(&self, heap: &mut otter_gc::GcHeap, proto: Option<Value>) {
        match &self.inner {
            PromiseRepr::Pure(p) => p.set_prototype_override(heap, proto),
        }
    }

    /// Convenience: pending pure promise.
    pub fn pending(heap: &mut otter_gc::GcHeap) -> Result<Self, otter_gc::OutOfMemory> {
        Ok(Self::from_pure(PurePromise::pending(heap)?))
    }

    /// Convenience: pending pure promise with caller-owned roots.
    pub(crate) fn pending_with_roots(
        heap: &mut otter_gc::GcHeap,
        external_visit: &mut RootSlotVisitor<'_>,
    ) -> Result<Self, otter_gc::OutOfMemory> {
        Ok(Self::from_pure(PurePromise::pending_with_roots(
            heap,
            external_visit,
        )?))
    }

    /// Convenience: pre-fulfilled pure promise.
    pub fn fulfilled(
        heap: &mut otter_gc::GcHeap,
        value: Value,
    ) -> Result<Self, otter_gc::OutOfMemory> {
        Ok(Self::from_pure(PurePromise::fulfilled(heap, value)?))
    }

    /// Convenience: pre-fulfilled pure promise with caller-owned roots.
    pub(crate) fn fulfilled_with_roots(
        heap: &mut otter_gc::GcHeap,
        value: Value,
        external_visit: &mut RootSlotVisitor<'_>,
    ) -> Result<Self, otter_gc::OutOfMemory> {
        Ok(Self::from_pure(PurePromise::fulfilled_with_roots(
            heap,
            value,
            external_visit,
        )?))
    }

    /// Convenience: pre-rejected pure promise.
    pub fn rejected(
        heap: &mut otter_gc::GcHeap,
        reason: Value,
    ) -> Result<Self, otter_gc::OutOfMemory> {
        Ok(Self::from_pure(PurePromise::rejected(heap, reason)?))
    }

    /// Convenience: pre-rejected pure promise with caller-owned roots.
    pub(crate) fn rejected_with_roots(
        heap: &mut otter_gc::GcHeap,
        reason: Value,
        external_visit: &mut RootSlotVisitor<'_>,
    ) -> Result<Self, otter_gc::OutOfMemory> {
        Ok(Self::from_pure(PurePromise::rejected_with_roots(
            heap,
            reason,
            external_visit,
        )?))
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
    pub(crate) fn raw(&self) -> RawGc {
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

    #[cfg(test)]
    pub(crate) fn debug_fulfill_reactions(&self, heap: &otter_gc::GcHeap) -> Vec<PromiseReaction> {
        match self.inner {
            PromiseRepr::Pure(p) => p.debug_fulfill_reactions(heap),
        }
    }

    /// Trace this handle as a root slot.
    pub(crate) fn trace_value_slots(&self, visitor: &mut SlotVisitor<'_>) {
        match &self.inner {
            PromiseRepr::Pure(promise) => {
                let p = &promise.inner as *const otter_gc::Gc<PurePromiseBody> as *mut RawGc;
                visitor(p);
            }
        }
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
        self.perform_async_resume_then_with_context(
            heap, parked, await_dst, capability, owner, None,
        )
    }

    /// Attach parked-frame reactions for `await` with explicit
    /// context ownership.
    pub fn perform_async_resume_then_with_context(
        &self,
        heap: &mut otter_gc::GcHeap,
        parked: crate::generator::ParkedFrame,
        await_dst: u16,
        capability: PromiseCapability,
        owner: Option<crate::generator::JsGenerator>,
        context: Option<ExecutionContext>,
    ) -> PromiseThenOutcome {
        match self.inner {
            PromiseRepr::Pure(p) => {
                p.perform_async_resume_then(heap, parked, await_dst, capability, owner, context)
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

    fn perform_then_with_context(
        &self,
        heap: &mut otter_gc::GcHeap,
        on_fulfilled: Option<Value>,
        on_rejected: Option<Value>,
        capability: PromiseCapability,
        context: Option<ExecutionContext>,
    ) -> PromiseThenOutcome {
        match self.inner {
            PromiseRepr::Pure(p) => {
                p.perform_then_with_context(heap, on_fulfilled, on_rejected, capability, context)
            }
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
                        resolve: reaction.capability.resolve,
                        reject: reaction.capability.reject,
                    }),
                ),
                None => match reaction.kind {
                    ReactionKind::Fulfill => (reaction.capability.resolve, None),
                    ReactionKind::Reject => (reaction.capability.reject, None),
                },
            };
            Some(Microtask {
                callee,
                this_value: Value::undefined(),
                args: smallvec![value],
                context: reaction.context,
                result_capability,
                kind: MicrotaskKind::Call,
            })
        }
        PromiseReactionHandler::AsyncResume {
            parked,
            await_dst,
            fulfilled,
        } => {
            let (frame, cold) = crate::generator::take_parked_frame(parked, heap)?;
            Some(Microtask {
                callee: Value::undefined(),
                this_value: Value::undefined(),
                args: smallvec![value],
                context: reaction.context,
                result_capability: None,
                kind: MicrotaskKind::AsyncResume {
                    frame,
                    cold,
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
            let (frame, cold) = crate::generator::take_parked_frame(parked, heap)?;
            Some(Microtask {
                callee: Value::undefined(),
                this_value: Value::undefined(),
                args: smallvec![value],
                context: reaction.context,
                result_capability: None,
                kind: MicrotaskKind::AsyncGenResume {
                    frame,
                    cold,
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
    heap.record_write(parent, reaction);
}

#[cfg(test)]
mod tests {
    use super::*;
    use otter_bytecode::{BytecodeModule, SourceKind};

    fn n(v: i32) -> Value {
        Value::number_i32(v)
    }

    fn cap_for(heap: &mut otter_gc::GcHeap) -> PromiseCapability {
        let p = JsPromiseHandle::pending(heap).expect("cap promise");
        PromiseCapability {
            promise: Value::promise(p),
            resolve: Value::undefined(),
            reject: Value::undefined(),
            context: None,
        }
    }

    fn empty_context() -> ExecutionContext {
        ExecutionContext::from_module(BytecodeModule {
            module: "promise-test".to_string(),
            source_kind: SourceKind::JavaScript,
            functions: Vec::new(),
            constants: Vec::new(),
            module_resolutions: Vec::new(),
            module_inits: Vec::new(),
        })
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
    fn capability_context_flows_into_reaction_job() {
        let mut heap = otter_gc::GcHeap::new().expect("heap");
        let p = PurePromise::fulfilled(&mut heap, n(42)).expect("promise");
        let mut cap = cap_for(&mut heap);
        cap.context = Some(empty_context());
        let outcome = p.perform_then(&mut heap, None, None, cap);
        let job = outcome.immediate_job.expect("reaction job");
        assert!(job.context.is_some());
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
