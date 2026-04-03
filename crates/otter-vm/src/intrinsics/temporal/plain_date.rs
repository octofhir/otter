//! Temporal.PlainDate constructor and prototype intrinsics.
//!
//! §10.2 Temporal.PlainDate
//! <https://tc39.es/proposal-temporal/#sec-temporal-plaindate-objects>

use std::cmp::Ordering;

use crate::descriptors::{
    JsClassDescriptor, NativeBindingDescriptor, NativeBindingTarget, NativeFunctionDescriptor,
    VmNativeCallError,
};
use crate::object::ObjectHandle;
use crate::value::RegisterValue;

use super::helpers::{self, temporal_err, to_integer_or_zero, to_string_arg};
use super::payload::{TemporalPayload, construct_temporal, require_temporal_payload};
use super::duration::to_duration;

// ── Descriptor ──────────────────────────────────────────────────────

pub fn plain_date_class_descriptor() -> JsClassDescriptor {
    JsClassDescriptor::new("PlainDate")
        .with_constructor(NativeFunctionDescriptor::constructor(
            "PlainDate", 3, plain_date_constructor,
        ))
        .with_binding(stat("from", 1, plain_date_from))
        .with_binding(stat("compare", 2, plain_date_compare))
        .with_binding(proto("calendarId", 0, pd_calendar_id))
        .with_binding(proto("year", 0, pd_year))
        .with_binding(proto("month", 0, pd_month))
        .with_binding(proto("monthCode", 0, pd_month_code))
        .with_binding(proto("day", 0, pd_day))
        .with_binding(proto("dayOfWeek", 0, pd_day_of_week))
        .with_binding(proto("dayOfYear", 0, pd_day_of_year))
        .with_binding(proto("weekOfYear", 0, pd_week_of_year))
        .with_binding(proto("yearOfWeek", 0, pd_year_of_week))
        .with_binding(proto("daysInWeek", 0, pd_days_in_week))
        .with_binding(proto("daysInMonth", 0, pd_days_in_month))
        .with_binding(proto("daysInYear", 0, pd_days_in_year))
        .with_binding(proto("monthsInYear", 0, pd_months_in_year))
        .with_binding(proto("inLeapYear", 0, pd_in_leap_year))
        .with_binding(proto("add", 1, pd_add))
        .with_binding(proto("subtract", 1, pd_subtract))
        .with_binding(proto("equals", 1, pd_equals))
        .with_binding(proto("toString", 0, pd_to_string))
        .with_binding(proto("toJSON", 0, pd_to_json))
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

fn require_plain_date(
    this: &RegisterValue,
    runtime: &crate::interpreter::RuntimeState,
) -> Result<temporal_rs::PlainDate, VmNativeCallError> {
    let payload = require_temporal_payload(this, runtime)
        .map_err(|_| VmNativeCallError::Internal("expected Temporal.PlainDate".into()))?;
    payload
        .as_plain_date()
        .cloned()
        .ok_or_else(|| VmNativeCallError::Internal("expected Temporal.PlainDate".into()))
}

fn wrap_plain_date(
    pd: temporal_rs::PlainDate,
    runtime: &mut crate::interpreter::RuntimeState,
) -> RegisterValue {
    let proto = runtime.intrinsics().temporal_plain_date_prototype();
    let handle = construct_temporal(TemporalPayload::PlainDate(pd), proto, runtime);
    RegisterValue::from_object_handle(handle.0)
}

/// Extracts a PlainDate from an argument — accepts PlainDate objects or ISO strings.
fn to_plain_date(
    val: RegisterValue,
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<temporal_rs::PlainDate, VmNativeCallError> {
    if let Some(handle) = val.as_object_handle().map(ObjectHandle)
        && let Ok(payload) = runtime.native_payload::<TemporalPayload>(handle)
        && let Some(pd) = payload.as_plain_date()
    {
        return Ok(pd.clone());
    }
    let s = to_string_arg(&[val], 0, runtime)?;
    temporal_rs::PlainDate::from_utf8(s.as_bytes()).map_err(|e| temporal_err(e, runtime))
}

// ── Constructor ─────────────────────────────────────────────────────

/// §10.2.1 new Temporal.PlainDate ( isoYear, isoMonth, isoDay [ , calendar ] )
/// <https://tc39.es/proposal-temporal/#sec-temporal.plaindate>
fn plain_date_constructor(
    _this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let year = to_integer_or_zero(args, 0, runtime)? as i32;
    let month = to_integer_or_zero(args, 1, runtime)? as u8;
    let day = to_integer_or_zero(args, 2, runtime)? as u8;

    let pd = temporal_rs::PlainDate::new(year, month, day, temporal_rs::Calendar::default())
        .map_err(|e| temporal_err(e, runtime))?;
    Ok(wrap_plain_date(pd, runtime))
}

// ── Static methods ──────────────────────────────────────────────────

/// §10.2.2.1 Temporal.PlainDate.from ( item )
/// <https://tc39.es/proposal-temporal/#sec-temporal.plaindate.from>
fn plain_date_from(
    _this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let val = args.first().copied().unwrap_or(RegisterValue::undefined());
    let pd = to_plain_date(val, runtime)?;
    Ok(wrap_plain_date(pd, runtime))
}

/// §10.2.2.2 Temporal.PlainDate.compare ( one, two )
/// <https://tc39.es/proposal-temporal/#sec-temporal.plaindate.compare>
fn plain_date_compare(
    _this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let one = args.first().copied().unwrap_or(RegisterValue::undefined());
    let two = args.get(1).copied().unwrap_or(RegisterValue::undefined());
    let a = to_plain_date(one, runtime)?;
    let b = to_plain_date(two, runtime)?;
    let result = match a.compare_iso(&b) {
        Ordering::Less => -1i32,
        Ordering::Equal => 0,
        Ordering::Greater => 1,
    };
    Ok(RegisterValue::from_i32(result))
}

// ── Getters ─────────────────────────────────────────────────────────

macro_rules! pd_getter_num {
    ($name:ident, $method:ident) => {
        fn $name(
            this: &RegisterValue,
            _args: &[RegisterValue],
            runtime: &mut crate::interpreter::RuntimeState,
        ) -> Result<RegisterValue, VmNativeCallError> {
            let pd = require_plain_date(this, runtime)?;
            Ok(RegisterValue::from_number(pd.$method() as f64))
        }
    };
}

/// §10.2.3.2 get Temporal.PlainDate.prototype.calendarId
/// <https://tc39.es/proposal-temporal/#sec-get-temporal.plaindate.prototype.calendarid>
fn pd_calendar_id(
    this: &RegisterValue,
    _args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let pd = require_plain_date(this, runtime)?;
    let id = pd.calendar().identifier();
    let handle = runtime.alloc_string(id.to_string());
    Ok(RegisterValue::from_object_handle(handle.0))
}

pd_getter_num!(pd_year, year);
pd_getter_num!(pd_month, month);

/// §10.2.3.5 get Temporal.PlainDate.prototype.monthCode
/// <https://tc39.es/proposal-temporal/#sec-get-temporal.plaindate.prototype.monthcode>
fn pd_month_code(
    this: &RegisterValue,
    _args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let pd = require_plain_date(this, runtime)?;
    let code = pd.month_code().as_str().to_string();
    let handle = runtime.alloc_string(code);
    Ok(RegisterValue::from_object_handle(handle.0))
}

pd_getter_num!(pd_day, day);
pd_getter_num!(pd_day_of_week, day_of_week);
pd_getter_num!(pd_day_of_year, day_of_year);

/// §10.2.3.9 get Temporal.PlainDate.prototype.weekOfYear
/// <https://tc39.es/proposal-temporal/#sec-get-temporal.plaindate.prototype.weekofyear>
fn pd_week_of_year(
    this: &RegisterValue,
    _args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let pd = require_plain_date(this, runtime)?;
    match pd.week_of_year() {
        Some(w) => Ok(RegisterValue::from_number(w as f64)),
        None => Ok(RegisterValue::undefined()),
    }
}

/// §10.2.3.10 get Temporal.PlainDate.prototype.yearOfWeek
/// <https://tc39.es/proposal-temporal/#sec-get-temporal.plaindate.prototype.yearofweek>
fn pd_year_of_week(
    this: &RegisterValue,
    _args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let pd = require_plain_date(this, runtime)?;
    match pd.year_of_week() {
        Some(y) => Ok(RegisterValue::from_number(y as f64)),
        None => Ok(RegisterValue::undefined()),
    }
}

pd_getter_num!(pd_days_in_week, days_in_week);
pd_getter_num!(pd_days_in_month, days_in_month);
pd_getter_num!(pd_days_in_year, days_in_year);
pd_getter_num!(pd_months_in_year, months_in_year);

/// §10.2.3.15 get Temporal.PlainDate.prototype.inLeapYear
/// <https://tc39.es/proposal-temporal/#sec-get-temporal.plaindate.prototype.inleapyear>
fn pd_in_leap_year(
    this: &RegisterValue,
    _args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let pd = require_plain_date(this, runtime)?;
    Ok(RegisterValue::from_bool(pd.in_leap_year()))
}

// ── Prototype methods ───────────────────────────────────────────────

/// §10.2.3.17 Temporal.PlainDate.prototype.add ( duration )
/// <https://tc39.es/proposal-temporal/#sec-temporal.plaindate.prototype.add>
fn pd_add(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let pd = require_plain_date(this, runtime)?;
    let dur_val = args.first().copied().unwrap_or(RegisterValue::undefined());
    let dur = to_duration(dur_val, runtime)?;
    let result = pd.add(&dur, None).map_err(|e| temporal_err(e, runtime))?;
    Ok(wrap_plain_date(result, runtime))
}

/// §10.2.3.18 Temporal.PlainDate.prototype.subtract ( duration )
/// <https://tc39.es/proposal-temporal/#sec-temporal.plaindate.prototype.subtract>
fn pd_subtract(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let pd = require_plain_date(this, runtime)?;
    let dur_val = args.first().copied().unwrap_or(RegisterValue::undefined());
    let dur = to_duration(dur_val, runtime)?;
    let result = pd
        .subtract(&dur, None)
        .map_err(|e| temporal_err(e, runtime))?;
    Ok(wrap_plain_date(result, runtime))
}

/// §10.2.3.20 Temporal.PlainDate.prototype.equals ( other )
/// <https://tc39.es/proposal-temporal/#sec-temporal.plaindate.prototype.equals>
fn pd_equals(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let pd = require_plain_date(this, runtime)?;
    let other_val = args.first().copied().unwrap_or(RegisterValue::undefined());
    let other = to_plain_date(other_val, runtime)?;
    Ok(RegisterValue::from_bool(pd.compare_iso(&other) == Ordering::Equal))
}

/// §10.2.3.21 Temporal.PlainDate.prototype.toString ( [ options ] )
/// <https://tc39.es/proposal-temporal/#sec-temporal.plaindate.prototype.tostring>
fn pd_to_string(
    this: &RegisterValue,
    _args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let pd = require_plain_date(this, runtime)?;
    let text = pd.to_ixdtf_string(temporal_rs::options::DisplayCalendar::Auto);
    let handle = runtime.alloc_string(text);
    Ok(RegisterValue::from_object_handle(handle.0))
}

/// §10.2.3.22 Temporal.PlainDate.prototype.toJSON ( )
/// <https://tc39.es/proposal-temporal/#sec-temporal.plaindate.prototype.tojson>
fn pd_to_json(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    pd_to_string(this, args, runtime)
}
