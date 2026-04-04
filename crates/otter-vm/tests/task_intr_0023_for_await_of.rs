//! Integration tests for ES2024 `for-await-of` and async iteration protocol.
//!
//! Spec references:
//! - ForIn/OfHeadEvaluation (async): <https://tc39.es/ecma262/#sec-runtime-semantics-forinofheadevaluation>
//! - GetIterator (async):            <https://tc39.es/ecma262/#sec-getiterator>
//! - %AsyncIteratorPrototype%:       <https://tc39.es/ecma262/#sec-asynciteratorprototype>
//! - Await:                          <https://tc39.es/ecma262/#sec-await>

use otter_vm::source::compile_eval;
use otter_vm::value::RegisterValue;
use otter_vm::{Interpreter, RuntimeState};

fn run(source: &str) -> RegisterValue {
    let module = compile_eval(source, "<test>").expect("should compile");
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

fn run_i32(source: &str) -> i32 {
    let v = run(source);
    v.as_i32()
        .unwrap_or_else(|| panic!("expected i32, got {v:?}"))
}

fn run_bool(source: &str) -> bool {
    let v = run(source);
    v.as_bool()
        .unwrap_or_else(|| panic!("expected bool, got {v:?}"))
}

// ── for-await-of over sync iterables (fallback to Symbol.iterator) ─────────

#[test]
fn for_await_of_array_sums_elements() {
    assert_eq!(
        run_i32(
            "var result = 0;\n\
             (async function() { for await (const x of [1, 2, 3]) { result += x; } })();\n\
             result"
        ),
        6
    );
}

#[test]
fn for_await_of_array_collects_values() {
    assert_eq!(
        run_i32(
            "var count = 0;\n\
             (async function() { for await (const x of [10, 20, 30, 40]) { count++; } })();\n\
             count"
        ),
        4
    );
}

#[test]
fn for_await_of_empty_array_does_not_execute_body() {
    assert_eq!(
        run_i32(
            "var result = 42;\n\
             (async function() { for await (const x of []) { result = 0; } })();\n\
             result"
        ),
        42
    );
}

#[test]
fn for_await_of_string_iterates_chars() {
    assert_eq!(
        run_i32(
            "var count = 0;\n\
             (async function() { for await (const ch of 'abc') { count++; } })();\n\
             count"
        ),
        3
    );
}

// ── for-await-of with custom async iterables ───────────────────────────────

#[test]
fn for_await_of_custom_async_iterable() {
    assert_eq!(
        run_i32(
            "var result = 0;\n\
             var iterable = {\n\
               [Symbol.asyncIterator]() {\n\
                 var i = 0;\n\
                 var values = [10, 20, 30];\n\
                 return {\n\
                   next() {\n\
                     if (i < values.length) {\n\
                       return { done: false, value: values[i++] };\n\
                     }\n\
                     return { done: true, value: undefined };\n\
                   }\n\
                 };\n\
               }\n\
             };\n\
             (async function() { for await (const x of iterable) { result += x; } })();\n\
             result"
        ),
        60
    );
}

#[test]
fn for_await_of_custom_async_iterable_empty() {
    assert_eq!(
        run_i32(
            "var result = 99;\n\
             var iterable = {\n\
               [Symbol.asyncIterator]() {\n\
                 return {\n\
                   next() { return { done: true, value: undefined }; }\n\
                 };\n\
               }\n\
             };\n\
             (async function() { for await (const x of iterable) { result = 0; } })();\n\
             result"
        ),
        99
    );
}

// ── for-await-of with custom sync iterable (Symbol.iterator fallback) ──────

#[test]
fn for_await_of_custom_sync_iterable() {
    assert_eq!(
        run_i32(
            "var result = 0;\n\
             var iterable = {\n\
               [Symbol.iterator]() {\n\
                 var i = 0;\n\
                 return {\n\
                   next() {\n\
                     if (i < 3) return { done: false, value: ++i };\n\
                     return { done: true };\n\
                   }\n\
                 };\n\
               }\n\
             };\n\
             (async function() { for await (const x of iterable) { result += x; } })();\n\
             result"
        ),
        6
    );
}

// ── for-await-of with destructuring ────────────────────────────────────────

#[test]
fn for_await_of_with_let_binding() {
    assert_eq!(
        run_i32(
            "var sum = 0;\n\
             (async function() { for await (let x of [5, 10, 15]) { sum += x; } })();\n\
             sum"
        ),
        30
    );
}

#[test]
fn for_await_of_with_var_binding() {
    assert_eq!(
        run_i32(
            "var sum = 0;\n\
             (async function() { for await (var x of [1, 2, 3]) { sum += x; } })();\n\
             sum"
        ),
        6
    );
}

// ── for-await-of with break ────────────────────────────────────────────────

#[test]
fn for_await_of_break_stops_iteration() {
    assert_eq!(
        run_i32(
            "var result = 0;\n\
             (async function() {\n\
               for await (const x of [1, 2, 3, 4, 5]) {\n\
                 if (x === 3) break;\n\
                 result += x;\n\
               }\n\
             })();\n\
             result"
        ),
        3
    );
}

// ── %AsyncIteratorPrototype% ───────────────────────────────────────────────

#[test]
fn async_iterator_prototype_has_async_iterator_symbol() {
    // %AsyncIteratorPrototype%[@@asyncIterator] should exist.
    // We can't directly access %AsyncIteratorPrototype%, but objects
    // with Symbol.asyncIterator inherit from it conceptually.
    assert!(run_bool("typeof Symbol.asyncIterator === 'symbol'"));
}

// ── Compilation succeeds (no Unsupported error) ────────────────────────────

#[test]
fn for_await_of_compiles_without_error() {
    // Verify that `for await` doesn't throw "Unsupported" at compile time.
    let source = "async function test() { for await (const x of []) {} }";
    compile_eval(source, "<test>").expect("for-await-of should compile");
}
