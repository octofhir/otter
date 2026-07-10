//! `Intl.DurationFormat` — locale-aware duration formatting (ECMA-402
//! §1, Intl DurationFormat proposal).
//!
//! Like `Intl.Locale`, the constructor must fire its `options` getters
//! in spec order, so it is built through a `NativeCtx`-based
//! constructor rather than the heap-only
//! its own `NativeCtx` constructor path.
//!
//! # Contents
//! - [`duration_format_ctor`] — `new Intl.DurationFormat(locales,
//!   options?)` (option resolution + `GetDurationUnitOptions`).
//! - [`resolved_options`] — `Intl.DurationFormat.prototype.resolvedOptions()`.
//!
//! # See also
//! - <https://tc39.es/proposal-intl-duration-format/>

use std::str::FromStr;

use crate::intl::payload::{DurationFormatPayload, IntlPayload, JsIntl, NumberFormatPayload};
use crate::intl::supported::canonicalize_locale_list;
use crate::string::JsString;
use crate::temporal::helpers::get_option_value;
use crate::{NativeCtx, NativeError, Value};

const CLASS: &str = "DurationFormat";

/// The ten duration units in spec order: `(name, stylesList,
/// digitalBase)`. Years..days take only word styles (digital base
/// `"short"`); hours/minutes/seconds additionally accept `numeric` /
/// `2-digit` (digital base `"numeric"` for hours, `"2-digit"` for
/// minutes/seconds); sub-second units accept `numeric` (digital base
/// `"numeric"`).
const UNITS: &[(&str, &[&str], &str)] = &[
    ("years", &["long", "short", "narrow"], "short"),
    ("months", &["long", "short", "narrow"], "short"),
    ("weeks", &["long", "short", "narrow"], "short"),
    ("days", &["long", "short", "narrow"], "short"),
    (
        "hours",
        &["long", "short", "narrow", "numeric", "2-digit"],
        "numeric",
    ),
    (
        "minutes",
        &["long", "short", "narrow", "numeric", "2-digit"],
        "2-digit",
    ),
    (
        "seconds",
        &["long", "short", "narrow", "numeric", "2-digit"],
        "2-digit",
    ),
    (
        "milliseconds",
        &["long", "short", "narrow", "numeric"],
        "numeric",
    ),
    (
        "microseconds",
        &["long", "short", "narrow", "numeric"],
        "numeric",
    ),
    (
        "nanoseconds",
        &["long", "short", "narrow", "numeric"],
        "numeric",
    ),
];

fn range_err(reason: impl Into<String>) -> NativeError {
    NativeError::RangeError {
        name: CLASS,
        reason: reason.into(),
    }
}
fn type_err(reason: impl Into<String>) -> NativeError {
    NativeError::TypeError {
        name: CLASS,
        reason: reason.into(),
    }
}

fn coerce_to_string(ctx: &mut NativeCtx<'_>, value: Value) -> Result<String, NativeError> {
    if value.is_null() {
        return Ok("null".to_string());
    }
    if value.is_undefined() {
        return Ok("undefined".to_string());
    }
    if let Some(s) = value.as_string(ctx.heap()) {
        return Ok(s.to_lossy_string(ctx.heap()));
    }
    if let Some(b) = value.as_boolean() {
        return Ok((if b { "true" } else { "false" }).to_string());
    }
    if let Some(n) = value.as_number() {
        return Ok(n.to_display_string());
    }
    if value.is_object_type() {
        let exec = ctx
            .execution_context()
            .cloned()
            .ok_or_else(|| type_err("missing execution context"))?;
        let prim = ctx.cx.interp.to_primitive_string_hint_sync(&exec, value);
        let prim =
            prim.map_err(|e| crate::native_function::vm_to_native_error(ctx.cx.interp, e, CLASS))?;
        if let Some(s) = prim.as_string(ctx.heap()) {
            return Ok(s.to_lossy_string(ctx.heap()));
        }
        if let Some(n) = prim.as_number() {
            return Ok(n.to_display_string());
        }
    }
    Err(type_err("option value cannot be converted to a string"))
}

/// `[[Get]]` on the options bag, treating an absent (undefined) bag —
/// per `GetOptionsObject` — as an empty object whose reads all yield
/// `undefined`.
fn opt_get(ctx: &mut NativeCtx<'_>, options: Value, name: &str) -> Result<Value, NativeError> {
    if options.is_undefined() {
        return Ok(Value::undefined());
    }
    get_option_value(ctx, options, name, CLASS)
}

/// `GetOption(options, name, "string", allowed, undefined)`.
fn get_enum_option(
    ctx: &mut NativeCtx<'_>,
    options: Value,
    name: &str,
    allowed: &[&str],
) -> Result<Option<String>, NativeError> {
    let v = opt_get(ctx, options, name)?;
    if v.is_undefined() {
        return Ok(None);
    }
    let s = coerce_to_string(ctx, v)?;
    if !allowed.contains(&s.as_str()) {
        return Err(range_err(format!("invalid {name} option value")));
    }
    Ok(Some(s))
}

fn is_alnum_type(s: &str) -> bool {
    !s.is_empty()
        && s.split('-')
            .all(|p| (3..=8).contains(&p.len()) && p.bytes().all(|b| b.is_ascii_alphanumeric()))
}

fn prev_in_numeric_chain(prev: &Option<String>) -> bool {
    matches!(
        prev.as_deref(),
        Some("fractional") | Some("numeric") | Some("2-digit")
    )
}

pub(crate) fn duration_format_ctor(
    ctx: &mut NativeCtx<'_>,
    args: &[Value],
) -> Result<Value, NativeError> {
    if !ctx.is_construct_call() {
        return Err(type_err("constructor Intl.DurationFormat requires 'new'"));
    }

    let locales = args.first().copied().unwrap_or_else(Value::undefined);
    let options_arg = args.get(1).copied().unwrap_or_else(Value::undefined);
    if !options_arg.is_undefined() && !options_arg.is_object_type() {
        return Err(type_err("options must be an object"));
    }

    let requested = canonicalize_locale_list(ctx, locales)?;
    let locale = requested
        .into_iter()
        .next()
        .unwrap_or_else(|| "en-US".to_string());

    // §1.1.2 options reads, in spec order.
    let _locale_matcher =
        get_enum_option(ctx, options_arg, "localeMatcher", &["lookup", "best fit"])?;
    let numbering_system = match opt_get(ctx, options_arg, "numberingSystem")? {
        v if v.is_undefined() => None,
        v => {
            let s = coerce_to_string(ctx, v)?;
            if !is_alnum_type(&s) {
                return Err(range_err("invalid numberingSystem option"));
            }
            Some(s)
        }
    };
    let style = get_enum_option(
        ctx,
        options_arg,
        "style",
        &["long", "short", "narrow", "digital"],
    )?
    .unwrap_or_else(|| "short".to_string());

    let mut units: Vec<(String, String)> = Vec::with_capacity(UNITS.len());
    // `prev_internal` carries the previous unit's *internal* resolved
    // style, which may be `"fractional"` (sub-second numeric).
    let mut prev_internal: Option<String> = None;
    for (unit, styles, digital_base) in UNITS {
        let unit = *unit;
        let is_hms = matches!(unit, "hours" | "minutes" | "seconds");
        let is_min_sec = matches!(unit, "minutes" | "seconds");
        let is_subsec = matches!(unit, "milliseconds" | "microseconds" | "nanoseconds");

        let unit_style = get_enum_option(ctx, options_arg, unit, styles)?;
        let mut display_default = "always";
        let mut internal = match unit_style {
            Some(s) => s,
            None => {
                if style == "digital" {
                    if !is_hms {
                        display_default = "auto";
                    }
                    (*digital_base).to_string()
                } else {
                    display_default = "auto";
                    if prev_in_numeric_chain(&prev_internal) {
                        if is_min_sec {
                            "2-digit".to_string()
                        } else {
                            "numeric".to_string()
                        }
                    } else {
                        style.clone()
                    }
                }
            }
        };

        // §sub-second numeric → internal "fractional".
        if internal == "numeric" && is_subsec {
            internal = "fractional".to_string();
            display_default = "auto";
        }

        let display = get_enum_option(
            ctx,
            options_arg,
            &format!("{unit}Display"),
            &["auto", "always"],
        )?
        .unwrap_or_else(|| display_default.to_string());

        if display == "always" && internal == "fractional" {
            return Err(range_err(format!(
                "{unit} with fractional style cannot use display \"always\""
            )));
        }

        // §conflict: a unit following a numeric / 2-digit / fractional
        // unit must itself be fractional / numeric / 2-digit.
        if prev_in_numeric_chain(&prev_internal)
            && !matches!(internal.as_str(), "fractional" | "numeric" | "2-digit")
        {
            return Err(range_err(format!(
                "{unit} style conflicts with the preceding numeric unit"
            )));
        }

        let reported = if internal == "fractional" {
            "numeric".to_string()
        } else {
            internal.clone()
        };
        units.push((reported, display));
        prev_internal = Some(internal);
    }

    let fractional_digits = get_fractional_digits(ctx, options_arg)?;

    let payload = IntlPayload::DurationFormat(DurationFormatPayload {
        locale,
        numbering_system: numbering_system.unwrap_or_else(|| "latn".to_string()),
        style,
        units,
        fractional_digits,
    });
    let intl = JsIntl::new(ctx.heap_mut(), payload).map_err(|_| type_err("out of memory"))?;
    Ok(Value::intl(intl))
}

/// `GetNumberOption(options, "fractionalDigits", 0, 9, undefined)`.
fn get_fractional_digits(
    ctx: &mut NativeCtx<'_>,
    options: Value,
) -> Result<Option<u8>, NativeError> {
    let v = opt_get(ctx, options, "fractionalDigits")?;
    if v.is_undefined() {
        return Ok(None);
    }
    let n = crate::number::to_number_value(&v, ctx.heap());
    if n.is_nan() || !(0.0..=9.0).contains(&n) {
        return Err(range_err("fractionalDigits must be between 0 and 9"));
    }
    Ok(Some(n.floor() as u8))
}

/// Alphabetical field-read order for `ToDurationRecord` on a plain
/// object (matches the Temporal partial-duration read order).
const FIELD_READ_ORDER: &[(&str, usize)] = &[
    ("days", 3),
    ("hours", 4),
    ("microseconds", 8),
    ("milliseconds", 7),
    ("minutes", 5),
    ("months", 1),
    ("nanoseconds", 9),
    ("seconds", 6),
    ("weeks", 2),
    ("years", 0),
];

/// `ToDurationRecord(input)` → the ten unit amounts in spec
/// (years..nanoseconds) order.
fn to_duration_record(ctx: &mut NativeCtx<'_>, arg: Value) -> Result<[f64; 10], NativeError> {
    use crate::temporal::payload::TemporalPayload;

    // Temporal.Duration instance.
    if let Some(t) = arg.as_temporal(ctx.heap())
        && let TemporalPayload::Duration(d) = t.payload_clone(ctx.heap())
    {
        return Ok([
            d.years() as f64,
            d.months() as f64,
            d.weeks() as f64,
            d.days() as f64,
            d.hours() as f64,
            d.minutes() as f64,
            d.seconds() as f64,
            d.milliseconds() as f64,
            d.microseconds() as f64,
            d.nanoseconds() as f64,
        ]);
    }

    // ISO-8601 duration string.
    if let Some(s) = arg.as_string(ctx.heap()) {
        let text = s.to_lossy_string(ctx.heap());
        let d = temporal_rs::Duration::from_str(&text)
            .map_err(|_| range_err("invalid ISO-8601 duration string"))?;
        return Ok([
            d.years() as f64,
            d.months() as f64,
            d.weeks() as f64,
            d.days() as f64,
            d.hours() as f64,
            d.minutes() as f64,
            d.seconds() as f64,
            d.milliseconds() as f64,
            d.microseconds() as f64,
            d.nanoseconds() as f64,
        ]);
    }

    if !arg.is_object_type() {
        return Err(type_err("duration argument must be an object or string"));
    }

    let mut record = [0.0f64; 10];
    let mut any = false;
    for (field, idx) in FIELD_READ_ORDER {
        let v = get_option_value(ctx, arg, field, CLASS)?;
        if v.is_undefined() {
            continue;
        }
        any = true;
        record[*idx] = crate::temporal::helpers::to_integer_if_integral(ctx, &v, CLASS, field)?;
    }
    if !any {
        return Err(type_err("duration object has no recognized unit fields"));
    }
    // §IsValidDurationRecord — every non-zero field must share one sign.
    let mut sign = 0i32;
    for &amt in &record {
        if amt > 0.0 {
            if sign < 0 {
                return Err(range_err("duration fields have inconsistent signs"));
            }
            sign = 1;
        } else if amt < 0.0 {
            if sign > 0 {
                return Err(range_err("duration fields have inconsistent signs"));
            }
            sign = -1;
        }
    }
    // §IsValidDuration — calendar units are bounded by 2^32 and the
    // normalized day-plus-time total by 2^53 seconds.
    for &(idx, name) in &[(0usize, "years"), (1, "months"), (2, "weeks")] {
        if record[idx].abs() >= 4_294_967_296.0 {
            return Err(range_err(format!("{name} out of range")));
        }
    }
    let total_seconds = record[3].abs() * 86_400.0
        + record[4].abs() * 3_600.0
        + record[5].abs() * 60.0
        + record[6].abs()
        + record[7].abs() / 1e3
        + record[8].abs() / 1e6
        + record[9].abs() / 1e9;
    if total_seconds >= 9_007_199_254_740_992.0 {
        return Err(range_err("duration time total out of range"));
    }
    Ok(record)
}

/// Combine seconds + sub-seconds into a single fractional value (spec
/// `ToFractionalValue`), mirroring the test262 reference helper.
fn fractional_value(d: &[f64; 10], exponent: i32) -> f64 {
    let (sec, ms, us, ns) = (d[6], d[7], d[8], d[9]);
    match exponent {
        9 if ms == 0.0 && us == 0.0 && ns == 0.0 => return sec,
        6 if us == 0.0 && ns == 0.0 => return ms,
        3 if ns == 0.0 => return us,
        _ => {}
    }
    let mut total_ns = ns;
    match exponent {
        9 => total_ns += sec * 1e9 + ms * 1e6 + us * 1e3,
        6 => total_ns += ms * 1e6 + us * 1e3,
        _ => total_ns += us * 1e3,
    }
    total_ns / 10f64.powi(exponent)
}

/// Render each displayed unit, grouping consecutive numeric units into
/// one `":"`-joined element. Returns the list of element strings.
fn partition(payload: &DurationFormatPayload, d: &[f64; 10]) -> Vec<String> {
    let unit_names = [
        "years",
        "months",
        "weeks",
        "days",
        "hours",
        "minutes",
        "seconds",
        "milliseconds",
        "microseconds",
        "nanoseconds",
    ];
    let mut result: Vec<String> = Vec::new();
    let mut need_separator = false;
    let mut display_negative_sign = true;
    let any_negative = d.iter().any(|&x| x < 0.0);

    for i in 0..10 {
        let mut value = d[i];
        let style = payload.units[i].0.as_str();
        let display = payload.units[i].1.as_str();
        let is_numeric = style == "numeric" || style == "2-digit";

        // Seconds / milli / micro fold into a fraction when the next
        // unit is numeric.
        let mut fractional = false;
        if (6..=8).contains(&i) && payload.units[i + 1].0 == "numeric" {
            value = fractional_value(d, [9, 6, 3][i - 6]);
            fractional = true;
        }

        let mut display_required = false;
        if unit_names[i] == "minutes" && need_separator {
            display_required = payload.units[6].1 == "always"
                || d[6] != 0.0
                || d[7] != 0.0
                || d[8] != 0.0
                || d[9] != 0.0;
        }

        if value != 0.0 || display != "auto" || display_required {
            if display_negative_sign {
                display_negative_sign = false;
                if value == 0.0 && any_negative {
                    value = -0.0;
                }
            } else {
                // §1.1.9 PartitionDurationFormatPattern — only the first
                // displayed component carries the sign; later components
                // render their absolute value.
                value = value.abs();
            }

            let (min_frac, max_frac) = if fractional {
                (
                    payload.fractional_digits.unwrap_or(0),
                    payload.fractional_digits.unwrap_or(9),
                )
            } else {
                (0, 3)
            };
            // `long`/`short`/`narrow` styles render the value with its
            // unit label through the NumberFormat `unit` style; the
            // `numeric`/`2-digit` digital styles render a bare decimal.
            const SINGULAR: [&str; 10] = [
                "year",
                "month",
                "week",
                "day",
                "hour",
                "minute",
                "second",
                "millisecond",
                "microsecond",
                "nanosecond",
            ];
            let np = if is_numeric {
                NumberFormatPayload {
                    locale: payload.locale.clone(),
                    numbering_system: "latn".to_string(),
                    style: "decimal".to_string(),
                    currency: None,
                    // Digital "2-digit" pads to two digits ("0:00:01"),
                    // and any unit appended after a ":" separator pads
                    // likewise ("1 hr, 2:03").
                    minimum_integer_digits: if style == "2-digit" || need_separator {
                        2
                    } else {
                        1
                    },
                    minimum_significant_digits: None,
                    maximum_significant_digits: None,
                    minimum_fraction_digits: min_frac,
                    maximum_fraction_digits: max_frac,
                    use_grouping: false,
                    sign_display: "auto".to_string(),
                    notation: "standard".to_string(),
                    currency_display: "symbol".to_string(),
                    currency_sign: "standard".to_string(),
                    unit: None,
                    unit_display: "short".to_string(),
                    compact_display: "short".to_string(),
                    rounding_mode: "halfExpand".to_string(),
                    rounding_increment: 1,
                    trailing_zero_display: "auto".to_string(),
                    rounding_priority: "auto".to_string(),
                }
            } else {
                NumberFormatPayload {
                    locale: payload.locale.clone(),
                    numbering_system: "latn".to_string(),
                    style: "unit".to_string(),
                    currency: None,
                    minimum_integer_digits: 1,
                    minimum_significant_digits: None,
                    maximum_significant_digits: None,
                    minimum_fraction_digits: min_frac,
                    maximum_fraction_digits: max_frac,
                    use_grouping: true,
                    sign_display: "auto".to_string(),
                    notation: "standard".to_string(),
                    currency_display: "symbol".to_string(),
                    currency_sign: "standard".to_string(),
                    unit: Some(SINGULAR[i].to_string()),
                    unit_display: style.to_string(),
                    compact_display: "short".to_string(),
                    rounding_mode: "halfExpand".to_string(),
                    rounding_increment: 1,
                    trailing_zero_display: "auto".to_string(),
                    rounding_priority: "auto".to_string(),
                }
            };
            let rendered = crate::intl::number_format::format_number(value, &np);

            if need_separator {
                if let Some(last) = result.last_mut() {
                    last.push(':');
                    last.push_str(&rendered);
                }
            } else {
                result.push(rendered);
                if is_numeric {
                    need_separator = true;
                }
            }
        }

        if fractional {
            break;
        }
    }
    result
}

pub(crate) fn format(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let payload = require_payload(ctx)?;
    let arg = args.first().copied().unwrap_or_else(Value::undefined);
    let record = to_duration_record(ctx, arg)?;
    let elements = partition(&payload, &record);

    let list_style = if payload.style == "digital" {
        "short".to_string()
    } else {
        payload.style.clone()
    };
    let lf = crate::intl::payload::ListFormatPayload {
        locale: payload.locale.clone(),
        kind: "unit".to_string(),
        style: list_style,
    };
    let rendered = crate::intl::list_format::join(&elements, &lf);
    Ok(Value::string(JsString::from_str(
        &rendered,
        ctx.heap_mut(),
    )?))
}

/// One emitted `formatToParts` entry: `{ type, value }` plus an
/// optional `unit` tag for number parts.
struct DurationPart {
    ty: &'static str,
    value: String,
    unit: Option<&'static str>,
}

const UNIT_SINGULAR: [&str; 10] = [
    "year",
    "month",
    "week",
    "day",
    "hour",
    "minute",
    "second",
    "millisecond",
    "microsecond",
    "nanosecond",
];

/// Build the per-unit part groups (mirrors [`partition`] but keeps each
/// rendered piece's `{type, value}` components tagged with their unit).
fn partition_parts(payload: &DurationFormatPayload, d: &[f64; 10]) -> Vec<Vec<DurationPart>> {
    let mut result: Vec<Vec<DurationPart>> = Vec::new();
    let mut need_separator = false;
    let mut display_negative_sign = true;
    let any_negative = d.iter().any(|&x| x < 0.0);

    for i in 0..10 {
        let mut value = d[i];
        let style = payload.units[i].0.as_str();
        let display = payload.units[i].1.as_str();
        let is_numeric = style == "numeric" || style == "2-digit";

        let mut fractional = false;
        if (6..=8).contains(&i) && payload.units[i + 1].0 == "numeric" {
            value = fractional_value(d, [9, 6, 3][i - 6]);
            fractional = true;
        }

        let mut display_required = false;
        if i == 5 && need_separator {
            display_required = payload.units[6].1 == "always"
                || d[6] != 0.0
                || d[7] != 0.0
                || d[8] != 0.0
                || d[9] != 0.0;
        }

        if value != 0.0 || display != "auto" || display_required {
            if display_negative_sign {
                display_negative_sign = false;
                if value == 0.0 && any_negative {
                    value = -0.0;
                }
            } else {
                // §1.1.9 PartitionDurationFormatPattern — only the first
                // displayed component carries the sign; later components
                // render their absolute value.
                value = value.abs();
            }
            let (min_frac, max_frac) = if fractional {
                (
                    payload.fractional_digits.unwrap_or(0),
                    payload.fractional_digits.unwrap_or(9),
                )
            } else {
                (0, 3)
            };
            let unit = UNIT_SINGULAR[i];
            let np = if is_numeric {
                NumberFormatPayload {
                    locale: payload.locale.clone(),
                    numbering_system: "latn".to_string(),
                    style: "decimal".to_string(),
                    currency: None,
                    // Digital "2-digit" pads to two digits ("0:00:01"),
                    // and any unit appended after a ":" separator pads
                    // likewise ("1 hr, 2:03").
                    minimum_integer_digits: if style == "2-digit" || need_separator {
                        2
                    } else {
                        1
                    },
                    minimum_significant_digits: None,
                    maximum_significant_digits: None,
                    minimum_fraction_digits: min_frac,
                    maximum_fraction_digits: max_frac,
                    use_grouping: false,
                    sign_display: "auto".to_string(),
                    notation: "standard".to_string(),
                    currency_display: "symbol".to_string(),
                    currency_sign: "standard".to_string(),
                    unit: None,
                    unit_display: "short".to_string(),
                    compact_display: "short".to_string(),
                    rounding_mode: "halfExpand".to_string(),
                    rounding_increment: 1,
                    trailing_zero_display: "auto".to_string(),
                    rounding_priority: "auto".to_string(),
                }
            } else {
                NumberFormatPayload {
                    locale: payload.locale.clone(),
                    numbering_system: "latn".to_string(),
                    style: "unit".to_string(),
                    currency: None,
                    minimum_integer_digits: 1,
                    minimum_significant_digits: None,
                    maximum_significant_digits: None,
                    minimum_fraction_digits: min_frac,
                    maximum_fraction_digits: max_frac,
                    use_grouping: true,
                    sign_display: "auto".to_string(),
                    notation: "standard".to_string(),
                    currency_display: "symbol".to_string(),
                    currency_sign: "standard".to_string(),
                    unit: Some(unit.to_string()),
                    unit_display: style.to_string(),
                    compact_display: "short".to_string(),
                    rounding_mode: "halfExpand".to_string(),
                    rounding_increment: 1,
                    trailing_zero_display: "auto".to_string(),
                    rounding_priority: "auto".to_string(),
                }
            };
            let number_parts = crate::intl::number_format::partition_number(value, &np);
            let tagged = number_parts.into_iter().map(|(ty, value)| DurationPart {
                ty,
                value,
                unit: Some(unit),
            });

            if need_separator {
                if let Some(last) = result.last_mut() {
                    last.push(DurationPart {
                        ty: "literal",
                        value: ":".to_string(),
                        unit: None,
                    });
                    last.extend(tagged);
                }
            } else {
                result.push(tagged.collect());
                if is_numeric {
                    need_separator = true;
                }
            }
        }

        if fractional {
            break;
        }
    }
    result
}

pub(crate) fn format_to_parts(
    ctx: &mut NativeCtx<'_>,
    args: &[Value],
) -> Result<Value, NativeError> {
    let payload = require_payload(ctx)?;
    let arg = args.first().copied().unwrap_or_else(Value::undefined);
    let record = to_duration_record(ctx, arg)?;
    let groups = partition_parts(&payload, &record);

    // Join the element groups with the CLDR `unit` list separator —
    // narrow joins with a bare space, everything else with ", ".
    let separator = if payload.style == "narrow" { " " } else { ", " };
    let mut flat: Vec<DurationPart> = Vec::new();
    for (gi, group) in groups.into_iter().enumerate() {
        if gi > 0 {
            flat.push(DurationPart {
                ty: "literal",
                value: separator.to_string(),
                unit: None,
            });
        }
        flat.extend(group);
    }

    let mut elements: Vec<Value> = Vec::with_capacity(flat.len());
    for part in &flat {
        let ty_s = Value::string(JsString::from_str(part.ty, ctx.heap_mut())?);
        let val_s = Value::string(JsString::from_str(&part.value, ctx.heap_mut())?);
        let unit_v = match part.unit {
            Some(u) => Some(Value::string(JsString::from_str(u, ctx.heap_mut())?)),
            None => None,
        };
        let snapshot = elements.clone();
        let mut obj = ctx.alloc_object_with_roots(&[&ty_s, &val_s], &[&snapshot])?;
        crate::object::set(&mut obj, ctx.heap_mut(), "type", ty_s);
        crate::object::set(&mut obj, ctx.heap_mut(), "value", val_s);
        if let Some(u) = unit_v {
            crate::object::set(&mut obj, ctx.heap_mut(), "unit", u);
        }
        elements.push(Value::object(obj));
    }
    let element_roots = elements.clone();
    let mut visit = |visitor: &mut dyn FnMut(*mut otter_gc::raw::RawGc)| {
        for v in &element_roots {
            v.trace_value_slots(visitor);
        }
    };
    let arr = crate::array::from_elements_with_roots(ctx.heap_mut(), elements, &mut visit)?;
    Ok(Value::array(arr))
}

fn require_payload(ctx: &NativeCtx<'_>) -> Result<DurationFormatPayload, NativeError> {
    let bad = || type_err("intrinsic called on a non-Intl.DurationFormat receiver");
    let intl = ctx.this_value().as_intl(ctx.heap()).ok_or_else(bad)?;
    match intl.payload_clone(ctx.heap()) {
        IntlPayload::DurationFormat(p) => Ok(p),
        _ => Err(bad()),
    }
}

pub(crate) fn resolved_options(
    ctx: &mut NativeCtx<'_>,
    _args: &[Value],
) -> Result<Value, NativeError> {
    let p = require_payload(ctx)?;
    let locale = Value::string(JsString::from_str(&p.locale, ctx.heap_mut())?);
    let numbering = Value::string(JsString::from_str(&p.numbering_system, ctx.heap_mut())?);
    let style = Value::string(JsString::from_str(&p.style, ctx.heap_mut())?);
    let mut obj = ctx.alloc_object_with_roots(&[&locale, &numbering, &style], &[])?;
    let proto = ctx.cx.interp.object_prototype_object_opt();
    if let Some(proto) = proto {
        crate::object::set_prototype(obj, ctx.heap_mut(), Some(proto));
    }
    crate::object::set(&mut obj, ctx.heap_mut(), "locale", locale);
    crate::object::set(&mut obj, ctx.heap_mut(), "numberingSystem", numbering);
    crate::object::set(&mut obj, ctx.heap_mut(), "style", style);
    for ((unit, _, _), (unit_style, display)) in UNITS.iter().zip(p.units.iter()) {
        let sv = Value::string(JsString::from_str(unit_style, ctx.heap_mut())?);
        crate::object::set(&mut obj, ctx.heap_mut(), unit, sv);
        let dv = Value::string(JsString::from_str(display, ctx.heap_mut())?);
        let display_key = format!("{unit}Display");
        crate::object::set(&mut obj, ctx.heap_mut(), &display_key, dv);
    }
    if let Some(fd) = p.fractional_digits {
        crate::object::set(
            &mut obj,
            ctx.heap_mut(),
            "fractionalDigits",
            Value::number_i32(fd as i32),
        );
    }
    Ok(Value::object(obj))
}
