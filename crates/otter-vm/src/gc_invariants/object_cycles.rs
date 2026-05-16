//! Cycle reclamation regression — verifies that mutually
//! prototype-linked objects are reaped by a forced full GC
//! once every host-side handle is dropped.
//!
//! Mark-sweep handles cycles natively (unlike refcounting), so
//! the contract here is: an `a → b → a` prototype loop with no
//! external root collapses to baseline after `force_gc`.
//!
//! # Spec
//!
//! - <https://tc39.es/ecma262/#sec-ordinary-object-internal-methods-and-internal-slots-getprototypeof>
//!   (§10.1.7 [[GetPrototypeOf]])
//! - <https://tc39.es/ecma262/#sec-ordinary-object-internal-methods-and-internal-slots-setprototypeof>
//!   (§10.4.7 [[SetPrototypeOf]])
//!
//! # See also
//!
//! - GC architecture plan §4.1 (cycle handling).
//! - Companion: `root_enumeration::globals_keep_object_alive` for the
//!   survival side of the same root walker.

use crate::Interpreter;
use crate::object::OBJECT_BODY_TYPE_TAG;

/// `let a = {}; let b = {}; b.__proto__ = a; a.__proto__ = b; drop a, b;`
/// — after a forced full GC, the per-tag `live_bytes` row for
/// `ObjectBody` returns to the baseline measured before the
/// cycle was constructed.
#[test]
fn proto_cycle_reaped() {
    let mut interp = Interpreter::new();

    // Baseline AFTER intrinsics + globalThis are wired by
    // `Interpreter::new`. Anything reachable from globalThis
    // (which is itself a strong root per
    // `RuntimeState::trace_roots`) is part of the baseline.
    interp.force_gc();
    let baseline =
        interp.gc_heap_mut().gc_stats().by_type[OBJECT_BODY_TYPE_TAG as usize].live_bytes;

    // Construct the cycle. Both objects route through
    // `alloc_object` → `alloc_old`, so they live in old-space
    // and stay pinned across collections; sweep is what reaps
    // them once they are unmarked.
    let a = crate::test_support::alloc_old_object(interp.gc_heap_mut()).expect("alloc a");
    let b = crate::test_support::alloc_old_object(interp.gc_heap_mut()).expect("alloc b");
    crate::object::set_prototype(b, interp.gc_heap_mut(), Some(a));
    crate::object::set_prototype(a, interp.gc_heap_mut(), Some(b));

    let with_cycle =
        interp.gc_heap_mut().gc_stats().by_type[OBJECT_BODY_TYPE_TAG as usize].live_bytes;
    assert!(
        with_cycle > baseline,
        "cycle construction must bump live_bytes (baseline={baseline}, with_cycle={with_cycle})"
    );

    // Drop the local handles. `JsObject` is `Copy` (compressed
    // offset); `let _ = ...` documents intent — neither is
    // reachable from any root the walker enumerates.
    let _ = a;
    let _ = b;

    // Mark-sweep handles cycles natively: even though `a` and
    // `b` reference each other through their `[[Prototype]]`
    // slots, neither is white-anchored to a root, so the
    // marker leaves them white and the sweep reclaims them.
    interp.force_gc();
    let after = interp.gc_heap_mut().gc_stats().by_type[OBJECT_BODY_TYPE_TAG as usize].live_bytes;
    assert!(
        after <= baseline,
        "proto cycle must be reaped by force_gc (baseline={baseline}, after={after})"
    );
}
