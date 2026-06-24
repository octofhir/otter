//! `Temporal.PlainMonthDay` — calendar month+day (`MM-DD`).
//!
//! # See also
//! - <https://tc39.es/proposal-temporal/#sec-temporal-plainmonthday-objects>

#![allow(missing_docs)]

use crate::js_surface::{Attr, MethodSpec};
use crate::native_function::NativeCall;
use crate::temporal::helpers::parse_overflow;
use crate::temporal::helpers::{
    arg_or_undef, arg_to_calendar, clamp_to_u8, js_string_value, make_temporal,
    parse_calendar_fields, parse_display_calendar, read_calendar_field, require_construct,
    require_plain_month_day, str_or_undef, temporal_err, to_integer_with_truncation,
};
use crate::temporal::payload::{JsTemporal, TemporalPayload};
use crate::{NativeCtx, NativeError, Value};

const CLASS: &str = "Temporal.PlainMonthDay";

pub fn construct(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    require_construct(ctx, CLASS)?;
    let month_f = to_integer_with_truncation(ctx, &arg_or_undef(args, 0), CLASS, "isoMonth")?;
    let day_f = to_integer_with_truncation(ctx, &arg_or_undef(args, 1), CLASS, "isoDay")?;
    let calendar = arg_to_calendar(args, 2, ctx.heap(), CLASS)?;
    let ref_year_v = arg_or_undef(args, 3);
    let ref_year = if ref_year_v.is_undefined() {
        None
    } else {
        Some(to_integer_with_truncation(ctx, &ref_year_v, CLASS, "referenceISOYear")? as i32)
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
    let pmd = parse_pmd_arg_with_overflow(ctx, &arg_or_undef(args, 0), Some(args))?;
    make_temporal(ctx, TemporalPayload::PlainMonthDay(pmd))
}

fn parse_pmd_arg(
    ctx: &mut NativeCtx<'_>,
    v: &Value,
) -> Result<temporal_rs::PlainMonthDay, NativeError> {
    parse_pmd_arg_with_overflow(ctx, v, None)
}

fn parse_pmd_arg_with_overflow(
    ctx: &mut NativeCtx<'_>,
    v: &Value,
    overflow_opts: Option<&[Value]>,
) -> Result<temporal_rs::PlainMonthDay, NativeError> {
    if let Some(t) = v.as_temporal(ctx.heap()) {
        let pmd = match t.payload_clone(ctx.heap()) {
            TemporalPayload::PlainMonthDay(v) => v,
            _ => {
                return Err(NativeError::TypeError {
                    name: CLASS,
                    reason: "argument must be a Temporal.PlainMonthDay".to_string(),
                });
            }
        };
        if let Some(args) = overflow_opts {
            parse_overflow(ctx, args, 1)?;
        }
        Ok(pmd)
    } else if v.is_object_type() {
        let calendar = read_calendar_field(ctx, *v, CLASS)?;
        let fields = parse_calendar_fields(ctx, *v, &calendar, CLASS)?;
        let overflow = match overflow_opts {
            Some(args) => parse_overflow(ctx, args, 1)?,
            None => None,
        };
        let partial = temporal_rs::partial::PartialDate {
            calendar_fields: fields,
            calendar,
        };
        temporal_rs::PlainMonthDay::from_partial(partial, overflow)
            .map_err(|e| temporal_err(e, CLASS))
    } else if let Some(s) = v.as_string(ctx.heap()) {
        let pmd = temporal_rs::PlainMonthDay::from_utf8(s.to_lossy_string(ctx.heap()).as_bytes())
            .map_err(|e| temporal_err(e, CLASS))?;
        if let Some(args) = overflow_opts {
            parse_overflow(ctx, args, 1)?;
        }
        Ok(pmd)
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
    let display = parse_display_calendar(args, 0, ctx, CLASS)?;
    let s = pmd.to_ixdtf_string(display);
    js_string_value(s, ctx)
}

fn impl_to_json(ctx: &mut NativeCtx<'_>, _args: &[Value]) -> Result<Value, NativeError> {
    impl_to_string(ctx, &[])
}

fn impl_value_of(_ctx: &mut NativeCtx<'_>, _args: &[Value]) -> Result<Value, NativeError> {
    Err(NativeError::TypeError {
        name: CLASS,
        reason: "Temporal.PlainMonthDay has no `.valueOf` — use `equals`".to_string(),
    })
}

fn impl_equals(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let pmd = require_plain_month_day(ctx)?;
    let other = parse_pmd_arg(ctx, &arg_or_undef(args, 0))?;
    Ok(Value::boolean(pmd == other))
}

fn impl_with(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let pmd = require_plain_month_day(ctx)?;
    let arg = arg_or_undef(args, 0);
    if !arg.is_object_type() || arg.as_temporal(ctx.heap()).is_some() {
        return Err(NativeError::TypeError {
            name: CLASS,
            reason: "first argument must be a plain object".to_string(),
        });
    }
    let options = arg_or_undef(args, 1);
    if !options.is_undefined() && !options.is_object_type() {
        return Err(NativeError::TypeError {
            name: CLASS,
            reason: "options must be an object or undefined".to_string(),
        });
    }
    crate::temporal::helpers::reject_temporal_like_keys(ctx, arg, CLASS)?;
    let calendar = pmd.calendar().clone();
    let fields = parse_calendar_fields(ctx, arg, &calendar, CLASS)?;
    if crate::temporal::helpers::calendar_fields_empty(&fields) {
        return Err(NativeError::TypeError {
            name: CLASS,
            reason: "with() requires at least one recognized field".to_string(),
        });
    }
    let overflow = parse_overflow(ctx, args, 1)?;
    let result = pmd
        .with(fields, overflow)
        .map_err(|e| temporal_err(e, CLASS))?;
    make_temporal(ctx, TemporalPayload::PlainMonthDay(result))
}

fn impl_to_plain_date(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let pmd = require_plain_month_day(ctx)?;
    let arg = arg_or_undef(args, 0);
    if !arg.is_object_type() || arg.as_temporal(ctx.heap()).is_some() {
        return Err(NativeError::TypeError {
            name: CLASS,
            reason: "first argument must be a plain object with a `year` field".to_string(),
        });
    }
    let calendar = pmd.calendar().clone();
    let year_fields = parse_calendar_fields(ctx, arg, &calendar, CLASS)?;
    let result = pmd
        .to_plain_date(Some(year_fields))
        .map_err(|e| temporal_err(e, CLASS))?;
    make_temporal(ctx, TemporalPayload::PlainDate(result))
}

/// Generate a `Temporal.PlainMonthDay.prototype` accessor getter,
/// re-validating the receiver via [`require_plain_month_day`]
/// (branding `TypeError`). The heap arm exposes `&mut GcHeap` for
/// string-valued fields.
macro_rules! plain_month_day_getter {
    ($fn:ident, $pmd:ident => $val:expr) => {
        fn $fn(ctx: &mut NativeCtx<'_>, _args: &[Value]) -> Result<Value, NativeError> {
            let $pmd = require_plain_month_day(ctx)?;
            Ok($val)
        }
    };
    ($fn:ident, $pmd:ident, $heap:ident => $val:expr) => {
        fn $fn(ctx: &mut NativeCtx<'_>, _args: &[Value]) -> Result<Value, NativeError> {
            let $pmd = require_plain_month_day(ctx)?;
            let $heap = ctx.heap_mut();
            Ok($val)
        }
    };
}

plain_month_day_getter!(get_day, pmd => Value::number_i32(pmd.day() as i32));
plain_month_day_getter!(get_month_code, pmd, heap => str_or_undef(pmd.month_code().as_str(), heap));
plain_month_day_getter!(get_calendar_id, pmd, heap => str_or_undef(pmd.calendar().identifier(), heap));

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
    method("toLocaleString", 0, impl_to_locale_string),
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
        accessors = [
            ("calendarId", get = get_calendar_id),
            ("monthCode",  get = get_month_code),
            ("day",        get = get_day),
        ],
    },
    install_on = crate::temporal::native_dispatch::temporal_host,
    string_tag = "Temporal.PlainMonthDay",
}

/// §sec-temporal.*.prototype.tolocalestring — brand-checks the receiver,
/// then (absent the Intl formatting data path) renders the same canonical
/// string as `toString`.
fn impl_to_locale_string(ctx: &mut NativeCtx<'_>, _args: &[Value]) -> Result<Value, NativeError> {
    impl_to_string(ctx, &[])
}
