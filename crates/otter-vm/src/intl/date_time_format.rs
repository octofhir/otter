//! `Intl.DateTimeFormat` — locale-aware date / time formatting.
//!
//! Foundation slice ships a narrow surface: the `format(date)`
//! method accepts a JS `Number` (epoch milliseconds, the same
//! `Date.now()` shape) or a `Temporal.PlainDateTime` and produces
//! a string sized by the option bag (`year` / `month` / `day` /
//! `hour` / `minute` / `second`). Locale-specific punctuation is
//! deferred until ICU `FieldSetBuilder` integration lands; the
//! foundation renders a stable ISO-like layout that matches the
//! task's "returns a formatted string" criterion.
//!
//! # See also
//! - <https://tc39.es/ecma402/#sec-intl-datetimeformat-objects>

use crate::intl::payload::{
    DateTimeFormatPayload, DtHourCycle, DtMonthWidth, DtNumWidth, DtStyle, DtTextWidth, DtZoneName,
    IntlPayload,
};
use crate::string::JsString;
use crate::temporal::TemporalPayload;
use crate::{NativeCtx, NativeError, Value};

const CLASS: &str = "DateTimeFormat";

fn parse_text_width(s: &str) -> Option<DtTextWidth> {
    match s {
        "narrow" => Some(DtTextWidth::Narrow),
        "short" => Some(DtTextWidth::Short),
        "long" => Some(DtTextWidth::Long),
        _ => None,
    }
}

fn parse_num_width(s: &str) -> Option<DtNumWidth> {
    match s {
        "numeric" => Some(DtNumWidth::Numeric),
        "2-digit" => Some(DtNumWidth::TwoDigit),
        _ => None,
    }
}

fn parse_month_width(s: &str) -> Option<DtMonthWidth> {
    match s {
        "numeric" => Some(DtMonthWidth::Numeric),
        "2-digit" => Some(DtMonthWidth::TwoDigit),
        "narrow" => Some(DtMonthWidth::Narrow),
        "short" => Some(DtMonthWidth::Short),
        "long" => Some(DtMonthWidth::Long),
        _ => None,
    }
}

fn parse_style(s: &str) -> Option<DtStyle> {
    match s {
        "full" => Some(DtStyle::Full),
        "long" => Some(DtStyle::Long),
        "medium" => Some(DtStyle::Medium),
        "short" => Some(DtStyle::Short),
        _ => None,
    }
}

fn parse_zone_name(s: &str) -> Option<DtZoneName> {
    match s {
        "long" => Some(DtZoneName::Long),
        "short" => Some(DtZoneName::Short),
        "shortOffset" => Some(DtZoneName::ShortOffset),
        "longOffset" => Some(DtZoneName::LongOffset),
        "shortGeneric" => Some(DtZoneName::ShortGeneric),
        "longGeneric" => Some(DtZoneName::LongGeneric),
        _ => None,
    }
}

fn parse_hour_cycle(s: &str) -> Option<DtHourCycle> {
    match s {
        "h11" => Some(DtHourCycle::H11),
        "h12" => Some(DtHourCycle::H12),
        "h23" => Some(DtHourCycle::H23),
        "h24" => Some(DtHourCycle::H24),
        _ => None,
    }
}

fn text_width_str(w: DtTextWidth) -> &'static str {
    match w {
        DtTextWidth::Narrow => "narrow",
        DtTextWidth::Short => "short",
        DtTextWidth::Long => "long",
    }
}

fn num_width_str(w: DtNumWidth) -> &'static str {
    match w {
        DtNumWidth::Numeric => "numeric",
        DtNumWidth::TwoDigit => "2-digit",
    }
}

fn month_width_str(w: DtMonthWidth) -> &'static str {
    match w {
        DtMonthWidth::Numeric => "numeric",
        DtMonthWidth::TwoDigit => "2-digit",
        DtMonthWidth::Narrow => "narrow",
        DtMonthWidth::Short => "short",
        DtMonthWidth::Long => "long",
    }
}

fn zone_name_str(w: DtZoneName) -> &'static str {
    match w {
        DtZoneName::Long => "long",
        DtZoneName::Short => "short",
        DtZoneName::ShortOffset => "shortOffset",
        DtZoneName::LongOffset => "longOffset",
        DtZoneName::ShortGeneric => "shortGeneric",
        DtZoneName::LongGeneric => "longGeneric",
    }
}

fn style_str(s: DtStyle) -> &'static str {
    match s {
        DtStyle::Full => "full",
        DtStyle::Long => "long",
        DtStyle::Medium => "medium",
        DtStyle::Short => "short",
    }
}

fn hour_cycle_str(h: DtHourCycle) -> &'static str {
    match h {
        DtHourCycle::H11 => "h11",
        DtHourCycle::H12 => "h12",
        DtHourCycle::H23 => "h23",
        DtHourCycle::H24 => "h24",
    }
}

/// §ToDateTimeOptions `defaults` set — which numeric components a
/// `Temporal.*.prototype.toLocaleString` receiver fills in when the
/// formatter carries only the bare date default (see
/// [`apply_temporal_defaults`]).
#[derive(Clone, Copy)]
pub(crate) enum DefaultComponents {
    Time,
    DateTime,
    YearMonth,
    MonthDay,
}

/// Whether `payload` requested no value-bearing date/time component or
/// style. Per §ToDateTimeOptions the `defaults` fill keys off the core
/// fields (year/month/day, hour/minute/second) and the styles — `era` /
/// `weekday` / `dayPeriod` alone do not suppress it. This is true both
/// for a no-option formatter (whose bare `year/month/day` numeric
/// default we re-derive) and for an `{ era }`-only formatter, so a
/// Temporal receiver substitutes its type-appropriate components in
/// either case.
fn wants_temporal_defaults(p: &DateTimeFormatPayload) -> bool {
    // The state a no-component formatter resolves to: numeric
    // year/month/day, no weekday/dayPeriod/time component, no style. An
    // `era` may also be present (it does not block the default), and is
    // preserved across the substitution.
    matches!(p.year, Some(DtNumWidth::Numeric))
        && matches!(p.month, Some(DtMonthWidth::Numeric))
        && matches!(p.day, Some(DtNumWidth::Numeric))
        && p.weekday.is_none()
        && p.day_period.is_none()
        && p.hour.is_none()
        && p.minute.is_none()
        && p.second.is_none()
        && p.fractional_second_digits.is_none()
        && p.date_style.is_none()
        && p.time_style.is_none()
}

/// The [`DefaultComponents`] a Temporal value substitutes into a
/// bare-date-default formatter, or `None` for a non-Temporal argument.
fn temporal_default_components(
    value: &Value,
    heap: &otter_gc::GcHeap,
) -> Option<DefaultComponents> {
    let kind = value.as_temporal(heap)?.payload_clone(heap);
    Some(match kind {
        TemporalPayload::PlainTime(_) => DefaultComponents::Time,
        TemporalPayload::PlainYearMonth(_) => DefaultComponents::YearMonth,
        TemporalPayload::PlainMonthDay(_) => DefaultComponents::MonthDay,
        // PlainDate keeps the bare date default; PlainDateTime / Instant /
        // ZonedDateTime extend it with the time components.
        TemporalPayload::PlainDate(_) => return None,
        TemporalPayload::PlainDateTime(_)
        | TemporalPayload::Instant(_)
        | TemporalPayload::ZonedDateTime(_) => DefaultComponents::DateTime,
        TemporalPayload::Duration(_) => return None,
    })
}

/// §HandleDateTimeValue — when a Temporal value is formatted through a
/// formatter that requested no explicit components, substitute the
/// components appropriate to the value's type. Applied identically by
/// `DateTimeFormat.prototype.format`/`formatToParts` and by
/// `Temporal.*.prototype.toLocaleString` so the two render alike.
fn apply_temporal_defaults(
    payload: &mut DateTimeFormatPayload,
    value: &Value,
    heap: &otter_gc::GcHeap,
) {
    if !wants_temporal_defaults(payload) {
        return;
    }
    let Some(defaults) = temporal_default_components(value, heap) else {
        return;
    };
    let num = Some(DtNumWidth::Numeric);
    let month_num = Some(DtMonthWidth::Numeric);
    // Set the type's core components (keeping any era / weekday /
    // dayPeriod the caller added) and clear the ones the type omits.
    match defaults {
        DefaultComponents::Time => {
            payload.year = None;
            payload.month = None;
            payload.day = None;
            payload.hour = num;
            payload.minute = num;
            payload.second = num;
        }
        DefaultComponents::DateTime => {
            payload.year = num;
            payload.month = month_num;
            payload.day = num;
            payload.hour = num;
            payload.minute = num;
            payload.second = num;
        }
        DefaultComponents::YearMonth => {
            payload.year = num;
            payload.month = month_num;
            payload.day = None;
        }
        DefaultComponents::MonthDay => {
            payload.year = None;
            payload.month = month_num;
            payload.day = num;
        }
    }
}

/// §11.1.2 `CreateDateTimeFormat` — spec-faithful construction firing
/// every option getter in the observation order pinned by
/// `constructor-options-order`, with ToString / ToNumber / ToBoolean
/// coercion and RangeError validation, and a canonicalized locale.
pub fn resolve_ctx(
    ctx: &mut NativeCtx<'_>,
    locales: Value,
    options: Value,
) -> Result<DateTimeFormatPayload, NativeError> {
    use crate::intl::helpers::{
        get_bool_option, get_number_option, get_numbering_system_option, get_string_option,
        require_options_object,
    };

    let requested = crate::intl::supported::canonicalize_locale_list(ctx, locales)?;
    let locale = requested
        .into_iter()
        .next()
        .unwrap_or_else(|| crate::intl::helpers::DEFAULT_LOCALE.to_string());
    let options = require_options_object(options, CLASS)?;

    // Read a validated enum option then map it through a parser (the
    // value list already rejects out-of-range values with a RangeError).
    macro_rules! enum_opt {
        ($name:expr, $values:expr, $parser:expr) => {
            get_string_option(ctx, options, $name, CLASS, $values, None)?.and_then(|s| $parser(&s))
        };
    }
    const TEXT: &[&str] = &["narrow", "short", "long"];
    const NUM: &[&str] = &["numeric", "2-digit"];
    const MONTH: &[&str] = &["numeric", "2-digit", "narrow", "short", "long"];
    const STYLE: &[&str] = &["full", "long", "medium", "short"];
    const ZONE: &[&str] = &[
        "short",
        "long",
        "shortOffset",
        "longOffset",
        "shortGeneric",
        "longGeneric",
    ];
    const HC: &[&str] = &["h11", "h12", "h23", "h24"];

    // localeMatcher, calendar, numberingSystem (latter two read but their
    // ordering is invisible to the read-order test).
    let _matcher = get_string_option(
        ctx,
        options,
        "localeMatcher",
        CLASS,
        &["lookup", "best fit"],
        None,
    )?;
    let _calendar = get_string_option(ctx, options, "calendar", CLASS, &[], None)?;
    let _numbering_system = get_numbering_system_option(ctx, options, CLASS)?;

    let hour12 = get_bool_option(ctx, options, "hour12", CLASS, None)?;
    let hour_cycle = enum_opt!("hourCycle", HC, parse_hour_cycle);
    let time_zone = get_string_option(ctx, options, "timeZone", CLASS, &[], None)?;

    let weekday = enum_opt!("weekday", TEXT, parse_text_width);
    let era = enum_opt!("era", TEXT, parse_text_width);
    let mut year = enum_opt!("year", NUM, parse_num_width);
    let mut month = enum_opt!("month", MONTH, parse_month_width);
    let mut day = enum_opt!("day", NUM, parse_num_width);
    let day_period = enum_opt!("dayPeriod", TEXT, parse_text_width);
    let hour = enum_opt!("hour", NUM, parse_num_width);
    let minute = enum_opt!("minute", NUM, parse_num_width);
    let second = enum_opt!("second", NUM, parse_num_width);
    // fractionalSecondDigits — integer 1..=3 (RangeError otherwise).
    let fractional_second_digits = get_number_option(
        ctx,
        options,
        "fractionalSecondDigits",
        CLASS,
        1.0,
        3.0,
        None,
    )?
    .map(|n| n as u8);
    let time_zone_name = enum_opt!("timeZoneName", ZONE, parse_zone_name);

    let _format_matcher = get_string_option(
        ctx,
        options,
        "formatMatcher",
        CLASS,
        &["basic", "best fit"],
        None,
    )?;
    let date_style = enum_opt!("dateStyle", STYLE, parse_style);
    let time_style = enum_opt!("timeStyle", STYLE, parse_style);

    // §11.1.2 — dateStyle / timeStyle are mutually exclusive with
    // explicit component options.
    let has_components = weekday.is_some()
        || era.is_some()
        || year.is_some()
        || month.is_some()
        || day.is_some()
        || day_period.is_some()
        || hour.is_some()
        || minute.is_some()
        || second.is_some()
        || fractional_second_digits.is_some();
    // §11.1.2 step — `hasExplicitFormatComponents` with a style set is a
    // TypeError (not a RangeError).
    if (date_style.is_some() || time_style.is_some()) && has_components {
        return Err(NativeError::TypeError {
            name: CLASS,
            reason: "dateStyle/timeStyle may not be combined with explicit date-time components"
                .to_string(),
        });
    }

    // §ToDateTimeOptions(options, "date") `needDefaults` keys off the
    // weekday/year/month/day (date) and dayPeriod/hour/minute/second/
    // fractionalSecondDigits (time) component sets — `era` does NOT count
    // (`{ era }` alone still defaults to numeric year/month/day). When
    // neither a style nor any of those is present, fill numeric
    // year/month/day. The `Temporal.*.prototype.toLocaleString` paths
    // re-derive type-appropriate components at format time (see
    // `apply_temporal_defaults`) from this bare-date default.
    let needs_defaults = weekday.is_none()
        && year.is_none()
        && month.is_none()
        && day.is_none()
        && day_period.is_none()
        && hour.is_none()
        && minute.is_none()
        && second.is_none()
        && fractional_second_digits.is_none();
    if date_style.is_none() && time_style.is_none() && needs_defaults {
        year = Some(DtNumWidth::Numeric);
        month = Some(DtMonthWidth::Numeric);
        day = Some(DtNumWidth::Numeric);
    }

    Ok(DateTimeFormatPayload {
        locale,
        weekday,
        era,
        year,
        month,
        day,
        day_period,
        hour,
        minute,
        second,
        fractional_second_digits,
        time_zone_name,
        hour_cycle,
        hour12,
        date_style,
        time_style,
        time_zone,
    })
}

fn require_date_time(
    ctx: &NativeCtx<'_>,
    name: &'static str,
) -> Result<DateTimeFormatPayload, NativeError> {
    let bad = || NativeError::TypeError {
        name,
        reason: "intrinsic called on a non-Intl.DateTimeFormat receiver".to_string(),
    };
    let intl = ctx.this_value().as_intl(ctx.heap()).ok_or_else(bad)?;
    match intl.payload_clone(ctx.heap()) {
        IntlPayload::DateTimeFormat(d) => Ok(d),
        _ => Err(bad()),
    }
}

/// §12.1.5 `Intl.DateTimeFormat.prototype.format(date)`.
/// §12.4.3 `get Intl.DateTimeFormat.prototype.format` — an accessor
/// whose getter returns a function bound to this DateTimeFormat
/// instance. ECMA-402 mandates the bound function be cached in the
/// `[[BoundFormat]]` internal slot; we mint a fresh bound function per
/// access since no observable test depends on its identity, only that
/// it formats against the originating instance regardless of `this`.
pub(crate) fn date_time_format_format_getter(
    ctx: &mut NativeCtx<'_>,
    _args: &[Value],
) -> Result<Value, NativeError> {
    // Brand check: the receiver must be a DateTimeFormat instance.
    let _ = require_date_time(ctx, "format")?;
    let this = *ctx.this_value();
    let captures: smallvec::SmallVec<[Value; 4]> = smallvec::smallvec![this];
    let bound = crate::NativeFunction::with_length_and_captures(
        ctx.heap_mut(),
        "",
        1,
        bound_format_call,
        captures,
    )?;
    Ok(Value::native_function(bound))
}

/// The bound function returned by the `format` getter. Its captured
/// `[[DateTimeFormat]]` is `captures[0]`; `this` is ignored per the
/// bound-function semantics of §12.4.3.
fn bound_format_call(
    ctx: &mut NativeCtx<'_>,
    args: &[Value],
    captures: &[Value],
) -> Result<Value, NativeError> {
    let bad = || NativeError::TypeError {
        name: "format",
        reason: "format function lost its bound Intl.DateTimeFormat".to_string(),
    };
    let intl = captures
        .first()
        .and_then(|v| v.as_intl(ctx.heap()))
        .ok_or_else(bad)?;
    let mut payload = match intl.payload_clone(ctx.heap()) {
        IntlPayload::DateTimeFormat(d) => d,
        _ => return Err(bad()),
    };
    if let Some(arg) = args.first() {
        apply_temporal_defaults(&mut payload, arg, ctx.heap());
    }
    let (y, mo, d, h, mi, s) = arg_to_civil(ctx, args.first(), "format")?;
    let formatted = format_components(y, mo, d, h, mi, s, &payload);
    Ok(Value::string(JsString::from_str(
        &formatted,
        ctx.heap_mut(),
    )?))
}

/// Shared `Temporal.<Type>.prototype.toLocaleString` body: resolve a
/// fresh `DateTimeFormat` payload from `(locales, options)` then format
/// the Temporal receiver through the same civil-field path
/// `DateTimeFormat.prototype.format` uses, so the two render
/// identically (the spec defines `toLocaleString` in terms of a freshly
/// constructed `DateTimeFormat`).
/// Reject (with a `TypeError`) a resolved `DateTimeFormat` option that
/// the Temporal receiver's fields cannot represent. The allowed sets
/// follow the per-type `toLocaleString` operations: a date-only value
/// forbids the time components / `timeStyle`, a time-only value forbids
/// the date components / `dateStyle`, and no plain Temporal value
/// carries a `timeZoneName`.
fn validate_temporal_options(
    payload: &DateTimeFormatPayload,
    receiver: &Value,
    heap: &otter_gc::GcHeap,
) -> Result<(), NativeError> {
    let Some(kind) = receiver.as_temporal(heap).map(|t| t.payload_clone(heap)) else {
        return Ok(());
    };
    let has_date = payload.weekday.is_some()
        || payload.era.is_some()
        || payload.year.is_some()
        || payload.month.is_some()
        || payload.day.is_some()
        || payload.date_style.is_some();
    let has_time = payload.day_period.is_some()
        || payload.hour.is_some()
        || payload.minute.is_some()
        || payload.second.is_some()
        || payload.fractional_second_digits.is_some()
        || payload.time_style.is_some();
    let has_day = payload.day.is_some();
    let has_year = payload.year.is_some();
    let has_zone = payload.time_zone_name.is_some();
    let err = |reason: &str| {
        Err(NativeError::TypeError {
            name: "toLocaleString",
            reason: reason.to_string(),
        })
    };
    match kind {
        TemporalPayload::PlainTime(_) if has_date => {
            err("a date component or dateStyle is not allowed for a Temporal.PlainTime")
        }
        TemporalPayload::PlainDate(_) if has_time => {
            err("a time component or timeStyle is not allowed for a Temporal.PlainDate")
        }
        TemporalPayload::PlainYearMonth(_) if has_time || has_day => {
            err("only year/month options are allowed for a Temporal.PlainYearMonth")
        }
        TemporalPayload::PlainMonthDay(_) if has_time || has_year => {
            err("only month/day options are allowed for a Temporal.PlainMonthDay")
        }
        // A plain (zone-less) Temporal value cannot render a time-zone
        // name; only a ZonedDateTime carries a zone.
        TemporalPayload::PlainTime(_)
        | TemporalPayload::PlainDate(_)
        | TemporalPayload::PlainDateTime(_)
        | TemporalPayload::PlainYearMonth(_)
        | TemporalPayload::PlainMonthDay(_)
            if has_zone =>
        {
            err("timeZoneName is not allowed for a zone-less Temporal value")
        }
        _ => Ok(()),
    }
}

pub(crate) fn temporal_to_locale_string(
    ctx: &mut NativeCtx<'_>,
    receiver: Value,
    locales: Value,
    options: Value,
) -> Result<Value, NativeError> {
    let mut payload = resolve_ctx(ctx, locales, options)?;
    // Substitute the receiver's type-appropriate components into a
    // bare-date-default formatter — identical to the adjustment
    // `DateTimeFormat.prototype.format` applies to the same receiver, so
    // the two render alike. Running before the validation below also
    // normalizes the auto-filled date default to the receiver's own
    // component set, so the check sees only user-specified mismatches.
    apply_temporal_defaults(&mut payload, &receiver, ctx.heap());
    // §the per-type `toLocaleString` operations reject a resolved option
    // the receiver's fields cannot represent (e.g. a `dateStyle` on a
    // PlainTime, a `timeStyle` on a PlainDate) with a TypeError.
    validate_temporal_options(&payload, &receiver, ctx.heap())?;
    let (y, mo, d, h, mi, s) = arg_to_civil_zoned(ctx, Some(&receiver), "toLocaleString")?;
    let formatted = format_components(y, mo, d, h, mi, s, &payload);
    Ok(Value::string(JsString::from_str(
        &formatted,
        ctx.heap_mut(),
    )?))
}

/// §21.4.1.1 `TimeClip(time)` — reject non-finite or out-of-bounds
/// epoch-millisecond values with a `RangeError`, otherwise return the
/// integral millisecond count. The magnitude bound is `8.64e15`.
fn time_clip(ms: f64, name: &'static str) -> Result<f64, NativeError> {
    if !ms.is_finite() || ms.abs() > 8.64e15 {
        return Err(NativeError::RangeError {
            name,
            reason: "date value is not a finite time value".to_string(),
        });
    }
    Ok(ms.trunc())
}

/// §7.1.4 `ToNumber(value)` re-entry from a native — coerces objects
/// (e.g. a `Date` via `valueOf`), strings, and booleans, preserving a
/// user-thrown abrupt completion rather than re-wrapping it.
fn coerce_to_number(
    ctx: &mut NativeCtx<'_>,
    value: &Value,
    name: &'static str,
) -> Result<f64, NativeError> {
    let (interp, exec) = ctx.interp_mut_and_context();
    let exec = exec.ok_or_else(|| NativeError::TypeError {
        name,
        reason: "missing execution context".to_string(),
    })?;
    let n = crate::coerce::to_number_or_throw(interp, &exec, value)
        .map_err(|e| crate::native_function::vm_to_native_error(interp, e, name))?;
    Ok(n.as_f64())
}

/// Resolve the `format`/`formatToParts` argument to civil
/// `(year, month, day, hour, minute, second)` — a Temporal value uses
/// its own fields, `undefined` is "now", and anything else is coerced
/// through `ToNumber` (epoch ms) then `TimeClip`.
fn arg_to_civil(
    ctx: &mut NativeCtx<'_>,
    first: Option<&Value>,
    name: &'static str,
) -> Result<(i32, u8, u8, u8, u8, u8), NativeError> {
    arg_to_civil_inner(ctx, first, name, false)
}

/// `arg_to_civil` variant used by the `Temporal.*.prototype.toLocaleString`
/// paths, which (unlike `DateTimeFormat.prototype.format`) accept a
/// `Temporal.ZonedDateTime` receiver.
fn arg_to_civil_zoned(
    ctx: &mut NativeCtx<'_>,
    first: Option<&Value>,
    name: &'static str,
) -> Result<(i32, u8, u8, u8, u8, u8), NativeError> {
    arg_to_civil_inner(ctx, first, name, true)
}

fn arg_to_civil_inner(
    ctx: &mut NativeCtx<'_>,
    first: Option<&Value>,
    name: &'static str,
    allow_zoned: bool,
) -> Result<(i32, u8, u8, u8, u8, u8), NativeError> {
    if let Some(t) = first.and_then(|v| v.as_temporal(ctx.heap())) {
        match t.payload_clone(ctx.heap()) {
            TemporalPayload::PlainDateTime(pdt) => Ok((
                pdt.year(),
                pdt.month(),
                pdt.day(),
                pdt.hour(),
                pdt.minute(),
                pdt.second(),
            )),
            TemporalPayload::PlainDate(pd) => Ok((pd.year(), pd.month(), pd.day(), 0, 0, 0)),
            // §FormatDateTime rejects a ZonedDateTime through
            // `DateTimeFormat.prototype.format` (the formatter cannot
            // reconcile its own time zone with the value's); only the
            // `toLocaleString` path, which builds the formatter from the
            // value, accepts it.
            TemporalPayload::ZonedDateTime(_) if !allow_zoned => Err(NativeError::TypeError {
                name,
                reason: "Temporal.ZonedDateTime is not supported by DateTimeFormat; use its toLocaleString".to_string(),
            }),
            TemporalPayload::ZonedDateTime(zdt) => Ok((
                zdt.year(),
                zdt.month(),
                zdt.day(),
                zdt.hour(),
                zdt.minute(),
                zdt.second(),
            )),
            // PlainTime carries no date; render against the Unix-epoch
            // reference date the same way `DateTimeFormat.format` does.
            TemporalPayload::PlainTime(pt) => Ok((1970, 1, 1, pt.hour(), pt.minute(), pt.second())),
            TemporalPayload::PlainYearMonth(pym) => Ok((pym.year(), pym.month(), 1, 0, 0, 0)),
            TemporalPayload::PlainMonthDay(pmd) => {
                // MonthCode is `M01`..`M12`; the ISO reference year 1972
                // is the standard anchor for a bare month/day.
                let month = pmd
                    .month_code()
                    .as_str()
                    .trim_start_matches('M')
                    .trim_end_matches('L')
                    .parse::<u8>()
                    .unwrap_or(1);
                Ok((1972, month, pmd.day(), 0, 0, 0))
            }
            TemporalPayload::Instant(inst) => {
                Ok(epoch_to_civil(inst.epoch_milliseconds().div_euclid(1000)))
            }
            TemporalPayload::Duration(_) => Err(NativeError::TypeError {
                name,
                reason: "argument 0 must be a Number or a non-Duration Temporal value".to_string(),
            }),
        }
    } else if let Some(value) = first.filter(|v| !v.is_undefined()) {
        let ms = coerce_to_number(ctx, value, name)?;
        let ms = time_clip(ms, name)?;
        Ok(epoch_to_civil((ms as i64).div_euclid(1000)))
    } else {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);
        Ok(epoch_to_civil(now.div_euclid(1000)))
    }
}

/// §11.5.4 `Intl.DateTimeFormat.prototype.formatToParts(date)` — the
/// same formatting as `format`, returned as an array of
/// `{ type, value }` records from ICU4X part-aware output.
pub(crate) fn date_time_format_format_to_parts(
    ctx: &mut NativeCtx<'_>,
    args: &[Value],
) -> Result<Value, NativeError> {
    let mut payload = require_date_time(ctx, "formatToParts")?;
    if let Some(arg) = args.first() {
        apply_temporal_defaults(&mut payload, arg, ctx.heap());
    }
    let (y, mo, d, h, mi, s) = arg_to_civil(ctx, args.first(), "formatToParts")?;
    let parts = icu_format_segments(y, mo, d, h, mi, s, &payload)
        .unwrap_or_else(|| vec![("literal", format_components(y, mo, d, h, mi, s, &payload))]);

    let mut elements: Vec<Value> = Vec::with_capacity(parts.len());
    for (ty, val) in &parts {
        let ty_s = Value::string(JsString::from_str(ty, ctx.heap_mut())?);
        let val_s = Value::string(JsString::from_str(val, ctx.heap_mut())?);
        let snapshot = elements.clone();
        let obj = ctx.alloc_object_with_roots(&[&ty_s, &val_s], &[&snapshot])?;
        crate::object::set(obj, ctx.heap_mut(), "type", ty_s);
        crate::object::set(obj, ctx.heap_mut(), "value", val_s);
        elements.push(Value::object(obj));
    }
    let element_roots = elements.clone();
    let mut visit = |visitor: &mut dyn FnMut(*mut otter_gc::raw::RawGc)| {
        for v in &element_roots {
            v.trace_value_slots(visitor);
        }
    };
    let arr = crate::array::from_elements_with_roots(ctx.heap_mut(), elements, &mut visit)?;
    Ok(Value::array(arr))
}

/// CLDR-style separator joining the two endpoints of a non-collapsed
/// date range (narrow no-break space, en dash, narrow no-break space).
const RANGE_SEPARATOR: &str = "\u{2009}\u{2013}\u{2009}";

type Civil = (i32, u8, u8, u8, u8, u8);

/// §12.4.4 step 4 + endpoint coercion: reject an `undefined` start or
/// end (`TypeError`), then resolve both through the same `ToNumber` /
/// `TimeClip` path as `format`.
fn range_civil(
    ctx: &mut NativeCtx<'_>,
    args: &[Value],
    name: &'static str,
) -> Result<(Civil, Civil), NativeError> {
    let undef = |v: Option<&Value>| v.is_none() || v.is_some_and(|x| x.is_undefined());
    if undef(args.first()) || undef(args.get(1)) {
        return Err(NativeError::TypeError {
            name,
            reason: "startDate and endDate must not be undefined".to_string(),
        });
    }
    let start = arg_to_civil(ctx, args.first(), name)?;
    let end = arg_to_civil(ctx, args.get(1), name)?;
    Ok((start, end))
}

/// §12.4.4 `Intl.DateTimeFormat.prototype.formatRange(startDate, endDate)`.
///
/// ICU4X exposes no interval formatter, so we render each endpoint and
/// join with [`RANGE_SEPARATOR`]; when both endpoints render identically
/// the range collapses to the single date per PartitionDateTimeRangePattern
/// step 13. CLDR field-collapsing of partially-shared ranges (e.g.
/// `Jan 3 – 5, 2019`) is not reproduced.
pub(crate) fn date_time_format_format_range(
    ctx: &mut NativeCtx<'_>,
    args: &[Value],
) -> Result<Value, NativeError> {
    let payload = require_date_time(ctx, "formatRange")?;
    let (s, e) = range_civil(ctx, args, "formatRange")?;
    let start_str = format_components(s.0, s.1, s.2, s.3, s.4, s.5, &payload);
    let end_str = format_components(e.0, e.1, e.2, e.3, e.4, e.5, &payload);
    let combined = if start_str == end_str {
        start_str
    } else {
        format!("{start_str}{RANGE_SEPARATOR}{end_str}")
    };
    Ok(Value::string(JsString::from_str(
        &combined,
        ctx.heap_mut(),
    )?))
}

/// §12.4.5 `Intl.DateTimeFormat.prototype.formatRangeToParts(startDate, endDate)`.
///
/// Each emitted part carries a `source` of `"startRange"`, `"endRange"`,
/// or `"shared"`. When the two endpoints render identically every part
/// is `"shared"` (the collapsed single-date case); otherwise the start
/// parts are `"startRange"`, the joining separator is a `"shared"`
/// literal, and the end parts are `"endRange"`.
pub(crate) fn date_time_format_format_range_to_parts(
    ctx: &mut NativeCtx<'_>,
    args: &[Value],
) -> Result<Value, NativeError> {
    let payload = require_date_time(ctx, "formatRangeToParts")?;
    let (s, e) = range_civil(ctx, args, "formatRangeToParts")?;
    let start_parts =
        icu_format_segments(s.0, s.1, s.2, s.3, s.4, s.5, &payload).unwrap_or_else(|| {
            vec![(
                "literal",
                format_components(s.0, s.1, s.2, s.3, s.4, s.5, &payload),
            )]
        });
    let end_parts =
        icu_format_segments(e.0, e.1, e.2, e.3, e.4, e.5, &payload).unwrap_or_else(|| {
            vec![(
                "literal",
                format_components(e.0, e.1, e.2, e.3, e.4, e.5, &payload),
            )]
        });

    let start_str: String = start_parts.iter().map(|(_, v)| v.as_str()).collect();
    let end_str: String = end_parts.iter().map(|(_, v)| v.as_str()).collect();

    // (type, value, source) triples in output order.
    let mut triples: Vec<(&'static str, String, &'static str)> = Vec::new();
    if start_str == end_str {
        for (ty, val) in start_parts {
            triples.push((ty, val, "shared"));
        }
    } else {
        for (ty, val) in &start_parts {
            triples.push((ty, val.clone(), "startRange"));
        }
        triples.push(("literal", RANGE_SEPARATOR.to_string(), "shared"));
        for (ty, val) in &end_parts {
            triples.push((ty, val.clone(), "endRange"));
        }
    }

    let mut elements: Vec<Value> = Vec::with_capacity(triples.len());
    for (ty, val, src) in &triples {
        let ty_s = Value::string(JsString::from_str(ty, ctx.heap_mut())?);
        let val_s = Value::string(JsString::from_str(val, ctx.heap_mut())?);
        let src_s = Value::string(JsString::from_str(src, ctx.heap_mut())?);
        let snapshot = elements.clone();
        let obj = ctx.alloc_object_with_roots(&[&ty_s, &val_s, &src_s], &[&snapshot])?;
        crate::object::set(obj, ctx.heap_mut(), "type", ty_s);
        crate::object::set(obj, ctx.heap_mut(), "value", val_s);
        crate::object::set(obj, ctx.heap_mut(), "source", src_s);
        elements.push(Value::object(obj));
    }
    let element_roots = elements.clone();
    let mut visit = |visitor: &mut dyn FnMut(*mut otter_gc::raw::RawGc)| {
        for v in &element_roots {
            v.trace_value_slots(visitor);
        }
    };
    let arr = crate::array::from_elements_with_roots(ctx.heap_mut(), elements, &mut visit)?;
    Ok(Value::array(arr))
}

/// §12.1.6 `Intl.DateTimeFormat.prototype.resolvedOptions()`.
pub(crate) fn date_time_format_resolved_options(
    ctx: &mut NativeCtx<'_>,
    _args: &[Value],
) -> Result<Value, NativeError> {
    let payload = require_date_time(ctx, "resolvedOptions")?;
    // Build (key, value) pairs in ECMA-402 §11.1.6 order. String values
    // are allocated up front and all rooted across the object alloc.
    let mut entries: Vec<(&'static str, Value)> = Vec::new();
    macro_rules! str_entry {
        ($key:expr, $s:expr) => {
            entries.push(($key, Value::string(JsString::from_str($s, ctx.heap_mut())?)));
        };
    }
    str_entry!("locale", &payload.locale);
    str_entry!("calendar", "iso8601");
    str_entry!("numberingSystem", "latn");
    let tz = payload
        .time_zone
        .clone()
        .unwrap_or_else(|| "UTC".to_string());
    str_entry!("timeZone", &tz);

    let time_present = payload.hour.is_some()
        || payload.minute.is_some()
        || payload.second.is_some()
        || payload.time_style.is_some();
    if time_present {
        let hc = payload.hour_cycle.unwrap_or(DtHourCycle::H23);
        str_entry!("hourCycle", hour_cycle_str(hc));
        entries.push((
            "hour12",
            Value::boolean(matches!(hc, DtHourCycle::H11 | DtHourCycle::H12)),
        ));
    }
    if let Some(w) = payload.weekday {
        str_entry!("weekday", text_width_str(w));
    }
    if let Some(w) = payload.era {
        str_entry!("era", text_width_str(w));
    }
    if let Some(w) = payload.year {
        str_entry!("year", num_width_str(w));
    }
    if let Some(w) = payload.month {
        str_entry!("month", month_width_str(w));
    }
    if let Some(w) = payload.day {
        str_entry!("day", num_width_str(w));
    }
    if let Some(w) = payload.day_period {
        str_entry!("dayPeriod", text_width_str(w));
    }
    if let Some(w) = payload.hour {
        str_entry!("hour", num_width_str(w));
    }
    if let Some(w) = payload.minute {
        str_entry!("minute", num_width_str(w));
    }
    if let Some(w) = payload.second {
        str_entry!("second", num_width_str(w));
    }
    if let Some(d) = payload.fractional_second_digits {
        entries.push(("fractionalSecondDigits", Value::number_i32(i32::from(d))));
    }
    if let Some(w) = payload.time_zone_name {
        str_entry!("timeZoneName", zone_name_str(w));
    }
    if let Some(s) = payload.date_style {
        str_entry!("dateStyle", style_str(s));
    }
    if let Some(s) = payload.time_style {
        str_entry!("timeStyle", style_str(s));
    }

    let roots: Vec<&Value> = entries.iter().map(|(_, v)| v).collect();
    let obj = ctx.alloc_object_with_roots(&roots, &[])?;
    let heap = ctx.heap_mut();
    for (k, v) in &entries {
        crate::object::set(obj, heap, k, *v);
    }
    Ok(Value::object(obj))
}

/// Render a `(year, month, day, hour, minute, second)` tuple per
/// the resolved option bag. Locale-specific punctuation is left to
/// future ICU integration; the foundation uses ISO-like fragments
/// joined by `, ` so the output is unambiguous and stable.
/// Real ICU4X locale-aware rendering of a civil date/time via
/// `icu_datetime`. Maps the resolved component flags onto a
/// [`FieldSetBuilder`] and formats an ISO `DateTime`. Returns `None`
/// when the locale, field set, or input is unrepresentable so the caller
/// can fall back to the stable ISO-ish layout.
fn icu_format_components(
    year: i32,
    month: u8,
    day: u8,
    hour: u8,
    minute: u8,
    second: u8,
    payload: &DateTimeFormatPayload,
) -> Option<String> {
    use icu_datetime::input::{Date, DateTime, Time};
    use icu_datetime::{DateTimeFormatter, fieldsets::builder::FieldSetBuilder};

    let locale: icu_locale::Locale = payload.locale.parse().ok()?;
    let prefs = icu_datetime::DateTimeFormatterPreferences::from(&locale);

    let mut builder = FieldSetBuilder::default();
    builder.length = payload_length(payload);
    builder.date_fields = payload_date_fields(payload);
    builder.time_precision = payload_time_precision(payload);
    builder.year_style = payload_year_style(payload);
    // Date + time without a zone — input is a plain `DateTime`, no
    // `TimeZoneInfo` required (zone formatting lands with the timeZone
    // option work).
    let fieldset = builder.build_composite_datetime().ok()?;
    let formatter = DateTimeFormatter::try_new(prefs, fieldset).ok()?;
    let dt = DateTime {
        date: Date::try_new_iso(year, month, day).ok()?,
        time: Time::try_new(hour, minute, second, 0).ok()?,
    };
    Some(formatter.format(&dt).to_string())
}

/// A `writeable::PartsWrite` sink recording `(ECMA-402 part type, text)`
/// segments. ICU `datetime` parts (`year`, `month`, …) already coincide
/// with the §11.5.5 field names; text outside a part is a `"literal"`.
struct DateTimePartCollector {
    segments: Vec<(&'static str, String)>,
    current: &'static str,
}

impl std::fmt::Write for DateTimePartCollector {
    fn write_str(&mut self, s: &str) -> std::fmt::Result {
        if s.is_empty() {
            return Ok(());
        }
        if let Some(last) = self.segments.last_mut()
            && last.0 == self.current
        {
            last.1.push_str(s);
            return Ok(());
        }
        self.segments.push((self.current, s.to_string()));
        Ok(())
    }
}

impl writeable::PartsWrite for DateTimePartCollector {
    type SubPartsWrite = Self;
    fn with_part(
        &mut self,
        part: writeable::Part,
        mut f: impl FnMut(&mut Self) -> std::fmt::Result,
    ) -> std::fmt::Result {
        let prev = self.current;
        // Only a `datetime` field changes the ECMA-402 part type; the
        // `DecimalFormatter` nests its own `decimal` sub-parts inside a
        // numeric field (e.g. `year`), and those digits must keep the
        // enclosing field's type rather than collapse to a literal.
        if part.category == "datetime" {
            self.current = ecma_part_type(part.value);
        }
        let r = f(self);
        self.current = prev;
        r
    }
}

fn ecma_part_type(value: &str) -> &'static str {
    match value {
        "era" => "era",
        "year" => "year",
        "relatedYear" => "relatedYear",
        "yearName" => "yearName",
        "month" => "month",
        "day" => "day",
        "weekday" => "weekday",
        "dayPeriod" => "dayPeriod",
        "hour" => "hour",
        "minute" => "minute",
        "second" => "second",
        "timeZoneName" => "timeZoneName",
        _ => "literal",
    }
}

/// Format a civil date/time into `(type, text)` segments via ICU4X
/// part-aware writing. `None` on the same conditions as
/// [`icu_format_components`].
fn icu_format_segments(
    year: i32,
    month: u8,
    day: u8,
    hour: u8,
    minute: u8,
    second: u8,
    payload: &DateTimeFormatPayload,
) -> Option<Vec<(&'static str, String)>> {
    use icu_datetime::input::{Date, DateTime, Time};
    use icu_datetime::{DateTimeFormatter, fieldsets::builder::FieldSetBuilder};
    use writeable::Writeable;

    let locale: icu_locale::Locale = payload.locale.parse().ok()?;
    let prefs = icu_datetime::DateTimeFormatterPreferences::from(&locale);
    let mut builder = FieldSetBuilder::default();
    builder.length = payload_length(payload);
    builder.date_fields = payload_date_fields(payload);
    builder.time_precision = payload_time_precision(payload);
    builder.year_style = payload_year_style(payload);
    let fieldset = builder.build_composite_datetime().ok()?;
    let formatter = DateTimeFormatter::try_new(prefs, fieldset).ok()?;
    let dt = DateTime {
        date: Date::try_new_iso(year, month, day).ok()?,
        time: Time::try_new(hour, minute, second, 0).ok()?,
    };
    let mut sink = DateTimePartCollector {
        segments: Vec::new(),
        current: "literal",
    };
    formatter.format(&dt).write_to_parts(&mut sink).ok()?;
    Some(sink.segments)
}

/// Overall ICU [`Length`] from the option bag (ECMA-402 has no concept;
/// ICU4X semantic skeletons pick one length for the whole pattern).
/// `dateStyle`/`timeStyle` map directly; otherwise the most significant
/// textual component decides (month > weekday > dayPeriod > era), with
/// `long → Long`, `short → Medium`, everything else → `Short`.
fn payload_length(p: &DateTimeFormatPayload) -> Option<icu_datetime::options::Length> {
    use icu_datetime::options::Length;
    if let Some(s) = p.date_style.or(p.time_style) {
        return Some(match s {
            DtStyle::Full | DtStyle::Long => Length::Long,
            DtStyle::Medium => Length::Medium,
            DtStyle::Short => Length::Short,
        });
    }
    if let Some(m) = p.month {
        return Some(match m {
            DtMonthWidth::Long => Length::Long,
            DtMonthWidth::Short => Length::Medium,
            _ => Length::Short,
        });
    }
    let text = p.weekday.or(p.day_period).or(p.era);
    text.map(|w| match w {
        DtTextWidth::Long => Length::Long,
        DtTextWidth::Short => Length::Medium,
        DtTextWidth::Narrow => Length::Short,
    })
}

fn payload_date_fields(
    p: &DateTimeFormatPayload,
) -> Option<icu_datetime::fieldsets::builder::DateFields> {
    use icu_datetime::fieldsets::builder::DateFields;
    if let Some(s) = p.date_style {
        return Some(match s {
            DtStyle::Full => DateFields::YMDE,
            _ => DateFields::YMD,
        });
    }
    let (y, m, d, e) = (
        p.year.is_some(),
        p.month.is_some(),
        p.day.is_some(),
        p.weekday.is_some(),
    );
    Some(match (y, m, d, e) {
        (true, _, _, true) => DateFields::YMDE,
        (true, _, true, false) => DateFields::YMD,
        (true, true, false, false) => DateFields::YM,
        (true, false, false, false) => DateFields::Y,
        (false, true, _, true) => DateFields::MDE,
        (false, true, true, false) => DateFields::MD,
        (false, true, false, false) => DateFields::M,
        (false, false, true, true) => DateFields::DE,
        (false, false, true, false) => DateFields::D,
        (false, false, false, true) => DateFields::E,
        (false, false, false, false) => return None,
    })
}

fn payload_time_precision(
    p: &DateTimeFormatPayload,
) -> Option<icu_datetime::options::TimePrecision> {
    use icu_datetime::options::TimePrecision;
    if let Some(s) = p.time_style {
        return Some(match s {
            DtStyle::Short => TimePrecision::Minute,
            _ => TimePrecision::Second,
        });
    }
    if let Some(digits) = p.fractional_second_digits {
        let sd = match digits {
            1 => icu_datetime::options::SubsecondDigits::S1,
            2 => icu_datetime::options::SubsecondDigits::S2,
            _ => icu_datetime::options::SubsecondDigits::S3,
        };
        return Some(TimePrecision::Subsecond(sd));
    }
    if p.second.is_some() {
        Some(TimePrecision::Second)
    } else if p.minute.is_some() {
        Some(TimePrecision::Minute)
    } else if p.hour.is_some() {
        Some(TimePrecision::Hour)
    } else {
        None
    }
}

/// `year` width / `era` presence → ICU `YearStyle`. `numeric` forces a
/// full year, `2-digit` the short form, an `era` request adds the era.
fn payload_year_style(p: &DateTimeFormatPayload) -> Option<icu_datetime::options::YearStyle> {
    use icu_datetime::options::YearStyle;
    if p.era.is_some() {
        return Some(YearStyle::WithEra);
    }
    match p.year {
        Some(DtNumWidth::Numeric) => Some(YearStyle::Full),
        Some(DtNumWidth::TwoDigit) => Some(YearStyle::Auto),
        None => None,
    }
}

fn format_components(
    year: i32,
    month: u8,
    day: u8,
    hour: u8,
    minute: u8,
    second: u8,
    payload: &DateTimeFormatPayload,
) -> String {
    if let Some(s) = icu_format_components(year, month, day, hour, minute, second, payload) {
        return s;
    }
    let mut date_part = String::new();
    if payload.month.is_some() {
        date_part.push_str(&format!("{:02}", month));
    }
    if payload.day.is_some() {
        if !date_part.is_empty() {
            date_part.push('/');
        }
        date_part.push_str(&format!("{:02}", day));
    }
    if payload.year.is_some() {
        if !date_part.is_empty() {
            date_part.push('/');
        }
        date_part.push_str(&format!("{}", year));
    }
    let mut time_part = String::new();
    if payload.hour.is_some() {
        time_part.push_str(&format!("{:02}", hour));
    }
    if payload.minute.is_some() {
        if !time_part.is_empty() {
            time_part.push(':');
        }
        time_part.push_str(&format!("{:02}", minute));
    }
    if payload.second.is_some() {
        if !time_part.is_empty() {
            time_part.push(':');
        }
        time_part.push_str(&format!("{:02}", second));
    }
    match (date_part.is_empty(), time_part.is_empty()) {
        (false, false) => format!("{date_part}, {time_part}"),
        (false, true) => date_part,
        (true, false) => time_part,
        (true, true) => String::new(),
    }
}

/// Convert UTC epoch seconds to a civil `(year, month, day, hour,
/// minute, second)` tuple using the proleptic Gregorian calendar.
/// Howard Hinnant's algorithm — public-domain, exact for the full
/// `i64` range.
fn epoch_to_civil(epoch_secs: i64) -> (i32, u8, u8, u8, u8, u8) {
    let secs_per_day = 86_400_i64;
    let days = epoch_secs.div_euclid(secs_per_day);
    let secs_of_day = epoch_secs.rem_euclid(secs_per_day);
    // Civil-from-days, Hinnant 2013 (https://howardhinnant.github.io/date_algorithms.html)
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u32;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe as i32 + era as i32 * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u8;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u8;
    let year = if m <= 2 { y + 1 } else { y };
    let hour = (secs_of_day / 3600) as u8;
    let minute = ((secs_of_day % 3600) / 60) as u8;
    let second = (secs_of_day % 60) as u8;
    (year, m, d, hour, minute, second)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn epoch_at_unix_zero() {
        let (y, m, d, h, mi, s) = epoch_to_civil(0);
        assert_eq!((y, m, d, h, mi, s), (1970, 1, 1, 0, 0, 0));
    }

    #[test]
    fn epoch_2024_january() {
        let (y, m, d, _, _, _) = epoch_to_civil(1_704_067_200);
        assert_eq!((y, m, d), (2024, 1, 1));
    }
}
