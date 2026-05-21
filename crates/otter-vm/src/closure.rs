//! GC-managed closure body — replaces the inline `Rc<[UpvalueCell]> +
//! Option<Box<Value>>` payload of the legacy `Value::Closure` variant.
//!
//! A closure on the JS heap is exactly three things:
//!
//! - the bytecode function id it executes,
//! - the captured upvalue spine (one [`crate::UpvalueCell`] per
//!   captured binding, in declaration order — see
//!   ECMA-262 §15.2.5 / `Op::MakeClosure`),
//! - an optional bound `this` (arrow closures lexically capture
//!   their receiver; non-arrow closures take `this` from the call
//!   site).
//!
//! This module wraps those fields in a single GC body so the value
//! handle is a 4-byte compressed offset rather than the legacy
//! inline enum payload that carried an atomic-refcounted slice plus
//! a heap-boxed `Value`.
//!
//! # Contents
//!
//! - [`JsClosureBody`] — fixed-size GC body holding `function_id`,
//!   an owned upvalue slice, and `bound_this`.
//! - [`JsClosure`] — 4-byte `Gc<JsClosureBody>` handle.
//! - [`alloc_closure`] / [`alloc_closure_with_roots`] — allocation
//!   helpers that surface pending roots across allocation-triggered
//!   GC.
//! - [`JS_CLOSURE_BODY_TYPE_TAG`] — reserved
//!   [`otter_gc::Traceable::TYPE_TAG`].
//!
//! # Invariants
//!
//! - The upvalue array is allocated once at closure creation
//!   ([`Op::MakeClosure`](otter_bytecode::Op::MakeClosure)) and never
//!   resized; mutation flows through
//!   [`crate::store_upvalue`] / [`crate::read_upvalue`] on the
//!   individual cells.
//! - `bound_this == None` means "take `this` from the call site"
//!   (non-arrow closures); `Some(value)` overrides any caller-supplied
//!   receiver (arrow closures).
//! - The trace impl walks every upvalue handle as a `*mut RawGc` slot
//!   plus the `bound_this` value slots.
//! - The body is `Box<[UpvalueCell]>` (Rust-owned slice of 4-byte GC
//!   handles), not a separate GC array. This keeps closure layout
//!   uniform with [`crate::BoundFunctionBody`] and avoids a second
//!   allocation per closure.
//!
//! # Spec
//!
//! - ECMA-262 §15.2.5 — closure environment construction.
//! - ECMA-262 §13.3.6 — `[[Call]]` for ordinary functions / closures.
//! - ECMA-262 §10.2.1.1 — `[[ThisMode]]` for arrow functions.
//!
//! # See also
//!
//! - [`crate::value::Value::from_function_gc`] — the eight-byte
//!   tagged value constructor that packs a [`JsClosure`] handle.
//! - [`crate::UpvalueCellBody`] — the captured-binding cell.
//! - `docs/value-cutover-plan.md` step 1.

use otter_gc::GcHeap;
use otter_gc::OutOfMemory;
use otter_gc::heap::RootSlotVisitor;
use otter_gc::raw::{RawGc, SlotVisitor};

use crate::{UpvalueCell, Value};

/// Reserved [`otter_gc::Traceable::TYPE_TAG`] for [`JsClosureBody`].
pub const JS_CLOSURE_BODY_TYPE_TAG: u8 = 0x23;

/// GC-allocated payload backing every closure value.
///
/// See module docs for the field contract.
#[derive(Debug)]
pub struct JsClosureBody {
    /// Index into [`otter_bytecode::BytecodeModule::functions`] —
    /// the body this closure executes when called.
    pub function_id: u32,
    /// Captured upvalue spine in declaration order. Built once by
    /// `Op::MakeClosure`; immutable thereafter (the cells themselves
    /// are mutable, but the slice is not).
    pub upvalues: Box<[UpvalueCell]>,
    /// `Some(this)` for arrow closures — wins over any caller-supplied
    /// receiver. `None` for non-arrow closures, which take `this`
    /// from the call site.
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

/// Compressed handle to a [`JsClosureBody`].
///
/// `#[repr(transparent)]` over [`otter_gc::Gc<JsClosureBody>`]
/// (4 bytes, `Copy`). Identity comparison is offset equality —
/// `c1 == c2` iff they refer to the same body cell, matching JS
/// `===` semantics for closure values.
pub type JsClosure = otter_gc::Gc<JsClosureBody>;

/// Allocate a closure body.
///
/// Routes through [`GcHeap::alloc_old`] so the body lives in
/// old-space (the same policy [`crate::alloc_upvalue`] uses today —
/// closure bodies hold `UpvalueCell` slots whose scavenger support
/// arrives in Phase 2).
///
/// # Errors
///
/// Surfaces [`OutOfMemory`] verbatim; runtime callers translate it
/// into `VmError::OutOfMemory`.
pub fn alloc_closure(
    heap: &mut GcHeap,
    function_id: u32,
    upvalues: Box<[UpvalueCell]>,
    bound_this: Option<Value>,
) -> Result<JsClosure, OutOfMemory> {
    heap.alloc_old(JsClosureBody {
        function_id,
        upvalues,
        bound_this,
    })
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
    upvalues: Box<[UpvalueCell]>,
    bound_this: Option<Value>,
    external_visit: &mut RootSlotVisitor<'_>,
) -> Result<JsClosure, OutOfMemory> {
    heap.alloc_with_roots(
        JsClosureBody {
            function_id,
            upvalues,
            bound_this,
        },
        external_visit,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::alloc_upvalue;

    #[test]
    fn allocates_empty_closure() {
        let mut heap = GcHeap::new().expect("heap");
        let closure = alloc_closure(&mut heap, 7, Box::new([]), None).expect("alloc");
        heap.read_payload(closure, |body| {
            assert_eq!(body.function_id, 7);
            assert!(body.upvalues.is_empty());
            assert!(body.bound_this.is_none());
        });
    }

    #[test]
    fn allocates_closure_with_upvalues_and_bound_this() {
        let mut heap = GcHeap::new().expect("heap");
        let cell_a = alloc_upvalue(&mut heap, Value::Undefined).expect("cell");
        let cell_b = alloc_upvalue(&mut heap, Value::Undefined).expect("cell");
        let upvalues = vec![cell_a, cell_b].into_boxed_slice();
        let closure = alloc_closure(&mut heap, 42, upvalues, Some(Value::Null)).expect("alloc");
        heap.read_payload(closure, |body| {
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
