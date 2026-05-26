//! Shared coercion / extraction helpers for `Temporal.*` native
//! function bodies.
//!
//! Every helper takes a [`NativeCtx`] / `&[Value]` pair so the
//! per-class `couch!` blocks can call algorithms with the same
//! signature the macro itself uses (no bridge layer).

#![allow(missing_docs)]

use crate::object::{self, JsObject};
use crate::string::JsString;
use crate::temporal::payload::{JsTemporal, TemporalPayload};
use crate::{NativeCtx, NativeError, Value};

#[must_use]
pub fn arg_or_undef(args: &[Value], index: usize) -> Value {
    args.get(index).copied().unwrap_or(Value::undefined())
}

pub fn make_temporal(
    ctx: &mut NativeCtx<'_>,
    payload: TemporalPayload,
) -> Result<Value, NativeError> {
    let handle = JsTemporal::new(ctx.heap_mut(), payload).map_err(|_| NativeError::TypeError {
        name: "Temporal",
        reason: "out of memory".to_string(),
    })?;
    Ok(Value::temporal(handle))
}

pub fn js_string_value(value: String, ctx: &mut NativeCtx<'_>) -> Result<Value, NativeError> {
    let s = JsString::from_str(&value, ctx.heap_mut()).map_err(|_| NativeError::TypeError {
        name: "Temporal",
        reason: "out of memory".to_string(),
    })?;
    Ok(Value::string(s))
}

/// Build a string [`Value`] from `s` for `load_property` getters,
/// falling back to `undefined` if the GC string allocation fails.
pub fn str_or_undef(s: &str, heap: &mut otter_gc::GcHeap) -> Value {
    match JsString::from_str(s, heap) {
        Ok(js) => Value::string(js),
        Err(_) => Value::undefined(),
    }
}

pub fn require_construct(ctx: &NativeCtx<'_>, class: &'static str) -> Result<(), NativeError> {
    if ctx.is_construct_call() {
        Ok(())
    } else {
        Err(NativeError::TypeError {
            name: class,
            reason: format!("{class} constructor must be invoked with `new`"),
        })
    }
}

/// §7.1.6 `ToIntegerWithTruncation` — `ToNumber`, reject `NaN`/`±∞`
/// with `RangeError`, truncate toward zero.
pub fn to_integer_with_truncation(
    value: &Value,
    heap: &otter_gc::GcHeap,
    class: &'static str,
    field: &str,
) -> Result<f64, NativeError> {
    if value.is_symbol() {
        return Err(NativeError::TypeError {
            name: class,
            reason: format!("{field}: cannot convert a Symbol to a Number"),
        });
    }
    if value.is_big_int() {
        return Err(NativeError::TypeError {
            name: class,
            reason: format!("{field}: cannot convert a BigInt to a Number"),
        });
    }
    let n = crate::number::parse::to_number_value(value, heap);
    if n.is_nan() || n.is_infinite() {
        return Err(NativeError::RangeError {
            name: class,
            reason: format!("{field}: must be a finite integer"),
        });
    }
    Ok(n.trunc())
}

pub fn to_integer_if_integral(
    value: &Value,
    heap: &otter_gc::GcHeap,
    class: &'static str,
    field: &str,
) -> Result<f64, NativeError> {
    let n = to_integer_with_truncation(value, heap, class, field)?;
    let raw = crate::number::parse::to_number_value(value, heap);
    if (raw - n).abs() > 0.0 {
        return Err(NativeError::RangeError {
            name: class,
            reason: format!("{field}: must be an integer"),
        });
    }
    Ok(n)
}

pub fn opt_integer_with_truncation(
    args: &[Value],
    index: usize,
    heap: &otter_gc::GcHeap,
    class: &'static str,
    field: &str,
) -> Result<f64, NativeError> {
    let v = arg_or_undef(args, index);
    if v.is_undefined() {
        return Ok(0.0);
    }
    to_integer_with_truncation(&v, heap, class, field)
}

pub fn opt_integer_if_integral(
    args: &[Value],
    index: usize,
    heap: &otter_gc::GcHeap,
    class: &'static str,
    field: &str,
) -> Result<f64, NativeError> {
    let v = arg_or_undef(args, index);
    if v.is_undefined() {
        return Ok(0.0);
    }
    to_integer_if_integral(&v, heap, class, field)
}

pub fn arg_to_calendar(
    args: &[Value],
    index: usize,
    heap: &otter_gc::GcHeap,
    class: &'static str,
) -> Result<temporal_rs::Calendar, NativeError> {
    let v = arg_or_undef(args, index);
    if v.is_undefined() {
        return Ok(temporal_rs::Calendar::default());
    }
    let Some(js) = v.as_string(heap) else {
        return Err(NativeError::TypeError {
            name: class,
            reason: "calendar argument must be a string".to_string(),
        });
    };
    let s = js.to_lossy_string(heap);
    temporal_rs::Calendar::try_from_utf8(s.as_bytes()).map_err(|e| NativeError::RangeError {
        name: class,
        reason: format!("invalid calendar identifier: {e}"),
    })
}

pub fn clamp_to_u8(n: f64, class: &'static str, field: &str) -> Result<u8, NativeError> {
    if !(0.0..=255.0).contains(&n) {
        return Err(NativeError::RangeError {
            name: class,
            reason: format!("{field} out of range"),
        });
    }
    Ok(n as u8)
}

pub fn clamp_to_u16(n: f64, class: &'static str, field: &str) -> Result<u16, NativeError> {
    if !(0.0..=65_535.0).contains(&n) {
        return Err(NativeError::RangeError {
            name: class,
            reason: format!("{field} out of range"),
        });
    }
    Ok(n as u16)
}

/// Convert a [`temporal_rs::TemporalError`] into a [`NativeError`]
/// honouring the spec error class: `Range` → `RangeError`, `Type` →
/// `TypeError`, `Syntax` → `SyntaxError`. Other engine variants fall
/// through as `TypeError`.
pub fn temporal_err(err: temporal_rs::TemporalError, class: &'static str) -> NativeError {
    use temporal_rs::error::ErrorKind;
    let reason = err.to_string();
    match err.kind() {
        ErrorKind::Range => NativeError::RangeError {
            name: class,
            reason,
        },
        ErrorKind::Type => NativeError::TypeError {
            name: class,
            reason,
        },
        ErrorKind::Syntax => NativeError::SyntaxError {
            name: class,
            reason,
        },
        _ => NativeError::TypeError {
            name: class,
            reason,
        },
    }
}

// ── Receiver extractors ──────────────────────────────────────────

fn require_payload<F, T>(
    ctx: &NativeCtx<'_>,
    expected: &'static str,
    extract: F,
) -> Result<T, NativeError>
where
    F: FnOnce(TemporalPayload) -> Option<T>,
{
    let recv = *ctx.this_value();
    let t = recv
        .as_temporal(ctx.heap())
        .ok_or_else(|| NativeError::TypeError {
            name: expected,
            reason: format!("receiver must be a {expected}"),
        })?;
    extract(t.payload_clone(ctx.heap())).ok_or_else(|| NativeError::TypeError {
        name: expected,
        reason: format!("receiver must be a {expected}"),
    })
}

pub fn require_instant(ctx: &NativeCtx<'_>) -> Result<temporal_rs::Instant, NativeError> {
    require_payload(ctx, "Temporal.Instant", |p| match p {
        TemporalPayload::Instant(v) => Some(v),
        _ => None,
    })
}

pub fn require_duration(ctx: &NativeCtx<'_>) -> Result<temporal_rs::Duration, NativeError> {
    require_payload(ctx, "Temporal.Duration", |p| match p {
        TemporalPayload::Duration(v) => Some(v),
        _ => None,
    })
}

pub fn require_plain_date(ctx: &NativeCtx<'_>) -> Result<temporal_rs::PlainDate, NativeError> {
    require_payload(ctx, "Temporal.PlainDate", |p| match p {
        TemporalPayload::PlainDate(v) => Some(v),
        _ => None,
    })
}

pub fn require_plain_time(ctx: &NativeCtx<'_>) -> Result<temporal_rs::PlainTime, NativeError> {
    require_payload(ctx, "Temporal.PlainTime", |p| match p {
        TemporalPayload::PlainTime(v) => Some(v),
        _ => None,
    })
}

pub fn require_plain_date_time(
    ctx: &NativeCtx<'_>,
) -> Result<temporal_rs::PlainDateTime, NativeError> {
    require_payload(ctx, "Temporal.PlainDateTime", |p| match p {
        TemporalPayload::PlainDateTime(v) => Some(v),
        _ => None,
    })
}

pub fn require_plain_year_month(
    ctx: &NativeCtx<'_>,
) -> Result<temporal_rs::PlainYearMonth, NativeError> {
    require_payload(ctx, "Temporal.PlainYearMonth", |p| match p {
        TemporalPayload::PlainYearMonth(v) => Some(v),
        _ => None,
    })
}

pub fn require_plain_month_day(
    ctx: &NativeCtx<'_>,
) -> Result<temporal_rs::PlainMonthDay, NativeError> {
    require_payload(ctx, "Temporal.PlainMonthDay", |p| match p {
        TemporalPayload::PlainMonthDay(v) => Some(v),
        _ => None,
    })
}

pub fn require_zoned_date_time(
    ctx: &NativeCtx<'_>,
) -> Result<temporal_rs::ZonedDateTime, NativeError> {
    require_payload(ctx, "Temporal.ZonedDateTime", |p| match p {
        TemporalPayload::ZonedDateTime(v) => Some(v),
        _ => None,
    })
}

// ── Options bag parsers ──────────────────────────────────────────

fn read_string_field(obj: JsObject, name: &str, heap: &otter_gc::GcHeap) -> Option<String> {
    let v = object::get(obj, heap, name)?;
    v.as_string(heap).map(|s| s.to_lossy_string(heap))
}

/// Parse the `calendarName` option (`"auto"`/`"always"`/`"never"`/
/// `"critical"`) from a Temporal `toString` options argument into a
/// [`temporal_rs::options::DisplayCalendar`]. Absent options or an
/// absent `calendarName` default to `Auto`.
pub fn parse_display_calendar(
    args: &[Value],
    index: usize,
    heap: &otter_gc::GcHeap,
    class: &'static str,
) -> Result<temporal_rs::options::DisplayCalendar, NativeError> {
    use core::str::FromStr;
    let v = arg_or_undef(args, index);
    if v.is_undefined() {
        return Ok(temporal_rs::options::DisplayCalendar::Auto);
    }
    let Some(obj) = v.as_object() else {
        return Err(NativeError::TypeError {
            name: class,
            reason: "toString() options must be an object or undefined".to_string(),
        });
    };
    match read_string_field(obj, "calendarName", heap) {
        Some(name) => temporal_rs::options::DisplayCalendar::from_str(&name).map_err(|_| {
            NativeError::RangeError {
                name: class,
                reason: "invalid `calendarName`".to_string(),
            }
        }),
        None => Ok(temporal_rs::options::DisplayCalendar::Auto),
    }
}

fn read_partial_integer(
    obj: JsObject,
    name: &str,
    heap: &otter_gc::GcHeap,
    class: &'static str,
) -> Result<Option<i64>, NativeError> {
    let Some(v) = object::get(obj, heap, name) else {
        return Ok(None);
    };
    if v.is_undefined() {
        return Ok(None);
    }
    let Some(n) = v.as_number() else {
        return Err(NativeError::TypeError {
            name: class,
            reason: format!("{name}: partial-record field must be a number"),
        });
    };
    let raw = n.as_f64();
    if !raw.is_finite() {
        return Err(NativeError::RangeError {
            name: class,
            reason: format!("{name}: partial-record field must be finite"),
        });
    }
    if (raw - raw.trunc()).abs() > 0.0 {
        return Err(NativeError::RangeError {
            name: class,
            reason: format!("{name}: partial-record field must be an integer"),
        });
    }
    Ok(Some(raw.trunc() as i64))
}

pub fn parse_difference_settings(
    args: &[Value],
    index: usize,
    heap: &otter_gc::GcHeap,
    class: &'static str,
) -> Result<temporal_rs::options::DifferenceSettings, NativeError> {
    use core::str::FromStr;
    let mut settings = temporal_rs::options::DifferenceSettings::default();
    let v = arg_or_undef(args, index);
    if v.is_undefined() {
        return Ok(settings);
    }
    let Some(obj) = v.as_object() else {
        return Err(NativeError::TypeError {
            name: class,
            reason: "options must be an object".to_string(),
        });
    };
    if let Some(name) = read_string_field(obj, "largestUnit", heap)
        && !name.is_empty()
        && !name.eq_ignore_ascii_case("auto")
    {
        let unit =
            temporal_rs::options::Unit::from_str(&name).map_err(|_| NativeError::RangeError {
                name: class,
                reason: "invalid `largestUnit`".to_string(),
            })?;
        settings.largest_unit = Some(unit);
    }
    if let Some(name) = read_string_field(obj, "smallestUnit", heap) {
        let unit =
            temporal_rs::options::Unit::from_str(&name).map_err(|_| NativeError::RangeError {
                name: class,
                reason: "invalid `smallestUnit`".to_string(),
            })?;
        settings.smallest_unit = Some(unit);
    }
    if let Some(name) = read_string_field(obj, "roundingMode", heap) {
        let mode = temporal_rs::options::RoundingMode::from_str(&name).map_err(|_| {
            NativeError::RangeError {
                name: class,
                reason: "invalid `roundingMode`".to_string(),
            }
        })?;
        settings.rounding_mode = Some(mode);
    }
    if let Some(n) = object::get(obj, heap, "roundingIncrement")
        && !n.is_undefined()
        && let Some(num) = n.as_number()
    {
        let raw = num.as_f64();
        if raw.is_finite() && raw >= 1.0 {
            if let Ok(incr) = temporal_rs::options::RoundingIncrement::try_from(raw.trunc()) {
                settings.increment = Some(incr);
            } else {
                return Err(NativeError::RangeError {
                    name: class,
                    reason: "invalid `roundingIncrement`".to_string(),
                });
            }
        }
    }
    Ok(settings)
}

pub fn parse_rounding_options(
    args: &[Value],
    index: usize,
    heap: &otter_gc::GcHeap,
    class: &'static str,
) -> Result<temporal_rs::options::RoundingOptions, NativeError> {
    use core::str::FromStr;
    let mut options = temporal_rs::options::RoundingOptions::default();
    let v = arg_or_undef(args, index);
    if let Some(s) = v.as_string(heap) {
        let name = s.to_lossy_string(heap);
        let unit =
            temporal_rs::options::Unit::from_str(&name).map_err(|_| NativeError::RangeError {
                name: class,
                reason: "invalid smallest-unit shorthand".to_string(),
            })?;
        options.smallest_unit = Some(unit);
        return Ok(options);
    }
    if v.is_undefined() {
        return Ok(options);
    }
    let Some(obj) = v.as_object() else {
        return Err(NativeError::TypeError {
            name: class,
            reason: "round() requires an options object or smallest-unit string".to_string(),
        });
    };
    if let Some(name) = read_string_field(obj, "largestUnit", heap) {
        let unit =
            temporal_rs::options::Unit::from_str(&name).map_err(|_| NativeError::RangeError {
                name: class,
                reason: "invalid `largestUnit`".to_string(),
            })?;
        options.largest_unit = Some(unit);
    }
    if let Some(name) = read_string_field(obj, "smallestUnit", heap) {
        let unit =
            temporal_rs::options::Unit::from_str(&name).map_err(|_| NativeError::RangeError {
                name: class,
                reason: "invalid `smallestUnit`".to_string(),
            })?;
        options.smallest_unit = Some(unit);
    }
    if let Some(name) = read_string_field(obj, "roundingMode", heap) {
        let mode = temporal_rs::options::RoundingMode::from_str(&name).map_err(|_| {
            NativeError::RangeError {
                name: class,
                reason: "invalid `roundingMode`".to_string(),
            }
        })?;
        options.rounding_mode = Some(mode);
    }
    if let Some(n) = object::get(obj, heap, "roundingIncrement")
        && !n.is_undefined()
        && let Some(num) = n.as_number()
    {
        let raw = num.as_f64();
        if raw.is_finite() && raw >= 1.0 {
            if let Ok(incr) = temporal_rs::options::RoundingIncrement::try_from(raw.trunc()) {
                options.increment = Some(incr);
            } else {
                return Err(NativeError::RangeError {
                    name: class,
                    reason: "invalid `roundingIncrement`".to_string(),
                });
            }
        }
    }
    Ok(options)
}

pub fn parse_partial_time(
    obj: JsObject,
    heap: &otter_gc::GcHeap,
    class: &'static str,
) -> Result<temporal_rs::partial::PartialTime, NativeError> {
    let mut t = temporal_rs::partial::PartialTime::default();
    if let Some(v) = read_partial_integer(obj, "hour", heap, class)? {
        t.hour = Some(v.clamp(0, u8::MAX as i64) as u8);
    }
    if let Some(v) = read_partial_integer(obj, "minute", heap, class)? {
        t.minute = Some(v.clamp(0, u8::MAX as i64) as u8);
    }
    if let Some(v) = read_partial_integer(obj, "second", heap, class)? {
        t.second = Some(v.clamp(0, u8::MAX as i64) as u8);
    }
    if let Some(v) = read_partial_integer(obj, "millisecond", heap, class)? {
        t.millisecond = Some(v.clamp(0, u16::MAX as i64) as u16);
    }
    if let Some(v) = read_partial_integer(obj, "microsecond", heap, class)? {
        t.microsecond = Some(v.clamp(0, u16::MAX as i64) as u16);
    }
    if let Some(v) = read_partial_integer(obj, "nanosecond", heap, class)? {
        t.nanosecond = Some(v.clamp(0, u16::MAX as i64) as u16);
    }
    Ok(t)
}

pub fn parse_calendar_fields(
    obj: JsObject,
    heap: &otter_gc::GcHeap,
    class: &'static str,
) -> Result<temporal_rs::fields::CalendarFields, NativeError> {
    let mut f = temporal_rs::fields::CalendarFields::default();
    if let Some(v) = read_partial_integer(obj, "year", heap, class)? {
        f.year = Some(v.clamp(i32::MIN as i64, i32::MAX as i64) as i32);
    }
    if let Some(v) = read_partial_integer(obj, "month", heap, class)? {
        f.month = Some(v.clamp(0, u8::MAX as i64) as u8);
    }
    if let Some(v) = read_partial_integer(obj, "day", heap, class)? {
        f.day = Some(v.clamp(0, u8::MAX as i64) as u8);
    }
    Ok(f)
}

pub fn parse_date_time_fields(
    obj: JsObject,
    heap: &otter_gc::GcHeap,
    class: &'static str,
) -> Result<temporal_rs::fields::DateTimeFields, NativeError> {
    Ok(temporal_rs::fields::DateTimeFields {
        calendar_fields: parse_calendar_fields(obj, heap, class)?,
        time: parse_partial_time(obj, heap, class)?,
    })
}

pub fn parse_year_month_fields(
    obj: JsObject,
    heap: &otter_gc::GcHeap,
    class: &'static str,
) -> Result<temporal_rs::fields::YearMonthCalendarFields, NativeError> {
    let mut f = temporal_rs::fields::YearMonthCalendarFields::default();
    if let Some(v) = read_partial_integer(obj, "year", heap, class)? {
        f.year = Some(v.clamp(i32::MIN as i64, i32::MAX as i64) as i32);
    }
    if let Some(v) = read_partial_integer(obj, "month", heap, class)? {
        f.month = Some(v.clamp(0, u8::MAX as i64) as u8);
    }
    if let Some(s) = read_string_field(obj, "monthCode", heap) {
        let code = temporal_rs::MonthCode::try_from_utf8(s.as_bytes()).map_err(|_| {
            NativeError::TypeError {
                name: class,
                reason: "invalid monthCode".to_string(),
            }
        })?;
        f.month_code = Some(code);
    }
    Ok(f)
}
