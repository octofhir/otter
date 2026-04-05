//! Integration tests for ECMA-402 Intl.Collator.
//!
//! Spec: <https://tc39.es/ecma402/#collator-objects>

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

// ═══════════════════════════════════════════════════════════════════
//  Constructor
// ═══════════════════════════════════════════════════════════════════

#[test]
fn collator_is_function() {
    assert!(run_bool("typeof Intl.Collator === 'function'"));
}

#[test]
fn collator_constructor_no_args() {
    assert!(run_bool("new Intl.Collator() instanceof Intl.Collator"));
}

#[test]
fn collator_constructor_with_locale() {
    assert!(run_bool(
        "new Intl.Collator('en-US') instanceof Intl.Collator"
    ));
}

// ═══════════════════════════════════════════════════════════════════
//  compare()
// ═══════════════════════════════════════════════════════════════════

#[test]
fn compare_equal_strings() {
    assert_eq!(run_i32("new Intl.Collator().compare('abc', 'abc')"), 0);
}

#[test]
fn compare_less_than() {
    let result = run_i32("new Intl.Collator().compare('a', 'b')");
    assert!(result < 0, "expected negative, got {result}");
}

#[test]
fn compare_greater_than() {
    let result = run_i32("new Intl.Collator().compare('b', 'a')");
    assert!(result > 0, "expected positive, got {result}");
}

#[test]
fn compare_case_insensitive_base() {
    assert_eq!(
        run_i32("new Intl.Collator('en', { sensitivity: 'base' }).compare('a', 'A')"),
        0
    );
}

#[test]
fn compare_accent_sensitivity() {
    // 'a' and 'á' differ at accent level.
    let result = run_i32("new Intl.Collator('en', { sensitivity: 'base' }).compare('a', 'á')");
    assert_eq!(result, 0, "base sensitivity should ignore accents");
}

#[test]
fn compare_accent_sensitive() {
    let result = run_i32("new Intl.Collator('en', { sensitivity: 'accent' }).compare('a', 'á')");
    assert_ne!(result, 0, "accent sensitivity should distinguish accents");
}

// ═══════════════════════════════════════════════════════════════════
//  resolvedOptions()
// ═══════════════════════════════════════════════════════════════════

#[test]
fn resolved_options_locale() {
    let locale = run_string("new Intl.Collator('en').resolvedOptions().locale");
    assert!(
        locale.starts_with("en"),
        "locale should start with 'en', got: {locale}"
    );
}

#[test]
fn resolved_options_usage() {
    assert_eq!(
        run_string("new Intl.Collator('en').resolvedOptions().usage"),
        "sort"
    );
}

#[test]
fn resolved_options_sensitivity() {
    assert_eq!(
        run_string("new Intl.Collator('en').resolvedOptions().sensitivity"),
        "variant"
    );
}

#[test]
fn resolved_options_custom_sensitivity() {
    assert_eq!(
        run_string(
            "new Intl.Collator('en', { sensitivity: 'base' }).resolvedOptions().sensitivity"
        ),
        "base"
    );
}

#[test]
fn resolved_options_collation() {
    assert_eq!(
        run_string("new Intl.Collator('en').resolvedOptions().collation"),
        "default"
    );
}

#[test]
fn resolved_options_case_first() {
    assert_eq!(
        run_string("new Intl.Collator('en').resolvedOptions().caseFirst"),
        "false"
    );
}

// ═══════════════════════════════════════════════════════════════════
//  supportedLocalesOf()
// ═══════════════════════════════════════════════════════════════════

#[test]
fn supported_locales_of_returns_array() {
    assert!(run_bool(
        "Array.isArray(Intl.Collator.supportedLocalesOf(['en']))"
    ));
}

#[test]
fn supported_locales_of_en() {
    let result = run_string("Intl.Collator.supportedLocalesOf(['en'])[0]");
    assert!(result.starts_with("en"), "got: {result}");
}
