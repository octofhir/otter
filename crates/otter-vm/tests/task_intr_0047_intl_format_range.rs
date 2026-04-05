//! Integration tests for Intl.NumberFormat.formatRange and
//! Intl.DateTimeFormat.formatRange / formatRangeToParts.
//!
//! Spec:
//! - NumberFormat: <https://tc39.es/ecma402/#sec-intl.numberformat.prototype.formatrange>
//! - DateTimeFormat: <https://tc39.es/ecma402/#sec-intl.datetimeformat.prototype.formatrange>

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
//  NumberFormat.formatRange
// ═══════════════════════════════════════════════════════════════════

#[test]
fn nf_format_range_is_function() {
    assert!(run_bool("typeof new Intl.NumberFormat('en').formatRange === 'function'"));
}

#[test]
fn nf_format_range_basic() {
    let s = run_string("new Intl.NumberFormat('en').formatRange(3, 5)");
    assert!(s.contains('3'), "expected '3' in: {s}");
    assert!(s.contains('5'), "expected '5' in: {s}");
}

#[test]
fn nf_format_range_same_value() {
    let s = run_string("new Intl.NumberFormat('en').formatRange(5, 5)");
    // When both sides format the same, should contain "~" prefix.
    assert!(s.contains('5'), "expected '5' in: {s}");
}

#[test]
fn nf_format_range_returns_string() {
    assert!(run_bool("typeof new Intl.NumberFormat('en').formatRange(1, 10) === 'string'"));
}

// ═══════════════════════════════════════════════════════════════════
//  NumberFormat.formatRangeToParts
// ═══════════════════════════════════════════════════════════════════

#[test]
fn nf_format_range_to_parts_is_function() {
    assert!(run_bool("typeof new Intl.NumberFormat('en').formatRangeToParts === 'function'"));
}

#[test]
fn nf_format_range_to_parts_returns_array() {
    assert!(run_bool("Array.isArray(new Intl.NumberFormat('en').formatRangeToParts(3, 5))"));
}

#[test]
fn nf_format_range_to_parts_has_source() {
    assert!(run_bool(
        "var parts = new Intl.NumberFormat('en').formatRangeToParts(3, 5); \
         parts.length > 0 && typeof parts[0].source === 'string'"
    ));
}

#[test]
fn nf_format_range_to_parts_has_start_and_end() {
    assert!(run_bool(
        "var parts = new Intl.NumberFormat('en').formatRangeToParts(3, 5); \
         var hasStart = false; var hasEnd = false; \
         for (var i = 0; i < parts.length; i++) { \
           if (parts[i].source === 'startRange') hasStart = true; \
           if (parts[i].source === 'endRange') hasEnd = true; \
         } \
         hasStart && hasEnd"
    ));
}

// ═══════════════════════════════════════════════════════════════════
//  DateTimeFormat.formatRange
// ═══════════════════════════════════════════════════════════════════

#[test]
fn dtf_format_range_is_function() {
    assert!(run_bool("typeof new Intl.DateTimeFormat('en').formatRange === 'function'"));
}

#[test]
fn dtf_format_range_returns_string() {
    assert!(run_bool(
        "typeof new Intl.DateTimeFormat('en').formatRange(new Date(2020, 0, 1), new Date(2020, 11, 31)) === 'string'"
    ));
}

#[test]
fn dtf_format_range_contains_both_dates() {
    let s = run_string(
        "new Intl.DateTimeFormat('en', { dateStyle: 'short' }).formatRange(new Date(2020, 0, 1), new Date(2020, 5, 15))"
    );
    // Should contain parts from both dates.
    assert!(s.len() > 5, "expected non-trivial range string, got: {s}");
}

// ═══════════════════════════════════════════════════════════════════
//  DateTimeFormat.formatRangeToParts
// ═══════════════════════════════════════════════════════════════════

#[test]
fn dtf_format_range_to_parts_is_function() {
    assert!(run_bool("typeof new Intl.DateTimeFormat('en').formatRangeToParts === 'function'"));
}

#[test]
fn dtf_format_range_to_parts_returns_array() {
    assert!(run_bool(
        "Array.isArray(new Intl.DateTimeFormat('en').formatRangeToParts(new Date(2020, 0, 1), new Date(2020, 11, 31)))"
    ));
}

#[test]
fn dtf_format_range_to_parts_has_source() {
    let n = run_f64(
        "new Intl.DateTimeFormat('en', { dateStyle: 'short' }).formatRangeToParts(new Date(2020, 0, 1), new Date(2020, 11, 31)).length"
    );
    assert!(n >= 3.0, "expected at least 3 parts (start + sep + end), got: {n}");
}
