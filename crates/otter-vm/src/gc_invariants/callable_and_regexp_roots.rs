//! Bound/native function and RegExp GC invariants.

use smallvec::smallvec;

use crate::native_function::NATIVE_FUNCTION_BODY_TYPE_TAG;
use crate::object::OBJECT_BODY_TYPE_TAG;
use crate::regexp::REGEXP_BODY_TYPE_TAG;
use crate::rooting::RootScopeExt;
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
    let _runtime_roots = interp.scope_runtime_roots_guard();
    let mut global = *interp.global_this();
    let mut target = Value::undefined();
    let mut bound_this = Value::undefined();
    let mut arg = Value::undefined();
    let mut bound = Value::undefined();
    let mut roots = otter_gc::RootScope::new(interp.gc_heap_mut());
    // SAFETY: all slots precede the scope and remain stationary through the
    // complete allocation and forced-collection sequence.
    unsafe {
        roots.add_object(&mut global);
        roots.add_value(&mut target);
        roots.add_value(&mut bound_this);
        roots.add_value(&mut arg);
        roots.add_value(&mut bound);
    }
    target =
        Value::object(crate::test_support::alloc_old_object(interp.gc_heap_mut()).expect("target"));
    bound_this =
        Value::object(crate::test_support::alloc_old_object(interp.gc_heap_mut()).expect("this"));
    arg = Value::object(crate::test_support::alloc_old_object(interp.gc_heap_mut()).expect("arg"));
    bound = Value::bound_function(
        BoundFunction::new(interp.gc_heap_mut(), target, bound_this, smallvec![arg])
            .expect("bound function"),
    );

    crate::object::set(
        &mut global,
        interp.gc_heap_mut(),
        "__gc_bound_function",
        bound,
    );

    interp.force_gc().expect("force GC");

    let rooted = crate::object::get(global, interp.gc_heap(), "__gc_bound_function")
        .expect("bound function survives");
    let Some(bound) = rooted.as_bound_function() else {
        panic!("expected bound function root");
    };
    let (target, bound_this, args) = bound.parts(interp.gc_heap());
    assert!(target.is_object());
    assert!(bound_this.is_object());
    assert!(args.first().is_some_and(|v| v.is_object()));
}

#[test]
fn native_function_captures_root_gc_values_when_rooted() {
    let mut interp = Interpreter::new();
    let _runtime_roots = interp.scope_runtime_roots_guard();
    let mut global = *interp.global_this();
    let mut captured = Value::undefined();
    let mut native = Value::undefined();
    let mut roots = otter_gc::RootScope::new(interp.gc_heap_mut());
    // SAFETY: all slots precede the scope and remain stationary through the
    // complete allocation and forced-collection sequence.
    unsafe {
        roots.add_object(&mut global);
        roots.add_value(&mut captured);
        roots.add_value(&mut native);
    }
    captured = Value::object(
        crate::test_support::alloc_old_object(interp.gc_heap_mut()).expect("capture"),
    );
    native = native_value_with_captures(
        interp.gc_heap_mut(),
        "capture-root",
        smallvec![captured],
        |_, _, _| Ok(Value::undefined()),
    )
    .expect("native");
    crate::object::set(
        &mut global,
        interp.gc_heap_mut(),
        "__gc_native_function",
        native,
    );

    interp.force_gc().expect("force GC");

    let rooted = crate::object::get(global, interp.gc_heap(), "__gc_native_function")
        .expect("native function survives");
    let Some(native) = rooted.as_native_function() else {
        panic!("expected native value after force_gc");
    };
    assert_eq!(native.name_string(interp.gc_heap()), "capture-root");
    let captures = native_function_captures(native, interp.gc_heap());
    assert!(
        captures.first().is_some_and(|v| v.is_object()),
        "rooted native function must trace explicit captures"
    );
}

#[test]
fn regexp_body_survives_force_gc_when_rooted() {
    let mut interp = Interpreter::new();
    let units: Vec<u16> = "ab+c".encode_utf16().collect();
    let re = crate::JsRegExp::compile(interp.gc_heap_mut(), &units, "i").expect("regexp");

    let mut global = *interp.global_this();
    crate::object::set(
        &mut global,
        interp.gc_heap_mut(),
        "__gc_regexp",
        Value::regexp(re),
    );
    let _ = re;
    interp.force_gc().expect("force GC");

    let rooted =
        crate::object::get(global, interp.gc_heap(), "__gc_regexp").expect("regexp survives");
    let Some(re) = rooted.as_regexp() else {
        panic!("expected regexp root");
    };
    let haystack: Vec<u16> = "abbbc".encode_utf16().collect();
    let m = re
        .find_from_utf16(interp.gc_heap(), &haystack, 0)
        .result
        .expect("within matcher budget")
        .into_iter()
        .next()
        .expect("match after force_gc");
    assert_eq!(m.range, 0..5);
}

#[test]
fn bound_native_and_regexp_unrooted_graphs_are_reclaimed() {
    let mut interp = Interpreter::new();
    interp.force_gc().expect("force GC");
    let object_baseline = live_bytes(&mut interp, OBJECT_BODY_TYPE_TAG);
    let bound_baseline = live_bytes(&mut interp, BOUND_FUNCTION_BODY_TYPE_TAG);
    let native_baseline = live_bytes(&mut interp, NATIVE_FUNCTION_BODY_TYPE_TAG);
    let regexp_baseline = live_bytes(&mut interp, REGEXP_BODY_TYPE_TAG);

    let mut bound_object =
        crate::test_support::alloc_old_object(interp.gc_heap_mut()).expect("object");
    let bound = BoundFunction::new(
        interp.gc_heap_mut(),
        Value::object(bound_object),
        Value::object(bound_object),
        smallvec![Value::object(bound_object)],
    )
    .expect("bound");
    crate::object::set(
        &mut bound_object,
        interp.gc_heap_mut(),
        "back",
        Value::bound_function(bound),
    );

    let mut native_object =
        crate::test_support::alloc_old_object(interp.gc_heap_mut()).expect("object");
    let native = native_value_with_captures(
        interp.gc_heap_mut(),
        "cycle-native",
        smallvec![Value::object(native_object)],
        |_, _, _| Ok(Value::undefined()),
    )
    .expect("native");
    crate::object::set(&mut native_object, interp.gc_heap_mut(), "back", native);

    let re = crate::JsRegExp::compile(
        interp.gc_heap_mut(),
        &"z+".encode_utf16().collect::<Vec<_>>(),
        "",
    )
    .expect("regexp");
    let mut regexp_object =
        crate::test_support::alloc_old_object(interp.gc_heap_mut()).expect("object");
    crate::object::set(
        &mut regexp_object,
        interp.gc_heap_mut(),
        "regexp",
        Value::regexp(re),
    );

    let _ = bound_object;
    let _ = bound;
    let _ = native_object;
    let _ = regexp_object;
    let _ = re;
    interp.force_gc().expect("force GC");

    assert!(live_bytes(&mut interp, BOUND_FUNCTION_BODY_TYPE_TAG) <= bound_baseline);
    assert!(live_bytes(&mut interp, NATIVE_FUNCTION_BODY_TYPE_TAG) <= native_baseline);
    assert!(live_bytes(&mut interp, REGEXP_BODY_TYPE_TAG) <= regexp_baseline);
    assert!(
        live_bytes(&mut interp, OBJECT_BODY_TYPE_TAG) <= object_baseline,
        "unrooted object cycles involving callable/regexp bodies must be reclaimed"
    );
}
