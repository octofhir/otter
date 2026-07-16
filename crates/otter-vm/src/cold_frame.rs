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
    AsyncFrameState, PendingBindFunction, PendingGetIterator, PendingIteratorNext,
    PendingToPrimitive, TryHandler,
};
use crate::{JsObject, UpvalueCell, Value};
use smallvec::SmallVec;

/// A non-throw abrupt completion (`return` / `break` / `continue`)
/// that must run intervening `finally` blocks before reaching its
/// target. Parked on the frame while a `finally` block executes; the
/// `EndFinally` opcode resumes the completion afterwards.
///
/// # See also
/// - <https://tc39.es/ecma262/#sec-try-statement-runtime-semantics-evaluation>
#[derive(Debug, Clone, Copy)]
pub enum AbruptKind {
    /// `return value;` — pop the frame yielding `value` once all
    /// enclosing `finally` blocks have run.
    Return(Value),
    /// `break` / `continue` — jump to `target` pc (intra-frame loop
    /// label) once the crossed `finally` blocks have run.
    Jump(u32),
}

/// Frame-local result of advancing an [`AbruptKind`] without changing the
/// owning ActivationStack shape.
pub(crate) enum AbruptFrameOutcome {
    /// Execution resumes in the same frame at its updated PC.
    Resume,
    /// No finally remains; the frame boundary must return this value.
    Return(Value),
}

/// A completion parked while its `finally` block runs (§14.15.3
/// Try-statement evaluation: B's completion replaces F when B is
/// abrupt). One entry per in-flight `finally`, innermost on top; the
/// paired `u32` in [`ColdFrame::parked_finally`] is the handler-stack
/// depth recorded at entry — an unwind that pops the handler stack
/// below that depth abandons the finally block, so the entry is
/// discarded and the new abrupt completion wins.
#[derive(Debug, Clone)]
pub enum ParkedFinally {
    /// Normal completion — `Op::EndFinally` falls through.
    Normal,
    /// In-flight exception that routed into the `finally`; re-thrown
    /// by `Op::EndFinally`.
    Throw(Value),
    /// Parked `return` / `break` / `continue` with its unwind floor;
    /// `Op::EndFinally` resumes the walk toward the target.
    Abrupt(AbruptKind, u32),
}

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
/// Ordinary synchronous frames never allocate this record. Suspension ownership
/// (`async_state` / `generator_owner`) lives here with the rest of the uncommon
/// control-flow state instead of inflating every hot [`crate::Frame`].
#[derive(Debug, Default, Clone)]
pub struct ColdFrame {
    /// Result promise owned by a regular async-function invocation. Return and
    /// uncaught-throw paths settle it instead of writing into a caller window.
    pub async_state: Option<AsyncFrameState>,
    /// Generator object whose currently active or parked body owns this frame.
    /// Yield/start/async-generator await paths use the backlink to transfer the
    /// frame back into the generator body.
    pub generator_owner: Option<crate::generator::JsGenerator>,
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
    /// Completions parked while `finally` blocks run, innermost on
    /// top. Each entry pairs the parked completion with the
    /// handler-stack depth at finally entry; unwinds that pop below
    /// that depth discard the entry (§14.15.3 — the finally's own
    /// abrupt completion replaces the parked one). Pushed by the
    /// unwind walks and by `Op::LeaveTry` on a finally handler
    /// (normal entry); popped by `Op::EndFinally`.
    pub parked_finally: SmallVec<[(ParkedFinally, u32); 2]>,
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
    /// Iterators whose `[[return]]` must run on an abrupt completion
    /// that exits their region (§7.4.9 IteratorClose). Each entry pairs
    /// the iterator with the `handlers` stack depth recorded when its
    /// region opened (`Op::IteratorCloseStart`). On throw-unwind a
    /// closer is run only when the catching handler sits *below* that
    /// depth (the throw genuinely leaves the region); a `try`/`catch`
    /// nested *inside* the region leaves the iterator open. Also drained
    /// innermost-first when a parked generator is resumed with
    /// `.return()`.
    pub active_iterator_closers: SmallVec<[(Value, u32); 2]>,
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
    /// Shared `this` binding cell for derived constructors. Arrow
    /// closures created in the constructor capture this cell so
    /// `super()` can bind the original constructor environment even
    /// when the arrow runs through a nested sync dispatch.
    pub derived_this_cell: Option<UpvalueCell>,
    /// Var-scoped bindings a direct eval introduced into this frame's
    /// variable environment at runtime (§19.2.1.3
    /// EvalDeclarationInstantiation step 16.b). Consulted by
    /// [`otter_bytecode::Op::LoadDynamic`] /
    /// [`otter_bytecode::Op::StoreDynamic`] /
    /// [`otter_bytecode::Op::TypeofDynamic`] before the global
    /// fallback, and folded into the caller scope handed to any later
    /// direct eval from the same frame.
    pub eval_vars: Option<Box<rustc_hash::FxHashMap<String, crate::UpvalueCell>>>,
    /// §9.1 variable-environment record for direct eval — the
    /// GC-owned, closure-shareable successor of `eval_vars`.
    /// Created at frame entry for `contains_direct_eval` functions;
    /// closures made in this frame capture the handle so
    /// eval-introduced bindings outlive the frame.
    pub eval_env: Option<crate::eval_env::EvalEnvHandle>,
}

impl ColdFrame {
    /// Whether this slot is logically empty (no cold state worth
    /// keeping).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.async_state.is_none()
            && self.generator_owner.is_none()
            && self.pending_to_primitive.is_none()
            && self.pending_bind_function.is_none()
            && self.pending_get_iterator.is_none()
            && self.pending_iterator_next.is_none()
            && self.parked_finally.is_empty()
            && self.construct_target.is_none()
            && self.new_target.is_none()
            && self.rest_args.is_empty()
            && self.incoming_args.is_empty()
            && self.handlers.is_empty()
            && self.active_iterator_closers.is_empty()
            && !self.is_derived_constructor
            && self.derived_this_cell.is_none()
            && self.eval_vars.is_none()
            && self.eval_env.is_none()
    }

    /// Trace GC slots reachable through cold protocol state.
    ///
    /// `crate::Frame::trace_frame_slots` traces hot fields; the
    /// caller must additionally trace this record for any frame whose
    /// `cold` slot is `Some`.
    pub fn trace_cold_slots(&self, visitor: &mut SlotVisitor<'_>) {
        if let Some(async_state) = &self.async_state {
            async_state.result_promise.trace_value_slots(visitor);
        }
        if let Some(owner) = &self.generator_owner {
            owner.trace_value_slots(visitor);
        }
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
        for (parked, _) in &self.parked_finally {
            match parked {
                ParkedFinally::Throw(v) | ParkedFinally::Abrupt(AbruptKind::Return(v), _) => {
                    v.trace_value_slots(visitor);
                }
                ParkedFinally::Normal | ParkedFinally::Abrupt(AbruptKind::Jump(_), _) => {}
            }
        }
        if let Some(obj) = &self.construct_target {
            let p = obj as *const JsObject as *mut otter_gc::raw::RawGc;
            visitor(p);
        }
        if let Some(v) = &self.new_target {
            v.trace_value_slots(visitor);
        }
        if let Some(cell) = &self.derived_this_cell {
            let p = cell as *const UpvalueCell as *mut otter_gc::raw::RawGc;
            visitor(p);
        }
        for v in &self.rest_args {
            v.trace_value_slots(visitor);
        }
        for v in &self.incoming_args {
            v.trace_value_slots(visitor);
        }
        for (v, _) in &self.active_iterator_closers {
            v.trace_value_slots(visitor);
        }
        if let Some(map) = &self.eval_vars {
            for cell in map.values() {
                let p = cell as *const crate::UpvalueCell as *mut otter_gc::raw::RawGc;
                visitor(p);
            }
        }
        if let Some(env) = &self.eval_env {
            let p = env as *const crate::eval_env::EvalEnvHandle as *mut otter_gc::raw::RawGc;
            visitor(p);
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

    /// Trace every pool slot. Released slots are reset to the empty
    /// default on release, so tracing them is a no-op; acquired slots are
    /// traced whether or not a stacked frame references them yet. This is
    /// what keeps a cold record filled during frame construction — before
    /// the frame is pushed onto any traced stack — alive and forwarded
    /// across a collection.
    pub fn trace_all(&self, visitor: &mut SlotVisitor<'_>) {
        for slot in &self.slots {
            slot.trace_cold_slots(visitor);
        }
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
