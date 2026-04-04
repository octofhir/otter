//! Temporal.ZonedDateTime constructor and prototype intrinsics.
//!
//! §6.2 Temporal.ZonedDateTime
//! <https://tc39.es/proposal-temporal/#sec-temporal-zoneddatetime-objects>

use std::cmp::Ordering;

use crate::descriptors::{
    JsClassDescriptor, NativeBindingDescriptor, NativeBindingTarget, NativeFunctionDescriptor,
    VmNativeCallError,
};
use crate::object::ObjectHandle;
use crate::value::RegisterValue;

use super::duration::to_duration;
use super::helpers::{self, temporal_err, to_bigint_i128, to_string_arg};
use super::payload::{TemporalPayload, construct_temporal, require_temporal_payload};

fn tz_provider() -> &'static impl temporal_rs::provider::TimeZoneProvider {
    &*temporal_rs::provider::COMPILED_TZ_PROVIDER
}

// ── Descriptor ──────────────────────────────────────────────────────

pub fn zoned_date_time_class_descriptor() -> JsClassDescriptor {
    JsClassDescriptor::new("ZonedDateTime")
        .with_constructor(NativeFunctionDescriptor::constructor(
            "ZonedDateTime",
            2,
            zdt_constructor,
        ))
        .with_binding(stat("from", 1, zdt_from))
        .with_binding(stat("compare", 2, zdt_compare))
        // Getters
        .with_binding(getter("calendarId", zdt_calendar_id))
        .with_binding(getter("timeZoneId", zdt_time_zone_id))
        .with_binding(getter("year", zdt_year))
        .with_binding(getter("month", zdt_month))
        .with_binding(getter("monthCode", zdt_month_code))
        .with_binding(getter("day", zdt_day))
        .with_binding(getter("hour", zdt_hour))
        .with_binding(getter("minute", zdt_minute))
        .with_binding(getter("second", zdt_second))
        .with_binding(getter("millisecond", zdt_millisecond))
        .with_binding(getter("microsecond", zdt_microsecond))
        .with_binding(getter("nanosecond", zdt_nanosecond))
        .with_binding(getter("epochMilliseconds", zdt_epoch_ms))
        .with_binding(getter("epochNanoseconds", zdt_epoch_ns))
        .with_binding(getter("dayOfWeek", zdt_day_of_week))
        .with_binding(getter("dayOfYear", zdt_day_of_year))
        .with_binding(getter("weekOfYear", zdt_week_of_year))
        .with_binding(getter("yearOfWeek", zdt_year_of_week))
        .with_binding(getter("daysInWeek", zdt_days_in_week))
        .with_binding(getter("daysInMonth", zdt_days_in_month))
        .with_binding(getter("daysInYear", zdt_days_in_year))
        .with_binding(getter("monthsInYear", zdt_months_in_year))
        .with_binding(getter("inLeapYear", zdt_in_leap_year))
        .with_binding(getter("offset", zdt_offset))
        .with_binding(getter("offsetNanoseconds", zdt_offset_nanoseconds))
        // Methods
        .with_binding(proto("add", 1, zdt_add))
        .with_binding(proto("subtract", 1, zdt_subtract))
        .with_binding(proto("equals", 1, zdt_equals))
        .with_binding(proto("toString", 0, zdt_to_string))
        .with_binding(proto("toJSON", 0, zdt_to_json))
        .with_binding(proto("toInstant", 0, zdt_to_instant))
        .with_binding(proto("toPlainDate", 0, zdt_to_plain_date))
        .with_binding(proto("toPlainTime", 0, zdt_to_plain_time))
        .with_binding(proto("toPlainDateTime", 0, zdt_to_plain_date_time))
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

fn require_zoned_date_time(
    this: &RegisterValue,
    runtime: &crate::interpreter::RuntimeState,
) -> Result<temporal_rs::ZonedDateTime, VmNativeCallError> {
    let payload = require_temporal_payload(this, runtime)
        .map_err(|_| VmNativeCallError::Internal("expected Temporal.ZonedDateTime".into()))?;
    payload
        .as_zoned_date_time()
        .cloned()
        .ok_or_else(|| VmNativeCallError::Internal("expected Temporal.ZonedDateTime".into()))
}

fn wrap_zoned_date_time(
    zdt: temporal_rs::ZonedDateTime,
    runtime: &mut crate::interpreter::RuntimeState,
) -> RegisterValue {
    let proto = runtime.intrinsics().temporal_zoned_date_time_prototype();
    let handle = construct_temporal(TemporalPayload::ZonedDateTime(zdt), proto, runtime);
    RegisterValue::from_object_handle(handle.0)
}

/// Extracts a ZonedDateTime from an argument — accepts objects or ISO strings.
fn to_zoned_date_time(
    val: RegisterValue,
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<temporal_rs::ZonedDateTime, VmNativeCallError> {
    if let Some(handle) = val.as_object_handle().map(ObjectHandle)
        && let Ok(payload) = runtime.native_payload::<TemporalPayload>(handle)
        && let Some(zdt) = payload.as_zoned_date_time()
    {
        return Ok(zdt.clone());
    }
    let s = to_string_arg(&[val], 0, runtime)?;
    temporal_rs::ZonedDateTime::from_utf8_with_provider(
        s.as_bytes(),
        temporal_rs::options::Disambiguation::default(),
        temporal_rs::options::OffsetDisambiguation::Reject,
        tz_provider(),
    )
    .map_err(|e| temporal_err(e, runtime))
}

// ── Constructor ─────────────────────────────────────────────────────

/// §6.2.1 new Temporal.ZonedDateTime ( epochNanoseconds, timeZoneId [, calendar ] )
/// <https://tc39.es/proposal-temporal/#sec-temporal.zoneddatetime>
fn zdt_constructor(
    _this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let ns = to_bigint_i128(args, 0, runtime)?;
    let tz_str = to_string_arg(args, 1, runtime)?;
    let tz = temporal_rs::TimeZone::try_from_identifier_str_with_provider(&tz_str, tz_provider())
        .map_err(|e| temporal_err(e, runtime))?;
    let zdt = temporal_rs::ZonedDateTime::try_new_iso_with_provider(ns, tz, tz_provider())
        .map_err(|e| temporal_err(e, runtime))?;
    Ok(wrap_zoned_date_time(zdt, runtime))
}

// ── Static methods ──────────────────────────────────────────────────

/// §6.2.2.1 Temporal.ZonedDateTime.from ( item )
/// <https://tc39.es/proposal-temporal/#sec-temporal.zoneddatetime.from>
fn zdt_from(
    _this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let val = args.first().copied().unwrap_or(RegisterValue::undefined());
    let zdt = to_zoned_date_time(val, runtime)?;
    Ok(wrap_zoned_date_time(zdt, runtime))
}

/// §6.2.2.2 Temporal.ZonedDateTime.compare ( one, two )
/// <https://tc39.es/proposal-temporal/#sec-temporal.zoneddatetime.compare>
fn zdt_compare(
    _this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let one = args.first().copied().unwrap_or(RegisterValue::undefined());
    let two = args.get(1).copied().unwrap_or(RegisterValue::undefined());
    let a = to_zoned_date_time(one, runtime)?;
    let b = to_zoned_date_time(two, runtime)?;
    let result = match a.compare_instant(&b) {
        Ordering::Less => -1i32,
        Ordering::Equal => 0,
        Ordering::Greater => 1,
    };
    Ok(RegisterValue::from_i32(result))
}

// ── Getters ─────────────────────────────────────────────────────────

macro_rules! zdt_getter_num {
    ($name:ident, $method:ident) => {
        fn $name(
            this: &RegisterValue,
            _args: &[RegisterValue],
            runtime: &mut crate::interpreter::RuntimeState,
        ) -> Result<RegisterValue, VmNativeCallError> {
            let zdt = require_zoned_date_time(this, runtime)?;
            Ok(RegisterValue::from_number(zdt.$method() as f64))
        }
    };
}

/// §6.2.3.2 get Temporal.ZonedDateTime.prototype.calendarId
/// <https://tc39.es/proposal-temporal/#sec-get-temporal.zoneddatetime.prototype.calendarid>
fn zdt_calendar_id(
    this: &RegisterValue,
    _args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let zdt = require_zoned_date_time(this, runtime)?;
    let id = zdt.calendar().identifier();
    let handle = runtime.alloc_string(id.to_string());
    Ok(RegisterValue::from_object_handle(handle.0))
}

/// §6.2.3.3 get Temporal.ZonedDateTime.prototype.timeZoneId
/// <https://tc39.es/proposal-temporal/#sec-get-temporal.zoneddatetime.prototype.timezoneid>
fn zdt_time_zone_id(
    this: &RegisterValue,
    _args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let zdt = require_zoned_date_time(this, runtime)?;
    let id = zdt
        .time_zone()
        .identifier_with_provider(tz_provider())
        .map_err(|e| temporal_err(e, runtime))?;
    let handle = runtime.alloc_string(id);
    Ok(RegisterValue::from_object_handle(handle.0))
}

zdt_getter_num!(zdt_year, year);
zdt_getter_num!(zdt_month, month);

/// §6.2.3.6 get Temporal.ZonedDateTime.prototype.monthCode
/// <https://tc39.es/proposal-temporal/#sec-get-temporal.zoneddatetime.prototype.monthcode>
fn zdt_month_code(
    this: &RegisterValue,
    _args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let zdt = require_zoned_date_time(this, runtime)?;
    let code = zdt.month_code().as_str().to_string();
    let handle = runtime.alloc_string(code);
    Ok(RegisterValue::from_object_handle(handle.0))
}

zdt_getter_num!(zdt_day, day);
zdt_getter_num!(zdt_hour, hour);
zdt_getter_num!(zdt_minute, minute);
zdt_getter_num!(zdt_second, second);
zdt_getter_num!(zdt_millisecond, millisecond);
zdt_getter_num!(zdt_microsecond, microsecond);
zdt_getter_num!(zdt_nanosecond, nanosecond);

/// §6.2.3.26 get Temporal.ZonedDateTime.prototype.epochMilliseconds
/// <https://tc39.es/proposal-temporal/#sec-get-temporal.zoneddatetime.prototype.epochmilliseconds>
fn zdt_epoch_ms(
    this: &RegisterValue,
    _args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let zdt = require_zoned_date_time(this, runtime)?;
    Ok(RegisterValue::from_number(zdt.epoch_milliseconds() as f64))
}

/// §6.2.3.27 get Temporal.ZonedDateTime.prototype.epochNanoseconds
/// <https://tc39.es/proposal-temporal/#sec-get-temporal.zoneddatetime.prototype.epochnanoseconds>
fn zdt_epoch_ns(
    this: &RegisterValue,
    _args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let zdt = require_zoned_date_time(this, runtime)?;
    let ns = zdt.epoch_nanoseconds().0;
    let handle = runtime.alloc_bigint(&ns.to_string());
    Ok(RegisterValue::from_bigint_handle(handle.0))
}

zdt_getter_num!(zdt_day_of_week, day_of_week);
zdt_getter_num!(zdt_day_of_year, day_of_year);

/// §6.2.3.17 get Temporal.ZonedDateTime.prototype.weekOfYear
/// <https://tc39.es/proposal-temporal/#sec-get-temporal.zoneddatetime.prototype.weekofyear>
fn zdt_week_of_year(
    this: &RegisterValue,
    _args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let zdt = require_zoned_date_time(this, runtime)?;
    match zdt.week_of_year() {
        Some(w) => Ok(RegisterValue::from_number(w as f64)),
        None => Ok(RegisterValue::undefined()),
    }
}

/// §6.2.3.18 get Temporal.ZonedDateTime.prototype.yearOfWeek
/// <https://tc39.es/proposal-temporal/#sec-get-temporal.zoneddatetime.prototype.yearofweek>
fn zdt_year_of_week(
    this: &RegisterValue,
    _args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let zdt = require_zoned_date_time(this, runtime)?;
    match zdt.year_of_week() {
        Some(y) => Ok(RegisterValue::from_number(y as f64)),
        None => Ok(RegisterValue::undefined()),
    }
}

zdt_getter_num!(zdt_days_in_week, days_in_week);
zdt_getter_num!(zdt_days_in_month, days_in_month);
zdt_getter_num!(zdt_days_in_year, days_in_year);
zdt_getter_num!(zdt_months_in_year, months_in_year);

/// §6.2.3.23 get Temporal.ZonedDateTime.prototype.inLeapYear
/// <https://tc39.es/proposal-temporal/#sec-get-temporal.zoneddatetime.prototype.inleapyear>
fn zdt_in_leap_year(
    this: &RegisterValue,
    _args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let zdt = require_zoned_date_time(this, runtime)?;
    Ok(RegisterValue::from_bool(zdt.in_leap_year()))
}

/// §6.2.3.24 get Temporal.ZonedDateTime.prototype.offset
/// <https://tc39.es/proposal-temporal/#sec-get-temporal.zoneddatetime.prototype.offset>
fn zdt_offset(
    this: &RegisterValue,
    _args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let zdt = require_zoned_date_time(this, runtime)?;
    let offset = zdt.offset();
    let handle = runtime.alloc_string(offset);
    Ok(RegisterValue::from_object_handle(handle.0))
}

/// §6.2.3.25 get Temporal.ZonedDateTime.prototype.offsetNanoseconds
/// <https://tc39.es/proposal-temporal/#sec-get-temporal.zoneddatetime.prototype.offsetnanoseconds>
fn zdt_offset_nanoseconds(
    this: &RegisterValue,
    _args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let zdt = require_zoned_date_time(this, runtime)?;
    Ok(RegisterValue::from_number(zdt.offset_nanoseconds() as f64))
}

// ── Prototype methods ───────────────────────────────────────────────

/// §6.2.3.34 Temporal.ZonedDateTime.prototype.add ( duration )
/// <https://tc39.es/proposal-temporal/#sec-temporal.zoneddatetime.prototype.add>
fn zdt_add(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let zdt = require_zoned_date_time(this, runtime)?;
    let dur_val = args.first().copied().unwrap_or(RegisterValue::undefined());
    let dur = to_duration(dur_val, runtime)?;
    let result = zdt
        .add_with_provider(&dur, None, tz_provider())
        .map_err(|e| temporal_err(e, runtime))?;
    Ok(wrap_zoned_date_time(result, runtime))
}

/// §6.2.3.35 Temporal.ZonedDateTime.prototype.subtract ( duration )
/// <https://tc39.es/proposal-temporal/#sec-temporal.zoneddatetime.prototype.subtract>
fn zdt_subtract(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let zdt = require_zoned_date_time(this, runtime)?;
    let dur_val = args.first().copied().unwrap_or(RegisterValue::undefined());
    let dur = to_duration(dur_val, runtime)?;
    let result = zdt
        .subtract_with_provider(&dur, None, tz_provider())
        .map_err(|e| temporal_err(e, runtime))?;
    Ok(wrap_zoned_date_time(result, runtime))
}

/// §6.2.3.42 Temporal.ZonedDateTime.prototype.equals ( other )
/// <https://tc39.es/proposal-temporal/#sec-temporal.zoneddatetime.prototype.equals>
fn zdt_equals(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let zdt = require_zoned_date_time(this, runtime)?;
    let other_val = args.first().copied().unwrap_or(RegisterValue::undefined());
    let other = to_zoned_date_time(other_val, runtime)?;
    let eq = zdt
        .equals_with_provider(&other, tz_provider())
        .map_err(|e| temporal_err(e, runtime))?;
    Ok(RegisterValue::from_bool(eq))
}

/// §6.2.3.45 Temporal.ZonedDateTime.prototype.toString ( [ options ] )
/// <https://tc39.es/proposal-temporal/#sec-temporal.zoneddatetime.prototype.tostring>
fn zdt_to_string(
    this: &RegisterValue,
    _args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let zdt = require_zoned_date_time(this, runtime)?;
    let text = zdt
        .to_ixdtf_string_with_provider(
            temporal_rs::options::DisplayOffset::default(),
            temporal_rs::options::DisplayTimeZone::default(),
            temporal_rs::options::DisplayCalendar::Auto,
            temporal_rs::options::ToStringRoundingOptions::default(),
            tz_provider(),
        )
        .map_err(|e| temporal_err(e, runtime))?;
    let handle = runtime.alloc_string(text);
    Ok(RegisterValue::from_object_handle(handle.0))
}

/// §6.2.3.46 Temporal.ZonedDateTime.prototype.toJSON ( )
/// <https://tc39.es/proposal-temporal/#sec-temporal.zoneddatetime.prototype.tojson>
fn zdt_to_json(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    zdt_to_string(this, args, runtime)
}

/// §6.2.3.47 Temporal.ZonedDateTime.prototype.toInstant ( )
/// <https://tc39.es/proposal-temporal/#sec-temporal.zoneddatetime.prototype.toinstant>
fn zdt_to_instant(
    this: &RegisterValue,
    _args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let zdt = require_zoned_date_time(this, runtime)?;
    let instant = zdt.to_instant();
    let proto = runtime.intrinsics().temporal_instant_prototype();
    let handle = construct_temporal(TemporalPayload::Instant(instant), proto, runtime);
    Ok(RegisterValue::from_object_handle(handle.0))
}

/// §6.2.3.48 Temporal.ZonedDateTime.prototype.toPlainDate ( )
/// <https://tc39.es/proposal-temporal/#sec-temporal.zoneddatetime.prototype.toplaindate>
fn zdt_to_plain_date(
    this: &RegisterValue,
    _args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let zdt = require_zoned_date_time(this, runtime)?;
    let pd = zdt.to_plain_date();
    let proto = runtime.intrinsics().temporal_plain_date_prototype();
    let handle = construct_temporal(TemporalPayload::PlainDate(pd), proto, runtime);
    Ok(RegisterValue::from_object_handle(handle.0))
}

/// §6.2.3.49 Temporal.ZonedDateTime.prototype.toPlainTime ( )
/// <https://tc39.es/proposal-temporal/#sec-temporal.zoneddatetime.prototype.toplaintime>
fn zdt_to_plain_time(
    this: &RegisterValue,
    _args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let zdt = require_zoned_date_time(this, runtime)?;
    let pt = zdt.to_plain_time();
    let proto = runtime.intrinsics().temporal_plain_time_prototype();
    let handle = construct_temporal(TemporalPayload::PlainTime(pt), proto, runtime);
    Ok(RegisterValue::from_object_handle(handle.0))
}

/// §6.2.3.50 Temporal.ZonedDateTime.prototype.toPlainDateTime ( )
/// <https://tc39.es/proposal-temporal/#sec-temporal.zoneddatetime.prototype.toplaindatetime>
fn zdt_to_plain_date_time(
    this: &RegisterValue,
    _args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let zdt = require_zoned_date_time(this, runtime)?;
    let pdt = zdt.to_plain_date_time();
    let proto = runtime.intrinsics().temporal_plain_date_time_prototype();
    let handle = construct_temporal(TemporalPayload::PlainDateTime(pdt), proto, runtime);
    Ok(RegisterValue::from_object_handle(handle.0))
}
