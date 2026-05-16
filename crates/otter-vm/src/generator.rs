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

use crate::{Frame, Value};
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
#[derive(Debug)]
pub struct GeneratorBody {
    /// `Some(frame)` when the generator can still resume; `None`
    /// once done or while an async-generator await owns the frame.
    pub frame: Option<Box<Frame>>,
    /// Register slot that the most recent `Op::Yield` paused on.
    pub resume_dst: u16,
    /// `true` once the body has returned, thrown, or had `.return()`
    /// invoked.
    pub done: bool,
    /// Most recent value yielded by the body.
    pub yielded: Option<crate::Value>,
    /// `true` for `async function*` generators.
    pub is_async: bool,
    /// Pending Promise capability for an in-flight async-generator
    /// request.
    pub pending_request: Option<crate::promise::PromiseCapability>,
}

impl otter_gc::SafeTraceable for GeneratorBody {
    const TYPE_TAG: u8 = GENERATOR_BODY_TYPE_TAG;

    fn trace_slots_safe(&self, visitor: &mut SlotVisitor<'_>) {
        if let Some(frame) = &self.frame {
            frame.trace_frame_slots(visitor);
        }
        if let Some(value) = &self.yielded {
            value.trace_value_slots(visitor);
        }
        if let Some(capability) = &self.pending_request {
            capability.promise.trace_value_slots(visitor);
            capability.resolve.trace_value_slots(visitor);
            capability.reject.trace_value_slots(visitor);
        }
    }
}

/// GC-managed async suspension payload.
#[derive(Debug)]
pub struct ParkedFrameBody {
    frame: Option<Box<Frame>>,
}

impl otter_gc::SafeTraceable for ParkedFrameBody {
    const TYPE_TAG: u8 = PARKED_FRAME_BODY_TYPE_TAG;

    fn trace_slots_safe(&self, visitor: &mut SlotVisitor<'_>) {
        if let Some(frame) = &self.frame {
            frame.trace_frame_slots(visitor);
        }
    }
}

impl JsGenerator {
    /// Allocate a fresh generator over `frame`.
    pub fn new(heap: &mut otter_gc::GcHeap, frame: Frame) -> Result<Self, otter_gc::OutOfMemory> {
        Ok(Self {
            inner: heap.alloc_old(GeneratorBody {
                frame: Some(Box::new(frame)),
                resume_dst: 0,
                done: false,
                yielded: None,
                is_async: false,
                pending_request: None,
            })?,
        })
    }

    /// Raw handle used by root tracing and write barriers.
    #[must_use]
    pub(crate) fn raw(&self) -> RawGc {
        self.inner.raw()
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
        let p = self as *const JsGenerator as *mut RawGc;
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

    /// Take the saved frame.
    pub fn take_frame(&self, heap: &mut otter_gc::GcHeap) -> Option<Box<Frame>> {
        heap.with_payload(self.inner, |body| body.frame.take())
    }

    /// Store a saved frame and resume metadata.
    pub fn park_after_yield(
        &self,
        heap: &mut otter_gc::GcHeap,
        frame: Frame,
        resume_dst: u16,
        yielded: crate::Value,
    ) {
        let barrier_value = yielded.clone();
        heap.with_payload(self.inner, |body| {
            body.frame = Some(Box::new(frame));
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

/// Allocate a parked async frame.
pub fn alloc_parked_frame(
    heap: &mut otter_gc::GcHeap,
    frame: Frame,
) -> Result<ParkedFrame, otter_gc::OutOfMemory> {
    heap.alloc_old(ParkedFrameBody {
        frame: Some(Box::new(frame)),
    })
}

pub(crate) fn parked_frame_register_is_object(
    parked: ParkedFrame,
    heap: &otter_gc::GcHeap,
    register: usize,
) -> bool {
    heap.read_payload(parked, |body| {
        body.frame
            .as_ref()
            .and_then(|frame| frame.registers.get(register))
            .is_some_and(|value| matches!(value, Value::Object(_)))
    })
}

/// Take a parked frame. Returns `None` if the twin reaction already
/// consumed it.
pub fn take_parked_frame(parked: ParkedFrame, heap: &mut otter_gc::GcHeap) -> Option<Box<Frame>> {
    heap.with_payload(parked, |body| body.frame.take())
}
