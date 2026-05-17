//! `Date.prototype.<name>` intrinsic table.
//!
//! Date instances are ordinary objects with a `[[DateValue]]`
//! internal slot per §21.4.5. The receiver helpers in this module
//! validate that brand by checking
//! [`crate::object::date_data`]; ordinary objects without the
//! slot trigger `TypeError`.
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
//! - [`DATE_PROTOTYPE_METHODS`] — JS-visible method specs installed
//!   on `Date.prototype` during bootstrap.
//!
//! # See also
//! - <https://tc39.es/ecma262/#sec-properties-of-the-date-prototype-object>
//! - <https://tc39.es/ecma262/#sec-thistimevalue>

use super::{broken_down, to_iso_string};
use crate::Value;
use crate::intrinsics::{IntrinsicArgs, IntrinsicError, IntrinsicReceiver, IntrinsicTable};
use crate::js_surface::{Attr, MethodSpec};
use crate::native_function::NativeCall;
use crate::number::NumberValue;
use crate::object::{self, JsObject};
use crate::string::JsString;
use crate::{NativeCtx, NativeError};

/// `thisTimeValue` (§21.4.1.1): validate the receiver brand and
/// extract the `[[DateValue]]` slot. Returns the JsObject handle
/// (for setters that need to write back) and the current time.
fn receiver_handle(args: &IntrinsicArgs<'_>) -> Result<(JsObject, f64), IntrinsicError> {
    if let Value::Object(o) = args.receiver
        && let Some(time) = object::date_data(*o, args.gc_heap)
    {
        return Ok((*o, time));
    }
    Err(IntrinsicError::BadReceiver { expected: "date" })
}

/// Read-only `thisTimeValue` — getters only need the time.
fn receiver_time(args: &IntrinsicArgs<'_>) -> Result<f64, IntrinsicError> {
    receiver_handle(args).map(|(_, t)| t)
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
    Ok(Value::Number(NumberValue::from_f64(receiver_time(args)?)))
}

fn impl_get_full_year(args: &mut IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    Ok(broken_down(receiver_time(args)?)
        .map(|bd| smi(bd.year))
        .unwrap_or_else(nan))
}

fn impl_get_month(args: &mut IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    Ok(broken_down(receiver_time(args)?)
        .map(|bd| smi(bd.month as i32))
        .unwrap_or_else(nan))
}

fn impl_get_date(args: &mut IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    Ok(broken_down(receiver_time(args)?)
        .map(|bd| smi(bd.day as i32))
        .unwrap_or_else(nan))
}

fn impl_get_day(args: &mut IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    Ok(broken_down(receiver_time(args)?)
        .map(|bd| smi(bd.weekday as i32))
        .unwrap_or_else(nan))
}

fn impl_get_hours(args: &mut IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    Ok(broken_down(receiver_time(args)?)
        .map(|bd| smi(bd.hour as i32))
        .unwrap_or_else(nan))
}

fn impl_get_minutes(args: &mut IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    Ok(broken_down(receiver_time(args)?)
        .map(|bd| smi(bd.minute as i32))
        .unwrap_or_else(nan))
}

fn impl_get_seconds(args: &mut IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    Ok(broken_down(receiver_time(args)?)
        .map(|bd| smi(bd.second as i32))
        .unwrap_or_else(nan))
}

fn impl_get_milliseconds(args: &mut IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    Ok(broken_down(receiver_time(args)?)
        .map(|bd| smi(bd.millisecond as i32))
        .unwrap_or_else(nan))
}

/// §21.4.4.21 — `getTimezoneOffset()`. Foundation treats local time
/// as UTC, so the offset is always `0`.
fn impl_get_timezone_offset(args: &mut IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    // Validate brand even though the result is constant — `Date.prototype.getTimezoneOffset.call({})`
    // must throw, not return 0.
    receiver_time(args)?;
    Ok(smi(0))
}

/// §21.4.4.36 — `toISOString()`. Throws RangeError on Invalid Date
/// per spec; the foundation surfaces that via `BadArgument`.
fn impl_to_iso_string(args: &mut IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    let time = receiver_time(args)?;
    let s = to_iso_string(time).ok_or(IntrinsicError::OutOfRange {
        index: 0,
        reason: "Invalid Date",
    })?;
    Ok(Value::String(JsString::from_str(&s, args.string_heap)?))
}

/// §21.4.4.41 — `toJSON()`. Returns `toISOString()` for finite
/// dates and `null` for Invalid Date.
fn impl_to_json(args: &mut IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    let time = receiver_time(args)?;
    match to_iso_string(time) {
        Some(s) => Ok(Value::String(JsString::from_str(&s, args.string_heap)?)),
        None => Ok(Value::Null),
    }
}

/// §21.4.4.42 — `toString()`. Foundation returns the ISO string
/// (matching `toISOString` shape; spec uses a locale-friendly
/// rendering that requires host integration).
fn impl_to_string(args: &mut IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    let time = receiver_time(args)?;
    let s = to_iso_string(time).unwrap_or_else(|| "Invalid Date".to_string());
    Ok(Value::String(JsString::from_str(&s, args.string_heap)?))
}

/// §21.4.4.27 / §21.4.4.43 / §21.4.4.40 — `toDateString` /
/// `toTimeString` / `toLocaleString` / `toLocaleDateString` /
/// `toLocaleTimeString`. Foundation form returns the ISO string
/// for backward compatibility with `toString`. Locale-aware
/// rendering ships once Intl lands.
fn impl_to_date_string(args: &mut IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    let time = receiver_time(args)?;
    let s = match broken_down(time) {
        Some(bd) => format!("{:04}-{:02}-{:02}", bd.year, bd.month + 1, bd.day),
        None => "Invalid Date".to_string(),
    };
    Ok(Value::String(JsString::from_str(&s, args.string_heap)?))
}

fn impl_to_time_string(args: &mut IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    let time = receiver_time(args)?;
    let s = match broken_down(time) {
        Some(bd) => format!(
            "{:02}:{:02}:{:02}.{:03}Z",
            bd.hour, bd.minute, bd.second, bd.millisecond
        ),
        None => "Invalid Date".to_string(),
    };
    Ok(Value::String(JsString::from_str(&s, args.string_heap)?))
}

/// Helper for `setX`-family methods. Reads each `args.args[idx]` as
/// a `f64` via `ToNumber`; missing args fall back to the value
/// supplied by `fallback`. Returns `NaN` if any provided arg is
/// non-finite or `undefined`.
fn read_arg_number(args: &IntrinsicArgs<'_>, idx: usize, fallback: f64) -> f64 {
    match args.args.get(idx) {
        None => fallback,
        Some(Value::Number(n)) => n.as_f64(),
        Some(Value::Boolean(true)) => 1.0,
        Some(Value::Boolean(false)) | Some(Value::Null) => 0.0,
        Some(Value::Undefined) => f64::NAN,
        _ => f64::NAN,
    }
}

/// §21.4.4.27 — `setTime(ms)`. Direct write, returns the time value.
fn impl_set_time(args: &mut IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    let (obj, _) = receiver_handle(args)?;
    let ms = read_arg_number(args, 0, f64::NAN);
    object::set_date_data(obj, args.gc_heap, ms);
    let written = object::date_data(obj, args.gc_heap).unwrap_or(f64::NAN);
    Ok(Value::Number(NumberValue::from_f64(written)))
}

/// Broken-down components packaged as a 7-tuple for the setter
/// closure pattern. Avoids needing to capture mutable component
/// references across the `args` borrow.
type Components = (f64, f64, f64, f64, f64, f64, f64);

fn current_components(time: f64) -> Components {
    match broken_down(time) {
        Some(b) => (
            b.year as f64,
            b.month as f64,
            b.day as f64,
            b.hour as f64,
            b.minute as f64,
            b.second as f64,
            b.millisecond as f64,
        ),
        None => (f64::NAN, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0),
    }
}

fn finish_set(
    obj: JsObject,
    args: &mut IntrinsicArgs<'_>,
    c: Components,
) -> Result<Value, IntrinsicError> {
    let (year, month, day, hour, minute, second, ms) = c;
    let new_ms = super::make_date(year, month, day, hour, minute, second, ms);
    object::set_date_data(obj, args.gc_heap, new_ms);
    let written = object::date_data(obj, args.gc_heap).unwrap_or(f64::NAN);
    Ok(Value::Number(NumberValue::from_f64(written)))
}

fn impl_set_full_year(args: &mut IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    let (obj, time) = receiver_handle(args)?;
    let mut c = current_components(time);
    c.0 = read_arg_number(args, 0, c.0);
    if args.args.len() >= 2 {
        c.1 = read_arg_number(args, 1, c.1);
    }
    if args.args.len() >= 3 {
        c.2 = read_arg_number(args, 2, c.2);
    }
    finish_set(obj, args, c)
}

fn impl_set_month(args: &mut IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    let (obj, time) = receiver_handle(args)?;
    // §21.4.4.21 — `setMonth(month [, date])`. Invalid Date passes
    // through (NaN year → NaN result).
    if broken_down(time).is_none() {
        return Ok(Value::Number(NumberValue::from_f64(f64::NAN)));
    }
    let mut c = current_components(time);
    c.1 = read_arg_number(args, 0, c.1);
    if args.args.len() >= 2 {
        c.2 = read_arg_number(args, 1, c.2);
    }
    finish_set(obj, args, c)
}

fn impl_set_date(args: &mut IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    let (obj, time) = receiver_handle(args)?;
    if broken_down(time).is_none() {
        return Ok(Value::Number(NumberValue::from_f64(f64::NAN)));
    }
    let mut c = current_components(time);
    c.2 = read_arg_number(args, 0, c.2);
    finish_set(obj, args, c)
}

fn impl_set_hours(args: &mut IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    let (obj, time) = receiver_handle(args)?;
    if broken_down(time).is_none() {
        return Ok(Value::Number(NumberValue::from_f64(f64::NAN)));
    }
    let mut c = current_components(time);
    c.3 = read_arg_number(args, 0, c.3);
    if args.args.len() >= 2 {
        c.4 = read_arg_number(args, 1, c.4);
    }
    if args.args.len() >= 3 {
        c.5 = read_arg_number(args, 2, c.5);
    }
    if args.args.len() >= 4 {
        c.6 = read_arg_number(args, 3, c.6);
    }
    finish_set(obj, args, c)
}

fn impl_set_minutes(args: &mut IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    let (obj, time) = receiver_handle(args)?;
    if broken_down(time).is_none() {
        return Ok(Value::Number(NumberValue::from_f64(f64::NAN)));
    }
    let mut c = current_components(time);
    c.4 = read_arg_number(args, 0, c.4);
    if args.args.len() >= 2 {
        c.5 = read_arg_number(args, 1, c.5);
    }
    if args.args.len() >= 3 {
        c.6 = read_arg_number(args, 2, c.6);
    }
    finish_set(obj, args, c)
}

fn impl_set_seconds(args: &mut IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    let (obj, time) = receiver_handle(args)?;
    if broken_down(time).is_none() {
        return Ok(Value::Number(NumberValue::from_f64(f64::NAN)));
    }
    let mut c = current_components(time);
    c.5 = read_arg_number(args, 0, c.5);
    if args.args.len() >= 2 {
        c.6 = read_arg_number(args, 1, c.6);
    }
    finish_set(obj, args, c)
}

fn impl_set_milliseconds(args: &mut IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    let (obj, time) = receiver_handle(args)?;
    if broken_down(time).is_none() {
        return Ok(Value::Number(NumberValue::from_f64(f64::NAN)));
    }
    let mut c = current_components(time);
    c.6 = read_arg_number(args, 0, c.6);
    finish_set(obj, args, c)
}

/// §B.2.4.1 — `Date.prototype.getYear()`. Returns
/// `YearFromTime(LocalTime(t)) - 1900`, or `NaN` if the receiver's
/// `[[DateValue]]` is `NaN`. The foundation treats LocalTime as
/// UTC, mirroring the rest of the `getUTC*` / `get*` impls.
///
/// # See also
/// - <https://tc39.es/ecma262/#sec-date.prototype.getyear>
fn impl_get_year(args: &mut IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    let time = receiver_time(args)?;
    Ok(broken_down(time)
        .map(|bd| smi(bd.year - 1900))
        .unwrap_or_else(nan))
}

/// §B.2.4.2 — `Date.prototype.setYear(year)`.
///
/// 1. Let `t` be `thisTimeValue(this)`.
/// 2. If `t` is `NaN`, set `t` to `+0`; otherwise `t = LocalTime(t)`.
/// 3. Let `y` be `ToNumber(year)`.
/// 4. If `y` is `NaN`, `[[DateValue]] = NaN`, return `NaN`.
/// 5. If `y` is finite and `0 ≤ ToInteger(y) ≤ 99`,
///    `yyyy = ToInteger(y) + 1900`.
/// 6. Else `yyyy = y`.
/// 7. `d = MakeDay(yyyy, MonthFromTime(t), DateFromTime(t))`.
/// 8. `date = UTC(MakeDate(d, TimeWithinDay(t)))`.
/// 9. `[[DateValue]] = TimeClip(date)`; return `TimeClip(date)`.
///
/// # See also
/// - <https://tc39.es/ecma262/#sec-date.prototype.setyear>
fn impl_set_year(args: &mut IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    let (obj, time) = receiver_handle(args)?;
    let y = read_arg_number(args, 0, f64::NAN);
    if y.is_nan() {
        object::set_date_data(obj, args.gc_heap, f64::NAN);
        return Ok(Value::Number(NumberValue::from_f64(f64::NAN)));
    }
    // §B.2.4.2 step 2: t = NaN → +0; else LocalTime(t) (== t under UTC).
    let base_time = if time.is_nan() { 0.0 } else { time };
    // §B.2.4.2 step 5–6.
    let y_int = y.trunc();
    let yyyy = if (0.0..=99.0).contains(&y_int) {
        y_int + 1900.0
    } else {
        y
    };
    let mut c = current_components(base_time);
    c.0 = yyyy;
    finish_set(obj, args, c)
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
            "toDateString"        / 0 => impl_to_date_string,
            "toTimeString"        / 0 => impl_to_time_string,
            "toLocaleString"      / 0 => impl_to_string,
            "toLocaleDateString"  / 0 => impl_to_date_string,
            "toLocaleTimeString"  / 0 => impl_to_time_string,
            "setTime"             / 1 => impl_set_time,
            "setFullYear"         / 3 => impl_set_full_year,
            "setUTCFullYear"      / 3 => impl_set_full_year,
            "setMonth"            / 2 => impl_set_month,
            "setUTCMonth"         / 2 => impl_set_month,
            "setDate"             / 1 => impl_set_date,
            "setUTCDate"          / 1 => impl_set_date,
            "setHours"            / 4 => impl_set_hours,
            "setUTCHours"         / 4 => impl_set_hours,
            "setMinutes"          / 3 => impl_set_minutes,
            "setUTCMinutes"       / 3 => impl_set_minutes,
            "setSeconds"          / 2 => impl_set_seconds,
            "setUTCSeconds"       / 2 => impl_set_seconds,
            "setMilliseconds"     / 1 => impl_set_milliseconds,
            "setUTCMilliseconds"  / 1 => impl_set_milliseconds,
            "getYear"             / 0 => impl_get_year,
            "setYear"             / 1 => impl_set_year,
        )
    });

/// Convenience accessor used by the dispatcher.
#[must_use]
pub fn lookup(name: &str) -> Option<&'static crate::intrinsics::IntrinsicEntry> {
    DATE_PROTOTYPE_TABLE.lookup(IntrinsicReceiver::Date, name)
}

/// Generic bridge that exposes a `Date.prototype.<name>` intrinsic
/// as a JS-visible NativeFunction so user code reading the property
/// directly (`const f = d.getTime; f.call(d)`) resolves to a real
/// callable. The compiler's `CallDate` fast path keeps using the
/// table directly.
fn native_date_method(
    name: &'static str,
    ctx: &mut NativeCtx<'_>,
    args: &[Value],
) -> Result<Value, NativeError> {
    let receiver = ctx.this_value().clone();
    let (string_heap, allocation_roots) = {
        let interp = ctx.interp_mut();
        (interp.string_heap_clone(), interp.collect_runtime_roots())
    };
    let entry = lookup(name).ok_or_else(|| NativeError::TypeError {
        name,
        reason: "unknown Date.prototype method".to_string(),
    })?;
    (entry.impl_fn)(&mut IntrinsicArgs {
        receiver: &receiver,
        args,
        string_heap: &string_heap,
        gc_heap: ctx.heap_mut(),
        allocation_roots: allocation_roots.as_slice(),
    })
    .map_err(|err| match err {
        IntrinsicError::OutOfRange { .. } => NativeError::RangeError {
            name,
            reason: err.to_string(),
        },
        _ => NativeError::TypeError {
            name,
            reason: err.to_string(),
        },
    })
}

/// Per-method trampoline + spec-table entry generator. Same shape as
/// the `string_prototype_methods!` macro in `crate::string::prototype`.
macro_rules! date_prototype_methods {
    ($($bridge:ident => $name:literal, $length:literal;)*) => {
        $(
            fn $bridge(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
                native_date_method($name, ctx, args)
            }
        )*

        /// Declarative `Date.prototype` method specs installed as
        /// JS-visible own properties during the `Date` bootstrap.
        pub static DATE_PROTOTYPE_METHODS: &[MethodSpec] = &[
            $(MethodSpec {
                name: $name,
                length: $length,
                attrs: Attr::builtin_function(),
                call: NativeCall::Static($bridge),
            },)*
        ];
    };
}

date_prototype_methods!(
    bridge_get_time              => "getTime",             0;
    bridge_value_of              => "valueOf",             0;
    bridge_get_full_year         => "getFullYear",         0;
    bridge_get_utc_full_year     => "getUTCFullYear",      0;
    bridge_get_month             => "getMonth",            0;
    bridge_get_utc_month         => "getUTCMonth",         0;
    bridge_get_date              => "getDate",             0;
    bridge_get_utc_date          => "getUTCDate",          0;
    bridge_get_day               => "getDay",              0;
    bridge_get_utc_day           => "getUTCDay",           0;
    bridge_get_hours             => "getHours",            0;
    bridge_get_utc_hours         => "getUTCHours",         0;
    bridge_get_minutes           => "getMinutes",          0;
    bridge_get_utc_minutes       => "getUTCMinutes",       0;
    bridge_get_seconds           => "getSeconds",          0;
    bridge_get_utc_seconds       => "getUTCSeconds",       0;
    bridge_get_milliseconds      => "getMilliseconds",     0;
    bridge_get_utc_milliseconds  => "getUTCMilliseconds",  0;
    bridge_get_timezone_offset   => "getTimezoneOffset",   0;
    bridge_to_iso_string         => "toISOString",         0;
    bridge_to_json               => "toJSON",              0;
    bridge_to_string             => "toString",            0;
    bridge_to_utc_string         => "toUTCString",         0;
    bridge_to_date_string        => "toDateString",        0;
    bridge_to_time_string        => "toTimeString",        0;
    bridge_to_locale_string      => "toLocaleString",      0;
    bridge_to_locale_date_string => "toLocaleDateString",  0;
    bridge_to_locale_time_string => "toLocaleTimeString",  0;
    bridge_set_time              => "setTime",             1;
    bridge_set_full_year         => "setFullYear",         3;
    bridge_set_utc_full_year     => "setUTCFullYear",      3;
    bridge_set_month             => "setMonth",            2;
    bridge_set_utc_month         => "setUTCMonth",         2;
    bridge_set_date              => "setDate",             1;
    bridge_set_utc_date          => "setUTCDate",          1;
    bridge_set_hours             => "setHours",            4;
    bridge_set_utc_hours         => "setUTCHours",         4;
    bridge_set_minutes           => "setMinutes",          3;
    bridge_set_utc_minutes       => "setUTCMinutes",       3;
    bridge_set_seconds           => "setSeconds",          2;
    bridge_set_utc_seconds       => "setUTCSeconds",       2;
    bridge_set_milliseconds      => "setMilliseconds",     1;
    bridge_set_utc_milliseconds  => "setUTCMilliseconds",  1;
    bridge_get_year              => "getYear",             0;
    bridge_set_year              => "setYear",             1;
);
