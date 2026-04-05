//! Integration tests for ECMA-402 Intl.RelativeTimeFormat.
//!
//! Spec: <https://tc39.es/ecma402/#relativetimeformat-objects>

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

// ── Constructor ──────────────────────────────────────────────────

#[test]
fn rtf_is_function() {
    assert!(run_bool("typeof Intl.RelativeTimeFormat === 'function'"));
}

#[test]
fn rtf_constructor_no_args() {
    assert!(run_bool("typeof new Intl.RelativeTimeFormat() === 'object'"));
}

#[test]
fn rtf_constructor_with_locale() {
    assert!(run_bool("typeof new Intl.RelativeTimeFormat('en') === 'object'"));
}

// ── format() ─────────────────────────────────────────────────────

#[test]
fn format_past_day() {
    let s = run_string("new Intl.RelativeTimeFormat('en').format(-1, 'day')");
    // ICU4X produces "1 day ago"
    assert!(s.contains("1") && s.contains("day"), "expected '1 day ago'-like, got: {s}");
    assert!(s.contains("ago"), "expected 'ago' in: {s}");
}

#[test]
fn format_future_day() {
    let s = run_string("new Intl.RelativeTimeFormat('en').format(1, 'day')");
    assert!(s.contains("1") && s.contains("day"), "expected 'in 1 day'-like, got: {s}");
    assert!(s.contains("in"), "expected 'in' in: {s}");
}

#[test]
fn format_past_years() {
    let s = run_string("new Intl.RelativeTimeFormat('en').format(-3, 'year')");
    assert!(s.contains("3"), "expected '3' in: {s}");
    assert!(s.contains("year"), "expected 'year' in: {s}");
    assert!(s.contains("ago"), "expected 'ago' in: {s}");
}

#[test]
fn format_future_hours() {
    let s = run_string("new Intl.RelativeTimeFormat('en').format(5, 'hour')");
    assert!(s.contains("5"), "expected '5' in: {s}");
    assert!(s.contains("hour"), "expected 'hour' in: {s}");
}

#[test]
fn format_zero_seconds() {
    let s = run_string("new Intl.RelativeTimeFormat('en').format(0, 'second')");
    assert!(s.contains("0") || s.contains("second"), "expected something with seconds, got: {s}");
}

// ── formatToParts() ──────────────────────────────────────────────

#[test]
fn format_to_parts_returns_array() {
    assert!(run_bool("Array.isArray(new Intl.RelativeTimeFormat('en').formatToParts(-1, 'day'))"));
}

#[test]
fn format_to_parts_has_type_and_value() {
    assert!(run_bool(
        "var parts = new Intl.RelativeTimeFormat('en').formatToParts(-1, 'day'); \
         parts.length > 0 && typeof parts[0].type === 'string' && typeof parts[0].value === 'string'"
    ));
}

// ── resolvedOptions() ────────────────────────────────────────────

#[test]
fn resolved_options_locale() {
    let s = run_string("new Intl.RelativeTimeFormat('en').resolvedOptions().locale");
    assert!(s.starts_with("en"), "expected locale starting with 'en', got: {s}");
}

#[test]
fn resolved_options_style_default() {
    let s = run_string("new Intl.RelativeTimeFormat('en').resolvedOptions().style");
    assert_eq!(s, "long");
}

#[test]
fn resolved_options_numeric_default() {
    let s = run_string("new Intl.RelativeTimeFormat('en').resolvedOptions().numeric");
    assert_eq!(s, "always");
}

#[test]
fn resolved_options_style_short() {
    let s = run_string("new Intl.RelativeTimeFormat('en', { style: 'short' }).resolvedOptions().style");
    assert_eq!(s, "short");
}

#[test]
fn resolved_options_numeric_auto() {
    let s = run_string("new Intl.RelativeTimeFormat('en', { numeric: 'auto' }).resolvedOptions().numeric");
    assert_eq!(s, "auto");
}

#[test]
fn resolved_options_numbering_system() {
    let s = run_string("new Intl.RelativeTimeFormat('en').resolvedOptions().numberingSystem");
    assert_eq!(s, "latn");
}

// ── supportedLocalesOf() ─────────────────────────────────────────

#[test]
fn supported_locales_of_returns_array() {
    assert!(run_bool("Array.isArray(Intl.RelativeTimeFormat.supportedLocalesOf('en'))"));
}

#[test]
fn supported_locales_of_en() {
    assert!(run_bool("Intl.RelativeTimeFormat.supportedLocalesOf('en').length >= 1"));
}
