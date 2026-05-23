//! Integration coverage for the `#[js_class]` macro.

use otter_macros::js_class;
use otter_vm::object;
use otter_vm::{ClassBuilder, Interpreter};

#[js_class(name = "MacroClass", spec = MACRO_CLASS_SPEC)]
mod macro_class {
    use otter_vm::{NativeCtx, NativeError, Value};

    #[js_constructor(length = 1)]
    pub fn construct(_: &mut NativeCtx<'_>, _: &[Value]) -> Result<Value, NativeError> {
        Ok(Value::undefined())
    }

    #[js_static_method(name = "from", length = 1)]
    pub fn from(_: &mut NativeCtx<'_>, _: &[Value]) -> Result<Value, NativeError> {
        Ok(Value::number_i32(1))
    }

    #[js_method(name = "valueOf", length = 0)]
    pub fn value_of(_: &mut NativeCtx<'_>, _: &[Value]) -> Result<Value, NativeError> {
        Ok(Value::number_i32(2))
    }

    #[js_getter(name = "answer")]
    pub fn get_answer(_: &mut NativeCtx<'_>, _: &[Value]) -> Result<Value, NativeError> {
        Ok(Value::number_i32(42))
    }

    #[js_setter(name = "answer")]
    pub fn set_answer(_: &mut NativeCtx<'_>, _: &[Value]) -> Result<Value, NativeError> {
        Ok(Value::undefined())
    }
}

#[test]
fn js_class_generates_spec_shaped_class_constructor() {
    assert_eq!(MACRO_CLASS_SPEC.constructor.name, "MacroClass");
    assert_eq!(MACRO_CLASS_SPEC.constructor.length, 1);
    assert_eq!(MACRO_CLASS_SPEC.constructor.static_methods.len(), 1);
    assert_eq!(MACRO_CLASS_SPEC.constructor.prototype_methods.len(), 1);
    assert_eq!(MACRO_CLASS_SPEC.prototype_accessors.len(), 1);

    let mut interp = Interpreter::new();
    let class = ClassBuilder::from_spec(interp.gc_heap_mut(), &MACRO_CLASS_SPEC)
        .build()
        .expect("class");
    let class = class
        .as_class_constructor()
        .expect("macro class should build a class constructor value");

    let class_ctor = class.ctor(interp.gc_heap());
    let ctor = class_ctor
        .as_native_function()
        .expect("constructor should be native");
    assert!(ctor.is_static_call(interp.gc_heap()));
    assert_eq!(ctor.length(interp.gc_heap()), 1);

    let class_statics = class.statics(interp.gc_heap());
    let from = object::get(class_statics, interp.gc_heap(), "from")
        .and_then(|v| v.as_native_function())
        .expect("static method should be native");
    assert!(from.is_static_call(interp.gc_heap()));
    assert_eq!(from.length(interp.gc_heap()), 1);

    let class_prototype = class.prototype(interp.gc_heap());
    let value_of = object::get(class_prototype, interp.gc_heap(), "valueOf")
        .and_then(|v| v.as_native_function())
        .expect("prototype method should be native");
    assert!(value_of.is_static_call(interp.gc_heap()));
    assert_eq!(value_of.length(interp.gc_heap()), 0);

    assert!(
        object::get(class_prototype, interp.gc_heap(), "constructor")
            .is_some_and(|v| v.is_class_constructor())
    );

    let answer = object::get_own_descriptor(class_prototype, interp.gc_heap(), "answer")
        .expect("answer accessor");
    assert!(answer.is_accessor());
    assert!(!answer.enumerable());
    assert!(answer.configurable());
}
