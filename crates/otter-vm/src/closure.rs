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
    pub upvalues: Box<[UpvalueCell]>,
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

/// 4-byte compressed `Gc<JsClosureBody>` handle. `Copy`. Identity is
/// offset equality — `c1 == c2` iff they refer to the same body cell
/// (`===` for closure values). Packs into [`crate::Value`] under
/// `TAG_PTR_FUNCTION`.
pub type JsClosure = otter_gc::Gc<JsClosureBody>;

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
