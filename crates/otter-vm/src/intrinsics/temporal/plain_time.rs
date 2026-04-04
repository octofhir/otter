//! Temporal.PlainTime constructor and prototype intrinsics.
//!
//! §11.2 Temporal.PlainTime
//! <https://tc39.es/proposal-temporal/#sec-temporal-plaintime-objects>

use std::cmp::Ordering;

use crate::descriptors::{
    JsClassDescriptor, NativeBindingDescriptor, NativeBindingTarget, NativeFunctionDescriptor,
    VmNativeCallError,
};
use crate::object::ObjectHandle;
use crate::value::RegisterValue;

use super::duration::to_duration;
use super::helpers::{self, temporal_err, to_integer_or_zero, to_string_arg};
use super::payload::{TemporalPayload, construct_temporal, require_temporal_payload};

// ── Descriptor ──────────────────────────────────────────────────────

pub fn plain_time_class_descriptor() -> JsClassDescriptor {
    JsClassDescriptor::new("PlainTime")
        .with_constructor(NativeFunctionDescriptor::constructor(
            "PlainTime",
            0,
            plain_time_constructor,
        ))
        .with_binding(stat("from", 1, plain_time_from))
        .with_binding(stat("compare", 2, plain_time_compare))
        .with_binding(getter("hour", pt_hour))
        .with_binding(getter("minute", pt_minute))
        .with_binding(getter("second", pt_second))
        .with_binding(getter("millisecond", pt_millisecond))
        .with_binding(getter("microsecond", pt_microsecond))
        .with_binding(getter("nanosecond", pt_nanosecond))
        .with_binding(proto("add", 1, pt_add))
        .with_binding(proto("subtract", 1, pt_subtract))
        .with_binding(proto("equals", 1, pt_equals))
        .with_binding(proto("toString", 0, pt_to_string))
        .with_binding(proto("toJSON", 0, pt_to_json))
        .with_binding(proto("valueOf", 0, helpers::temporal_value_of))
}

type VmNativeFn = fn(
    &RegisterValue,
    &[RegisterValue],
    &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError>;

fn proto(name: &str, arity: u16, f: VmNativeFn) -> NativeBindingDescriptor {
    NativeBindingDescriptor::new(
        NativeBindingTarget::Prototype,
        NativeFunctionDescriptor::method(name, arity, f),
    )
}

fn getter(name: &str, f: VmNativeFn) -> NativeBindingDescriptor {
    NativeBindingDescriptor::new(
        NativeBindingTarget::Prototype,
        NativeFunctionDescriptor::getter(name, f),
    )
}

fn stat(name: &str, arity: u16, f: VmNativeFn) -> NativeBindingDescriptor {
    NativeBindingDescriptor::new(
        NativeBindingTarget::Constructor,
        NativeFunctionDescriptor::method(name, arity, f),
    )
}

// ── Helpers ─────────────────────────────────────────────────────────

fn require_plain_time(
    this: &RegisterValue,
    runtime: &crate::interpreter::RuntimeState,
) -> Result<temporal_rs::PlainTime, VmNativeCallError> {
    let payload = require_temporal_payload(this, runtime)
        .map_err(|_| VmNativeCallError::Internal("expected Temporal.PlainTime".into()))?;
    payload
        .as_plain_time()
        .copied()
        .ok_or_else(|| VmNativeCallError::Internal("expected Temporal.PlainTime".into()))
}

fn wrap_plain_time(
    pt: temporal_rs::PlainTime,
    runtime: &mut crate::interpreter::RuntimeState,
) -> RegisterValue {
    let proto = runtime.intrinsics().temporal_plain_time_prototype();
    let handle = construct_temporal(TemporalPayload::PlainTime(pt), proto, runtime);
    RegisterValue::from_object_handle(handle.0)
}

/// Extracts a PlainTime from an argument — accepts PlainTime objects or ISO strings.
fn to_plain_time(
    val: RegisterValue,
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<temporal_rs::PlainTime, VmNativeCallError> {
    if let Some(handle) = val.as_object_handle().map(ObjectHandle)
        && let Ok(payload) = runtime.native_payload::<TemporalPayload>(handle)
        && let Some(pt) = payload.as_plain_time()
    {
        return Ok(*pt);
    }
    let s = to_string_arg(&[val], 0, runtime)?;
    temporal_rs::PlainTime::from_utf8(s.as_bytes()).map_err(|e| temporal_err(e, runtime))
}

// ── Constructor ─────────────────────────────────────────────────────

/// §11.2.1 new Temporal.PlainTime ( [ hour, minute, second, ms, us, ns ] )
/// <https://tc39.es/proposal-temporal/#sec-temporal.plaintime>
fn plain_time_constructor(
    _this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let hour = to_integer_or_zero(args, 0, runtime)? as u8;
    let minute = to_integer_or_zero(args, 1, runtime)? as u8;
    let second = to_integer_or_zero(args, 2, runtime)? as u8;
    let millisecond = to_integer_or_zero(args, 3, runtime)? as u16;
    let microsecond = to_integer_or_zero(args, 4, runtime)? as u16;
    let nanosecond = to_integer_or_zero(args, 5, runtime)? as u16;

    let pt =
        temporal_rs::PlainTime::new(hour, minute, second, millisecond, microsecond, nanosecond)
            .map_err(|e| temporal_err(e, runtime))?;
    Ok(wrap_plain_time(pt, runtime))
}

// ── Static methods ──────────────────────────────────────────────────

/// §11.2.2.1 Temporal.PlainTime.from ( item )
/// <https://tc39.es/proposal-temporal/#sec-temporal.plaintime.from>
fn plain_time_from(
    _this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let val = args.first().copied().unwrap_or(RegisterValue::undefined());
    let pt = to_plain_time(val, runtime)?;
    Ok(wrap_plain_time(pt, runtime))
}

/// §11.2.2.2 Temporal.PlainTime.compare ( one, two )
/// <https://tc39.es/proposal-temporal/#sec-temporal.plaintime.compare>
fn plain_time_compare(
    _this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let one = args.first().copied().unwrap_or(RegisterValue::undefined());
    let two = args.get(1).copied().unwrap_or(RegisterValue::undefined());
    let a = to_plain_time(one, runtime)?;
    let b = to_plain_time(two, runtime)?;
    let result = match a.cmp(&b) {
        Ordering::Less => -1i32,
        Ordering::Equal => 0,
        Ordering::Greater => 1,
    };
    Ok(RegisterValue::from_i32(result))
}

// ── Getters ─────────────────────────────────────────────────────────

macro_rules! pt_getter {
    ($name:ident, $method:ident) => {
        fn $name(
            this: &RegisterValue,
            _args: &[RegisterValue],
            runtime: &mut crate::interpreter::RuntimeState,
        ) -> Result<RegisterValue, VmNativeCallError> {
            let pt = require_plain_time(this, runtime)?;
            Ok(RegisterValue::from_number(pt.$method() as f64))
        }
    };
}

// §11.2.3.2–7 PlainTime getters: hour, minute, second, millisecond, microsecond, nanosecond
pt_getter!(pt_hour, hour);
pt_getter!(pt_minute, minute);
pt_getter!(pt_second, second);
pt_getter!(pt_millisecond, millisecond);
pt_getter!(pt_microsecond, microsecond);
pt_getter!(pt_nanosecond, nanosecond);

// ── Prototype methods ───────────────────────────────────────────────

/// §11.2.3.8 Temporal.PlainTime.prototype.add ( duration )
/// <https://tc39.es/proposal-temporal/#sec-temporal.plaintime.prototype.add>
fn pt_add(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let pt = require_plain_time(this, runtime)?;
    let dur_val = args.first().copied().unwrap_or(RegisterValue::undefined());
    let dur = to_duration(dur_val, runtime)?;
    let result = pt.add(&dur).map_err(|e| temporal_err(e, runtime))?;
    Ok(wrap_plain_time(result, runtime))
}

/// §11.2.3.9 Temporal.PlainTime.prototype.subtract ( duration )
/// <https://tc39.es/proposal-temporal/#sec-temporal.plaintime.prototype.subtract>
fn pt_subtract(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let pt = require_plain_time(this, runtime)?;
    let dur_val = args.first().copied().unwrap_or(RegisterValue::undefined());
    let dur = to_duration(dur_val, runtime)?;
    let result = pt.subtract(&dur).map_err(|e| temporal_err(e, runtime))?;
    Ok(wrap_plain_time(result, runtime))
}

/// §11.2.3.14 Temporal.PlainTime.prototype.equals ( other )
/// <https://tc39.es/proposal-temporal/#sec-temporal.plaintime.prototype.equals>
fn pt_equals(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let pt = require_plain_time(this, runtime)?;
    let other_val = args.first().copied().unwrap_or(RegisterValue::undefined());
    let other = to_plain_time(other_val, runtime)?;
    Ok(RegisterValue::from_bool(pt == other))
}

/// §11.2.3.16 Temporal.PlainTime.prototype.toString ( [ options ] )
/// <https://tc39.es/proposal-temporal/#sec-temporal.plaintime.prototype.tostring>
fn pt_to_string(
    this: &RegisterValue,
    _args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let pt = require_plain_time(this, runtime)?;
    let text = pt
        .to_ixdtf_string(temporal_rs::options::ToStringRoundingOptions::default())
        .map_err(|e| temporal_err(e, runtime))?;
    let handle = runtime.alloc_string(text);
    Ok(RegisterValue::from_object_handle(handle.0))
}

/// §11.2.3.17 Temporal.PlainTime.prototype.toJSON ( )
/// <https://tc39.es/proposal-temporal/#sec-temporal.plaintime.prototype.tojson>
fn pt_to_json(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    pt_to_string(this, args, runtime)
}
