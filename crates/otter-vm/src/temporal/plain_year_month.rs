//! `Temporal.PlainYearMonth` — calendar year+month (`YYYY-MM`).
//!
//! # See also
//! - <https://tc39.es/proposal-temporal/#sec-temporal-plainyearmonth-objects>

#![allow(missing_docs)]

use crate::js_surface::{Attr, MethodSpec};
use crate::native_function::NativeCall;
use crate::temporal::duration::partial_from_object;
use crate::temporal::helpers::{
    arg_or_undef, arg_to_calendar, clamp_to_u8, js_string_value, make_temporal,
    parse_calendar_fields, parse_difference_settings, parse_display_calendar,
    parse_year_month_fields, read_calendar_field, require_construct, require_plain_year_month,
    str_or_undef, temporal_err, to_integer_with_truncation,
};
use crate::temporal::payload::{JsTemporal, TemporalPayload};
use crate::{NativeCtx, NativeError, Value};

const CLASS: &str = "Temporal.PlainYearMonth";

pub fn construct(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    require_construct(ctx, CLASS)?;
    let heap = ctx.heap();
    let year = to_integer_with_truncation(&arg_or_undef(args, 0), heap, CLASS, "isoYear")? as i32;
    let month_f = to_integer_with_truncation(&arg_or_undef(args, 1), heap, CLASS, "isoMonth")?;
    let calendar = arg_to_calendar(args, 2, heap, CLASS)?;
    let ref_day_v = arg_or_undef(args, 3);
    let ref_day = if ref_day_v.is_undefined() {
        None
    } else {
        let n = to_integer_with_truncation(&ref_day_v, heap, CLASS, "referenceISODay")?;
        Some(clamp_to_u8(n, CLASS, "referenceISODay")?)
    };
    let month = clamp_to_u8(month_f, CLASS, "isoMonth")?;
    let pym = temporal_rs::PlainYearMonth::try_new(year, month, ref_day, calendar)
        .map_err(|e| temporal_err(e, CLASS))?;
    make_temporal(ctx, TemporalPayload::PlainYearMonth(pym))
}

fn from(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let pym = parse_pym_arg(&arg_or_undef(args, 0), ctx.heap())?;
    make_temporal(ctx, TemporalPayload::PlainYearMonth(pym))
}

fn compare(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let a = parse_pym_arg(&arg_or_undef(args, 0), ctx.heap())?;
    let b = parse_pym_arg(&arg_or_undef(args, 1), ctx.heap())?;
    let n = match a.compare_iso(&b) {
        std::cmp::Ordering::Less => -1,
        std::cmp::Ordering::Equal => 0,
        std::cmp::Ordering::Greater => 1,
    };
    Ok(Value::number_i32(n))
}

fn parse_pym_arg(
    v: &Value,
    heap: &otter_gc::GcHeap,
) -> Result<temporal_rs::PlainYearMonth, NativeError> {
    if let Some(t) = v.as_temporal(heap) {
        match t.payload_clone(heap) {
            TemporalPayload::PlainYearMonth(v) => Ok(v),
            _ => Err(NativeError::TypeError {
                name: CLASS,
                reason: "argument must be a Temporal.PlainYearMonth".to_string(),
            }),
        }
    } else if let Some(s) = v.as_string(heap) {
        temporal_rs::PlainYearMonth::from_utf8(s.to_lossy_string(heap).as_bytes())
            .map_err(|e| temporal_err(e, CLASS))
    } else if let Some(obj) = v.as_object() {
        let fields = parse_year_month_fields(obj, heap, CLASS)?;
        let calendar = read_calendar_field(obj, heap, CLASS)?;
        let partial = temporal_rs::partial::PartialYearMonth {
            calendar_fields: fields,
            calendar,
        };
        temporal_rs::PlainYearMonth::from_partial(partial, None).map_err(|e| temporal_err(e, CLASS))
    } else {
        Err(NativeError::TypeError {
            name: CLASS,
            reason:
                "argument must be a Temporal.PlainYearMonth, ISO string, or year-month-like object"
                    .to_string(),
        })
    }
}

pub fn load_property(temporal: JsTemporal, heap: &mut otter_gc::GcHeap, name: &str) -> Value {
    let pym = match temporal.payload_clone(heap) {
        TemporalPayload::PlainYearMonth(v) => v,
        _ => return Value::undefined(),
    };
    match name {
        "year" => Value::number_i32(pym.year()),
        "month" => Value::number_i32(pym.month() as i32),
        "monthCode" => str_or_undef(pym.month_code().as_str(), heap),
        "daysInMonth" => Value::number_i32(pym.days_in_month() as i32),
        "daysInYear" => Value::number_i32(pym.days_in_year() as i32),
        "monthsInYear" => Value::number_i32(pym.months_in_year() as i32),
        "inLeapYear" => Value::boolean(pym.in_leap_year()),
        "era" => pym
            .era()
            .map_or(Value::undefined(), |era| str_or_undef(era.as_str(), heap)),
        "eraYear" => pym.era_year().map_or(Value::undefined(), Value::number_i32),
        "calendarId" => str_or_undef(pym.calendar().identifier(), heap),
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
    let pym = require_plain_year_month(ctx)?;
    let display = parse_display_calendar(args, 0, ctx.heap(), CLASS)?;
    let s = pym.to_ixdtf_string(display);
    js_string_value(s, ctx)
}

fn impl_to_json(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    impl_to_string(ctx, args)
}

fn impl_value_of(_ctx: &mut NativeCtx<'_>, _args: &[Value]) -> Result<Value, NativeError> {
    Err(NativeError::TypeError {
        name: CLASS,
        reason: "Temporal.PlainYearMonth has no `.valueOf` — use `compare` or `equals`".to_string(),
    })
}

fn impl_add(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let pym = require_plain_year_month(ctx)?;
    let dur = duration_arg(&arg_or_undef(args, 0), ctx.heap())?;
    let result = pym
        .add(&dur, temporal_rs::options::Overflow::Constrain)
        .map_err(|e| temporal_err(e, CLASS))?;
    make_temporal(ctx, TemporalPayload::PlainYearMonth(result))
}

fn impl_subtract(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let pym = require_plain_year_month(ctx)?;
    let dur = duration_arg(&arg_or_undef(args, 0), ctx.heap())?;
    let result = pym
        .subtract(&dur, temporal_rs::options::Overflow::Constrain)
        .map_err(|e| temporal_err(e, CLASS))?;
    make_temporal(ctx, TemporalPayload::PlainYearMonth(result))
}

fn impl_equals(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let pym = require_plain_year_month(ctx)?;
    let other = parse_pym_arg(&arg_or_undef(args, 0), ctx.heap())?;
    Ok(Value::boolean(
        pym.compare_iso(&other) == std::cmp::Ordering::Equal,
    ))
}

fn impl_until(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let pym = require_plain_year_month(ctx)?;
    let other = parse_pym_arg(&arg_or_undef(args, 0), ctx.heap())?;
    let settings = parse_difference_settings(args, 1, ctx.heap(), CLASS)?;
    let result = pym
        .until(&other, settings)
        .map_err(|e| temporal_err(e, CLASS))?;
    make_temporal(ctx, TemporalPayload::Duration(result))
}

fn impl_since(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let pym = require_plain_year_month(ctx)?;
    let other = parse_pym_arg(&arg_or_undef(args, 0), ctx.heap())?;
    let settings = parse_difference_settings(args, 1, ctx.heap(), CLASS)?;
    let result = pym
        .since(&other, settings)
        .map_err(|e| temporal_err(e, CLASS))?;
    make_temporal(ctx, TemporalPayload::Duration(result))
}

fn impl_with(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let pym = require_plain_year_month(ctx)?;
    let Some(obj) = arg_or_undef(args, 0).as_object() else {
        return Err(NativeError::TypeError {
            name: CLASS,
            reason: "first argument must be an object".to_string(),
        });
    };
    let fields = parse_year_month_fields(obj, ctx.heap(), CLASS)?;
    let result = pym.with(fields, None).map_err(|e| temporal_err(e, CLASS))?;
    make_temporal(ctx, TemporalPayload::PlainYearMonth(result))
}

fn impl_to_plain_date(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let pym = require_plain_year_month(ctx)?;
    let Some(obj) = arg_or_undef(args, 0).as_object() else {
        return Err(NativeError::TypeError {
            name: CLASS,
            reason: "first argument must be an object with a `day` field".to_string(),
        });
    };
    let day_fields = parse_calendar_fields(obj, ctx.heap(), CLASS)?;
    let result = pym
        .to_plain_date(Some(day_fields))
        .map_err(|e| temporal_err(e, CLASS))?;
    make_temporal(ctx, TemporalPayload::PlainDate(result))
}

/// Generate a `Temporal.PlainYearMonth.prototype` accessor getter,
/// re-validating the receiver via [`require_plain_year_month`]
/// (branding `TypeError`). The heap arm exposes `&mut GcHeap`.
macro_rules! plain_year_month_getter {
    ($fn:ident, $pym:ident => $val:expr) => {
        fn $fn(ctx: &mut NativeCtx<'_>, _args: &[Value]) -> Result<Value, NativeError> {
            let $pym = require_plain_year_month(ctx)?;
            Ok($val)
        }
    };
    ($fn:ident, $pym:ident, $heap:ident => $val:expr) => {
        fn $fn(ctx: &mut NativeCtx<'_>, _args: &[Value]) -> Result<Value, NativeError> {
            let $pym = require_plain_year_month(ctx)?;
            let $heap = ctx.heap_mut();
            Ok($val)
        }
    };
}

plain_year_month_getter!(get_year, pym => Value::number_i32(pym.year()));
plain_year_month_getter!(get_month, pym => Value::number_i32(pym.month() as i32));
plain_year_month_getter!(get_month_code, pym, heap => str_or_undef(pym.month_code().as_str(), heap));
plain_year_month_getter!(get_days_in_month, pym => Value::number_i32(pym.days_in_month() as i32));
plain_year_month_getter!(get_days_in_year, pym => Value::number_i32(pym.days_in_year() as i32));
plain_year_month_getter!(get_months_in_year, pym => Value::number_i32(pym.months_in_year() as i32));
plain_year_month_getter!(get_in_leap_year, pym => Value::boolean(pym.in_leap_year()));
plain_year_month_getter!(get_era, pym, heap => pym
    .era()
    .map_or(Value::undefined(), |era| str_or_undef(era.as_str(), heap)));
plain_year_month_getter!(get_era_year, pym => pym
    .era_year()
    .map_or(Value::undefined(), Value::number_i32));
plain_year_month_getter!(get_calendar_id, pym, heap => str_or_undef(pym.calendar().identifier(), heap));

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

pub static PLAIN_YEAR_MONTH_PROTOTYPE_METHODS: &[MethodSpec] = &[
    method("toString", 0, impl_to_string),
    method("toJSON", 0, impl_to_json),
    method("valueOf", 0, impl_value_of),
    method("add", 1, impl_add),
    method("subtract", 1, impl_subtract),
    method("equals", 1, impl_equals),
    method("until", 1, impl_until),
    method("since", 1, impl_since),
    method("with", 1, impl_with),
    method("toPlainDate", 1, impl_to_plain_date),
];

otter_macros::couch! {
    name = "PlainYearMonth",
    feature = CORE,
    intrinsic = PlainYearMonthIntrinsic,
    constructor = (length = 2, call = construct),
    statics = {
        "from"    / 1 => from,
        "compare" / 2 => compare,
    },
    prototype = {
        method_specs = [PLAIN_YEAR_MONTH_PROTOTYPE_METHODS],
        accessors = [
            ("calendarId",  get = get_calendar_id),
            ("era",         get = get_era),
            ("eraYear",     get = get_era_year),
            ("year",        get = get_year),
            ("month",       get = get_month),
            ("monthCode",   get = get_month_code),
            ("daysInMonth", get = get_days_in_month),
            ("daysInYear",  get = get_days_in_year),
            ("monthsInYear", get = get_months_in_year),
            ("inLeapYear",  get = get_in_leap_year),
        ],
    },
    install_on = crate::temporal::native_dispatch::temporal_host,
}
