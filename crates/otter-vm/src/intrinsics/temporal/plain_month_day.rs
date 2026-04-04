//! Temporal.PlainMonthDay constructor and prototype intrinsics.
//!
//! §14.2 Temporal.PlainMonthDay
//! <https://tc39.es/proposal-temporal/#sec-temporal-plainmonthday-objects>

use crate::descriptors::{
    JsClassDescriptor, NativeBindingDescriptor, NativeBindingTarget, NativeFunctionDescriptor,
    VmNativeCallError,
};
use crate::object::ObjectHandle;
use crate::value::RegisterValue;

use super::helpers::{self, temporal_err, to_integer_or_zero, to_string_arg};
use super::payload::{TemporalPayload, construct_temporal, require_temporal_payload};

// ── Descriptor ──────────────────────────────────────────────────────

pub fn plain_month_day_class_descriptor() -> JsClassDescriptor {
    JsClassDescriptor::new("PlainMonthDay")
        .with_constructor(NativeFunctionDescriptor::constructor(
            "PlainMonthDay",
            2,
            pmd_constructor,
        ))
        .with_binding(stat("from", 1, pmd_from))
        .with_binding(getter("calendarId", pmd_calendar_id))
        .with_binding(getter("monthCode", pmd_month_code))
        .with_binding(getter("day", pmd_day))
        .with_binding(proto("equals", 1, pmd_equals))
        .with_binding(proto("toString", 0, pmd_to_string))
        .with_binding(proto("toJSON", 0, pmd_to_json))
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

fn require_plain_month_day(
    this: &RegisterValue,
    runtime: &crate::interpreter::RuntimeState,
) -> Result<temporal_rs::PlainMonthDay, VmNativeCallError> {
    let payload = require_temporal_payload(this, runtime)
        .map_err(|_| VmNativeCallError::Internal("expected Temporal.PlainMonthDay".into()))?;
    payload
        .as_plain_month_day()
        .cloned()
        .ok_or_else(|| VmNativeCallError::Internal("expected Temporal.PlainMonthDay".into()))
}

fn wrap_plain_month_day(
    pmd: temporal_rs::PlainMonthDay,
    runtime: &mut crate::interpreter::RuntimeState,
) -> RegisterValue {
    let proto = runtime.intrinsics().temporal_plain_month_day_prototype();
    let handle = construct_temporal(TemporalPayload::PlainMonthDay(pmd), proto, runtime);
    RegisterValue::from_object_handle(handle.0)
}

/// Extracts a PlainMonthDay from an argument — accepts objects or ISO strings.
fn to_plain_month_day(
    val: RegisterValue,
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<temporal_rs::PlainMonthDay, VmNativeCallError> {
    if let Some(handle) = val.as_object_handle().map(ObjectHandle)
        && let Ok(payload) = runtime.native_payload::<TemporalPayload>(handle)
        && let Some(pmd) = payload.as_plain_month_day()
    {
        return Ok(pmd.clone());
    }
    let s = to_string_arg(&[val], 0, runtime)?;
    temporal_rs::PlainMonthDay::from_utf8(s.as_bytes()).map_err(|e| temporal_err(e, runtime))
}

// ── Constructor ─────────────────────────────────────────────────────

/// §14.2.1 new Temporal.PlainMonthDay ( month, day [, calendar, refYear ] )
/// <https://tc39.es/proposal-temporal/#sec-temporal.plainmonthday>
fn pmd_constructor(
    _this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let month = to_integer_or_zero(args, 0, runtime)? as u8;
    let day = to_integer_or_zero(args, 1, runtime)? as u8;
    let ref_year = if args.len() > 3 {
        to_integer_or_zero(args, 3, runtime)? as i32
    } else {
        1972 // ISO reference year
    };

    let pmd = temporal_rs::PlainMonthDay::new_with_overflow(
        month,
        day,
        temporal_rs::Calendar::default(),
        temporal_rs::options::Overflow::default(),
        Some(ref_year),
    )
    .map_err(|e| temporal_err(e, runtime))?;
    Ok(wrap_plain_month_day(pmd, runtime))
}

// ── Static methods ──────────────────────────────────────────────────

/// §14.2.2.1 Temporal.PlainMonthDay.from ( item )
/// <https://tc39.es/proposal-temporal/#sec-temporal.plainmonthday.from>
fn pmd_from(
    _this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let val = args.first().copied().unwrap_or(RegisterValue::undefined());
    let pmd = to_plain_month_day(val, runtime)?;
    Ok(wrap_plain_month_day(pmd, runtime))
}

// ── Getters ─────────────────────────────────────────────────────────

/// §14.2.3.2 get Temporal.PlainMonthDay.prototype.calendarId
/// <https://tc39.es/proposal-temporal/#sec-get-temporal.plainmonthday.prototype.calendarid>
fn pmd_calendar_id(
    this: &RegisterValue,
    _args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let pmd = require_plain_month_day(this, runtime)?;
    let id = pmd.calendar().identifier();
    let handle = runtime.alloc_string(id.to_string());
    Ok(RegisterValue::from_object_handle(handle.0))
}

/// §14.2.3.3 get Temporal.PlainMonthDay.prototype.monthCode
/// <https://tc39.es/proposal-temporal/#sec-get-temporal.plainmonthday.prototype.monthcode>
fn pmd_month_code(
    this: &RegisterValue,
    _args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let pmd = require_plain_month_day(this, runtime)?;
    let code = pmd.month_code().as_str().to_string();
    let handle = runtime.alloc_string(code);
    Ok(RegisterValue::from_object_handle(handle.0))
}

/// §14.2.3.4 get Temporal.PlainMonthDay.prototype.day
/// <https://tc39.es/proposal-temporal/#sec-get-temporal.plainmonthday.prototype.day>
fn pmd_day(
    this: &RegisterValue,
    _args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let pmd = require_plain_month_day(this, runtime)?;
    Ok(RegisterValue::from_number(pmd.day() as f64))
}

// ── Prototype methods ───────────────────────────────────────────────

/// §14.2.3.6 Temporal.PlainMonthDay.prototype.equals ( other )
/// <https://tc39.es/proposal-temporal/#sec-temporal.plainmonthday.prototype.equals>
fn pmd_equals(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let pmd = require_plain_month_day(this, runtime)?;
    let other_val = args.first().copied().unwrap_or(RegisterValue::undefined());
    let other = to_plain_month_day(other_val, runtime)?;
    // PlainMonthDay equality: same month_code, same day, same calendar
    let eq = pmd.month_code() == other.month_code()
        && pmd.day() == other.day()
        && pmd.calendar().identifier() == other.calendar().identifier();
    Ok(RegisterValue::from_bool(eq))
}

/// §14.2.3.8 Temporal.PlainMonthDay.prototype.toString ( [ options ] )
/// <https://tc39.es/proposal-temporal/#sec-temporal.plainmonthday.prototype.tostring>
fn pmd_to_string(
    this: &RegisterValue,
    _args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let pmd = require_plain_month_day(this, runtime)?;
    let text = pmd.to_ixdtf_string(temporal_rs::options::DisplayCalendar::Auto);
    let handle = runtime.alloc_string(text);
    Ok(RegisterValue::from_object_handle(handle.0))
}

/// §14.2.3.9 Temporal.PlainMonthDay.prototype.toJSON ( )
/// <https://tc39.es/proposal-temporal/#sec-temporal.plainmonthday.prototype.tojson>
fn pmd_to_json(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    pmd_to_string(this, args, runtime)
}
