//! `Temporal.PlainDateTime` — combined wall-clock date + time without a zone.
//!
//! # See also
//! - <https://tc39.es/proposal-temporal/#sec-temporal-plaindatetime-objects>

#![allow(missing_docs)]

use crate::js_surface::{Attr, MethodSpec};
use crate::native_function::NativeCall;
use crate::temporal::duration::partial_from_object;
use crate::temporal::helpers::{
    arg_or_undef, arg_to_calendar, clamp_to_u8, clamp_to_u16, js_string_value, make_temporal,
    opt_integer_with_truncation, parse_date_time_fields, parse_difference_settings,
    parse_display_calendar, parse_partial_time, parse_rounding_options, require_construct,
    require_plain_date_time, str_or_undef, temporal_err, to_integer_with_truncation,
};
use crate::temporal::payload::{JsTemporal, TemporalPayload};
use crate::{NativeCtx, NativeError, Value};

const CLASS: &str = "Temporal.PlainDateTime";

pub fn construct(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    require_construct(ctx, CLASS)?;
    let heap = ctx.heap();
    let year = to_integer_with_truncation(&arg_or_undef(args, 0), heap, CLASS, "isoYear")? as i32;
    let month = clamp_to_u8(
        to_integer_with_truncation(&arg_or_undef(args, 1), heap, CLASS, "isoMonth")?,
        CLASS,
        "isoMonth",
    )?;
    let day = clamp_to_u8(
        to_integer_with_truncation(&arg_or_undef(args, 2), heap, CLASS, "isoDay")?,
        CLASS,
        "isoDay",
    )?;
    let hour = clamp_to_u8(
        opt_integer_with_truncation(args, 3, heap, CLASS, "hour")?,
        CLASS,
        "hour",
    )?;
    let minute = clamp_to_u8(
        opt_integer_with_truncation(args, 4, heap, CLASS, "minute")?,
        CLASS,
        "minute",
    )?;
    let second = clamp_to_u8(
        opt_integer_with_truncation(args, 5, heap, CLASS, "second")?,
        CLASS,
        "second",
    )?;
    let millisecond = clamp_to_u16(
        opt_integer_with_truncation(args, 6, heap, CLASS, "millisecond")?,
        CLASS,
        "millisecond",
    )?;
    let microsecond = clamp_to_u16(
        opt_integer_with_truncation(args, 7, heap, CLASS, "microsecond")?,
        CLASS,
        "microsecond",
    )?;
    let nanosecond = clamp_to_u16(
        opt_integer_with_truncation(args, 8, heap, CLASS, "nanosecond")?,
        CLASS,
        "nanosecond",
    )?;
    let calendar = arg_to_calendar(args, 9, heap, CLASS)?;
    let pdt = temporal_rs::PlainDateTime::try_new(
        year,
        month,
        day,
        hour,
        minute,
        second,
        millisecond,
        microsecond,
        nanosecond,
        calendar,
    )
    .map_err(|e| temporal_err(e, CLASS))?;
    make_temporal(ctx, TemporalPayload::PlainDateTime(pdt))
}

fn from(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let pdt = parse_plain_date_time_arg(&arg_or_undef(args, 0), ctx.heap())?;
    make_temporal(ctx, TemporalPayload::PlainDateTime(pdt))
}

fn compare(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let a = parse_plain_date_time_arg(&arg_or_undef(args, 0), ctx.heap())?;
    let b = parse_plain_date_time_arg(&arg_or_undef(args, 1), ctx.heap())?;
    let n = match temporal_rs::PlainDateTime::compare_iso(&a, &b) {
        std::cmp::Ordering::Less => -1,
        std::cmp::Ordering::Equal => 0,
        std::cmp::Ordering::Greater => 1,
    };
    Ok(Value::number_i32(n))
}

fn parse_plain_date_time_arg(
    v: &Value,
    heap: &otter_gc::GcHeap,
) -> Result<temporal_rs::PlainDateTime, NativeError> {
    if let Some(t) = v.as_temporal(heap) {
        match t.payload_clone(heap) {
            TemporalPayload::PlainDateTime(v) => Ok(v),
            _ => Err(NativeError::TypeError {
                name: CLASS,
                reason: "argument must be a Temporal.PlainDateTime".to_string(),
            }),
        }
    } else if let Some(s) = v.as_string(heap) {
        temporal_rs::PlainDateTime::from_utf8(s.to_lossy_string(heap).as_bytes())
            .map_err(|e| temporal_err(e, CLASS))
    } else {
        Err(NativeError::TypeError {
            name: CLASS,
            reason: "argument must be a Temporal.PlainDateTime or ISO string".to_string(),
        })
    }
}

pub fn load_property(temporal: JsTemporal, heap: &mut otter_gc::GcHeap, name: &str) -> Value {
    let pdt = match temporal.payload_clone(heap) {
        TemporalPayload::PlainDateTime(v) => v,
        _ => return Value::undefined(),
    };
    match name {
        "year" => Value::number_i32(pdt.year()),
        "month" => Value::number_i32(pdt.month() as i32),
        "monthCode" => str_or_undef(pdt.month_code().as_str(), heap),
        "day" => Value::number_i32(pdt.day() as i32),
        "hour" => Value::number_i32(pdt.hour() as i32),
        "minute" => Value::number_i32(pdt.minute() as i32),
        "second" => Value::number_i32(pdt.second() as i32),
        "millisecond" => Value::number_i32(pdt.millisecond() as i32),
        "microsecond" => Value::number_i32(pdt.microsecond() as i32),
        "nanosecond" => Value::number_i32(pdt.nanosecond() as i32),
        "dayOfWeek" => Value::number_i32(pdt.day_of_week() as i32),
        "dayOfYear" => Value::number_i32(pdt.day_of_year() as i32),
        "weekOfYear" => pdt
            .week_of_year()
            .map_or(Value::undefined(), |w| Value::number_i32(w as i32)),
        "yearOfWeek" => pdt.year_of_week().map_or(Value::undefined(), Value::number_i32),
        "daysInWeek" => Value::number_i32(pdt.days_in_week() as i32),
        "daysInMonth" => Value::number_i32(pdt.days_in_month() as i32),
        "daysInYear" => Value::number_i32(pdt.days_in_year() as i32),
        "monthsInYear" => Value::number_i32(pdt.months_in_year() as i32),
        "inLeapYear" => Value::boolean(pdt.in_leap_year()),
        "era" => pdt
            .era()
            .map_or(Value::undefined(), |era| str_or_undef(era.as_str(), heap)),
        "eraYear" => pdt.era_year().map_or(Value::undefined(), Value::number_i32),
        "calendarId" => str_or_undef(pdt.calendar().identifier(), heap),
        _ => Value::undefined(),
    }
}

fn duration_arg(v: &Value, heap: &otter_gc::GcHeap) -> Result<temporal_rs::Duration, NativeError> {
    if let Some(t) = v.as_temporal(heap) {
        match t.payload_clone(heap) {
            TemporalPayload::Duration(d) => Ok(d),
            _ => Err(NativeError::TypeError {
                name: CLASS,
                reason: "must be a Temporal.Duration".to_string(),
            }),
        }
    } else if let Some(obj) = v.as_object() {
        partial_from_object(&obj, heap).map_err(|e| temporal_err(e, CLASS))
    } else {
        Err(NativeError::TypeError {
            name: CLASS,
            reason: "must be a Temporal.Duration".to_string(),
        })
    }
}

fn impl_to_string(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let pdt = require_plain_date_time(ctx)?;
    let display = parse_display_calendar(args, 0, ctx.heap(), CLASS)?;
    let s = pdt
        .to_ixdtf_string(temporal_rs::options::ToStringRoundingOptions::default(), display)
        .map_err(|e| temporal_err(e, CLASS))?;
    js_string_value(s, ctx)
}

fn impl_to_json(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    impl_to_string(ctx, args)
}

fn impl_value_of(_ctx: &mut NativeCtx<'_>, _args: &[Value]) -> Result<Value, NativeError> {
    Err(NativeError::TypeError {
        name: CLASS,
        reason: "Temporal.PlainDateTime has no `.valueOf` — use `compare` or `equals`".to_string(),
    })
}

fn impl_add(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let pdt = require_plain_date_time(ctx)?;
    let dur = duration_arg(&arg_or_undef(args, 0), ctx.heap())?;
    let result = pdt.add(&dur, None).map_err(|e| temporal_err(e, CLASS))?;
    make_temporal(ctx, TemporalPayload::PlainDateTime(result))
}

fn impl_subtract(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let pdt = require_plain_date_time(ctx)?;
    let dur = duration_arg(&arg_or_undef(args, 0), ctx.heap())?;
    let result = pdt
        .subtract(&dur, None)
        .map_err(|e| temporal_err(e, CLASS))?;
    make_temporal(ctx, TemporalPayload::PlainDateTime(result))
}

fn impl_equals(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let pdt = require_plain_date_time(ctx)?;
    let other = parse_plain_date_time_arg(&arg_or_undef(args, 0), ctx.heap())?;
    Ok(Value::boolean(pdt == other))
}

fn impl_until(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let pdt = require_plain_date_time(ctx)?;
    let other = parse_plain_date_time_arg(&arg_or_undef(args, 0), ctx.heap())?;
    let settings = parse_difference_settings(args, 1, ctx.heap(), CLASS)?;
    let result = pdt
        .until(&other, settings)
        .map_err(|e| temporal_err(e, CLASS))?;
    make_temporal(ctx, TemporalPayload::Duration(result))
}

fn impl_since(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let pdt = require_plain_date_time(ctx)?;
    let other = parse_plain_date_time_arg(&arg_or_undef(args, 0), ctx.heap())?;
    let settings = parse_difference_settings(args, 1, ctx.heap(), CLASS)?;
    let result = pdt
        .since(&other, settings)
        .map_err(|e| temporal_err(e, CLASS))?;
    make_temporal(ctx, TemporalPayload::Duration(result))
}

fn impl_round(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let pdt = require_plain_date_time(ctx)?;
    let options = parse_rounding_options(args, 0, ctx.heap(), CLASS)?;
    let result = pdt.round(options).map_err(|e| temporal_err(e, CLASS))?;
    make_temporal(ctx, TemporalPayload::PlainDateTime(result))
}

fn impl_with(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let pdt = require_plain_date_time(ctx)?;
    let Some(obj) = arg_or_undef(args, 0).as_object() else {
        return Err(NativeError::TypeError {
            name: CLASS,
            reason: "first argument must be an object".to_string(),
        });
    };
    let fields = parse_date_time_fields(obj, ctx.heap(), CLASS)?;
    let result = pdt.with(fields, None).map_err(|e| temporal_err(e, CLASS))?;
    make_temporal(ctx, TemporalPayload::PlainDateTime(result))
}

fn impl_with_calendar(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let pdt = require_plain_date_time(ctx)?;
    let Some(js) = arg_or_undef(args, 0).as_string(ctx.heap()) else {
        return Err(NativeError::TypeError {
            name: CLASS,
            reason: "calendar identifier must be a string".to_string(),
        });
    };
    let s = js.to_lossy_string(ctx.heap());
    let calendar =
        temporal_rs::Calendar::try_from_utf8(s.as_bytes()).map_err(|e| temporal_err(e, CLASS))?;
    let result = pdt.with_calendar(calendar);
    make_temporal(ctx, TemporalPayload::PlainDateTime(result))
}

fn impl_with_plain_time(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let pdt = require_plain_date_time(ctx)?;
    let arg = arg_or_undef(args, 0);
    let time = if arg.is_undefined() {
        temporal_rs::PlainTime::default()
    } else if let Some(t) = arg.as_temporal(ctx.heap()) {
        match t.payload_clone(ctx.heap()) {
            TemporalPayload::PlainTime(pt) => pt,
            _ => {
                return Err(NativeError::TypeError {
                    name: CLASS,
                    reason: "must be a Temporal.PlainTime or partial-time object".to_string(),
                });
            }
        }
    } else if let Some(obj) = arg.as_object() {
        let partial = parse_partial_time(obj, ctx.heap(), CLASS)?;
        temporal_rs::PlainTime::default()
            .with(partial, None)
            .map_err(|e| temporal_err(e, CLASS))?
    } else {
        return Err(NativeError::TypeError {
            name: CLASS,
            reason: "must be a Temporal.PlainTime, partial-time object, or undefined".to_string(),
        });
    };
    let pd = pdt.to_plain_date();
    let result = pd
        .to_plain_date_time(Some(time))
        .map_err(|e| temporal_err(e, CLASS))?;
    make_temporal(ctx, TemporalPayload::PlainDateTime(result))
}

fn impl_to_plain_date(ctx: &mut NativeCtx<'_>, _args: &[Value]) -> Result<Value, NativeError> {
    let pdt = require_plain_date_time(ctx)?;
    make_temporal(ctx, TemporalPayload::PlainDate(pdt.to_plain_date()))
}

fn impl_to_plain_time(ctx: &mut NativeCtx<'_>, _args: &[Value]) -> Result<Value, NativeError> {
    let pdt = require_plain_date_time(ctx)?;
    make_temporal(ctx, TemporalPayload::PlainTime(pdt.to_plain_time()))
}

const fn method(
    name: &'static str,
    length: u8,
    call: for<'rt> fn(&mut NativeCtx<'rt>, &[Value]) -> Result<Value, NativeError>,
) -> MethodSpec {
    MethodSpec {
        name,
        length,
        attrs: Attr::builtin_function(),
        call: NativeCall::Static(call),
    }
}

pub static PLAIN_DATE_TIME_PROTOTYPE_METHODS: &[MethodSpec] = &[
    method("toString", 0, impl_to_string),
    method("toJSON", 0, impl_to_json),
    method("valueOf", 0, impl_value_of),
    method("add", 1, impl_add),
    method("subtract", 1, impl_subtract),
    method("equals", 1, impl_equals),
    method("until", 1, impl_until),
    method("since", 1, impl_since),
    method("round", 1, impl_round),
    method("with", 1, impl_with),
    method("withCalendar", 1, impl_with_calendar),
    method("withPlainTime", 0, impl_with_plain_time),
    method("toPlainDate", 0, impl_to_plain_date),
    method("toPlainTime", 0, impl_to_plain_time),
];

otter_macros::couch! {
    name = "PlainDateTime",
    feature = CORE,
    intrinsic = PlainDateTimeIntrinsic,
    constructor = (length = 3, call = construct),
    statics = {
        "from"    / 1 => from,
        "compare" / 2 => compare,
    },
    prototype = {
        method_specs = [PLAIN_DATE_TIME_PROTOTYPE_METHODS],
    },
    install_on = crate::temporal::native_dispatch::temporal_host,
}
