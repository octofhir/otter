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

fn run_f64(source: &str) -> f64 {
    let v = run(source);
    v.as_number()
        .or_else(|| v.as_i32().map(f64::from))
        .unwrap_or_else(|| panic!("expected number, got {v:?}"))
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
    assert_eq!(run_string("new Intl.Locale('en-US').language"), "en");
}

#[test]
fn region_getter() {
    assert_eq!(run_string("new Intl.Locale('en-US').region"), "US");
}

#[test]
fn script_getter() {
    assert_eq!(run_string("new Intl.Locale('zh-Hans-CN').script"), "Hans");
}

#[test]
fn region_getter_with_script() {
    assert_eq!(run_string("new Intl.Locale('zh-Hans-CN').region"), "CN");
}

#[test]
fn base_name_getter() {
    assert_eq!(run_string("new Intl.Locale('en-US').baseName"), "en-US");
}

#[test]
fn region_undefined_when_absent() {
    assert!(run_bool("new Intl.Locale('en').region === undefined"));
}

#[test]
fn script_undefined_when_absent() {
    assert!(run_bool("new Intl.Locale('en-US').script === undefined"));
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
        run_string("new Intl.Locale('en', { calendar: 'islamic' }).calendar"),
        "islamic"
    );
}

#[test]
fn collation_option() {
    assert_eq!(
        run_string("new Intl.Locale('en', { collation: 'phonebk' }).collation"),
        "phonebk"
    );
}

#[test]
fn numbering_system_option() {
    assert_eq!(
        run_string("new Intl.Locale('en', { numberingSystem: 'arab' }).numberingSystem"),
        "arab"
    );
}

#[test]
fn calendar_undefined_when_absent() {
    assert!(run_bool("new Intl.Locale('en').calendar === undefined"));
}

// ═══════════════════════════════════════════════════════════════════
//  §14.3.8-10 Additional accessor getters
// ═══════════════════════════════════════════════════════════════════

#[test]
fn hour_cycle_option() {
    assert_eq!(
        run_string("new Intl.Locale('en', { hourCycle: 'h12' }).hourCycle"),
        "h12"
    );
}

#[test]
fn hour_cycle_undefined_when_absent() {
    assert!(run_bool("new Intl.Locale('en').hourCycle === undefined"));
}

#[test]
fn case_first_option() {
    assert_eq!(
        run_string("new Intl.Locale('en', { caseFirst: 'upper' }).caseFirst"),
        "upper"
    );
}

#[test]
fn case_first_undefined_when_absent() {
    assert!(run_bool("new Intl.Locale('en').caseFirst === undefined"));
}

#[test]
fn numeric_option_true() {
    assert!(run_bool("new Intl.Locale('en', { numeric: true }).numeric === true"));
}

#[test]
fn numeric_option_false() {
    assert!(run_bool("new Intl.Locale('en', { numeric: false }).numeric === false"));
}

#[test]
fn numeric_default_false() {
    assert!(run_bool("new Intl.Locale('en').numeric === false"));
}

// ═══════════════════════════════════════════════════════════════════
//  §14.3.11-16 Query methods
// ═══════════════════════════════════════════════════════════════════

#[test]
fn get_calendars_returns_array() {
    assert!(run_bool("Array.isArray(new Intl.Locale('en').getCalendars())"));
}

#[test]
fn get_calendars_has_elements() {
    assert!(run_bool("new Intl.Locale('en').getCalendars().length >= 1"));
}

#[test]
fn get_calendars_with_option() {
    let s = run_string("new Intl.Locale('en', { calendar: 'islamic' }).getCalendars()[0]");
    assert_eq!(s, "islamic");
}

#[test]
fn get_collations_returns_array() {
    assert!(run_bool("Array.isArray(new Intl.Locale('en').getCollations())"));
}

#[test]
fn get_hour_cycles_returns_array() {
    assert!(run_bool("Array.isArray(new Intl.Locale('en').getHourCycles())"));
}

#[test]
fn get_hour_cycles_en_default() {
    let s = run_string("new Intl.Locale('en').getHourCycles()[0]");
    assert_eq!(s, "h12");
}

#[test]
fn get_numbering_systems_returns_array() {
    assert!(run_bool("Array.isArray(new Intl.Locale('en').getNumberingSystems())"));
}

#[test]
fn get_numbering_systems_default_latn() {
    let s = run_string("new Intl.Locale('en').getNumberingSystems()[0]");
    assert_eq!(s, "latn");
}

#[test]
fn get_time_zones_us() {
    assert!(run_bool("new Intl.Locale('en-US').getTimeZones().length >= 1"));
}

#[test]
fn get_time_zones_us_contains_new_york() {
    // Use includes() or manual loop since indexOf may have string-comparison edge cases.
    assert!(run_bool(
        "var tzs = new Intl.Locale('en-US').getTimeZones(); \
         var found = false; \
         for (var i = 0; i < tzs.length; i++) { if (tzs[i] === 'America/New_York') found = true; } \
         found"
    ));
}

#[test]
fn get_time_zones_no_region_undefined() {
    assert!(run_bool("new Intl.Locale('en').getTimeZones() === undefined"));
}

#[test]
fn get_text_info_returns_object() {
    assert!(run_bool("typeof new Intl.Locale('en').getTextInfo() === 'object'"));
}

#[test]
fn get_text_info_en_ltr() {
    let s = run_string("new Intl.Locale('en').getTextInfo().direction");
    assert_eq!(s, "ltr");
}

#[test]
fn get_text_info_ar_rtl() {
    let s = run_string("new Intl.Locale('ar').getTextInfo().direction");
    assert_eq!(s, "rtl");
}

#[test]
fn get_text_info_he_rtl() {
    let s = run_string("new Intl.Locale('he').getTextInfo().direction");
    assert_eq!(s, "rtl");
}

// ── getWeekInfo() ────────────────────────────────────────────────

#[test]
fn get_week_info_is_function() {
    assert!(run_bool("typeof new Intl.Locale('en').getWeekInfo === 'function'"));
}

#[test]
fn get_week_info_returns_object() {
    assert!(run_bool("typeof new Intl.Locale('en-US').getWeekInfo() === 'object'"));
}

#[test]
fn get_week_info_us_first_day() {
    // US: week starts on Sunday (7)
    let n = run_f64("new Intl.Locale('en-US').getWeekInfo().firstDay");
    assert_eq!(n, 7.0);
}

#[test]
fn get_week_info_de_first_day() {
    // Germany: week starts on Monday (1)
    let n = run_f64("new Intl.Locale('de-DE').getWeekInfo().firstDay");
    assert_eq!(n, 1.0);
}

#[test]
fn get_week_info_us_weekend() {
    // US: weekend is [6, 7] (Saturday, Sunday)
    assert!(run_bool(
        "var w = new Intl.Locale('en-US').getWeekInfo().weekend; \
         Array.isArray(w) && w.length === 2"
    ));
}

#[test]
fn get_week_info_de_minimal_days() {
    // Germany follows ISO 8601: 4 minimal days
    let n = run_f64("new Intl.Locale('de-DE').getWeekInfo().minimalDays");
    assert_eq!(n, 4.0);
}

#[test]
fn get_week_info_us_minimal_days() {
    // US: 1 minimal day
    let n = run_f64("new Intl.Locale('en-US').getWeekInfo().minimalDays");
    assert_eq!(n, 1.0);
}

#[test]
fn get_week_info_sa_weekend() {
    // Saudi Arabia: weekend is [5, 6] (Friday, Saturday)
    assert!(run_bool(
        "var w = new Intl.Locale('ar-SA').getWeekInfo().weekend; \
         w[0] === 5 && w[1] === 6"
    ));
}
