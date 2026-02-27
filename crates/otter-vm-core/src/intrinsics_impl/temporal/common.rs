//! Shared helper functions for the Temporal namespace implementation.
//!
//! These are used across PlainMonthDay, PlainDate, PlainDateTime,
//! and the install_temporal_namespace entry point.

use crate::context::NativeContext;
use crate::error::VmError;
use crate::gc::GcRef;
use crate::memory::MemoryManager;
use crate::object::{JsObject, PropertyDescriptor, PropertyKey};
use crate::string::JsString;
use crate::temporal_value::TemporalValue;
use crate::value::Value;
use std::sync::Arc;
use temporal_rs::options::Overflow;

/// Get the compiled timezone provider for temporal operations.
pub(super) fn tz_provider() -> &'static impl temporal_rs::provider::TimeZoneProvider {
    &*temporal_rs::provider::COMPILED_TZ_PROVIDER
}

// ============================================================================
// Property keys (internal slots)
// ============================================================================

pub(super) const SLOT_ISO_MONTH: &str = "__temporal_iso_month__";
pub(super) const SLOT_ISO_DAY: &str = "__temporal_iso_day__";
pub(super) const SLOT_ISO_YEAR: &str = "__temporal_iso_year__";
pub(super) const SLOT_TEMPORAL_TYPE: &str = "__temporal_type__";

pub(super) const SLOT_ISO_HOUR: &str = "__iso_hour";
pub(super) const SLOT_ISO_MINUTE: &str = "__iso_minute";
pub(super) const SLOT_ISO_SECOND: &str = "__iso_second";
pub(super) const SLOT_ISO_MILLISECOND: &str = "__iso_millisecond";
pub(super) const SLOT_ISO_MICROSECOND: &str = "__iso_microsecond";
pub(super) const SLOT_ISO_NANOSECOND: &str = "__iso_nanosecond";

/// Internal slot holding a `Value::temporal(TemporalValue::...)` — the native temporal_rs type.
pub(super) const SLOT_TEMPORAL_INNER: &str = "__temporal_inner__";

// ============================================================================
// TemporalValue storage helpers
// ============================================================================

/// Store a `TemporalValue` on a JsObject in the `__temporal_inner__` slot,
/// plus set the `__temporal_type__` branding.
pub(super) fn store_temporal_inner(obj: &GcRef<JsObject>, tv: TemporalValue) {
    let type_name = match &tv {
        TemporalValue::PlainDate(_) => "PlainDate",
        TemporalValue::PlainTime(_) => "PlainTime",
        TemporalValue::PlainDateTime(_) => "PlainDateTime",
        TemporalValue::PlainYearMonth(_) => "PlainYearMonth",
        TemporalValue::PlainMonthDay(_) => "PlainMonthDay",
        TemporalValue::Instant(_) => "Instant",
        TemporalValue::ZonedDateTime(_) => "ZonedDateTime",
        TemporalValue::Duration(_) => "Duration",
    };
    obj.define_property(
        PropertyKey::string(SLOT_TEMPORAL_INNER),
        PropertyDescriptor::builtin_data(Value::temporal(tv)),
    );
    obj.define_property(
        PropertyKey::string(SLOT_TEMPORAL_TYPE),
        PropertyDescriptor::builtin_data(Value::string(JsString::intern(type_name))),
    );
}

/// Extract the `TemporalValue` from a JsObject's `__temporal_inner__` slot.
pub(super) fn extract_temporal_inner(
    obj: &GcRef<JsObject>,
) -> Result<GcRef<TemporalValue>, VmError> {
    obj.get(&PropertyKey::string(SLOT_TEMPORAL_INNER))
        .and_then(|v| v.as_temporal())
        .ok_or_else(|| VmError::type_error("object is not a Temporal value"))
}

/// Extract a `temporal_rs::PlainDate` from a JsObject.
pub(super) fn extract_plain_date(obj: &GcRef<JsObject>) -> Result<temporal_rs::PlainDate, VmError> {
    let inner = extract_temporal_inner(obj)?;
    match &*inner {
        TemporalValue::PlainDate(pd) => Ok(pd.clone()),
        _ => Err(VmError::type_error("object is not a Temporal.PlainDate")),
    }
}

/// Extract a `temporal_rs::PlainTime` from a JsObject.
pub(super) fn extract_plain_time(obj: &GcRef<JsObject>) -> Result<temporal_rs::PlainTime, VmError> {
    let inner = extract_temporal_inner(obj)?;
    match &*inner {
        TemporalValue::PlainTime(pt) => Ok(*pt),
        _ => Err(VmError::type_error("object is not a Temporal.PlainTime")),
    }
}

/// Extract a `temporal_rs::PlainDateTime` from a JsObject.
pub(super) fn extract_plain_date_time(
    obj: &GcRef<JsObject>,
) -> Result<temporal_rs::PlainDateTime, VmError> {
    let inner = extract_temporal_inner(obj)?;
    match &*inner {
        TemporalValue::PlainDateTime(pdt) => Ok(pdt.clone()),
        _ => Err(VmError::type_error(
            "object is not a Temporal.PlainDateTime",
        )),
    }
}

/// Extract a `temporal_rs::Duration` from a JsObject's TemporalValue.
pub(super) fn extract_duration(obj: &GcRef<JsObject>) -> Result<temporal_rs::Duration, VmError> {
    let inner = extract_temporal_inner(obj)?;
    match &*inner {
        TemporalValue::Duration(d) => Ok(*d),
        _ => Err(VmError::type_error("object is not a Temporal.Duration")),
    }
}

/// Extract a `temporal_rs::Instant` from a JsObject.
pub(super) fn extract_instant(obj: &GcRef<JsObject>) -> Result<temporal_rs::Instant, VmError> {
    let inner = extract_temporal_inner(obj)?;
    match &*inner {
        TemporalValue::Instant(i) => Ok(*i),
        _ => Err(VmError::type_error("object is not a Temporal.Instant")),
    }
}

/// Extract a `temporal_rs::PlainYearMonth` from a JsObject.
pub(super) fn extract_plain_year_month(
    obj: &GcRef<JsObject>,
) -> Result<temporal_rs::PlainYearMonth, VmError> {
    let inner = extract_temporal_inner(obj)?;
    match &*inner {
        TemporalValue::PlainYearMonth(pym) => Ok(pym.clone()),
        _ => Err(VmError::type_error(
            "object is not a Temporal.PlainYearMonth",
        )),
    }
}

/// Extract a `temporal_rs::PlainMonthDay` from a JsObject.
pub(super) fn extract_plain_month_day(
    obj: &GcRef<JsObject>,
) -> Result<temporal_rs::PlainMonthDay, VmError> {
    let inner = extract_temporal_inner(obj)?;
    match &*inner {
        TemporalValue::PlainMonthDay(pmd) => Ok(pmd.clone()),
        _ => Err(VmError::type_error(
            "object is not a Temporal.PlainMonthDay",
        )),
    }
}

/// Extract a `temporal_rs::ZonedDateTime` from a JsObject.
pub(super) fn extract_zoned_date_time(
    obj: &GcRef<JsObject>,
) -> Result<temporal_rs::ZonedDateTime, VmError> {
    let inner = extract_temporal_inner(obj)?;
    match &*inner {
        TemporalValue::ZonedDateTime(zdt) => Ok(zdt.clone()),
        _ => Err(VmError::type_error(
            "object is not a Temporal.ZonedDateTime",
        )),
    }
}

/// Extract a `temporal_rs::PlainDate` from a JsObject that is either a PlainDate or PlainDateTime.
pub(super) fn extract_date_like(obj: &GcRef<JsObject>) -> Result<temporal_rs::PlainDate, VmError> {
    let inner = extract_temporal_inner(obj)?;
    match &*inner {
        TemporalValue::PlainDate(pd) => Ok(pd.clone()),
        TemporalValue::PlainDateTime(pdt) => {
            temporal_rs::PlainDate::try_new_iso(pdt.year(), pdt.month(), pdt.day())
                .map_err(temporal_err)
        }
        _ => Err(VmError::type_error("object is not a Temporal date-like")),
    }
}

// ============================================================================
// Error conversion
// ============================================================================

/// Convert a temporal_rs error into a VmError preserving TypeError vs RangeError
pub(super) fn temporal_err(e: temporal_rs::error::TemporalError) -> VmError {
    let msg = format!("{e}");
    match e.kind() {
        temporal_rs::error::ErrorKind::Type => VmError::type_error(msg),
        temporal_rs::error::ErrorKind::Range => VmError::range_error(msg),
        temporal_rs::error::ErrorKind::Syntax => VmError::range_error(msg),
        _ => VmError::type_error(msg),
    }
}

// ============================================================================
// Duration helpers
// ============================================================================

/// Duration field names in constructor order (public JS names)
pub(super) const DURATION_FIELDS: [&str; 10] = [
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

/// Internal slot names for Duration fields (avoid shadowing prototype getters)
pub(super) const DURATION_SLOTS: [&str; 10] = [
    "__dur_years",
    "__dur_months",
    "__dur_weeks",
    "__dur_days",
    "__dur_hours",
    "__dur_minutes",
    "__dur_seconds",
    "__dur_milliseconds",
    "__dur_microseconds",
    "__dur_nanoseconds",
];

/// ToIntegerIfIntegral (spec 7.4.39) — converts a JS value to an integer,
/// throwing RangeError for NaN, Infinity, or non-integer values.
pub(super) fn to_integer_if_integral(
    ncx: &mut NativeContext<'_>,
    val: &Value,
) -> Result<f64, VmError> {
    let n = ncx.to_number_value(val)?;
    if n.is_nan() {
        return Err(VmError::range_error("Cannot convert NaN to an integer"));
    }
    if n.is_infinite() {
        return Err(VmError::range_error(
            "Cannot convert Infinity to an integer",
        ));
    }
    if n != n.trunc() {
        return Err(VmError::range_error("Value must be an integer"));
    }
    Ok(n)
}

/// Extract a `temporal_rs::Duration` from a JsObject — tries new TemporalValue first,
/// falls back to legacy duration slots.
pub(super) fn extract_duration_from_slots(
    obj: &GcRef<JsObject>,
) -> Result<temporal_rs::Duration, VmError> {
    // Try new TemporalValue path first
    if let Ok(d) = extract_duration(obj) {
        return Ok(d);
    }
    // Legacy fallback: read individual slots
    let get_f64 = |slot: &str| -> f64 {
        obj.get(&PropertyKey::string(slot))
            .and_then(|v| v.as_number())
            .unwrap_or(0.0)
    };
    temporal_rs::Duration::new(
        get_f64(DURATION_SLOTS[0]) as i64,
        get_f64(DURATION_SLOTS[1]) as i64,
        get_f64(DURATION_SLOTS[2]) as i64,
        get_f64(DURATION_SLOTS[3]) as i64,
        get_f64(DURATION_SLOTS[4]) as i64,
        get_f64(DURATION_SLOTS[5]) as i64,
        get_f64(DURATION_SLOTS[6]) as i64,
        get_f64(DURATION_SLOTS[7]) as i64,
        get_f64(DURATION_SLOTS[8]) as i128,
        get_f64(DURATION_SLOTS[9]) as i128,
    )
    .map_err(temporal_err)
}

/// Create a new Duration JS object from a `temporal_rs::Duration`, storing
/// the TemporalValue in the `__temporal_inner__` slot.
pub(super) fn construct_duration_object(
    dur: &temporal_rs::Duration,
    proto: &GcRef<JsObject>,
    mm: &Arc<MemoryManager>,
) -> GcRef<JsObject> {
    let obj = GcRef::new(JsObject::new(Value::object(proto.clone()), mm.clone()));
    store_temporal_inner(&obj, TemporalValue::Duration(*dur));
    obj
}

// ============================================================================
// ISO calendar math
// ============================================================================

/// Days in each month for a common year (index 0 = unused, 1-12 = Jan-Dec)
pub(super) const DAYS_IN_MONTH: [u32; 13] = [0, 31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];

pub(super) fn is_leap_year(year: i32) -> bool {
    (year % 4 == 0 && year % 100 != 0) || year % 400 == 0
}

pub(super) fn days_in_month(month: u32, year: i32) -> u32 {
    if month == 2 && is_leap_year(year) {
        29
    } else if month >= 1 && month <= 12 {
        DAYS_IN_MONTH[month as usize]
    } else {
        31
    }
}

/// Convert ISO date to days from Unix epoch (1970-01-01)
pub(super) fn iso_date_to_epoch_days(year: i32, month: i32, day: i32) -> i64 {
    // Algorithm from https://howardhinnant.github.io/date_algorithms.html
    let y = if month <= 2 {
        year as i64 - 1
    } else {
        year as i64
    };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = (y - era * 400) as u32;
    let m = month as u32;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + day as u32 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe as i64 - 719468
}

/// Parse timezone offset string to nanoseconds
/// Supports: "UTC", "+HH:MM", "-HH:MM", "+HH:MM:SS", "+HHMM", "+HH"
pub(super) fn parse_tz_offset_ns(tz_id: &str) -> Result<i128, VmError> {
    let upper = tz_id.to_ascii_uppercase();
    if upper == "UTC" || upper == "Z" {
        return Ok(0);
    }
    if tz_id.starts_with('+') || tz_id.starts_with('-') || tz_id.starts_with('\u{2212}') {
        let sign: i128 = if tz_id.starts_with('-') || tz_id.starts_with('\u{2212}') {
            -1
        } else {
            1
        };
        let offset_part = if tz_id.starts_with('\u{2212}') {
            &tz_id[3..]
        } else {
            &tz_id[1..]
        };
        let parts: Vec<&str> = offset_part.split(':').collect();
        let (hours, minutes, seconds) = match parts.len() {
            1 => {
                // +HH or +HHMM or +HHMMSS
                if offset_part.len() == 2 {
                    (offset_part.parse::<i128>().unwrap_or(0), 0i128, 0i128)
                } else if offset_part.len() == 4 {
                    let h = offset_part[..2].parse::<i128>().unwrap_or(0);
                    let m = offset_part[2..4].parse::<i128>().unwrap_or(0);
                    (h, m, 0i128)
                } else if offset_part.len() >= 6 {
                    let h = offset_part[..2].parse::<i128>().unwrap_or(0);
                    let m = offset_part[2..4].parse::<i128>().unwrap_or(0);
                    let s = offset_part[4..6].parse::<i128>().unwrap_or(0);
                    (h, m, s)
                } else {
                    return Err(VmError::range_error(format!(
                        "invalid time zone offset: {}",
                        tz_id
                    )));
                }
            }
            2 => {
                let h = parts[0].parse::<i128>().unwrap_or(0);
                let m = parts[1].parse::<i128>().unwrap_or(0);
                (h, m, 0i128)
            }
            3 => {
                let h = parts[0].parse::<i128>().unwrap_or(0);
                let m = parts[1].parse::<i128>().unwrap_or(0);
                // seconds might have fractional part
                let s_parts: Vec<&str> = parts[2].split('.').collect();
                let s = s_parts[0].parse::<i128>().unwrap_or(0);
                (h, m, s)
            }
            _ => {
                return Err(VmError::range_error(format!(
                    "invalid time zone offset: {}",
                    tz_id
                )));
            }
        };
        return Ok(
            sign * (hours * 3_600_000_000_000 + minutes * 60_000_000_000 + seconds * 1_000_000_000)
        );
    }
    // Named timezone (e.g., "America/New_York") — not supported without IANA data
    Err(VmError::range_error(format!(
        "named time zone {} requires IANA timezone data which is not available",
        tz_id
    )))
}

/// Per spec: ToPrimitive(value, string) → RequireString.
/// Handles Proxy-wrapped values by first converting to a primitive.
pub(super) fn to_primitive_require_string(
    ncx: &mut NativeContext<'_>,
    val: &Value,
) -> Result<GcRef<crate::string::JsString>, VmError> {
    require_string_impl(ncx, val, "monthCode")
}

/// Like to_primitive_require_string but with a configurable field name for error messages.
pub(super) fn require_string_for_field(
    ncx: &mut NativeContext<'_>,
    val: &Value,
    field_name: &str,
) -> Result<GcRef<crate::string::JsString>, VmError> {
    require_string_impl(ncx, val, field_name)
}

fn require_string_impl(
    ncx: &mut NativeContext<'_>,
    val: &Value,
    field_name: &str,
) -> Result<GcRef<crate::string::JsString>, VmError> {
    if val.as_symbol().is_some() {
        return Err(VmError::type_error(
            "Cannot convert a Symbol value to a string",
        ));
    }
    // ToPrimitive for objects/proxies calls toString/valueOf
    let primitive = if val.as_object().is_some() || val.as_proxy().is_some() {
        ncx.to_primitive(val, crate::interpreter::PreferredType::String)?
    } else {
        val.clone()
    };
    // RequireString: result must be a String
    if !primitive.is_string() {
        return Err(VmError::type_error(format!(
            "{} must be a string, got {}",
            field_name,
            primitive.type_of()
        )));
    }
    Ok(primitive.as_string().unwrap().clone())
}

/// Convert a ToIntegerWithTruncation per Temporal spec (like ToIntegerIfIntegral but truncates)
pub(super) fn to_integer_with_truncation(
    ncx: &mut NativeContext<'_>,
    val: &Value,
) -> Result<f64, VmError> {
    if val.is_undefined() {
        return Err(VmError::range_error("undefined is not a valid integer"));
    }
    let n = ncx.to_number_value(val)?;
    if n.is_nan() || n.is_infinite() {
        return Err(VmError::range_error(format!(
            "{} is not a valid integer for Temporal",
            n
        )));
    }
    Ok(n.trunc())
}

// ============================================================================
// Month/calendar validation
// ============================================================================

/// Validate ISO month-day, returning (month, day, referenceISOYear)
pub(super) fn validate_iso_month_day(
    month: i32,
    day: i32,
    reference_year: i32,
) -> Result<(u32, u32, i32), VmError> {
    if month < 1 || month > 12 {
        return Err(VmError::range_error(format!(
            "month must be between 1 and 12, got {}",
            month
        )));
    }
    let max_day = days_in_month(month as u32, reference_year);
    if day < 1 || day as u32 > max_day {
        return Err(VmError::range_error(format!(
            "day must be between 1 and {}, got {}",
            max_day, day
        )));
    }
    Ok((month as u32, day as u32, reference_year))
}

/// Parse a monthCode string like "M01" through "M12" (lenient)
pub(super) fn parse_month_code(s: &str) -> Option<u32> {
    let mc = temporal_rs::MonthCode::try_from_utf8(s.as_bytes()).ok()?;
    if mc.is_leap_month() {
        return None;
    }
    let m = mc.to_month_integer() as u32;
    if m >= 1 && m <= 12 { Some(m) } else { None }
}

/// Validate monthCode SYNTAX — delegates to temporal_rs::MonthCode
pub(super) fn validate_month_code_syntax(s: &str) -> Result<(), VmError> {
    temporal_rs::MonthCode::try_from_utf8(s.as_bytes())
        .map(|_| ())
        .map_err(|_| VmError::range_error(format!("monthCode '{}' is not well-formed", s)))
}

/// Validate monthCode for ISO calendar — no leap months, range 1-12
pub(super) fn validate_month_code_iso_suitability(s: &str) -> Result<u32, VmError> {
    let mc = temporal_rs::MonthCode::try_from_utf8(s.as_bytes())
        .map_err(|_| VmError::range_error(format!("monthCode '{}' is not well-formed", s)))?;
    if mc.is_leap_month() {
        return Err(VmError::range_error(format!(
            "monthCode {} is not valid for ISO 8601 calendar",
            s
        )));
    }
    let month = mc.to_month_integer() as u32;
    if month < 1 || month > 12 {
        return Err(VmError::range_error(format!(
            "monthCode {} is not valid for ISO 8601 calendar",
            s
        )));
    }
    Ok(month)
}

/// Resolve calendar from a property bag's calendar property
pub(super) fn resolve_calendar_from_property(
    ncx: &mut NativeContext<'_>,
    val: &Value,
) -> Result<(), VmError> {
    // Per spec: null, boolean, number, bigint → TypeError
    if val.is_null() || val.is_boolean() || val.is_number() || val.is_bigint() {
        return Err(VmError::type_error(format!(
            "{} is not a valid calendar",
            if val.is_null() {
                "null".to_string()
            } else {
                val.type_of().to_string()
            }
        )));
    }
    if val.as_symbol().is_some() {
        return Err(VmError::type_error(
            "Cannot convert a Symbol value to a string",
        ));
    }
    // Per spec ToTemporalCalendar step 1.a:
    // If temporalCalendarLike has a [[InitializedTemporalDate]] etc., return its [[Calendar]]
    if let Some(obj) = val.as_object() {
        if let Some(ty) = obj
            .get(&PropertyKey::string(SLOT_TEMPORAL_TYPE))
            .and_then(|v| v.as_string().map(|s| s.as_str().to_string()))
        {
            match ty.as_str() {
                "PlainDate" | "PlainDateTime" | "PlainMonthDay" | "PlainYearMonth"
                | "ZonedDateTime" => {
                    // Valid Temporal object — its calendar is always iso8601
                    return Ok(());
                }
                _ => {
                    // Duration or other non-calendar Temporal type → TypeError
                    return Err(VmError::type_error(format!(
                        "{} is not a valid calendar",
                        ty
                    )));
                }
            }
        }
        // Plain object (not a Temporal type) → TypeError
        return Err(VmError::type_error("object is not a valid calendar"));
    }

    let cal_str = ncx.to_string_value(val)?;
    let s = cal_str.as_str();

    if s.is_empty() {
        return Err(VmError::range_error("calendar must not be empty string"));
    }

    // Check for -000000 (negative zero year) in any ISO-like string
    let date_part_for_check = if let Some(bracket_pos) = s.find('[') {
        &s[..bracket_pos]
    } else {
        s
    };
    let date_only_for_check = if let Some(t_pos) = date_part_for_check.find('T') {
        &date_part_for_check[..t_pos]
    } else {
        date_part_for_check
    };
    if date_only_for_check.starts_with("-000000") {
        return Err(VmError::range_error("reject minus zero as extended year"));
    }

    // Per spec GetTemporalCalendarIdentifierWithDefault: if the calendar value is a
    // string, try parsing as an ISO string first (extract calendar annotation),
    // then fall back to parsing as a bare calendar identifier.
    let lower = s.to_ascii_lowercase();
    if lower == "iso8601" {
        return Ok(());
    }

    // Try parsing as various Temporal ISO string formats — extract calendar
    if temporal_rs::PlainDateTime::from_utf8(s.as_bytes()).is_ok()
        || temporal_rs::PlainDate::from_utf8(s.as_bytes()).is_ok()
        || temporal_rs::PlainMonthDay::from_utf8(s.as_bytes()).is_ok()
        || temporal_rs::PlainYearMonth::from_utf8(s.as_bytes()).is_ok()
        || temporal_rs::ZonedDateTime::from_utf8_with_provider(
            s.as_bytes(),
            temporal_rs::options::Disambiguation::Compatible,
            temporal_rs::options::OffsetDisambiguation::Use,
            tz_provider(),
        )
        .is_ok()
        || temporal_rs::Instant::from_utf8(s.as_bytes()).is_ok()
    {
        // Valid ISO string — calendar annotation (if present) is validated by temporal_rs
        return Ok(());
    }

    // Not a valid ISO string — try as plain calendar ID
    let _cal: temporal_rs::Calendar = s
        .parse()
        .map_err(|_| VmError::range_error(format!("{} is not a valid calendar ID", s)))?;
    Ok(())
}

/// Format monthCode from month number
pub(super) fn format_month_code(month: u32) -> String {
    format!("M{:02}", month)
}

// ============================================================================
// Extract helpers
// ============================================================================

/// Extract a temporal_rs::PlainDateTime from a JsObject — tries TemporalValue first,
/// falls back to legacy ISO slots.
pub(super) fn extract_pdt_standalone(
    obj: &GcRef<JsObject>,
) -> Result<temporal_rs::PlainDateTime, VmError> {
    // Try new TemporalValue path first
    if let Ok(pdt) = extract_plain_date_time(obj) {
        return Ok(pdt);
    }
    // Legacy fallback
    let y = obj
        .get(&PropertyKey::string(SLOT_ISO_YEAR))
        .and_then(|v| v.as_int32())
        .unwrap_or(0);
    let mo = obj
        .get(&PropertyKey::string(SLOT_ISO_MONTH))
        .and_then(|v| v.as_int32())
        .unwrap_or(1) as u8;
    let d = obj
        .get(&PropertyKey::string(SLOT_ISO_DAY))
        .and_then(|v| v.as_int32())
        .unwrap_or(1) as u8;
    let h = obj
        .get(&PropertyKey::string(SLOT_ISO_HOUR))
        .and_then(|v| v.as_int32())
        .unwrap_or(0) as u8;
    let mi = obj
        .get(&PropertyKey::string(SLOT_ISO_MINUTE))
        .and_then(|v| v.as_int32())
        .unwrap_or(0) as u8;
    let sec = obj
        .get(&PropertyKey::string(SLOT_ISO_SECOND))
        .and_then(|v| v.as_int32())
        .unwrap_or(0) as u8;
    let ms = obj
        .get(&PropertyKey::string(SLOT_ISO_MILLISECOND))
        .and_then(|v| v.as_int32())
        .unwrap_or(0) as u16;
    let us = obj
        .get(&PropertyKey::string(SLOT_ISO_MICROSECOND))
        .and_then(|v| v.as_int32())
        .unwrap_or(0) as u16;
    let ns = obj
        .get(&PropertyKey::string(SLOT_ISO_NANOSECOND))
        .and_then(|v| v.as_int32())
        .unwrap_or(0) as u16;
    temporal_rs::PlainDateTime::try_new(
        y,
        mo,
        d,
        h,
        mi,
        sec,
        ms,
        us,
        ns,
        temporal_rs::Calendar::default(),
    )
    .map_err(temporal_err)
}

/// Extract a `temporal_rs::PlainDate` from a JsObject — tries TemporalValue first,
/// falls back to legacy ISO slots with branding check.
pub(super) fn extract_iso_date_from_slots(
    obj: &GcRef<JsObject>,
) -> Result<temporal_rs::PlainDate, VmError> {
    // Try new TemporalValue path first
    if let Ok(pd) = extract_plain_date(obj) {
        return Ok(pd);
    }
    // Legacy fallback: branding check + individual slots
    let tt = obj
        .get(&PropertyKey::string(SLOT_TEMPORAL_TYPE))
        .and_then(|v| v.as_string().map(|s| s.as_str().to_string()));
    if tt.as_deref() != Some("PlainDate") {
        return Err(VmError::type_error("this is not a Temporal.PlainDate"));
    }
    let y = obj
        .get(&PropertyKey::string(SLOT_ISO_YEAR))
        .and_then(|v| v.as_int32())
        .unwrap_or(0);
    let m = obj
        .get(&PropertyKey::string(SLOT_ISO_MONTH))
        .and_then(|v| v.as_int32())
        .unwrap_or(1) as u8;
    let d = obj
        .get(&PropertyKey::string(SLOT_ISO_DAY))
        .and_then(|v| v.as_int32())
        .unwrap_or(1) as u8;
    temporal_rs::PlainDate::try_new(y, m, d, temporal_rs::Calendar::default()).map_err(temporal_err)
}

/// Extract ISO date fields from an object with PlainDate OR PlainDateTime branding.
/// Tries TemporalValue first, falls back to legacy slots.
pub(super) fn extract_iso_date_from_date_like_slots(
    obj: &GcRef<JsObject>,
) -> Result<temporal_rs::PlainDate, VmError> {
    // Try new TemporalValue path first
    if let Ok(pd) = extract_date_like(obj) {
        return Ok(pd);
    }
    // Legacy fallback
    let tt = obj
        .get(&PropertyKey::string(SLOT_TEMPORAL_TYPE))
        .and_then(|v| v.as_string().map(|s| s.as_str().to_string()));
    match tt.as_deref() {
        Some("PlainDate") | Some("PlainDateTime") => {}
        _ => {
            return Err(VmError::type_error(
                "this is not a Temporal.PlainDate or Temporal.PlainDateTime",
            ));
        }
    }
    let y = obj
        .get(&PropertyKey::string(SLOT_ISO_YEAR))
        .and_then(|v| v.as_int32())
        .unwrap_or(0);
    let m = obj
        .get(&PropertyKey::string(SLOT_ISO_MONTH))
        .and_then(|v| v.as_int32())
        .unwrap_or(1) as u8;
    let d = obj
        .get(&PropertyKey::string(SLOT_ISO_DAY))
        .and_then(|v| v.as_int32())
        .unwrap_or(1) as u8;
    temporal_rs::PlainDate::try_new(y, m, d, temporal_rs::Calendar::default()).map_err(temporal_err)
}

// ============================================================================
// Standalone conversion helpers
// ============================================================================

/// Standalone calendar validation (for use outside of install_plain_datetime block scope)
pub(super) fn validate_calendar_arg_standalone(
    ncx: &mut NativeContext<'_>,
    cal: &Value,
) -> Result<String, VmError> {
    if cal.is_undefined() {
        return Ok("iso8601".to_string());
    }
    if cal.as_symbol().is_some() {
        return Err(VmError::type_error(
            "Cannot convert a Symbol value to a string",
        ));
    }
    if let Some(obj) = cal.as_object() {
        let tt = obj
            .get(&PropertyKey::string(SLOT_TEMPORAL_TYPE))
            .and_then(|v| v.as_string().map(|s| s.as_str().to_string()));
        match tt.as_deref() {
            Some("PlainDate")
            | Some("PlainDateTime")
            | Some("PlainMonthDay")
            | Some("PlainYearMonth")
            | Some("ZonedDateTime") => return Ok("iso8601".to_string()),
            Some("Duration") | Some("Instant") => {
                return Err(VmError::type_error(format!(
                    "{} instance is not a valid calendar",
                    tt.unwrap()
                )));
            }
            _ => {}
        }
    }
    if !cal.is_string() {
        if cal.is_null()
            || cal.is_boolean()
            || cal.is_number()
            || cal.is_bigint()
            || cal.as_object().is_some()
        {
            return Err(VmError::type_error(format!(
                "{} is not a valid calendar",
                ncx.to_string_value(cal).unwrap_or_default()
            )));
        }
        return Err(VmError::type_error("calendar must be a string"));
    }
    let s = cal.as_string().unwrap().as_str().to_string();
    if s.is_empty() {
        return Err(VmError::range_error(
            "empty string is not a valid calendar ID",
        ));
    }
    // Per spec, calendar ID must be a bare identifier (e.g., "iso8601", "japanese"),
    // NOT an ISO date string with a calendar annotation like "1997-12-04[u-ca=iso8601]".
    if s.contains('[') || s.contains(']') {
        return Err(VmError::range_error(format!(
            "{} is not a valid calendar ID",
            s
        )));
    }
    // Validate via temporal_rs::Calendar which supports all ICU calendars
    let cal_obj: temporal_rs::Calendar = s
        .parse()
        .map_err(|_| VmError::range_error(format!("{} is not a valid calendar ID", s)))?;
    Ok(cal_obj.identifier().to_string())
}

/// Standalone ToTemporalDateTime — for use outside install_plain_datetime block scope
pub(super) fn to_temporal_datetime_standalone(
    ncx: &mut NativeContext<'_>,
    item: &Value,
) -> Result<temporal_rs::PlainDateTime, VmError> {
    if item.is_string() {
        let s = ncx.to_string_value(item)?;
        reject_utc_designator_for_plain(s.as_str())?;
        return temporal_rs::PlainDateTime::from_utf8(s.as_bytes()).map_err(temporal_err);
    }
    if item.is_undefined()
        || item.is_null()
        || item.is_boolean()
        || item.is_number()
        || item.is_bigint()
    {
        return Err(VmError::type_error(format!(
            "cannot convert {} to a PlainDateTime",
            item.type_of()
        )));
    }
    if item.as_symbol().is_some() {
        return Err(VmError::type_error(
            "Cannot convert a Symbol value to a string",
        ));
    }
    if let Some(obj) = item.as_object() {
        let temporal_type = obj
            .get(&PropertyKey::string(SLOT_TEMPORAL_TYPE))
            .and_then(|v| v.as_string().map(|s| s.as_str().to_string()));
        if temporal_type.as_deref() == Some("PlainDateTime") {
            return extract_pdt_standalone(&obj);
        }
        if temporal_type.as_deref() == Some("PlainDate") {
            let y = obj
                .get(&PropertyKey::string(SLOT_ISO_YEAR))
                .and_then(|v| v.as_int32())
                .unwrap_or(0);
            let mo = obj
                .get(&PropertyKey::string(SLOT_ISO_MONTH))
                .and_then(|v| v.as_int32())
                .unwrap_or(1);
            let d = obj
                .get(&PropertyKey::string(SLOT_ISO_DAY))
                .and_then(|v| v.as_int32())
                .unwrap_or(1);
            return temporal_rs::PlainDateTime::try_new(
                y,
                mo as u8,
                d as u8,
                0,
                0,
                0,
                0,
                0,
                0,
                temporal_rs::Calendar::default(),
            )
            .map_err(temporal_err);
        }
        // ZonedDateTime → convert to PlainDateTime via temporal_rs
        if temporal_type.as_deref() == Some("ZonedDateTime") {
            let zdt = extract_zoned_date_time(&obj)?;
            return Ok(zdt.to_plain_date_time());
        }
        // Property bag: calendar, day, hour, microsecond, millisecond, minute, month, monthCode, nanosecond, second, year
        let calendar_val = ncx.get_property(&obj, &PropertyKey::string("calendar"))?;
        if !calendar_val.is_undefined() {
            resolve_calendar_from_property(ncx, &calendar_val)?;
        }
        let day_val = ncx.get_property(&obj, &PropertyKey::string("day"))?;
        let d = if !day_val.is_undefined() {
            let n = ncx.to_number_value(&day_val)?;
            if n.is_infinite() {
                return Err(VmError::range_error("day cannot be Infinity"));
            }
            n as i32
        } else {
            return Err(VmError::type_error("day is required"));
        };
        let hour_val = ncx.get_property(&obj, &PropertyKey::string("hour"))?;
        let h = if !hour_val.is_undefined() {
            let n = ncx.to_number_value(&hour_val)?;
            if n.is_infinite() {
                return Err(VmError::range_error("hour cannot be Infinity"));
            }
            n as i32
        } else {
            0
        };
        let us_val = ncx.get_property(&obj, &PropertyKey::string("microsecond"))?;
        let us = if !us_val.is_undefined() {
            let n = ncx.to_number_value(&us_val)?;
            if n.is_infinite() {
                return Err(VmError::range_error("microsecond cannot be Infinity"));
            }
            n as i32
        } else {
            0
        };
        let ms_val = ncx.get_property(&obj, &PropertyKey::string("millisecond"))?;
        let ms = if !ms_val.is_undefined() {
            let n = ncx.to_number_value(&ms_val)?;
            if n.is_infinite() {
                return Err(VmError::range_error("millisecond cannot be Infinity"));
            }
            n as i32
        } else {
            0
        };
        let min_val = ncx.get_property(&obj, &PropertyKey::string("minute"))?;
        let mi = if !min_val.is_undefined() {
            let n = ncx.to_number_value(&min_val)?;
            if n.is_infinite() {
                return Err(VmError::range_error("minute cannot be Infinity"));
            }
            n as i32
        } else {
            0
        };
        let month_val = ncx.get_property(&obj, &PropertyKey::string("month"))?;
        let month_code_val = ncx.get_property(&obj, &PropertyKey::string("monthCode"))?;
        let month = if !month_code_val.is_undefined() {
            let mc_str = ncx.to_string_value(&month_code_val)?;
            validate_month_code_syntax(&mc_str)?;
            validate_month_code_iso_suitability(&mc_str)? as i32
        } else if !month_val.is_undefined() {
            let n = ncx.to_number_value(&month_val)?;
            if n.is_infinite() {
                return Err(VmError::range_error("month cannot be Infinity"));
            }
            n as i32
        } else {
            return Err(VmError::type_error("month or monthCode is required"));
        };
        let ns_val = ncx.get_property(&obj, &PropertyKey::string("nanosecond"))?;
        let ns = if !ns_val.is_undefined() {
            let n = ncx.to_number_value(&ns_val)?;
            if n.is_infinite() {
                return Err(VmError::range_error("nanosecond cannot be Infinity"));
            }
            n as i32
        } else {
            0
        };
        let sec_val = ncx.get_property(&obj, &PropertyKey::string("second"))?;
        let sec = if !sec_val.is_undefined() {
            let n = ncx.to_number_value(&sec_val)?;
            if n.is_infinite() {
                return Err(VmError::range_error("second property cannot be Infinity"));
            }
            let sv = n as i32;
            if sv == 60 { 59 } else { sv }
        } else {
            0
        };
        let year_val = ncx.get_property(&obj, &PropertyKey::string("year"))?;
        let y = if !year_val.is_undefined() {
            let n = ncx.to_number_value(&year_val)?;
            if n.is_infinite() {
                return Err(VmError::range_error("year cannot be Infinity"));
            }
            n as i32
        } else {
            return Err(VmError::type_error("year is required"));
        };
        return temporal_rs::PlainDateTime::try_new(
            y,
            month as u8,
            d as u8,
            h as u8,
            mi as u8,
            sec as u8,
            ms as u16,
            us as u16,
            ns as u16,
            temporal_rs::Calendar::default(),
        )
        .map_err(temporal_err);
    }
    Err(VmError::type_error("Expected an object or string"))
}

// ============================================================================
// Temporal ISO string parsing
// ============================================================================

/// Parse an ISO date string for PlainMonthDay, returning (year, month, day)
/// Uses temporal_rs for spec-compliant parsing of all ISO 8601 + RFC 9557 formats.
pub(super) fn parse_temporal_month_day_string(s: &str) -> Result<(i32, u32, u32), VmError> {
    let pmd = temporal_rs::PlainMonthDay::from_utf8(s.as_bytes()).map_err(temporal_err)?;
    Ok((
        pmd.reference_year(),
        pmd.month_code().to_month_integer() as u32,
        pmd.day() as u32,
    ))
}

/// Find the position of a time separator (T, t, or space) in an ISO datetime string
/// Returns None if no time separator found
pub(super) fn find_time_separator(s: &str) -> Option<usize> {
    // For extended year format (+/-YYYYYY-MM-DD), we need to skip the date portion
    let bytes = s.as_bytes();

    // Check for T or t anywhere
    if let Some(pos) = s.find('T').or_else(|| s.find('t')) {
        return Some(pos);
    }

    // Check for space as separator — must be after the date portion
    // Date portion is at least "MM-DD" (5 chars) or "YYYY-MM-DD" (10 chars)
    // Find space after position 5
    for (i, &b) in bytes.iter().enumerate() {
        if b == b' ' && i >= 5 {
            return Some(i);
        }
    }

    None
}

/// Check if a bare date string (without time) has a standalone UTC offset
/// that isn't a date separator. E.g., "09-15+01:00" has an offset, "09-15" doesn't.
pub(super) fn has_standalone_utc_offset(s: &str) -> bool {
    // Look for +/-HH:MM pattern after date portion
    // MM-DD is 5 chars, --MM-DD is 7 chars, YYYY-MM-DD is 10 chars
    let bytes = s.as_bytes();
    let len = bytes.len();

    // After valid date portion, look for +/- that starts a UTC offset
    // A date string: MM-DD (5), --MM-DD (7), YYYY-MM-DD (10), +/-YYYYYY-MM-DD (16)
    // An offset starts with + or - followed by HH or HH:MM or HHMM

    // Check for +HH:MM or -HH:MM after date
    if len >= 11 {
        // Could be YYYY-MM-DD+HH:MM or MM-DD+HH:MM
        for start in [5usize, 7, 10] {
            if start < len && (bytes[start] == b'+' || bytes[start] == b'-') {
                // Check if the rest is HH:MM
                let rest = &s[start + 1..];
                if rest.len() >= 5
                    && rest.as_bytes()[2] == b':'
                    && rest[..2].chars().all(|c| c.is_ascii_digit())
                    && rest[3..5].chars().all(|c| c.is_ascii_digit())
                {
                    return true;
                }
                if rest.len() >= 2 && rest[..2].chars().all(|c| c.is_ascii_digit()) {
                    return true;
                }
            }
        }
    } else if len >= 8 {
        // MM-DD+HH  (5+3 = 8)
        if (bytes[5] == b'+' || bytes[5] == b'-') && s[6..].chars().all(|c| c.is_ascii_digit()) {
            return true;
        }
    }

    false
}

/// Check if a string has a UTC offset (Z, +HH:MM, -HH:MM, +HH, -HH)
pub(super) fn has_utc_offset(s: &str) -> bool {
    if s.ends_with('Z') || s.ends_with('z') {
        return true;
    }
    let bytes = s.as_bytes();
    let len = bytes.len();
    // +HH:MM or -HH:MM at end
    if len >= 6 {
        let offset_start = len - 6;
        if (bytes[offset_start] == b'+' || bytes[offset_start] == b'-')
            && bytes[offset_start + 3] == b':'
        {
            return true;
        }
    }
    // +HHMM or -HHMM at end
    if len >= 5 {
        let offset_start = len - 5;
        if (bytes[offset_start] == b'+' || bytes[offset_start] == b'-')
            && bytes[offset_start + 1..].iter().all(|b| b.is_ascii_digit())
        {
            return true;
        }
    }
    // +HH or -HH at end
    if len >= 3 {
        let offset_start = len - 3;
        if (bytes[offset_start] == b'+' || bytes[offset_start] == b'-')
            && bytes[offset_start + 1].is_ascii_digit()
            && bytes[offset_start + 2].is_ascii_digit()
        {
            return true;
        }
    }
    false
}

pub(super) fn validate_annotations(s: &str) -> Result<(), VmError> {
    // Parse annotations like [UTC][u-ca=iso8601][!u-ca=iso8601]
    let mut remaining = s;
    let mut seen_calendar = false;
    let mut seen_critical = false;
    let mut seen_timezone = false;
    let mut _calendar_value = String::new();

    while !remaining.is_empty() {
        if !remaining.starts_with('[') {
            return Err(VmError::range_error(format!(
                "unexpected character in annotations: {}",
                remaining
            )));
        }

        let close = remaining
            .find(']')
            .ok_or_else(|| VmError::range_error("unterminated annotation bracket"))?;

        let inner = &remaining[1..close];
        remaining = &remaining[close + 1..];

        // Check for critical flag
        let (is_critical, content) = if inner.starts_with('!') {
            (true, &inner[1..])
        } else {
            (false, inner)
        };

        if content.contains('=') {
            // Key-value annotation like u-ca=iso8601
            let parts: Vec<&str> = content.splitn(2, '=').collect();
            let key = parts[0];
            let value = parts[1];

            // Keys must be lowercase
            if key.chars().any(|c| c.is_ascii_uppercase()) {
                return Err(VmError::range_error(format!(
                    "annotation keys must be lowercase: {} - invalid capitalized key",
                    s
                )));
            }

            if key == "u-ca" {
                if seen_calendar {
                    // Multiple calendar annotations
                    if is_critical || seen_critical {
                        return Err(VmError::range_error(format!(
                            "reject more than one calendar annotation if any critical: {}",
                            s
                        )));
                    }
                    // Non-critical duplicate: ignore (per spec, first wins)
                } else {
                    seen_calendar = true;
                    if is_critical {
                        seen_critical = true;
                    }
                    // Validate calendar ID via temporal_rs (supports all ICU calendars)
                    let _cal: temporal_rs::Calendar = value.parse().map_err(|_| {
                        VmError::range_error(format!("Unknown calendar: {}", value))
                    })?;
                    _calendar_value = value.to_string();
                }
            } else if is_critical {
                // Unknown critical annotation
                return Err(VmError::range_error(format!(
                    "reject unknown annotation with critical flag: {}",
                    s
                )));
            }
        } else {
            // Time zone annotation like UTC, America/New_York, etc.
            if seen_timezone {
                return Err(VmError::range_error(format!(
                    "reject more than one time zone annotation: {}",
                    s
                )));
            }
            seen_timezone = true;
        }
    }

    Ok(())
}

// ============================================================================
// Proxy helpers and overflow option parsing
// ============================================================================

/// Read a property from a proxy using proxy_get trap
pub(super) fn proxy_get_property(
    ncx: &mut NativeContext<'_>,
    proxy: GcRef<crate::proxy::JsProxy>,
    receiver: &Value,
    key: &str,
) -> Result<Value, VmError> {
    let pk = PropertyKey::string(key);
    let kv = crate::proxy_operations::property_key_to_value_pub(&pk);
    crate::proxy_operations::proxy_get(ncx, proxy.clone(), &pk, kv, receiver.clone())
}

pub(super) fn parse_overflow_option(
    ncx: &mut NativeContext<'_>,
    options_val: &Value,
) -> Result<Overflow, VmError> {
    if options_val.is_undefined() {
        return Ok(Overflow::Constrain);
    }

    // Per spec: null is treated as empty options object (no overflow property)
    if options_val.is_null() {
        return Err(VmError::type_error(
            "options must be an object or undefined",
        ));
    }

    // Primitives (string, number, boolean, symbol, bigint) are not valid
    if options_val.is_string() || options_val.is_number() || options_val.is_boolean() {
        return Err(VmError::type_error(
            "options must be an object or undefined",
        ));
    }

    if options_val.as_symbol().is_some() {
        return Err(VmError::type_error(
            "options must be an object or undefined",
        ));
    }

    if options_val.is_bigint() {
        return Err(VmError::type_error(
            "options must be an object or undefined",
        ));
    }

    if let Some(obj) = options_val.as_object() {
        // Use ncx.get_property for observable getter invocation (per spec)
        let overflow_val = ncx.get_property(&obj, &PropertyKey::string("overflow"))?;
        if overflow_val.is_undefined() {
            return Ok(Overflow::Constrain);
        }
        let overflow_str = ncx.to_string_value(&overflow_val)?;
        match overflow_str.as_str() {
            "constrain" => return Ok(Overflow::Constrain),
            "reject" => return Ok(Overflow::Reject),
            other => {
                return Err(VmError::range_error(format!(
                    "{} is not a valid value for overflow",
                    other
                )));
            }
        }
    }

    // Handle Proxy as options (TemporalHelpers.propertyBagObserver creates Proxies)
    if let Some(proxy) = options_val.as_proxy() {
        let key = PropertyKey::string("overflow");
        let key_value = crate::proxy_operations::property_key_to_value_pub(&key);
        let overflow_val =
            crate::proxy_operations::proxy_get(ncx, proxy, &key, key_value, options_val.clone())?;
        if overflow_val.is_undefined() {
            return Ok(Overflow::Constrain);
        }
        let overflow_str = ncx.to_string_value(&overflow_val)?;
        match overflow_str.as_str() {
            "constrain" => return Ok(Overflow::Constrain),
            "reject" => return Ok(Overflow::Reject),
            other => {
                return Err(VmError::range_error(format!(
                    "{} is not a valid value for overflow",
                    other
                )));
            }
        }
    }

    // Functions and other non-plain objects are acceptable as options
    Ok(Overflow::Constrain)
}

// ============================================================================
// Timezone offset parsing (second version)
// ============================================================================

/// Parse a timezone identifier string into an offset in nanoseconds.
/// Handles fixed-offset timezones like "+05:30", "-00:02", "UTC".
pub(super) fn parse_timezone_offset_ns(tz_id: &str) -> i128 {
    if tz_id == "UTC" || tz_id.eq_ignore_ascii_case("utc") {
        return 0;
    }
    // Fixed offset: +HH:MM or -HH:MM
    if (tz_id.starts_with('+') || tz_id.starts_with('-')) && tz_id.len() >= 5 {
        let sign: i128 = if tz_id.starts_with('-') { -1 } else { 1 };
        let s = &tz_id[1..];
        let parts: Vec<&str> = s.split(':').collect();
        if let Some(&hours_str) = parts.first() {
            let hours: i128 = hours_str.parse().unwrap_or(0);
            let minutes: i128 = parts.get(1).and_then(|s| s.parse().ok()).unwrap_or(0);
            let seconds: i128 = parts.get(2).and_then(|s| s.parse().ok()).unwrap_or(0);
            return sign
                * (hours * 3_600_000_000_000 + minutes * 60_000_000_000 + seconds * 1_000_000_000);
        }
    }
    0
}

// ============================================================================
// Reject/strip helpers
// ============================================================================

/// Reject strings with Z UTC designator for PlainMonthDay/PlainDate/etc
pub(super) fn reject_utc_designator_for_plain(s: &str) -> Result<(), VmError> {
    // Strip annotations
    let without_annot = if let Some(bracket_pos) = s.find('[') {
        &s[..bracket_pos]
    } else {
        s
    };

    // Check if the time portion contains Z
    if let Some(time_sep) = find_time_separator(without_annot) {
        let time_part = &without_annot[time_sep + 1..];
        // Z at end of time portion
        if time_part.ends_with('Z') || time_part.ends_with('z') {
            return Err(VmError::range_error(
                "UTC designator Z is not allowed for PlainMonthDay",
            ));
        }
        // Z before offset: e.g., "09:00:00Z+01:00" — unlikely but check
    }

    Ok(())
}

/// Reject fractional hours or minutes in ISO time strings
pub(super) fn reject_fractional_hours_minutes(s: &str) -> Result<(), VmError> {
    // Find time separator
    let without_annot = if let Some(bracket_pos) = s.find('[') {
        &s[..bracket_pos]
    } else {
        s
    };

    if let Some(time_sep) = find_time_separator(without_annot) {
        let time_part = &without_annot[time_sep + 1..];
        // Strip UTC offset from time
        let time_clean = strip_time_offset(time_part);

        // Parse time components looking for decimal point
        // Valid: HH:MM:SS.sss or HHMMSS.sss
        // Invalid: HH.xxx or HH:MM.xxx
        let parts: Vec<&str> = time_clean
            .splitn(2, |c: char| c == '.' || c == ',')
            .collect();
        if parts.len() == 2 {
            // There's a fractional part — check what it's attached to
            let before_dot = parts[0];
            // Count colons to determine what's fractional
            let colon_count = before_dot.chars().filter(|&c| c == ':').count();
            let digit_count = before_dot.chars().filter(|c| c.is_ascii_digit()).count();

            if colon_count == 0 && digit_count == 2 {
                // Only HH before dot: fractional hours
                return Err(VmError::range_error(
                    "Fractional hours are not allowed in time strings",
                ));
            }
            if (colon_count == 1 && digit_count == 4) || (colon_count == 0 && digit_count == 4) {
                // HH:MM or HHMM before dot: fractional minutes
                return Err(VmError::range_error(
                    "Fractional minutes are not allowed in time strings",
                ));
            }
        }
    }

    Ok(())
}

/// Strip UTC offset from time portion
pub(super) fn strip_time_offset(time: &str) -> &str {
    // Strip Z
    if time.ends_with('Z') || time.ends_with('z') {
        return &time[..time.len() - 1];
    }
    // Strip +HH:MM or -HH:MM from end
    let bytes = time.as_bytes();
    let len = bytes.len();
    if len >= 6 {
        let start = len - 6;
        if (bytes[start] == b'+' || bytes[start] == b'-') && bytes[start + 3] == b':' {
            return &time[..start];
        }
    }
    if len >= 5 {
        let start = len - 5;
        if (bytes[start] == b'+' || bytes[start] == b'-')
            && bytes[start + 1..].iter().all(|b| b.is_ascii_digit())
        {
            return &time[..start];
        }
    }
    if len >= 3 {
        let start = len - 3;
        if (bytes[start] == b'+' || bytes[start] == b'-')
            && bytes[start + 1].is_ascii_digit()
            && bytes[start + 2].is_ascii_digit()
        {
            return &time[..start];
        }
    }
    time
}

// ============================================================================
// ISO datetime string parsing
// ============================================================================

/// Parse ISO datetime string into (year, month, day, hour, min, sec, ms, us, ns)
/// Uses temporal_rs for spec-compliant parsing.
pub(super) fn parse_iso_datetime_string(
    s: &str,
) -> Result<(i32, u32, u32, i32, i32, i32, i32, i32, i32), VmError> {
    let pdt = temporal_rs::PlainDateTime::from_utf8(s.as_bytes()).map_err(temporal_err)?;
    Ok((
        pdt.year(),
        pdt.month() as u32,
        pdt.day() as u32,
        pdt.hour() as i32,
        pdt.minute() as i32,
        pdt.second() as i32,
        pdt.millisecond() as i32,
        pdt.microsecond() as i32,
        pdt.nanosecond() as i32,
    ))
}

// ============================================================================
// ToTemporalDate — spec abstract operation
// ============================================================================

/// Convert a JS value to a temporal_rs::PlainDate.
/// Handles: PlainDate object, PlainDateTime object, string, property bag (including Proxy).
/// When `options_val` is provided, overflow is read from it AFTER field reads (per spec ordering).
/// When `None`, uses Constrain by default.
pub(super) fn to_temporal_plain_date(
    ncx: &mut NativeContext<'_>,
    item: &Value,
    options_val: Option<&Value>,
) -> Result<temporal_rs::PlainDate, VmError> {
    if item.is_string() {
        let s = ncx.to_string_value(item)?;
        reject_utc_designator_for_plain(s.as_str())?;
        return temporal_rs::PlainDate::from_utf8(s.as_bytes()).map_err(temporal_err);
    }
    if item.is_undefined()
        || item.is_null()
        || item.is_boolean()
        || item.is_number()
        || item.is_bigint()
    {
        return Err(VmError::type_error(format!(
            "cannot convert {} to a PlainDate",
            item.type_of()
        )));
    }
    if item.as_symbol().is_some() {
        return Err(VmError::type_error(
            "Cannot convert a Symbol value to a string",
        ));
    }
    // Handle objects AND proxies
    if item.as_object().is_some() || item.as_proxy().is_some() {
        // Check temporal branding (only for real objects, not proxies)
        if let Some(obj) = item.as_object() {
            let temporal_type = obj
                .get(&PropertyKey::string(SLOT_TEMPORAL_TYPE))
                .and_then(|v| v.as_string().map(|s| s.as_str().to_string()));
            if temporal_type.as_deref() == Some("PlainDate") {
                return extract_iso_date_from_slots(&obj);
            }
            if temporal_type.as_deref() == Some("PlainDateTime") {
                let pdt = extract_plain_date_time(&obj)?;
                return Ok(temporal_rs::PlainDate::from(pdt));
            }
            if temporal_type.as_deref() == Some("ZonedDateTime") {
                let zdt = extract_zoned_date_time(&obj)?;
                return Ok(zdt.to_plain_date());
            }
        }
        // Property bag: read fields in alphabetical order with INTERLEAVED coercion
        // Per spec: PrepareTemporalFields reads each field and immediately coerces
        // Order: calendar, day, month, monthCode, year (alphabetical)
        let calendar_val = ncx.get_property_of_value(item, &PropertyKey::string("calendar"))?;
        if !calendar_val.is_undefined() {
            resolve_calendar_from_property(ncx, &calendar_val)?;
        }

        // day — read + coerce immediately via ToIntegerWithTruncation
        let day_val = ncx.get_property_of_value(item, &PropertyKey::string("day"))?;
        let d = if !day_val.is_undefined() {
            Some(to_integer_with_truncation(ncx, &day_val)? as i32)
        } else {
            None
        };

        // month — read + coerce immediately (BEFORE monthCode per alphabetical order)
        let month_val = ncx.get_property_of_value(item, &PropertyKey::string("month"))?;
        let m = if !month_val.is_undefined() {
            Some(to_integer_with_truncation(ncx, &month_val)? as i32)
        } else {
            None
        };

        // monthCode — read + coerce immediately
        let month_code_val = ncx.get_property_of_value(item, &PropertyKey::string("monthCode"))?;
        let mc_str = if !month_code_val.is_undefined() {
            // Per spec: ToPrimitive(string) then RequireString
            let coerced = to_primitive_require_string(ncx, &month_code_val)?;
            validate_month_code_syntax(coerced.as_str())?;
            Some(coerced)
        } else {
            None
        };

        // year — read + coerce immediately
        let year_val = ncx.get_property_of_value(item, &PropertyKey::string("year"))?;
        let y = if !year_val.is_undefined() {
            Some(to_integer_with_truncation(ncx, &year_val)? as i32)
        } else {
            None
        };

        // Read overflow AFTER all field reads (per spec observable ordering)
        let ov = if let Some(opts) = options_val {
            parse_overflow_option(ncx, opts)?
        } else {
            Overflow::Constrain
        };

        // Required fields: year, day, and month or monthCode
        let y = y.ok_or_else(|| VmError::type_error("year is required"))?;
        if mc_str.is_none() && m.is_none() {
            return Err(VmError::type_error("month or monthCode is required"));
        }
        let d = d.ok_or_else(|| VmError::type_error("day is required"))?;

        // Resolve month from monthCode (values already coerced above)
        let month = if let Some(ref mc) = mc_str {
            let mc_month = validate_month_code_iso_suitability(mc.as_str())? as i32;
            if let Some(mn) = m {
                if mn != mc_month {
                    return Err(VmError::range_error("month and monthCode must agree"));
                }
            }
            mc_month
        } else {
            m.unwrap() // validated above: mc_str.is_none() && m.is_none() already checked
        };

        // Reject month/day < 1 — per spec these must be positive integers
        if month < 1 || d < 1 {
            return Err(VmError::range_error(format!(
                "month ({}) and day ({}) must be positive",
                month, d
            )));
        }
        let month_u8 = month.min(255) as u8;
        let d_u8 = d.min(255) as u8;
        return temporal_rs::PlainDate::new_with_overflow(
            y,
            month_u8,
            d_u8,
            temporal_rs::Calendar::default(),
            ov,
        )
        .map_err(temporal_err);
    }
    Err(VmError::type_error("Expected an object or string"))
}

/// Convert a JS value to a `temporal_rs::PlainTime`.
/// Accepts PlainTime objects, PlainDateTime objects (extracts time), property bags (including Proxy), and strings.
pub(super) fn to_temporal_plain_time(
    ncx: &mut NativeContext<'_>,
    item: &Value,
) -> Result<temporal_rs::PlainTime, VmError> {
    if item.is_string() {
        let s = ncx.to_string_value(item)?;
        return temporal_rs::PlainTime::from_utf8(s.as_bytes()).map_err(temporal_err);
    }
    // Handle objects AND proxies
    if item.as_object().is_some() || item.as_proxy().is_some() {
        // Check temporal branding (only for real objects, not proxies)
        if let Some(obj) = item.as_object() {
            let tt = obj
                .get(&PropertyKey::string(SLOT_TEMPORAL_TYPE))
                .and_then(|v| v.as_string().map(|s| s.as_str().to_string()));
            if tt.as_deref() == Some("PlainTime") {
                return extract_plain_time(&obj);
            }
            if tt.as_deref() == Some("PlainDateTime") {
                let pdt = extract_plain_date_time(&obj)?;
                return Ok(temporal_rs::PlainTime::from(pdt));
            }
            if tt.as_deref() == Some("ZonedDateTime") {
                let zdt = extract_zoned_date_time(&obj)?;
                return Ok(zdt.to_plain_time());
            }
        }
        // Property bag — read fields in alphabetical order with INTERLEAVED coercion
        // Per spec: PrepareTemporalFields reads each field and immediately calls ToIntegerWithTruncation
        // Alphabetical: hour, microsecond, millisecond, minute, nanosecond, second
        let mut has_any = false;
        let h_val = ncx.get_property_of_value(item, &PropertyKey::string("hour"))?;
        let h = if !h_val.is_undefined() {
            has_any = true;
            to_integer_with_truncation(ncx, &h_val)? as i32
        } else {
            0
        };
        let us_val = ncx.get_property_of_value(item, &PropertyKey::string("microsecond"))?;
        let us = if !us_val.is_undefined() {
            has_any = true;
            to_integer_with_truncation(ncx, &us_val)? as i32
        } else {
            0
        };
        let ms_val = ncx.get_property_of_value(item, &PropertyKey::string("millisecond"))?;
        let ms = if !ms_val.is_undefined() {
            has_any = true;
            to_integer_with_truncation(ncx, &ms_val)? as i32
        } else {
            0
        };
        let mi_val = ncx.get_property_of_value(item, &PropertyKey::string("minute"))?;
        let mi = if !mi_val.is_undefined() {
            has_any = true;
            to_integer_with_truncation(ncx, &mi_val)? as i32
        } else {
            0
        };
        let ns_val = ncx.get_property_of_value(item, &PropertyKey::string("nanosecond"))?;
        let ns = if !ns_val.is_undefined() {
            has_any = true;
            to_integer_with_truncation(ncx, &ns_val)? as i32
        } else {
            0
        };
        let s_val = ncx.get_property_of_value(item, &PropertyKey::string("second"))?;
        let sec = if !s_val.is_undefined() {
            has_any = true;
            to_integer_with_truncation(ncx, &s_val)? as i32
        } else {
            0
        };
        if !has_any {
            return Err(VmError::type_error(
                "property bag must have at least one time property",
            ));
        }
        return temporal_rs::PlainTime::new_with_overflow(
            h as u8,
            mi as u8,
            sec as u8,
            ms as u16,
            us as u16,
            ns as u16,
            Overflow::Constrain,
        )
        .map_err(temporal_err);
    }
    Err(VmError::type_error("Expected a PlainTime object or string"))
}

/// Read optional time fields (hour, minute, second, ...) from a property bag object.
/// Returns `Some(PlainTime)` if any time field is present, `None` if none are present.
pub(super) fn read_time_fields_from_bag(
    ncx: &mut NativeContext<'_>,
    obj: &GcRef<JsObject>,
) -> Result<Option<temporal_rs::PlainTime>, VmError> {
    let h_val = ncx.get_property(obj, &PropertyKey::string("hour"))?;
    let mi_val = ncx.get_property(obj, &PropertyKey::string("minute"))?;
    let s_val = ncx.get_property(obj, &PropertyKey::string("second"))?;
    let ms_val = ncx.get_property(obj, &PropertyKey::string("millisecond"))?;
    let us_val = ncx.get_property(obj, &PropertyKey::string("microsecond"))?;
    let ns_val = ncx.get_property(obj, &PropertyKey::string("nanosecond"))?;

    let has_any = !h_val.is_undefined()
        || !mi_val.is_undefined()
        || !s_val.is_undefined()
        || !ms_val.is_undefined()
        || !us_val.is_undefined()
        || !ns_val.is_undefined();
    if !has_any {
        return Ok(None);
    }

    let h = if !h_val.is_undefined() {
        to_integer_if_integral(ncx, &h_val)? as u8
    } else {
        0
    };
    let mi = if !mi_val.is_undefined() {
        to_integer_if_integral(ncx, &mi_val)? as u8
    } else {
        0
    };
    let sec = if !s_val.is_undefined() {
        to_integer_if_integral(ncx, &s_val)? as u8
    } else {
        0
    };
    let ms = if !ms_val.is_undefined() {
        to_integer_if_integral(ncx, &ms_val)? as u16
    } else {
        0
    };
    let us = if !us_val.is_undefined() {
        to_integer_if_integral(ncx, &us_val)? as u16
    } else {
        0
    };
    let ns = if !ns_val.is_undefined() {
        to_integer_if_integral(ncx, &ns_val)? as u16
    } else {
        0
    };

    let pt = temporal_rs::PlainTime::try_new(h, mi, sec, ms, us, ns).map_err(temporal_err)?;
    Ok(Some(pt))
}

/// Read time fields from a property bag, accepting leap seconds (second:60 → 59).
/// Used by ToRelativeTemporalObject to validate and read all time fields.
pub(super) fn read_time_fields_from_bag_with_leap_second(
    ncx: &mut NativeContext<'_>,
    obj: &GcRef<JsObject>,
) -> Result<Option<temporal_rs::PlainTime>, VmError> {
    let h_val = ncx.get_property(obj, &PropertyKey::string("hour"))?;
    let mi_val = ncx.get_property(obj, &PropertyKey::string("minute"))?;
    let s_val = ncx.get_property(obj, &PropertyKey::string("second"))?;
    let ms_val = ncx.get_property(obj, &PropertyKey::string("millisecond"))?;
    let us_val = ncx.get_property(obj, &PropertyKey::string("microsecond"))?;
    let ns_val = ncx.get_property(obj, &PropertyKey::string("nanosecond"))?;

    let has_any = !h_val.is_undefined()
        || !mi_val.is_undefined()
        || !s_val.is_undefined()
        || !ms_val.is_undefined()
        || !us_val.is_undefined()
        || !ns_val.is_undefined();
    if !has_any {
        return Ok(None);
    }

    let h = if !h_val.is_undefined() {
        to_integer_if_integral(ncx, &h_val)? as i32
    } else {
        0
    };
    let mi = if !mi_val.is_undefined() {
        to_integer_if_integral(ncx, &mi_val)? as i32
    } else {
        0
    };
    let mut sec = if !s_val.is_undefined() {
        to_integer_if_integral(ncx, &s_val)? as i32
    } else {
        0
    };
    let ms = if !ms_val.is_undefined() {
        to_integer_if_integral(ncx, &ms_val)? as u16
    } else {
        0
    };
    let us = if !us_val.is_undefined() {
        to_integer_if_integral(ncx, &us_val)? as u16
    } else {
        0
    };
    let ns = if !ns_val.is_undefined() {
        to_integer_if_integral(ncx, &ns_val)? as u16
    } else {
        0
    };

    // Per spec: accept leap second (60 → clamped to 59)
    if sec == 60 {
        sec = 59;
    }

    let pt = temporal_rs::PlainTime::try_new(h as u8, mi as u8, sec as u8, ms, us, ns)
        .map_err(temporal_err)?;
    Ok(Some(pt))
}

/// Read time fields from a Value (object or Proxy), accepting leap seconds (second:60 → 59).
/// Uses get_property_of_value for Proxy support.
/// Fields are read in ALPHABETICAL order with interleaved coercion via ToIntegerWithTruncation.
pub(super) fn read_time_fields_from_bag_value(
    ncx: &mut NativeContext<'_>,
    val: &Value,
) -> Result<Option<temporal_rs::PlainTime>, VmError> {
    // Alphabetical: hour, microsecond, millisecond, minute, nanosecond, second
    let mut has_any = false;
    let h_val = ncx.get_property_of_value(val, &PropertyKey::string("hour"))?;
    let h = if !h_val.is_undefined() {
        has_any = true;
        to_integer_with_truncation(ncx, &h_val)? as i32
    } else {
        0
    };
    let us_val = ncx.get_property_of_value(val, &PropertyKey::string("microsecond"))?;
    let us = if !us_val.is_undefined() {
        has_any = true;
        to_integer_with_truncation(ncx, &us_val)? as u16
    } else {
        0
    };
    let ms_val = ncx.get_property_of_value(val, &PropertyKey::string("millisecond"))?;
    let ms = if !ms_val.is_undefined() {
        has_any = true;
        to_integer_with_truncation(ncx, &ms_val)? as u16
    } else {
        0
    };
    let mi_val = ncx.get_property_of_value(val, &PropertyKey::string("minute"))?;
    let mi = if !mi_val.is_undefined() {
        has_any = true;
        to_integer_with_truncation(ncx, &mi_val)? as i32
    } else {
        0
    };
    let ns_val = ncx.get_property_of_value(val, &PropertyKey::string("nanosecond"))?;
    let ns = if !ns_val.is_undefined() {
        has_any = true;
        to_integer_with_truncation(ncx, &ns_val)? as u16
    } else {
        0
    };
    let s_val = ncx.get_property_of_value(val, &PropertyKey::string("second"))?;
    let mut sec = if !s_val.is_undefined() {
        has_any = true;
        to_integer_with_truncation(ncx, &s_val)? as i32
    } else {
        0
    };

    if !has_any {
        return Ok(None);
    }

    // Per spec: accept leap second (60 → clamped to 59)
    if sec == 60 {
        sec = 59;
    }

    let pt = temporal_rs::PlainTime::try_new(h as u8, mi as u8, sec as u8, ms, us, ns)
        .map_err(temporal_err)?;
    Ok(Some(pt))
}

/// Validate a UTC offset string using temporal_rs::UtcOffset::from_utf8().
/// Accepts full precision ±HH:MM:SS.fffffffff (not just ±HH:MM like TimeZone).
pub(super) fn validate_utc_offset_string(s: &str) -> Result<(), VmError> {
    temporal_rs::UtcOffset::from_utf8(s.as_bytes())
        .map(|_| ())
        .map_err(|_| VmError::range_error(format!("{} is not a valid offset string", s)))
}

// ============================================================================
// Construct JS Temporal values from temporal_rs types
// ============================================================================

/// Create a JS Temporal.PlainTime value by calling the constructor.
pub(super) fn construct_plain_time_value(
    ncx: &mut NativeContext<'_>,
    pt: &temporal_rs::PlainTime,
) -> Result<Value, VmError> {
    let temporal_ns = ncx
        .ctx
        .get_global("Temporal")
        .ok_or_else(|| VmError::type_error("Temporal namespace not found"))?;
    let temporal_obj = temporal_ns
        .as_object()
        .ok_or_else(|| VmError::type_error("Temporal namespace not found"))?;
    let ctor = temporal_obj
        .get(&PropertyKey::string("PlainTime"))
        .ok_or_else(|| VmError::type_error("PlainTime constructor not found"))?;
    ncx.call_function_construct(
        &ctor,
        Value::undefined(),
        &[
            Value::int32(pt.hour() as i32),
            Value::int32(pt.minute() as i32),
            Value::int32(pt.second() as i32),
            Value::int32(pt.millisecond() as i32),
            Value::int32(pt.microsecond() as i32),
            Value::int32(pt.nanosecond() as i32),
        ],
    )
}

/// Create a JS Temporal.PlainDate value by calling the constructor.
pub(super) fn construct_plain_date_value(
    ncx: &mut NativeContext<'_>,
    year: i32,
    month: i32,
    day: i32,
) -> Result<Value, VmError> {
    let temporal_ns = ncx
        .ctx
        .get_global("Temporal")
        .ok_or_else(|| VmError::type_error("Temporal namespace not found"))?;
    let temporal_obj = temporal_ns
        .as_object()
        .ok_or_else(|| VmError::type_error("Temporal namespace not found"))?;
    let ctor = temporal_obj
        .get(&PropertyKey::string("PlainDate"))
        .ok_or_else(|| VmError::type_error("PlainDate constructor not found"))?;
    ncx.call_function_construct(
        &ctor,
        Value::undefined(),
        &[Value::int32(year), Value::int32(month), Value::int32(day)],
    )
}

/// Create a JS Temporal.PlainDateTime value by calling the constructor.
pub(super) fn construct_plain_date_time_value(
    ncx: &mut NativeContext<'_>,
    pdt: &temporal_rs::PlainDateTime,
) -> Result<Value, VmError> {
    let temporal_ns = ncx
        .ctx
        .get_global("Temporal")
        .ok_or_else(|| VmError::type_error("Temporal namespace not found"))?;
    let temporal_obj = temporal_ns
        .as_object()
        .ok_or_else(|| VmError::type_error("Temporal namespace not found"))?;
    let ctor = temporal_obj
        .get(&PropertyKey::string("PlainDateTime"))
        .ok_or_else(|| VmError::type_error("PlainDateTime constructor not found"))?;
    ncx.call_function_construct(
        &ctor,
        Value::undefined(),
        &[
            Value::int32(pdt.year()),
            Value::int32(pdt.month() as i32),
            Value::int32(pdt.day() as i32),
            Value::int32(pdt.hour() as i32),
            Value::int32(pdt.minute() as i32),
            Value::int32(pdt.second() as i32),
            Value::int32(pdt.millisecond() as i32),
            Value::int32(pdt.microsecond() as i32),
            Value::int32(pdt.nanosecond() as i32),
        ],
    )
}

/// Create a JS Temporal.Duration value from a temporal_rs Duration.
pub(super) fn construct_duration_value(
    ncx: &mut NativeContext<'_>,
    dur: &temporal_rs::Duration,
) -> Result<Value, VmError> {
    let temporal_ns = ncx
        .ctx
        .get_global("Temporal")
        .ok_or_else(|| VmError::type_error("Temporal namespace not found"))?;
    let temporal_obj = temporal_ns
        .as_object()
        .ok_or_else(|| VmError::type_error("Temporal namespace not found"))?;
    let ctor = temporal_obj
        .get(&PropertyKey::string("Duration"))
        .ok_or_else(|| VmError::type_error("Duration constructor not found"))?;
    ncx.call_function_construct(
        &ctor,
        Value::undefined(),
        &[
            Value::number(dur.years() as f64),
            Value::number(dur.months() as f64),
            Value::number(dur.weeks() as f64),
            Value::number(dur.days() as f64),
            Value::number(dur.hours() as f64),
            Value::number(dur.minutes() as f64),
            Value::number(dur.seconds() as f64),
            Value::number(dur.milliseconds() as f64),
            Value::number(dur.microseconds() as f64),
            Value::number(dur.nanoseconds() as f64),
        ],
    )
}

/// Create a JS Temporal.ZonedDateTime value from a temporal_rs ZonedDateTime.
/// Constructs via `new Temporal.ZonedDateTime(epochNs, timeZoneId, calendarId)`.
pub(super) fn construct_zoned_date_time_value(
    ncx: &mut NativeContext<'_>,
    zdt: &temporal_rs::ZonedDateTime,
) -> Result<Value, VmError> {
    let temporal_ns = ncx
        .ctx
        .get_global("Temporal")
        .ok_or_else(|| VmError::type_error("Temporal namespace not found"))?;
    let temporal_obj = temporal_ns
        .as_object()
        .ok_or_else(|| VmError::type_error("Temporal namespace not found"))?;
    let ctor = temporal_obj
        .get(&PropertyKey::string("ZonedDateTime"))
        .ok_or_else(|| VmError::type_error("ZonedDateTime constructor not found"))?;

    let epoch_ns_str = zdt.epoch_nanoseconds().0.to_string();
    let tz_id = zdt
        .time_zone()
        .identifier_with_provider(tz_provider())
        .unwrap_or_else(|_| "UTC".to_string());
    let cal_id = zdt.calendar().identifier().to_string();

    ncx.call_function_construct(
        &ctor,
        Value::undefined(),
        &[
            Value::bigint(epoch_ns_str),
            Value::string(JsString::intern(&tz_id)),
            Value::string(JsString::intern(&cal_id)),
        ],
    )
}

/// Create a JS Temporal.PlainYearMonth value by calling the constructor.
pub(super) fn construct_plain_year_month_value(
    ncx: &mut NativeContext<'_>,
    year: i32,
    month: i32,
) -> Result<Value, VmError> {
    let temporal_ns = ncx
        .ctx
        .get_global("Temporal")
        .ok_or_else(|| VmError::type_error("Temporal namespace not found"))?;
    let temporal_obj = temporal_ns
        .as_object()
        .ok_or_else(|| VmError::type_error("Temporal namespace not found"))?;
    let ctor = temporal_obj
        .get(&PropertyKey::string("PlainYearMonth"))
        .ok_or_else(|| VmError::type_error("PlainYearMonth constructor not found"))?;
    ncx.call_function_construct(
        &ctor,
        Value::undefined(),
        &[Value::int32(year), Value::int32(month)],
    )
}

/// Create a JS Temporal.PlainMonthDay value by calling the constructor.
pub(super) fn construct_plain_month_day_value(
    ncx: &mut NativeContext<'_>,
    month: i32,
    day: i32,
) -> Result<Value, VmError> {
    let temporal_ns = ncx
        .ctx
        .get_global("Temporal")
        .ok_or_else(|| VmError::type_error("Temporal namespace not found"))?;
    let temporal_obj = temporal_ns
        .as_object()
        .ok_or_else(|| VmError::type_error("Temporal namespace not found"))?;
    let ctor = temporal_obj
        .get(&PropertyKey::string("PlainMonthDay"))
        .ok_or_else(|| VmError::type_error("PlainMonthDay constructor not found"))?;
    ncx.call_function_construct(
        &ctor,
        Value::undefined(),
        &[Value::int32(month), Value::int32(day)],
    )
}

// ============================================================================
// ToTemporalDuration — convert JS value to temporal_rs::Duration
// ============================================================================

/// Convert a JS value (string or Duration object) to temporal_rs::Duration.
pub(super) fn to_temporal_duration(
    ncx: &mut NativeContext<'_>,
    item: &Value,
) -> Result<temporal_rs::Duration, VmError> {
    if item.is_string() {
        let s = ncx.to_string_value(item)?;
        return temporal_rs::Duration::from_utf8(s.as_bytes()).map_err(temporal_err);
    }
    if item.as_object().is_some() || item.as_proxy().is_some() {
        // Check if it's a real Duration object via TemporalValue
        if let Some(obj) = item.as_object() {
            if let Ok(dur) = extract_duration(&obj) {
                return Ok(dur);
            }
        }

        // Helper for property access (supports both object and proxy)
        let get_field = |ncx: &mut NativeContext<'_>, name: &str| -> Result<Value, VmError> {
            if let Some(proxy) = item.as_proxy() {
                proxy_get_property(ncx, proxy, item, name)
            } else if let Some(obj) = item.as_object() {
                ncx.get_property(&obj, &PropertyKey::string(name))
            } else {
                Ok(Value::undefined())
            }
        };

        // Property bag: read fields in alphabetical order (spec)
        let field_names = [
            "days",
            "hours",
            "microseconds",
            "milliseconds",
            "minutes",
            "months",
            "nanoseconds",
            "seconds",
            "weeks",
            "years",
        ];
        let mut field_map = std::collections::HashMap::new();
        for &f in &field_names {
            let v = get_field(ncx, f)?;
            if !v.is_undefined() {
                let n = ncx.to_number_value(&v)?;
                if n.is_infinite() {
                    return Err(VmError::range_error(format!("{} cannot be Infinity", f)));
                }
                if n.is_nan() {
                    return Err(VmError::range_error(format!("{} cannot be NaN", f)));
                }
                if n != n.trunc() {
                    return Err(VmError::range_error(format!("{} must be an integer", f)));
                }
                field_map.insert(f, n);
            }
        }
        if field_map.is_empty() {
            return Err(VmError::type_error(
                "duration object must have at least one temporal property",
            ));
        }
        let g = |f: &str| *field_map.get(f).unwrap_or(&0.0);
        return temporal_rs::Duration::new(
            g("years") as i64,
            g("months") as i64,
            g("weeks") as i64,
            g("days") as i64,
            g("hours") as i64,
            g("minutes") as i64,
            g("seconds") as i64,
            g("milliseconds") as i64,
            g("microseconds") as i128,
            g("nanoseconds") as i128,
        )
        .map_err(temporal_err);
    }
    Err(VmError::type_error("Expected a Duration object or string"))
}

// ============================================================================
// DifferenceSettings parsing for date types
// ============================================================================

/// Parse since/until options into temporal_rs::options::DifferenceSettings.
pub(super) fn parse_difference_settings_for_date(
    ncx: &mut NativeContext<'_>,
    options_val: &Value,
) -> Result<temporal_rs::options::DifferenceSettings, VmError> {
    let mut settings = temporal_rs::options::DifferenceSettings::default();
    if options_val.is_undefined() {
        return Ok(settings);
    }
    get_options_object(options_val)?;

    // Per spec: Read ALL options in alphabetical order using get_option_value (handles Proxy).
    // 1. largestUnit
    let lu = get_option_value(ncx, options_val, "largestUnit")?;
    if !lu.is_undefined() {
        let lu_str = ncx.to_string_value(&lu)?;
        settings.largest_unit = Some(parse_temporal_unit(lu_str.as_str())?);
    }
    // 2. roundingIncrement
    let ri = get_option_value(ncx, options_val, "roundingIncrement")?;
    if !ri.is_undefined() {
        let n = ncx.to_number_value(&ri)?;
        if n.is_nan() || n.is_infinite() || n <= 0.0 {
            return Err(VmError::range_error(
                "roundingIncrement must be a positive integer",
            ));
        }
        let truncated = n.trunc() as u32;
        settings.increment = Some(
            temporal_rs::options::RoundingIncrement::try_new(truncated)
                .map_err(|_| VmError::range_error("roundingIncrement out of range"))?,
        );
    }
    // 3. roundingMode
    let rm = get_option_value(ncx, options_val, "roundingMode")?;
    if !rm.is_undefined() {
        let rm_str = ncx.to_string_value(&rm)?;
        settings.rounding_mode = Some(parse_rounding_mode(rm_str.as_str())?);
    }
    // 4. smallestUnit
    let su = get_option_value(ncx, options_val, "smallestUnit")?;
    if !su.is_undefined() {
        let su_str = ncx.to_string_value(&su)?;
        settings.smallest_unit = Some(parse_temporal_unit(su_str.as_str())?);
    }
    Ok(settings)
}

/// Parse difference settings for PlainYearMonth.until / .since
/// The `_is_since` parameter is reserved for future use; temporal_rs handles sign internally.
pub(super) fn parse_difference_settings_for_year_month(
    ncx: &mut NativeContext<'_>,
    options_val: &Value,
    _is_since: bool,
) -> Result<temporal_rs::options::DifferenceSettings, VmError> {
    let mut settings = temporal_rs::options::DifferenceSettings::default();
    if options_val.is_undefined() {
        return Ok(settings);
    }
    // Type validation: primitives are not valid options
    if options_val.is_null()
        || options_val.is_boolean()
        || options_val.is_number()
        || options_val.is_bigint()
        || options_val.is_string()
        || options_val.as_symbol().is_some()
    {
        return Err(VmError::type_error(format!(
            "{} is not a valid options argument",
            options_val.type_of()
        )));
    }
    // Read and coerce each option one at a time in spec order (interleaved for observable side effects)
    // largestUnit
    let lu = ncx.get_property_of_value(options_val, &PropertyKey::string("largestUnit"))?;
    if !lu.is_undefined() {
        let lu_str = ncx.to_string_value(&lu)?;
        settings.largest_unit = Some(parse_temporal_unit(lu_str.as_str())?);
    }
    // roundingIncrement
    let ri = ncx.get_property_of_value(options_val, &PropertyKey::string("roundingIncrement"))?;
    if !ri.is_undefined() {
        let n = ncx.to_number_value(&ri)?;
        if n.is_nan() || n.is_infinite() || n <= 0.0 {
            return Err(VmError::range_error(
                "roundingIncrement must be a positive integer",
            ));
        }
        let truncated = n.trunc() as u32;
        settings.increment = Some(
            temporal_rs::options::RoundingIncrement::try_new(truncated)
                .map_err(|_| VmError::range_error("roundingIncrement out of range"))?,
        );
    }
    // roundingMode
    let rm = ncx.get_property_of_value(options_val, &PropertyKey::string("roundingMode"))?;
    if !rm.is_undefined() {
        let rm_str = ncx.to_string_value(&rm)?;
        settings.rounding_mode = Some(parse_rounding_mode(rm_str.as_str())?);
    }
    // smallestUnit
    let su = ncx.get_property_of_value(options_val, &PropertyKey::string("smallestUnit"))?;
    if !su.is_undefined() {
        let su_str = ncx.to_string_value(&su)?;
        settings.smallest_unit = Some(parse_temporal_unit(su_str.as_str())?);
    }
    Ok(settings)
}

/// Parse since/until options for PlainTime into temporal_rs DifferenceSettings.
/// For PlainTime, defaults are: largestUnit = "hour", smallestUnit = "nanosecond".
pub(super) fn parse_difference_settings_for_time(
    ncx: &mut NativeContext<'_>,
    options_val: &Value,
) -> Result<temporal_rs::options::DifferenceSettings, VmError> {
    let mut settings = temporal_rs::options::DifferenceSettings::default();
    if options_val.is_undefined() {
        return Ok(settings);
    }
    get_options_object(options_val)?;

    // Per spec: Read ALL options in alphabetical order BEFORE validation.
    // Use get_option_value to handle both objects and Proxy values.

    // 1. largestUnit (read + coerce to string)
    let lu = get_option_value(ncx, options_val, "largestUnit")?;
    let lu_parsed = if !lu.is_undefined() {
        let lu_str = ncx.to_string_value(&lu)?;
        Some(parse_temporal_unit(lu_str.as_str())?)
    } else {
        None
    };

    // 2. roundingIncrement (read + coerce to number)
    let ri = get_option_value(ncx, options_val, "roundingIncrement")?;
    let ri_parsed = if !ri.is_undefined() {
        let n = ncx.to_number_value(&ri)?;
        if n.is_nan() || n.is_infinite() || n <= 0.0 {
            return Err(VmError::range_error(
                "roundingIncrement must be a positive integer",
            ));
        }
        Some(
            temporal_rs::options::RoundingIncrement::try_new(n.trunc() as u32)
                .map_err(|_| VmError::range_error("roundingIncrement out of range"))?,
        )
    } else {
        None
    };

    // 3. roundingMode (read + coerce to string)
    let rm = get_option_value(ncx, options_val, "roundingMode")?;
    let rm_parsed = if !rm.is_undefined() {
        let rm_str = ncx.to_string_value(&rm)?;
        Some(parse_rounding_mode(rm_str.as_str())?)
    } else {
        None
    };

    // 4. smallestUnit (read + coerce to string)
    let su = get_option_value(ncx, options_val, "smallestUnit")?;
    let su_parsed = if !su.is_undefined() {
        let su_str = ncx.to_string_value(&su)?;
        Some(parse_temporal_unit(su_str.as_str())?)
    } else {
        None
    };

    // NOW validate after all properties have been read
    if let Some(unit) = lu_parsed {
        if !unit.is_time_unit() && unit != temporal_rs::options::Unit::Auto {
            return Err(VmError::range_error(
                "largestUnit is not a valid unit for PlainTime difference",
            ));
        }
        settings.largest_unit = Some(unit);
    }
    if let Some(unit) = su_parsed {
        if !unit.is_time_unit() && unit != temporal_rs::options::Unit::Auto {
            return Err(VmError::range_error(
                "smallestUnit is not a valid unit for PlainTime difference",
            ));
        }
        settings.smallest_unit = Some(unit);
    }
    settings.rounding_mode = rm_parsed;
    settings.increment = ri_parsed;
    Ok(settings)
}

/// Parse rounding options for PlainTime.round().
/// Accepts a string (smallestUnit shorthand) or an options object.
pub(super) fn parse_rounding_options_for_time(
    ncx: &mut NativeContext<'_>,
    options_val: &Value,
) -> Result<temporal_rs::options::RoundingOptions, VmError> {
    // Per spec: round() with no arguments / undefined → TypeError
    if options_val.is_undefined() {
        return Err(VmError::type_error("round() requires an options argument"));
    }
    // Per spec: null, boolean, number, bigint, symbol → TypeError
    if options_val.is_null()
        || options_val.is_boolean()
        || options_val.is_number()
        || options_val.is_bigint()
        || options_val.as_symbol().is_some()
    {
        return Err(VmError::type_error(format!(
            "options must be a string or object, got {}",
            options_val.type_of()
        )));
    }
    if let Some(s) = options_val.as_string() {
        let unit = parse_temporal_unit(s.as_str())?;
        if !unit.is_time_unit() {
            return Err(VmError::range_error(format!(
                "{} is not a valid unit for PlainTime rounding",
                s.as_str()
            )));
        }
        let mut opts = temporal_rs::options::RoundingOptions::default();
        opts.smallest_unit = Some(unit);
        return Ok(opts);
    }
    // Object or Proxy — read all options in alphabetical order via get_option_value
    let mut opts = temporal_rs::options::RoundingOptions::default();

    // 1. roundingIncrement
    let ri = get_option_value(ncx, options_val, "roundingIncrement")?;
    if !ri.is_undefined() {
        let n = ncx.to_number_value(&ri)?;
        if n.is_nan() || n.is_infinite() || n <= 0.0 {
            return Err(VmError::range_error(
                "roundingIncrement must be a positive integer",
            ));
        }
        let truncated = n.trunc() as u32;
        opts.increment = Some(
            temporal_rs::options::RoundingIncrement::try_new(truncated)
                .map_err(|_| VmError::range_error("roundingIncrement out of range"))?,
        );
    }
    // 2. roundingMode
    let rm = get_option_value(ncx, options_val, "roundingMode")?;
    if !rm.is_undefined() {
        let rm_str = ncx.to_string_value(&rm)?;
        opts.rounding_mode = Some(parse_rounding_mode(rm_str.as_str())?);
    }
    // 3. smallestUnit
    let su = get_option_value(ncx, options_val, "smallestUnit")?;
    if !su.is_undefined() {
        let su_str = ncx.to_string_value(&su)?;
        let unit = parse_temporal_unit(su_str.as_str())?;
        if !unit.is_time_unit() {
            return Err(VmError::range_error(format!(
                "{} is not a valid unit for PlainTime rounding",
                su_str.as_str()
            )));
        }
        opts.smallest_unit = Some(unit);
    }
    Ok(opts)
}

pub(super) fn parse_temporal_unit(s: &str) -> Result<temporal_rs::options::Unit, VmError> {
    match s {
        "auto" => Ok(temporal_rs::options::Unit::Auto),
        "year" | "years" => Ok(temporal_rs::options::Unit::Year),
        "month" | "months" => Ok(temporal_rs::options::Unit::Month),
        "week" | "weeks" => Ok(temporal_rs::options::Unit::Week),
        "day" | "days" => Ok(temporal_rs::options::Unit::Day),
        "hour" | "hours" => Ok(temporal_rs::options::Unit::Hour),
        "minute" | "minutes" => Ok(temporal_rs::options::Unit::Minute),
        "second" | "seconds" => Ok(temporal_rs::options::Unit::Second),
        "millisecond" | "milliseconds" => Ok(temporal_rs::options::Unit::Millisecond),
        "microsecond" | "microseconds" => Ok(temporal_rs::options::Unit::Microsecond),
        "nanosecond" | "nanoseconds" => Ok(temporal_rs::options::Unit::Nanosecond),
        _ => Err(VmError::range_error(format!("{} is not a valid unit", s))),
    }
}

fn parse_rounding_mode(s: &str) -> Result<temporal_rs::options::RoundingMode, VmError> {
    match s {
        "ceil" => Ok(temporal_rs::options::RoundingMode::Ceil),
        "floor" => Ok(temporal_rs::options::RoundingMode::Floor),
        "expand" => Ok(temporal_rs::options::RoundingMode::Expand),
        "trunc" => Ok(temporal_rs::options::RoundingMode::Trunc),
        "halfCeil" => Ok(temporal_rs::options::RoundingMode::HalfCeil),
        "halfFloor" => Ok(temporal_rs::options::RoundingMode::HalfFloor),
        "halfExpand" => Ok(temporal_rs::options::RoundingMode::HalfExpand),
        "halfTrunc" => Ok(temporal_rs::options::RoundingMode::HalfTrunc),
        "halfEven" => Ok(temporal_rs::options::RoundingMode::HalfEven),
        _ => Err(VmError::range_error(format!(
            "{} is not a valid rounding mode",
            s
        ))),
    }
}

/// Format an ISO year for Temporal toString output.
/// Years 0000-9999 use 4 digits, others use +/- 6-digit format.
pub(super) fn format_iso_year(year: i32) -> String {
    if (0..=9999).contains(&year) {
        format!("{:04}", year)
    } else if year < 0 {
        format!("-{:06}", -year)
    } else {
        format!("+{:06}", year)
    }
}

/// Validate that `options_val` is either undefined or an object (not null, boolean, number, string, symbol).
pub(super) fn get_options_object(val: &Value) -> Result<(), VmError> {
    if val.is_undefined() {
        return Ok(());
    }
    if val.is_null()
        || val.is_boolean()
        || val.is_number()
        || val.is_string()
        || val.is_symbol()
        || val.is_bigint()
    {
        return Err(VmError::type_error(
            "Options must be an object or undefined",
        ));
    }
    Ok(())
}

/// Get a named property from an options value (handles undefined gracefully).
pub(super) fn get_option_value(
    ncx: &mut NativeContext<'_>,
    options_val: &Value,
    name: &str,
) -> Result<Value, VmError> {
    if options_val.is_undefined() {
        return Ok(Value::undefined());
    }
    ncx.get_property_of_value(options_val, &PropertyKey::string(name))
}

/// Parse a JS options argument into `ToStringRoundingOptions`.
/// Handles fractionalSecondDigits, roundingMode, smallestUnit.
pub(super) fn parse_to_string_rounding_options(
    ncx: &mut NativeContext<'_>,
    options_val: &Value,
) -> Result<temporal_rs::options::ToStringRoundingOptions, VmError> {
    if options_val.is_undefined() {
        return Ok(temporal_rs::options::ToStringRoundingOptions::default());
    }

    get_options_object(options_val)?;

    // fractionalSecondDigits
    let fsd_val = get_option_value(ncx, options_val, "fractionalSecondDigits")?;
    let precision = if fsd_val.is_undefined() {
        temporal_rs::parsers::Precision::Auto
    } else if fsd_val.is_number() {
        let n = fsd_val.as_number().unwrap();
        if n.is_nan() {
            return Err(VmError::range_error(
                "fractionalSecondDigits must not be NaN",
            ));
        }
        let d = n.floor();
        if d < 0.0 || d > 9.0 || !d.is_finite() {
            return Err(VmError::range_error(
                "fractionalSecondDigits must be 0-9 or 'auto'",
            ));
        }
        temporal_rs::parsers::Precision::Digit(d as u8)
    } else {
        let s = ncx.to_string_value(&fsd_val)?;
        if s.as_str() == "auto" {
            temporal_rs::parsers::Precision::Auto
        } else {
            return Err(VmError::range_error(format!(
                "Invalid fractionalSecondDigits: {}",
                s
            )));
        }
    };

    // roundingMode
    let rm_val = get_option_value(ncx, options_val, "roundingMode")?;
    let rounding_mode = if rm_val.is_undefined() {
        None
    } else {
        let s = ncx.to_string_value(&rm_val)?;
        Some(
            s.as_str()
                .parse::<temporal_rs::options::RoundingMode>()
                .map_err(|_| VmError::range_error(format!("Invalid roundingMode: {}", s)))?,
        )
    };

    // smallestUnit
    let su_val = get_option_value(ncx, options_val, "smallestUnit")?;
    let smallest_unit = if su_val.is_undefined() {
        None
    } else {
        let s = ncx.to_string_value(&su_val)?;
        Some(parse_temporal_unit(s.as_str())?)
    };

    Ok(temporal_rs::options::ToStringRoundingOptions {
        precision,
        smallest_unit,
        rounding_mode,
    })
}

/// Parse a `relativeTo` option from a JS options object.
/// Returns `Some(RelativeTo::PlainDate(...))` or `Some(RelativeTo::ZonedDateTime(...))` if present,
/// or `None` if the option is undefined/absent.
pub(super) fn parse_relative_to(
    ncx: &mut NativeContext<'_>,
    options_val: &Value,
) -> Result<Option<temporal_rs::options::RelativeTo>, VmError> {
    let rt_val = get_option_value(ncx, options_val, "relativeTo")?;
    parse_relative_to_value(ncx, &rt_val)
}

/// Parse a relativeTo value that was already extracted from options.
/// This avoids double-reading the `relativeTo` property when it was already
/// observed via `get_property_of_value`.
pub(super) fn parse_relative_to_value(
    ncx: &mut NativeContext<'_>,
    rt_val: &Value,
) -> Result<Option<temporal_rs::options::RelativeTo>, VmError> {
    if rt_val.is_undefined() {
        return Ok(None);
    }

    // String → delegate to temporal_rs::RelativeTo::try_from_str() which handles:
    // - PlainDate strings (no timezone annotation)
    // - ZonedDateTime strings (with timezone annotation)
    // - UTC designator distinction
    // - Offset validation, match-minutes behavior, etc.
    if rt_val.is_string() {
        let s = ncx.to_string_value(&rt_val)?;
        let rt =
            temporal_rs::options::RelativeTo::try_from_str(s.as_str()).map_err(temporal_err)?;
        return Ok(Some(rt));
    }

    // Object or Proxy — check temporal type
    if rt_val.as_object().is_some() || rt_val.as_proxy().is_some() {
        // Check temporal branding (only for real objects, not proxies)
        if let Some(obj) = rt_val.as_object() {
            let tt = obj
                .get(&PropertyKey::string(SLOT_TEMPORAL_TYPE))
                .and_then(|v| v.as_string().map(|s| s.as_str().to_string()));
            match tt.as_deref() {
                Some("PlainDate") => {
                    let pd = extract_plain_date(&obj)?;
                    return Ok(Some(temporal_rs::options::RelativeTo::PlainDate(pd)));
                }
                Some("ZonedDateTime") => {
                    let zdt = extract_zoned_date_time(&obj)?;
                    return Ok(Some(temporal_rs::options::RelativeTo::ZonedDateTime(zdt)));
                }
                Some("PlainDateTime") => {
                    let pdt = extract_plain_date_time(&obj)?;
                    let pd = temporal_rs::PlainDate::from(pdt);
                    return Ok(Some(temporal_rs::options::RelativeTo::PlainDate(pd)));
                }
                _ => {}
            }
        }

        // Property bag (plain object or Proxy) — per spec (ToRelativeTemporalObject),
        // read ALL fields in ALPHABETICAL order with interleaved coercion.
        // Order: calendar, day, era, eraYear, hour, microsecond, millisecond, minute, month, monthCode,
        //        nanosecond, offset, second, timeZone, year

        // calendar
        let calendar_val = ncx.get_property_of_value(rt_val, &PropertyKey::string("calendar"))?;
        let has_non_iso_calendar = if !calendar_val.is_undefined() {
            // Check if the calendar is non-ISO before the validation call
            let cal_is_non_iso = if calendar_val.is_string() {
                let s = calendar_val
                    .as_string()
                    .map(|s| s.as_str().to_ascii_lowercase());
                s.as_deref() != Some("iso8601")
            } else {
                false
            };
            resolve_calendar_from_property(ncx, &calendar_val)?;
            cal_is_non_iso
        } else {
            false
        };

        // day — read + coerce
        let day_val = ncx.get_property_of_value(rt_val, &PropertyKey::string("day"))?;
        let d = if !day_val.is_undefined() {
            Some(to_integer_with_truncation(ncx, &day_val)? as i32)
        } else {
            None
        };

        // era, eraYear — only read for non-ISO calendars (per spec: PrepareTemporalFields
        // only includes era/eraYear when the calendar's fieldDescriptors declares them)
        if has_non_iso_calendar {
            // era — read + RequireString (per spec: non-string non-undefined → TypeError)
            let era_val = ncx.get_property_of_value(rt_val, &PropertyKey::string("era"))?;
            if !era_val.is_undefined() {
                if !era_val.is_string() {
                    return Err(VmError::type_error("era must be a string"));
                }
                let _era = ncx.to_string_value(&era_val)?;
            }

            // eraYear — read + ToIntegerWithTruncation (Infinity → RangeError)
            let era_year_val =
                ncx.get_property_of_value(rt_val, &PropertyKey::string("eraYear"))?;
            if !era_year_val.is_undefined() {
                let _era_year = to_integer_with_truncation(ncx, &era_year_val)? as i32;
            }
        }

        // hour — read + coerce
        let h_val = ncx.get_property_of_value(rt_val, &PropertyKey::string("hour"))?;
        let h = if !h_val.is_undefined() {
            to_integer_with_truncation(ncx, &h_val)? as i32
        } else {
            0
        };

        // microsecond — read + coerce
        let us_val = ncx.get_property_of_value(rt_val, &PropertyKey::string("microsecond"))?;
        let us = if !us_val.is_undefined() {
            to_integer_with_truncation(ncx, &us_val)? as u16
        } else {
            0
        };

        // millisecond — read + coerce
        let ms_val = ncx.get_property_of_value(rt_val, &PropertyKey::string("millisecond"))?;
        let ms = if !ms_val.is_undefined() {
            to_integer_with_truncation(ncx, &ms_val)? as u16
        } else {
            0
        };

        // minute — read + coerce
        let mi_val = ncx.get_property_of_value(rt_val, &PropertyKey::string("minute"))?;
        let mi = if !mi_val.is_undefined() {
            to_integer_with_truncation(ncx, &mi_val)? as i32
        } else {
            0
        };

        // month — read + coerce immediately (before monthCode)
        let month_val = ncx.get_property_of_value(rt_val, &PropertyKey::string("month"))?;
        let m = if !month_val.is_undefined() {
            Some(to_integer_with_truncation(ncx, &month_val)? as i32)
        } else {
            None
        };

        // monthCode — read + coerce
        let month_code_val =
            ncx.get_property_of_value(rt_val, &PropertyKey::string("monthCode"))?;
        let mc_str = if !month_code_val.is_undefined() {
            let coerced = to_primitive_require_string(ncx, &month_code_val)?;
            validate_month_code_syntax(coerced.as_str())?;
            Some(coerced)
        } else {
            None
        };

        // nanosecond — read + coerce
        let ns_val = ncx.get_property_of_value(rt_val, &PropertyKey::string("nanosecond"))?;
        let ns = if !ns_val.is_undefined() {
            to_integer_with_truncation(ncx, &ns_val)? as u16
        } else {
            0
        };

        // offset — read + RequireString (objects → ToPrimitive string, non-string primitives → TypeError)
        let offset_val = ncx.get_property_of_value(rt_val, &PropertyKey::string("offset"))?;
        let offset_str = if !offset_val.is_undefined() {
            Some(require_string_for_field(ncx, &offset_val, "offset")?)
        } else {
            None
        };

        // second — read + coerce
        let s_val = ncx.get_property_of_value(rt_val, &PropertyKey::string("second"))?;
        let mut sec = if !s_val.is_undefined() {
            to_integer_with_truncation(ncx, &s_val)? as i32
        } else {
            0
        };

        // timeZone — read
        let tz_val = ncx.get_property_of_value(rt_val, &PropertyKey::string("timeZone"))?;

        // year — read + coerce
        let year_val = ncx.get_property_of_value(rt_val, &PropertyKey::string("year"))?;
        let y = if !year_val.is_undefined() {
            Some(to_integer_with_truncation(ncx, &year_val)? as i32)
        } else {
            None
        };

        // Leap second handling
        if sec == 60 {
            sec = 59;
        }

        // Build time (if any time fields present)
        let has_time = !h_val.is_undefined()
            || !us_val.is_undefined()
            || !ms_val.is_undefined()
            || !mi_val.is_undefined()
            || !ns_val.is_undefined()
            || !s_val.is_undefined();
        let time = if has_time {
            Some(
                temporal_rs::PlainTime::try_new(h as u8, mi as u8, sec as u8, ms, us, ns)
                    .map_err(temporal_err)?,
            )
        } else {
            None
        };

        // Resolve date fields
        let y = y.ok_or_else(|| VmError::type_error("year is required"))?;
        if mc_str.is_none() && m.is_none() {
            return Err(VmError::type_error("month or monthCode is required"));
        }
        let d = d.ok_or_else(|| VmError::type_error("day is required"))?;
        let month = if let Some(ref mc) = mc_str {
            let mc_month = validate_month_code_iso_suitability(mc.as_str())? as i32;
            if let Some(mn) = m {
                if mn != mc_month {
                    return Err(VmError::range_error("month and monthCode must agree"));
                }
            }
            mc_month
        } else {
            m.unwrap()
        };
        if month < 1 || d < 1 {
            return Err(VmError::range_error(format!(
                "month ({}) and day ({}) must be positive",
                month, d
            )));
        }
        let pd = temporal_rs::PlainDate::new_with_overflow(
            y,
            month.min(255) as u8,
            d.min(255) as u8,
            temporal_rs::Calendar::default(),
            Overflow::Constrain,
        )
        .map_err(temporal_err)?;

        // Decide ZonedDateTime vs PlainDate
        if !tz_val.is_undefined() {
            let tz = to_temporal_timezone_identifier(ncx, &tz_val)?;
            if let Some(ref os) = offset_str {
                validate_utc_offset_string(os.as_str())?;
            }
            let zdt = pd.to_zoned_date_time(tz, time).map_err(temporal_err)?;
            return Ok(Some(temporal_rs::options::RelativeTo::ZonedDateTime(zdt)));
        }
        return Ok(Some(temporal_rs::options::RelativeTo::PlainDate(pd)));
    }

    Err(VmError::type_error(
        "relativeTo must be a string, PlainDate, or ZonedDateTime",
    ))
}

/// ToTemporalTimeZoneIdentifier — validate and parse a timezone argument.
/// Per spec:
/// 1. If the value is a ZonedDateTime, return its time zone.
/// 2. If not a String, throw TypeError.
/// 3. Parse the string via temporal_rs::TimeZone (validates IANA names, UTC offsets, etc.)
pub(super) fn to_temporal_timezone_identifier(
    ncx: &mut NativeContext<'_>,
    value: &Value,
) -> Result<temporal_rs::TimeZone, VmError> {
    // Step 1: If ZonedDateTime, extract its timezone
    if let Some(obj) = value.as_object() {
        let tt = obj
            .get(&PropertyKey::string(SLOT_TEMPORAL_TYPE))
            .and_then(|v| v.as_string().map(|s| s.as_str().to_string()));
        if tt.as_deref() == Some("ZonedDateTime") {
            let zdt = extract_zoned_date_time(&obj)?;
            return Ok(*zdt.time_zone());
        }
    }

    // Step 2: Must be a string — reject all other types
    if !value.is_string() {
        // Per spec: null, boolean, number, bigint, symbol, object → TypeError
        return Err(VmError::type_error(format!(
            "{} does not convert to a valid ISO string",
            if value.is_null() {
                "null".to_string()
            } else {
                value.type_of().to_string()
            }
        )));
    }

    // Step 3-9: Parse via temporal_rs::TimeZone
    let s = ncx.to_string_value(value)?;
    temporal_rs::TimeZone::try_from_str_with_provider(s.as_str(), tz_provider())
        .map_err(temporal_err)
}
