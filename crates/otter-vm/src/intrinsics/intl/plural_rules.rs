//! Intl.PluralRules implementation.
//!
//! Spec: <https://tc39.es/ecma402/#sec-intl-pluralrules-constructor>

use fixed_decimal::{Decimal as FixedDecimal, FloatPrecision};
use icu_locale::Locale;
use icu_plurals::{PluralCategory, PluralRules as IcuPluralRules};
use std::str::FromStr;

use crate::descriptors::{
    JsClassDescriptor, NativeBindingDescriptor, NativeBindingTarget, NativeFunctionDescriptor,
    VmNativeCallError,
};
use crate::value::RegisterValue;

use super::locale_utils;
use super::options_utils::{get_option_number, get_option_string};
use super::payload::{self, IntlPayload, PluralRulesData, PluralRulesType};

// ═══════════════════════════════════════════════════════════════════
//  Class descriptor
// ═══════════════════════════════════════════════════════════════════

pub fn plural_rules_class_descriptor() -> JsClassDescriptor {
    JsClassDescriptor::new("PluralRules")
        .with_constructor(NativeFunctionDescriptor::constructor(
            "PluralRules",
            0,
            plural_rules_constructor,
        ))
        .with_binding(NativeBindingDescriptor::new(
            NativeBindingTarget::Prototype,
            NativeFunctionDescriptor::method("select", 1, plural_rules_select),
        ))
        .with_binding(NativeBindingDescriptor::new(
            NativeBindingTarget::Prototype,
            NativeFunctionDescriptor::method("selectRange", 2, plural_rules_select_range),
        ))
        .with_binding(NativeBindingDescriptor::new(
            NativeBindingTarget::Prototype,
            NativeFunctionDescriptor::method("resolvedOptions", 0, plural_rules_resolved_options),
        ))
        .with_binding(NativeBindingDescriptor::new(
            NativeBindingTarget::Constructor,
            NativeFunctionDescriptor::method(
                "supportedLocalesOf",
                1,
                plural_rules_supported_locales_of,
            ),
        ))
}

// ═══════════════════════════════════════════════════════════════════
//  §13.1.1 Intl.PluralRules(locales, options)
// ═══════════════════════════════════════════════════════════════════

fn plural_rules_constructor(
    _this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let locales_arg = args
        .first()
        .copied()
        .unwrap_or_else(RegisterValue::undefined);
    let options_arg = args
        .get(1)
        .copied()
        .unwrap_or_else(RegisterValue::undefined);

    let locale = super::resolve_locale(locales_arg, runtime)?;
    let data = resolve_plural_rules_options(&locale, options_arg, runtime)?;

    let prototype = runtime.intrinsics().intl_plural_rules_prototype();
    let handle = payload::construct_intl(IntlPayload::PluralRules(data), prototype, runtime);

    Ok(RegisterValue::from_object_handle(handle.0))
}

// ═══════════════════════════════════════════════════════════════════
//  §13.5.4 Intl.PluralRules.prototype.select(value)
// ═══════════════════════════════════════════════════════════════════

/// §13.5.4 Intl.PluralRules.prototype.select(value)
///
/// Spec: <https://tc39.es/ecma402/#sec-intl.pluralrules.prototype.select>
fn plural_rules_select(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let data = require_plural_rules_data(this, runtime)?.clone();

    let value = args
        .first()
        .copied()
        .unwrap_or_else(RegisterValue::undefined);
    let number = runtime
        .js_to_number(value)
        .map_err(|e| VmNativeCallError::Internal(format!("PluralRules.select: {e}").into()))?;

    let category = resolve_plural(number, &data);
    let handle = runtime.alloc_string(plural_category_str(category));
    Ok(RegisterValue::from_object_handle(handle.0))
}

// ═══════════════════════════════════════════════════════════════════
//  §1.4.5 Intl.PluralRules.prototype.selectRange(start, end)
// ═══════════════════════════════════════════════════════════════════

/// Intl.PluralRules.prototype.selectRange(start, end)
///
/// Spec: <https://tc39.es/ecma402/#sec-intl.pluralrules.prototype.selectrange>
fn plural_rules_select_range(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let data = require_plural_rules_data(this, runtime)?.clone();

    let start_val = args
        .first()
        .copied()
        .unwrap_or_else(RegisterValue::undefined);
    let end_val = args
        .get(1)
        .copied()
        .unwrap_or_else(RegisterValue::undefined);

    if start_val == RegisterValue::undefined() || end_val == RegisterValue::undefined() {
        return Err(type_error(
            runtime,
            "start and end are required for selectRange",
        ));
    }

    let start = runtime
        .js_to_number(start_val)
        .map_err(|e| VmNativeCallError::Internal(format!("selectRange: {e}").into()))?;
    let end = runtime
        .js_to_number(end_val)
        .map_err(|e| VmNativeCallError::Internal(format!("selectRange: {e}").into()))?;

    if start.is_nan() || end.is_nan() {
        return Err(range_error(runtime, "Invalid number for selectRange"));
    }

    // Simplified: select the end category (full impl would use PluralRulesWithRanges).
    let category = resolve_plural(end, &data);
    let handle = runtime.alloc_string(plural_category_str(category));
    Ok(RegisterValue::from_object_handle(handle.0))
}

// ═══════════════════════════════════════════════════════════════════
//  §13.5.5 Intl.PluralRules.prototype.resolvedOptions()
// ═══════════════════════════════════════════════════════════════════

fn plural_rules_resolved_options(
    this: &RegisterValue,
    _args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let data = require_plural_rules_data(this, runtime)?.clone();

    let obj = runtime.alloc_object();
    set_string_prop(runtime, obj, "locale", &data.locale);
    set_string_prop(runtime, obj, "type", data.plural_type.as_str());
    set_i32_prop(
        runtime,
        obj,
        "minimumIntegerDigits",
        data.minimum_integer_digits as i32,
    );
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

    // pluralCategories: list of categories used by this locale.
    let locale = Locale::from_str(&data.locale).unwrap_or_else(|_| {
        Locale::from_str(locale_utils::DEFAULT_LOCALE).expect("default locale should parse")
    });
    let rules = match data.plural_type {
        PluralRulesType::Cardinal => IcuPluralRules::try_new_cardinal((&locale).into()),
        PluralRulesType::Ordinal => IcuPluralRules::try_new_ordinal((&locale).into()),
    };
    if let Ok(rules) = rules {
        let cats_arr = runtime.alloc_array();
        for cat in rules.categories() {
            let s = runtime.alloc_string(plural_category_str(cat));
            let _ = runtime
                .objects_mut()
                .push_element(cats_arr, RegisterValue::from_object_handle(s.0));
        }
        let prop = runtime.intern_property_name("pluralCategories");
        let _ = runtime.objects_mut().set_property(
            obj,
            prop,
            RegisterValue::from_object_handle(cats_arr.0),
        );
    }

    Ok(RegisterValue::from_object_handle(obj.0))
}

// ═══════════════════════════════════════════════════════════════════
//  §13.2.2 Intl.PluralRules.supportedLocalesOf(locales)
// ═══════════════════════════════════════════════════════════════════

fn plural_rules_supported_locales_of(
    _this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let locales_arg = args
        .first()
        .copied()
        .unwrap_or_else(RegisterValue::undefined);
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

fn resolve_plural_rules_options(
    locale: &str,
    options: RegisterValue,
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<PluralRulesData, VmNativeCallError> {
    let plural_type = parse_enum(
        get_option_string(options, "type", runtime)?,
        PluralRulesType::from_str_opt,
        PluralRulesType::Cardinal,
        "type",
        runtime,
    )?;

    let min_int = resolve_int_option(options, "minimumIntegerDigits", 1, 21, 1, runtime)?;

    let min_frac = get_option_number(options, "minimumFractionDigits", runtime)?
        .map(|v| validate_int_range(v, 0, 100, "minimumFractionDigits", runtime))
        .transpose()?;
    let max_frac = get_option_number(options, "maximumFractionDigits", runtime)?
        .map(|v| validate_int_range(v, 0, 100, "maximumFractionDigits", runtime))
        .transpose()?;

    let min_sig = get_option_number(options, "minimumSignificantDigits", runtime)?
        .map(|v| validate_int_range(v, 1, 21, "minimumSignificantDigits", runtime))
        .transpose()?;
    let max_sig = get_option_number(options, "maximumSignificantDigits", runtime)?
        .map(|v| validate_int_range(v, 1, 21, "maximumSignificantDigits", runtime))
        .transpose()?;

    // Defaults: fraction digits 0–3 for cardinal, unless significant digits are set.
    let (min_frac, max_frac) = if min_sig.is_some() || max_sig.is_some() {
        (None, None)
    } else {
        (min_frac.or(Some(0)), max_frac.or(Some(3)))
    };

    Ok(PluralRulesData {
        locale: locale.to_string(),
        plural_type,
        minimum_integer_digits: min_int,
        minimum_fraction_digits: min_frac,
        maximum_fraction_digits: max_frac,
        minimum_significant_digits: min_sig,
        maximum_significant_digits: max_sig,
    })
}

// ═══════════════════════════════════════════════════════════════════
//  Core plural resolution
// ═══════════════════════════════════════════════════════════════════

fn resolve_plural(number: f64, data: &PluralRulesData) -> PluralCategory {
    if number.is_nan() || number.is_infinite() {
        return PluralCategory::Other;
    }

    let locale = Locale::from_str(&data.locale).unwrap_or_else(|_| {
        Locale::from_str(locale_utils::DEFAULT_LOCALE).expect("default locale should parse")
    });

    let rules = match data.plural_type {
        PluralRulesType::Cardinal => IcuPluralRules::try_new_cardinal((&locale).into()),
        PluralRulesType::Ordinal => IcuPluralRules::try_new_ordinal((&locale).into()),
    };

    let Ok(rules) = rules else {
        return PluralCategory::Other;
    };

    // Convert to FixedDecimal for proper plural operands.
    let Ok(decimal) = FixedDecimal::try_from_f64(number.abs(), FloatPrecision::RoundTrip) else {
        return PluralCategory::Other;
    };

    // Apply fraction digit rounding if specified.
    let mut decimal = decimal;
    if let Some(max_frac) = data.maximum_fraction_digits {
        decimal.round(-(max_frac as i16));
    }
    if let Some(min_frac) = data.minimum_fraction_digits {
        decimal.pad_end(-(min_frac as i16));
    }

    rules.category_for(&decimal)
}

fn plural_category_str(cat: PluralCategory) -> &'static str {
    match cat {
        PluralCategory::Zero => "zero",
        PluralCategory::One => "one",
        PluralCategory::Two => "two",
        PluralCategory::Few => "few",
        PluralCategory::Many => "many",
        PluralCategory::Other => "other",
    }
}

// ═══════════════════════════════════════════════════════════════════
//  Enum implementations
// ═══════════════════════════════════════════════════════════════════

impl PluralRulesType {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Cardinal => "cardinal",
            Self::Ordinal => "ordinal",
        }
    }
    fn from_str_opt(s: &str) -> Option<Self> {
        match s {
            "cardinal" => Some(Self::Cardinal),
            "ordinal" => Some(Self::Ordinal),
            _ => None,
        }
    }
}

// ═══════════════════════════════════════════════════════════════════
//  Internal helpers
// ═══════════════════════════════════════════════════════════════════

fn require_plural_rules_data<'a>(
    this: &RegisterValue,
    runtime: &'a crate::interpreter::RuntimeState,
) -> Result<&'a PluralRulesData, VmNativeCallError> {
    let payload = payload::require_intl_payload(this, runtime)
        .map_err(|e| VmNativeCallError::Internal(format!("PluralRules: {e}").into()))?;
    payload.as_plural_rules().ok_or_else(|| {
        VmNativeCallError::Internal("called on incompatible Intl receiver (not PluralRules)".into())
    })
}

fn set_string_prop(
    runtime: &mut crate::interpreter::RuntimeState,
    obj: crate::object::ObjectHandle,
    name: &str,
    value: &str,
) {
    let prop = runtime.intern_property_name(name);
    let s = runtime.alloc_string(value);
    let _ = runtime
        .objects_mut()
        .set_property(obj, prop, RegisterValue::from_object_handle(s.0));
}

fn set_i32_prop(
    runtime: &mut crate::interpreter::RuntimeState,
    obj: crate::object::ObjectHandle,
    name: &str,
    value: i32,
) {
    let prop = runtime.intern_property_name(name);
    let _ = runtime
        .objects_mut()
        .set_property(obj, prop, RegisterValue::from_i32(value));
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
        Some(s) => {
            from_str(&s).ok_or_else(|| range_error(runtime, &format!("Invalid {name} option")))
        }
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

fn range_error(runtime: &mut crate::interpreter::RuntimeState, message: &str) -> VmNativeCallError {
    match runtime.alloc_range_error(message) {
        Ok(err) => VmNativeCallError::Thrown(RegisterValue::from_object_handle(err.0)),
        Err(e) => VmNativeCallError::Internal(format!("RangeError alloc: {e}").into()),
    }
}

fn type_error(runtime: &mut crate::interpreter::RuntimeState, message: &str) -> VmNativeCallError {
    match runtime.alloc_type_error(message) {
        Ok(err) => VmNativeCallError::Thrown(RegisterValue::from_object_handle(err.0)),
        Err(e) => VmNativeCallError::Internal(format!("TypeError alloc: {e}").into()),
    }
}
