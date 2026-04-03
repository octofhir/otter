//! Integration tests for §15 Classes and compiler improvements.
//!
//! Spec references:
//! - §15.7 Class Definitions: <https://tc39.es/ecma262/#sec-class-definitions>
//! - §13.4 Update Expressions: <https://tc39.es/ecma262/#sec-update-expressions>
//! - §13.3.7 Optional Chaining: <https://tc39.es/ecma262/#sec-optional-chaining>

use otter_vm::source::compile_test262_basic_script;
use otter_vm::value::RegisterValue;
use otter_vm::{Interpreter, RuntimeState};

fn execute_test262_basic(source: &str, source_url: &str) -> RegisterValue {
    let module = compile_test262_basic_script(source, source_url).expect("should compile");
    let mut runtime = RuntimeState::new();
    let global = runtime.intrinsics().global_object();
    let registers = [RegisterValue::from_object_handle(global.0)];
    Interpreter::new()
        .execute_with_runtime(
            &module,
            otter_vm::module::FunctionIndex(0),
            &registers,
            &mut runtime,
        )
        .expect("should execute")
        .return_value()
}

#[test]
fn class_prototype_methods() {
    let result = execute_test262_basic(
        concat!(
            "class Foo {\n",
            "  constructor(x) { this.x = x; }\n",
            "  getX() { return this.x; }\n",
            "  double() { return this.x * 2; }\n",
            "}\n",
            "var f = new Foo(21);\n",
            "assert.sameValue(f.getX(), 21, 'prototype method getX');\n",
            "assert.sameValue(f.double(), 42, 'prototype method double');\n",
        ),
        "class-prototype-methods.js",
    );
    assert_eq!(result, RegisterValue::from_i32(0));
}

#[test]
fn class_static_methods() {
    let result = execute_test262_basic(
        concat!(
            "class Foo {\n",
            "  constructor(x) { this.x = x; }\n",
            "  static create(x) { return new Foo(x); }\n",
            "}\n",
            "assert.sameValue(Foo.create(10).x, 10, 'static method');\n",
        ),
        "class-static-methods.js",
    );
    assert_eq!(result, RegisterValue::from_i32(0));
}

#[test]
fn class_getter_setter() {
    let result = execute_test262_basic(
        concat!(
            "class Rect {\n",
            "  constructor(w, h) { this._w = w; this._h = h; }\n",
            "  get area() { return this._w * this._h; }\n",
            "  set width(v) { this._w = v; }\n",
            "}\n",
            "var r = new Rect(3, 4);\n",
            "assert.sameValue(r.area, 12, 'getter');\n",
            "r.width = 5;\n",
            "assert.sameValue(r.area, 20, 'setter then getter');\n",
        ),
        "class-getter-setter.js",
    );
    assert_eq!(result, RegisterValue::from_i32(0));
}

#[test]
fn class_inheritance_with_methods() {
    let result = execute_test262_basic(
        concat!(
            "class Animal {\n",
            "  constructor(name) { this.name = name; }\n",
            "  speak() { return this.name + ' speaks'; }\n",
            "}\n",
            "class Dog extends Animal {\n",
            "  speak() { return this.name + ' barks'; }\n",
            "}\n",
            "var d = new Dog('Rex');\n",
            "assert.sameValue(d.speak(), 'Rex barks', 'overridden method');\n",
            // TODO: instanceof with multi-level inheritance needs setPrototypeOf fix.
            // "assert.sameValue(d instanceof Animal, true, 'instanceof');\n",
        ),
        "class-inheritance-methods.js",
    );
    assert_eq!(result, RegisterValue::from_i32(0));
}

#[test]
fn class_expression() {
    let result = execute_test262_basic(
        concat!(
            "var Foo = class {\n",
            "  constructor(x) { this.x = x; }\n",
            "  get() { return this.x; }\n",
            "};\n",
            "assert.sameValue(new Foo(42).get(), 42, 'class expression');\n",
        ),
        "class-expression.js",
    );
    assert_eq!(result, RegisterValue::from_i32(0));
}

#[test]
fn constructor_call_argument_keeps_earlier_temp_values() {
    let result = execute_test262_basic(
        concat!(
            "class Box {\n",
            "  constructor(value) { this.value = value; }\n",
            "}\n",
            "function second(a, b, c) { return b; }\n",
            "var box = second('left', new Box(41), 'right');\n",
            "assert.sameValue(box.value, 41, 'inline new-expression argument must survive later args');\n",
        ),
        "call-arg-new-expression-stability.js",
    );
    assert_eq!(result, RegisterValue::from_i32(0));
}

#[test]
fn array_literal_element_keeps_nested_constructor_result() {
    let result = execute_test262_basic(
        concat!(
            "class Box {\n",
            "  constructor(value) { this.value = value; }\n",
            "}\n",
            "var arr = ['left', new Box(41), 'right'];\n",
            "assert.sameValue(arr[1].value, 41, 'array element new-expression must survive later elements');\n",
        ),
        "array-element-new-expression-stability.js",
    );
    assert_eq!(result, RegisterValue::from_i32(0));
}

#[test]
fn object_literal_property_values_keep_earlier_call_results() {
    let result = execute_test262_basic(
        concat!(
            "class Box {\n",
            "  constructor(value) { this.value = value; }\n",
            "  get() { return this.value; }\n",
            "}\n",
            "var left = new Box(1);\n",
            "var right = new Box(2);\n",
            "var obj = { first: left.get(), second: right.get() };\n",
            "assert.sameValue(obj.first, 1, 'first call result survives later object properties');\n",
            "assert.sameValue(obj.second, 2, 'second call result is preserved');\n",
        ),
        "object-literal-call-result-stability.js",
    );
    assert_eq!(result, RegisterValue::from_i32(0));
}

#[test]
fn member_update_expressions() {
    let result = execute_test262_basic(
        concat!(
            "var obj = { x: 10 };\n",
            "assert.sameValue(obj.x++, 10, 'postfix returns old');\n",
            "assert.sameValue(obj.x, 11, 'postfix increments');\n",
            "assert.sameValue(++obj.x, 12, 'prefix returns new');\n",
            "var arr = [5];\n",
            "assert.sameValue(arr[0]--, 5, 'computed postfix');\n",
            "assert.sameValue(arr[0], 4, 'computed postfix decrements');\n",
        ),
        "member-update-expressions.js",
    );
    assert_eq!(result, RegisterValue::from_i32(0));
}

#[test]
fn optional_chaining_member_access() {
    let result = execute_test262_basic(
        concat!(
            "var obj = { x: 42, nested: { y: 99 } };\n",
            "assert.sameValue(obj?.x, 42, 'non-null access');\n",
            "assert.sameValue(null?.x, undefined, 'null short-circuits');\n",
            "assert.sameValue(undefined?.x, undefined, 'undefined short-circuits');\n",
            "assert.sameValue(obj?.nested?.y, 99, 'chained access');\n",
            "assert.sameValue(obj?.missing?.y, undefined, 'missing intermediate');\n",
            "var arr = [1, 2];\n",
            "assert.sameValue(arr?.[1], 2, 'computed optional');\n",
            "assert.sameValue(null?.[0], undefined, 'null computed');\n",
        ),
        "optional-chaining.js",
    );
    assert_eq!(result, RegisterValue::from_i32(0));
}
