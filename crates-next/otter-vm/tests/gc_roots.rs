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

/// Sanity check: the walker compiles and runs. As of task 77
/// the `JsObject` arm of [`GcTrace`] emits its slot pointer,
/// so a fresh interpreter yields at least one slot —
/// `globalThis`. The exact count is not load-bearing; what
/// matters is that the walker terminates without panicking
/// and emits *some* roots once the JsObject migration lands.
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
        "post-task-77 walker must surface at least globalThis (got {count})"
    );
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

/// Globals act as a strong root: an object stamped onto
/// `globalThis` survives a forced full GC even after every
/// host-side handle is dropped.
///
/// Spec: <https://tc39.es/ecma262/#sec-global-object>
/// Walker root source: GC architecture plan §4.2.
#[test]
fn globals_keep_object_alive() {
    use otter_vm::object::OBJECT_BODY_TYPE_TAG;

    let mut interp = Interpreter::new();

    // Allocate a fresh object and stash it on globalThis under
    // a unique key. From this point the only path to the body
    // is through globalThis → property slot → ObjectBody.
    let baseline =
        interp.gc_heap_mut().gc_stats().by_type[OBJECT_BODY_TYPE_TAG as usize].live_bytes;
    let stashed = otter_vm::object::alloc_object(interp.gc_heap_mut()).expect("alloc_object");
    let global = *interp.global_this();
    otter_vm::object::set(
        global,
        interp.gc_heap_mut(),
        "__gc_roots_test_stash",
        otter_vm::Value::Object(stashed),
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
    let resolved = otter_vm::object::get(global, interp.gc_heap(), "__gc_roots_test_stash")
        .expect("globalThis property survives force_gc");
    match resolved {
        otter_vm::Value::Object(_) => {}
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
    use otter_vm::object::OBJECT_BODY_TYPE_TAG;

    let mut interp = Interpreter::new();

    // Register a fresh empty `module_env` object under a
    // synthetic URL. Then stash a property on it whose value
    // is a freshly-allocated object. The body of the stashed
    // object is now reachable only through:
    //   module_environments[url] → module_env → property slot.
    let baseline =
        interp.gc_heap_mut().gc_stats().by_type[OBJECT_BODY_TYPE_TAG as usize].live_bytes;
    let module_env = otter_vm::object::alloc_object(interp.gc_heap_mut()).expect("alloc_object");
    let url: std::rc::Rc<str> = std::rc::Rc::from("file:///gc_roots_test.js");
    interp.register_module_env(std::rc::Rc::clone(&url), module_env);

    let stashed = otter_vm::object::alloc_object(interp.gc_heap_mut()).expect("alloc_object");
    otter_vm::object::set(
        module_env,
        interp.gc_heap_mut(),
        "stash",
        otter_vm::Value::Object(stashed),
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
    let resolved = otter_vm::object::get(env_handle, interp.gc_heap(), "stash")
        .expect("module-env property survives force_gc");
    match resolved {
        otter_vm::Value::Object(_) => {}
        other => panic!("expected Value::Object after force_gc, got {other:?}"),
    }
}

#[test]
fn error_class_registry_prototypes_survive_force_gc() {
    let mut interp = Interpreter::new();
    interp.force_gc();

    let registry = interp.error_classes_clone();
    let proto = registry.prototype(otter_vm::ErrorKind::TypeError);
    let name = otter_vm::object::get(proto, interp.gc_heap(), "name")
        .expect("TypeError.prototype.name survives force_gc");

    match name {
        otter_vm::Value::String(s) => assert_eq!(s.to_lossy_string(), "TypeError"),
        other => panic!("expected TypeError.prototype.name string, got {other:?}"),
    }
}

#[test]
fn array_element_root_survives_force_gc() {
    let mut interp = Interpreter::new();
    let arr = otter_vm::array::alloc_array(interp.gc_heap_mut()).expect("alloc array");
    otter_vm::array::push(arr, interp.gc_heap_mut(), otter_vm::Value::Boolean(true))
        .expect("push element");
    let global_this = *interp.global_this();
    otter_vm::object::set(
        global_this,
        interp.gc_heap_mut(),
        "__array_root",
        otter_vm::Value::Array(arr),
    );

    let _ = arr;
    interp.force_gc();

    let rooted = otter_vm::object::get(global_this, interp.gc_heap(), "__array_root")
        .expect("array root survives force_gc");
    match rooted {
        otter_vm::Value::Array(array) => {
            assert_eq!(
                otter_vm::array::get(array, interp.gc_heap(), 0),
                otter_vm::Value::Boolean(true)
            );
        }
        other => panic!("expected Value::Array after force_gc, got {other:?}"),
    }
}

#[test]
fn map_entry_root_survives_force_gc() {
    let mut interp = Interpreter::new();
    let map = otter_vm::collections::alloc_map(interp.gc_heap_mut()).expect("alloc map");
    let stashed = otter_vm::object::alloc_object(interp.gc_heap_mut()).expect("alloc object");
    let key = otter_vm::Value::Number(otter_vm::NumberValue::Smi(7));
    otter_vm::collections::map_set(
        map,
        interp.gc_heap_mut(),
        key.clone(),
        otter_vm::Value::Object(stashed),
    )
    .expect("map set");

    let global_this = *interp.global_this();
    otter_vm::object::set(
        global_this,
        interp.gc_heap_mut(),
        "__map_root",
        otter_vm::Value::Map(map),
    );

    let _ = map;
    let _ = stashed;
    interp.force_gc();

    let rooted = otter_vm::object::get(global_this, interp.gc_heap(), "__map_root")
        .expect("map root survives force_gc");
    match rooted {
        otter_vm::Value::Map(rooted_map) => {
            let value = otter_vm::collections::map_get(rooted_map, interp.gc_heap(), &key)
                .expect("map entry survives force_gc");
            assert!(matches!(value, otter_vm::Value::Object(_)));
        }
        other => panic!("expected Value::Map after force_gc, got {other:?}"),
    }
}

#[test]
fn weak_collections_root_survives_force_gc() {
    let mut interp = Interpreter::new();
    let map = otter_vm::collections::alloc_weak_map(interp.gc_heap_mut()).expect("alloc WeakMap");
    let set = otter_vm::collections::alloc_weak_set(interp.gc_heap_mut()).expect("alloc WeakSet");
    let key = otter_vm::object::alloc_object(interp.gc_heap_mut()).expect("alloc key");
    let value = otter_vm::object::alloc_object(interp.gc_heap_mut()).expect("alloc value");

    otter_vm::collections::weak_map_set(
        map,
        interp.gc_heap_mut(),
        otter_vm::Value::Object(key),
        otter_vm::Value::Object(value),
    )
    .expect("weak map set");
    otter_vm::collections::weak_set_add(set, interp.gc_heap_mut(), otter_vm::Value::Object(key))
        .expect("weak set add");

    let global_this = *interp.global_this();
    otter_vm::object::set(
        global_this,
        interp.gc_heap_mut(),
        "__weak_map_root",
        otter_vm::Value::WeakMap(map),
    );
    otter_vm::object::set(
        global_this,
        interp.gc_heap_mut(),
        "__weak_set_root",
        otter_vm::Value::WeakSet(set),
    );
    otter_vm::object::set(
        global_this,
        interp.gc_heap_mut(),
        "__weak_key_root",
        otter_vm::Value::Object(key),
    );

    let _ = map;
    let _ = set;
    let _ = key;
    let _ = value;
    interp.force_gc();

    let rooted_key = otter_vm::object::get(global_this, interp.gc_heap(), "__weak_key_root")
        .expect("weak key root survives force_gc");
    let rooted_map = otter_vm::object::get(global_this, interp.gc_heap(), "__weak_map_root")
        .expect("weak map root survives force_gc");
    let rooted_set = otter_vm::object::get(global_this, interp.gc_heap(), "__weak_set_root")
        .expect("weak set root survives force_gc");

    match (rooted_map, rooted_set) {
        (otter_vm::Value::WeakMap(map), otter_vm::Value::WeakSet(set)) => {
            assert!(
                otter_vm::collections::weak_map_has(map, interp.gc_heap(), &rooted_key)
                    .expect("weak map has")
            );
            assert!(
                otter_vm::collections::weak_set_has(set, interp.gc_heap(), &rooted_key)
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
    use otter_vm::object::OBJECT_BODY_TYPE_TAG;
    use otter_vm::promise::{JsPromise, PromiseState};

    let mut interp = Interpreter::new();
    interp.force_gc();
    let baseline =
        interp.gc_heap_mut().gc_stats().by_type[OBJECT_BODY_TYPE_TAG as usize].live_bytes;

    let promise = otter_vm::JsPromiseHandle::pending(interp.gc_heap_mut()).expect("promise");
    let object = otter_vm::object::alloc_object(interp.gc_heap_mut()).expect("object");
    promise.fulfill(interp.gc_heap_mut(), otter_vm::Value::Object(object));

    let global_this = *interp.global_this();
    otter_vm::object::set(
        global_this,
        interp.gc_heap_mut(),
        "__promise_root",
        otter_vm::Value::Promise(promise),
    );

    let _ = object;
    let _ = promise;
    interp.force_gc();

    let after = interp.gc_heap_mut().gc_stats().by_type[OBJECT_BODY_TYPE_TAG as usize].live_bytes;
    assert!(
        after > baseline,
        "rooted fulfilled promise must retain its object value"
    );

    let rooted = otter_vm::object::get(global_this, interp.gc_heap(), "__promise_root")
        .expect("promise root survives force_gc");
    match rooted {
        otter_vm::Value::Promise(promise) => match promise.state(interp.gc_heap()) {
            PromiseState::Fulfilled(otter_vm::Value::Object(_)) => {}
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
    use otter_vm::object::OBJECT_BODY_TYPE_TAG;

    let mut interp = Interpreter::new();
    interp.force_gc();
    let baseline =
        interp.gc_heap_mut().gc_stats().by_type[OBJECT_BODY_TYPE_TAG as usize].live_bytes;

    let object = otter_vm::object::alloc_object(interp.gc_heap_mut()).expect("object");
    interp.microtasks_mut().enqueue(otter_vm::Microtask {
        callee: otter_vm::Value::Undefined,
        this_value: otter_vm::Value::Undefined,
        args: smallvec::smallvec![otter_vm::Value::Object(object)],
        result_capability: None,
        kind: otter_vm::MicrotaskKind::Call,
    });

    let _ = object;
    interp.force_gc();
    let after = interp.gc_heap_mut().gc_stats().by_type[OBJECT_BODY_TYPE_TAG as usize].live_bytes;
    assert!(
        after > baseline,
        "pending microtask payload must retain its object argument"
    );

    let mut batch = interp
        .microtasks_mut()
        .begin_drain()
        .expect("outer drain batch");
    let task = batch.tasks.pop_front().expect("queued task");
    assert!(
        matches!(task.args.first(), Some(otter_vm::Value::Object(_))),
        "microtask payload remains observable after force_gc"
    );
    interp.microtasks_mut().end_drain();
}

#[test]
fn parked_frame_keeps_alive() {
    use otter_bytecode::Function;
    use otter_vm::generator::PARKED_FRAME_BODY_TYPE_TAG;
    use otter_vm::{Frame, PromiseCapability, Value};

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
    let object = otter_vm::object::alloc_object(interp.gc_heap_mut()).expect("object");
    frame.registers[0] = Value::Object(object);

    let parked =
        otter_vm::generator::alloc_parked_frame(interp.gc_heap_mut(), frame).expect("parked frame");
    let promise = otter_vm::JsPromiseHandle::pending(interp.gc_heap_mut()).expect("promise");
    promise.perform_async_resume_then(
        interp.gc_heap_mut(),
        parked,
        0,
        PromiseCapability {
            promise: Value::Undefined,
            resolve: Value::Undefined,
            reject: Value::Undefined,
        },
        None,
    );

    let global_this = *interp.global_this();
    otter_vm::object::set(
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
