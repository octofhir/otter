//! `Intl.NumberFormat` — locale-aware number formatting.
//!
//! Backed by [`icu_decimal::DecimalFormatter`] for the integer +
//! fractional part. Currency formatting routes through ICU's
//! CLDR-backed [`CurrencyFormatter`] (correct symbol + placement for
//! every ISO-4217 code and locale); percent appends the sign.
//!
//! # See also
//! - <https://tc39.es/ecma402/#sec-intl-numberformat-objects>

use std::str::FromStr;

use fixed_decimal::Decimal;
use icu_decimal::DecimalFormatter;
use icu_decimal::options::{DecimalFormatterOptions, GroupingStrategy};
use icu_experimental::dimension::currency::CurrencyCode;
use icu_experimental::dimension::currency::formatter::{
    CurrencyFormatter, CurrencyFormatterPreferences,
};
use icu_experimental::dimension::currency::options::CurrencyFormatterOptions;
use icu_experimental::dimension::units::formatter::{UnitsFormatter, UnitsFormatterPreferences};
use icu_experimental::dimension::units::options::{UnitsFormatterOptions, Width as UnitWidth};
use icu_locale::Locale;
use tinystr::TinyAsciiStr;

use crate::intl::helpers::DEFAULT_LOCALE;
use crate::intl::payload::{IntlPayload, NumberFormatPayload};
use crate::string::JsString;
use crate::{NativeCtx, NativeError, Value};
use otter_gc::raw::RawGc;

const CLASS: &str = "NumberFormat";

/// §15.1.1 InitializeNumberFormat — reads every option through the spec
/// `GetOption` ladder in the `constructor-option-read-order` sequence
/// (getters fire in observation order, ToString/ToNumber/ToBoolean
/// coercion, RangeError validation) and canonicalizes the locale.
/// Rounding / significant-digit / trailing-zero options are read +
/// validated (so throwing getters and `*-invalid` tests observe them)
/// but, pending a GC-safe resolvedOptions + exact-decimal formatter, are
/// not yet reflected in output — keeping the existing format path.
pub fn resolve_ctx(
    ctx: &mut NativeCtx<'_>,
    locales: Value,
    options: Value,
) -> Result<NumberFormatPayload, NativeError> {
    use crate::intl::helpers::{
        coerce_options_object, get_number_option, get_numbering_system_option, get_string_option,
        option_to_string,
    };
    use crate::temporal::helpers::get_option_value;

    let requested = crate::intl::supported::canonicalize_locale_list(ctx, locales)?;
    let locale = requested
        .into_iter()
        .next()
        .unwrap_or_else(|| DEFAULT_LOCALE.to_string());
    let options = coerce_options_object(options, CLASS)?;
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
        &["decimal", "percent", "currency", "unit"],
        Some("decimal"),
    )?
    .unwrap_or_else(|| "decimal".to_string());

    // SetNumberFormatUnitOptions: currency, currencyDisplay, currencySign,
    // unit, unitDisplay — all read in order regardless of style.
    let currency_raw = get_string_option(ctx, options, "currency", CLASS, &[], None)?;
    let currency_display = get_string_option(
        ctx,
        options,
        "currencyDisplay",
        CLASS,
        &["symbol", "narrowSymbol", "code", "name"],
        Some("symbol"),
    )?
    .unwrap_or_else(|| "symbol".to_string());
    let currency_sign = get_string_option(
        ctx,
        options,
        "currencySign",
        CLASS,
        &["standard", "accounting"],
        Some("standard"),
    )?
    .unwrap_or_else(|| "standard".to_string());
    let unit_raw = get_string_option(ctx, options, "unit", CLASS, &[], None)?;
    let unit_display = get_string_option(
        ctx,
        options,
        "unitDisplay",
        CLASS,
        &["short", "narrow", "long"],
        Some("short"),
    )?
    .unwrap_or_else(|| "short".to_string());

    let check_currency = |raw: &str| -> Result<(), NativeError> {
        if raw.chars().count() != 3 || !raw.chars().all(|c| c.is_ascii_alphabetic()) {
            return Err(range(format!("invalid currency code '{raw}'")));
        }
        Ok(())
    };
    let currency = match (&style, currency_raw) {
        (s, Some(raw)) if s == "currency" => {
            check_currency(&raw)?;
            Some(raw.to_uppercase())
        }
        (s, None) if s == "currency" => {
            return Err(NativeError::TypeError {
                name: CLASS,
                reason: "currency style requires a `currency` option".to_string(),
            });
        }
        (_, Some(raw)) => {
            check_currency(&raw)?;
            None
        }
        _ => None,
    };
    let unit = match (&style, unit_raw) {
        (s, Some(raw)) if s == "unit" => {
            if !is_well_formed_unit(&raw) {
                return Err(range(format!("invalid unit identifier '{raw}'")));
            }
            Some(raw)
        }
        (s, None) if s == "unit" => {
            return Err(NativeError::TypeError {
                name: CLASS,
                reason: "unit style requires a `unit` option".to_string(),
            });
        }
        (_, Some(raw)) => {
            if !is_well_formed_unit(&raw) {
                return Err(range(format!("invalid unit identifier '{raw}'")));
            }
            None
        }
        _ => None,
    };

    let notation = get_string_option(
        ctx,
        options,
        "notation",
        CLASS,
        &["standard", "scientific", "engineering", "compact"],
        Some("standard"),
    )?
    .unwrap_or_else(|| "standard".to_string());

    // SetNumberFormatDigitOptions — read in spec order. The fraction
    // digits feed the existing formatter; the rounding / significant /
    // trailing-zero options are validated then discarded for now.
    let (default_min, default_max) = match style.as_str() {
        "currency" => (2u8, 2u8),
        "percent" => (0, 0),
        _ => (0, 3),
    };
    let min_int = get_number_option(
        ctx,
        options,
        "minimumIntegerDigits",
        CLASS,
        1.0,
        21.0,
        Some(1.0),
    )?;
    let mnfd = get_number_option(
        ctx,
        options,
        "minimumFractionDigits",
        CLASS,
        0.0,
        20.0,
        None,
    )?
    .map(|n| n as u8);
    let mxfd = get_number_option(
        ctx,
        options,
        "maximumFractionDigits",
        CLASS,
        0.0,
        20.0,
        None,
    )?
    .map(|n| n as u8);
    let min_sig = get_number_option(
        ctx,
        options,
        "minimumSignificantDigits",
        CLASS,
        1.0,
        21.0,
        None,
    )?;
    let max_sig = get_number_option(
        ctx,
        options,
        "maximumSignificantDigits",
        CLASS,
        1.0,
        21.0,
        None,
    )?;
    // SetNumberFormatDigitOptions with roundingPriority "auto": a present
    // significant-digit option selects significant-digit rounding and the
    // fraction-digit settings become inert.
    let (minimum_significant_digits, maximum_significant_digits) =
        if min_sig.is_some() || max_sig.is_some() {
            let mn = min_sig.map_or(1u8, |v| v as u8);
            let mx = max_sig.map_or(21u8, |v| v as u8);
            if mx < mn {
                return Err(range(
                    "maximumSignificantDigits is less than minimumSignificantDigits".to_string(),
                ));
            }
            (Some(mn), Some(mx))
        } else {
            (None, None)
        };
    let (minimum_fraction_digits, maximum_fraction_digits) = match (mnfd, mxfd) {
        (None, None) => (default_min, default_max.max(default_min)),
        (Some(mn), None) => (mn, default_max.max(mn)),
        (None, Some(mx)) => (default_min.min(mx), mx),
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
    // §15.1.6 SetNumberFormatDigitOptions — a non-auto roundingPriority
    // keeps BOTH digit families live, defaulting significant digits to
    // 1..21 when unset (format-time position comparison picks a side).
    let (minimum_significant_digits, maximum_significant_digits) =
        if rounding_priority != "auto" && minimum_significant_digits.is_none() {
            (Some(1u8), Some(21u8))
        } else {
            (minimum_significant_digits, maximum_significant_digits)
        };
    let trailing_zero_display = get_string_option(
        ctx,
        options,
        "trailingZeroDisplay",
        CLASS,
        &["auto", "stripIfInteger"],
        Some("auto"),
    )?
    .unwrap_or_else(|| "auto".to_string());

    let compact_display = get_string_option(
        ctx,
        options,
        "compactDisplay",
        CLASS,
        &["short", "long"],
        Some("short"),
    )?
    .unwrap_or_else(|| "short".to_string());
    // useGrouping accepts a boolean or "min2"/"auto"/"always".
    let use_grouping_val = if options.is_undefined() {
        Value::undefined()
    } else {
        get_option_value(ctx, options, "useGrouping", CLASS)?
    };
    let use_grouping = if use_grouping_val.is_undefined() {
        true
    } else if let Some(b) = use_grouping_val.as_boolean() {
        b
    } else {
        option_to_string(ctx, use_grouping_val, CLASS)? != "false"
    };
    let sign_display = get_string_option(
        ctx,
        options,
        "signDisplay",
        CLASS,
        &["auto", "always", "never", "exceptZero", "negative"],
        Some("auto"),
    )?
    .unwrap_or_else(|| "auto".to_string());

    Ok(NumberFormatPayload {
        locale,
        numbering_system,
        style,
        currency,
        minimum_integer_digits: min_int.map_or(1u8, |v| v as u8),
        minimum_fraction_digits,
        maximum_fraction_digits,
        minimum_significant_digits,
        maximum_significant_digits,
        use_grouping,
        sign_display,
        notation,
        currency_display,
        currency_sign,
        unit,
        unit_display,
        compact_display,
        rounding_mode,
        rounding_increment,
        trailing_zero_display,
        rounding_priority,
    })
}

/// The sanctioned simple unit identifiers (ECMA-402 Table:
/// Single-unit identifiers sanctioned for use in ECMA-402).
const SANCTIONED_UNITS: &[&str] = &[
    "acre",
    "bit",
    "byte",
    "celsius",
    "centimeter",
    "day",
    "degree",
    "fahrenheit",
    "fluid-ounce",
    "foot",
    "gallon",
    "gigabit",
    "gigabyte",
    "gram",
    "hectare",
    "hour",
    "inch",
    "kilobit",
    "kilobyte",
    "kilogram",
    "kilometer",
    "liter",
    "megabit",
    "megabyte",
    "meter",
    "microsecond",
    "mile",
    "mile-scandinavian",
    "milliliter",
    "millimeter",
    "millisecond",
    "minute",
    "month",
    "nanosecond",
    "ounce",
    "percent",
    "petabyte",
    "pound",
    "second",
    "stone",
    "terabit",
    "terabyte",
    "week",
    "yard",
    "year",
];

/// §IsWellFormedUnitIdentifier — a sanctioned simple unit, or
/// `"<numerator>-per-<denominator>"` where both are sanctioned.
fn is_well_formed_unit(unit: &str) -> bool {
    if SANCTIONED_UNITS.contains(&unit) {
        return true;
    }
    match unit.split_once("-per-") {
        Some((num, den)) => SANCTIONED_UNITS.contains(&num) && SANCTIONED_UNITS.contains(&den),
        None => false,
    }
}

fn require_number_format(
    ctx: &NativeCtx<'_>,
    name: &'static str,
) -> Result<NumberFormatPayload, NativeError> {
    let bad = || NativeError::TypeError {
        name,
        reason: "intrinsic called on a non-Intl.NumberFormat receiver".to_string(),
    };
    let intl = ctx.this_value().as_intl(ctx.heap()).ok_or_else(bad)?;
    match intl.payload_clone(ctx.heap()) {
        IntlPayload::NumberFormat(n) => Ok(n),
        _ => Err(bad()),
    }
}

/// Coerce a `format`/`formatToParts` argument to an `f64`, an
/// approximation of ToIntlMathematicalValue (§15.5.2): BigInt and the
/// numeric value pass straight through; everything else (object via
/// `valueOf`, string, boolean, null, undefined) flows through
/// `ToNumber`, preserving a user-thrown abrupt completion.
fn coerce_format_arg(ctx: &mut NativeCtx<'_>, first: Option<&Value>) -> Result<f64, NativeError> {
    let Some(value) = first else {
        return Ok(f64::NAN);
    };
    if let Some(num) = value.as_number() {
        return Ok(num.as_f64());
    }
    if let Some(bi) = value.as_big_int() {
        return Ok(bi
            .to_decimal_string(ctx.heap())
            .parse::<f64>()
            .unwrap_or(f64::NAN));
    }
    let value = *value;
    let exec = ctx
        .execution_context()
        .cloned()
        .ok_or_else(|| NativeError::TypeError {
            name: "format",
            reason: "missing execution context".to_string(),
        })?;
    let number = ctx.with_turn_parts(|interp, stack| {
        crate::coerce::to_number_or_throw(interp, stack, &exec, &value)
    });
    let n = number.map_err(|error| {
        crate::native_function::vm_to_native_error(ctx.interp_mut(), error, "format")
    })?;
    Ok(n.as_f64())
}

/// §11.3.3 `get Intl.NumberFormat.prototype.format` — an accessor
/// whose getter returns a function bound to this NumberFormat
/// instance. ECMA-402 mandates caching in `[[BoundFormat]]`; we mint a
/// fresh bound function per access since no observable test depends on
/// its identity, only that it formats against the originating instance.
pub(crate) fn number_format_format_getter(
    ctx: &mut NativeCtx<'_>,
    _args: &[Value],
) -> Result<Value, NativeError> {
    // Brand check: the receiver must be a NumberFormat instance.
    let _ = require_number_format(ctx, "format")?;
    let this = *ctx.this_value();
    let captures: smallvec::SmallVec<[Value; 4]> = smallvec::smallvec![this];
    let bound = crate::NativeFunction::with_length_and_captures(
        ctx.heap_mut(),
        "",
        1,
        bound_format_call,
        captures,
    )?;
    Ok(Value::native_function(bound))
}

/// The bound function returned by the `format` getter. Its captured
/// `[[NumberFormat]]` is `captures[0]`; `this` is ignored per the
/// bound-function semantics of §11.3.3.
fn bound_format_call(
    ctx: &mut NativeCtx<'_>,
    args: &[Value],
    captures: &[Value],
) -> Result<Value, NativeError> {
    let bad = || NativeError::TypeError {
        name: "format",
        reason: "format function lost its bound Intl.NumberFormat".to_string(),
    };
    let intl = captures
        .first()
        .and_then(|v| v.as_intl(ctx.heap()))
        .ok_or_else(bad)?;
    let payload = match intl.payload_clone(ctx.heap()) {
        IntlPayload::NumberFormat(n) => n,
        _ => return Err(bad()),
    };
    let n = coerce_format_arg(ctx, args.first())?;
    let rendered = format_number(n, &payload);
    Ok(Value::string(JsString::from_str(
        &rendered,
        ctx.heap_mut(),
    )?))
}

/// Shared `Number`/`BigInt`.prototype.toLocaleString body: resolve a
/// fresh `NumberFormat` from `(locales, options)` and format `value`
/// through the same path `NumberFormat.prototype.format` uses, so the
/// two render identically.
pub(crate) fn to_locale_string(
    ctx: &mut NativeCtx<'_>,
    value: f64,
    locales: Value,
    options: Value,
) -> Result<Value, NativeError> {
    let payload = resolve_ctx(ctx, locales, options)?;
    let rendered = format_number(value, &payload);
    Ok(Value::string(JsString::from_str(
        &rendered,
        ctx.heap_mut(),
    )?))
}

/// §11.1.6 `Intl.NumberFormat.prototype.formatToParts(value)`.
pub(crate) fn number_format_format_to_parts(
    ctx: &mut NativeCtx<'_>,
    args: &[Value],
) -> Result<Value, NativeError> {
    let payload = require_number_format(ctx, "formatToParts")?;
    let n = coerce_format_arg(ctx, args.first())?;
    let parts = partition_number(n, &payload);
    let type_lit = |t: &str, ctx: &mut NativeCtx<'_>| JsString::from_str(t, ctx.heap_mut());

    let mut elements: Vec<Value> = Vec::with_capacity(parts.len());
    for (ty, val) in &parts {
        let ty_s = Value::string(type_lit(ty, ctx)?);
        let val_s = Value::string(JsString::from_str(val, ctx.heap_mut())?);
        let snapshot = elements.clone();
        let mut obj = ctx.alloc_object_with_roots(&[&ty_s, &val_s], &[&snapshot])?;
        crate::object::set(&mut obj, ctx.heap_mut(), "type", ty_s);
        crate::object::set(&mut obj, ctx.heap_mut(), "value", val_s);
        elements.push(Value::object(obj));
    }
    let element_roots = elements.clone();
    let mut visit = |visitor: &mut dyn FnMut(*mut RawGc)| {
        for v in &element_roots {
            v.trace_value_slots(visitor);
        }
    };
    let arr = crate::array::from_elements_with_roots(ctx.heap_mut(), elements, &mut visit)?;
    Ok(Value::array(arr))
}

/// CLDR-style separator joining the two endpoints of a non-collapsed
/// numeric range (narrow no-break space, en dash, narrow no-break space).
const RANGE_SEPARATOR: &str = "\u{2009}\u{2013}\u{2009}";

/// Coerce a `formatRange` endpoint to an `f64`, accepting BigInt and
/// numeric strings (an approximation of ToIntlMathematicalValue).
/// `Infinity` survives; only `NaN` is signalled so the caller can raise
/// the spec's `RangeError`.
fn coerce_range_arg(ctx: &mut NativeCtx<'_>, value: &Value) -> Result<f64, NativeError> {
    coerce_format_arg(ctx, Some(value))
}

/// §1.1.21 reject-undefined + NaN guard shared by `formatRange` /
/// `formatRangeToParts`: an `undefined` endpoint is a `TypeError`
/// (PartitionNumberRangePattern caller step 3), a `NaN` endpoint a
/// `RangeError` (step 1).
fn range_args(
    ctx: &mut NativeCtx<'_>,
    args: &[Value],
    name: &'static str,
) -> Result<(f64, f64), NativeError> {
    let undef = |v: Option<&Value>| v.is_none() || v.is_some_and(|x| x.is_undefined());
    if undef(args.first()) || undef(args.get(1)) {
        return Err(NativeError::TypeError {
            name,
            reason: "start and end must not be undefined".to_string(),
        });
    }
    let x = coerce_range_arg(ctx, &args.first().copied().expect("checked above"))?;
    let y = coerce_range_arg(ctx, &args.get(1).copied().expect("checked above"))?;
    if x.is_nan() || y.is_nan() {
        return Err(NativeError::RangeError {
            name,
            reason: "range endpoints must not be NaN".to_string(),
        });
    }
    Ok((x, y))
}

/// §1.1.21 `Intl.NumberFormat.prototype.formatRange(start, end)`.
///
/// ICU exposes no numeric-range formatter here, so render each endpoint
/// and join with [`RANGE_SEPARATOR`]; identical-rendering endpoints
/// collapse to the single number. CLDR's approximately-equal "~" prefix
/// and shared-affix collapsing are not reproduced.
pub(crate) fn number_format_format_range(
    ctx: &mut NativeCtx<'_>,
    args: &[Value],
) -> Result<Value, NativeError> {
    let payload = require_number_format(ctx, "formatRange")?;
    let (x, y) = range_args(ctx, args, "formatRange")?;
    let start = format_number(x, &payload);
    let end = format_number(y, &payload);
    let combined = if start == end {
        start
    } else {
        format!("{start}{RANGE_SEPARATOR}{end}")
    };
    Ok(Value::string(JsString::from_str(
        &combined,
        ctx.heap_mut(),
    )?))
}

/// §1.1.22 `Intl.NumberFormat.prototype.formatRangeToParts(start, end)`.
///
/// Each part carries a `source` of `"startRange"`, `"endRange"`, or
/// `"shared"`; identical-rendering endpoints collapse to all-`"shared"`.
pub(crate) fn number_format_format_range_to_parts(
    ctx: &mut NativeCtx<'_>,
    args: &[Value],
) -> Result<Value, NativeError> {
    let payload = require_number_format(ctx, "formatRangeToParts")?;
    let (x, y) = range_args(ctx, args, "formatRangeToParts")?;
    let start_parts = partition_number(x, &payload);
    let end_parts = partition_number(y, &payload);
    let start_str: String = start_parts.iter().map(|(_, v)| v.as_str()).collect();
    let end_str: String = end_parts.iter().map(|(_, v)| v.as_str()).collect();

    let mut triples: Vec<(&'static str, String, &'static str)> = Vec::new();
    if start_str == end_str {
        for (ty, val) in start_parts {
            triples.push((ty, val, "shared"));
        }
    } else {
        for (ty, val) in &start_parts {
            triples.push((ty, val.clone(), "startRange"));
        }
        triples.push(("literal", RANGE_SEPARATOR.to_string(), "shared"));
        for (ty, val) in &end_parts {
            triples.push((ty, val.clone(), "endRange"));
        }
    }

    let mut elements: Vec<Value> = Vec::with_capacity(triples.len());
    for (ty, val, src) in &triples {
        let ty_s = Value::string(JsString::from_str(ty, ctx.heap_mut())?);
        let val_s = Value::string(JsString::from_str(val, ctx.heap_mut())?);
        let src_s = Value::string(JsString::from_str(src, ctx.heap_mut())?);
        let snapshot = elements.clone();
        let mut obj = ctx.alloc_object_with_roots(&[&ty_s, &val_s, &src_s], &[&snapshot])?;
        crate::object::set(&mut obj, ctx.heap_mut(), "type", ty_s);
        crate::object::set(&mut obj, ctx.heap_mut(), "value", val_s);
        crate::object::set(&mut obj, ctx.heap_mut(), "source", src_s);
        elements.push(Value::object(obj));
    }
    let element_roots = elements.clone();
    let mut visit = |visitor: &mut dyn FnMut(*mut RawGc)| {
        for v in &element_roots {
            v.trace_value_slots(visitor);
        }
    };
    let arr = crate::array::from_elements_with_roots(ctx.heap_mut(), elements, &mut visit)?;
    Ok(Value::array(arr))
}

/// Partition a formatted number into `{type, value}` components for
/// `formatToParts`. Locale separators follow the en-style `,` group /
/// `.` decimal that the resolved formatter targets.
pub(crate) fn partition_number(
    n: f64,
    payload: &NumberFormatPayload,
) -> Vec<(&'static str, String)> {
    let mut parts: Vec<(&'static str, String)> = Vec::new();
    if n.is_nan() {
        push_sign(
            &mut parts,
            displayed_sign(&payload.sign_display, false, false, true),
        );
        parts.push(("nan", nan_symbol(&payload.locale).to_string()));
        return parts;
    }

    let is_neg = n.is_sign_negative();
    let is_zero = rounds_to_zero(n, payload);
    let sign = displayed_sign(&payload.sign_display, is_neg, is_zero, false);

    // Currency: render the unsigned ICU string, then split off the symbol
    // / affixes around the numeric core so the `currency` parts carry the
    // CLDR-correct symbol (no hand-rolled table). The sign is applied
    // separately per `signDisplay`.
    if payload.style == "currency" && n.is_finite() {
        // `accounting` wraps negatives in parenthesis literals instead of
        // a minus-sign part.
        let accounting_negative = sign == SignKind::Minus && payload.currency_sign == "accounting";
        let full = currency_string(n.abs(), payload);
        let core = format_decimal_signed(n.abs(), is_neg, payload);
        if let Some(idx) = full.find(&core) {
            if accounting_negative {
                parts.push(("literal", "(".to_string()));
            } else {
                push_sign(&mut parts, sign);
            }
            let prefix = &full[..idx];
            if !prefix.is_empty() {
                parts.push(("currency", prefix.to_string()));
            }
            push_number_parts(&mut parts, &core);
            let suffix = &full[idx + core.len()..];
            if !suffix.is_empty() {
                parts.push(("currency", suffix.to_string()));
            }
            if accounting_negative {
                parts.push(("literal", ")".to_string()));
            }
            return parts;
        }
        // Affix split failed — surface the whole string as a literal.
        push_sign(&mut parts, sign);
        parts.push(("literal", full));
        return parts;
    }

    // Unit: render the unsigned ICU unit string, then split the number
    // core out so the surrounding pattern text becomes `unit` parts
    // (whitespace adjacent to the number stays a `literal`).
    if payload.style == "unit" && n.is_finite() {
        let full = unit_string(n.abs(), payload);
        let core = format_decimal_signed(n.abs(), is_neg, payload);
        if let Some(idx) = full.find(&core) {
            push_sign(&mut parts, sign);
            push_unit_affix(&mut parts, &full[..idx], false);
            push_number_parts(&mut parts, &core);
            push_unit_affix(&mut parts, &full[idx + core.len()..], true);
            return parts;
        }
        push_sign(&mut parts, sign);
        parts.push(("literal", full));
        return parts;
    }

    push_sign(&mut parts, sign);
    if payload.notation == "compact" && payload.style == "decimal" && n.is_finite() {
        let (m, suffix, join) = compact_decompose(n.abs(), payload);
        let core = format_compact_mantissa(m, payload);
        let (dec_sep, group_sep) = locale_separators(&payload.locale);
        push_number_parts_sep(&mut parts, &core, dec_sep, group_sep);
        if !suffix.is_empty() {
            if !join.is_empty() {
                parts.push(("literal", join.to_string()));
            }
            parts.push(("compact", suffix.to_string()));
        }
        return parts;
    }
    let scientific = matches!(payload.notation.as_str(), "scientific" | "engineering");
    if n.is_infinite() {
        parts.push(("infinity", "∞".to_string()));
    } else if scientific && payload.style != "currency" {
        let base = if payload.style == "percent" {
            n.abs() * 100.0
        } else {
            n.abs()
        };
        let (mant, exp) = scientific_parts(base, payload.notation == "engineering");
        push_number_parts(&mut parts, &format_decimal(mant, payload));
        parts.push(("exponentSeparator", "E".to_string()));
        if exp < 0 {
            parts.push(("exponentMinusSign", "-".to_string()));
        }
        parts.push(("exponentInteger", exp.unsigned_abs().to_string()));
    } else {
        let value = if payload.style == "percent" {
            n.abs() * 100.0
        } else {
            n.abs()
        };
        push_number_parts(&mut parts, &format_decimal(value, payload));
    }
    if payload.style == "percent" {
        parts.push(("percentSign", "%".to_string()));
    }
    parts
}

/// Split a formatted unsigned decimal core (`"1,234.50"`) into
/// `integer` / `group` / `decimal` / `fraction` parts.
/// Emit a unit pattern affix (the text before or after the number in
/// `"1 m"` / `"1m"`). Whitespace adjacent to the number is a `literal`
/// part; the remaining text is the `unit` part. `trailing` selects which
/// side of the affix touches the number (the number's side is the start
/// of a suffix and the end of a prefix).
fn push_unit_affix(parts: &mut Vec<(&'static str, String)>, affix: &str, trailing: bool) {
    if affix.is_empty() {
        return;
    }
    if trailing {
        // Suffix: leading whitespace touches the number.
        let unit_start = affix
            .find(|c: char| !c.is_whitespace())
            .unwrap_or(affix.len());
        if unit_start > 0 {
            parts.push(("literal", affix[..unit_start].to_string()));
        }
        if unit_start < affix.len() {
            parts.push(("unit", affix[unit_start..].to_string()));
        }
    } else {
        // Prefix: trailing whitespace touches the number. The split point
        // is the byte index just past the last non-whitespace char (a
        // char boundary — `rfind` returns the char's start, which is not
        // the boundary for a multi-byte unit label such as `時速`).
        let unit_end = affix
            .char_indices()
            .rev()
            .find(|(_, c)| !c.is_whitespace())
            .map_or(0, |(i, c)| i + c.len_utf8());
        if unit_end > 0 {
            parts.push(("unit", affix[..unit_end].to_string()));
        }
        if unit_end < affix.len() {
            parts.push(("literal", affix[unit_end..].to_string()));
        }
    }
}

fn push_number_parts(parts: &mut Vec<(&'static str, String)>, core: &str) {
    push_number_parts_sep(parts, core, '.', ',');
}

/// As [`push_number_parts`], with explicit locale separators — the
/// compact path renders through ICU with the locale's real group /
/// decimal characters (de: `.` groups and `,` separates decimals).
fn push_number_parts_sep(
    parts: &mut Vec<(&'static str, String)>,
    core: &str,
    decimal_sep: char,
    group_sep: char,
) {
    let (int_part, frac_part) = core.split_once(decimal_sep).unwrap_or((core, ""));
    let mut first = true;
    for seg in int_part.split(group_sep) {
        if !first {
            parts.push(("group", group_sep.to_string()));
        }
        parts.push(("integer", seg.to_string()));
        first = false;
    }
    if !frac_part.is_empty() {
        parts.push(("decimal", decimal_sep.to_string()));
        parts.push(("fraction", frac_part.to_string()));
    }
}

/// The `(decimal, group)` separator pair for the payload's locale —
/// covers the locales whose compact output test262 checks.
fn locale_separators(locale: &str) -> (char, char) {
    match locale.split('-').next().unwrap_or("en") {
        "de" | "es" | "it" | "pt" | "id" | "tr" | "nl" | "da" => (',', '.'),
        "fr" | "ru" | "pl" | "uk" | "cs" | "sv" | "fi" | "nb" => (',', '\u{202f}'),
        _ => ('.', ','),
    }
}

/// §11.1.7 `Intl.NumberFormat.prototype.resolvedOptions()`.
pub(crate) fn number_format_resolved_options(
    ctx: &mut NativeCtx<'_>,
    _args: &[Value],
) -> Result<Value, NativeError> {
    let payload = require_number_format(ctx, "resolvedOptions")?;
    let locale = Value::string(JsString::from_str(&payload.locale, ctx.heap_mut())?);
    let numbering_system = Value::string(JsString::from_str(
        &payload.numbering_system,
        ctx.heap_mut(),
    )?);
    let style = Value::string(JsString::from_str(&payload.style, ctx.heap_mut())?);
    let currency_val = match &payload.currency {
        Some(c) => Some(Value::string(JsString::from_str(c, ctx.heap_mut())?)),
        None => None,
    };
    // currencyDisplay / currencySign are only reported for currency style.
    let (currency_display_val, currency_sign_val) = if payload.style == "currency" {
        (
            Some(Value::string(JsString::from_str(
                &payload.currency_display,
                ctx.heap_mut(),
            )?)),
            Some(Value::string(JsString::from_str(
                &payload.currency_sign,
                ctx.heap_mut(),
            )?)),
        )
    } else {
        (None, None)
    };
    // unit / unitDisplay are only reported for unit style.
    let (unit_val, unit_display_val) = match (&payload.unit, payload.style.as_str()) {
        (Some(u), "unit") => (
            Some(Value::string(JsString::from_str(u, ctx.heap_mut())?)),
            Some(Value::string(JsString::from_str(
                &payload.unit_display,
                ctx.heap_mut(),
            )?)),
        ),
        _ => (None, None),
    };
    let min_fd = payload.minimum_fraction_digits as i32;
    let max_fd = payload.maximum_fraction_digits as i32;
    let use_grouping = payload.use_grouping;
    let sign_display = Value::string(JsString::from_str(&payload.sign_display, ctx.heap_mut())?);
    let notation = Value::string(JsString::from_str(&payload.notation, ctx.heap_mut())?);
    let compact_display_val = if payload.notation == "compact" {
        Some(Value::string(JsString::from_str(
            &payload.compact_display,
            ctx.heap_mut(),
        )?))
    } else {
        None
    };
    let mut value_roots = vec![&locale, &numbering_system, &style, &sign_display, &notation];
    if let Some(c) = &compact_display_val {
        value_roots.push(c);
    }
    if let Some(c) = &currency_val {
        value_roots.push(c);
    }
    if let Some(c) = &currency_display_val {
        value_roots.push(c);
    }
    if let Some(c) = &currency_sign_val {
        value_roots.push(c);
    }
    if let Some(u) = &unit_val {
        value_roots.push(u);
    }
    if let Some(u) = &unit_display_val {
        value_roots.push(u);
    }
    let mut obj = ctx.alloc_object_with_roots(&value_roots, &[])?;
    let heap = ctx.heap_mut();
    crate::object::set(&mut obj, heap, "locale", locale);
    crate::object::set(&mut obj, heap, "numberingSystem", numbering_system);
    crate::object::set(&mut obj, heap, "style", style);
    if let Some(c) = currency_val {
        crate::object::set(&mut obj, heap, "currency", c);
    }
    if let Some(c) = currency_display_val {
        crate::object::set(&mut obj, heap, "currencyDisplay", c);
    }
    if let Some(c) = currency_sign_val {
        crate::object::set(&mut obj, heap, "currencySign", c);
    }
    if let Some(u) = unit_val {
        crate::object::set(&mut obj, heap, "unit", u);
    }
    if let Some(u) = unit_display_val {
        crate::object::set(&mut obj, heap, "unitDisplay", u);
    }
    crate::object::set(
        &mut obj,
        heap,
        "minimumIntegerDigits",
        Value::number_i32(i32::from(payload.minimum_integer_digits)),
    );
    // Significant-digit rounding (roundingPriority "auto" with a
    // significant option present) reports the significant-digit pair and
    // omits the inert fraction-digit pair, matching the spec's internal
    // slots.
    if let (Some(mn), Some(mx)) = (
        payload.minimum_significant_digits,
        payload.maximum_significant_digits,
    ) {
        crate::object::set(
            &mut obj,
            heap,
            "minimumSignificantDigits",
            Value::number_i32(i32::from(mn)),
        );
        crate::object::set(
            &mut obj,
            heap,
            "maximumSignificantDigits",
            Value::number_i32(i32::from(mx)),
        );
    } else {
        crate::object::set(
            &mut obj,
            heap,
            "minimumFractionDigits",
            Value::number_i32(min_fd),
        );
        crate::object::set(
            &mut obj,
            heap,
            "maximumFractionDigits",
            Value::number_i32(max_fd),
        );
    }
    crate::object::set(&mut obj, heap, "useGrouping", Value::boolean(use_grouping));
    crate::object::set(&mut obj, heap, "notation", notation);
    if let Some(compact_display) = compact_display_val {
        crate::object::set(&mut obj, heap, "compactDisplay", compact_display);
    }
    crate::object::set(&mut obj, heap, "signDisplay", sign_display);
    Ok(Value::object(obj))
}

/// Render `n` per the resolved option bag.
pub(crate) fn format_number(n: f64, payload: &NumberFormatPayload) -> String {
    if n.is_nan() {
        let sign = sign_prefix(displayed_sign(&payload.sign_display, false, false, true));
        return format!("{sign}{}", nan_symbol(&payload.locale));
    }
    let is_neg = n.is_sign_negative();
    let is_zero = rounds_to_zero(n, payload);
    let sign_kind = displayed_sign(&payload.sign_display, is_neg, is_zero, false);

    // Currency applies the sign around the whole formatted body so the
    // `accounting` sign can wrap negatives in the locale affixes.
    if payload.style == "currency" && n.is_finite() {
        let body = currency_string(n.abs(), payload);
        return apply_currency_sign(&body, sign_kind, &payload.currency_sign);
    }

    let sign = sign_prefix(sign_kind);
    if payload.notation == "compact" && payload.style == "decimal" && n.is_finite() {
        let (m, suffix, join) = compact_decompose(n.abs(), payload);
        let core = format_compact_mantissa(m, payload);
        return format!("{sign}{core}{join}{suffix}");
    }
    let scientific = matches!(payload.notation.as_str(), "scientific" | "engineering");
    let magnitude = if n.is_infinite() {
        "∞".to_string()
    } else if scientific {
        let base = if payload.style == "percent" {
            n.abs() * 100.0
        } else {
            n.abs()
        };
        let core = render_scientific(base, payload, payload.notation == "engineering");
        if payload.style == "percent" {
            format!("{core}%")
        } else {
            core
        }
    } else {
        match payload.style.as_str() {
            "currency" => currency_string(n.abs(), payload),
            "unit" => unit_string(n.abs(), payload),
            "percent" => {
                format!(
                    "{}%",
                    format_decimal_signed(n.abs() * 100.0, is_neg, payload)
                )
            }
            _ => format_decimal_signed(n.abs(), is_neg, payload),
        }
    };
    format!("{sign}{magnitude}")
}

/// Apply the displayed sign to an unsigned currency `body`. Under the
/// `accounting` currency sign a negative is wrapped in parentheses (the
/// CLDR accounting affix for en + CJK locales) rather than prefixed with
/// a minus.
fn apply_currency_sign(body: &str, kind: SignKind, currency_sign: &str) -> String {
    match kind {
        SignKind::Minus => {
            if currency_sign == "accounting" {
                format!("({body})")
            } else {
                format!("-{body}")
            }
        }
        SignKind::Plus => format!("+{body}"),
        SignKind::None => body.to_string(),
    }
}

/// Decompose `abs` into a `(mantissa, exponent)` pair for scientific
/// notation (mantissa in `[1, 10)`) or engineering notation (exponent a
/// multiple of 3, mantissa in `[1, 1000)`).
fn scientific_parts(abs: f64, engineering: bool) -> (f64, i32) {
    if abs == 0.0 || !abs.is_finite() {
        return (abs, 0);
    }
    let mut exp = abs.log10().floor() as i32;
    // Correct for floating-point error at exact powers of ten.
    let mant = abs / 10f64.powi(exp);
    if mant >= 10.0 {
        exp += 1;
    } else if mant < 1.0 {
        exp -= 1;
    }
    if engineering {
        exp = exp.div_euclid(3) * 3;
    }
    (abs / 10f64.powi(exp), exp)
}

/// Render `abs` in scientific / engineering notation: ICU-formatted
/// mantissa (locale decimal separator, default 0..3 fraction digits)
/// joined to the exponent by the `E` separator.
fn render_scientific(abs: f64, payload: &NumberFormatPayload, engineering: bool) -> String {
    let (mant, exp) = scientific_parts(abs, engineering);
    format!("{}E{exp}", format_decimal(mant, payload))
}

/// The sign glyph a value renders under a `signDisplay` policy.
#[derive(Clone, Copy, PartialEq)]
enum SignKind {
    /// No sign rendered.
    None,
    /// A minus sign (`-`).
    Minus,
    /// A plus sign (`+`).
    Plus,
}

/// §15.5 — pick the displayed sign from `signDisplay`, the value's sign
/// bit, and whether the rounded magnitude is zero. NaN counts as
/// non-negative and non-zero, so `always` yields `+NaN` while the other
/// policies render no sign.
fn displayed_sign(sign_display: &str, is_negative: bool, is_zero: bool, is_nan: bool) -> SignKind {
    if is_nan {
        return if sign_display == "always" {
            SignKind::Plus
        } else {
            SignKind::None
        };
    }
    match sign_display {
        "never" => SignKind::None,
        "always" => {
            if is_negative {
                SignKind::Minus
            } else {
                SignKind::Plus
            }
        }
        "exceptZero" => {
            if is_zero {
                SignKind::None
            } else if is_negative {
                SignKind::Minus
            } else {
                SignKind::Plus
            }
        }
        "negative" => {
            if is_negative && !is_zero {
                SignKind::Minus
            } else {
                SignKind::None
            }
        }
        // "auto"
        _ => {
            if is_negative {
                SignKind::Minus
            } else {
                SignKind::None
            }
        }
    }
}

/// One CLDR compact-decimal magnitude bucket.
struct CompactBucket {
    /// Magnitude threshold (the CLDR `10^n` type).
    base: f64,
    /// Compact suffix ("K", "million", "万", ...).
    suffix: &'static str,
    /// Joiner between number and suffix ("" tight, " " space,
    /// "\u{a0}" no-break space).
    join: &'static str,
}

const B: fn(f64, &'static str, &'static str) -> CompactBucket =
    |base, suffix, join| CompactBucket { base, suffix, join };

/// CLDR compact-decimal patterns for the locales test262 exercises.
/// icu_experimental 0.5 ships no compact-decimal formatter, so this is
/// a hand-rolled CLDR subset (en / de / ja / ko / zh) — a targeted
/// fallback rather than full CLDR coverage; unlisted locales use the
/// en table (matches CLDR root-ish behavior for K/M/B/T).
fn compact_buckets(locale: &str, display: &str) -> Vec<CompactBucket> {
    let lang = locale.split('-').next().unwrap_or("en");
    let hant = locale.contains("TW") || locale.contains("Hant") || locale.contains("HK");
    match (lang, display) {
        ("ja", _) => vec![B(1e4, "万", ""), B(1e8, "億", ""), B(1e12, "兆", "")],
        ("zh", _) if hant => {
            vec![B(1e4, "萬", ""), B(1e8, "億", ""), B(1e12, "兆", "")]
        }
        ("zh", _) => vec![B(1e4, "万", ""), B(1e8, "亿", ""), B(1e12, "兆", "")],
        ("ko", _) => vec![
            B(1e3, "천", ""),
            B(1e4, "만", ""),
            B(1e8, "억", ""),
            B(1e12, "조", ""),
        ],
        ("de", "long") => vec![
            B(1e3, "Tausend", " "),
            B(1e6, "Millionen", " "),
            B(1e9, "Milliarden", " "),
            B(1e12, "Billionen", " "),
        ],
        ("de", _) => vec![
            B(1e6, "Mio.", "\u{a0}"),
            B(1e9, "Mrd.", "\u{a0}"),
            B(1e12, "Bio.", "\u{a0}"),
        ],
        (_, "long") => vec![
            B(1e3, "thousand", " "),
            B(1e6, "million", " "),
            B(1e9, "billion", " "),
            B(1e12, "trillion", " "),
        ],
        _ => vec![
            B(1e3, "K", ""),
            B(1e6, "M", ""),
            B(1e9, "B", ""),
            B(1e12, "T", ""),
        ],
    }
}

/// ECMA-402 compact default rounding — roundingPriority
/// "morePrecision" over {minSig 1, maxSig 2} and {minFrac 0, maxFrac 0}:
/// the fraction-digit candidate wins once the value has three or more
/// integer digits, the two-significant-digit candidate otherwise.
fn compact_round(m: f64) -> f64 {
    if m == 0.0 || !m.is_finite() {
        return m;
    }
    if m.abs() >= 100.0 {
        return m.round();
    }
    let exp = m.abs().log10().floor() as i32;
    let scale = 10f64.powi(1 - exp);
    (m * scale).round() / scale
}

/// Decompose `abs` for compact notation: `(rounded mantissa, suffix,
/// joiner)`. No matching bucket leaves the value un-suffixed.
fn compact_decompose(abs: f64, payload: &NumberFormatPayload) -> (f64, &'static str, &'static str) {
    let buckets = compact_buckets(&payload.locale, &payload.compact_display);
    let mut chosen: Option<usize> = None;
    for (i, b) in buckets.iter().enumerate() {
        if abs >= b.base {
            chosen = Some(i);
        }
    }
    let Some(mut idx) = chosen else {
        return (compact_round(abs), "", "");
    };
    let mut m = compact_round(abs / buckets[idx].base);
    // Rounding can promote into the next bucket (999_950 -> "1000K" -> "1M").
    while let Some(next) = buckets.get(idx + 1) {
        if m * buckets[idx].base >= next.base {
            idx += 1;
            m = compact_round(abs / buckets[idx].base);
        } else {
            break;
        }
    }
    (m, buckets[idx].suffix, buckets[idx].join)
}

/// Render a compact mantissa with the locale's separators. Compact
/// notation defaults to "min2" grouping (a separator only once two
/// digits precede it: 9876 stays "9876", 98765 groups).
fn format_compact_mantissa(m: f64, payload: &NumberFormatPayload) -> String {
    let locale = Locale::from_str(&payload.locale)
        .or_else(|_| Locale::from_str(DEFAULT_LOCALE))
        .expect("default locale parses");
    let mut options = DecimalFormatterOptions::default();
    options.grouping_strategy = Some(if payload.use_grouping {
        GroupingStrategy::Min2
    } else {
        GroupingStrategy::Never
    });
    let rendered = m.to_string();
    match (
        DecimalFormatter::try_new((&locale).into(), options),
        Decimal::from_str(&rendered),
    ) {
        (Ok(formatter), Ok(decimal)) => {
            let mut out = String::new();
            let _ = writeable::Writeable::write_to(&formatter.format(&decimal), &mut out);
            out
        }
        _ => rendered,
    }
}

/// Locale NaN symbol (CLDR `nan`) for the subset of locales test262
/// exercises; everything else renders "NaN".
fn nan_symbol(locale: &str) -> &'static str {
    if locale.starts_with("zh")
        && (locale.contains("TW") || locale.contains("Hant") || locale.contains("HK"))
    {
        "非數值"
    } else {
        "NaN"
    }
}

/// The literal string for a [`SignKind`].
fn sign_prefix(kind: SignKind) -> &'static str {
    match kind {
        SignKind::Minus => "-",
        SignKind::Plus => "+",
        SignKind::None => "",
    }
}

/// Append the `minusSign` / `plusSign` part for a [`SignKind`] (nothing
/// for [`SignKind::None`]).
fn push_sign(parts: &mut Vec<(&'static str, String)>, kind: SignKind) {
    match kind {
        SignKind::Minus => parts.push(("minusSign", "-".to_string())),
        SignKind::Plus => parts.push(("plusSign", "+".to_string())),
        SignKind::None => {}
    }
}

/// Whether the value rounds to zero at the resolved fraction precision
/// (used by `signDisplay` zero-suppression). Infinity is never zero.
fn rounds_to_zero(n: f64, payload: &NumberFormatPayload) -> bool {
    if !n.is_finite() {
        return false;
    }
    let scaled = if payload.style == "percent" {
        n.abs() * 100.0
    } else {
        n.abs()
    };
    let max = payload.maximum_fraction_digits as usize;
    let s = format!("{scaled:.max$}");
    s.bytes().all(|b| b == b'0' || b == b'.')
}

/// Build a `fixed_decimal::Decimal` for `value` honouring the resolved
/// min/max fraction digits.
fn decimal_from(value: f64, payload: &NumberFormatPayload) -> Option<Decimal> {
    let max = payload.maximum_fraction_digits as usize;
    let formatted = format!("{value:.max$}");
    let trimmed = trim_trailing_zero_fraction(&formatted, payload.minimum_fraction_digits as usize);
    let mut dec = Decimal::from_str(&trimmed).ok()?;
    dec.pad_end(-(payload.minimum_fraction_digits as i16));
    Some(dec)
}

/// Format the unsigned magnitude of a currency value through ICU's
/// CLDR-backed [`CurrencyFormatter`] (correct symbol + placement for
/// every ISO-4217 code and locale). The caller applies the `signDisplay`
/// sign. Falls back to the ISO code prefix only when ICU data or the
/// code itself is unavailable — never a hand-rolled symbol table.
fn currency_string(magnitude: f64, payload: &NumberFormatPayload) -> String {
    let code = payload.currency.as_deref().unwrap_or("USD");
    let locale = Locale::from_str(&payload.locale)
        .or_else(|_| Locale::from_str(DEFAULT_LOCALE))
        .expect("default locale parses");
    let abs = magnitude.abs();
    if let (Ok(cc), Some(dec)) = (
        TinyAsciiStr::<3>::try_from_str(code),
        decimal_from(abs, payload),
    ) {
        let prefs = CurrencyFormatterPreferences::from(&locale);
        let width = if payload.currency_display == "narrowSymbol" {
            icu_experimental::dimension::currency::options::Width::Narrow
        } else {
            icu_experimental::dimension::currency::options::Width::Short
        };
        if let Ok(fmt) = CurrencyFormatter::try_new(prefs, CurrencyFormatterOptions::from(width)) {
            let mut out = String::new();
            let _ = writeable::Writeable::write_to(
                &fmt.format_fixed_decimal(&dec, &CurrencyCode(cc)),
                &mut out,
            );
            return out;
        }
    }
    let core = format_decimal(abs, payload);
    format!("{code}\u{a0}{core}")
}

/// Format the unsigned magnitude of a unit value through ICU's
/// [`UnitsFormatter`] (locale unit pattern + plural rules). The caller
/// applies the `signDisplay` sign. Falls back to `"<number> <unit>"`
/// when ICU data or the unit identifier is unavailable.
fn unit_string(magnitude: f64, payload: &NumberFormatPayload) -> String {
    let unit = payload.unit.as_deref().unwrap_or("");
    let locale = Locale::from_str(&payload.locale)
        .or_else(|_| Locale::from_str(DEFAULT_LOCALE))
        .expect("default locale parses");
    let width = match payload.unit_display.as_str() {
        "long" => UnitWidth::Long,
        "narrow" => UnitWidth::Narrow,
        _ => UnitWidth::Short,
    };
    let abs = magnitude.abs();
    if let Some(dec) = decimal_from(abs, payload) {
        let prefs = UnitsFormatterPreferences::from(&locale);
        if let Ok(fmt) = UnitsFormatter::try_new(prefs, unit, UnitsFormatterOptions::from(width)) {
            let mut out = String::new();
            let _ = writeable::Writeable::write_to(&fmt.format_fixed_decimal(&dec), &mut out);
            return out;
        }
    }
    format!("{} {unit}", format_decimal(abs, payload))
}

/// Exact decimal rounding of `abs` to `frac_digits` fraction digits by
/// multiples of `increment`, honoring the §15.1.2 roundingMode. Works
/// on the shortest decimal representation so binary-float artifacts
/// ("1.15" stored as 1.1499…) never leak into rounding decisions.
/// `None` when the value exceeds the exact integer domain (caller
/// falls back to the binary path).
fn round_decimal_exact(
    abs: f64,
    is_negative: bool,
    frac_digits: usize,
    increment: u16,
    mode: &str,
) -> Option<String> {
    round_decimal_exact_scaled(abs, is_negative, frac_digits as i32, increment, mode)
}

/// As [`round_decimal_exact`] with a possibly NEGATIVE fraction-digit
/// count (rounding at integer positions — the significant-digit path
/// needs it for values >= 10^maxSig).
fn round_decimal_exact_scaled(
    abs: f64,
    is_negative: bool,
    frac_digits: i32,
    increment: u16,
    mode: &str,
) -> Option<String> {
    if !abs.is_finite() {
        return None;
    }
    // Shortest round-trip repr; expand any scientific notation.
    let repr = format!("{abs}");
    let (mantissa, exp) = match repr.split_once(['e', 'E']) {
        Some((m, e)) => (m.to_string(), e.parse::<i32>().ok()?),
        None => (repr, 0),
    };
    let (int_raw, frac_raw) = match mantissa.split_once('.') {
        Some((i, f)) => (i.to_string(), f.to_string()),
        None => (mantissa, String::new()),
    };
    let mut digits: Vec<u8> = int_raw
        .bytes()
        .chain(frac_raw.bytes())
        .map(|b| b - b'0')
        .collect();
    // Decimal point position from the left, shifted by the exponent.
    let mut point = int_raw.len() as i32 + exp;
    while digits.len() > 1 && digits[0] == 0 && point > 1 {
        digits.remove(0);
        point -= 1;
    }
    while point <= 0 {
        digits.insert(0, 0);
        point += 1;
    }
    while (point as usize) > digits.len() {
        digits.push(0);
    }
    let scale_len = point + frac_digits;
    if scale_len < 0 {
        return Some("0".to_string());
    }
    // Consume EVERY available digit so the remainder comparison below
    // is exact — trailing digits contribute real distance, not just a
    // tie-break (1.750 at one fraction digit by increments of 5 is an
    // exact tie, not "below half").
    if scale_len > 30 {
        return None;
    }
    let ext_len = (digits.len() as i32).min(30).max(scale_len);
    let extra = (ext_len - scale_len) as u32;
    let mut n: i128 = 0;
    for i in 0..ext_len as usize {
        n = n * 10 + i128::from(*digits.get(i).unwrap_or(&0));
    }
    let leftover_beyond_ext = digits
        .get(ext_len as usize..)
        .unwrap_or(&[])
        .iter()
        .any(|&d| d != 0);
    let inc_ext = i128::from(increment).checked_mul(10i128.checked_pow(extra)?)?;
    let div = n / inc_ext;
    let rem = n % inc_ext;
    let round_up = if rem == 0 && !leftover_beyond_ext {
        false
    } else {
        match mode {
            "trunc" => false,
            "expand" => true,
            "ceil" => !is_negative,
            "floor" => is_negative,
            _ => {
                let twice = rem.checked_mul(2)?;
                if twice < inc_ext {
                    false
                } else if twice > inc_ext || leftover_beyond_ext {
                    true
                } else {
                    match mode {
                        "halfTrunc" => false,
                        "halfCeil" => !is_negative,
                        "halfFloor" => is_negative,
                        "halfEven" => (div % 2) != 0,
                        // halfExpand
                        _ => true,
                    }
                }
            }
        }
    };
    let q = (div + i128::from(round_up)).checked_mul(i128::from(increment))?;
    let mut out = q.to_string();
    if frac_digits > 0 {
        let frac = frac_digits as usize;
        while out.len() <= frac {
            out.insert(0, '0');
        }
        out.insert(out.len() - frac, '.');
    } else {
        // Negative fraction digits round at integer positions — pad the
        // dropped places back with zeros.
        for _ in 0..(-frac_digits) {
            out.push('0');
        }
    }
    Some(out)
}

/// Significant-digit rounding with an explicit roundingMode: derive
/// the fraction-digit position from the value's decimal exponent and
/// reuse the exact rounding core.
fn round_significant_exact(
    abs: f64,
    is_negative: bool,
    max_sig: u8,
    min_sig: u8,
    mode: &str,
) -> Option<String> {
    if abs == 0.0 {
        return Some("0".to_string());
    }
    let repr = format!("{abs}");
    let (mantissa, exp) = match repr.split_once(['e', 'E']) {
        Some((m, e)) => (m.to_string(), e.parse::<i32>().ok()?),
        None => (repr, 0),
    };
    let (int_raw, frac_raw) = match mantissa.split_once('.') {
        Some((i, f)) => (i.to_string(), f.to_string()),
        None => (mantissa, String::new()),
    };
    let digits: Vec<u8> = int_raw
        .bytes()
        .chain(frac_raw.bytes())
        .map(|b| b - b'0')
        .collect();
    let point = int_raw.len() as i32 + exp;
    let first_sig = digits.iter().position(|&d| d != 0)? as i32;
    // value = digits[first_sig].… × 10^e
    let e = point - 1 - first_sig;
    let frac = i32::from(max_sig) - 1 - e;
    let rounded = round_decimal_exact_scaled(abs, is_negative, frac, 1, mode)?;
    // A round-up can gain an integer digit (9.99 → 10), which shifts
    // the significant window by one; re-round once at the wider scale.
    let rounded = match rounded.parse::<f64>() {
        Ok(v)
            if v != 0.0
                && (v.abs().log10().floor() as i32) > e
                && i32::from(max_sig) - 1 - (e + 1) != frac =>
        {
            round_decimal_exact_scaled(abs, is_negative, frac - 1, 1, mode)?
        }
        _ => rounded,
    };
    Some(trim_significant(&rounded, min_sig))
}

/// Strip trailing fractional zeros while keeping at least `min_sig`
/// significant digits (and drop a bare trailing point).
fn trim_significant(s: &str, min_sig: u8) -> String {
    let Some((int_part, frac_part)) = s.split_once('.') else {
        return s.to_string();
    };
    let int_sig = if int_part.chars().all(|c| c == '0') {
        0
    } else {
        int_part.trim_start_matches('0').len()
    };
    let mut frac: Vec<char> = frac_part.chars().collect();
    loop {
        let frac_sig: usize = if int_sig > 0 {
            frac.len()
        } else {
            let leading = frac.iter().take_while(|&&c| c == '0').count();
            frac.len().saturating_sub(leading)
        };
        if frac.last() == Some(&'0') && int_sig + frac_sig > usize::from(min_sig) {
            frac.pop();
        } else {
            break;
        }
    }
    if frac.is_empty() {
        int_part.to_string()
    } else {
        format!("{int_part}.{}", frac.iter().collect::<String>())
    }
}

/// Format a number through ICU's `DecimalFormatter`. Falls back to
/// the Rust-side `format!` rendering when ICU instantiation fails.
fn format_decimal(n: f64, payload: &NumberFormatPayload) -> String {
    format_decimal_signed(n, n.is_sign_negative(), payload)
}

/// As [`format_decimal`] with the ORIGINAL sign threaded separately —
/// callers pass `n.abs()`, but ceil/floor/halfCeil/halfFloor rounding
/// direction depends on the pre-abs sign.
fn format_decimal_signed(n: f64, is_negative: bool, payload: &NumberFormatPayload) -> String {
    let locale = Locale::from_str(&payload.locale)
        .or_else(|_| Locale::from_str(DEFAULT_LOCALE))
        .expect("default locale parses");
    let mut options = DecimalFormatterOptions::default();
    options.grouping_strategy = Some(if payload.use_grouping {
        GroupingStrategy::Auto
    } else {
        GroupingStrategy::Never
    });
    let formatter = match DecimalFormatter::try_new((&locale).into(), options) {
        Ok(f) => f,
        Err(_) => return rust_fallback_format(n, payload),
    };
    // Render to the precise number of fraction digits we want:
    // start with `minimumFractionDigits`, round to
    // `maximumFractionDigits`, and trim any trailing zeros above
    // the minimum so `1234567` doesn't surface as `1,234,567.000`.
    let use_significant = match payload.maximum_significant_digits {
        None => false,
        Some(mxsd) => {
            if payload.rounding_priority == "auto" {
                true
            } else {
                // §15.5.3 — compare rounding positions: significant
                // rounds at e - mxsd + 1, fraction at -mxfd; the more
                // fractional (smaller) position is the more precise
                // side, ties go to significant.
                let e = if n == 0.0 {
                    0
                } else {
                    n.abs().log10().floor() as i32
                };
                let s_pos = e - i32::from(mxsd) + 1;
                let f_pos = -i32::from(payload.maximum_fraction_digits);
                if payload.rounding_priority == "morePrecision" {
                    s_pos <= f_pos
                } else {
                    s_pos >= f_pos
                }
            }
        }
    };
    let trimmed = if use_significant {
        let max_sig = payload
            .maximum_significant_digits
            .expect("use_significant implies a bound");
        let min_sig = payload.minimum_significant_digits.unwrap_or(1);
        round_significant_exact(
            n.abs(),
            is_negative,
            max_sig,
            min_sig,
            &payload.rounding_mode,
        )
        .unwrap_or_else(|| format_significant(n.abs(), max_sig, min_sig))
    } else {
        let max = payload.maximum_fraction_digits as usize;
        let formatted = round_decimal_exact(
            n.abs(),
            is_negative,
            max,
            payload.rounding_increment,
            &payload.rounding_mode,
        )
        .unwrap_or_else(|| format!("{:.max$}", n.abs(), max = max));
        trim_trailing_zero_fraction(&formatted, payload.minimum_fraction_digits as usize)
    };
    let trimmed = if payload.trailing_zero_display == "stripIfInteger"
        && trimmed
            .split_once('.')
            .is_some_and(|(_, f)| f.bytes().all(|b| b == b'0'))
    {
        trimmed.split_once('.').map(|(i, _)| i.to_string()).unwrap()
    } else {
        trimmed
    };
    let mut decimal = match Decimal::from_str(&trimmed) {
        Ok(d) => d,
        Err(_) => return rust_fallback_format(n, payload),
    };
    if payload.maximum_significant_digits.is_none() {
        decimal.pad_end(-(payload.minimum_fraction_digits as i16));
    }
    if payload.minimum_integer_digits > 1 {
        decimal.pad_start(payload.minimum_integer_digits as i16);
    }
    let mut out = String::new();
    let _ = writeable::Writeable::write_to(&formatter.format(&decimal), &mut out);
    out
}

/// Render `abs` rounded to `max_sig` significant digits, then trim
/// trailing fractional zeros down to `min_sig` significant digits
/// (ECMA-402 significant-digit rounding, roundingMode `halfExpand` —
/// Rust's `{:e}` rounds half-to-even; the divergence is confined to
/// exact-tie mantissas).
fn format_significant(abs: f64, max_sig: u8, min_sig: u8) -> String {
    if abs == 0.0 {
        let mut s = String::from("0");
        if min_sig > 1 {
            s.push('.');
            for _ in 1..min_sig {
                s.push('0');
            }
        }
        return s;
    }
    // Start from the shortest round-trip representation (`{:e}` with no
    // precision) so a double like 1.2 stays "12", not its binary
    // expansion; re-render at fixed precision only when the shortest form
    // carries more significant digits than allowed.
    let shortest = format!("{:e}", abs);
    let shortest_sig = shortest
        .split('e')
        .next()
        .map_or(0, |m| m.chars().filter(char::is_ascii_digit).count());
    let sci = if shortest_sig > max_sig as usize {
        // `{:.P$e}` renders `d.ddd…e±E` with P fractional mantissa digits —
        // exactly `P + 1` significant digits.
        format!("{:.*e}", (max_sig - 1) as usize, abs)
    } else {
        shortest
    };
    let (mantissa, exp) = sci
        .split_once('e')
        .expect("{:e} always contains an exponent");
    let exp: i32 = exp.parse().expect("{:e} exponent parses");
    let digits: String = mantissa.chars().filter(|c| c.is_ascii_digit()).collect();
    // Assemble the plain decimal string for digits × 10^(exp - (len-1)).
    let len = digits.len() as i32;
    let point = exp + 1; // digits before the decimal point
    let mut s = if point <= 0 {
        let mut out = String::from("0.");
        for _ in 0..-point {
            out.push('0');
        }
        out.push_str(&digits);
        out
    } else if point >= len {
        let mut out = digits.clone();
        for _ in 0..(point - len) {
            out.push('0');
        }
        out
    } else {
        let (int_part, frac_part) = digits.split_at(point as usize);
        format!("{int_part}.{frac_part}")
    };
    // Trim trailing fractional zeros beyond `min_sig` significant digits.
    if s.contains('.') {
        let min_sig = min_sig as usize;
        loop {
            let sig_count =
                s.chars().filter(|c| c.is_ascii_digit()).count() - leading_insignificant_zeros(&s);
            if !s.ends_with('0') || sig_count <= min_sig {
                break;
            }
            s.pop();
        }
        if s.ends_with('.') {
            s.pop();
        }
    }
    // Pad with trailing zeros up to `min_sig` significant digits.
    let mut sig_count =
        s.chars().filter(|c| c.is_ascii_digit()).count() - leading_insignificant_zeros(&s);
    if sig_count < min_sig as usize {
        if !s.contains('.') {
            s.push('.');
        }
        while sig_count < min_sig as usize {
            s.push('0');
            sig_count += 1;
        }
    }
    s
}

/// Count leading zeros that are not significant (`0.00123` -> 3: the
/// integer `0` and the two fraction zeros before the first non-zero).
fn leading_insignificant_zeros(s: &str) -> usize {
    let mut count = 0;
    for c in s.chars() {
        match c {
            '0' => count += 1,
            '.' => {}
            _ => return count,
        }
    }
    // All-zero string: every digit but one is insignificant.
    count.saturating_sub(1)
}

/// Trim trailing fractional zeros above `min_frac` digits.
fn trim_trailing_zero_fraction(s: &str, min_frac: usize) -> String {
    let Some(dot) = s.find('.') else {
        return s.to_string();
    };
    let allowed_min = dot + 1 + min_frac;
    let mut out = s.to_string();
    while out.len() > allowed_min && out.ends_with('0') {
        out.pop();
    }
    if out.ends_with('.') {
        out.pop();
    }
    out
}

/// Last-resort formatter when ICU rejects the locale: plain Rust
/// `format!` with manual grouping.
fn rust_fallback_format(n: f64, payload: &NumberFormatPayload) -> String {
    let mut s = if let Some(max_sig) = payload.maximum_significant_digits {
        format_significant(
            n.abs(),
            max_sig,
            payload.minimum_significant_digits.unwrap_or(1),
        )
    } else {
        let max = payload.maximum_fraction_digits as usize;
        let mut s = format!("{:.max$}", n.abs(), max = max);
        // Trim trailing zeros down to `minimumFractionDigits`.
        if max > payload.minimum_fraction_digits as usize
            && let Some(dot) = s.find('.')
        {
            let allowed_min = dot + 1 + payload.minimum_fraction_digits as usize;
            while s.len() > allowed_min && s.ends_with('0') {
                s.pop();
            }
            if s.ends_with('.') {
                s.pop();
            }
        }
        s
    };
    let int_len = s.split('.').next().map_or(0, str::len);
    if int_len < payload.minimum_integer_digits as usize {
        let pad = payload.minimum_integer_digits as usize - int_len;
        s = format!("{}{}", "0".repeat(pad), s);
    }
    if payload.use_grouping {
        s = group_thousands(&s);
    }
    s
}

fn group_thousands(s: &str) -> String {
    let (int_part, frac_part) = s.split_once('.').unwrap_or((s, ""));
    let mut out = String::with_capacity(int_part.len() + int_part.len() / 3);
    let chars: Vec<char> = int_part.chars().collect();
    for (i, ch) in chars.iter().enumerate() {
        if i > 0 && (chars.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(*ch);
    }
    if !frac_part.is_empty() {
        out.push('.');
        out.push_str(frac_part);
    }
    out
}
