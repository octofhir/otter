//! Date constructor and prototype intrinsics.
//!
//! Spec: <https://tc39.es/ecma262/#sec-date-objects>
//!
//! All Date objects store their internal [[DateValue]] as a hidden
//! `__otter_date_data__` property holding an f64 (milliseconds since epoch).

use std::time::{SystemTime, UNIX_EPOCH};

use chrono::{DateTime, Datelike, Local, NaiveDate, TimeZone, Timelike};

use crate::builders::ClassBuilder;
use crate::descriptors::{
    JsClassDescriptor, NativeBindingDescriptor, NativeBindingTarget, NativeFunctionDescriptor,
    VmNativeCallError,
};
use crate::interpreter::{InterpreterError, RuntimeState, ToPrimitiveHint};
use crate::object::{ObjectHandle, PropertyAttributes, PropertyValue};
use crate::value::RegisterValue;

use super::{
    IntrinsicsError, VmIntrinsics, WellKnownSymbol,
    install::{IntrinsicInstallContext, IntrinsicInstaller, install_class_plan},
};

// ── Constants ────────────────────────────────────────────────────────────────

pub(super) static DATE_INTRINSIC: DateIntrinsic = DateIntrinsic;

const DATE_DATA_SLOT: &str = "__otter_date_data__";

const MS_PER_DAY: f64 = 86_400_000.0;
const MS_PER_HOUR: f64 = 3_600_000.0;
const MS_PER_MINUTE: f64 = 60_000.0;
const MS_PER_SECOND: f64 = 1_000.0;

// ── Spec helper functions (§21.4.1) ──────────────────────────────────────────

/// §21.4.1.11 MakeTime ( hour, min, sec, ms )
/// <https://tc39.es/ecma262/#sec-maketime>
fn make_time(hour: f64, min: f64, sec: f64, ms: f64) -> f64 {
    if !hour.is_finite() || !min.is_finite() || !sec.is_finite() || !ms.is_finite() {
        return f64::NAN;
    }
    hour * MS_PER_HOUR + min * MS_PER_MINUTE + sec * MS_PER_SECOND + ms
}

/// §21.4.1.3 DayFromYear ( y )
/// <https://tc39.es/ecma262/#sec-year-number>
fn day_from_year(y: f64) -> f64 {
    365.0 * (y - 1970.0) + ((y - 1969.0) / 4.0).floor() - ((y - 1901.0) / 100.0).floor()
        + ((y - 1601.0) / 400.0).floor()
}

fn days_in_year(y: f64) -> f64 {
    if y % 4.0 != 0.0 {
        365.0
    } else if y % 100.0 != 0.0 {
        366.0
    } else if y % 400.0 != 0.0 {
        365.0
    } else {
        366.0
    }
}

fn day_from_month(m: f64, leap: bool) -> f64 {
    let l = if leap { 1.0 } else { 0.0 };
    match m as i32 {
        0 => 0.0,
        1 => 31.0,
        2 => 59.0 + l,
        3 => 90.0 + l,
        4 => 120.0 + l,
        5 => 151.0 + l,
        6 => 181.0 + l,
        7 => 212.0 + l,
        8 => 243.0 + l,
        9 => 273.0 + l,
        10 => 304.0 + l,
        11 => 334.0 + l,
        _ => 0.0,
    }
}

/// §21.4.1.12 MakeDay ( year, month, date )
/// <https://tc39.es/ecma262/#sec-makeday>
fn make_day(year: f64, month: f64, date: f64) -> f64 {
    if !year.is_finite() || !month.is_finite() || !date.is_finite() {
        return f64::NAN;
    }
    let y = year + (month / 12.0).floor();
    let m = month % 12.0;
    let m = if m < 0.0 { m + 12.0 } else { m };
    let leap = days_in_year(y) == 366.0;
    day_from_year(y) + day_from_month(m, leap) + date - 1.0
}

/// §21.4.1.13 MakeDate ( day, time )
/// <https://tc39.es/ecma262/#sec-makedate>
fn make_date(day: f64, time: f64) -> f64 {
    if !day.is_finite() || !time.is_finite() {
        return f64::NAN;
    }
    day * MS_PER_DAY + time
}

/// §21.4.1.14 TimeClip ( time )
/// <https://tc39.es/ecma262/#sec-timeclip>
fn time_clip(t: f64) -> f64 {
    if !t.is_finite() || t.abs() > 8_640_000_000_000_000.0 {
        return f64::NAN;
    }
    let t = t.trunc();
    if t == 0.0 { 0.0 } else { t }
}

// ── Date component extraction (pure arithmetic, §21.4.1) ────────────────────

/// Compute year from time value using binary search.
fn year_from_time(t: f64) -> f64 {
    let day = (t / MS_PER_DAY).floor();
    let est = (day / 365.2425 + 1970.0).floor() as i64;
    let mut lo = est - 2;
    let mut hi = est + 2;
    while day_from_year(lo as f64) > day {
        lo -= 10;
    }
    while day_from_year((hi + 1) as f64) <= day {
        hi += 10;
    }
    while lo < hi {
        let mid = lo + (hi - lo + 1) / 2;
        if day_from_year(mid as f64) <= day {
            lo = mid;
        } else {
            hi = mid - 1;
        }
    }
    lo as f64
}

/// Compute month (0-11) from time value.
fn month_from_time(t: f64) -> f64 {
    let day = (t / MS_PER_DAY).floor();
    let y = year_from_time(t);
    let day_in_year = day - day_from_year(y);
    let l = if days_in_year(y) == 366.0 { 1.0 } else { 0.0 };
    if day_in_year < 31.0 {
        0.0
    } else if day_in_year < 59.0 + l {
        1.0
    } else if day_in_year < 90.0 + l {
        2.0
    } else if day_in_year < 120.0 + l {
        3.0
    } else if day_in_year < 151.0 + l {
        4.0
    } else if day_in_year < 181.0 + l {
        5.0
    } else if day_in_year < 212.0 + l {
        6.0
    } else if day_in_year < 243.0 + l {
        7.0
    } else if day_in_year < 273.0 + l {
        8.0
    } else if day_in_year < 304.0 + l {
        9.0
    } else if day_in_year < 334.0 + l {
        10.0
    } else {
        11.0
    }
}

/// Compute day of month (1-31) from time value.
fn date_from_time(t: f64) -> f64 {
    let day = (t / MS_PER_DAY).floor();
    let y = year_from_time(t);
    let day_in_year = day - day_from_year(y);
    let l = if days_in_year(y) == 366.0 { 1.0 } else { 0.0 };
    let m = month_from_time(t);
    match m as i32 {
        0 => day_in_year + 1.0,
        1 => day_in_year - 30.0,
        2 => day_in_year - 58.0 - l,
        3 => day_in_year - 89.0 - l,
        4 => day_in_year - 119.0 - l,
        5 => day_in_year - 150.0 - l,
        6 => day_in_year - 180.0 - l,
        7 => day_in_year - 211.0 - l,
        8 => day_in_year - 242.0 - l,
        9 => day_in_year - 272.0 - l,
        10 => day_in_year - 303.0 - l,
        11 => day_in_year - 333.0 - l,
        _ => f64::NAN,
    }
}

/// §21.4.1.11 HourFromTime
fn hour_from_time(t: f64) -> f64 {
    ((t / MS_PER_HOUR).floor()).rem_euclid(24.0)
}

/// §21.4.1.11 MinFromTime
fn min_from_time(t: f64) -> f64 {
    ((t / MS_PER_MINUTE).floor()).rem_euclid(60.0)
}

/// §21.4.1.11 SecFromTime
fn sec_from_time(t: f64) -> f64 {
    ((t / MS_PER_SECOND).floor()).rem_euclid(60.0)
}

/// §21.4.1.11 msFromTime
fn ms_from_time(t: f64) -> f64 {
    t.rem_euclid(1000.0)
}

/// §21.4.1.6 WeekDay
fn week_day(t: f64) -> f64 {
    ((t / MS_PER_DAY).floor() + 4.0).rem_euclid(7.0)
}

/// Extract UTC time components from a timestamp (ms since epoch).
/// Returns (year, month0, day, hour, minute, second, ms).
fn utc_components(ts: f64) -> (f64, f64, f64, f64, f64, f64, f64) {
    if !ts.is_finite() {
        return (
            f64::NAN, f64::NAN, f64::NAN, f64::NAN, f64::NAN, f64::NAN, f64::NAN,
        );
    }
    (
        year_from_time(ts),
        month_from_time(ts),
        date_from_time(ts),
        hour_from_time(ts),
        min_from_time(ts),
        sec_from_time(ts),
        ms_from_time(ts),
    )
}

// ── Chrono-based local time helpers ─────────────────────────────────────────

/// Create DateTime<Utc> from timestamp in ms.
fn ts_to_utc(ts: f64) -> Option<DateTime<chrono::Utc>> {
    let secs = (ts / 1000.0).floor() as i64;
    let sub_ms = ts.rem_euclid(1000.0);
    let nanos = (sub_ms * 1_000_000.0) as u32;
    DateTime::from_timestamp(secs, nanos)
}

fn local_to_ms(res: chrono::LocalResult<DateTime<Local>>) -> f64 {
    match res {
        chrono::LocalResult::Single(dt) => dt.timestamp_millis() as f64,
        chrono::LocalResult::Ambiguous(dt, _) => dt.timestamp_millis() as f64,
        chrono::LocalResult::None => f64::NAN,
    }
}

/// Extract local time components from a timestamp (ms since epoch).
fn local_components(ts: f64) -> (f64, f64, f64, f64, f64, f64, f64) {
    if let Some(dt) = ts_to_utc(ts) {
        let local: DateTime<Local> = dt.into();
        (
            local.year() as f64,
            local.month0() as f64,
            local.day() as f64,
            local.hour() as f64,
            local.minute() as f64,
            local.second() as f64,
            ts.rem_euclid(1000.0),
        )
    } else {
        let offset_ms = Local::now().offset().local_minus_utc() as f64 * 1000.0;
        utc_components(ts + offset_ms)
    }
}

/// Convert local time components to UTC timestamp.
fn local_to_utc_ms(year: f64, month: f64, day: f64, h: f64, min: f64, sec: f64, ms: f64) -> f64 {
    let d = make_day(year, month, day);
    let t = make_time(h, min, sec, ms);
    let date = make_date(d, t);
    if date.is_nan() || !date.is_finite() {
        return f64::NAN;
    }
    let total_secs = (date / 1000.0).floor() as i64;
    let sub_ms = date.rem_euclid(1000.0) as u32;
    let naive =
        chrono::DateTime::from_timestamp(total_secs, sub_ms * 1_000_000).map(|dt| dt.naive_utc());
    if let Some(n) = naive {
        let res = Local.from_local_datetime(&n);
        local_to_ms(res)
    } else {
        let now_offset = Local::now().offset().local_minus_utc() as f64;
        date - (now_offset * 1000.0)
    }
}

// ── Date string parsing (§21.4.3.2) ─────────────────────────────────────────

fn parse_date_string(s: &str) -> f64 {
    let s = s.trim();
    if s.contains("-000000") {
        return f64::NAN;
    }

    let result = if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(s) {
        dt.timestamp_millis() as f64
    } else if (s.starts_with('+') || s.starts_with('-'))
        && s.len() >= 7
        && (s.contains('T') || s.contains('-'))
    {
        let year_str = &s[0..7];
        if let Ok(y) = year_str.parse::<i64>() {
            parse_extended_year(y, &s[7..])
        } else {
            f64::NAN
        }
    } else {
        parse_date_string_internal(s)
    };

    if result.is_nan() { f64::NAN } else { time_clip(result) }
}

fn parse_extended_year(year: i64, rest: &str) -> f64 {
    let y = year as f64;
    let (month, day, hour, min, sec, ms, is_utc) = if rest.is_empty() {
        (1.0, 1.0, 0.0, 0.0, 0.0, 0.0, true)
    } else if let Some(parts) = rest.strip_prefix('-') {
        if parts.len() < 2 {
            return f64::NAN;
        }
        let month: f64 = match parts[0..2].parse::<u32>() {
            Ok(m) => m as f64,
            Err(_) => return f64::NAN,
        };
        if parts.len() == 2 {
            (month, 1.0, 0.0, 0.0, 0.0, 0.0, true)
        } else if parts.len() >= 5 && parts.as_bytes()[2] == b'-' {
            let day: f64 = match parts[3..5].parse::<u32>() {
                Ok(d) => d as f64,
                Err(_) => return f64::NAN,
            };
            if parts.len() == 5 {
                (month, day, 0.0, 0.0, 0.0, 0.0, true)
            } else if parts.len() >= 11 && parts.as_bytes()[5] == b'T' {
                let time_part = &parts[6..];
                let (time_str, utc) = if let Some(stripped) = time_part.strip_suffix('Z') {
                    (stripped, true)
                } else {
                    (time_part, false)
                };
                let tparts: Vec<&str> = time_str.split(':').collect();
                if tparts.len() < 2 {
                    return f64::NAN;
                }
                let h: f64 = tparts[0].parse().unwrap_or(f64::NAN);
                let m: f64 = tparts[1].parse().unwrap_or(f64::NAN);
                let (s_val, ms_val) = if tparts.len() >= 3 {
                    let sec_parts: Vec<&str> = tparts[2].split('.').collect();
                    let sv: f64 = sec_parts[0].parse().unwrap_or(f64::NAN);
                    let msv: f64 = if sec_parts.len() >= 2 {
                        sec_parts[1].parse().unwrap_or(0.0)
                    } else {
                        0.0
                    };
                    (sv, msv)
                } else {
                    (0.0, 0.0)
                };
                (month, day, h, m, s_val, ms_val, utc)
            } else {
                return f64::NAN;
            }
        } else {
            return f64::NAN;
        }
    } else {
        return f64::NAN;
    };

    let d = make_day(y, month - 1.0, day);
    let t = make_time(hour, min, sec, ms);
    let date = make_date(d, t);
    if is_utc {
        date
    } else {
        local_to_utc_ms(y, month - 1.0, day, hour, min, sec, ms)
    }
}

fn parse_date_string_internal(s: &str) -> f64 {
    if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(s) {
        return dt.timestamp_millis() as f64;
    }
    if let Some(base) = s.strip_suffix('Z') {
        if let Ok(dt) = chrono::NaiveDateTime::parse_from_str(base, "%Y-%m-%dT%H:%M:%S") {
            return dt.and_utc().timestamp_millis() as f64;
        }
        if let Ok(dt) = chrono::NaiveDateTime::parse_from_str(base, "%Y-%m-%dT%H:%M:%S%.f") {
            return dt.and_utc().timestamp_millis() as f64;
        }
        if let Ok(dt) = chrono::NaiveDateTime::parse_from_str(base, "%Y-%m-%dT%H:%M") {
            return dt.and_utc().timestamp_millis() as f64;
        }
        if let Ok(d) = NaiveDate::parse_from_str(base, "%Y-%m-%d") {
            return d
                .and_hms_opt(0, 0, 0)
                .unwrap()
                .and_utc()
                .timestamp_millis() as f64;
        }
        return f64::NAN;
    }
    // Local time formats (no Z suffix).
    if let Ok(dt) = chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S") {
        return local_to_ms(Local.from_local_datetime(&dt));
    }
    if let Ok(dt) = chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S%.f") {
        return local_to_ms(Local.from_local_datetime(&dt));
    }
    if let Ok(dt) = chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M") {
        return local_to_ms(Local.from_local_datetime(&dt));
    }
    // Date-only → UTC per ES spec.
    if let Ok(d) = NaiveDate::parse_from_str(s, "%Y-%m-%d") {
        return d
            .and_hms_opt(0, 0, 0)
            .unwrap()
            .and_utc()
            .timestamp_millis() as f64;
    }
    if let Ok(d) = NaiveDate::parse_from_str(s, "%Y-%m") {
        return NaiveDate::from_ymd_opt(d.year(), d.month(), 1)
            .unwrap()
            .and_hms_opt(0, 0, 0)
            .unwrap()
            .and_utc()
            .timestamp_millis() as f64;
    }
    if s.len() == 4 {
        if let Ok(y) = s.parse::<i32>() {
            if let Some(d) = NaiveDate::from_ymd_opt(y, 1, 1) {
                return d.and_hms_opt(0, 0, 0).unwrap().and_utc().timestamp_millis() as f64;
            }
        }
        return f64::NAN;
    }
    // RFC 1123 / toUTCString format: "Thu, 01 Jan 1970 00:00:00 GMT"
    if let Some(stripped) = s.strip_suffix(" GMT") {
        if let Ok(dt) = chrono::NaiveDateTime::parse_from_str(stripped, "%a, %d %b %Y %H:%M:%S") {
            return dt.and_utc().timestamp_millis() as f64;
        }
        return f64::NAN;
    }
    // toString format: "Mon Jan 01 1970 00:00:00 GMT+0000"
    if let Ok(dt) = chrono::DateTime::parse_from_str(s, "%a %b %d %Y %H:%M:%S GMT%z") {
        return dt.timestamp_millis() as f64;
    }
    if let Ok(dt) = chrono::NaiveDateTime::parse_from_str(s, "%a %b %d %Y %H:%M:%S") {
        return local_to_ms(Local.from_local_datetime(&dt));
    }
    if let Ok(d) = NaiveDate::parse_from_str(s, "%a %b %d %Y") {
        if let Some(dt) = d.and_hms_opt(0, 0, 0) {
            return local_to_ms(Local.from_local_datetime(&dt));
        }
    }
    f64::NAN
}

// ── String formatting helpers ────────────────────────────────────────────────

const WEEKDAYS: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
const MONTHS: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];

fn format_year(y: i32) -> String {
    if y < 0 {
        format!("-{:04}", y.abs())
    } else {
        format!("{:04}", y)
    }
}

fn month_name(m: u32) -> &'static str {
    MONTHS[m.saturating_sub(1).min(11) as usize]
}

// ── Internal helpers ─────────────────────────────────────────────────────────

fn current_time_millis() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as f64)
        .unwrap_or(f64::NAN)
}

/// Read the [[DateValue]] from a Date object.
fn date_data(
    value: RegisterValue,
    runtime: &mut RuntimeState,
) -> Result<f64, VmNativeCallError> {
    let Some(handle) = value.as_object_handle().map(ObjectHandle) else {
        return Err(type_error(runtime, "Method requires a Date receiver")?);
    };
    let backing = runtime.intern_property_name(DATE_DATA_SLOT);
    let Some(lookup) = runtime
        .objects()
        .get_property(handle, backing)
        .map_err(|e| VmNativeCallError::Internal(format!("Date data lookup: {e:?}").into()))?
    else {
        return Err(type_error(runtime, "Method requires a Date receiver")?);
    };
    let PropertyValue::Data { value, .. } = lookup.value() else {
        return Err(type_error(runtime, "Method requires a Date receiver")?);
    };
    Ok(value.as_number().unwrap_or(f64::NAN))
}

/// Write [[DateValue]] to a Date object and return it.
fn set_date_data(
    this: &RegisterValue,
    ts: f64,
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let new_ts = time_clip(ts);
    let handle = this.as_object_handle().map(ObjectHandle).ok_or_else(|| {
        VmNativeCallError::Internal("Date set requires object receiver".into())
    })?;
    let backing = runtime.intern_property_name(DATE_DATA_SLOT);
    runtime
        .objects_mut()
        .define_own_property(
            handle,
            backing,
            PropertyValue::data_with_attrs(
                RegisterValue::from_number(new_ts),
                PropertyAttributes::from_flags(true, false, true),
            ),
        )
        .map_err(|e| {
            VmNativeCallError::Internal(format!("Date backing store failed: {e:?}").into())
        })?;
    Ok(RegisterValue::from_number(new_ts))
}

/// Wrap RuntimeState::js_to_number for use in native callbacks.
fn to_number(
    value: RegisterValue,
    runtime: &mut RuntimeState,
) -> Result<f64, VmNativeCallError> {
    runtime.js_to_number(value).map_err(|e| interp_err(e, runtime))
}

fn to_string(
    value: RegisterValue,
    runtime: &mut RuntimeState,
) -> Result<Box<str>, VmNativeCallError> {
    runtime.js_to_string(value).map_err(|e| interp_err(e, runtime))
}

fn to_primitive_default(
    value: RegisterValue,
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    // §21.4.2.1 step 3b: ToPrimitive(v) — default hint.
    // For non-Date objects, default acts like number.
    runtime
        .js_to_primitive_with_hint(value, ToPrimitiveHint::Number)
        .map_err(|e| interp_err(e, runtime))
}

fn interp_err(e: InterpreterError, runtime: &mut RuntimeState) -> VmNativeCallError {
    match e {
        InterpreterError::UncaughtThrow(v) => VmNativeCallError::Thrown(v),
        InterpreterError::TypeError(msg) => match runtime.alloc_type_error(&msg) {
            Ok(h) => VmNativeCallError::Thrown(RegisterValue::from_object_handle(h.0)),
            Err(e) => VmNativeCallError::Internal(format!("{e}").into()),
        },
        other => VmNativeCallError::Internal(format!("{other}").into()),
    }
}

fn type_error(
    runtime: &mut RuntimeState,
    message: &str,
) -> Result<VmNativeCallError, VmNativeCallError> {
    let error = runtime.alloc_type_error(message).map_err(|e| {
        VmNativeCallError::Internal(format!("TypeError alloc failed: {e}").into())
    })?;
    Ok(VmNativeCallError::Thrown(
        RegisterValue::from_object_handle(error.0),
    ))
}

fn range_error(runtime: &mut RuntimeState, message: &str) -> VmNativeCallError {
    let prototype = runtime.intrinsics().range_error_prototype;
    let handle = runtime.alloc_object_with_prototype(Some(prototype));
    let msg = runtime.alloc_string(message);
    let msg_prop = runtime.intern_property_name("message");
    runtime
        .objects_mut()
        .set_property(handle, msg_prop, RegisterValue::from_object_handle(msg.0))
        .ok();
    VmNativeCallError::Thrown(RegisterValue::from_object_handle(handle.0))
}

/// Helper: get optional f64 arg at index, applying ToNumber.
fn opt_number_arg(
    args: &[RegisterValue],
    index: usize,
    runtime: &mut RuntimeState,
) -> Result<Option<f64>, VmNativeCallError> {
    if index < args.len() {
        Ok(Some(to_number(args[index], runtime)?.trunc()))
    } else {
        Ok(None)
    }
}

/// Helper: get required f64 arg at index, applying ToNumber.
fn req_number_arg(
    args: &[RegisterValue],
    index: usize,
    runtime: &mut RuntimeState,
) -> Result<f64, VmNativeCallError> {
    let val = args.get(index).copied().unwrap_or(RegisterValue::undefined());
    Ok(to_number(val, runtime)?.trunc())
}

// ── Intrinsic installer ──────────────────────────────────────────────────────

pub(super) struct DateIntrinsic;

impl IntrinsicInstaller for DateIntrinsic {
    fn init(
        &self,
        intrinsics: &mut VmIntrinsics,
        cx: &mut IntrinsicInstallContext<'_>,
    ) -> Result<(), IntrinsicsError> {
        let descriptor = date_class_descriptor();
        let plan = ClassBuilder::from_descriptor(&descriptor)
            .expect("Date class descriptors should normalize")
            .build();

        let constructor = if let Some(desc) = plan.constructor() {
            let host_function = cx.native_functions.register(desc.clone());
            cx.alloc_intrinsic_host_function(host_function, intrinsics.function_prototype())?
        } else {
            cx.alloc_intrinsic_object(Some(intrinsics.object_prototype()))?
        };

        intrinsics.date_constructor = constructor;
        install_class_plan(
            intrinsics.date_prototype(),
            intrinsics.date_constructor(),
            &plan,
            intrinsics.function_prototype(),
            cx,
        )?;

        // §20.1.3.6 Symbol.toStringTag = "Date"
        let to_string_tag = cx
            .property_names
            .intern_symbol(WellKnownSymbol::ToStringTag.stable_id());
        let tag = cx.heap.alloc_string("Date");
        cx.heap.define_own_property(
            intrinsics.date_prototype(),
            to_string_tag,
            PropertyValue::data_with_attrs(
                RegisterValue::from_object_handle(tag.0),
                PropertyAttributes::from_flags(false, false, true),
            ),
        )?;

        // §21.4.4.45 Symbol.toPrimitive
        install_symbol_method(
            intrinsics.date_prototype(),
            "[Symbol.toPrimitive]",
            WellKnownSymbol::ToPrimitive,
            1,
            date_prototype_to_primitive,
            intrinsics,
            cx,
        )?;

        Ok(())
    }

    fn install_on_global(
        &self,
        intrinsics: &VmIntrinsics,
        cx: &mut IntrinsicInstallContext<'_>,
    ) -> Result<(), IntrinsicsError> {
        cx.install_global_value(
            intrinsics,
            "Date",
            RegisterValue::from_object_handle(intrinsics.date_constructor().0),
        )
    }
}

fn install_symbol_method(
    target: ObjectHandle,
    name: &str,
    symbol: WellKnownSymbol,
    length: u16,
    callback: fn(&RegisterValue, &[RegisterValue], &mut RuntimeState) -> Result<RegisterValue, VmNativeCallError>,
    intrinsics: &VmIntrinsics,
    cx: &mut IntrinsicInstallContext<'_>,
) -> Result<(), IntrinsicsError> {
    let desc = NativeFunctionDescriptor::method(name, length, callback);
    let host_id = cx.native_functions.register(desc);
    let handle = cx.alloc_intrinsic_host_function(host_id, intrinsics.function_prototype())?;
    // Set .name and .length on the function object.
    let name_prop = cx.property_names.intern("name");
    let name_val = cx.heap.alloc_string(name);
    cx.heap.define_own_property(
        handle,
        name_prop,
        PropertyValue::data_with_attrs(
            RegisterValue::from_object_handle(name_val.0),
            PropertyAttributes::from_flags(false, false, true),
        ),
    )?;
    let length_prop = cx.property_names.intern("length");
    cx.heap.define_own_property(
        handle,
        length_prop,
        PropertyValue::data_with_attrs(
            RegisterValue::from_number(length as f64),
            PropertyAttributes::from_flags(false, false, true),
        ),
    )?;
    let sym_prop = cx.property_names.intern_symbol(symbol.stable_id());
    cx.heap.define_own_property(
        target,
        sym_prop,
        PropertyValue::data_with_attrs(
            RegisterValue::from_object_handle(handle.0),
            PropertyAttributes::from_flags(false, false, true),
        ),
    )?;
    Ok(())
}

// ── Class descriptor ─────────────────────────────────────────────────────────

fn date_class_descriptor() -> JsClassDescriptor {
    JsClassDescriptor::new("Date")
        .with_constructor(NativeFunctionDescriptor::constructor("Date", 7, date_constructor))
        // ── Static methods ──
        .with_binding(NativeBindingDescriptor::new(
            NativeBindingTarget::Constructor,
            NativeFunctionDescriptor::method("now", 0, date_now),
        ))
        .with_binding(NativeBindingDescriptor::new(
            NativeBindingTarget::Constructor,
            NativeFunctionDescriptor::method("parse", 1, date_parse),
        ))
        .with_binding(NativeBindingDescriptor::new(
            NativeBindingTarget::Constructor,
            NativeFunctionDescriptor::method("UTC", 7, date_utc),
        ))
        // ── Prototype getters ──
        .with_binding(NativeBindingDescriptor::new(
            NativeBindingTarget::Prototype,
            NativeFunctionDescriptor::method("getTime", 0, date_get_time),
        ))
        .with_binding(NativeBindingDescriptor::new(
            NativeBindingTarget::Prototype,
            NativeFunctionDescriptor::method("valueOf", 0, date_get_time),
        ))
        .with_binding(NativeBindingDescriptor::new(
            NativeBindingTarget::Prototype,
            NativeFunctionDescriptor::method("getFullYear", 0, date_get_full_year),
        ))
        .with_binding(NativeBindingDescriptor::new(
            NativeBindingTarget::Prototype,
            NativeFunctionDescriptor::method("getUTCFullYear", 0, date_get_utc_full_year),
        ))
        .with_binding(NativeBindingDescriptor::new(
            NativeBindingTarget::Prototype,
            NativeFunctionDescriptor::method("getMonth", 0, date_get_month),
        ))
        .with_binding(NativeBindingDescriptor::new(
            NativeBindingTarget::Prototype,
            NativeFunctionDescriptor::method("getUTCMonth", 0, date_get_utc_month),
        ))
        .with_binding(NativeBindingDescriptor::new(
            NativeBindingTarget::Prototype,
            NativeFunctionDescriptor::method("getDate", 0, date_get_date),
        ))
        .with_binding(NativeBindingDescriptor::new(
            NativeBindingTarget::Prototype,
            NativeFunctionDescriptor::method("getUTCDate", 0, date_get_utc_date),
        ))
        .with_binding(NativeBindingDescriptor::new(
            NativeBindingTarget::Prototype,
            NativeFunctionDescriptor::method("getDay", 0, date_get_day),
        ))
        .with_binding(NativeBindingDescriptor::new(
            NativeBindingTarget::Prototype,
            NativeFunctionDescriptor::method("getUTCDay", 0, date_get_utc_day),
        ))
        .with_binding(NativeBindingDescriptor::new(
            NativeBindingTarget::Prototype,
            NativeFunctionDescriptor::method("getHours", 0, date_get_hours),
        ))
        .with_binding(NativeBindingDescriptor::new(
            NativeBindingTarget::Prototype,
            NativeFunctionDescriptor::method("getUTCHours", 0, date_get_utc_hours),
        ))
        .with_binding(NativeBindingDescriptor::new(
            NativeBindingTarget::Prototype,
            NativeFunctionDescriptor::method("getMinutes", 0, date_get_minutes),
        ))
        .with_binding(NativeBindingDescriptor::new(
            NativeBindingTarget::Prototype,
            NativeFunctionDescriptor::method("getUTCMinutes", 0, date_get_utc_minutes),
        ))
        .with_binding(NativeBindingDescriptor::new(
            NativeBindingTarget::Prototype,
            NativeFunctionDescriptor::method("getSeconds", 0, date_get_seconds),
        ))
        .with_binding(NativeBindingDescriptor::new(
            NativeBindingTarget::Prototype,
            NativeFunctionDescriptor::method("getUTCSeconds", 0, date_get_utc_seconds),
        ))
        .with_binding(NativeBindingDescriptor::new(
            NativeBindingTarget::Prototype,
            NativeFunctionDescriptor::method("getMilliseconds", 0, date_get_milliseconds),
        ))
        .with_binding(NativeBindingDescriptor::new(
            NativeBindingTarget::Prototype,
            NativeFunctionDescriptor::method("getUTCMilliseconds", 0, date_get_utc_milliseconds),
        ))
        .with_binding(NativeBindingDescriptor::new(
            NativeBindingTarget::Prototype,
            NativeFunctionDescriptor::method("getTimezoneOffset", 0, date_get_timezone_offset),
        ))
        // ── Prototype setters ──
        .with_binding(NativeBindingDescriptor::new(
            NativeBindingTarget::Prototype,
            NativeFunctionDescriptor::method("setTime", 1, date_set_time),
        ))
        .with_binding(NativeBindingDescriptor::new(
            NativeBindingTarget::Prototype,
            NativeFunctionDescriptor::method("setMilliseconds", 1, date_set_milliseconds),
        ))
        .with_binding(NativeBindingDescriptor::new(
            NativeBindingTarget::Prototype,
            NativeFunctionDescriptor::method("setUTCMilliseconds", 1, date_set_utc_milliseconds),
        ))
        .with_binding(NativeBindingDescriptor::new(
            NativeBindingTarget::Prototype,
            NativeFunctionDescriptor::method("setSeconds", 2, date_set_seconds),
        ))
        .with_binding(NativeBindingDescriptor::new(
            NativeBindingTarget::Prototype,
            NativeFunctionDescriptor::method("setUTCSeconds", 2, date_set_utc_seconds),
        ))
        .with_binding(NativeBindingDescriptor::new(
            NativeBindingTarget::Prototype,
            NativeFunctionDescriptor::method("setMinutes", 3, date_set_minutes),
        ))
        .with_binding(NativeBindingDescriptor::new(
            NativeBindingTarget::Prototype,
            NativeFunctionDescriptor::method("setUTCMinutes", 3, date_set_utc_minutes),
        ))
        .with_binding(NativeBindingDescriptor::new(
            NativeBindingTarget::Prototype,
            NativeFunctionDescriptor::method("setHours", 4, date_set_hours),
        ))
        .with_binding(NativeBindingDescriptor::new(
            NativeBindingTarget::Prototype,
            NativeFunctionDescriptor::method("setUTCHours", 4, date_set_utc_hours),
        ))
        .with_binding(NativeBindingDescriptor::new(
            NativeBindingTarget::Prototype,
            NativeFunctionDescriptor::method("setDate", 1, date_set_date),
        ))
        .with_binding(NativeBindingDescriptor::new(
            NativeBindingTarget::Prototype,
            NativeFunctionDescriptor::method("setUTCDate", 1, date_set_utc_date),
        ))
        .with_binding(NativeBindingDescriptor::new(
            NativeBindingTarget::Prototype,
            NativeFunctionDescriptor::method("setMonth", 2, date_set_month),
        ))
        .with_binding(NativeBindingDescriptor::new(
            NativeBindingTarget::Prototype,
            NativeFunctionDescriptor::method("setUTCMonth", 2, date_set_utc_month),
        ))
        .with_binding(NativeBindingDescriptor::new(
            NativeBindingTarget::Prototype,
            NativeFunctionDescriptor::method("setFullYear", 3, date_set_full_year),
        ))
        .with_binding(NativeBindingDescriptor::new(
            NativeBindingTarget::Prototype,
            NativeFunctionDescriptor::method("setUTCFullYear", 3, date_set_utc_full_year),
        ))
        // ── String methods ──
        .with_binding(NativeBindingDescriptor::new(
            NativeBindingTarget::Prototype,
            NativeFunctionDescriptor::method("toString", 0, date_to_string),
        ))
        .with_binding(NativeBindingDescriptor::new(
            NativeBindingTarget::Prototype,
            NativeFunctionDescriptor::method("toDateString", 0, date_to_date_string),
        ))
        .with_binding(NativeBindingDescriptor::new(
            NativeBindingTarget::Prototype,
            NativeFunctionDescriptor::method("toTimeString", 0, date_to_time_string),
        ))
        .with_binding(NativeBindingDescriptor::new(
            NativeBindingTarget::Prototype,
            NativeFunctionDescriptor::method("toISOString", 0, date_to_iso_string),
        ))
        .with_binding(NativeBindingDescriptor::new(
            NativeBindingTarget::Prototype,
            NativeFunctionDescriptor::method("toUTCString", 0, date_to_utc_string),
        ))
        .with_binding(NativeBindingDescriptor::new(
            NativeBindingTarget::Prototype,
            NativeFunctionDescriptor::method("toJSON", 1, date_to_json),
        ))
        .with_binding(NativeBindingDescriptor::new(
            NativeBindingTarget::Prototype,
            NativeFunctionDescriptor::method("toLocaleString", 0, date_to_string),
        ))
        .with_binding(NativeBindingDescriptor::new(
            NativeBindingTarget::Prototype,
            NativeFunctionDescriptor::method("toLocaleDateString", 0, date_to_date_string),
        ))
        .with_binding(NativeBindingDescriptor::new(
            NativeBindingTarget::Prototype,
            NativeFunctionDescriptor::method("toLocaleTimeString", 0, date_to_time_string),
        ))
        // ── Legacy (Annex B) ──
        .with_binding(NativeBindingDescriptor::new(
            NativeBindingTarget::Prototype,
            NativeFunctionDescriptor::method("getYear", 0, date_get_year),
        ))
        .with_binding(NativeBindingDescriptor::new(
            NativeBindingTarget::Prototype,
            NativeFunctionDescriptor::method("setYear", 1, date_set_year),
        ))
        .with_binding(NativeBindingDescriptor::new(
            NativeBindingTarget::Prototype,
            NativeFunctionDescriptor::method("toGMTString", 0, date_to_utc_string),
        ))
}

// ── Constructor (§21.4.2) ────────────────────────────────────────────────────

fn date_constructor(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    // §21.4.2: Called as function → return string.
    if !runtime.is_current_native_construct_call() {
        let now = chrono::Local::now();
        let text = now.format("%a %b %d %Y %H:%M:%S GMT%z").to_string();
        let handle = runtime.alloc_string(text);
        return Ok(RegisterValue::from_object_handle(handle.0));
    }

    let receiver = this.as_object_handle().map(ObjectHandle).ok_or_else(|| {
        VmNativeCallError::Internal("Date constructor missing construct receiver".into())
    })?;

    let timestamp = match args.len() {
        // §21.4.2.1 new Date() — current time.
        0 => current_time_millis(),

        // §21.4.2.1 new Date(value) — single value.
        1 => {
            // Check if arg is a Date object (extract [[DateValue]] directly).
            if let Ok(ts) = date_data(args[0], runtime) {
                time_clip(ts)
            } else {
                // ToPrimitive, then: string → parse, else → ToNumber.
                let prim = to_primitive_default(args[0], runtime)?;
                if let Some(handle) = prim.as_object_handle().map(ObjectHandle) {
                    if let Ok(Some(s)) = runtime.objects().string_value(handle) {
                        time_clip(parse_date_string(&s.to_string()))
                    } else {
                        let n = to_number(prim, runtime)?;
                        time_clip(n)
                    }
                } else if prim.as_number().is_some() || prim == RegisterValue::undefined()
                    || prim == RegisterValue::null() || prim.as_bool().is_some()
                {
                    let n = to_number(prim, runtime)?;
                    time_clip(n)
                } else {
                    // Should not happen after ToPrimitive, but fallback.
                    let n = to_number(prim, runtime)?;
                    time_clip(n)
                }
            }
        }

        // §21.4.2.1 new Date(year, month [, date [, hours [, min [, sec [, ms]]]]]) — components.
        _ => {
            let year = to_number(args[0], runtime)?.trunc();
            let month = to_number(args[1], runtime)?.trunc();
            let day = if args.len() > 2 {
                to_number(args[2], runtime)?.trunc()
            } else {
                1.0
            };
            let hour = if args.len() > 3 {
                to_number(args[3], runtime)?.trunc()
            } else {
                0.0
            };
            let min = if args.len() > 4 {
                to_number(args[4], runtime)?.trunc()
            } else {
                0.0
            };
            let sec = if args.len() > 5 {
                to_number(args[5], runtime)?.trunc()
            } else {
                0.0
            };
            let ms = if args.len() > 6 {
                to_number(args[6], runtime)?.trunc()
            } else {
                0.0
            };
            // §21.4.2.1 step 7: 0-99 → 1900+year.
            let full_year = if (0.0..=99.0).contains(&year) {
                1900.0 + year
            } else {
                year
            };
            // Interpreted as local time → convert to UTC.
            time_clip(local_to_utc_ms(full_year, month, day, hour, min, sec, ms))
        }
    };

    let backing = runtime.intern_property_name(DATE_DATA_SLOT);
    runtime
        .objects_mut()
        .define_own_property(
            receiver,
            backing,
            PropertyValue::data_with_attrs(
                RegisterValue::from_number(timestamp),
                PropertyAttributes::from_flags(true, false, true),
            ),
        )
        .map_err(|e| {
            VmNativeCallError::Internal(format!("Date constructor backing failed: {e:?}").into())
        })?;
    Ok(*this)
}

// ── Static methods (§21.4.3) ─────────────────────────────────────────────────

/// §21.4.3.1 Date.now()
fn date_now(
    _this: &RegisterValue,
    _args: &[RegisterValue],
    _runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    Ok(RegisterValue::from_number(current_time_millis()))
}

/// §21.4.3.2 Date.parse(string)
fn date_parse(
    _this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let s = if args.is_empty() {
        "undefined".into()
    } else {
        to_string(args[0], runtime)?
    };
    Ok(RegisterValue::from_number(parse_date_string(&s)))
}

/// §21.4.3.4 Date.UTC(year, month, ...)
fn date_utc(
    _this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let year = to_number(args.first().copied().unwrap_or(RegisterValue::undefined()), runtime)?.trunc();
    let month = if args.len() > 1 { to_number(args[1], runtime)?.trunc() } else { 0.0 };
    let date = if args.len() > 2 { to_number(args[2], runtime)?.trunc() } else { 1.0 };
    let hours = if args.len() > 3 { to_number(args[3], runtime)?.trunc() } else { 0.0 };
    let minutes = if args.len() > 4 { to_number(args[4], runtime)?.trunc() } else { 0.0 };
    let seconds = if args.len() > 5 { to_number(args[5], runtime)?.trunc() } else { 0.0 };
    let ms = if args.len() > 6 { to_number(args[6], runtime)?.trunc() } else { 0.0 };

    if year.is_nan() || month.is_nan() || date.is_nan() || hours.is_nan()
        || minutes.is_nan() || seconds.is_nan() || ms.is_nan()
    {
        return Ok(RegisterValue::from_number(f64::NAN));
    }

    let full_year = if (0.0..=99.0).contains(&year) { 1900.0 + year } else { year };
    let t = make_time(hours, minutes, seconds, ms);
    let d = make_day(full_year, month, date);
    Ok(RegisterValue::from_number(time_clip(make_date(d, t))))
}

// ── Prototype getters (§21.4.4) ──────────────────────────────────────────────

/// §21.4.4.10 getTime / valueOf
fn date_get_time(
    this: &RegisterValue,
    _args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    Ok(RegisterValue::from_number(date_data(*this, runtime)?))
}

/// §21.4.4.4 getFullYear
fn date_get_full_year(
    this: &RegisterValue,
    _args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let ts = date_data(*this, runtime)?;
    if ts.is_nan() {
        return Ok(RegisterValue::from_number(f64::NAN));
    }
    let (y, _, _, _, _, _, _) = local_components(ts);
    Ok(RegisterValue::from_number(y))
}

/// §21.4.4.11 getUTCFullYear
fn date_get_utc_full_year(
    this: &RegisterValue,
    _args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let ts = date_data(*this, runtime)?;
    if ts.is_nan() {
        return Ok(RegisterValue::from_number(f64::NAN));
    }
    Ok(RegisterValue::from_number(year_from_time(ts)))
}

/// §21.4.4.7 getMonth
fn date_get_month(
    this: &RegisterValue,
    _args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let ts = date_data(*this, runtime)?;
    if ts.is_nan() {
        return Ok(RegisterValue::from_number(f64::NAN));
    }
    let (_, m, _, _, _, _, _) = local_components(ts);
    Ok(RegisterValue::from_number(m))
}

/// §21.4.4.14 getUTCMonth
fn date_get_utc_month(
    this: &RegisterValue,
    _args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let ts = date_data(*this, runtime)?;
    if ts.is_nan() {
        return Ok(RegisterValue::from_number(f64::NAN));
    }
    Ok(RegisterValue::from_number(month_from_time(ts)))
}

/// §21.4.4.2 getDate
fn date_get_date(
    this: &RegisterValue,
    _args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let ts = date_data(*this, runtime)?;
    if ts.is_nan() {
        return Ok(RegisterValue::from_number(f64::NAN));
    }
    let (_, _, d, _, _, _, _) = local_components(ts);
    Ok(RegisterValue::from_number(d))
}

/// §21.4.4.12 getUTCDate
fn date_get_utc_date(
    this: &RegisterValue,
    _args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let ts = date_data(*this, runtime)?;
    if ts.is_nan() {
        return Ok(RegisterValue::from_number(f64::NAN));
    }
    Ok(RegisterValue::from_number(date_from_time(ts)))
}

/// §21.4.4.3 getDay
fn date_get_day(
    this: &RegisterValue,
    _args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let ts = date_data(*this, runtime)?;
    if ts.is_nan() {
        return Ok(RegisterValue::from_number(f64::NAN));
    }
    if let Some(dt) = ts_to_utc(ts) {
        let local: DateTime<Local> = dt.into();
        Ok(RegisterValue::from_number(
            local.weekday().num_days_from_sunday() as f64,
        ))
    } else {
        Ok(RegisterValue::from_number(week_day(ts)))
    }
}

/// §21.4.4.13 getUTCDay
fn date_get_utc_day(
    this: &RegisterValue,
    _args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let ts = date_data(*this, runtime)?;
    if ts.is_nan() {
        return Ok(RegisterValue::from_number(f64::NAN));
    }
    Ok(RegisterValue::from_number(week_day(ts)))
}

/// §21.4.4.5 getHours
fn date_get_hours(
    this: &RegisterValue,
    _args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let ts = date_data(*this, runtime)?;
    if ts.is_nan() {
        return Ok(RegisterValue::from_number(f64::NAN));
    }
    let (_, _, _, h, _, _, _) = local_components(ts);
    Ok(RegisterValue::from_number(h))
}

/// §21.4.4.15 getUTCHours
fn date_get_utc_hours(
    this: &RegisterValue,
    _args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let ts = date_data(*this, runtime)?;
    if ts.is_nan() {
        return Ok(RegisterValue::from_number(f64::NAN));
    }
    Ok(RegisterValue::from_number(hour_from_time(ts)))
}

/// §21.4.4.6 getMinutes
fn date_get_minutes(
    this: &RegisterValue,
    _args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let ts = date_data(*this, runtime)?;
    if ts.is_nan() {
        return Ok(RegisterValue::from_number(f64::NAN));
    }
    let (_, _, _, _, min, _, _) = local_components(ts);
    Ok(RegisterValue::from_number(min))
}

/// §21.4.4.16 getUTCMinutes
fn date_get_utc_minutes(
    this: &RegisterValue,
    _args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let ts = date_data(*this, runtime)?;
    if ts.is_nan() {
        return Ok(RegisterValue::from_number(f64::NAN));
    }
    Ok(RegisterValue::from_number(min_from_time(ts)))
}

/// §21.4.4.8 getSeconds
fn date_get_seconds(
    this: &RegisterValue,
    _args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let ts = date_data(*this, runtime)?;
    if ts.is_nan() {
        return Ok(RegisterValue::from_number(f64::NAN));
    }
    let (_, _, _, _, _, sec, _) = local_components(ts);
    Ok(RegisterValue::from_number(sec))
}

/// §21.4.4.17 getUTCSeconds
fn date_get_utc_seconds(
    this: &RegisterValue,
    _args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let ts = date_data(*this, runtime)?;
    if ts.is_nan() {
        return Ok(RegisterValue::from_number(f64::NAN));
    }
    Ok(RegisterValue::from_number(sec_from_time(ts)))
}

/// §21.4.4.9 getMilliseconds
fn date_get_milliseconds(
    this: &RegisterValue,
    _args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let ts = date_data(*this, runtime)?;
    if ts.is_nan() {
        return Ok(RegisterValue::from_number(f64::NAN));
    }
    Ok(RegisterValue::from_number(ms_from_time(ts)))
}

/// §21.4.4.18 getUTCMilliseconds
fn date_get_utc_milliseconds(
    this: &RegisterValue,
    _args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let ts = date_data(*this, runtime)?;
    if ts.is_nan() {
        return Ok(RegisterValue::from_number(f64::NAN));
    }
    Ok(RegisterValue::from_number(ms_from_time(ts)))
}

/// §21.4.4.19 getTimezoneOffset
fn date_get_timezone_offset(
    this: &RegisterValue,
    _args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let ts = date_data(*this, runtime)?;
    if ts.is_nan() {
        return Ok(RegisterValue::from_number(f64::NAN));
    }
    if let Some(dt) = ts_to_utc(ts) {
        let local: DateTime<Local> = dt.into();
        let offset_secs = local.offset().local_minus_utc();
        Ok(RegisterValue::from_number((-offset_secs / 60) as f64))
    } else {
        Ok(RegisterValue::from_number(f64::NAN))
    }
}

// ── Prototype setters (§21.4.4) ──────────────────────────────────────────────

/// §21.4.4.27 setTime
fn date_set_time(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let _ts = date_data(*this, runtime)?; // Validate receiver.
    let t = req_number_arg(args, 0, runtime)?;
    set_date_data(this, t, runtime)
}

/// §21.4.4.24 setMilliseconds — local time
fn date_set_milliseconds(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let ts = date_data(*this, runtime)?;
    let ms = req_number_arg(args, 0, runtime)?;
    if ts.is_nan() {
        return Ok(RegisterValue::from_number(f64::NAN));
    }
    let (y, m, d, h, min, sec, _) = local_components(ts);
    set_date_data(this, local_to_utc_ms(y, m, d, h, min, sec, ms), runtime)
}

/// §21.4.4.31 setUTCMilliseconds
fn date_set_utc_milliseconds(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let ts = date_data(*this, runtime)?;
    let ms = req_number_arg(args, 0, runtime)?;
    if ts.is_nan() {
        return Ok(RegisterValue::from_number(f64::NAN));
    }
    let (y, m, d, h, min, sec, _) = utc_components(ts);
    let day = make_day(y, m, d);
    let time = make_time(h, min, sec, ms);
    set_date_data(this, make_date(day, time), runtime)
}

/// §21.4.4.26 setSeconds — local time
fn date_set_seconds(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let ts = date_data(*this, runtime)?;
    let sec = req_number_arg(args, 0, runtime)?;
    let ms = opt_number_arg(args, 1, runtime)?;
    if ts.is_nan() {
        return Ok(RegisterValue::from_number(f64::NAN));
    }
    let (y, m, d, h, min, cur_sec, cur_ms) = local_components(ts);
    let _ = cur_sec;
    set_date_data(
        this,
        local_to_utc_ms(y, m, d, h, min, sec, ms.unwrap_or(cur_ms)),
        runtime,
    )
}

/// §21.4.4.33 setUTCSeconds
fn date_set_utc_seconds(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let ts = date_data(*this, runtime)?;
    let sec = req_number_arg(args, 0, runtime)?;
    let ms = opt_number_arg(args, 1, runtime)?;
    if ts.is_nan() {
        return Ok(RegisterValue::from_number(f64::NAN));
    }
    let (y, m, d, h, min, _, cur_ms) = utc_components(ts);
    let day = make_day(y, m, d);
    let time = make_time(h, min, sec, ms.unwrap_or(cur_ms));
    set_date_data(this, make_date(day, time), runtime)
}

/// §21.4.4.25 setMinutes — local time
fn date_set_minutes(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let ts = date_data(*this, runtime)?;
    let min = req_number_arg(args, 0, runtime)?;
    let sec = opt_number_arg(args, 1, runtime)?;
    let ms = opt_number_arg(args, 2, runtime)?;
    if ts.is_nan() {
        return Ok(RegisterValue::from_number(f64::NAN));
    }
    let (y, m, d, h, _, cur_sec, cur_ms) = local_components(ts);
    set_date_data(
        this,
        local_to_utc_ms(y, m, d, h, min, sec.unwrap_or(cur_sec), ms.unwrap_or(cur_ms)),
        runtime,
    )
}

/// §21.4.4.32 setUTCMinutes
fn date_set_utc_minutes(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let ts = date_data(*this, runtime)?;
    let min = req_number_arg(args, 0, runtime)?;
    let sec = opt_number_arg(args, 1, runtime)?;
    let ms = opt_number_arg(args, 2, runtime)?;
    if ts.is_nan() {
        return Ok(RegisterValue::from_number(f64::NAN));
    }
    let (y, m, d, h, _, cur_sec, cur_ms) = utc_components(ts);
    let day = make_day(y, m, d);
    let time = make_time(h, min, sec.unwrap_or(cur_sec), ms.unwrap_or(cur_ms));
    set_date_data(this, make_date(day, time), runtime)
}

/// §21.4.4.21 setHours — local time
fn date_set_hours(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let ts = date_data(*this, runtime)?;
    let hour = req_number_arg(args, 0, runtime)?;
    let min = opt_number_arg(args, 1, runtime)?;
    let sec = opt_number_arg(args, 2, runtime)?;
    let ms = opt_number_arg(args, 3, runtime)?;
    if ts.is_nan() {
        return Ok(RegisterValue::from_number(f64::NAN));
    }
    let (y, m, d, _, cur_min, cur_sec, cur_ms) = local_components(ts);
    set_date_data(
        this,
        local_to_utc_ms(
            y, m, d, hour,
            min.unwrap_or(cur_min),
            sec.unwrap_or(cur_sec),
            ms.unwrap_or(cur_ms),
        ),
        runtime,
    )
}

/// §21.4.4.28 setUTCHours
fn date_set_utc_hours(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let ts = date_data(*this, runtime)?;
    let hour = req_number_arg(args, 0, runtime)?;
    let min = opt_number_arg(args, 1, runtime)?;
    let sec = opt_number_arg(args, 2, runtime)?;
    let ms = opt_number_arg(args, 3, runtime)?;
    if ts.is_nan() {
        return Ok(RegisterValue::from_number(f64::NAN));
    }
    let (y, m, d, _, cur_min, cur_sec, cur_ms) = utc_components(ts);
    let day = make_day(y, m, d);
    let time = make_time(
        hour,
        min.unwrap_or(cur_min),
        sec.unwrap_or(cur_sec),
        ms.unwrap_or(cur_ms),
    );
    set_date_data(this, make_date(day, time), runtime)
}

/// §21.4.4.20 setDate — local time
fn date_set_date(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let ts = date_data(*this, runtime)?;
    let date_arg = req_number_arg(args, 0, runtime)?;
    if ts.is_nan() {
        return Ok(RegisterValue::from_number(f64::NAN));
    }
    let (y, m, _, h, min, sec, ms) = local_components(ts);
    set_date_data(this, local_to_utc_ms(y, m, date_arg, h, min, sec, ms), runtime)
}

/// §21.4.4.29 setUTCDate
fn date_set_utc_date(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let ts = date_data(*this, runtime)?;
    let date_arg = req_number_arg(args, 0, runtime)?;
    if ts.is_nan() {
        return Ok(RegisterValue::from_number(f64::NAN));
    }
    let (y, m, _, h, min, sec, ms) = utc_components(ts);
    let day = make_day(y, m, date_arg);
    let time = make_time(h, min, sec, ms);
    set_date_data(this, make_date(day, time), runtime)
}

/// §21.4.4.22 setMonth — local time
fn date_set_month(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let ts = date_data(*this, runtime)?;
    let mon = req_number_arg(args, 0, runtime)?;
    let date_arg = opt_number_arg(args, 1, runtime)?;
    if ts.is_nan() {
        return Ok(RegisterValue::from_number(f64::NAN));
    }
    let (y, _, d, h, min, sec, ms) = local_components(ts);
    set_date_data(
        this,
        local_to_utc_ms(y, mon, date_arg.unwrap_or(d), h, min, sec, ms),
        runtime,
    )
}

/// §21.4.4.30 setUTCMonth
fn date_set_utc_month(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let ts = date_data(*this, runtime)?;
    let mon = req_number_arg(args, 0, runtime)?;
    let date_arg = opt_number_arg(args, 1, runtime)?;
    if ts.is_nan() {
        return Ok(RegisterValue::from_number(f64::NAN));
    }
    let (y, _, d, h, min, sec, ms) = utc_components(ts);
    let day = make_day(y, mon, date_arg.unwrap_or(d));
    let time = make_time(h, min, sec, ms);
    set_date_data(this, make_date(day, time), runtime)
}

/// §21.4.4.23 setFullYear — local time
fn date_set_full_year(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let ts = date_data(*this, runtime)?;
    // Per spec: If t is NaN, let t = +0.
    let t = if ts.is_nan() { 0.0 } else { ts };
    let y = req_number_arg(args, 0, runtime)?;
    let mon = opt_number_arg(args, 1, runtime)?;
    let date_arg = opt_number_arg(args, 2, runtime)?;
    let (_, cur_m, cur_d, h, min, sec, ms) = local_components(t);
    set_date_data(
        this,
        local_to_utc_ms(y, mon.unwrap_or(cur_m), date_arg.unwrap_or(cur_d), h, min, sec, ms),
        runtime,
    )
}

/// §21.4.4.34 setUTCFullYear
fn date_set_utc_full_year(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let ts = date_data(*this, runtime)?;
    let t = if ts.is_nan() { 0.0 } else { ts };
    let y = req_number_arg(args, 0, runtime)?;
    let mon = opt_number_arg(args, 1, runtime)?;
    let date_arg = opt_number_arg(args, 2, runtime)?;
    let (_, cur_m, cur_d, h, min, sec, ms) = utc_components(t);
    let day = make_day(y, mon.unwrap_or(cur_m), date_arg.unwrap_or(cur_d));
    let time = make_time(h, min, sec, ms);
    set_date_data(this, make_date(day, time), runtime)
}

// ── String methods (§21.4.4) ─────────────────────────────────────────────────

/// §21.4.4.41 toString
fn date_to_string(
    this: &RegisterValue,
    _args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let ts = date_data(*this, runtime)?;
    if ts.is_nan() {
        let h = runtime.alloc_string("Invalid Date");
        return Ok(RegisterValue::from_object_handle(h.0));
    }
    let Some(dt) = ts_to_utc(ts) else {
        let h = runtime.alloc_string("Invalid Date");
        return Ok(RegisterValue::from_object_handle(h.0));
    };
    let local: DateTime<Local> = dt.into();
    let year_str = format_year(local.year());
    let offset_secs = local.offset().local_minus_utc();
    let sign = if offset_secs >= 0 { '+' } else { '-' };
    let abs_offset = offset_secs.abs();
    let s = format!(
        "{} {} {:02} {} {:02}:{:02}:{:02} GMT{}{:02}{:02}",
        WEEKDAYS[local.weekday().num_days_from_sunday() as usize],
        month_name(local.month()),
        local.day(),
        year_str,
        local.hour(),
        local.minute(),
        local.second(),
        sign,
        abs_offset / 3600,
        (abs_offset % 3600) / 60,
    );
    let h = runtime.alloc_string(s);
    Ok(RegisterValue::from_object_handle(h.0))
}

/// §21.4.4.35 toDateString
fn date_to_date_string(
    this: &RegisterValue,
    _args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let ts = date_data(*this, runtime)?;
    if ts.is_nan() {
        let h = runtime.alloc_string("Invalid Date");
        return Ok(RegisterValue::from_object_handle(h.0));
    }
    let Some(dt) = ts_to_utc(ts) else {
        let h = runtime.alloc_string("Invalid Date");
        return Ok(RegisterValue::from_object_handle(h.0));
    };
    let local: DateTime<Local> = dt.into();
    let s = format!(
        "{} {} {:02} {}",
        WEEKDAYS[local.weekday().num_days_from_sunday() as usize],
        month_name(local.month()),
        local.day(),
        format_year(local.year()),
    );
    let h = runtime.alloc_string(s);
    Ok(RegisterValue::from_object_handle(h.0))
}

/// §21.4.4.40 toTimeString
fn date_to_time_string(
    this: &RegisterValue,
    _args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let ts = date_data(*this, runtime)?;
    if ts.is_nan() {
        let h = runtime.alloc_string("Invalid Date");
        return Ok(RegisterValue::from_object_handle(h.0));
    }
    let Some(dt) = ts_to_utc(ts) else {
        let h = runtime.alloc_string("Invalid Date");
        return Ok(RegisterValue::from_object_handle(h.0));
    };
    let local: DateTime<Local> = dt.into();
    let offset_secs = local.offset().local_minus_utc();
    let sign = if offset_secs >= 0 { '+' } else { '-' };
    let abs_offset = offset_secs.abs();
    let s = format!(
        "{:02}:{:02}:{:02} GMT{}{:02}{:02}",
        local.hour(),
        local.minute(),
        local.second(),
        sign,
        abs_offset / 3600,
        (abs_offset % 3600) / 60,
    );
    let h = runtime.alloc_string(s);
    Ok(RegisterValue::from_object_handle(h.0))
}

/// §21.4.4.36 toISOString
fn date_to_iso_string(
    this: &RegisterValue,
    _args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let ts = date_data(*this, runtime)?;
    if ts.is_nan() || !ts.is_finite() || ts.abs() > 8_640_000_000_000_000.0 {
        return Err(range_error(runtime, "Invalid time value"));
    }
    let (y, m, d, h, min, sec, ms_val) = utc_components(ts);
    if y.is_nan() {
        return Err(range_error(runtime, "Invalid time value"));
    }
    let yi = y as i64;
    let year_str = if yi < 0 {
        format!("-{:06}", yi.abs())
    } else if yi > 9999 {
        format!("+{:06}", yi)
    } else {
        format!("{:04}", yi)
    };
    let s = format!(
        "{}-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}Z",
        year_str,
        m as u32 + 1,
        d as u32,
        h as u32,
        min as u32,
        sec as u32,
        ms_val as u32,
    );
    let h = runtime.alloc_string(s);
    Ok(RegisterValue::from_object_handle(h.0))
}

/// §21.4.4.43 toUTCString
fn date_to_utc_string(
    this: &RegisterValue,
    _args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let ts = date_data(*this, runtime)?;
    if ts.is_nan() {
        let h = runtime.alloc_string("Invalid Date");
        return Ok(RegisterValue::from_object_handle(h.0));
    }
    let Some(dt) = ts_to_utc(ts) else {
        let h = runtime.alloc_string("Invalid Date");
        return Ok(RegisterValue::from_object_handle(h.0));
    };
    let year_str = if dt.year() < 0 {
        format!("-{:04}", dt.year().abs())
    } else {
        format!("{:04}", dt.year())
    };
    let s = format!(
        "{}, {:02} {} {} {:02}:{:02}:{:02} GMT",
        WEEKDAYS[dt.weekday().num_days_from_sunday() as usize],
        dt.day(),
        month_name(dt.month()),
        year_str,
        dt.hour(),
        dt.minute(),
        dt.second(),
    );
    let h = runtime.alloc_string(s);
    Ok(RegisterValue::from_object_handle(h.0))
}

/// §21.4.4.37 toJSON
fn date_to_json(
    this: &RegisterValue,
    _args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    // 1. Let O = ? ToObject(this value).
    // 2. Let tv = ? ToPrimitive(O, number).
    let tv = runtime
        .js_to_primitive_with_hint(*this, ToPrimitiveHint::Number)
        .map_err(|e| interp_err(e, runtime))?;
    // 3. If Type(tv) is Number and tv is not finite, return null.
    if let Some(n) = tv.as_number() {
        if n.is_nan() || n.is_infinite() {
            return Ok(RegisterValue::null());
        }
    }
    // 4. Return ? Invoke(O, "toISOString").
    date_to_iso_string(this, &[], runtime)
}

// ── Symbol.toPrimitive (§21.4.4.45) ─────────────────────────────────────────

/// Date.prototype[@@toPrimitive](hint)
/// <https://tc39.es/ecma262/#sec-date.prototype-%symbol.toprimitive%>
fn date_prototype_to_primitive(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    // 1. If this is not an Object, throw TypeError.
    if this.as_object_handle().is_none() {
        return Err(type_error(runtime, "Symbol.toPrimitive requires an object receiver")?);
    }

    // 2. Get hint string.
    let hint_val = args.first().copied().unwrap_or(RegisterValue::undefined());
    let hint = to_string(hint_val, runtime)?;

    // 3. Based on hint, select method order.
    let method_names: &[&str] = match hint.as_ref() {
        "string" | "default" => &["toString", "valueOf"],
        "number" => &["valueOf", "toString"],
        _ => {
            return Err(type_error(runtime, "Invalid Symbol.toPrimitive hint")?);
        }
    };

    // 4. OrdinaryToPrimitive with the ordered method list.
    for name in method_names {
        let prop = runtime.intern_property_name(name);
        let handle = this.as_object_handle().map(ObjectHandle).unwrap();
        if let Ok(Some(lookup)) = runtime.objects().get_property(handle, prop) {
            if let PropertyValue::Data { value: func_val, .. } = lookup.value() {
                if let Some(fn_handle) = func_val.as_object_handle().map(ObjectHandle) {
                    if runtime.objects().is_callable(fn_handle) {
                        let result = runtime
                            .call_callable(fn_handle, *this, &[])
                            .map_err(|e| match e {
                                VmNativeCallError::Thrown(v) => VmNativeCallError::Thrown(v),
                                other => other,
                            })?;
                        // If result is not an object, return it.
                        if result.as_object_handle().is_none() || result.as_number().is_some()
                            || result.as_bool().is_some() || result == RegisterValue::undefined()
                            || result == RegisterValue::null()
                        {
                            return Ok(result);
                        }
                        // Check if it's a string (string handles are object handles).
                        if let Some(h) = result.as_object_handle().map(ObjectHandle) {
                            if runtime.objects().string_value(h).ok().flatten().is_some() {
                                return Ok(result);
                            }
                        }
                    }
                }
            }
        }
    }

    Err(type_error(runtime, "Cannot convert object to primitive value")?)
}

// ── Legacy methods (Annex B) ─────────────────────────────────────────────────

/// §B.2.4.1 getYear
fn date_get_year(
    this: &RegisterValue,
    _args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let ts = date_data(*this, runtime)?;
    if ts.is_nan() {
        return Ok(RegisterValue::from_number(f64::NAN));
    }
    let (y, _, _, _, _, _, _) = local_components(ts);
    Ok(RegisterValue::from_number(y - 1900.0))
}

/// §B.2.4.2 setYear
fn date_set_year(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let ts = date_data(*this, runtime)?;
    let y = req_number_arg(args, 0, runtime)?;
    if y.is_nan() {
        return set_date_data(this, f64::NAN, runtime);
    }
    let t = if ts.is_nan() { 0.0 } else { ts };
    let full_year = if (0.0..=99.0).contains(&y) { 1900.0 + y } else { y };
    let (_, cur_m, cur_d, h, min, sec, ms) = local_components(t);
    set_date_data(this, local_to_utc_ms(full_year, cur_m, cur_d, h, min, sec, ms), runtime)
}
