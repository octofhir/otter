//! Integration tests for the #[js_class] macro

#![allow(dead_code)]

use otter_macros::{js_class, js_constructor, js_getter, js_method};

// Test struct with custom name and field attributes
#[js_class(name = "Counter")]
pub struct Counter {
    #[js_readonly]
    pub value: i32,

    #[js_skip]
    internal: String,

    // Regular field (writable)
    pub multiplier: i32,
}

// Test impl block with method attributes
#[js_class]
impl Counter {
    #[js_constructor]
    pub fn new(initial: i32) -> Self {
        Self {
            value: initial,
            internal: String::new(),
            multiplier: 1,
        }
    }

    #[js_method]
    pub fn increment(&mut self) {
        self.value += 1;
    }

    #[js_method]
    pub fn add(&mut self, n: i32) {
        self.value += n;
    }

    #[js_getter]
    pub fn doubled(&self) -> i32 {
        self.value * 2
    }
}

#[test]
fn test_js_class_name() {
    assert_eq!(Counter::JS_CLASS_NAME, "Counter");
}

#[test]
fn test_js_properties() {
    let props = Counter::js_properties();
    assert!(props.contains(&"multiplier"));
    // value is readonly, so not in js_properties
    assert!(!props.contains(&"value"));
    // internal is skipped
    assert!(!props.contains(&"internal"));
}

#[test]
fn test_js_readonly_properties() {
    let readonly = Counter::js_readonly_properties();
    assert!(readonly.contains(&"value"));
    assert!(!readonly.contains(&"multiplier"));
}

#[test]
fn test_js_constructors() {
    let constructors = Counter::js_constructors();
    assert!(constructors.contains(&"new"));
}

#[test]
fn test_js_methods() {
    let methods = Counter::js_methods();
    assert!(methods.contains(&"increment"));
    assert!(methods.contains(&"add"));
}

#[test]
fn test_js_getters() {
    let getters = Counter::js_getters();
    assert!(getters.contains(&"doubled"));
}

#[test]
fn test_original_methods_work() {
    let mut counter = Counter::new(0);
    counter.increment();
    assert_eq!(counter.value, 1);
    counter.add(5);
    assert_eq!(counter.value, 6);
    assert_eq!(counter.doubled(), 12);
}

// --- Test NativeFn-compatible class with _decl() generation ---

use otter_macros::js_static;
use otter_vm_core::context::NativeContext;
use otter_vm_core::error::VmError;
use otter_vm_core::value::Value;

#[js_class(name = "Greeter")]
pub struct Greeter;

#[js_class]
impl Greeter {
    #[js_constructor(name = "Greeter", length = 0)]
    pub fn construct(
        _this: &Value,
        _args: &[Value],
        _ncx: &mut NativeContext,
    ) -> Result<Value, VmError> {
        Ok(Value::undefined())
    }

    #[js_method(name = "greet", length = 1)]
    pub fn greet(
        _this: &Value,
        _args: &[Value],
        _ncx: &mut NativeContext,
    ) -> Result<Value, VmError> {
        Ok(Value::undefined())
    }

    #[js_static(name = "create", length = 0)]
    pub fn create(
        _this: &Value,
        _args: &[Value],
        _ncx: &mut NativeContext,
    ) -> Result<Value, VmError> {
        Ok(Value::undefined())
    }

    #[js_getter(name = "name")]
    pub fn get_name(
        _this: &Value,
        _args: &[Value],
        _ncx: &mut NativeContext,
    ) -> Result<Value, VmError> {
        Ok(Value::undefined())
    }
}

#[test]
fn test_decl_generation_metadata() {
    // Metadata still works
    assert_eq!(Greeter::JS_CLASS_NAME, "Greeter");
    assert!(Greeter::js_constructors().contains(&"construct"));
    assert!(Greeter::js_methods().contains(&"greet"));
    assert!(Greeter::js_static_methods().contains(&"create"));
    assert!(Greeter::js_getters().contains(&"get_name"));
}

#[test]
fn test_decl_functions_exist() {
    // _decl() functions return (name, NativeFn, length)
    let (name, _fn, length) = Greeter::construct_decl();
    assert_eq!(name, "Greeter");
    assert_eq!(length, 0);

    let (name, _fn, length) = Greeter::greet_decl();
    assert_eq!(name, "greet");
    assert_eq!(length, 1);

    let (name, _fn, length) = Greeter::create_decl();
    assert_eq!(name, "create");
    assert_eq!(length, 0);

    let (name, _fn, length) = Greeter::get_name_decl();
    assert_eq!(name, "name");
    assert_eq!(length, 0);
}

#[test]
fn test_decl_caching() {
    // Calling _decl() twice returns the same Arc (cached via OnceLock)
    let (_, fn1, _) = Greeter::greet_decl();
    let (_, fn2, _) = Greeter::greet_decl();
    assert!(std::sync::Arc::ptr_eq(&fn1, &fn2));
}

// Test struct with default name (struct name)
#[js_class]
pub struct Point {
    pub x: f64,
    pub y: f64,
}

#[test]
fn test_default_class_name() {
    assert_eq!(Point::JS_CLASS_NAME, "Point");
}

#[test]
fn test_all_fields_writable() {
    let props = Point::js_properties();
    assert!(props.contains(&"x"));
    assert!(props.contains(&"y"));

    let readonly = Point::js_readonly_properties();
    assert!(readonly.is_empty());
}
