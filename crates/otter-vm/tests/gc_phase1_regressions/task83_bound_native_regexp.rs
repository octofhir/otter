//! GC regressions for task 83 bound/native function and RegExp bodies.

use smallvec::smallvec;

use otter_vm::native_function::NATIVE_FUNCTION_BODY_TYPE_TAG;
use otter_vm::object::OBJECT_BODY_TYPE_TAG;
use otter_vm::regexp::REGEXP_BODY_TYPE_TAG;
use otter_vm::test_support::native_function_captures;
use otter_vm::{
    BOUND_FUNCTION_BODY_TYPE_TAG, BoundFunction, Interpreter, Value, native_value_with_captures,
};

fn live_bytes(interp: &mut Interpreter, tag: u8) -> usize {
    interp.gc_heap_mut().gc_stats().by_type[tag as usize].live_bytes
}

#[test]
fn bound_function_roots_target_this_and_args_when_rooted() {
    let mut interp = Interpreter::new();

    let target = otter_vm::object::alloc_object(interp.gc_heap_mut()).expect("target");
    let bound_this = otter_vm::object::alloc_object(interp.gc_heap_mut()).expect("this");
    let arg = otter_vm::object::alloc_object(interp.gc_heap_mut()).expect("arg");
    let bound = BoundFunction::new(
        interp.gc_heap_mut(),
        Value::Object(target),
        Value::Object(bound_this),
        smallvec![Value::Object(arg)],
    )
    .expect("bound function");

    let global = *interp.global_this();
    otter_vm::object::set(
        global,
        interp.gc_heap_mut(),
        "__task83_bound",
        Value::BoundFunction(bound),
    );

    let _ = target;
    let _ = bound_this;
    let _ = arg;
    let _ = bound;
    interp.force_gc();

    let rooted = otter_vm::object::get(global, interp.gc_heap(), "__task83_bound")
        .expect("bound function survives");
    let Value::BoundFunction(bound) = rooted else {
        panic!("expected bound function root");
    };
    let (target, bound_this, args) = bound.parts(interp.gc_heap());
    assert!(matches!(target, Value::Object(_)));
    assert!(matches!(bound_this, Value::Object(_)));
    assert!(matches!(args.first(), Some(Value::Object(_))));
}

#[test]
fn native_function_captures_root_gc_values_when_rooted() {
    let mut interp = Interpreter::new();

    let captured = otter_vm::object::alloc_object(interp.gc_heap_mut()).expect("capture");
    let native = native_value_with_captures(
        interp.gc_heap_mut(),
        "capture-root",
        smallvec![Value::Object(captured)],
        |_, _, _| Ok(Value::Undefined),
    )
    .expect("native");
    let global = *interp.global_this();
    otter_vm::object::set(
        global,
        interp.gc_heap_mut(),
        "__task83_native",
        native.clone(),
    );

    let _ = captured;
    interp.force_gc();

    let rooted = otter_vm::object::get(global, interp.gc_heap(), "__task83_native")
        .expect("native function survives");
    let Value::NativeFunction(native) = rooted else {
        panic!("expected native value after force_gc");
    };
    assert_eq!(native.name(interp.gc_heap()), "capture-root");
    let captures = native_function_captures(native, interp.gc_heap());
    assert!(
        matches!(captures.first(), Some(Value::Object(_))),
        "rooted native function must trace explicit captures"
    );
}

#[test]
fn regexp_body_survives_force_gc_when_rooted() {
    let mut interp = Interpreter::new();
    let units: Vec<u16> = "ab+c".encode_utf16().collect();
    let re = otter_vm::JsRegExp::compile(interp.gc_heap_mut(), &units, "i").expect("regexp");

    let global = *interp.global_this();
    otter_vm::object::set(
        global,
        interp.gc_heap_mut(),
        "__task83_regexp",
        Value::RegExp(re),
    );
    let _ = re;
    interp.force_gc();

    let rooted = otter_vm::object::get(global, interp.gc_heap(), "__task83_regexp")
        .expect("regexp survives");
    let Value::RegExp(re) = rooted else {
        panic!("expected regexp root");
    };
    let haystack: Vec<u16> = "abbbc".encode_utf16().collect();
    let m = re
        .find_from_utf16(interp.gc_heap(), &haystack, 0)
        .into_iter()
        .next()
        .expect("match after force_gc");
    assert_eq!(m.range, 0..5);
}

#[test]
fn bound_native_and_regexp_unrooted_graphs_are_reclaimed() {
    let mut interp = Interpreter::new();
    interp.force_gc();
    let object_baseline = live_bytes(&mut interp, OBJECT_BODY_TYPE_TAG);
    let bound_baseline = live_bytes(&mut interp, BOUND_FUNCTION_BODY_TYPE_TAG);
    let native_baseline = live_bytes(&mut interp, NATIVE_FUNCTION_BODY_TYPE_TAG);
    let regexp_baseline = live_bytes(&mut interp, REGEXP_BODY_TYPE_TAG);

    let bound_object = otter_vm::object::alloc_object(interp.gc_heap_mut()).expect("object");
    let bound = BoundFunction::new(
        interp.gc_heap_mut(),
        Value::Object(bound_object),
        Value::Object(bound_object),
        smallvec![Value::Object(bound_object)],
    )
    .expect("bound");
    otter_vm::object::set(
        bound_object,
        interp.gc_heap_mut(),
        "back",
        Value::BoundFunction(bound),
    );

    let native_object = otter_vm::object::alloc_object(interp.gc_heap_mut()).expect("object");
    let native = native_value_with_captures(
        interp.gc_heap_mut(),
        "cycle-native",
        smallvec![Value::Object(native_object)],
        |_, _, _| Ok(Value::Undefined),
    )
    .expect("native");
    otter_vm::object::set(native_object, interp.gc_heap_mut(), "back", native);

    let re = otter_vm::JsRegExp::compile(
        interp.gc_heap_mut(),
        &"z+".encode_utf16().collect::<Vec<_>>(),
        "",
    )
    .expect("regexp");
    let regexp_object = otter_vm::object::alloc_object(interp.gc_heap_mut()).expect("object");
    otter_vm::object::set(
        regexp_object,
        interp.gc_heap_mut(),
        "regexp",
        Value::RegExp(re),
    );

    let _ = bound_object;
    let _ = bound;
    let _ = native_object;
    let _ = regexp_object;
    let _ = re;
    interp.force_gc();

    assert!(live_bytes(&mut interp, BOUND_FUNCTION_BODY_TYPE_TAG) <= bound_baseline);
    assert!(live_bytes(&mut interp, NATIVE_FUNCTION_BODY_TYPE_TAG) <= native_baseline);
    assert!(live_bytes(&mut interp, REGEXP_BODY_TYPE_TAG) <= regexp_baseline);
    assert!(
        live_bytes(&mut interp, OBJECT_BODY_TYPE_TAG) <= object_baseline,
        "unrooted object cycles involving task83 bodies must be reclaimed"
    );
}
