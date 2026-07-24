//! `Temporal.PlainDate` — calendar date `YYYY-MM-DD`.
//!
//! # See also
//! - <https://tc39.es/proposal-temporal/#sec-temporal-plaindate-objects>

#![allow(missing_docs)]

use crate::js_surface::{Attr, MethodSpec};
use crate::native_function::NativeCall;
use crate::temporal::duration::partial_from_object;
use crate::temporal::helpers::{
    arg_or_undef, arg_to_calendar, clamp_to_u8, get_option_value, js_string_value, make_temporal,
    parse_calendar_fields, parse_difference_settings, parse_display_calendar, parse_overflow,
    parse_partial_time, parse_time_zone, read_calendar_field, require_construct,
    require_plain_date, str_or_undef, temporal_err, to_integer_with_truncation,
};
use crate::temporal::payload::{JsTemporal, TemporalPayload};
use crate::{NativeCtx, NativeError, Value};

const CLASS: &str = "Temporal.PlainDate";

pub fn construct(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    require_construct(ctx, CLASS)?;
    let year = to_integer_with_truncation(ctx, &arg_or_undef(args, 0), CLASS, "isoYear")? as i32;
    let month_f = to_integer_with_truncation(ctx, &arg_or_undef(args, 1), CLASS, "isoMonth")?;
    let day_f = to_integer_with_truncation(ctx, &arg_or_undef(args, 2), CLASS, "isoDay")?;
    let calendar = arg_to_calendar(args, 3, ctx.heap(), CLASS)?;
    let month = clamp_to_u8(month_f, CLASS, "isoMonth")?;
    let day = clamp_to_u8(day_f, CLASS, "isoDay")?;
    let pd = temporal_rs::PlainDate::try_new(year, month, day, calendar)
        .map_err(|e| temporal_err(e, CLASS))?;
    make_temporal(ctx, TemporalPayload::PlainDate(pd))
}

fn from(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let pd = parse_plain_date_arg_with_overflow(ctx, &arg_or_undef(args, 0), Some(args))?;
    make_temporal(ctx, TemporalPayload::PlainDate(pd))
}

fn compare(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let a = parse_plain_date_arg(ctx, &arg_or_undef(args, 0))?;
    let b = parse_plain_date_arg(ctx, &arg_or_undef(args, 1))?;
    let n = match a.compare_iso(&b) {
        std::cmp::Ordering::Less => -1,
        std::cmp::Ordering::Equal => 0,
        std::cmp::Ordering::Greater => 1,
    };
    Ok(Value::number_i32(n))
}

pub(crate) fn parse_plain_date_arg(
    ctx: &mut NativeCtx<'_>,
    v: &Value,
) -> Result<temporal_rs::PlainDate, NativeError> {
    parse_plain_date_arg_with_overflow(ctx, v, None)
}

/// §ToTemporalDate. `overflow_opts`, when `Some(args)`, is the native
/// argument slice whose index-1 element carries the options object;
/// `GetTemporalOverflowOption` is then performed at the spec-mandated
/// point (after the primary argument is recognised / its fields are
/// read / its ISO string is parsed), so a primitive that fails the
/// type check or an ISO-invalid string rejects before the option is
/// observed. `None` skips overflow entirely (e.g. `compare`).
pub(crate) fn parse_plain_date_arg_with_overflow(
    ctx: &mut NativeCtx<'_>,
    v: &Value,
    overflow_opts: Option<&[Value]>,
) -> Result<temporal_rs::PlainDate, NativeError> {
    // A Temporal instance with date slots converts directly
    // (PlainDateTime / ZonedDateTime project onto their calendar
    // date); a plain object is read as a calendar-date property bag;
    // a string is parsed as ISO.
    if let Some(t) = v.as_temporal(ctx.heap()) {
        let pd = match t.payload_clone(ctx.heap()) {
            TemporalPayload::PlainDate(v) => v,
            TemporalPayload::PlainDateTime(pdt) => pdt.to_plain_date(),
            TemporalPayload::ZonedDateTime(zdt) => zdt.to_plain_date(),
            _ => {
                return Err(NativeError::TypeError {
                    name: CLASS,
                    reason: "argument must be a Temporal.PlainDate".to_string(),
                });
            }
        };
        if let Some(args) = overflow_opts {
            parse_overflow(ctx, args, 1)?;
        }
        Ok(pd)
    } else if v.is_object_type() {
        let calendar = read_calendar_field(ctx, *v, CLASS)?;
        let calendar_fields = parse_calendar_fields(ctx, *v, &calendar, CLASS)?;
        let overflow = match overflow_opts {
            Some(args) => parse_overflow(ctx, args, 1)?,
            None => None,
        };
        let partial = temporal_rs::partial::PartialDate {
            calendar_fields,
            calendar,
        };
        temporal_rs::PlainDate::from_partial(partial, overflow).map_err(|e| temporal_err(e, CLASS))
    } else if let Some(s) = v.as_string(ctx.heap()) {
        let pd = temporal_rs::PlainDate::from_utf8(s.to_lossy_string(ctx.heap()).as_bytes())
            .map_err(|e| temporal_err(e, CLASS))?;
        if let Some(args) = overflow_opts {
            parse_overflow(ctx, args, 1)?;
        }
        Ok(pd)
    } else {
        Err(NativeError::TypeError {
            name: CLASS,
            reason: "argument must be a Temporal.PlainDate, ISO string, or date-like object"
                .to_string(),
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
        "yearOfWeek" => pd
            .year_of_week()
            .map_or(Value::undefined(), Value::number_i32),
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

fn impl_to_string(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let pd = require_plain_date(ctx)?;
    let display = parse_display_calendar(args, 0, ctx, CLASS)?;
    let s = pd.to_ixdtf_string(display);
    js_string_value(s, ctx)
}

fn impl_to_json(ctx: &mut NativeCtx<'_>, _args: &[Value]) -> Result<Value, NativeError> {
    impl_to_string(ctx, &[])
}

fn impl_value_of(_ctx: &mut NativeCtx<'_>, _args: &[Value]) -> Result<Value, NativeError> {
    Err(NativeError::TypeError {
        name: CLASS,
        reason: "Temporal.PlainDate has no `.valueOf` — use `compare` or `equals`".to_string(),
    })
}

fn impl_add(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let pd = require_plain_date(ctx)?;
    let dur = duration_arg(ctx, &arg_or_undef(args, 0))?;
    let overflow = parse_overflow(ctx, args, 1)?;
    let result = pd.add(&dur, overflow).map_err(|e| temporal_err(e, CLASS))?;
    make_temporal(ctx, TemporalPayload::PlainDate(result))
}

fn impl_subtract(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let pd = require_plain_date(ctx)?;
    let dur = duration_arg(ctx, &arg_or_undef(args, 0))?;
    let overflow = parse_overflow(ctx, args, 1)?;
    let result = pd
        .subtract(&dur, overflow)
        .map_err(|e| temporal_err(e, CLASS))?;
    make_temporal(ctx, TemporalPayload::PlainDate(result))
}

fn impl_equals(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let pd = require_plain_date(ctx)?;
    let other = parse_plain_date_arg(ctx, &arg_or_undef(args, 0))?;
    Ok(Value::boolean(
        pd.compare_iso(&other) == std::cmp::Ordering::Equal,
    ))
}

fn impl_until(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let pd = require_plain_date(ctx)?;
    let other = parse_plain_date_arg(ctx, &arg_or_undef(args, 0))?;
    let settings = parse_difference_settings(args, 1, ctx, CLASS)?;
    let result = pd
        .until(&other, settings)
        .map_err(|e| temporal_err(e, CLASS))?;
    make_temporal(ctx, TemporalPayload::Duration(result))
}

fn impl_since(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let pd = require_plain_date(ctx)?;
    let other = parse_plain_date_arg(ctx, &arg_or_undef(args, 0))?;
    let settings = parse_difference_settings(args, 1, ctx, CLASS)?;
    let result = pd
        .since(&other, settings)
        .map_err(|e| temporal_err(e, CLASS))?;
    make_temporal(ctx, TemporalPayload::Duration(result))
}

fn impl_with(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let pd = require_plain_date(ctx)?;
    let arg = arg_or_undef(args, 0);
    // §RejectObjectWithCalendarOrTimeZone: the argument must be a plain
    // fields object, not a Temporal instance.
    if !arg.is_object_type() || arg.as_temporal(ctx.heap()).is_some() {
        return Err(NativeError::TypeError {
            name: CLASS,
            reason: "first argument must be a plain object".to_string(),
        });
    }
    // §GetOptionsObject — a non-object, non-undefined options argument
    // is a TypeError before the fields object is processed.
    let options = arg_or_undef(args, 1);
    if !options.is_undefined() && !options.is_object_type() {
        return Err(NativeError::TypeError {
            name: CLASS,
            reason: "options must be an object or undefined".to_string(),
        });
    }
    crate::temporal::helpers::reject_temporal_like_keys(ctx, arg, CLASS)?;
    let calendar = pd.calendar().clone();
    let fields = parse_calendar_fields(ctx, arg, &calendar, CLASS)?;
    if crate::temporal::helpers::calendar_fields_empty(&fields) {
        return Err(NativeError::TypeError {
            name: CLASS,
            reason: "with() requires at least one recognized field".to_string(),
        });
    }
    let overflow = parse_overflow(ctx, args, 1)?;
    let result = pd
        .with(fields, overflow)
        .map_err(|e| temporal_err(e, CLASS))?;
    make_temporal(ctx, TemporalPayload::PlainDate(result))
}

fn impl_with_calendar(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let pd = require_plain_date(ctx)?;
    let calendar =
        crate::temporal::helpers::to_calendar_slot_value(ctx, &arg_or_undef(args, 0), CLASS)?;
    let result = pd.with_calendar(calendar);
    make_temporal(ctx, TemporalPayload::PlainDate(result))
}

fn impl_to_plain_date_time(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let pd = require_plain_date(ctx)?;
    let v = arg_or_undef(args, 0);
    let time = if v.is_undefined() {
        None
    } else if let Some(t) = v.as_temporal(ctx.heap()) {
        // §ToTemporalTime: PlainDateTime / ZonedDateTime contribute
        // their wall-clock time component.
        match t.payload_clone(ctx.heap()) {
            TemporalPayload::PlainTime(pt) => Some(pt),
            TemporalPayload::PlainDateTime(pdt) => Some(pdt.to_plain_time()),
            TemporalPayload::ZonedDateTime(zdt) => Some(zdt.to_plain_time()),
            _ => {
                return Err(NativeError::TypeError {
                    name: CLASS,
                    reason: "must be a Temporal.PlainTime or partial-time object".to_string(),
                });
            }
        }
    } else if v.is_object_type() {
        let partial = parse_partial_time(ctx, v, CLASS)?;
        let pt = temporal_rs::PlainTime::default()
            .with(partial, None)
            .map_err(|e| temporal_err(e, CLASS))?;
        Some(pt)
    } else if let Some(s) = v.as_string(ctx.heap()) {
        let pt = temporal_rs::PlainTime::from_utf8(s.to_lossy_string(ctx.heap()).as_bytes())
            .map_err(|e| temporal_err(e, CLASS))?;
        Some(pt)
    } else {
        return Err(NativeError::TypeError {
            name: CLASS,
            reason: "first argument must be an object, string, or undefined".to_string(),
        });
    };
    let pdt = pd
        .to_plain_date_time(time)
        .map_err(|e| temporal_err(e, CLASS))?;
    make_temporal(ctx, TemporalPayload::PlainDateTime(pdt))
}

fn impl_to_zoned_date_time(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let pd = require_plain_date(ctx)?;
    let item = arg_or_undef(args, 0);
    // §ToZonedDateTime: an object with a `timeZone` property supplies
    // {timeZone, plainTime}; any other value is itself the time-zone
    // identifier (string or ZonedDateTime).
    let (tz, time) = if item.is_object_type() {
        // §ToZonedDateTime reads `timeZone` then `plainTime` through
        // observable [[Get]]s (accessor getters / Proxy traps fire).
        let tz_v = get_option_value(ctx, item, "timeZone", CLASS)?;
        if tz_v.is_undefined() {
            (parse_time_zone(&item, ctx.heap(), CLASS)?, None)
        } else {
            let tz = parse_time_zone(&tz_v, ctx.heap(), CLASS)?;
            let time_v = get_option_value(ctx, item, "plainTime", CLASS)?;
            let time = if time_v.is_undefined() {
                None
            } else {
                Some(crate::temporal::plain_time::parse_plain_time_arg(ctx, &time_v)?)
            };
            (tz, time)
        }
    } else {
        (parse_time_zone(&item, ctx.heap(), CLASS)?, None)
    };
    let zdt = pd
        .to_zoned_date_time(tz, time)
        .map_err(|e| temporal_err(e, CLASS))?;
    make_temporal(ctx, TemporalPayload::ZonedDateTime(zdt))
}

fn impl_to_plain_year_month(
    ctx: &mut NativeCtx<'_>,
    _args: &[Value],
) -> Result<Value, NativeError> {
    let pd = require_plain_date(ctx)?;
    let pym = pd
        .to_plain_year_month()
        .map_err(|e| temporal_err(e, CLASS))?;
    make_temporal(ctx, TemporalPayload::PlainYearMonth(pym))
}

fn impl_to_plain_month_day(ctx: &mut NativeCtx<'_>, _args: &[Value]) -> Result<Value, NativeError> {
    let pd = require_plain_date(ctx)?;
    let pmd = pd
        .to_plain_month_day()
        .map_err(|e| temporal_err(e, CLASS))?;
    make_temporal(ctx, TemporalPayload::PlainMonthDay(pmd))
}

/// Generate a `Temporal.PlainDate.prototype` accessor getter,
/// re-validating the receiver via [`require_plain_date`] (branding
/// `TypeError`). The heap arm exposes `&mut GcHeap` for string fields.
macro_rules! plain_date_getter {
    ($fn:ident, $pd:ident => $val:expr) => {
        fn $fn(ctx: &mut NativeCtx<'_>, _args: &[Value]) -> Result<Value, NativeError> {
            let $pd = require_plain_date(ctx)?;
            Ok($val)
        }
    };
    ($fn:ident, $pd:ident, $heap:ident => $val:expr) => {
        fn $fn(ctx: &mut NativeCtx<'_>, _args: &[Value]) -> Result<Value, NativeError> {
            let $pd = require_plain_date(ctx)?;
            let $heap = ctx.heap_mut();
            Ok($val)
        }
    };
}

plain_date_getter!(get_year, pd => Value::number_i32(pd.year()));
plain_date_getter!(get_month, pd => Value::number_i32(pd.month() as i32));
plain_date_getter!(get_month_code, pd, heap => str_or_undef(pd.month_code().as_str(), heap));
plain_date_getter!(get_day, pd => Value::number_i32(pd.day() as i32));
plain_date_getter!(get_day_of_week, pd => Value::number_i32(pd.day_of_week() as i32));
plain_date_getter!(get_day_of_year, pd => Value::number_i32(pd.day_of_year() as i32));
plain_date_getter!(get_week_of_year, pd => pd
    .week_of_year()
    .map_or(Value::undefined(), |w| Value::number_i32(w as i32)));
plain_date_getter!(get_year_of_week, pd => pd
    .year_of_week()
    .map_or(Value::undefined(), Value::number_i32));
plain_date_getter!(get_days_in_week, pd => Value::number_i32(pd.days_in_week() as i32));
plain_date_getter!(get_days_in_month, pd => Value::number_i32(pd.days_in_month() as i32));
plain_date_getter!(get_days_in_year, pd => Value::number_i32(pd.days_in_year() as i32));
plain_date_getter!(get_months_in_year, pd => Value::number_i32(pd.months_in_year() as i32));
plain_date_getter!(get_in_leap_year, pd => Value::boolean(pd.in_leap_year()));
plain_date_getter!(get_era, pd, heap => pd
    .era()
    .map_or(Value::undefined(), |era| str_or_undef(era.as_str(), heap)));
plain_date_getter!(get_era_year, pd => pd.era_year().map_or(Value::undefined(), Value::number_i32));
plain_date_getter!(get_calendar_id, pd, heap => str_or_undef(pd.calendar().identifier(), heap));

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
    method("toLocaleString", 0, impl_to_locale_string),
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
    method("toPlainYearMonth", 0, impl_to_plain_year_month),
    method("toPlainMonthDay", 0, impl_to_plain_month_day),
    method("toZonedDateTime", 1, impl_to_zoned_date_time),
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
        accessors = [
            ("calendarId",  get = get_calendar_id),
            ("era",         get = get_era),
            ("eraYear",     get = get_era_year),
            ("year",        get = get_year),
            ("month",       get = get_month),
            ("monthCode",   get = get_month_code),
            ("day",         get = get_day),
            ("dayOfWeek",   get = get_day_of_week),
            ("dayOfYear",   get = get_day_of_year),
            ("weekOfYear",  get = get_week_of_year),
            ("yearOfWeek",  get = get_year_of_week),
            ("daysInWeek",  get = get_days_in_week),
            ("daysInMonth", get = get_days_in_month),
            ("daysInYear",  get = get_days_in_year),
            ("monthsInYear", get = get_months_in_year),
            ("inLeapYear",  get = get_in_leap_year),
        ],
    },
    install_on = crate::temporal::native_dispatch::temporal_host,
    string_tag = "Temporal.PlainDate",
}

/// §sec-temporal.*.prototype.tolocalestring — brand-checks the receiver,
/// then (absent the Intl formatting data path) renders the same canonical
/// string as `toString`.
fn impl_to_locale_string(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    // §Temporal.*.prototype.toLocaleString renders through a freshly
    // constructed DateTimeFormat so the output matches
    // `new Intl.DateTimeFormat(locales, options).format(this)`.
    let _ = require_plain_date(ctx)?;
    let receiver = *ctx.this_value();
    crate::intl::date_time_format::temporal_to_locale_string(
        ctx,
        receiver,
        crate::temporal::helpers::arg_or_undef(args, 0),
        crate::temporal::helpers::arg_or_undef(args, 1),
    )
}
