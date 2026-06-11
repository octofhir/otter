//! `Temporal.PlainDateTime` — combined wall-clock date + time without a zone.
//!
//! # See also
//! - <https://tc39.es/proposal-temporal/#sec-temporal-plaindatetime-objects>

#![allow(missing_docs)]

use crate::js_surface::{Attr, MethodSpec};
use crate::native_function::NativeCall;
use crate::temporal::duration::partial_from_object;
use crate::temporal::helpers::parse_overflow;
use crate::temporal::helpers::parse_to_string_rounding_options;
use crate::temporal::helpers::{
    arg_or_undef, arg_to_calendar, clamp_to_u8, clamp_to_u16, js_string_value, make_temporal,
    opt_integer_with_truncation, parse_date_time_fields, parse_difference_settings,
    parse_disambiguation, parse_display_calendar, parse_partial_time, parse_rounding_options,
    parse_time_zone, read_calendar_field, require_construct, require_plain_date_time, str_or_undef,
    temporal_err, to_integer_with_truncation,
};
use crate::temporal::payload::{JsTemporal, TemporalPayload};
use crate::{NativeCtx, NativeError, Value};

const CLASS: &str = "Temporal.PlainDateTime";

pub fn construct(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    require_construct(ctx, CLASS)?;
    let year = to_integer_with_truncation(ctx, &arg_or_undef(args, 0), CLASS, "isoYear")? as i32;
    let month = clamp_to_u8(
        to_integer_with_truncation(ctx, &arg_or_undef(args, 1), CLASS, "isoMonth")?,
        CLASS,
        "isoMonth",
    )?;
    let day = clamp_to_u8(
        to_integer_with_truncation(ctx, &arg_or_undef(args, 2), CLASS, "isoDay")?,
        CLASS,
        "isoDay",
    )?;
    let hour = clamp_to_u8(
        opt_integer_with_truncation(ctx, args, 3, CLASS, "hour")?,
        CLASS,
        "hour",
    )?;
    let minute = clamp_to_u8(
        opt_integer_with_truncation(ctx, args, 4, CLASS, "minute")?,
        CLASS,
        "minute",
    )?;
    let second = clamp_to_u8(
        opt_integer_with_truncation(ctx, args, 5, CLASS, "second")?,
        CLASS,
        "second",
    )?;
    let millisecond = clamp_to_u16(
        opt_integer_with_truncation(ctx, args, 6, CLASS, "millisecond")?,
        CLASS,
        "millisecond",
    )?;
    let microsecond = clamp_to_u16(
        opt_integer_with_truncation(ctx, args, 7, CLASS, "microsecond")?,
        CLASS,
        "microsecond",
    )?;
    let nanosecond = clamp_to_u16(
        opt_integer_with_truncation(ctx, args, 8, CLASS, "nanosecond")?,
        CLASS,
        "nanosecond",
    )?;
    let calendar = arg_to_calendar(args, 9, ctx.heap(), CLASS)?;
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
    let arg = arg_or_undef(args, 0);
    // §ToTemporalDateTime: parse a primitive ISO string before
    // GetTemporalOverflowOption, so an invalid string rejects before
    // the `overflow` option is observed.
    if arg.as_temporal(ctx.heap()).is_none()
        && let Some(s) = arg.as_string(ctx.heap())
    {
        let pdt = temporal_rs::PlainDateTime::from_utf8(s.to_lossy_string(ctx.heap()).as_bytes())
            .map_err(|e| temporal_err(e, CLASS))?;
        parse_overflow(ctx, args, 1)?;
        return make_temporal(ctx, TemporalPayload::PlainDateTime(pdt));
    }
    let overflow = parse_overflow(ctx, args, 1)?;
    let pdt = parse_plain_date_time_arg_with_overflow(ctx, &arg, overflow)?;
    make_temporal(ctx, TemporalPayload::PlainDateTime(pdt))
}

fn compare(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let a = parse_plain_date_time_arg(ctx, &arg_or_undef(args, 0))?;
    let b = parse_plain_date_time_arg(ctx, &arg_or_undef(args, 1))?;
    let n = match temporal_rs::PlainDateTime::compare_iso(&a, &b) {
        std::cmp::Ordering::Less => -1,
        std::cmp::Ordering::Equal => 0,
        std::cmp::Ordering::Greater => 1,
    };
    Ok(Value::number_i32(n))
}

fn parse_plain_date_time_arg(
    ctx: &mut NativeCtx<'_>,
    v: &Value,
) -> Result<temporal_rs::PlainDateTime, NativeError> {
    parse_plain_date_time_arg_with_overflow(ctx, v, None)
}

fn parse_plain_date_time_arg_with_overflow(
    ctx: &mut NativeCtx<'_>,
    v: &Value,
    overflow: Option<temporal_rs::options::Overflow>,
) -> Result<temporal_rs::PlainDateTime, NativeError> {
    // §ToTemporalDateTime: ZonedDateTime projects onto its wall-clock
    // date-time; a plain object is read as a date-time property bag; a
    // string is parsed as ISO.
    if let Some(t) = v.as_temporal(ctx.heap()) {
        match t.payload_clone(ctx.heap()) {
            TemporalPayload::PlainDateTime(v) => Ok(v),
            TemporalPayload::ZonedDateTime(zdt) => Ok(zdt.to_plain_date_time()),
            _ => Err(NativeError::TypeError {
                name: CLASS,
                reason: "argument must be a Temporal.PlainDateTime".to_string(),
            }),
        }
    } else if let Some(obj) = v.as_object() {
        let fields = parse_date_time_fields(ctx, obj, CLASS)?;
        let calendar = read_calendar_field(obj, ctx.heap(), CLASS)?;
        let partial = temporal_rs::partial::PartialDateTime { fields, calendar };
        temporal_rs::PlainDateTime::from_partial(partial, overflow)
            .map_err(|e| temporal_err(e, CLASS))
    } else if let Some(s) = v.as_string(ctx.heap()) {
        temporal_rs::PlainDateTime::from_utf8(s.to_lossy_string(ctx.heap()).as_bytes())
            .map_err(|e| temporal_err(e, CLASS))
    } else {
        Err(NativeError::TypeError {
            name: CLASS,
            reason:
                "argument must be a Temporal.PlainDateTime, ISO string, or date-time-like object"
                    .to_string(),
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
        "yearOfWeek" => pdt
            .year_of_week()
            .map_or(Value::undefined(), Value::number_i32),
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

fn duration_arg(ctx: &mut NativeCtx<'_>, v: &Value) -> Result<temporal_rs::Duration, NativeError> {
    if let Some(t) = v.as_temporal(ctx.heap()) {
        match t.payload_clone(ctx.heap()) {
            TemporalPayload::Duration(d) => Ok(d),
            _ => Err(NativeError::TypeError {
                name: CLASS,
                reason: "must be a Temporal.Duration".to_string(),
            }),
        }
    } else if let Some(obj) = v.as_object() {
        partial_from_object(ctx, &obj)
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

fn impl_to_string(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let pdt = require_plain_date_time(ctx)?;
    let display = parse_display_calendar(args, 0, ctx, CLASS)?;
    let rounding = parse_to_string_rounding_options(args, 0, ctx, CLASS)?;
    let s = pdt
        .to_ixdtf_string(rounding, display)
        .map_err(|e| temporal_err(e, CLASS))?;
    js_string_value(s, ctx)
}

fn impl_to_json(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    impl_to_string(ctx, args)
}

fn impl_to_zoned_date_time(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let pdt = require_plain_date_time(ctx)?;
    let tz = parse_time_zone(&arg_or_undef(args, 0), ctx.heap(), CLASS)?;
    let disambiguation = parse_disambiguation(args, 1, ctx, CLASS)?;
    let zdt = pdt
        .to_zoned_date_time(tz, disambiguation)
        .map_err(|e| temporal_err(e, CLASS))?;
    make_temporal(ctx, TemporalPayload::ZonedDateTime(zdt))
}

fn impl_value_of(_ctx: &mut NativeCtx<'_>, _args: &[Value]) -> Result<Value, NativeError> {
    Err(NativeError::TypeError {
        name: CLASS,
        reason: "Temporal.PlainDateTime has no `.valueOf` — use `compare` or `equals`".to_string(),
    })
}

fn impl_add(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let pdt = require_plain_date_time(ctx)?;
    let dur = duration_arg(ctx, &arg_or_undef(args, 0))?;
    let overflow = parse_overflow(ctx, args, 1)?;
    let result = pdt
        .add(&dur, overflow)
        .map_err(|e| temporal_err(e, CLASS))?;
    make_temporal(ctx, TemporalPayload::PlainDateTime(result))
}

fn impl_subtract(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let pdt = require_plain_date_time(ctx)?;
    let dur = duration_arg(ctx, &arg_or_undef(args, 0))?;
    let overflow = parse_overflow(ctx, args, 1)?;
    let result = pdt
        .subtract(&dur, overflow)
        .map_err(|e| temporal_err(e, CLASS))?;
    make_temporal(ctx, TemporalPayload::PlainDateTime(result))
}

fn impl_equals(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let pdt = require_plain_date_time(ctx)?;
    let other = parse_plain_date_time_arg(ctx, &arg_or_undef(args, 0))?;
    Ok(Value::boolean(pdt == other))
}

fn impl_until(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let pdt = require_plain_date_time(ctx)?;
    let other = parse_plain_date_time_arg(ctx, &arg_or_undef(args, 0))?;
    let settings = parse_difference_settings(args, 1, ctx, CLASS)?;
    let result = pdt
        .until(&other, settings)
        .map_err(|e| temporal_err(e, CLASS))?;
    make_temporal(ctx, TemporalPayload::Duration(result))
}

fn impl_since(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let pdt = require_plain_date_time(ctx)?;
    let other = parse_plain_date_time_arg(ctx, &arg_or_undef(args, 0))?;
    let settings = parse_difference_settings(args, 1, ctx, CLASS)?;
    let result = pdt
        .since(&other, settings)
        .map_err(|e| temporal_err(e, CLASS))?;
    make_temporal(ctx, TemporalPayload::Duration(result))
}

fn impl_round(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let pdt = require_plain_date_time(ctx)?;
    let options = parse_rounding_options(args, 0, ctx, CLASS)?;
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
    let fields = parse_date_time_fields(ctx, obj, CLASS)?;
    let overflow = parse_overflow(ctx, args, 1)?;
    let result = pdt
        .with(fields, overflow)
        .map_err(|e| temporal_err(e, CLASS))?;
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
        let partial = parse_partial_time(ctx, obj, CLASS)?;
        temporal_rs::PlainTime::default()
            .with(partial, None)
            .map_err(|e| temporal_err(e, CLASS))?
    } else if let Some(s) = arg.as_string(ctx.heap()) {
        temporal_rs::PlainTime::from_utf8(s.to_lossy_string(ctx.heap()).as_bytes())
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

/// Generate a `Temporal.PlainDateTime.prototype` accessor getter,
/// re-validating the receiver via [`require_plain_date_time`]
/// (branding `TypeError`). The heap arm exposes `&mut GcHeap`.
macro_rules! plain_date_time_getter {
    ($fn:ident, $pdt:ident => $val:expr) => {
        fn $fn(ctx: &mut NativeCtx<'_>, _args: &[Value]) -> Result<Value, NativeError> {
            let $pdt = require_plain_date_time(ctx)?;
            Ok($val)
        }
    };
    ($fn:ident, $pdt:ident, $heap:ident => $val:expr) => {
        fn $fn(ctx: &mut NativeCtx<'_>, _args: &[Value]) -> Result<Value, NativeError> {
            let $pdt = require_plain_date_time(ctx)?;
            let $heap = ctx.heap_mut();
            Ok($val)
        }
    };
}

plain_date_time_getter!(get_year, pdt => Value::number_i32(pdt.year()));
plain_date_time_getter!(get_month, pdt => Value::number_i32(pdt.month() as i32));
plain_date_time_getter!(get_month_code, pdt, heap => str_or_undef(pdt.month_code().as_str(), heap));
plain_date_time_getter!(get_day, pdt => Value::number_i32(pdt.day() as i32));
plain_date_time_getter!(get_hour, pdt => Value::number_i32(pdt.hour() as i32));
plain_date_time_getter!(get_minute, pdt => Value::number_i32(pdt.minute() as i32));
plain_date_time_getter!(get_second, pdt => Value::number_i32(pdt.second() as i32));
plain_date_time_getter!(get_millisecond, pdt => Value::number_i32(pdt.millisecond() as i32));
plain_date_time_getter!(get_microsecond, pdt => Value::number_i32(pdt.microsecond() as i32));
plain_date_time_getter!(get_nanosecond, pdt => Value::number_i32(pdt.nanosecond() as i32));
plain_date_time_getter!(get_day_of_week, pdt => Value::number_i32(pdt.day_of_week() as i32));
plain_date_time_getter!(get_day_of_year, pdt => Value::number_i32(pdt.day_of_year() as i32));
plain_date_time_getter!(get_week_of_year, pdt => pdt
    .week_of_year()
    .map_or(Value::undefined(), |w| Value::number_i32(w as i32)));
plain_date_time_getter!(get_year_of_week, pdt => pdt
    .year_of_week()
    .map_or(Value::undefined(), Value::number_i32));
plain_date_time_getter!(get_days_in_week, pdt => Value::number_i32(pdt.days_in_week() as i32));
plain_date_time_getter!(get_days_in_month, pdt => Value::number_i32(pdt.days_in_month() as i32));
plain_date_time_getter!(get_days_in_year, pdt => Value::number_i32(pdt.days_in_year() as i32));
plain_date_time_getter!(get_months_in_year, pdt => Value::number_i32(pdt.months_in_year() as i32));
plain_date_time_getter!(get_in_leap_year, pdt => Value::boolean(pdt.in_leap_year()));
plain_date_time_getter!(get_era, pdt, heap => pdt
    .era()
    .map_or(Value::undefined(), |era| str_or_undef(era.as_str(), heap)));
plain_date_time_getter!(get_era_year, pdt => pdt
    .era_year()
    .map_or(Value::undefined(), Value::number_i32));
plain_date_time_getter!(get_calendar_id, pdt, heap => str_or_undef(pdt.calendar().identifier(), heap));

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
    method("toZonedDateTime", 1, impl_to_zoned_date_time),
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
        accessors = [
            ("calendarId",   get = get_calendar_id),
            ("era",          get = get_era),
            ("eraYear",      get = get_era_year),
            ("year",         get = get_year),
            ("month",        get = get_month),
            ("monthCode",    get = get_month_code),
            ("day",          get = get_day),
            ("hour",         get = get_hour),
            ("minute",       get = get_minute),
            ("second",       get = get_second),
            ("millisecond",  get = get_millisecond),
            ("microsecond",  get = get_microsecond),
            ("nanosecond",   get = get_nanosecond),
            ("dayOfWeek",    get = get_day_of_week),
            ("dayOfYear",    get = get_day_of_year),
            ("weekOfYear",   get = get_week_of_year),
            ("yearOfWeek",   get = get_year_of_week),
            ("daysInWeek",   get = get_days_in_week),
            ("daysInMonth",  get = get_days_in_month),
            ("daysInYear",   get = get_days_in_year),
            ("monthsInYear", get = get_months_in_year),
            ("inLeapYear",   get = get_in_leap_year),
        ],
    },
    install_on = crate::temporal::native_dispatch::temporal_host,
}
