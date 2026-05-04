//! `Intl.RelativeTimeFormat` — locale-aware relative-time strings.
//!
//! Foundation surface: English long-form templates such as
//! `"in 3 days"` / `"5 minutes ago"`. The full ICU CLDR pattern
//! database is filed alongside the wider Intl follow-up.
//!
//! # See also
//! - <https://tc39.es/ecma402/#relativetimeformat-objects>

use std::sync::LazyLock;

use crate::Value;
use crate::intl::dispatch::IntlError;
use crate::intl::helpers::{coerce_locale, js_string, options_object, read_string_option};
use crate::intl::payload::{IntlPayload, RelativeTimeFormatPayload};
use crate::intrinsics::{IntrinsicArgs, IntrinsicError, IntrinsicReceiver, IntrinsicTable};

/// Resolve constructor options for this Intl class.
pub fn resolve(
    locale: &Value,
    options: &Value,
    gc_heap: &otter_gc::GcHeap,
) -> RelativeTimeFormatPayload {
    let opts = options_object(Some(options));
    let opts_ref = opts.as_ref();
    RelativeTimeFormatPayload {
        locale: coerce_locale(Some(locale)),
        style: read_string_option(opts_ref, "style", "long", gc_heap),
        numeric: read_string_option(opts_ref, "numeric", "always", gc_heap),
    }
}

fn require_payload<'a>(
    args: &'a IntrinsicArgs<'_>,
) -> Result<&'a RelativeTimeFormatPayload, IntrinsicError> {
    match args.receiver {
        Value::Intl(intl) => match intl.payload() {
            IntlPayload::RelativeTimeFormat(p) => Ok(p),
            _ => Err(IntrinsicError::BadReceiver {
                expected: "Intl.RelativeTimeFormat",
            }),
        },
        _ => Err(IntrinsicError::BadReceiver {
            expected: "Intl.RelativeTimeFormat",
        }),
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

/// §17.5.3 `format(value, unit)`.
fn impl_format(args: &IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    let payload = require_payload(args)?;
    let value = match args.args.first() {
        Some(Value::Number(n)) => n.as_f64(),
        Some(Value::Boolean(true)) => 1.0,
        Some(Value::Boolean(false)) | Some(Value::Null) | None => 0.0,
        _ => f64::NAN,
    };
    let unit = match args.args.get(1) {
        Some(Value::String(s)) => s.to_lossy_string(),
        _ => {
            return Err(IntrinsicError::BadArgument {
                index: 1,
                reason: "must be a string unit",
            });
        }
    };
    let rendered = render_format(value, &unit, payload);
    Ok(Value::String(crate::string::JsString::from_str(
        &rendered,
        args.string_heap,
    )?))
}

/// §17.5.4 `formatToParts(value, unit)` — foundation returns a
/// single `{ type: "literal", value: <full string> }` part. The
/// shape is spec-compatible; per-token splitting arrives with the
/// full ICU integration.
fn impl_format_to_parts(args: &IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    let s = impl_format(args)?;
    let literal = Value::String(crate::string::JsString::from_str(
        "literal",
        args.string_heap,
    )?);
    let mut heap = args.gc_heap.borrow_mut();
    let part = crate::object::alloc_object(*heap)?;
    crate::object::set(part, *heap, "type", literal);
    crate::object::set(part, *heap, "value", s);
    Ok(Value::Array(crate::array::JsArray::from_elements([
        Value::Object(part),
    ])))
}

fn impl_resolved_options(args: &IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    let payload = require_payload(args)?;
    let locale = js_string(&payload.locale, args.string_heap).map_err(intl_to_intrinsic)?;
    let style = js_string(&payload.style, args.string_heap).map_err(intl_to_intrinsic)?;
    let numeric = js_string(&payload.numeric, args.string_heap).map_err(intl_to_intrinsic)?;
    let mut heap = args.gc_heap.borrow_mut();
    let obj = crate::object::alloc_object(*heap)?;
    crate::object::set(obj, *heap, "locale", locale);
    crate::object::set(obj, *heap, "style", style);
    crate::object::set(obj, *heap, "numeric", numeric);
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
            expected: "Intl.RelativeTimeFormat",
        },
    }
}

/// `Intl.RelativeTimeFormat.prototype` table.
pub static RELATIVE_TIME_PROTOTYPE_TABLE: LazyLock<IntrinsicTable> = LazyLock::new(|| {
    crate::intrinsics!(
        Intl,
        "format"           / 2 => impl_format,
        "formatToParts"    / 2 => impl_format_to_parts,
        "resolvedOptions"  / 0 => impl_resolved_options,
    )
});

#[must_use]
/// Convenience accessor used by [`super::lookup_prototype`].
pub fn lookup(name: &str) -> Option<&'static crate::intrinsics::IntrinsicEntry> {
    RELATIVE_TIME_PROTOTYPE_TABLE.lookup(IntrinsicReceiver::Intl, name)
}
