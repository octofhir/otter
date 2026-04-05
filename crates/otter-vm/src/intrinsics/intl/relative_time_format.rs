//! Intl.RelativeTimeFormat implementation.
//!
//! Spec: <https://tc39.es/ecma402/#sec-intl-relativetimeformat-constructor>
//!
//! Uses ICU4X `icu_experimental::relativetime::RelativeTimeFormatter`.

use fixed_decimal::Decimal;
use icu_experimental::relativetime::{RelativeTimeFormatter, RelativeTimeFormatterPreferences};
use icu_locale::Locale;
use std::str::FromStr;

use crate::descriptors::{
    JsClassDescriptor, NativeBindingDescriptor, NativeBindingTarget, NativeFunctionDescriptor,
    VmNativeCallError,
};
use crate::value::RegisterValue;

use super::locale_utils;
use super::options_utils::get_option_string;
use super::payload::{
    self, IntlPayload, RelativeTimeFormatData, RelativeTimeNumeric, RelativeTimeStyle,
};

// ═══════════════════════════════════════════════════════════════════
//  Class descriptor
// ═══════════════════════════════════════════════════════════════════

pub fn relative_time_format_class_descriptor() -> JsClassDescriptor {
    JsClassDescriptor::new("RelativeTimeFormat")
        .with_constructor(NativeFunctionDescriptor::constructor(
            "RelativeTimeFormat",
            0,
            rtf_constructor,
        ))
        .with_binding(NativeBindingDescriptor::new(
            NativeBindingTarget::Prototype,
            NativeFunctionDescriptor::method("format", 2, rtf_format),
        ))
        .with_binding(NativeBindingDescriptor::new(
            NativeBindingTarget::Prototype,
            NativeFunctionDescriptor::method("formatToParts", 2, rtf_format_to_parts),
        ))
        .with_binding(NativeBindingDescriptor::new(
            NativeBindingTarget::Prototype,
            NativeFunctionDescriptor::method("resolvedOptions", 0, rtf_resolved_options),
        ))
        .with_binding(NativeBindingDescriptor::new(
            NativeBindingTarget::Constructor,
            NativeFunctionDescriptor::method("supportedLocalesOf", 1, rtf_supported_locales_of),
        ))
}

// ═══════════════════════════════════════════════════════════════════
//  §17.1.1 Intl.RelativeTimeFormat(locales, options)
// ═══════════════════════════════════════════════════════════════════

fn rtf_constructor(
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

    let style = parse_enum(
        get_option_string(options_arg, "style", runtime)?,
        RelativeTimeStyle::from_str_opt,
        RelativeTimeStyle::Long,
        "style",
        runtime,
    )?;

    let numeric = parse_enum(
        get_option_string(options_arg, "numeric", runtime)?,
        RelativeTimeNumeric::from_str_opt,
        RelativeTimeNumeric::Always,
        "numeric",
        runtime,
    )?;

    let data = RelativeTimeFormatData {
        locale,
        style,
        numeric,
    };

    let prototype = runtime.intrinsics().intl_relative_time_format_prototype();
    let handle = payload::construct_intl(IntlPayload::RelativeTimeFormat(data), prototype, runtime);
    Ok(RegisterValue::from_object_handle(handle.0))
}

// ═══════════════════════════════════════════════════════════════════
//  §17.3.3 Intl.RelativeTimeFormat.prototype.format(value, unit)
// ═══════════════════════════════════════════════════════════════════

fn rtf_format(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let data = require_rtf_data(this, runtime)?.clone();
    let value_arg = args
        .first()
        .copied()
        .unwrap_or_else(RegisterValue::undefined);
    let unit_arg = args
        .get(1)
        .copied()
        .unwrap_or_else(RegisterValue::undefined);

    let value = runtime
        .js_to_number(value_arg)
        .map_err(|e| VmNativeCallError::Internal(format!("RelativeTimeFormat: {e}").into()))?;
    let unit_str = runtime
        .js_to_string(unit_arg)
        .map_err(|e| VmNativeCallError::Internal(format!("RelativeTimeFormat: {e}").into()))?;

    let unit = parse_relative_time_unit(&unit_str)
        .ok_or_else(|| range_error(runtime, &format!("Invalid unit: {unit_str}")))?;

    let formatted = perform_relative_time_format(value, unit, &data);
    let handle = runtime.alloc_string(formatted);
    Ok(RegisterValue::from_object_handle(handle.0))
}

// ═══════════════════════════════════════════════════════════════════
//  §17.3.4 Intl.RelativeTimeFormat.prototype.formatToParts(value, unit)
// ═══════════════════════════════════════════════════════════════════

fn rtf_format_to_parts(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let data = require_rtf_data(this, runtime)?.clone();
    let value_arg = args
        .first()
        .copied()
        .unwrap_or_else(RegisterValue::undefined);
    let unit_arg = args
        .get(1)
        .copied()
        .unwrap_or_else(RegisterValue::undefined);

    let value = runtime
        .js_to_number(value_arg)
        .map_err(|e| VmNativeCallError::Internal(format!("RelativeTimeFormat: {e}").into()))?;
    let unit_str = runtime
        .js_to_string(unit_arg)
        .map_err(|e| VmNativeCallError::Internal(format!("RelativeTimeFormat: {e}").into()))?;

    let unit = parse_relative_time_unit(&unit_str)
        .ok_or_else(|| range_error(runtime, &format!("Invalid unit: {unit_str}")))?;

    let formatted = perform_relative_time_format(value, unit, &data);

    // Simple decomposition: split into integer + literal parts.
    let parts = decompose_relative_time(&formatted, value, &unit_str);
    let arr = runtime.alloc_array();
    for (part_type, part_value, part_unit) in &parts {
        let obj = runtime.alloc_object();
        set_string_prop(runtime, obj, "type", part_type);
        set_string_prop(runtime, obj, "value", part_value);
        if !part_unit.is_empty() {
            set_string_prop(runtime, obj, "unit", part_unit);
        }
        runtime
            .objects_mut()
            .push_element(arr, RegisterValue::from_object_handle(obj.0))
            .map_err(|e| VmNativeCallError::Internal(format!("formatToParts: {e:?}").into()))?;
    }
    Ok(RegisterValue::from_object_handle(arr.0))
}

// ═══════════════════════════════════════════════════════════════════
//  §17.3.5 Intl.RelativeTimeFormat.prototype.resolvedOptions()
// ═══════════════════════════════════════════════════════════════════

fn rtf_resolved_options(
    this: &RegisterValue,
    _args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let data = require_rtf_data(this, runtime)?.clone();
    let obj = runtime.alloc_object();
    set_string_prop(runtime, obj, "locale", &data.locale);
    set_string_prop(runtime, obj, "style", data.style.as_str());
    set_string_prop(runtime, obj, "numeric", data.numeric.as_str());
    set_string_prop(runtime, obj, "numberingSystem", "latn");
    Ok(RegisterValue::from_object_handle(obj.0))
}

// ═══════════════════════════════════════════════════════════════════
//  supportedLocalesOf()
// ═══════════════════════════════════════════════════════════════════

fn rtf_supported_locales_of(
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
//  ICU4X relative time formatting
// ═══════════════════════════════════════════════════════════════════

#[derive(Debug, Clone, Copy)]
enum RelativeTimeUnit {
    Second,
    Minute,
    Hour,
    Day,
    Week,
    Month,
    Quarter,
    Year,
}

fn parse_relative_time_unit(s: &str) -> Option<RelativeTimeUnit> {
    match s {
        "second" | "seconds" => Some(RelativeTimeUnit::Second),
        "minute" | "minutes" => Some(RelativeTimeUnit::Minute),
        "hour" | "hours" => Some(RelativeTimeUnit::Hour),
        "day" | "days" => Some(RelativeTimeUnit::Day),
        "week" | "weeks" => Some(RelativeTimeUnit::Week),
        "month" | "months" => Some(RelativeTimeUnit::Month),
        "quarter" | "quarters" => Some(RelativeTimeUnit::Quarter),
        "year" | "years" => Some(RelativeTimeUnit::Year),
        _ => None,
    }
}

fn perform_relative_time_format(
    value: f64,
    unit: RelativeTimeUnit,
    data: &RelativeTimeFormatData,
) -> String {
    let locale = Locale::from_str(&data.locale).unwrap_or_else(|_| {
        Locale::from_str(locale_utils::DEFAULT_LOCALE).expect("default locale should parse")
    });
    let prefs = RelativeTimeFormatterPreferences::from(&locale);

    // ICU4X uses FixedDecimal; convert the f64 value.
    let int_val = value as i64;
    let decimal = Decimal::from(int_val);

    // Create the appropriate formatter based on style + unit.
    macro_rules! try_format {
        ($long:ident, $short:ident, $narrow:ident) => {
            match data.style {
                RelativeTimeStyle::Long => {
                    if let Ok(fmt) = RelativeTimeFormatter::$long(prefs, Default::default()) {
                        return fmt.format(decimal.clone()).to_string();
                    }
                }
                RelativeTimeStyle::Short => {
                    if let Ok(fmt) = RelativeTimeFormatter::$short(prefs, Default::default()) {
                        return fmt.format(decimal.clone()).to_string();
                    }
                }
                RelativeTimeStyle::Narrow => {
                    if let Ok(fmt) = RelativeTimeFormatter::$narrow(prefs, Default::default()) {
                        return fmt.format(decimal.clone()).to_string();
                    }
                }
            }
        };
    }

    match unit {
        RelativeTimeUnit::Second => try_format!(
            try_new_long_second,
            try_new_short_second,
            try_new_narrow_second
        ),
        RelativeTimeUnit::Minute => try_format!(
            try_new_long_minute,
            try_new_short_minute,
            try_new_narrow_minute
        ),
        RelativeTimeUnit::Hour => {
            try_format!(try_new_long_hour, try_new_short_hour, try_new_narrow_hour)
        }
        RelativeTimeUnit::Day => {
            try_format!(try_new_long_day, try_new_short_day, try_new_narrow_day)
        }
        RelativeTimeUnit::Week => {
            try_format!(try_new_long_week, try_new_short_week, try_new_narrow_week)
        }
        RelativeTimeUnit::Month => try_format!(
            try_new_long_month,
            try_new_short_month,
            try_new_narrow_month
        ),
        RelativeTimeUnit::Quarter => try_format!(
            try_new_long_quarter,
            try_new_short_quarter,
            try_new_narrow_quarter
        ),
        RelativeTimeUnit::Year => {
            try_format!(try_new_long_year, try_new_short_year, try_new_narrow_year)
        }
    }

    // Fallback if ICU4X fails.
    let abs_val = int_val.unsigned_abs();
    let unit_name = match unit {
        RelativeTimeUnit::Second => "second",
        RelativeTimeUnit::Minute => "minute",
        RelativeTimeUnit::Hour => "hour",
        RelativeTimeUnit::Day => "day",
        RelativeTimeUnit::Week => "week",
        RelativeTimeUnit::Month => "month",
        RelativeTimeUnit::Quarter => "quarter",
        RelativeTimeUnit::Year => "year",
    };
    let plural = if abs_val == 1 { "" } else { "s" };
    if int_val < 0 {
        format!("{abs_val} {unit_name}{plural} ago")
    } else {
        format!("in {abs_val} {unit_name}{plural}")
    }
}

/// Simple decomposition of formatted relative time into parts.
fn decompose_relative_time(
    formatted: &str,
    value: f64,
    unit: &str,
) -> Vec<(&'static str, String, String)> {
    let int_val = value as i64;
    let abs_str = int_val.unsigned_abs().to_string();

    // Try to find the number in the formatted string.
    if let Some(pos) = formatted.find(&abs_str) {
        let mut parts = Vec::new();
        if pos > 0 {
            parts.push(("literal", formatted[..pos].to_string(), String::new()));
        }
        parts.push(("integer", abs_str, unit.to_string()));
        let after = pos + int_val.unsigned_abs().to_string().len();
        if after < formatted.len() {
            parts.push(("literal", formatted[after..].to_string(), String::new()));
        }
        parts
    } else {
        vec![("literal", formatted.to_string(), String::new())]
    }
}

// ═══════════════════════════════════════════════════════════════════
//  Enum implementations
// ═══════════════════════════════════════════════════════════════════

impl RelativeTimeStyle {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Long => "long",
            Self::Short => "short",
            Self::Narrow => "narrow",
        }
    }
    pub fn from_str_opt(s: &str) -> Option<Self> {
        match s {
            "long" => Some(Self::Long),
            "short" => Some(Self::Short),
            "narrow" => Some(Self::Narrow),
            _ => None,
        }
    }
}

impl RelativeTimeNumeric {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Always => "always",
            Self::Auto => "auto",
        }
    }
    pub fn from_str_opt(s: &str) -> Option<Self> {
        match s {
            "always" => Some(Self::Always),
            "auto" => Some(Self::Auto),
            _ => None,
        }
    }
}

// ═══════════════════════════════════════════════════════════════════
//  Internal helpers
// ═══════════════════════════════════════════════════════════════════

fn require_rtf_data<'a>(
    this: &RegisterValue,
    runtime: &'a crate::interpreter::RuntimeState,
) -> Result<&'a RelativeTimeFormatData, VmNativeCallError> {
    let payload = payload::require_intl_payload(this, runtime)
        .map_err(|e| VmNativeCallError::Internal(format!("RelativeTimeFormat: {e}").into()))?;
    payload.as_relative_time_format().ok_or_else(|| {
        VmNativeCallError::Internal(
            "called on incompatible Intl receiver (not RelativeTimeFormat)".into(),
        )
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

fn range_error(runtime: &mut crate::interpreter::RuntimeState, message: &str) -> VmNativeCallError {
    match runtime.alloc_range_error(message) {
        Ok(err) => VmNativeCallError::Thrown(RegisterValue::from_object_handle(err.0)),
        Err(e) => VmNativeCallError::Internal(format!("RangeError alloc: {e}").into()),
    }
}
