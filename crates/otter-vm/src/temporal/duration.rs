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
use crate::object;
use crate::temporal::helpers::parse_to_string_rounding_options;
use crate::temporal::helpers::{
    arg_or_undef, js_string_value, make_temporal, opt_integer_if_integral, options_object,
    parse_rounding_options, require_construct, require_duration, temporal_err,
};
use crate::temporal::payload::{JsTemporal, TemporalPayload};
use crate::{NativeCtx, NativeError, Value};

const CLASS: &str = "Temporal.Duration";

/// §7.1.1 `Temporal.Duration(years, months, weeks, days, hours,
/// minutes, seconds, milliseconds, microseconds, nanoseconds)`.
pub fn construct(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    require_construct(ctx, CLASS)?;
    let years = opt_integer_if_integral(ctx, args, 0, CLASS, "years")? as i64;
    let months = opt_integer_if_integral(ctx, args, 1, CLASS, "months")? as i64;
    let weeks = opt_integer_if_integral(ctx, args, 2, CLASS, "weeks")? as i64;
    let days = opt_integer_if_integral(ctx, args, 3, CLASS, "days")? as i64;
    let hours = opt_integer_if_integral(ctx, args, 4, CLASS, "hours")? as i64;
    let minutes = opt_integer_if_integral(ctx, args, 5, CLASS, "minutes")? as i64;
    let seconds = opt_integer_if_integral(ctx, args, 6, CLASS, "seconds")? as i64;
    let ms = opt_integer_if_integral(ctx, args, 7, CLASS, "milliseconds")? as i64;
    let us = opt_integer_if_integral(ctx, args, 8, CLASS, "microseconds")? as i128;
    let ns = opt_integer_if_integral(ctx, args, 9, CLASS, "nanoseconds")? as i128;
    let dur = temporal_rs::Duration::new(
        years, months, weeks, days, hours, minutes, seconds, ms, us, ns,
    )
    .map_err(|e| temporal_err(e, CLASS))?;
    make_temporal(ctx, TemporalPayload::Duration(dur))
}

fn from(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let first = arg_or_undef(args, 0);
    let dur = parse_duration_arg(ctx, &first)?;
    make_temporal(ctx, TemporalPayload::Duration(dur))
}

fn compare(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let a = parse_duration_arg(ctx, &arg_or_undef(args, 0))?;
    let b = parse_duration_arg(ctx, &arg_or_undef(args, 1))?;
    // §7.3.34: GetOptionsObject(options) — a non-object, non-undefined
    // options argument is a TypeError before relativeTo is read.
    options_object(&arg_or_undef(args, 2), CLASS)?;
    let relative_to = parse_relative_to(ctx, args, 2)?;
    let n = match a
        .compare(&b, relative_to)
        .map_err(|e| temporal_err(e, CLASS))?
    {
        std::cmp::Ordering::Less => -1,
        std::cmp::Ordering::Equal => 0,
        std::cmp::Ordering::Greater => 1,
    };
    Ok(Value::number_i32(n))
}

fn parse_duration_arg(
    ctx: &mut NativeCtx<'_>,
    v: &Value,
) -> Result<temporal_rs::Duration, NativeError> {
    if let Some(t) = v.as_temporal(ctx.heap()) {
        match t.payload_clone(ctx.heap()) {
            TemporalPayload::Duration(d) => Ok(d),
            _ => Err(NativeError::TypeError {
                name: CLASS,
                reason: "argument must be a Temporal.Duration".to_string(),
            }),
        }
    } else if let Some(s) = v.as_string(ctx.heap()) {
        temporal_rs::Duration::from_utf8(s.to_lossy_string(ctx.heap()).as_bytes())
            .map_err(|e| temporal_err(e, CLASS))
    } else if v.is_object_type() {
        partial_from_object(ctx, *v)
    } else {
        Err(NativeError::TypeError {
            name: CLASS,
            reason: "argument must be a Temporal.Duration, partial-record, or ISO string"
                .to_string(),
        })
    }
}

pub fn partial_from_object(
    ctx: &mut NativeCtx<'_>,
    target: Value,
) -> Result<temporal_rs::Duration, NativeError> {
    // §ToTemporalPartialDurationRecord reads the unit keys in
    // alphabetical order (days, hours, microseconds, milliseconds,
    // minutes, months, nanoseconds, seconds, weeks, years).
    let mut partial = temporal_rs::partial::PartialDuration::empty();
    if let Some(v) = optional_field(ctx, target, "days")? {
        partial = partial.with_days(v);
    }
    if let Some(v) = optional_field(ctx, target, "hours")? {
        partial = partial.with_hours(v);
    }
    if let Some(v) = optional_field(ctx, target, "microseconds")? {
        partial = partial.with_microseconds(v as i128);
    }
    if let Some(v) = optional_field(ctx, target, "milliseconds")? {
        partial = partial.with_milliseconds(v);
    }
    if let Some(v) = optional_field(ctx, target, "minutes")? {
        partial = partial.with_minutes(v);
    }
    if let Some(v) = optional_field(ctx, target, "months")? {
        partial = partial.with_months(v);
    }
    if let Some(v) = optional_field(ctx, target, "nanoseconds")? {
        partial = partial.with_nanoseconds(v as i128);
    }
    if let Some(v) = optional_field(ctx, target, "seconds")? {
        partial = partial.with_seconds(v);
    }
    if let Some(v) = optional_field(ctx, target, "weeks")? {
        partial = partial.with_weeks(v);
    }
    if let Some(v) = optional_field(ctx, target, "years")? {
        partial = partial.with_years(v);
    }
    temporal_rs::Duration::from_partial_duration(partial)
        .map_err(|e| crate::temporal::helpers::temporal_err(e, CLASS))
}

fn optional_field(
    ctx: &mut NativeCtx<'_>,
    target: Value,
    name: &str,
) -> Result<Option<i64>, NativeError> {
    // Getter/Proxy-aware [[Get]] so a duration-like property bag with
    // accessors or a Proxy is read observably.
    let v = crate::temporal::helpers::get_option_value(ctx, target, name, CLASS)?;
    if v.is_undefined() {
        return Ok(None);
    }
    // §ToTemporalPartialDurationRecord — each field runs
    // ToIntegerIfIntegral: ToNumber (observable valueOf), reject NaN
    // / Infinity / a non-integral value with RangeError.
    let n = crate::temporal::helpers::to_integer_if_integral(ctx, &v, CLASS, name)?;
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
    let rounding = parse_to_string_rounding_options(args, 0, ctx, CLASS)?;
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
    let rhs = duration_arg(ctx, &arg_or_undef(args, 0))?;
    let result = lhs.add(&rhs).map_err(|e| temporal_err(e, CLASS))?;
    make_temporal(ctx, TemporalPayload::Duration(result))
}

fn impl_subtract(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let lhs = require_duration(ctx)?;
    let rhs = duration_arg(ctx, &arg_or_undef(args, 0))?;
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
    let relative_to = parse_relative_to(ctx, args, 0)?;
    let total = dur
        .total(unit, relative_to)
        .map_err(|e| temporal_err(e, CLASS))?;
    Ok(Value::number_f64(total.as_inner()))
}

fn impl_with(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let dur = require_duration(ctx)?;
    let arg = arg_or_undef(args, 0);
    if !arg.is_object_type() {
        return Err(NativeError::TypeError {
            name: CLASS,
            reason: "with() requires a Temporal.Duration-like object".to_string(),
        });
    }
    // §7.3.21: fields present on the argument override the receiver's;
    // absent fields are inherited unchanged. The argument's keys are
    // read in alphabetical order (ToTemporalPartialDurationRecord).
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
    if let Some(v) = optional_field(ctx, arg, "days")? {
        p.days = Some(v);
    }
    if let Some(v) = optional_field(ctx, arg, "hours")? {
        p.hours = Some(v);
    }
    if let Some(v) = optional_field(ctx, arg, "microseconds")? {
        p.microseconds = Some(v as i128);
    }
    if let Some(v) = optional_field(ctx, arg, "milliseconds")? {
        p.milliseconds = Some(v);
    }
    if let Some(v) = optional_field(ctx, arg, "minutes")? {
        p.minutes = Some(v);
    }
    if let Some(v) = optional_field(ctx, arg, "months")? {
        p.months = Some(v);
    }
    if let Some(v) = optional_field(ctx, arg, "nanoseconds")? {
        p.nanoseconds = Some(v as i128);
    }
    if let Some(v) = optional_field(ctx, arg, "seconds")? {
        p.seconds = Some(v);
    }
    if let Some(v) = optional_field(ctx, arg, "weeks")? {
        p.weeks = Some(v);
    }
    if let Some(v) = optional_field(ctx, arg, "years")? {
        p.years = Some(v);
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
    ctx: &mut NativeCtx<'_>,
    args: &[Value],
    index: usize,
) -> Result<Option<temporal_rs::options::RelativeTo>, NativeError> {
    let Some(opts) = arg_or_undef(args, index).as_object() else {
        return Ok(None);
    };
    let Some(rel) = object::get(opts, ctx.heap(), "relativeTo").filter(|v| !v.is_undefined())
    else {
        return Ok(None);
    };
    if let Some(t) = rel.as_temporal(ctx.heap()) {
        return match t.payload_clone(ctx.heap()) {
            TemporalPayload::PlainDate(pd) => Ok(Some(pd.into())),
            TemporalPayload::PlainDateTime(pdt) => Ok(Some(pdt.to_plain_date().into())),
            TemporalPayload::ZonedDateTime(zdt) => Ok(Some(zdt.into())),
            _ => Err(NativeError::TypeError {
                name: CLASS,
                reason: "relativeTo must be a date, date-time, or zoned-date-time".to_string(),
            }),
        };
    }
    if let Some(s) = rel.as_string(ctx.heap()) {
        return temporal_rs::options::RelativeTo::try_from_str(&s.to_lossy_string(ctx.heap()))
            .map(Some)
            .map_err(|e| temporal_err(e, CLASS));
    }
    if let Some(obj) = rel.as_object() {
        if object::get(obj, ctx.heap(), "timeZone")
            .filter(|v| !v.is_undefined())
            .is_some()
        {
            return Ok(Some(
                crate::temporal::zoned_date_time::parse_zdt_arg(ctx, &rel)?.into(),
            ));
        }
        return Ok(Some(
            crate::temporal::plain_date::parse_plain_date_arg(ctx, &rel)?.into(),
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
    let options = parse_rounding_options(args, 0, ctx, CLASS)?;
    let relative_to = parse_relative_to(ctx, args, 0)?;
    let result = dur
        .round(options, relative_to)
        .map_err(|e| temporal_err(e, CLASS))?;
    make_temporal(ctx, TemporalPayload::Duration(result))
}

fn duration_arg(ctx: &mut NativeCtx<'_>, v: &Value) -> Result<temporal_rs::Duration, NativeError> {
    if let Some(t) = v.as_temporal(ctx.heap()) {
        match t.payload_clone(ctx.heap()) {
            TemporalPayload::Duration(d) => Ok(d),
            _ => Err(NativeError::TypeError {
                name: CLASS,
                reason: "must be a Temporal.Duration".to_string(),
            }),
        }
    } else if v.is_object_type() {
        partial_from_object(ctx, *v)
    } else if let Some(s) = v.as_string(ctx.heap()) {
        temporal_rs::Duration::from_utf8(s.to_lossy_string(ctx.heap()).as_bytes())
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
