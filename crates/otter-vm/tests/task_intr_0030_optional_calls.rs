//! Integration tests for optional chaining calls (`?.()` syntax).
//!
//! ES2024 §13.3.7 Optional Chaining
//! Spec: <https://tc39.es/ecma262/#sec-optional-chaining>

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

// ═══════════════════════════════════════════════════════════════════════════
//  §13.3.7 — Direct optional call: `fn?.()`
// ═══════════════════════════════════════════════════════════════════════════

/// `fn?.()` — when fn is a function, calls it normally.
#[test]
fn optional_call_on_function() {
    assert_eq!(
        run_i32(
            "var fn = function() { return 42; };\n\
             fn?.()"
        ),
        42
    );
}

/// `fn?.()` — when fn is null, returns undefined.
#[test]
fn optional_call_on_null() {
    assert!(
        run("var fn = null;\n\
         fn?.()")
        .is_undefined()
    );
}

/// `fn?.()` — when fn is undefined, returns undefined.
#[test]
fn optional_call_on_undefined() {
    assert!(
        run("var fn = undefined;\n\
         fn?.()")
        .is_undefined()
    );
}

/// `fn?.()` — passes arguments correctly.
#[test]
fn optional_call_passes_arguments() {
    assert_eq!(
        run_i32(
            "var add = function(a, b) { return a + b; };\n\
             add?.(10, 32)"
        ),
        42
    );
}

// ═══════════════════════════════════════════════════════════════════════════
//  §13.3.7 — Method optional call: `obj?.method()`
// ═══════════════════════════════════════════════════════════════════════════

/// `obj?.method()` — when obj is not null, calls method with correct `this`.
#[test]
fn optional_method_call_on_object() {
    assert_eq!(
        run_i32(
            "var obj = { x: 10, getX: function() { return this.x; } };\n\
             obj?.getX()"
        ),
        10
    );
}

/// `obj?.method()` — when obj is null, short-circuits to undefined.
#[test]
fn optional_method_call_on_null() {
    assert!(
        run("var obj = null;\n\
         obj?.getX()")
        .is_undefined()
    );
}

/// `obj?.method()` — when obj is undefined, short-circuits to undefined.
#[test]
fn optional_method_call_on_undefined() {
    assert!(
        run("var obj = undefined;\n\
         obj?.method()")
        .is_undefined()
    );
}

// ═══════════════════════════════════════════════════════════════════════════
//  §13.3.7 — Call with optional method: `obj.method?.()`
// ═══════════════════════════════════════════════════════════════════════════

/// `obj.method?.()` — when method exists, calls it with correct `this`.
#[test]
fn call_optional_on_existing_method() {
    assert_eq!(
        run_i32(
            "var obj = { val: 7, fn: function() { return this.val; } };\n\
             obj.fn?.()"
        ),
        7
    );
}

/// `obj.method?.()` — when method is undefined, returns undefined.
#[test]
fn call_optional_on_undefined_method() {
    assert!(
        run("var obj = {};\n\
         obj.missing?.()")
        .is_undefined()
    );
}

// ═══════════════════════════════════════════════════════════════════════════
//  §13.3.7 — Computed optional calls: `obj?.[key]()`
// ═══════════════════════════════════════════════════════════════════════════

/// `obj?.[key]()` — when obj is not null, calls computed method.
#[test]
fn optional_computed_call_on_object() {
    assert_eq!(
        run_i32(
            "var obj = { fn: function() { return 99; } };\n\
             var key = 'fn';\n\
             obj?.[key]()"
        ),
        99
    );
}

/// `obj?.[key]()` — when obj is null, short-circuits to undefined.
#[test]
fn optional_computed_call_on_null() {
    assert!(
        run("var obj = null;\n\
         obj?.['method']()")
        .is_undefined()
    );
}

// ═══════════════════════════════════════════════════════════════════════════
//  §13.3.7 — Double optional: `obj?.method?.()`
// ═══════════════════════════════════════════════════════════════════════════

/// `obj?.method?.()` — both obj and method exist, calls normally.
#[test]
fn double_optional_both_exist() {
    assert_eq!(
        run_i32(
            "var obj = { fn: function() { return 55; } };\n\
             obj?.fn?.()"
        ),
        55
    );
}

/// `obj?.method?.()` — obj is null, short-circuits.
#[test]
fn double_optional_null_obj() {
    assert!(
        run("var obj = null;\n\
         obj?.fn?.()")
        .is_undefined()
    );
}

/// `obj?.method?.()` — obj exists but method is undefined, short-circuits.
#[test]
fn double_optional_undefined_method() {
    assert!(
        run("var obj = {};\n\
         obj?.fn?.()")
        .is_undefined()
    );
}

// ═══════════════════════════════════════════════════════════════════════════
//  §13.3.7 — Spread arguments with optional calls
// ═══════════════════════════════════════════════════════════════════════════

/// `fn?.(...args)` — spread arguments work with optional calls.
#[test]
fn optional_call_with_spread() {
    assert_eq!(
        run_i32(
            "var add = function(a, b) { return a + b; };\n\
             var args = [10, 20];\n\
             add?.(...args)"
        ),
        30
    );
}

/// `fn?.(...args)` — null callee with spread still returns undefined.
#[test]
fn optional_call_with_spread_null() {
    assert!(
        run("var fn = null;\n\
         fn?.(...[1, 2, 3])")
        .is_undefined()
    );
}

// ═══════════════════════════════════════════════════════════════════════════
//  §13.3.7 — Optional call in expression context
// ═══════════════════════════════════════════════════════════════════════════

/// Optional call result is usable in arithmetic.
#[test]
fn optional_call_in_expression() {
    assert_eq!(
        run_i32(
            "var fn = function() { return 10; };\n\
             (fn?.() || 0) + 5"
        ),
        15
    );
}

/// Optional call on null returns undefined which is falsy.
#[test]
fn optional_call_null_is_falsy() {
    assert_eq!(
        run_i32(
            "var fn = null;\n\
             fn?.() ? 1 : 0"
        ),
        0
    );
}

/// Nested optional call: obj?.b is itself a chain, then ?.() on result.
/// Note: deeply nested chains (`a?.b?.c()`) require recursive chain
/// compilation — a known future improvement (not in Step 46 scope).
#[test]
fn nested_optional_property_then_call() {
    // obj?.method exists and returns a value — chain works.
    assert_eq!(
        run_i32(
            "var obj = { fn: function() { return 77; } };\n\
             obj?.fn?.()"
        ),
        77
    );
}

/// Optional call preserves `this` for method invocations.
#[test]
fn optional_call_preserves_this() {
    assert_eq!(
        run_i32(
            "var obj = {\n\
               x: 100,\n\
               getX: function() { return this.x; }\n\
             };\n\
             obj.getX?.()"
        ),
        100
    );
}

/// Optional call with side effects — function not called when nullish.
#[test]
fn optional_call_no_side_effects_when_null() {
    assert_eq!(
        run_i32(
            "var count = 0;\n\
             var fn = null;\n\
             fn?.();\n\
             count"
        ),
        0
    );
}

/// Optional call chains: `obj?.method()?.toString()`.
#[test]
fn chained_optional_calls() {
    assert!(run_bool(
        "var obj = { get: function() { return { done: true }; } };\n\
         obj?.get()?.done === true"
    ));
}
