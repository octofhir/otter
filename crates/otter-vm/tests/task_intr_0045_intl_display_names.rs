//! Integration tests for ECMA-402 Intl.DisplayNames.
//!
//! Spec: <https://tc39.es/ecma402/#displaynames-objects>

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
fn display_names_is_function() {
    assert!(run_bool("typeof Intl.DisplayNames === 'function'"));
}

#[test]
fn display_names_constructor_with_type() {
    assert!(run_bool(
        "typeof new Intl.DisplayNames('en', { type: 'language' }) === 'object'"
    ));
}

// ── of() — language ──────────────────────────────────────────────

#[test]
fn of_language_en() {
    let s = run_string("new Intl.DisplayNames('en', { type: 'language' }).of('en')");
    assert_eq!(s, "English");
}

#[test]
fn of_language_fr() {
    let s = run_string("new Intl.DisplayNames('en', { type: 'language' }).of('fr')");
    assert_eq!(s, "French");
}

#[test]
fn of_language_de() {
    let s = run_string("new Intl.DisplayNames('en', { type: 'language' }).of('de')");
    assert_eq!(s, "German");
}

#[test]
fn of_language_ja() {
    let s = run_string("new Intl.DisplayNames('en', { type: 'language' }).of('ja')");
    assert_eq!(s, "Japanese");
}

// ── of() — region ────────────────────────────────────────────────

#[test]
fn of_region_us() {
    let s = run_string("new Intl.DisplayNames('en', { type: 'region' }).of('US')");
    assert_eq!(s, "United States");
}

#[test]
fn of_region_gb() {
    let s = run_string("new Intl.DisplayNames('en', { type: 'region' }).of('GB')");
    assert_eq!(s, "United Kingdom");
}

// ── of() — currency ──────────────────────────────────────────────

#[test]
fn of_currency_usd() {
    let s = run_string("new Intl.DisplayNames('en', { type: 'currency' }).of('USD')");
    assert_eq!(s, "US Dollar");
}

#[test]
fn of_currency_eur() {
    let s = run_string("new Intl.DisplayNames('en', { type: 'currency' }).of('EUR')");
    assert_eq!(s, "Euro");
}

// ── of() — script ────────────────────────────────────────────────

#[test]
fn of_script_latn() {
    let s = run_string("new Intl.DisplayNames('en', { type: 'script' }).of('Latn')");
    assert_eq!(s, "Latin");
}

// ── of() — fallback code ─────────────────────────────────────────

#[test]
fn of_unknown_code_fallback_code() {
    let s = run_string(
        "new Intl.DisplayNames('en', { type: 'language', fallback: 'code' }).of('zzzz')",
    );
    assert_eq!(s, "zzzz");
}

#[test]
fn of_unknown_code_fallback_none() {
    assert!(run_bool(
        "new Intl.DisplayNames('en', { type: 'language', fallback: 'none' }).of('zzzz') === undefined"
    ));
}

// ── resolvedOptions() ────────────────────────────────────────────

#[test]
fn resolved_options_locale() {
    let s =
        run_string("new Intl.DisplayNames('en', { type: 'language' }).resolvedOptions().locale");
    assert!(
        s.starts_with("en"),
        "expected locale starting with 'en', got: {s}"
    );
}

#[test]
fn resolved_options_type() {
    let s = run_string("new Intl.DisplayNames('en', { type: 'region' }).resolvedOptions().type");
    assert_eq!(s, "region");
}

#[test]
fn resolved_options_style_default() {
    let s = run_string("new Intl.DisplayNames('en', { type: 'language' }).resolvedOptions().style");
    assert_eq!(s, "long");
}

#[test]
fn resolved_options_fallback_default() {
    let s =
        run_string("new Intl.DisplayNames('en', { type: 'language' }).resolvedOptions().fallback");
    assert_eq!(s, "code");
}

// ── supportedLocalesOf() ─────────────────────────────────────────

#[test]
fn supported_locales_of_returns_array() {
    assert!(run_bool(
        "Array.isArray(Intl.DisplayNames.supportedLocalesOf('en'))"
    ));
}
