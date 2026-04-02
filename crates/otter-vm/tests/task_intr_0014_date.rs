//! Integration tests for ES2024 Date (§21.4).
//!
//! Spec references:
//! - Date Constructor: <https://tc39.es/ecma262/#sec-date-constructor>
//! - Date.now: <https://tc39.es/ecma262/#sec-date.now>
//! - Date.parse: <https://tc39.es/ecma262/#sec-date.parse>
//! - Date.UTC: <https://tc39.es/ecma262/#sec-date.utc>
//! - Date.prototype: <https://tc39.es/ecma262/#sec-properties-of-the-date-prototype-object>

use otter_vm::source::compile_test262_basic_script;
use otter_vm::value::RegisterValue;
use otter_vm::{Interpreter, RuntimeState};

fn run(source: &str, url: &str) -> RegisterValue {
    let module = compile_test262_basic_script(source, url).expect("should compile");
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

// ── Constructor ──────────────────────────────────────────────────────────────

#[test]
fn date_now_returns_number() {
    let r = run(
        "assert.sameValue(typeof Date.now(), 'number', 'Date.now returns number');",
        "date-now.js",
    );
    assert_eq!(r, RegisterValue::from_i32(0));
}

#[test]
fn date_constructor_no_args() {
    let r = run(
        concat!(
            "var d = new Date();\n",
            "assert.sameValue(typeof d.getTime(), 'number', 'getTime returns number');\n",
            "assert.sameValue(d.getTime() > 0, true, 'timestamp is positive');\n",
        ),
        "date-ctor-no-args.js",
    );
    assert_eq!(r, RegisterValue::from_i32(0));
}

#[test]
fn date_constructor_single_number() {
    let r = run(
        concat!(
            "var d = new Date(0);\n",
            "assert.sameValue(d.getTime(), 0, 'epoch zero');\n",
            "var d2 = new Date(86400000);\n",
            "assert.sameValue(d2.getTime(), 86400000, 'one day');\n",
        ),
        "date-ctor-number.js",
    );
    assert_eq!(r, RegisterValue::from_i32(0));
}

#[test]
fn date_constructor_nan() {
    let r = run(
        concat!(
            "var d = new Date(NaN);\n",
            "assert.sameValue(isNaN(d.getTime()), true, 'NaN input → NaN time');\n",
        ),
        "date-ctor-nan.js",
    );
    assert_eq!(r, RegisterValue::from_i32(0));
}

#[test]
fn date_constructor_multi_args() {
    let r = run(
        concat!(
            "var d = new Date(Date.UTC(2024, 0, 15, 12, 30, 45, 500));\n",
            "assert.sameValue(d.getUTCFullYear(), 2024, 'year');\n",
            "assert.sameValue(d.getUTCMonth(), 0, 'month');\n",
            "assert.sameValue(d.getUTCDate(), 15, 'date');\n",
            "assert.sameValue(d.getUTCHours(), 12, 'hours');\n",
            "assert.sameValue(d.getUTCMinutes(), 30, 'minutes');\n",
            "assert.sameValue(d.getUTCSeconds(), 45, 'seconds');\n",
            "assert.sameValue(d.getUTCMilliseconds(), 500, 'ms');\n",
        ),
        "date-ctor-multi.js",
    );
    assert_eq!(r, RegisterValue::from_i32(0));
}

#[test]
fn date_constructor_string_iso() {
    let r = run(
        concat!(
            "var d = new Date('2024-06-15T00:00:00.000Z');\n",
            "assert.sameValue(d.getUTCFullYear(), 2024, 'year');\n",
            "assert.sameValue(d.getUTCMonth(), 5, 'month (June=5)');\n",
            "assert.sameValue(d.getUTCDate(), 15, 'date');\n",
        ),
        "date-ctor-string.js",
    );
    assert_eq!(r, RegisterValue::from_i32(0));
}

#[test]
fn date_called_as_function() {
    let r = run(
        concat!(
            "var s = Date();\n",
            "assert.sameValue(typeof s, 'string', 'Date() returns string');\n",
        ),
        "date-as-function.js",
    );
    assert_eq!(r, RegisterValue::from_i32(0));
}

// ── Static methods ───────────────────────────────────────────────────────────

#[test]
fn date_utc_basic() {
    let r = run(
        concat!(
            "assert.sameValue(Date.UTC(1970, 0, 1), 0, 'epoch');\n",
            "assert.sameValue(Date.UTC(2000, 0, 1), 946684800000, 'Y2K');\n",
            "assert.sameValue(Date.UTC(1970, 0, 2), 86400000, 'one day');\n",
        ),
        "date-utc.js",
    );
    assert_eq!(r, RegisterValue::from_i32(0));
}

#[test]
fn date_utc_year_adjustment() {
    // Years 0-99 map to 1900-1999.
    let r = run(
        concat!(
            "var a = Date.UTC(99, 0, 1);\n",
            "var b = Date.UTC(1999, 0, 1);\n",
            "assert.sameValue(a, b, '99 → 1999');\n",
        ),
        "date-utc-year-adj.js",
    );
    assert_eq!(r, RegisterValue::from_i32(0));
}

#[test]
fn date_utc_year_zero_adjustment() {
    let r = run(
        concat!(
            "var a = Date.UTC(0, 0, 1);\n",
            "var b = Date.UTC(1900, 0, 1);\n",
            "assert.sameValue(a, b, '0 → 1900');\n",
        ),
        "date-utc-year-zero.js",
    );
    assert_eq!(r, RegisterValue::from_i32(0));
}

#[test]
fn date_parse_iso() {
    let r = run(
        concat!(
            "assert.sameValue(Date.parse('1970-01-01T00:00:00.000Z'), 0, 'epoch ISO');\n",
            "assert.sameValue(Date.parse('2024-01-15T00:00:00.000Z'), 1705276800000, '2024-01-15');\n",
            "assert.sameValue(isNaN(Date.parse('not-a-date')), true, 'invalid string');\n",
        ),
        "date-parse.js",
    );
    assert_eq!(r, RegisterValue::from_i32(0));
}

#[test]
fn date_parse_date_only() {
    // Date-only strings are UTC per ES spec.
    let r = run(
        "assert.sameValue(Date.parse('1970-01-01'), 0, 'date-only is UTC');",
        "date-parse-dateonly.js",
    );
    assert_eq!(r, RegisterValue::from_i32(0));
}

// ── UTC getters ──────────────────────────────────────────────────────────────

#[test]
fn date_utc_getters() {
    let r = run(
        concat!(
            "var d = new Date(Date.UTC(2023, 11, 25, 10, 30, 45, 123));\n",
            "assert.sameValue(d.getUTCFullYear(), 2023, 'year');\n",
            "assert.sameValue(d.getUTCMonth(), 11, 'month');\n",
            "assert.sameValue(d.getUTCDate(), 25, 'date');\n",
            "assert.sameValue(d.getUTCHours(), 10, 'hours');\n",
            "assert.sameValue(d.getUTCMinutes(), 30, 'minutes');\n",
            "assert.sameValue(d.getUTCSeconds(), 45, 'seconds');\n",
            "assert.sameValue(d.getUTCMilliseconds(), 123, 'ms');\n",
        ),
        "date-utc-getters.js",
    );
    assert_eq!(r, RegisterValue::from_i32(0));
}

#[test]
fn date_utc_day() {
    let r = run(
        concat!(
            // 1970-01-01 is Thursday (day 4).
            "var d = new Date(0);\n",
            "assert.sameValue(d.getUTCDay(), 4, 'epoch is Thursday');\n",
        ),
        "date-utc-day.js",
    );
    assert_eq!(r, RegisterValue::from_i32(0));
}

// ── Setters (UTC) ────────────────────────────────────────────────────────────

#[test]
fn date_set_utc_full_year() {
    let r = run(
        concat!(
            "var d = new Date(Date.UTC(2020, 5, 15));\n",
            "d.setUTCFullYear(2025);\n",
            "assert.sameValue(d.getUTCFullYear(), 2025, 'year updated');\n",
            "assert.sameValue(d.getUTCMonth(), 5, 'month preserved');\n",
            "assert.sameValue(d.getUTCDate(), 15, 'date preserved');\n",
        ),
        "date-set-utc-fullyear.js",
    );
    assert_eq!(r, RegisterValue::from_i32(0));
}

#[test]
fn date_set_utc_month() {
    let r = run(
        concat!(
            "var d = new Date(Date.UTC(2020, 0, 15));\n",
            "d.setUTCMonth(6);\n",
            "assert.sameValue(d.getUTCMonth(), 6, 'month updated');\n",
            "assert.sameValue(d.getUTCDate(), 15, 'date preserved');\n",
        ),
        "date-set-utc-month.js",
    );
    assert_eq!(r, RegisterValue::from_i32(0));
}

#[test]
fn date_set_utc_date() {
    let r = run(
        concat!(
            "var d = new Date(Date.UTC(2020, 0, 1));\n",
            "d.setUTCDate(20);\n",
            "assert.sameValue(d.getUTCDate(), 20, 'date updated');\n",
        ),
        "date-set-utc-date.js",
    );
    assert_eq!(r, RegisterValue::from_i32(0));
}

#[test]
fn date_set_utc_hours() {
    let r = run(
        concat!(
            "var d = new Date(Date.UTC(2020, 0, 1, 0, 0, 0));\n",
            "d.setUTCHours(15);\n",
            "assert.sameValue(d.getUTCHours(), 15, 'hours updated');\n",
        ),
        "date-set-utc-hours.js",
    );
    assert_eq!(r, RegisterValue::from_i32(0));
}

#[test]
fn date_set_utc_minutes() {
    let r = run(
        concat!(
            "var d = new Date(Date.UTC(2020, 0, 1, 12, 0));\n",
            "d.setUTCMinutes(45);\n",
            "assert.sameValue(d.getUTCMinutes(), 45, 'minutes updated');\n",
        ),
        "date-set-utc-minutes.js",
    );
    assert_eq!(r, RegisterValue::from_i32(0));
}

#[test]
fn date_set_utc_seconds() {
    let r = run(
        concat!(
            "var d = new Date(Date.UTC(2020, 0, 1));\n",
            "d.setUTCSeconds(30);\n",
            "assert.sameValue(d.getUTCSeconds(), 30, 'seconds updated');\n",
        ),
        "date-set-utc-seconds.js",
    );
    assert_eq!(r, RegisterValue::from_i32(0));
}

#[test]
fn date_set_utc_milliseconds() {
    let r = run(
        concat!(
            "var d = new Date(Date.UTC(2020, 0, 1));\n",
            "d.setUTCMilliseconds(999);\n",
            "assert.sameValue(d.getUTCMilliseconds(), 999, 'ms updated');\n",
        ),
        "date-set-utc-ms.js",
    );
    assert_eq!(r, RegisterValue::from_i32(0));
}

#[test]
fn date_set_time() {
    let r = run(
        concat!(
            "var d = new Date(0);\n",
            "var ret = d.setTime(86400000);\n",
            "assert.sameValue(d.getTime(), 86400000, 'time updated');\n",
            "assert.sameValue(ret, 86400000, 'returns new time');\n",
        ),
        "date-set-time.js",
    );
    assert_eq!(r, RegisterValue::from_i32(0));
}

// ── Setters (local time) ─────────────────────────────────────────────────────

#[test]
fn date_set_full_year_from_nan() {
    // Per spec: If t is NaN, let t = +0.
    let r = run(
        concat!(
            "var d = new Date(NaN);\n",
            "d.setFullYear(2020);\n",
            "assert.sameValue(isNaN(d.getTime()), false, 'recovered from NaN');\n",
        ),
        "date-set-fullyear-nan.js",
    );
    assert_eq!(r, RegisterValue::from_i32(0));
}

// ── String methods ───────────────────────────────────────────────────────────

#[test]
fn date_to_iso_string() {
    let r = run(
        concat!(
            "var d = new Date(Date.UTC(2024, 0, 15, 12, 30, 45, 123));\n",
            "assert.sameValue(\n",
            "  d.toISOString(),\n",
            "  '2024-01-15T12:30:45.123Z',\n",
            "  'ISO string'\n",
            ");\n",
        ),
        "date-to-iso.js",
    );
    assert_eq!(r, RegisterValue::from_i32(0));
}

#[test]
fn date_to_iso_string_epoch() {
    let r = run(
        concat!(
            "var d = new Date(0);\n",
            "assert.sameValue(d.toISOString(), '1970-01-01T00:00:00.000Z', 'epoch ISO');\n",
        ),
        "date-to-iso-epoch.js",
    );
    assert_eq!(r, RegisterValue::from_i32(0));
}

#[test]
fn date_to_utc_string() {
    let r = run(
        concat!(
            "var d = new Date(0);\n",
            "assert.sameValue(d.toUTCString(), 'Thu, 01 Jan 1970 00:00:00 GMT', 'UTC string');\n",
        ),
        "date-to-utc.js",
    );
    assert_eq!(r, RegisterValue::from_i32(0));
}

#[test]
fn date_to_string_invalid() {
    let r = run(
        concat!(
            "var d = new Date(NaN);\n",
            "assert.sameValue(d.toString(), 'Invalid Date', 'NaN → Invalid Date');\n",
            "assert.sameValue(d.toDateString(), 'Invalid Date', 'toDateString invalid');\n",
            "assert.sameValue(d.toTimeString(), 'Invalid Date', 'toTimeString invalid');\n",
            "assert.sameValue(d.toUTCString(), 'Invalid Date', 'toUTCString invalid');\n",
        ),
        "date-to-string-invalid.js",
    );
    assert_eq!(r, RegisterValue::from_i32(0));
}

#[test]
fn date_to_iso_string_throws_on_invalid() {
    let r = run(
        concat!(
            "var threw = false;\n",
            "try { new Date(NaN).toISOString(); } catch (e) { threw = true; }\n",
            "assert.sameValue(threw, true, 'toISOString throws on invalid');\n",
        ),
        "date-to-iso-throws.js",
    );
    assert_eq!(r, RegisterValue::from_i32(0));
}

#[test]
fn date_to_json() {
    let r = run(
        concat!(
            "var d = new Date(0);\n",
            "assert.sameValue(d.toJSON(), '1970-01-01T00:00:00.000Z', 'toJSON epoch');\n",
            "var d2 = new Date(NaN);\n",
            "assert.sameValue(d2.toJSON(), null, 'toJSON NaN → null');\n",
        ),
        "date-to-json.js",
    );
    assert_eq!(r, RegisterValue::from_i32(0));
}

// ── valueOf and getTime ──────────────────────────────────────────────────────

#[test]
fn date_value_of() {
    let r = run(
        concat!(
            "var d = new Date(12345);\n",
            "assert.sameValue(d.valueOf(), 12345, 'valueOf');\n",
            "assert.sameValue(d.getTime(), 12345, 'getTime');\n",
            "assert.sameValue(d.valueOf(), d.getTime(), 'valueOf === getTime');\n",
        ),
        "date-valueof.js",
    );
    assert_eq!(r, RegisterValue::from_i32(0));
}

// ── getTimezoneOffset ────────────────────────────────────────────────────────

#[test]
fn date_get_timezone_offset() {
    let r = run(
        concat!(
            "var d = new Date(0);\n",
            "var offset = d.getTimezoneOffset();\n",
            "assert.sameValue(typeof offset, 'number', 'offset is number');\n",
            // Offset is in minutes, should be between -720 and +840.
            "assert.sameValue(offset >= -720 && offset <= 840, true, 'reasonable range');\n",
        ),
        "date-tz-offset.js",
    );
    assert_eq!(r, RegisterValue::from_i32(0));
}

// ── NaN propagation ──────────────────────────────────────────────────────────

#[test]
fn date_nan_getters() {
    let r = run(
        concat!(
            "var d = new Date(NaN);\n",
            "assert.sameValue(isNaN(d.getFullYear()), true, 'getFullYear NaN');\n",
            "assert.sameValue(isNaN(d.getMonth()), true, 'getMonth NaN');\n",
            "assert.sameValue(isNaN(d.getDate()), true, 'getDate NaN');\n",
            "assert.sameValue(isNaN(d.getHours()), true, 'getHours NaN');\n",
            "assert.sameValue(isNaN(d.getMinutes()), true, 'getMinutes NaN');\n",
            "assert.sameValue(isNaN(d.getSeconds()), true, 'getSeconds NaN');\n",
            "assert.sameValue(isNaN(d.getMilliseconds()), true, 'getMilliseconds NaN');\n",
            "assert.sameValue(isNaN(d.getDay()), true, 'getDay NaN');\n",
            "assert.sameValue(isNaN(d.getTimezoneOffset()), true, 'getTimezoneOffset NaN');\n",
        ),
        "date-nan-getters.js",
    );
    assert_eq!(r, RegisterValue::from_i32(0));
}

// ── Legacy methods (Annex B) ─────────────────────────────────────────────────

#[test]
fn date_get_year_set_year() {
    let r = run(
        concat!(
            "var d = new Date(Date.UTC(2024, 0, 1));\n",
            "assert.sameValue(d.getYear(), 124, 'getYear = year - 1900');\n",
            "d.setYear(99);\n",
            "assert.sameValue(d.getFullYear(), 1999, 'setYear 99 → 1999');\n",
        ),
        "date-legacy.js",
    );
    assert_eq!(r, RegisterValue::from_i32(0));
}

// ── Date.parse round-trip ────────────────────────────────────────────────────

#[test]
fn date_parse_utc_string_roundtrip() {
    let r = run(
        concat!(
            "var d = new Date(Date.UTC(2024, 5, 15, 12, 30, 45));\n",
            "var utcStr = d.toUTCString();\n",
            "var parsed = Date.parse(utcStr);\n",
            "assert.sameValue(parsed, d.getTime(), 'round-trip through toUTCString');\n",
        ),
        "date-roundtrip-utc.js",
    );
    assert_eq!(r, RegisterValue::from_i32(0));
}

#[test]
fn date_parse_iso_string_roundtrip() {
    let r = run(
        concat!(
            "var d = new Date(Date.UTC(2024, 5, 15, 12, 30, 45, 123));\n",
            "var isoStr = d.toISOString();\n",
            "var parsed = Date.parse(isoStr);\n",
            "assert.sameValue(parsed, d.getTime(), 'round-trip through toISOString');\n",
        ),
        "date-roundtrip-iso.js",
    );
    assert_eq!(r, RegisterValue::from_i32(0));
}

// ── Symbol.toPrimitive ───────────────────────────────────────────────────────

#[test]
fn date_to_primitive_hint_number() {
    let r = run(
        concat!(
            "var d = new Date(42);\n",
            "assert.sameValue(+d, 42, 'unary + uses number hint');\n",
        ),
        "date-toprimitive-number.js",
    );
    assert_eq!(r, RegisterValue::from_i32(0));
}

#[test]
fn date_to_primitive_hint_string() {
    // Test that Symbol.toPrimitive exists and is callable.
    let r = run(
        concat!(
            "var d = new Date(0);\n",
            "var tp = d[Symbol.toPrimitive];\n",
            "assert.sameValue(typeof tp, 'function', 'toPrimitive is function');\n",
            "var s = tp.call(d, 'string');\n",
            "assert.sameValue(typeof s, 'string', 'string hint returns string');\n",
        ),
        "date-toprimitive-string.js",
    );
    assert_eq!(r, RegisterValue::from_i32(0));
}

// ── Setter return values ─────────────────────────────────────────────────────

#[test]
fn date_setters_return_new_timestamp() {
    let r = run(
        concat!(
            "var d = new Date(Date.UTC(2020, 0, 1));\n",
            "var t1 = d.setUTCFullYear(2025);\n",
            "assert.sameValue(t1, d.getTime(), 'setUTCFullYear returns time');\n",
            "var t2 = d.setUTCMonth(6);\n",
            "assert.sameValue(t2, d.getTime(), 'setUTCMonth returns time');\n",
            "var t3 = d.setUTCDate(20);\n",
            "assert.sameValue(t3, d.getTime(), 'setUTCDate returns time');\n",
            "var t4 = d.setUTCHours(15);\n",
            "assert.sameValue(t4, d.getTime(), 'setUTCHours returns time');\n",
        ),
        "date-setter-return.js",
    );
    assert_eq!(r, RegisterValue::from_i32(0));
}
