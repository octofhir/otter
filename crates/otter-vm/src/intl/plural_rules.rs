//! `Intl.PluralRules` — locale-aware plural-category selection.
//!
//! Category selection and the `pluralCategories` listing run on ICU4X's
//! CLDR plural rules (`icu_plurals`); the option ladder implements the
//! v3 shape (notation, significant digits, and the rounding tail read
//! through `SetNumberFormatDigitOptions` in observation order).
//!
//! # Contents
//! - [`resolve_ctx`] — §16.1.2 InitializePluralRules.
//! - `select` / `selectRange` / `resolvedOptions` prototype bodies.
//!
//! # Invariants
//! - Option getters fire in the order pinned by
//!   `constructor-option-read-order`.
//! - `pluralCategories` is a fresh array per `resolvedOptions` call, in
//!   CLDR category order (zero, one, two, few, many, other).
//! - `selectRange` follows CLDR range rules through
//!   `PluralRules::category_for_range`.
//!
//! # See also
//! - <https://tc39.es/ecma402/#pluralrules-objects>

use crate::intl::helpers::{
    DEFAULT_LOCALE, get_number_option, get_string_option, require_options_object,
};
use crate::intl::payload::{IntlPayload, PluralRulesPayload};
use crate::string::JsString;
use crate::{NativeCtx, NativeError, Value};
use icu_plurals::{PluralCategory, PluralRuleType, PluralRules, PluralRulesOptions};

const CLASS: &str = "PluralRules";

/// §16.1.2 InitializePluralRules — fires `localeMatcher` / `type` /
/// `notation` / `compactDisplay` and the digit-option getters in spec
/// order with coercion + RangeError validation; canonicalizes the
/// locale.
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
    let range = |m: String| NativeError::RangeError {
        name: CLASS,
        reason: m,
    };
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
    let notation = get_string_option(
        ctx,
        options,
        "notation",
        CLASS,
        &["standard", "scientific", "engineering", "compact"],
        Some("standard"),
    )?
    .unwrap_or_else(|| "standard".to_string());
    let compact_display = get_string_option(
        ctx,
        options,
        "compactDisplay",
        CLASS,
        &["short", "long"],
        Some("short"),
    )?
    .unwrap_or_else(|| "short".to_string());
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
    let mnfd = get_number_option(
        ctx,
        options,
        "minimumFractionDigits",
        CLASS,
        0.0,
        100.0,
        None,
    )?
    .map(|n| n as u8);
    let mxfd = get_number_option(
        ctx,
        options,
        "maximumFractionDigits",
        CLASS,
        0.0,
        100.0,
        None,
    )?
    .map(|n| n as u8);
    let mnsd = get_number_option(
        ctx,
        options,
        "minimumSignificantDigits",
        CLASS,
        1.0,
        21.0,
        None,
    )?
    .map(|n| n as u8);
    let mxsd = get_number_option(
        ctx,
        options,
        "maximumSignificantDigits",
        CLASS,
        1.0,
        21.0,
        None,
    )?
    .map(|n| n as u8);
    let (minimum_significant_digits, maximum_significant_digits) = match (mnsd, mxsd) {
        (None, None) => (None, None),
        (mn, mx) => {
            let mn = mn.unwrap_or(1);
            let mx = mx.unwrap_or(21);
            if mx < mn {
                return Err(range(
                    "maximumSignificantDigits is less than minimumSignificantDigits".to_string(),
                ));
            }
            (Some(mn), Some(mx))
        }
    };
    let (minimum_fraction_digits, maximum_fraction_digits) = match (mnfd, mxfd) {
        (None, None) => (0, 3),
        (Some(mn), None) => (mn, mn.max(3)),
        (None, Some(mx)) => (0u8, mx),
        (Some(mn), Some(mx)) => {
            if mx < mn {
                return Err(range(
                    "maximumFractionDigits is less than minimumFractionDigits".to_string(),
                ));
            }
            (mn, mx)
        }
    };
    let rounding_increment = get_number_option(
        ctx,
        options,
        "roundingIncrement",
        CLASS,
        1.0,
        5000.0,
        Some(1.0),
    )?
    .unwrap_or(1.0) as u16;
    const INCREMENTS: &[u16] = &[
        1, 2, 5, 10, 20, 25, 50, 100, 200, 250, 500, 1000, 2000, 2500, 5000,
    ];
    if !INCREMENTS.contains(&rounding_increment) {
        return Err(range(format!(
            "invalid roundingIncrement {rounding_increment}"
        )));
    }
    let rounding_mode = get_string_option(
        ctx,
        options,
        "roundingMode",
        CLASS,
        &[
            "ceil",
            "floor",
            "expand",
            "trunc",
            "halfCeil",
            "halfFloor",
            "halfExpand",
            "halfTrunc",
            "halfEven",
        ],
        Some("halfExpand"),
    )?
    .unwrap_or_else(|| "halfExpand".to_string());
    let rounding_priority = get_string_option(
        ctx,
        options,
        "roundingPriority",
        CLASS,
        &["auto", "morePrecision", "lessPrecision"],
        Some("auto"),
    )?
    .unwrap_or_else(|| "auto".to_string());
    let trailing_zero_display = get_string_option(
        ctx,
        options,
        "trailingZeroDisplay",
        CLASS,
        &["auto", "stripIfInteger"],
        Some("auto"),
    )?
    .unwrap_or_else(|| "auto".to_string());
    Ok(PluralRulesPayload {
        locale,
        kind,
        notation,
        compact_display,
        minimum_integer_digits,
        minimum_fraction_digits,
        maximum_fraction_digits,
        minimum_significant_digits,
        maximum_significant_digits,
        rounding_increment,
        rounding_mode,
        rounding_priority,
        trailing_zero_display,
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

/// CLDR cardinal rules for locales absent from `icu_plurals_data`'s
/// trimmed locale set (currently Manx). `None` means the ICU data
/// applies.
fn manual_cardinal_category(locale: &str, n: f64) -> Option<&'static str> {
    if locale.split('-').next() != Some("gv") {
        return None;
    }
    let abs = n.abs();
    let i = abs.trunc() as i64;
    let has_fraction = abs.fract() != 0.0;
    Some(if has_fraction {
        "many"
    } else if i % 10 == 1 {
        "one"
    } else if i % 10 == 2 {
        "two"
    } else if matches!(i % 100, 0 | 20 | 40 | 60 | 80) {
        "few"
    } else {
        "other"
    })
}

/// The category list for the manual-rule locales above.
fn manual_categories(locale: &str) -> Option<Vec<&'static str>> {
    if locale.split('-').next() == Some("gv") {
        return Some(vec!["one", "two", "few", "many", "other"]);
    }
    None
}

/// The ICU rule set for this payload's locale and type.
fn icu_rules(payload: &PluralRulesPayload) -> Option<PluralRules> {
    let locale: icu_locale::Locale = payload
        .locale
        .parse()
        .or_else(|_| DEFAULT_LOCALE.parse())
        .ok()?;
    let rule_type = if payload.kind == "ordinal" {
        PluralRuleType::Ordinal
    } else {
        PluralRuleType::Cardinal
    };
    PluralRules::try_new((&locale).into(), PluralRulesOptions::from(rule_type)).ok()
}

const fn category_name(category: PluralCategory) -> &'static str {
    match category {
        PluralCategory::Zero => "zero",
        PluralCategory::One => "one",
        PluralCategory::Two => "two",
        PluralCategory::Few => "few",
        PluralCategory::Many => "many",
        PluralCategory::Other => "other",
    }
}

/// Plural operands of `n` after the payload's digit formatting — the
/// visible fraction digits participate in selection (`1` is "one" but
/// `1.0` with two forced fraction digits can be "other").
fn operands_category(payload: &PluralRulesPayload, n: f64) -> &'static str {
    if n.is_nan() || n.is_infinite() {
        return "other";
    }
    if payload.kind == "cardinal"
        && let Some(category) = manual_cardinal_category(&payload.locale, n)
    {
        return category;
    }
    let Some(rules) = icu_rules(payload) else {
        return "other";
    };
    // Compact notation categorizes the compact form: mantissa plus the
    // suppressed power-of-ten exponent (CLDR's `c`/`e` operand).
    if payload.notation == "compact" {
        let abs = n.abs();
        let exponent = if abs >= 1.0 {
            (abs.log10().floor() as i32 / 3 * 3).clamp(0, 15)
        } else {
            0
        };
        if exponent >= 3 {
            let mantissa = abs / 10f64.powi(exponent);
            // Compact default rounding: at most one fraction digit on a
            // sub-100 mantissa.
            let rounded = if mantissa < 100.0 {
                (mantissa * 10.0).round() / 10.0
            } else {
                mantissa.round()
            };
            let significand = fixed_decimal::Decimal::try_from_f64(
                rounded,
                fixed_decimal::FloatPrecision::RoundTrip,
            )
            .unwrap_or_else(|_| fixed_decimal::Decimal::from(0u32));
            let compact = fixed_decimal::CompactDecimal::from_significand_and_exponent(
                significand,
                exponent as u8,
            );
            return category_name(rules.category_for(&compact));
        }
    }
    let mut decimal =
        fixed_decimal::Decimal::try_from_f64(n.abs(), fixed_decimal::FloatPrecision::RoundTrip)
            .unwrap_or_else(|_| fixed_decimal::Decimal::from(0u32));
    if payload.minimum_significant_digits.is_none() {
        decimal.round(-i16::from(payload.maximum_fraction_digits));
        decimal.pad_end(-(i16::from(payload.minimum_fraction_digits)));
    }
    category_name(rules.category_for(&decimal))
}

/// §16.3.2 `Intl.PluralRules.prototype.select(value)`.
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
    } else if let Some(value) = first {
        let value = *value;
        let exec = ctx
            .execution_context()
            .cloned()
            .ok_or_else(|| NativeError::TypeError {
                name: "select",
                reason: "missing execution context".to_string(),
            })?;
        let number = ctx.with_turn_parts(|interp, stack| {
            crate::coerce::to_number_or_throw(interp, stack, &exec, &value)
        });
        number
            .map_err(|error| {
                crate::native_function::vm_to_native_error(ctx.interp_mut(), error, "select")
            })?
            .as_f64()
    } else {
        f64::NAN
    };
    Ok(Value::string(JsString::from_str(
        operands_category(&payload, n),
        ctx.heap_mut(),
    )?))
}

/// §16.3.3 `Intl.PluralRules.prototype.selectRange(start, end)` —
/// `start`/`end` are required (a `TypeError` otherwise), coerced through
/// `ToNumber` (a Symbol throws), and a `NaN` endpoint is a `RangeError`.
pub(crate) fn plural_rules_select_range(
    ctx: &mut NativeCtx<'_>,
    args: &[Value],
) -> Result<Value, NativeError> {
    let payload = require_payload(ctx, "selectRange")?;
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
    let (x, y) = ctx.scope(|mut scope| {
        let start = scope.value(start);
        let end = scope.value(end);
        let start_value = scope.raw(start);
        let start_number = scope.with_turn_parts(|interp, stack| {
            crate::coerce::to_number_or_throw(interp, stack, &exec, &start_value)
        });
        let x = start_number
            .map_err(|error| {
                crate::native_function::vm_to_native_error(
                    scope.context().interp_mut(),
                    error,
                    "selectRange",
                )
            })?
            .as_f64();
        let end_value = scope.raw(end);
        let end_number = scope.with_turn_parts(|interp, stack| {
            crate::coerce::to_number_or_throw(interp, stack, &exec, &end_value)
        });
        let y = end_number
            .map_err(|error| {
                crate::native_function::vm_to_native_error(
                    scope.context().interp_mut(),
                    error,
                    "selectRange",
                )
            })?
            .as_f64();
        Ok::<_, NativeError>((x, y))
    })?;
    if x.is_nan() || y.is_nan() {
        return Err(NativeError::RangeError {
            name: "selectRange",
            reason: "selectRange arguments must not be NaN".to_string(),
        });
    }
    let category = icu_ranges(&payload)
        .map(|ranges| {
            let sd = fixed_decimal::Decimal::try_from_f64(
                x.abs(),
                fixed_decimal::FloatPrecision::RoundTrip,
            )
            .unwrap_or_else(|_| fixed_decimal::Decimal::from(0u32));
            let ed = fixed_decimal::Decimal::try_from_f64(
                y.abs(),
                fixed_decimal::FloatPrecision::RoundTrip,
            )
            .unwrap_or_else(|_| fixed_decimal::Decimal::from(0u32));
            category_name(ranges.category_for_range(&sd, &ed))
        })
        .unwrap_or("other");
    Ok(Value::string(JsString::from_str(category, ctx.heap_mut())?))
}

/// The CLDR plural-range rule set for this payload's locale.
fn icu_ranges(
    payload: &PluralRulesPayload,
) -> Option<icu_plurals::PluralRulesWithRanges<PluralRules>> {
    let locale: icu_locale::Locale = payload
        .locale
        .parse()
        .or_else(|_| DEFAULT_LOCALE.parse())
        .ok()?;
    let rule_type = if payload.kind == "ordinal" {
        PluralRuleType::Ordinal
    } else {
        PluralRuleType::Cardinal
    };
    icu_plurals::PluralRulesWithRanges::try_new(
        (&locale).into(),
        PluralRulesOptions::from(rule_type),
    )
    .ok()
}

/// §16.3.4 `Intl.PluralRules.prototype.resolvedOptions()`.
pub(crate) fn plural_rules_resolved_options(
    ctx: &mut NativeCtx<'_>,
    _args: &[Value],
) -> Result<Value, NativeError> {
    let payload = require_payload(ctx, "resolvedOptions")?;
    let categories: Vec<&'static str> = if payload.kind == "cardinal"
        && let Some(manual) = manual_categories(&payload.locale)
    {
        manual
    } else {
        icu_rules(&payload)
            .map(|rules| rules.categories().map(category_name).collect())
            .unwrap_or_else(|| vec!["other"])
    };
    ctx.scope(|mut scope| {
        let result = scope.object()?;
        let locale = scope.string(&payload.locale)?;
        scope.set(result, "locale", locale)?;
        let kind = scope.string(&payload.kind)?;
        scope.set(result, "type", kind)?;
        let notation = scope.string(&payload.notation)?;
        scope.set(result, "notation", notation)?;
        if payload.notation == "compact" {
            let compact_display = scope.string(&payload.compact_display)?;
            scope.set(result, "compactDisplay", compact_display)?;
        }
        let mid = scope.number(f64::from(payload.minimum_integer_digits));
        scope.set(result, "minimumIntegerDigits", mid)?;
        if let (Some(mn), Some(mx)) = (
            payload.minimum_significant_digits,
            payload.maximum_significant_digits,
        ) {
            let mn = scope.number(f64::from(mn));
            scope.set(result, "minimumSignificantDigits", mn)?;
            let mx = scope.number(f64::from(mx));
            scope.set(result, "maximumSignificantDigits", mx)?;
        } else {
            let mn = scope.number(f64::from(payload.minimum_fraction_digits));
            scope.set(result, "minimumFractionDigits", mn)?;
            let mx = scope.number(f64::from(payload.maximum_fraction_digits));
            scope.set(result, "maximumFractionDigits", mx)?;
        }
        let array = scope.array(categories.len())?;
        for (index, name) in categories.iter().enumerate() {
            let name = scope.string(name)?;
            scope.set_index(array, index, name)?;
        }
        scope.set(result, "pluralCategories", array)?;
        let rounding_increment = scope.number(f64::from(payload.rounding_increment));
        scope.set(result, "roundingIncrement", rounding_increment)?;
        let rounding_mode = scope.string(&payload.rounding_mode)?;
        scope.set(result, "roundingMode", rounding_mode)?;
        let rounding_priority = scope.string(&payload.rounding_priority)?;
        scope.set(result, "roundingPriority", rounding_priority)?;
        let trailing_zero_display = scope.string(&payload.trailing_zero_display)?;
        scope.set(result, "trailingZeroDisplay", trailing_zero_display)?;
        Ok(scope.finish(result))
    })
}
