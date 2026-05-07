//! Integration coverage for the `#[js_namespace]` macro.

use otter_macros::js_namespace;
use otter_vm::object;
use otter_vm::{Interpreter, NamespaceBuilder, Value};

#[js_namespace(name = "MacroNs", spec = MACRO_NS_SPEC)]
mod macro_ns {
    use otter_vm::{NativeCtx, NativeError, NumberValue, Value};

    #[js_fn(name = "one", length = 0)]
    pub fn one(_: &mut NativeCtx<'_>, _: &[Value]) -> Result<Value, NativeError> {
        Ok(Value::Number(NumberValue::from_i32(1)))
    }

    #[js_fn(name = "addOne", length = 1)]
    pub fn add_one(_: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
        let value = args
            .first()
            .and_then(Value::as_number)
            .map(NumberValue::as_f64)
            .unwrap_or(0.0);
        Ok(Value::Number(NumberValue::from_f64(value + 1.0)))
    }
}

#[test]
fn js_namespace_generates_static_spec_for_builder_backend() {
    assert_eq!(MACRO_NS_SPEC.name, "MacroNs");
    assert_eq!(MACRO_NS_SPEC.methods.len(), 2);
    assert!(MACRO_NS_SPEC.accessors.is_empty());
    assert!(MACRO_NS_SPEC.constants.is_empty());

    let mut interp = Interpreter::new();
    let ns = NamespaceBuilder::from_spec(interp.gc_heap_mut(), &MACRO_NS_SPEC)
        .expect("builder")
        .build()
        .expect("namespace");

    let Value::NativeFunction(one) = object::get(ns, interp.gc_heap(), "one").expect("one") else {
        panic!("one should be installed as a native function");
    };
    assert!(one.is_static_call(interp.gc_heap()));
    assert_eq!(one.length(interp.gc_heap()), 0);

    let Value::NativeFunction(add_one) =
        object::get(ns, interp.gc_heap(), "addOne").expect("addOne")
    else {
        panic!("addOne should be installed as a native function");
    };
    assert!(add_one.is_static_call(interp.gc_heap()));
    assert_eq!(add_one.length(interp.gc_heap()), 1);

    let desc = object::get_own_descriptor(ns, interp.gc_heap(), "one").expect("descriptor");
    assert!(desc.writable());
    assert!(!desc.enumerable());
    assert!(desc.configurable());
}
