//! Integration tests for ES2024 Generators (§27.3–27.5).
//!
//! Spec references:
//! - Generator Function Definitions: <https://tc39.es/ecma262/#sec-generator-function-definitions>
//! - Generator Objects: <https://tc39.es/ecma262/#sec-generator-objects>
//! - Yield: <https://tc39.es/ecma262/#sec-yield>

use otter_vm::source::compile_test262_basic_script;
use otter_vm::value::RegisterValue;
use otter_vm::{Interpreter, RuntimeState};

fn run(source: &str, url: &str) -> RegisterValue {
    let module = compile_test262_basic_script(source, url).expect("should compile");
    let mut runtime = RuntimeState::new();
    let global = runtime.intrinsics().global_object();
    let registers = [RegisterValue::from_object_handle(global.0)];
    Interpreter::new()
        .execute_with_runtime(&module, otter_vm::module::FunctionIndex(0), &registers, &mut runtime)
        .expect("should execute")
        .return_value()
}

#[test]
fn basic_generator_yield() {
    let r = run(concat!(
        "function* g() { yield 1; yield 2; yield 3; }\n",
        "var it = g();\n",
        "assert.sameValue(it.next().value, 1, 'first yield');\n",
        "assert.sameValue(it.next().value, 2, 'second yield');\n",
        "assert.sameValue(it.next().value, 3, 'third yield');\n",
        "assert.sameValue(it.next().done, true, 'exhausted');\n",
    ), "gen-basic.js");
    assert_eq!(r, RegisterValue::from_i32(0));
}

#[test]
fn generator_for_of() {
    let r = run(concat!(
        "function* nums() { yield 10; yield 20; yield 30; }\n",
        "var result = [];\n",
        "for (var v of nums()) { result.push(v); }\n",
        "assert.sameValue(result.length, 3, 'for-of length');\n",
        "assert.sameValue(result[0], 10, 'for-of first');\n",
        "assert.sameValue(result[2], 30, 'for-of last');\n",
    ), "gen-for-of.js");
    assert_eq!(r, RegisterValue::from_i32(0));
}

#[test]
fn generator_with_arguments() {
    let r = run(concat!(
        "function* range(start, end) {\n",
        "  var i = start;\n",
        "  while (i < end) { yield i; i = i + 1; }\n",
        "}\n",
        "var result = [];\n",
        "for (var v of range(3, 6)) { result.push(v); }\n",
        "assert.sameValue(result.length, 3, 'range length');\n",
        "assert.sameValue(result[0], 3, 'range start');\n",
        "assert.sameValue(result[2], 5, 'range end');\n",
    ), "gen-args.js");
    assert_eq!(r, RegisterValue::from_i32(0));
}

#[test]
fn generator_send_value_via_next() {
    let r = run(concat!(
        "function* adder() {\n",
        "  var sum = 0;\n",
        "  while (true) { var n = yield sum; sum = sum + n; }\n",
        "}\n",
        "var it = adder();\n",
        "assert.sameValue(it.next().value, 0, 'initial sum');\n",
        "assert.sameValue(it.next(10).value, 10, 'after +10');\n",
        "assert.sameValue(it.next(20).value, 30, 'after +20');\n",
    ), "gen-send.js");
    assert_eq!(r, RegisterValue::from_i32(0));
}

#[test]
fn generator_return_method() {
    let r = run(concat!(
        "function* g() { yield 1; yield 2; }\n",
        "var it = g();\n",
        "assert.sameValue(it.next().value, 1, 'first');\n",
        "var ret = it.return(42);\n",
        "assert.sameValue(ret.value, 42, 'return value');\n",
        "assert.sameValue(ret.done, true, 'return done');\n",
        "assert.sameValue(it.next().done, true, 'after return');\n",
    ), "gen-return.js");
    assert_eq!(r, RegisterValue::from_i32(0));
}

#[test]
fn generator_expression() {
    let r = run(concat!(
        "var g = function*() { yield 'a'; yield 'b'; };\n",
        "var it = g();\n",
        "assert.sameValue(it.next().value, 'a', 'gen expr first');\n",
        "assert.sameValue(it.next().value, 'b', 'gen expr second');\n",
        "assert.sameValue(it.next().done, true, 'gen expr done');\n",
    ), "gen-expr.js");
    assert_eq!(r, RegisterValue::from_i32(0));
}
