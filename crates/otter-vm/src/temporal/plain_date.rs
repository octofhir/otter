//! `Temporal.PlainDate` — calendar date `YYYY-MM-DD`.
//!
//! # See also
//! - <https://tc39.es/proposal-temporal/#sec-temporal-plaindate-objects>

#![allow(missing_docs)]

use crate::js_surface::{Attr, MethodSpec};
use crate::native_function::NativeCall;
use crate::temporal::duration::partial_from_object;
use crate::temporal::helpers::{
    arg_or_undef, arg_to_calendar, clamp_to_u8, js_string_value, make_temporal,
    parse_calendar_fields, parse_difference_settings, parse_display_calendar, parse_partial_time,
    require_construct, require_plain_date, str_or_undef, temporal_err, to_integer_with_truncation,
};
use crate::temporal::payload::{JsTemporal, TemporalPayload};
use crate::{NativeCtx, NativeError, Value};

const CLASS: &str = "Temporal.PlainDate";

pub fn construct(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    require_construct(ctx, CLASS)?;
    let heap = ctx.heap();
    let year = to_integer_with_truncation(&arg_or_undef(args, 0), heap, CLASS, "isoYear")? as i32;
    let month_f = to_integer_with_truncation(&arg_or_undef(args, 1), heap, CLASS, "isoMonth")?;
    let day_f = to_integer_with_truncation(&arg_or_undef(args, 2), heap, CLASS, "isoDay")?;
    let calendar = arg_to_calendar(args, 3, heap, CLASS)?;
    let month = clamp_to_u8(month_f, CLASS, "isoMonth")?;
    let day = clamp_to_u8(day_f, CLASS, "isoDay")?;
    let pd = temporal_rs::PlainDate::try_new(year, month, day, calendar)
        .map_err(|e| temporal_err(e, CLASS))?;
    make_temporal(ctx, TemporalPayload::PlainDate(pd))
}

fn from(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let pd = parse_plain_date_arg(&arg_or_undef(args, 0), ctx.heap())?;
    make_temporal(ctx, TemporalPayload::PlainDate(pd))
}

fn compare(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let a = parse_plain_date_arg(&arg_or_undef(args, 0), ctx.heap())?;
    let b = parse_plain_date_arg(&arg_or_undef(args, 1), ctx.heap())?;
    let n = match a.compare_iso(&b) {
        std::cmp::Ordering::Less => -1,
        std::cmp::Ordering::Equal => 0,
        std::cmp::Ordering::Greater => 1,
    };
    Ok(Value::number_i32(n))
}

fn parse_plain_date_arg(
    v: &Value,
    heap: &otter_gc::GcHeap,
) -> Result<temporal_rs::PlainDate, NativeError> {
    if let Some(t) = v.as_temporal(heap) {
        match t.payload_clone(heap) {
            TemporalPayload::PlainDate(v) => Ok(v),
            _ => Err(NativeError::TypeError {
                name: CLASS,
                reason: "argument must be a Temporal.PlainDate".to_string(),
            }),
        }
    } else if let Some(s) = v.as_string(heap) {
        temporal_rs::PlainDate::from_utf8(s.to_lossy_string(heap).as_bytes())
            .map_err(|e| temporal_err(e, CLASS))
    } else {
        Err(NativeError::TypeError {
            name: CLASS,
            reason: "argument must be a Temporal.PlainDate or ISO string".to_string(),
        })
    }
}

pub fn load_property(temporal: JsTemporal, heap: &mut otter_gc::GcHeap, name: &str) -> Value {
    let pd = match temporal.payload_clone(heap) {
        TemporalPayload::PlainDate(v) => v,
        _ => return Value::undefined(),
    };
    match name {
        "year" => Value::number_i32(pd.year()),
        "month" => Value::number_i32(pd.month() as i32),
        "monthCode" => str_or_undef(pd.month_code().as_str(), heap),
        "day" => Value::number_i32(pd.day() as i32),
        "dayOfWeek" => Value::number_i32(pd.day_of_week() as i32),
        "dayOfYear" => Value::number_i32(pd.day_of_year() as i32),
        "weekOfYear" => pd
            .week_of_year()
            .map_or(Value::undefined(), |w| Value::number_i32(w as i32)),
        "yearOfWeek" => pd.year_of_week().map_or(Value::undefined(), Value::number_i32),
        "daysInWeek" => Value::number_i32(pd.days_in_week() as i32),
        "daysInMonth" => Value::number_i32(pd.days_in_month() as i32),
        "daysInYear" => Value::number_i32(pd.days_in_year() as i32),
        "monthsInYear" => Value::number_i32(pd.months_in_year() as i32),
        "inLeapYear" => Value::boolean(pd.in_leap_year()),
        "era" => pd
            .era()
            .map_or(Value::undefined(), |era| str_or_undef(era.as_str(), heap)),
        "eraYear" => pd.era_year().map_or(Value::undefined(), Value::number_i32),
        "calendarId" => str_or_undef(pd.calendar().identifier(), heap),
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
    let pd = require_plain_date(ctx)?;
    let display = parse_display_calendar(args, 0, ctx.heap(), CLASS)?;
    let s = pd.to_ixdtf_string(display);
    js_string_value(s, ctx)
}

fn impl_to_json(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    impl_to_string(ctx, args)
}

fn impl_value_of(_ctx: &mut NativeCtx<'_>, _args: &[Value]) -> Result<Value, NativeError> {
    Err(NativeError::TypeError {
        name: CLASS,
        reason: "Temporal.PlainDate has no `.valueOf` — use `compare` or `equals`".to_string(),
    })
}

fn impl_add(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let pd = require_plain_date(ctx)?;
    let dur = duration_arg(&arg_or_undef(args, 0), ctx.heap())?;
    let result = pd.add(&dur, None).map_err(|e| temporal_err(e, CLASS))?;
    make_temporal(ctx, TemporalPayload::PlainDate(result))
}

fn impl_subtract(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let pd = require_plain_date(ctx)?;
    let dur = duration_arg(&arg_or_undef(args, 0), ctx.heap())?;
    let result = pd
        .subtract(&dur, None)
        .map_err(|e| temporal_err(e, CLASS))?;
    make_temporal(ctx, TemporalPayload::PlainDate(result))
}

fn impl_equals(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let pd = require_plain_date(ctx)?;
    let other = parse_plain_date_arg(&arg_or_undef(args, 0), ctx.heap())?;
    Ok(Value::boolean(
        pd.compare_iso(&other) == std::cmp::Ordering::Equal,
    ))
}

fn impl_until(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let pd = require_plain_date(ctx)?;
    let other = parse_plain_date_arg(&arg_or_undef(args, 0), ctx.heap())?;
    let settings = parse_difference_settings(args, 1, ctx.heap(), CLASS)?;
    let result = pd
        .until(&other, settings)
        .map_err(|e| temporal_err(e, CLASS))?;
    make_temporal(ctx, TemporalPayload::Duration(result))
}

fn impl_since(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let pd = require_plain_date(ctx)?;
    let other = parse_plain_date_arg(&arg_or_undef(args, 0), ctx.heap())?;
    let settings = parse_difference_settings(args, 1, ctx.heap(), CLASS)?;
    let result = pd
        .since(&other, settings)
        .map_err(|e| temporal_err(e, CLASS))?;
    make_temporal(ctx, TemporalPayload::Duration(result))
}

fn impl_with(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let pd = require_plain_date(ctx)?;
    let Some(obj) = arg_or_undef(args, 0).as_object() else {
        return Err(NativeError::TypeError {
            name: CLASS,
            reason: "first argument must be an object".to_string(),
        });
    };
    let fields = parse_calendar_fields(obj, ctx.heap(), CLASS)?;
    let result = pd.with(fields, None).map_err(|e| temporal_err(e, CLASS))?;
    make_temporal(ctx, TemporalPayload::PlainDate(result))
}

fn impl_with_calendar(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let pd = require_plain_date(ctx)?;
    let Some(js) = arg_or_undef(args, 0).as_string(ctx.heap()) else {
        return Err(NativeError::TypeError {
            name: CLASS,
            reason: "calendar identifier must be a string".to_string(),
        });
    };
    let s = js.to_lossy_string(ctx.heap());
    let calendar =
        temporal_rs::Calendar::try_from_utf8(s.as_bytes()).map_err(|e| temporal_err(e, CLASS))?;
    let result = pd.with_calendar(calendar);
    make_temporal(ctx, TemporalPayload::PlainDate(result))
}

fn impl_to_plain_date_time(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let pd = require_plain_date(ctx)?;
    let v = arg_or_undef(args, 0);
    let time = if v.is_undefined() {
        None
    } else if let Some(t) = v.as_temporal(ctx.heap()) {
        match t.payload_clone(ctx.heap()) {
            TemporalPayload::PlainTime(pt) => Some(pt),
            _ => {
                return Err(NativeError::TypeError {
                    name: CLASS,
                    reason: "must be a Temporal.PlainTime or partial-time object".to_string(),
                });
            }
        }
    } else if let Some(obj) = v.as_object() {
        let partial = parse_partial_time(obj, ctx.heap(), CLASS)?;
        let pt = temporal_rs::PlainTime::default()
            .with(partial, None)
            .map_err(|e| temporal_err(e, CLASS))?;
        Some(pt)
    } else {
        return Err(NativeError::TypeError {
            name: CLASS,
            reason: "first argument must be an object or undefined".to_string(),
        });
    };
    let pdt = pd
        .to_plain_date_time(time)
        .map_err(|e| temporal_err(e, CLASS))?;
    make_temporal(ctx, TemporalPayload::PlainDateTime(pdt))
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

pub static PLAIN_DATE_PROTOTYPE_METHODS: &[MethodSpec] = &[
    method("toString", 0, impl_to_string),
    method("toJSON", 0, impl_to_json),
    method("valueOf", 0, impl_value_of),
    method("add", 1, impl_add),
    method("subtract", 1, impl_subtract),
    method("equals", 1, impl_equals),
    method("until", 1, impl_until),
    method("since", 1, impl_since),
    method("with", 1, impl_with),
    method("withCalendar", 1, impl_with_calendar),
    method("toPlainDateTime", 0, impl_to_plain_date_time),
];

otter_macros::couch! {
    name = "PlainDate",
    feature = CORE,
    intrinsic = PlainDateIntrinsic,
    constructor = (length = 3, call = construct),
    statics = {
        "from"    / 1 => from,
        "compare" / 2 => compare,
    },
    prototype = {
        method_specs = [PLAIN_DATE_PROTOTYPE_METHODS],
    },
    install_on = crate::temporal::native_dispatch::temporal_host,
}
