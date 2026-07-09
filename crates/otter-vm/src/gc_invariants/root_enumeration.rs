//! Per-root coverage for the GC root walker
//! ([`crate::runtime_state::RuntimeState::trace_roots`]).
//!
//! This module tests the root families that are live in the current VM:
//! globals, arrays, collections, weak registries, promises, iterators,
//! generators, callables, and error registries. Symbols remain interned leaf
//! values, so they do not need GC slot coverage here.
//!
//! # Mapping
//!
//! | Root | Invariant |
//! | --- | --- |
//! | upvalue cell | unrooted cells are reclaimed |
//! | global object | global properties keep values alive |
//! | array element | element slots trace values |
//! | map entry | key/value slots trace values |
//! | weak-map / weak-set | keys stay weak; live keys update values |
//! | weak-ref / finalization registry | targets stay weak; jobs keep held values |
//! | promise / iterator / generator | async state traces parked values |
//! | bound / native / regexp | callable metadata traces captured values |
//! | microtask queue | queued jobs trace argument values |
//! | symbol registry | out of scope while symbols stay leaf interned values |
//! | error-class registry | constructor/prototype registries are roots |
//!
//! # See also
//!
//! - GC architecture plan §4.2.

use crate::Interpreter;
use crate::runtime_state::RuntimeState;

/// Sanity check: the walker compiles and runs. A fresh interpreter
/// yields at least `globalThis`. The exact count is not load-bearing;
/// what matters is that the walker terminates and emits some roots.
#[test]
fn root_walker_runs_with_empty_state() {
    let interp = Interpreter::default();
    let state = RuntimeState::new(&interp);
    let mut count = 0usize;
    state.trace_roots(&mut |_slot| {
        count = count.wrapping_add(1);
    });
    assert!(
        count >= 1,
        "root walker must surface at least globalThis (got {count})"
    );
}

#[test]
fn pending_uncaught_throw_is_enumerated_as_a_root() {
    use otter_gc::raw::RawGc;

    let mut interp = Interpreter::new();
    let thrown = crate::object::alloc_object_with_roots(interp.gc_heap_mut(), &mut |_| {})
        .expect("alloc object");
    interp.set_pending_uncaught_throw(crate::Value::object(thrown));

    let expected_slot = interp
        .pending_uncaught_throw_for_trace()
        .expect("pending throw") as *const crate::Value as *mut RawGc;
    let mut found = false;
    RuntimeState::new(&interp).trace_roots(&mut |slot| {
        found |= slot == expected_slot;
    });

    assert!(found, "pending uncaught throw must be part of the root set");
}

#[test]
fn upvalue_cell_root_survives_force_gc() {
    use crate::{
        UPVALUE_CELL_TYPE_TAG, UpvalueCellBody, Value, alloc_upvalue, read_upvalue, store_upvalue,
    };
    use otter_gc::Traceable;

    let mut interp = Interpreter::new();
    // Allocate a fresh upvalue cell carrying a primitive payload. The cell is
    // rooted only via the local handle here. After we drop the handle and force
    // a full GC, the body should be reclaimed because no walker root reaches it.
    let baseline =
        interp.gc_heap_mut().gc_stats().by_type[UPVALUE_CELL_TYPE_TAG as usize].live_bytes;
    let cell = alloc_upvalue(interp.gc_heap_mut(), Value::number_i32(42)).expect("alloc_upvalue");
    let stats_with_cell =
        interp.gc_heap_mut().gc_stats().by_type[UPVALUE_CELL_TYPE_TAG as usize].live_bytes;
    assert!(
        stats_with_cell > baseline,
        "alloc_upvalue must bump per-tag live bytes"
    );

    // Read + write through the safe API while the cell is
    // rooted by `cell`.
    let v = read_upvalue(interp.gc_heap(), cell);
    assert!(v.is_number());
    store_upvalue(interp.gc_heap_mut(), cell, Value::boolean(true));
    assert_eq!(
        read_upvalue(interp.gc_heap(), cell).as_boolean(),
        Some(true)
    );

    // Drop the handle and force GC. `cell` is `Copy` (a 4-byte
    // compressed offset); explicit `let _ = cell` documents
    // intent to release the reference.
    let _ = cell;
    interp.force_gc().expect("force GC");
    let after = interp.gc_heap_mut().gc_stats().by_type[UPVALUE_CELL_TYPE_TAG as usize].live_bytes;
    assert_eq!(
        after,
        baseline,
        "upvalue cell should be reclaimed once unrooted (UpvalueCellBody TYPE_TAG = {})",
        <UpvalueCellBody as Traceable>::TYPE_TAG
    );
}

/// Globals act as a strong root: an object stamped onto
/// `globalThis` survives a forced full GC even after every
/// host-side handle is dropped.
///
/// Spec: <https://tc39.es/ecma262/#sec-global-object>
/// Walker root source: GC architecture plan §4.2.
#[test]
fn globals_keep_object_alive() {
    use crate::object::OBJECT_BODY_TYPE_TAG;

    let mut interp = Interpreter::new();

    // Allocate a fresh object and stash it on globalThis under
    // a unique key. From this point the only path to the body
    // is through globalThis → property slot → ObjectBody.
    let baseline =
        interp.gc_heap_mut().gc_stats().by_type[OBJECT_BODY_TYPE_TAG as usize].live_bytes;
    let stashed =
        crate::test_support::alloc_old_object(interp.gc_heap_mut()).expect("alloc_object");
    let mut global = *interp.global_this();
    crate::object::set(
        &mut global,
        interp.gc_heap_mut(),
        "__gc_roots_test_stash",
        crate::Value::object(stashed),
    );
    let after_alloc =
        interp.gc_heap_mut().gc_stats().by_type[OBJECT_BODY_TYPE_TAG as usize].live_bytes;
    assert!(
        after_alloc > baseline,
        "alloc_object + set must bump live_bytes (baseline={baseline}, after={after_alloc})"
    );

    // Drop the local handle. `JsObject` is `Copy` (compressed
    // offset); `let _ = stashed` documents intent — the only
    // remaining root is globalThis itself.
    let _ = stashed;

    // Force GC. With `GcTrace for JsObject` emitting the slot
    // and `ObjectBody::trace_slots_safe` walking property
    // values, the global's property must still resolve.
    interp.force_gc().expect("force GC");
    let resolved = crate::object::get(global, interp.gc_heap(), "__gc_roots_test_stash")
        .expect("globalThis property survives force_gc");
    assert!(
        resolved.is_object(),
        "expected Value::Object after force_gc, got {resolved:?}"
    );
}

/// Module-environment registry acts as a strong root: an
/// object stamped onto a registered module env survives a
/// forced full GC even after every host-side handle is dropped.
///
/// Spec: <https://tc39.es/ecma262/#sec-module-environment-records>
/// Walker root source: GC architecture plan §4.2.
#[test]
fn module_env_keeps_object_alive() {
    use crate::object::OBJECT_BODY_TYPE_TAG;

    let mut interp = Interpreter::new();

    // Register a fresh empty `module_env` object under a
    // synthetic URL. Then stash a property on it whose value
    // is a freshly-allocated object. The body of the stashed
    // object is now reachable only through:
    //   module_environments[url] → module_env → property slot.
    let baseline =
        interp.gc_heap_mut().gc_stats().by_type[OBJECT_BODY_TYPE_TAG as usize].live_bytes;
    let mut module_env =
        crate::test_support::alloc_old_object(interp.gc_heap_mut()).expect("alloc_object");
    let url: std::sync::Arc<str> = std::sync::Arc::from("file:///gc_roots_test.js");
    interp.register_module_env(std::sync::Arc::clone(&url), module_env);

    let stashed =
        crate::test_support::alloc_old_object(interp.gc_heap_mut()).expect("alloc_object");
    crate::object::set(
        &mut module_env,
        interp.gc_heap_mut(),
        "stash",
        crate::Value::object(stashed),
    );
    let after_alloc =
        interp.gc_heap_mut().gc_stats().by_type[OBJECT_BODY_TYPE_TAG as usize].live_bytes;
    assert!(
        after_alloc > baseline,
        "alloc_object + set must bump live_bytes (baseline={baseline}, after={after_alloc})"
    );

    let _ = stashed;
    interp.force_gc().expect("force GC");
    let env_handle = interp
        .module_env(&url)
        .expect("module env still registered");
    let resolved = crate::object::get(env_handle, interp.gc_heap(), "stash")
        .expect("module-env property survives force_gc");
    assert!(
        resolved.is_object(),
        "expected Value::Object after force_gc, got {resolved:?}"
    );
}

#[test]
fn error_class_registry_prototypes_survive_force_gc() {
    let mut interp = Interpreter::new();
    interp.force_gc().expect("force GC");

    let registry = interp.error_classes_clone();
    let proto = registry.prototype(crate::ErrorKind::TypeError);
    let name = crate::object::get(proto, interp.gc_heap(), "name")
        .expect("TypeError.prototype.name survives force_gc");

    if let Some(s) = name.as_string(interp.gc_heap()) {
        assert_eq!(s.to_lossy_string(interp.gc_heap()), "TypeError");
    } else {
        panic!("expected TypeError.prototype.name string, got {name:?}");
    }
}

#[test]
fn array_element_root_survives_force_gc() {
    let mut interp = Interpreter::new();
    let arr = crate::test_support::alloc_old_array(interp.gc_heap_mut()).expect("alloc array");
    crate::array::push(arr, interp.gc_heap_mut(), crate::Value::boolean(true))
        .expect("push element");
    let mut global_this = *interp.global_this();
    crate::object::set(
        &mut global_this,
        interp.gc_heap_mut(),
        "__array_root",
        crate::Value::array(arr),
    );

    let _ = arr;
    interp.force_gc().expect("force GC");

    let rooted = crate::object::get(global_this, interp.gc_heap(), "__array_root")
        .expect("array root survives force_gc");
    if let Some(array) = rooted.as_array() {
        assert_eq!(
            crate::array::get(array, interp.gc_heap(), 0),
            crate::Value::boolean(true)
        );
    } else {
        panic!("expected Value::Array after force_gc, got {rooted:?}");
    }
}

#[test]
fn map_entry_root_survives_force_gc() {
    let mut interp = Interpreter::new();
    let map = crate::collections::alloc_map(interp.gc_heap_mut()).expect("alloc map");
    let stashed =
        crate::test_support::alloc_old_object(interp.gc_heap_mut()).expect("alloc object");
    let key = crate::Value::number_i32(7);
    crate::collections::map_set(
        map,
        interp.gc_heap_mut(),
        key,
        crate::Value::object(stashed),
    )
    .expect("map set");

    let mut global_this = *interp.global_this();
    crate::object::set(
        &mut global_this,
        interp.gc_heap_mut(),
        "__map_root",
        crate::Value::map(map),
    );

    let _ = map;
    let _ = stashed;
    interp.force_gc().expect("force GC");

    let rooted = crate::object::get(global_this, interp.gc_heap(), "__map_root")
        .expect("map root survives force_gc");
    if let Some(rooted_map) = rooted.as_map() {
        let value = crate::collections::map_get(rooted_map, interp.gc_heap(), &key)
            .expect("map entry survives force_gc");
        assert!(value.is_object());
    } else {
        panic!("expected Value::Map after force_gc, got {rooted:?}");
    }
}

#[test]
fn weak_collections_root_survives_force_gc() {
    let mut interp = Interpreter::new();
    let map = crate::collections::alloc_weak_map(interp.gc_heap_mut()).expect("alloc WeakMap");
    let set = crate::collections::alloc_weak_set(interp.gc_heap_mut()).expect("alloc WeakSet");
    let key = crate::test_support::alloc_old_object(interp.gc_heap_mut()).expect("alloc key");
    let value = crate::test_support::alloc_old_object(interp.gc_heap_mut()).expect("alloc value");

    crate::collections::weak_map_set(
        map,
        interp.gc_heap_mut(),
        crate::Value::object(key),
        crate::Value::object(value),
    )
    .expect("weak map set");
    crate::collections::weak_set_add(set, interp.gc_heap_mut(), crate::Value::object(key))
        .expect("weak set add");

    let mut global_this = *interp.global_this();
    crate::object::set(
        &mut global_this,
        interp.gc_heap_mut(),
        "__weak_map_root",
        crate::Value::weak_map(map),
    );
    crate::object::set(
        &mut global_this,
        interp.gc_heap_mut(),
        "__weak_set_root",
        crate::Value::weak_set(set),
    );
    crate::object::set(
        &mut global_this,
        interp.gc_heap_mut(),
        "__weak_key_root",
        crate::Value::object(key),
    );

    let _ = map;
    let _ = set;
    let _ = key;
    let _ = value;
    interp.force_gc().expect("force GC");

    let rooted_key = crate::object::get(global_this, interp.gc_heap(), "__weak_key_root")
        .expect("weak key root survives force_gc");
    let rooted_map = crate::object::get(global_this, interp.gc_heap(), "__weak_map_root")
        .expect("weak map root survives force_gc");
    let rooted_set = crate::object::get(global_this, interp.gc_heap(), "__weak_set_root")
        .expect("weak set root survives force_gc");

    match (rooted_map.as_weak_map(), rooted_set.as_weak_set()) {
        (Some(map), Some(set)) => {
            assert!(
                crate::collections::weak_map_has(map, interp.gc_heap(), &rooted_key)
                    .expect("weak map has")
            );
            assert!(
                crate::collections::weak_set_has(set, interp.gc_heap(), &rooted_key)
                    .expect("weak set has")
            );
        }
        _ => panic!(
            "expected WeakMap/WeakSet after force_gc, got {:?} / {:?}",
            rooted_map, rooted_set
        ),
    }
}

/// Promise bodies are strong GC objects: a promise rooted through
/// `globalThis` keeps its settlement value alive across a forced
/// collection after the transient host handles are gone.
#[test]
fn promise_resolution_root_survives_force_gc() {
    use crate::promise::{JsPromise, PromiseState};

    let mut interp = Interpreter::new();

    let promise = crate::JsPromiseHandle::pending(interp.gc_heap_mut()).expect("promise");
    let object = crate::test_support::alloc_old_object(interp.gc_heap_mut()).expect("object");
    promise.fulfill(interp.gc_heap_mut(), crate::Value::object(object));

    let mut global_this = *interp.global_this();
    crate::object::set(
        &mut global_this,
        interp.gc_heap_mut(),
        "__promise_root",
        crate::Value::promise(promise),
    );

    let _ = object;
    let _ = promise;
    interp.force_gc().expect("force GC");

    let rooted = crate::object::get(global_this, interp.gc_heap(), "__promise_root")
        .expect("promise root survives force_gc");
    let Some(promise) = rooted.as_promise() else {
        panic!("expected Value::Promise after force_gc, got {rooted:?}");
    };
    match promise.state(interp.gc_heap()) {
        PromiseState::Fulfilled(v) if v.is_object() => {}
        other => panic!("expected fulfilled object promise after force_gc, got {other:?}"),
    }
}

/// Queued microtasks are runtime roots. Their callee, receiver,
/// arguments, result capability, and async-resume frame payloads
/// must be traced while the job is pending.
#[test]
fn microtask_payload_root_survives_force_gc() {
    let mut interp = Interpreter::new();

    let object = crate::test_support::alloc_old_object(interp.gc_heap_mut()).expect("object");
    interp.microtasks_mut().enqueue(crate::Microtask {
        callee: crate::Value::undefined(),
        this_value: crate::Value::undefined(),
        args: smallvec::smallvec![crate::Value::object(object)],
        context: None,
        result_capability: None,
        kind: crate::MicrotaskKind::Call,
    });

    let _ = object;
    interp.force_gc().expect("force GC");

    let _ = interp
        .microtasks_mut()
        .begin_drain()
        .expect("outer drain batch");
    let task = interp
        .microtasks_mut()
        .next_in_flight()
        .expect("queued task");
    assert!(
        task.args.first().is_some_and(|v| v.is_object()),
        "microtask payload remains observable after force_gc"
    );
    interp.microtasks_mut().end_drain();
}

#[test]
fn parked_frame_keeps_alive() {
    use crate::generator::PARKED_FRAME_BODY_TYPE_TAG;
    use crate::{Frame, PromiseCapability, Value};
    use otter_bytecode::Function;

    let mut interp = Interpreter::new();
    interp.force_gc().expect("force GC");
    let baseline =
        interp.gc_heap_mut().gc_stats().by_type[PARKED_FRAME_BODY_TYPE_TAG as usize].live_bytes;

    let function = Function {
        id: 0,
        name: "gc-roots-parked-frame".to_string(),
        locals: 1,
        scratch: 1,
        ..Function::default()
    };
    let mut frame = Frame::for_function_with_heap(&function, interp.gc_heap_mut()).expect("frame");
    let object = crate::test_support::alloc_old_object(interp.gc_heap_mut()).expect("object");
    frame.registers[0] = Value::object(object);

    let parked = crate::generator::alloc_parked_frame(interp.gc_heap_mut(), frame, None)
        .expect("parked frame");
    let promise = crate::JsPromiseHandle::pending(interp.gc_heap_mut()).expect("promise");
    promise.perform_async_resume_then(
        interp.gc_heap_mut(),
        parked,
        0,
        PromiseCapability {
            promise: Value::undefined(),
            resolve: Value::undefined(),
            reject: Value::undefined(),
            context: None,
        },
        None,
    );

    let mut global_this = *interp.global_this();
    crate::object::set(
        &mut global_this,
        interp.gc_heap_mut(),
        "__parked_frame_promise_root",
        Value::promise(promise),
    );

    let _ = object;
    let _ = parked;
    let _ = promise;
    interp.force_gc().expect("force GC");

    let after =
        interp.gc_heap_mut().gc_stats().by_type[PARKED_FRAME_BODY_TYPE_TAG as usize].live_bytes;
    assert!(
        after > baseline,
        "pending async reaction must retain its parked frame"
    );
}

#[test]
fn bound_function_root_survives_force_gc() {
    let mut interp = Interpreter::new();
    let target = crate::test_support::alloc_old_object(interp.gc_heap_mut()).expect("target");
    let bound_this = crate::test_support::alloc_old_object(interp.gc_heap_mut()).expect("this");
    let bound = crate::BoundFunction::new(
        interp.gc_heap_mut(),
        crate::Value::object(target),
        crate::Value::object(bound_this),
        smallvec::smallvec![crate::Value::boolean(true)],
    )
    .expect("bound");
    let mut global_this = *interp.global_this();
    crate::object::set(
        &mut global_this,
        interp.gc_heap_mut(),
        "__bound_root",
        crate::Value::bound_function(bound),
    );

    let _ = target;
    let _ = bound_this;
    let _ = bound;
    interp.force_gc().expect("force GC");

    let rooted = crate::object::get(global_this, interp.gc_heap(), "__bound_root")
        .expect("bound root survives force_gc");
    let Some(bound) = rooted.as_bound_function() else {
        panic!("expected Value::BoundFunction after force_gc, got {rooted:?}");
    };
    let (target, bound_this, args) = bound.parts(interp.gc_heap());
    assert!(target.is_object());
    assert!(bound_this.is_object());
    assert_eq!(args.first().and_then(|v| v.as_boolean()), Some(true));
}

#[test]
fn regexp_root_survives_force_gc() {
    let mut interp = Interpreter::new();
    let pattern: Vec<u16> = "a+".encode_utf16().collect();
    let re = crate::JsRegExp::compile(interp.gc_heap_mut(), &pattern, "g").expect("regexp");
    let mut global_this = *interp.global_this();
    crate::object::set(
        &mut global_this,
        interp.gc_heap_mut(),
        "__regexp_root",
        crate::Value::regexp(re),
    );

    let _ = re;
    interp.force_gc().expect("force GC");

    let rooted = crate::object::get(global_this, interp.gc_heap(), "__regexp_root")
        .expect("regexp root survives force_gc");
    let Some(re) = rooted.as_regexp() else {
        panic!("expected Value::RegExp after force_gc, got {rooted:?}");
    };
    let text: Vec<u16> = "aaab".encode_utf16().collect();
    let first = re
        .find_from_utf16(interp.gc_heap(), &text, 0)
        .into_iter()
        .next()
        .expect("regexp remains executable after force_gc");
    assert_eq!(first.range, 0..3);
}
