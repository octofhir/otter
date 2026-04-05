//! Integration tests for Object.getOwnPropertySymbols (Step 59).
//!
//! Spec references:
//! - §20.1.2.11 Object.getOwnPropertySymbols: <https://tc39.es/ecma262/#sec-object.getownpropertysymbols>

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
//  Basic functionality
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn exists_as_function() {
    assert!(run_bool(
        "typeof Object.getOwnPropertySymbols === 'function'"
    ));
}

#[test]
fn length_is_one() {
    assert_eq!(run_i32("Object.getOwnPropertySymbols.length"), 1);
}

#[test]
fn empty_object_returns_empty_array() {
    assert_eq!(run_i32("Object.getOwnPropertySymbols({}).length"), 0);
}

#[test]
fn returns_array() {
    assert!(run_bool("Array.isArray(Object.getOwnPropertySymbols({}))"));
}

// ═══════════════════════════════════════════════════════════════════════════
//  Symbol-keyed properties
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn finds_symbol_keyed_property() {
    assert_eq!(
        run_i32(
            "var s = Symbol('test'); \
             var obj = {}; \
             obj[s] = 42; \
             Object.getOwnPropertySymbols(obj).length"
        ),
        1
    );
}

#[test]
fn symbol_key_matches_original() {
    assert!(run_bool(
        "var s = Symbol('test'); \
         var obj = {}; \
         obj[s] = 42; \
         Object.getOwnPropertySymbols(obj)[0] === s"
    ));
}

#[test]
fn multiple_symbol_keys() {
    assert_eq!(
        run_i32(
            "var s1 = Symbol('a'); \
             var s2 = Symbol('b'); \
             var obj = {}; \
             obj[s1] = 1; \
             obj[s2] = 2; \
             Object.getOwnPropertySymbols(obj).length"
        ),
        2
    );
}

#[test]
fn excludes_string_keyed_properties() {
    assert_eq!(
        run_i32(
            "var s = Symbol('test'); \
             var obj = { x: 1, y: 2 }; \
             obj[s] = 3; \
             Object.getOwnPropertySymbols(obj).length"
        ),
        1
    );
}

// ═══════════════════════════════════════════════════════════════════════════
//  Well-known symbols
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn finds_well_known_symbol_iterator() {
    assert!(run_bool(
        "var arr = [1, 2, 3]; \
         var syms = Object.getOwnPropertySymbols(arr); \
         syms.indexOf(Symbol.iterator) !== -1 || \
         Object.getOwnPropertySymbols(Array.prototype).indexOf(Symbol.iterator) !== -1"
    ));
}

#[test]
fn symbol_property_on_custom_object() {
    // Manually set a well-known symbol on an object and find it
    assert!(run_bool(
        "var s1 = Symbol('x'); \
         var s2 = Symbol('y'); \
         var obj = {}; \
         obj[s1] = 1; \
         obj[s2] = 2; \
         obj.foo = 3; \
         var syms = Object.getOwnPropertySymbols(obj); \
         syms.length === 2 && syms.indexOf(s1) !== -1 && syms.indexOf(s2) !== -1"
    ));
}

// ═══════════════════════════════════════════════════════════════════════════
//  Edge cases
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn no_symbol_properties_returns_empty() {
    assert_eq!(
        run_i32("Object.getOwnPropertySymbols({ a: 1, b: 2 }).length"),
        0
    );
}

#[test]
fn does_not_include_inherited_symbols() {
    assert_eq!(
        run_i32(
            "var s = Symbol('parent'); \
             var parent = {}; \
             parent[s] = 1; \
             var child = Object.create(parent); \
             Object.getOwnPropertySymbols(child).length"
        ),
        0
    );
}

#[test]
fn string_argument_coerced_to_object() {
    // String primitives have no own symbol properties
    assert_eq!(run_i32("Object.getOwnPropertySymbols('hello').length"), 0);
}

#[test]
fn number_argument_coerced_to_object() {
    assert_eq!(run_i32("Object.getOwnPropertySymbols(42).length"), 0);
}
