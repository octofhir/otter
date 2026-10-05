//! Reclaim an unreachable cycle of ordinary prototype and data-property edges.
//!
//! # Contents
//! - A real mixed-edge cycle that survives full GC while rooted and is reaped
//!   after its canonical scope ends.
//!
//! # Invariants
//! - Ordinary [[SetPrototypeOf]] rejects a cyclic prototype chain.
//! - The permitted data-property back edge still forms a collector cycle.
//! - Every allocating transition uses handles; no stale raw receiver is reused.
//!
//! # See also
//! - `root_enumeration::globals_keep_object_alive` checks rooted survival.

use crate::Interpreter;
use crate::object::OBJECT_BODY_TYPE_TAG;

/// A rooted `b.[[Prototype]] = a; a.link = b` cycle is reclaimed after
/// the scope releases both objects. A prototype-only cycle is rejected.
#[test]
fn proto_cycle_reaped() {
    let mut interp = Interpreter::new().expect("fixture interpreter bootstrap");

    // Baseline AFTER intrinsics + globalThis are wired by
    // `Interpreter::new`. Anything reachable from globalThis
    // (which is itself a strong root per
    // `RuntimeState::trace_roots`) is part of the baseline.
    interp.force_gc().expect("force GC");
    let baseline =
        interp.gc_heap_mut().gc_stats().by_type[OBJECT_BODY_TYPE_TAG as usize].live_bytes;

    interp.with_handle_scope(|interp, scope| {
        let a = interp.scoped_object_bare(scope).expect("alloc a");
        let b = interp.scoped_object_bare(scope).expect("alloc b");
        interp.scoped_set_prototype(scope, b, Some(a)).expect("acyclic prototype edge");
        assert!(matches!(interp.scoped_set_prototype(scope, a, Some(b)), Err(crate::VmError::TypeMismatch)),
            "ordinary prototype cycles must be rejected");
        interp.scoped_define_data(scope, a, "link", b,
            crate::object::PropertyFlags::data_default()).expect("data back edge");
        interp.force_gc().expect("rooted full GC");
        let rooted_a = interp.escape_scoped(a).as_object().expect("rooted a");
        let rooted_b = interp.escape_scoped(b).as_object().expect("rooted b");
        assert_eq!(crate::object::prototype(rooted_b, interp.gc_heap()), Some(rooted_a));
        assert_eq!(crate::object::get_own(rooted_a, interp.gc_heap(), "link"), Some(crate::Value::object(rooted_b)));
        let with_cycle = interp.gc_heap_mut().gc_stats().by_type[OBJECT_BODY_TYPE_TAG as usize].live_bytes;
        assert!(with_cycle > baseline,
            "rooted cycle must survive actual full GC (baseline={baseline}, with_cycle={with_cycle})");
    });

    // End the fixture's implicit runtime turn, just as the activation-stack
    // owner does. Its shape pins deliberately retain source/prototype edges
    // until this boundary; the mixed cycle then has no strong root.
    interp.shape_runtime.unpin_turn_shapes();
    interp.force_gc().expect("unrooted full GC");
    let after = interp.gc_heap_mut().gc_stats().by_type[OBJECT_BODY_TYPE_TAG as usize].live_bytes;
    assert!(
        after <= baseline,
        "proto cycle must be reaped by force_gc (baseline={baseline}, after={after})"
    );
}
