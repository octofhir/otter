//! ECMA-262 §27 Generator and parked-frame state.
//!
//! Generator objects and async `await` suspension both park full VM frames off
//! the active interpreter stack. Those parked states are GC-managed so locals,
//! register windows, `this`, and pending promise capabilities are ordinary
//! traceable roots.
//!
//! # Contents
//!
//! - [`JsGenerator`] / [`GeneratorBody`] — generator object state.
//! - [`ParkedFrame`] / [`ParkedFrameBody`] — async-await suspension
//!   payload shared by promise reactions until settlement.
//!
//! # Invariants
//!
//! - Every operation that reads or mutates a body receives an explicit
//!   [`otter_gc::GcHeap`].
//! - Stored frames are isolate-owned. They are traced through the GC
//!   body and are moved back onto an interpreter stack only by the
//!   microtask drain.
//! - A [`ParkedFrame`] is single-shot: the first settling reaction
//!   takes the frame; the twin reaction observes `None`.
//!
//! # See also
//!
//! - <https://tc39.es/ecma262/#sec-generator-objects>
//! - <https://tc39.es/ecma262/#await>
//! - [GC API](../../../docs/book/src/engine/gc-api.md)

use crate::{Frame, GeneratorResumeKind};
use otter_gc::raw::{RawGc, SlotVisitor};
use std::collections::VecDeque;

/// Reserved [`otter_gc::Traceable::TYPE_TAG`] for [`GeneratorBody`].
pub const GENERATOR_BODY_TYPE_TAG: u8 = 0x1a;

/// Reserved [`otter_gc::Traceable::TYPE_TAG`] for [`ParkedFrameBody`].
pub const PARKED_FRAME_BODY_TYPE_TAG: u8 = 0x1b;

/// Cheap-to-clone generator handle.
#[derive(Debug, Clone, Copy)]
pub struct JsGenerator {
    inner: otter_gc::Gc<GeneratorBody>,
}

/// GC-managed parked async frame handle.
pub type ParkedFrame = otter_gc::Gc<ParkedFrameBody>;

/// Async-generator scheduling state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AsyncGeneratorState {
    /// Body is parked before its first user statement.
    SuspendedStart,
    /// Body is parked at an async-generator `yield`.
    SuspendedYield,
    /// Body is running on an interpreter stack.
    Executing,
    /// Body is parked on an awaited promise.
    Awaiting,
    /// Body finished; queued requests drain as done.
    Draining,
    /// No frame or queued work remains.
    Completed,
}

/// Queued `.next` / `.return` / `.throw` request.
#[derive(Debug, Clone)]
pub struct AsyncGeneratorRequest {
    /// Resume operation to inject into the generator body.
    pub resume: GeneratorResumeKind,
    /// Promise capability returned from the public async-generator method.
    pub capability: crate::promise::PromiseCapability,
}

/// Internal generator storage.
#[derive(Debug, otter_macros::Pelt)]
#[pelt(tag = GENERATOR_BODY_TYPE_TAG)]
pub struct GeneratorBody {
    /// `Some(frame)` when the generator can still resume; `None`
    /// once done or while an async-generator await owns the frame.
    #[pelt(via = trace_generator_frame)]
    pub frame: Option<Box<Frame>>,
    /// Detached cold record for the suspended frame. Acquired by
    /// the interpreter at yield time via
    /// [`crate::Interpreter::frame_detach_cold`]; re-attached on
    /// resume so try handlers, async parking, and other cold state
    /// survive the suspension.
    #[pelt(via = trace_generator_cold)]
    pub cold: Option<Box<crate::cold_frame::ColdFrame>>,
    /// Register slot that the most recent `Op::Yield` paused on.
    #[pelt(skip)]
    pub resume_dst: u16,
    /// Register receiving the resume KIND (0 = next, 1 = throw,
    /// 2 = return) when the frame is parked on `Op::YieldDelegate`
    /// (§27.5.3.7 `yield*` — abrupt resumes forward to the inner
    /// iterator instead of unwinding the generator body).
    #[pelt(skip)]
    pub resume_kind_dst: u16,
    /// `true` while the frame is parked on `Op::YieldDelegate`: the
    /// yielded value is the inner iterator result and must surface
    /// from `.next()` verbatim (no re-wrapping), and `.throw()` /
    /// `.return()` resume the body with a kind code instead of
    /// throwing / completing.
    #[pelt(skip)]
    pub delegating: bool,
    /// `true` once the body has returned, thrown, or had `.return()`
    /// invoked.
    #[pelt(skip)]
    pub done: bool,
    /// Most recent value yielded by the body.
    pub yielded: Option<crate::Value>,
    /// `true` for `async function*` generators.
    #[pelt(skip)]
    pub is_async: bool,
    /// `[[Prototype]]` captured from the generator function's own
    /// `prototype` property at call time.
    pub prototype_override: Option<crate::Value>,
    /// Pending Promise capability for an in-flight async-generator
    /// request queue.
    #[pelt(via = trace_async_generator_queue)]
    pub async_requests: VecDeque<AsyncGeneratorRequest>,
    /// Async-generator state. Ignored for sync generators.
    #[pelt(skip)]
    pub async_state: AsyncGeneratorState,
}

fn trace_generator_frame(field: &Option<Box<Frame>>, visitor: &mut SlotVisitor<'_>) {
    if let Some(frame) = field {
        frame.trace_frame_slots(visitor);
    }
}

fn trace_generator_cold(
    field: &Option<Box<crate::cold_frame::ColdFrame>>,
    visitor: &mut SlotVisitor<'_>,
) {
    if let Some(cold) = field {
        cold.trace_cold_slots(visitor);
    }
}

fn trace_async_generator_queue(
    field: &VecDeque<AsyncGeneratorRequest>,
    visitor: &mut SlotVisitor<'_>,
) {
    for request in field {
        match &request.resume {
            GeneratorResumeKind::Next(value)
            | GeneratorResumeKind::Return(value)
            | GeneratorResumeKind::Throw(value) => value.trace_value_slots(visitor),
        }
        request.capability.promise.trace_value_slots(visitor);
        request.capability.resolve.trace_value_slots(visitor);
        request.capability.reject.trace_value_slots(visitor);
    }
}

/// GC-managed async suspension payload.
///
/// `frame.cold` is `None` while parked — the cold record is detached
/// out of the interpreter pool at suspend time and stored alongside
/// the frame so pool slots can be reused while the parked frame
/// sleeps. The matching attach happens on resume.
#[derive(Debug, otter_macros::Pelt)]
#[pelt(tag = PARKED_FRAME_BODY_TYPE_TAG)]
pub struct ParkedFrameBody {
    #[pelt(via = trace_generator_frame)]
    frame: Option<Box<Frame>>,
    #[pelt(via = trace_generator_cold)]
    cold: Option<Box<crate::cold_frame::ColdFrame>>,
}

impl JsGenerator {
    /// Allocate a fresh generator over `frame`.
    pub fn new(heap: &mut otter_gc::GcHeap, frame: Frame) -> Result<Self, otter_gc::OutOfMemory> {
        Self::new_with_prototype(heap, frame, None)
    }

    /// Allocate a fresh generator over `frame` with the call-time
    /// generator prototype.
    pub fn new_with_prototype(
        heap: &mut otter_gc::GcHeap,
        frame: Frame,
        prototype_override: Option<crate::Value>,
    ) -> Result<Self, otter_gc::OutOfMemory> {
        Ok(Self {
            inner: heap.alloc_old(GeneratorBody {
                frame: Some(Box::new(frame)),
                cold: None,
                resume_dst: 0,
                done: false,
                yielded: None,
                resume_kind_dst: 0,
                delegating: false,
                is_async: false,
                prototype_override,
                async_requests: VecDeque::new(),
                async_state: AsyncGeneratorState::SuspendedStart,
            })?,
        })
    }

    /// Raw handle used by root tracing and write barriers.
    #[must_use]
    pub(crate) fn raw(&self) -> RawGc {
        self.inner.raw()
    }

    /// Reinterpret a body handle as the public [`JsGenerator`] wrapper.
    /// Used by [`crate::value::Value::as_generator`] after a
    /// `GcHeader::type_tag` check has confirmed the body is a
    /// [`GeneratorBody`].
    #[inline]
    #[must_use]
    pub fn from_gc(inner: otter_gc::Gc<GeneratorBody>) -> Self {
        Self { inner }
    }

    /// Stable identity token.
    #[must_use]
    pub fn identity_addr(&self) -> *const () {
        self.inner.as_header_ptr() as *const ()
    }

    /// Identity comparison.
    #[must_use]
    pub fn ptr_eq(&self, other: &Self) -> bool {
        self.inner == other.inner
    }

    /// Trace this handle as a root slot.
    pub(crate) fn trace_value_slots(&self, visitor: &mut SlotVisitor<'_>) {
        let p = &self.inner as *const otter_gc::Gc<GeneratorBody> as *mut RawGc;
        visitor(p);
    }

    /// Read-only body access.
    pub fn with_body<R>(&self, heap: &otter_gc::GcHeap, f: impl FnOnce(&GeneratorBody) -> R) -> R {
        heap.read_payload(self.inner, f)
    }

    /// §9.1.14 GetPrototypeFromConstructor outcome — installed AFTER
    /// FunctionDeclarationInstantiation runs (parameter side effects
    /// may replace `fn.prototype` first). `None` falls back to the
    /// realm's shared `%GeneratorPrototype%` / `%AsyncGeneratorPrototype%`.
    pub fn set_prototype_override(&self, heap: &mut otter_gc::GcHeap, proto: Option<crate::Value>) {
        heap.with_payload(self.inner, |body| {
            body.prototype_override = proto;
        });
        if let Some(value) = proto {
            heap.record_write(self.inner, &value);
        }
    }

    /// Set the async-generator flag.
    pub fn set_async(&self, heap: &mut otter_gc::GcHeap, is_async: bool) {
        heap.with_payload(self.inner, |body| {
            body.is_async = is_async;
            if is_async {
                body.async_state = AsyncGeneratorState::SuspendedStart;
            }
        });
    }

    /// `true` for async generators.
    #[must_use]
    pub fn is_async(&self, heap: &otter_gc::GcHeap) -> bool {
        heap.read_payload(self.inner, |body| body.is_async)
    }

    /// Call-time `[[Prototype]]` override.
    #[must_use]
    pub fn prototype_override(&self, heap: &otter_gc::GcHeap) -> Option<crate::Value> {
        heap.read_payload(self.inner, |body| body.prototype_override)
    }

    /// `true` when a frame is currently saved on the generator.
    #[must_use]
    pub fn has_frame(&self, heap: &otter_gc::GcHeap) -> bool {
        heap.read_payload(self.inner, |body| body.frame.is_some())
    }

    /// `true` once the body ran to completion (or was force-finished).
    /// A generator with no parked frame that is *not* done is
    /// currently executing (§27.5.3.2 GeneratorValidate).
    #[must_use]
    pub fn is_done(&self, heap: &otter_gc::GcHeap) -> bool {
        heap.read_payload(self.inner, |body| body.done)
    }

    /// Resume destination register.
    #[must_use]
    pub fn resume_dst(&self, heap: &otter_gc::GcHeap) -> u16 {
        heap.read_payload(self.inner, |body| body.resume_dst)
    }

    /// Take the saved frame along with its detached cold record.
    pub fn take_frame(
        &self,
        heap: &mut otter_gc::GcHeap,
    ) -> Option<(Box<Frame>, Option<Box<crate::cold_frame::ColdFrame>>)> {
        heap.with_payload(self.inner, |body| {
            let frame = body.frame.take()?;
            let cold = body.cold.take();
            Some((frame, cold))
        })
    }

    /// Store a saved frame, its detached cold record, and resume metadata.
    pub fn park_after_yield(
        &self,
        heap: &mut otter_gc::GcHeap,
        frame: Frame,
        cold: Option<Box<crate::cold_frame::ColdFrame>>,
        resume_dst: u16,
        yielded: crate::Value,
    ) {
        let barrier_value = yielded;
        heap.with_payload(self.inner, |body| {
            body.frame = Some(Box::new(frame));
            body.cold = cold;
            body.resume_dst = resume_dst;
            body.yielded = Some(yielded);
            body.delegating = false;
        });
        heap.record_write(self.inner, &barrier_value);
    }

    /// Park the frame on an `Op::YieldDelegate` suspension
    /// (§27.5.3.7). `yielded` is the inner iterator result object,
    /// surfaced from `.next()` verbatim; resume writes the kind code
    /// into `kind_dst` and the resume argument into `value_dst`.
    pub fn park_after_yield_delegate(
        &self,
        heap: &mut otter_gc::GcHeap,
        frame: Frame,
        cold: Option<Box<crate::cold_frame::ColdFrame>>,
        kind_dst: u16,
        value_dst: u16,
        yielded: crate::Value,
    ) {
        let barrier_value = yielded;
        heap.with_payload(self.inner, |body| {
            body.frame = Some(Box::new(frame));
            body.cold = cold;
            body.resume_kind_dst = kind_dst;
            body.resume_dst = value_dst;
            body.yielded = Some(yielded);
            body.delegating = true;
        });
        heap.record_write(self.inner, &barrier_value);
    }

    /// `true` while parked on `Op::YieldDelegate`.
    #[must_use]
    pub fn is_delegating(&self, heap: &otter_gc::GcHeap) -> bool {
        heap.read_payload(self.inner, |body| body.delegating)
    }

    /// Resume-kind destination register for a delegating park.
    #[must_use]
    pub fn resume_kind_dst(&self, heap: &otter_gc::GcHeap) -> u16 {
        heap.read_payload(self.inner, |body| body.resume_kind_dst)
    }

    /// Clear the delegating flag (called when the frame is taken
    /// for a delegating resume).
    pub fn clear_delegating(&self, heap: &mut otter_gc::GcHeap) {
        heap.with_payload(self.inner, |body| body.delegating = false);
    }

    /// Sentinel `resume_dst` for a frame parked at `GeneratorStart`:
    /// §27.5.3.3 discards the value passed to the first `next()`, so
    /// resume must not write it into any register (register 0 is a
    /// live local — e.g. the `arguments` binding).
    pub const RESUME_DST_NONE: u16 = u16::MAX;

    /// Store a frame that is suspended before the first body
    /// statement runs.
    pub fn park_frame(
        &self,
        heap: &mut otter_gc::GcHeap,
        frame: Frame,
        cold: Option<Box<crate::cold_frame::ColdFrame>>,
    ) {
        heap.with_payload(self.inner, |body| {
            body.frame = Some(Box::new(frame));
            body.cold = cold;
            body.resume_dst = Self::RESUME_DST_NONE;
            body.delegating = false;
            body.yielded = None;
        });
    }

    /// Take the last yielded value.
    pub fn take_yielded(&self, heap: &mut otter_gc::GcHeap) -> Option<crate::Value> {
        heap.with_payload(self.inner, |body| body.yielded.take())
    }

    /// `true` when `yielded` is still populated.
    #[must_use]
    pub fn has_yielded(&self, heap: &otter_gc::GcHeap) -> bool {
        heap.read_payload(self.inner, |body| body.yielded.is_some())
    }

    /// Mark done and clear the saved frame.
    pub fn mark_done(&self, heap: &mut otter_gc::GcHeap) {
        heap.with_payload(self.inner, |body| {
            body.done = true;
            body.frame = None;
        });
    }

    /// Clear the saved frame without marking done.
    pub fn clear_frame(&self, heap: &mut otter_gc::GcHeap) {
        heap.with_payload(self.inner, |body| body.frame = None);
    }

    /// Read async-generator scheduling state.
    #[must_use]
    pub fn async_state(&self, heap: &otter_gc::GcHeap) -> AsyncGeneratorState {
        heap.read_payload(self.inner, |body| body.async_state)
    }

    /// Set async-generator scheduling state.
    pub fn set_async_state(&self, heap: &mut otter_gc::GcHeap, state: AsyncGeneratorState) {
        heap.with_payload(self.inner, |body| body.async_state = state);
    }

    /// Enqueue an async-generator request.
    pub fn enqueue_async_request(
        &self,
        heap: &mut otter_gc::GcHeap,
        resume: GeneratorResumeKind,
        capability: crate::promise::PromiseCapability,
    ) {
        let barrier_resume = resume.clone();
        let barrier_capability = capability.clone();
        heap.with_payload(self.inner, |body| {
            body.async_requests
                .push_back(AsyncGeneratorRequest { resume, capability });
        });
        match barrier_resume {
            GeneratorResumeKind::Next(value)
            | GeneratorResumeKind::Return(value)
            | GeneratorResumeKind::Throw(value) => heap.record_write(self.inner, &value),
        }
        heap.record_write(self.inner, &barrier_capability);
    }

    /// Clear all queued async-generator requests.
    pub fn clear_async_requests(&self, heap: &mut otter_gc::GcHeap) {
        heap.with_payload(self.inner, |body| body.async_requests.clear());
    }

    /// Pop the front async-generator request.
    pub fn pop_async_request(&self, heap: &mut otter_gc::GcHeap) -> Option<AsyncGeneratorRequest> {
        heap.with_payload(self.inner, |body| body.async_requests.pop_front())
    }

    /// Clone the front async-generator resume request.
    #[must_use]
    pub fn front_async_resume(&self, heap: &otter_gc::GcHeap) -> Option<GeneratorResumeKind> {
        heap.read_payload(self.inner, |body| {
            body.async_requests
                .front()
                .map(|request| request.resume.clone())
        })
    }

    /// `true` when queued async-generator requests exist.
    #[must_use]
    pub fn has_async_requests(&self, heap: &otter_gc::GcHeap) -> bool {
        heap.read_payload(self.inner, |body| !body.async_requests.is_empty())
    }

    /// Install the generator self-reference into the saved frame.
    pub fn install_owner_on_frame(&self, heap: &mut otter_gc::GcHeap) {
        heap.with_payload(self.inner, |body| {
            if let Some(frame) = body.frame.as_mut() {
                frame.generator_owner = Some(*self);
            }
        });
    }
}

impl PartialEq for JsGenerator {
    fn eq(&self, other: &Self) -> bool {
        self.ptr_eq(other)
    }
}

/// Allocate a parked async frame. `cold` is the detached cold record
/// pulled out of the interpreter's pool at suspend time (None if the
/// frame had no cold state).
pub fn alloc_parked_frame(
    heap: &mut otter_gc::GcHeap,
    frame: Frame,
    cold: Option<Box<crate::cold_frame::ColdFrame>>,
) -> Result<ParkedFrame, otter_gc::OutOfMemory> {
    heap.alloc_old(ParkedFrameBody {
        frame: Some(Box::new(frame)),
        cold,
    })
}

/// Take a parked frame along with its detached cold record. Returns
/// `None` if the twin reaction already consumed it.
pub fn take_parked_frame(
    parked: ParkedFrame,
    heap: &mut otter_gc::GcHeap,
) -> Option<(Box<Frame>, Option<Box<crate::cold_frame::ColdFrame>>)> {
    heap.with_payload(parked, |body| {
        let frame = body.frame.take()?;
        let cold = body.cold.take();
        Some((frame, cold))
    })
}
