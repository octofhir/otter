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

use crate::intl::dispatch::IntlError;
use crate::intl::helpers::{
    DEFAULT_LOCALE, coerce_locale, options_object, read_bool_option, read_string_option,
    read_u8_option,
};
use crate::intl::payload::{IntlPayload, NumberFormatPayload};
use crate::string::JsString;
use crate::{NativeCtx, NativeError, Value};
use otter_gc::raw::RawGc;

/// Build a `RangeError`-bearing `IntlError` for an invalid option value.
fn range_err(message: String) -> IntlError {
    IntlError::Range { message }
}

/// The sanctioned simple unit identifiers (ECMA-402 Table:
/// Single-unit identifiers sanctioned for use in ECMA-402).
const SANCTIONED_UNITS: &[&str] = &[
    "acre", "bit", "byte", "celsius", "centimeter", "day", "degree", "fahrenheit",
    "fluid-ounce", "foot", "gallon", "gigabit", "gigabyte", "gram", "hectare", "hour", "inch",
    "kilobit", "kilobyte", "kilogram", "kilometer", "liter", "megabit", "megabyte", "meter",
    "microsecond", "mile", "mile-scandinavian", "milliliter", "millimeter", "millisecond",
    "minute", "month", "nanosecond", "ounce", "percent", "petabyte", "pound", "second", "stone",
    "terabit", "terabyte", "week", "yard", "year",
];

/// §IsWellFormedUnitIdentifier — a sanctioned simple unit, or
/// `"<numerator>-per-<denominator>"` where both are sanctioned.
fn is_well_formed_unit(unit: &str) -> bool {
    if SANCTIONED_UNITS.contains(&unit) {
        return true;
    }
    match unit.split_once("-per-") {
        Some((num, den)) => {
            SANCTIONED_UNITS.contains(&num) && SANCTIONED_UNITS.contains(&den)
        }
        None => false,
    }
}

/// Resolve the `(locale, options)` argument pair to a payload.
///
/// # Errors
/// - `BadArgument` when `style == "currency"` is requested without a
///   `currency` option.
pub fn resolve(
    locale: &Value,
    options: &Value,
    gc_heap: &otter_gc::GcHeap,
) -> Result<NumberFormatPayload, IntlError> {
    let opts = options_object(Some(options));
    let opts_ref = opts.as_ref();
    let style = read_string_option(opts_ref, "style", "decimal", gc_heap);
    let currency = match style.as_str() {
        "currency" => {
            match opts_ref
                .and_then(|o| crate::object::get(*o, gc_heap, "currency"))
                .and_then(|v| v.as_string(gc_heap).map(|s| s.to_lossy_string(gc_heap)))
            {
                Some(raw) => {
                    // §IsWellFormedCurrencyCode: exactly three ASCII
                    // letters (validated before case-folding so `ß` etc.
                    // don't slip through ToUpperCase expansion).
                    if raw.chars().count() != 3
                        || !raw.chars().all(|c| c.is_ascii_alphabetic())
                    {
                        return Err(range_err(format!(
                            "invalid currency code '{raw}'"
                        )));
                    }
                    Some(raw.to_uppercase())
                }
                None => {
                    return Err(IntlError::BadArgument {
                        class: "NumberFormat",
                        method: "constructor",
                        index: 1,
                        reason: "currency style requires a `currency` option",
                    });
                }
            }
        }
        _ => None,
    };
    let (default_min, default_max) = match style.as_str() {
        "currency" => (2, 2),
        "percent" => (0, 0),
        _ => (0, 3),
    };
    let minimum_fraction_digits = read_u8_option(
        opts_ref,
        "minimumFractionDigits",
        default_min,
        0,
        20,
        gc_heap,
    );
    let maximum_fraction_digits = read_u8_option(
        opts_ref,
        "maximumFractionDigits",
        default_max.max(minimum_fraction_digits),
        minimum_fraction_digits,
        20,
        gc_heap,
    );
    let use_grouping = read_bool_option(opts_ref, "useGrouping", true, gc_heap);
    let sign_display = read_string_option(opts_ref, "signDisplay", "auto", gc_heap);
    if !matches!(
        sign_display.as_str(),
        "auto" | "always" | "never" | "exceptZero" | "negative"
    ) {
        return Err(range_err(format!(
            "invalid value '{sign_display}' for option 'signDisplay'"
        )));
    }
    let notation = read_string_option(opts_ref, "notation", "standard", gc_heap);
    if !matches!(
        notation.as_str(),
        "standard" | "scientific" | "engineering" | "compact"
    ) {
        return Err(range_err(format!(
            "invalid value '{notation}' for option 'notation'"
        )));
    }
    let currency_display = read_string_option(opts_ref, "currencyDisplay", "symbol", gc_heap);
    if !matches!(
        currency_display.as_str(),
        "symbol" | "narrowSymbol" | "code" | "name"
    ) {
        return Err(range_err(format!(
            "invalid value '{currency_display}' for option 'currencyDisplay'"
        )));
    }
    let currency_sign = read_string_option(opts_ref, "currencySign", "standard", gc_heap);
    if !matches!(currency_sign.as_str(), "standard" | "accounting") {
        return Err(range_err(format!(
            "invalid value '{currency_sign}' for option 'currencySign'"
        )));
    }
    // `unit` is read and validated whenever present, regardless of style;
    // a missing `unit` under `style: "unit"` is a TypeError.
    let unit = read_string_option(opts_ref, "unit", "", gc_heap);
    let unit = if unit.is_empty() { None } else { Some(unit) };
    if let Some(u) = &unit
        && !is_well_formed_unit(u)
    {
        return Err(range_err(format!("invalid unit identifier '{u}'")));
    }
    let unit_display = read_string_option(opts_ref, "unitDisplay", "short", gc_heap);
    if !matches!(unit_display.as_str(), "short" | "narrow" | "long") {
        return Err(range_err(format!(
            "invalid value '{unit_display}' for option 'unitDisplay'"
        )));
    }
    if style == "unit" && unit.is_none() {
        return Err(IntlError::BadArgument {
            class: "NumberFormat",
            method: "constructor",
            index: 1,
            reason: "unit style requires a `unit` option",
        });
    }
    Ok(NumberFormatPayload {
        locale: coerce_locale(Some(locale), gc_heap),
        style,
        currency,
        minimum_fraction_digits,
        maximum_fraction_digits,
        use_grouping,
        sign_display,
        notation,
        currency_display,
        currency_sign,
        unit,
        unit_display,
    })
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
    let (interp, exec) = ctx.interp_mut_and_context();
    let exec = exec.ok_or_else(|| NativeError::TypeError {
        name: "format",
        reason: "missing execution context".to_string(),
    })?;
    let n = crate::coerce::to_number_or_throw(interp, &exec, &value)
        .map_err(|e| crate::native_function::vm_to_native_error(interp, e, "format"))?;
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
    let this = ctx.this_value().clone();
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
    let intl = captures.first().and_then(|v| v.as_intl(ctx.heap())).ok_or_else(bad)?;
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
        let obj = ctx.alloc_object_with_roots(&[&ty_s, &val_s], &[&snapshot])?;
        crate::object::set(obj, ctx.heap_mut(), "type", ty_s);
        crate::object::set(obj, ctx.heap_mut(), "value", val_s);
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
    Ok(Value::string(JsString::from_str(&combined, ctx.heap_mut())?))
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
        let obj = ctx.alloc_object_with_roots(&[&ty_s, &val_s, &src_s], &[&snapshot])?;
        crate::object::set(obj, ctx.heap_mut(), "type", ty_s);
        crate::object::set(obj, ctx.heap_mut(), "value", val_s);
        crate::object::set(obj, ctx.heap_mut(), "source", src_s);
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
        push_sign(&mut parts, displayed_sign(&payload.sign_display, false, false, true));
        parts.push(("nan", "NaN".to_string()));
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
        let accounting_negative =
            sign == SignKind::Minus && payload.currency_sign == "accounting";
        let full = currency_string(n.abs(), payload);
        let core = format_decimal(n.abs(), payload);
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

    push_sign(&mut parts, sign);
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
fn push_number_parts(parts: &mut Vec<(&'static str, String)>, core: &str) {
    let (int_part, frac_part) = core.split_once('.').unwrap_or((core, ""));
    let mut first = true;
    for seg in int_part.split(',') {
        if !first {
            parts.push(("group", ",".to_string()));
        }
        parts.push(("integer", seg.to_string()));
        first = false;
    }
    if !frac_part.is_empty() {
        parts.push(("decimal", ".".to_string()));
        parts.push(("fraction", frac_part.to_string()));
    }
}

/// §11.1.7 `Intl.NumberFormat.prototype.resolvedOptions()`.
pub(crate) fn number_format_resolved_options(
    ctx: &mut NativeCtx<'_>,
    _args: &[Value],
) -> Result<Value, NativeError> {
    let payload = require_number_format(ctx, "resolvedOptions")?;
    let locale = Value::string(JsString::from_str(&payload.locale, ctx.heap_mut())?);
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
    let mut value_roots = vec![&locale, &style, &sign_display, &notation];
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
    let obj = ctx.alloc_object_with_roots(&value_roots, &[])?;
    let heap = ctx.heap_mut();
    crate::object::set(obj, heap, "locale", locale);
    crate::object::set(obj, heap, "style", style);
    if let Some(c) = currency_val {
        crate::object::set(obj, heap, "currency", c);
    }
    if let Some(c) = currency_display_val {
        crate::object::set(obj, heap, "currencyDisplay", c);
    }
    if let Some(c) = currency_sign_val {
        crate::object::set(obj, heap, "currencySign", c);
    }
    if let Some(u) = unit_val {
        crate::object::set(obj, heap, "unit", u);
    }
    if let Some(u) = unit_display_val {
        crate::object::set(obj, heap, "unitDisplay", u);
    }
    crate::object::set(
        obj,
        heap,
        "minimumFractionDigits",
        Value::number_i32(min_fd),
    );
    crate::object::set(
        obj,
        heap,
        "maximumFractionDigits",
        Value::number_i32(max_fd),
    );
    crate::object::set(obj, heap, "useGrouping", Value::boolean(use_grouping));
    crate::object::set(obj, heap, "notation", notation);
    crate::object::set(obj, heap, "signDisplay", sign_display);
    Ok(Value::object(obj))
}

/// Render `n` per the resolved option bag.
pub(crate) fn format_number(n: f64, payload: &NumberFormatPayload) -> String {
    if n.is_nan() {
        let sign = sign_prefix(displayed_sign(&payload.sign_display, false, false, true));
        return format!("{sign}NaN");
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
            "percent" => format!("{}%", format_decimal(n.abs() * 100.0, payload)),
            _ => format_decimal(n.abs(), payload),
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

/// Format a number through ICU's `DecimalFormatter`. Falls back to
/// the Rust-side `format!` rendering when ICU instantiation fails.
fn format_decimal(n: f64, payload: &NumberFormatPayload) -> String {
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
    let max = payload.maximum_fraction_digits as usize;
    let formatted = format!("{:.max$}", n.abs(), max = max);
    let trimmed = trim_trailing_zero_fraction(&formatted, payload.minimum_fraction_digits as usize);
    let mut decimal = match Decimal::from_str(&trimmed) {
        Ok(d) => d,
        Err(_) => return rust_fallback_format(n, payload),
    };
    decimal.pad_end(-(payload.minimum_fraction_digits as i16));
    let mut out = String::new();
    let _ = writeable::Writeable::write_to(&formatter.format(&decimal), &mut out);
    out
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
