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
        numbering_system: numbering_system.unwrap_or_else(|| "latn".to_string()),
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
        format!("{}", n as i64)
    } else {
        format!("{n}")
    }
}

/// §18.4.3 `Intl.RelativeTimeFormat.prototype.format(value, unit)`.
pub(crate) fn relative_time_format_format(
    ctx: &mut NativeCtx<'_>,
    args: &[Value],
) -> Result<Value, NativeError> {
    let payload = require_payload(ctx, "format")?;
    let first = args.first();
    let value = if let Some(n) = first.and_then(|v| v.as_number()) {
        n.as_f64()
    } else if let Some(b) = first.and_then(|v| v.as_boolean()) {
        if b { 1.0 } else { 0.0 }
    } else if first.is_none() || first.is_some_and(|v| v.is_null()) {
        0.0
    } else {
        f64::NAN
    };
    let Some(unit_str) = args.get(1).and_then(|v| v.as_string(ctx.heap())) else {
        return Err(NativeError::TypeError {
            name: "format",
            reason: "argument 1 must be a string unit".to_string(),
        });
    };
    let unit = unit_str.to_lossy_string(ctx.heap());
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
    let part = ctx.alloc_object_with_roots(&[&literal, &s], &[])?;
    crate::object::set(part, ctx.heap_mut(), "type", literal);
    crate::object::set(part, ctx.heap_mut(), "value", s);
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
    let obj = ctx.alloc_object_with_roots(&[&locale, &style, &numeric, &numbering_system], &[])?;
    let heap = ctx.heap_mut();
    crate::object::set(obj, heap, "locale", locale);
    crate::object::set(obj, heap, "style", style);
    crate::object::set(obj, heap, "numeric", numeric);
    crate::object::set(obj, heap, "numberingSystem", numbering_system);
    Ok(Value::object(obj))
}
