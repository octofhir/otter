//! `Temporal.Duration` — calendar / time difference value.
//!
//! Backed by [`temporal_rs::Duration`].
//!
//! # See also
//! - <https://tc39.es/proposal-temporal/#sec-temporal-duration-objects>

#![allow(missing_docs)]

use std::str::FromStr;

use crate::js_surface::{Attr, MethodSpec};
use crate::native_function::NativeCall;
use crate::object::{self, JsObject};
use crate::temporal::helpers::parse_to_string_rounding_options;
use crate::temporal::helpers::{
    arg_or_undef, js_string_value, make_temporal, opt_integer_if_integral, parse_rounding_options,
    require_construct, require_duration, temporal_err,
};
use crate::temporal::payload::{JsTemporal, TemporalPayload};
use crate::{NativeCtx, NativeError, Value};

const CLASS: &str = "Temporal.Duration";

/// §7.1.1 `Temporal.Duration(years, months, weeks, days, hours,
/// minutes, seconds, milliseconds, microseconds, nanoseconds)`.
pub fn construct(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    require_construct(ctx, CLASS)?;
    let heap = ctx.heap();
    let years = opt_integer_if_integral(args, 0, heap, CLASS, "years")? as i64;
    let months = opt_integer_if_integral(args, 1, heap, CLASS, "months")? as i64;
    let weeks = opt_integer_if_integral(args, 2, heap, CLASS, "weeks")? as i64;
    let days = opt_integer_if_integral(args, 3, heap, CLASS, "days")? as i64;
    let hours = opt_integer_if_integral(args, 4, heap, CLASS, "hours")? as i64;
    let minutes = opt_integer_if_integral(args, 5, heap, CLASS, "minutes")? as i64;
    let seconds = opt_integer_if_integral(args, 6, heap, CLASS, "seconds")? as i64;
    let ms = opt_integer_if_integral(args, 7, heap, CLASS, "milliseconds")? as i64;
    let us = opt_integer_if_integral(args, 8, heap, CLASS, "microseconds")? as i128;
    let ns = opt_integer_if_integral(args, 9, heap, CLASS, "nanoseconds")? as i128;
    let dur = temporal_rs::Duration::new(
        years, months, weeks, days, hours, minutes, seconds, ms, us, ns,
    )
    .map_err(|e| temporal_err(e, CLASS))?;
    make_temporal(ctx, TemporalPayload::Duration(dur))
}

fn from(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let first = arg_or_undef(args, 0);
    let dur = parse_duration_arg(&first, ctx.heap())?;
    make_temporal(ctx, TemporalPayload::Duration(dur))
}

fn compare(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let a = parse_duration_arg(&arg_or_undef(args, 0), ctx.heap())?;
    let b = parse_duration_arg(&arg_or_undef(args, 1), ctx.heap())?;
    let n = match a.compare(&b, None).map_err(|e| temporal_err(e, CLASS))? {
        std::cmp::Ordering::Less => -1,
        std::cmp::Ordering::Equal => 0,
        std::cmp::Ordering::Greater => 1,
    };
    Ok(Value::number_i32(n))
}

fn parse_duration_arg(
    v: &Value,
    heap: &otter_gc::GcHeap,
) -> Result<temporal_rs::Duration, NativeError> {
    if let Some(t) = v.as_temporal(heap) {
        match t.payload_clone(heap) {
            TemporalPayload::Duration(d) => Ok(d),
            _ => Err(NativeError::TypeError {
                name: CLASS,
                reason: "argument must be a Temporal.Duration".to_string(),
            }),
        }
    } else if let Some(s) = v.as_string(heap) {
        temporal_rs::Duration::from_utf8(s.to_lossy_string(heap).as_bytes())
            .map_err(|e| temporal_err(e, CLASS))
    } else if let Some(obj) = v.as_object() {
        partial_from_object(&obj, heap)
    } else {
        Err(NativeError::TypeError {
            name: CLASS,
            reason: "argument must be a Temporal.Duration, partial-record, or ISO string"
                .to_string(),
        })
    }
}

pub fn partial_from_object(
    obj: &JsObject,
    heap: &otter_gc::GcHeap,
) -> Result<temporal_rs::Duration, NativeError> {
    let mut partial = temporal_rs::partial::PartialDuration::empty();
    if let Some(v) = optional_field(obj, "years", heap)? {
        partial = partial.with_years(v);
    }
    if let Some(v) = optional_field(obj, "months", heap)? {
        partial = partial.with_months(v);
    }
    if let Some(v) = optional_field(obj, "weeks", heap)? {
        partial = partial.with_weeks(v);
    }
    if let Some(v) = optional_field(obj, "days", heap)? {
        partial = partial.with_days(v);
    }
    if let Some(v) = optional_field(obj, "hours", heap)? {
        partial = partial.with_hours(v);
    }
    if let Some(v) = optional_field(obj, "minutes", heap)? {
        partial = partial.with_minutes(v);
    }
    if let Some(v) = optional_field(obj, "seconds", heap)? {
        partial = partial.with_seconds(v);
    }
    if let Some(v) = optional_field(obj, "milliseconds", heap)? {
        partial = partial.with_milliseconds(v);
    }
    if let Some(v) = optional_field(obj, "microseconds", heap)? {
        partial = partial.with_microseconds(v as i128);
    }
    if let Some(v) = optional_field(obj, "nanoseconds", heap)? {
        partial = partial.with_nanoseconds(v as i128);
    }
    temporal_rs::Duration::from_partial_duration(partial)
        .map_err(|e| crate::temporal::helpers::temporal_err(e, CLASS))
}

fn optional_field(
    obj: &JsObject,
    name: &str,
    heap: &otter_gc::GcHeap,
) -> Result<Option<i64>, NativeError> {
    let Some(v) = object::get(*obj, heap, name) else {
        return Ok(None);
    };
    if v.is_undefined() {
        return Ok(None);
    }
    // §ToTemporalPartialDurationRecord — each field runs
    // ToIntegerIfIntegral: ToNumber (observable valueOf), reject NaN
    // / Infinity / a non-integral value with RangeError.
    let n = crate::temporal::helpers::to_integer_if_integral(&v, heap, CLASS, name)?;
    Ok(Some(n as i64))
}

#[must_use]
pub fn load_property(temporal: JsTemporal, heap: &otter_gc::GcHeap, name: &str) -> Value {
    let d = match temporal.payload_clone(heap) {
        TemporalPayload::Duration(v) => v,
        _ => return Value::undefined(),
    };
    match name {
        "years" => Value::number_i32(d.years() as i32),
        "months" => Value::number_i32(d.months() as i32),
        "weeks" => Value::number_i32(d.weeks() as i32),
        "days" => Value::number_i32(d.days() as i32),
        "hours" => Value::number_i32(d.hours() as i32),
        "minutes" => Value::number_i32(d.minutes() as i32),
        "seconds" => Value::number_i32(d.seconds() as i32),
        "milliseconds" => Value::number_i32(d.milliseconds() as i32),
        "microseconds" => Value::number_f64(d.microseconds() as f64),
        "nanoseconds" => Value::number_f64(d.nanoseconds() as f64),
        "sign" => Value::number_i32(d.sign() as i32),
        "blank" => Value::boolean(d.is_zero()),
        _ => Value::undefined(),
    }
}

fn impl_to_string(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let dur = require_duration(ctx)?;
    let rounding = parse_to_string_rounding_options(args, 0, ctx.heap(), CLASS)?;
    let s = dur
        .as_temporal_string(rounding)
        .map_err(|e| temporal_err(e, CLASS))?;
    js_string_value(s, ctx)
}

fn impl_to_json(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    impl_to_string(ctx, args)
}

fn impl_value_of(_ctx: &mut NativeCtx<'_>, _args: &[Value]) -> Result<Value, NativeError> {
    Err(NativeError::TypeError {
        name: CLASS,
        reason: "Temporal.Duration has no `.valueOf` — use `compare`".to_string(),
    })
}

fn impl_add(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let lhs = require_duration(ctx)?;
    let rhs = duration_arg(&arg_or_undef(args, 0), ctx.heap())?;
    let result = lhs.add(&rhs).map_err(|e| temporal_err(e, CLASS))?;
    make_temporal(ctx, TemporalPayload::Duration(result))
}

fn impl_subtract(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let lhs = require_duration(ctx)?;
    let rhs = duration_arg(&arg_or_undef(args, 0), ctx.heap())?;
    let result = lhs.subtract(&rhs).map_err(|e| temporal_err(e, CLASS))?;
    make_temporal(ctx, TemporalPayload::Duration(result))
}

fn impl_negated(ctx: &mut NativeCtx<'_>, _args: &[Value]) -> Result<Value, NativeError> {
    let dur = require_duration(ctx)?;
    make_temporal(ctx, TemporalPayload::Duration(dur.negated()))
}

fn impl_abs(ctx: &mut NativeCtx<'_>, _args: &[Value]) -> Result<Value, NativeError> {
    let dur = require_duration(ctx)?;
    make_temporal(ctx, TemporalPayload::Duration(dur.abs()))
}

fn impl_total(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let dur = require_duration(ctx)?;
    // §7.3.23 totalOf: a string is the `unit` shorthand; an object
    // supplies `{ unit }`.
    let total_of = arg_or_undef(args, 0);
    let unit_name = if let Some(s) = total_of.as_string(ctx.heap()) {
        s.to_lossy_string(ctx.heap())
    } else if let Some(opts) = total_of.as_object() {
        object::get(opts, ctx.heap(), "unit")
            .and_then(|v| {
                v.as_string(ctx.heap())
                    .map(|s| s.to_lossy_string(ctx.heap()))
            })
            .ok_or_else(|| NativeError::TypeError {
                name: CLASS,
                reason: "options must include a `unit` string".to_string(),
            })?
    } else {
        return Err(NativeError::TypeError {
            name: CLASS,
            reason: "total() requires a unit string or { unit } options".to_string(),
        });
    };
    let unit =
        temporal_rs::options::Unit::from_str(&unit_name).map_err(|_| NativeError::RangeError {
            name: CLASS,
            reason: "unknown duration unit".to_string(),
        })?;
    let relative_to = parse_relative_to(args, 0, ctx.heap())?;
    let total = dur
        .total(unit, relative_to)
        .map_err(|e| temporal_err(e, CLASS))?;
    Ok(Value::number_f64(total.as_inner()))
}

fn impl_with(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let dur = require_duration(ctx)?;
    let Some(obj) = arg_or_undef(args, 0).as_object() else {
        return Err(NativeError::TypeError {
            name: CLASS,
            reason: "with() requires a Temporal.Duration-like object".to_string(),
        });
    };
    // §7.3.21: fields present on the argument override the receiver's;
    // absent fields are inherited unchanged.
    let mut p = temporal_rs::partial::PartialDuration {
        years: Some(dur.years()),
        months: Some(dur.months()),
        weeks: Some(dur.weeks()),
        days: Some(dur.days()),
        hours: Some(dur.hours()),
        minutes: Some(dur.minutes()),
        seconds: Some(dur.seconds()),
        milliseconds: Some(dur.milliseconds()),
        microseconds: Some(dur.microseconds()),
        nanoseconds: Some(dur.nanoseconds()),
    };
    let heap = ctx.heap();
    if let Some(v) = optional_field(&obj, "years", heap)? {
        p.years = Some(v);
    }
    if let Some(v) = optional_field(&obj, "months", heap)? {
        p.months = Some(v);
    }
    if let Some(v) = optional_field(&obj, "weeks", heap)? {
        p.weeks = Some(v);
    }
    if let Some(v) = optional_field(&obj, "days", heap)? {
        p.days = Some(v);
    }
    if let Some(v) = optional_field(&obj, "hours", heap)? {
        p.hours = Some(v);
    }
    if let Some(v) = optional_field(&obj, "minutes", heap)? {
        p.minutes = Some(v);
    }
    if let Some(v) = optional_field(&obj, "seconds", heap)? {
        p.seconds = Some(v);
    }
    if let Some(v) = optional_field(&obj, "milliseconds", heap)? {
        p.milliseconds = Some(v);
    }
    if let Some(v) = optional_field(&obj, "microseconds", heap)? {
        p.microseconds = Some(v as i128);
    }
    if let Some(v) = optional_field(&obj, "nanoseconds", heap)? {
        p.nanoseconds = Some(v as i128);
    }
    let result =
        temporal_rs::Duration::from_partial_duration(p).map_err(|e| temporal_err(e, CLASS))?;
    make_temporal(ctx, TemporalPayload::Duration(result))
}

/// Parse the `relativeTo` option from a rounding/total options
/// argument into a [`temporal_rs::options::RelativeTo`]. Accepts a
/// `Temporal.PlainDate`/`PlainDateTime`/`ZonedDateTime`, an ISO
/// string, or a property bag (`timeZone` present → zoned).
fn parse_relative_to(
    args: &[Value],
    index: usize,
    heap: &otter_gc::GcHeap,
) -> Result<Option<temporal_rs::options::RelativeTo>, NativeError> {
    let Some(opts) = arg_or_undef(args, index).as_object() else {
        return Ok(None);
    };
    let Some(rel) = object::get(opts, heap, "relativeTo").filter(|v| !v.is_undefined()) else {
        return Ok(None);
    };
    if let Some(t) = rel.as_temporal(heap) {
        return match t.payload_clone(heap) {
            TemporalPayload::PlainDate(pd) => Ok(Some(pd.into())),
            TemporalPayload::PlainDateTime(pdt) => Ok(Some(pdt.to_plain_date().into())),
            TemporalPayload::ZonedDateTime(zdt) => Ok(Some(zdt.into())),
            _ => Err(NativeError::TypeError {
                name: CLASS,
                reason: "relativeTo must be a date, date-time, or zoned-date-time".to_string(),
            }),
        };
    }
    if let Some(s) = rel.as_string(heap) {
        return temporal_rs::options::RelativeTo::try_from_str(&s.to_lossy_string(heap))
            .map(Some)
            .map_err(|e| temporal_err(e, CLASS));
    }
    if let Some(obj) = rel.as_object() {
        if object::get(obj, heap, "timeZone")
            .filter(|v| !v.is_undefined())
            .is_some()
        {
            return Ok(Some(
                crate::temporal::zoned_date_time::parse_zdt_arg(&rel, heap)?.into(),
            ));
        }
        return Ok(Some(
            crate::temporal::plain_date::parse_plain_date_arg(&rel, heap)?.into(),
        ));
    }
    Err(NativeError::TypeError {
        name: CLASS,
        reason: "relativeTo must be a date, date-time, zoned-date-time, string, or object"
            .to_string(),
    })
}

fn impl_round(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let dur = require_duration(ctx)?;
    let options = parse_rounding_options(args, 0, ctx.heap(), CLASS)?;
    let relative_to = parse_relative_to(args, 0, ctx.heap())?;
    let result = dur
        .round(options, relative_to)
        .map_err(|e| temporal_err(e, CLASS))?;
    make_temporal(ctx, TemporalPayload::Duration(result))
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
        partial_from_object(&obj, heap)
    } else if let Some(s) = v.as_string(heap) {
        temporal_rs::Duration::from_utf8(s.to_lossy_string(heap).as_bytes())
            .map_err(|e| temporal_err(e, CLASS))
    } else {
        Err(NativeError::TypeError {
            name: CLASS,
            reason: "must be a Temporal.Duration, ISO string, or duration-like object".to_string(),
        })
    }
}

/// Generate a `Temporal.Duration.prototype` accessor getter. Each
/// getter re-validates the receiver through [`require_duration`] so a
/// branding `TypeError` is thrown when `this` is not a Duration, then
/// returns the corresponding field as a [`Value`].
macro_rules! duration_getter {
    ($fn:ident, $d:ident => $val:expr) => {
        fn $fn(ctx: &mut NativeCtx<'_>, _args: &[Value]) -> Result<Value, NativeError> {
            let $d = require_duration(ctx)?;
            Ok($val)
        }
    };
}

duration_getter!(get_years, d => Value::number_i32(d.years() as i32));
duration_getter!(get_months, d => Value::number_i32(d.months() as i32));
duration_getter!(get_weeks, d => Value::number_i32(d.weeks() as i32));
duration_getter!(get_days, d => Value::number_i32(d.days() as i32));
duration_getter!(get_hours, d => Value::number_i32(d.hours() as i32));
duration_getter!(get_minutes, d => Value::number_i32(d.minutes() as i32));
duration_getter!(get_seconds, d => Value::number_i32(d.seconds() as i32));
duration_getter!(get_milliseconds, d => Value::number_i32(d.milliseconds() as i32));
duration_getter!(get_microseconds, d => Value::number_f64(d.microseconds() as f64));
duration_getter!(get_nanoseconds, d => Value::number_f64(d.nanoseconds() as f64));
duration_getter!(get_sign, d => Value::number_i32(d.sign() as i32));
duration_getter!(get_blank, d => Value::boolean(d.is_zero()));

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

pub static DURATION_PROTOTYPE_METHODS: &[MethodSpec] = &[
    method("toString", 0, impl_to_string),
    method("toJSON", 0, impl_to_json),
    method("valueOf", 0, impl_value_of),
    method("add", 1, impl_add),
    method("subtract", 1, impl_subtract),
    method("negated", 0, impl_negated),
    method("abs", 0, impl_abs),
    method("with", 1, impl_with),
    method("round", 1, impl_round),
    method("total", 1, impl_total),
];

otter_macros::couch! {
    name = "Duration",
    feature = CORE,
    intrinsic = DurationIntrinsic,
    constructor = (length = 0, call = construct),
    statics = {
        "from"    / 1 => from,
        "compare" / 2 => compare,
    },
    prototype = {
        method_specs = [DURATION_PROTOTYPE_METHODS],
        accessors = [
            ("years",        get = get_years),
            ("months",       get = get_months),
            ("weeks",        get = get_weeks),
            ("days",         get = get_days),
            ("hours",        get = get_hours),
            ("minutes",      get = get_minutes),
            ("seconds",      get = get_seconds),
            ("milliseconds", get = get_milliseconds),
            ("microseconds", get = get_microseconds),
            ("nanoseconds",  get = get_nanoseconds),
            ("sign",         get = get_sign),
            ("blank",        get = get_blank),
        ],
    },
    install_on = crate::temporal::native_dispatch::temporal_host,
}
