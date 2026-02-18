//! Date.prototype methods implementation
//!
//! All Date object methods for ES2026 standard.
//! Date objects store timestamp in `__timestamp__` property (milliseconds since epoch).

use crate::context::NativeContext;
use crate::error::VmError;
use crate::gc::GcRef;
use crate::interpreter::PreferredType;
use crate::memory::MemoryManager;
use crate::object::{JsObject, PropertyAttributes, PropertyDescriptor, PropertyKey};
use crate::string::JsString;
use crate::value::Value;
use chrono::{DateTime, Datelike, Local, NaiveDate, TimeZone, Timelike};
use std::sync::Arc;

/// Helper to extract timestamp from Date object
/// Returns Ok(f64) where f64 is the timestamp (can be NaN)
/// Returns Err(VmError) if `this` is not a Date object
fn get_timestamp_value(this_val: &Value) -> Result<f64, VmError> {
    let obj = this_val
        .as_object()
        .ok_or_else(|| VmError::type_error("Method called on incompatible receiver"))?;

    let ts_val = obj
        .get(&PropertyKey::string("__timestamp__"))
        .ok_or_else(|| VmError::type_error("Method called on incompatible receiver"))?;

    if let Some(n) = ts_val.as_number() {
        Ok(n)
    } else if let Some(i) = ts_val.as_int32() {
        Ok(i as f64)
    } else {
        Ok(f64::NAN)
    }
}

/// Helper to convert value to object — delegates to the full ToObject from object.rs
fn to_object(ncx: &mut NativeContext<'_>, val: &Value) -> Result<GcRef<JsObject>, VmError> {
    crate::intrinsics_impl::object::to_object_for_builtin(ncx, val)
}

/// Spec-compliant [[Get]] on an object: walks prototype chain and invokes getters.
fn get_value_from_object(
    ncx: &mut NativeContext<'_>,
    obj: &GcRef<JsObject>,
    key: &PropertyKey,
    receiver: Value,
) -> Result<Value, VmError> {
    if let Some(desc) = obj.lookup_property_descriptor(key) {
        match desc {
            PropertyDescriptor::Data { value, .. } => Ok(value),
            PropertyDescriptor::Accessor { get, .. } => {
                if let Some(getter) = get {
                    if getter.is_callable() {
                        ncx.call_function(&getter, receiver, &[])
                    } else {
                        Ok(Value::undefined())
                    }
                } else {
                    Ok(Value::undefined())
                }
            }
            PropertyDescriptor::Deleted => Ok(Value::undefined()),
        }
    } else {
        Ok(Value::undefined())
    }
}

/// Helper to perform safe casting from f64 (milliseconds) to i64
fn safe_cast_time(t: f64) -> Option<i64> {
    if t.is_finite() && t.abs() <= 8_640_000_000_000_000.0 {
        // TimeClip: If t is -0, return +0.
        let res = if t == 0.0 { 0 } else { t as i64 };
        Some(res)
    } else {
        None
    }
}

/// Helper to apply TimeClip: normalize -0 to +0, truncate to integer, and handle range
fn time_clip(t: f64) -> f64 {
    if !t.is_finite() || t.abs() > 8_640_000_000_000_000.0 {
        return f64::NAN;
    }
    let t = t.trunc();
    if t == 0.0 { 0.0 } else { t }
}

/// Decompose a timestamp (ms since epoch) into (seconds, nanoseconds) correctly for negative values.
/// Returns None if the timestamp can't be represented.
fn ts_to_secs_nanos(ts: f64) -> Option<(i64, u32)> {
    let secs = (ts / 1000.0).floor() as i64;
    let sub_ms = ts.rem_euclid(1000.0);
    let nanos = (sub_ms * 1_000_000.0) as u32;
    Some((secs, nanos))
}

/// Helper to create DateTime<Utc> from a timestamp in ms.
fn ts_to_utc(ts: f64) -> Option<DateTime<chrono::Utc>> {
    let (secs, nanos) = ts_to_secs_nanos(ts)?;
    DateTime::from_timestamp(secs, nanos)
}

fn format_year(y: i32) -> String {
    if y < 0 {
        format!("-{:04}", y.abs())
    } else {
        format!("{:04}", y)
    }
}

fn safe_add_days(date: NaiveDate, days: f64) -> Option<NaiveDate> {
    if !days.is_finite() {
        return None;
    }
    let current_days = date.num_days_from_ce() as i64;
    let new_days = current_days + days as i64;
    // JS limit is roughly +/- 100M days. i32 range is +/- 2B days.
    if new_days > (i32::MAX as i64) || new_days < (i32::MIN as i64) {
        return None;
    }
    NaiveDate::from_num_days_from_ce_opt(new_days as i32)
}

fn local_to_ms(res: chrono::LocalResult<chrono::DateTime<chrono::Local>>) -> f64 {
    match res {
        chrono::LocalResult::Single(dt) => dt.timestamp_millis() as f64,
        chrono::LocalResult::Ambiguous(dt, _) => dt.timestamp_millis() as f64,
        chrono::LocalResult::None => f64::NAN,
    }
}

/// Compute year from time value using ES spec algorithm
fn year_from_time(t: f64) -> f64 {
    // Binary search for the year containing day number `day`
    let day = (t / MS_PER_DAY).floor();
    // Estimate bounds — account for negative days giving negative years
    let est = (day / 365.2425 + 1970.0).floor() as i64;
    let mut lo = est - 2;
    let mut hi = est + 2;
    // Widen bounds until they bracket the answer
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

/// Compute month (0-11) from time value
fn month_from_time(t: f64) -> f64 {
    let day = (t / MS_PER_DAY).floor();
    let y = year_from_time(t);
    let day_in_year = day - day_from_year(y);
    let leap = days_in_year(y) == 366.0;
    let l = if leap { 1.0 } else { 0.0 };
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

/// Compute day of month (1-31) from time value
fn date_from_time(t: f64) -> f64 {
    let day = (t / MS_PER_DAY).floor();
    let y = year_from_time(t);
    let day_in_year = day - day_from_year(y);
    let leap = days_in_year(y) == 366.0;
    let l = if leap { 1.0 } else { 0.0 };
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

/// ES spec time-within-day components
fn hour_from_time(t: f64) -> f64 {
    ((t / MS_PER_HOUR).floor()).rem_euclid(24.0)
}
fn min_from_time(t: f64) -> f64 {
    ((t / MS_PER_MINUTE).floor()).rem_euclid(60.0)
}
fn sec_from_time(t: f64) -> f64 {
    ((t / MS_PER_SECOND).floor()).rem_euclid(60.0)
}
fn ms_from_time(t: f64) -> f64 {
    t.rem_euclid(1000.0)
}

/// Extract UTC time components from a timestamp (ms since epoch).
/// Returns (year, month0, day, hour, minute, second, ms_within_second)
/// Uses pure arithmetic — works for all valid JS Date ranges.
fn utc_components(ts: f64) -> (f64, f64, f64, f64, f64, f64, f64) {
    if !ts.is_finite() {
        return (
            f64::NAN,
            f64::NAN,
            f64::NAN,
            f64::NAN,
            f64::NAN,
            f64::NAN,
            f64::NAN,
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

/// Extract local time components from a timestamp (ms since epoch).
/// Returns (year, month0, day, hour, minute, second, ms_within_second)
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
        // Chrono can't represent this date — compute from UTC components with offset
        let offset_ms = Local::now().offset().local_minus_utc() as f64 * 1000.0;
        utc_components(ts + offset_ms)
    }
}

/// Convert local time components to UTC timestamp using make_day/make_date/make_time
fn local_to_utc_ms(year: f64, month: f64, day: f64, h: f64, min: f64, sec: f64, ms: f64) -> f64 {
    let d = make_day(year, month, day);
    let t = make_time(h, min, sec, ms);
    let date = make_date(d, t);
    if date.is_nan() || !date.is_finite() {
        return f64::NAN;
    }
    // Convert local → UTC by computing offset
    let approx_utc_ms = date;
    let total_secs = (approx_utc_ms / 1000.0).floor() as i64;
    let sub_ms = approx_utc_ms.rem_euclid(1000.0) as u32;

    let naive = chrono::NaiveDateTime::from_timestamp_opt(total_secs, sub_ms * 1_000_000);
    if let Some(n) = naive {
        let res = Local.from_local_datetime(&n);
        local_to_ms(res)
    } else {
        // Chrono can't represent this date (year outside ±262143).
        // Fall back: use current timezone offset as approximation.
        let now_offset = Local::now().offset().local_minus_utc() as f64;
        date - (now_offset * 1000.0)
    }
}

/// Store a new timestamp on a Date object and return it
fn set_date_value(this_val: &Value, ts: f64) -> Result<Value, VmError> {
    let new_ts = time_clip(ts);
    if let Some(obj) = this_val.as_object() {
        obj.set(PropertyKey::string("__timestamp__"), Value::number(new_ts))
            .unwrap();
    }
    Ok(Value::number(new_ts))
}

const MS_PER_DAY: f64 = 86400000.0;
const MS_PER_HOUR: f64 = 3600000.0;
const MS_PER_MINUTE: f64 = 60000.0;
const MS_PER_SECOND: f64 = 1000.0;

/// ES2023 §21.4.1.7 MakeTime ( hour, min, sec, ms )
fn make_time(hour: f64, min: f64, sec: f64, ms: f64) -> f64 {
    if !hour.is_finite() || !min.is_finite() || !sec.is_finite() || !ms.is_finite() {
        return f64::NAN;
    }
    ((hour * MS_PER_HOUR + min * MS_PER_MINUTE) + sec * MS_PER_SECOND) + ms
}

/// ES2023 §21.4.1.3 DayFromYear ( y )
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
    let leap_off = if leap { 1.0 } else { 0.0 };
    match m as i32 {
        0 => 0.0,
        1 => 31.0,
        2 => 59.0 + leap_off,
        3 => 90.0 + leap_off,
        4 => 120.0 + leap_off,
        5 => 151.0 + leap_off,
        6 => 181.0 + leap_off,
        7 => 212.0 + leap_off,
        8 => 243.0 + leap_off,
        9 => 273.0 + leap_off,
        10 => 304.0 + leap_off,
        11 => 334.0 + leap_off,
        _ => 0.0,
    }
}

/// ES2023 §21.4.1.12 MakeDay ( year, month, date )
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

/// ES2023 §21.4.1.13 MakeDate ( day, time )
fn make_date(day: f64, time: f64) -> f64 {
    if !day.is_finite() || !time.is_finite() {
        return f64::NAN;
    }
    day * MS_PER_DAY + time
}

/// Create Date constructor function
pub fn create_date_constructor()
-> Box<dyn Fn(&Value, &[Value], &mut NativeContext<'_>) -> Result<Value, VmError> + Send + Sync> {
    Box::new(|this, args, ncx| {
        use chrono::Local;
        use std::time::{SystemTime, UNIX_EPOCH};

        let is_constructor_call = this.as_object().map_or(false, |obj| {
            let proto = obj.prototype();
            !proto.is_null() && !proto.is_undefined()
        });

        if !is_constructor_call {
            let now = Local::now();
            let date_str = now.format("%a %b %d %Y %H:%M:%S GMT%z").to_string();
            return Ok(Value::string(JsString::intern(&date_str)));
        }

        let timestamp = if args.is_empty() {
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_millis() as f64)
                .unwrap_or(0.0)
        } else if args.len() == 1 {
            // ES2023 §21.4.2.1: If value is an Object with [[DateValue]], extract it directly.
            let date_val = if let Some(obj) = args[0].as_object() {
                obj.get(&PropertyKey::string("__timestamp__"))
            } else {
                None
            };

            if let Some(dv) = date_val {
                dv.as_number().unwrap_or(f64::NAN)
            } else {
                let prim = ncx.to_primitive(&args[0], PreferredType::Default)?;
                if let Some(date_str) = prim.as_string() {
                    parse_date_string(date_str.as_str())
                } else {
                    let num = ncx.to_number_value(&prim)?;
                    time_clip(num)
                }
            }
        } else {
            let year = ncx.to_number_value(&args[0])?.trunc();
            let month = args
                .get(1)
                .map(|v| ncx.to_number_value(v))
                .transpose()?
                .unwrap_or(0.0)
                .trunc();
            let day = args
                .get(2)
                .map(|v| ncx.to_number_value(v))
                .transpose()?
                .unwrap_or(1.0)
                .trunc();
            let hour = args
                .get(3)
                .map(|v| ncx.to_number_value(v))
                .transpose()?
                .unwrap_or(0.0)
                .trunc();
            let min = args
                .get(4)
                .map(|v| ncx.to_number_value(v))
                .transpose()?
                .unwrap_or(0.0)
                .trunc();
            let sec = args
                .get(5)
                .map(|v| ncx.to_number_value(v))
                .transpose()?
                .unwrap_or(0.0)
                .trunc();
            let ms = args
                .get(6)
                .map(|v| ncx.to_number_value(v))
                .transpose()?
                .unwrap_or(0.0)
                .trunc();

            let full_year = if year >= 0.0 && year <= 99.0 {
                1900.0 + year
            } else {
                year
            };

            let time = make_time(hour, min, sec, ms);
            let day = make_day(full_year, month, day);
            time_clip(make_date(day, time))
        };

        if let Some(obj) = this.as_object() {
            let _ = obj.set(
                PropertyKey::string("__timestamp__"),
                Value::number(timestamp),
            );
        }
        Ok(Value::undefined())
    })
}

/// Install static methods on Date constructor
pub fn install_date_statics(
    date_ctor: GcRef<JsObject>,
    fn_proto: GcRef<JsObject>,
    mm: &Arc<MemoryManager>,
) {
    // Date.now
    let now_fn = Value::native_function_with_proto(
        |_this, _args, _ncx| {
            use std::time::{SystemTime, UNIX_EPOCH};
            let timestamp = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_millis() as f64)
                .unwrap_or(0.0);
            Ok(Value::number(timestamp))
        },
        mm.clone(),
        fn_proto.clone(),
    );
    if let Some(obj) = now_fn.as_object() {
        obj.define_property(
            PropertyKey::string("__non_constructor"),
            PropertyDescriptor::builtin_data(Value::boolean(true)),
        );
        obj.define_property(
            PropertyKey::string("name"),
            PropertyDescriptor::function_length(Value::string(JsString::intern("now"))),
        );
    }
    date_ctor.define_property(
        PropertyKey::string("now"),
        PropertyDescriptor::builtin_method(now_fn),
    );

    // Date.parse(dateString)
    let parse_fn = Value::native_function_with_proto(
        |_this, args, ncx| {
            let s_val = if args.is_empty() {
                "undefined".to_string()
            } else {
                ncx.to_string_value(args.get(0).unwrap())?
            };

            let ts = parse_date_string(&s_val);
            Ok(Value::number(ts))
        },
        mm.clone(),
        fn_proto.clone(),
    );
    if let Some(obj) = parse_fn.as_object() {
        obj.define_property(
            PropertyKey::string("__non_constructor"),
            PropertyDescriptor::builtin_data(Value::boolean(true)),
        );
        obj.define_property(
            PropertyKey::string("name"),
            PropertyDescriptor::function_length(Value::string(JsString::intern("parse"))),
        );
        obj.define_property(
            PropertyKey::string("length"),
            PropertyDescriptor::function_length(Value::int32(1)),
        );
    }
    date_ctor.define_property(
        PropertyKey::string("parse"),
        PropertyDescriptor::builtin_method(parse_fn),
    );

    // Date.UTC(year, month, ...)
    let utc_fn = Value::native_function_with_proto(
        |_this, args, ncx| {
            let year = ncx
                .to_number_value(args.get(0).unwrap_or(&Value::undefined()))?
                .trunc();
            let month = if args.len() > 1 {
                ncx.to_number_value(&args[1])?.trunc()
            } else {
                0.0
            };
            let date = if args.len() > 2 {
                ncx.to_number_value(&args[2])?.trunc()
            } else {
                1.0
            };
            let hours = if args.len() > 3 {
                ncx.to_number_value(&args[3])?.trunc()
            } else {
                0.0
            };
            let minutes = if args.len() > 4 {
                ncx.to_number_value(&args[4])?.trunc()
            } else {
                0.0
            };
            let seconds = if args.len() > 5 {
                ncx.to_number_value(&args[5])?.trunc()
            } else {
                0.0
            };
            let ms = if args.len() > 6 {
                ncx.to_number_value(&args[6])?.trunc()
            } else {
                0.0
            };

            if year.is_nan()
                || month.is_nan()
                || date.is_nan()
                || hours.is_nan()
                || minutes.is_nan()
                || seconds.is_nan()
                || ms.is_nan()
            {
                return Ok(Value::number(f64::NAN));
            }

            let y_int = year as i32;
            let full_year = if y_int >= 0 && y_int <= 99 {
                1900.0 + year
            } else {
                year
            };

            let time = make_time(hours, minutes, seconds, ms);
            let day = make_day(full_year, month, date);
            let clipped = time_clip(make_date(day, time));
            Ok(Value::number(clipped))
        },
        mm.clone(),
        fn_proto.clone(),
    );
    if let Some(obj) = utc_fn.as_object() {
        obj.define_property(
            PropertyKey::string("__non_constructor"),
            PropertyDescriptor::builtin_data(Value::boolean(true)),
        );
        obj.define_property(
            PropertyKey::string("name"),
            PropertyDescriptor::function_length(Value::string(JsString::intern("UTC"))),
        );
        obj.define_property(
            PropertyKey::string("length"),
            PropertyDescriptor::function_length(Value::int32(7)),
        );
    }
    date_ctor.define_property(
        PropertyKey::string("UTC"),
        PropertyDescriptor::builtin_method(utc_fn),
    );

    date_ctor.define_property(
        PropertyKey::string("length"),
        PropertyDescriptor::data_with_attrs(Value::int32(7), PropertyAttributes::function_length()),
    );
}

fn parse_date_string(s: &str) -> f64 {
    use chrono::TimeZone;
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
        // Extended year ±YYYYYY-MM-DDTHH:MM:SS.sssZ
        let year_str = &s[0..7];
        if let Ok(y) = year_str.parse::<i64>() {
            parse_extended_year(y, &s[7..])
        } else {
            f64::NAN
        }
    } else {
        parse_date_string_internal(s)
    };

    if result.is_nan() {
        f64::NAN
    } else {
        time_clip(result)
    }
}

/// Parse an extended year date string like "-04-20T00:00:00.000Z" with a given year.
/// Uses make_day/make_time/make_date for correct arithmetic.
fn parse_extended_year(year: i64, rest: &str) -> f64 {
    // rest should be like "-MM-DD", "-MM-DDTHH:MM:SS.sssZ", "-MM", or ""
    let y = year as f64;
    let (month, day, hour, min, sec, ms, is_utc) = if rest.is_empty() {
        (1.0, 1.0, 0.0, 0.0, 0.0, 0.0, true)
    } else if rest.starts_with('-') {
        let parts = &rest[1..]; // skip leading '-'
        // Parse MM
        if parts.len() < 2 {
            return f64::NAN;
        }
        let month_str = &parts[0..2];
        let month: f64 = match month_str.parse::<u32>() {
            Ok(m) => m as f64,
            Err(_) => return f64::NAN,
        };
        if parts.len() == 2 {
            // Just year-month
            (month, 1.0, 0.0, 0.0, 0.0, 0.0, true)
        } else if parts.len() >= 5 && parts.as_bytes()[2] == b'-' {
            let day_str = &parts[3..5];
            let day: f64 = match day_str.parse::<u32>() {
                Ok(d) => d as f64,
                Err(_) => return f64::NAN,
            };
            if parts.len() == 5 {
                (month, day, 0.0, 0.0, 0.0, 0.0, true)
            } else if parts.len() >= 11 && parts.as_bytes()[5] == b'T' {
                // Parse time: HH:MM, HH:MM:SS, or HH:MM:SS.sss, possibly followed by Z
                let time_part = &parts[6..];
                let (time_str, utc) = if time_part.ends_with('Z') {
                    (&time_part[..time_part.len() - 1], true)
                } else {
                    (time_part, false) // treat as local time
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

    let d = make_day(y, month - 1.0, day); // month is 1-based, make_day uses 0-based
    let t = make_time(hour, min, sec, ms);
    let date = make_date(d, t);
    if is_utc {
        date
    } else {
        // Local time — need to convert
        local_to_utc_ms(y, month - 1.0, day, hour, min, sec, ms)
    }
}

fn parse_date_string_internal(s: &str) -> f64 {
    use chrono::{Local, NaiveDate, NaiveDateTime, TimeZone};
    if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(s) {
        dt.timestamp_millis() as f64
    } else if s.ends_with('Z') {
        let base = &s[..s.len() - 1];
        if let Ok(dt) = NaiveDateTime::parse_from_str(base, "%Y-%m-%dT%H:%M:%S") {
            dt.and_utc().timestamp_millis() as f64
        } else if let Ok(dt) = NaiveDateTime::parse_from_str(base, "%Y-%m-%dT%H:%M:%S%.f") {
            dt.and_utc().timestamp_millis() as f64
        } else if let Ok(dt) = NaiveDateTime::parse_from_str(base, "%Y-%m-%dT%H:%M") {
            dt.and_utc().timestamp_millis() as f64
        } else if let Ok(d) = NaiveDate::parse_from_str(base, "%Y-%m-%d") {
            d.and_hms_opt(0, 0, 0).unwrap().and_utc().timestamp_millis() as f64
        } else {
            f64::NAN
        }
    } else if let Ok(dt) = NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S") {
        local_to_ms(Local.from_local_datetime(&dt))
    } else if let Ok(dt) = NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S%.f") {
        local_to_ms(Local.from_local_datetime(&dt))
    } else if let Ok(dt) = NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M") {
        local_to_ms(Local.from_local_datetime(&dt))
    } else if let Ok(d) = NaiveDate::parse_from_str(s, "%Y-%m-%d") {
        // ES ISO 8601: Date-only is UTC
        d.and_hms_opt(0, 0, 0).unwrap().and_utc().timestamp_millis() as f64
    } else if let Ok(d) = NaiveDate::parse_from_str(s, "%Y-%m") {
        NaiveDate::from_ymd_opt(d.year(), d.month(), 1)
            .unwrap()
            .and_hms_opt(0, 0, 0)
            .unwrap()
            .and_utc()
            .timestamp_millis() as f64
    } else if s.len() == 4 {
        if let Ok(y) = s.parse::<i32>() {
            if let Some(d) = NaiveDate::from_ymd_opt(y, 1, 1) {
                d.and_hms_opt(0, 0, 0).unwrap().and_utc().timestamp_millis() as f64
            } else {
                f64::NAN
            }
        } else {
            f64::NAN
        }
    } else if s.ends_with(" GMT") {
        // RFC 1123 / toUTCString format: "Thu, 01 Jan 1970 00:00:00 GMT"
        if let Ok(dt) = NaiveDateTime::parse_from_str(&s[..s.len() - 4], "%a, %d %b %Y %H:%M:%S") {
            dt.and_utc().timestamp_millis() as f64
        } else {
            f64::NAN
        }
    } else if let Ok(dt) = chrono::DateTime::parse_from_str(s, "%a %b %d %Y %H:%M:%S GMT%z") {
        dt.timestamp_millis() as f64
    } else if let Ok(dt) = NaiveDateTime::parse_from_str(s, "%a %b %d %Y %H:%M:%S") {
        local_to_ms(Local.from_local_datetime(&dt))
    } else if let Ok(d) = NaiveDate::parse_from_str(s, "%a %b %d %Y") {
        if let Some(dt) = d.and_hms_opt(0, 0, 0) {
            local_to_ms(Local.from_local_datetime(&dt))
        } else {
            f64::NAN
        }
    } else {
        f64::NAN
    }
}

/// Wire all Date.prototype methods to the prototype object
pub fn init_date_prototype(
    date_proto: GcRef<JsObject>,
    fn_proto: GcRef<JsObject>,
    mm: &Arc<MemoryManager>,
    to_string_tag_symbol: crate::gc::GcRef<crate::value::Symbol>,
    to_primitive_symbol: crate::gc::GcRef<crate::value::Symbol>,
) {
    // Helper to define methods with correct attributes
    let define_method = {
        let mm = mm.clone();
        let fn_proto = fn_proto.clone();
        let date_proto = date_proto.clone();
        move |name: &str,
              length: i32,
              func: Box<
            dyn Fn(
                    &Value,
                    &[Value],
                    &mut crate::context::NativeContext<'_>,
                ) -> Result<Value, VmError>
                + Send
                + Sync,
        >| {
            let fn_val = Value::native_function_with_proto(func, mm.clone(), fn_proto.clone());
            if let Some(fn_obj) = fn_val.as_object() {
                fn_obj.define_property(
                    PropertyKey::string("name"),
                    PropertyDescriptor::function_length(Value::string(JsString::intern(name))),
                );
                fn_obj.define_property(
                    PropertyKey::string("length"),
                    PropertyDescriptor::function_length(Value::int32(length)),
                );
                fn_obj.define_property(
                    PropertyKey::string("__non_constructor"),
                    PropertyDescriptor::builtin_data(Value::boolean(true)),
                );
            }
            date_proto.define_property(
                PropertyKey::string(name),
                PropertyDescriptor::builtin_method(fn_val),
            );
        }
    };

    // Symbol.toStringTag
    date_proto.define_property(
        PropertyKey::Symbol(to_string_tag_symbol),
        PropertyDescriptor::data_with_attrs(
            Value::string(JsString::intern("Date")),
            PropertyAttributes::builtin_method(),
        ),
    );

    // Symbol.toPrimitive
    let to_prim_fn = Value::native_function_with_proto(
        |this_val, args, ncx| {
            let _obj = this_val
                .as_object()
                .ok_or_else(|| VmError::type_error("toPrimitive called on non-object"))?;
            let prim_arg = args.get(0).cloned().unwrap_or_else(Value::undefined);
            if !prim_arg.is_string() {
                return Err(VmError::type_error(
                    "Symbol.toPrimitive hint must be a string",
                ));
            }
            let hint_str = prim_arg.as_string().unwrap().as_str().to_string();
            if hint_str != "default" && hint_str != "number" && hint_str != "string" {
                return Err(VmError::type_error("Invalid Symbol.toPrimitive hint"));
            }
            let method_names = if hint_str == "string" || hint_str == "default" {
                &["toString", "valueOf"]
            } else {
                &["valueOf", "toString"]
            };

            for name in method_names {
                if let Some(obj) = this_val.as_object() {
                    if let Some(func) = obj.get(&PropertyKey::string(name)) {
                        // Check if callable
                        let is_callable = func.is_callable();
                        if is_callable {
                            match ncx.call_function(&func, this_val.clone(), &[]) {
                                Ok(res) => {
                                    if !res.is_object() {
                                        return Ok(res);
                                    }
                                }
                                Err(e) => return Err(e),
                            }
                        }
                    }
                }
            }
            Err(VmError::type_error(
                "Cannot convert object to primitive value",
            ))
        },
        mm.clone(),
        fn_proto.clone(),
    );
    if let Some(fn_obj) = to_prim_fn.as_object() {
        fn_obj.define_property(
            PropertyKey::string("name"),
            PropertyDescriptor::function_length(Value::string(JsString::intern(
                "[Symbol.toPrimitive]",
            ))),
        );
        fn_obj.define_property(
            PropertyKey::string("length"),
            PropertyDescriptor::function_length(Value::int32(1)),
        );
        fn_obj.define_property(
            PropertyKey::string("__non_constructor"),
            PropertyDescriptor::builtin_data(Value::boolean(true)),
        );
    }
    date_proto.define_property(
        PropertyKey::Symbol(to_primitive_symbol),
        PropertyDescriptor::data_with_attrs(to_prim_fn, PropertyAttributes::function_length()),
    );

    // Methods
    // valueOf()
    define_method(
        "valueOf",
        0,
        Box::new(|this_val, _args, _ncx| {
            let ts = get_timestamp_value(this_val)?;
            Ok(Value::number(time_clip(ts)))
        }),
    );

    // getTime()
    define_method(
        "getTime",
        0,
        Box::new(|this_val, _args, _ncx| {
            let ts = get_timestamp_value(this_val)?;
            Ok(Value::number(time_clip(ts)))
        }),
    );

    // getFullYear()
    define_method(
        "getFullYear",
        0,
        Box::new(|this_val, _args, _ncx| {
            let ts = get_timestamp_value(this_val)?;
            if ts.is_nan() {
                return Ok(Value::nan());
            }
            let dt = ts_to_utc(ts).unwrap(); // valid ts should unwrap
            let local: DateTime<Local> = dt.into();
            Ok(Value::int32(local.year()))
        }),
    );

    // getUTCFullYear()
    define_method(
        "getUTCFullYear",
        0,
        Box::new(|this_val, _args, _ncx| {
            let ts = get_timestamp_value(this_val)?;
            if ts.is_nan() {
                return Ok(Value::nan());
            }
            let dt = ts_to_utc(ts).unwrap();
            Ok(Value::int32(dt.year()))
        }),
    );

    // getMonth()
    define_method(
        "getMonth",
        0,
        Box::new(|this_val, _args, _ncx| {
            let ts = get_timestamp_value(this_val)?;
            if ts.is_nan() {
                return Ok(Value::nan());
            }
            let dt = ts_to_utc(ts).unwrap();
            let local: DateTime<Local> = dt.into();
            Ok(Value::int32(local.month0() as i32))
        }),
    );

    // getUTCMonth()
    define_method(
        "getUTCMonth",
        0,
        Box::new(|this_val, _args, _ncx| {
            let ts = get_timestamp_value(this_val)?;
            if ts.is_nan() {
                return Ok(Value::nan());
            }
            let dt = ts_to_utc(ts).unwrap();
            Ok(Value::int32(dt.month0() as i32))
        }),
    );

    // getDate()
    define_method(
        "getDate",
        0,
        Box::new(|this_val, _args, _ncx| {
            let ts = get_timestamp_value(this_val)?;
            if ts.is_nan() {
                return Ok(Value::nan());
            }
            let dt = ts_to_utc(ts).unwrap();
            let local: DateTime<Local> = dt.into();
            Ok(Value::int32(local.day() as i32))
        }),
    );

    // getUTCDate()
    define_method(
        "getUTCDate",
        0,
        Box::new(|this_val, _args, _ncx| {
            let ts = get_timestamp_value(this_val)?;
            if ts.is_nan() {
                return Ok(Value::nan());
            }
            let dt = ts_to_utc(ts).unwrap();
            Ok(Value::int32(dt.day() as i32))
        }),
    );

    // getDay()
    define_method(
        "getDay",
        0,
        Box::new(|this_val, _args, _ncx| {
            let ts = get_timestamp_value(this_val)?;
            if ts.is_nan() {
                return Ok(Value::nan());
            }
            let dt = ts_to_utc(ts).unwrap();
            let local: DateTime<Local> = dt.into();
            Ok(Value::int32(local.weekday().num_days_from_sunday() as i32))
        }),
    );

    // getUTCDay()
    define_method(
        "getUTCDay",
        0,
        Box::new(|this_val, _args, _ncx| {
            let ts = get_timestamp_value(this_val)?;
            if ts.is_nan() {
                return Ok(Value::nan());
            }
            let dt = ts_to_utc(ts).unwrap();
            Ok(Value::int32(dt.weekday().num_days_from_sunday() as i32))
        }),
    );

    // getHours()
    define_method(
        "getHours",
        0,
        Box::new(|this_val, _args, _ncx| {
            let ts = get_timestamp_value(this_val)?;
            if ts.is_nan() {
                return Ok(Value::nan());
            }
            let dt = ts_to_utc(ts).unwrap();
            let local: DateTime<Local> = dt.into();
            Ok(Value::int32(local.hour() as i32))
        }),
    );

    // getUTCHours()
    define_method(
        "getUTCHours",
        0,
        Box::new(|this_val, _args, _ncx| {
            let ts = get_timestamp_value(this_val)?;
            if ts.is_nan() {
                return Ok(Value::nan());
            }
            let dt = ts_to_utc(ts).unwrap();
            Ok(Value::int32(dt.hour() as i32))
        }),
    );

    // getMinutes()
    define_method(
        "getMinutes",
        0,
        Box::new(|this_val, _args, _ncx| {
            let ts = get_timestamp_value(this_val)?;
            if ts.is_nan() {
                return Ok(Value::nan());
            }
            let dt = ts_to_utc(ts).unwrap();
            let local: DateTime<Local> = dt.into();
            Ok(Value::int32(local.minute() as i32))
        }),
    );

    // getUTCMinutes()
    define_method(
        "getUTCMinutes",
        0,
        Box::new(|this_val, _args, _ncx| {
            let ts = get_timestamp_value(this_val)?;
            if ts.is_nan() {
                return Ok(Value::nan());
            }
            let dt = ts_to_utc(ts).unwrap();
            Ok(Value::int32(dt.minute() as i32))
        }),
    );

    // getSeconds()
    define_method(
        "getSeconds",
        0,
        Box::new(|this_val, _args, _ncx| {
            let ts = get_timestamp_value(this_val)?;
            if ts.is_nan() {
                return Ok(Value::nan());
            }
            let dt = ts_to_utc(ts).unwrap();
            let local: DateTime<Local> = dt.into();
            Ok(Value::int32(local.second() as i32))
        }),
    );

    // getUTCSeconds()
    define_method(
        "getUTCSeconds",
        0,
        Box::new(|this_val, _args, _ncx| {
            let ts = get_timestamp_value(this_val)?;
            if ts.is_nan() {
                return Ok(Value::nan());
            }
            let dt = ts_to_utc(ts).unwrap();
            Ok(Value::int32(dt.second() as i32))
        }),
    );

    // getMilliseconds()
    define_method(
        "getMilliseconds",
        0,
        Box::new(|this_val, _args, _ncx| {
            let ts = get_timestamp_value(this_val)?;
            if ts.is_nan() {
                return Ok(Value::nan());
            }
            Ok(Value::number(ts.rem_euclid(1000.0)))
        }),
    );

    // getUTCMilliseconds()
    define_method(
        "getUTCMilliseconds",
        0,
        Box::new(|this_val, _args, _ncx| {
            let ts = get_timestamp_value(this_val)?;
            if ts.is_nan() {
                return Ok(Value::nan());
            }
            Ok(Value::number(ts.rem_euclid(1000.0)))
        }),
    );

    // getTimezoneOffset()
    define_method(
        "getTimezoneOffset",
        0,
        Box::new(|this_val, _args, _ncx| {
            let ts = get_timestamp_value(this_val)?;
            if ts.is_nan() {
                return Ok(Value::nan());
            }
            let dt = match ts_to_utc(ts) {
                Some(d) => d,
                None => return Ok(Value::nan()),
            };
            let local: DateTime<Local> = dt.into();
            // JS getTimezoneOffset returns minutes from GMT.
            // local.offset().local_minus_utc() returns seconds east of UTC.
            // e.g. UTC+1 -> 3600.
            // JS expects -60.
            // So: -(local_minus_utc / 60)
            let offset_secs = local.offset().local_minus_utc();
            Ok(Value::number((-offset_secs / 60) as f64))
        }),
    );

    // toString()
    define_method(
        "toString",
        0,
        Box::new(|this_val, _args, _ncx| {
            let ts = get_timestamp_value(this_val)?;
            if ts.is_nan() {
                return Ok(Value::string(JsString::intern("Invalid Date")));
            }
            let dt = match ts_to_utc(ts) {
                Some(d) => d,
                None => return Ok(Value::string(JsString::intern("Invalid Date"))),
            };
            let local: DateTime<Local> = dt.into();
            let year_str = format_year(local.year());
            let mon = match local.month() {
                1 => "Jan",
                2 => "Feb",
                3 => "Mar",
                4 => "Apr",
                5 => "May",
                6 => "Jun",
                7 => "Jul",
                8 => "Aug",
                9 => "Sep",
                10 => "Oct",
                11 => "Nov",
                12 => "Dec",
                _ => "Jan",
            };
            let wdy = match local.weekday() {
                chrono::Weekday::Mon => "Mon",
                chrono::Weekday::Tue => "Tue",
                chrono::Weekday::Wed => "Wed",
                chrono::Weekday::Thu => "Thu",
                chrono::Weekday::Fri => "Fri",
                chrono::Weekday::Sat => "Sat",
                chrono::Weekday::Sun => "Sun",
            };
            let offset_secs = local.offset().local_minus_utc();
            let sign = if offset_secs >= 0 { '+' } else { '-' };
            let offset_secs = offset_secs.abs();
            let s = format!(
                "{} {} {:02} {} {:02}:{:02}:{:02} GMT{}{:02}{:02}",
                wdy,
                mon,
                local.day(),
                year_str,
                local.hour(),
                local.minute(),
                local.second(),
                sign,
                offset_secs / 3600,
                (offset_secs % 3600) / 60
            );
            Ok(Value::string(JsString::intern(&s)))
        }),
    );

    // toDateString()
    define_method(
        "toDateString",
        0,
        Box::new(|this_val, _args, _ncx| {
            let ts = get_timestamp_value(this_val)?;
            if ts.is_nan() {
                return Ok(Value::string(JsString::intern("Invalid Date")));
            }
            let dt = match ts_to_utc(ts) {
                Some(d) => d,
                None => return Ok(Value::string(JsString::intern("Invalid Date"))),
            };
            let local: DateTime<Local> = dt.into();
            let year_str = format_year(local.year());
            let mon = match local.month() {
                1 => "Jan",
                2 => "Feb",
                3 => "Mar",
                4 => "Apr",
                5 => "May",
                6 => "Jun",
                7 => "Jul",
                8 => "Aug",
                9 => "Sep",
                10 => "Oct",
                11 => "Nov",
                12 => "Dec",
                _ => "Jan",
            };
            let wdy = match local.weekday() {
                chrono::Weekday::Mon => "Mon",
                chrono::Weekday::Tue => "Tue",
                chrono::Weekday::Wed => "Wed",
                chrono::Weekday::Thu => "Thu",
                chrono::Weekday::Fri => "Fri",
                chrono::Weekday::Sat => "Sat",
                chrono::Weekday::Sun => "Sun",
            };
            let s = format!("{} {} {:02} {}", wdy, mon, local.day(), year_str);
            Ok(Value::string(JsString::intern(&s)))
        }),
    );

    // toTimeString()
    define_method(
        "toTimeString",
        0,
        Box::new(|this_val, _args, _ncx| {
            let ts = get_timestamp_value(this_val)?;
            if ts.is_nan() {
                return Ok(Value::string(JsString::intern("Invalid Date")));
            }
            let dt = match ts_to_utc(ts) {
                Some(d) => d,
                None => return Ok(Value::string(JsString::intern("Invalid Date"))),
            };
            let local: DateTime<Local> = dt.into();
            let offset_secs = local.offset().local_minus_utc();
            let sign = if offset_secs >= 0 { '+' } else { '-' };
            let offset_secs = offset_secs.abs();
            let s = format!(
                "{:02}:{:02}:{:02} GMT{}{:02}{:02}",
                local.hour(),
                local.minute(),
                local.second(),
                sign,
                offset_secs / 3600,
                (offset_secs % 3600) / 60
            );
            Ok(Value::string(JsString::intern(&s)))
        }),
    );

    // toISOString()
    define_method(
        "toISOString",
        0,
        Box::new(|this_val, _args, _ncx| {
            let ts = get_timestamp_value(this_val)?;
            if ts.is_nan() || !ts.is_finite() || ts.abs() > 8_640_000_000_000_000.0 {
                return Err(VmError::range_error("Invalid time value"));
            }
            // Use UTC components (works for all valid JS Date ranges including extreme years)
            let (y, m, d, h, min, sec, ms_val) = utc_components(ts);
            if y.is_nan() {
                return Err(VmError::range_error("Invalid time value"));
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
                m as u32 + 1, // month0 → month1
                d as u32,
                h as u32,
                min as u32,
                sec as u32,
                ms_val as u32
            );
            Ok(Value::string(JsString::intern(&s)))
        }),
    );

    // toUTCString()
    define_method(
        "toUTCString",
        0,
        Box::new(|this_val, _args, _ncx| {
            let ts = get_timestamp_value(this_val)?;
            if ts.is_nan() {
                return Ok(Value::string(JsString::intern("Invalid Date")));
            }
            let dt = match ts_to_utc(ts) {
                Some(d) => d,
                None => return Ok(Value::string(JsString::intern("Invalid Date"))),
            };
            let y = dt.year();
            let year_str = if y < 0 {
                format!("-{:04}", y.abs())
            } else {
                format!("{:04}", y)
            };
            let wdy = match dt.weekday() {
                chrono::Weekday::Mon => "Mon",
                chrono::Weekday::Tue => "Tue",
                chrono::Weekday::Wed => "Wed",
                chrono::Weekday::Thu => "Thu",
                chrono::Weekday::Fri => "Fri",
                chrono::Weekday::Sat => "Sat",
                chrono::Weekday::Sun => "Sun",
            };
            let mon = match dt.month() {
                1 => "Jan",
                2 => "Feb",
                3 => "Mar",
                4 => "Apr",
                5 => "May",
                6 => "Jun",
                7 => "Jul",
                8 => "Aug",
                9 => "Sep",
                10 => "Oct",
                11 => "Nov",
                12 => "Dec",
                _ => "Jan",
            };
            let s = format!(
                "{}, {:02} {} {} {:02}:{:02}:{:02} GMT",
                wdy,
                dt.day(),
                mon,
                year_str,
                dt.hour(),
                dt.minute(),
                dt.second()
            );
            Ok(Value::string(JsString::intern(&s)))
        }),
    );

    // toJSON()
    define_method(
        "toJSON",
        1,
        Box::new(|this_val, _args, ncx| {
            // 1. Let O be ? ToObject(this value).
            let obj = to_object(ncx, this_val)?;
            let obj_val = Value::object(obj.clone());
            // 2. Let tv be ? ToPrimitive(O, number).
            let tv = ncx.to_primitive(&obj_val, PreferredType::Number)?;
            // 3. If Type(tv) is Number and tv is not finite, return null.
            if let Some(n) = tv.as_number() {
                if n.is_nan() || n.is_infinite() {
                    return Ok(Value::null());
                }
            }
            if let Some(i) = tv.as_int32() {
                // int32 is always finite, continue
                let _ = i;
            }
            // 4. Return ? Invoke(O, "toISOString").
            // Use spec-compliant [[Get]] that invokes getters
            let key = PropertyKey::string("toISOString");
            let func = get_value_from_object(ncx, &obj, &key, obj_val.clone())?;
            if !func.is_callable() {
                return Err(VmError::type_error("toISOString is not a function"));
            }
            ncx.call_function(&func, obj_val, &[])
        }),
    );

    // toLocaleString, toLocaleDateString, toLocaleTimeString - simple stubs wrapping toString
    define_method(
        "toLocaleString",
        0,
        Box::new(|this_val, _args, ncx| {
            let intrinsics = ncx
                .ctx
                .realm_intrinsics(ncx.ctx.realm_id())
                .expect("intrinsics");
            let fn_val = Value::object(intrinsics.date_prototype.clone())
                .as_object()
                .unwrap()
                .get(&PropertyKey::string("toString"))
                .unwrap();
            ncx.call_function(&fn_val, this_val.clone(), &[])
        }),
    );
    define_method(
        "toLocaleDateString",
        0,
        Box::new(|this_val, _args, ncx| {
            let intrinsics = ncx
                .ctx
                .realm_intrinsics(ncx.ctx.realm_id())
                .expect("intrinsics");
            let fn_val = Value::object(intrinsics.date_prototype.clone())
                .as_object()
                .unwrap()
                .get(&PropertyKey::string("toDateString"))
                .unwrap();
            ncx.call_function(&fn_val, this_val.clone(), &[])
        }),
    );
    define_method(
        "toLocaleTimeString",
        0,
        Box::new(|this_val, _args, ncx| {
            let intrinsics = ncx
                .ctx
                .realm_intrinsics(ncx.ctx.realm_id())
                .expect("intrinsics");
            let fn_val = Value::object(intrinsics.date_prototype.clone())
                .as_object()
                .unwrap()
                .get(&PropertyKey::string("toTimeString"))
                .unwrap();
            ncx.call_function(&fn_val, this_val.clone(), &[])
        }),
    );

    // setTime()
    define_method(
        "setTime",
        1,
        Box::new(|this_val, args, ncx| {
            let _ts = get_timestamp_value(this_val)?; // Check receiver
            let t = ncx.to_number_value(args.first().unwrap_or(&Value::undefined()))?;
            let t = time_clip(t);
            // Update slot
            if let Some(obj) = this_val.as_object() {
                obj.set(PropertyKey::string("__timestamp__"), Value::number(t))
                    .unwrap();
            }
            Ok(Value::number(t))
        }),
    );

    // setMilliseconds()
    define_method(
        "setMilliseconds",
        1,
        Box::new(|this_val, args, ncx| {
            let ts = get_timestamp_value(this_val)?;
            let ms = ncx
                .to_number_value(args.get(0).unwrap_or(&Value::undefined()))?
                .trunc();
            if ts.is_nan() {
                return Ok(Value::nan());
            }

            let dt = match ts_to_utc(ts) {
                Some(d) => d,
                None => return Ok(Value::nan()),
            };
            let local: DateTime<Local> = dt.into();
            let naive = local.naive_local();

            let h = naive.hour();
            let min = naive.minute();
            let s = naive.second();
            let time_ms =
                (h as f64 * 3_600_000.0) + (min as f64 * 60_000.0) + (s as f64 * 1_000.0) + ms;

            let days = (time_ms / 86_400_000.0).floor();
            let rem_ms = time_ms % 86_400_000.0;
            let rem_ms = if rem_ms < 0.0 {
                rem_ms + 86_400_000.0
            } else {
                rem_ms
            };

            let new_date = safe_add_days(naive.date(), days);
            let new_naive = new_date.and_then(|d| {
                d.and_hms_opt(0, 0, 0)
                    .unwrap()
                    .checked_add_signed(chrono::Duration::milliseconds(rem_ms as i64))
            });

            if let Some(n) = new_naive {
                let res = Local.from_local_datetime(&n);
                let new_ts = time_clip(local_to_ms(res));
                if new_ts.is_nan() {
                    return Ok(Value::nan());
                }

                if let Some(obj) = this_val.as_object() {
                    obj.set(PropertyKey::string("__timestamp__"), Value::number(new_ts))
                        .unwrap();
                }
                return Ok(Value::number(new_ts));
            }
            Ok(Value::nan())
        }),
    );

    // setUTCMilliseconds()
    define_method(
        "setUTCMilliseconds",
        1,
        Box::new(|this_val, args, ncx| {
            let ts = get_timestamp_value(this_val)?;
            let ms = ncx
                .to_number_value(args.get(0).unwrap_or(&Value::undefined()))?
                .trunc();
            if ts.is_nan() {
                return Ok(Value::nan());
            }

            let dt = match ts_to_utc(ts) {
                Some(d) => d,
                None => return Ok(Value::nan()),
            };
            let h = dt.hour();
            let min = dt.minute();
            let s = dt.second();
            let time_ms =
                (h as f64 * 3_600_000.0) + (min as f64 * 60_000.0) + (s as f64 * 1_000.0) + ms;

            let days = (time_ms / 86_400_000.0).floor();
            let rem_ms = time_ms % 86_400_000.0;
            let rem_ms = if rem_ms < 0.0 {
                rem_ms + 86_400_000.0
            } else {
                rem_ms
            };

            let new_date = safe_add_days(dt.date_naive(), days);
            let new_dt = new_date.and_then(|d| {
                d.and_hms_opt(0, 0, 0)
                    .unwrap()
                    .and_utc()
                    .checked_add_signed(chrono::Duration::milliseconds(rem_ms as i64))
            });

            if let Some(res) = new_dt {
                let new_ts = time_clip(res.timestamp_millis() as f64);
                if new_ts.is_nan() {
                    return Ok(Value::nan());
                }

                if let Some(obj) = this_val.as_object() {
                    obj.set(PropertyKey::string("__timestamp__"), Value::number(new_ts))
                        .unwrap();
                }
                return Ok(Value::number(new_ts));
            }
            Ok(Value::nan())
        }),
    );

    // setSeconds()
    define_method(
        "setSeconds",
        2,
        Box::new(|this_val, args, ncx| {
            let ts = get_timestamp_value(this_val)?;
            let sec = ncx
                .to_number_value(args.get(0).unwrap_or(&Value::undefined()))?
                .trunc();
            let ms = if args.len() > 1 {
                Some(ncx.to_number_value(&args[1])?.trunc())
            } else {
                None
            };
            if ts.is_nan() {
                return Ok(Value::nan());
            }

            let dt = match ts_to_utc(ts) {
                Some(d) => d,
                None => return Ok(Value::nan()),
            };
            let local: DateTime<Local> = dt.into();
            let naive = local.naive_local();

            let h = naive.hour();
            let m = naive.minute();
            let time_ms = (h as f64 * 3_600_000.0)
                + (m as f64 * 60_000.0)
                + (sec * 1_000.0)
                + ms.unwrap_or(naive.nanosecond() as f64 / 1_000_000.0);

            let days = (time_ms / 86_400_000.0).floor();
            let rem_ms = time_ms % 86_400_000.0;
            let rem_ms = if rem_ms < 0.0 {
                rem_ms + 86_400_000.0
            } else {
                rem_ms
            };

            let new_date = safe_add_days(naive.date(), days);
            let new_naive = new_date.and_then(|d| {
                d.and_hms_opt(0, 0, 0)
                    .unwrap()
                    .checked_add_signed(chrono::Duration::milliseconds(rem_ms as i64))
            });

            if let Some(n) = new_naive {
                let res = Local.from_local_datetime(&n);
                let new_ts = time_clip(local_to_ms(res));
                if new_ts.is_nan() {
                    return Ok(Value::nan());
                }

                if let Some(obj) = this_val.as_object() {
                    obj.set(PropertyKey::string("__timestamp__"), Value::number(new_ts))
                        .unwrap();
                }
                return Ok(Value::number(new_ts));
            }
            Ok(Value::nan())
        }),
    );

    // setUTCSeconds()
    define_method(
        "setUTCSeconds",
        2,
        Box::new(|this_val, args, ncx| {
            let ts = get_timestamp_value(this_val)?;
            let sec = ncx
                .to_number_value(args.get(0).unwrap_or(&Value::undefined()))?
                .trunc();
            let ms = if args.len() > 1 {
                Some(ncx.to_number_value(&args[1])?.trunc())
            } else {
                None
            };
            if ts.is_nan() {
                return Ok(Value::nan());
            }

            // UTC: Extract Y,M,D,H,M. Replace S, MS.
            let dt = match ts_to_utc(ts) {
                Some(d) => d,
                None => return Ok(Value::nan()),
            };
            let h = dt.hour();
            let m = dt.minute();
            let cur_ms = dt.nanosecond() as f64 / 1_000_000.0;

            // time part in ms from start of day
            let time_ms = (h as f64 * 3_600_000.0)
                + (m as f64 * 60_000.0)
                + (sec * 1_000.0)
                + ms.unwrap_or(cur_ms);

            let days = (time_ms / 86_400_000.0).floor();
            let rem_ms = time_ms % 86_400_000.0;
            let rem_ms = if rem_ms < 0.0 {
                rem_ms + 86_400_000.0
            } else {
                rem_ms
            };

            let new_date = safe_add_days(dt.date_naive(), days);
            let new_dt = new_date.and_then(|d| {
                d.and_hms_opt(0, 0, 0)
                    .unwrap()
                    .and_utc()
                    .checked_add_signed(chrono::Duration::milliseconds(rem_ms as i64))
            });

            if let Some(res) = new_dt {
                let new_ts = time_clip(res.timestamp_millis() as f64);
                if new_ts.is_nan() {
                    return Ok(Value::nan());
                }

                if let Some(obj) = this_val.as_object() {
                    obj.set(PropertyKey::string("__timestamp__"), Value::number(new_ts))
                        .unwrap();
                }
                return Ok(Value::number(new_ts));
            }
            Ok(Value::nan())
        }),
    );

    // setMinutes()
    define_method(
        "setMinutes",
        3,
        Box::new(|this_val, args, ncx| {
            let ts = get_timestamp_value(this_val)?;
            let min = ncx
                .to_number_value(args.get(0).unwrap_or(&Value::undefined()))?
                .trunc();
            let sec = if args.len() > 1 {
                Some(ncx.to_number_value(&args[1])?.trunc())
            } else {
                None
            };
            let ms = if args.len() > 2 {
                Some(ncx.to_number_value(&args[2])?.trunc())
            } else {
                None
            };
            if ts.is_nan() {
                return Ok(Value::nan());
            }

            let dt = match ts_to_utc(ts) {
                Some(d) => d,
                None => return Ok(Value::nan()),
            };
            let local: DateTime<Local> = dt.into();
            let naive = local.naive_local();

            let h = naive.hour();
            let cur_sec = naive.second() as f64;
            let cur_ms = naive.nanosecond() as f64 / 1_000_000.0;

            let time_ms = (h as f64 * 3_600_000.0)
                + (min * 60_000.0)
                + (sec.unwrap_or(cur_sec) * 1_000.0)
                + ms.unwrap_or(cur_ms);

            let days = (time_ms / 86_400_000.0).floor();
            let rem_ms = time_ms % 86_400_000.0;
            let rem_ms = if rem_ms < 0.0 {
                rem_ms + 86_400_000.0
            } else {
                rem_ms
            };

            let new_date = safe_add_days(naive.date(), days);
            let new_naive = new_date.and_then(|d| {
                d.and_hms_opt(0, 0, 0)
                    .unwrap()
                    .checked_add_signed(chrono::Duration::milliseconds(rem_ms as i64))
            });

            if let Some(n) = new_naive {
                let res = Local.from_local_datetime(&n);
                let mut new_ts = local_to_ms(res);
                // TimeClip: normalize -0 to +0
                if new_ts == 0.0 {
                    new_ts = 0.0;
                }

                if let Some(obj) = this_val.as_object() {
                    obj.set(PropertyKey::string("__timestamp__"), Value::number(new_ts))
                        .unwrap();
                }
                return Ok(Value::number(new_ts));
            }
            Ok(Value::nan())
        }),
    );

    // setUTCMinutes()
    define_method(
        "setUTCMinutes",
        3,
        Box::new(|this_val, args, ncx| {
            let ts = get_timestamp_value(this_val)?;
            let min = ncx
                .to_number_value(args.get(0).unwrap_or(&Value::undefined()))?
                .trunc();
            let sec = if args.len() > 1 {
                Some(ncx.to_number_value(&args[1])?.trunc())
            } else {
                None
            };
            let ms = if args.len() > 2 {
                Some(ncx.to_number_value(&args[2])?.trunc())
            } else {
                None
            };
            if ts.is_nan() {
                return Ok(Value::nan());
            }

            let dt = match ts_to_utc(ts) {
                Some(d) => d,
                None => return Ok(Value::nan()),
            };
            let h = dt.hour();
            let cur_sec = dt.second() as f64;
            let cur_ms = dt.nanosecond() as f64 / 1_000_000.0;

            let time_ms = (h as f64 * 3_600_000.0)
                + (min * 60_000.0)
                + (sec.unwrap_or(cur_sec) * 1_000.0)
                + ms.unwrap_or(cur_ms);

            let days = (time_ms / 86_400_000.0).floor();
            let rem_ms = time_ms % 86_400_000.0;
            let rem_ms = if rem_ms < 0.0 {
                rem_ms + 86_400_000.0
            } else {
                rem_ms
            };

            let new_date = safe_add_days(dt.date_naive(), days);
            let new_dt = new_date.and_then(|d| {
                d.and_hms_opt(0, 0, 0)
                    .unwrap()
                    .and_utc()
                    .checked_add_signed(chrono::Duration::milliseconds(rem_ms as i64))
            });

            if let Some(res) = new_dt {
                let mut new_ts = res.timestamp_millis() as f64;
                // TimeClip: normalize -0 to +0
                if new_ts == 0.0 {
                    new_ts = 0.0;
                }

                if let Some(obj) = this_val.as_object() {
                    obj.set(PropertyKey::string("__timestamp__"), Value::number(new_ts))
                        .unwrap();
                }
                return Ok(Value::number(new_ts));
            }
            Ok(Value::nan())
        }),
    );

    // setHours()
    define_method(
        "setHours",
        4,
        Box::new(|this_val, args, ncx| {
            let ts = get_timestamp_value(this_val)?;
            let hour = ncx
                .to_number_value(args.get(0).unwrap_or(&Value::undefined()))?
                .trunc();
            let min = if args.len() > 1 {
                Some(ncx.to_number_value(&args[1])?.trunc())
            } else {
                None
            };
            let sec = if args.len() > 2 {
                Some(ncx.to_number_value(&args[2])?.trunc())
            } else {
                None
            };
            let ms = if args.len() > 3 {
                Some(ncx.to_number_value(&args[3])?.trunc())
            } else {
                None
            };
            if ts.is_nan() {
                return Ok(Value::nan());
            }

            let dt = match ts_to_utc(ts) {
                Some(d) => d,
                None => return Ok(Value::nan()),
            };

            let local: DateTime<Local> = dt.into();
            let naive = local.naive_local();

            let cur_min = naive.minute() as f64;
            let cur_sec = naive.second() as f64;
            let cur_ms = naive.nanosecond() as f64 / 1_000_000.0;

            let time_ms = (hour * 3_600_000.0)
                + (min.unwrap_or(cur_min) * 60_000.0)
                + (sec.unwrap_or(cur_sec) * 1_000.0)
                + ms.unwrap_or(cur_ms);

            let days = (time_ms / 86_400_000.0).floor();
            let rem_ms = time_ms % 86_400_000.0;
            let rem_ms = if rem_ms < 0.0 {
                rem_ms + 86_400_000.0
            } else {
                rem_ms
            };

            let new_date = safe_add_days(naive.date(), days);
            let new_naive = new_date.and_then(|d| {
                d.and_hms_opt(0, 0, 0)
                    .unwrap()
                    .checked_add_signed(chrono::Duration::milliseconds(rem_ms as i64))
            });

            if let Some(n) = new_naive {
                let res = Local.from_local_datetime(&n);
                let mut new_ts = local_to_ms(res);
                // TimeClip: normalize -0 to +0
                if new_ts == 0.0 {
                    new_ts = 0.0;
                }

                if let Some(obj) = this_val.as_object() {
                    obj.set(PropertyKey::string("__timestamp__"), Value::number(new_ts))
                        .unwrap();
                }
                return Ok(Value::number(new_ts));
            }
            Ok(Value::nan())
        }),
    );

    // setUTCHours()
    define_method(
        "setUTCHours",
        4,
        Box::new(|this_val, args, ncx| {
            let ts = get_timestamp_value(this_val)?;
            let hour = ncx
                .to_number_value(args.get(0).unwrap_or(&Value::undefined()))?
                .trunc();
            let min = if args.len() > 1 {
                Some(ncx.to_number_value(&args[1])?.trunc())
            } else {
                None
            };
            let sec = if args.len() > 2 {
                Some(ncx.to_number_value(&args[2])?.trunc())
            } else {
                None
            };
            let ms = if args.len() > 3 {
                Some(ncx.to_number_value(&args[3])?.trunc())
            } else {
                None
            };
            if ts.is_nan() {
                return Ok(Value::nan());
            }

            let dt = match ts_to_utc(ts) {
                Some(d) => d,
                None => return Ok(Value::nan()),
            };
            let cur_min = dt.minute() as f64;
            let cur_sec = dt.second() as f64;
            let cur_ms = dt.nanosecond() as f64 / 1_000_000.0;

            let time_ms = (hour * 3_600_000.0)
                + (min.unwrap_or(cur_min) * 60_000.0)
                + (sec.unwrap_or(cur_sec) * 1_000.0)
                + ms.unwrap_or(cur_ms);

            let days = (time_ms / 86_400_000.0).floor();
            let rem_ms = time_ms % 86_400_000.0;
            let rem_ms = if rem_ms < 0.0 {
                rem_ms + 86_400_000.0
            } else {
                rem_ms
            };

            let new_date = safe_add_days(dt.date_naive(), days);
            let new_dt = new_date.and_then(|d| {
                d.and_hms_opt(0, 0, 0)
                    .unwrap()
                    .and_utc()
                    .checked_add_signed(chrono::Duration::milliseconds(rem_ms as i64))
            });

            if let Some(res) = new_dt {
                let mut new_ts = res.timestamp_millis() as f64;
                // TimeClip: normalize -0 to +0
                if new_ts == 0.0 {
                    new_ts = 0.0;
                }

                if let Some(obj) = this_val.as_object() {
                    obj.set(PropertyKey::string("__timestamp__"), Value::number(new_ts))
                        .unwrap();
                }
                return Ok(Value::number(new_ts));
            }
            Ok(Value::nan())
        }),
    );

    // setDate()
    define_method(
        "setDate",
        1,
        Box::new(|this_val, args, ncx| {
            let ts = get_timestamp_value(this_val)?;
            let date_arg = ncx
                .to_number_value(args.first().unwrap_or(&Value::undefined()))?
                .trunc();
            if ts.is_nan() {
                return Ok(Value::nan());
            }

            let dt = match ts_to_utc(ts) {
                Some(d) => d,
                None => return Ok(Value::nan()),
            };
            let local: DateTime<Local> = dt.into();
            let naive = local.naive_local();

            // MakeDay(Year, Month, date_arg)
            let y = naive.year();
            let m = naive.month0() as i32; // 0-11
            let d = date_arg; // f64

            // Logic: Construct (y, m, 1), add (d-1) days.
            let base_date = NaiveDate::from_ymd_opt(y, (m as u32) + 1, 1).unwrap();
            let days_to_add = d - 1.0;
            let ms_to_add = days_to_add * 86_400_000.0;
            let dur = match safe_cast_time(ms_to_add) {
                Some(v) => chrono::Duration::milliseconds(v),
                None => return Ok(Value::nan()),
            };
            let new_date = base_date.checked_add_signed(dur);

            if let Some(nd) = new_date {
                let new_naive = nd.and_time(naive.time());
                let res = Local.from_local_datetime(&new_naive);
                let mut new_ts = local_to_ms(res);
                // TimeClip: normalize -0 to +0
                if new_ts == 0.0 {
                    new_ts = 0.0;
                }

                if let Some(obj) = this_val.as_object() {
                    obj.set(PropertyKey::string("__timestamp__"), Value::number(new_ts))
                        .unwrap();
                }
                return Ok(Value::number(new_ts));
            }
            Ok(Value::nan())
        }),
    );

    // setUTCDate()
    define_method(
        "setUTCDate",
        1,
        Box::new(|this_val, args, ncx| {
            let ts = get_timestamp_value(this_val)?;
            let date_arg = ncx
                .to_number_value(args.first().unwrap_or(&Value::undefined()))?
                .trunc();
            if ts.is_nan() {
                return Ok(Value::nan());
            }

            let dt = match ts_to_utc(ts) {
                Some(d) => d,
                None => return Ok(Value::nan()),
            };
            let y = dt.year();
            let m = dt.month0() as i32;
            let d = date_arg;

            let base_date = NaiveDate::from_ymd_opt(y, (m as u32) + 1, 1).unwrap();
            let days_to_add = d - 1.0;
            let new_date = safe_add_days(base_date, days_to_add);

            if let Some(nd) = new_date {
                let new_dt = nd.and_time(dt.time()).and_utc();
                let mut new_ts = new_dt.timestamp_millis() as f64;
                // TimeClip: normalize -0 to +0
                if new_ts == 0.0 {
                    new_ts = 0.0;
                }

                if let Some(obj) = this_val.as_object() {
                    obj.set(PropertyKey::string("__timestamp__"), Value::number(new_ts))
                        .unwrap();
                }
                return Ok(Value::number(new_ts));
            }
            Ok(Value::nan())
        }),
    );

    // setMonth()
    define_method(
        "setMonth",
        2,
        Box::new(|this_val, args, ncx| {
            let ts = get_timestamp_value(this_val)?;
            // Evaluate args BEFORE NaN check (for side effects)
            let mon = ncx.to_number_value(args.get(0).unwrap_or(&Value::undefined()))?;
            let date_arg = if args.len() > 1 {
                Some(ncx.to_number_value(&args[1])?)
            } else {
                None
            };
            // Per spec: If t is NaN, return NaN (don't recover)
            if ts.is_nan() {
                return Ok(Value::nan());
            }
            let (cur_y, _cur_m, cur_d, h, min, sec, ms) = local_components(ts);
            let d = date_arg.unwrap_or(cur_d);
            let new_date = local_to_utc_ms(cur_y, mon, d, h, min, sec, ms);
            set_date_value(this_val, new_date)
        }),
    );

    // setUTCMonth()
    define_method(
        "setUTCMonth",
        2,
        Box::new(|this_val, args, ncx| {
            let ts = get_timestamp_value(this_val)?;
            // Evaluate args BEFORE NaN check (for side effects)
            let mon = ncx.to_number_value(args.get(0).unwrap_or(&Value::undefined()))?;
            let date_arg = if args.len() > 1 {
                Some(ncx.to_number_value(&args[1])?)
            } else {
                None
            };
            // Per spec: If t is NaN, return NaN
            if ts.is_nan() {
                return Ok(Value::nan());
            }
            let (cur_y, _cur_m, cur_d, h, min, sec, ms) = utc_components(ts);
            let d = date_arg.unwrap_or(cur_d);
            let day = make_day(cur_y, mon, d);
            let time = make_time(h, min, sec, ms);
            let new_date = make_date(day, time);
            set_date_value(this_val, new_date)
        }),
    );

    // setFullYear()
    define_method(
        "setFullYear",
        3,
        Box::new(|this_val, args, ncx| {
            let ts = get_timestamp_value(this_val)?;
            // Per spec: If t is NaN, let t be +0
            let t = if ts.is_nan() { 0.0 } else { ts };
            let y = ncx.to_number_value(args.get(0).unwrap_or(&Value::undefined()))?;
            let mon_arg = if args.len() > 1 {
                Some(ncx.to_number_value(&args[1])?)
            } else {
                None
            };
            let date_arg = if args.len() > 2 {
                Some(ncx.to_number_value(&args[2])?)
            } else {
                None
            };
            let (_cur_y, cur_m, cur_d, h, min, sec, ms) = local_components(t);
            let m = mon_arg.unwrap_or(cur_m);
            let d = date_arg.unwrap_or(cur_d);
            let new_date = local_to_utc_ms(y, m, d, h, min, sec, ms);
            set_date_value(this_val, new_date)
        }),
    );

    // setUTCFullYear()
    define_method(
        "setUTCFullYear",
        3,
        Box::new(|this_val, args, ncx| {
            let ts = get_timestamp_value(this_val)?;
            // Per spec: If t is NaN, let t be +0
            let t = if ts.is_nan() { 0.0 } else { ts };
            let y = ncx.to_number_value(args.get(0).unwrap_or(&Value::undefined()))?;
            let mon_arg = if args.len() > 1 {
                Some(ncx.to_number_value(&args[1])?)
            } else {
                None
            };
            let date_arg = if args.len() > 2 {
                Some(ncx.to_number_value(&args[2])?)
            } else {
                None
            };
            let (_cur_y, cur_m, cur_d, h, min, sec, ms) = utc_components(t);
            let m = mon_arg.unwrap_or(cur_m);
            let d = date_arg.unwrap_or(cur_d);
            // UTC version: no local conversion needed
            let day = make_day(y, m, d);
            let time = make_time(h, min, sec, ms);
            let new_date = make_date(day, time);
            set_date_value(this_val, new_date)
        }),
    );

    // setYear() - Legacy (Annex B §B.2.4.2)
    define_method(
        "setYear",
        1,
        Box::new(|this_val, args, ncx| {
            // 1-3. Let t be thisTimeValue(this value).
            let ts = get_timestamp_value(this_val)?;
            // 4. Let y be ? ToNumber(year).
            let y = ncx.to_number_value(args.first().unwrap_or(&Value::undefined()))?;
            // If y is NaN, set [[DateValue]] to NaN and return NaN
            if y.is_nan() {
                return set_date_value(this_val, f64::NAN);
            }
            // 5. If t is NaN, set t to +0; otherwise, set t to LocalTime(t).
            let t = if ts.is_nan() { 0.0 } else { ts };
            // 6. MakeFullYear: if 0 <= y <= 99, yyyy = 1900 + y, else yyyy = y
            let y_int = y.trunc();
            let full_year = if y_int >= 0.0 && y_int <= 99.0 {
                1900.0 + y_int
            } else {
                y_int
            };
            let (_cur_y, cur_m, cur_d, h, min, sec, ms) = local_components(t);
            // 7-8. date = MakeDay(yyyy, m, dt), result = UTC(MakeDate(date, TimeWithinDay(t)))
            let new_date = local_to_utc_ms(full_year, cur_m, cur_d, h, min, sec, ms);
            set_date_value(this_val, new_date)
        }),
    );

    // getYear() - Legacy
    define_method(
        "getYear",
        0,
        Box::new(|this_val, _args, _ncx| {
            let ts = get_timestamp_value(this_val)?;
            if ts.is_nan() {
                return Ok(Value::nan());
            }
            let dt = ts_to_utc(ts).unwrap();
            let local: DateTime<Local> = dt.into();
            Ok(Value::int32(local.year() - 1900))
        }),
    );

    // toGMTString() - Legacy: must be the SAME function object as toUTCString
    if let Some(utc_string_fn) = date_proto.get(&PropertyKey::string("toUTCString")) {
        date_proto.define_property(
            PropertyKey::string("toGMTString"),
            PropertyDescriptor::builtin_method(utc_string_fn),
        );
    }

    // toTemporalInstant()
    // Spec: Date.prototype.toTemporalInstant ( )
    // 1. Let dateObject be the this value.
    // 2. Perform ? RequireInternalSlot(dateObject, [[DateValue]]).
    // 3. Let t be dateObject.[[DateValue]].
    // 4. Let ns be ? NumberToBigInt(t) × ℤ(10**6).
    // 5. Return ! CreateTemporalInstant(ns).
    define_method(
        "toTemporalInstant",
        0,
        Box::new(|this_val, _args, ncx| {
            // RequireInternalSlot(dateObject, [[DateValue]])
            let ts = get_timestamp_value(this_val)?;

            // NumberToBigInt(t) — must be an integer, NaN throws RangeError
            if ts.is_nan() || !ts.is_finite() {
                return Err(VmError::range_error(
                    "Cannot convert NaN or Infinity to BigInt",
                ));
            }
            if ts.fract() != 0.0 {
                return Err(VmError::range_error("Cannot convert non-integer to BigInt"));
            }

            // Compute ns = t * 10^6 using i128 to avoid overflow
            // t is in range [-8.64e15, 8.64e15], so t * 10^6 is [-8.64e21, 8.64e21]
            // which fits in i128 but not i64
            let t_int = ts as i128;
            let ns = t_int * 1_000_000;
            let ns_str = ns.to_string();

            // Create a Temporal.Instant-like object with epochNanoseconds
            let mm = ncx.memory_manager().clone();
            let global = ncx.global();

            // Try to get Temporal.Instant prototype if it exists
            let instant_proto = global
                .get(&PropertyKey::string("Temporal"))
                .and_then(|t| t.as_object())
                .and_then(|t| t.get(&PropertyKey::string("Instant")))
                .and_then(|i| i.as_object())
                .and_then(|i| i.get(&PropertyKey::string("prototype")))
                .and_then(|p| p.as_object());

            let proto = instant_proto.map(Value::object).unwrap_or_else(Value::null);

            let instant = GcRef::new(JsObject::new(proto, mm));
            let _ = instant.set(
                PropertyKey::string("epochNanoseconds"),
                Value::bigint(ns_str),
            );

            Ok(Value::object(instant))
        }),
    );
}
