//! Integration tests for @@toStringTag on all Intl type prototypes.
//!
//! Each Intl type prototype should have Symbol.toStringTag returning "Intl.X".
//! Spec: ECMA-402, each type's §X.3.2.

use otter_vm::source::compile_eval;
use otter_vm::value::RegisterValue;
use otter_vm::{Interpreter, RuntimeState};

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

#[test]
fn collator_to_string_tag() {
    let s = run_string("Object.prototype.toString.call(new Intl.Collator())");
    assert_eq!(s, "[object Intl.Collator]");
}

#[test]
fn number_format_to_string_tag() {
    let s = run_string("Object.prototype.toString.call(new Intl.NumberFormat())");
    assert_eq!(s, "[object Intl.NumberFormat]");
}

#[test]
fn plural_rules_to_string_tag() {
    let s = run_string("Object.prototype.toString.call(new Intl.PluralRules())");
    assert_eq!(s, "[object Intl.PluralRules]");
}

#[test]
fn locale_to_string_tag() {
    let s = run_string("Object.prototype.toString.call(new Intl.Locale('en'))");
    assert_eq!(s, "[object Intl.Locale]");
}

#[test]
fn date_time_format_to_string_tag() {
    let s = run_string("Object.prototype.toString.call(new Intl.DateTimeFormat())");
    assert_eq!(s, "[object Intl.DateTimeFormat]");
}

#[test]
fn list_format_to_string_tag() {
    let s = run_string("Object.prototype.toString.call(new Intl.ListFormat())");
    assert_eq!(s, "[object Intl.ListFormat]");
}

#[test]
fn segmenter_to_string_tag() {
    let s = run_string("Object.prototype.toString.call(new Intl.Segmenter())");
    assert_eq!(s, "[object Intl.Segmenter]");
}

#[test]
fn display_names_to_string_tag() {
    let s = run_string(
        "Object.prototype.toString.call(new Intl.DisplayNames('en', { type: 'language' }))",
    );
    assert_eq!(s, "[object Intl.DisplayNames]");
}

#[test]
fn relative_time_format_to_string_tag() {
    let s = run_string("Object.prototype.toString.call(new Intl.RelativeTimeFormat())");
    assert_eq!(s, "[object Intl.RelativeTimeFormat]");
}

#[test]
fn intl_namespace_to_string_tag() {
    let s = run_string("Object.prototype.toString.call(Intl)");
    assert_eq!(s, "[object Intl]");
}
