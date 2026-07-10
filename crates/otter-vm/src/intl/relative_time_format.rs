//! `Intl.RelativeTimeFormat` — locale-aware relative-time strings.
//!
//! Foundation surface: English long-form templates such as
//! `"in 3 days"` / `"5 minutes ago"`. The full ICU CLDR pattern
//! database is filed alongside the wider Intl follow-up.
//!
//! # See also
//! - <https://tc39.es/ecma402/#relativetimeformat-objects>

use otter_gc::raw::RawGc;

use crate::intl::helpers::{
    DEFAULT_LOCALE, coerce_options_object, get_numbering_system_option, get_string_option,
};
use crate::intl::payload::{IntlPayload, RelativeTimeFormatPayload};
use crate::string::JsString;
use crate::{NativeCtx, NativeError, Value};

const CLASS: &str = "RelativeTimeFormat";

/// §18.1.1 InitializeRelativeTimeFormat — fires `localeMatcher` /
/// `numberingSystem` / `style` / `numeric` getters in spec order with
/// ToString coercion + RangeError validation; canonicalizes the locale.
pub fn resolve_ctx(
    ctx: &mut NativeCtx<'_>,
    locales: Value,
    options: Value,
) -> Result<RelativeTimeFormatPayload, NativeError> {
    let requested = crate::intl::supported::canonicalize_locale_list(ctx, locales)?;
    let locale = requested
        .into_iter()
        .next()
        .unwrap_or_else(|| DEFAULT_LOCALE.to_string());
    let options = coerce_options_object(options, CLASS)?;
    let _matcher = get_string_option(
        ctx,
        options,
        "localeMatcher",
        CLASS,
        &["lookup", "best fit"],
        None,
    )?;
    let (numbering_system, locale) = crate::intl::helpers::resolve_unicode_keyword(
        &locale,
        "nu",
        get_numbering_system_option(ctx, options, CLASS)?,
        &crate::intl::supported::is_supported_numbering_system,
        "latn",
    );
    let style = get_string_option(
        ctx,
        options,
        "style",
        CLASS,
        &["long", "short", "narrow"],
        Some("long"),
    )?
    .unwrap_or_else(|| "long".to_string());
    let numeric = get_string_option(
        ctx,
        options,
        "numeric",
        CLASS,
        &["always", "auto"],
        Some("always"),
    )?
    .unwrap_or_else(|| "always".to_string());
    Ok(RelativeTimeFormatPayload {
        locale,
        style,
        numeric,
        numbering_system,
    })
}

fn require_payload(
    ctx: &NativeCtx<'_>,
    name: &'static str,
) -> Result<RelativeTimeFormatPayload, NativeError> {
    let bad = || NativeError::TypeError {
        name,
        reason: "intrinsic called on a non-Intl.RelativeTimeFormat receiver".to_string(),
    };
    let intl = ctx.this_value().as_intl(ctx.heap()).ok_or_else(bad)?;
    match intl.payload_clone(ctx.heap()) {
        IntlPayload::RelativeTimeFormat(p) => Ok(p),
        _ => Err(bad()),
    }
}

/// English unit pluralisation. `n.abs() === 1` → singular form.
fn unit_label(unit: &str, plural: bool, style: &str) -> &'static str {
    let narrow = style == "narrow";
    let short = style == "short" || narrow;
    match (unit, plural, short) {
        ("year" | "years", false, false) => "year",
        ("year" | "years", true, false) => "years",
        ("year" | "years", _, true) => "yr",
        ("quarter" | "quarters", false, false) => "quarter",
        ("quarter" | "quarters", true, false) => "quarters",
        ("quarter" | "quarters", _, true) => "qtr",
        ("month" | "months", false, false) => "month",
        ("month" | "months", true, false) => "months",
        ("month" | "months", _, true) => "mo",
        ("week" | "weeks", false, false) => "week",
        ("week" | "weeks", true, false) => "weeks",
        ("week" | "weeks", _, true) => "wk",
        ("day" | "days", false, false) => "day",
        ("day" | "days", true, false) => "days",
        ("day" | "days", _, true) => "day",
        ("hour" | "hours", false, false) => "hour",
        ("hour" | "hours", true, false) => "hours",
        ("hour" | "hours", _, true) => "hr",
        ("minute" | "minutes", false, false) => "minute",
        ("minute" | "minutes", true, false) => "minutes",
        ("minute" | "minutes", _, true) => "min",
        ("second" | "seconds", false, false) => "second",
        ("second" | "seconds", true, false) => "seconds",
        ("second" | "seconds", _, true) => "sec",
        _ => "unit",
    }
}

/// Build the icu relative-time formatter for the payload's locale,
/// style, and numeric option, keyed by the singular unit name.
fn icu_formatter(
    unit: &str,
    payload: &RelativeTimeFormatPayload,
) -> Option<icu_experimental::relativetime::RelativeTimeFormatter> {
    use icu_experimental::relativetime::options::Numeric;
    use icu_experimental::relativetime::{
        RelativeTimeFormatter as F, RelativeTimeFormatterOptions,
    };
    let locale: icu_locale::Locale = payload.locale.parse().ok()?;
    let prefs = (&locale).into();
    let mut options = RelativeTimeFormatterOptions::default();
    options.numeric = if payload.numeric == "auto" {
        Numeric::Auto
    } else {
        Numeric::Always
    };
    let ctor = match (payload.style.as_str(), singular_unit(unit)) {
        ("long", "second") => F::try_new_long_second,
        ("long", "minute") => F::try_new_long_minute,
        ("long", "hour") => F::try_new_long_hour,
        ("long", "day") => F::try_new_long_day,
        ("long", "week") => F::try_new_long_week,
        ("long", "month") => F::try_new_long_month,
        ("long", "quarter") => F::try_new_long_quarter,
        ("long", "year") => F::try_new_long_year,
        ("short", "second") => F::try_new_short_second,
        ("short", "minute") => F::try_new_short_minute,
        ("short", "hour") => F::try_new_short_hour,
        ("short", "day") => F::try_new_short_day,
        ("short", "week") => F::try_new_short_week,
        ("short", "month") => F::try_new_short_month,
        ("short", "quarter") => F::try_new_short_quarter,
        ("short", "year") => F::try_new_short_year,
        ("narrow", "second") => F::try_new_narrow_second,
        ("narrow", "minute") => F::try_new_narrow_minute,
        ("narrow", "hour") => F::try_new_narrow_hour,
        ("narrow", "day") => F::try_new_narrow_day,
        ("narrow", "week") => F::try_new_narrow_week,
        ("narrow", "month") => F::try_new_narrow_month,
        ("narrow", "quarter") => F::try_new_narrow_quarter,
        ("narrow", "year") => F::try_new_narrow_year,
        _ => return None,
    };
    ctor(prefs, options).ok()
}

/// The singular spelling of a sanctioned relative-time unit.
fn singular_unit(unit: &str) -> &str {
    unit.strip_suffix('s')
        .filter(|u| !u.is_empty())
        .unwrap_or(unit)
}

fn value_to_decimal(value: f64) -> Option<fixed_decimal::Decimal> {
    use std::str::FromStr as _;
    let rendered = if value == 0.0 && value.is_sign_negative() {
        "-0".to_string()
    } else {
        value.to_string()
    };
    fixed_decimal::Decimal::from_str(&rendered).ok()
}

fn render_format(value: f64, unit: &str, payload: &RelativeTimeFormatPayload) -> String {
    if let (Some(formatter), Some(decimal)) =
        (icu_formatter(unit, payload), value_to_decimal(value))
    {
        return writeable::Writeable::write_to_string(&formatter.format(decimal)).into_owned();
    }
    render_format_fallback(value, unit, payload)
}

/// The locale-formatted bare number for the value, matching the digits
/// the relative-time pattern interpolates (used to split parts).
fn render_number(value: f64, payload: &RelativeTimeFormatPayload) -> Option<String> {
    let locale: icu_locale::Locale = payload.locale.parse().ok()?;
    let formatter = icu_decimal::DecimalFormatter::try_new(
        (&locale).into(),
        icu_decimal::options::DecimalFormatterOptions::default(),
    )
    .ok()?;
    let decimal = value_to_decimal(value.abs())?.with_sign(fixed_decimal::Sign::None);
    Some(writeable::Writeable::write_to_string(&formatter.format(&decimal)).into_owned())
}

/// Pre-icu fallback used when the locale/unit has no bundled data.
fn render_format_fallback(value: f64, unit: &str, payload: &RelativeTimeFormatPayload) -> String {
    if value.is_nan() {
        return "NaN".to_string();
    }
    let plural = (value.abs() - 1.0).abs() > f64::EPSILON;
    let abs = format_number(value.abs());
    let label = unit_label(unit, plural, &payload.style);
    if value < 0.0 || (value == 0.0 && value.is_sign_negative()) {
        format!("{abs} {label} ago")
    } else {
        format!("in {abs} {label}")
    }
}

fn format_number(n: f64) -> String {
    if n.fract() == 0.0 {
        // §the relative-time value is rendered through the locale number
        // formatter; the foundation (en) applies `,` thousands grouping.
        let int = n as i64;
        let digits = int.abs().to_string();
        let mut grouped = String::new();
        let bytes = digits.as_bytes();
        for (i, b) in bytes.iter().enumerate() {
            if i > 0 && (bytes.len() - i).is_multiple_of(3) {
                grouped.push(',');
            }
            grouped.push(*b as char);
        }
        if int < 0 {
            format!("-{grouped}")
        } else {
            grouped
        }
    } else {
        format!("{n}")
    }
}

/// §18.4.3 `IsSanctionedSingleUnitIdentifier` for the relative-time
/// units, accepting both the singular and plural spellings.
fn is_valid_unit(unit: &str) -> bool {
    matches!(
        unit,
        "year"
            | "years"
            | "quarter"
            | "quarters"
            | "month"
            | "months"
            | "week"
            | "weeks"
            | "day"
            | "days"
            | "hour"
            | "hours"
            | "minute"
            | "minutes"
            | "second"
            | "seconds"
    )
}

/// §18.4.3 `Intl.RelativeTimeFormat.prototype.format(value, unit)`.
pub(crate) fn relative_time_format_format(
    ctx: &mut NativeCtx<'_>,
    args: &[Value],
) -> Result<Value, NativeError> {
    let payload = require_payload(ctx, "format")?;
    // §step 3 — `value = ? ToNumber(value)` (a Symbol / BigInt throws a
    // TypeError); a non-finite result is a RangeError.
    let first = args.first().copied().unwrap_or_else(Value::undefined);
    let exec = ctx
        .execution_context()
        .cloned()
        .ok_or_else(|| NativeError::TypeError {
            name: "format",
            reason: "missing execution context".to_string(),
        })?;
    let value = crate::coerce::to_number_or_throw(ctx.cx.interp, &exec, &first)
        .map(|n| n.as_f64())
        .map_err(|e| crate::native_function::vm_to_native_error(ctx.cx.interp, e, "format"))?;
    if !value.is_finite() {
        return Err(NativeError::RangeError {
            name: "format",
            reason: "value must be a finite number".to_string(),
        });
    }
    // §step 4 — `unit = ? ToString(unit)`, then validate against the
    // sanctioned relative-time units (a RangeError otherwise).
    let unit = {
        let unit_v = args.get(1).copied().unwrap_or_else(Value::undefined);
        crate::coerce::to_string_or_throw(ctx.cx.interp, &exec, &unit_v)
            .map_err(|e| crate::native_function::vm_to_native_error(ctx.cx.interp, e, "format"))?
    };
    if !is_valid_unit(&unit) {
        return Err(NativeError::RangeError {
            name: "format",
            reason: format!("invalid unit: {unit}"),
        });
    }
    let rendered = render_format(value, &unit, &payload);
    Ok(Value::string(JsString::from_str(
        &rendered,
        ctx.heap_mut(),
    )?))
}

/// §18.4.4 `Intl.RelativeTimeFormat.prototype.formatToParts(value, unit)`
/// — §18.5.3 PartitionRelativeTimePattern: the interpolated number's
/// integer/group/decimal/fraction segments carry a `unit` property; the
/// surrounding pattern text surfaces as `literal` parts.
pub(crate) fn relative_time_format_format_to_parts(
    ctx: &mut NativeCtx<'_>,
    args: &[Value],
) -> Result<Value, NativeError> {
    let payload = require_payload(ctx, "formatToParts")?;
    let first = args.first().copied().unwrap_or_else(Value::undefined);
    let exec = ctx
        .execution_context()
        .cloned()
        .ok_or_else(|| NativeError::TypeError {
            name: "formatToParts",
            reason: "missing execution context".to_string(),
        })?;
    let value = crate::coerce::to_number_or_throw(ctx.cx.interp, &exec, &first)
        .map(|n| n.as_f64())
        .map_err(|e| {
            crate::native_function::vm_to_native_error(ctx.cx.interp, e, "formatToParts")
        })?;
    if !value.is_finite() {
        return Err(NativeError::RangeError {
            name: "formatToParts",
            reason: "value must be a finite number".to_string(),
        });
    }
    let unit = {
        let unit_v = args.get(1).copied().unwrap_or_else(Value::undefined);
        crate::coerce::to_string_or_throw(ctx.cx.interp, &exec, &unit_v).map_err(|e| {
            crate::native_function::vm_to_native_error(ctx.cx.interp, e, "formatToParts")
        })?
    };
    if !is_valid_unit(&unit) {
        return Err(NativeError::RangeError {
            name: "formatToParts",
            reason: format!("invalid unit: {unit}"),
        });
    }
    let full = render_format(value, &unit, &payload);
    let number = render_number(value, &payload);
    // (type, value, carries-unit)
    let mut triples: Vec<(&'static str, String, bool)> = Vec::new();
    match number.as_deref().and_then(|n| full.find(n).map(|i| (i, n))) {
        Some((idx, num)) => {
            if idx > 0 {
                triples.push(("literal", full[..idx].to_string(), false));
            }
            push_number_segments(&mut triples, num, value.fract() != 0.0);
            let rest = &full[idx + num.len()..];
            if !rest.is_empty() {
                triples.push(("literal", rest.to_string(), false));
            }
        }
        // numeric:"auto" relatives ("now", "yesterday") interpolate no
        // number — a single literal part.
        None => triples.push(("literal", full.clone(), false)),
    }

    let singular = singular_unit(&unit).to_string();
    let mut elements: Vec<Value> = Vec::with_capacity(triples.len());
    for (ty, val, has_unit) in &triples {
        let ty_s = Value::string(JsString::from_str(ty, ctx.heap_mut())?);
        let val_s = Value::string(JsString::from_str(val, ctx.heap_mut())?);
        let unit_s = if *has_unit {
            Some(Value::string(JsString::from_str(
                &singular,
                ctx.heap_mut(),
            )?))
        } else {
            None
        };
        let snapshot = elements.clone();
        let mut roots = vec![&ty_s, &val_s];
        if let Some(u) = &unit_s {
            roots.push(u);
        }
        let mut part = ctx.alloc_object_with_roots(&roots, &[&snapshot])?;
        crate::object::set(&mut part, ctx.heap_mut(), "type", ty_s);
        crate::object::set(&mut part, ctx.heap_mut(), "value", val_s);
        if let Some(u) = unit_s {
            crate::object::set(&mut part, ctx.heap_mut(), "unit", u);
        }
        elements.push(Value::object(part));
    }
    let element_roots = elements.clone();
    let mut external_visit = |visitor: &mut dyn FnMut(*mut RawGc)| {
        for v in &element_roots {
            v.trace_value_slots(visitor);
        }
    };
    let arr =
        crate::array::from_elements_with_roots(ctx.heap_mut(), elements, &mut external_visit)?;
    Ok(Value::array(arr))
}

/// Split a locale-formatted number into ECMA-402 numeric part types.
/// Digit runs become `integer`, single non-digit separators between
/// digit runs become `group`, and a final separator + digits pair
/// becomes `decimal` + `fraction` when the separator differs from the
/// grouping character (heuristic sufficient for grouped integers).
fn push_number_segments(
    parts: &mut Vec<(&'static str, String, bool)>,
    num: &str,
    has_fraction: bool,
) {
    // With a fractional value the LAST separator is the decimal point
    // and everything after it the fraction; all earlier separators are
    // grouping characters.
    let decimal_at = if has_fraction {
        num.char_indices()
            .filter(|(_, c)| !c.is_numeric())
            .map(|(i, _)| i)
            .next_back()
    } else {
        None
    };
    let mut current = String::new();
    for (i, ch) in num.char_indices() {
        if ch.is_numeric() {
            current.push(ch);
        } else {
            if !current.is_empty() {
                parts.push(("integer", std::mem::take(&mut current), true));
            }
            if Some(i) == decimal_at {
                parts.push(("decimal", ch.to_string(), true));
            } else {
                parts.push(("group", ch.to_string(), true));
            }
        }
    }
    if !current.is_empty() {
        if decimal_at.is_some() {
            parts.push(("fraction", current, true));
        } else {
            parts.push(("integer", current, true));
        }
    }
}

/// §18.4.5 `Intl.RelativeTimeFormat.prototype.resolvedOptions()`.
pub(crate) fn relative_time_format_resolved_options(
    ctx: &mut NativeCtx<'_>,
    _args: &[Value],
) -> Result<Value, NativeError> {
    let payload = require_payload(ctx, "resolvedOptions")?;
    let locale = Value::string(JsString::from_str(&payload.locale, ctx.heap_mut())?);
    let style = Value::string(JsString::from_str(&payload.style, ctx.heap_mut())?);
    let numeric = Value::string(JsString::from_str(&payload.numeric, ctx.heap_mut())?);
    let numbering_system = Value::string(JsString::from_str(
        &payload.numbering_system,
        ctx.heap_mut(),
    )?);
    let mut obj =
        ctx.alloc_object_with_roots(&[&locale, &style, &numeric, &numbering_system], &[])?;
    let heap = ctx.heap_mut();
    crate::object::set(&mut obj, heap, "locale", locale);
    crate::object::set(&mut obj, heap, "style", style);
    crate::object::set(&mut obj, heap, "numeric", numeric);
    crate::object::set(&mut obj, heap, "numberingSystem", numbering_system);
    Ok(Value::object(obj))
}
