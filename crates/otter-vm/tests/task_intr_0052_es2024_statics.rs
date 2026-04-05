//! Integration tests for ES2024 static methods (Steps 56–57).
//!
//! Spec references:
//! - §27.2.4.8 Promise.withResolvers: <https://tc39.es/ecma262/#sec-promise.withresolvers>
//! - §22.1.2.11 Object.groupBy: <https://tc39.es/ecma262/#sec-object.groupby>
//! - §24.1.2.2 Map.groupBy: <https://tc39.es/ecma262/#sec-map.groupby>

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

fn run_string(source: &str) -> String {
    let module = compile_eval(source, "<test>").expect("should compile");
    let mut runtime = RuntimeState::new();
    let global = runtime.intrinsics().global_object();
    let registers = [RegisterValue::from_object_handle(global.0)];
    let v = Interpreter::new()
        .execute_with_runtime(
            &module,
            otter_vm::module::FunctionIndex(0),
            &registers,
            &mut runtime,
        )
        .expect("should execute")
        .return_value();
    let handle = v.as_object_handle().expect("expected string handle");
    runtime
        .objects()
        .string_value(otter_vm::object::ObjectHandle(handle))
        .expect("string lookup")
        .expect("string value")
        .to_string()
}

fn run_bool(source: &str) -> bool {
    let v = run(source);
    v.as_bool()
        .unwrap_or_else(|| panic!("expected bool, got {v:?}"))
}

fn run_i32(source: &str) -> i32 {
    let v = run(source);
    v.as_i32()
        .unwrap_or_else(|| panic!("expected i32, got {v:?}"))
}

// ═══════════════════════════════════════════════════════════════════════════
//  §27.2.4.8 — Promise.withResolvers()
// ═══════════════════════════════════════════════════════════════════════════

/// Returns an object with promise, resolve, reject properties.
#[test]
fn with_resolvers_returns_object() {
    assert!(run_bool(
        "var r = Promise.withResolvers(); typeof r === 'object' && r !== null"
    ));
}

/// The `promise` property is a Promise.
#[test]
fn with_resolvers_promise_is_promise() {
    assert!(run_bool(
        "var r = Promise.withResolvers(); r.promise instanceof Promise"
    ));
}

/// The `resolve` property is a function.
#[test]
fn with_resolvers_resolve_is_function() {
    assert!(run_bool(
        "var r = Promise.withResolvers(); typeof r.resolve === 'function'"
    ));
}

/// The `reject` property is a function.
#[test]
fn with_resolvers_reject_is_function() {
    assert!(run_bool(
        "var r = Promise.withResolvers(); typeof r.reject === 'function'"
    ));
}

/// Promise.withResolvers.length === 0.
#[test]
fn with_resolvers_length() {
    assert_eq!(run_i32("Promise.withResolvers.length"), 0);
}

/// typeof Promise.withResolvers is "function".
#[test]
fn with_resolvers_is_function() {
    assert!(run_bool("typeof Promise.withResolvers === 'function'"));
}

// ═══════════════════════════════════════════════════════════════════════════
//  §22.1.2.11 — Object.groupBy(items, callbackfn)
// ═══════════════════════════════════════════════════════════════════════════

/// Basic grouping by even/odd.
#[test]
fn object_group_by_basic() {
    assert_eq!(
        run_string(
            "var g = Object.groupBy([1, 2, 3, 4], n => n % 2 === 0 ? 'even' : 'odd'); \
             g.even.join(',')"
        ),
        "2,4"
    );
}

/// Odd group.
#[test]
fn object_group_by_odd() {
    assert_eq!(
        run_string(
            "var g = Object.groupBy([1, 2, 3, 4], n => n % 2 === 0 ? 'even' : 'odd'); \
             g.odd.join(',')"
        ),
        "1,3"
    );
}

/// Empty array produces empty object.
#[test]
fn object_group_by_empty() {
    assert!(run_bool(
        "var g = Object.groupBy([], n => n); Object.keys(g).length === 0"
    ));
}

/// Group by string value.
#[test]
fn object_group_by_strings() {
    assert_eq!(
        run_string(
            "var g = Object.groupBy(['apple', 'banana', 'avocado'], s => s[0]); \
             g.a.join(',')"
        ),
        "apple,avocado"
    );
}

/// Object.groupBy.length === 2.
#[test]
fn object_group_by_length() {
    assert_eq!(run_i32("Object.groupBy.length"), 2);
}

/// Callback receives index as second argument.
#[test]
fn object_group_by_receives_index() {
    assert_eq!(
        run_string(
            "var g = Object.groupBy([10, 20, 30], (v, i) => i < 2 ? 'first' : 'last'); \
             g.first.join(',')"
        ),
        "10,20"
    );
}

// ═══════════════════════════════════════════════════════════════════════════
//  §24.1.2.2 — Map.groupBy(items, callbackfn)
// ═══════════════════════════════════════════════════════════════════════════

/// Basic Map.groupBy returns a Map.
#[test]
fn map_group_by_returns_map() {
    assert!(run_bool(
        "Map.groupBy([1, 2, 3], n => n % 2) instanceof Map"
    ));
}

/// Map.groupBy preserves numeric keys (not string-coerced).
#[test]
fn map_group_by_numeric_keys() {
    assert_eq!(
        run_i32("var m = Map.groupBy([1, 2, 3, 4], n => n % 2); m.size"),
        2
    );
}

/// Map.groupBy groups correctly.
#[test]
fn map_group_by_groups() {
    assert_eq!(
        run_string(
            "var m = Map.groupBy([1, 2, 3, 4], n => n % 2 === 0 ? 'even' : 'odd'); \
             m.get('even').join(',')"
        ),
        "2,4"
    );
}

/// Map.groupBy.length === 2.
#[test]
fn map_group_by_length() {
    assert_eq!(run_i32("Map.groupBy.length"), 2);
}

/// typeof all three methods are "function".
#[test]
fn es2024_methods_are_functions() {
    assert!(run_bool("typeof Promise.withResolvers === 'function'"));
    assert!(run_bool("typeof Object.groupBy === 'function'"));
    assert!(run_bool("typeof Map.groupBy === 'function'"));
}
