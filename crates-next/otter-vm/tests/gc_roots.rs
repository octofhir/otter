//! Per-root smoke-test scaffold for the GC root walker
//! ([`otter_vm::runtime_state::RuntimeState::trace_roots`]).
//!
//! Each test is `#[ignore]` today: Phase 1's value model is
//! still `Rc`-shared, so allocating "via root R, drop the local
//! handle, force GC, assert the value is still readable" needs
//! the future `Gc<T>` API which has not landed yet. Each
//! migration task in 76–83 un-ignores the matching test as it
//! brings its type online.
//!
//! # Mapping
//!
//! | Root | Migration task |
//! | --- | --- |
//! | upvalue cell | 76 |
//! | global object | 77 |
//! | array element | 78 |
//! | map entry | 79 |
//! | weak-map / weak-set | 80 |
//! | weak-ref / finalization registry | 81 |
//! | promise / iterator / generator | 82 |
//! | bound / native / regexp | 83 |
//! | active call frame | 76 (concurrently with upvalue) |
//! | microtask queue | 82 |
//! | symbol registry | 83 |
//! | error-class registry | 77 |
//!
//! # See also
//!
//! - GC architecture plan §4.2.
//! - Task 75 — root enumeration.

use otter_vm::Interpreter;
use otter_vm::runtime_state::RuntimeState;

/// Sanity check: the walker compiles, runs, and emits zero
/// slot pointers under Phase 1 (every stub is empty).
#[test]
fn root_walker_runs_with_empty_state() {
    let interp = Interpreter::default();
    let state = RuntimeState::new(&interp);
    let mut count = 0usize;
    state.trace_roots(&mut |_slot| {
        count = count.wrapping_add(1);
    });
    assert_eq!(count, 0, "Phase-1 stubs must emit zero slot pointers");
}

#[test]
fn upvalue_cell_root_survives_force_gc() {
    use otter_gc::Traceable;
    use otter_vm::{
        UPVALUE_CELL_TYPE_TAG, UpvalueCellBody, Value, alloc_upvalue, read_upvalue, store_upvalue,
    };

    let mut interp = Interpreter::new();
    // Allocate a fresh upvalue cell carrying a primitive
    // payload (Number); the cell is rooted only via the local
    // handle here. After we drop the handle and force a full
    // GC, the body should be reclaimed because no walker root
    // reaches it (task 76 stops at the leaf cell — closure-
    // chain reachability through `JsObject` lands in task 77).
    let baseline =
        interp.gc_heap_mut().gc_stats().by_type[UPVALUE_CELL_TYPE_TAG as usize].live_bytes;
    let cell = alloc_upvalue(
        interp.gc_heap_mut(),
        Value::Number(otter_vm::NumberValue::Smi(42)),
    )
    .expect("alloc_upvalue");
    let stats_with_cell =
        interp.gc_heap_mut().gc_stats().by_type[UPVALUE_CELL_TYPE_TAG as usize].live_bytes;
    assert!(
        stats_with_cell > baseline,
        "alloc_upvalue must bump per-tag live bytes"
    );

    // Read + write through the safe API while the cell is
    // rooted by `cell`.
    let v = read_upvalue(interp.gc_heap(), cell);
    assert!(matches!(v, Value::Number(_)));
    store_upvalue(interp.gc_heap_mut(), cell, Value::Boolean(true));
    assert!(matches!(
        read_upvalue(interp.gc_heap(), cell),
        Value::Boolean(true)
    ));

    // Drop the handle and force GC. `cell` is `Copy` (a 4-byte
    // compressed offset); explicit `let _ = cell` documents
    // intent to release the reference.
    let _ = cell;
    interp.force_gc();
    let after = interp.gc_heap_mut().gc_stats().by_type[UPVALUE_CELL_TYPE_TAG as usize].live_bytes;
    assert_eq!(
        after,
        baseline,
        "upvalue cell should be reclaimed once unrooted (UpvalueCellBody TYPE_TAG = {})",
        <UpvalueCellBody as Traceable>::TYPE_TAG
    );
}

#[test]
#[ignore = "un-ignore in task 76 (active call frame trace)"]
fn active_frame_local_root_survives_force_gc() {
    // task 76: call into bytecode that allocs into a local,
    // suspend mid-frame, force_gc, assert local readable.
}

#[test]
#[ignore = "un-ignore in task 77 (JsObject migration)"]
fn global_object_root_survives_force_gc() {
    // task 77: assign through globalThis, drop handle, force_gc,
    // assert global property readable.
}

#[test]
#[ignore = "un-ignore in task 77 (error-class registry)"]
fn error_class_registry_prototypes_survive_force_gc() {
    // task 77: capture each canonical Error.prototype via
    // identity, force_gc, assert identity preserved.
}

#[test]
#[ignore = "un-ignore in task 78 (JsArray migration)"]
fn array_element_root_survives_force_gc() {
    // task 78: alloc array, populate, drop handle, force_gc,
    // assert element readable.
}

#[test]
#[ignore = "un-ignore in task 79 (JsMap / JsSet migration)"]
fn map_entry_root_survives_force_gc() {
    // task 79: alloc Map, set entry, drop, force_gc, assert.
}

#[test]
#[ignore = "un-ignore in task 80 (WeakMap / WeakSet ephemerons)"]
fn weak_collections_root_survives_force_gc() {
    // task 80: WeakMap key-value pair survives while key is
    // strongly rooted; collected when key dies.
}

#[test]
#[ignore = "un-ignore in task 82 (promise / iterator / generator)"]
fn promise_resolution_root_survives_force_gc() {
    // task 82: settle a pending promise, drop intermediate
    // handles, force_gc, assert resolution flows.
}

#[test]
#[ignore = "un-ignore in task 82 (microtask queue trace)"]
fn microtask_payload_root_survives_force_gc() {
    // task 82: enqueue microtask whose payload holds a Gc
    // handle; force_gc before drain; assert payload value
    // observable on drain.
}

#[test]
#[ignore = "un-ignore in task 83 (symbol registry)"]
fn symbol_registry_root_survives_force_gc() {
    // task 83: Symbol.for("k") retains the registered symbol
    // across force_gc and Symbol.keyFor returns "k".
}

#[test]
#[ignore = "un-ignore in task 83 (bound / native / regexp)"]
fn bound_function_root_survives_force_gc() {
    // task 83: f.bind(this), drop intermediate, force_gc,
    // assert callable.
}

#[test]
#[ignore = "un-ignore in task 83 (regexp body)"]
fn regexp_root_survives_force_gc() {
    // task 83: alloc /a/g, drop handle, force_gc, assert
    // re-execable.
}
