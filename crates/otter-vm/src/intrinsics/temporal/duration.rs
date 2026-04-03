//! Temporal.Duration constructor and prototype intrinsics.
//!
//! §7.2 Temporal.Duration
//! <https://tc39.es/proposal-temporal/#sec-temporal-duration-objects>

use crate::descriptors::{
    JsClassDescriptor, NativeBindingDescriptor, NativeBindingTarget, NativeFunctionDescriptor,
    VmNativeCallError,
};
use crate::object::ObjectHandle;
use crate::value::RegisterValue;

use super::helpers::{self, temporal_err, to_integer_or_zero, to_string_arg};
use super::payload::{TemporalPayload, construct_temporal, require_temporal_payload};

// ── Descriptor ──────────────────────────────────────────────────────

pub fn duration_class_descriptor() -> JsClassDescriptor {
    JsClassDescriptor::new("Duration")
        .with_constructor(NativeFunctionDescriptor::constructor(
            "Duration", 0, duration_constructor,
        ))
        .with_binding(stat("from", 1, duration_from))
        .with_binding(stat("compare", 2, duration_compare))
        .with_binding(proto("years", 0, dur_years))
        .with_binding(proto("months", 0, dur_months))
        .with_binding(proto("weeks", 0, dur_weeks))
        .with_binding(proto("days", 0, dur_days))
        .with_binding(proto("hours", 0, dur_hours))
        .with_binding(proto("minutes", 0, dur_minutes))
        .with_binding(proto("seconds", 0, dur_seconds))
        .with_binding(proto("milliseconds", 0, dur_milliseconds))
        .with_binding(proto("microseconds", 0, dur_microseconds))
        .with_binding(proto("nanoseconds", 0, dur_nanoseconds))
        .with_binding(proto("sign", 0, dur_sign))
        .with_binding(proto("blank", 0, dur_blank))
        .with_binding(proto("negated", 0, dur_negated))
        .with_binding(proto("abs", 0, dur_abs))
        .with_binding(proto("add", 1, dur_add))
        .with_binding(proto("subtract", 1, dur_subtract))
        .with_binding(proto("toString", 0, dur_to_string))
        .with_binding(proto("toJSON", 0, dur_to_json))
        .with_binding(proto("valueOf", 0, helpers::temporal_value_of))
}

fn proto(
    name: &str,
    arity: u16,
    f: fn(&RegisterValue, &[RegisterValue], &mut crate::interpreter::RuntimeState)
        -> Result<RegisterValue, VmNativeCallError>,
) -> NativeBindingDescriptor {
    NativeBindingDescriptor::new(
        NativeBindingTarget::Prototype,
        NativeFunctionDescriptor::method(name, arity, f),
    )
}

fn stat(
    name: &str,
    arity: u16,
    f: fn(&RegisterValue, &[RegisterValue], &mut crate::interpreter::RuntimeState)
        -> Result<RegisterValue, VmNativeCallError>,
) -> NativeBindingDescriptor {
    NativeBindingDescriptor::new(
        NativeBindingTarget::Constructor,
        NativeFunctionDescriptor::method(name, arity, f),
    )
}

// ── Helpers ─────────────────────────────────────────────────────────

fn require_duration(
    this: &RegisterValue,
    runtime: &crate::interpreter::RuntimeState,
) -> Result<temporal_rs::Duration, VmNativeCallError> {
    let payload = require_temporal_payload(this, runtime)
        .map_err(|_| VmNativeCallError::Internal("expected Temporal.Duration".into()))?;
    payload
        .as_duration()
        .copied()
        .ok_or_else(|| VmNativeCallError::Internal("expected Temporal.Duration".into()))
}

fn wrap_duration(
    dur: temporal_rs::Duration,
    runtime: &mut crate::interpreter::RuntimeState,
) -> RegisterValue {
    let proto = runtime.intrinsics().temporal_duration_prototype();
    let handle = construct_temporal(TemporalPayload::Duration(dur), proto, runtime);
    RegisterValue::from_object_handle(handle.0)
}

/// Extracts a Duration from an argument — accepts Duration objects or ISO strings.
pub fn to_duration(
    val: RegisterValue,
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<temporal_rs::Duration, VmNativeCallError> {
    if let Some(handle) = val.as_object_handle().map(ObjectHandle)
        && let Ok(payload) = runtime.native_payload::<TemporalPayload>(handle)
        && let Some(dur) = payload.as_duration()
    {
        return Ok(*dur);
    }
    let s = to_string_arg(&[val], 0, runtime)?;
    temporal_rs::Duration::from_utf8(s.as_bytes()).map_err(|e| temporal_err(e, runtime))
}

// ── Constructor ─────────────────────────────────────────────────────

/// §7.2.1 new Temporal.Duration ( [ y, mo, w, d, h, mi, s, ms, us, ns ] )
/// <https://tc39.es/proposal-temporal/#sec-temporal.duration>
#[allow(clippy::too_many_lines)]
fn duration_constructor(
    _this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let years = to_integer_or_zero(args, 0, runtime)? as i64;
    let months = to_integer_or_zero(args, 1, runtime)? as i64;
    let weeks = to_integer_or_zero(args, 2, runtime)? as i64;
    let days = to_integer_or_zero(args, 3, runtime)? as i64;
    let hours = to_integer_or_zero(args, 4, runtime)? as i64;
    let minutes = to_integer_or_zero(args, 5, runtime)? as i64;
    let seconds = to_integer_or_zero(args, 6, runtime)? as i64;
    let milliseconds = to_integer_or_zero(args, 7, runtime)? as i64;
    let microseconds = to_integer_or_zero(args, 8, runtime)? as i128;
    let nanoseconds = to_integer_or_zero(args, 9, runtime)? as i128;

    let dur = temporal_rs::Duration::new(
        years,
        months,
        weeks,
        days,
        hours,
        minutes,
        seconds,
        milliseconds,
        microseconds,
        nanoseconds,
    )
    .map_err(|e| temporal_err(e, runtime))?;
    Ok(wrap_duration(dur, runtime))
}

// ── Static methods ──────────────────────────────────────────────────

/// §7.2.2.1 Temporal.Duration.from ( item )
/// <https://tc39.es/proposal-temporal/#sec-temporal.duration.from>
fn duration_from(
    _this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let val = args.first().copied().unwrap_or(RegisterValue::undefined());
    let dur = to_duration(val, runtime)?;
    Ok(wrap_duration(dur, runtime))
}

/// §7.2.2.2 Temporal.Duration.compare ( one, two )
/// <https://tc39.es/proposal-temporal/#sec-temporal.duration.compare>
fn duration_compare(
    _this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let one = args.first().copied().unwrap_or(RegisterValue::undefined());
    let two = args.get(1).copied().unwrap_or(RegisterValue::undefined());
    let a = to_duration(one, runtime)?;
    let b = to_duration(two, runtime)?;
    let cmp = a
        .compare_with_provider(&b, None, &*temporal_rs::provider::COMPILED_TZ_PROVIDER)
        .map_err(|e| temporal_err(e, runtime))?;
    Ok(RegisterValue::from_i32(cmp as i32))
}

// ── Getters ─────────────────────────────────────────────────────────

macro_rules! dur_getter_i64 {
    ($name:ident, $method:ident) => {
        fn $name(
            this: &RegisterValue,
            _args: &[RegisterValue],
            runtime: &mut crate::interpreter::RuntimeState,
        ) -> Result<RegisterValue, VmNativeCallError> {
            let dur = require_duration(this, runtime)?;
            Ok(RegisterValue::from_number(dur.$method() as f64))
        }
    };
}

macro_rules! dur_getter_i128 {
    ($name:ident, $method:ident) => {
        fn $name(
            this: &RegisterValue,
            _args: &[RegisterValue],
            runtime: &mut crate::interpreter::RuntimeState,
        ) -> Result<RegisterValue, VmNativeCallError> {
            let dur = require_duration(this, runtime)?;
            Ok(RegisterValue::from_number(dur.$method() as f64))
        }
    };
}

dur_getter_i64!(dur_years, years);
dur_getter_i64!(dur_months, months);
dur_getter_i64!(dur_weeks, weeks);
dur_getter_i64!(dur_days, days);
dur_getter_i64!(dur_hours, hours);
dur_getter_i64!(dur_minutes, minutes);
dur_getter_i64!(dur_seconds, seconds);
dur_getter_i64!(dur_milliseconds, milliseconds);
dur_getter_i128!(dur_microseconds, microseconds);
dur_getter_i128!(dur_nanoseconds, nanoseconds);

/// §7.2.3.12 get Temporal.Duration.prototype.sign
/// <https://tc39.es/proposal-temporal/#sec-get-temporal.duration.prototype.sign>
fn dur_sign(
    this: &RegisterValue,
    _args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let dur = require_duration(this, runtime)?;
    Ok(RegisterValue::from_i32(dur.sign() as i32))
}

/// §7.2.3.13 get Temporal.Duration.prototype.blank
/// <https://tc39.es/proposal-temporal/#sec-get-temporal.duration.prototype.blank>
fn dur_blank(
    this: &RegisterValue,
    _args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let dur = require_duration(this, runtime)?;
    Ok(RegisterValue::from_bool(dur.is_zero()))
}

// ── Prototype methods ───────────────────────────────────────────────

/// §7.2.3.15 Temporal.Duration.prototype.negated ( )
/// <https://tc39.es/proposal-temporal/#sec-temporal.duration.prototype.negated>
fn dur_negated(
    this: &RegisterValue,
    _args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let dur = require_duration(this, runtime)?;
    Ok(wrap_duration(dur.negated(), runtime))
}

/// §7.2.3.16 Temporal.Duration.prototype.abs ( )
/// <https://tc39.es/proposal-temporal/#sec-temporal.duration.prototype.abs>
fn dur_abs(
    this: &RegisterValue,
    _args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let dur = require_duration(this, runtime)?;
    Ok(wrap_duration(dur.abs(), runtime))
}

/// §7.2.3.17 Temporal.Duration.prototype.add ( other )
/// <https://tc39.es/proposal-temporal/#sec-temporal.duration.prototype.add>
fn dur_add(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let dur = require_duration(this, runtime)?;
    let other_val = args.first().copied().unwrap_or(RegisterValue::undefined());
    let other = to_duration(other_val, runtime)?;
    let result = dur.add(&other).map_err(|e| temporal_err(e, runtime))?;
    Ok(wrap_duration(result, runtime))
}

/// §7.2.3.18 Temporal.Duration.prototype.subtract ( other )
/// <https://tc39.es/proposal-temporal/#sec-temporal.duration.prototype.subtract>
fn dur_subtract(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let dur = require_duration(this, runtime)?;
    let other_val = args.first().copied().unwrap_or(RegisterValue::undefined());
    let other = to_duration(other_val, runtime)?;
    let result = dur.subtract(&other).map_err(|e| temporal_err(e, runtime))?;
    Ok(wrap_duration(result, runtime))
}

/// §7.2.3.21 Temporal.Duration.prototype.toString ( [ options ] )
/// <https://tc39.es/proposal-temporal/#sec-temporal.duration.prototype.tostring>
fn dur_to_string(
    this: &RegisterValue,
    _args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let dur = require_duration(this, runtime)?;
    let text = dur
        .as_temporal_string(temporal_rs::options::ToStringRoundingOptions::default())
        .map_err(|e| temporal_err(e, runtime))?;
    let handle = runtime.alloc_string(text);
    Ok(RegisterValue::from_object_handle(handle.0))
}

/// §7.2.3.22 Temporal.Duration.prototype.toJSON ( )
/// <https://tc39.es/proposal-temporal/#sec-temporal.duration.prototype.tojson>
fn dur_to_json(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    dur_to_string(this, args, runtime)
}
