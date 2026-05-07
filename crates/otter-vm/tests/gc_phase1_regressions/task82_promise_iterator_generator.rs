//! GC regressions for task 82 promise / iterator / generator bodies.

use smallvec::smallvec;

use otter_bytecode::Function;
use otter_vm::generator::{GENERATOR_BODY_TYPE_TAG, PARKED_FRAME_BODY_TYPE_TAG};
use otter_vm::object::OBJECT_BODY_TYPE_TAG;
use otter_vm::promise::{JsPromise, PURE_PROMISE_BODY_TYPE_TAG};
use otter_vm::{Frame, ITERATOR_STATE_TYPE_TAG, Interpreter, IteratorState, Value};

fn empty_function() -> Function {
    Function {
        id: 0,
        name: "gc-test".to_string(),
        locals: 1,
        scratch: 1,
        ..Function::default()
    }
}

fn live_bytes(interp: &mut Interpreter, tag: u8) -> usize {
    interp.gc_heap_mut().gc_stats().by_type[tag as usize].live_bytes
}

#[test]
fn promise_reaction_graph_survives_force_gc_when_rooted() {
    let mut interp = Interpreter::new();
    interp.force_gc();
    let baseline = live_bytes(&mut interp, OBJECT_BODY_TYPE_TAG);

    let promise = otter_vm::JsPromiseHandle::pending(interp.gc_heap_mut()).expect("promise");
    let retained = otter_vm::object::alloc_object(interp.gc_heap_mut()).expect("object");
    let capability = otter_vm::PromiseCapability {
        promise: Value::Object(retained),
        resolve: Value::Undefined,
        reject: Value::Undefined,
    };
    promise.perform_then(interp.gc_heap_mut(), None, None, capability);

    let global = *interp.global_this();
    otter_vm::object::set(
        global,
        interp.gc_heap_mut(),
        "promise",
        Value::Promise(promise),
    );
    let _ = retained;
    interp.force_gc();

    let after = live_bytes(&mut interp, OBJECT_BODY_TYPE_TAG);
    assert!(
        after > baseline,
        "rooted promise reaction capability must keep object live"
    );
}

#[test]
fn deep_promise_chain_is_reaped_when_unrooted() {
    let mut interp = Interpreter::new();
    interp.force_gc();
    let baseline = live_bytes(&mut interp, PURE_PROMISE_BODY_TYPE_TAG);

    let mut current = otter_vm::JsPromiseHandle::pending(interp.gc_heap_mut()).expect("promise");
    for _ in 0..100_000 {
        let next = otter_vm::JsPromiseHandle::pending(interp.gc_heap_mut()).expect("promise");
        let capability = otter_vm::PromiseCapability {
            promise: Value::Promise(next),
            resolve: Value::Undefined,
            reject: Value::Undefined,
        };
        current.perform_then(interp.gc_heap_mut(), None, None, capability);
        current = next;
    }

    assert!(
        live_bytes(&mut interp, PURE_PROMISE_BODY_TYPE_TAG) > baseline,
        "promise chain setup must allocate pure promise bodies"
    );
    let _ = current;
    interp.force_gc();
    assert_eq!(
        live_bytes(&mut interp, PURE_PROMISE_BODY_TYPE_TAG),
        baseline,
        "unrooted deep promise chain should be reclaimed"
    );
}

#[test]
fn pending_promise_microtask_payload_roots_until_drained() {
    let mut interp = Interpreter::new();
    interp.force_gc();
    let baseline = live_bytes(&mut interp, OBJECT_BODY_TYPE_TAG);
    let payload = otter_vm::object::alloc_object(interp.gc_heap_mut()).expect("object");
    interp.microtasks_mut().enqueue(otter_vm::Microtask {
        callee: Value::Undefined,
        this_value: Value::Undefined,
        args: smallvec![Value::Object(payload)],
        result_capability: None,
        kind: otter_vm::MicrotaskKind::Call,
    });

    let _ = payload;
    interp.force_gc();
    let rooted = live_bytes(&mut interp, OBJECT_BODY_TYPE_TAG);
    assert!(
        rooted > baseline,
        "pending microtask payload must root object"
    );

    interp.microtasks_mut().clear_for_tests();
    interp.force_gc();
    let after = live_bytes(&mut interp, OBJECT_BODY_TYPE_TAG);
    assert_eq!(
        after, baseline,
        "cleared microtask payload should be reaped"
    );
}

#[test]
fn iterator_state_holding_array_object_survives_force_gc() {
    let mut interp = Interpreter::new();
    let object = otter_vm::object::alloc_object(interp.gc_heap_mut()).expect("object");
    let array = otter_vm::array::from_elements(interp.gc_heap_mut(), [Value::Object(object)])
        .expect("array");
    let iter = otter_vm::alloc_iterator_state(
        interp.gc_heap_mut(),
        IteratorState::Array { array, index: 0 },
    )
    .expect("iterator");
    let global = *interp.global_this();
    otter_vm::object::set(global, interp.gc_heap_mut(), "iter", Value::Iterator(iter));

    let _ = object;
    let _ = array;
    let _ = iter;
    interp.force_gc();

    let rooted = otter_vm::object::get(global, interp.gc_heap(), "iter").expect("iter");
    let Value::Iterator(iter) = rooted else {
        panic!("expected iterator root")
    };
    let array = interp.gc_heap().read_payload(iter, |state| match state {
        IteratorState::Array { array, .. } => *array,
        other => panic!("expected array iterator, got {other:?}"),
    });
    assert!(matches!(
        otter_vm::array::get(array, interp.gc_heap(), 0),
        Value::Object(_)
    ));
}

#[test]
fn generator_and_parked_frame_roots_register_values() {
    let mut interp = Interpreter::new();
    let function = empty_function();
    let mut frame = Frame::for_function_with_heap(&function, interp.gc_heap_mut()).expect("frame");
    let object = otter_vm::object::alloc_object(interp.gc_heap_mut()).expect("object");
    frame.registers[0] = Value::Object(object);
    let generator =
        otter_vm::generator::JsGenerator::new(interp.gc_heap_mut(), frame).expect("generator");
    let global = *interp.global_this();
    otter_vm::object::set(
        global,
        interp.gc_heap_mut(),
        "generator",
        Value::Generator(generator),
    );

    let _ = object;
    interp.force_gc();
    generator.with_body(interp.gc_heap(), |body| {
        let frame = body.frame.as_ref().expect("saved frame");
        assert!(matches!(frame.registers[0], Value::Object(_)));
    });

    let mut parked_frame =
        Frame::for_function_with_heap(&function, interp.gc_heap_mut()).expect("parked frame");
    let parked_object = otter_vm::object::alloc_object(interp.gc_heap_mut()).expect("object");
    parked_frame.registers[0] = Value::Object(parked_object);
    let parked =
        otter_vm::generator::alloc_parked_frame(interp.gc_heap_mut(), parked_frame).expect("park");
    let promise = otter_vm::JsPromiseHandle::pending(interp.gc_heap_mut()).expect("promise");
    let capability =
        otter_vm::promise_dispatch::make_capability(interp.gc_heap_mut()).expect("capability");
    promise.perform_async_resume_then(interp.gc_heap_mut(), parked, 0, capability, None);
    otter_vm::object::set(
        global,
        interp.gc_heap_mut(),
        "awaitPromise",
        Value::Promise(promise),
    );

    let _ = parked_object;
    interp.force_gc();
    let live = live_bytes(&mut interp, PARKED_FRAME_BODY_TYPE_TAG);
    assert!(
        live > 0,
        "pending await reaction must retain parked frame body"
    );
}

#[test]
fn promise_iterator_generator_cycles_reclaimed_when_unrooted() {
    let mut interp = Interpreter::new();
    interp.force_gc();
    let promise_baseline = live_bytes(&mut interp, PURE_PROMISE_BODY_TYPE_TAG);
    let iter_baseline = live_bytes(&mut interp, ITERATOR_STATE_TYPE_TAG);
    let gen_baseline = live_bytes(&mut interp, GENERATOR_BODY_TYPE_TAG);

    let promise = otter_vm::JsPromiseHandle::pending(interp.gc_heap_mut()).expect("promise");
    promise.fulfill(interp.gc_heap_mut(), Value::Promise(promise));

    let iter = otter_vm::alloc_iterator_state(interp.gc_heap_mut(), IteratorState::Exhausted)
        .expect("iterator");
    interp.gc_heap_mut().with_payload(iter, |state| {
        *state = IteratorState::FlatMap {
            source: iter,
            mapper: Value::Undefined,
            inner: Some(iter),
        };
    });

    let function = empty_function();
    let frame = Frame::for_function_with_heap(&function, interp.gc_heap_mut()).expect("frame");
    let generator =
        otter_vm::generator::JsGenerator::new(interp.gc_heap_mut(), frame).expect("generator");
    generator.install_owner_on_frame(interp.gc_heap_mut());

    let _ = promise;
    let _ = iter;
    let _ = generator;
    interp.force_gc();

    assert_eq!(
        live_bytes(&mut interp, PURE_PROMISE_BODY_TYPE_TAG),
        promise_baseline
    );
    assert_eq!(
        live_bytes(&mut interp, ITERATOR_STATE_TYPE_TAG),
        iter_baseline
    );
    assert_eq!(
        live_bytes(&mut interp, GENERATOR_BODY_TYPE_TAG),
        gen_baseline
    );
}
