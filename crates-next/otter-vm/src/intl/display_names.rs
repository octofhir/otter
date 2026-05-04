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

use std::sync::LazyLock;

use crate::Value;
use crate::intl::dispatch::IntlError;
use crate::intl::helpers::{coerce_locale, js_string, options_object, read_string_option};
use crate::intl::payload::{DisplayNamesPayload, IntlPayload};
use crate::intrinsics::{IntrinsicArgs, IntrinsicError, IntrinsicReceiver, IntrinsicTable};

/// Resolve constructor options for this Intl class.
pub fn resolve(locale: &Value, options: &Value, gc_heap: &otter_gc::GcHeap) -> DisplayNamesPayload {
    let opts = options_object(Some(options));
    let opts_ref = opts.as_ref();
    DisplayNamesPayload {
        locale: coerce_locale(Some(locale)),
        kind: read_string_option(opts_ref, "type", "language", gc_heap),
        style: read_string_option(opts_ref, "style", "long", gc_heap),
        fallback: read_string_option(opts_ref, "fallback", "code", gc_heap),
    }
}

fn require_payload<'a>(
    args: &'a IntrinsicArgs<'_>,
) -> Result<&'a DisplayNamesPayload, IntrinsicError> {
    match args.receiver {
        Value::Intl(intl) => match intl.payload() {
            IntlPayload::DisplayNames(p) => Ok(p),
            _ => Err(IntrinsicError::BadReceiver {
                expected: "Intl.DisplayNames",
            }),
        },
        _ => Err(IntrinsicError::BadReceiver {
            expected: "Intl.DisplayNames",
        }),
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

/// §12.5.5 `of(code)`.
fn impl_of(args: &IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    let payload = require_payload(args)?;
    let code = match args.args.first() {
        Some(Value::String(s)) => s.to_lossy_string(),
        Some(Value::Number(n)) => n.to_display_string(),
        _ => {
            return Err(IntrinsicError::BadArgument {
                index: 0,
                reason: "must be a string code",
            });
        }
    };
    if let Some(name) = lookup_name(&payload.kind, &code) {
        return Ok(Value::String(crate::string::JsString::from_str(
            name,
            args.string_heap,
        )?));
    }
    if payload.fallback == "none" {
        return Ok(Value::Undefined);
    }
    Ok(Value::String(crate::string::JsString::from_str(
        &code,
        args.string_heap,
    )?))
}

fn impl_resolved_options(args: &IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    let payload = require_payload(args)?;
    let locale = js_string(&payload.locale, args.string_heap).map_err(intl_to_intrinsic)?;
    let kind = js_string(&payload.kind, args.string_heap).map_err(intl_to_intrinsic)?;
    let style = js_string(&payload.style, args.string_heap).map_err(intl_to_intrinsic)?;
    let fallback = js_string(&payload.fallback, args.string_heap).map_err(intl_to_intrinsic)?;
    let mut heap = args.gc_heap.borrow_mut();
    let obj = crate::object::alloc_object(*heap)?;
    crate::object::set(obj, *heap, "locale", locale);
    crate::object::set(obj, *heap, "type", kind);
    crate::object::set(obj, *heap, "style", style);
    crate::object::set(obj, *heap, "fallback", fallback);
    Ok(Value::Object(obj))
}

fn intl_to_intrinsic(err: IntlError) -> IntrinsicError {
    match err {
        IntlError::OutOfMemory {
            requested_bytes,
            heap_limit_bytes,
        } => IntrinsicError::OutOfMemory {
            requested_bytes,
            heap_limit_bytes,
        },
        _ => IntrinsicError::BadReceiver {
            expected: "Intl.DisplayNames",
        },
    }
}

/// `Intl.DisplayNames.prototype` table.
pub static DISPLAY_NAMES_PROTOTYPE_TABLE: LazyLock<IntrinsicTable> = LazyLock::new(|| {
    crate::intrinsics!(
        Intl,
        "of"               / 1 => impl_of,
        "resolvedOptions"  / 0 => impl_resolved_options,
    )
});

#[must_use]
/// Convenience accessor used by [`super::lookup_prototype`].
pub fn lookup(name: &str) -> Option<&'static crate::intrinsics::IntrinsicEntry> {
    DISPLAY_NAMES_PROTOTYPE_TABLE.lookup(IntrinsicReceiver::Intl, name)
}
