//! `Temporal.Instant` — point on the UTC timeline.
//!
//! # See also
//! - <https://tc39.es/proposal-temporal/#sec-temporal-instant-objects>

#![allow(missing_docs)]

use num_traits::ToPrimitive;

use crate::js_surface::{Attr, MethodSpec};
use crate::native_function::NativeCall;
use crate::temporal::helpers::parse_to_string_rounding_options;
use crate::temporal::helpers::{
    arg_or_undef, js_string_value, make_temporal, parse_difference_settings,
    parse_rounding_options, parse_time_zone, require_construct, require_instant, temporal_err,
};
use crate::temporal::payload::{JsTemporal, TemporalPayload};
use crate::{NativeCtx, NativeError, Value};

const CLASS: &str = "Temporal.Instant";

pub fn construct(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    require_construct(ctx, CLASS)?;
    let raw = arg_or_undef(args, 0);
    let ns = if let Some(b) = raw.as_big_int() {
        b.with_inner(ctx.heap(), |bi| bi.to_i128())
    } else if let Some(s) = raw.as_string(ctx.heap()) {
        let text = s.to_lossy_string(ctx.heap());
        let parsed =
            crate::abstract_ops::string_to_big_int(&text).ok_or(NativeError::SyntaxError {
                name: CLASS,
                reason: format!("cannot convert {text:?} to a BigInt"),
            })?;
        parsed.to_i128()
    } else if let Some(b) = raw.as_boolean() {
        Some(i128::from(b))
    } else if raw.is_number() {
        return Err(NativeError::TypeError {
            name: CLASS,
            reason: "epochNanoseconds: cannot convert a Number to a BigInt".to_string(),
        });
    } else if raw.is_symbol() {
        return Err(NativeError::TypeError {
            name: CLASS,
            reason: "epochNanoseconds: cannot convert a Symbol to a BigInt".to_string(),
        });
    } else {
        return Err(NativeError::TypeError {
            name: CLASS,
            reason: "epochNanoseconds must be a BigInt".to_string(),
        });
    };
    let Some(ns) = ns else {
        return Err(NativeError::RangeError {
            name: CLASS,
            reason: "epochNanoseconds out of i128 range".to_string(),
        });
    };
    let inst = temporal_rs::Instant::try_new(ns).map_err(|e| temporal_err(e, CLASS))?;
    make_temporal(ctx, TemporalPayload::Instant(inst))
}

fn from(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let inst = parse_instant_arg(ctx, &arg_or_undef(args, 0))?;
    make_temporal(ctx, TemporalPayload::Instant(inst))
}

fn from_epoch_milliseconds(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let Some(ms) = arg_or_undef(args, 0).as_number().map(|n| n.as_f64() as i64) else {
        return Err(NativeError::TypeError {
            name: CLASS,
            reason: "fromEpochMilliseconds: argument must be a number".to_string(),
        });
    };
    let inst =
        temporal_rs::Instant::from_epoch_milliseconds(ms).map_err(|e| temporal_err(e, CLASS))?;
    make_temporal(ctx, TemporalPayload::Instant(inst))
}

fn impl_to_zoned_date_time_iso(
    ctx: &mut NativeCtx<'_>,
    args: &[Value],
) -> Result<Value, NativeError> {
    let inst = require_instant(ctx)?;
    let tz = parse_time_zone(&arg_or_undef(args, 0), ctx.heap(), CLASS)?;
    let zdt = inst
        .to_zoned_date_time_iso(tz)
        .map_err(|e| temporal_err(e, CLASS))?;
    make_temporal(ctx, TemporalPayload::ZonedDateTime(zdt))
}

fn from_epoch_nanoseconds(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let v = arg_or_undef(args, 0);
    let Some(bv) = v.as_big_int() else {
        return Err(NativeError::TypeError {
            name: CLASS,
            reason: "fromEpochNanoseconds: argument must be a BigInt".to_string(),
        });
    };
    let nanos =
        i128::try_from(bv.clone_inner(ctx.heap())).map_err(|_| NativeError::RangeError {
            name: CLASS,
            reason: "epoch nanoseconds out of range".to_string(),
        })?;
    let inst = temporal_rs::Instant::try_new(nanos).map_err(|e| temporal_err(e, CLASS))?;
    make_temporal(ctx, TemporalPayload::Instant(inst))
}

fn compare(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let a = parse_instant_arg(ctx, &arg_or_undef(args, 0))?;
    let b = parse_instant_arg(ctx, &arg_or_undef(args, 1))?;
    let n = match a.as_i128().cmp(&b.as_i128()) {
        std::cmp::Ordering::Less => -1,
        std::cmp::Ordering::Equal => 0,
        std::cmp::Ordering::Greater => 1,
    };
    Ok(Value::number_i32(n))
}

fn parse_instant_arg(
    ctx: &mut NativeCtx<'_>,
    v: &Value,
) -> Result<temporal_rs::Instant, NativeError> {
    if let Some(t) = v.as_temporal(ctx.heap()) {
        return match t.payload_clone(ctx.heap()) {
            TemporalPayload::Instant(v) => Ok(v),
            // §ToTemporalInstant fast path: a ZonedDateTime yields the
            // instant at its [[Nanoseconds]].
            TemporalPayload::ZonedDateTime(zdt) => Ok(zdt.to_instant()),
            _ => Err(NativeError::TypeError {
                name: CLASS,
                reason: "argument must be a Temporal.Instant".to_string(),
            }),
        };
    }
    if let Some(s) = v.as_string(ctx.heap()) {
        return temporal_rs::Instant::from_utf8(s.to_lossy_string(ctx.heap()).as_bytes())
            .map_err(|e| temporal_err(e, CLASS));
    }
    // §ToTemporalInstant: a non-Temporal Object is coerced with
    // ToPrimitive(string) — firing a user `toString` — and the result
    // parsed as an ISO instant string. Other primitives are a TypeError.
    if v.is_object_type() {
        let exec = ctx
            .execution_context()
            .cloned()
            .ok_or_else(|| NativeError::TypeError {
                name: CLASS,
                reason: "missing execution context".to_string(),
            })?;
        let s = ctx
            .cx
            .interp
            .coerce_to_string(&exec, v)
            .map_err(|e| crate::native_function::vm_to_native_error(e, CLASS))?;
        return temporal_rs::Instant::from_utf8(s.as_bytes()).map_err(|e| temporal_err(e, CLASS));
    }
    Err(NativeError::TypeError {
        name: CLASS,
        reason: "argument must be a Temporal.Instant, ISO string, or coercible object".to_string(),
    })
}

pub fn load_property(temporal: JsTemporal, heap: &mut otter_gc::GcHeap, name: &str) -> Value {
    let inst = match temporal.payload_clone(heap) {
        TemporalPayload::Instant(v) => v,
        _ => return Value::undefined(),
    };
    match name {
        "epochMilliseconds" => Value::number_f64(inst.epoch_milliseconds() as f64),
        "epochNanoseconds" => match crate::bigint::BigIntValue::from_i128(heap, inst.as_i128()) {
            Ok(handle) => Value::big_int(handle),
            Err(_) => Value::undefined(),
        },
        _ => Value::undefined(),
    }
}

fn impl_to_string(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let inst = require_instant(ctx)?;
    let rounding = parse_to_string_rounding_options(args, 0, ctx, CLASS)?;
    // §sec-temporal.instant.prototype.tostring step 7 — `timeZone`
    // option, when present, is parsed through ToTemporalTimeZone
    // (rejecting e.g. a `-000000` extended year), and the instant is
    // rendered in that zone.
    let time_zone = match args.first() {
        Some(opts) if opts.as_object().is_some() => {
            let tz_value = crate::object::get(opts.as_object().unwrap(), ctx.heap(), "timeZone");
            match tz_value {
                Some(v) if !v.is_undefined() => Some(parse_time_zone(&v, ctx.heap(), CLASS)?),
                _ => None,
            }
        }
        _ => None,
    };
    let s = inst
        .to_ixdtf_string(time_zone, rounding)
        .map_err(|e| temporal_err(e, CLASS))?;
    js_string_value(s, ctx)
}

fn impl_to_json(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    impl_to_string(ctx, args)
}

fn impl_value_of(_ctx: &mut NativeCtx<'_>, _args: &[Value]) -> Result<Value, NativeError> {
    Err(NativeError::TypeError {
        name: CLASS,
        reason: "Temporal.Instant has no `.valueOf` — use `compare` or `equals`".to_string(),
    })
}

fn arg_as_duration(
    ctx: &mut NativeCtx<'_>,
    v: &Value,
) -> Result<temporal_rs::Duration, NativeError> {
    if let Some(t) = v.as_temporal(ctx.heap()) {
        match t.payload_clone(ctx.heap()) {
            TemporalPayload::Duration(d) => Ok(d),
            _ => Err(NativeError::TypeError {
                name: CLASS,
                reason: "must be a Temporal.Duration".to_string(),
            }),
        }
    } else if v.is_object_type() {
        crate::temporal::duration::partial_from_object(ctx, *v)
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

fn impl_add(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let inst = require_instant(ctx)?;
    let dur = arg_as_duration(ctx, &arg_or_undef(args, 0))?;
    let result = inst.add(&dur).map_err(|e| temporal_err(e, CLASS))?;
    make_temporal(ctx, TemporalPayload::Instant(result))
}

fn impl_subtract(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let inst = require_instant(ctx)?;
    let dur = arg_as_duration(ctx, &arg_or_undef(args, 0))?;
    let result = inst.subtract(&dur).map_err(|e| temporal_err(e, CLASS))?;
    make_temporal(ctx, TemporalPayload::Instant(result))
}

fn impl_equals(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let inst = require_instant(ctx)?;
    let other = parse_instant_arg(ctx, &arg_or_undef(args, 0))?;
    Ok(Value::boolean(inst.as_i128() == other.as_i128()))
}

fn impl_until(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let inst = require_instant(ctx)?;
    let other = parse_instant_arg(ctx, &arg_or_undef(args, 0))?;
    let settings = parse_difference_settings(args, 1, ctx, CLASS)?;
    let result = inst
        .until(&other, settings)
        .map_err(|e| temporal_err(e, CLASS))?;
    make_temporal(ctx, TemporalPayload::Duration(result))
}

fn impl_since(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let inst = require_instant(ctx)?;
    let other = parse_instant_arg(ctx, &arg_or_undef(args, 0))?;
    let settings = parse_difference_settings(args, 1, ctx, CLASS)?;
    let result = inst
        .since(&other, settings)
        .map_err(|e| temporal_err(e, CLASS))?;
    make_temporal(ctx, TemporalPayload::Duration(result))
}

fn impl_round(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let inst = require_instant(ctx)?;
    let options = parse_rounding_options(args, 0, ctx, CLASS)?;
    let result = inst.round(options).map_err(|e| temporal_err(e, CLASS))?;
    make_temporal(ctx, TemporalPayload::Instant(result))
}

/// `Temporal.Instant.prototype` accessor getters, re-validating the
/// receiver via [`require_instant`] (branding `TypeError`).
fn get_epoch_milliseconds(ctx: &mut NativeCtx<'_>, _args: &[Value]) -> Result<Value, NativeError> {
    let inst = require_instant(ctx)?;
    Ok(Value::number_f64(inst.epoch_milliseconds() as f64))
}

fn get_epoch_nanoseconds(ctx: &mut NativeCtx<'_>, _args: &[Value]) -> Result<Value, NativeError> {
    let inst = require_instant(ctx)?;
    let nanos = inst.as_i128();
    match crate::bigint::BigIntValue::from_i128(ctx.heap_mut(), nanos) {
        Ok(handle) => Ok(Value::big_int(handle)),
        Err(_) => Ok(Value::undefined()),
    }
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

pub static INSTANT_PROTOTYPE_METHODS: &[MethodSpec] = &[
    method("toString", 0, impl_to_string),
    method("toJSON", 0, impl_to_json),
    method("valueOf", 0, impl_value_of),
    method("add", 1, impl_add),
    method("subtract", 1, impl_subtract),
    method("equals", 1, impl_equals),
    method("until", 1, impl_until),
    method("since", 1, impl_since),
    method("round", 1, impl_round),
    method("toZonedDateTimeISO", 1, impl_to_zoned_date_time_iso),
];

otter_macros::couch! {
    name = "Instant",
    feature = CORE,
    intrinsic = InstantIntrinsic,
    constructor = (length = 1, call = construct),
    statics = {
        "from"                  / 1 => from,
        "fromEpochMilliseconds" / 1 => from_epoch_milliseconds,
        "fromEpochNanoseconds"  / 1 => from_epoch_nanoseconds,
        "compare"               / 2 => compare,
    },
    prototype = {
        method_specs = [INSTANT_PROTOTYPE_METHODS],
        accessors = [
            ("epochMilliseconds", get = get_epoch_milliseconds),
            ("epochNanoseconds",  get = get_epoch_nanoseconds),
        ],
    },
    install_on = crate::temporal::native_dispatch::temporal_host,
}
