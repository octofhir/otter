//! Temporal.PlainDateTime constructor and prototype intrinsics.
//!
//! §12.2 Temporal.PlainDateTime
//! <https://tc39.es/proposal-temporal/#sec-temporal-plaindatetime-objects>

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

pub fn plain_date_time_class_descriptor() -> JsClassDescriptor {
    JsClassDescriptor::new("PlainDateTime")
        .with_constructor(NativeFunctionDescriptor::constructor(
            "PlainDateTime",
            3,
            pdt_constructor,
        ))
        .with_binding(stat("from", 1, pdt_from))
        .with_binding(stat("compare", 2, pdt_compare))
        // Date getters
        .with_binding(getter("calendarId", pdt_calendar_id))
        .with_binding(getter("year", pdt_year))
        .with_binding(getter("month", pdt_month))
        .with_binding(getter("monthCode", pdt_month_code))
        .with_binding(getter("day", pdt_day))
        .with_binding(getter("dayOfWeek", pdt_day_of_week))
        .with_binding(getter("dayOfYear", pdt_day_of_year))
        .with_binding(getter("weekOfYear", pdt_week_of_year))
        .with_binding(getter("yearOfWeek", pdt_year_of_week))
        .with_binding(getter("daysInWeek", pdt_days_in_week))
        .with_binding(getter("daysInMonth", pdt_days_in_month))
        .with_binding(getter("daysInYear", pdt_days_in_year))
        .with_binding(getter("monthsInYear", pdt_months_in_year))
        .with_binding(getter("inLeapYear", pdt_in_leap_year))
        // Time getters
        .with_binding(getter("hour", pdt_hour))
        .with_binding(getter("minute", pdt_minute))
        .with_binding(getter("second", pdt_second))
        .with_binding(getter("millisecond", pdt_millisecond))
        .with_binding(getter("microsecond", pdt_microsecond))
        .with_binding(getter("nanosecond", pdt_nanosecond))
        // Methods
        .with_binding(proto("add", 1, pdt_add))
        .with_binding(proto("subtract", 1, pdt_subtract))
        .with_binding(proto("equals", 1, pdt_equals))
        .with_binding(proto("toString", 0, pdt_to_string))
        .with_binding(proto("toJSON", 0, pdt_to_json))
        .with_binding(proto("toPlainDate", 0, pdt_to_plain_date))
        .with_binding(proto("toPlainTime", 0, pdt_to_plain_time))
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

fn require_plain_date_time(
    this: &RegisterValue,
    runtime: &crate::interpreter::RuntimeState,
) -> Result<temporal_rs::PlainDateTime, VmNativeCallError> {
    let payload = require_temporal_payload(this, runtime)
        .map_err(|_| VmNativeCallError::Internal("expected Temporal.PlainDateTime".into()))?;
    payload
        .as_plain_date_time()
        .cloned()
        .ok_or_else(|| VmNativeCallError::Internal("expected Temporal.PlainDateTime".into()))
}

fn wrap_plain_date_time(
    pdt: temporal_rs::PlainDateTime,
    runtime: &mut crate::interpreter::RuntimeState,
) -> RegisterValue {
    let proto = runtime.intrinsics().temporal_plain_date_time_prototype();
    let handle = construct_temporal(TemporalPayload::PlainDateTime(pdt), proto, runtime);
    RegisterValue::from_object_handle(handle.0)
}

/// Extracts a PlainDateTime from an argument — accepts objects or ISO strings.
fn to_plain_date_time(
    val: RegisterValue,
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<temporal_rs::PlainDateTime, VmNativeCallError> {
    if let Some(handle) = val.as_object_handle().map(ObjectHandle)
        && let Ok(payload) = runtime.native_payload::<TemporalPayload>(handle)
        && let Some(pdt) = payload.as_plain_date_time()
    {
        return Ok(pdt.clone());
    }
    let s = to_string_arg(&[val], 0, runtime)?;
    temporal_rs::PlainDateTime::from_utf8(s.as_bytes()).map_err(|e| temporal_err(e, runtime))
}

// ── Constructor ─────────────────────────────────────────────────────

/// §12.2.1 new Temporal.PlainDateTime ( y, m, d [, h, min, s, ms, us, ns ] )
/// <https://tc39.es/proposal-temporal/#sec-temporal.plaindatetime>
fn pdt_constructor(
    _this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let year = to_integer_or_zero(args, 0, runtime)? as i32;
    let month = to_integer_or_zero(args, 1, runtime)? as u8;
    let day = to_integer_or_zero(args, 2, runtime)? as u8;
    let hour = to_integer_or_zero(args, 3, runtime)? as u8;
    let minute = to_integer_or_zero(args, 4, runtime)? as u8;
    let second = to_integer_or_zero(args, 5, runtime)? as u8;
    let millisecond = to_integer_or_zero(args, 6, runtime)? as u16;
    let microsecond = to_integer_or_zero(args, 7, runtime)? as u16;
    let nanosecond = to_integer_or_zero(args, 8, runtime)? as u16;

    let pdt = temporal_rs::PlainDateTime::new(
        year,
        month,
        day,
        hour,
        minute,
        second,
        millisecond,
        microsecond,
        nanosecond,
        temporal_rs::Calendar::default(),
    )
    .map_err(|e| temporal_err(e, runtime))?;
    Ok(wrap_plain_date_time(pdt, runtime))
}

// ── Static methods ──────────────────────────────────────────────────

/// §12.2.2.1 Temporal.PlainDateTime.from ( item )
/// <https://tc39.es/proposal-temporal/#sec-temporal.plaindatetime.from>
fn pdt_from(
    _this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let val = args.first().copied().unwrap_or(RegisterValue::undefined());
    let pdt = to_plain_date_time(val, runtime)?;
    Ok(wrap_plain_date_time(pdt, runtime))
}

/// §12.2.2.2 Temporal.PlainDateTime.compare ( one, two )
/// <https://tc39.es/proposal-temporal/#sec-temporal.plaindatetime.compare>
fn pdt_compare(
    _this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let one = args.first().copied().unwrap_or(RegisterValue::undefined());
    let two = args.get(1).copied().unwrap_or(RegisterValue::undefined());
    let a = to_plain_date_time(one, runtime)?;
    let b = to_plain_date_time(two, runtime)?;
    let result = match a.compare_iso(&b) {
        Ordering::Less => -1i32,
        Ordering::Equal => 0,
        Ordering::Greater => 1,
    };
    Ok(RegisterValue::from_i32(result))
}

// ── Date getters ────────────────────────────────────────────────────

macro_rules! pdt_getter_num {
    ($name:ident, $method:ident) => {
        fn $name(
            this: &RegisterValue,
            _args: &[RegisterValue],
            runtime: &mut crate::interpreter::RuntimeState,
        ) -> Result<RegisterValue, VmNativeCallError> {
            let pdt = require_plain_date_time(this, runtime)?;
            Ok(RegisterValue::from_number(pdt.$method() as f64))
        }
    };
}

/// §12.2.3.2 get Temporal.PlainDateTime.prototype.calendarId
/// <https://tc39.es/proposal-temporal/#sec-get-temporal.plaindatetime.prototype.calendarid>
fn pdt_calendar_id(
    this: &RegisterValue,
    _args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let pdt = require_plain_date_time(this, runtime)?;
    let id = pdt.calendar().identifier();
    let handle = runtime.alloc_string(id.to_string());
    Ok(RegisterValue::from_object_handle(handle.0))
}

pdt_getter_num!(pdt_year, year);
pdt_getter_num!(pdt_month, month);

/// §12.2.3.5 get Temporal.PlainDateTime.prototype.monthCode
/// <https://tc39.es/proposal-temporal/#sec-get-temporal.plaindatetime.prototype.monthcode>
fn pdt_month_code(
    this: &RegisterValue,
    _args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let pdt = require_plain_date_time(this, runtime)?;
    let code = pdt.month_code().as_str().to_string();
    let handle = runtime.alloc_string(code);
    Ok(RegisterValue::from_object_handle(handle.0))
}

pdt_getter_num!(pdt_day, day);
pdt_getter_num!(pdt_day_of_week, day_of_week);
pdt_getter_num!(pdt_day_of_year, day_of_year);

/// §12.2.3.9 get Temporal.PlainDateTime.prototype.weekOfYear
/// <https://tc39.es/proposal-temporal/#sec-get-temporal.plaindatetime.prototype.weekofyear>
fn pdt_week_of_year(
    this: &RegisterValue,
    _args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let pdt = require_plain_date_time(this, runtime)?;
    match pdt.week_of_year() {
        Some(w) => Ok(RegisterValue::from_number(w as f64)),
        None => Ok(RegisterValue::undefined()),
    }
}

/// §12.2.3.10 get Temporal.PlainDateTime.prototype.yearOfWeek
/// <https://tc39.es/proposal-temporal/#sec-get-temporal.plaindatetime.prototype.yearofweek>
fn pdt_year_of_week(
    this: &RegisterValue,
    _args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let pdt = require_plain_date_time(this, runtime)?;
    match pdt.year_of_week() {
        Some(y) => Ok(RegisterValue::from_number(y as f64)),
        None => Ok(RegisterValue::undefined()),
    }
}

pdt_getter_num!(pdt_days_in_week, days_in_week);
pdt_getter_num!(pdt_days_in_month, days_in_month);
pdt_getter_num!(pdt_days_in_year, days_in_year);
pdt_getter_num!(pdt_months_in_year, months_in_year);

/// §12.2.3.15 get Temporal.PlainDateTime.prototype.inLeapYear
/// <https://tc39.es/proposal-temporal/#sec-get-temporal.plaindatetime.prototype.inleapyear>
fn pdt_in_leap_year(
    this: &RegisterValue,
    _args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let pdt = require_plain_date_time(this, runtime)?;
    Ok(RegisterValue::from_bool(pdt.in_leap_year()))
}

// ── Time getters ────────────────────────────────────────────────────

pdt_getter_num!(pdt_hour, hour);
pdt_getter_num!(pdt_minute, minute);
pdt_getter_num!(pdt_second, second);
pdt_getter_num!(pdt_millisecond, millisecond);
pdt_getter_num!(pdt_microsecond, microsecond);
pdt_getter_num!(pdt_nanosecond, nanosecond);

// ── Prototype methods ───────────────────────────────────────────────

/// §12.2.3.26 Temporal.PlainDateTime.prototype.add ( duration )
/// <https://tc39.es/proposal-temporal/#sec-temporal.plaindatetime.prototype.add>
fn pdt_add(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let pdt = require_plain_date_time(this, runtime)?;
    let dur_val = args.first().copied().unwrap_or(RegisterValue::undefined());
    let dur = to_duration(dur_val, runtime)?;
    let result = pdt.add(&dur, None).map_err(|e| temporal_err(e, runtime))?;
    Ok(wrap_plain_date_time(result, runtime))
}

/// §12.2.3.27 Temporal.PlainDateTime.prototype.subtract ( duration )
/// <https://tc39.es/proposal-temporal/#sec-temporal.plaindatetime.prototype.subtract>
fn pdt_subtract(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let pdt = require_plain_date_time(this, runtime)?;
    let dur_val = args.first().copied().unwrap_or(RegisterValue::undefined());
    let dur = to_duration(dur_val, runtime)?;
    let result = pdt
        .subtract(&dur, None)
        .map_err(|e| temporal_err(e, runtime))?;
    Ok(wrap_plain_date_time(result, runtime))
}

/// §12.2.3.31 Temporal.PlainDateTime.prototype.equals ( other )
/// <https://tc39.es/proposal-temporal/#sec-temporal.plaindatetime.prototype.equals>
fn pdt_equals(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let pdt = require_plain_date_time(this, runtime)?;
    let other_val = args.first().copied().unwrap_or(RegisterValue::undefined());
    let other = to_plain_date_time(other_val, runtime)?;
    Ok(RegisterValue::from_bool(
        pdt.compare_iso(&other) == Ordering::Equal,
    ))
}

/// §12.2.3.32 Temporal.PlainDateTime.prototype.toString ( [ options ] )
/// <https://tc39.es/proposal-temporal/#sec-temporal.plaindatetime.prototype.tostring>
fn pdt_to_string(
    this: &RegisterValue,
    _args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let pdt = require_plain_date_time(this, runtime)?;
    let text = pdt
        .to_ixdtf_string(
            temporal_rs::options::ToStringRoundingOptions::default(),
            temporal_rs::options::DisplayCalendar::Auto,
        )
        .map_err(|e| temporal_err(e, runtime))?;
    let handle = runtime.alloc_string(text);
    Ok(RegisterValue::from_object_handle(handle.0))
}

/// §12.2.3.33 Temporal.PlainDateTime.prototype.toJSON ( )
/// <https://tc39.es/proposal-temporal/#sec-temporal.plaindatetime.prototype.tojson>
fn pdt_to_json(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    pdt_to_string(this, args, runtime)
}

/// §12.2.3.24 Temporal.PlainDateTime.prototype.toPlainDate ( )
/// <https://tc39.es/proposal-temporal/#sec-temporal.plaindatetime.prototype.toplaindate>
fn pdt_to_plain_date(
    this: &RegisterValue,
    _args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let pdt = require_plain_date_time(this, runtime)?;
    let pd = pdt.to_plain_date();
    let proto = runtime.intrinsics().temporal_plain_date_prototype();
    let handle = construct_temporal(TemporalPayload::PlainDate(pd), proto, runtime);
    Ok(RegisterValue::from_object_handle(handle.0))
}

/// §12.2.3.25 Temporal.PlainDateTime.prototype.toPlainTime ( )
/// <https://tc39.es/proposal-temporal/#sec-temporal.plaindatetime.prototype.toplaintime>
fn pdt_to_plain_time(
    this: &RegisterValue,
    _args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let pdt = require_plain_date_time(this, runtime)?;
    let pt = pdt.to_plain_time();
    let proto = runtime.intrinsics().temporal_plain_time_prototype();
    let handle = construct_temporal(TemporalPayload::PlainTime(pt), proto, runtime);
    Ok(RegisterValue::from_object_handle(handle.0))
}
