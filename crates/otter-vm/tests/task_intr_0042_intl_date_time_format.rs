//! Integration tests for ECMA-402 Intl.DateTimeFormat.
//!
//! Spec: <https://tc39.es/ecma402/#datetimeformat-objects>

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

// ═══════════════════════════════════════════════════════════════════
//  Constructor
// ═══════════════════════════════════════════════════════════════════

#[test]
fn date_time_format_is_function() {
    assert!(run_bool("typeof Intl.DateTimeFormat === 'function'"));
}

#[test]
fn date_time_format_constructor_no_args() {
    assert!(run_bool(
        "new Intl.DateTimeFormat() instanceof Intl.DateTimeFormat"
    ));
}

#[test]
fn date_time_format_constructor_with_locale() {
    assert!(run_bool(
        "new Intl.DateTimeFormat('en-US') instanceof Intl.DateTimeFormat"
    ));
}

// ═══════════════════════════════════════════════════════════════════
//  format()
// ═══════════════════════════════════════════════════════════════════

#[test]
fn format_known_date() {
    // 2024-01-15T00:00:00Z = 1705276800000 ms.
    let result =
        run_string("new Intl.DateTimeFormat('en-US', { timeZone: 'UTC' }).format(1705276800000)");
    // Should contain "1" and "15" and "2024" in some form.
    assert!(
        result.contains("1") && result.contains("15") && result.contains("2024"),
        "format(2024-01-15) returned: {result}"
    );
}

#[test]
fn format_returns_string() {
    assert!(run_bool(
        "typeof new Intl.DateTimeFormat('en').format(0) === 'string'"
    ));
}

#[test]
fn format_epoch_zero() {
    // 1970-01-01 UTC.
    let result = run_string("new Intl.DateTimeFormat('en-US', { timeZone: 'UTC' }).format(0)");
    assert!(
        result.contains("1970") || result.contains("1"),
        "format(0) = epoch should include 1970 or 1: {result}"
    );
}

// ═══════════════════════════════════════════════════════════════════
//  format() with dateStyle / timeStyle
// ═══════════════════════════════════════════════════════════════════

#[test]
fn format_date_style_short() {
    let result = run_string(
        "new Intl.DateTimeFormat('en-US', { dateStyle: 'short', timeZone: 'UTC' }).format(1705276800000)",
    );
    assert!(
        !result.is_empty(),
        "dateStyle short should produce non-empty: {result}"
    );
}

#[test]
fn format_time_style_short() {
    let result = run_string(
        "new Intl.DateTimeFormat('en-US', { timeStyle: 'short', timeZone: 'UTC' }).format(1705276800000)",
    );
    assert!(
        !result.is_empty(),
        "timeStyle short should produce non-empty: {result}"
    );
}

// ═══════════════════════════════════════════════════════════════════
//  formatToParts()
// ═══════════════════════════════════════════════════════════════════

#[test]
fn format_to_parts_returns_array() {
    assert!(run_bool(
        "Array.isArray(new Intl.DateTimeFormat('en-US', { timeZone: 'UTC' }).formatToParts(0))"
    ));
}

#[test]
fn format_to_parts_has_type_and_value() {
    assert!(run_bool(
        "var p = new Intl.DateTimeFormat('en-US', { timeZone: 'UTC' }).formatToParts(0); \
         p.length > 0 && typeof p[0].type === 'string' && typeof p[0].value === 'string'"
    ));
}

// ═══════════════════════════════════════════════════════════════════
//  resolvedOptions()
// ═══════════════════════════════════════════════════════════════════

#[test]
fn resolved_options_locale() {
    let locale = run_string("new Intl.DateTimeFormat('en').resolvedOptions().locale");
    assert!(
        locale.starts_with("en"),
        "locale should start with 'en', got: {locale}"
    );
}

#[test]
fn resolved_options_calendar() {
    let cal = run_string("new Intl.DateTimeFormat('en').resolvedOptions().calendar");
    assert_eq!(cal, "gregory");
}

#[test]
fn resolved_options_time_zone() {
    let tz =
        run_string("new Intl.DateTimeFormat('en', { timeZone: 'UTC' }).resolvedOptions().timeZone");
    assert_eq!(tz, "UTC");
}

// ═══════════════════════════════════════════════════════════════════
//  supportedLocalesOf()
// ═══════════════════════════════════════════════════════════════════

#[test]
fn supported_locales_of_returns_array() {
    assert!(run_bool(
        "Array.isArray(Intl.DateTimeFormat.supportedLocalesOf(['en']))"
    ));
}

#[test]
fn supported_locales_of_en() {
    let result = run_string("Intl.DateTimeFormat.supportedLocalesOf(['en'])[0]");
    assert!(result.starts_with("en"), "got: {result}");
}
