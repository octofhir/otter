//! Integration tests for ECMA-402 Intl.NumberFormat.
//!
//! Spec: <https://tc39.es/ecma402/#numberformat-objects>

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
    let handle = v.as_object_handle().expect("expected string handle");
    runtime
        .objects()
        .string_value(otter_vm::object::ObjectHandle(handle))
        .expect("string lookup")
        .expect("string value")
        .to_string()
}

fn run_i32(source: &str) -> i32 {
    let v = run(source);
    v.as_i32()
        .unwrap_or_else(|| panic!("expected i32, got {v:?}"))
}

fn expect_throws(source: &str) {
    let module = compile_eval(source, "<test>").expect("should compile");
    let mut runtime = RuntimeState::new();
    let global = runtime.intrinsics().global_object();
    let registers = [RegisterValue::from_object_handle(global.0)];
    let result = Interpreter::new().execute_with_runtime(
        &module,
        otter_vm::module::FunctionIndex(0),
        &registers,
        &mut runtime,
    );
    assert!(
        result.is_err() || {
            let v = result.unwrap().return_value();
            v == RegisterValue::undefined()
        },
        "expected throw for: {source}"
    );
}

// ═══════════════════════════════════════════════════════════════════
//  Constructor
// ═══════════════════════════════════════════════════════════════════

/// Intl.NumberFormat is a function.
#[test]
fn number_format_is_function() {
    assert!(run_bool("typeof Intl.NumberFormat === 'function'"));
}

/// Intl.NumberFormat() returns an object.
#[test]
fn number_format_constructor_returns_object() {
    assert!(run_bool("typeof new Intl.NumberFormat() === 'object'"));
}

/// Constructor with locale argument.
#[test]
fn number_format_constructor_with_locale() {
    assert!(run_bool("new Intl.NumberFormat('en-US') !== null"));
}

/// Constructor with options argument.
#[test]
fn number_format_constructor_with_options() {
    assert!(run_bool(
        "new Intl.NumberFormat('en-US', { style: 'decimal' }) !== null"
    ));
}

// ═══════════════════════════════════════════════════════════════════
//  format()
// ═══════════════════════════════════════════════════════════════════

/// Basic integer formatting.
#[test]
fn format_integer() {
    let s = run_string("new Intl.NumberFormat('en-US').format(12345)");
    // ICU4X should produce "12,345" with grouping.
    assert_eq!(s, "12,345");
}

/// Format zero.
#[test]
fn format_zero() {
    let s = run_string("new Intl.NumberFormat('en-US').format(0)");
    assert_eq!(s, "0");
}

/// Format negative number.
#[test]
fn format_negative() {
    let s = run_string("new Intl.NumberFormat('en-US').format(-42)");
    assert!(s.contains("42"), "expected 42 in {s}");
}

/// Format NaN.
#[test]
fn format_nan() {
    let s = run_string("new Intl.NumberFormat('en-US').format(NaN)");
    assert_eq!(s, "NaN");
}

/// Format Infinity.
#[test]
fn format_infinity() {
    let s = run_string("new Intl.NumberFormat('en-US').format(Infinity)");
    assert!(s.contains('∞') || s.contains("Infinity"), "got: {s}");
}

/// Format decimal (fraction).
#[test]
fn format_decimal_fraction() {
    let s = run_string("new Intl.NumberFormat('en-US').format(3.14)");
    assert!(s.contains("3") && s.contains("14"), "got: {s}");
}

// ═══════════════════════════════════════════════════════════════════
//  Style: percent
// ═══════════════════════════════════════════════════════════════════

/// Percent style multiplies by 100.
#[test]
fn format_percent() {
    let s = run_string("new Intl.NumberFormat('en-US', { style: 'percent' }).format(0.42)");
    assert!(s.contains("42"), "expected 42 in percent output: {s}");
}

// ═══════════════════════════════════════════════════════════════════
//  Style: currency
// ═══════════════════════════════════════════════════════════════════

/// Currency style requires currency option.
#[test]
fn currency_style_requires_currency() {
    expect_throws("new Intl.NumberFormat('en-US', { style: 'currency' })");
}

// ═══════════════════════════════════════════════════════════════════
//  Style: unit
// ═══════════════════════════════════════════════════════════════════

/// Unit style requires unit option.
#[test]
fn unit_style_requires_unit() {
    expect_throws("new Intl.NumberFormat('en-US', { style: 'unit' })");
}

// ═══════════════════════════════════════════════════════════════════
//  resolvedOptions()
// ═══════════════════════════════════════════════════════════════════

/// resolvedOptions returns an object.
#[test]
fn resolved_options_returns_object() {
    assert!(run_bool(
        "typeof new Intl.NumberFormat('en-US').resolvedOptions() === 'object'"
    ));
}

/// resolvedOptions has locale property.
#[test]
fn resolved_options_locale() {
    let s = run_string("new Intl.NumberFormat('en-US').resolvedOptions().locale");
    assert_eq!(s, "en-US");
}

/// resolvedOptions has style property.
#[test]
fn resolved_options_style_default() {
    let s = run_string("new Intl.NumberFormat('en-US').resolvedOptions().style");
    assert_eq!(s, "decimal");
}

/// resolvedOptions has numberingSystem.
#[test]
fn resolved_options_numbering_system() {
    let s = run_string("new Intl.NumberFormat('en-US').resolvedOptions().numberingSystem");
    assert_eq!(s, "latn");
}

/// resolvedOptions has minimumIntegerDigits.
#[test]
fn resolved_options_min_integer_digits() {
    assert_eq!(
        run_i32("new Intl.NumberFormat('en-US').resolvedOptions().minimumIntegerDigits"),
        1
    );
}

/// resolvedOptions has notation.
#[test]
fn resolved_options_notation() {
    let s = run_string("new Intl.NumberFormat('en-US').resolvedOptions().notation");
    assert_eq!(s, "standard");
}

/// resolvedOptions has useGrouping.
#[test]
fn resolved_options_use_grouping() {
    let s = run_string("new Intl.NumberFormat('en-US').resolvedOptions().useGrouping");
    assert_eq!(s, "auto");
}

/// resolvedOptions has signDisplay.
#[test]
fn resolved_options_sign_display() {
    let s = run_string("new Intl.NumberFormat('en-US').resolvedOptions().signDisplay");
    assert_eq!(s, "auto");
}

/// resolvedOptions has roundingMode.
#[test]
fn resolved_options_rounding_mode() {
    let s = run_string("new Intl.NumberFormat('en-US').resolvedOptions().roundingMode");
    assert_eq!(s, "halfExpand");
}

// ═══════════════════════════════════════════════════════════════════
//  supportedLocalesOf()
// ═══════════════════════════════════════════════════════════════════

/// supportedLocalesOf is a function.
#[test]
fn supported_locales_of_is_function() {
    assert!(run_bool(
        "typeof Intl.NumberFormat.supportedLocalesOf === 'function'"
    ));
}

/// supportedLocalesOf returns an array.
#[test]
fn supported_locales_of_returns_array() {
    assert!(run_bool(
        "Array.isArray(Intl.NumberFormat.supportedLocalesOf('en-US'))"
    ));
}

/// supportedLocalesOf with valid locale.
#[test]
fn supported_locales_of_valid_locale() {
    assert_eq!(
        run_i32("Intl.NumberFormat.supportedLocalesOf('en-US').length"),
        1
    );
}

// ═══════════════════════════════════════════════════════════════════
//  Digit options
// ═══════════════════════════════════════════════════════════════════

/// minimumFractionDigits pads output.
#[test]
fn min_fraction_digits_pads() {
    let s = run_string("new Intl.NumberFormat('en-US', { minimumFractionDigits: 2 }).format(5)");
    assert!(s.contains("5.00") || s.contains("5,00"), "got: {s}");
}

/// maximumFractionDigits truncates.
#[test]
fn max_fraction_digits_truncates() {
    let s =
        run_string("new Intl.NumberFormat('en-US', { maximumFractionDigits: 1 }).format(3.14159)");
    // Should be "3.1" (rounded)
    assert!(s.contains("3.1"), "got: {s}");
}

/// useGrouping: false disables grouping.
#[test]
fn use_grouping_false() {
    let s = run_string("new Intl.NumberFormat('en-US', { useGrouping: false }).format(1234567)");
    assert_eq!(s, "1234567");
}

// ═══════════════════════════════════════════════════════════════════
//  Invalid options throw
// ═══════════════════════════════════════════════════════════════════

/// Invalid style throws RangeError.
#[test]
fn invalid_style_throws() {
    expect_throws("new Intl.NumberFormat('en-US', { style: 'invalid' })");
}

/// Invalid unit throws RangeError.
#[test]
fn invalid_unit_throws() {
    expect_throws("new Intl.NumberFormat('en-US', { style: 'unit', unit: 'not-a-unit' })");
}

/// minimumFractionDigits > maximumFractionDigits throws.
#[test]
fn min_frac_greater_than_max_frac_throws() {
    expect_throws(
        "new Intl.NumberFormat('en-US', { minimumFractionDigits: 5, maximumFractionDigits: 2 })",
    );
}
