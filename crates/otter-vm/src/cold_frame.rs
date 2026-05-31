//! Cold side records for interpreter call frames.
//!
//! Each [`crate::Frame`] carries an `Option<ColdFrameIdx>` slot. When
//! an opcode needs cold protocol state (try handlers, async parking,
//! pending ToPrimitive / bind / iterator ladder, etc.) it acquires a
//! slot from the per-interpreter [`ColdFramePool`] and writes through
//! it. Frames that never run a cold-state opcode (most short helpers,
//! arithmetic-only inner loops) never touch the pool.
//!
//! # Contents
//! - [`ColdFrame`] — the cold half of a call frame's bookkeeping.
//! - [`ColdFrameIdx`] — niche-encoded handle into the pool.
//! - [`ColdFramePool`] — Interpreter-owned slot + freelist storage.
//!
//! # Invariants
//! - `ColdFrameIdx` indexes `ColdFramePool::slots` as `idx.get() - 1`.
//! - Released slots are reset to [`ColdFrame::default`] so a freshly
//!   acquired slot never observes a previous frame's state.
//! - Frames parked off the dispatcher stack (async await / generator
//!   yield) **must** detach their cold record (see
//!   [`ColdFramePool::detach`]) before being stored on a heap-owned
//!   continuation, then re-attach via [`ColdFramePool::attach`] on
//!   resume. Pool indices are not stable across detach/attach.
//!
//! # See also
//! - [`crate::frame_state`]

use std::num::NonZeroU32;

use otter_gc::raw::SlotVisitor;

use crate::frame_state::{
    PendingBindFunction, PendingGetIterator, PendingIteratorNext, PendingToPrimitive, TryHandler,
};
use crate::{JsObject, Value};
use smallvec::SmallVec;

/// Niche-encoded handle into a [`ColdFramePool`]. Stored as
/// `Option<ColdFrameIdx>` (4 bytes) on the hot frame; `None` means no
/// cold record has been acquired yet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(transparent)]
pub struct ColdFrameIdx(NonZeroU32);

impl ColdFrameIdx {
    /// Index into `ColdFramePool::slots` (zero-based).
    #[inline]
    #[must_use]
    pub fn slot(self) -> usize {
        self.0.get() as usize - 1
    }

    #[inline]
    fn from_slot(slot: u32) -> Self {
        // Slot indices originate from `slots.len()` (already < u32::MAX
        // for any practical pool) — `slot + 1` cannot wrap.
        Self(NonZeroU32::new(slot + 1).expect("pool slot fits in u32"))
    }
}

/// Cold half of a call frame. Pool-allocated and shared via
/// [`ColdFrameIdx`].
///
/// Subsequent commits migrate the remaining cold fields (`handlers`,
/// `module_url`, `async_state`, `generator_owner`, …) into this
/// struct.
#[derive(Debug, Default, Clone)]
pub struct ColdFrame {
    /// State machine for the in-flight ECMA-262 §7.1.1 `ToPrimitive`
    /// ladder. See [`crate::frame_state::PendingToPrimitive`].
    pub pending_to_primitive: Option<PendingToPrimitive>,
    /// In-flight ECMA-262 §20.2.3.2 `Function.prototype.bind` metadata
    /// collection.
    pub pending_bind_function: Option<PendingBindFunction>,
    /// In-flight ECMA-262 §7.4.3 `GetIterator` over a user object.
    pub pending_get_iterator: Option<PendingGetIterator>,
    /// In-flight ECMA-262 §7.4.5 `IteratorNext` over a user iterator.
    pub pending_iterator_next: Option<PendingIteratorNext>,
    /// In-flight exception parked when a throw routed into a `finally`
    /// block. [`otter_bytecode::Op::EndFinally`] consumes it: `Some`
    /// re-throws, `None` falls through.
    pub pending_throw: Option<Value>,
    /// Newly-allocated receiver when this frame was entered via
    /// `Op::New`. On return, the dispatcher substitutes this object
    /// for any non-object return value so constructors that don't
    /// `return` a replacement still hand the caller the fresh instance.
    pub construct_target: Option<JsObject>,
    /// `new.target` visible to the active function body. Set only for
    /// frames entered through `[[Construct]]`.
    pub new_target: Option<Value>,
    /// Trailing arguments past the declared `param_count`. Populated
    /// by the call dispatcher only when the callee declares a rest
    /// parameter; consumed by `Op::CollectRest`.
    pub rest_args: SmallVec<[Value; 4]>,
    /// Full incoming-argument list captured at call entry. Populated
    /// only when the callee was compiled with `needs_arguments`;
    /// consumed by `Op::CollectArguments`.
    pub incoming_args: SmallVec<[Value; 4]>,
    /// Active try-handler stack. Pushed by `Op::EnterTry`, popped by
    /// `Op::LeaveTry` or by exception unwind landing on a matching
    /// catch / finally. Innermost handler on top.
    pub handlers: SmallVec<[TryHandler; 4]>,
    /// Iterators that must be closed if a generator is parked inside
    /// destructuring and later resumed with `.return()`.
    pub active_iterator_closers: SmallVec<[Value; 2]>,
    /// `true` when this frame runs a *derived* class constructor.
    /// Its `this` starts in the TDZ (a `Value::hole()` in
    /// `Frame::this_value`) until `super(...)` runs
    /// [`otter_bytecode::Op::BindThisValue`]. The return path
    /// (`pop_frame`) consults this to apply §10.2.2 derived
    /// constructor return semantics: an object return is honoured
    /// verbatim, an undefined return yields the bound `this`, and an
    /// undefined return with `this` still in the TDZ is a
    /// `ReferenceError`.
    pub is_derived_constructor: bool,
}

impl ColdFrame {
    /// Whether this slot is logically empty (no cold state worth
    /// keeping).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.pending_to_primitive.is_none()
            && self.pending_bind_function.is_none()
            && self.pending_get_iterator.is_none()
            && self.pending_iterator_next.is_none()
            && self.pending_throw.is_none()
            && self.construct_target.is_none()
            && self.new_target.is_none()
            && self.rest_args.is_empty()
            && self.incoming_args.is_empty()
            && self.handlers.is_empty()
            && self.active_iterator_closers.is_empty()
            && !self.is_derived_constructor
    }

    /// Trace GC slots reachable through cold protocol state.
    ///
    /// `crate::Frame::trace_frame_slots` traces hot fields; the
    /// caller must additionally trace this record for any frame whose
    /// `cold` slot is `Some`.
    pub fn trace_cold_slots(&self, visitor: &mut SlotVisitor<'_>) {
        if let Some(p) = &self.pending_to_primitive {
            p.obj.trace_value_slots(visitor);
        }
        if let Some(p) = &self.pending_bind_function {
            p.target.trace_value_slots(visitor);
            p.bound_this.trace_value_slots(visitor);
            for arg in &p.bound_args {
                arg.trace_value_slots(visitor);
            }
            if let Some(name) = &p.target_name {
                name.trace_value_slots(visitor);
            }
        }
        if let Some(p) = &self.pending_iterator_next {
            p.iterator.trace_value_slots(visitor);
        }
        // `pending_get_iterator` carries only pc + dst, no values.
        if let Some(v) = &self.pending_throw {
            v.trace_value_slots(visitor);
        }
        if let Some(obj) = &self.construct_target {
            let p = obj as *const JsObject as *mut otter_gc::raw::RawGc;
            visitor(p);
        }
        if let Some(v) = &self.new_target {
            v.trace_value_slots(visitor);
        }
        for v in &self.rest_args {
            v.trace_value_slots(visitor);
        }
        for v in &self.incoming_args {
            v.trace_value_slots(visitor);
        }
        for v in &self.active_iterator_closers {
            v.trace_value_slots(visitor);
        }
    }
}

/// Per-interpreter pool of cold frame records.
///
/// Growth is monotonic; the freelist makes reuse O(1). Typical peak
/// occupancy is on the order of the live frame stack depth (dozens),
/// so the backing `Vec<ColdFrame>` stays small.
#[derive(Debug, Default)]
pub struct ColdFramePool {
    slots: Vec<ColdFrame>,
    free: Vec<u32>,
}

impl ColdFramePool {
    /// Construct an empty pool. Equivalent to [`Self::default`].
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Drop every pool slot. Called when the interpreter is reset
    /// between top-level runs.
    pub fn clear(&mut self) {
        self.slots.clear();
        self.free.clear();
    }

    /// Acquire a fresh, zeroed slot and return its handle.
    pub fn acquire(&mut self) -> ColdFrameIdx {
        if let Some(slot) = self.free.pop() {
            // Slot was reset on release; nothing to do here.
            return ColdFrameIdx::from_slot(slot);
        }
        let slot = u32::try_from(self.slots.len()).expect("cold-frame pool fits in u32");
        self.slots.push(ColdFrame::default());
        ColdFrameIdx::from_slot(slot)
    }

    /// Hand a slot back to the freelist. Resets the slot so future
    /// callers never observe leftover state.
    pub fn release(&mut self, idx: ColdFrameIdx) {
        let slot = idx.slot();
        self.slots[slot] = ColdFrame::default();
        self.free.push(slot as u32);
    }

    /// Detach an owned cold record from the pool. Used when a frame is
    /// parked off the dispatcher stack (async await, generator yield)
    /// and must carry its cold state on a heap-owned continuation.
    pub fn detach(&mut self, idx: ColdFrameIdx) -> ColdFrame {
        let slot = idx.slot();
        let owned = std::mem::take(&mut self.slots[slot]);
        self.free.push(slot as u32);
        owned
    }

    /// Re-attach an owned cold record into the pool. Used on
    /// async/generator resume to restore the parked cold state.
    pub fn attach(&mut self, cold: ColdFrame) -> ColdFrameIdx {
        let idx = self.acquire();
        self.slots[idx.slot()] = cold;
        idx
    }

    /// Borrow a slot. `None` is not possible for valid `idx`; the
    /// option shape is for caller ergonomics with `Option<ColdFrameIdx>`.
    #[inline]
    #[must_use]
    pub fn get(&self, idx: ColdFrameIdx) -> &ColdFrame {
        &self.slots[idx.slot()]
    }

    /// Mutable borrow of a slot.
    #[inline]
    #[must_use]
    pub fn get_mut(&mut self, idx: ColdFrameIdx) -> &mut ColdFrame {
        &mut self.slots[idx.slot()]
    }

    /// Number of live (acquired but not released) slots.
    #[must_use]
    pub fn live_len(&self) -> usize {
        self.slots.len() - self.free.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn acquire_release_round_trip() {
        let mut pool = ColdFramePool::new();
        let a = pool.acquire();
        let b = pool.acquire();
        assert_ne!(a, b);
        assert_eq!(pool.live_len(), 2);
        pool.release(a);
        assert_eq!(pool.live_len(), 1);
        let c = pool.acquire();
        // Freelist reuse: c should reclaim a's slot.
        assert_eq!(c, a);
        pool.release(b);
        pool.release(c);
        assert_eq!(pool.live_len(), 0);
    }

    #[test]
    fn idx_size_is_four() {
        // Hot frame depends on this niche encoding for its size goal.
        assert_eq!(
            std::mem::size_of::<Option<ColdFrameIdx>>(),
            std::mem::size_of::<u32>(),
        );
    }

    #[test]
    fn detach_attach_round_trip() {
        let mut pool = ColdFramePool::new();
        let a = pool.acquire();
        let cold = pool.detach(a);
        assert!(cold.is_empty());
        // Detach freed the slot; next acquire reuses it.
        let b = pool.acquire();
        assert_eq!(a, b);
        let _re = pool.attach(cold);
    }
}
