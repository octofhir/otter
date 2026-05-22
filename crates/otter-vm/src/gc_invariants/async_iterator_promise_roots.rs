//! Promise, iterator, generator, and microtask GC invariants.

use smallvec::smallvec;

use crate::generator::GENERATOR_BODY_TYPE_TAG;
use crate::promise::{JsPromise, PURE_PROMISE_BODY_TYPE_TAG};
use crate::test_support::{
    promise_fulfill_reaction_count, promise_fulfill_reaction_debug,
    promise_has_object_fulfill_capability,
};
use crate::{Frame, ITERATOR_STATE_TYPE_TAG, Interpreter, IteratorState, Value};
use otter_bytecode::Function;

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

    let promise = crate::JsPromiseHandle::pending(interp.gc_heap_mut()).expect("promise");
    let retained = crate::test_support::alloc_old_object(interp.gc_heap_mut()).expect("object");
    let capability = crate::PromiseCapability {
        promise: Value::Object(retained),
        resolve: Value::Undefined,
        reject: Value::Undefined,
        context: None,
    };
    promise.perform_then(interp.gc_heap_mut(), None, None, capability);

    let global = *interp.global_this();
    crate::object::set(
        global,
        interp.gc_heap_mut(),
        "promise",
        Value::Promise(promise),
    );
    let _ = retained;
    interp.force_gc();

    let global = *interp.global_this();
    let rooted = crate::object::get(global, interp.gc_heap(), "promise")
        .expect("promise root survives force_gc");
    let Value::Promise(promise) = rooted else {
        panic!("expected rooted promise after force_gc")
    };
    assert!(
        promise_has_object_fulfill_capability(promise, interp.gc_heap()),
        "rooted promise reaction capability must keep object live"
    );
}

#[test]
fn deep_promise_chain_is_reaped_when_unrooted() {
    let mut interp = Interpreter::new();
    interp.force_gc();
    let baseline = live_bytes(&mut interp, PURE_PROMISE_BODY_TYPE_TAG);

    let mut current = crate::JsPromiseHandle::pending(interp.gc_heap_mut()).expect("promise");
    for _ in 0..10_000 {
        let next = crate::JsPromiseHandle::pending(interp.gc_heap_mut()).expect("promise");
        let capability = crate::PromiseCapability {
            promise: Value::Promise(next),
            resolve: Value::Undefined,
            reject: Value::Undefined,
            context: None,
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
    let payload = crate::test_support::alloc_old_object(interp.gc_heap_mut()).expect("object");
    interp.microtasks_mut().enqueue(crate::Microtask {
        callee: Value::Undefined,
        this_value: Value::Undefined,
        args: smallvec![Value::object(payload)],
        context: None,
        result_capability: None,
        kind: crate::MicrotaskKind::Call,
    });

    let _ = payload;
    interp.force_gc();

    let mut batch = interp
        .microtasks_mut()
        .begin_drain()
        .expect("outer drain batch");
    let task = batch.tasks.pop_front().expect("queued task");
    assert!(
        matches!(task.args.first(), Some(Value::Object(_))),
        "pending microtask payload must root object"
    );
    interp.microtasks_mut().end_drain();
}

#[test]
fn iterator_state_holding_array_object_survives_force_gc() {
    let mut interp = Interpreter::new();
    let object = crate::test_support::alloc_old_object(interp.gc_heap_mut()).expect("object");
    let array =
        crate::test_support::array_from_elements_old(interp.gc_heap_mut(), [Value::object(object)])
            .expect("array");
    let iter = interp
        .gc_heap_mut()
        .alloc_old(IteratorState::Array {
            array,
            index: 0,
            origin: crate::BuiltinIteratorOrigin::Array,
        })
        .expect("iterator");
    let global = *interp.global_this();
    crate::object::set(global, interp.gc_heap_mut(), "iter", Value::Iterator(iter));

    let _ = object;
    let _ = array;
    let _ = iter;
    interp.force_gc();

    let global = *interp.global_this();
    let rooted = crate::object::get(global, interp.gc_heap(), "iter").expect("iter");
    let Value::Iterator(iter) = rooted else {
        panic!("expected iterator root")
    };
    let array = interp.gc_heap().read_payload(iter, |state| match state {
        IteratorState::Array { array, .. } => *array,
        other => panic!("expected array iterator, got {other:?}"),
    });
    assert!(matches!(
        crate::array::get(array, interp.gc_heap(), 0),
        Value::Object(_)
    ));
}

#[test]
fn generator_and_parked_frame_roots_register_values() {
    let mut interp = Interpreter::new();
    let function = empty_function();
    let mut frame = Frame::for_function_with_heap(&function, interp.gc_heap_mut()).expect("frame");
    let object = crate::test_support::alloc_old_object(interp.gc_heap_mut()).expect("object");
    frame.registers[0] = Value::object(object);
    let generator =
        crate::generator::JsGenerator::new(interp.gc_heap_mut(), frame).expect("generator");
    let global = *interp.global_this();
    crate::object::set(
        global,
        interp.gc_heap_mut(),
        "generator",
        Value::Generator(generator),
    );

    let _ = object;
    interp.force_gc();
    let global = *interp.global_this();
    let rooted = crate::object::get(global, interp.gc_heap(), "generator")
        .expect("generator root survives force_gc");
    let Value::Generator(generator) = rooted else {
        panic!("expected rooted generator after force_gc")
    };
    generator.with_body(interp.gc_heap(), |body| {
        let frame = body.frame.as_ref().expect("saved frame");
        assert!(matches!(frame.registers[0], Value::Object(_)));
    });

    let mut parked_frame =
        Frame::for_function_with_heap(&function, interp.gc_heap_mut()).expect("parked frame");
    let parked_object =
        crate::test_support::alloc_old_object(interp.gc_heap_mut()).expect("object");
    parked_frame.registers[0] = Value::object(parked_object);
    let parked =
        crate::generator::alloc_parked_frame(interp.gc_heap_mut(), parked_frame).expect("park");
    let promise = crate::JsPromiseHandle::pending(interp.gc_heap_mut()).expect("promise");
    let capability = crate::PromiseCapability {
        promise: Value::Undefined,
        resolve: Value::Undefined,
        reject: Value::Undefined,
        context: None,
    };
    promise.perform_async_resume_then(interp.gc_heap_mut(), parked, 0, capability, None);
    crate::object::set(
        global,
        interp.gc_heap_mut(),
        "awaitPromise",
        Value::Promise(promise),
    );

    let _ = parked_object;
    interp.force_gc();
    let global = *interp.global_this();
    let rooted = crate::object::get(global, interp.gc_heap(), "awaitPromise")
        .expect("await promise root survives force_gc");
    let Value::Promise(promise) = rooted else {
        panic!("expected rooted await promise after force_gc")
    };
    let reaction_count = promise_fulfill_reaction_count(promise, interp.gc_heap());
    assert!(
        reaction_count > 0,
        "pending await reaction must retain parked frame body; fulfill reaction count={}, handlers={:?}",
        reaction_count,
        promise_fulfill_reaction_debug(promise, interp.gc_heap())
    );
}

#[test]
fn promise_iterator_generator_cycles_reclaimed_when_unrooted() {
    let mut interp = Interpreter::new();
    interp.force_gc();
    let promise_baseline = live_bytes(&mut interp, PURE_PROMISE_BODY_TYPE_TAG);
    let iter_baseline = live_bytes(&mut interp, ITERATOR_STATE_TYPE_TAG);
    let gen_baseline = live_bytes(&mut interp, GENERATOR_BODY_TYPE_TAG);

    let promise = crate::JsPromiseHandle::pending(interp.gc_heap_mut()).expect("promise");
    promise.fulfill(interp.gc_heap_mut(), Value::Promise(promise));

    let iter = interp
        .gc_heap_mut()
        .alloc_old(IteratorState::Exhausted)
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
        crate::generator::JsGenerator::new(interp.gc_heap_mut(), frame).expect("generator");
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
