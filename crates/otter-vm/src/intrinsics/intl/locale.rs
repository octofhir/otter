//! Intl.Locale implementation.
//!
//! Spec: <https://tc39.es/ecma402/#sec-intl-locale-constructor>

use icu_locale::{Locale, LocaleExpander};
use std::str::FromStr;

use crate::descriptors::{
    JsClassDescriptor, NativeBindingDescriptor, NativeBindingTarget, NativeFunctionDescriptor,
    VmNativeCallError,
};
use crate::value::RegisterValue;

use super::locale_utils;
use super::options_utils::get_option_string;
use super::payload::{self, IntlPayload, LocaleData};

// ═══════════════════════════════════════════════════════════════════
//  Class descriptor
// ═══════════════════════════════════════════════════════════════════

pub fn locale_class_descriptor() -> JsClassDescriptor {
    JsClassDescriptor::new("Locale")
        .with_constructor(NativeFunctionDescriptor::constructor(
            "Locale",
            1,
            locale_constructor,
        ))
        .with_binding(NativeBindingDescriptor::new(
            NativeBindingTarget::Prototype,
            NativeFunctionDescriptor::method("toString", 0, locale_to_string),
        ))
        .with_binding(NativeBindingDescriptor::new(
            NativeBindingTarget::Prototype,
            NativeFunctionDescriptor::method("maximize", 0, locale_maximize),
        ))
        .with_binding(NativeBindingDescriptor::new(
            NativeBindingTarget::Prototype,
            NativeFunctionDescriptor::method("minimize", 0, locale_minimize),
        ))
        // Accessors implemented as getter methods.
        .with_binding(NativeBindingDescriptor::new(
            NativeBindingTarget::Prototype,
            NativeFunctionDescriptor::method("language", 0, locale_language_getter),
        ))
        .with_binding(NativeBindingDescriptor::new(
            NativeBindingTarget::Prototype,
            NativeFunctionDescriptor::method("script", 0, locale_script_getter),
        ))
        .with_binding(NativeBindingDescriptor::new(
            NativeBindingTarget::Prototype,
            NativeFunctionDescriptor::method("region", 0, locale_region_getter),
        ))
        .with_binding(NativeBindingDescriptor::new(
            NativeBindingTarget::Prototype,
            NativeFunctionDescriptor::method("baseName", 0, locale_base_name_getter),
        ))
        .with_binding(NativeBindingDescriptor::new(
            NativeBindingTarget::Prototype,
            NativeFunctionDescriptor::method("calendar", 0, locale_calendar_getter),
        ))
        .with_binding(NativeBindingDescriptor::new(
            NativeBindingTarget::Prototype,
            NativeFunctionDescriptor::method("collation", 0, locale_collation_getter),
        ))
        .with_binding(NativeBindingDescriptor::new(
            NativeBindingTarget::Prototype,
            NativeFunctionDescriptor::method("numberingSystem", 0, locale_numbering_system_getter),
        ))
}

// ═══════════════════════════════════════════════════════════════════
//  §14.1.1 Intl.Locale(tag, options)
// ═══════════════════════════════════════════════════════════════════

fn locale_constructor(
    _this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let tag_val = args.first().copied().unwrap_or_else(RegisterValue::undefined);
    let options_arg = args.get(1).copied().unwrap_or_else(RegisterValue::undefined);

    if tag_val == RegisterValue::undefined() {
        return Err(type_error(runtime, "First argument to Intl.Locale must be a string or Locale object"));
    }

    // Coerce tag to string.
    let tag_str = runtime
        .js_to_string(tag_val)
        .map_err(|e| VmNativeCallError::Internal(format!("Intl.Locale: {e}").into()))?;

    // Canonicalize.
    let canonical = locale_utils::canonicalize_locale_tag(&tag_str)
        .map_err(|_| range_error(runtime, &format!("Invalid language tag: {tag_str}")))?;

    // Apply options overrides.
    let calendar = get_option_string(options_arg, "calendar", runtime)?;
    let collation = get_option_string(options_arg, "collation", runtime)?;
    let numbering_system = get_option_string(options_arg, "numberingSystem", runtime)?;

    // Build locale tag with options applied as -u- extension keywords.
    let locale_str = apply_unicode_extensions(&canonical, &calendar, &collation, &numbering_system);

    let data = LocaleData {
        locale: locale_str,
        calendar,
        collation,
        numbering_system,
    };

    let prototype = runtime.intrinsics().intl_locale_prototype();
    let handle = payload::construct_intl(IntlPayload::Locale(data), prototype, runtime);

    Ok(RegisterValue::from_object_handle(handle.0))
}

// ═══════════════════════════════════════════════════════════════════
//  §14.3.5 Intl.Locale.prototype.toString()
// ═══════════════════════════════════════════════════════════════════

fn locale_to_string(
    this: &RegisterValue,
    _args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let data = require_locale_data(this, runtime)?;
    let locale_str = data.locale.clone();
    let handle = runtime.alloc_string(locale_str);
    Ok(RegisterValue::from_object_handle(handle.0))
}

// ═══════════════════════════════════════════════════════════════════
//  §14.3.6 Intl.Locale.prototype.maximize()
// ═══════════════════════════════════════════════════════════════════

fn locale_maximize(
    this: &RegisterValue,
    _args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let data = require_locale_data(this, runtime)?.clone();

    // Use icu_locale's LocaleExpander to maximize.
    let maximized = match Locale::from_str(&data.locale) {
        Ok(mut loc) => {
            let expander = LocaleExpander::new_common();
            expander.maximize(&mut loc.id);
            loc.to_string()
        }
        Err(_) => data.locale.clone(),
    };

    let new_data = LocaleData {
        locale: maximized,
        calendar: data.calendar.clone(),
        collation: data.collation.clone(),
        numbering_system: data.numbering_system.clone(),
    };

    let prototype = runtime.intrinsics().intl_locale_prototype();
    let handle = payload::construct_intl(IntlPayload::Locale(new_data), prototype, runtime);
    Ok(RegisterValue::from_object_handle(handle.0))
}

// ═══════════════════════════════════════════════════════════════════
//  §14.3.7 Intl.Locale.prototype.minimize()
// ═══════════════════════════════════════════════════════════════════

fn locale_minimize(
    this: &RegisterValue,
    _args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let data = require_locale_data(this, runtime)?.clone();

    let minimized = match Locale::from_str(&data.locale) {
        Ok(mut loc) => {
            let expander = LocaleExpander::new_common();
            expander.minimize(&mut loc.id);
            loc.to_string()
        }
        Err(_) => data.locale.clone(),
    };

    let new_data = LocaleData {
        locale: minimized,
        calendar: data.calendar.clone(),
        collation: data.collation.clone(),
        numbering_system: data.numbering_system.clone(),
    };

    let prototype = runtime.intrinsics().intl_locale_prototype();
    let handle = payload::construct_intl(IntlPayload::Locale(new_data), prototype, runtime);
    Ok(RegisterValue::from_object_handle(handle.0))
}

// ═══════════════════════════════════════════════════════════════════
//  Accessor getters
// ═══════════════════════════════════════════════════════════════════

fn locale_language_getter(
    this: &RegisterValue,
    _args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let data = require_locale_data(this, runtime)?;
    let tag = &data.locale;
    // Language is the first subtag before any '-'.
    let language = tag.split('-').next().unwrap_or(tag).to_string();
    let handle = runtime.alloc_string(language);
    Ok(RegisterValue::from_object_handle(handle.0))
}

fn locale_script_getter(
    this: &RegisterValue,
    _args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let data = require_locale_data(this, runtime)?;
    let script = extract_subtag_script(&data.locale).map(str::to_string);
    match script {
        Some(s) => {
            let handle = runtime.alloc_string(s);
            Ok(RegisterValue::from_object_handle(handle.0))
        }
        None => Ok(RegisterValue::undefined()),
    }
}

fn locale_region_getter(
    this: &RegisterValue,
    _args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let data = require_locale_data(this, runtime)?;
    let region = extract_subtag_region(&data.locale).map(str::to_string);
    match region {
        Some(r) => {
            let handle = runtime.alloc_string(r);
            Ok(RegisterValue::from_object_handle(handle.0))
        }
        None => Ok(RegisterValue::undefined()),
    }
}

fn locale_base_name_getter(
    this: &RegisterValue,
    _args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let data = require_locale_data(this, runtime)?;
    let base = extract_base_name(&data.locale).to_string();
    let handle = runtime.alloc_string(base);
    Ok(RegisterValue::from_object_handle(handle.0))
}

fn locale_calendar_getter(
    this: &RegisterValue,
    _args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let data = require_locale_data(this, runtime)?;
    let calendar = data.calendar.clone();
    match calendar {
        Some(c) => {
            let handle = runtime.alloc_string(c);
            Ok(RegisterValue::from_object_handle(handle.0))
        }
        None => Ok(RegisterValue::undefined()),
    }
}

fn locale_collation_getter(
    this: &RegisterValue,
    _args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let data = require_locale_data(this, runtime)?;
    let collation = data.collation.clone();
    match collation {
        Some(c) => {
            let handle = runtime.alloc_string(c);
            Ok(RegisterValue::from_object_handle(handle.0))
        }
        None => Ok(RegisterValue::undefined()),
    }
}

fn locale_numbering_system_getter(
    this: &RegisterValue,
    _args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let data = require_locale_data(this, runtime)?;
    let ns = data.numbering_system.clone();
    match ns {
        Some(ns) => {
            let handle = runtime.alloc_string(ns);
            Ok(RegisterValue::from_object_handle(handle.0))
        }
        None => Ok(RegisterValue::undefined()),
    }
}

// ═══════════════════════════════════════════════════════════════════
//  Locale tag helpers
// ═══════════════════════════════════════════════════════════════════

/// Applies Unicode extension keywords (-u-ca, -u-co, -u-nu) to a locale tag.
fn apply_unicode_extensions(
    tag: &str,
    calendar: &Option<String>,
    collation: &Option<String>,
    numbering_system: &Option<String>,
) -> String {
    if calendar.is_none() && collation.is_none() && numbering_system.is_none() {
        return tag.to_string();
    }

    // Strip existing -u- extension if present, then rebuild.
    let base = strip_unicode_extension(tag);
    let mut ext = String::from("-u");
    if let Some(ca) = calendar {
        ext.push_str("-ca-");
        ext.push_str(ca);
    }
    if let Some(co) = collation {
        ext.push_str("-co-");
        ext.push_str(co);
    }
    if let Some(nu) = numbering_system {
        ext.push_str("-nu-");
        ext.push_str(nu);
    }
    format!("{base}{ext}")
}

fn strip_unicode_extension(tag: &str) -> &str {
    // Find "-u-" and strip it and everything after it (simplified).
    if let Some(pos) = tag.find("-u-") {
        // Check if there's a non-unicode extension after (e.g., -t-, -x-).
        let rest = &tag[pos + 3..];
        // Find next singleton extension.
        for (i, _) in rest.match_indices('-') {
            let after = &rest[i + 1..];
            if after.len() >= 2 && after.as_bytes()[1] == b'-' && locale_utils::is_singleton(&after[..1]) {
                // Found next extension, return base + that.
                return &tag[..pos + 3 + i];
            }
        }
        &tag[..pos]
    } else {
        tag
    }
}

/// Extract the script subtag from a BCP 47 tag (4-letter, title-case).
fn extract_subtag_script(tag: &str) -> Option<&str> {
    let parts: Vec<&str> = tag.split('-').collect();
    // Language-Script or Language-Script-Region pattern.
    if parts.len() >= 2 && locale_utils::is_script(parts[1]) {
        return Some(parts[1]);
    }
    None
}

/// Extract the region subtag from a BCP 47 tag (2-letter uppercase or 3 digits).
fn extract_subtag_region(tag: &str) -> Option<&str> {
    let parts: Vec<&str> = tag.split('-').collect();
    parts[1..].iter().find(|p| locale_utils::is_region(p)).copied()
}

/// Extract the base name (language[-script][-region][-variant]*) without extensions.
fn extract_base_name(tag: &str) -> &str {
    // Extensions start with singleton-dash (e.g., -u-, -t-, -x-).
    let parts: Vec<&str> = tag.split('-').collect();
    let mut end = parts.len();
    for (i, part) in parts.iter().enumerate() {
        if i > 0 && part.len() == 1 && locale_utils::is_singleton(part) {
            end = i;
            break;
        }
    }
    let base_parts = &parts[..end];
    // Find the byte offset in the original string.
    if end == parts.len() {
        // Check for -u- style extension that might not be at a part boundary.
        tag
    } else {
        let total_len: usize = base_parts.iter().map(|p| p.len()).sum::<usize>() + (end - 1);
        &tag[..total_len]
    }
}

// ═══════════════════════════════════════════════════════════════════
//  Internal helpers
// ═══════════════════════════════════════════════════════════════════

fn require_locale_data<'a>(
    this: &RegisterValue,
    runtime: &'a crate::interpreter::RuntimeState,
) -> Result<&'a LocaleData, VmNativeCallError> {
    let payload = payload::require_intl_payload(this, runtime).map_err(|e| {
        VmNativeCallError::Internal(format!("Locale: {e}").into())
    })?;
    payload.as_locale().ok_or_else(|| {
        VmNativeCallError::Internal(
            "called on incompatible Intl receiver (not Locale)".into(),
        )
    })
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
