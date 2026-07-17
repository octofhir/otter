//! `Intl.RelativeTimeFormat` — locale-aware relative-time strings.
//!
//! Foundation surface: English long-form templates such as
//! `"in 3 days"` / `"5 minutes ago"`. The full ICU CLDR pattern
//! database is filed alongside the wider Intl follow-up.
//!
//! # Contents
//!
//! - Relative-time option resolution and ICU-backed rendering.
//! - Number-pattern partitioning for `formatToParts`.
//! - Rooted builders for parts arrays and `resolvedOptions`.
//!
//! # Invariants
//!
//! - Every GC value retained across an allocation lives in a handle-scope
//!   [`crate::Local`] and is reread at each mutation boundary.
//! - Parts arrays are allocated once and filled directly without cloning
//!   accumulated GC values or materializing root snapshots.
//!
//! # See also
//! - <https://tc39.es/ecma402/#relativetimeformat-objects>

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
    let number = ctx.with_turn_parts(|interp, stack| {
        crate::coerce::to_number_or_throw(interp, stack, &exec, &first)
    });
    let value = number
        .map(|n| n.as_f64())
        .map_err(|e| crate::native_function::vm_to_native_error(ctx.interp_mut(), e, "format"))?;
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
        let string = ctx.with_turn_parts(|interp, stack| {
            crate::coerce::to_string_or_throw(interp, stack, &exec, &unit_v)
        });
        string.map_err(|e| {
            crate::native_function::vm_to_native_error(ctx.interp_mut(), e, "format")
        })?
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
    let number = ctx.with_turn_parts(|interp, stack| {
        crate::coerce::to_number_or_throw(interp, stack, &exec, &first)
    });
    let value = number.map(|n| n.as_f64()).map_err(|e| {
        crate::native_function::vm_to_native_error(ctx.interp_mut(), e, "formatToParts")
    })?;
    if !value.is_finite() {
        return Err(NativeError::RangeError {
            name: "formatToParts",
            reason: "value must be a finite number".to_string(),
        });
    }
    let unit = {
        let unit_v = args.get(1).copied().unwrap_or_else(Value::undefined);
        let string = ctx.with_turn_parts(|interp, stack| {
            crate::coerce::to_string_or_throw(interp, stack, &exec, &unit_v)
        });
        string.map_err(|e| {
            crate::native_function::vm_to_native_error(ctx.interp_mut(), e, "formatToParts")
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
    ctx.scope(|mut scope| {
        let result = scope.array(triples.len())?;
        let unit = if triples.iter().any(|(_, _, has_unit)| *has_unit) {
            Some(scope.string(&singular)?)
        } else {
            None
        };

        for (index, (ty, value, has_unit)) in triples.iter().enumerate() {
            scope.scope(|mut part_scope| {
                let part = part_scope.object()?;
                let ty = part_scope.string(ty)?;
                part_scope.set(part, "type", ty)?;
                let value = part_scope.string(value)?;
                part_scope.set(part, "value", value)?;
                if *has_unit {
                    part_scope.set(
                        part,
                        "unit",
                        unit.expect("numeric relative-time part has a rooted unit"),
                    )?;
                }
                part_scope.set_index(result, index, part)
            })?;
        }

        Ok(scope.finish(result))
    })
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
    ctx.scope(|mut scope| {
        let result = scope.object()?;
        let locale = scope.string(&payload.locale)?;
        scope.set(result, "locale", locale)?;
        let style = scope.string(&payload.style)?;
        scope.set(result, "style", style)?;
        let numeric = scope.string(&payload.numeric)?;
        scope.set(result, "numeric", numeric)?;
        let numbering_system = scope.string(&payload.numbering_system)?;
        scope.set(result, "numberingSystem", numbering_system)?;
        Ok(scope.finish(result))
    })
}
