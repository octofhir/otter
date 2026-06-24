//! `Temporal.ZonedDateTime` — instant + IANA time zone + calendar.
//!
//! Built on the `compiled_data` `temporal_rs` feature so every
//! `*_with_provider` method resolves against the bundled tzdb
//! through the crate-internal `TZ_PROVIDER` singleton.
//!
//! # See also
//! - <https://tc39.es/proposal-temporal/#sec-temporal-zoneddatetime-objects>

#![allow(missing_docs)]

use num_traits::ToPrimitive;

use crate::bigint::BigIntValue;
use crate::js_surface::{Attr, MethodSpec};
use crate::native_function::NativeCall;
use crate::string::JsString;
use crate::temporal::duration::partial_from_object;
use crate::temporal::helpers::parse_to_string_rounding_options;
use crate::temporal::helpers::read_calendar_field;
use crate::temporal::helpers::{
    arg_or_undef, arg_to_calendar, js_string_value, make_temporal, parse_calendar_fields,
    parse_difference_settings, parse_overflow, parse_partial_time, parse_rounding_options,
    parse_time_zone, read_option_string, require_construct, require_zoned_date_time, str_or_undef,
    temporal_err,
};
use crate::temporal::payload::{JsTemporal, TemporalPayload};
use crate::{NativeCtx, NativeError, Value};

const CLASS: &str = "Temporal.ZonedDateTime";

pub fn construct(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    require_construct(ctx, CLASS)?;
    let Some(bi) = arg_or_undef(args, 0).as_big_int() else {
        return Err(NativeError::TypeError {
            name: CLASS,
            reason: "epochNanoseconds must be a BigInt".to_string(),
        });
    };
    let nanos = bi
        .with_inner(ctx.heap(), |big| big.to_i128())
        .ok_or_else(|| NativeError::RangeError {
            name: CLASS,
            reason: "epochNanoseconds out of i128 range".to_string(),
        })?;
    let Some(tz_str) = arg_or_undef(args, 1).as_string(ctx.heap()) else {
        return Err(NativeError::TypeError {
            name: CLASS,
            reason: "timeZoneIdentifier must be a string".to_string(),
        });
    };
    let tz_text = tz_str.to_lossy_string(ctx.heap());
    let time_zone =
        temporal_rs::TimeZone::try_from_str(&tz_text).map_err(|e| temporal_err(e, CLASS))?;
    let calendar = arg_to_calendar(args, 2, ctx.heap(), CLASS)?;
    let zdt = temporal_rs::ZonedDateTime::try_new(nanos, time_zone, calendar)
        .map_err(|e| temporal_err(e, CLASS))?;
    make_temporal(ctx, TemporalPayload::ZonedDateTime(zdt))
}

fn from(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let zdt = parse_zdt_arg_with_options(ctx, &arg_or_undef(args, 0), Some(args))?;
    make_temporal(ctx, TemporalPayload::ZonedDateTime(zdt))
}

fn compare(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let a = parse_zdt_arg(ctx, &arg_or_undef(args, 0))?;
    let b = parse_zdt_arg(ctx, &arg_or_undef(args, 1))?;
    let n = match a.compare_instant(&b) {
        std::cmp::Ordering::Less => -1,
        std::cmp::Ordering::Equal => 0,
        std::cmp::Ordering::Greater => 1,
    };
    Ok(Value::number_i32(n))
}

pub(crate) fn parse_zdt_arg(
    ctx: &mut NativeCtx<'_>,
    v: &Value,
) -> Result<temporal_rs::ZonedDateTime, NativeError> {
    parse_zdt_arg_with_options(ctx, v, None)
}

/// §ToTemporalZonedDateTime. `options_args`, when `Some(args)`, carries
/// the native arguments whose index-1 element is the options object;
/// the `disambiguation`, `offset`, and `overflow` options are read from
/// it (after the bag fields). Other callers (compare/since/until/with,
/// `relativeTo`) pass `None` and use the defaults.
pub(crate) fn parse_zdt_arg_with_options(
    ctx: &mut NativeCtx<'_>,
    v: &Value,
    options_args: Option<&[Value]>,
) -> Result<temporal_rs::ZonedDateTime, NativeError> {
    if let Some(t) = v.as_temporal(ctx.heap()) {
        let zdt = match t.payload_clone(ctx.heap()) {
            TemporalPayload::ZonedDateTime(v) => v,
            _ => {
                return Err(NativeError::TypeError {
                    name: CLASS,
                    reason: "argument must be a Temporal.ZonedDateTime".to_string(),
                });
            }
        };
        // §step 2.c still reads (and validates) the options.
        read_zdt_options(ctx, options_args)?;
        return Ok(zdt);
    }
    if v.is_object_type() {
        // §ToTemporalZonedDateTime property bag: `timeZone` is
        // required; calendar/time fields and `offset` are optional.
        // Reads fire through getter/Proxy-aware [[Get]]s.
        let calendar = read_calendar_field(ctx, *v, CLASS)?;
        let calendar_fields = parse_calendar_fields(ctx, *v, &calendar, CLASS)?;
        let offset = crate::temporal::helpers::read_required_string(ctx, *v, "offset", CLASS)?
            .map(|s| {
                temporal_rs::UtcOffset::from_utf8(s.as_bytes()).map_err(|e| temporal_err(e, CLASS))
            })
            .transpose()?;
        let tz_v = crate::temporal::helpers::get_option_value(ctx, *v, "timeZone", CLASS)?;
        if tz_v.is_undefined() {
            return Err(NativeError::TypeError {
                name: CLASS,
                reason: "object must have a timeZone property".to_string(),
            });
        }
        let tz = parse_time_zone(&tz_v, ctx.heap(), CLASS)?;
        let time = parse_partial_time(ctx, *v, CLASS)?;
        let (overflow, disambiguation, offset_option) = read_zdt_options(ctx, options_args)?;
        let mut partial = temporal_rs::partial::PartialZonedDateTime::new()
            .with_calendar_fields(calendar_fields)
            .with_time(time)
            .with_timezone(Some(tz));
        partial.calendar = calendar;
        if let Some(o) = offset {
            partial = partial.with_offset(o);
        }
        return temporal_rs::ZonedDateTime::from_partial(
            partial,
            overflow,
            disambiguation,
            offset_option,
        )
        .map_err(|e| temporal_err(e, CLASS));
    }
    if let Some(s) = v.as_string(ctx.heap()) {
        let text = s.to_lossy_string(ctx.heap());
        // §ToTemporalZonedDateTime parses the string before reading the
        // options, so an ISO-invalid string rejects before any option is
        // observed. Validate the parse first, then read the options and
        // produce the final result with them applied.
        temporal_rs::ZonedDateTime::from_utf8(
            text.as_bytes(),
            temporal_rs::options::Disambiguation::Compatible,
            temporal_rs::options::OffsetDisambiguation::Reject,
        )
        .map_err(|e| temporal_err(e, CLASS))?;
        let (_, disambiguation, offset_option) = read_zdt_options(ctx, options_args)?;
        return temporal_rs::ZonedDateTime::from_utf8(
            text.as_bytes(),
            disambiguation.unwrap_or(temporal_rs::options::Disambiguation::Compatible),
            offset_option.unwrap_or(temporal_rs::options::OffsetDisambiguation::Reject),
        )
        .map_err(|e| temporal_err(e, CLASS));
    }
    Err(NativeError::TypeError {
        name: CLASS,
        reason: "argument must be a Temporal.ZonedDateTime, ISO string, or object with a timeZone"
            .to_string(),
    })
}

/// Read the `disambiguation`, `offset`, and `overflow` options from a
/// `ZonedDateTime.from` options argument (`options_args[1]`), each via a
/// getter/Proxy-aware [[Get]] and validated against its enum.
#[allow(clippy::type_complexity)]
fn read_zdt_options(
    ctx: &mut NativeCtx<'_>,
    options_args: Option<&[Value]>,
) -> Result<
    (
        Option<temporal_rs::options::Overflow>,
        Option<temporal_rs::options::Disambiguation>,
        Option<temporal_rs::options::OffsetDisambiguation>,
    ),
    NativeError,
> {
    use core::str::FromStr;
    let Some(args) = options_args else {
        return Ok((None, None, None));
    };
    let v = arg_or_undef(args, 1);
    if v.is_undefined() {
        return Ok((None, None, None));
    }
    if !v.is_object_type() {
        return Err(NativeError::TypeError {
            name: CLASS,
            reason: "options must be an object or undefined".to_string(),
        });
    }
    let disambiguation = match read_option_string(ctx, v, "disambiguation", CLASS)? {
        Some(name) => Some(
            temporal_rs::options::Disambiguation::from_str(&name).map_err(|_| {
                NativeError::RangeError {
                    name: CLASS,
                    reason: "invalid `disambiguation`".to_string(),
                }
            })?,
        ),
        None => None,
    };
    let offset_option = match read_option_string(ctx, v, "offset", CLASS)? {
        Some(name) => Some(
            temporal_rs::options::OffsetDisambiguation::from_str(&name).map_err(|_| {
                NativeError::RangeError {
                    name: CLASS,
                    reason: "invalid `offset`".to_string(),
                }
            })?,
        ),
        None => None,
    };
    let overflow = parse_overflow(ctx, args, 1)?;
    Ok((overflow, disambiguation, offset_option))
}

pub fn load_property(temporal: JsTemporal, heap: &mut otter_gc::GcHeap, name: &str) -> Value {
    let zdt = match temporal.payload_clone(heap) {
        TemporalPayload::ZonedDateTime(v) => v,
        _ => return Value::undefined(),
    };
    match name {
        "year" => Value::number_i32(zdt.year()),
        "month" => Value::number_i32(zdt.month() as i32),
        "day" => Value::number_i32(zdt.day() as i32),
        "hour" => Value::number_i32(zdt.hour() as i32),
        "minute" => Value::number_i32(zdt.minute() as i32),
        "second" => Value::number_i32(zdt.second() as i32),
        "millisecond" => Value::number_i32(zdt.millisecond() as i32),
        "microsecond" => Value::number_i32(zdt.microsecond() as i32),
        "nanosecond" => Value::number_i32(zdt.nanosecond() as i32),
        "dayOfWeek" => Value::number_i32(zdt.day_of_week() as i32),
        "dayOfYear" => Value::number_i32(zdt.day_of_year() as i32),
        "daysInWeek" => Value::number_i32(zdt.days_in_week() as i32),
        "daysInMonth" => Value::number_i32(zdt.days_in_month() as i32),
        "daysInYear" => Value::number_i32(zdt.days_in_year() as i32),
        "monthsInYear" => Value::number_i32(zdt.months_in_year() as i32),
        "inLeapYear" => Value::boolean(zdt.in_leap_year()),
        "epochMilliseconds" => Value::number_f64(zdt.epoch_milliseconds() as f64),
        "offsetNanoseconds" => Value::number_f64(zdt.offset_nanoseconds() as f64),
        "epochNanoseconds" => match BigIntValue::from_i128(heap, zdt.epoch_nanoseconds().0) {
            Ok(b) => Value::big_int(b),
            Err(_) => Value::undefined(),
        },
        "offset" => match JsString::from_str(&zdt.offset(), heap) {
            Ok(js) => Value::string(js),
            Err(_) => Value::undefined(),
        },
        "timeZoneId" => {
            let id = zdt.time_zone().identifier().unwrap_or_default();
            match JsString::from_str(&id, heap) {
                Ok(js) => Value::string(js),
                Err(_) => Value::undefined(),
            }
        }
        "calendarId" => match JsString::from_str(zdt.calendar().identifier(), heap) {
            Ok(js) => Value::string(js),
            Err(_) => Value::undefined(),
        },
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
    let zdt = require_zoned_date_time(ctx)?;
    let rounding = parse_to_string_rounding_options(args, 0, ctx, CLASS)?;
    let s = zdt
        .to_ixdtf_string(
            temporal_rs::options::DisplayOffset::Auto,
            temporal_rs::options::DisplayTimeZone::Auto,
            temporal_rs::options::DisplayCalendar::Auto,
            rounding,
        )
        .map_err(|e| temporal_err(e, CLASS))?;
    js_string_value(s, ctx)
}

fn impl_to_json(ctx: &mut NativeCtx<'_>, _args: &[Value]) -> Result<Value, NativeError> {
    impl_to_string(ctx, &[])
}

fn impl_value_of(_ctx: &mut NativeCtx<'_>, _args: &[Value]) -> Result<Value, NativeError> {
    Err(NativeError::TypeError {
        name: CLASS,
        reason: "Temporal.ZonedDateTime has no `.valueOf` — use `compare` or `equals`".to_string(),
    })
}

fn impl_add(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let zdt = require_zoned_date_time(ctx)?;
    let dur = duration_arg(ctx, &arg_or_undef(args, 0))?;
    let overflow = parse_overflow(ctx, args, 1)?;
    let result = zdt
        .add(&dur, overflow)
        .map_err(|e| temporal_err(e, CLASS))?;
    make_temporal(ctx, TemporalPayload::ZonedDateTime(result))
}

fn impl_subtract(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let zdt = require_zoned_date_time(ctx)?;
    let dur = duration_arg(ctx, &arg_or_undef(args, 0))?;
    let overflow = parse_overflow(ctx, args, 1)?;
    let result = zdt
        .subtract(&dur, overflow)
        .map_err(|e| temporal_err(e, CLASS))?;
    make_temporal(ctx, TemporalPayload::ZonedDateTime(result))
}

fn impl_equals(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let zdt = require_zoned_date_time(ctx)?;
    let other = parse_zdt_arg(ctx, &arg_or_undef(args, 0))?;
    Ok(Value::boolean(
        zdt.equals(&other).map_err(|e| temporal_err(e, CLASS))?,
    ))
}

fn impl_until(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let zdt = require_zoned_date_time(ctx)?;
    let other = parse_zdt_arg(ctx, &arg_or_undef(args, 0))?;
    let settings = parse_difference_settings(args, 1, ctx, CLASS)?;
    let result = zdt
        .until(&other, settings)
        .map_err(|e| temporal_err(e, CLASS))?;
    make_temporal(ctx, TemporalPayload::Duration(result))
}

fn impl_since(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let zdt = require_zoned_date_time(ctx)?;
    let other = parse_zdt_arg(ctx, &arg_or_undef(args, 0))?;
    let settings = parse_difference_settings(args, 1, ctx, CLASS)?;
    let result = zdt
        .since(&other, settings)
        .map_err(|e| temporal_err(e, CLASS))?;
    make_temporal(ctx, TemporalPayload::Duration(result))
}

fn impl_round(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let zdt = require_zoned_date_time(ctx)?;
    let options = parse_rounding_options(args, 0, ctx, CLASS)?;
    let result = zdt.round(options).map_err(|e| temporal_err(e, CLASS))?;
    make_temporal(ctx, TemporalPayload::ZonedDateTime(result))
}

fn impl_start_of_day(ctx: &mut NativeCtx<'_>, _args: &[Value]) -> Result<Value, NativeError> {
    let zdt = require_zoned_date_time(ctx)?;
    let result = zdt.start_of_day().map_err(|e| temporal_err(e, CLASS))?;
    make_temporal(ctx, TemporalPayload::ZonedDateTime(result))
}

fn impl_with_calendar(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let zdt = require_zoned_date_time(ctx)?;
    let calendar =
        crate::temporal::helpers::to_calendar_slot_value(ctx, &arg_or_undef(args, 0), CLASS)?;
    let result = zdt.with_calendar(calendar);
    make_temporal(ctx, TemporalPayload::ZonedDateTime(result))
}

fn impl_with_time_zone(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let zdt = require_zoned_date_time(ctx)?;
    let Some(js) = arg_or_undef(args, 0).as_string(ctx.heap()) else {
        return Err(NativeError::TypeError {
            name: CLASS,
            reason: "timeZone must be a string".to_string(),
        });
    };
    let s = js.to_lossy_string(ctx.heap());
    let tz = temporal_rs::TimeZone::try_from_str(&s).map_err(|e| temporal_err(e, CLASS))?;
    let result = zdt.with_timezone(tz).map_err(|e| temporal_err(e, CLASS))?;
    make_temporal(ctx, TemporalPayload::ZonedDateTime(result))
}

fn impl_with(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let zdt = require_zoned_date_time(ctx)?;
    let arg = arg_or_undef(args, 0);
    if !arg.is_object_type() || arg.as_temporal(ctx.heap()).is_some() {
        return Err(NativeError::TypeError {
            name: CLASS,
            reason: "with() requires a plain ZonedDateTime-like object".to_string(),
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
    let calendar = zdt.calendar().clone();
    let calendar_fields = parse_calendar_fields(ctx, arg, &calendar, CLASS)?;
    let time = parse_partial_time(ctx, arg, CLASS)?;
    let fields = temporal_rs::fields::ZonedDateTimeFields {
        calendar_fields,
        time,
        offset: None,
    };
    let overflow = parse_overflow(ctx, args, 1)?;
    let result = zdt
        .with(fields, None, None, overflow)
        .map_err(|e| temporal_err(e, CLASS))?;
    make_temporal(ctx, TemporalPayload::ZonedDateTime(result))
}

fn impl_with_plain_time(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let zdt = require_zoned_date_time(ctx)?;
    let v = arg_or_undef(args, 0);
    let time = if v.is_undefined() {
        None
    } else {
        Some(crate::temporal::plain_time::parse_plain_time_arg(ctx, &v)?)
    };
    let result = zdt
        .with_plain_time(time)
        .map_err(|e| temporal_err(e, CLASS))?;
    make_temporal(ctx, TemporalPayload::ZonedDateTime(result))
}

fn impl_to_instant(ctx: &mut NativeCtx<'_>, _args: &[Value]) -> Result<Value, NativeError> {
    let zdt = require_zoned_date_time(ctx)?;
    make_temporal(ctx, TemporalPayload::Instant(zdt.to_instant()))
}

fn impl_to_plain_date(ctx: &mut NativeCtx<'_>, _args: &[Value]) -> Result<Value, NativeError> {
    let zdt = require_zoned_date_time(ctx)?;
    make_temporal(ctx, TemporalPayload::PlainDate(zdt.to_plain_date()))
}

fn impl_to_plain_time(ctx: &mut NativeCtx<'_>, _args: &[Value]) -> Result<Value, NativeError> {
    let zdt = require_zoned_date_time(ctx)?;
    make_temporal(ctx, TemporalPayload::PlainTime(zdt.to_plain_time()))
}

fn impl_to_plain_date_time(ctx: &mut NativeCtx<'_>, _args: &[Value]) -> Result<Value, NativeError> {
    let zdt = require_zoned_date_time(ctx)?;
    make_temporal(
        ctx,
        TemporalPayload::PlainDateTime(zdt.to_plain_date_time()),
    )
}

/// `Temporal.ZonedDateTime.prototype.getTimeZoneTransition` — the next or
/// previous instant at which the UTC offset of the receiver's time zone
/// changes. `directionParam` is required: a `"next"`/`"previous"` string,
/// or an options object carrying a `direction` property (§GetDirectionOption).
/// Returns a `ZonedDateTime` or `null` when there is no further transition
/// (e.g. an offset or transition-free zone).
fn impl_get_time_zone_transition(
    ctx: &mut NativeCtx<'_>,
    args: &[Value],
) -> Result<Value, NativeError> {
    use core::str::FromStr;
    let zdt = require_zoned_date_time(ctx)?;
    let param = arg_or_undef(args, 0);
    if param.is_undefined() {
        return Err(NativeError::TypeError {
            name: CLASS,
            reason: "getTimeZoneTransition: direction parameter is required".to_string(),
        });
    }
    // A bare string is treated as the `direction` directly; any other
    // value must be an options object whose `direction` is read and
    // coerced to a string (observable getter + toString).
    let dir_str = if let Some(s) = param.as_string(ctx.heap()) {
        s.to_lossy_string(ctx.heap())
    } else if let Some(obj) = param.as_object() {
        read_option_string(ctx, Value::object(obj), "direction", CLASS)?.ok_or_else(|| {
            NativeError::RangeError {
                name: CLASS,
                reason: "getTimeZoneTransition: `direction` option is required".to_string(),
            }
        })?
    } else {
        return Err(NativeError::TypeError {
            name: CLASS,
            reason: "getTimeZoneTransition: direction must be a string or options object"
                .to_string(),
        });
    };
    let direction =
        temporal_rs::provider::TransitionDirection::from_str(&dir_str).map_err(|_| {
            NativeError::RangeError {
                name: CLASS,
                reason: format!("getTimeZoneTransition: invalid direction `{dir_str}`"),
            }
        })?;
    match zdt
        .get_time_zone_transition(direction)
        .map_err(|e| temporal_err(e, CLASS))?
    {
        Some(next) => make_temporal(ctx, TemporalPayload::ZonedDateTime(next)),
        None => Ok(Value::null()),
    }
}

/// Generate a `Temporal.ZonedDateTime.prototype` accessor getter,
/// re-validating the receiver via [`require_zoned_date_time`]
/// (branding `TypeError`). The heap arm exposes `&mut GcHeap` for
/// string- and BigInt-valued fields.
macro_rules! zoned_date_time_getter {
    ($fn:ident, $zdt:ident => $val:expr) => {
        fn $fn(ctx: &mut NativeCtx<'_>, _args: &[Value]) -> Result<Value, NativeError> {
            let $zdt = require_zoned_date_time(ctx)?;
            Ok($val)
        }
    };
    ($fn:ident, $zdt:ident, $heap:ident => $val:expr) => {
        fn $fn(ctx: &mut NativeCtx<'_>, _args: &[Value]) -> Result<Value, NativeError> {
            let $zdt = require_zoned_date_time(ctx)?;
            let $heap = ctx.heap_mut();
            Ok($val)
        }
    };
}

zoned_date_time_getter!(get_year, zdt => Value::number_i32(zdt.year()));
zoned_date_time_getter!(get_month, zdt => Value::number_i32(zdt.month() as i32));
zoned_date_time_getter!(get_month_code, zdt, heap => str_or_undef(zdt.month_code().as_str(), heap));
zoned_date_time_getter!(get_day, zdt => Value::number_i32(zdt.day() as i32));
zoned_date_time_getter!(get_hour, zdt => Value::number_i32(zdt.hour() as i32));
zoned_date_time_getter!(get_minute, zdt => Value::number_i32(zdt.minute() as i32));
zoned_date_time_getter!(get_second, zdt => Value::number_i32(zdt.second() as i32));
zoned_date_time_getter!(get_millisecond, zdt => Value::number_i32(zdt.millisecond() as i32));
zoned_date_time_getter!(get_microsecond, zdt => Value::number_i32(zdt.microsecond() as i32));
zoned_date_time_getter!(get_nanosecond, zdt => Value::number_i32(zdt.nanosecond() as i32));
zoned_date_time_getter!(get_day_of_week, zdt => Value::number_i32(zdt.day_of_week() as i32));
zoned_date_time_getter!(get_day_of_year, zdt => Value::number_i32(zdt.day_of_year() as i32));
zoned_date_time_getter!(get_week_of_year, zdt => zdt
    .week_of_year()
    .map_or(Value::undefined(), |w| Value::number_i32(w as i32)));
zoned_date_time_getter!(get_year_of_week, zdt => zdt
    .year_of_week()
    .map_or(Value::undefined(), Value::number_i32));
zoned_date_time_getter!(get_days_in_week, zdt => Value::number_i32(zdt.days_in_week() as i32));
zoned_date_time_getter!(get_days_in_month, zdt => Value::number_i32(zdt.days_in_month() as i32));
zoned_date_time_getter!(get_days_in_year, zdt => Value::number_i32(zdt.days_in_year() as i32));
zoned_date_time_getter!(get_months_in_year, zdt => Value::number_i32(zdt.months_in_year() as i32));
zoned_date_time_getter!(get_in_leap_year, zdt => Value::boolean(zdt.in_leap_year()));
zoned_date_time_getter!(get_hours_in_day, zdt => zdt
    .hours_in_day()
    .map_or(Value::undefined(), Value::number_f64));
zoned_date_time_getter!(get_epoch_milliseconds, zdt => Value::number_f64(zdt.epoch_milliseconds() as f64));
zoned_date_time_getter!(get_offset_nanoseconds, zdt => Value::number_f64(zdt.offset_nanoseconds() as f64));
zoned_date_time_getter!(get_era, zdt, heap => zdt
    .era()
    .map_or(Value::undefined(), |era| str_or_undef(era.as_str(), heap)));
zoned_date_time_getter!(get_era_year, zdt => zdt.era_year().map_or(Value::undefined(), Value::number_i32));
zoned_date_time_getter!(get_epoch_nanoseconds, zdt, heap => {
    match BigIntValue::from_i128(heap, zdt.epoch_nanoseconds().0) {
        Ok(b) => Value::big_int(b),
        Err(_) => Value::undefined(),
    }
});
zoned_date_time_getter!(get_offset, zdt, heap => str_or_undef(&zdt.offset(), heap));
zoned_date_time_getter!(get_time_zone_id, zdt, heap => {
    let id = zdt.time_zone().identifier().unwrap_or_default();
    str_or_undef(&id, heap)
});
zoned_date_time_getter!(get_calendar_id, zdt, heap => str_or_undef(zdt.calendar().identifier(), heap));

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

pub static ZONED_DATE_TIME_PROTOTYPE_METHODS: &[MethodSpec] = &[
    method("toString", 0, impl_to_string),
    method("toLocaleString", 0, impl_to_locale_string),
    method("toJSON", 0, impl_to_json),
    method("valueOf", 0, impl_value_of),
    method("add", 1, impl_add),
    method("subtract", 1, impl_subtract),
    method("equals", 1, impl_equals),
    method("until", 1, impl_until),
    method("since", 1, impl_since),
    method("round", 1, impl_round),
    method("startOfDay", 0, impl_start_of_day),
    method("with", 1, impl_with),
    method("withCalendar", 1, impl_with_calendar),
    method("withTimeZone", 1, impl_with_time_zone),
    method("withPlainTime", 0, impl_with_plain_time),
    method("toInstant", 0, impl_to_instant),
    method("toPlainDate", 0, impl_to_plain_date),
    method("toPlainTime", 0, impl_to_plain_time),
    method("toPlainDateTime", 0, impl_to_plain_date_time),
    method("getTimeZoneTransition", 1, impl_get_time_zone_transition),
];

otter_macros::couch! {
    name = "ZonedDateTime",
    feature = CORE,
    intrinsic = ZonedDateTimeIntrinsic,
    constructor = (length = 2, call = construct),
    statics = {
        "from"    / 1 => from,
        "compare" / 2 => compare,
    },
    prototype = {
        method_specs = [ZONED_DATE_TIME_PROTOTYPE_METHODS],
        accessors = [
            ("calendarId",        get = get_calendar_id),
            ("timeZoneId",        get = get_time_zone_id),
            ("era",               get = get_era),
            ("eraYear",           get = get_era_year),
            ("year",              get = get_year),
            ("month",             get = get_month),
            ("monthCode",         get = get_month_code),
            ("day",               get = get_day),
            ("hour",              get = get_hour),
            ("minute",            get = get_minute),
            ("second",            get = get_second),
            ("millisecond",       get = get_millisecond),
            ("microsecond",       get = get_microsecond),
            ("nanosecond",        get = get_nanosecond),
            ("epochMilliseconds", get = get_epoch_milliseconds),
            ("epochNanoseconds",  get = get_epoch_nanoseconds),
            ("dayOfWeek",         get = get_day_of_week),
            ("dayOfYear",         get = get_day_of_year),
            ("weekOfYear",        get = get_week_of_year),
            ("yearOfWeek",        get = get_year_of_week),
            ("hoursInDay",        get = get_hours_in_day),
            ("daysInWeek",        get = get_days_in_week),
            ("daysInMonth",       get = get_days_in_month),
            ("daysInYear",        get = get_days_in_year),
            ("monthsInYear",      get = get_months_in_year),
            ("inLeapYear",        get = get_in_leap_year),
            ("offsetNanoseconds", get = get_offset_nanoseconds),
            ("offset",            get = get_offset),
        ],
    },
    install_on = crate::temporal::native_dispatch::temporal_host,
    string_tag = "Temporal.ZonedDateTime",
}

/// §sec-temporal.*.prototype.tolocalestring — brand-checks the receiver,
/// then (absent the Intl formatting data path) renders the same canonical
/// string as `toString`.
fn impl_to_locale_string(ctx: &mut NativeCtx<'_>, _args: &[Value]) -> Result<Value, NativeError> {
    impl_to_string(ctx, &[])
}
