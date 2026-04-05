//! Integration tests for ECMA-402 Intl namespace.
//!
//! Spec references:
//! - §8.3.1 Intl.getCanonicalLocales: <https://tc39.es/ecma402/#sec-intl.getcanonicallocales>
//! - §8.3.2 Intl.supportedValuesOf: <https://tc39.es/ecma402/#sec-intl.supportedvaluesof>

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

fn run_i32(source: &str) -> i32 {
    let v = run(source);
    v.as_i32()
        .unwrap_or_else(|| panic!("expected i32, got {v:?}"))
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
//  Intl namespace presence
// ═══════════════════════════════════════════════════════════════════

/// typeof Intl is 'object'.
#[test]
fn intl_typeof() {
    assert!(run_bool("typeof Intl === 'object'"));
}

/// Intl is not null.
#[test]
fn intl_is_not_null() {
    assert!(run_bool("Intl !== null"));
}

/// Intl[Symbol.toStringTag] is 'Intl'.
#[test]
fn intl_to_string_tag() {
    assert!(run_bool("Intl[Symbol.toStringTag] === 'Intl'"));
}

// ═══════════════════════════════════════════════════════════════════
//  §8.3.1 Intl.getCanonicalLocales
// ═══════════════════════════════════════════════════════════════════

/// getCanonicalLocales is a function.
#[test]
fn get_canonical_locales_is_function() {
    assert!(run_bool("typeof Intl.getCanonicalLocales === 'function'"));
}

/// getCanonicalLocales.length is 1.
#[test]
fn get_canonical_locales_length() {
    assert_eq!(run_i32("Intl.getCanonicalLocales.length"), 1);
}

/// getCanonicalLocales with undefined returns empty array.
#[test]
fn get_canonical_locales_undefined() {
    assert_eq!(run_i32("Intl.getCanonicalLocales(undefined).length"), 0);
}

/// getCanonicalLocales with no args returns empty array.
#[test]
fn get_canonical_locales_no_args() {
    assert_eq!(run_i32("Intl.getCanonicalLocales().length"), 0);
}

/// getCanonicalLocales with a string returns array of one.
#[test]
fn get_canonical_locales_single_string() {
    assert_eq!(run_i32("Intl.getCanonicalLocales('en-US').length"), 1);
}

/// getCanonicalLocales canonicalizes case.
#[test]
fn get_canonical_locales_case_canonicalization() {
    assert_eq!(run_string("Intl.getCanonicalLocales('EN-us')[0]"), "en-US");
}

/// getCanonicalLocales canonicalizes language alias.
#[test]
fn get_canonical_locales_language_alias() {
    assert_eq!(run_string("Intl.getCanonicalLocales('iw')[0]"), "he");
}

/// getCanonicalLocales with array of locales.
#[test]
fn get_canonical_locales_array() {
    assert_eq!(
        run_i32("Intl.getCanonicalLocales(['en-US', 'de-DE']).length"),
        2
    );
}

/// getCanonicalLocales deduplicates.
#[test]
fn get_canonical_locales_deduplicates() {
    assert_eq!(
        run_i32("Intl.getCanonicalLocales(['en-US', 'en-US']).length"),
        1
    );
}

/// getCanonicalLocales throws RangeError on invalid tag.
#[test]
fn get_canonical_locales_invalid_tag_throws() {
    assert!(run_bool(
        "var ok = false; try { Intl.getCanonicalLocales(''); } catch(e) { ok = e instanceof RangeError; } ok"
    ));
}

/// getCanonicalLocales handles grandfathered tags.
#[test]
fn get_canonical_locales_grandfathered() {
    assert_eq!(
        run_string("Intl.getCanonicalLocales('art-lojban')[0]"),
        "jbo"
    );
}

/// getCanonicalLocales canonicalizes sh → sr-Latn.
#[test]
fn get_canonical_locales_sh_to_sr_latn() {
    assert_eq!(run_string("Intl.getCanonicalLocales('sh')[0]"), "sr-Latn");
}

// ═══════════════════════════════════════════════════════════════════
//  §8.3.2 Intl.supportedValuesOf
// ═══════════════════════════════════════════════════════════════════

/// supportedValuesOf is a function.
#[test]
fn supported_values_of_is_function() {
    assert!(run_bool("typeof Intl.supportedValuesOf === 'function'"));
}

/// supportedValuesOf.length is 1.
#[test]
fn supported_values_of_length() {
    assert_eq!(run_i32("Intl.supportedValuesOf.length"), 1);
}

/// supportedValuesOf('calendar') returns non-empty array.
#[test]
fn supported_values_of_calendar() {
    assert!(run_bool("Intl.supportedValuesOf('calendar').length > 0"));
}

/// supportedValuesOf('calendar') includes 'gregory'.
#[test]
fn supported_values_of_calendar_includes_gregory() {
    assert!(run_bool(
        "Intl.supportedValuesOf('calendar').includes('gregory')"
    ));
}

/// supportedValuesOf('collation') returns non-empty array.
#[test]
fn supported_values_of_collation() {
    assert!(run_bool("Intl.supportedValuesOf('collation').length > 0"));
}

/// supportedValuesOf('currency') returns non-empty array.
#[test]
fn supported_values_of_currency() {
    assert!(run_bool("Intl.supportedValuesOf('currency').length > 0"));
}

/// supportedValuesOf('numberingSystem') returns array including 'latn'.
#[test]
fn supported_values_of_numbering_system_includes_latn() {
    assert!(run_bool(
        "Intl.supportedValuesOf('numberingSystem').includes('latn')"
    ));
}

/// supportedValuesOf('timeZone') returns non-empty array.
#[test]
fn supported_values_of_timezone() {
    assert!(run_bool("Intl.supportedValuesOf('timeZone').length > 0"));
}

/// supportedValuesOf('unit') returns array including 'meter'.
#[test]
fn supported_values_of_unit_includes_meter() {
    assert!(run_bool("Intl.supportedValuesOf('unit').includes('meter')"));
}

/// supportedValuesOf returns sorted array.
#[test]
fn supported_values_of_is_sorted() {
    assert!(run_bool(
        "var arr = Intl.supportedValuesOf('calendar'); \
         var sorted = true; \
         for (var i = 1; i < arr.length; i++) { \
           if (arr[i] < arr[i-1]) { sorted = false; break; } \
         } \
         sorted"
    ));
}

/// supportedValuesOf throws RangeError on invalid key.
#[test]
fn supported_values_of_invalid_key_throws() {
    assert!(run_bool(
        "var ok = false; try { Intl.supportedValuesOf('invalid'); } catch(e) { ok = e instanceof RangeError; } ok"
    ));
}
