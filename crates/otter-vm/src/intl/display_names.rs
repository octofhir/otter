//! `Intl.DisplayNames` — locale-aware display names for codes.
//!
//! Foundation surface: a small English lookup table for the most
//! common BCP-47 language tags, ISO 3166 region codes, ISO 4217
//! currencies, and ISO 15924 scripts. Unknown codes fall back to
//! the supplied `fallback` option (`"code"` returns the input;
//! `"none"` returns `undefined`).
//!
//! # See also
//! - <https://tc39.es/ecma402/#sec-intl-displaynames-objects>

use crate::intl::helpers::{coerce_locale, options_object, read_string_option};
use crate::intl::payload::{DisplayNamesPayload, IntlPayload};
use crate::string::JsString;
use crate::{NativeCtx, NativeError, Value};

/// Resolve constructor options for this Intl class.
pub fn resolve(locale: &Value, options: &Value, gc_heap: &otter_gc::GcHeap) -> DisplayNamesPayload {
    let opts = options_object(Some(options));
    let opts_ref = opts.as_ref();
    DisplayNamesPayload {
        locale: coerce_locale(Some(locale), gc_heap),
        kind: read_string_option(opts_ref, "type", "language", gc_heap),
        style: read_string_option(opts_ref, "style", "long", gc_heap),
        fallback: read_string_option(opts_ref, "fallback", "code", gc_heap),
    }
}

fn require_payload(
    ctx: &NativeCtx<'_>,
    name: &'static str,
) -> Result<DisplayNamesPayload, NativeError> {
    let bad = || NativeError::TypeError {
        name,
        reason: "intrinsic called on a non-Intl.DisplayNames receiver".to_string(),
    };
    let intl = ctx.this_value().as_intl(ctx.heap()).ok_or_else(bad)?;
    match intl.payload_clone(ctx.heap()) {
        IntlPayload::DisplayNames(p) => Ok(p),
        _ => Err(bad()),
    }
}

fn lookup_name(kind: &str, code: &str) -> Option<&'static str> {
    let lower = code.to_ascii_lowercase();
    let upper = code.to_ascii_uppercase();
    match kind {
        "language" => match lower.as_str() {
            "en" => Some("English"),
            "fr" => Some("French"),
            "de" => Some("German"),
            "es" => Some("Spanish"),
            "it" => Some("Italian"),
            "pt" => Some("Portuguese"),
            "ru" => Some("Russian"),
            "zh" => Some("Chinese"),
            "ja" => Some("Japanese"),
            "ko" => Some("Korean"),
            "ar" => Some("Arabic"),
            "uk" => Some("Ukrainian"),
            "pl" => Some("Polish"),
            "nl" => Some("Dutch"),
            "sv" => Some("Swedish"),
            "tr" => Some("Turkish"),
            "hi" => Some("Hindi"),
            _ => None,
        },
        "region" => match upper.as_str() {
            "US" => Some("United States"),
            "GB" => Some("United Kingdom"),
            "FR" => Some("France"),
            "DE" => Some("Germany"),
            "ES" => Some("Spain"),
            "IT" => Some("Italy"),
            "PT" => Some("Portugal"),
            "RU" => Some("Russia"),
            "CN" => Some("China"),
            "JP" => Some("Japan"),
            "KR" => Some("South Korea"),
            "BR" => Some("Brazil"),
            "CA" => Some("Canada"),
            "MX" => Some("Mexico"),
            "AU" => Some("Australia"),
            "NZ" => Some("New Zealand"),
            "IN" => Some("India"),
            "UA" => Some("Ukraine"),
            "PL" => Some("Poland"),
            "NL" => Some("Netherlands"),
            "SE" => Some("Sweden"),
            "TR" => Some("Turkey"),
            _ => None,
        },
        "script" => match upper.as_str() {
            "LATN" => Some("Latin"),
            "CYRL" => Some("Cyrillic"),
            "GREK" => Some("Greek"),
            "ARAB" => Some("Arabic"),
            "HANS" => Some("Simplified Han"),
            "HANT" => Some("Traditional Han"),
            "JPAN" => Some("Japanese"),
            "KORE" => Some("Korean"),
            "DEVA" => Some("Devanagari"),
            _ => None,
        },
        "currency" => match upper.as_str() {
            "USD" => Some("US Dollar"),
            "EUR" => Some("Euro"),
            "GBP" => Some("British Pound"),
            "JPY" => Some("Japanese Yen"),
            "CNY" => Some("Chinese Yuan"),
            "RUB" => Some("Russian Ruble"),
            "INR" => Some("Indian Rupee"),
            "BRL" => Some("Brazilian Real"),
            "CAD" => Some("Canadian Dollar"),
            "AUD" => Some("Australian Dollar"),
            "CHF" => Some("Swiss Franc"),
            "KRW" => Some("South Korean Won"),
            _ => None,
        },
        "calendar" => match lower.as_str() {
            "gregory" => Some("Gregorian Calendar"),
            "buddhist" => Some("Buddhist Calendar"),
            "chinese" => Some("Chinese Calendar"),
            "hebrew" => Some("Hebrew Calendar"),
            "islamic" => Some("Islamic Calendar"),
            "japanese" => Some("Japanese Calendar"),
            "persian" => Some("Persian Calendar"),
            _ => None,
        },
        _ => None,
    }
}

/// §12.4.3 `Intl.DisplayNames.prototype.of(code)`.
pub(crate) fn display_names_of(
    ctx: &mut NativeCtx<'_>,
    args: &[Value],
) -> Result<Value, NativeError> {
    let payload = require_payload(ctx, "of")?;
    let code = if let Some(s) = args.first().and_then(|v| v.as_string(ctx.heap())) {
        s.to_lossy_string(ctx.heap())
    } else if let Some(n) = args.first().and_then(|v| v.as_number()) {
        n.to_display_string()
    } else {
        return Err(NativeError::TypeError {
            name: "of",
            reason: "argument 0 must be a string code".to_string(),
        });
    };
    if let Some(name) = lookup_name(&payload.kind, &code) {
        return Ok(Value::string(JsString::from_str(name, ctx.heap_mut())?));
    }
    if payload.fallback == "none" {
        return Ok(Value::undefined());
    }
    Ok(Value::string(JsString::from_str(&code, ctx.heap_mut())?))
}

/// §12.4.4 `Intl.DisplayNames.prototype.resolvedOptions()`.
pub(crate) fn display_names_resolved_options(
    ctx: &mut NativeCtx<'_>,
    _args: &[Value],
) -> Result<Value, NativeError> {
    let payload = require_payload(ctx, "resolvedOptions")?;
    let locale = Value::string(JsString::from_str(&payload.locale, ctx.heap_mut())?);
    let kind = Value::string(JsString::from_str(&payload.kind, ctx.heap_mut())?);
    let style = Value::string(JsString::from_str(&payload.style, ctx.heap_mut())?);
    let fallback = Value::string(JsString::from_str(&payload.fallback, ctx.heap_mut())?);
    let obj = ctx.alloc_object_with_roots(&[&locale, &kind, &style, &fallback], &[])?;
    let heap = ctx.heap_mut();
    crate::object::set(obj, heap, "locale", locale);
    crate::object::set(obj, heap, "type", kind);
    crate::object::set(obj, heap, "style", style);
    crate::object::set(obj, heap, "fallback", fallback);
    Ok(Value::object(obj))
}
