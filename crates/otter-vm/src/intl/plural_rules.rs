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

use crate::intl::helpers::{
    DEFAULT_LOCALE, get_number_option, get_string_option, require_options_object,
};
use crate::intl::payload::{IntlPayload, PluralRulesPayload};
use crate::string::JsString;
use crate::{NativeCtx, NativeError, Value};

const CLASS: &str = "PluralRules";

/// §16.1.1 InitializePluralRules — fires `localeMatcher` / `type` and the
/// digit-option getters in spec order with coercion + RangeError
/// validation; canonicalizes the locale.
pub fn resolve_ctx(
    ctx: &mut NativeCtx<'_>,
    locales: Value,
    options: Value,
) -> Result<PluralRulesPayload, NativeError> {
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
    let kind = get_string_option(
        ctx,
        options,
        "type",
        CLASS,
        &["cardinal", "ordinal"],
        Some("cardinal"),
    )?
    .unwrap_or_else(|| "cardinal".to_string());
    let minimum_integer_digits = get_number_option(
        ctx,
        options,
        "minimumIntegerDigits",
        CLASS,
        1.0,
        21.0,
        Some(1.0),
    )?
    .unwrap_or(1.0) as u8;
    let minimum_fraction_digits = get_number_option(
        ctx,
        options,
        "minimumFractionDigits",
        CLASS,
        0.0,
        20.0,
        Some(0.0),
    )?
    .unwrap_or(0.0) as u8;
    let default_max = minimum_fraction_digits.max(3);
    let maximum_fraction_digits = get_number_option(
        ctx,
        options,
        "maximumFractionDigits",
        CLASS,
        minimum_fraction_digits as f64,
        20.0,
        Some(default_max as f64),
    )?
    .unwrap_or(default_max as f64) as u8;
    Ok(PluralRulesPayload {
        locale,
        kind,
        minimum_integer_digits,
        minimum_fraction_digits,
        maximum_fraction_digits,
    })
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

/// §1.1.6 `Intl.PluralRules.prototype.selectRange(start, end)` —
/// `start`/`end` are required (a `TypeError` otherwise), coerced through
/// `ToNumber` (a Symbol throws), and a `NaN` endpoint is a `RangeError`.
/// The English plural-range rules collapse every category pair to
/// `"other"`, which the foundation locale returns directly.
pub(crate) fn plural_rules_select_range(
    ctx: &mut NativeCtx<'_>,
    args: &[Value],
) -> Result<Value, NativeError> {
    let _payload = require_payload(ctx, "selectRange")?;
    let start = args.first().copied().unwrap_or_else(Value::undefined);
    let end = args.get(1).copied().unwrap_or_else(Value::undefined);
    if start.is_undefined() || end.is_undefined() {
        return Err(NativeError::TypeError {
            name: "selectRange",
            reason: "start and end are required".to_string(),
        });
    }
    let exec = ctx
        .execution_context()
        .cloned()
        .ok_or_else(|| NativeError::TypeError {
            name: "selectRange",
            reason: "missing execution context".to_string(),
        })?;
    let to_num = |ctx: &mut NativeCtx<'_>, v: &Value| -> Result<f64, NativeError> {
        crate::coerce::to_number_or_throw(ctx.cx.interp, &exec, v)
            .map(|n| n.as_f64())
            .map_err(|e| {
                crate::native_function::vm_to_native_error(ctx.cx.interp, e, "selectRange")
            })
    };
    let x = to_num(ctx, &start)?;
    let y = to_num(ctx, &end)?;
    if x.is_nan() || y.is_nan() {
        return Err(NativeError::RangeError {
            name: "selectRange",
            reason: "selectRange arguments must not be NaN".to_string(),
        });
    }
    Ok(Value::string(JsString::from_str("other", ctx.heap_mut())?))
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
    let mut obj = ctx.alloc_object_with_roots(&[&locale, &kind], &[])?;
    let heap = ctx.heap_mut();
    crate::object::set(&mut obj, heap, "locale", locale);
    crate::object::set(&mut obj, heap, "type", kind);
    crate::object::set(
        &mut obj,
        heap,
        "minimumIntegerDigits",
        Value::number_i32(mid),
    );
    crate::object::set(
        &mut obj,
        heap,
        "minimumFractionDigits",
        Value::number_i32(mfd),
    );
    crate::object::set(
        &mut obj,
        heap,
        "maximumFractionDigits",
        Value::number_i32(xfd),
    );
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
