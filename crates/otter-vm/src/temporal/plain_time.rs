//! `Temporal.PlainTime` — wall-clock time without a date or zone.
//!
//! # See also
//! - <https://tc39.es/proposal-temporal/#sec-temporal-plaintime-objects>

#![allow(missing_docs)]

use crate::js_surface::{Attr, MethodSpec};
use crate::native_function::NativeCall;
use crate::temporal::duration::partial_from_object;
use crate::temporal::helpers::parse_to_string_rounding_options;
use crate::temporal::helpers::{
    arg_or_undef, clamp_to_u8, clamp_to_u16, js_string_value, make_temporal,
    opt_integer_with_truncation, parse_difference_settings, parse_partial_time,
    parse_rounding_options, require_construct, require_plain_time, temporal_err,
};
use crate::temporal::payload::{JsTemporal, TemporalPayload};
use crate::{NativeCtx, NativeError, Value};

const CLASS: &str = "Temporal.PlainTime";

pub fn construct(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    require_construct(ctx, CLASS)?;
    let heap = ctx.heap();
    let hour = clamp_to_u8(
        opt_integer_with_truncation(args, 0, heap, CLASS, "hour")?,
        CLASS,
        "hour",
    )?;
    let minute = clamp_to_u8(
        opt_integer_with_truncation(args, 1, heap, CLASS, "minute")?,
        CLASS,
        "minute",
    )?;
    let second = clamp_to_u8(
        opt_integer_with_truncation(args, 2, heap, CLASS, "second")?,
        CLASS,
        "second",
    )?;
    let millisecond = clamp_to_u16(
        opt_integer_with_truncation(args, 3, heap, CLASS, "millisecond")?,
        CLASS,
        "millisecond",
    )?;
    let microsecond = clamp_to_u16(
        opt_integer_with_truncation(args, 4, heap, CLASS, "microsecond")?,
        CLASS,
        "microsecond",
    )?;
    let nanosecond = clamp_to_u16(
        opt_integer_with_truncation(args, 5, heap, CLASS, "nanosecond")?,
        CLASS,
        "nanosecond",
    )?;
    let pt =
        temporal_rs::PlainTime::try_new(hour, minute, second, millisecond, microsecond, nanosecond)
            .map_err(|e| temporal_err(e, CLASS))?;
    make_temporal(ctx, TemporalPayload::PlainTime(pt))
}

fn from(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let pt = parse_plain_time_arg(&arg_or_undef(args, 0), ctx.heap())?;
    make_temporal(ctx, TemporalPayload::PlainTime(pt))
}

fn compare(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let a = parse_plain_time_arg(&arg_or_undef(args, 0), ctx.heap())?;
    let b = parse_plain_time_arg(&arg_or_undef(args, 1), ctx.heap())?;
    let n = if a == b {
        0
    } else if a.hour() < b.hour()
        || (a.hour() == b.hour() && a.minute() < b.minute())
        || (a.hour() == b.hour() && a.minute() == b.minute() && a.second() < b.second())
    {
        -1
    } else {
        1
    };
    Ok(Value::number_i32(n))
}

pub(crate) fn parse_plain_time_arg(
    v: &Value,
    heap: &otter_gc::GcHeap,
) -> Result<temporal_rs::PlainTime, NativeError> {
    // §ToTemporalTime: PlainDateTime / ZonedDateTime project onto
    // their wall-clock time; a plain object is read as a time
    // property bag; a string is parsed as ISO.
    if let Some(t) = v.as_temporal(heap) {
        match t.payload_clone(heap) {
            TemporalPayload::PlainTime(v) => Ok(v),
            TemporalPayload::PlainDateTime(pdt) => Ok(pdt.to_plain_time()),
            TemporalPayload::ZonedDateTime(zdt) => Ok(zdt.to_plain_time()),
            _ => Err(NativeError::TypeError {
                name: CLASS,
                reason: "argument must be a Temporal.PlainTime".to_string(),
            }),
        }
    } else if let Some(obj) = v.as_object() {
        let partial = parse_partial_time(obj, heap, CLASS)?;
        temporal_rs::PlainTime::from_partial(partial, None).map_err(|e| temporal_err(e, CLASS))
    } else if let Some(s) = v.as_string(heap) {
        temporal_rs::PlainTime::from_utf8(s.to_lossy_string(heap).as_bytes())
            .map_err(|e| temporal_err(e, CLASS))
    } else {
        Err(NativeError::TypeError {
            name: CLASS,
            reason: "argument must be a Temporal.PlainTime, ISO string, or time-like object"
                .to_string(),
        })
    }
}

pub fn load_property(temporal: JsTemporal, heap: &otter_gc::GcHeap, name: &str) -> Value {
    let pt = match temporal.payload_clone(heap) {
        TemporalPayload::PlainTime(v) => v,
        _ => return Value::undefined(),
    };
    match name {
        "hour" => Value::number_i32(pt.hour() as i32),
        "minute" => Value::number_i32(pt.minute() as i32),
        "second" => Value::number_i32(pt.second() as i32),
        "millisecond" => Value::number_i32(pt.millisecond() as i32),
        "microsecond" => Value::number_i32(pt.microsecond() as i32),
        "nanosecond" => Value::number_i32(pt.nanosecond() as i32),
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

fn impl_to_string(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let pt = require_plain_time(ctx)?;
    let rounding = parse_to_string_rounding_options(args, 0, ctx.heap(), CLASS)?;
    let s = pt
        .to_ixdtf_string(rounding)
        .map_err(|e| temporal_err(e, CLASS))?;
    js_string_value(s, ctx)
}

fn impl_to_json(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    impl_to_string(ctx, args)
}

fn impl_value_of(_ctx: &mut NativeCtx<'_>, _args: &[Value]) -> Result<Value, NativeError> {
    Err(NativeError::TypeError {
        name: CLASS,
        reason: "Temporal.PlainTime has no `.valueOf` — use `compare` or `equals`".to_string(),
    })
}

fn impl_add(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let pt = require_plain_time(ctx)?;
    let dur = duration_arg(&arg_or_undef(args, 0), ctx.heap())?;
    let result = pt.add(&dur).map_err(|e| temporal_err(e, CLASS))?;
    make_temporal(ctx, TemporalPayload::PlainTime(result))
}

fn impl_subtract(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let pt = require_plain_time(ctx)?;
    let dur = duration_arg(&arg_or_undef(args, 0), ctx.heap())?;
    let result = pt.subtract(&dur).map_err(|e| temporal_err(e, CLASS))?;
    make_temporal(ctx, TemporalPayload::PlainTime(result))
}

fn impl_equals(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let pt = require_plain_time(ctx)?;
    let other = parse_plain_time_arg(&arg_or_undef(args, 0), ctx.heap())?;
    Ok(Value::boolean(pt == other))
}

fn impl_until(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let pt = require_plain_time(ctx)?;
    let other = parse_plain_time_arg(&arg_or_undef(args, 0), ctx.heap())?;
    let settings = parse_difference_settings(args, 1, ctx.heap(), CLASS)?;
    let result = pt
        .until(&other, settings)
        .map_err(|e| temporal_err(e, CLASS))?;
    make_temporal(ctx, TemporalPayload::Duration(result))
}

fn impl_since(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let pt = require_plain_time(ctx)?;
    let other = parse_plain_time_arg(&arg_or_undef(args, 0), ctx.heap())?;
    let settings = parse_difference_settings(args, 1, ctx.heap(), CLASS)?;
    let result = pt
        .since(&other, settings)
        .map_err(|e| temporal_err(e, CLASS))?;
    make_temporal(ctx, TemporalPayload::Duration(result))
}

fn impl_round(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let pt = require_plain_time(ctx)?;
    let options = parse_rounding_options(args, 0, ctx.heap(), CLASS)?;
    let result = pt.round(options).map_err(|e| temporal_err(e, CLASS))?;
    make_temporal(ctx, TemporalPayload::PlainTime(result))
}

fn impl_with(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let pt = require_plain_time(ctx)?;
    let Some(obj) = arg_or_undef(args, 0).as_object() else {
        return Err(NativeError::TypeError {
            name: CLASS,
            reason: "first argument must be an object".to_string(),
        });
    };
    let partial = parse_partial_time(obj, ctx.heap(), CLASS)?;
    let result = pt.with(partial, None).map_err(|e| temporal_err(e, CLASS))?;
    make_temporal(ctx, TemporalPayload::PlainTime(result))
}

/// Generate a `Temporal.PlainTime.prototype` accessor getter. Each
/// re-validates the receiver via [`require_plain_time`] (branding
/// `TypeError`) before returning its field.
macro_rules! plain_time_getter {
    ($fn:ident, $pt:ident => $val:expr) => {
        fn $fn(ctx: &mut NativeCtx<'_>, _args: &[Value]) -> Result<Value, NativeError> {
            let $pt = require_plain_time(ctx)?;
            Ok($val)
        }
    };
}

plain_time_getter!(get_hour, pt => Value::number_i32(pt.hour() as i32));
plain_time_getter!(get_minute, pt => Value::number_i32(pt.minute() as i32));
plain_time_getter!(get_second, pt => Value::number_i32(pt.second() as i32));
plain_time_getter!(get_millisecond, pt => Value::number_i32(pt.millisecond() as i32));
plain_time_getter!(get_microsecond, pt => Value::number_i32(pt.microsecond() as i32));
plain_time_getter!(get_nanosecond, pt => Value::number_i32(pt.nanosecond() as i32));

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

pub static PLAIN_TIME_PROTOTYPE_METHODS: &[MethodSpec] = &[
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
];

otter_macros::couch! {
    name = "PlainTime",
    feature = CORE,
    intrinsic = PlainTimeIntrinsic,
    constructor = (length = 0, call = construct),
    statics = {
        "from"    / 1 => from,
        "compare" / 2 => compare,
    },
    prototype = {
        method_specs = [PLAIN_TIME_PROTOTYPE_METHODS],
        accessors = [
            ("hour",        get = get_hour),
            ("minute",      get = get_minute),
            ("second",      get = get_second),
            ("millisecond", get = get_millisecond),
            ("microsecond", get = get_microsecond),
            ("nanosecond",  get = get_nanosecond),
        ],
    },
    install_on = crate::temporal::native_dispatch::temporal_host,
}
