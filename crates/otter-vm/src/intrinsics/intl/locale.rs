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
use super::options_utils::{get_option_bool, get_option_string};
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
        // §14.3.3-14.3.12 Accessor getter properties (spec: get Intl.Locale.prototype.X)
        .with_binding(NativeBindingDescriptor::new(
            NativeBindingTarget::Prototype,
            NativeFunctionDescriptor::getter("language", locale_language_getter),
        ))
        .with_binding(NativeBindingDescriptor::new(
            NativeBindingTarget::Prototype,
            NativeFunctionDescriptor::getter("script", locale_script_getter),
        ))
        .with_binding(NativeBindingDescriptor::new(
            NativeBindingTarget::Prototype,
            NativeFunctionDescriptor::getter("region", locale_region_getter),
        ))
        .with_binding(NativeBindingDescriptor::new(
            NativeBindingTarget::Prototype,
            NativeFunctionDescriptor::getter("baseName", locale_base_name_getter),
        ))
        .with_binding(NativeBindingDescriptor::new(
            NativeBindingTarget::Prototype,
            NativeFunctionDescriptor::getter("calendar", locale_calendar_getter),
        ))
        .with_binding(NativeBindingDescriptor::new(
            NativeBindingTarget::Prototype,
            NativeFunctionDescriptor::getter("collation", locale_collation_getter),
        ))
        .with_binding(NativeBindingDescriptor::new(
            NativeBindingTarget::Prototype,
            NativeFunctionDescriptor::getter("numberingSystem", locale_numbering_system_getter),
        ))
        .with_binding(NativeBindingDescriptor::new(
            NativeBindingTarget::Prototype,
            NativeFunctionDescriptor::getter("hourCycle", locale_hour_cycle_getter),
        ))
        .with_binding(NativeBindingDescriptor::new(
            NativeBindingTarget::Prototype,
            NativeFunctionDescriptor::getter("caseFirst", locale_case_first_getter),
        ))
        .with_binding(NativeBindingDescriptor::new(
            NativeBindingTarget::Prototype,
            NativeFunctionDescriptor::getter("numeric", locale_numeric_getter),
        ))
        .with_binding(NativeBindingDescriptor::new(
            NativeBindingTarget::Prototype,
            NativeFunctionDescriptor::method("getCalendars", 0, locale_get_calendars),
        ))
        .with_binding(NativeBindingDescriptor::new(
            NativeBindingTarget::Prototype,
            NativeFunctionDescriptor::method("getCollations", 0, locale_get_collations),
        ))
        .with_binding(NativeBindingDescriptor::new(
            NativeBindingTarget::Prototype,
            NativeFunctionDescriptor::method("getHourCycles", 0, locale_get_hour_cycles),
        ))
        .with_binding(NativeBindingDescriptor::new(
            NativeBindingTarget::Prototype,
            NativeFunctionDescriptor::method(
                "getNumberingSystems",
                0,
                locale_get_numbering_systems,
            ),
        ))
        .with_binding(NativeBindingDescriptor::new(
            NativeBindingTarget::Prototype,
            NativeFunctionDescriptor::method("getTimeZones", 0, locale_get_time_zones),
        ))
        .with_binding(NativeBindingDescriptor::new(
            NativeBindingTarget::Prototype,
            NativeFunctionDescriptor::method("getTextInfo", 0, locale_get_text_info),
        ))
        .with_binding(NativeBindingDescriptor::new(
            NativeBindingTarget::Prototype,
            NativeFunctionDescriptor::method("getWeekInfo", 0, locale_get_week_info),
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
    let tag_val = args
        .first()
        .copied()
        .unwrap_or_else(RegisterValue::undefined);
    let options_arg = args
        .get(1)
        .copied()
        .unwrap_or_else(RegisterValue::undefined);

    if tag_val == RegisterValue::undefined() {
        return Err(type_error(
            runtime,
            "First argument to Intl.Locale must be a string or Locale object",
        ));
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
    let hour_cycle = get_option_string(options_arg, "hourCycle", runtime)?;
    let case_first = get_option_string(options_arg, "caseFirst", runtime)?;
    let numeric = get_option_bool(options_arg, "numeric", runtime)?;

    // Build locale tag with options applied as -u- extension keywords.
    let locale_str = apply_unicode_extensions(&canonical, &calendar, &collation, &numbering_system);

    let data = LocaleData {
        locale: locale_str,
        calendar,
        collation,
        numbering_system,
        hour_cycle,
        case_first,
        numeric,
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
        hour_cycle: data.hour_cycle.clone(),
        case_first: data.case_first.clone(),
        numeric: data.numeric,
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
        hour_cycle: data.hour_cycle.clone(),
        case_first: data.case_first.clone(),
        numeric: data.numeric,
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
//  §14.3.8-10 Additional accessor getters
// ═══════════════════════════════════════════════════════════════════

fn locale_hour_cycle_getter(
    this: &RegisterValue,
    _args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let data = require_locale_data(this, runtime)?;
    let hc = data.hour_cycle.clone();
    match hc {
        Some(h) => {
            let handle = runtime.alloc_string(h);
            Ok(RegisterValue::from_object_handle(handle.0))
        }
        None => Ok(RegisterValue::undefined()),
    }
}

fn locale_case_first_getter(
    this: &RegisterValue,
    _args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let data = require_locale_data(this, runtime)?;
    let cf = data.case_first.clone();
    match cf {
        Some(c) => {
            let handle = runtime.alloc_string(c);
            Ok(RegisterValue::from_object_handle(handle.0))
        }
        None => Ok(RegisterValue::undefined()),
    }
}

fn locale_numeric_getter(
    this: &RegisterValue,
    _args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let data = require_locale_data(this, runtime)?;
    match data.numeric {
        Some(b) => Ok(RegisterValue::from_bool(b)),
        None => Ok(RegisterValue::from_bool(false)),
    }
}

// ═══════════════════════════════════════════════════════════════════
//  §14.3.11-16 Query methods (getCalendars, getCollations, etc.)
// ═══════════════════════════════════════════════════════════════════

/// §14.3.11 Intl.Locale.prototype.getCalendars()
/// Spec: <https://tc39.es/ecma402/#sec-intl.locale.prototype.getcalendars>
fn locale_get_calendars(
    this: &RegisterValue,
    _args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let data = require_locale_data(this, runtime)?.clone();
    let items: Vec<String> = if let Some(ca) = &data.calendar {
        vec![ca.clone()]
    } else {
        vec!["gregory".to_string()]
    };
    alloc_owned_string_array(runtime, &items)
}

/// §14.3.12 Intl.Locale.prototype.getCollations()
/// Spec: <https://tc39.es/ecma402/#sec-intl.locale.prototype.getcollations>
fn locale_get_collations(
    this: &RegisterValue,
    _args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let data = require_locale_data(this, runtime)?.clone();
    let items: Vec<String> = if let Some(co) = &data.collation {
        vec![co.clone()]
    } else {
        vec!["default".to_string()]
    };
    alloc_owned_string_array(runtime, &items)
}

/// §14.3.13 Intl.Locale.prototype.getHourCycles()
/// Spec: <https://tc39.es/ecma402/#sec-intl.locale.prototype.gethourcycles>
fn locale_get_hour_cycles(
    this: &RegisterValue,
    _args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let data = require_locale_data(this, runtime)?.clone();
    let items: Vec<String> = if let Some(hc) = &data.hour_cycle {
        vec![hc.clone()]
    } else {
        let lang = data.locale.split('-').next().unwrap_or("en");
        match lang {
            "en" | "ko" | "hi" | "bn" => vec!["h12".to_string()],
            _ => vec!["h23".to_string()],
        }
    };
    alloc_owned_string_array(runtime, &items)
}

/// §14.3.14 Intl.Locale.prototype.getNumberingSystems()
/// Spec: <https://tc39.es/ecma402/#sec-intl.locale.prototype.getnumberingsystems>
fn locale_get_numbering_systems(
    this: &RegisterValue,
    _args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let data = require_locale_data(this, runtime)?.clone();
    let items: Vec<String> = if let Some(ns) = &data.numbering_system {
        vec![ns.clone()]
    } else {
        vec!["latn".to_string()]
    };
    alloc_owned_string_array(runtime, &items)
}

/// §14.3.15 Intl.Locale.prototype.getTimeZones()
/// Spec: <https://tc39.es/ecma402/#sec-intl.locale.prototype.gettimezones>
///
/// Returns an array of representative time zones for the locale's region.
/// If no region, returns undefined.
fn locale_get_time_zones(
    this: &RegisterValue,
    _args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let data = require_locale_data(this, runtime)?.clone();
    let region = extract_subtag_region(&data.locale).map(str::to_string);
    match region {
        None => Ok(RegisterValue::undefined()),
        Some(r) => {
            let tzs = locale_utils::time_zones_for_region(&r);
            alloc_string_array(runtime, &tzs)
        }
    }
}

/// §14.3.16 Intl.Locale.prototype.getTextInfo()
/// Spec: <https://tc39.es/ecma402/#sec-intl.locale.prototype.gettextinfo>
///
/// Returns `{ direction: "ltr" | "rtl" }`.
fn locale_get_text_info(
    this: &RegisterValue,
    _args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let data = require_locale_data(this, runtime)?.clone();
    let lang = data.locale.split('-').next().unwrap_or("en").to_string();
    let direction = if locale_utils::is_rtl_language(&lang) {
        "rtl"
    } else {
        "ltr"
    };

    let obj = runtime.alloc_object();
    let prop = runtime.intern_property_name("direction");
    let s = runtime.alloc_string(direction);
    let _ = runtime
        .objects_mut()
        .set_property(obj, prop, RegisterValue::from_object_handle(s.0));
    Ok(RegisterValue::from_object_handle(obj.0))
}

/// §14.3.17 Intl.Locale.prototype.getWeekInfo()
/// Spec: <https://tc39.es/ecma402/#sec-intl.locale.prototype.getweekinfo>
///
/// Returns `{ firstDay, weekend, minimalDays }`.
fn locale_get_week_info(
    this: &RegisterValue,
    _args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let data = require_locale_data(this, runtime)?.clone();
    let region = extract_subtag_region(&data.locale)
        .unwrap_or("US")
        .to_uppercase();

    let (first_day, weekend, minimal_days) = locale_utils::week_info_for_region(&region);

    let obj = runtime.alloc_object();

    // firstDay: 1=Monday … 7=Sunday
    let prop_first = runtime.intern_property_name("firstDay");
    let _ = runtime
        .objects_mut()
        .set_property(obj, prop_first, RegisterValue::from_i32(first_day));

    // weekend: array of day numbers
    let weekend_arr = runtime.alloc_array();
    for &day in &weekend {
        runtime
            .objects_mut()
            .push_element(weekend_arr, RegisterValue::from_i32(day))
            .map_err(|e| VmNativeCallError::Internal(format!("Locale: {e:?}").into()))?;
    }
    let prop_weekend = runtime.intern_property_name("weekend");
    let _ = runtime.objects_mut().set_property(
        obj,
        prop_weekend,
        RegisterValue::from_object_handle(weekend_arr.0),
    );

    // minimalDays: 1-7
    let prop_minimal = runtime.intern_property_name("minimalDays");
    let _ = runtime.objects_mut().set_property(
        obj,
        prop_minimal,
        RegisterValue::from_i32(minimal_days),
    );

    Ok(RegisterValue::from_object_handle(obj.0))
}

/// Helper: allocate a JS array of string slices.
fn alloc_string_array(
    runtime: &mut crate::interpreter::RuntimeState,
    items: &[&str],
) -> Result<RegisterValue, VmNativeCallError> {
    let arr = runtime.alloc_array();
    for &item in items {
        let s = runtime.alloc_string(item);
        runtime
            .objects_mut()
            .push_element(arr, RegisterValue::from_object_handle(s.0))
            .map_err(|e| VmNativeCallError::Internal(format!("Locale: {e:?}").into()))?;
    }
    Ok(RegisterValue::from_object_handle(arr.0))
}

/// Helper: allocate a JS array of owned strings.
fn alloc_owned_string_array(
    runtime: &mut crate::interpreter::RuntimeState,
    items: &[String],
) -> Result<RegisterValue, VmNativeCallError> {
    let arr = runtime.alloc_array();
    for item in items {
        let s = runtime.alloc_string(item.as_str());
        runtime
            .objects_mut()
            .push_element(arr, RegisterValue::from_object_handle(s.0))
            .map_err(|e| VmNativeCallError::Internal(format!("Locale: {e:?}").into()))?;
    }
    Ok(RegisterValue::from_object_handle(arr.0))
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
            if after.len() >= 2
                && after.as_bytes()[1] == b'-'
                && locale_utils::is_singleton(&after[..1])
            {
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
    parts[1..]
        .iter()
        .find(|p| locale_utils::is_region(p))
        .copied()
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
    let payload = payload::require_intl_payload(this, runtime)
        .map_err(|e| VmNativeCallError::Internal(format!("Locale: {e}").into()))?;
    payload.as_locale().ok_or_else(|| {
        VmNativeCallError::Internal("called on incompatible Intl receiver (not Locale)".into())
    })
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
