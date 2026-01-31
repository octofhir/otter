//! Date built-in
//!
//! Provides Date constructor and all Date.prototype methods:
//! - Static: now, parse, UTC
//! - Getters: getFullYear, getMonth, getDate, getDay, getHours, getMinutes, getSeconds, getMilliseconds
//! - UTC Getters: getUTCFullYear, getUTCMonth, getUTCDate, getUTCDay, getUTCHours, getUTCMinutes, getUTCSeconds, getUTCMilliseconds
//! - Other: getTime, getTimezoneOffset
//! - Setters: setFullYear, setMonth, setDate, setHours, setMinutes, setSeconds, setMilliseconds, setTime
//! - UTC Setters: setUTCFullYear, setUTCMonth, setUTCDate, setUTCHours, setUTCMinutes, setUTCSeconds, setUTCMilliseconds
//! - Formatting: toString, toDateString, toTimeString, toISOString, toUTCString, toJSON, valueOf
//! - Locale: toLocaleDateString, toLocaleTimeString, toLocaleString

use chrono::{DateTime, Datelike, Local, NaiveDateTime, TimeZone, Timelike, Utc};
use otter_vm_core::string::JsString;
use otter_vm_core::value::Value;
use otter_vm_core::{VmError, memory};
use otter_vm_runtime::{Op, op_native_with_mm as op_native};
use std::sync::Arc;

/// Get Date ops for extension registration
pub fn ops() -> Vec<Op> {
    vec![
        // Static methods
        op_native("__Date_now", date_now),
        op_native("__Date_parse", date_parse),
        op_native("__Date_UTC", date_utc),
        // Local getters
        op_native("__Date_getFullYear", date_get_full_year),
        op_native("__Date_getMonth", date_get_month),
        op_native("__Date_getDate", date_get_date),
        op_native("__Date_getDay", date_get_day),
        op_native("__Date_getHours", date_get_hours),
        op_native("__Date_getMinutes", date_get_minutes),
        op_native("__Date_getSeconds", date_get_seconds),
        op_native("__Date_getMilliseconds", date_get_milliseconds),
        // UTC getters
        op_native("__Date_getUTCFullYear", date_get_utc_full_year),
        op_native("__Date_getUTCMonth", date_get_utc_month),
        op_native("__Date_getUTCDate", date_get_utc_date),
        op_native("__Date_getUTCDay", date_get_utc_day),
        op_native("__Date_getUTCHours", date_get_utc_hours),
        op_native("__Date_getUTCMinutes", date_get_utc_minutes),
        op_native("__Date_getUTCSeconds", date_get_utc_seconds),
        op_native("__Date_getUTCMilliseconds", date_get_utc_milliseconds),
        // Other getters
        op_native("__Date_getTime", date_get_time),
        op_native("__Date_getTimezoneOffset", date_get_timezone_offset),
        // Local setters
        op_native("__Date_setFullYear", date_set_full_year),
        op_native("__Date_setMonth", date_set_month),
        op_native("__Date_setDate", date_set_date),
        op_native("__Date_setHours", date_set_hours),
        op_native("__Date_setMinutes", date_set_minutes),
        op_native("__Date_setSeconds", date_set_seconds),
        op_native("__Date_setMilliseconds", date_set_milliseconds),
        op_native("__Date_setTime", date_set_time),
        // UTC setters
        op_native("__Date_setUTCFullYear", date_set_utc_full_year),
        op_native("__Date_setUTCMonth", date_set_utc_month),
        op_native("__Date_setUTCDate", date_set_utc_date),
        op_native("__Date_setUTCHours", date_set_utc_hours),
        op_native("__Date_setUTCMinutes", date_set_utc_minutes),
        op_native("__Date_setUTCSeconds", date_set_utc_seconds),
        op_native("__Date_setUTCMilliseconds", date_set_utc_milliseconds),
        // Formatting
        op_native("__Date_toString", date_to_string),
        op_native("__Date_toDateString", date_to_date_string),
        op_native("__Date_toTimeString", date_to_time_string),
        op_native("__Date_toISOString", date_to_iso_string),
        op_native("__Date_toUTCString", date_to_utc_string),
        op_native("__Date_toJSON", date_to_json),
        op_native("__Date_valueOf", date_value_of),
        // Locale (simplified)
        op_native("__Date_toLocaleDateString", date_to_locale_date_string),
        op_native("__Date_toLocaleTimeString", date_to_locale_time_string),
        op_native("__Date_toLocaleString", date_to_locale_string),
    ]
}

// =============================================================================
// Helper functions
// =============================================================================

/// Get timestamp (ms since epoch) from first argument
fn get_timestamp(args: &[Value]) -> Option<i64> {
    args.first().and_then(|v| {
        if let Some(n) = v.as_number() {
            if n.is_finite() { Some(n as i64) } else { None }
        } else {
            v.as_int32().map(|n| n as i64)
        }
    })
}

/// Get optional i32 argument at index
fn get_arg_i32(args: &[Value], idx: usize) -> Option<i32> {
    args.get(idx).and_then(|v| {
        if let Some(n) = v.as_int32() {
            Some(n)
        } else if let Some(n) = v.as_number() {
            if n.is_finite() { Some(n as i32) } else { None }
        } else {
            None
        }
    })
}

/// Convert timestamp to local DateTime
fn timestamp_to_local(ts: i64) -> Option<DateTime<Local>> {
    let secs = ts / 1000;
    let nsecs = ((ts % 1000) * 1_000_000) as u32;
    DateTime::from_timestamp(secs, nsecs).map(|dt: DateTime<Utc>| dt.with_timezone(&Local))
}

/// Convert timestamp to UTC DateTime
fn timestamp_to_utc(ts: i64) -> Option<DateTime<Utc>> {
    let secs = ts / 1000;
    let nsecs = ((ts % 1000) * 1_000_000) as u32;
    DateTime::from_timestamp(secs, nsecs)
}

// =============================================================================
// Static methods
// =============================================================================

/// Date.now() - returns current timestamp in milliseconds
fn date_now(_args: &[Value], _mm: Arc<memory::MemoryManager>) -> Result<Value, VmError> {
    let now = Utc::now().timestamp_millis();
    Ok(Value::number(now as f64))
}

/// Date.parse(dateString) - parses a date string and returns timestamp
fn date_parse(args: &[Value], _mm: Arc<memory::MemoryManager>) -> Result<Value, VmError> {
    let s = match args.first() {
        Some(v) if v.is_string() => v.as_string().unwrap().to_string(),
        _ => return Ok(Value::number(f64::NAN)),
    };

    // Try ISO 8601 format first
    if let Ok(dt) = DateTime::parse_from_rfc3339(&s) {
        return Ok(Value::number(dt.timestamp_millis() as f64));
    }

    // Try RFC 2822
    if let Ok(dt) = DateTime::parse_from_rfc2822(&s) {
        return Ok(Value::number(dt.timestamp_millis() as f64));
    }

    // Try common formats
    let formats = [
        "%Y-%m-%dT%H:%M:%S%.fZ",
        "%Y-%m-%dT%H:%M:%SZ",
        "%Y-%m-%dT%H:%M:%S",
        "%Y-%m-%d %H:%M:%S",
        "%Y-%m-%d",
        "%Y/%m/%d",
        "%m/%d/%Y",
    ];

    for fmt in formats {
        if let Ok(naive) = NaiveDateTime::parse_from_str(&s, fmt) {
            let dt = Utc.from_utc_datetime(&naive);
            return Ok(Value::number(dt.timestamp_millis() as f64));
        }
        // Try date-only formats
        if let Ok(date) = chrono::NaiveDate::parse_from_str(&s, fmt) {
            let naive = date.and_hms_opt(0, 0, 0).unwrap();
            let dt = Utc.from_utc_datetime(&naive);
            return Ok(Value::number(dt.timestamp_millis() as f64));
        }
    }

    Ok(Value::number(f64::NAN))
}

/// Date.UTC(year, month, ...) - returns timestamp for UTC date
fn date_utc(args: &[Value], _mm: Arc<memory::MemoryManager>) -> Result<Value, VmError> {
    let year = get_arg_i32(args, 0).unwrap_or(1970);
    let month = get_arg_i32(args, 1).unwrap_or(0); // 0-indexed
    let day = get_arg_i32(args, 2).unwrap_or(1);
    let hours = get_arg_i32(args, 3).unwrap_or(0);
    let minutes = get_arg_i32(args, 4).unwrap_or(0);
    let seconds = get_arg_i32(args, 5).unwrap_or(0);
    let ms = get_arg_i32(args, 6).unwrap_or(0);

    // Handle 2-digit years (0-99 -> 1900-1999)
    let year = if (0..100).contains(&year) {
        1900 + year
    } else {
        year
    };

    let date = chrono::NaiveDate::from_ymd_opt(year, (month + 1) as u32, day as u32);
    let time = chrono::NaiveTime::from_hms_milli_opt(
        hours as u32,
        minutes as u32,
        seconds as u32,
        ms as u32,
    );

    match (date, time) {
        (Some(d), Some(t)) => {
            let naive = NaiveDateTime::new(d, t);
            let dt = Utc.from_utc_datetime(&naive);
            Ok(Value::number(dt.timestamp_millis() as f64))
        }
        _ => Ok(Value::number(f64::NAN)),
    }
}

// =============================================================================
// Local getters
// =============================================================================

fn date_get_full_year(args: &[Value], _mm: Arc<memory::MemoryManager>) -> Result<Value, VmError> {
    match get_timestamp(args).and_then(timestamp_to_local) {
        Some(dt) => Ok(Value::int32(dt.year())),
        None => Ok(Value::number(f64::NAN)),
    }
}

fn date_get_month(args: &[Value], _mm: Arc<memory::MemoryManager>) -> Result<Value, VmError> {
    match get_timestamp(args).and_then(timestamp_to_local) {
        Some(dt) => Ok(Value::int32(dt.month0() as i32)), // 0-indexed
        None => Ok(Value::number(f64::NAN)),
    }
}

fn date_get_date(args: &[Value], _mm: Arc<memory::MemoryManager>) -> Result<Value, VmError> {
    match get_timestamp(args).and_then(timestamp_to_local) {
        Some(dt) => Ok(Value::int32(dt.day() as i32)),
        None => Ok(Value::number(f64::NAN)),
    }
}

fn date_get_day(args: &[Value], _mm: Arc<memory::MemoryManager>) -> Result<Value, VmError> {
    match get_timestamp(args).and_then(timestamp_to_local) {
        Some(dt) => {
            // chrono: Mon=0, Sun=6; JS: Sun=0, Sat=6
            let day = dt.weekday().num_days_from_sunday();
            Ok(Value::int32(day as i32))
        }
        None => Ok(Value::number(f64::NAN)),
    }
}

fn date_get_hours(args: &[Value], _mm: Arc<memory::MemoryManager>) -> Result<Value, VmError> {
    match get_timestamp(args).and_then(timestamp_to_local) {
        Some(dt) => Ok(Value::int32(dt.hour() as i32)),
        None => Ok(Value::number(f64::NAN)),
    }
}

fn date_get_minutes(args: &[Value], _mm: Arc<memory::MemoryManager>) -> Result<Value, VmError> {
    match get_timestamp(args).and_then(timestamp_to_local) {
        Some(dt) => Ok(Value::int32(dt.minute() as i32)),
        None => Ok(Value::number(f64::NAN)),
    }
}

fn date_get_seconds(args: &[Value], _mm: Arc<memory::MemoryManager>) -> Result<Value, VmError> {
    match get_timestamp(args).and_then(timestamp_to_local) {
        Some(dt) => Ok(Value::int32(dt.second() as i32)),
        None => Ok(Value::number(f64::NAN)),
    }
}

fn date_get_milliseconds(
    args: &[Value],
    _mm: Arc<memory::MemoryManager>,
) -> Result<Value, VmError> {
    match get_timestamp(args).and_then(timestamp_to_local) {
        Some(dt) => Ok(Value::int32((dt.nanosecond() / 1_000_000) as i32)),
        None => Ok(Value::number(f64::NAN)),
    }
}

// =============================================================================
// UTC getters
// =============================================================================

fn date_get_utc_full_year(
    args: &[Value],
    _mm: Arc<memory::MemoryManager>,
) -> Result<Value, VmError> {
    match get_timestamp(args).and_then(timestamp_to_utc) {
        Some(dt) => Ok(Value::int32(dt.year())),
        None => Ok(Value::number(f64::NAN)),
    }
}

fn date_get_utc_month(args: &[Value], _mm: Arc<memory::MemoryManager>) -> Result<Value, VmError> {
    match get_timestamp(args).and_then(timestamp_to_utc) {
        Some(dt) => Ok(Value::int32(dt.month0() as i32)),
        None => Ok(Value::number(f64::NAN)),
    }
}

fn date_get_utc_date(args: &[Value], _mm: Arc<memory::MemoryManager>) -> Result<Value, VmError> {
    match get_timestamp(args).and_then(timestamp_to_utc) {
        Some(dt) => Ok(Value::int32(dt.day() as i32)),
        None => Ok(Value::number(f64::NAN)),
    }
}

fn date_get_utc_day(args: &[Value], _mm: Arc<memory::MemoryManager>) -> Result<Value, VmError> {
    match get_timestamp(args).and_then(timestamp_to_utc) {
        Some(dt) => {
            let day = dt.weekday().num_days_from_sunday();
            Ok(Value::int32(day as i32))
        }
        None => Ok(Value::number(f64::NAN)),
    }
}

fn date_get_utc_hours(args: &[Value], _mm: Arc<memory::MemoryManager>) -> Result<Value, VmError> {
    match get_timestamp(args).and_then(timestamp_to_utc) {
        Some(dt) => Ok(Value::int32(dt.hour() as i32)),
        None => Ok(Value::number(f64::NAN)),
    }
}

fn date_get_utc_minutes(args: &[Value], _mm: Arc<memory::MemoryManager>) -> Result<Value, VmError> {
    match get_timestamp(args).and_then(timestamp_to_utc) {
        Some(dt) => Ok(Value::int32(dt.minute() as i32)),
        None => Ok(Value::number(f64::NAN)),
    }
}

fn date_get_utc_seconds(args: &[Value], _mm: Arc<memory::MemoryManager>) -> Result<Value, VmError> {
    match get_timestamp(args).and_then(timestamp_to_utc) {
        Some(dt) => Ok(Value::int32(dt.second() as i32)),
        None => Ok(Value::number(f64::NAN)),
    }
}

fn date_get_utc_milliseconds(
    args: &[Value],
    _mm: Arc<memory::MemoryManager>,
) -> Result<Value, VmError> {
    match get_timestamp(args).and_then(timestamp_to_utc) {
        Some(dt) => Ok(Value::int32((dt.nanosecond() / 1_000_000) as i32)),
        None => Ok(Value::number(f64::NAN)),
    }
}

// =============================================================================
// Other getters
// =============================================================================

fn date_get_time(args: &[Value], _mm: Arc<memory::MemoryManager>) -> Result<Value, VmError> {
    match get_timestamp(args) {
        Some(ts) => Ok(Value::number(ts as f64)),
        None => Ok(Value::number(f64::NAN)),
    }
}

fn date_get_timezone_offset(
    _args: &[Value],
    _mm: Arc<memory::MemoryManager>,
) -> Result<Value, VmError> {
    // Return offset in minutes (negative for east of UTC)
    let local = Local::now();
    let offset_secs = local.offset().local_minus_utc();
    Ok(Value::int32(-(offset_secs / 60)))
}

// =============================================================================
// Local setters (return new timestamp)
// =============================================================================

fn date_set_full_year(args: &[Value], _mm: Arc<memory::MemoryManager>) -> Result<Value, VmError> {
    let ts = get_timestamp(args).unwrap_or_else(|| Utc::now().timestamp_millis());
    let year = get_arg_i32(args, 1).unwrap_or(1970);
    let month = get_arg_i32(args, 2);
    let day = get_arg_i32(args, 3);

    if let Some(dt) = timestamp_to_local(ts) {
        let new_month = month.map(|m| m + 1).unwrap_or(dt.month() as i32) as u32;
        let new_day = day.unwrap_or(dt.day() as i32) as u32;

        if let Some(date) = chrono::NaiveDate::from_ymd_opt(year, new_month, new_day) {
            let time = dt.time();
            let naive = NaiveDateTime::new(date, time);
            let new_dt = Local.from_local_datetime(&naive).single();
            if let Some(new_dt) = new_dt {
                return Ok(Value::number(new_dt.timestamp_millis() as f64));
            }
        }
    }
    Ok(Value::number(f64::NAN))
}

fn date_set_month(args: &[Value], _mm: Arc<memory::MemoryManager>) -> Result<Value, VmError> {
    let ts = get_timestamp(args).unwrap_or_else(|| Utc::now().timestamp_millis());
    let month = get_arg_i32(args, 1).unwrap_or(0);
    let day = get_arg_i32(args, 2);

    if let Some(dt) = timestamp_to_local(ts) {
        let new_day = day.unwrap_or(dt.day() as i32) as u32;

        if let Some(date) = chrono::NaiveDate::from_ymd_opt(dt.year(), (month + 1) as u32, new_day)
        {
            let time = dt.time();
            let naive = NaiveDateTime::new(date, time);
            let new_dt = Local.from_local_datetime(&naive).single();
            if let Some(new_dt) = new_dt {
                return Ok(Value::number(new_dt.timestamp_millis() as f64));
            }
        }
    }
    Ok(Value::number(f64::NAN))
}

fn date_set_date(args: &[Value], _mm: Arc<memory::MemoryManager>) -> Result<Value, VmError> {
    let ts = get_timestamp(args).unwrap_or_else(|| Utc::now().timestamp_millis());
    let day = get_arg_i32(args, 1).unwrap_or(1);

    if let Some(dt) = timestamp_to_local(ts)
        && let Some(date) = chrono::NaiveDate::from_ymd_opt(dt.year(), dt.month(), day as u32)
    {
        let time = dt.time();
        let naive = NaiveDateTime::new(date, time);
        if let Some(new_dt) = Local.from_local_datetime(&naive).single() {
            return Ok(Value::number(new_dt.timestamp_millis() as f64));
        }
    }
    Ok(Value::number(f64::NAN))
}

fn date_set_hours(args: &[Value], _mm: Arc<memory::MemoryManager>) -> Result<Value, VmError> {
    let ts = get_timestamp(args).unwrap_or_else(|| Utc::now().timestamp_millis());
    let hours = get_arg_i32(args, 1).unwrap_or(0) as u32;
    let minutes = get_arg_i32(args, 2);
    let seconds = get_arg_i32(args, 3);
    let ms = get_arg_i32(args, 4);

    if let Some(dt) = timestamp_to_local(ts) {
        let new_min = minutes.unwrap_or(dt.minute() as i32) as u32;
        let new_sec = seconds.unwrap_or(dt.second() as i32) as u32;
        let new_ms = ms.unwrap_or((dt.nanosecond() / 1_000_000) as i32) as u32;

        if let Some(time) = chrono::NaiveTime::from_hms_milli_opt(hours, new_min, new_sec, new_ms) {
            let naive = NaiveDateTime::new(dt.date_naive(), time);
            let new_dt = Local.from_local_datetime(&naive).single();
            if let Some(new_dt) = new_dt {
                return Ok(Value::number(new_dt.timestamp_millis() as f64));
            }
        }
    }
    Ok(Value::number(f64::NAN))
}

fn date_set_minutes(args: &[Value], _mm: Arc<memory::MemoryManager>) -> Result<Value, VmError> {
    let ts = get_timestamp(args).unwrap_or_else(|| Utc::now().timestamp_millis());
    let minutes = get_arg_i32(args, 1).unwrap_or(0) as u32;
    let seconds = get_arg_i32(args, 2);
    let ms = get_arg_i32(args, 3);

    if let Some(dt) = timestamp_to_local(ts) {
        let new_sec = seconds.unwrap_or(dt.second() as i32) as u32;
        let new_ms = ms.unwrap_or((dt.nanosecond() / 1_000_000) as i32) as u32;

        if let Some(time) =
            chrono::NaiveTime::from_hms_milli_opt(dt.hour(), minutes, new_sec, new_ms)
        {
            let naive = NaiveDateTime::new(dt.date_naive(), time);
            let new_dt = Local.from_local_datetime(&naive).single();
            if let Some(new_dt) = new_dt {
                return Ok(Value::number(new_dt.timestamp_millis() as f64));
            }
        }
    }
    Ok(Value::number(f64::NAN))
}

fn date_set_seconds(args: &[Value], _mm: Arc<memory::MemoryManager>) -> Result<Value, VmError> {
    let ts = get_timestamp(args).unwrap_or_else(|| Utc::now().timestamp_millis());
    let seconds = get_arg_i32(args, 1).unwrap_or(0) as u32;
    let ms = get_arg_i32(args, 2);

    if let Some(dt) = timestamp_to_local(ts) {
        let new_ms = ms.unwrap_or((dt.nanosecond() / 1_000_000) as i32) as u32;

        if let Some(time) =
            chrono::NaiveTime::from_hms_milli_opt(dt.hour(), dt.minute(), seconds, new_ms)
        {
            let naive = NaiveDateTime::new(dt.date_naive(), time);
            let new_dt = Local.from_local_datetime(&naive).single();
            if let Some(new_dt) = new_dt {
                return Ok(Value::number(new_dt.timestamp_millis() as f64));
            }
        }
    }
    Ok(Value::number(f64::NAN))
}

fn date_set_milliseconds(
    args: &[Value],
    _mm: Arc<memory::MemoryManager>,
) -> Result<Value, VmError> {
    let ts = get_timestamp(args).unwrap_or_else(|| Utc::now().timestamp_millis());
    let ms = get_arg_i32(args, 1).unwrap_or(0) as u32;

    if let Some(dt) = timestamp_to_local(ts)
        && let Some(time) =
            chrono::NaiveTime::from_hms_milli_opt(dt.hour(), dt.minute(), dt.second(), ms)
    {
        let naive = NaiveDateTime::new(dt.date_naive(), time);
        if let Some(new_dt) = Local.from_local_datetime(&naive).single() {
            return Ok(Value::number(new_dt.timestamp_millis() as f64));
        }
    }
    Ok(Value::number(f64::NAN))
}

fn date_set_time(args: &[Value], _mm: Arc<memory::MemoryManager>) -> Result<Value, VmError> {
    match get_arg_i32(args, 1) {
        Some(ts) => Ok(Value::number(ts as f64)),
        None => {
            // Try as number (larger timestamps)
            if let Some(v) = args.get(1)
                && let Some(n) = v.as_number()
                && n.is_finite()
            {
                return Ok(Value::number(n));
            }
            Ok(Value::number(f64::NAN))
        }
    }
}

// =============================================================================
// UTC setters
// =============================================================================

fn date_set_utc_full_year(
    args: &[Value],
    _mm: Arc<memory::MemoryManager>,
) -> Result<Value, VmError> {
    let ts = get_timestamp(args).unwrap_or_else(|| Utc::now().timestamp_millis());
    let year = get_arg_i32(args, 1).unwrap_or(1970);
    let month = get_arg_i32(args, 2);
    let day = get_arg_i32(args, 3);

    if let Some(dt) = timestamp_to_utc(ts) {
        let new_month = month.map(|m| m + 1).unwrap_or(dt.month() as i32) as u32;
        let new_day = day.unwrap_or(dt.day() as i32) as u32;

        if let Some(date) = chrono::NaiveDate::from_ymd_opt(year, new_month, new_day) {
            let time = dt.time();
            let naive = NaiveDateTime::new(date, time);
            let new_dt = Utc.from_utc_datetime(&naive);
            return Ok(Value::number(new_dt.timestamp_millis() as f64));
        }
    }
    Ok(Value::number(f64::NAN))
}

fn date_set_utc_month(args: &[Value], _mm: Arc<memory::MemoryManager>) -> Result<Value, VmError> {
    let ts = get_timestamp(args).unwrap_or_else(|| Utc::now().timestamp_millis());
    let month = get_arg_i32(args, 1).unwrap_or(0);
    let day = get_arg_i32(args, 2);

    if let Some(dt) = timestamp_to_utc(ts) {
        let new_day = day.unwrap_or(dt.day() as i32) as u32;

        if let Some(date) = chrono::NaiveDate::from_ymd_opt(dt.year(), (month + 1) as u32, new_day)
        {
            let time = dt.time();
            let naive = NaiveDateTime::new(date, time);
            let new_dt = Utc.from_utc_datetime(&naive);
            return Ok(Value::number(new_dt.timestamp_millis() as f64));
        }
    }
    Ok(Value::number(f64::NAN))
}

fn date_set_utc_date(args: &[Value], _mm: Arc<memory::MemoryManager>) -> Result<Value, VmError> {
    let ts = get_timestamp(args).unwrap_or_else(|| Utc::now().timestamp_millis());
    let day = get_arg_i32(args, 1).unwrap_or(1);

    if let Some(dt) = timestamp_to_utc(ts)
        && let Some(date) = chrono::NaiveDate::from_ymd_opt(dt.year(), dt.month(), day as u32)
    {
        let time = dt.time();
        let naive = NaiveDateTime::new(date, time);
        let new_dt = Utc.from_utc_datetime(&naive);
        return Ok(Value::number(new_dt.timestamp_millis() as f64));
    }
    Ok(Value::number(f64::NAN))
}

fn date_set_utc_hours(args: &[Value], _mm: Arc<memory::MemoryManager>) -> Result<Value, VmError> {
    let ts = get_timestamp(args).unwrap_or_else(|| Utc::now().timestamp_millis());
    let hours = get_arg_i32(args, 1).unwrap_or(0) as u32;
    let minutes = get_arg_i32(args, 2);
    let seconds = get_arg_i32(args, 3);
    let ms = get_arg_i32(args, 4);

    if let Some(dt) = timestamp_to_utc(ts) {
        let new_min = minutes.unwrap_or(dt.minute() as i32) as u32;
        let new_sec = seconds.unwrap_or(dt.second() as i32) as u32;
        let new_ms = ms.unwrap_or((dt.nanosecond() / 1_000_000) as i32) as u32;

        if let Some(time) = chrono::NaiveTime::from_hms_milli_opt(hours, new_min, new_sec, new_ms) {
            let naive = NaiveDateTime::new(dt.date_naive(), time);
            let new_dt = Utc.from_utc_datetime(&naive);
            return Ok(Value::number(new_dt.timestamp_millis() as f64));
        }
    }
    Ok(Value::number(f64::NAN))
}

fn date_set_utc_minutes(args: &[Value], _mm: Arc<memory::MemoryManager>) -> Result<Value, VmError> {
    let ts = get_timestamp(args).unwrap_or_else(|| Utc::now().timestamp_millis());
    let minutes = get_arg_i32(args, 1).unwrap_or(0) as u32;
    let seconds = get_arg_i32(args, 2);
    let ms = get_arg_i32(args, 3);

    if let Some(dt) = timestamp_to_utc(ts) {
        let new_sec = seconds.unwrap_or(dt.second() as i32) as u32;
        let new_ms = ms.unwrap_or((dt.nanosecond() / 1_000_000) as i32) as u32;

        if let Some(time) =
            chrono::NaiveTime::from_hms_milli_opt(dt.hour(), minutes, new_sec, new_ms)
        {
            let naive = NaiveDateTime::new(dt.date_naive(), time);
            let new_dt = Utc.from_utc_datetime(&naive);
            return Ok(Value::number(new_dt.timestamp_millis() as f64));
        }
    }
    Ok(Value::number(f64::NAN))
}

fn date_set_utc_seconds(args: &[Value], _mm: Arc<memory::MemoryManager>) -> Result<Value, VmError> {
    let ts = get_timestamp(args).unwrap_or_else(|| Utc::now().timestamp_millis());
    let seconds = get_arg_i32(args, 1).unwrap_or(0) as u32;
    let ms = get_arg_i32(args, 2);

    if let Some(dt) = timestamp_to_utc(ts) {
        let new_ms = ms.unwrap_or((dt.nanosecond() / 1_000_000) as i32) as u32;

        if let Some(time) =
            chrono::NaiveTime::from_hms_milli_opt(dt.hour(), dt.minute(), seconds, new_ms)
        {
            let naive = NaiveDateTime::new(dt.date_naive(), time);
            let new_dt = Utc.from_utc_datetime(&naive);
            return Ok(Value::number(new_dt.timestamp_millis() as f64));
        }
    }
    Ok(Value::number(f64::NAN))
}

fn date_set_utc_milliseconds(
    args: &[Value],
    _mm: Arc<memory::MemoryManager>,
) -> Result<Value, VmError> {
    let ts = get_timestamp(args).unwrap_or_else(|| Utc::now().timestamp_millis());
    let ms = get_arg_i32(args, 1).unwrap_or(0) as u32;

    if let Some(dt) = timestamp_to_utc(ts)
        && let Some(time) =
            chrono::NaiveTime::from_hms_milli_opt(dt.hour(), dt.minute(), dt.second(), ms)
    {
        let naive = NaiveDateTime::new(dt.date_naive(), time);
        let new_dt = Utc.from_utc_datetime(&naive);
        return Ok(Value::number(new_dt.timestamp_millis() as f64));
    }
    Ok(Value::number(f64::NAN))
}

// =============================================================================
// Formatting methods
// =============================================================================

/// toString - e.g., "Thu Jan 23 2026 15:30:45 GMT-0500 (Eastern Standard Time)"
fn date_to_string(args: &[Value], _mm: Arc<memory::MemoryManager>) -> Result<Value, VmError> {
    match get_timestamp(args).and_then(timestamp_to_local) {
        Some(dt) => {
            let s = dt.format("%a %b %d %Y %H:%M:%S GMT%z").to_string();
            Ok(Value::string(JsString::intern(&s)))
        }
        None => Ok(Value::string(JsString::intern("Invalid Date"))),
    }
}

/// toDateString - e.g., "Thu Jan 23 2026"
fn date_to_date_string(args: &[Value], _mm: Arc<memory::MemoryManager>) -> Result<Value, VmError> {
    match get_timestamp(args).and_then(timestamp_to_local) {
        Some(dt) => {
            let s = dt.format("%a %b %d %Y").to_string();
            Ok(Value::string(JsString::intern(&s)))
        }
        None => Ok(Value::string(JsString::intern("Invalid Date"))),
    }
}

/// toTimeString - e.g., "15:30:45 GMT-0500 (Eastern Standard Time)"
fn date_to_time_string(args: &[Value], _mm: Arc<memory::MemoryManager>) -> Result<Value, VmError> {
    match get_timestamp(args).and_then(timestamp_to_local) {
        Some(dt) => {
            let s = dt.format("%H:%M:%S GMT%z").to_string();
            Ok(Value::string(JsString::intern(&s)))
        }
        None => Ok(Value::string(JsString::intern("Invalid Date"))),
    }
}

/// toISOString - e.g., "2026-01-23T20:30:45.000Z"
fn date_to_iso_string(args: &[Value], _mm: Arc<memory::MemoryManager>) -> Result<Value, VmError> {
    match get_timestamp(args).and_then(timestamp_to_utc) {
        Some(dt) => {
            let s = dt.format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string();
            Ok(Value::string(JsString::intern(&s)))
        }
        None => Err(VmError::type_error("Invalid Date")),
    }
}

/// toUTCString - e.g., "Thu, 23 Jan 2026 20:30:45 GMT"
fn date_to_utc_string(args: &[Value], _mm: Arc<memory::MemoryManager>) -> Result<Value, VmError> {
    match get_timestamp(args).and_then(timestamp_to_utc) {
        Some(dt) => {
            let s = dt.format("%a, %d %b %Y %H:%M:%S GMT").to_string();
            Ok(Value::string(JsString::intern(&s)))
        }
        None => Ok(Value::string(JsString::intern("Invalid Date"))),
    }
}

/// toJSON - same as toISOString (used by JSON.stringify)
fn date_to_json(args: &[Value], _mm: Arc<memory::MemoryManager>) -> Result<Value, VmError> {
    match get_timestamp(args).and_then(timestamp_to_utc) {
        Some(dt) => {
            let s = dt.format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string();
            Ok(Value::string(JsString::intern(&s)))
        }
        None => Ok(Value::null()),
    }
}

/// valueOf - returns timestamp
fn date_value_of(args: &[Value], _mm: Arc<memory::MemoryManager>) -> Result<Value, VmError> {
    match get_timestamp(args) {
        Some(ts) => Ok(Value::number(ts as f64)),
        None => Ok(Value::number(f64::NAN)),
    }
}

/// toLocaleDateString (simplified - no locale support yet)
fn date_to_locale_date_string(
    args: &[Value],
    _mm: Arc<memory::MemoryManager>,
) -> Result<Value, VmError> {
    match get_timestamp(args).and_then(timestamp_to_local) {
        Some(dt) => {
            let s = dt.format("%m/%d/%Y").to_string();
            Ok(Value::string(JsString::intern(&s)))
        }
        None => Ok(Value::string(JsString::intern("Invalid Date"))),
    }
}

/// toLocaleTimeString (simplified - no locale support yet)
fn date_to_locale_time_string(
    args: &[Value],
    _mm: Arc<memory::MemoryManager>,
) -> Result<Value, VmError> {
    match get_timestamp(args).and_then(timestamp_to_local) {
        Some(dt) => {
            let s = dt.format("%I:%M:%S %p").to_string();
            Ok(Value::string(JsString::intern(&s)))
        }
        None => Ok(Value::string(JsString::intern("Invalid Date"))),
    }
}

/// toLocaleString (simplified - no locale support yet)
fn date_to_locale_string(
    args: &[Value],
    _mm: Arc<memory::MemoryManager>,
) -> Result<Value, VmError> {
    match get_timestamp(args).and_then(timestamp_to_local) {
        Some(dt) => {
            let s = dt.format("%m/%d/%Y, %I:%M:%S %p").to_string();
            Ok(Value::string(JsString::intern(&s)))
        }
        None => Ok(Value::string(JsString::intern("Invalid Date"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_date_now() {
        let mm = Arc::new(memory::MemoryManager::test());
        let result = date_now(&[], mm).unwrap();
        let ts = result.as_number().unwrap();
        assert!(ts > 1700000000000.0); // After 2023
    }

    #[test]
    fn test_date_utc() {
        let mm = Arc::new(memory::MemoryManager::test());
        // Date.UTC(2026, 0, 23, 12, 30, 45, 500)
        let args = vec![
            Value::int32(2026),
            Value::int32(0), // January (0-indexed)
            Value::int32(23),
            Value::int32(12),
            Value::int32(30),
            Value::int32(45),
            Value::int32(500),
        ];
        let result = date_utc(&args, mm).unwrap();
        let ts = result.as_number().unwrap();
        assert!(ts > 0.0);
    }

    #[test]
    fn test_date_parse_iso() {
        let mm = Arc::new(memory::MemoryManager::test());
        let args = vec![Value::string(JsString::intern("2026-01-23T12:30:45.000Z"))];
        let result = date_parse(&args, mm).unwrap();
        let ts = result.as_number().unwrap();
        assert!(ts > 0.0);
    }

    #[test]
    fn test_date_getters() {
        let mm = Arc::new(memory::MemoryManager::test());
        // Use a known timestamp: 2026-01-23T12:30:45.500Z
        let ts = 1769170245500i64; // approximately
        let args = vec![Value::number(ts as f64)];

        let year = date_get_utc_full_year(&args, mm.clone()).unwrap();
        assert_eq!(year.as_int32(), Some(2026));

        let month = date_get_utc_month(&args, mm.clone()).unwrap();
        assert_eq!(month.as_int32(), Some(0)); // January = 0

        let date = date_get_utc_date(&args, mm).unwrap();
        assert!(date.as_int32().is_some());
    }

    #[test]
    fn test_date_to_iso_string() {
        let mm = Arc::new(memory::MemoryManager::test());
        let ts = 0i64; // Unix epoch
        let args = vec![Value::number(ts as f64)];
        let result = date_to_iso_string(&args, mm).unwrap();
        let s = result.as_string().unwrap().to_string();
        assert_eq!(s, "1970-01-01T00:00:00.000Z");
    }

    #[test]
    fn test_date_timezone_offset() {
        let mm = Arc::new(memory::MemoryManager::test());
        let result = date_get_timezone_offset(&[], mm).unwrap();
        let offset = result.as_int32().unwrap();
        // Offset should be within reasonable range (-720 to 840 minutes)
        assert!((-720..=840).contains(&offset));
    }

    #[test]
    fn test_invalid_date() {
        let mm = Arc::new(memory::MemoryManager::test());
        let args = vec![Value::number(f64::NAN)];
        let result = date_to_string(&args, mm).unwrap();
        let s = result.as_string().unwrap().to_string();
        assert_eq!(s, "Invalid Date");
    }
}
