//! Integration tests for ECMA-402 Intl.ListFormat.
//!
//! Spec: <https://tc39.es/ecma402/#listformat-objects>

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
fn list_format_is_function() {
    assert!(run_bool("typeof Intl.ListFormat === 'function'"));
}

#[test]
fn list_format_constructor_no_args() {
    assert!(run_bool("typeof new Intl.ListFormat() === 'object'"));
}

#[test]
fn list_format_constructor_with_locale() {
    assert!(run_bool("typeof new Intl.ListFormat('en') === 'object'"));
}

// ── format() ─────────────────────────────────────────────────────

#[test]
fn format_conjunction_two_items() {
    let s = run_string("new Intl.ListFormat('en', { type: 'conjunction' }).format(['a', 'b'])");
    assert!(s.contains("a"), "expected 'a' in: {s}");
    assert!(s.contains("b"), "expected 'b' in: {s}");
    assert!(s.contains("and"), "expected 'and' in: {s}");
}

#[test]
fn format_conjunction_three_items() {
    let s = run_string("new Intl.ListFormat('en', { type: 'conjunction' }).format(['a', 'b', 'c'])");
    assert!(s.contains("a"), "expected 'a' in: {s}");
    assert!(s.contains("c"), "expected 'c' in: {s}");
}

#[test]
fn format_disjunction() {
    let s = run_string("new Intl.ListFormat('en', { type: 'disjunction' }).format(['a', 'b'])");
    assert!(s.contains("a"), "expected 'a' in: {s}");
    assert!(s.contains("b"), "expected 'b' in: {s}");
    assert!(s.contains("or"), "expected 'or' in: {s}");
}

#[test]
fn format_single_item() {
    let s = run_string("new Intl.ListFormat('en').format(['hello'])");
    assert_eq!(s, "hello");
}

#[test]
fn format_empty_array() {
    let s = run_string("new Intl.ListFormat('en').format([])");
    assert_eq!(s, "");
}

// ── formatToParts() ──────────────────────────────────────────────

#[test]
fn format_to_parts_returns_array() {
    assert!(run_bool("Array.isArray(new Intl.ListFormat('en').formatToParts(['a', 'b']))"));
}

#[test]
fn format_to_parts_has_type_and_value() {
    assert!(run_bool(
        "var parts = new Intl.ListFormat('en').formatToParts(['a', 'b']); \
         parts.length > 0 && typeof parts[0].type === 'string' && typeof parts[0].value === 'string'"
    ));
}

// ── resolvedOptions() ────────────────────────────────────────────

#[test]
fn resolved_options_locale() {
    let s = run_string("new Intl.ListFormat('en').resolvedOptions().locale");
    assert!(s.starts_with("en"), "expected locale starting with 'en', got: {s}");
}

#[test]
fn resolved_options_type() {
    let s = run_string("new Intl.ListFormat('en', { type: 'disjunction' }).resolvedOptions().type");
    assert_eq!(s, "disjunction");
}

#[test]
fn resolved_options_style_default() {
    let s = run_string("new Intl.ListFormat('en').resolvedOptions().style");
    assert_eq!(s, "long");
}

#[test]
fn resolved_options_style_short() {
    let s = run_string("new Intl.ListFormat('en', { style: 'short' }).resolvedOptions().style");
    assert_eq!(s, "short");
}

// ── supportedLocalesOf() ─────────────────────────────────────────

#[test]
fn supported_locales_of_returns_array() {
    assert!(run_bool("Array.isArray(Intl.ListFormat.supportedLocalesOf('en'))"));
}

#[test]
fn supported_locales_of_en() {
    assert!(run_bool("Intl.ListFormat.supportedLocalesOf('en').length >= 1"));
}
