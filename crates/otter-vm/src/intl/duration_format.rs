//! `Intl.DurationFormat` — locale-aware duration formatting (ECMA-402
//! §1, Intl DurationFormat proposal).
//!
//! Like `Intl.Locale`, the constructor must fire its `options` getters
//! in spec order, so it is built through a `NativeCtx`-based
//! constructor rather than the heap-only
//! [`crate::intl::dispatch::construct`] path.
//!
//! # Contents
//! - [`duration_format_ctor`] — `new Intl.DurationFormat(locales,
//!   options?)` (option resolution + `GetDurationUnitOptions`).
//! - [`resolved_options`] — `Intl.DurationFormat.prototype.resolvedOptions()`.
//!
//! # See also
//! - <https://tc39.es/proposal-intl-duration-format/>

use crate::intl::payload::{DurationFormatPayload, IntlPayload, JsIntl};
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
    ("hours", &["long", "short", "narrow", "numeric", "2-digit"], "numeric"),
    ("minutes", &["long", "short", "narrow", "numeric", "2-digit"], "2-digit"),
    ("seconds", &["long", "short", "narrow", "numeric", "2-digit"], "2-digit"),
    ("milliseconds", &["long", "short", "narrow", "numeric"], "numeric"),
    ("microseconds", &["long", "short", "narrow", "numeric"], "numeric"),
    ("nanoseconds", &["long", "short", "narrow", "numeric"], "numeric"),
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
        let prim = ctx
            .cx
            .interp
            .to_primitive_string_hint_sync(&exec, value)
            .map_err(|e| crate::native_function::vm_to_native_error(e, CLASS))?;
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

fn prev_is_numeric(prev: &Option<String>) -> bool {
    matches!(prev.as_deref(), Some("numeric") | Some("2-digit"))
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
    let _locale_matcher = get_enum_option(ctx, options_arg, "localeMatcher", &["lookup", "best fit"])?;
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
    let style = get_enum_option(ctx, options_arg, "style", &["long", "short", "narrow", "digital"])?
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

        let display = get_enum_option(ctx, options_arg, &format!("{unit}Display"), &["auto", "always"])?
            .unwrap_or_else(|| display_default.to_string());

        if display == "always" && internal == "fractional" {
            return Err(range_err(format!(
                "{unit} with fractional style cannot use display \"always\""
            )));
        }

        // §conflict: a unit following a numeric / 2-digit unit must
        // itself be fractional / numeric / 2-digit.
        if prev_is_numeric(&prev_internal)
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
    if n.is_nan() || n < 0.0 || n > 9.0 {
        return Err(range_err("fractionalDigits must be between 0 and 9"));
    }
    Ok(Some(n.floor() as u8))
}

pub(crate) fn format(ctx: &mut NativeCtx<'_>, _args: &[Value]) -> Result<Value, NativeError> {
    let _ = require_payload(ctx)?;
    Err(type_err("Intl.DurationFormat.prototype.format is not yet implemented"))
}

pub(crate) fn format_to_parts(
    ctx: &mut NativeCtx<'_>,
    _args: &[Value],
) -> Result<Value, NativeError> {
    let _ = require_payload(ctx)?;
    Err(type_err(
        "Intl.DurationFormat.prototype.formatToParts is not yet implemented",
    ))
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
    let obj = ctx.alloc_object_with_roots(&[&locale, &numbering, &style], &[])?;
    crate::object::set(obj, ctx.heap_mut(), "locale", locale);
    crate::object::set(obj, ctx.heap_mut(), "numberingSystem", numbering);
    crate::object::set(obj, ctx.heap_mut(), "style", style);
    for ((unit, _, _), (unit_style, display)) in UNITS.iter().zip(p.units.iter()) {
        let sv = Value::string(JsString::from_str(unit_style, ctx.heap_mut())?);
        crate::object::set(obj, ctx.heap_mut(), unit, sv);
        let dv = Value::string(JsString::from_str(display, ctx.heap_mut())?);
        let display_key = format!("{unit}Display");
        crate::object::set(obj, ctx.heap_mut(), &display_key, dv);
    }
    if let Some(fd) = p.fractional_digits {
        crate::object::set(
            obj,
            ctx.heap_mut(),
            "fractionalDigits",
            Value::number_i32(fd as i32),
        );
    }
    Ok(Value::object(obj))
}
