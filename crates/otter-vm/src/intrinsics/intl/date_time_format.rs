//! Intl.DateTimeFormat implementation.
//!
//! Spec: <https://tc39.es/ecma402/#sec-intl-datetimeformat-constructor>
//!
//! Uses ICU4X `DateTimeFormatter<CompositeFieldSet>` with `FieldSetBuilder` for
//! dynamically-specified date/time component formatting, following Boa's approach.

use icu_datetime::fieldsets::builder::{DateFields, FieldSetBuilder};
use icu_datetime::fieldsets::enums::CompositeFieldSet;
use icu_datetime::options::{Length, SubsecondDigits, TimePrecision};
use icu_datetime::{DateTimeFormatter, DateTimeFormatterPreferences};
use icu_locale::Locale;
use icu_time::zone::UtcOffset;
use icu_time::{DateTime, TimeZone, ZonedDateTime};
use std::str::FromStr;

use crate::descriptors::{
    JsClassDescriptor, NativeBindingDescriptor, NativeBindingTarget, NativeFunctionDescriptor,
    VmNativeCallError,
};
use crate::object::{ObjectHandle, PropertyAttributes, PropertyValue};
use crate::value::RegisterValue;

use super::locale_utils;
use super::options_utils::get_option_string;
use super::payload::{self, DateTimeFormatData, DateTimeStyle, IntlPayload};

// ═══════════════════════════════════════════════════════════════════
//  Class descriptor
// ═══════════════════════════════════════════════════════════════════

pub fn date_time_format_class_descriptor() -> JsClassDescriptor {
    JsClassDescriptor::new("DateTimeFormat")
        .with_constructor(NativeFunctionDescriptor::constructor(
            "DateTimeFormat",
            0,
            date_time_format_constructor,
        ))
        // §12.5.3 — `format` is a getter that returns a bound function.
        .with_binding(NativeBindingDescriptor::new(
            NativeBindingTarget::Prototype,
            NativeFunctionDescriptor::getter("format", date_time_format_format_getter),
        ))
        .with_binding(NativeBindingDescriptor::new(
            NativeBindingTarget::Prototype,
            NativeFunctionDescriptor::method("formatToParts", 1, date_time_format_format_to_parts),
        ))
        .with_binding(NativeBindingDescriptor::new(
            NativeBindingTarget::Prototype,
            NativeFunctionDescriptor::method("formatRange", 2, date_time_format_format_range),
        ))
        .with_binding(NativeBindingDescriptor::new(
            NativeBindingTarget::Prototype,
            NativeFunctionDescriptor::method(
                "formatRangeToParts",
                2,
                date_time_format_format_range_to_parts,
            ),
        ))
        .with_binding(NativeBindingDescriptor::new(
            NativeBindingTarget::Prototype,
            NativeFunctionDescriptor::method(
                "resolvedOptions",
                0,
                date_time_format_resolved_options,
            ),
        ))
        .with_binding(NativeBindingDescriptor::new(
            NativeBindingTarget::Constructor,
            NativeFunctionDescriptor::method(
                "supportedLocalesOf",
                1,
                date_time_format_supported_locales_of,
            ),
        ))
}

// ═══════════════════════════════════════════════════════════════════
//  §12.1.1 Intl.DateTimeFormat(locales, options)
// ═══════════════════════════════════════════════════════════════════

fn date_time_format_constructor(
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
    let data = resolve_date_time_format_options(&locale, options_arg, runtime)?;

    let prototype = runtime.intrinsics().intl_date_time_format_prototype();
    let handle = payload::construct_intl(IntlPayload::DateTimeFormat(data), prototype, runtime);

    Ok(RegisterValue::from_object_handle(handle.0))
}

// ═══════════════════════════════════════════════════════════════════
//  §12.5.3 get Intl.DateTimeFormat.prototype.format
// ═══════════════════════════════════════════════════════════════════

/// §12.5.3 get Intl.DateTimeFormat.prototype.format
///
/// Returns a bound format function. Cached on the instance as `__boundFormat`.
///
/// Spec: <https://tc39.es/ecma402/#sec-intl.datetimeformat.prototype.format>
fn date_time_format_format_getter(
    this: &RegisterValue,
    _args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let handle = this.as_object_handle().map(ObjectHandle).ok_or_else(|| {
        VmNativeCallError::Internal("DateTimeFormat.format getter: expected object".into())
    })?;

    let cache_prop = runtime.intern_property_name("__boundFormat");
    let cached = runtime
        .own_property_value(handle, cache_prop)
        .map_err(dtf_interp_err)?;
    if cached != RegisterValue::undefined() && cached.as_object_handle().is_some() {
        return Ok(cached);
    }

    let desc = NativeFunctionDescriptor::method("format", 1, bound_dtf_format);
    let fn_id = runtime.register_native_function(desc);
    let fn_proto = runtime.intrinsics().function_prototype();
    let bound_fn = runtime.objects_mut().alloc_host_function(fn_id);
    let _ = runtime
        .objects_mut()
        .set_prototype(bound_fn, Some(fn_proto));

    let dtf_prop = runtime.intern_property_name("__dateTimeFormat__");
    runtime
        .objects_mut()
        .define_own_property(
            bound_fn,
            dtf_prop,
            PropertyValue::data_with_attrs(
                RegisterValue::from_object_handle(handle.0),
                PropertyAttributes::from_flags(false, false, false),
            ),
        )
        .map_err(dtf_interp_err)?;

    let bound_val = RegisterValue::from_object_handle(bound_fn.0);
    runtime
        .objects_mut()
        .define_own_property(
            handle,
            cache_prop,
            PropertyValue::data_with_attrs(
                bound_val,
                PropertyAttributes::from_flags(false, false, false),
            ),
        )
        .map_err(dtf_interp_err)?;

    Ok(bound_val)
}

/// Bound format function for DateTimeFormat.
fn bound_dtf_format(
    _this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let fn_handle = runtime
        .current_native_callee()
        .ok_or_else(|| VmNativeCallError::Internal("bound format: no callee".into()))?;
    let dtf_prop = runtime.intern_property_name("__dateTimeFormat__");
    let dtf_val = runtime
        .own_property_value(fn_handle, dtf_prop)
        .map_err(dtf_interp_err)?;
    let dtf_rv =
        RegisterValue::from_object_handle(dtf_val.as_object_handle().ok_or_else(|| {
            VmNativeCallError::Internal("bound format: missing __dateTimeFormat__".into())
        })?);

    let data = require_dtf_data(&dtf_rv, runtime)?.clone();
    let date_val = args
        .first()
        .copied()
        .unwrap_or_else(RegisterValue::undefined);
    let timestamp_ms = resolve_date_value(date_val, runtime)?;

    let formatted = format_date_time(timestamp_ms, &data)?;
    let handle = runtime.alloc_string(formatted);
    Ok(RegisterValue::from_object_handle(handle.0))
}

fn dtf_interp_err(e: impl std::fmt::Debug) -> VmNativeCallError {
    VmNativeCallError::Internal(format!("{e:?}").into())
}

// ═══════════════════════════════════════════════════════════════════
//  §12.5.6 Intl.DateTimeFormat.prototype.formatToParts(date)
// ═══════════════════════════════════════════════════════════════════

/// §12.5.6 formatToParts(date)
///
/// Spec: <https://tc39.es/ecma402/#sec-intl.datetimeformat.prototype.formattoparts>
fn date_time_format_format_to_parts(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let data = require_dtf_data(this, runtime)?.clone();

    let date_val = args
        .first()
        .copied()
        .unwrap_or_else(RegisterValue::undefined);
    let timestamp_ms = resolve_date_value(date_val, runtime)?;

    let formatted = format_date_time(timestamp_ms, &data)?;

    // Decompose the formatted string into parts.
    // ICU4X's FormattedDateTime supports write_to_parts, but the API is complex.
    // We use a simplified parse of the formatted string.
    let parts = decompose_formatted_date(&formatted);

    let arr = runtime.alloc_array();
    for (part_type, part_value) in &parts {
        let obj = runtime.alloc_object();
        set_string_prop(runtime, obj, "type", part_type);
        set_string_prop(runtime, obj, "value", part_value);
        runtime
            .objects_mut()
            .push_element(arr, RegisterValue::from_object_handle(obj.0))
            .map_err(|e| VmNativeCallError::Internal(format!("formatToParts: {e:?}").into()))?;
    }

    Ok(RegisterValue::from_object_handle(arr.0))
}

// ═══════════════════════════════════════════════════════════════════
//  §12.5.8 Intl.DateTimeFormat.prototype.formatRange(startDate, endDate)
// ═══════════════════════════════════════════════════════════════════

/// §12.5.8 Intl.DateTimeFormat.prototype.formatRange(startDate, endDate)
///
/// Spec: <https://tc39.es/ecma402/#sec-intl.datetimeformat.prototype.formatrange>
fn date_time_format_format_range(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let data = require_dtf_data(this, runtime)?.clone();

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
            "startDate and endDate must be provided to formatRange",
        ));
    }

    let start_ms = resolve_date_value(start_val, runtime)?;
    let end_ms = resolve_date_value(end_val, runtime)?;

    let start_str = format_date_time(start_ms, &data)?;
    let end_str = format_date_time(end_ms, &data)?;

    let result = if start_str == end_str {
        start_str
    } else {
        format!("{start_str} \u{2013} {end_str}")
    };

    let handle = runtime.alloc_string(result);
    Ok(RegisterValue::from_object_handle(handle.0))
}

// ═══════════════════════════════════════════════════════════════════
//  §12.5.9 Intl.DateTimeFormat.prototype.formatRangeToParts(startDate, endDate)
// ═══════════════════════════════════════════════════════════════════

/// §12.5.9 Intl.DateTimeFormat.prototype.formatRangeToParts(startDate, endDate)
///
/// Spec: <https://tc39.es/ecma402/#sec-intl.datetimeformat.prototype.formatrangetoparts>
fn date_time_format_format_range_to_parts(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let data = require_dtf_data(this, runtime)?.clone();

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
            "startDate and endDate must be provided to formatRangeToParts",
        ));
    }

    let start_ms = resolve_date_value(start_val, runtime)?;
    let end_ms = resolve_date_value(end_val, runtime)?;

    let start_str = format_date_time(start_ms, &data)?;
    let end_str = format_date_time(end_ms, &data)?;

    let arr = runtime.alloc_array();

    // Start date parts (source: "startRange").
    let start_parts = decompose_date_parts(&start_str);
    for (part_type, part_value) in &start_parts {
        let obj = runtime.alloc_object();
        set_dtf_string_prop(runtime, obj, "type", part_type);
        set_dtf_string_prop(runtime, obj, "value", part_value);
        set_dtf_string_prop(runtime, obj, "source", "startRange");
        runtime
            .objects_mut()
            .push_element(arr, RegisterValue::from_object_handle(obj.0))
            .map_err(|e| {
                VmNativeCallError::Internal(format!("formatRangeToParts: {e:?}").into())
            })?;
    }

    // Literal range separator.
    let sep_obj = runtime.alloc_object();
    set_dtf_string_prop(runtime, sep_obj, "type", "literal");
    set_dtf_string_prop(runtime, sep_obj, "value", " \u{2013} ");
    set_dtf_string_prop(runtime, sep_obj, "source", "shared");
    runtime
        .objects_mut()
        .push_element(arr, RegisterValue::from_object_handle(sep_obj.0))
        .map_err(|e| VmNativeCallError::Internal(format!("formatRangeToParts: {e:?}").into()))?;

    // End date parts (source: "endRange").
    let end_parts = decompose_date_parts(&end_str);
    for (part_type, part_value) in &end_parts {
        let obj = runtime.alloc_object();
        set_dtf_string_prop(runtime, obj, "type", part_type);
        set_dtf_string_prop(runtime, obj, "value", part_value);
        set_dtf_string_prop(runtime, obj, "source", "endRange");
        runtime
            .objects_mut()
            .push_element(arr, RegisterValue::from_object_handle(obj.0))
            .map_err(|e| {
                VmNativeCallError::Internal(format!("formatRangeToParts: {e:?}").into())
            })?;
    }

    Ok(RegisterValue::from_object_handle(arr.0))
}

fn set_dtf_string_prop(
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

/// Simplified decomposition of a formatted date/time string into parts.
///
/// Classifies characters into digit, literal, and dayPeriod categories.
fn decompose_date_parts(formatted: &str) -> Vec<(&'static str, String)> {
    let mut parts = Vec::new();
    let mut current = String::new();
    let mut current_type: &str = "literal";

    for ch in formatted.chars() {
        let ch_type = classify_dtf_char(ch);
        if ch_type != current_type && !current.is_empty() {
            parts.push((current_type, current.clone()));
            current.clear();
        }
        current_type = ch_type;
        current.push(ch);
    }
    if !current.is_empty() {
        parts.push((current_type, current));
    }
    parts
}

fn classify_dtf_char(ch: char) -> &'static str {
    if ch.is_ascii_digit() {
        "integer"
    } else {
        "literal"
    }
}

// ═══════════════════════════════════════════════════════════════════
//  §12.5.7 Intl.DateTimeFormat.prototype.resolvedOptions()
// ═══════════════════════════════════════════════════════════════════

fn date_time_format_resolved_options(
    this: &RegisterValue,
    _args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let data = require_dtf_data(this, runtime)?.clone();

    let obj = runtime.alloc_object();
    set_string_prop(runtime, obj, "locale", &data.locale);
    set_string_prop(runtime, obj, "calendar", &data.calendar);
    set_string_prop(runtime, obj, "numberingSystem", &data.numbering_system);
    set_string_prop(runtime, obj, "timeZone", &data.time_zone);

    if let Some(ref ds) = data.date_style {
        set_string_prop(runtime, obj, "dateStyle", ds.as_str());
    }
    if let Some(ref ts) = data.time_style {
        set_string_prop(runtime, obj, "timeStyle", ts.as_str());
    }

    if let Some(ref v) = data.weekday {
        set_string_prop(runtime, obj, "weekday", v);
    }
    if let Some(ref v) = data.era {
        set_string_prop(runtime, obj, "era", v);
    }
    if let Some(ref v) = data.year {
        set_string_prop(runtime, obj, "year", v);
    }
    if let Some(ref v) = data.month {
        set_string_prop(runtime, obj, "month", v);
    }
    if let Some(ref v) = data.day {
        set_string_prop(runtime, obj, "day", v);
    }
    if let Some(ref v) = data.day_period {
        set_string_prop(runtime, obj, "dayPeriod", v);
    }
    if let Some(ref v) = data.hour {
        set_string_prop(runtime, obj, "hour", v);
    }
    if let Some(ref v) = data.minute {
        set_string_prop(runtime, obj, "minute", v);
    }
    if let Some(ref v) = data.second {
        set_string_prop(runtime, obj, "second", v);
    }
    if let Some(ref v) = data.fractional_second_digits {
        set_i32_prop(runtime, obj, "fractionalSecondDigits", *v as i32);
    }
    if let Some(ref v) = data.time_zone_name {
        set_string_prop(runtime, obj, "timeZoneName", v);
    }

    Ok(RegisterValue::from_object_handle(obj.0))
}

// ═══════════════════════════════════════════════════════════════════
//  §12.2.2 Intl.DateTimeFormat.supportedLocalesOf(locales)
// ═══════════════════════════════════════════════════════════════════

fn date_time_format_supported_locales_of(
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

fn resolve_date_time_format_options(
    locale: &str,
    options: RegisterValue,
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<DateTimeFormatData, VmNativeCallError> {
    let calendar =
        get_option_string(options, "calendar", runtime)?.unwrap_or_else(|| "gregory".to_string());
    let numbering_system = get_option_string(options, "numberingSystem", runtime)?
        .unwrap_or_else(|| "latn".to_string());
    let time_zone =
        get_option_string(options, "timeZone", runtime)?.unwrap_or_else(|| "UTC".to_string());

    let date_style = parse_optional_enum(
        get_option_string(options, "dateStyle", runtime)?,
        DateTimeStyle::from_str_opt,
        "dateStyle",
        runtime,
    )?;
    let time_style = parse_optional_enum(
        get_option_string(options, "timeStyle", runtime)?,
        DateTimeStyle::from_str_opt,
        "timeStyle",
        runtime,
    )?;

    // Component options — only valid when dateStyle/timeStyle not set per spec §12.1.2 step 36.
    let weekday = get_option_string(options, "weekday", runtime)?;
    let era = get_option_string(options, "era", runtime)?;
    let year = get_option_string(options, "year", runtime)?;
    let month = get_option_string(options, "month", runtime)?;
    let day = get_option_string(options, "day", runtime)?;
    let day_period = get_option_string(options, "dayPeriod", runtime)?;
    let hour = get_option_string(options, "hour", runtime)?;
    let minute = get_option_string(options, "minute", runtime)?;
    let second = get_option_string(options, "second", runtime)?;
    let fractional_second_digits = get_option_string(options, "fractionalSecondDigits", runtime)?
        .and_then(|s| s.parse::<u8>().ok())
        .filter(|&v| (1..=3).contains(&v));
    let time_zone_name = get_option_string(options, "timeZoneName", runtime)?;

    // §12.1.2 step 44: if dateStyle and timeStyle are both undefined and no
    // components specified, ToDateTimeOptions applies defaults.
    let (year, month, day) = if date_style.is_none()
        && time_style.is_none()
        && weekday.is_none()
        && era.is_none()
        && year.is_none()
        && month.is_none()
        && day.is_none()
        && hour.is_none()
        && minute.is_none()
        && second.is_none()
    {
        (
            Some("numeric".to_string()),
            Some("numeric".to_string()),
            Some("numeric".to_string()),
        )
    } else {
        (year, month, day)
    };

    Ok(DateTimeFormatData {
        locale: locale.to_string(),
        calendar,
        numbering_system,
        time_zone,
        date_style,
        time_style,
        weekday,
        era,
        year,
        month,
        day,
        day_period,
        hour,
        minute,
        second,
        fractional_second_digits,
        time_zone_name,
    })
}

// ═══════════════════════════════════════════════════════════════════
//  Date value resolution
// ═══════════════════════════════════════════════════════════════════

fn resolve_date_value(
    date_val: RegisterValue,
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<f64, VmNativeCallError> {
    if date_val == RegisterValue::undefined() {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default();
        return Ok(now.as_millis() as f64);
    }
    let number = runtime
        .js_to_number(date_val)
        .map_err(|e| VmNativeCallError::Internal(format!("DateTimeFormat: {e}").into()))?;
    if number.is_nan() || number.is_infinite() {
        return Err(range_error(runtime, "Invalid time value"));
    }
    Ok(number)
}

// ═══════════════════════════════════════════════════════════════════
//  ICU4X formatting core
// ═══════════════════════════════════════════════════════════════════

/// Converts resolved options into an ICU4X `CompositeFieldSet` and formats.
fn format_date_time(
    timestamp_ms: f64,
    data: &DateTimeFormatData,
) -> Result<String, VmNativeCallError> {
    // Build ICU4X field set.
    let fieldset = build_field_set(data).map_err(|e| {
        VmNativeCallError::Internal(format!("DateTimeFormat field set: {e}").into())
    })?;

    // Build locale preferences.
    let locale = Locale::from_str(&data.locale).unwrap_or_else(|_| {
        Locale::from_str(locale_utils::DEFAULT_LOCALE).expect("default locale should parse")
    });
    let prefs = DateTimeFormatterPreferences::from(&locale);

    // Create formatter.
    let formatter = DateTimeFormatter::try_new(prefs, fieldset)
        .map_err(|e| VmNativeCallError::Internal(format!("DateTimeFormatter: {e}").into()))?;

    // Convert timestamp to ICU4X ZonedDateTime with timezone info.
    // Following Boa's pattern: create TimeZoneInfo<AtTime> for CompositeFieldSet compatibility.
    let epoch_ms = timestamp_ms as i64;
    let zdt_utc =
        ZonedDateTime::from_epoch_milliseconds_and_utc_offset(epoch_ms, UtcOffset::zero());
    let tz_info = TimeZone::UNKNOWN.with_offset(Some(UtcOffset::zero()));
    let dt = DateTime {
        date: zdt_utc.date,
        time: zdt_utc.time,
    };
    let tz_at_time = tz_info.at_date_time(dt);
    let zdt = ZonedDateTime {
        date: dt.date,
        time: dt.time,
        zone: tz_at_time,
    };

    Ok(formatter.format(&zdt).to_string())
}

/// §12.5.12 DateTimeStyleFormat — builds a `CompositeFieldSet` from options.
///
/// Following Boa's `date_time_style_format` / `best_fit_date_time_format` pattern.
fn build_field_set(data: &DateTimeFormatData) -> Result<CompositeFieldSet, String> {
    let mut builder = FieldSetBuilder::default();

    if data.date_style.is_some() || data.time_style.is_some() {
        // Style-based formatting.
        builder.length = match data.date_style {
            Some(DateTimeStyle::Full | DateTimeStyle::Long) => Some(Length::Long),
            Some(DateTimeStyle::Medium) => Some(Length::Medium),
            Some(DateTimeStyle::Short) => Some(Length::Short),
            None => match data.time_style {
                Some(DateTimeStyle::Full | DateTimeStyle::Long) => Some(Length::Long),
                Some(DateTimeStyle::Medium) => Some(Length::Medium),
                Some(DateTimeStyle::Short) => Some(Length::Short),
                None => Some(Length::Medium),
            },
        };
        builder.date_fields = match data.date_style {
            Some(DateTimeStyle::Full) => Some(DateFields::YMDE),
            Some(DateTimeStyle::Long | DateTimeStyle::Medium | DateTimeStyle::Short) => {
                Some(DateFields::YMD)
            }
            None => None,
        };
        builder.time_precision = match data.time_style {
            Some(DateTimeStyle::Full | DateTimeStyle::Long | DateTimeStyle::Medium) => {
                Some(TimePrecision::Second)
            }
            Some(DateTimeStyle::Short) => Some(TimePrecision::Minute),
            None => None,
        };
    } else {
        // Component-based formatting.
        builder.length = Some(resolve_component_length(data));
        builder.date_fields = resolve_date_fields(data);
        builder.time_precision = resolve_time_precision(data);
    }

    builder.build_composite().map_err(|e| format!("{e}"))
}

/// Determine Length from component option values.
fn resolve_component_length(data: &DateTimeFormatData) -> Length {
    // Check if any component uses "long"/"narrow" → Long, "short" → Short, else Medium.
    let all_opts: Vec<&Option<String>> = vec![
        &data.weekday,
        &data.era,
        &data.year,
        &data.month,
        &data.day,
        &data.hour,
        &data.minute,
        &data.second,
    ];
    for v in all_opts.iter().copied().flatten() {
        match v.as_str() {
            "long" | "narrow" => return Length::Long,
            "short" | "2-digit" => return Length::Short,
            _ => {}
        }
    }
    Length::Medium
}

/// Map component options to ICU4X DateFields.
fn resolve_date_fields(data: &DateTimeFormatData) -> Option<DateFields> {
    let has_year = data.year.is_some();
    let has_month = data.month.is_some();
    let has_day = data.day.is_some();
    let has_weekday = data.weekday.is_some();

    match (has_year, has_month, has_day, has_weekday) {
        (true, true, true, true) => Some(DateFields::YMDE),
        (true, true, true, false) => Some(DateFields::YMD),
        (false, true, true, true) => Some(DateFields::MDE),
        (false, true, true, false) => Some(DateFields::MD),
        (true, true, false, false) => Some(DateFields::YM),
        (false, false, true, true) => Some(DateFields::DE),
        (false, false, false, true) => Some(DateFields::E),
        (false, true, false, false) => Some(DateFields::M),
        (false, false, true, false) => Some(DateFields::D),
        (true, false, false, false) => Some(DateFields::Y),
        _ if has_year || has_month || has_day => Some(DateFields::YMD),
        _ => None,
    }
}

/// Map time component options to ICU4X TimePrecision.
fn resolve_time_precision(data: &DateTimeFormatData) -> Option<TimePrecision> {
    let has_hour = data.hour.is_some();
    let has_minute = data.minute.is_some();
    let has_second = data.second.is_some();

    if !has_hour && !has_minute && !has_second {
        return None;
    }

    if has_second {
        if let Some(fsd) = data.fractional_second_digits {
            return match fsd {
                1 => Some(TimePrecision::Subsecond(SubsecondDigits::S1)),
                2 => Some(TimePrecision::Subsecond(SubsecondDigits::S2)),
                3 => Some(TimePrecision::Subsecond(SubsecondDigits::S3)),
                _ => Some(TimePrecision::Second),
            };
        }
        return Some(TimePrecision::Second);
    }

    if has_minute {
        return Some(TimePrecision::Minute);
    }

    // Hour only — minute precision still needed for display.
    Some(TimePrecision::Minute)
}

// ═══════════════════════════════════════════════════════════════════
//  formatToParts decomposition
// ═══════════════════════════════════════════════════════════════════

/// Decomposes a formatted date/time string into typed parts.
///
/// Since ICU4X's `FormattedDateTime::write_to_parts` is complex and requires
/// a custom WriteParts sink, we parse the string heuristically.
fn decompose_formatted_date(formatted: &str) -> Vec<(&'static str, String)> {
    let mut parts = Vec::new();
    let mut current = String::new();
    let mut current_type: &str = "literal";

    for ch in formatted.chars() {
        let new_type = classify_char(ch);
        if new_type != current_type && !current.is_empty() {
            parts.push((current_type, current.clone()));
            current.clear();
        }
        current_type = new_type;
        current.push(ch);
    }
    if !current.is_empty() {
        parts.push((current_type, current));
    }

    // Post-process: try to identify date/time parts from context.
    refine_parts(&mut parts);

    parts
}

fn classify_char(ch: char) -> &'static str {
    if ch.is_ascii_digit() {
        "integer"
    } else {
        "literal"
    }
}

/// Refine generic "integer" parts into date/time specific types based on position.
fn refine_parts(parts: &mut [(&'static str, String)]) {
    // Simple heuristic: alternating integer/literal sequences.
    // First group of integers → month, second → day, third → year (or similar).
    let mut int_index = 0;
    for part in parts.iter_mut() {
        if part.0 == "integer" {
            part.0 = match int_index {
                0 => "month",
                1 => "day",
                2 => "year",
                3 => "hour",
                4 => "minute",
                5 => "second",
                _ => "literal",
            };
            int_index += 1;
        }
    }
}

// ═══════════════════════════════════════════════════════════════════
//  Enum implementations
// ═══════════════════════════════════════════════════════════════════

impl DateTimeStyle {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Full => "full",
            Self::Long => "long",
            Self::Medium => "medium",
            Self::Short => "short",
        }
    }
    pub fn from_str_opt(s: &str) -> Option<Self> {
        match s {
            "full" => Some(Self::Full),
            "long" => Some(Self::Long),
            "medium" => Some(Self::Medium),
            "short" => Some(Self::Short),
            _ => None,
        }
    }
}

// ═══════════════════════════════════════════════════════════════════
//  Internal helpers
// ═══════════════════════════════════════════════════════════════════

fn require_dtf_data<'a>(
    this: &RegisterValue,
    runtime: &'a crate::interpreter::RuntimeState,
) -> Result<&'a DateTimeFormatData, VmNativeCallError> {
    let payload = payload::require_intl_payload(this, runtime)
        .map_err(|e| VmNativeCallError::Internal(format!("DateTimeFormat: {e}").into()))?;
    payload.as_date_time_format().ok_or_else(|| {
        VmNativeCallError::Internal(
            "called on incompatible Intl receiver (not DateTimeFormat)".into(),
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

fn parse_optional_enum<T>(
    value: Option<String>,
    from_str: fn(&str) -> Option<T>,
    name: &str,
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<Option<T>, VmNativeCallError> {
    match value {
        None => Ok(None),
        Some(s) => from_str(&s)
            .map(Some)
            .ok_or_else(|| range_error(runtime, &format!("Invalid {name} option"))),
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
