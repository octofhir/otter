//! Integration tests for the Temporal proposal (Stage 4).
//!
//! Covers Temporal.Instant, Duration, PlainDate, PlainTime, PlainDateTime,
//! PlainYearMonth, PlainMonthDay, ZonedDateTime, and Temporal.Now.
//!
//! Spec: <https://tc39.es/proposal-temporal/>

use otter_vm::object::ObjectHandle;
use otter_vm::source::compile_eval;
use otter_vm::{Interpreter, RuntimeState};

fn eval(source: &str) -> String {
    let module = compile_eval(source, "<test>").expect("should compile");
    let mut runtime = RuntimeState::new();
    let global = runtime.intrinsics().global_object();
    let registers = [otter_vm::value::RegisterValue::from_object_handle(global.0)];
    let v = Interpreter::new()
        .execute_with_runtime(
            &module,
            otter_vm::module::FunctionIndex(0),
            &registers,
            &mut runtime,
        )
        .expect("should execute")
        .return_value();
    if let Some(b) = v.as_bool() {
        return b.to_string();
    }
    if let Some(n) = v.as_number() {
        if n == (n as i64) as f64 {
            return (n as i64).to_string();
        }
        return n.to_string();
    }
    if v.raw_bits() == otter_vm::value::RegisterValue::undefined().raw_bits() {
        return "undefined".to_string();
    }
    if v.raw_bits() == otter_vm::value::RegisterValue::null().raw_bits() {
        return "null".to_string();
    }
    if let Some(handle) = v.as_object_handle()
        && let Ok(Some(s)) = runtime.objects().string_value(ObjectHandle(handle))
    {
        return s.to_string();
    }
    format!("{v:?}")
}

// ── Temporal namespace ──────────────────────────────────────────────

#[test]
fn temporal_namespace_exists() {
    assert_eq!(eval("typeof Temporal"), "object");
}

#[test]
fn temporal_to_string_tag() {
    assert_eq!(eval("Temporal[Symbol.toStringTag]"), "Temporal");
}

// ── Temporal.Instant ────────────────────────────────────────────────

#[test]
fn instant_from_epoch_ms() {
    assert_eq!(
        eval("Temporal.Instant.fromEpochMilliseconds(0).epochMilliseconds"),
        "0"
    );
}

#[test]
fn instant_from_epoch_ns() {
    assert_eq!(
        eval("Temporal.Instant.fromEpochNanoseconds(1000000000000000000n).epochMilliseconds"),
        "1000000000000"
    );
}

#[test]
fn instant_to_string() {
    let result = eval("Temporal.Instant.fromEpochMilliseconds(0).toString()");
    assert!(result.starts_with("1970-01-01T00:00:00"), "got: {result}");
}

#[test]
fn instant_equals() {
    assert_eq!(
        eval(
            "Temporal.Instant.fromEpochMilliseconds(100).equals(Temporal.Instant.fromEpochMilliseconds(100))"
        ),
        "true"
    );
}

#[test]
fn instant_compare() {
    assert_eq!(
        eval(
            "Temporal.Instant.compare(Temporal.Instant.fromEpochMilliseconds(100), Temporal.Instant.fromEpochMilliseconds(200))"
        ),
        "-1"
    );
}

#[test]
fn instant_value_of_throws() {
    assert_eq!(
        eval(
            "try { Temporal.Instant.fromEpochMilliseconds(0).valueOf(); 'no' } catch(e) { 'threw' }"
        ),
        "threw"
    );
}

// ── Temporal.Duration ───────────────────────────────────────────────

#[test]
fn duration_constructor() {
    assert_eq!(
        eval("new Temporal.Duration(1, 2, 3, 4, 5, 6, 7).toString()"),
        "P1Y2M3W4DT5H6M7S"
    );
}

#[test]
fn duration_from_string() {
    assert_eq!(eval("Temporal.Duration.from('PT1H30M').minutes"), "30");
}

#[test]
fn duration_getters() {
    assert_eq!(eval("new Temporal.Duration(0, 0, 0, 0, 1, 30).hours"), "1");
    assert_eq!(
        eval("new Temporal.Duration(0, 0, 0, 0, 1, 30).minutes"),
        "30"
    );
}

#[test]
fn duration_negated() {
    assert_eq!(eval("new Temporal.Duration(1).negated().years"), "-1");
}

#[test]
fn duration_abs() {
    assert_eq!(eval("new Temporal.Duration(-5).abs().years"), "5");
}

#[test]
fn duration_sign() {
    assert_eq!(eval("new Temporal.Duration(1).sign"), "1");
    assert_eq!(eval("new Temporal.Duration(-1).sign"), "-1");
    assert_eq!(eval("new Temporal.Duration().sign"), "0");
}

#[test]
fn duration_blank() {
    assert_eq!(eval("new Temporal.Duration().blank"), "true");
    assert_eq!(eval("new Temporal.Duration(1).blank"), "false");
}

#[test]
fn duration_add() {
    assert_eq!(
        eval("Temporal.Duration.from('PT1H').add('PT30M').toString()"),
        "PT1H30M"
    );
}

// ── Temporal.PlainDate ──────────────────────────────────────────────

#[test]
fn plain_date_constructor() {
    // Separate construction from method call to diagnose
    assert_eq!(
        eval("const pd = new Temporal.PlainDate(2024, 3, 15); pd.toString()"),
        "2024-03-15"
    );
}

#[test]
fn plain_date_from_string() {
    assert_eq!(eval("Temporal.PlainDate.from('2024-12-25').day"), "25");
}

#[test]
fn plain_date_getters() {
    let setup = "const pd = new Temporal.PlainDate(2024, 6, 15);";
    assert_eq!(eval(&format!("{setup} pd.year")), "2024");
    assert_eq!(eval(&format!("{setup} pd.month")), "6");
    assert_eq!(eval(&format!("{setup} pd.day")), "15");
    assert_eq!(eval(&format!("{setup} pd.calendarId")), "iso8601");
    assert_eq!(eval(&format!("{setup} pd.inLeapYear")), "true");
}

#[test]
fn plain_date_compare() {
    assert_eq!(
        eval(
            "Temporal.PlainDate.compare(new Temporal.PlainDate(2024, 1, 1), new Temporal.PlainDate(2024, 12, 31))"
        ),
        "-1"
    );
}

#[test]
fn plain_date_equals() {
    assert_eq!(
        eval(
            "const a = new Temporal.PlainDate(2024, 1, 1); const b = new Temporal.PlainDate(2024, 1, 1); a.equals(b)"
        ),
        "true"
    );
}

#[test]
fn plain_date_add() {
    assert_eq!(
        eval(
            "const pd = new Temporal.PlainDate(2024, 1, 1); const pd2 = pd.add('P1M'); pd2.toString()"
        ),
        "2024-02-01"
    );
}

// ── Temporal.PlainTime ──────────────────────────────────────────────

#[test]
fn plain_time_constructor() {
    assert_eq!(
        eval("const pt = new Temporal.PlainTime(14, 30, 0); pt.toString()"),
        "14:30:00"
    );
}

#[test]
fn plain_time_from_string() {
    assert_eq!(eval("Temporal.PlainTime.from('09:30:00').hour"), "9");
}

#[test]
fn plain_time_getters() {
    let setup = "const pt = new Temporal.PlainTime(10, 30, 45);";
    assert_eq!(eval(&format!("{setup} pt.hour")), "10");
    assert_eq!(eval(&format!("{setup} pt.minute")), "30");
    assert_eq!(eval(&format!("{setup} pt.second")), "45");
}

#[test]
fn plain_time_compare() {
    assert_eq!(
        eval("Temporal.PlainTime.compare(new Temporal.PlainTime(10), new Temporal.PlainTime(12))"),
        "-1"
    );
}

#[test]
fn plain_time_equals() {
    assert_eq!(
        eval("new Temporal.PlainTime(10, 30).equals(new Temporal.PlainTime(10, 30))"),
        "true"
    );
}

// ── Temporal.PlainDateTime ──────────────────────────────────────────

#[test]
fn plain_date_time_constructor() {
    let result = eval("new Temporal.PlainDateTime(2024, 3, 15, 14, 30).toString()");
    assert!(result.starts_with("2024-03-15T14:30:00"), "got: {result}");
}

#[test]
fn plain_date_time_from_string() {
    assert_eq!(
        eval("Temporal.PlainDateTime.from('2024-06-15T10:30:00').hour"),
        "10"
    );
}

#[test]
fn plain_date_time_getters() {
    let setup = "const pdt = new Temporal.PlainDateTime(2024, 6, 15, 10, 30, 45);";
    assert_eq!(eval(&format!("{setup} pdt.year")), "2024");
    assert_eq!(eval(&format!("{setup} pdt.month")), "6");
    assert_eq!(eval(&format!("{setup} pdt.day")), "15");
    assert_eq!(eval(&format!("{setup} pdt.hour")), "10");
    assert_eq!(eval(&format!("{setup} pdt.minute")), "30");
    assert_eq!(eval(&format!("{setup} pdt.second")), "45");
    assert_eq!(eval(&format!("{setup} pdt.calendarId")), "iso8601");
}

#[test]
fn plain_date_time_to_plain_date() {
    assert_eq!(
        eval("new Temporal.PlainDateTime(2024, 3, 15, 10, 30).toPlainDate().toString()"),
        "2024-03-15"
    );
}

#[test]
fn plain_date_time_to_plain_time() {
    let result = eval("new Temporal.PlainDateTime(2024, 3, 15, 10, 30).toPlainTime().toString()");
    assert!(result.starts_with("10:30:00"), "got: {result}");
}

// ── Temporal.PlainYearMonth ─────────────────────────────────────────

#[test]
fn plain_year_month_constructor() {
    let result = eval("new Temporal.PlainYearMonth(2024, 6).toString()");
    assert!(result.starts_with("2024-06"), "got: {result}");
}

#[test]
fn plain_year_month_from_string() {
    assert_eq!(eval("Temporal.PlainYearMonth.from('2024-12').month"), "12");
}

#[test]
fn plain_year_month_getters() {
    let setup = "const pym = new Temporal.PlainYearMonth(2024, 6);";
    assert_eq!(eval(&format!("{setup} pym.year")), "2024");
    assert_eq!(eval(&format!("{setup} pym.month")), "6");
    assert_eq!(eval(&format!("{setup} pym.calendarId")), "iso8601");
    assert_eq!(eval(&format!("{setup} pym.inLeapYear")), "true");
}

// ── Temporal.PlainMonthDay ──────────────────────────────────────────

#[test]
fn plain_month_day_constructor() {
    let result = eval("new Temporal.PlainMonthDay(12, 25).toString()");
    assert!(result.contains("12-25"), "got: {result}");
}

#[test]
fn plain_month_day_from_string() {
    assert_eq!(eval("Temporal.PlainMonthDay.from('--12-25').day"), "25");
}

#[test]
fn plain_month_day_getters() {
    let setup = "const pmd = new Temporal.PlainMonthDay(6, 15);";
    assert_eq!(eval(&format!("{setup} pmd.day")), "15");
    assert_eq!(eval(&format!("{setup} pmd.calendarId")), "iso8601");
}

// ── Temporal.Now ────────────────────────────────────────────────────

#[test]
fn now_instant_returns_instant() {
    assert_eq!(eval("Temporal.Now.instant().epochMilliseconds > 0"), "true");
}

#[test]
fn now_time_zone_id_returns_string() {
    assert_eq!(eval("typeof Temporal.Now.timeZoneId()"), "string");
}

#[test]
fn now_plain_date_iso_returns_date() {
    // The current year should be >= 2024
    assert_eq!(eval("Temporal.Now.plainDateISO().year >= 2024"), "true");
}

#[test]
fn now_plain_time_iso_returns_time() {
    // hour should be 0..23
    assert_eq!(
        eval("let h = Temporal.Now.plainTimeISO().hour; h >= 0 && h <= 23"),
        "true"
    );
}

#[test]
fn now_plain_date_time_iso_returns_datetime() {
    assert_eq!(eval("Temporal.Now.plainDateTimeISO().year >= 2024"), "true");
}

// ── Cross-type conversions ──────────────────────────────────────────

#[test]
fn plain_date_time_compare() {
    assert_eq!(
        eval(
            "Temporal.PlainDateTime.compare(new Temporal.PlainDateTime(2024, 1, 1), new Temporal.PlainDateTime(2024, 12, 31))"
        ),
        "-1"
    );
}

#[test]
fn duration_value_of_throws() {
    assert_eq!(
        eval("try { new Temporal.Duration(1).valueOf(); 'no' } catch(e) { 'threw' }"),
        "threw"
    );
}

#[test]
fn plain_date_value_of_throws() {
    assert_eq!(
        eval("try { new Temporal.PlainDate(2024, 1, 1).valueOf(); 'no' } catch(e) { 'threw' }"),
        "threw"
    );
}
