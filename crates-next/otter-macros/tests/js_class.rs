//! Integration coverage for the `#[js_class]` macro.

use otter_macros::js_class;
use otter_vm::object;
use otter_vm::{ClassBuilder, Interpreter, Value};

#[js_class(name = "MacroClass", spec = MACRO_CLASS_SPEC)]
mod macro_class {
    use otter_vm::{NativeCtx, NativeError, NumberValue, Value};

    #[js_constructor(length = 1)]
    pub fn construct(_: &mut NativeCtx<'_>, _: &[Value]) -> Result<Value, NativeError> {
        Ok(Value::Undefined)
    }

    #[js_static_method(name = "from", length = 1)]
    pub fn from(_: &mut NativeCtx<'_>, _: &[Value]) -> Result<Value, NativeError> {
        Ok(Value::Number(NumberValue::from_i32(1)))
    }

    #[js_method(name = "valueOf", length = 0)]
    pub fn value_of(_: &mut NativeCtx<'_>, _: &[Value]) -> Result<Value, NativeError> {
        Ok(Value::Number(NumberValue::from_i32(2)))
    }

    #[js_getter(name = "answer")]
    pub fn get_answer(_: &mut NativeCtx<'_>, _: &[Value]) -> Result<Value, NativeError> {
        Ok(Value::Number(NumberValue::from_i32(42)))
    }

    #[js_setter(name = "answer")]
    pub fn set_answer(_: &mut NativeCtx<'_>, _: &[Value]) -> Result<Value, NativeError> {
        Ok(Value::Undefined)
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
    let Value::ClassConstructor(class) = class else {
        panic!("macro class should build a class constructor value");
    };

    let Value::NativeFunction(ctor) = &class.ctor else {
        panic!("constructor should be native");
    };
    assert!(ctor.is_static_call(interp.gc_heap()));
    assert_eq!(ctor.length(interp.gc_heap()), 1);

    let Value::NativeFunction(from) =
        object::get(class.statics, interp.gc_heap(), "from").expect("from")
    else {
        panic!("static method should be native");
    };
    assert!(from.is_static_call(interp.gc_heap()));
    assert_eq!(from.length(interp.gc_heap()), 1);

    let Value::NativeFunction(value_of) =
        object::get(class.prototype, interp.gc_heap(), "valueOf").expect("valueOf")
    else {
        panic!("prototype method should be native");
    };
    assert!(value_of.is_static_call(interp.gc_heap()));
    assert_eq!(value_of.length(interp.gc_heap()), 0);

    assert!(matches!(
        object::get(class.prototype, interp.gc_heap(), "constructor"),
        Some(Value::ClassConstructor(_))
    ));

    let answer = object::get_own_descriptor(class.prototype, interp.gc_heap(), "answer")
        .expect("answer accessor");
    assert!(answer.is_accessor());
    assert!(!answer.enumerable());
    assert!(answer.configurable());
}
