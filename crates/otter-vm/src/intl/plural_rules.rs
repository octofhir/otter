//! `Intl.PluralRules` — locale-aware plural-category selection.
//!
//! Foundation surface ships English cardinal / ordinal rules:
//! - cardinal: `one` for `n === 1`, `other` otherwise.
//! - ordinal: `one`/`two`/`few` for the canonical English suffixes
//!   (1st, 2nd, 3rd), `other` otherwise.
//!
//! Other locales fall back to the same rules — full ICU CLDR plural
//! tables are filed alongside the wider Intl follow-up. The surface
//! returns spec-shape values so user code that switches on the
//! result keeps working under every locale; the foundation just
//! biases toward English categories.
//!
//! # See also
//! - <https://tc39.es/ecma402/#pluralrules-objects>

use crate::intl::helpers::{coerce_locale, options_object, read_string_option, read_u8_option};
use crate::intl::payload::{IntlPayload, PluralRulesPayload};
use crate::string::JsString;
use crate::{NativeCtx, NativeError, Value};

/// §15.2.1 — resolve constructor options.
pub fn resolve(locale: &Value, options: &Value, gc_heap: &otter_gc::GcHeap) -> PluralRulesPayload {
    let opts = options_object(Some(options));
    let opts_ref = opts.as_ref();
    PluralRulesPayload {
        locale: coerce_locale(Some(locale), gc_heap),
        kind: read_string_option(opts_ref, "type", "cardinal", gc_heap),
        minimum_integer_digits: read_u8_option(opts_ref, "minimumIntegerDigits", 1, 1, 21, gc_heap),
        minimum_fraction_digits: read_u8_option(
            opts_ref,
            "minimumFractionDigits",
            0,
            0,
            20,
            gc_heap,
        ),
        maximum_fraction_digits: read_u8_option(
            opts_ref,
            "maximumFractionDigits",
            3,
            0,
            20,
            gc_heap,
        ),
    }
}

fn require_payload(
    ctx: &NativeCtx<'_>,
    name: &'static str,
) -> Result<PluralRulesPayload, NativeError> {
    let bad = || NativeError::TypeError {
        name,
        reason: "intrinsic called on a non-Intl.PluralRules receiver".to_string(),
    };
    let intl = ctx.this_value().as_intl(ctx.heap()).ok_or_else(bad)?;
    match intl.payload_clone(ctx.heap()) {
        IntlPayload::PluralRules(p) => Ok(p),
        _ => Err(bad()),
    }
}

/// §16.3.3 `Intl.PluralRules.prototype.select(value)`.
pub(crate) fn plural_rules_select(
    ctx: &mut NativeCtx<'_>,
    args: &[Value],
) -> Result<Value, NativeError> {
    let payload = require_payload(ctx, "select")?;
    let first = args.first();
    let n = if let Some(n) = first.and_then(|v| v.as_number()) {
        n.as_f64()
    } else if let Some(b) = first.and_then(|v| v.as_boolean()) {
        if b { 1.0 } else { 0.0 }
    } else if first.is_some_and(|v| v.is_null()) {
        0.0
    } else {
        f64::NAN
    };
    Ok(Value::string(JsString::from_str(
        plural_category_en(n, &payload.kind),
        ctx.heap_mut(),
    )?))
}

/// §16.3.4 `Intl.PluralRules.prototype.resolvedOptions()`.
pub(crate) fn plural_rules_resolved_options(
    ctx: &mut NativeCtx<'_>,
    _args: &[Value],
) -> Result<Value, NativeError> {
    let payload = require_payload(ctx, "resolvedOptions")?;
    let locale = Value::string(JsString::from_str(&payload.locale, ctx.heap_mut())?);
    let kind = Value::string(JsString::from_str(&payload.kind, ctx.heap_mut())?);
    let mid = payload.minimum_integer_digits as i32;
    let mfd = payload.minimum_fraction_digits as i32;
    let xfd = payload.maximum_fraction_digits as i32;
    let obj = ctx.alloc_object_with_roots(&[&locale, &kind], &[])?;
    let heap = ctx.heap_mut();
    crate::object::set(obj, heap, "locale", locale);
    crate::object::set(obj, heap, "type", kind);
    crate::object::set(obj, heap, "minimumIntegerDigits", Value::number_i32(mid));
    crate::object::set(obj, heap, "minimumFractionDigits", Value::number_i32(mfd));
    crate::object::set(obj, heap, "maximumFractionDigits", Value::number_i32(xfd));
    Ok(Value::object(obj))
}

/// English plural-category fallback. `kind` is `"cardinal"` or
/// `"ordinal"`. Negative inputs use absolute value.
fn plural_category_en(n: f64, kind: &str) -> &'static str {
    if n.is_nan() {
        return "other";
    }
    let abs = n.abs();
    if kind == "ordinal" {
        let i = abs as i64;
        let mod10 = i % 10;
        let mod100 = i % 100;
        if mod10 == 1 && mod100 != 11 {
            return "one";
        }
        if mod10 == 2 && mod100 != 12 {
            return "two";
        }
        if mod10 == 3 && mod100 != 13 {
            return "few";
        }
        return "other";
    }
    if (abs - 1.0).abs() < f64::EPSILON {
        "one"
    } else {
        "other"
    }
}
