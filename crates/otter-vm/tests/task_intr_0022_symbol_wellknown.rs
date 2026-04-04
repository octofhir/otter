//! Integration tests for ES2024 Symbol well-known symbols completion.
//!
//! Spec references:
//! - IsConcatSpreadable: <https://tc39.es/ecma262/#sec-isconcatspreadable>
//! - Array.prototype.concat: <https://tc39.es/ecma262/#sec-array.prototype.concat>
//! - Array.prototype[@@unscopables]: <https://tc39.es/ecma262/#sec-array.prototype-%symbol.unscopables%>
//! - Symbol.for: <https://tc39.es/ecma262/#sec-symbol.for>
//! - Symbol.keyFor: <https://tc39.es/ecma262/#sec-symbol.keyfor>

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

// ── §22.1.3.1.1 IsConcatSpreadable ���─────────────────────────────────────────

#[test]
fn concat_spreads_arrays_by_default() {
    assert_eq!(run_i32("[1,2].concat([3,4]).length"), 4);
}

#[test]
fn concat_does_not_spread_non_arrays() {
    // Non-array objects are appended as single elements
    assert_eq!(run_i32("[1].concat({length:2, 0:'a', 1:'b'}).length"), 2);
}

#[test]
fn concat_is_concat_spreadable_true_forces_spread() {
    // Setting @@isConcatSpreadable to true on a non-array makes it spreadable
    assert_eq!(
        run_i32(
            "var obj = {length:2, 0:'a', 1:'b', [Symbol.isConcatSpreadable]: true};\n\
             [].concat(obj).length"
        ),
        2
    );
}

#[test]
fn concat_is_concat_spreadable_false_prevents_spread() {
    // Setting @@isConcatSpreadable to false on an array prevents spreading
    assert_eq!(
        run_i32(
            "var arr = [3,4]; arr[Symbol.isConcatSpreadable] = false;\n\
             [1,2].concat(arr).length"
        ),
        3
    );
}

#[test]
fn concat_is_concat_spreadable_undefined_falls_back_to_is_array() {
    // When @@isConcatSpreadable is undefined, fall back to IsArray
    assert!(run_bool("[1,2].concat([3]).length === 3"));
}

#[test]
fn concat_spreads_this_value() {
    // The receiver (this) is also checked for @@isConcatSpreadable
    assert_eq!(run_i32("[1,2,3].concat().length"), 3);
}

#[test]
fn concat_this_not_spreadable() {
    // Array with @@isConcatSpreadable = false as receiver
    assert_eq!(
        run_i32(
            "var arr = [1,2,3]; arr[Symbol.isConcatSpreadable] = false;\n\
             Array.prototype.concat.call(arr, 4).length"
        ),
        2
    );
}

// ── §23.1.3.38 Array.prototype[@@unscopables] ───────────────────────────────

#[test]
fn unscopables_object_exists() {
    assert!(run_bool(
        "Array.prototype[Symbol.unscopables] !== undefined"
    ));
}

#[test]
fn unscopables_has_null_prototype() {
    assert!(run_bool(
        "Object.getPrototypeOf(Array.prototype[Symbol.unscopables]) === null"
    ));
}

#[test]
fn unscopables_contains_expected_keys() {
    let keys = [
        "at",
        "copyWithin",
        "entries",
        "fill",
        "find",
        "findIndex",
        "findLast",
        "findLastIndex",
        "flat",
        "flatMap",
        "includes",
        "keys",
        "toReversed",
        "toSorted",
        "toSpliced",
        "values",
    ];
    for key in keys {
        assert!(
            run_bool(&format!(
                "Array.prototype[Symbol.unscopables]['{key}'] === true"
            )),
            "@@unscopables should have {key} set to true"
        );
    }
}

#[test]
fn unscopables_does_not_contain_non_members() {
    // push, pop, etc. should NOT be in @@unscopables
    assert!(run_bool(
        "Array.prototype[Symbol.unscopables].push === undefined"
    ));
    assert!(run_bool(
        "Array.prototype[Symbol.unscopables].pop === undefined"
    ));
}

// ── Symbol.for / Symbol.keyFor ───────────────────────────────────��──────────

#[test]
fn symbol_for_reuses_registry() {
    assert!(run_bool("Symbol.for('x') === Symbol.for('x')"));
}

#[test]
fn symbol_constructor_creates_unique() {
    assert!(run_bool("Symbol('x') !== Symbol('x')"));
}

#[test]
fn symbol_for_differs_from_constructor() {
    assert!(run_bool("Symbol.for('x') !== Symbol('x')"));
}

#[test]
fn symbol_key_for_returns_key() {
    assert!(run_bool("Symbol.keyFor(Symbol.for('hello')) === 'hello'"));
}

#[test]
fn symbol_key_for_undefined_for_unregistered() {
    assert!(run_bool("Symbol.keyFor(Symbol('hello')) === undefined"));
}

// ── Well-known symbol properties on Symbol constructor ──────────────────────

#[test]
fn well_known_symbol_properties_exist() {
    let symbols = [
        "iterator",
        "asyncIterator",
        "toStringTag",
        "species",
        "hasInstance",
        "isConcatSpreadable",
        "match",
        "matchAll",
        "replace",
        "search",
        "split",
        "toPrimitive",
        "unscopables",
    ];
    for name in symbols {
        assert!(
            run_bool(&format!("typeof Symbol.{name} === 'symbol'")),
            "Symbol.{name} should be a symbol"
        );
    }
}

#[test]
fn symbol_description_reflects_well_known_names() {
    assert!(run_bool(
        "Symbol.iterator.description === 'Symbol.iterator'"
    ));
    assert!(run_bool(
        "Symbol.toStringTag.description === 'Symbol.toStringTag'"
    ));
}

// ── Symbol.toPrimitive ──────────────────────────────────────────────────────

#[test]
fn symbol_to_primitive_returns_symbol() {
    assert!(run_bool(
        "var s = Symbol('test'); s[Symbol.toPrimitive]('default') === s"
    ));
}

// ── Symbol.hasInstance ──────────────────────────────────────────────────────

#[test]
fn has_instance_overrides_instanceof() {
    assert!(run_bool(
        "function Foo() {} Foo[Symbol.hasInstance] = function(x) { return x === 42; };\n\
         (42 instanceof Foo) === true"
    ));
}

// ── typeof symbol ──────────────────────────────────────────────────────────

#[test]
fn typeof_symbol_is_symbol() {
    assert!(run_bool("typeof Symbol() === 'symbol'"));
    assert!(run_bool("typeof Symbol.iterator === 'symbol'"));
    assert!(run_bool("typeof Symbol.for('x') === 'symbol'"));
}

// ── Symbol.prototype.toString ──────────────────────────────────────────────

#[test]
fn symbol_to_string_produces_descriptive_string() {
    assert_eq!(run_string("Symbol('otter').toString()"), "Symbol(otter)");
    assert_eq!(run_string("Symbol().toString()"), "Symbol()");
}
