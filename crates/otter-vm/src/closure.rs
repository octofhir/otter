//! GC body for closure values.
//!
//! A closure carries three things:
//!
//! - the bytecode function id it executes,
//! - the captured upvalue spine (one [`crate::UpvalueCell`] per
//!   binding, in declaration order),
//! - an optional bound `this` (arrow closures capture their receiver
//!   lexically; non-arrow closures take `this` from the call site).
//!
//! # Contents
//!
//! - [`JsClosureBody`] — GC body holding `function_id`, the upvalue
//!   slice, and `bound_this`.
//! - [`JsClosure`] — 4-byte `Gc<JsClosureBody>` handle.
//! - [`alloc_closure`] / [`alloc_closure_with_roots`] — allocators.
//! - [`JS_CLOSURE_BODY_TYPE_TAG`] — reserved
//!   [`otter_gc::Traceable::TYPE_TAG`].
//!
//! # Invariants
//!
//! - Upvalue slice is built once at closure creation
//!   ([`Op::MakeClosure`](otter_bytecode::Op::MakeClosure)) and never
//!   resized; per-cell mutation flows through
//!   [`crate::store_upvalue`] / [`crate::read_upvalue`].
//! - `bound_this == None` → take `this` from the call site (non-arrow).
//!   `Some(value)` → override any caller-supplied receiver (arrow).
//! - Trace walks every upvalue handle plus the `bound_this` value
//!   slots.
//!
//! # Spec
//!
//! - ECMA-262 §15.2.5 — closure environment construction.
//! - ECMA-262 §13.3.6 — `[[Call]]` for ordinary functions / closures.
//! - ECMA-262 §10.2.1.1 — `[[ThisMode]]` for arrow functions.

use otter_gc::GcHeap;
use otter_gc::OutOfMemory;
use otter_gc::heap::RootSlotVisitor;
use otter_gc::raw::{RawGc, SlotVisitor};

use crate::{UpvalueCell, Value};

/// Reserved [`otter_gc::Traceable::TYPE_TAG`] for [`JsClosureBody`].
pub const JS_CLOSURE_BODY_TYPE_TAG: u8 = 0x23;

/// GC body backing every closure value.
#[derive(Debug)]
pub struct JsClosureBody {
    /// Index into [`otter_bytecode::BytecodeModule::functions`].
    pub function_id: u32,
    /// Captured upvalue spine in declaration order. Per-cell mutation
    /// flows through [`crate::store_upvalue`] /
    /// [`crate::read_upvalue`]; the slice itself never resizes.
    pub upvalues: Vec<UpvalueCell>,
    /// Arrow closures: `Some(this)` overrides the caller's receiver.
    /// Non-arrow closures: `None` (take `this` from the call site).
    pub bound_this: Option<Value>,
}

impl otter_gc::SafeTraceable for JsClosureBody {
    const TYPE_TAG: u8 = JS_CLOSURE_BODY_TYPE_TAG;

    fn trace_slots_safe(&self, visitor: &mut SlotVisitor<'_>) {
        for cell in self.upvalues.iter() {
            let p = cell as *const UpvalueCell as *mut RawGc;
            visitor(p);
        }
        if let Some(value) = &self.bound_this {
            value.trace_value_slots(visitor);
        }
    }
}

/// 4-byte compressed `Gc<JsClosureBody>` handle to the underlying
/// body cell.
pub type JsClosureHandle = otter_gc::Gc<JsClosureBody>;

/// 8-byte `Copy` closure value: 4-byte GC handle to the body plus
/// a 4-byte cached `function_id` so the call path can dispatch
/// without a heap touch. Identity (`===`) is handle-offset equality.
///
/// Matches V8 / JSC `JSFunction` cell-with-cached-code-entry layout.
/// Packs into [`crate::Value`] under `TAG_PTR_FUNCTION`.
#[derive(Debug, Clone, Copy, Eq, PartialEq, Hash)]
pub struct JsClosure {
    /// GC handle to the body cell. Field is `pub` so call-site
    /// pattern matches can bind it alongside the cached function id;
    /// mutation should still happen through dedicated helpers.
    pub handle: JsClosureHandle,
    /// Function-id cache. Mirrors [`JsClosureBody::function_id`];
    /// kept on the wrapper so the call path stays heap-free.
    pub cached_function_id: u32,
}

impl JsClosure {
    /// Construct from a raw handle + the function id stored inside
    /// it. Mirrors `from_handle` constructors on the other GC
    /// wrappers; callers that already hold both fields skip the
    /// `heap.read_payload` re-read.
    #[must_use]
    pub fn from_parts(handle: JsClosureHandle, function_id: u32) -> Self {
        Self {
            handle,
            cached_function_id: function_id,
        }
    }

    /// Underlying GC handle.
    #[must_use]
    pub fn handle(self) -> JsClosureHandle {
        self.handle
    }

    /// Underlying type-erased GC pointer; used by the tagged-value
    /// packer.
    #[must_use]
    pub fn raw(self) -> otter_gc::raw::RawGc {
        self.handle.raw()
    }

    /// Bytecode function id. Cached on the wrapper for heap-free
    /// hot-path access.
    #[must_use]
    pub fn function_id(self) -> u32 {
        self.cached_function_id
    }

    /// `Some(this)` for arrow closures, `None` otherwise. Reads the
    /// body once.
    #[must_use]
    pub fn bound_this(self, heap: &GcHeap) -> Option<Value> {
        heap.read_payload(self.handle, |body| body.bound_this)
    }

    /// Number of captured upvalue cells. Reads the body once.
    #[must_use]
    pub fn upvalue_count(self, heap: &GcHeap) -> usize {
        heap.read_payload(self.handle, |body| body.upvalues.len())
    }

    /// Run `f` with the captured upvalue spine. The slice borrow
    /// never escapes the closure; callers that need to retain a
    /// cell beyond `f` should snapshot the `UpvalueCell` handle
    /// (it is itself a `Copy` GC handle).
    pub fn with_upvalues<F, R>(self, heap: &GcHeap, f: F) -> R
    where
        F: FnOnce(&[UpvalueCell]) -> R,
    {
        heap.read_payload(self.handle, |body| f(&body.upvalues))
    }

    /// Snapshot the captured upvalue spine into a fresh `Vec`.
    /// Use when the caller needs to return cells across a borrow
    /// boundary; otherwise prefer [`Self::with_upvalues`].
    #[must_use]
    pub fn upvalues_snapshot(self, heap: &GcHeap) -> Vec<UpvalueCell> {
        heap.read_payload(self.handle, |body| body.upvalues.to_vec())
    }

    /// Identity comparison via GC handle offset.
    #[must_use]
    pub fn ptr_eq(self, other: Self) -> bool {
        self.handle == other.handle
    }

    /// Backing-pointer for cycle / identity sets.
    #[must_use]
    pub fn identity_addr(self) -> *const () {
        self.handle.offset() as usize as *const ()
    }

    /// Visit the embedded GC handle slot during root tracing.
    pub fn trace_value_slots(&self, visitor: &mut SlotVisitor<'_>) {
        let p = &self.handle as *const JsClosureHandle as *mut RawGc;
        visitor(p);
    }
}

/// Allocate a closure body in old-space (consistent with
/// [`crate::alloc_upvalue`]; the scavenger does not yet rewrite
/// embedded `UpvalueCell` slots).
///
/// # Errors
///
/// Surfaces [`OutOfMemory`] verbatim.
pub fn alloc_closure(
    heap: &mut GcHeap,
    function_id: u32,
    upvalues: Vec<UpvalueCell>,
    bound_this: Option<Value>,
) -> Result<JsClosure, OutOfMemory> {
    let handle = heap.alloc_old(JsClosureBody {
        function_id,
        upvalues,
        bound_this,
    })?;
    Ok(JsClosure::from_parts(handle, function_id))
}

/// Allocate a closure body while exposing caller-owned roots across
/// any allocation-triggered collection.
///
/// Use this from interpreter call sites where the surrounding
/// `Value`s on the Rust stack must be preserved (per the
/// [`GcHeap::alloc_with_roots`] contract).
///
/// # Errors
///
/// Surfaces [`OutOfMemory`] verbatim.
pub fn alloc_closure_with_roots(
    heap: &mut GcHeap,
    function_id: u32,
    upvalues: Vec<UpvalueCell>,
    bound_this: Option<Value>,
    external_visit: &mut RootSlotVisitor<'_>,
) -> Result<JsClosure, OutOfMemory> {
    let handle = heap.alloc_with_roots(
        JsClosureBody {
            function_id,
            upvalues,
            bound_this,
        },
        external_visit,
    )?;
    Ok(JsClosure::from_parts(handle, function_id))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::alloc_upvalue;

    #[test]
    fn allocates_empty_closure() {
        let mut heap = GcHeap::new().expect("heap");
        let closure = alloc_closure(&mut heap, 7, Vec::new(), None).expect("alloc");
        assert_eq!(closure.function_id(), 7);
        heap.read_payload(closure.handle(), |body| {
            assert_eq!(body.function_id, 7);
            assert!(body.upvalues.is_empty());
            assert!(body.bound_this.is_none());
        });
    }

    #[test]
    fn allocates_closure_with_upvalues_and_bound_this() {
        let mut heap = GcHeap::new().expect("heap");
        let cell_a = alloc_upvalue(&mut heap, Value::undefined()).expect("cell");
        let cell_b = alloc_upvalue(&mut heap, Value::undefined()).expect("cell");
        let upvalues = vec![cell_a, cell_b];
        let closure = alloc_closure(&mut heap, 42, upvalues, Some(Value::null())).expect("alloc");
        assert_eq!(closure.function_id(), 42);
        assert_eq!(closure.upvalue_count(&heap), 2);
        heap.read_payload(closure.handle(), |body| {
            assert_eq!(body.function_id, 42);
            assert_eq!(body.upvalues.len(), 2);
            assert_eq!(body.upvalues[0], cell_a);
            assert_eq!(body.upvalues[1], cell_b);
            assert!(matches!(body.bound_this, Some(Value::Null)));
        });
    }

    #[test]
    fn type_tag_matches_traceable_const() {
        assert_eq!(
            <JsClosureBody as otter_gc::SafeTraceable>::TYPE_TAG,
            JS_CLOSURE_BODY_TYPE_TAG
        );
    }
}
