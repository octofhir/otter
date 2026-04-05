//! Intl.DisplayNames implementation.
//!
//! Spec: <https://tc39.es/ecma402/#sec-intl-displaynames-constructor>
//!
//! Uses built-in CLDR English display name tables. Full ICU4X displaynames
//! support requires icu_experimental which has version conflicts with our
//! ICU4X 2.x stack. English-only tables are sufficient for most use cases;
//! locale-specific display names can be added when icu_experimental stabilizes.

use crate::descriptors::{
    JsClassDescriptor, NativeBindingDescriptor, NativeBindingTarget, NativeFunctionDescriptor,
    VmNativeCallError,
};
use crate::value::RegisterValue;

use super::options_utils::get_option_string;
use super::payload::{self, DisplayNamesData, DisplayNamesFallback, DisplayNamesStyle, DisplayNamesType, IntlPayload};

// ═══════════════════════════════════════════════════════════════════
//  Class descriptor
// ═══════════════════════════════════════════════════════════════════

pub fn display_names_class_descriptor() -> JsClassDescriptor {
    JsClassDescriptor::new("DisplayNames")
        .with_constructor(NativeFunctionDescriptor::constructor(
            "DisplayNames",
            2,
            display_names_constructor,
        ))
        .with_binding(NativeBindingDescriptor::new(
            NativeBindingTarget::Prototype,
            NativeFunctionDescriptor::method("of", 1, display_names_of),
        ))
        .with_binding(NativeBindingDescriptor::new(
            NativeBindingTarget::Prototype,
            NativeFunctionDescriptor::method("resolvedOptions", 0, display_names_resolved_options),
        ))
        .with_binding(NativeBindingDescriptor::new(
            NativeBindingTarget::Constructor,
            NativeFunctionDescriptor::method(
                "supportedLocalesOf",
                1,
                display_names_supported_locales_of,
            ),
        ))
}

// ═══════════════════════════════════════════════════════════════════
//  Constructor
// ═══════════════════════════════════════════════════════════════════

fn display_names_constructor(
    _this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let locales_arg = args.first().copied().unwrap_or_else(RegisterValue::undefined);
    let options_arg = args.get(1).copied().unwrap_or_else(RegisterValue::undefined);

    let locale = super::resolve_locale(locales_arg, runtime)?;

    // "type" is required per spec.
    let type_opt = get_option_string(options_arg, "type", runtime)?;
    let display_type = match type_opt {
        Some(s) => DisplayNamesType::from_str_opt(&s).ok_or_else(|| {
            range_error(runtime, &format!("Invalid DisplayNames type: {s}"))
        })?,
        None => return Err(type_error(runtime, "DisplayNames requires a 'type' option")),
    };

    let style = parse_enum(
        get_option_string(options_arg, "style", runtime)?,
        DisplayNamesStyle::from_str_opt,
        DisplayNamesStyle::Long,
        "style",
        runtime,
    )?;

    let fallback = parse_enum(
        get_option_string(options_arg, "fallback", runtime)?,
        DisplayNamesFallback::from_str_opt,
        DisplayNamesFallback::Code,
        "fallback",
        runtime,
    )?;

    let data = DisplayNamesData {
        locale,
        display_type,
        style,
        fallback,
    };

    let prototype = runtime.intrinsics().intl_display_names_prototype();
    let handle = payload::construct_intl(IntlPayload::DisplayNames(data), prototype, runtime);
    Ok(RegisterValue::from_object_handle(handle.0))
}

// ═══════════════════════════════════════════════════════════════════
//  §12.3.3 Intl.DisplayNames.prototype.of(code)
// ═══════════════════════════════════════════════════════════════════

fn display_names_of(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let data = require_display_names_data(this, runtime)?.clone();
    let code_arg = args.first().copied().unwrap_or_else(RegisterValue::undefined);
    let code = runtime
        .js_to_string(code_arg)
        .map_err(|e| VmNativeCallError::Internal(format!("DisplayNames.of: {e}").into()))?;

    let name = lookup_display_name(&code, &data);

    match name {
        Some(n) => {
            let handle = runtime.alloc_string(n);
            Ok(RegisterValue::from_object_handle(handle.0))
        }
        None => match data.fallback {
            DisplayNamesFallback::Code => {
                let handle = runtime.alloc_string(&*code);
                Ok(RegisterValue::from_object_handle(handle.0))
            }
            DisplayNamesFallback::None => Ok(RegisterValue::undefined()),
        },
    }
}

// ═══════════════════════════════════════════════════════════════════
//  resolvedOptions()
// ═══════════════════════════════════════════════════════════════════

fn display_names_resolved_options(
    this: &RegisterValue,
    _args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let data = require_display_names_data(this, runtime)?.clone();
    let obj = runtime.alloc_object();
    set_string_prop(runtime, obj, "locale", &data.locale);
    set_string_prop(runtime, obj, "type", data.display_type.as_str());
    set_string_prop(runtime, obj, "style", data.style.as_str());
    set_string_prop(runtime, obj, "fallback", data.fallback.as_str());
    Ok(RegisterValue::from_object_handle(obj.0))
}

// ═══════════════════════════════════════════════════════════════════
//  supportedLocalesOf()
// ═══════════════════════════════════════════════════════════════════

fn display_names_supported_locales_of(
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
//  Display name lookup (English CLDR data)
// ═══════════════════════════════════════════════════════════════════

fn lookup_display_name(code: &str, data: &DisplayNamesData) -> Option<String> {
    match data.display_type {
        DisplayNamesType::Language => lookup_language(code),
        DisplayNamesType::Region => lookup_region(code),
        DisplayNamesType::Script => lookup_script(code),
        DisplayNamesType::Currency => lookup_currency(code),
        DisplayNamesType::Calendar => lookup_calendar(code),
        DisplayNamesType::DateTimeField => lookup_datetime_field(code),
    }
}

fn lookup_language(code: &str) -> Option<String> {
    // Common language codes → English names.
    let name = match code {
        "ar" => "Arabic", "bg" => "Bulgarian", "bn" => "Bangla", "ca" => "Catalan",
        "cs" => "Czech", "da" => "Danish", "de" => "German", "el" => "Greek",
        "en" => "English", "en-US" => "American English", "en-GB" => "British English",
        "es" => "Spanish", "et" => "Estonian", "fa" => "Persian", "fi" => "Finnish",
        "fr" => "French", "gu" => "Gujarati", "he" => "Hebrew", "hi" => "Hindi",
        "hr" => "Croatian", "hu" => "Hungarian", "id" => "Indonesian", "it" => "Italian",
        "ja" => "Japanese", "kn" => "Kannada", "ko" => "Korean", "lt" => "Lithuanian",
        "lv" => "Latvian", "ml" => "Malayalam", "mr" => "Marathi", "ms" => "Malay",
        "nb" => "Norwegian Bokmål", "nl" => "Dutch", "pa" => "Punjabi", "pl" => "Polish",
        "pt" => "Portuguese", "ro" => "Romanian", "ru" => "Russian", "sk" => "Slovak",
        "sl" => "Slovenian", "sr" => "Serbian", "sv" => "Swedish", "sw" => "Swahili",
        "ta" => "Tamil", "te" => "Telugu", "th" => "Thai", "tr" => "Turkish",
        "uk" => "Ukrainian", "ur" => "Urdu", "vi" => "Vietnamese", "zh" => "Chinese",
        "zh-Hans" => "Simplified Chinese", "zh-Hant" => "Traditional Chinese",
        _ => return None,
    };
    Some(name.to_string())
}

fn lookup_region(code: &str) -> Option<String> {
    let name = match code {
        "AD" => "Andorra", "AE" => "United Arab Emirates", "AF" => "Afghanistan",
        "AR" => "Argentina", "AT" => "Austria", "AU" => "Australia", "BE" => "Belgium",
        "BG" => "Bulgaria", "BR" => "Brazil", "CA" => "Canada", "CH" => "Switzerland",
        "CL" => "Chile", "CN" => "China", "CO" => "Colombia", "CZ" => "Czechia",
        "DE" => "Germany", "DK" => "Denmark", "EG" => "Egypt", "ES" => "Spain",
        "FI" => "Finland", "FR" => "France", "GB" => "United Kingdom", "GR" => "Greece",
        "HK" => "Hong Kong SAR China", "HR" => "Croatia", "HU" => "Hungary",
        "ID" => "Indonesia", "IE" => "Ireland", "IL" => "Israel", "IN" => "India",
        "IQ" => "Iraq", "IR" => "Iran", "IT" => "Italy", "JP" => "Japan",
        "KR" => "South Korea", "MX" => "Mexico", "MY" => "Malaysia", "NG" => "Nigeria",
        "NL" => "Netherlands", "NO" => "Norway", "NZ" => "New Zealand", "PH" => "Philippines",
        "PK" => "Pakistan", "PL" => "Poland", "PT" => "Portugal", "RO" => "Romania",
        "RU" => "Russia", "SA" => "Saudi Arabia", "SE" => "Sweden", "SG" => "Singapore",
        "TH" => "Thailand", "TR" => "Türkiye", "TW" => "Taiwan", "UA" => "Ukraine",
        "US" => "United States", "VN" => "Vietnam", "ZA" => "South Africa",
        _ => return None,
    };
    Some(name.to_string())
}

fn lookup_script(code: &str) -> Option<String> {
    let name = match code {
        "Arab" => "Arabic", "Armn" => "Armenian", "Beng" => "Bangla", "Cyrl" => "Cyrillic",
        "Deva" => "Devanagari", "Geor" => "Georgian", "Grek" => "Greek", "Gujr" => "Gujarati",
        "Guru" => "Gurmukhi", "Hang" => "Hangul", "Hani" => "Han", "Hans" => "Simplified",
        "Hant" => "Traditional", "Hebr" => "Hebrew", "Jpan" => "Japanese", "Kana" => "Katakana",
        "Knda" => "Kannada", "Kore" => "Korean", "Latn" => "Latin", "Mlym" => "Malayalam",
        "Mymr" => "Myanmar", "Orya" => "Odia", "Sinh" => "Sinhala", "Taml" => "Tamil",
        "Telu" => "Telugu", "Thai" => "Thai", "Tibt" => "Tibetan",
        _ => return None,
    };
    Some(name.to_string())
}

fn lookup_currency(code: &str) -> Option<String> {
    let name = match code {
        "AUD" => "Australian Dollar", "BRL" => "Brazilian Real", "CAD" => "Canadian Dollar",
        "CHF" => "Swiss Franc", "CNY" => "Chinese Yuan", "EUR" => "Euro",
        "GBP" => "British Pound", "HKD" => "Hong Kong Dollar", "INR" => "Indian Rupee",
        "JPY" => "Japanese Yen", "KRW" => "South Korean Won", "MXN" => "Mexican Peso",
        "NOK" => "Norwegian Krone", "NZD" => "New Zealand Dollar", "RUB" => "Russian Ruble",
        "SEK" => "Swedish Krona", "SGD" => "Singapore Dollar", "THB" => "Thai Baht",
        "TRY" => "Turkish Lira", "TWD" => "New Taiwan Dollar", "USD" => "US Dollar",
        "ZAR" => "South African Rand",
        _ => return None,
    };
    Some(name.to_string())
}

fn lookup_calendar(code: &str) -> Option<String> {
    let name = match code {
        "buddhist" => "Buddhist Calendar", "chinese" => "Chinese Calendar",
        "coptic" => "Coptic Calendar", "dangi" => "Dangi Calendar",
        "ethioaa" => "Ethiopic Amete Alem Calendar", "ethiopic" => "Ethiopic Calendar",
        "gregory" => "Gregorian Calendar", "hebrew" => "Hebrew Calendar",
        "indian" => "Indian National Calendar", "islamic" => "Islamic Calendar",
        "islamic-civil" => "Islamic Calendar (tabular, civil epoch)",
        "islamic-umalqura" => "Islamic Calendar (Umm al-Qura)",
        "iso8601" => "ISO-8601 Calendar", "japanese" => "Japanese Calendar",
        "persian" => "Persian Calendar", "roc" => "Minguo Calendar",
        _ => return None,
    };
    Some(name.to_string())
}

fn lookup_datetime_field(code: &str) -> Option<String> {
    let name = match code {
        "era" => "era", "year" => "year", "quarter" => "quarter", "month" => "month",
        "weekOfYear" => "week", "weekday" => "day of the week", "day" => "day",
        "dayPeriod" => "AM/PM", "hour" => "hour", "minute" => "minute",
        "second" => "second", "timeZoneName" => "time zone",
        _ => return None,
    };
    Some(name.to_string())
}

// ═══════════════════════════════════════════════════════════════════
//  Enum implementations
// ═══════════════════════════════════════════════════════════════════

impl DisplayNamesType {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Language => "language",
            Self::Region => "region",
            Self::Script => "script",
            Self::Currency => "currency",
            Self::Calendar => "calendar",
            Self::DateTimeField => "dateTimeField",
        }
    }
    pub fn from_str_opt(s: &str) -> Option<Self> {
        match s {
            "language" => Some(Self::Language),
            "region" => Some(Self::Region),
            "script" => Some(Self::Script),
            "currency" => Some(Self::Currency),
            "calendar" => Some(Self::Calendar),
            "dateTimeField" => Some(Self::DateTimeField),
            _ => None,
        }
    }
}

impl DisplayNamesStyle {
    pub fn as_str(&self) -> &'static str {
        match self { Self::Long => "long", Self::Short => "short", Self::Narrow => "narrow" }
    }
    pub fn from_str_opt(s: &str) -> Option<Self> {
        match s { "long" => Some(Self::Long), "short" => Some(Self::Short), "narrow" => Some(Self::Narrow), _ => None }
    }
}

impl DisplayNamesFallback {
    pub fn as_str(&self) -> &'static str {
        match self { Self::Code => "code", Self::None => "none" }
    }
    pub fn from_str_opt(s: &str) -> Option<Self> {
        match s { "code" => Some(Self::Code), "none" => Some(Self::None), _ => None }
    }
}

// ═══════════════════════════════════════════════════════════════════
//  Internal helpers
// ═══════════════════════════════════════════════════════════════════

fn require_display_names_data<'a>(
    this: &RegisterValue,
    runtime: &'a crate::interpreter::RuntimeState,
) -> Result<&'a DisplayNamesData, VmNativeCallError> {
    let payload = payload::require_intl_payload(this, runtime).map_err(|e| {
        VmNativeCallError::Internal(format!("DisplayNames: {e}").into())
    })?;
    payload.as_display_names().ok_or_else(|| {
        VmNativeCallError::Internal("called on incompatible Intl receiver (not DisplayNames)".into())
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
    let _ = runtime.objects_mut().set_property(obj, prop, RegisterValue::from_object_handle(s.0));
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
