//! `Date.prototype.<name>` intrinsic table.
//!
//! Foundation surface (UTC-only — host timezone integration is
//! filed). Every accessor returns the broken-down UTC component
//! per ECMA-262 §21.4.4.x; the `getUTC*` and "local" `get*` shapes
//! intentionally share one implementation since the foundation
//! treats local time as UTC.
//!
//! # Contents
//! - [`DATE_PROTOTYPE_TABLE`] — declarative table built with the
//!   [`crate::intrinsics!`] macro.
//! - [`lookup`] — convenience accessor used by the dispatcher.
//!
//! # See also
//! - <https://tc39.es/ecma262/#sec-properties-of-the-date-prototype-object>

use super::{JsDate, broken_down, to_iso_string};
use crate::Value;
use crate::intrinsics::{IntrinsicArgs, IntrinsicError, IntrinsicReceiver, IntrinsicTable};
use crate::number::NumberValue;
use crate::string::JsString;

fn receiver(args: &IntrinsicArgs<'_>) -> Result<JsDate, IntrinsicError> {
    match args.receiver {
        Value::Date(d) => Ok(d.clone()),
        _ => Err(IntrinsicError::BadReceiver { expected: "date" }),
    }
}

fn nan() -> Value {
    Value::Number(NumberValue::from_f64(f64::NAN))
}

fn smi(n: i32) -> Value {
    Value::Number(NumberValue::from_i32(n))
}

/// §21.4.4.10 / §21.4.4.44 — `getTime()` / `valueOf()` return the
/// raw time value.
fn impl_get_time(args: &mut IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    Ok(Value::Number(NumberValue::from_f64(receiver(args)?.time())))
}

fn impl_get_full_year(args: &mut IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    Ok(broken_down(receiver(args)?.time())
        .map(|bd| smi(bd.year))
        .unwrap_or_else(nan))
}

fn impl_get_month(args: &mut IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    Ok(broken_down(receiver(args)?.time())
        .map(|bd| smi(bd.month as i32))
        .unwrap_or_else(nan))
}

fn impl_get_date(args: &mut IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    Ok(broken_down(receiver(args)?.time())
        .map(|bd| smi(bd.day as i32))
        .unwrap_or_else(nan))
}

fn impl_get_day(args: &mut IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    Ok(broken_down(receiver(args)?.time())
        .map(|bd| smi(bd.weekday as i32))
        .unwrap_or_else(nan))
}

fn impl_get_hours(args: &mut IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    Ok(broken_down(receiver(args)?.time())
        .map(|bd| smi(bd.hour as i32))
        .unwrap_or_else(nan))
}

fn impl_get_minutes(args: &mut IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    Ok(broken_down(receiver(args)?.time())
        .map(|bd| smi(bd.minute as i32))
        .unwrap_or_else(nan))
}

fn impl_get_seconds(args: &mut IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    Ok(broken_down(receiver(args)?.time())
        .map(|bd| smi(bd.second as i32))
        .unwrap_or_else(nan))
}

fn impl_get_milliseconds(args: &mut IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    Ok(broken_down(receiver(args)?.time())
        .map(|bd| smi(bd.millisecond as i32))
        .unwrap_or_else(nan))
}

/// §21.4.4.21 — `getTimezoneOffset()`. Foundation treats local time
/// as UTC, so the offset is always `0`.
fn impl_get_timezone_offset(_args: &mut IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    Ok(smi(0))
}

/// §21.4.4.36 — `toISOString()`. Throws RangeError on Invalid Date
/// per spec; the foundation surfaces that via `BadArgument`.
fn impl_to_iso_string(args: &mut IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    let date = receiver(args)?;
    let s = to_iso_string(date.time()).ok_or(IntrinsicError::BadArgument {
        index: 0,
        reason: "Invalid Date",
    })?;
    Ok(Value::String(JsString::from_str(&s, args.string_heap)?))
}

/// §21.4.4.41 — `toJSON()`. Returns `toISOString()` for finite
/// dates and `null` for Invalid Date.
fn impl_to_json(args: &mut IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    let date = receiver(args)?;
    match to_iso_string(date.time()) {
        Some(s) => Ok(Value::String(JsString::from_str(&s, args.string_heap)?)),
        None => Ok(Value::Null),
    }
}

/// §21.4.4.42 — `toString()`. Foundation returns the ISO string
/// (matching `toISOString` shape; spec uses a locale-friendly
/// rendering that requires host integration).
fn impl_to_string(args: &mut IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    let date = receiver(args)?;
    let s = to_iso_string(date.time()).unwrap_or_else(|| "Invalid Date".to_string());
    Ok(Value::String(JsString::from_str(&s, args.string_heap)?))
}

/// Declarative `Date.prototype` table. Local-time getters share
/// the UTC implementations.
pub static DATE_PROTOTYPE_TABLE: std::sync::LazyLock<IntrinsicTable> =
    std::sync::LazyLock::new(|| {
        crate::intrinsics!(
            Date,
            "getTime"             / 0 => impl_get_time,
            "valueOf"             / 0 => impl_get_time,
            "getFullYear"         / 0 => impl_get_full_year,
            "getUTCFullYear"      / 0 => impl_get_full_year,
            "getMonth"            / 0 => impl_get_month,
            "getUTCMonth"         / 0 => impl_get_month,
            "getDate"             / 0 => impl_get_date,
            "getUTCDate"          / 0 => impl_get_date,
            "getDay"              / 0 => impl_get_day,
            "getUTCDay"           / 0 => impl_get_day,
            "getHours"            / 0 => impl_get_hours,
            "getUTCHours"         / 0 => impl_get_hours,
            "getMinutes"          / 0 => impl_get_minutes,
            "getUTCMinutes"       / 0 => impl_get_minutes,
            "getSeconds"          / 0 => impl_get_seconds,
            "getUTCSeconds"       / 0 => impl_get_seconds,
            "getMilliseconds"     / 0 => impl_get_milliseconds,
            "getUTCMilliseconds"  / 0 => impl_get_milliseconds,
            "getTimezoneOffset"   / 0 => impl_get_timezone_offset,
            "toISOString"         / 0 => impl_to_iso_string,
            "toJSON"              / 0 => impl_to_json,
            "toString"            / 0 => impl_to_string,
            "toUTCString"         / 0 => impl_to_string,
        )
    });

/// Convenience accessor used by the dispatcher.
#[must_use]
pub fn lookup(name: &str) -> Option<&'static crate::intrinsics::IntrinsicEntry> {
    DATE_PROTOTYPE_TABLE.lookup(IntrinsicReceiver::Date, name)
}
