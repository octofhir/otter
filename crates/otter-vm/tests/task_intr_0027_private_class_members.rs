//! Integration tests for private class members (#field, #method, get #x/set #x)
//! and `class extends null`.
//!
//! Spec references:
//! - §6.2.12  PrivateName:      <https://tc39.es/ecma262/#sec-private-names>
//! - §7.3.31  PrivateFieldAdd:  <https://tc39.es/ecma262/#sec-privatefieldadd>
//! - §7.3.32  PrivateGet:       <https://tc39.es/ecma262/#sec-privateget>
//! - §7.3.33  PrivateSet:       <https://tc39.es/ecma262/#sec-privateset>
//! - §13.10.1 PrivateIn:        <https://tc39.es/ecma262/#sec-relational-operators-runtime-semantics-evaluation>
//! - §15.7.14 ClassDefinition:  <https://tc39.es/ecma262/#sec-runtime-semantics-classdefinitionevaluation>

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
//  Private Instance Fields — §7.3.31 PrivateFieldAdd
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn private_field_basic() {
    let result = execute_test262_basic(
        concat!(
            "class Foo {\n",
            "  #x = 42;\n",
            "  getX() { return this.#x; }\n",
            "}\n",
            "var f = new Foo();\n",
            "assert.sameValue(f.getX(), 42, '#x should be 42');\n",
        ),
        "private-field-basic.js",
    );
    assert_eq!(result, RegisterValue::from_i32(0));
}

#[test]
fn private_field_set() {
    let result = execute_test262_basic(
        concat!(
            "class Foo {\n",
            "  #x = 0;\n",
            "  setX(v) { this.#x = v; }\n",
            "  getX() { return this.#x; }\n",
            "}\n",
            "var f = new Foo();\n",
            "f.setX(99);\n",
            "assert.sameValue(f.getX(), 99, '#x should be 99 after set');\n",
        ),
        "private-field-set.js",
    );
    assert_eq!(result, RegisterValue::from_i32(0));
}

#[test]
fn private_field_per_instance() {
    let result = execute_test262_basic(
        concat!(
            "class Counter {\n",
            "  #count = 0;\n",
            "  inc() { this.#count++; }\n",
            "  get() { return this.#count; }\n",
            "}\n",
            "var a = new Counter();\n",
            "var b = new Counter();\n",
            "a.inc(); a.inc();\n",
            "b.inc();\n",
            "assert.sameValue(a.get(), 2, 'a should be 2');\n",
            "assert.sameValue(b.get(), 1, 'b should be 1');\n",
        ),
        "private-field-per-instance.js",
    );
    assert_eq!(result, RegisterValue::from_i32(0));
}

#[test]
fn private_field_no_initializer() {
    let result = execute_test262_basic(
        concat!(
            "class Foo {\n",
            "  #x;\n",
            "  getX() { return this.#x; }\n",
            "}\n",
            "var f = new Foo();\n",
            "assert.sameValue(f.getX(), undefined, '#x should be undefined');\n",
        ),
        "private-field-no-init.js",
    );
    assert_eq!(result, RegisterValue::from_i32(0));
}

#[test]
fn private_field_multiple() {
    let result = execute_test262_basic(
        concat!(
            "class Point {\n",
            "  #x;\n",
            "  #y;\n",
            "  constructor(x, y) { this.#x = x; this.#y = y; }\n",
            "  sum() { return this.#x + this.#y; }\n",
            "}\n",
            "var p = new Point(3, 4);\n",
            "assert.sameValue(p.sum(), 7, 'sum should be 7');\n",
        ),
        "private-field-multiple.js",
    );
    assert_eq!(result, RegisterValue::from_i32(0));
}

// ═══════════════════════════════════════════════════════════════════════════
//  Private Methods — §15.7.14 PushPrivateMethod
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn private_method_basic() {
    let result = execute_test262_basic(
        concat!(
            "class Foo {\n",
            "  #bar() { return 'secret'; }\n",
            "  callBar() { return this.#bar(); }\n",
            "}\n",
            "var f = new Foo();\n",
            "assert.sameValue(f.callBar(), 'secret', '#bar should return secret');\n",
        ),
        "private-method-basic.js",
    );
    assert_eq!(result, RegisterValue::from_i32(0));
}

#[test]
fn private_method_with_args() {
    let result = execute_test262_basic(
        concat!(
            "class Calc {\n",
            "  #add(a, b) { return a + b; }\n",
            "  compute(x, y) { return this.#add(x, y); }\n",
            "}\n",
            "var c = new Calc();\n",
            "assert.sameValue(c.compute(10, 20), 30, '#add(10,20) should be 30');\n",
        ),
        "private-method-args.js",
    );
    assert_eq!(result, RegisterValue::from_i32(0));
}

// ═══════════════════════════════════════════════════════════════════════════
//  Private Accessors — §15.7.14 PushPrivateGetter/PushPrivateSetter
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn private_getter() {
    let result = execute_test262_basic(
        concat!(
            "class Foo {\n",
            "  #x = 10;\n",
            "  get #val() { return this.#x * 2; }\n",
            "  read() { return this.#val; }\n",
            "}\n",
            "var f = new Foo();\n",
            "assert.sameValue(f.read(), 20, 'get #val should return 20');\n",
        ),
        "private-getter.js",
    );
    assert_eq!(result, RegisterValue::from_i32(0));
}

#[test]
fn private_setter() {
    let result = execute_test262_basic(
        concat!(
            "class Foo {\n",
            "  #x = 0;\n",
            "  set #val(v) { this.#x = v + 1; }\n",
            "  write(v) { this.#val = v; }\n",
            "  read() { return this.#x; }\n",
            "}\n",
            "var f = new Foo();\n",
            "f.write(5);\n",
            "assert.sameValue(f.read(), 6, 'set #val(5) should set #x to 6');\n",
        ),
        "private-setter.js",
    );
    assert_eq!(result, RegisterValue::from_i32(0));
}

#[test]
fn private_getter_setter_pair() {
    let result = execute_test262_basic(
        concat!(
            "class Foo {\n",
            "  #backing = 0;\n",
            "  get #prop() { return this.#backing; }\n",
            "  set #prop(v) { this.#backing = v * 2; }\n",
            "  test() {\n",
            "    this.#prop = 5;\n",
            "    return this.#prop;\n",
            "  }\n",
            "}\n",
            "var f = new Foo();\n",
            "assert.sameValue(f.test(), 10, 'getter/setter pair should work');\n",
        ),
        "private-getter-setter-pair.js",
    );
    assert_eq!(result, RegisterValue::from_i32(0));
}

// ═══════════════════════════════════════════════════════════════════════════
//  Static Private Fields/Methods — §15.7.14
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn static_private_field() {
    let result = execute_test262_basic(
        concat!(
            "class Foo {\n",
            "  static #count = 0;\n",
            "  static inc() { Foo.#count++; }\n",
            "  static get() { return Foo.#count; }\n",
            "}\n",
            "Foo.inc();\n",
            "Foo.inc();\n",
            "assert.sameValue(Foo.get(), 2, 'static #count should be 2');\n",
        ),
        "static-private-field.js",
    );
    assert_eq!(result, RegisterValue::from_i32(0));
}

#[test]
fn static_private_method() {
    let result = execute_test262_basic(
        concat!(
            "class Foo {\n",
            "  static #helper() { return 42; }\n",
            "  static callHelper() { return Foo.#helper(); }\n",
            "}\n",
            "assert.sameValue(Foo.callHelper(), 42, 'static #helper should return 42');\n",
        ),
        "static-private-method.js",
    );
    assert_eq!(result, RegisterValue::from_i32(0));
}

// ═══════════════════════════════════════════════════════════════════════════
//  `#field in obj` — §13.10.1
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn private_in_check() {
    let result = execute_test262_basic(
        concat!(
            "class Foo {\n",
            "  #brand;\n",
            "  static check(obj) { return #brand in obj; }\n",
            "}\n",
            "var f = new Foo();\n",
            "assert.sameValue(Foo.check(f), true, '#brand should be in f');\n",
            "assert.sameValue(Foo.check({}), false, '#brand should not be in {}');\n",
        ),
        "private-in-check.js",
    );
    assert_eq!(result, RegisterValue::from_i32(0));
}

// ═══════════════════════════════════════════════════════════════════════════
//  class extends null — §15.7.14 step 5.b
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn class_extends_null_prototype() {
    let result = execute_test262_basic(
        concat!(
            "class NullBase extends null {\n",
            "  constructor() { /* no super() call */ }\n",
            "}\n",
            "var proto = NullBase.prototype;\n",
            "assert.sameValue(\n",
            "  Object.getPrototypeOf(proto), null,\n",
            "  'prototype.__proto__ should be null'\n",
            ");\n",
        ),
        "class-extends-null-proto.js",
    );
    assert_eq!(result, RegisterValue::from_i32(0));
}

#[test]
fn class_extends_null_constructor_parent() {
    let result = execute_test262_basic(
        concat!(
            "class NullBase extends null {\n",
            "  constructor() {}\n",
            "}\n",
            "assert.sameValue(\n",
            "  Object.getPrototypeOf(NullBase), Function.prototype,\n",
            "  'constructor.__proto__ should be Function.prototype'\n",
            ");\n",
        ),
        "class-extends-null-ctor-parent.js",
    );
    assert_eq!(result, RegisterValue::from_i32(0));
}
