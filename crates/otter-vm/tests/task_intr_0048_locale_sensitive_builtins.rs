//! Integration tests for ECMA-402 locale-sensitive built-in prototype methods.
//!
//! §18: String.prototype.localeCompare, toLocaleLowerCase, toLocaleUpperCase
//! §19: Number.prototype.toLocaleString
//! §20: Date.prototype.toLocaleString, toLocaleDateString, toLocaleTimeString
//! §23.1.3.29: Array.prototype.toLocaleString

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
    let h = v
        .as_object_handle()
        .map(otter_vm::object::ObjectHandle)
        .expect("expected string object");
    runtime
        .objects()
        .string_value(h)
        .ok()
        .flatten()
        .expect("expected string")
        .to_string()
}

fn run_f64(source: &str) -> f64 {
    let v = run(source);
    v.as_number().unwrap_or_else(|| panic!("expected number, got {v:?}"))
}

// ═══════════════════════════════════════════════════════════════════
//  String.prototype.localeCompare
// ═══════════════════════════════════════════════════════════════════

#[test]
fn locale_compare_equal() {
    let n = run_f64("'a'.localeCompare('a')");
    assert_eq!(n, 0.0);
}

#[test]
fn locale_compare_less() {
    let n = run_f64("'a'.localeCompare('b')");
    assert!(n < 0.0, "expected negative, got: {n}");
}

#[test]
fn locale_compare_greater() {
    let n = run_f64("'b'.localeCompare('a')");
    assert!(n > 0.0, "expected positive, got: {n}");
}

#[test]
fn locale_compare_with_locale() {
    // Swedish: ä sorts after z.
    let n = run_f64("'ä'.localeCompare('z', 'sv')");
    assert!(n > 0.0, "in Swedish, ä should sort after z, got: {n}");
}

#[test]
fn locale_compare_returns_number() {
    assert!(run_bool("typeof 'a'.localeCompare('b') === 'number'"));
}

// ═══════════════════════════════════════════════════════════════════
//  String.prototype.toLocaleLowerCase
// ═══════════════════════════════════════════════════════════════════

#[test]
fn to_locale_lower_case_basic() {
    let s = run_string("'HELLO'.toLocaleLowerCase()");
    assert_eq!(s, "hello");
}

#[test]
fn to_locale_lower_case_already_lower() {
    let s = run_string("'hello'.toLocaleLowerCase()");
    assert_eq!(s, "hello");
}

#[test]
fn to_locale_lower_case_turkic() {
    // Turkish: I → ı (dotless i), not i
    let s = run_string("'I'.toLocaleLowerCase('tr')");
    assert_eq!(s, "ı");
}

// ═══════════════════════════════════════════════════════════════════
//  String.prototype.toLocaleUpperCase
// ═══════════════════════════════════════════════════════════════════

#[test]
fn to_locale_upper_case_basic() {
    let s = run_string("'hello'.toLocaleUpperCase()");
    assert_eq!(s, "HELLO");
}

#[test]
fn to_locale_upper_case_already_upper() {
    let s = run_string("'HELLO'.toLocaleUpperCase()");
    assert_eq!(s, "HELLO");
}

#[test]
fn to_locale_upper_case_turkic() {
    // Turkish: i → İ (dotted I), not I
    let s = run_string("'i'.toLocaleUpperCase('tr')");
    assert_eq!(s, "\u{130}"); // İ
}

// ═══════════════════════════════════════════════════════════════════
//  Number.prototype.toLocaleString
// ═══════════════════════════════════════════════════════════════════

#[test]
fn number_to_locale_string_basic() {
    let s = run_string("(1234).toLocaleString()");
    // ICU4X default locale should produce at least the digits.
    assert!(s.contains("1"), "expected '1' in: {s}");
    assert!(s.contains("234"), "expected '234' in: {s}");
}

#[test]
fn number_to_locale_string_nan() {
    let s = run_string("NaN.toLocaleString()");
    assert_eq!(s, "NaN");
}

#[test]
fn number_to_locale_string_infinity() {
    let s = run_string("Infinity.toLocaleString()");
    assert_eq!(s, "Infinity");
}

#[test]
fn number_to_locale_string_returns_string() {
    assert!(run_bool("typeof (42).toLocaleString() === 'string'"));
}

// ═══════════════════════════════════════════════════════════════════
//  Date.prototype.toLocaleString / toLocaleDateString / toLocaleTimeString
// ═══════════════════════════════════════════════════════════════════

#[test]
fn date_to_locale_string_returns_string() {
    assert!(run_bool("typeof new Date(0).toLocaleString() === 'string'"));
}

#[test]
fn date_to_locale_date_string_returns_string() {
    assert!(run_bool("typeof new Date(0).toLocaleDateString() === 'string'"));
}

#[test]
fn date_to_locale_time_string_returns_string() {
    assert!(run_bool("typeof new Date(0).toLocaleTimeString() === 'string'"));
}

#[test]
fn date_to_locale_string_is_function() {
    assert!(run_bool("typeof Date.prototype.toLocaleString === 'function'"));
}

#[test]
fn date_to_locale_date_string_is_function() {
    assert!(run_bool("typeof Date.prototype.toLocaleDateString === 'function'"));
}

#[test]
fn date_to_locale_time_string_is_function() {
    assert!(run_bool("typeof Date.prototype.toLocaleTimeString === 'function'"));
}

// ═══════════════════════════════════════════════════════════════════
//  Array.prototype.toLocaleString
// ═══════════════════════════════════════════════════════════════════

#[test]
fn array_to_locale_string_is_function() {
    assert!(run_bool("typeof Array.prototype.toLocaleString === 'function'"));
}

#[test]
fn array_to_locale_string_empty() {
    let s = run_string("[].toLocaleString()");
    assert_eq!(s, "");
}

#[test]
fn array_to_locale_string_single_element() {
    let s = run_string("[42].toLocaleString()");
    // Should contain "42" (possibly formatted with locale)
    assert!(s.contains("42"), "expected '42' in: {s}");
}

#[test]
fn array_to_locale_string_multiple_elements() {
    let s = run_string("[1, 2, 3].toLocaleString()");
    // Elements separated by commas; each element calls toLocaleString
    assert!(s.contains("1"), "expected '1' in: {s}");
    assert!(s.contains("2"), "expected '2' in: {s}");
    assert!(s.contains("3"), "expected '3' in: {s}");
}

#[test]
fn array_to_locale_string_null_undefined() {
    // null and undefined produce empty strings per spec
    let s = run_string("[1, null, undefined, 2].toLocaleString()");
    // Should be "1,,,2" (with locale formatting of 1 and 2)
    let parts: Vec<&str> = s.split(',').collect();
    assert_eq!(parts.len(), 4, "expected 4 parts, got: {s}");
    assert_eq!(parts[1], "", "null should produce empty string");
    assert_eq!(parts[2], "", "undefined should produce empty string");
}

#[test]
fn array_to_locale_string_strings() {
    let s = run_string("['a', 'b', 'c'].toLocaleString()");
    assert_eq!(s, "a,b,c");
}

#[test]
fn array_to_locale_string_returns_string() {
    assert!(run_bool("typeof [1,2,3].toLocaleString() === 'string'"));
}

// ═══════════════════════════════════════════════════════════════════
//  BigInt.prototype.toLocaleString
// ═══════════════════════════════════════════════════════════════════

#[test]
fn bigint_to_locale_string_is_function() {
    assert!(run_bool("typeof BigInt.prototype.toLocaleString === 'function'"));
}

#[test]
fn bigint_to_locale_string_basic() {
    let s = run_string("BigInt(42).toLocaleString()");
    assert!(s.contains("42"), "expected '42' in: {s}");
}

#[test]
fn bigint_to_locale_string_zero() {
    let s = run_string("BigInt(0).toLocaleString()");
    assert_eq!(s, "0");
}

#[test]
fn bigint_to_locale_string_negative() {
    let s = run_string("BigInt(-123).toLocaleString()");
    assert!(s.contains("123"), "expected '123' in: {s}");
}

#[test]
fn bigint_to_locale_string_returns_string() {
    assert!(run_bool("typeof BigInt(99).toLocaleString() === 'string'"));
}
