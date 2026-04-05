//! Intl.ListFormat implementation.
//!
//! Spec: <https://tc39.es/ecma402/#sec-intl-listformat-constructor>

use icu_list::options::{ListFormatterOptions, ListLength};
use icu_list::{ListFormatter, ListFormatterPreferences};
use icu_locale::Locale;
use std::str::FromStr;

use crate::descriptors::{
    JsClassDescriptor, NativeBindingDescriptor, NativeBindingTarget, NativeFunctionDescriptor,
    VmNativeCallError,
};
use crate::value::RegisterValue;

use super::locale_utils;
use super::options_utils::get_option_string;
use super::payload::{self, IntlPayload, ListFormatData, ListFormatStyle, ListFormatType};

// ═══════════════════════════════════════════════════════════════════
//  Class descriptor
// ═══════════════════════════════════════════════════════════════════

pub fn list_format_class_descriptor() -> JsClassDescriptor {
    JsClassDescriptor::new("ListFormat")
        .with_constructor(NativeFunctionDescriptor::constructor(
            "ListFormat",
            0,
            list_format_constructor,
        ))
        .with_binding(NativeBindingDescriptor::new(
            NativeBindingTarget::Prototype,
            NativeFunctionDescriptor::method("format", 1, list_format_format),
        ))
        .with_binding(NativeBindingDescriptor::new(
            NativeBindingTarget::Prototype,
            NativeFunctionDescriptor::method("formatToParts", 1, list_format_format_to_parts),
        ))
        .with_binding(NativeBindingDescriptor::new(
            NativeBindingTarget::Prototype,
            NativeFunctionDescriptor::method("resolvedOptions", 0, list_format_resolved_options),
        ))
        .with_binding(NativeBindingDescriptor::new(
            NativeBindingTarget::Constructor,
            NativeFunctionDescriptor::method(
                "supportedLocalesOf",
                1,
                list_format_supported_locales_of,
            ),
        ))
}

// ═══════════════════════════════════════════════════════════════════
//  §13.1.1 Intl.ListFormat(locales, options)
// ═══════════════════════════════════════════════════════════════════

fn list_format_constructor(
    _this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let locales_arg = args.first().copied().unwrap_or_else(RegisterValue::undefined);
    let options_arg = args.get(1).copied().unwrap_or_else(RegisterValue::undefined);

    let locale = super::resolve_locale(locales_arg, runtime)?;

    let list_type = parse_enum(
        get_option_string(options_arg, "type", runtime)?,
        ListFormatType::from_str_opt,
        ListFormatType::Conjunction,
        "type",
        runtime,
    )?;

    let style = parse_enum(
        get_option_string(options_arg, "style", runtime)?,
        ListFormatStyle::from_str_opt,
        ListFormatStyle::Long,
        "style",
        runtime,
    )?;

    let data = ListFormatData {
        locale,
        list_type,
        style,
    };

    let prototype = runtime.intrinsics().intl_list_format_prototype();
    let handle = payload::construct_intl(IntlPayload::ListFormat(data), prototype, runtime);
    Ok(RegisterValue::from_object_handle(handle.0))
}

// ═══════════════════════════════════════════════════════════════════
//  §13.3.4 Intl.ListFormat.prototype.format(list)
// ═══════════════════════════════════════════════════════════════════

fn list_format_format(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let data = require_list_format_data(this, runtime)?.clone();
    let list_arg = args.first().copied().unwrap_or_else(RegisterValue::undefined);

    let strings = extract_string_list(list_arg, runtime)?;
    let formatted = perform_list_format(&strings, &data);

    let handle = runtime.alloc_string(formatted);
    Ok(RegisterValue::from_object_handle(handle.0))
}

// ═══════════════════════════════════════════════════════════════════
//  §13.3.5 Intl.ListFormat.prototype.formatToParts(list)
// ═══════════════════════════════════════════════════════════════════

fn list_format_format_to_parts(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let data = require_list_format_data(this, runtime)?.clone();
    let list_arg = args.first().copied().unwrap_or_else(RegisterValue::undefined);

    let strings = extract_string_list(list_arg, runtime)?;
    let formatted = perform_list_format(&strings, &data);

    // Decompose into parts: identify element vs literal segments.
    let parts = decompose_list_parts(&formatted, &strings);

    let arr = runtime.alloc_array();
    for (part_type, value) in &parts {
        let obj = runtime.alloc_object();
        set_string_prop(runtime, obj, "type", part_type);
        set_string_prop(runtime, obj, "value", value);
        runtime
            .objects_mut()
            .push_element(arr, RegisterValue::from_object_handle(obj.0))
            .map_err(|e| VmNativeCallError::Internal(format!("formatToParts: {e:?}").into()))?;
    }
    Ok(RegisterValue::from_object_handle(arr.0))
}

// ═══════════════════════════════════════════════════════════════════
//  §13.3.6 Intl.ListFormat.prototype.resolvedOptions()
// ═══════════════════════════════════════════════════════════════════

fn list_format_resolved_options(
    this: &RegisterValue,
    _args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let data = require_list_format_data(this, runtime)?.clone();
    let obj = runtime.alloc_object();
    set_string_prop(runtime, obj, "locale", &data.locale);
    set_string_prop(runtime, obj, "type", data.list_type.as_str());
    set_string_prop(runtime, obj, "style", data.style.as_str());
    Ok(RegisterValue::from_object_handle(obj.0))
}

// ═══════════════════════════════════════════════════════════════════
//  §13.2.2 Intl.ListFormat.supportedLocalesOf(locales)
// ═══════════════════════════════════════════════════════════════════

fn list_format_supported_locales_of(
    _this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let locales_arg = args.first().copied().unwrap_or_else(RegisterValue::undefined);
    let locale_list = super::canonicalize_locale_list_from_value(locales_arg, runtime)?;
    let arr = runtime.alloc_array();
    for locale in &locale_list {
        let s = runtime.alloc_string(locale.as_str());
        runtime
            .objects_mut()
            .push_element(arr, RegisterValue::from_object_handle(s.0))
            .map_err(|e| VmNativeCallError::Internal(format!("supportedLocalesOf: {e:?}").into()))?;
    }
    Ok(RegisterValue::from_object_handle(arr.0))
}

// ═══════════════════════════════════════════════════════════════════
//  ICU4X list formatting
// ═══════════════════════════════════════════════════════════════════

fn perform_list_format(strings: &[String], data: &ListFormatData) -> String {
    let locale = Locale::from_str(&data.locale).unwrap_or_else(|_| {
        Locale::from_str(locale_utils::DEFAULT_LOCALE).expect("default locale should parse")
    });
    let prefs = ListFormatterPreferences::from(&locale);

    let length = match data.style {
        ListFormatStyle::Long => ListLength::Wide,
        ListFormatStyle::Short => ListLength::Short,
        ListFormatStyle::Narrow => ListLength::Narrow,
    };
    let mut options = ListFormatterOptions::default();
    options.length = Some(length);

    match data.list_type {
        ListFormatType::Conjunction => {
            let fmt = ListFormatter::try_new_and(prefs, options)
                .unwrap_or_else(|_| ListFormatter::try_new_and(Default::default(), options).unwrap());
            fmt.format(strings.iter().map(|s| s.as_str())).to_string()
        }
        ListFormatType::Disjunction => {
            let fmt = ListFormatter::try_new_or(prefs, options)
                .unwrap_or_else(|_| ListFormatter::try_new_or(Default::default(), options).unwrap());
            fmt.format(strings.iter().map(|s| s.as_str())).to_string()
        }
        ListFormatType::Unit => {
            let fmt = ListFormatter::try_new_unit(prefs, options)
                .unwrap_or_else(|_| ListFormatter::try_new_unit(Default::default(), options).unwrap());
            fmt.format(strings.iter().map(|s| s.as_str())).to_string()
        }
    }
}

/// Decompose formatted list into element/literal parts.
fn decompose_list_parts(formatted: &str, elements: &[String]) -> Vec<(&'static str, String)> {
    let mut parts = Vec::new();
    let mut remaining = formatted;

    for (i, elem) in elements.iter().enumerate() {
        if let Some(pos) = remaining.find(elem.as_str()) {
            if pos > 0 {
                parts.push(("literal", remaining[..pos].to_string()));
            }
            parts.push(("element", elem.clone()));
            remaining = &remaining[pos + elem.len()..];
        } else {
            // Element not found literally — append what's left.
            if i == elements.len() - 1 && !remaining.is_empty() {
                parts.push(("literal", remaining.to_string()));
                remaining = "";
            }
        }
    }
    if !remaining.is_empty() {
        parts.push(("literal", remaining.to_string()));
    }
    parts
}

// ═══════════════════════════════════════════════════════════════════
//  Enum implementations
// ═══════════════════════════════════════════════════════════════════

impl ListFormatType {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Conjunction => "conjunction",
            Self::Disjunction => "disjunction",
            Self::Unit => "unit",
        }
    }
    pub fn from_str_opt(s: &str) -> Option<Self> {
        match s {
            "conjunction" => Some(Self::Conjunction),
            "disjunction" => Some(Self::Disjunction),
            "unit" => Some(Self::Unit),
            _ => None,
        }
    }
}

impl ListFormatStyle {
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

// ═══════════════════════════════════════════════════════════════════
//  Internal helpers
// ═══════════════════════════════════════════════════════════════════

fn extract_string_list(
    list_arg: RegisterValue,
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<Vec<String>, VmNativeCallError> {
    if list_arg == RegisterValue::undefined() {
        return Ok(Vec::new());
    }
    let handle = list_arg
        .as_object_handle()
        .map(crate::object::ObjectHandle)
        .ok_or_else(|| {
            VmNativeCallError::Internal("ListFormat: expected iterable".into())
        })?;

    let elements = runtime.list_from_array_like(handle)?;
    let mut strings = Vec::new();
    for elem in elements {
        let s = runtime.js_to_string(elem).map_err(|e| {
            VmNativeCallError::Internal(format!("ListFormat: {e}").into())
        })?;
        strings.push(s.to_string());
    }
    Ok(strings)
}

fn require_list_format_data<'a>(
    this: &RegisterValue,
    runtime: &'a crate::interpreter::RuntimeState,
) -> Result<&'a ListFormatData, VmNativeCallError> {
    let payload = payload::require_intl_payload(this, runtime).map_err(|e| {
        VmNativeCallError::Internal(format!("ListFormat: {e}").into())
    })?;
    payload.as_list_format().ok_or_else(|| {
        VmNativeCallError::Internal(
            "called on incompatible Intl receiver (not ListFormat)".into(),
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
    let _ = runtime.objects_mut().set_property(
        obj,
        prop,
        RegisterValue::from_object_handle(s.0),
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

fn range_error(
    runtime: &mut crate::interpreter::RuntimeState,
    message: &str,
) -> VmNativeCallError {
    match runtime.alloc_range_error(message) {
        Ok(err) => VmNativeCallError::Thrown(RegisterValue::from_object_handle(err.0)),
        Err(e) => VmNativeCallError::Internal(format!("RangeError alloc: {e}").into()),
    }
}
