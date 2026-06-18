//! `Date.prototype.<name>` native implementations.
//!
//! Date instances are ordinary objects with a `[[DateValue]]`
//! internal slot per §21.4.5. The receiver helpers in this module
//! validate that brand by checking [`crate::object::date_data`];
//! ordinary objects without the slot trigger `TypeError`.
//!
//! UTC methods use direct epoch arithmetic; local methods lower
//! through the engine's Temporal-backed host time-zone provider.
//!
//! # Contents
//! - [`DATE_PROTOTYPE_METHODS`] — JS-visible method specs installed on
//!   `Date.prototype` during bootstrap. Each spec points at a thin
//!   `bridge_*` native that binds the JS method name to one of the
//!   shared `*_impl` bodies.
//! - [`DATE_PROTOTYPE_EXTRA_METHODS`] — specs that re-enter the full
//!   interpreter (currently `toJSON`, which runs a generic
//!   `Invoke(O, "toISOString")` per §21.4.4.41 step 4).
//!
//! # See also
//! - <https://tc39.es/ecma262/#sec-properties-of-the-date-prototype-object>
//! - <https://tc39.es/ecma262/#sec-thistimevalue>

use smallvec::SmallVec;

use super::{
    MONTH_ABBREVIATIONS, WEEKDAY_ABBREVIATIONS, broken_down, local_broken_down,
    local_timezone_string, local_utc_offset_minutes, to_iso_string,
};
use crate::Value;
use crate::abstract_ops::{self, ToPrimitiveHint};
use crate::js_surface::{Attr, MethodSpec};
use crate::native_function::NativeCall;
use crate::object::{self, JsObject};
use crate::string::JsString;
use crate::{NativeCtx, NativeError, VmError, VmGetOutcome, VmPropertyKey};

/// `thisTimeValue` (§21.4.1.1): validate the receiver brand and
/// extract the `[[DateValue]]` slot. Returns the JsObject handle (for
/// setters that need to write back) and the current time.
fn this_handle(ctx: &NativeCtx<'_>, name: &'static str) -> Result<(JsObject, f64), NativeError> {
    if let Some(o) = ctx.this_value().as_object()
        && let Some(time) = object::date_data(o, ctx.heap())
    {
        return Ok((o, time));
    }
    Err(NativeError::TypeError {
        name,
        reason: "Date.prototype method called on incompatible receiver".to_string(),
    })
}

/// Read-only `thisTimeValue` — getters only need the time.
fn this_time(ctx: &NativeCtx<'_>, name: &'static str) -> Result<f64, NativeError> {
    this_handle(ctx, name).map(|(_, t)| t)
}

fn nan() -> Value {
    Value::number_f64(f64::NAN)
}

fn smi(n: i32) -> Value {
    Value::number_i32(n)
}

fn broken_down_for_method(time: f64, name: &str) -> Option<super::BrokenDown> {
    if name.contains("UTC") {
        broken_down(time)
    } else {
        local_broken_down(time)
    }
}

/// §21.4.4.10 / §21.4.4.44 — `getTime()` / `valueOf()`.
fn get_time_impl(
    ctx: &mut NativeCtx<'_>,
    _args: &[Value],
    name: &'static str,
) -> Result<Value, NativeError> {
    Ok(Value::number_f64(this_time(ctx, name)?))
}

fn get_full_year_impl(
    ctx: &mut NativeCtx<'_>,
    _args: &[Value],
    name: &'static str,
) -> Result<Value, NativeError> {
    let time = this_time(ctx, name)?;
    Ok(broken_down_for_method(time, name)
        .map(|bd| smi(bd.year))
        .unwrap_or_else(nan))
}

fn get_month_impl(
    ctx: &mut NativeCtx<'_>,
    _args: &[Value],
    name: &'static str,
) -> Result<Value, NativeError> {
    let time = this_time(ctx, name)?;
    Ok(broken_down_for_method(time, name)
        .map(|bd| smi(bd.month as i32))
        .unwrap_or_else(nan))
}

fn get_date_impl(
    ctx: &mut NativeCtx<'_>,
    _args: &[Value],
    name: &'static str,
) -> Result<Value, NativeError> {
    let time = this_time(ctx, name)?;
    Ok(broken_down_for_method(time, name)
        .map(|bd| smi(bd.day as i32))
        .unwrap_or_else(nan))
}

fn get_day_impl(
    ctx: &mut NativeCtx<'_>,
    _args: &[Value],
    name: &'static str,
) -> Result<Value, NativeError> {
    let time = this_time(ctx, name)?;
    Ok(broken_down_for_method(time, name)
        .map(|bd| smi(bd.weekday as i32))
        .unwrap_or_else(nan))
}

fn get_hours_impl(
    ctx: &mut NativeCtx<'_>,
    _args: &[Value],
    name: &'static str,
) -> Result<Value, NativeError> {
    let time = this_time(ctx, name)?;
    Ok(broken_down_for_method(time, name)
        .map(|bd| smi(bd.hour as i32))
        .unwrap_or_else(nan))
}

fn get_minutes_impl(
    ctx: &mut NativeCtx<'_>,
    _args: &[Value],
    name: &'static str,
) -> Result<Value, NativeError> {
    let time = this_time(ctx, name)?;
    Ok(broken_down_for_method(time, name)
        .map(|bd| smi(bd.minute as i32))
        .unwrap_or_else(nan))
}

fn get_seconds_impl(
    ctx: &mut NativeCtx<'_>,
    _args: &[Value],
    name: &'static str,
) -> Result<Value, NativeError> {
    let time = this_time(ctx, name)?;
    Ok(broken_down_for_method(time, name)
        .map(|bd| smi(bd.second as i32))
        .unwrap_or_else(nan))
}

fn get_milliseconds_impl(
    ctx: &mut NativeCtx<'_>,
    _args: &[Value],
    name: &'static str,
) -> Result<Value, NativeError> {
    let time = this_time(ctx, name)?;
    Ok(broken_down_for_method(time, name)
        .map(|bd| smi(bd.millisecond as i32))
        .unwrap_or_else(nan))
}

/// §21.4.4.21 — `getTimezoneOffset()`.
fn get_timezone_offset_impl(
    ctx: &mut NativeCtx<'_>,
    _args: &[Value],
    name: &'static str,
) -> Result<Value, NativeError> {
    let time = this_time(ctx, name)?;
    Ok(Value::number_f64(-local_utc_offset_minutes(time)))
}

/// §B.2.4 (Temporal proposal) — `Date.prototype.toTemporalInstant()`.
fn to_temporal_instant_impl(
    ctx: &mut NativeCtx<'_>,
    _args: &[Value],
    name: &'static str,
) -> Result<Value, NativeError> {
    let time = this_time(ctx, name)?;
    if !time.is_finite() {
        return Err(NativeError::RangeError {
            name,
            reason: "Invalid Date".to_string(),
        });
    }
    let ms = time as i64;
    let inst =
        temporal_rs::Instant::from_epoch_milliseconds(ms).map_err(|_| NativeError::RangeError {
            name,
            reason: "Temporal.Instant out of range".to_string(),
        })?;
    let handle = crate::temporal::payload::JsTemporal::new(
        ctx.heap_mut(),
        crate::temporal::payload::TemporalPayload::Instant(inst),
    )?;
    Ok(Value::temporal(handle))
}

/// §21.4.4.36 — `toISOString()`. `RangeError` on Invalid Date.
fn to_iso_string_impl(
    ctx: &mut NativeCtx<'_>,
    _args: &[Value],
    name: &'static str,
) -> Result<Value, NativeError> {
    let time = this_time(ctx, name)?;
    let s = to_iso_string(time).ok_or_else(|| NativeError::RangeError {
        name,
        reason: "Invalid Date".to_string(),
    })?;
    Ok(Value::string(JsString::from_str(&s, ctx.heap_mut())?))
}

/// §21.4.4.42 — `toString()` / `toUTCString` / `toLocaleString`.
/// Foundation treats local time as UTC but preserves the legacy
/// DateString / TimeString / TimeZoneString shapes.
fn to_string_impl(
    ctx: &mut NativeCtx<'_>,
    _args: &[Value],
    name: &'static str,
) -> Result<Value, NativeError> {
    let time = this_time(ctx, name)?;
    let s = match name {
        "toUTCString" | "toGMTString" => utc_date_time_string(time),
        _ => local_date_time_string(time),
    }
    .unwrap_or_else(|| "Invalid Date".to_string());
    Ok(Value::string(JsString::from_str(&s, ctx.heap_mut())?))
}

/// §21.4.4.27 — `toDateString` / `toLocaleDateString`.
fn to_date_string_impl(
    ctx: &mut NativeCtx<'_>,
    _args: &[Value],
    name: &'static str,
) -> Result<Value, NativeError> {
    let time = this_time(ctx, name)?;
    let s = date_string(time).unwrap_or_else(|| "Invalid Date".to_string());
    Ok(Value::string(JsString::from_str(&s, ctx.heap_mut())?))
}

/// §21.4.4.43 — `toTimeString` / `toLocaleTimeString`.
fn to_time_string_impl(
    ctx: &mut NativeCtx<'_>,
    _args: &[Value],
    name: &'static str,
) -> Result<Value, NativeError> {
    let time = this_time(ctx, name)?;
    let s = time_string(time)
        .map(|clock| format!("{clock} {}", local_timezone_string(time)))
        .unwrap_or_else(|| "Invalid Date".to_string());
    Ok(Value::string(JsString::from_str(&s, ctx.heap_mut())?))
}

/// §21.4.4.x `Date.prototype.set*` — `ToNumber` every provided
/// argument in declaration order. A `valueOf` callback may mutate the
/// receiver's `[[DateValue]]` via `setTime`; the captured time is read
/// by the caller before this runs, the component math uses it, and the
/// final assignment overwrites any in-callback mutation.
fn coerce_set_args(
    ctx: &mut NativeCtx<'_>,
    name: &'static str,
    args: &[Value],
) -> Result<SmallVec<[Value; 4]>, NativeError> {
    let mut coerced: SmallVec<[Value; 4]> = args.iter().cloned().collect();
    if let Some(exec) = ctx.execution_context().cloned() {
        let interp = ctx.interp_mut();
        for slot in coerced.iter_mut() {
            let n = interp
                .coerce_to_number(&exec, slot)
                .map_err(|err| crate::native_function::vm_to_native_error(interp, err, name))?;
            *slot = Value::number(n);
        }
    }
    Ok(coerced)
}

/// Read `coerced[idx]` as `f64`; a missing slot falls back to the
/// component-from-time value (§21.4.4.x "if X is present" branch).
fn read_arg_number(args: &[Value], idx: usize, fallback: f64) -> f64 {
    let Some(v) = args.get(idx) else {
        return fallback;
    };
    primitive_to_number(v)
}

/// First-arg helper. Spec treats the leading parameter as always
/// present (`ToNumber(value)` always runs), so a missing arg becomes
/// `ToNumber(undefined) = NaN` rather than the component fallback.
fn read_primary_arg_number(args: &[Value]) -> f64 {
    let Some(v) = args.first() else {
        return f64::NAN;
    };
    primitive_to_number(v)
}

fn primitive_to_number(v: &Value) -> f64 {
    if let Some(n) = v.as_number() {
        n.as_f64()
    } else if v.is_boolean() {
        if v.as_boolean() == Some(true) {
            1.0
        } else {
            0.0
        }
    } else if v.is_null() {
        0.0
    } else {
        f64::NAN
    }
}

fn year_string(year: i32) -> String {
    if year < 0 {
        format!("-{:04}", year.abs())
    } else {
        format!("{year:04}")
    }
}

fn date_string(time: f64) -> Option<String> {
    let bd = local_broken_down(time)?;
    Some(format!(
        "{} {} {:02} {}",
        WEEKDAY_ABBREVIATIONS[bd.weekday as usize],
        MONTH_ABBREVIATIONS[bd.month as usize],
        bd.day,
        year_string(bd.year)
    ))
}

fn utc_date_string(time: f64) -> Option<String> {
    let bd = broken_down(time)?;
    Some(format!(
        "{}, {:02} {} {}",
        WEEKDAY_ABBREVIATIONS[bd.weekday as usize],
        bd.day,
        MONTH_ABBREVIATIONS[bd.month as usize],
        year_string(bd.year)
    ))
}

fn time_string(time: f64) -> Option<String> {
    let bd = local_broken_down(time)?;
    Some(format!("{:02}:{:02}:{:02}", bd.hour, bd.minute, bd.second))
}

pub(crate) fn local_date_time_string(time: f64) -> Option<String> {
    Some(format!(
        "{} {} {}",
        date_string(time)?,
        time_string(time)?,
        local_timezone_string(time)
    ))
}

fn utc_date_time_string(time: f64) -> Option<String> {
    let bd = broken_down(time)?;
    Some(format!(
        "{} {:02}:{:02}:{:02} GMT",
        utc_date_string(time)?,
        bd.hour,
        bd.minute,
        bd.second
    ))
}

/// Broken-down components packaged as a 7-tuple for the setter pattern.
type Components = (f64, f64, f64, f64, f64, f64, f64);

fn current_components(time: f64, use_utc: bool) -> Components {
    let parts = if use_utc {
        broken_down(time)
    } else {
        local_broken_down(time)
    };
    match parts {
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

fn set_full_year_base_components(time: f64, use_utc: bool) -> Components {
    if !use_utc && time.is_nan() {
        current_components(0.0, true)
    } else {
        current_components(if time.is_nan() { 0.0 } else { time }, use_utc)
    }
}

fn finish_set(
    obj: JsObject,
    ctx: &mut NativeCtx<'_>,
    c: Components,
    use_utc: bool,
) -> Result<Value, NativeError> {
    let (year, month, day, hour, minute, second, ms) = c;
    let new_ms = if use_utc {
        super::make_date(year, month, day, hour, minute, second, ms)
    } else {
        super::make_local_date(year, month, day, hour, minute, second, ms)
    };
    object::set_date_data(obj, ctx.heap_mut(), new_ms);
    let written = object::date_data(obj, ctx.heap()).unwrap_or(f64::NAN);
    Ok(Value::number_f64(written))
}

/// §21.4.4.27 — `setTime(ms)`. Direct write.
fn set_time_impl(
    ctx: &mut NativeCtx<'_>,
    args: &[Value],
    name: &'static str,
) -> Result<Value, NativeError> {
    let (obj, _) = this_handle(ctx, name)?;
    let coerced = coerce_set_args(ctx, name, args)?;
    let ms = read_primary_arg_number(&coerced);
    object::set_date_data(obj, ctx.heap_mut(), ms);
    let written = object::date_data(obj, ctx.heap()).unwrap_or(f64::NAN);
    Ok(Value::number_f64(written))
}

/// §21.4.4.{20,38} `setFullYear` / `setUTCFullYear` — always writes
/// through; a NaN receiver rebases to the epoch (step 3).
fn set_full_year_impl(
    ctx: &mut NativeCtx<'_>,
    args: &[Value],
    name: &'static str,
) -> Result<Value, NativeError> {
    let (obj, time) = this_handle(ctx, name)?;
    let coerced = coerce_set_args(ctx, name, args)?;
    let use_utc = name.contains("UTC");
    let mut c = set_full_year_base_components(time, use_utc);
    c.0 = read_primary_arg_number(&coerced);
    if coerced.len() >= 2 {
        c.1 = read_arg_number(&coerced, 1, c.1);
    }
    if coerced.len() >= 3 {
        c.2 = read_arg_number(&coerced, 2, c.2);
    }
    finish_set(obj, ctx, c, use_utc)
}

/// §21.4.4.x component setters (`setMonth` … `setMilliseconds`) — step
/// 8 returns NaN without writing when the captured time was NaN.
fn set_month_impl(
    ctx: &mut NativeCtx<'_>,
    args: &[Value],
    name: &'static str,
) -> Result<Value, NativeError> {
    let (obj, time) = this_handle(ctx, name)?;
    let coerced = coerce_set_args(ctx, name, args)?;
    if time.is_nan() {
        return Ok(nan());
    }
    let use_utc = name.contains("UTC");
    let mut c = current_components(time, use_utc);
    c.1 = read_primary_arg_number(&coerced);
    if coerced.len() >= 2 {
        c.2 = read_arg_number(&coerced, 1, c.2);
    }
    finish_set(obj, ctx, c, use_utc)
}

fn set_date_impl(
    ctx: &mut NativeCtx<'_>,
    args: &[Value],
    name: &'static str,
) -> Result<Value, NativeError> {
    let (obj, time) = this_handle(ctx, name)?;
    let coerced = coerce_set_args(ctx, name, args)?;
    if time.is_nan() {
        return Ok(nan());
    }
    let use_utc = name.contains("UTC");
    let mut c = current_components(time, use_utc);
    c.2 = read_primary_arg_number(&coerced);
    finish_set(obj, ctx, c, use_utc)
}

fn set_hours_impl(
    ctx: &mut NativeCtx<'_>,
    args: &[Value],
    name: &'static str,
) -> Result<Value, NativeError> {
    let (obj, time) = this_handle(ctx, name)?;
    let coerced = coerce_set_args(ctx, name, args)?;
    if time.is_nan() {
        return Ok(nan());
    }
    let use_utc = name.contains("UTC");
    let mut c = current_components(time, use_utc);
    c.3 = read_primary_arg_number(&coerced);
    if coerced.len() >= 2 {
        c.4 = read_arg_number(&coerced, 1, c.4);
    }
    if coerced.len() >= 3 {
        c.5 = read_arg_number(&coerced, 2, c.5);
    }
    if coerced.len() >= 4 {
        c.6 = read_arg_number(&coerced, 3, c.6);
    }
    finish_set(obj, ctx, c, use_utc)
}

fn set_minutes_impl(
    ctx: &mut NativeCtx<'_>,
    args: &[Value],
    name: &'static str,
) -> Result<Value, NativeError> {
    let (obj, time) = this_handle(ctx, name)?;
    let coerced = coerce_set_args(ctx, name, args)?;
    if time.is_nan() {
        return Ok(nan());
    }
    let use_utc = name.contains("UTC");
    let mut c = current_components(time, use_utc);
    c.4 = read_primary_arg_number(&coerced);
    if coerced.len() >= 2 {
        c.5 = read_arg_number(&coerced, 1, c.5);
    }
    if coerced.len() >= 3 {
        c.6 = read_arg_number(&coerced, 2, c.6);
    }
    finish_set(obj, ctx, c, use_utc)
}

fn set_seconds_impl(
    ctx: &mut NativeCtx<'_>,
    args: &[Value],
    name: &'static str,
) -> Result<Value, NativeError> {
    let (obj, time) = this_handle(ctx, name)?;
    let coerced = coerce_set_args(ctx, name, args)?;
    if time.is_nan() {
        return Ok(nan());
    }
    let use_utc = name.contains("UTC");
    let mut c = current_components(time, use_utc);
    c.5 = read_primary_arg_number(&coerced);
    if coerced.len() >= 2 {
        c.6 = read_arg_number(&coerced, 1, c.6);
    }
    finish_set(obj, ctx, c, use_utc)
}

fn set_milliseconds_impl(
    ctx: &mut NativeCtx<'_>,
    args: &[Value],
    name: &'static str,
) -> Result<Value, NativeError> {
    let (obj, time) = this_handle(ctx, name)?;
    let coerced = coerce_set_args(ctx, name, args)?;
    if time.is_nan() {
        return Ok(nan());
    }
    let use_utc = name.contains("UTC");
    let mut c = current_components(time, use_utc);
    c.6 = read_primary_arg_number(&coerced);
    finish_set(obj, ctx, c, use_utc)
}

/// §B.2.4.1 — `Date.prototype.getYear()` returns `year - 1900`.
fn get_year_impl(
    ctx: &mut NativeCtx<'_>,
    _args: &[Value],
    name: &'static str,
) -> Result<Value, NativeError> {
    let time = this_time(ctx, name)?;
    Ok(local_broken_down(time)
        .map(|bd| smi(bd.year - 1900))
        .unwrap_or_else(nan))
}

/// §B.2.4.2 — `Date.prototype.setYear(year)`.
fn set_year_impl(
    ctx: &mut NativeCtx<'_>,
    args: &[Value],
    name: &'static str,
) -> Result<Value, NativeError> {
    let (obj, time) = this_handle(ctx, name)?;
    let coerced = coerce_set_args(ctx, name, args)?;
    let y = read_primary_arg_number(&coerced);
    if y.is_nan() {
        object::set_date_data(obj, ctx.heap_mut(), f64::NAN);
        return Ok(Value::number_f64(f64::NAN));
    }
    let y_int = y.trunc();
    let yyyy = if (0.0..=99.0).contains(&y_int) {
        y_int + 1900.0
    } else {
        y
    };
    let mut c = set_full_year_base_components(time, false);
    c.0 = yyyy;
    finish_set(obj, ctx, c, false)
}

/// Generates a thin `bridge_*` native per JS method name (binding the
/// name to a shared `*_impl` body) plus the installed `MethodSpec` list.
macro_rules! date_prototype_methods {
    ($($bridge:ident => $imp:ident, $name:literal, $length:literal;)*) => {
        $(
            fn $bridge(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
                $imp(ctx, args, $name)
            }
        )*

        /// Method specs installed as JS-visible own properties on
        /// `Date.prototype` during the `Date` bootstrap.
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
    bridge_get_time              => get_time_impl,             "getTime",             0;
    bridge_value_of              => get_time_impl,             "valueOf",             0;
    bridge_get_full_year         => get_full_year_impl,        "getFullYear",         0;
    bridge_get_utc_full_year     => get_full_year_impl,        "getUTCFullYear",      0;
    bridge_get_month             => get_month_impl,            "getMonth",            0;
    bridge_get_utc_month         => get_month_impl,            "getUTCMonth",         0;
    bridge_get_date              => get_date_impl,             "getDate",             0;
    bridge_get_utc_date          => get_date_impl,             "getUTCDate",          0;
    bridge_get_day               => get_day_impl,              "getDay",              0;
    bridge_get_utc_day           => get_day_impl,              "getUTCDay",           0;
    bridge_get_hours             => get_hours_impl,            "getHours",            0;
    bridge_get_utc_hours         => get_hours_impl,            "getUTCHours",         0;
    bridge_get_minutes           => get_minutes_impl,          "getMinutes",          0;
    bridge_get_utc_minutes       => get_minutes_impl,          "getUTCMinutes",       0;
    bridge_get_seconds           => get_seconds_impl,          "getSeconds",          0;
    bridge_get_utc_seconds       => get_seconds_impl,          "getUTCSeconds",       0;
    bridge_get_milliseconds      => get_milliseconds_impl,     "getMilliseconds",     0;
    bridge_get_utc_milliseconds  => get_milliseconds_impl,     "getUTCMilliseconds",  0;
    bridge_get_timezone_offset   => get_timezone_offset_impl,  "getTimezoneOffset",   0;
    bridge_to_iso_string         => to_iso_string_impl,        "toISOString",         0;
    bridge_to_temporal_instant   => to_temporal_instant_impl,  "toTemporalInstant",   0;
    bridge_to_string             => to_string_impl,            "toString",            0;
    bridge_to_utc_string         => to_string_impl,            "toUTCString",         0;
    bridge_to_gmt_string         => to_string_impl,            "toGMTString",         0;
    bridge_to_date_string        => to_date_string_impl,       "toDateString",        0;
    bridge_to_time_string        => to_time_string_impl,       "toTimeString",        0;
    bridge_to_locale_string      => to_string_impl,            "toLocaleString",      0;
    bridge_to_locale_date_string => to_date_string_impl,       "toLocaleDateString",  0;
    bridge_to_locale_time_string => to_time_string_impl,       "toLocaleTimeString",  0;
    bridge_set_time              => set_time_impl,             "setTime",             1;
    bridge_set_full_year         => set_full_year_impl,        "setFullYear",         3;
    bridge_set_utc_full_year     => set_full_year_impl,        "setUTCFullYear",      3;
    bridge_set_month             => set_month_impl,            "setMonth",            2;
    bridge_set_utc_month         => set_month_impl,            "setUTCMonth",         2;
    bridge_set_date              => set_date_impl,             "setDate",             1;
    bridge_set_utc_date          => set_date_impl,             "setUTCDate",          1;
    bridge_set_hours             => set_hours_impl,            "setHours",            4;
    bridge_set_utc_hours         => set_hours_impl,            "setUTCHours",         4;
    bridge_set_minutes           => set_minutes_impl,          "setMinutes",          3;
    bridge_set_utc_minutes       => set_minutes_impl,          "setUTCMinutes",       3;
    bridge_set_seconds           => set_seconds_impl,          "setSeconds",          2;
    bridge_set_utc_seconds       => set_seconds_impl,          "setUTCSeconds",       2;
    bridge_set_milliseconds      => set_milliseconds_impl,     "setMilliseconds",     1;
    bridge_set_utc_milliseconds  => set_milliseconds_impl,     "setUTCMilliseconds",  1;
    bridge_get_year              => get_year_impl,             "getYear",             0;
    bridge_set_year              => set_year_impl,             "setYear",             1;
);

/// §21.4.4.41 — generic `Date.prototype.toJSON(key)`.
///
/// 1. Let `O` be `? ToObject(this value)`.
/// 2. Let `tv` be `? ToPrimitive(O, number)`.
/// 3. If `tv` is a Number and `tv` is not finite, return `null`.
/// 4. Return `? Invoke(O, "toISOString")`.
///
/// Coercion routes through [`Interpreter::evaluate_to_primitive`] (so a
/// user `@@toPrimitive` / `valueOf` / `toString` fires) and re-enters
/// the interpreter to call `toISOString` so subclass overrides and
/// primitive wrappers are observable.
///
/// # See also
/// - <https://tc39.es/ecma262/#sec-date.prototype.tojson>
/// - <https://tc39.es/ecma262/#sec-invoke>
fn date_prototype_to_json(ctx: &mut NativeCtx<'_>, _args: &[Value]) -> Result<Value, NativeError> {
    const NAME: &str = "Date.prototype.toJSON";
    let receiver = *ctx.this_value();
    // §7.1.18 ToObject(undefined / null) → TypeError.
    if receiver.is_undefined() || receiver.is_null() {
        return Err(NativeError::TypeError {
            name: NAME,
            reason: "Cannot convert undefined or null to object".to_string(),
        });
    }

    let (interp, exec) = ctx.interp_mut_and_context();
    let exec = exec.ok_or_else(|| NativeError::TypeError {
        name: NAME,
        reason: "missing execution context".to_string(),
    })?;

    // Step 2 — ToPrimitive(O, number).
    let tv = match interp.evaluate_to_primitive(&exec, &receiver, ToPrimitiveHint::Number) {
        Ok(v) => v,
        Err(err) => return Err(vm_to_native(interp, NAME, err)),
    };
    // Step 3 — non-finite Number → null.
    if let Some(n) = tv.as_number()
        && !n.as_f64().is_finite()
    {
        return Ok(Value::null());
    }

    // Step 4 — Invoke(O, "toISOString").
    let receiver_is_primitive = receiver.is_number()
        || receiver.is_boolean()
        || receiver.is_string()
        || receiver.is_symbol();
    let method_outcome = if receiver_is_primitive {
        let proto = interp
            .intrinsic_prototype_object_for(&receiver)
            .ok_or_else(|| NativeError::TypeError {
                name: NAME,
                reason: "no intrinsic prototype for receiver".to_string(),
            })?;
        interp.ordinary_get_value(
            &exec,
            Value::object(proto),
            receiver,
            &VmPropertyKey::String("toISOString"),
            0,
        )
    } else {
        interp.ordinary_get_value(
            &exec,
            receiver,
            receiver,
            &VmPropertyKey::String("toISOString"),
            0,
        )
    };
    let method = match method_outcome {
        Ok(v) => v,
        Err(err) => return Err(vm_to_native(interp, NAME, err)),
    };
    let method = match method {
        VmGetOutcome::Value(v) => v,
        VmGetOutcome::InvokeGetter { getter } => {
            match interp.run_callable_sync(&exec, &getter, receiver, SmallVec::new()) {
                Ok(v) => v,
                Err(err) => return Err(vm_to_native(interp, NAME, err)),
            }
        }
    };
    if !abstract_ops::is_callable(&method) {
        return Err(NativeError::TypeError {
            name: NAME,
            reason: "toISOString is not callable".to_string(),
        });
    }
    match interp.run_callable_sync(&exec, &method, receiver, SmallVec::new()) {
        Ok(v) => Ok(v),
        Err(err) => Err(vm_to_native(interp, NAME, err)),
    }
}

/// `VmError → NativeError` mapper for the generic `toJSON` bridge.
/// Preserves user-thrown values via `NativeError::Thrown`.
fn vm_to_native(interp: &crate::Interpreter, name: &'static str, err: VmError) -> NativeError {
    match err {
        VmError::Uncaught => {
            let value = match interp.take_error_detail() {
                Some(crate::run_control::ErrorDetail::Uncaught(m)) => m,
                _ => Default::default(),
            };
            NativeError::Thrown {
                name,
                message: value.into(),
            }
        }
        VmError::TypeError => {
            let message = match interp.take_error_detail() {
                Some(crate::run_control::ErrorDetail::Message(m)) => m,
                _ => Default::default(),
            };
            NativeError::TypeError {
                name,
                reason: message.into(),
            }
        }
        VmError::RangeError => {
            let message = match interp.take_error_detail() {
                Some(crate::run_control::ErrorDetail::Message(m)) => m,
                _ => Default::default(),
            };
            NativeError::RangeError {
                name,
                reason: message.into(),
            }
        }
        other => NativeError::TypeError {
            name,
            reason: other.to_string(),
        },
    }
}

/// Extra method specs that re-enter the full interpreter entry path.
pub static DATE_PROTOTYPE_EXTRA_METHODS: &[MethodSpec] = &[MethodSpec {
    name: "toJSON",
    length: 1,
    attrs: Attr::builtin_function(),
    call: NativeCall::Static(date_prototype_to_json),
}];

static DATE_METHOD_NAMES: std::sync::LazyLock<rustc_hash::FxHashSet<&'static str>> =
    std::sync::LazyLock::new(|| {
        DATE_PROTOTYPE_METHODS
            .iter()
            .chain(DATE_PROTOTYPE_EXTRA_METHODS)
            .map(|m| m.name)
            .collect()
    });

/// Whether `name` is an installed `Date.prototype` method.
#[must_use]
pub fn is_builtin_method(name: &str) -> bool {
    DATE_METHOD_NAMES.contains(name)
}
