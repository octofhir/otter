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
    DEFAULT_LOCALE, get_numbering_system_option, get_string_option, require_options_object,
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
    let options = require_options_object(options, CLASS)?;
    let _matcher = get_string_option(
        ctx,
        options,
        "localeMatcher",
        CLASS,
        &["lookup", "best fit"],
        None,
    )?;
    let numbering_system = get_numbering_system_option(ctx, options, CLASS)?;
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
        numbering_system: numbering_system
            .filter(|ns| crate::intl::supported::is_supported_numbering_system(ns))
            .unwrap_or_else(|| "latn".to_string()),
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

fn render_format(value: f64, unit: &str, payload: &RelativeTimeFormatPayload) -> String {
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
/// — foundation returns a single `{ type: "literal", value: <full
/// string> }` part. The shape is spec-compatible; per-token splitting
/// arrives with the full ICU integration.
pub(crate) fn relative_time_format_format_to_parts(
    ctx: &mut NativeCtx<'_>,
    args: &[Value],
) -> Result<Value, NativeError> {
    let s = relative_time_format_format(ctx, args)?;
    let literal = Value::string(JsString::from_str("literal", ctx.heap_mut())?);
    let mut part = ctx.alloc_object_with_roots(&[&literal, &s], &[])?;
    crate::object::set(&mut part, ctx.heap_mut(), "type", literal);
    crate::object::set(&mut part, ctx.heap_mut(), "value", s);
    let elements = vec![Value::object(part)];
    let roots = ctx.collect_native_roots();
    let this_value = *ctx.this_value();
    let element_roots = elements.clone();
    let mut external_visit = |visitor: &mut dyn FnMut(*mut RawGc)| {
        for &slot in &roots {
            visitor(slot);
        }
        this_value.trace_value_slots(visitor);
        for v in &element_roots {
            v.trace_value_slots(visitor);
        }
    };
    let arr =
        crate::array::from_elements_with_roots(ctx.heap_mut(), elements, &mut external_visit)?;
    Ok(Value::array(arr))
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
