//! Integration tests for ECMA-402 Intl.Locale.
//!
//! Spec: <https://tc39.es/ecma402/#locale-objects>

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

#[test]
fn locale_is_function() {
    assert!(run_bool("typeof Intl.Locale === 'function'"));
}

#[test]
fn locale_constructor_basic() {
    assert!(run_bool("new Intl.Locale('en') instanceof Intl.Locale"));
}

#[test]
fn locale_constructor_with_region() {
    assert!(run_bool("new Intl.Locale('en-US') instanceof Intl.Locale"));
}

#[test]
fn locale_constructor_throws_on_undefined() {
    expect_throws("new Intl.Locale()");
}

// ═══════════════════════════════════════════════════════════════════
//  toString()
// ═══════════════════════════════════════════════════════════════════

#[test]
fn to_string_basic() {
    let result = run_string("new Intl.Locale('en').toString()");
    assert_eq!(result, "en");
}

#[test]
fn to_string_with_region() {
    let result = run_string("new Intl.Locale('en-US').toString()");
    assert_eq!(result, "en-US");
}

#[test]
fn to_string_with_script() {
    let result = run_string("new Intl.Locale('zh-Hans').toString()");
    assert_eq!(result, "zh-Hans");
}

// ═══════════════════════════════════════════════════════════════════
//  Accessor getters
// ═══════════════════════════════════════════════════════════════════

#[test]
fn language_getter() {
    assert_eq!(run_string("new Intl.Locale('en-US').language()"), "en");
}

#[test]
fn region_getter() {
    assert_eq!(run_string("new Intl.Locale('en-US').region()"), "US");
}

#[test]
fn script_getter() {
    assert_eq!(run_string("new Intl.Locale('zh-Hans-CN').script()"), "Hans");
}

#[test]
fn region_getter_with_script() {
    assert_eq!(run_string("new Intl.Locale('zh-Hans-CN').region()"), "CN");
}

#[test]
fn base_name_getter() {
    assert_eq!(run_string("new Intl.Locale('en-US').baseName()"), "en-US");
}

#[test]
fn region_undefined_when_absent() {
    assert!(run_bool("new Intl.Locale('en').region() === undefined"));
}

#[test]
fn script_undefined_when_absent() {
    assert!(run_bool("new Intl.Locale('en-US').script() === undefined"));
}

// ═══════════════════════════════════════════════════════════════════
//  maximize() / minimize()
// ═══════════════════════════════════════════════════════════════════

#[test]
fn maximize_basic() {
    let result = run_string("new Intl.Locale('en').maximize().toString()");
    // en → en-Latn-US (likely subtags).
    assert!(
        result.contains("Latn") && result.contains("US"),
        "maximize('en') should produce 'en-Latn-US', got: {result}"
    );
}

#[test]
fn minimize_basic() {
    let result = run_string("new Intl.Locale('en-Latn-US').minimize().toString()");
    assert_eq!(result, "en");
}

// ═══════════════════════════════════════════════════════════════════
//  Options: calendar, collation, numberingSystem
// ═══════════════════════════════════════════════════════════════════

#[test]
fn calendar_option() {
    assert_eq!(
        run_string("new Intl.Locale('en', { calendar: 'islamic' }).calendar()"),
        "islamic"
    );
}

#[test]
fn collation_option() {
    assert_eq!(
        run_string("new Intl.Locale('en', { collation: 'phonebk' }).collation()"),
        "phonebk"
    );
}

#[test]
fn numbering_system_option() {
    assert_eq!(
        run_string("new Intl.Locale('en', { numberingSystem: 'arab' }).numberingSystem()"),
        "arab"
    );
}

#[test]
fn calendar_undefined_when_absent() {
    assert!(run_bool("new Intl.Locale('en').calendar() === undefined"));
}
