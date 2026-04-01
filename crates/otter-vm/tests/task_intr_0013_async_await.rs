//! Integration tests for ES2024 Async Functions (§27.7).
//!
//! Spec references:
//! - Async Function Definitions: <https://tc39.es/ecma262/#sec-async-function-definitions>
//! - AsyncFunctionStart: <https://tc39.es/ecma262/#sec-async-functions-abstract-operations-async-function-start>
//! - Await: <https://tc39.es/ecma262/#sec-await>

use otter_vm::source::compile_test262_basic_script;
use otter_vm::value::RegisterValue;
use otter_vm::{Interpreter, RuntimeState};

fn run(source: &str, url: &str) -> RegisterValue {
    let module = compile_test262_basic_script(source, url).expect("should compile");
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
fn async_function_returns_promise() {
    // An async function that returns a value should produce a Promise
    // that is fulfilled with that value.
    let r = run(
        concat!(
            "async function f() { return 42; }\n",
            "var p = f();\n",
            "assert.sameValue(typeof p, 'object', 'promise is an object');\n",
            // The promise should have a `.then` method (duck-type check).
            "assert.sameValue(typeof p.then, 'function', 'promise has .then');\n",
        ),
        "async-basic.js",
    );
    assert_eq!(r, RegisterValue::from_i32(0));
}

#[test]
fn async_function_await_non_promise() {
    // `await <non-promise>` should resolve immediately with the value.
    let r = run(
        concat!(
            "async function f() { var x = await 10; return x; }\n",
            "var p = f();\n",
            "assert.sameValue(typeof p, 'object', 'returns a promise');\n",
            "assert.sameValue(typeof p.then, 'function', 'promise has .then');\n",
        ),
        "async-await-nonpromise.js",
    );
    assert_eq!(r, RegisterValue::from_i32(0));
}

#[test]
fn async_function_await_resolved_promise() {
    // `await Promise.resolve(99)` should resolve immediately.
    let r = run(
        concat!(
            "async function f() {\n",
            "  var x = await Promise.resolve(99);\n",
            "  return x;\n",
            "}\n",
            "var p = f();\n",
            "assert.sameValue(typeof p.then, 'function', 'result is thenable');\n",
        ),
        "async-await-resolved.js",
    );
    assert_eq!(r, RegisterValue::from_i32(0));
}

#[test]
fn async_function_throw_rejects_promise() {
    // An async function that throws should produce a rejected promise.
    let r = run(
        concat!(
            "async function f() { throw 'boom'; }\n",
            "var p = f();\n",
            // The promise should still be an object with .then.
            "assert.sameValue(typeof p, 'object', 'rejected promise is object');\n",
            "assert.sameValue(typeof p.then, 'function', 'rejected promise has .then');\n",
        ),
        "async-throw.js",
    );
    assert_eq!(r, RegisterValue::from_i32(0));
}

#[test]
fn async_arrow_function() {
    // Async arrow functions should also return promises.
    let r = run(
        concat!(
            "var f = async () => 42;\n",
            "var p = f();\n",
            "assert.sameValue(typeof p, 'object', 'async arrow returns object');\n",
            "assert.sameValue(typeof p.then, 'function', 'async arrow result has .then');\n",
        ),
        "async-arrow.js",
    );
    assert_eq!(r, RegisterValue::from_i32(0));
}

#[test]
fn async_function_expression() {
    // Async function expressions should compile and return promises.
    let r = run(
        concat!(
            "var f = async function() { return 100; };\n",
            "var p = f();\n",
            "assert.sameValue(typeof p, 'object', 'async fn expr returns object');\n",
            "assert.sameValue(typeof p.then, 'function', 'async fn expr has .then');\n",
        ),
        "async-fn-expr.js",
    );
    assert_eq!(r, RegisterValue::from_i32(0));
}

#[test]
fn async_function_with_multiple_awaits() {
    // Multiple awaits on non-promise values should work sequentially.
    let r = run(
        concat!(
            "async function f() {\n",
            "  var a = await 1;\n",
            "  var b = await 2;\n",
            "  var c = await 3;\n",
            "  return a + b + c;\n",
            "}\n",
            "var p = f();\n",
            "assert.sameValue(typeof p.then, 'function', 'result is thenable');\n",
        ),
        "async-multi-await.js",
    );
    assert_eq!(r, RegisterValue::from_i32(0));
}
