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

use crate::Frame;
use otter_gc::raw::{RawGc, SlotVisitor};

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
    /// request.
    #[pelt(via = trace_promise_capability)]
    pub pending_request: Option<crate::promise::PromiseCapability>,
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

fn trace_promise_capability(
    field: &Option<crate::promise::PromiseCapability>,
    visitor: &mut SlotVisitor<'_>,
) {
    if let Some(capability) = field {
        capability.promise.trace_value_slots(visitor);
        capability.resolve.trace_value_slots(visitor);
        capability.reject.trace_value_slots(visitor);
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
                is_async: false,
                prototype_override,
                pending_request: None,
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

    /// Set the async-generator flag.
    pub fn set_async(&self, heap: &mut otter_gc::GcHeap, is_async: bool) {
        heap.with_payload(self.inner, |body| body.is_async = is_async);
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
        });
        heap.record_write(self.inner, &barrier_value);
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

    /// Store a pending async-generator request.
    pub fn set_pending_request(
        &self,
        heap: &mut otter_gc::GcHeap,
        capability: crate::promise::PromiseCapability,
    ) {
        let barrier_capability = capability.clone();
        heap.with_payload(self.inner, |body| {
            body.pending_request = Some(capability);
        });
        heap.record_write(self.inner, &barrier_capability);
    }

    /// Clear the pending async-generator request.
    pub fn clear_pending_request(&self, heap: &mut otter_gc::GcHeap) {
        heap.with_payload(self.inner, |body| body.pending_request = None);
    }

    /// Take the pending async-generator request.
    pub fn take_pending_request(
        &self,
        heap: &mut otter_gc::GcHeap,
    ) -> Option<crate::promise::PromiseCapability> {
        heap.with_payload(self.inner, |body| body.pending_request.take())
    }

    /// `true` when a pending request exists.
    #[must_use]
    pub fn has_pending_request(&self, heap: &otter_gc::GcHeap) -> bool {
        heap.read_payload(self.inner, |body| body.pending_request.is_some())
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
