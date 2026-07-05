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

use crate::intl::helpers::DEFAULT_LOCALE;
use crate::intl::payload::{DisplayNamesPayload, IntlPayload};
use crate::string::JsString;
use crate::{NativeCtx, NativeError, Value};

const CLASS: &str = "DisplayNames";

/// §12.1.1 InitializeDisplayNames — fires `localeMatcher` / `style` /
/// `type` / `fallback` / `languageDisplay` getters in spec order with
/// ToString coercion + RangeError validation; `type` is required
/// (TypeError when absent), and a missing options bag is a TypeError.
/// The locale is canonicalized.
pub fn resolve_ctx(
    ctx: &mut NativeCtx<'_>,
    locales: Value,
    options: Value,
) -> Result<DisplayNamesPayload, NativeError> {
    use crate::intl::helpers::{get_string_option, require_options_object};

    let requested = crate::intl::supported::canonicalize_locale_list(ctx, locales)?;
    let locale = requested
        .into_iter()
        .next()
        .unwrap_or_else(|| DEFAULT_LOCALE.to_string());
    if options.is_undefined() {
        return Err(NativeError::TypeError {
            name: CLASS,
            reason: "options must be provided".to_string(),
        });
    }
    let options = require_options_object(options, CLASS)?;

    let _matcher = get_string_option(
        ctx,
        options,
        "localeMatcher",
        CLASS,
        &["lookup", "best fit"],
        None,
    )?;
    let style = get_string_option(
        ctx,
        options,
        "style",
        CLASS,
        &["narrow", "short", "long"],
        Some("long"),
    )?
    .unwrap_or_else(|| "long".to_string());
    let kind = get_string_option(
        ctx,
        options,
        "type",
        CLASS,
        &[
            "language",
            "region",
            "script",
            "currency",
            "calendar",
            "dateTimeField",
        ],
        None,
    )?
    .ok_or_else(|| NativeError::TypeError {
        name: CLASS,
        reason: "the `type` option is required".to_string(),
    })?;
    let fallback = get_string_option(
        ctx,
        options,
        "fallback",
        CLASS,
        &["code", "none"],
        Some("code"),
    )?
    .unwrap_or_else(|| "code".to_string());
    let language_display = get_string_option(
        ctx,
        options,
        "languageDisplay",
        CLASS,
        &["dialect", "standard"],
        Some("dialect"),
    )?
    .unwrap_or_else(|| "dialect".to_string());

    Ok(DisplayNamesPayload {
        locale,
        language_display: (kind == "language").then_some(language_display),
        kind,
        style,
        fallback,
    })
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

fn range_err(reason: impl Into<String>) -> NativeError {
    NativeError::RangeError {
        name: "of",
        reason: reason.into(),
    }
}

fn is_ascii_alpha_string(s: &str) -> bool {
    s.bytes().all(|b| b.is_ascii_alphabetic())
}

fn is_ascii_digit_string(s: &str) -> bool {
    s.bytes().all(|b| b.is_ascii_digit())
}

fn is_ascii_alnum_string(s: &str) -> bool {
    s.bytes().all(|b| b.is_ascii_alphanumeric())
}

fn is_unicode_region_subtag(code: &str) -> bool {
    (code.len() == 2 && is_ascii_alpha_string(code))
        || (code.len() == 3 && is_ascii_digit_string(code))
}

fn is_unicode_variant_subtag(code: &str) -> bool {
    ((5..=8).contains(&code.len()) && is_ascii_alnum_string(code))
        || (code.len() == 4 && code.as_bytes()[0].is_ascii_digit() && is_ascii_alnum_string(code))
}

fn validate_language_code(code: &str) -> Result<(), NativeError> {
    if code == "root" || code.contains('_') || code.starts_with('-') || code.ends_with('-') {
        return Err(range_err("invalid language code"));
    }
    let subtags: Vec<&str> = code.split('-').collect();
    if subtags.iter().any(|subtag| subtag.is_empty()) {
        return Err(range_err("invalid language code"));
    }
    let Some(language) = subtags.first() else {
        return Err(range_err("invalid language code"));
    };
    if !matches!(language.len(), 2 | 3 | 5..=8) || !is_ascii_alpha_string(language) {
        return Err(range_err("invalid language code"));
    }
    let mut idx = 1;
    if subtags
        .get(idx)
        .is_some_and(|subtag| subtag.len() == 4 && is_ascii_alpha_string(subtag))
    {
        idx += 1;
    }
    if subtags
        .get(idx)
        .is_some_and(|subtag| is_unicode_region_subtag(subtag))
    {
        idx += 1;
    }
    let mut variants: Vec<String> = Vec::new();
    while let Some(subtag) = subtags.get(idx) {
        if !is_unicode_variant_subtag(subtag) {
            return Err(range_err("invalid language code"));
        }
        let lower = subtag.to_ascii_lowercase();
        if variants.iter().any(|seen| seen == &lower) {
            return Err(range_err("duplicate language variant"));
        }
        variants.push(lower);
        idx += 1;
    }
    Ok(())
}

fn validate_calendar_code(code: &str) -> Result<(), NativeError> {
    let subtags: Vec<&str> = code.split('-').collect();
    if subtags.is_empty()
        || subtags
            .iter()
            .any(|subtag| !(3..=8).contains(&subtag.len()) || !is_ascii_alnum_string(subtag))
    {
        return Err(range_err("invalid calendar code"));
    }
    Ok(())
}

fn validate_datetime_field_code(code: &str) -> Result<(), NativeError> {
    match code {
        "era" | "year" | "quarter" | "month" | "weekOfYear" | "weekday" | "day" | "dayPeriod"
        | "hour" | "minute" | "second" | "timeZoneName" => Ok(()),
        _ => Err(range_err("invalid dateTimeField code")),
    }
}

fn validate_display_name_code(kind: &str, code: &str) -> Result<(), NativeError> {
    match kind {
        "language" => validate_language_code(code),
        "region" => {
            if is_unicode_region_subtag(code) {
                Ok(())
            } else {
                Err(range_err("invalid region code"))
            }
        }
        "script" => {
            if code.len() == 4 && is_ascii_alpha_string(code) {
                Ok(())
            } else {
                Err(range_err("invalid script code"))
            }
        }
        "currency" => {
            if code.len() == 3 && is_ascii_alpha_string(code) {
                Ok(())
            } else {
                Err(range_err("invalid currency code"))
            }
        }
        "calendar" => validate_calendar_code(code),
        "dateTimeField" => validate_datetime_field_code(code),
        _ => Ok(()),
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
    validate_display_name_code(&payload.kind, &code)?;
    if let Some(name) = lookup_name(&payload.kind, &code) {
        return Ok(Value::string(JsString::from_str(name, ctx.heap_mut())?));
    }
    let supported_identifier = match payload.kind.as_str() {
        "calendar" => crate::intl::supported::is_supported_calendar(&code.to_ascii_lowercase()),
        "currency" => crate::intl::supported::is_supported_currency(&code.to_ascii_uppercase()),
        _ => false,
    };
    if supported_identifier {
        return Ok(Value::string(JsString::from_str(&code, ctx.heap_mut())?));
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
    let language_display = if let Some(language_display) = payload.language_display.as_deref() {
        Some(Value::string(JsString::from_str(
            language_display,
            ctx.heap_mut(),
        )?))
    } else {
        None
    };
    let language_display_root = language_display.unwrap_or_else(Value::undefined);
    let mut obj = ctx.alloc_object_with_roots(
        &[&locale, &kind, &style, &fallback, &language_display_root],
        &[],
    )?;
    if let Some(proto) = ctx.cx.interp.object_prototype_object_opt() {
        crate::object::set_prototype(obj, ctx.heap_mut(), Some(proto));
    }
    let heap = ctx.heap_mut();
    crate::object::set(&mut obj, heap, "locale", locale);
    crate::object::set(&mut obj, heap, "style", style);
    crate::object::set(&mut obj, heap, "type", kind);
    crate::object::set(&mut obj, heap, "fallback", fallback);
    if !language_display_root.is_undefined() {
        crate::object::set(&mut obj, heap, "languageDisplay", language_display_root);
    }
    Ok(Value::object(obj))
}
