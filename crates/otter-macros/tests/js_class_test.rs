//! Integration tests for the #[js_class] macro

#![allow(dead_code)]

use otter_macros::{js_class, js_constructor, js_getter, js_method};
use serde::{Deserialize, Serialize};

// Test struct with custom name and field attributes
#[js_class(name = "Counter")]
#[derive(Serialize, Deserialize)]
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
fn test_generated_getters() {
    let counter = Counter::new(10);
    let value = counter.js_get_value();
    assert_eq!(value, serde_json::json!(10));

    let multiplier = counter.js_get_multiplier();
    assert_eq!(multiplier, serde_json::json!(1));
}

#[test]
fn test_generated_setters() {
    let mut counter = Counter::new(10);
    counter.js_set_multiplier(serde_json::json!(5));
    assert_eq!(counter.multiplier, 5);
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

// Test struct with default name (struct name)
#[js_class]
#[derive(Serialize, Deserialize)]
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
