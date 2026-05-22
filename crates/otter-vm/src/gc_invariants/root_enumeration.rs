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
    let cell = alloc_upvalue(
        interp.gc_heap_mut(),
        Value::Number(crate::NumberValue::Smi(42)),
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
    let global = *interp.global_this();
    crate::object::set(
        global,
        interp.gc_heap_mut(),
        "__gc_roots_test_stash",
        crate::Value::Object(stashed),
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
    interp.force_gc();
    let resolved = crate::object::get(global, interp.gc_heap(), "__gc_roots_test_stash")
        .expect("globalThis property survives force_gc");
    match resolved {
        crate::Value::Object(_) => {}
        other => panic!("expected Value::Object after force_gc, got {other:?}"),
    }
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
    let module_env =
        crate::test_support::alloc_old_object(interp.gc_heap_mut()).expect("alloc_object");
    let url: std::rc::Rc<str> = std::rc::Rc::from("file:///gc_roots_test.js");
    interp.register_module_env(std::rc::Rc::clone(&url), module_env);

    let stashed =
        crate::test_support::alloc_old_object(interp.gc_heap_mut()).expect("alloc_object");
    crate::object::set(
        module_env,
        interp.gc_heap_mut(),
        "stash",
        crate::Value::Object(stashed),
    );
    let after_alloc =
        interp.gc_heap_mut().gc_stats().by_type[OBJECT_BODY_TYPE_TAG as usize].live_bytes;
    assert!(
        after_alloc > baseline,
        "alloc_object + set must bump live_bytes (baseline={baseline}, after={after_alloc})"
    );

    let _ = stashed;
    interp.force_gc();
    let env_handle = interp
        .module_env(&url)
        .expect("module env still registered");
    let resolved = crate::object::get(env_handle, interp.gc_heap(), "stash")
        .expect("module-env property survives force_gc");
    match resolved {
        crate::Value::Object(_) => {}
        other => panic!("expected Value::Object after force_gc, got {other:?}"),
    }
}

#[test]
fn error_class_registry_prototypes_survive_force_gc() {
    let mut interp = Interpreter::new();
    interp.force_gc();

    let registry = interp.error_classes_clone();
    let proto = registry.prototype(crate::ErrorKind::TypeError);
    let name = crate::object::get(proto, interp.gc_heap(), "name")
        .expect("TypeError.prototype.name survives force_gc");

    match name {
        crate::Value::String(s) => assert_eq!(s.to_lossy_string(interp.gc_heap()), "TypeError"),
        other => panic!("expected TypeError.prototype.name string, got {other:?}"),
    }
}

#[test]
fn array_element_root_survives_force_gc() {
    let mut interp = Interpreter::new();
    let arr = crate::test_support::alloc_old_array(interp.gc_heap_mut()).expect("alloc array");
    crate::array::push(arr, interp.gc_heap_mut(), crate::Value::Boolean(true))
        .expect("push element");
    let global_this = *interp.global_this();
    crate::object::set(
        global_this,
        interp.gc_heap_mut(),
        "__array_root",
        crate::Value::Array(arr),
    );

    let _ = arr;
    interp.force_gc();

    let rooted = crate::object::get(global_this, interp.gc_heap(), "__array_root")
        .expect("array root survives force_gc");
    match rooted {
        crate::Value::Array(array) => {
            assert_eq!(
                crate::array::get(array, interp.gc_heap(), 0),
                crate::Value::Boolean(true)
            );
        }
        other => panic!("expected Value::Array after force_gc, got {other:?}"),
    }
}

#[test]
fn map_entry_root_survives_force_gc() {
    let mut interp = Interpreter::new();
    let map = crate::collections::alloc_map(interp.gc_heap_mut()).expect("alloc map");
    let stashed =
        crate::test_support::alloc_old_object(interp.gc_heap_mut()).expect("alloc object");
    let key = crate::Value::Number(crate::NumberValue::Smi(7));
    crate::collections::map_set(
        map,
        interp.gc_heap_mut(),
        key.clone(),
        crate::Value::Object(stashed),
    )
    .expect("map set");

    let global_this = *interp.global_this();
    crate::object::set(
        global_this,
        interp.gc_heap_mut(),
        "__map_root",
        crate::Value::Map(map),
    );

    let _ = map;
    let _ = stashed;
    interp.force_gc();

    let rooted = crate::object::get(global_this, interp.gc_heap(), "__map_root")
        .expect("map root survives force_gc");
    match rooted {
        crate::Value::Map(rooted_map) => {
            let value = crate::collections::map_get(rooted_map, interp.gc_heap(), &key)
                .expect("map entry survives force_gc");
            assert!(matches!(value, crate::Value::Object(_)));
        }
        other => panic!("expected Value::Map after force_gc, got {other:?}"),
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
        crate::Value::Object(key),
        crate::Value::Object(value),
    )
    .expect("weak map set");
    crate::collections::weak_set_add(set, interp.gc_heap_mut(), crate::Value::Object(key))
        .expect("weak set add");

    let global_this = *interp.global_this();
    crate::object::set(
        global_this,
        interp.gc_heap_mut(),
        "__weak_map_root",
        crate::Value::WeakMap(map),
    );
    crate::object::set(
        global_this,
        interp.gc_heap_mut(),
        "__weak_set_root",
        crate::Value::WeakSet(set),
    );
    crate::object::set(
        global_this,
        interp.gc_heap_mut(),
        "__weak_key_root",
        crate::Value::Object(key),
    );

    let _ = map;
    let _ = set;
    let _ = key;
    let _ = value;
    interp.force_gc();

    let rooted_key = crate::object::get(global_this, interp.gc_heap(), "__weak_key_root")
        .expect("weak key root survives force_gc");
    let rooted_map = crate::object::get(global_this, interp.gc_heap(), "__weak_map_root")
        .expect("weak map root survives force_gc");
    let rooted_set = crate::object::get(global_this, interp.gc_heap(), "__weak_set_root")
        .expect("weak set root survives force_gc");

    match (rooted_map, rooted_set) {
        (crate::Value::WeakMap(map), crate::Value::WeakSet(set)) => {
            assert!(
                crate::collections::weak_map_has(map, interp.gc_heap(), &rooted_key)
                    .expect("weak map has")
            );
            assert!(
                crate::collections::weak_set_has(set, interp.gc_heap(), &rooted_key)
                    .expect("weak set has")
            );
        }
        other => panic!("expected WeakMap/WeakSet after force_gc, got {other:?}"),
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
    promise.fulfill(interp.gc_heap_mut(), crate::Value::Object(object));

    let global_this = *interp.global_this();
    crate::object::set(
        global_this,
        interp.gc_heap_mut(),
        "__promise_root",
        crate::Value::Promise(promise),
    );

    let _ = object;
    let _ = promise;
    interp.force_gc();

    let rooted = crate::object::get(global_this, interp.gc_heap(), "__promise_root")
        .expect("promise root survives force_gc");
    match rooted {
        crate::Value::Promise(promise) => match promise.state(interp.gc_heap()) {
            PromiseState::Fulfilled(crate::Value::Object(_)) => {}
            other => panic!("expected fulfilled object promise after force_gc, got {other:?}"),
        },
        other => panic!("expected Value::Promise after force_gc, got {other:?}"),
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
        callee: crate::Value::Undefined,
        this_value: crate::Value::Undefined,
        args: smallvec::smallvec![crate::Value::Object(object)],
        context: None,
        result_capability: None,
        kind: crate::MicrotaskKind::Call,
    });

    let _ = object;
    interp.force_gc();

    let mut batch = interp
        .microtasks_mut()
        .begin_drain()
        .expect("outer drain batch");
    let task = batch.tasks.pop_front().expect("queued task");
    assert!(
        matches!(task.args.first(), Some(crate::Value::Object(_))),
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
    interp.force_gc();
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
    frame.registers[0] = Value::Object(object);

    let parked =
        crate::generator::alloc_parked_frame(interp.gc_heap_mut(), frame).expect("parked frame");
    let promise = crate::JsPromiseHandle::pending(interp.gc_heap_mut()).expect("promise");
    promise.perform_async_resume_then(
        interp.gc_heap_mut(),
        parked,
        0,
        PromiseCapability {
            promise: Value::Undefined,
            resolve: Value::Undefined,
            reject: Value::Undefined,
            context: None,
        },
        None,
    );

    let global_this = *interp.global_this();
    crate::object::set(
        global_this,
        interp.gc_heap_mut(),
        "__parked_frame_promise_root",
        Value::Promise(promise),
    );

    let _ = object;
    let _ = parked;
    let _ = promise;
    interp.force_gc();

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
        crate::Value::Object(target),
        crate::Value::Object(bound_this),
        smallvec::smallvec![crate::Value::Boolean(true)],
    )
    .expect("bound");
    let global_this = *interp.global_this();
    crate::object::set(
        global_this,
        interp.gc_heap_mut(),
        "__bound_root",
        crate::Value::BoundFunction(bound),
    );

    let _ = target;
    let _ = bound_this;
    let _ = bound;
    interp.force_gc();

    let rooted = crate::object::get(global_this, interp.gc_heap(), "__bound_root")
        .expect("bound root survives force_gc");
    match rooted {
        crate::Value::BoundFunction(bound) => {
            let (target, bound_this, args) = bound.parts(interp.gc_heap());
            assert!(matches!(target, crate::Value::Object(_)));
            assert!(matches!(bound_this, crate::Value::Object(_)));
            assert!(matches!(args.first(), Some(crate::Value::Boolean(true))));
        }
        other => panic!("expected Value::BoundFunction after force_gc, got {other:?}"),
    }
}

#[test]
fn regexp_root_survives_force_gc() {
    let mut interp = Interpreter::new();
    let pattern: Vec<u16> = "a+".encode_utf16().collect();
    let re = crate::JsRegExp::compile(interp.gc_heap_mut(), &pattern, "g").expect("regexp");
    let global_this = *interp.global_this();
    crate::object::set(
        global_this,
        interp.gc_heap_mut(),
        "__regexp_root",
        crate::Value::RegExp(re),
    );

    let _ = re;
    interp.force_gc();

    let rooted = crate::object::get(global_this, interp.gc_heap(), "__regexp_root")
        .expect("regexp root survives force_gc");
    match rooted {
        crate::Value::RegExp(re) => {
            let text: Vec<u16> = "aaab".encode_utf16().collect();
            let first = re
                .find_from_utf16(interp.gc_heap(), &text, 0)
                .into_iter()
                .next()
                .expect("regexp remains executable after force_gc");
            assert_eq!(first.range, 0..3);
        }
        other => panic!("expected Value::RegExp after force_gc, got {other:?}"),
    }
}
