//! Intl.NumberFormat implementation.
//!
//! Spec: <https://tc39.es/ecma402/#sec-intl-numberformat-constructor>

use fixed_decimal::{Decimal as FixedDecimal, FloatPrecision, SignedRoundingMode, UnsignedRoundingMode};
use fixed_decimal::{RoundingIncrement, SignDisplay as FixedSignDisplay};
use icu_decimal::DecimalFormatter;
use icu_decimal::options::{DecimalFormatterOptions, GroupingStrategy};
use icu_locale::Locale;
use std::str::FromStr;

use crate::descriptors::{
    JsClassDescriptor, NativeBindingDescriptor, NativeBindingTarget, NativeFunctionDescriptor,
    VmNativeCallError,
};
use crate::object::{ObjectHandle, PropertyAttributes, PropertyValue};
use crate::value::RegisterValue;

use super::locale_utils;
use super::options_utils::{get_option_number, get_option_string};
use super::payload::{
    self, CompactDisplay, CurrencyDisplay, CurrencySign, IntlPayload, Notation, NumberFormatData,
    NumberFormatStyle, RoundingMode, RoundingPriority, SignDisplay, TrailingZeroDisplay, UnitDisplay,
    UseGrouping,
};

// ═══════════════════════════════════════════════════════════════════
//  Class descriptor
// ═══════════════════════════════════════════════════════════════════

/// Returns the JsClassDescriptor for Intl.NumberFormat.
pub fn number_format_class_descriptor() -> JsClassDescriptor {
    JsClassDescriptor::new("NumberFormat")
        .with_constructor(NativeFunctionDescriptor::constructor(
            "NumberFormat",
            0,
            number_format_constructor,
        ))
        // §15.5.3 — `format` is a getter that returns a bound function.
        .with_binding(NativeBindingDescriptor::new(
            NativeBindingTarget::Prototype,
            NativeFunctionDescriptor::getter("format", number_format_format_getter),
        ))
        .with_binding(NativeBindingDescriptor::new(
            NativeBindingTarget::Prototype,
            NativeFunctionDescriptor::method("formatToParts", 1, number_format_format_to_parts),
        ))
        .with_binding(NativeBindingDescriptor::new(
            NativeBindingTarget::Prototype,
            NativeFunctionDescriptor::method("formatRange", 2, number_format_format_range),
        ))
        .with_binding(NativeBindingDescriptor::new(
            NativeBindingTarget::Prototype,
            NativeFunctionDescriptor::method(
                "formatRangeToParts",
                2,
                number_format_format_range_to_parts,
            ),
        ))
        .with_binding(NativeBindingDescriptor::new(
            NativeBindingTarget::Prototype,
            NativeFunctionDescriptor::method("resolvedOptions", 0, number_format_resolved_options),
        ))
        .with_binding(NativeBindingDescriptor::new(
            NativeBindingTarget::Constructor,
            NativeFunctionDescriptor::method(
                "supportedLocalesOf",
                1,
                number_format_supported_locales_of,
            ),
        ))
}

// ═══════════════════════════════════════════════════════════════════
//  §15.1.1 Intl.NumberFormat(locales, options)
// ═══════════════════════════════════════════════════════════════════

/// §15.1.1 Intl.NumberFormat(locales, options)
///
/// Spec: <https://tc39.es/ecma402/#sec-intl.numberformat>
fn number_format_constructor(
    _this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let locales_arg = args.first().copied().unwrap_or_else(RegisterValue::undefined);
    let options_arg = args.get(1).copied().unwrap_or_else(RegisterValue::undefined);

    // Resolve locale.
    let locale = resolve_locale(locales_arg, runtime)?;

    // Resolve options into NumberFormatData.
    let data = resolve_number_format_options(&locale, options_arg, runtime)?;

    // Construct the object with native payload.
    let prototype = runtime.intrinsics().intl_number_format_prototype();
    let handle = payload::construct_intl(IntlPayload::NumberFormat(data), prototype, runtime);

    Ok(RegisterValue::from_object_handle(handle.0))
}

// ═══════════════════════════════════════════════════════════════════
//  §15.5.4 Intl.NumberFormat.prototype.format(value)
// ═══════════════════════════════════════════════════════════════════

/// §15.5.3 get Intl.NumberFormat.prototype.format
///
/// Returns a bound format function. Cached on the instance as `__boundFormat`.
///
/// Spec: <https://tc39.es/ecma402/#sec-intl.numberformat.prototype.format>
fn number_format_format_getter(
    this: &RegisterValue,
    _args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let handle = this.as_object_handle().map(ObjectHandle).ok_or_else(|| {
        VmNativeCallError::Internal("NumberFormat.format getter: expected object".into())
    })?;

    let cache_prop = runtime.intern_property_name("__boundFormat");
    let cached = runtime.own_property_value(handle, cache_prop).map_err(interp_err)?;
    if cached != RegisterValue::undefined() && cached.as_object_handle().is_some() {
        return Ok(cached);
    }

    let desc = NativeFunctionDescriptor::method("format", 1, bound_number_format_format);
    let fn_id = runtime.register_native_function(desc);
    let fn_proto = runtime.intrinsics().function_prototype();
    let bound_fn = runtime.objects_mut().alloc_host_function(fn_id);
    let _ = runtime.objects_mut().set_prototype(bound_fn, Some(fn_proto));

    let nf_prop = runtime.intern_property_name("__numberFormat__");
    runtime.objects_mut().define_own_property(
        bound_fn,
        nf_prop,
        PropertyValue::data_with_attrs(
            RegisterValue::from_object_handle(handle.0),
            PropertyAttributes::from_flags(false, false, false),
        ),
    ).map_err(interp_err)?;

    let bound_val = RegisterValue::from_object_handle(bound_fn.0);
    runtime.objects_mut().define_own_property(
        handle,
        cache_prop,
        PropertyValue::data_with_attrs(bound_val, PropertyAttributes::from_flags(false, false, false)),
    ).map_err(interp_err)?;

    Ok(bound_val)
}

/// Bound format function. Reads NumberFormat instance from `__numberFormat__`.
fn bound_number_format_format(
    _this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let fn_handle = runtime.current_native_callee().ok_or_else(|| {
        VmNativeCallError::Internal("bound format: no callee".into())
    })?;
    let nf_prop = runtime.intern_property_name("__numberFormat__");
    let nf_val = runtime.own_property_value(fn_handle, nf_prop).map_err(interp_err)?;
    let nf_rv = RegisterValue::from_object_handle(
        nf_val.as_object_handle().ok_or_else(|| {
            VmNativeCallError::Internal("bound format: missing __numberFormat__".into())
        })?,
    );

    let data = require_number_format_data(&nf_rv, runtime)?.clone();
    let value = args.first().copied().unwrap_or_else(RegisterValue::undefined);
    let number = runtime
        .js_to_number(value)
        .map_err(|e| VmNativeCallError::Internal(format!("NumberFormat.format: {e}").into()))?;

    let formatted = format_number(number, &data);
    let handle = runtime.alloc_string(formatted);
    Ok(RegisterValue::from_object_handle(handle.0))
}

fn interp_err(e: impl std::fmt::Debug) -> VmNativeCallError {
    VmNativeCallError::Internal(format!("{e:?}").into())
}

// ═══════════════════════════════════════════════════════════════════
//  §15.5.6 Intl.NumberFormat.prototype.formatToParts(value)
// ═══════════════════════════════════════════════════════════════════

/// §15.5.6 Intl.NumberFormat.prototype.formatToParts(value)
///
/// Spec: <https://tc39.es/ecma402/#sec-intl.numberformat.prototype.formattoparts>
fn number_format_format_to_parts(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let data = require_number_format_data(this, runtime)?.clone();
    let value = args.first().copied().unwrap_or_else(RegisterValue::undefined);
    let number = runtime
        .js_to_number(value)
        .map_err(|e| VmNativeCallError::Internal(format!("formatToParts: {e}").into()))?;

    let formatted = format_number(number, &data);
    let parts = decompose_formatted_number(&formatted, number, &data);

    let arr = runtime.alloc_array();
    for (part_type, part_value) in &parts {
        let obj = runtime.alloc_object();
        set_string_prop(runtime, obj, "type", part_type);
        set_string_prop(runtime, obj, "value", part_value);
        runtime
            .objects_mut()
            .push_element(arr, RegisterValue::from_object_handle(obj.0))
            .map_err(|e| {
                VmNativeCallError::Internal(format!("formatToParts: {e:?}").into())
            })?;
    }

    Ok(RegisterValue::from_object_handle(arr.0))
}

/// Decomposes a formatted number string into typed parts.
///
/// Returns a list of (type, value) pairs following the spec part types:
/// "integer", "group", "decimal", "fraction", "minusSign", "plusSign",
/// "percentSign", "infinity", "nan", "literal"
fn decompose_formatted_number(
    formatted: &str,
    number: f64,
    data: &NumberFormatData,
) -> Vec<(&'static str, String)> {
    let mut parts = Vec::new();

    if number.is_nan() {
        parts.push(("nan", "NaN".to_string()));
        return parts;
    }

    if number.is_infinite() {
        if number.is_sign_negative() {
            parts.push(("minusSign", "-".to_string()));
        } else if matches!(data.sign_display, SignDisplay::Always | SignDisplay::ExceptZero) {
            parts.push(("plusSign", "+".to_string()));
        }
        parts.push(("infinity", "∞".to_string()));
        return parts;
    }

    // Parse the formatted string character by character.
    let mut chars = formatted.chars().peekable();
    let mut current = String::new();

    while let Some(&ch) = chars.peek() {
        match ch {
            '-' if current.is_empty() && parts.is_empty() => {
                chars.next();
                parts.push(("minusSign", "-".to_string()));
            }
            '+' if current.is_empty() && parts.is_empty() => {
                chars.next();
                parts.push(("plusSign", "+".to_string()));
            }
            '0'..='9' => {
                current.push(ch);
                chars.next();
            }
            '.' => {
                if !current.is_empty() {
                    parts.push(("integer", current.clone()));
                    current.clear();
                }
                chars.next();
                parts.push(("decimal", ".".to_string()));
                // Collect fraction digits.
                while let Some(&fc) = chars.peek() {
                    if fc.is_ascii_digit() {
                        current.push(fc);
                        chars.next();
                    } else {
                        break;
                    }
                }
                if !current.is_empty() {
                    parts.push(("fraction", current.clone()));
                    current.clear();
                }
            }
            ',' => {
                if !current.is_empty() {
                    parts.push(("integer", current.clone()));
                    current.clear();
                }
                chars.next();
                parts.push(("group", ",".to_string()));
            }
            '\u{00a0}' | '\u{202f}' => {
                // Non-breaking space / narrow no-break space used as grouping in some locales.
                if !current.is_empty() {
                    parts.push(("integer", current.clone()));
                    current.clear();
                }
                chars.next();
                parts.push(("group", ch.to_string()));
            }
            '%' => {
                if !current.is_empty() {
                    let part_type = if parts.iter().any(|(t, _)| *t == "decimal") {
                        "fraction"
                    } else {
                        "integer"
                    };
                    parts.push((part_type, current.clone()));
                    current.clear();
                }
                chars.next();
                parts.push(("percentSign", "%".to_string()));
            }
            _ => {
                // Any other character is a literal (currency symbols, unit text, etc.).
                if !current.is_empty() {
                    let part_type = if parts.iter().any(|(t, _)| *t == "decimal") {
                        "fraction"
                    } else {
                        "integer"
                    };
                    parts.push((part_type, current.clone()));
                    current.clear();
                }
                let mut literal = String::new();
                literal.push(ch);
                chars.next();
                while let Some(&nc) = chars.peek() {
                    if nc.is_ascii_digit() || nc == '.' || nc == ',' || nc == '-' || nc == '+' || nc == '%' {
                        break;
                    }
                    literal.push(nc);
                    chars.next();
                }
                parts.push(("literal", literal));
            }
        }
    }

    // Flush remaining digits.
    if !current.is_empty() {
        let part_type = if parts.iter().any(|(t, _)| *t == "decimal") {
            "fraction"
        } else {
            "integer"
        };
        parts.push((part_type, current));
    }

    parts
}

// ═══════════════════════════════════════════════════════════════════
//  §15.5.7 Intl.NumberFormat.prototype.formatRange(start, end)
// ═══════════════════════════════════════════════════════════════════

/// §15.5.7 Intl.NumberFormat.prototype.formatRange(start, end)
///
/// Spec: <https://tc39.es/ecma402/#sec-intl.numberformat.prototype.formatrange>
fn number_format_format_range(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let data = require_number_format_data(this, runtime)?.clone();

    let start_val = args.first().copied().unwrap_or_else(RegisterValue::undefined);
    let end_val = args.get(1).copied().unwrap_or_else(RegisterValue::undefined);

    if start_val == RegisterValue::undefined() || end_val == RegisterValue::undefined() {
        return Err(type_error(runtime, "start and end must be provided to formatRange"));
    }

    let start = runtime
        .js_to_number(start_val)
        .map_err(|e| VmNativeCallError::Internal(format!("formatRange: {e}").into()))?;
    let end = runtime
        .js_to_number(end_val)
        .map_err(|e| VmNativeCallError::Internal(format!("formatRange: {e}").into()))?;

    if start.is_nan() || end.is_nan() {
        return Err(range_error(runtime, "Invalid number value"));
    }

    let start_str = format_number(start, &data);
    let end_str = format_number(end, &data);
    let result = if start_str == end_str {
        // §15.5.7 step 5: if start and end produce the same formatted string, return it.
        format!("~{start_str}")
    } else {
        format!("{start_str}\u{2013}{end_str}")
    };

    let handle = runtime.alloc_string(result);
    Ok(RegisterValue::from_object_handle(handle.0))
}

// ═══════════════════════════════════════════════════════════════════
//  §15.5.8 Intl.NumberFormat.prototype.formatRangeToParts(start, end)
// ═══════════════════════════════════════════════════════════════════

/// §15.5.8 Intl.NumberFormat.prototype.formatRangeToParts(start, end)
///
/// Spec: <https://tc39.es/ecma402/#sec-intl.numberformat.prototype.formatrangetoparts>
fn number_format_format_range_to_parts(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let data = require_number_format_data(this, runtime)?.clone();

    let start_val = args.first().copied().unwrap_or_else(RegisterValue::undefined);
    let end_val = args.get(1).copied().unwrap_or_else(RegisterValue::undefined);

    if start_val == RegisterValue::undefined() || end_val == RegisterValue::undefined() {
        return Err(type_error(runtime, "start and end must be provided to formatRangeToParts"));
    }

    let start = runtime
        .js_to_number(start_val)
        .map_err(|e| VmNativeCallError::Internal(format!("formatRangeToParts: {e}").into()))?;
    let end = runtime
        .js_to_number(end_val)
        .map_err(|e| VmNativeCallError::Internal(format!("formatRangeToParts: {e}").into()))?;

    if start.is_nan() || end.is_nan() {
        return Err(range_error(runtime, "Invalid number value"));
    }

    let start_formatted = format_number(start, &data);
    let end_formatted = format_number(end, &data);

    let arr = runtime.alloc_array();

    // Start number parts (source: "startRange").
    let start_parts = decompose_formatted_number(&start_formatted, start, &data);
    for (part_type, part_value) in &start_parts {
        let obj = runtime.alloc_object();
        set_string_prop(runtime, obj, "type", part_type);
        set_string_prop(runtime, obj, "value", part_value);
        set_string_prop(runtime, obj, "source", "startRange");
        runtime
            .objects_mut()
            .push_element(arr, RegisterValue::from_object_handle(obj.0))
            .map_err(|e| VmNativeCallError::Internal(format!("formatRangeToParts: {e:?}").into()))?;
    }

    // Literal separator.
    let sep_obj = runtime.alloc_object();
    set_string_prop(runtime, sep_obj, "type", "literal");
    set_string_prop(runtime, sep_obj, "value", "\u{2013}");
    set_string_prop(runtime, sep_obj, "source", "shared");
    runtime
        .objects_mut()
        .push_element(arr, RegisterValue::from_object_handle(sep_obj.0))
        .map_err(|e| VmNativeCallError::Internal(format!("formatRangeToParts: {e:?}").into()))?;

    // End number parts (source: "endRange").
    let end_parts = decompose_formatted_number(&end_formatted, end, &data);
    for (part_type, part_value) in &end_parts {
        let obj = runtime.alloc_object();
        set_string_prop(runtime, obj, "type", part_type);
        set_string_prop(runtime, obj, "value", part_value);
        set_string_prop(runtime, obj, "source", "endRange");
        runtime
            .objects_mut()
            .push_element(arr, RegisterValue::from_object_handle(obj.0))
            .map_err(|e| VmNativeCallError::Internal(format!("formatRangeToParts: {e:?}").into()))?;
    }

    Ok(RegisterValue::from_object_handle(arr.0))
}

// ═══════════════════════════════════════════════════════════════════
//  §15.5.5 Intl.NumberFormat.prototype.resolvedOptions()
// ═══════════════════════════════════════════════════════════════════

/// §15.5.5 Intl.NumberFormat.prototype.resolvedOptions()
///
/// Spec: <https://tc39.es/ecma402/#sec-intl.numberformat.prototype.resolvedoptions>
fn number_format_resolved_options(
    this: &RegisterValue,
    _args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let data = require_number_format_data(this, runtime)?.clone();

    let obj = runtime.alloc_object();
    set_string_prop(runtime, obj, "locale", &data.locale);
    set_string_prop(runtime, obj, "numberingSystem", &data.numbering_system);
    set_string_prop(runtime, obj, "style", data.style.as_str());
    if data.style == NumberFormatStyle::Currency {
        if let Some(ref cur) = data.currency {
            set_string_prop(runtime, obj, "currency", cur);
        }
        set_string_prop(runtime, obj, "currencyDisplay", data.currency_display.as_str());
        set_string_prop(runtime, obj, "currencySign", data.currency_sign.as_str());
    }
    if data.style == NumberFormatStyle::Unit {
        if let Some(ref unit) = data.unit {
            set_string_prop(runtime, obj, "unit", unit);
        }
        set_string_prop(runtime, obj, "unitDisplay", data.unit_display.as_str());
    }
    set_i32_prop(runtime, obj, "minimumIntegerDigits", data.minimum_integer_digits as i32);
    if let Some(v) = data.minimum_fraction_digits {
        set_i32_prop(runtime, obj, "minimumFractionDigits", v as i32);
    }
    if let Some(v) = data.maximum_fraction_digits {
        set_i32_prop(runtime, obj, "maximumFractionDigits", v as i32);
    }
    if let Some(v) = data.minimum_significant_digits {
        set_i32_prop(runtime, obj, "minimumSignificantDigits", v as i32);
    }
    if let Some(v) = data.maximum_significant_digits {
        set_i32_prop(runtime, obj, "maximumSignificantDigits", v as i32);
    }
    set_string_prop(runtime, obj, "useGrouping", data.use_grouping.as_str());
    set_string_prop(runtime, obj, "notation", data.notation.as_str());
    if data.notation == Notation::Compact {
        set_string_prop(runtime, obj, "compactDisplay", data.compact_display.as_str());
    }
    set_string_prop(runtime, obj, "signDisplay", data.sign_display.as_str());
    set_string_prop(runtime, obj, "roundingMode", data.rounding_mode.as_str());
    set_i32_prop(runtime, obj, "roundingIncrement", data.rounding_increment as i32);
    set_string_prop(runtime, obj, "roundingPriority", data.rounding_priority.as_str());
    set_string_prop(runtime, obj, "trailingZeroDisplay", data.trailing_zero_display.as_str());

    Ok(RegisterValue::from_object_handle(obj.0))
}

// ═══════════════════════════════════════════════════════════════════
//  §15.2.2 Intl.NumberFormat.supportedLocalesOf(locales)
// ═══════════════════════════════════════════════════════════════════

fn number_format_supported_locales_of(
    _this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    // Simplified — return the canonical form of the requested locales.
    let locales_arg = args.first().copied().unwrap_or_else(RegisterValue::undefined);
    let locale_list = super::canonicalize_locale_list_from_value(locales_arg, runtime)?;
    let arr = runtime.alloc_array();
    for locale in &locale_list {
        let s = runtime.alloc_string(locale.as_str());
        runtime
            .objects_mut()
            .push_element(arr, RegisterValue::from_object_handle(s.0))
            .map_err(|e| {
                VmNativeCallError::Internal(format!("supportedLocalesOf: {e:?}").into())
            })?;
    }
    Ok(RegisterValue::from_object_handle(arr.0))
}

// ═══════════════════════════════════════════════════════════════════
//  Options resolution
// ═══════════════════════════════════════════════════════════════════

fn resolve_locale(
    locales_arg: RegisterValue,
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<String, VmNativeCallError> {
    let list = super::canonicalize_locale_list_from_value(locales_arg, runtime)?;
    Ok(list.into_iter().next().unwrap_or_else(|| locale_utils::DEFAULT_LOCALE.to_string()))
}

fn resolve_number_format_options(
    locale: &str,
    options: RegisterValue,
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<NumberFormatData, VmNativeCallError> {
    let style_str = get_option_string(options, "style", runtime)?
        .unwrap_or_else(|| "decimal".to_string());
    let style = match style_str.as_str() {
        "decimal" => NumberFormatStyle::Decimal,
        "percent" => NumberFormatStyle::Percent,
        "currency" => NumberFormatStyle::Currency,
        "unit" => NumberFormatStyle::Unit,
        _ => return Err(range_error(runtime, "Invalid style option")),
    };

    let currency = get_option_string(options, "currency", runtime)?;
    if style == NumberFormatStyle::Currency && currency.is_none() {
        return Err(type_error(runtime, "currency option is required with currency style"));
    }
    let currency = currency.map(|c| c.to_ascii_uppercase());

    let currency_display = parse_enum(
        get_option_string(options, "currencyDisplay", runtime)?,
        CurrencyDisplay::from_str_opt,
        CurrencyDisplay::Symbol,
        "currencyDisplay",
        runtime,
    )?;

    let currency_sign = parse_enum(
        get_option_string(options, "currencySign", runtime)?,
        CurrencySign::from_str_opt,
        CurrencySign::Standard,
        "currencySign",
        runtime,
    )?;

    let unit = get_option_string(options, "unit", runtime)?;
    if style == NumberFormatStyle::Unit && unit.is_none() {
        return Err(type_error(runtime, "unit option is required with unit style"));
    }
    if let Some(ref u) = unit
        && !locale_utils::is_well_formed_unit_identifier(u)
    {
        return Err(range_error(runtime, "Invalid unit option"));
    }

    let unit_display = parse_enum(
        get_option_string(options, "unitDisplay", runtime)?,
        UnitDisplay::from_str_opt,
        UnitDisplay::Short,
        "unitDisplay",
        runtime,
    )?;

    let notation = parse_enum(
        get_option_string(options, "notation", runtime)?,
        Notation::from_str_opt,
        Notation::Standard,
        "notation",
        runtime,
    )?;

    let compact_display = parse_enum(
        get_option_string(options, "compactDisplay", runtime)?,
        CompactDisplay::from_str_opt,
        CompactDisplay::Short,
        "compactDisplay",
        runtime,
    )?;

    let use_grouping_str = get_option_string(options, "useGrouping", runtime)?;
    let use_grouping = match use_grouping_str.as_deref() {
        None => {
            if notation == Notation::Compact {
                UseGrouping::Min2
            } else {
                UseGrouping::Auto
            }
        }
        Some("always") => UseGrouping::Always,
        Some("auto") => UseGrouping::Auto,
        Some("min2") => UseGrouping::Min2,
        Some("false") | Some("") => UseGrouping::False,
        Some("true") => {
            if notation == Notation::Compact {
                UseGrouping::Min2
            } else {
                UseGrouping::Auto
            }
        }
        Some(_) => return Err(range_error(runtime, "Invalid useGrouping option")),
    };

    let sign_display = parse_enum(
        get_option_string(options, "signDisplay", runtime)?,
        SignDisplay::from_str_opt,
        SignDisplay::Auto,
        "signDisplay",
        runtime,
    )?;

    // Digit options.
    let min_int = resolve_int_option(options, "minimumIntegerDigits", 1, 21, 1, runtime)?;
    let min_frac = get_option_number(options, "minimumFractionDigits", runtime)?
        .map(|v| validate_int_range(v, 0, 100, "minimumFractionDigits", runtime))
        .transpose()?;
    let max_frac = get_option_number(options, "maximumFractionDigits", runtime)?
        .map(|v| validate_int_range(v, 0, 100, "maximumFractionDigits", runtime))
        .transpose()?;
    if let (Some(minf), Some(maxf)) = (min_frac, max_frac)
        && minf > maxf
    {
        return Err(range_error(runtime, "minimumFractionDigits > maximumFractionDigits"));
    }
    // Defaults for fraction digits.
    let min_frac = min_frac.or(Some(if style == NumberFormatStyle::Currency { 2 } else { 0 }));
    let max_frac = max_frac.or(Some(if style == NumberFormatStyle::Currency { 2 } else { 3 }));

    let min_sig = get_option_number(options, "minimumSignificantDigits", runtime)?
        .map(|v| validate_int_range(v, 1, 21, "minimumSignificantDigits", runtime))
        .transpose()?;
    let max_sig = get_option_number(options, "maximumSignificantDigits", runtime)?
        .map(|v| validate_int_range(v, 1, 21, "maximumSignificantDigits", runtime))
        .transpose()?;
    let min_sig = if min_sig.is_none() && max_sig.is_some() { Some(1) } else { min_sig };
    if let (Some(mins), Some(maxs)) = (min_sig, max_sig)
        && mins > maxs
    {
        return Err(range_error(runtime, "minimumSignificantDigits > maximumSignificantDigits"));
    }

    let rounding_increment = get_option_number(options, "roundingIncrement", runtime)?
        .map(|v| {
            let allowed = [
                1.0, 2.0, 5.0, 10.0, 20.0, 25.0, 50.0, 100.0, 200.0, 250.0, 500.0, 1000.0,
                2000.0, 2500.0, 5000.0,
            ];
            if !v.is_finite() || v.fract() != 0.0 || !allowed.contains(&v) {
                return Err(range_error(runtime, "Invalid roundingIncrement option"));
            }
            Ok(v as u32)
        })
        .transpose()?
        .unwrap_or(1);

    let rounding_mode = parse_enum(
        get_option_string(options, "roundingMode", runtime)?,
        RoundingMode::from_str_opt,
        RoundingMode::HalfExpand,
        "roundingMode",
        runtime,
    )?;

    let rounding_priority = parse_enum(
        get_option_string(options, "roundingPriority", runtime)?,
        RoundingPriority::from_str_opt,
        RoundingPriority::Auto,
        "roundingPriority",
        runtime,
    )?;

    let trailing_zero_display = parse_enum(
        get_option_string(options, "trailingZeroDisplay", runtime)?,
        TrailingZeroDisplay::from_str_opt,
        TrailingZeroDisplay::Auto,
        "trailingZeroDisplay",
        runtime,
    )?;

    let numbering_system = get_option_string(options, "numberingSystem", runtime)?
        .unwrap_or_else(|| "latn".to_string());

    Ok(NumberFormatData {
        locale: locale.to_string(),
        style,
        currency,
        currency_display,
        currency_sign,
        unit,
        unit_display,
        notation,
        compact_display,
        use_grouping,
        sign_display,
        minimum_integer_digits: min_int,
        minimum_fraction_digits: min_frac,
        maximum_fraction_digits: max_frac,
        minimum_significant_digits: min_sig,
        maximum_significant_digits: max_sig,
        rounding_increment,
        rounding_mode,
        rounding_priority,
        trailing_zero_display,
        numbering_system,
    })
}

// ═══════════════════════════════════════════════════════════════════
//  Core formatting
// ═══════════════════════════════════════════════════════════════════

fn format_number(number: f64, data: &NumberFormatData) -> String {
    if number.is_nan() {
        return "NaN".to_string();
    }
    if number.is_infinite() {
        let sign = if number.is_sign_negative() { "-" } else {
            match data.sign_display {
                SignDisplay::Always | SignDisplay::ExceptZero => "+",
                _ => "",
            }
        };
        return format!("{sign}∞");
    }

    let mut value = number;
    if data.style == NumberFormatStyle::Percent {
        value *= 100.0;
    }

    let Ok(mut decimal) = FixedDecimal::try_from_f64(value, FloatPrecision::RoundTrip) else {
        return value.to_string();
    };

    apply_digit_options(data, &mut decimal);

    // Format with ICU4X.
    let formatted = match create_decimal_formatter(data) {
        Ok(fmt) => fmt.format(&decimal).to_string(),
        Err(_) => decimal.to_string(),
    };

    let mut out = formatted;

    if data.style == NumberFormatStyle::Percent {
        out.push('%');
    }

    out
}

fn apply_digit_options(data: &NumberFormatData, decimal: &mut FixedDecimal) {
    let rounding_mode = to_signed_rounding_mode(data.rounding_mode);
    let sign_display = to_fixed_sign_display(data.sign_display);

    // Significant digits path.
    let has_sig = data.minimum_significant_digits.is_some() || data.maximum_significant_digits.is_some();
    if has_sig && data.rounding_priority != RoundingPriority::Auto {
        // morePrecision / lessPrecision with both sig and frac — simplified.
        let mut sig_decimal = decimal.clone();
        if let Some(max_sig) = data.maximum_significant_digits
            && let Some(pos) = significant_round_position(&sig_decimal, max_sig as i16)
        {
            sig_decimal.round_with_mode(pos, rounding_mode);
        }
        if let Some(min_sig) = data.minimum_significant_digits
            && let Some(pad_pos) = significant_pad_position(&sig_decimal, min_sig as i16)
        {
            sig_decimal.pad_end(pad_pos);
        }
        *decimal = sig_decimal;
    } else if has_sig {
        if let Some(max_sig) = data.maximum_significant_digits
            && let Some(pos) = significant_round_position(decimal, max_sig as i16)
        {
            decimal.round_with_mode(pos, rounding_mode);
        }
        if let Some(min_sig) = data.minimum_significant_digits
            && let Some(pad_pos) = significant_pad_position(decimal, min_sig as i16)
        {
            decimal.pad_end(pad_pos);
        }
    } else {
        // Fraction digits path.
        if let Some(max_frac) = data.maximum_fraction_digits {
            if data.rounding_increment > 1 {
                let mut base = data.rounding_increment as i16;
                let mut shift = 0i16;
                while base % 10 == 0 {
                    base /= 10;
                    shift += 1;
                }
                let increment = match base {
                    2 => RoundingIncrement::MultiplesOf2,
                    5 => RoundingIncrement::MultiplesOf5,
                    25 => RoundingIncrement::MultiplesOf25,
                    _ => RoundingIncrement::MultiplesOf1,
                };
                let position = -(max_frac as i16) + shift;
                decimal.round_with_mode_and_increment(position, rounding_mode, increment);
            } else {
                decimal.round_with_mode(-(max_frac as i16), rounding_mode);
            }
        }
        if let Some(min_frac) = data.minimum_fraction_digits {
            decimal.pad_end(-(min_frac as i16));
        }
    }

    decimal.pad_start(data.minimum_integer_digits as i16);
    decimal.apply_sign_display(sign_display);
}

fn create_decimal_formatter(data: &NumberFormatData) -> Result<DecimalFormatter, String> {
    let mut options = DecimalFormatterOptions::default();
    options.grouping_strategy = Some(match data.use_grouping {
        UseGrouping::Always => GroupingStrategy::Always,
        UseGrouping::Auto => GroupingStrategy::Auto,
        UseGrouping::Min2 => GroupingStrategy::Min2,
        UseGrouping::False => GroupingStrategy::Never,
    });

    let locale = Locale::from_str(&data.locale).unwrap_or_else(|_| {
        Locale::from_str(locale_utils::DEFAULT_LOCALE).expect("default locale should parse")
    });

    DecimalFormatter::try_new(locale.into(), options)
        .map_err(|e| format!("ICU DecimalFormatter: {e}"))
}

// ═══════════════════════════════════════════════════════════════════
//  Significant digits helpers
// ═══════════════════════════════════════════════════════════════════

fn significant_round_position(decimal: &FixedDecimal, sig_digits: i16) -> Option<i16> {
    if sig_digits <= 0 {
        return None;
    }
    let s = decimal.to_string();
    let s = s
        .strip_prefix('-')
        .or_else(|| s.strip_prefix('+'))
        .unwrap_or(&s);
    let (int_part, frac_part) = s.split_once('.').unwrap_or((s, ""));
    let digits = format!("{int_part}{frac_part}");
    let first_nonzero = digits.bytes().position(|b| b != b'0')?;
    let exponent = int_part.len() as i16 - first_nonzero as i16 - 1;
    Some(exponent - sig_digits + 1)
}

fn significant_pad_position(decimal: &FixedDecimal, min_sig: i16) -> Option<i16> {
    if min_sig <= 0 {
        return None;
    }
    let s = decimal.to_string();
    let s = s
        .strip_prefix('-')
        .or_else(|| s.strip_prefix('+'))
        .unwrap_or(&s);
    let (int_part, frac_part) = s.split_once('.').unwrap_or((s, ""));
    let digits = format!("{int_part}{frac_part}");
    let Some(first_nonzero) = digits.bytes().position(|b| b != b'0') else {
        return Some(-(min_sig - 1));
    };
    let exponent = int_part.len() as i16 - first_nonzero as i16 - 1;
    Some(exponent - min_sig + 1)
}

// ═══════════════════════════════════════════════════════════════════
//  Enum conversions
// ═══════════════════════════════════════════════════════════════════

fn to_signed_rounding_mode(mode: RoundingMode) -> SignedRoundingMode {
    match mode {
        RoundingMode::Ceil => SignedRoundingMode::Ceil,
        RoundingMode::Floor => SignedRoundingMode::Floor,
        RoundingMode::Expand => SignedRoundingMode::Unsigned(UnsignedRoundingMode::Expand),
        RoundingMode::Trunc => SignedRoundingMode::Unsigned(UnsignedRoundingMode::Trunc),
        RoundingMode::HalfCeil => SignedRoundingMode::HalfCeil,
        RoundingMode::HalfFloor => SignedRoundingMode::HalfFloor,
        RoundingMode::HalfExpand => SignedRoundingMode::Unsigned(UnsignedRoundingMode::HalfExpand),
        RoundingMode::HalfTrunc => SignedRoundingMode::Unsigned(UnsignedRoundingMode::HalfTrunc),
        RoundingMode::HalfEven => SignedRoundingMode::Unsigned(UnsignedRoundingMode::HalfEven),
    }
}

fn to_fixed_sign_display(sd: SignDisplay) -> FixedSignDisplay {
    match sd {
        SignDisplay::Auto => FixedSignDisplay::Auto,
        SignDisplay::Never => FixedSignDisplay::Never,
        SignDisplay::Always => FixedSignDisplay::Always,
        SignDisplay::ExceptZero => FixedSignDisplay::ExceptZero,
        SignDisplay::Negative => FixedSignDisplay::Negative,
    }
}

// ═══════════════════════════════════════════════════════════════════
//  as_str() implementations for payload enums
// ═══════════════════════════════════════════════════════════════════

impl NumberFormatStyle {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Decimal => "decimal",
            Self::Currency => "currency",
            Self::Percent => "percent",
            Self::Unit => "unit",
        }
    }
}

impl CurrencyDisplay {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Code => "code",
            Self::Symbol => "symbol",
            Self::NarrowSymbol => "narrowSymbol",
            Self::Name => "name",
        }
    }
    fn from_str_opt(s: &str) -> Option<Self> {
        match s {
            "code" => Some(Self::Code),
            "symbol" => Some(Self::Symbol),
            "narrowSymbol" => Some(Self::NarrowSymbol),
            "name" => Some(Self::Name),
            _ => None,
        }
    }
}

impl CurrencySign {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Standard => "standard",
            Self::Accounting => "accounting",
        }
    }
    fn from_str_opt(s: &str) -> Option<Self> {
        match s {
            "standard" => Some(Self::Standard),
            "accounting" => Some(Self::Accounting),
            _ => None,
        }
    }
}

impl UnitDisplay {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Short => "short",
            Self::Narrow => "narrow",
            Self::Long => "long",
        }
    }
    fn from_str_opt(s: &str) -> Option<Self> {
        match s {
            "short" => Some(Self::Short),
            "narrow" => Some(Self::Narrow),
            "long" => Some(Self::Long),
            _ => None,
        }
    }
}

impl Notation {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Standard => "standard",
            Self::Scientific => "scientific",
            Self::Engineering => "engineering",
            Self::Compact => "compact",
        }
    }
    fn from_str_opt(s: &str) -> Option<Self> {
        match s {
            "standard" => Some(Self::Standard),
            "scientific" => Some(Self::Scientific),
            "engineering" => Some(Self::Engineering),
            "compact" => Some(Self::Compact),
            _ => None,
        }
    }
}

impl CompactDisplay {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Short => "short",
            Self::Long => "long",
        }
    }
    fn from_str_opt(s: &str) -> Option<Self> {
        match s {
            "short" => Some(Self::Short),
            "long" => Some(Self::Long),
            _ => None,
        }
    }
}

impl UseGrouping {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Always => "always",
            Self::Auto => "auto",
            Self::Min2 => "min2",
            Self::False => "false",
        }
    }
}

impl SignDisplay {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Never => "never",
            Self::Always => "always",
            Self::ExceptZero => "exceptZero",
            Self::Negative => "negative",
        }
    }
    fn from_str_opt(s: &str) -> Option<Self> {
        match s {
            "auto" => Some(Self::Auto),
            "never" => Some(Self::Never),
            "always" => Some(Self::Always),
            "exceptZero" => Some(Self::ExceptZero),
            "negative" => Some(Self::Negative),
            _ => None,
        }
    }
}

impl RoundingMode {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Ceil => "ceil",
            Self::Floor => "floor",
            Self::Expand => "expand",
            Self::Trunc => "trunc",
            Self::HalfCeil => "halfCeil",
            Self::HalfFloor => "halfFloor",
            Self::HalfExpand => "halfExpand",
            Self::HalfTrunc => "halfTrunc",
            Self::HalfEven => "halfEven",
        }
    }
    fn from_str_opt(s: &str) -> Option<Self> {
        match s {
            "ceil" => Some(Self::Ceil),
            "floor" => Some(Self::Floor),
            "expand" => Some(Self::Expand),
            "trunc" => Some(Self::Trunc),
            "halfCeil" => Some(Self::HalfCeil),
            "halfFloor" => Some(Self::HalfFloor),
            "halfExpand" => Some(Self::HalfExpand),
            "halfTrunc" => Some(Self::HalfTrunc),
            "halfEven" => Some(Self::HalfEven),
            _ => None,
        }
    }
}

impl RoundingPriority {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::MorePrecision => "morePrecision",
            Self::LessPrecision => "lessPrecision",
        }
    }
    fn from_str_opt(s: &str) -> Option<Self> {
        match s {
            "auto" => Some(Self::Auto),
            "morePrecision" => Some(Self::MorePrecision),
            "lessPrecision" => Some(Self::LessPrecision),
            _ => None,
        }
    }
}

impl TrailingZeroDisplay {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::StripIfInteger => "stripIfInteger",
        }
    }
    fn from_str_opt(s: &str) -> Option<Self> {
        match s {
            "auto" => Some(Self::Auto),
            "stripIfInteger" => Some(Self::StripIfInteger),
            _ => None,
        }
    }
}

// ═══════════════════════════════════════════════════════════════════
//  Internal helpers
// ═══════════════════════════════════════════════════════════════════

fn require_number_format_data<'a>(
    this: &RegisterValue,
    runtime: &'a crate::interpreter::RuntimeState,
) -> Result<&'a NumberFormatData, VmNativeCallError> {
    let payload = payload::require_intl_payload(this, runtime).map_err(|e| {
        VmNativeCallError::Internal(format!("NumberFormat: {e}").into())
    })?;
    payload.as_number_format().ok_or_else(|| {
        VmNativeCallError::Internal(
            "called on incompatible Intl receiver (not NumberFormat)".into(),
        )
    })
}

fn set_string_prop(
    runtime: &mut crate::interpreter::RuntimeState,
    obj: ObjectHandle,
    name: &str,
    value: &str,
) {
    let prop = runtime.intern_property_name(name);
    let s = runtime.alloc_string(value);
    let _ = runtime.objects_mut().set_property(
        obj,
        prop,
        RegisterValue::from_object_handle(s.0),
    );
}

fn set_i32_prop(
    runtime: &mut crate::interpreter::RuntimeState,
    obj: ObjectHandle,
    name: &str,
    value: i32,
) {
    let prop = runtime.intern_property_name(name);
    let _ = runtime.objects_mut().set_property(
        obj,
        prop,
        RegisterValue::from_i32(value),
    );
}

fn parse_enum<T>(
    value: Option<String>,
    from_str: fn(&str) -> Option<T>,
    default: T,
    name: &str,
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<T, VmNativeCallError> {
    match value {
        None => Ok(default),
        Some(s) => from_str(&s).ok_or_else(|| range_error(runtime, &format!("Invalid {name} option"))),
    }
}

fn resolve_int_option(
    options: RegisterValue,
    name: &str,
    min: u32,
    max: u32,
    default: u32,
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<u32, VmNativeCallError> {
    match get_option_number(options, name, runtime)? {
        None => Ok(default),
        Some(v) => validate_int_range(v, min, max, name, runtime),
    }
}

fn validate_int_range(
    v: f64,
    min: u32,
    max: u32,
    name: &str,
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<u32, VmNativeCallError> {
    if !v.is_finite() || v.fract() != 0.0 || v < f64::from(min) || v > f64::from(max) {
        return Err(range_error(runtime, &format!("Invalid {name} option")));
    }
    Ok(v as u32)
}

fn range_error(
    runtime: &mut crate::interpreter::RuntimeState,
    message: &str,
) -> VmNativeCallError {
    match runtime.alloc_range_error(message) {
        Ok(err) => VmNativeCallError::Thrown(RegisterValue::from_object_handle(err.0)),
        Err(e) => VmNativeCallError::Internal(format!("RangeError alloc: {e}").into()),
    }
}

fn type_error(
    runtime: &mut crate::interpreter::RuntimeState,
    message: &str,
) -> VmNativeCallError {
    match runtime.alloc_type_error(message) {
        Ok(err) => VmNativeCallError::Thrown(RegisterValue::from_object_handle(err.0)),
        Err(e) => VmNativeCallError::Internal(format!("TypeError alloc: {e}").into()),
    }
}
