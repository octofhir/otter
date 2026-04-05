//! Integration tests for ECMA-402 Intl.PluralRules.
//!
//! Spec: <https://tc39.es/ecma402/#pluralrules-objects>

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
fn plural_rules_is_function() {
    assert!(run_bool("typeof Intl.PluralRules === 'function'"));
}

#[test]
fn plural_rules_constructor_no_args() {
    assert!(run_bool("new Intl.PluralRules() instanceof Intl.PluralRules"));
}

// ═══════════════════════════════════════════════════════════════════
//  select()
// ═══════════════════════════════════════════════════════════════════

#[test]
fn select_one_english() {
    assert_eq!(
        run_string("new Intl.PluralRules('en').select(1)"),
        "one"
    );
}

#[test]
fn select_other_english() {
    assert_eq!(
        run_string("new Intl.PluralRules('en').select(0)"),
        "other"
    );
}

#[test]
fn select_two_english() {
    assert_eq!(
        run_string("new Intl.PluralRules('en').select(2)"),
        "other"
    );
}

#[test]
fn select_negative_one() {
    // In English, -1 is "one" per CLDR.
    assert_eq!(
        run_string("new Intl.PluralRules('en').select(-1)"),
        "one"
    );
}

#[test]
fn select_large_number() {
    assert_eq!(
        run_string("new Intl.PluralRules('en').select(100)"),
        "other"
    );
}

#[test]
fn select_fractional() {
    // 1.5 is "other" in English.
    assert_eq!(
        run_string("new Intl.PluralRules('en').select(1.5)"),
        "other"
    );
}

// ═══════════════════════════════════════════════════════════════════
//  select() with ordinal type
// ═══════════════════════════════════════════════════════════════════

#[test]
fn select_ordinal_one() {
    // English ordinal: 1 → "one" (1st).
    assert_eq!(
        run_string("new Intl.PluralRules('en', { type: 'ordinal' }).select(1)"),
        "one"
    );
}

#[test]
fn select_ordinal_two() {
    // English ordinal: 2 → "two" (2nd).
    assert_eq!(
        run_string("new Intl.PluralRules('en', { type: 'ordinal' }).select(2)"),
        "two"
    );
}

#[test]
fn select_ordinal_three() {
    // English ordinal: 3 → "few" (3rd).
    assert_eq!(
        run_string("new Intl.PluralRules('en', { type: 'ordinal' }).select(3)"),
        "few"
    );
}

#[test]
fn select_ordinal_four() {
    // English ordinal: 4 → "other" (4th).
    assert_eq!(
        run_string("new Intl.PluralRules('en', { type: 'ordinal' }).select(4)"),
        "other"
    );
}

// ═══════════════════════════════════════════════════════════════════
//  resolvedOptions()
// ═══════════════════════════════════════════════════════════════════

#[test]
fn resolved_options_locale() {
    let locale = run_string("new Intl.PluralRules('en').resolvedOptions().locale");
    assert!(locale.starts_with("en"), "got: {locale}");
}

#[test]
fn resolved_options_type_cardinal() {
    assert_eq!(
        run_string("new Intl.PluralRules('en').resolvedOptions().type"),
        "cardinal"
    );
}

#[test]
fn resolved_options_type_ordinal() {
    assert_eq!(
        run_string("new Intl.PluralRules('en', { type: 'ordinal' }).resolvedOptions().type"),
        "ordinal"
    );
}

#[test]
fn resolved_options_plural_categories() {
    // English cardinal has ["one", "other"].
    assert!(run_bool(
        "var cats = new Intl.PluralRules('en').resolvedOptions().pluralCategories; \
         Array.isArray(cats) && cats.length >= 2"
    ));
}

// ═══════════════════════════════════════════════════════════════════
//  supportedLocalesOf()
// ═══════════════════════════════════════════════════════════════════

#[test]
fn supported_locales_of_returns_array() {
    assert!(run_bool("Array.isArray(Intl.PluralRules.supportedLocalesOf(['en']))"));
}

// ═══════════════════════════════════════════════════════════════════
//  selectRange()
// ═══════════════════════════════════════════════════════════════════

#[test]
fn select_range_basic() {
    // selectRange(1, 2) in English should return "other".
    let result = run_string("new Intl.PluralRules('en').selectRange(1, 2)");
    assert!(
        result == "other" || result == "one",
        "selectRange(1,2) returned: {result}"
    );
}
