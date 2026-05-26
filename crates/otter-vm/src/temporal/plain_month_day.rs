//! `Temporal.PlainMonthDay` — calendar month+day (`MM-DD`).
//!
//! # See also
//! - <https://tc39.es/proposal-temporal/#sec-temporal-plainmonthday-objects>

#![allow(missing_docs)]

use crate::js_surface::{Attr, MethodSpec};
use crate::native_function::NativeCall;
use crate::temporal::helpers::{
    arg_or_undef, arg_to_calendar, clamp_to_u8, js_string_value, make_temporal,
    parse_calendar_fields, parse_display_calendar, require_construct, require_plain_month_day,
    str_or_undef, temporal_err, to_integer_with_truncation,
};
use crate::temporal::payload::{JsTemporal, TemporalPayload};
use crate::{NativeCtx, NativeError, Value};

const CLASS: &str = "Temporal.PlainMonthDay";

pub fn construct(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    require_construct(ctx, CLASS)?;
    let heap = ctx.heap();
    let month_f = to_integer_with_truncation(&arg_or_undef(args, 0), heap, CLASS, "isoMonth")?;
    let day_f = to_integer_with_truncation(&arg_or_undef(args, 1), heap, CLASS, "isoDay")?;
    let calendar = arg_to_calendar(args, 2, heap, CLASS)?;
    let ref_year_v = arg_or_undef(args, 3);
    let ref_year = if ref_year_v.is_undefined() {
        None
    } else {
        Some(to_integer_with_truncation(&ref_year_v, heap, CLASS, "referenceISOYear")? as i32)
    };
    let month = clamp_to_u8(month_f, CLASS, "isoMonth")?;
    let day = clamp_to_u8(day_f, CLASS, "isoDay")?;
    let pmd = temporal_rs::PlainMonthDay::new_with_overflow(
        month,
        day,
        calendar,
        temporal_rs::options::Overflow::Reject,
        ref_year,
    )
    .map_err(|e| temporal_err(e, CLASS))?;
    make_temporal(ctx, TemporalPayload::PlainMonthDay(pmd))
}

fn from(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let pmd = parse_pmd_arg(&arg_or_undef(args, 0), ctx.heap())?;
    make_temporal(ctx, TemporalPayload::PlainMonthDay(pmd))
}

fn parse_pmd_arg(
    v: &Value,
    heap: &otter_gc::GcHeap,
) -> Result<temporal_rs::PlainMonthDay, NativeError> {
    if let Some(t) = v.as_temporal(heap) {
        match t.payload_clone(heap) {
            TemporalPayload::PlainMonthDay(v) => Ok(v),
            _ => Err(NativeError::TypeError {
                name: CLASS,
                reason: "argument must be a Temporal.PlainMonthDay".to_string(),
            }),
        }
    } else if let Some(s) = v.as_string(heap) {
        temporal_rs::PlainMonthDay::from_utf8(s.to_lossy_string(heap).as_bytes())
            .map_err(|e| temporal_err(e, CLASS))
    } else if let Some(obj) = v.as_object() {
        let fields = parse_calendar_fields(obj, heap, CLASS)?;
        let partial = temporal_rs::partial::PartialDate {
            calendar_fields: fields,
            calendar: temporal_rs::Calendar::default(),
        };
        temporal_rs::PlainMonthDay::from_partial(partial, None).map_err(|e| temporal_err(e, CLASS))
    } else {
        Err(NativeError::TypeError {
            name: CLASS,
            reason:
                "argument must be a Temporal.PlainMonthDay, ISO string, or month-day-like object"
                    .to_string(),
        })
    }
}

pub fn load_property(temporal: JsTemporal, heap: &mut otter_gc::GcHeap, name: &str) -> Value {
    let pmd = match temporal.payload_clone(heap) {
        TemporalPayload::PlainMonthDay(v) => v,
        _ => return Value::undefined(),
    };
    match name {
        "day" => Value::number_i32(pmd.day() as i32),
        "monthCode" => str_or_undef(pmd.month_code().as_str(), heap),
        "calendarId" => str_or_undef(pmd.calendar().identifier(), heap),
        _ => Value::undefined(),
    }
}

fn impl_to_string(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let pmd = require_plain_month_day(ctx)?;
    let display = parse_display_calendar(args, 0, ctx.heap(), CLASS)?;
    let s = pmd.to_ixdtf_string(display);
    js_string_value(s, ctx)
}

fn impl_to_json(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    impl_to_string(ctx, args)
}

fn impl_value_of(_ctx: &mut NativeCtx<'_>, _args: &[Value]) -> Result<Value, NativeError> {
    Err(NativeError::TypeError {
        name: CLASS,
        reason: "Temporal.PlainMonthDay has no `.valueOf` — use `equals`".to_string(),
    })
}

fn impl_equals(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let pmd = require_plain_month_day(ctx)?;
    let other = parse_pmd_arg(&arg_or_undef(args, 0), ctx.heap())?;
    Ok(Value::boolean(pmd == other))
}

fn impl_with(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let pmd = require_plain_month_day(ctx)?;
    let Some(obj) = arg_or_undef(args, 0).as_object() else {
        return Err(NativeError::TypeError {
            name: CLASS,
            reason: "first argument must be an object".to_string(),
        });
    };
    let fields = parse_calendar_fields(obj, ctx.heap(), CLASS)?;
    let result = pmd.with(fields, None).map_err(|e| temporal_err(e, CLASS))?;
    make_temporal(ctx, TemporalPayload::PlainMonthDay(result))
}

fn impl_to_plain_date(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let pmd = require_plain_month_day(ctx)?;
    let Some(obj) = arg_or_undef(args, 0).as_object() else {
        return Err(NativeError::TypeError {
            name: CLASS,
            reason: "first argument must be an object with a `year` field".to_string(),
        });
    };
    let year_fields = parse_calendar_fields(obj, ctx.heap(), CLASS)?;
    let result = pmd
        .to_plain_date(Some(year_fields))
        .map_err(|e| temporal_err(e, CLASS))?;
    make_temporal(ctx, TemporalPayload::PlainDate(result))
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

pub static PLAIN_MONTH_DAY_PROTOTYPE_METHODS: &[MethodSpec] = &[
    method("toString", 0, impl_to_string),
    method("toJSON", 0, impl_to_json),
    method("valueOf", 0, impl_value_of),
    method("equals", 1, impl_equals),
    method("with", 1, impl_with),
    method("toPlainDate", 1, impl_to_plain_date),
];

otter_macros::couch! {
    name = "PlainMonthDay",
    feature = CORE,
    intrinsic = PlainMonthDayIntrinsic,
    constructor = (length = 2, call = construct),
    statics = {
        "from" / 1 => from,
    },
    prototype = {
        method_specs = [PLAIN_MONTH_DAY_PROTOTYPE_METHODS],
    },
    install_on = crate::temporal::native_dispatch::temporal_host,
}
