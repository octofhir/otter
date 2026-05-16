//! Bound/native function and RegExp GC invariants.

use smallvec::smallvec;

use crate::native_function::NATIVE_FUNCTION_BODY_TYPE_TAG;
use crate::object::OBJECT_BODY_TYPE_TAG;
use crate::regexp::REGEXP_BODY_TYPE_TAG;
use crate::test_support::native_function_captures;
use crate::{
    BOUND_FUNCTION_BODY_TYPE_TAG, BoundFunction, Interpreter, Value, native_value_with_captures,
};

fn live_bytes(interp: &mut Interpreter, tag: u8) -> usize {
    interp.gc_heap_mut().gc_stats().by_type[tag as usize].live_bytes
}

#[test]
fn bound_function_roots_target_this_and_args_when_rooted() {
    let mut interp = Interpreter::new();

    let target = crate::test_support::alloc_old_object(interp.gc_heap_mut()).expect("target");
    let bound_this = crate::test_support::alloc_old_object(interp.gc_heap_mut()).expect("this");
    let arg = crate::test_support::alloc_old_object(interp.gc_heap_mut()).expect("arg");
    let bound = BoundFunction::new(
        interp.gc_heap_mut(),
        Value::Object(target),
        Value::Object(bound_this),
        smallvec![Value::Object(arg)],
    )
    .expect("bound function");

    let global = *interp.global_this();
    crate::object::set(
        global,
        interp.gc_heap_mut(),
        "__gc_bound_function",
        Value::BoundFunction(bound),
    );

    let _ = target;
    let _ = bound_this;
    let _ = arg;
    let _ = bound;
    interp.force_gc();

    let rooted = crate::object::get(global, interp.gc_heap(), "__gc_bound_function")
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

    let captured = crate::test_support::alloc_old_object(interp.gc_heap_mut()).expect("capture");
    let native = native_value_with_captures(
        interp.gc_heap_mut(),
        "capture-root",
        smallvec![Value::Object(captured)],
        |_, _, _| Ok(Value::Undefined),
    )
    .expect("native");
    let global = *interp.global_this();
    crate::object::set(
        global,
        interp.gc_heap_mut(),
        "__gc_native_function",
        native.clone(),
    );

    let _ = captured;
    interp.force_gc();

    let rooted = crate::object::get(global, interp.gc_heap(), "__gc_native_function")
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
    let re = crate::JsRegExp::compile(interp.gc_heap_mut(), &units, "i").expect("regexp");

    let global = *interp.global_this();
    crate::object::set(
        global,
        interp.gc_heap_mut(),
        "__gc_regexp",
        Value::RegExp(re),
    );
    let _ = re;
    interp.force_gc();

    let rooted =
        crate::object::get(global, interp.gc_heap(), "__gc_regexp").expect("regexp survives");
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

    let bound_object = crate::test_support::alloc_old_object(interp.gc_heap_mut()).expect("object");
    let bound = BoundFunction::new(
        interp.gc_heap_mut(),
        Value::Object(bound_object),
        Value::Object(bound_object),
        smallvec![Value::Object(bound_object)],
    )
    .expect("bound");
    crate::object::set(
        bound_object,
        interp.gc_heap_mut(),
        "back",
        Value::BoundFunction(bound),
    );

    let native_object =
        crate::test_support::alloc_old_object(interp.gc_heap_mut()).expect("object");
    let native = native_value_with_captures(
        interp.gc_heap_mut(),
        "cycle-native",
        smallvec![Value::Object(native_object)],
        |_, _, _| Ok(Value::Undefined),
    )
    .expect("native");
    crate::object::set(native_object, interp.gc_heap_mut(), "back", native);

    let re = crate::JsRegExp::compile(
        interp.gc_heap_mut(),
        &"z+".encode_utf16().collect::<Vec<_>>(),
        "",
    )
    .expect("regexp");
    let regexp_object =
        crate::test_support::alloc_old_object(interp.gc_heap_mut()).expect("object");
    crate::object::set(
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
        "unrooted object cycles involving callable/regexp bodies must be reclaimed"
    );
}
