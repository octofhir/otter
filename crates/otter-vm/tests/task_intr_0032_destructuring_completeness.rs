//! Integration tests for destructuring completeness (Step 47).
//!
//! ES2024 §13.15.5 Destructuring Assignment
//! Spec: <https://tc39.es/ecma262/#sec-destructuring-assignment>
//!
//! §13.7.5.12 ForIn/OfBodyEvaluation
//! Spec: <https://tc39.es/ecma262/#sec-runtime-semantics-forinofbodyevaluation>

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

// ═══════════════════════════════════════════════════════════════════════════
//  §13.7.5.12 — For-of with array destructuring: `for (const [a, b] of arr)`
// ═══════════════════════════════════════════════════════════════════════════

/// Basic array destructuring in for-of loop.
#[test]
fn for_of_array_destructuring() {
    assert_eq!(
        run_i32(
            "var sum = 0;\n\
             var pairs = [[1, 2], [3, 4], [5, 6]];\n\
             for (var [a, b] of pairs) { sum = sum + a + b; }\n\
             sum"
        ),
        21
    );
}

/// Array destructuring with const binding in for-of.
#[test]
fn for_of_const_array_destructuring() {
    assert_eq!(
        run_i32(
            "var sum = 0;\n\
             for (const [x, y] of [[10, 20], [30, 40]]) { sum = sum + x + y; }\n\
             sum"
        ),
        100
    );
}

/// Array destructuring with rest in for-of.
#[test]
fn for_of_array_destructuring_rest() {
    assert_eq!(
        run_i32(
            "var total = 0;\n\
             for (const [first, ...rest] of [[1, 2, 3], [4, 5, 6]]) {\n\
               total = total + first + rest.length;\n\
             }\n\
             total"
        ),
        // first=1, rest.length=2 => 3;  first=4, rest.length=2 => 6;  total=9
        9
    );
}

/// Array destructuring with skipped elements in for-of.
#[test]
fn for_of_array_destructuring_holes() {
    assert_eq!(
        run_i32(
            "var sum = 0;\n\
             for (var [, second] of [[1, 2], [3, 4]]) { sum = sum + second; }\n\
             sum"
        ),
        6
    );
}

// ═══════════════════════════════════════════════════════════════════════════
//  §13.7.5.12 — For-of with object destructuring: `for (const {a, b} of arr)`
// ═══════════════════════════════════════════════════════════════════════════

/// Basic object destructuring in for-of loop.
#[test]
fn for_of_object_destructuring() {
    assert_eq!(
        run_i32(
            "var sum = 0;\n\
             var items = [{x: 1, y: 2}, {x: 3, y: 4}];\n\
             for (var {x, y} of items) { sum = sum + x + y; }\n\
             sum"
        ),
        10
    );
}

/// Object destructuring with renaming in for-of.
#[test]
fn for_of_object_destructuring_rename() {
    assert_eq!(
        run_i32(
            "var sum = 0;\n\
             for (const {a: val} of [{a: 10}, {a: 20}]) { sum = sum + val; }\n\
             sum"
        ),
        30
    );
}

/// Object destructuring with default values in for-of.
#[test]
fn for_of_object_destructuring_defaults() {
    assert_eq!(
        run_i32(
            "var sum = 0;\n\
             for (var {x, y = 100} of [{x: 1}, {x: 2, y: 3}]) {\n\
               sum = sum + x + y;\n\
             }\n\
             sum"
        ),
        // {x:1} → x=1, y=100 → 101; {x:2, y:3} → x=2, y=3 → 5; total=106
        106
    );
}

/// Object destructuring with rest in for-of.
#[test]
fn for_of_object_destructuring_rest() {
    assert_eq!(
        run_i32(
            "var count = 0;\n\
             for (const {a, ...rest} of [{a: 1, b: 2, c: 3}]) {\n\
               count = a + rest.b + rest.c;\n\
             }\n\
             count"
        ),
        6
    );
}

// ═══════════════════════════════════════════════════════════════════════════
//  §13.7.5.12 — For-of with nested destructuring
// ═══════════════════════════════════════════════════════════════════════════

/// Nested array + object destructuring in for-of.
#[test]
fn for_of_nested_destructuring() {
    assert_eq!(
        run_i32(
            "var sum = 0;\n\
             var data = [{point: [1, 2]}, {point: [3, 4]}];\n\
             for (const {point: [x, y]} of data) {\n\
               sum = sum + x + y;\n\
             }\n\
             sum"
        ),
        10
    );
}

// ═══════════════════════════════════════════════════════════════════════════
//  §13.7.5.12 — For-of destructuring with assignment targets
//  `for ({a, b} of arr)` (no const/let/var)
// ═══════════════════════════════════════════════════════════════════════════

/// Assignment-style object destructuring in for-of.
#[test]
fn for_of_assignment_object_destructuring() {
    assert_eq!(
        run_i32(
            "var a, b, sum = 0;\n\
             for ({a, b} of [{a: 1, b: 2}, {a: 3, b: 4}]) {\n\
               sum = sum + a + b;\n\
             }\n\
             sum"
        ),
        10
    );
}

/// Assignment-style array destructuring in for-of.
#[test]
fn for_of_assignment_array_destructuring() {
    assert_eq!(
        run_i32(
            "var x, y, sum = 0;\n\
             for ([x, y] of [[10, 20], [30, 40]]) {\n\
               sum = sum + x + y;\n\
             }\n\
             sum"
        ),
        100
    );
}

// ═══════════════════════════════════════════════════════════════════════════
//  §13.7.5.12 — For-in with destructuring
// ═══════════════════════════════════════════════════════════════════════════

/// For-in destructuring with array pattern extracts key characters.
#[test]
fn for_in_array_destructuring() {
    assert_eq!(
        run_i32(
            "var count = 0;\n\
             var obj = {a: 1, b: 2, c: 3};\n\
             for (var [first] of Object.keys(obj)) {\n\
               count = count + 1;\n\
             }\n\
             count"
        ),
        3
    );
}

// ═══════════════════════════════════════════════════════════════════════════
//  §14.15.2 — Catch clause destructuring: `catch ({message})`
// ═══════════════════════════════════════════════════════════════════════════

/// Object destructuring in catch clause.
#[test]
fn catch_object_destructuring() {
    assert_eq!(
        run_i32(
            "var result = 0;\n\
             try { throw {code: 42, msg: 'fail'}; }\n\
             catch ({code}) { result = code; }\n\
             result"
        ),
        42
    );
}

/// Array destructuring in catch clause.
#[test]
fn catch_array_destructuring() {
    assert_eq!(
        run_i32(
            "var result = 0;\n\
             try { throw [10, 20, 30]; }\n\
             catch ([a, b, c]) { result = a + b + c; }\n\
             result"
        ),
        60
    );
}

// ═══════════════════════════════════════════════════════════════════════════
//  §13.15.5 — Computed property keys in destructuring
// ═══════════════════════════════════════════════════════════════════════════

/// Computed key in object destructuring binding.
#[test]
fn computed_key_object_destructuring() {
    assert_eq!(
        run_i32(
            "var key = 'value';\n\
             var {[key]: x} = {value: 42};\n\
             x"
        ),
        42
    );
}

/// Computed key in object destructuring with default.
#[test]
fn computed_key_destructuring_with_default() {
    assert_eq!(
        run_i32(
            "var key = 'missing';\n\
             var {[key]: x = 99} = {};\n\
             x"
        ),
        99
    );
}

// ═══════════════════════════════════════════════════════════════════════════
//  §13.7.5.12 — For-await-of with destructuring
// ═══════════════════════════════════════════════════════════════════════════

/// For-await-of with array destructuring.
#[test]
fn for_await_of_array_destructuring() {
    assert_eq!(
        run_i32(
            "var sum = 0;\n\
             (async function() {\n\
               for await (const [a, b] of [[1, 2], [3, 4]]) {\n\
                 sum = sum + a + b;\n\
               }\n\
             })();\n\
             sum"
        ),
        10
    );
}

/// For-await-of with object destructuring.
#[test]
fn for_await_of_object_destructuring() {
    assert_eq!(
        run_i32(
            "var sum = 0;\n\
             (async function() {\n\
               for await (const {x, y} of [{x: 5, y: 10}, {x: 15, y: 20}]) {\n\
                 sum = sum + x + y;\n\
               }\n\
             })();\n\
             sum"
        ),
        50
    );
}

// ═══════════════════════════════════════════════════════════════════════════
//  Edge cases
// ═══════════════════════════════════════════════════════════════════════════

/// Empty destructuring pattern — loop body still executes.
#[test]
fn for_of_empty_destructuring() {
    assert_eq!(
        run_i32(
            "var count = 0;\n\
             for (var {} of [{a:1}, {b:2}]) { count = count + 1; }\n\
             count"
        ),
        2
    );
}

/// Destructuring a single-element array in for-of.
#[test]
fn for_of_single_element_array_destructuring() {
    assert_eq!(
        run_i32(
            "var sum = 0;\n\
             for (var [x] of [[10], [20], [30]]) { sum = sum + x; }\n\
             sum"
        ),
        60
    );
}

/// let binding in for-of destructuring.
#[test]
fn for_of_let_destructuring() {
    assert_eq!(
        run_i32(
            "var sum = 0;\n\
             for (let {val} of [{val: 7}, {val: 8}]) { sum = sum + val; }\n\
             sum"
        ),
        15
    );
}
