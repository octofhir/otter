//! Integration tests for class fields, static fields, and static blocks (§15.7).
//!
//! Spec references:
//! - §15.7.14 ClassDefinitionEvaluation: <https://tc39.es/ecma262/#sec-runtime-semantics-classdefinitionevaluation>
//! - §15.7.10 Instance Fields:           <https://tc39.es/ecma262/#sec-static-semantics-classelementevaluation>
//! - §15.7.11 Static Fields:             <https://tc39.es/ecma262/#sec-static-semantics-classelementevaluation>
//! - §15.7.12 Static Blocks:             <https://tc39.es/ecma262/#sec-static-blocks>
//! - DefineField:                         <https://tc39.es/ecma262/#sec-definefield>

use otter_vm::source::compile_test262_basic_script;
use otter_vm::value::RegisterValue;
use otter_vm::{Interpreter, RuntimeState};

fn execute_test262_basic(source: &str, source_url: &str) -> RegisterValue {
    let module = compile_test262_basic_script(source, source_url)
        .expect("test262 basic script should compile on the new VM path");

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
        .expect("test262 basic script should execute on the new VM path")
        .return_value()
}

// ═══════════════════════════════════════════════════════════════════════════
//  Public Instance Fields — §15.7.10
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn instance_field_with_initializer() {
    let result = execute_test262_basic(
        concat!(
            "class Foo {\n",
            "  x = 1;\n",
            "  y = 'hello';\n",
            "}\n",
            "var f = new Foo();\n",
            "assert.sameValue(f.x, 1, 'x should be 1');\n",
            "assert.sameValue(f.y, 'hello', 'y should be hello');\n",
        ),
        "instance-field-init.js",
    );
    assert_eq!(result, RegisterValue::from_i32(0));
}

#[test]
fn instance_field_without_initializer() {
    let result = execute_test262_basic(
        concat!(
            "class Foo {\n",
            "  x;\n",
            "}\n",
            "var f = new Foo();\n",
            "assert.sameValue(f.x, undefined, 'x should be undefined');\n",
            "assert.sameValue(f.hasOwnProperty('x'), true, 'x should be own');\n",
        ),
        "instance-field-no-init.js",
    );
    assert_eq!(result, RegisterValue::from_i32(0));
}

#[test]
fn instance_field_evaluation_per_instance() {
    let result = execute_test262_basic(
        concat!(
            "var counter = 0;\n",
            "class Foo {\n",
            "  id = ++counter;\n",
            "}\n",
            "var a = new Foo();\n",
            "var b = new Foo();\n",
            "assert.sameValue(a.id, 1, 'first instance gets 1');\n",
            "assert.sameValue(b.id, 2, 'second instance gets 2');\n",
        ),
        "instance-field-per-instance.js",
    );
    assert_eq!(result, RegisterValue::from_i32(0));
}

#[test]
fn instance_field_source_order() {
    let result = execute_test262_basic(
        concat!(
            "var log = [];\n",
            "class Foo {\n",
            "  a = (log.push('a'), 1);\n",
            "  b = (log.push('b'), 2);\n",
            "  c = (log.push('c'), 3);\n",
            "}\n",
            "var f = new Foo();\n",
            "assert.sameValue(log.length, 3, 'all fields initialized');\n",
            "assert.sameValue(log[0], 'a', 'a first');\n",
            "assert.sameValue(log[1], 'b', 'b second');\n",
            "assert.sameValue(log[2], 'c', 'c third');\n",
        ),
        "instance-field-order.js",
    );
    assert_eq!(result, RegisterValue::from_i32(0));
}

#[test]
fn instance_field_with_explicit_constructor() {
    let result = execute_test262_basic(
        concat!(
            "class Foo {\n",
            "  x = 10;\n",
            "  constructor(y) {\n",
            "    this.y = y;\n",
            "  }\n",
            "}\n",
            "var f = new Foo(20);\n",
            "assert.sameValue(f.x, 10, 'field initialized');\n",
            "assert.sameValue(f.y, 20, 'constructor ran');\n",
        ),
        "instance-field-explicit-ctor.js",
    );
    assert_eq!(result, RegisterValue::from_i32(0));
}

#[test]
fn instance_field_this_access() {
    let result = execute_test262_basic(
        concat!(
            "class Foo {\n",
            "  x = 1;\n",
            "  y = this.x + 1;\n",
            "}\n",
            "var f = new Foo();\n",
            "assert.sameValue(f.y, 2, 'field can access this');\n",
        ),
        "instance-field-this.js",
    );
    assert_eq!(result, RegisterValue::from_i32(0));
}

// ═══════════════════════════════════════════════════════════════════════════
//  Instance Fields in Derived Classes — §15.7.14
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn derived_class_instance_fields() {
    let result = execute_test262_basic(
        concat!(
            "class Base {\n",
            "  x = 1;\n",
            "}\n",
            "class Derived extends Base {\n",
            "  y = 2;\n",
            "}\n",
            "var d = new Derived();\n",
            "assert.sameValue(d.x, 1, 'base field on derived instance');\n",
            "assert.sameValue(d.y, 2, 'derived field on derived instance');\n",
        ),
        "derived-instance-fields.js",
    );
    assert_eq!(result, RegisterValue::from_i32(0));
}

#[test]
fn derived_class_fields_with_explicit_super() {
    let result = execute_test262_basic(
        concat!(
            "class Base {\n",
            "  x = 1;\n",
            "}\n",
            "class Derived extends Base {\n",
            "  y = 2;\n",
            "  constructor() {\n",
            "    super();\n",
            "    this.z = 3;\n",
            "  }\n",
            "}\n",
            "var d = new Derived();\n",
            "assert.sameValue(d.x, 1, 'base field');\n",
            "assert.sameValue(d.y, 2, 'derived field init runs after super()');\n",
            "assert.sameValue(d.z, 3, 'constructor body after super()');\n",
        ),
        "derived-fields-explicit-super.js",
    );
    assert_eq!(result, RegisterValue::from_i32(0));
}

#[test]
fn derived_class_default_constructor_fields() {
    let result = execute_test262_basic(
        concat!(
            "class Base {\n",
            "  constructor(v) { this.base = v; }\n",
            "}\n",
            "class Derived extends Base {\n",
            "  extra = 99;\n",
            "}\n",
            "var d = new Derived(42);\n",
            "assert.sameValue(d.base, 42, 'base constructor ran');\n",
            "assert.sameValue(d.extra, 99, 'derived field set');\n",
        ),
        "derived-default-ctor-fields.js",
    );
    assert_eq!(result, RegisterValue::from_i32(0));
}

// ═══════════════════════════════════════════════════════════════════════════
//  Static Fields — §15.7.11
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn static_field_basic() {
    let result = execute_test262_basic(
        concat!(
            "class Foo {\n",
            "  static x = 42;\n",
            "  static y = 'hello';\n",
            "}\n",
            "assert.sameValue(Foo.x, 42, 'static x');\n",
            "assert.sameValue(Foo.y, 'hello', 'static y');\n",
        ),
        "static-field-basic.js",
    );
    assert_eq!(result, RegisterValue::from_i32(0));
}

#[test]
fn static_field_without_initializer() {
    let result = execute_test262_basic(
        concat!(
            "class Foo {\n",
            "  static x;\n",
            "}\n",
            "assert.sameValue(Foo.x, undefined, 'static x is undefined');\n",
        ),
        "static-field-no-init.js",
    );
    assert_eq!(result, RegisterValue::from_i32(0));
}

#[test]
fn static_field_evaluation_order() {
    let result = execute_test262_basic(
        concat!(
            "var log = [];\n",
            "class Foo {\n",
            "  static a = (log.push('a'), 1);\n",
            "  static b = (log.push('b'), 2);\n",
            "}\n",
            "assert.sameValue(log.length, 2, 'both evaluated');\n",
            "assert.sameValue(log[0], 'a', 'a first');\n",
            "assert.sameValue(log[1], 'b', 'b second');\n",
        ),
        "static-field-order.js",
    );
    assert_eq!(result, RegisterValue::from_i32(0));
}

#[test]
fn static_field_this_is_constructor() {
    let result = execute_test262_basic(
        concat!(
            "class Foo {\n",
            "  static self = this;\n",
            "}\n",
            "assert.sameValue(Foo.self, Foo, 'this in static field is the constructor');\n",
        ),
        "static-field-this.js",
    );
    assert_eq!(result, RegisterValue::from_i32(0));
}

// ═══════════════════════════════════════════════════════════════════════════
//  Static Blocks — §15.7.12
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn static_block_basic() {
    let result = execute_test262_basic(
        concat!(
            "var log = [];\n",
            "class Foo {\n",
            "  static {\n",
            "    log.push('static block');\n",
            "  }\n",
            "}\n",
            "assert.sameValue(log.length, 1, 'static block executed');\n",
            "assert.sameValue(log[0], 'static block', 'correct value');\n",
        ),
        "static-block-basic.js",
    );
    assert_eq!(result, RegisterValue::from_i32(0));
}

#[test]
fn static_block_this_is_constructor() {
    let result = execute_test262_basic(
        concat!(
            "var captured;\n",
            "class Foo {\n",
            "  static {\n",
            "    captured = this;\n",
            "  }\n",
            "}\n",
            "assert.sameValue(captured, Foo, 'this in static block is the class');\n",
        ),
        "static-block-this.js",
    );
    assert_eq!(result, RegisterValue::from_i32(0));
}

#[test]
fn static_block_multiple() {
    let result = execute_test262_basic(
        concat!(
            "var log = [];\n",
            "class Foo {\n",
            "  static { log.push(1); }\n",
            "  static x = (log.push(2), 42);\n",
            "  static { log.push(3); }\n",
            "}\n",
            "assert.sameValue(log.length, 3, 'all executed');\n",
            "assert.sameValue(log[0], 1, 'block 1');\n",
            "assert.sameValue(log[1], 2, 'field');\n",
            "assert.sameValue(log[2], 3, 'block 2');\n",
        ),
        "static-block-multiple.js",
    );
    assert_eq!(result, RegisterValue::from_i32(0));
}

// ═══════════════════════════════════════════════════════════════════════════
//  Mixed Instance/Static Fields — ordering
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn mixed_instance_and_static_fields() {
    let result = execute_test262_basic(
        concat!(
            "class Foo {\n",
            "  x = 1;\n",
            "  static s = 10;\n",
            "  y = 2;\n",
            "}\n",
            "assert.sameValue(Foo.s, 10, 'static field');\n",
            "var f = new Foo();\n",
            "assert.sameValue(f.x, 1, 'instance x');\n",
            "assert.sameValue(f.y, 2, 'instance y');\n",
            "assert.sameValue(f.s, undefined, 'static not on instance');\n",
        ),
        "mixed-fields.js",
    );
    assert_eq!(result, RegisterValue::from_i32(0));
}

// ═══════════════════════════════════════════════════════════════════════════
//  Fields with Methods
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn fields_alongside_methods() {
    let result = execute_test262_basic(
        concat!(
            "class Foo {\n",
            "  x = 42;\n",
            "  getX() { return this.x; }\n",
            "  static s = 100;\n",
            "  static getS() { return this.s; }\n",
            "}\n",
            "var f = new Foo();\n",
            "assert.sameValue(f.getX(), 42, 'instance method reads field');\n",
            "assert.sameValue(Foo.getS(), 100, 'static method reads static field');\n",
        ),
        "fields-with-methods.js",
    );
    assert_eq!(result, RegisterValue::from_i32(0));
}

// ═══════════════════════════════════════════════════════════════════════════
//  Class Expression with Fields
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn class_expression_with_fields() {
    let result = execute_test262_basic(
        concat!(
            "var Foo = class {\n",
            "  x = 1;\n",
            "  static s = 2;\n",
            "};\n",
            "var f = new Foo();\n",
            "assert.sameValue(f.x, 1, 'instance field');\n",
            "assert.sameValue(Foo.s, 2, 'static field');\n",
        ),
        "class-expr-fields.js",
    );
    assert_eq!(result, RegisterValue::from_i32(0));
}

// ═══════════════════════════════════════════════════════════════════════════
//  DefineField semantics (does NOT trigger setters)
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn field_does_not_trigger_setter() {
    let result = execute_test262_basic(
        concat!(
            "var setterCalled = false;\n",
            "class Base {\n",
            "  set x(v) { setterCalled = true; }\n",
            "}\n",
            "class Derived extends Base {\n",
            "  x = 42;\n",
            "}\n",
            "var d = new Derived();\n",
            "assert.sameValue(setterCalled, false, 'setter not called by field define');\n",
            "assert.sameValue(d.x, 42, 'field value is set directly');\n",
        ),
        "field-no-setter.js",
    );
    assert_eq!(result, RegisterValue::from_i32(0));
}
