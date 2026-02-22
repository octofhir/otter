//! Temporal namespace initialization
//!
//! Creates the Temporal global namespace with constructors:
//! - Temporal.Now
//! - Temporal.Instant
//! - Temporal.PlainDate, PlainTime, PlainDateTime
//! - Temporal.PlainYearMonth, PlainMonthDay
//! - Temporal.ZonedDateTime
//! - Temporal.Duration

use crate::context::NativeContext;
use crate::error::VmError;
use crate::gc::GcRef;
use crate::memory::MemoryManager;
use crate::object::{JsObject, PropertyAttributes, PropertyDescriptor, PropertyKey};
use crate::string::JsString;
use crate::value::Value;
use chrono::{Datelike, Timelike};
use std::sync::Arc;

/// Convert a temporal_rs error into a VmError preserving TypeError vs RangeError
fn temporal_err(e: temporal_rs::error::TemporalError) -> VmError {
    let msg = format!("{e}");
    match e.kind() {
        temporal_rs::error::ErrorKind::Type => VmError::type_error(msg),
        temporal_rs::error::ErrorKind::Range => VmError::range_error(msg),
        temporal_rs::error::ErrorKind::Syntax => VmError::range_error(msg),
        _ => VmError::type_error(msg),
    }
}

/// Extract a temporal_rs::PlainDateTime from a JsObject with ISO slots (standalone version)
fn extract_pdt_standalone(obj: &GcRef<JsObject>) -> Result<temporal_rs::PlainDateTime, VmError> {
    let y = obj.get(&PropertyKey::string(SLOT_ISO_YEAR)).and_then(|v| v.as_int32()).unwrap_or(0);
    let mo = obj.get(&PropertyKey::string(SLOT_ISO_MONTH)).and_then(|v| v.as_int32()).unwrap_or(1) as u8;
    let d = obj.get(&PropertyKey::string(SLOT_ISO_DAY)).and_then(|v| v.as_int32()).unwrap_or(1) as u8;
    let h = obj.get(&PropertyKey::string(SLOT_ISO_HOUR)).and_then(|v| v.as_int32()).unwrap_or(0) as u8;
    let mi = obj.get(&PropertyKey::string(SLOT_ISO_MINUTE)).and_then(|v| v.as_int32()).unwrap_or(0) as u8;
    let sec = obj.get(&PropertyKey::string(SLOT_ISO_SECOND)).and_then(|v| v.as_int32()).unwrap_or(0) as u8;
    let ms = obj.get(&PropertyKey::string(SLOT_ISO_MILLISECOND)).and_then(|v| v.as_int32()).unwrap_or(0) as u16;
    let us = obj.get(&PropertyKey::string(SLOT_ISO_MICROSECOND)).and_then(|v| v.as_int32()).unwrap_or(0) as u16;
    let ns = obj.get(&PropertyKey::string(SLOT_ISO_NANOSECOND)).and_then(|v| v.as_int32()).unwrap_or(0) as u16;
    temporal_rs::PlainDateTime::try_new(y, mo, d, h, mi, sec, ms, us, ns, temporal_rs::Calendar::default())
        .map_err(temporal_err)
}

/// Standalone calendar validation (for use outside of install_plain_datetime block scope)
fn validate_calendar_arg_standalone(ncx: &mut NativeContext<'_>, cal: &Value) -> Result<String, VmError> {
    if cal.is_undefined() {
        return Ok("iso8601".to_string());
    }
    if cal.as_symbol().is_some() {
        return Err(VmError::type_error("Cannot convert a Symbol value to a string"));
    }
    if let Some(obj) = cal.as_object() {
        let tt = obj.get(&PropertyKey::string(SLOT_TEMPORAL_TYPE))
            .and_then(|v| v.as_string().map(|s| s.as_str().to_string()));
        match tt.as_deref() {
            Some("PlainDate") | Some("PlainDateTime") | Some("PlainMonthDay") |
            Some("PlainYearMonth") | Some("ZonedDateTime") => return Ok("iso8601".to_string()),
            Some("Duration") | Some("Instant") => return Err(VmError::type_error(format!("{} instance is not a valid calendar", tt.unwrap()))),
            _ => {}
        }
    }
    if !cal.is_string() {
        if cal.is_null() || cal.is_boolean() || cal.is_number() || cal.is_bigint() || cal.as_object().is_some() {
            return Err(VmError::type_error(format!("{} is not a valid calendar", ncx.to_string_value(cal).unwrap_or_default())));
        }
        return Err(VmError::type_error("calendar must be a string"));
    }
    let s = cal.as_string().unwrap().as_str().to_string();
    if s.is_empty() { return Err(VmError::range_error("empty string is not a valid calendar ID")); }
    let lower = s.to_ascii_lowercase();
    if lower == "iso8601" { return Ok("iso8601".to_string()); }
    if s.chars().any(|c| c.is_ascii_digit()) {
        if temporal_rs::PlainDateTime::from_utf8(s.as_bytes()).is_ok() { return Ok("iso8601".to_string()); }
        if temporal_rs::PlainDate::from_utf8(s.as_bytes()).is_ok() { return Ok("iso8601".to_string()); }
        if temporal_rs::PlainTime::from_utf8(s.as_bytes()).is_ok() { return Ok("iso8601".to_string()); }
        if temporal_rs::PlainMonthDay::from_utf8(s.as_bytes()).is_ok() { return Ok("iso8601".to_string()); }
        if temporal_rs::PlainYearMonth::from_utf8(s.as_bytes()).is_ok() { return Ok("iso8601".to_string()); }
        return Err(VmError::range_error(format!("{} is not a valid calendar ID", s)));
    }
    Err(VmError::range_error(format!("{} is not a valid calendar ID", s)))
}

/// Standalone ToTemporalDateTime — for use outside install_plain_datetime block scope
fn to_temporal_datetime_standalone(ncx: &mut NativeContext<'_>, item: &Value) -> Result<temporal_rs::PlainDateTime, VmError> {
    if item.is_string() {
        let s = ncx.to_string_value(item)?;
        reject_utc_designator_for_plain(s.as_str())?;
        return temporal_rs::PlainDateTime::from_utf8(s.as_bytes()).map_err(temporal_err);
    }
    if item.is_undefined() || item.is_null() || item.is_boolean() || item.is_number() || item.is_bigint() {
        return Err(VmError::type_error(format!("cannot convert {} to a PlainDateTime", item.type_of())));
    }
    if item.as_symbol().is_some() {
        return Err(VmError::type_error("Cannot convert a Symbol value to a string"));
    }
    if let Some(obj) = item.as_object() {
        let temporal_type = obj.get(&PropertyKey::string(SLOT_TEMPORAL_TYPE))
            .and_then(|v| v.as_string().map(|s| s.as_str().to_string()));
        if temporal_type.as_deref() == Some("PlainDateTime") {
            return extract_pdt_standalone(&obj);
        }
        if temporal_type.as_deref() == Some("PlainDate") {
            let y = obj.get(&PropertyKey::string(SLOT_ISO_YEAR)).and_then(|v| v.as_int32()).unwrap_or(0);
            let mo = obj.get(&PropertyKey::string(SLOT_ISO_MONTH)).and_then(|v| v.as_int32()).unwrap_or(1);
            let d = obj.get(&PropertyKey::string(SLOT_ISO_DAY)).and_then(|v| v.as_int32()).unwrap_or(1);
            return temporal_rs::PlainDateTime::try_new(y, mo as u8, d as u8, 0,0,0,0,0,0, temporal_rs::Calendar::default()).map_err(temporal_err);
        }
        // Property bag: calendar, day, hour, microsecond, millisecond, minute, month, monthCode, nanosecond, second, year
        let calendar_val = ncx.get_property(&obj, &PropertyKey::string("calendar"))?;
        if !calendar_val.is_undefined() { validate_calendar_arg_standalone(ncx, &calendar_val)?; }
        let day_val = ncx.get_property(&obj, &PropertyKey::string("day"))?;
        let d = if !day_val.is_undefined() { let n = ncx.to_number_value(&day_val)?; if n.is_infinite() { return Err(VmError::range_error("day cannot be Infinity")); } n as i32 } else { return Err(VmError::type_error("day is required")); };
        let hour_val = ncx.get_property(&obj, &PropertyKey::string("hour"))?;
        let h = if !hour_val.is_undefined() { let n = ncx.to_number_value(&hour_val)?; if n.is_infinite() { return Err(VmError::range_error("hour cannot be Infinity")); } n as i32 } else { 0 };
        let us_val = ncx.get_property(&obj, &PropertyKey::string("microsecond"))?;
        let us = if !us_val.is_undefined() { let n = ncx.to_number_value(&us_val)?; if n.is_infinite() { return Err(VmError::range_error("microsecond cannot be Infinity")); } n as i32 } else { 0 };
        let ms_val = ncx.get_property(&obj, &PropertyKey::string("millisecond"))?;
        let ms = if !ms_val.is_undefined() { let n = ncx.to_number_value(&ms_val)?; if n.is_infinite() { return Err(VmError::range_error("millisecond cannot be Infinity")); } n as i32 } else { 0 };
        let min_val = ncx.get_property(&obj, &PropertyKey::string("minute"))?;
        let mi = if !min_val.is_undefined() { let n = ncx.to_number_value(&min_val)?; if n.is_infinite() { return Err(VmError::range_error("minute cannot be Infinity")); } n as i32 } else { 0 };
        let month_val = ncx.get_property(&obj, &PropertyKey::string("month"))?;
        let month_code_val = ncx.get_property(&obj, &PropertyKey::string("monthCode"))?;
        let month = if !month_code_val.is_undefined() {
            let mc_str = ncx.to_string_value(&month_code_val)?;
            validate_month_code_syntax(&mc_str)?;
            validate_month_code_iso_suitability(&mc_str)? as i32
        } else if !month_val.is_undefined() {
            let n = ncx.to_number_value(&month_val)?;
            if n.is_infinite() { return Err(VmError::range_error("month cannot be Infinity")); }
            n as i32
        } else { return Err(VmError::type_error("month or monthCode is required")); };
        let ns_val = ncx.get_property(&obj, &PropertyKey::string("nanosecond"))?;
        let ns = if !ns_val.is_undefined() { let n = ncx.to_number_value(&ns_val)?; if n.is_infinite() { return Err(VmError::range_error("nanosecond cannot be Infinity")); } n as i32 } else { 0 };
        let sec_val = ncx.get_property(&obj, &PropertyKey::string("second"))?;
        let sec = if !sec_val.is_undefined() { let sv = ncx.to_number_value(&sec_val)? as i32; if sv == 60 { 59 } else { sv } } else { 0 };
        let year_val = ncx.get_property(&obj, &PropertyKey::string("year"))?;
        let y = if !year_val.is_undefined() { let n = ncx.to_number_value(&year_val)?; if n.is_infinite() { return Err(VmError::range_error("year cannot be Infinity")); } n as i32 } else { return Err(VmError::type_error("year is required")); };
        return temporal_rs::PlainDateTime::try_new(y, month as u8, d as u8, h as u8, mi as u8, sec as u8, ms as u16, us as u16, ns as u16, temporal_rs::Calendar::default()).map_err(temporal_err);
    }
    Err(VmError::type_error("Expected an object or string"))
}

// ============================================================================
// Internal helpers for ISO calendar
// ============================================================================

/// Days in each month for a common year (index 0 = unused, 1-12 = Jan-Dec)
const DAYS_IN_MONTH: [u32; 13] = [0, 31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];

fn is_leap_year(year: i32) -> bool {
    (year % 4 == 0 && year % 100 != 0) || year % 400 == 0
}

fn days_in_month(month: u32, year: i32) -> u32 {
    if month == 2 && is_leap_year(year) {
        29
    } else if month >= 1 && month <= 12 {
        DAYS_IN_MONTH[month as usize]
    } else {
        31
    }
}

/// Convert ISO date to days from Unix epoch (1970-01-01)
fn iso_date_to_epoch_days(year: i32, month: i32, day: i32) -> i64 {
    // Algorithm from https://howardhinnant.github.io/date_algorithms.html
    let y = if month <= 2 { year as i64 - 1 } else { year as i64 };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = (y - era * 400) as u32;
    let m = month as u32;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + day as u32 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe as i64 - 719468
}

/// Parse timezone offset string to nanoseconds
/// Supports: "UTC", "+HH:MM", "-HH:MM", "+HH:MM:SS", "+HHMM", "+HH"
fn parse_tz_offset_ns(tz_id: &str) -> Result<i128, VmError> {
    let upper = tz_id.to_ascii_uppercase();
    if upper == "UTC" || upper == "Z" {
        return Ok(0);
    }
    if tz_id.starts_with('+') || tz_id.starts_with('-') || tz_id.starts_with('\u{2212}') {
        let sign: i128 = if tz_id.starts_with('-') || tz_id.starts_with('\u{2212}') { -1 } else { 1 };
        let offset_part = if tz_id.starts_with('\u{2212}') { &tz_id[3..] } else { &tz_id[1..] };
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
                    return Err(VmError::range_error(format!("invalid time zone offset: {}", tz_id)));
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
            _ => return Err(VmError::range_error(format!("invalid time zone offset: {}", tz_id))),
        };
        return Ok(sign * (hours * 3_600_000_000_000 + minutes * 60_000_000_000 + seconds * 1_000_000_000));
    }
    // Named timezone (e.g., "America/New_York") — not supported without IANA data
    Err(VmError::range_error(format!("named time zone {} requires IANA timezone data which is not available", tz_id)))
}

/// Convert a ToIntegerWithTruncation per Temporal spec (like ToIntegerIfIntegral but truncates)
fn to_integer_with_truncation(ncx: &mut NativeContext<'_>, val: &Value) -> Result<f64, VmError> {
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

/// Validate ISO month-day, returning (month, day, referenceISOYear)
fn validate_iso_month_day(
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
fn parse_month_code(s: &str) -> Option<u32> {
    if s.len() == 3 && s.starts_with('M') {
        s[1..].parse::<u32>().ok().filter(|&m| m >= 1 && m <= 12)
    } else {
        None
    }
}

/// Validate monthCode SYNTAX only — catches "L99M", "m01", "M1" etc.
/// Does NOT check suitability for ISO calendar (leap months, out-of-range months)
fn validate_month_code_syntax(s: &str) -> Result<(), VmError> {
    // Must start with 'M' (capital)
    if !s.starts_with('M') {
        return Err(VmError::range_error(format!(
            "monthCode '{}' is not well-formed",
            s
        )));
    }

    // After 'M', must be two digits optionally followed by 'L'
    let rest = &s[1..];
    let without_l = if rest.ends_with('L') {
        &rest[..rest.len() - 1]
    } else {
        rest
    };

    if without_l.len() != 2 || !without_l.chars().all(|c| c.is_ascii_digit()) {
        return Err(VmError::range_error(format!(
            "monthCode '{}' is not well-formed",
            s
        )));
    }

    Ok(())
}

/// Validate monthCode SUITABILITY for ISO calendar — checks leap months, range 1-12
/// Call this AFTER year type validation
fn validate_month_code_iso_suitability(s: &str) -> Result<u32, VmError> {
    // At this point, syntax is already validated: M## or M##L
    let has_leap = s.ends_with('L');
    if has_leap {
        return Err(VmError::range_error(format!(
            "monthCode {} is not valid for ISO 8601 calendar",
            s
        )));
    }

    let digits = &s[1..3];
    let month: u32 = digits.parse().map_err(|_| {
        VmError::range_error(format!("monthCode '{}' is not well-formed", s))
    })?;

    if month < 1 || month > 12 {
        return Err(VmError::range_error(format!(
            "monthCode {} is not valid for ISO 8601 calendar",
            s
        )));
    }

    Ok(month)
}

/// Resolve calendar from a property bag's calendar property
fn resolve_calendar_from_property(ncx: &mut NativeContext<'_>, val: &Value) -> Result<(), VmError> {
    // Per spec: null, boolean, number, bigint → TypeError
    if val.is_null() || val.is_boolean() || val.is_number() || val.is_bigint() {
        return Err(VmError::type_error(format!(
            "{} is not a valid calendar",
            if val.is_null() { "null".to_string() } else { val.type_of().to_string() }
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
        if let Some(ty) = obj.get(&PropertyKey::string(SLOT_TEMPORAL_TYPE))
            .and_then(|v| v.as_string().map(|s| s.as_str().to_string())) {
            match ty.as_str() {
                "PlainDate" | "PlainDateTime" | "PlainMonthDay" | "PlainYearMonth" | "ZonedDateTime" => {
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
        return Err(VmError::range_error(
            "reject minus zero as extended year",
        ));
    }

    // Try to parse as Temporal ISO string first — temporal_rs handles all formats
    // If the string is a valid ISO date/datetime/yearmonth/monthday string,
    // the calendar is either extracted from annotation or defaults to "iso8601"
    let lower = s.to_ascii_lowercase();
    if lower == "iso8601" {
        return Ok(());
    }

    // Try parsing as various Temporal ISO string formats
    if temporal_rs::PlainDateTime::from_utf8(s.as_bytes()).is_ok()
        || temporal_rs::PlainDate::from_utf8(s.as_bytes()).is_ok()
        || temporal_rs::PlainMonthDay::from_utf8(s.as_bytes()).is_ok()
        || temporal_rs::PlainYearMonth::from_utf8(s.as_bytes()).is_ok()
    {
        // Valid ISO string — calendar defaults to "iso8601"
        // Check for invalid calendar annotation (if present, must be iso8601)
        if let Some(bracket_pos) = s.find("[u-ca=") {
            let after = &s[bracket_pos + 6..];
            if let Some(close) = after.find(']') {
                let cal = &after[..close];
                if cal.to_ascii_lowercase() != "iso8601" {
                    return Err(VmError::range_error(format!("Unknown calendar: {}", cal)));
                }
            }
        } else if let Some(bracket_pos) = s.find("[!u-ca=") {
            let after = &s[bracket_pos + 7..];
            if let Some(close) = after.find(']') {
                let cal = &after[..close];
                if cal.to_ascii_lowercase() != "iso8601" {
                    return Err(VmError::range_error(format!("Unknown calendar: {}", cal)));
                }
            }
        }
        return Ok(());
    }

    // Not a valid ISO string — treat as plain calendar ID
    Err(VmError::range_error(format!(
        "Unknown calendar: {}",
        s
    )))
}

/// Format monthCode from month number
fn format_month_code(month: u32) -> String {
    format!("M{:02}", month)
}

// ============================================================================
// Temporal ISO string parsing
// ============================================================================

/// Parse an ISO date string for PlainMonthDay, returning (year, month, day)
/// Uses temporal_rs for spec-compliant parsing of all ISO 8601 + RFC 9557 formats.
fn parse_temporal_month_day_string(s: &str) -> Result<(i32, u32, u32), VmError> {
    let pmd = temporal_rs::PlainMonthDay::from_utf8(s.as_bytes()).map_err(temporal_err)?;
    Ok((pmd.reference_year(), pmd.month_code().to_month_integer() as u32, pmd.day() as u32))
}

/// Find the position of a time separator (T, t, or space) in an ISO datetime string
/// Returns None if no time separator found
fn find_time_separator(s: &str) -> Option<usize> {
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
fn has_standalone_utc_offset(s: &str) -> bool {
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
        if (bytes[5] == b'+' || bytes[5] == b'-')
            && s[6..].chars().all(|c| c.is_ascii_digit())
        {
            return true;
        }
    }

    false
}

/// Check if a string has a UTC offset (Z, +HH:MM, -HH:MM, +HH, -HH)
fn has_utc_offset(s: &str) -> bool {
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

fn parse_iso_date(s: &str) -> Result<(i32, u32, u32), VmError> {
    // Handle extended year format: +YYYYYY-MM-DD or -YYYYYY-MM-DD
    let (year_str, rest) = if s.starts_with('+') || s.starts_with('-') {
        // Find the dash after the year
        if let Some(dash_pos) = s[1..].find('-').map(|p| p + 1) {
            // Make sure this isn't the minus sign for negative year
            if dash_pos > 4 {
                (&s[..dash_pos], &s[dash_pos + 1..])
            } else if let Some(dash_pos2) = s[dash_pos + 1..].find('-').map(|p| p + dash_pos + 1)
            {
                (&s[..dash_pos2], &s[dash_pos2 + 1..])
            } else {
                return Err(VmError::range_error(format!(
                    "invalid ISO date string: {}",
                    s
                )));
            }
        } else {
            return Err(VmError::range_error(format!(
                "invalid ISO date string: {}",
                s
            )));
        }
    } else {
        // Regular YYYY-MM-DD
        if s.len() < 10 || s.as_bytes()[4] != b'-' {
            return Err(VmError::range_error(format!(
                "invalid ISO date string: {}",
                s
            )));
        }
        (&s[..4], &s[5..])
    };

    let year: i32 = year_str.parse().map_err(|_| {
        VmError::range_error(format!("invalid year in ISO date string: {}", year_str))
    })?;

    // Reject -000000 (negative zero year)
    if year == 0 && year_str.starts_with('-') {
        return Err(VmError::range_error(
            "reject minus zero as extended year",
        ));
    }

    if rest.len() < 5 || rest.as_bytes()[2] != b'-' {
        return Err(VmError::range_error(format!(
            "invalid ISO date string: {}",
            s
        )));
    }

    let month: u32 = rest[..2].parse().map_err(|_| {
        VmError::range_error(format!("invalid month in ISO date string: {}", &rest[..2]))
    })?;

    let day: u32 = rest[3..5].parse().map_err(|_| {
        VmError::range_error(format!("invalid day in ISO date string: {}", &rest[3..5]))
    })?;

    if month < 1 || month > 12 {
        return Err(VmError::range_error(format!(
            "month must be between 1 and 12, got {}",
            month
        )));
    }
    let max_day = days_in_month(month, year);
    if day < 1 || day > max_day {
        return Err(VmError::range_error(format!(
            "day must be between 1 and {}, got {}",
            max_day, day
        )));
    }

    Ok((year, month, day))
}

fn validate_annotations(s: &str) -> Result<(), VmError> {
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

        let close = remaining.find(']').ok_or_else(|| {
            VmError::range_error("unterminated annotation bracket")
        })?;

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
                }
                seen_calendar = true;
                if is_critical {
                    seen_critical = true;
                }

                // Validate calendar ID
                if value != "iso8601" {
                    return Err(VmError::range_error(format!(
                        "Unknown calendar: {}",
                        value
                    )));
                }
                _calendar_value = value.to_string();
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
// Property keys (internal slots)
// ============================================================================

const SLOT_ISO_MONTH: &str = "__temporal_iso_month__";
const SLOT_ISO_DAY: &str = "__temporal_iso_day__";
const SLOT_ISO_YEAR: &str = "__temporal_iso_year__";
const SLOT_TEMPORAL_TYPE: &str = "__temporal_type__";

// ============================================================================
// Helper: resolve a value to (month, day, refYear) for PlainMonthDay comparison
// ============================================================================

/// Given a value (PlainMonthDay object, string, or property bag), extract (month, day, refYear).
fn resolve_plain_month_day_fields(
    ncx: &mut NativeContext<'_>,
    val: &Value,
) -> Result<(i32, i32, i32), VmError> {
    // Case 1: PlainMonthDay object (has temporal type)
    if let Some(obj) = val.as_object() {
        let temporal_type = obj.get(&PropertyKey::string(SLOT_TEMPORAL_TYPE))
            .and_then(|v| v.as_string().map(|s| s.as_str().to_string()));
        if temporal_type.as_deref() == Some("PlainMonthDay") {
            let m = obj.get(&PropertyKey::string(SLOT_ISO_MONTH)).and_then(|v| v.as_int32()).unwrap_or(1);
            let d = obj.get(&PropertyKey::string(SLOT_ISO_DAY)).and_then(|v| v.as_int32()).unwrap_or(1);
            let y = obj.get(&PropertyKey::string(SLOT_ISO_YEAR)).and_then(|v| v.as_int32()).unwrap_or(1972);
            return Ok((m, d, y));
        }

        // Property bag: validate calendar first
        let calendar_val = ncx.get_property(&obj, &PropertyKey::string("calendar"))?;
        if !calendar_val.is_undefined() {
            resolve_calendar_from_property(ncx, &calendar_val)?;
        }

        // Read fields via observable get (alphabetical order)
        let day_val = ncx.get_property(&obj, &PropertyKey::string("day"))?;
        let month_val = ncx.get_property(&obj, &PropertyKey::string("month"))?;
        let month_code_val = ncx.get_property(&obj, &PropertyKey::string("monthCode"))?;
        let year_val = ncx.get_property(&obj, &PropertyKey::string("year"))?;

        let has_day = !day_val.is_undefined();
        let has_month = !month_val.is_undefined();
        let has_month_code = !month_code_val.is_undefined();

        if !has_day {
            return Err(VmError::type_error("day is required"));
        }
        if !has_month && !has_month_code {
            return Err(VmError::type_error("either month or monthCode is required"));
        }

        let day_num = ncx.to_number_value(&day_val)?;
        if day_num.is_infinite() {
            return Err(VmError::range_error("day property cannot be Infinity"));
        }
        let day = day_num as i32;

        let month = if has_month_code {
            let mc = ncx.to_string_value(&month_code_val)?;
            validate_month_code_syntax(mc.as_str())?;
            validate_month_code_iso_suitability(mc.as_str())? as i32
        } else {
            let m_num = ncx.to_number_value(&month_val)?;
            if m_num.is_infinite() {
                return Err(VmError::range_error("month property cannot be Infinity"));
            }
            m_num as i32
        };

        // Year from property bag is NOT used as reference year for PlainMonthDay.
        // Per spec, ISO calendar always uses 1972 as the reference year.
        // We still read the year for observable side effects.
        if !year_val.is_undefined() {
            let y_num = ncx.to_number_value(&year_val)?;
            if y_num.is_infinite() {
                return Err(VmError::range_error("year property cannot be Infinity"));
            }
        }
        let year = 1972; // ISO reference year

        return Ok((month, day, year));
    }

    // Case 2: String — try month-day, then date, then datetime
    if val.is_string() {
        let s = ncx.to_string_value(val)?;
        // Try PlainMonthDay first
        if let Ok((ref_year, month, day)) = parse_temporal_month_day_string(s.as_str()) {
            return Ok((month as i32, day as i32, ref_year));
        }
        // Try PlainDate
        if let Ok(pd) = temporal_rs::PlainDate::from_utf8(s.as_str().as_bytes()) {
            return Ok((pd.month() as i32, pd.day() as i32, pd.year()));
        }
        // Try PlainDateTime (handles leap seconds — second:60 is clamped)
        if let Ok(pdt) = temporal_rs::PlainDateTime::from_utf8(s.as_str().as_bytes()) {
            return Ok((pdt.month() as i32, pdt.day() as i32, pdt.year()));
        }
        // If none parse, fall through to generic error
        let _ = parse_temporal_month_day_string(s.as_str())?;
    }

    Err(VmError::type_error("invalid argument for PlainMonthDay comparison"))
}

// ============================================================================
// PlainMonthDay constructor
// ============================================================================

fn create_plain_month_day_constructor(
    prototype: GcRef<JsObject>,
) -> Box<dyn Fn(&Value, &[Value], &mut NativeContext<'_>) -> Result<Value, VmError> + Send + Sync>
{
    Box::new(move |this, args, ncx| {
        // Step 1: If NewTarget is undefined, throw TypeError
        // When called with `new`, `this` is a new object with prototype === PlainMonthDay.prototype
        // When called without `new`, `this` is the receiver (Temporal namespace or undefined)
        let is_new_target = if let Some(obj) = this.as_object() {
            // Check if this was created by `new` by verifying prototype chain
            obj.prototype().as_object().map_or(false, |p| p.as_ptr() == prototype.as_ptr())
        } else {
            false
        };
        if !is_new_target {
            return Err(VmError::type_error("Temporal.PlainMonthDay constructor requires 'new'"));
        }

        // new Temporal.PlainMonthDay(isoMonth, isoDay [, calendar [, referenceISOYear]])
        let iso_month_val = args.first().cloned().unwrap_or(Value::undefined());
        let iso_day_val = args.get(1).cloned().unwrap_or(Value::undefined());
        let calendar_val = args.get(2).cloned().unwrap_or(Value::undefined());
        let ref_year_val = args.get(3).cloned().unwrap_or(Value::undefined());

        // ToIntegerWithTruncation for month
        let iso_month = to_integer_with_truncation(ncx, &iso_month_val)? as i32;

        // ToIntegerWithTruncation for day
        let iso_day = to_integer_with_truncation(ncx, &iso_day_val)? as i32;

        // Calendar validation: ToTemporalCalendarIdentifier requires a String type
        if !calendar_val.is_undefined() {
            if !calendar_val.is_string() {
                return Err(VmError::type_error(format!(
                    "{} is not a valid calendar",
                    if calendar_val.is_null() { "null".to_string() } else { calendar_val.type_of().to_string() }
                )));
            }
            let cal_str = calendar_val.as_string().unwrap().as_str().to_ascii_lowercase();
            if cal_str != "iso8601" {
                return Err(VmError::range_error(format!("Unknown calendar: {}", cal_str)));
            }
        }

        // Reference year (default 1972)
        let reference_year = if ref_year_val.is_undefined() {
            1972
        } else {
            to_integer_with_truncation(ncx, &ref_year_val)? as i32
        };

        // Validate
        let (month, day, year) = validate_iso_month_day(iso_month, iso_day, reference_year)?;

        // Validate reference year is within ISO date range
        temporal_rs::PlainDate::try_new_iso(year, month as u8, day as u8).map_err(temporal_err)?;

        // Store internal slots on `this`
        if let Some(obj) = this.as_object() {
            obj.define_property(
                PropertyKey::string(SLOT_ISO_MONTH),
                PropertyDescriptor::builtin_data(Value::int32(month as i32)),
            );
            obj.define_property(
                PropertyKey::string(SLOT_ISO_DAY),
                PropertyDescriptor::builtin_data(Value::int32(day as i32)),
            );
            obj.define_property(
                PropertyKey::string(SLOT_ISO_YEAR),
                PropertyDescriptor::builtin_data(Value::int32(year)),
            );
            obj.define_property(
                PropertyKey::string(SLOT_TEMPORAL_TYPE),
                PropertyDescriptor::builtin_data(Value::string(JsString::intern("PlainMonthDay"))),
            );
        }

        Ok(Value::undefined())
    })
}

// ============================================================================
// PlainMonthDay.from()
// ============================================================================

fn plain_month_day_from(
    pmd_ctor_value: Value,
    _this: &Value,
    args: &[Value],
    ncx: &mut NativeContext<'_>,
) -> Result<Value, VmError> {
    let item = args.first().cloned().unwrap_or(Value::undefined());
    let options_val = args.get(1).cloned().unwrap_or(Value::undefined());

    // For string arguments: parse string first, then validate options (per spec order)
    if item.is_string() {
        let result = plain_month_day_from_string(ncx, &pmd_ctor_value, &item)?;
        // Read overflow option (for observable side effects) but string parsing ignores it
        let _overflow = parse_overflow_option(ncx, &options_val)?;
        return Ok(result);
    }

    if item.is_undefined() || item.is_null() {
        return Err(VmError::type_error(
            "Cannot convert undefined or null to a Temporal.PlainMonthDay",
        ));
    }

    if item.is_number() || item.is_boolean() {
        return Err(VmError::type_error(format!(
            "invalid type for Temporal.PlainMonthDay.from: {}",
            if item.is_number() { "a number" } else { "a boolean" }
        )));
    }

    if item.as_symbol().is_some() {
        return Err(VmError::type_error(
            "invalid type for Temporal.PlainMonthDay.from: a Symbol",
        ));
    }

    // Check if it's already a PlainMonthDay or another Temporal type
    if let Some(obj) = item.as_object() {
        if let Some(type_val) = obj.get(&PropertyKey::string(SLOT_TEMPORAL_TYPE)) {
            let type_str = type_val.as_string().map(|s| s.as_str().to_string());
            if type_str.as_deref() == Some("PlainMonthDay") {
                // Read overflow option (for observable side effects)
                let _overflow = parse_overflow_option(ncx, &options_val)?;
                // Return a copy preserving the reference year
                let month = obj
                    .get(&PropertyKey::string(SLOT_ISO_MONTH))
                    .and_then(|v| v.as_int32())
                    .unwrap_or(1);
                let day = obj
                    .get(&PropertyKey::string(SLOT_ISO_DAY))
                    .and_then(|v| v.as_int32())
                    .unwrap_or(1);
                let year = obj
                    .get(&PropertyKey::string(SLOT_ISO_YEAR))
                    .and_then(|v| v.as_int32())
                    .unwrap_or(1972);
                return create_plain_month_day_value(ncx, &pmd_ctor_value, month, day, year);
            }
            if type_str.as_deref() == Some("PlainDate") {
                // Extract month and day from PlainDate, use 1972 as reference year
                let _overflow = parse_overflow_option(ncx, &options_val)?;
                let month = obj
                    .get(&PropertyKey::string(SLOT_ISO_MONTH))
                    .and_then(|v| v.as_int32())
                    .unwrap_or(1);
                let day = obj
                    .get(&PropertyKey::string(SLOT_ISO_DAY))
                    .and_then(|v| v.as_int32())
                    .unwrap_or(1);
                return create_plain_month_day_value(ncx, &pmd_ctor_value, month, day, 1972);
            }
        }

        // It's a property bag — per spec, fields are read first, then options
        return plain_month_day_from_fields(ncx, &pmd_ctor_value, &obj, &options_val);
    }

    // Handle Proxy as a property bag
    if let Some(proxy) = item.as_proxy() {
        return plain_month_day_from_proxy(ncx, &pmd_ctor_value, proxy, &item, &options_val);
    }

    Err(VmError::type_error(
        "invalid type for Temporal.PlainMonthDay.from",
    ))
}

/// Read a property from a proxy using proxy_get trap
fn proxy_get_property(
    ncx: &mut NativeContext<'_>,
    proxy: GcRef<crate::proxy::JsProxy>,
    receiver: &Value,
    key: &str,
) -> Result<Value, VmError> {
    let pk = PropertyKey::string(key);
    let kv = crate::proxy_operations::property_key_to_value_pub(&pk);
    crate::proxy_operations::proxy_get(ncx, proxy.clone(), &pk, kv, receiver.clone())
}

fn plain_month_day_from_proxy(
    ncx: &mut NativeContext<'_>,
    pmd_ctor_value: &Value,
    proxy: GcRef<crate::proxy::JsProxy>,
    receiver: &Value,
    options_val: &Value,
) -> Result<Value, VmError> {
    // Read fields through proxy traps with interleaved conversion
    // (per spec PrepareTemporalFields: get + convert each field in order)

    // 1. get calendar (string, no valueOf)
    let calendar_val = proxy_get_property(ncx, proxy.clone(), receiver, "calendar")?;
    if !calendar_val.is_undefined() {
        resolve_calendar_from_property(ncx, &calendar_val)?;
    }

    // 2. get day + convert to number
    let day_val = proxy_get_property(ncx, proxy.clone(), receiver, "day")?;
    let day_raw = if !day_val.is_undefined() {
        let n = ncx.to_number_value(&day_val)?;
        if n.is_infinite() {
            return Err(VmError::range_error("day property cannot be Infinity"));
        }
        n as i32
    } else {
        return Err(VmError::type_error("day is required"));
    };

    // 3. get month + convert to number
    let month_val = proxy_get_property(ncx, proxy.clone(), receiver, "month")?;
    let month_num = if !month_val.is_undefined() {
        let n = ncx.to_number_value(&month_val)?;
        if n.is_infinite() {
            return Err(VmError::range_error("month property cannot be Infinity"));
        }
        Some(n as i32)
    } else {
        None
    };

    // 4. get monthCode + convert to string
    let month_code_val = proxy_get_property(ncx, proxy.clone(), receiver, "monthCode")?;
    let mc_str = if !month_code_val.is_undefined() {
        let mc = ncx.to_string_value(&month_code_val)?;
        validate_month_code_syntax(mc.as_str())?;
        Some(mc)
    } else {
        None
    };

    // 5. get year + convert to number
    let year_val = proxy_get_property(ncx, proxy.clone(), receiver, "year")?;
    let validation_year = if !year_val.is_undefined() {
        let n = ncx.to_number_value(&year_val)?;
        if n.is_infinite() {
            return Err(VmError::range_error("year property cannot be Infinity"));
        }
        Some(n as i32)
    } else {
        None
    };

    // Require either month or monthCode
    if month_num.is_none() && mc_str.is_none() {
        return Err(VmError::type_error("either month or monthCode is required"));
    }

    // Parse overflow option AFTER reading fields but BEFORE algorithmic validation
    let overflow = parse_overflow_option(ncx, options_val)?;

    let month = if let Some(ref mc) = mc_str {
        let mc_month = validate_month_code_iso_suitability(mc.as_str())?;
        if let Some(m_int) = month_num {
            if m_int != mc_month as i32 {
                return Err(VmError::range_error(format!(
                    "monthCode {} and month {} conflict",
                    mc, m_int
                )));
            }
        }
        mc_month as i32
    } else {
        month_num.unwrap()
    };

    let reference_year = 1972;
    let year_for_validation = validation_year.unwrap_or(reference_year);

    match overflow {
        Overflow::Reject => {
            if month < 1 || month > 12 {
                return Err(VmError::range_error(format!(
                    "month must be between 1 and 12, got {}",
                    month
                )));
            }
            let max_day = days_in_month(month as u32, year_for_validation);
            if day_raw < 1 || day_raw as u32 > max_day {
                return Err(VmError::range_error(format!(
                    "day must be between 1 and {}, got {}",
                    max_day, day_raw
                )));
            }
            create_plain_month_day_value(ncx, pmd_ctor_value, month, day_raw, reference_year)
        }
        Overflow::Constrain => {
            if month < 1 {
                return Err(VmError::range_error(format!(
                    "month must be >= 1, got {}",
                    month
                )));
            }
            let clamped_month = month.min(12);
            let max_day = days_in_month(clamped_month as u32, year_for_validation);
            if day_raw < 1 {
                return Err(VmError::range_error(format!(
                    "day must be >= 1, got {}",
                    day_raw
                )));
            }
            let clamped_day = (day_raw as u32).min(max_day) as i32;
            create_plain_month_day_value(
                ncx,
                pmd_ctor_value,
                clamped_month,
                clamped_day,
                reference_year,
            )
        }
    }
}

fn parse_overflow_option(
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
        let overflow_val = crate::proxy_operations::proxy_get(
            ncx,
            proxy,
            &key,
            key_value,
            options_val.clone(),
        )?;
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

#[derive(Debug, Clone, Copy, PartialEq)]
enum Overflow {
    Constrain,
    Reject,
}

impl Overflow {
    fn to_temporal_rs(self) -> temporal_rs::options::Overflow {
        match self {
            Overflow::Constrain => temporal_rs::options::Overflow::Constrain,
            Overflow::Reject => temporal_rs::options::Overflow::Reject,
        }
    }
}

fn plain_month_day_from_string(
    ncx: &mut NativeContext<'_>,
    pmd_ctor_value: &Value,
    item: &Value,
) -> Result<Value, VmError> {
    let s = ncx.to_string_value(item)?;
    // temporal_rs handles all validation: UTC designator rejection,
    // non-ASCII minus, fractional hours/minutes, annotations, etc.
    let (ref_year, month, day) = parse_temporal_month_day_string(s.as_str())?;
    create_plain_month_day_value(ncx, pmd_ctor_value, month as i32, day as i32, ref_year)
}

/// Parse a timezone identifier string into an offset in nanoseconds.
/// Handles fixed-offset timezones like "+05:30", "-00:02", "UTC".
fn parse_timezone_offset_ns(tz_id: &str) -> i128 {
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
            return sign * (hours * 3_600_000_000_000 + minutes * 60_000_000_000 + seconds * 1_000_000_000);
        }
    }
    0
}

/// Reject strings with Z UTC designator for PlainMonthDay/PlainDate/etc
fn reject_utc_designator_for_plain(s: &str) -> Result<(), VmError> {
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
fn reject_fractional_hours_minutes(s: &str) -> Result<(), VmError> {
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
        let parts: Vec<&str> = time_clean.splitn(2, |c: char| c == '.' || c == ',').collect();
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
fn strip_time_offset(time: &str) -> &str {
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

fn plain_month_day_from_fields(
    ncx: &mut NativeContext<'_>,
    pmd_ctor_value: &Value,
    fields: &GcRef<JsObject>,
    options_val: &Value,
) -> Result<Value, VmError> {
    // Get calendar first (per spec order)
    let calendar_val = fields.get(&PropertyKey::string("calendar"));
    if let Some(ref cv) = calendar_val {
        if !cv.is_undefined() {
            resolve_calendar_from_property(ncx, cv)?;
        }
    }

    // Get month/monthCode, day, year
    let day_val = fields.get(&PropertyKey::string("day"));
    let month_val = fields.get(&PropertyKey::string("month"));
    let month_code_val = fields.get(&PropertyKey::string("monthCode"));
    let year_val = fields.get(&PropertyKey::string("year"));

    // day is always required
    let day_raw = match day_val {
        Some(ref dv) if !dv.is_undefined() => {
            let n = ncx.to_number_value(dv)?;
            if n.is_infinite() {
                return Err(VmError::range_error("day property cannot be Infinity"));
            }
            if n.is_nan() {
                return Err(VmError::range_error("day property cannot be NaN"));
            }
            n as i32
        }
        _ => {
            return Err(VmError::type_error("day is required"));
        }
    };

    // Need either month or monthCode
    let has_month = month_val.as_ref().map_or(false, |v| !v.is_undefined());
    let has_month_code = month_code_val.as_ref().map_or(false, |v| !v.is_undefined());

    if !has_month && !has_month_code {
        return Err(VmError::type_error("either month or monthCode is required"));
    }

    // Step 1: Validate monthCode SYNTAX (before year type validation)
    let mc_str = if has_month_code {
        let mc = ncx.to_string_value(&month_code_val.clone().unwrap())?;
        validate_month_code_syntax(mc.as_str())?;
        Some(mc)
    } else {
        None
    };

    // Step 2: Convert year to number (TypeError for Symbol, etc.)
    let validation_year = if let Some(ref yv) = year_val {
        if !yv.is_undefined() {
            let n = ncx.to_number_value(yv)?;
            if n.is_infinite() {
                return Err(VmError::range_error("year property cannot be Infinity"));
            }
            if n.is_nan() {
                return Err(VmError::range_error("year property cannot be NaN"));
            }
            Some(n as i32)
        } else {
            None
        }
    } else {
        None
    };

    // Parse overflow option AFTER reading fields but BEFORE algorithmic validation
    // (per spec order of operations: read fields → read options → validate)
    let overflow = parse_overflow_option(ncx, options_val)?;

    // Step 3: Validate monthCode SUITABILITY for ISO calendar (after options read)
    let month = if let Some(ref mc) = mc_str {
        let mc_month = validate_month_code_iso_suitability(mc.as_str())?;

        // Check for month/monthCode conflict
        if has_month {
            let m_num = ncx.to_number_value(&month_val.clone().unwrap())?;
            if m_num.is_infinite() {
                return Err(VmError::range_error("month property cannot be Infinity"));
            }
            let m_int = m_num as i32;
            if m_int != mc_month as i32 {
                return Err(VmError::range_error(format!(
                    "monthCode {} and month {} conflict",
                    mc, m_int
                )));
            }
        }
        mc_month as i32
    } else {
        // has_month only
        let n = ncx.to_number_value(&month_val.clone().unwrap())?;
        if n.is_infinite() {
            return Err(VmError::range_error("month property cannot be Infinity"));
        }
        if n.is_nan() {
            return Err(VmError::range_error("month property cannot be NaN"));
        }
        n as i32
    };

    // Per spec: PlainMonthDay.from({...}) ALWAYS uses 1972 as reference ISO year,
    // regardless of whether a year was provided. The year field is only used for
    // day-of-month validation (e.g., Feb 29 in a leap year).
    let reference_year = 1972;
    // Use validation year if provided, for day-of-month bounds checking
    let year_for_validation = validation_year.unwrap_or(reference_year);

    // Validate/constrain
    match overflow {
        Overflow::Reject => {
            if month < 1 || month > 12 {
                return Err(VmError::range_error(format!(
                    "month must be between 1 and 12, got {}",
                    month
                )));
            }
            let max_day = days_in_month(month as u32, year_for_validation);
            if day_raw < 1 || day_raw as u32 > max_day {
                return Err(VmError::range_error(format!(
                    "day must be between 1 and {}, got {}",
                    max_day, day_raw
                )));
            }
            create_plain_month_day_value(ncx, pmd_ctor_value, month, day_raw, reference_year)
        }
        Overflow::Constrain => {
            // Negative month or zero month always error even in constrain mode
            if month < 1 {
                return Err(VmError::range_error(format!(
                    "month must be >= 1, got {}",
                    month
                )));
            }
            let clamped_month = month.min(12);
            let max_day = days_in_month(clamped_month as u32, year_for_validation);
            // Negative day or zero day always error even in constrain mode
            if day_raw < 1 {
                return Err(VmError::range_error(format!(
                    "day must be >= 1, got {}",
                    day_raw
                )));
            }
            let clamped_day = (day_raw as u32).min(max_day) as i32;
            create_plain_month_day_value(
                ncx,
                pmd_ctor_value,
                clamped_month,
                clamped_day,
                reference_year,
            )
        }
    }
}

/// Create a new PlainMonthDay value by calling the constructor
fn create_plain_month_day_value(
    ncx: &mut NativeContext<'_>,
    ctor: &Value,
    month: i32,
    day: i32,
    reference_year: i32,
) -> Result<Value, VmError> {
    ncx.call_function_construct(
        ctor,
        Value::undefined(),
        &[
            Value::int32(month),
            Value::int32(day),
            Value::string(JsString::intern("iso8601")),
            Value::int32(reference_year),
        ],
    )
}

// ============================================================================
// PlainMonthDay prototype methods
// ============================================================================

fn install_plain_month_day_prototype(
    proto: GcRef<JsObject>,
    fn_proto: GcRef<JsObject>,
    mm: &Arc<MemoryManager>,
) {
    // .monthCode getter
    proto.define_property(
        PropertyKey::string("monthCode"),
        PropertyDescriptor::Accessor {
            get: Some(Value::native_function_with_proto(
                |this, _args, _ncx| {
                    let obj = this.as_object().ok_or_else(|| {
                        VmError::type_error("monthCode called on non-object")
                    })?;
                    let month = obj
                        .get(&PropertyKey::string(SLOT_ISO_MONTH))
                        .and_then(|v| v.as_int32())
                        .ok_or_else(|| {
                            VmError::type_error(
                                "monthCode called on non-PlainMonthDay",
                            )
                        })?;
                    Ok(Value::string(JsString::intern(&format_month_code(
                        month as u32,
                    ))))
                },
                mm.clone(),
                fn_proto.clone(),
            )),
            set: None,
            attributes: PropertyAttributes {
                writable: false,
                enumerable: false,
                configurable: true,
            },
        },
    );

    // .month getter — per spec, PlainMonthDay does NOT have a month property
    // Only monthCode is available. month should return undefined (no getter installed).

    // .day getter
    proto.define_property(
        PropertyKey::string("day"),
        PropertyDescriptor::Accessor {
            get: Some(Value::native_function_with_proto(
                |this, _args, _ncx| {
                    let obj = this.as_object().ok_or_else(|| {
                        VmError::type_error("day called on non-object")
                    })?;
                    let day = obj
                        .get(&PropertyKey::string(SLOT_ISO_DAY))
                        .and_then(|v| v.as_int32())
                        .ok_or_else(|| {
                            VmError::type_error("day called on non-PlainMonthDay")
                        })?;
                    Ok(Value::int32(day))
                },
                mm.clone(),
                fn_proto.clone(),
            )),
            set: None,
            attributes: PropertyAttributes {
                writable: false,
                enumerable: false,
                configurable: true,
            },
        },
    );

    // .calendarId getter — must check branding
    proto.define_property(
        PropertyKey::string("calendarId"),
        PropertyDescriptor::Accessor {
            get: Some(Value::native_function_with_proto(
                |this, _args, _ncx| {
                    let obj = this.as_object().ok_or_else(|| {
                        VmError::type_error("calendarId called on non-PlainMonthDay")
                    })?;
                    let ty = obj.get(&PropertyKey::string(SLOT_TEMPORAL_TYPE))
                        .and_then(|v| v.as_string().map(|s| s.as_str().to_string()));
                    if ty.as_deref() != Some("PlainMonthDay") {
                        return Err(VmError::type_error("calendarId called on non-PlainMonthDay"));
                    }
                    Ok(Value::string(JsString::intern("iso8601")))
                },
                mm.clone(),
                fn_proto.clone(),
            )),
            set: None,
            attributes: PropertyAttributes {
                writable: false,
                enumerable: false,
                configurable: true,
            },
        },
    );

    // .toString(options) method
    let to_string_fn = Value::native_function_with_proto_named(
        |this, args, ncx| {
            let obj = this
                .as_object()
                .ok_or_else(|| VmError::type_error("toString called on non-object"))?;

            // Branding check
            let ty = obj.get(&PropertyKey::string(SLOT_TEMPORAL_TYPE))
                .and_then(|v| v.as_string().map(|s| s.as_str().to_string()));
            if ty.as_deref() != Some("PlainMonthDay") {
                return Err(VmError::type_error("toString called on non-PlainMonthDay"));
            }

            let month = obj
                .get(&PropertyKey::string(SLOT_ISO_MONTH))
                .and_then(|v| v.as_int32())
                .unwrap_or(1);
            let day = obj
                .get(&PropertyKey::string(SLOT_ISO_DAY))
                .and_then(|v| v.as_int32())
                .unwrap_or(1);
            let year = obj
                .get(&PropertyKey::string(SLOT_ISO_YEAR))
                .and_then(|v| v.as_int32())
                .unwrap_or(1972);

            // Parse calendarName option from options argument
            let options_val = args.first().cloned().unwrap_or(Value::undefined());
            let calendar_name = if options_val.is_undefined() {
                "auto".to_string()
            } else {
                // GetOptionsObject: must be an object
                if options_val.is_null() || options_val.is_boolean() || options_val.is_number()
                    || options_val.is_string() || options_val.is_bigint() || options_val.as_symbol().is_some() {
                    return Err(VmError::type_error("options must be an object or undefined"));
                }
                // Handle Proxy first (as_object() returns None for proxies)
                if let Some(proxy) = options_val.as_proxy() {
                    let key = PropertyKey::string("calendarName");
                    let key_value = crate::proxy_operations::property_key_to_value_pub(&key);
                    let cn_val = crate::proxy_operations::proxy_get(ncx, proxy, &key, key_value, options_val.clone())?;
                    if cn_val.is_undefined() {
                        "auto".to_string()
                    } else {
                        let cn_str = ncx.to_string_value(&cn_val)?;
                        cn_str.as_str().to_string()
                    }
                } else if let Some(oo) = options_val.as_object() {
                    let cn_val = ncx.get_property(&oo, &PropertyKey::string("calendarName"))?;
                    if cn_val.is_undefined() {
                        "auto".to_string()
                    } else {
                        let cn_str = ncx.to_string_value(&cn_val)?;
                        cn_str.as_str().to_string()
                    }
                } else {
                    // Function or other callable — no calendarName property
                    "auto".to_string()
                }
            };

            // Validate calendarName option
            match calendar_name.as_str() {
                "auto" | "always" | "never" | "critical" => {}
                _ => return Err(VmError::range_error(format!("{} is not a valid value for calendarName", calendar_name))),
            }

            let result = match calendar_name.as_str() {
                "always" => format!("{:04}-{:02}-{:02}[u-ca=iso8601]", year, month, day),
                "critical" => format!("{:04}-{:02}-{:02}[!u-ca=iso8601]", year, month, day),
                "never" => format!("{:02}-{:02}", month, day),
                _ /* auto */ => format!("{:02}-{:02}", month, day),
            };

            Ok(Value::string(JsString::intern(&result)))
        },
        mm.clone(),
        fn_proto.clone(),
        "toString",
        0,
    );
    proto.define_property(
        PropertyKey::string("toString"),
        PropertyDescriptor::builtin_method(to_string_fn),
    );

    // .toJSON() method
    let to_json_fn = Value::native_function_with_proto_named(
        |this, _args, _ncx| {
            let obj = this
                .as_object()
                .ok_or_else(|| VmError::type_error("toJSON called on non-object"))?;

            let month = obj
                .get(&PropertyKey::string(SLOT_ISO_MONTH))
                .and_then(|v| v.as_int32())
                .ok_or_else(|| {
                    VmError::type_error("toJSON called on non-PlainMonthDay")
                })?;
            let day = obj
                .get(&PropertyKey::string(SLOT_ISO_DAY))
                .and_then(|v| v.as_int32())
                .ok_or_else(|| {
                    VmError::type_error("toJSON called on non-PlainMonthDay")
                })?;

            Ok(Value::string(JsString::intern(&format!(
                "{:02}-{:02}",
                month, day
            ))))
        },
        mm.clone(),
        fn_proto.clone(),
        "toJSON",
        0,
    );
    proto.define_property(
        PropertyKey::string("toJSON"),
        PropertyDescriptor::builtin_method(to_json_fn),
    );

    // .valueOf() - always throws TypeError per Temporal spec
    let value_of_fn = Value::native_function_with_proto_named(
        |_this, _args, _ncx| {
            Err(VmError::type_error(
                "use compare() or toString() to compare Temporal.PlainMonthDay",
            ))
        },
        mm.clone(),
        fn_proto.clone(),
        "valueOf",
        0,
    );
    proto.define_property(
        PropertyKey::string("valueOf"),
        PropertyDescriptor::builtin_method(value_of_fn),
    );

    // .equals(other) method — accepts PlainMonthDay, string, or property bag
    let equals_fn = Value::native_function_with_proto_named(
        |this, args, ncx| {
            let obj = this
                .as_object()
                .ok_or_else(|| VmError::type_error("equals called on non-object"))?;
            // Verify receiver is a PlainMonthDay
            let _ = obj
                .get(&PropertyKey::string(SLOT_ISO_MONTH))
                .and_then(|v| v.as_int32())
                .ok_or_else(|| {
                    VmError::type_error("equals called on non-PlainMonthDay")
                })?;

            let other_arg = args.first().cloned().unwrap_or(Value::undefined());

            // Resolve the other argument to a PlainMonthDay-like object
            let (m2, d2, y2) = resolve_plain_month_day_fields(ncx, &other_arg)?;

            let m1 = obj.get(&PropertyKey::string(SLOT_ISO_MONTH)).and_then(|v| v.as_int32()).unwrap();
            let d1 = obj.get(&PropertyKey::string(SLOT_ISO_DAY)).and_then(|v| v.as_int32()).unwrap();
            let y1 = obj.get(&PropertyKey::string(SLOT_ISO_YEAR)).and_then(|v| v.as_int32()).unwrap_or(1972);

            Ok(Value::boolean(m1 == m2 && d1 == d2 && y1 == y2))
        },
        mm.clone(),
        fn_proto.clone(),
        "equals",
        1,
    );
    proto.define_property(
        PropertyKey::string("equals"),
        PropertyDescriptor::builtin_method(equals_fn),
    );

    // .with(temporalMonthDayLike, options) method
    let with_fn = Value::native_function_with_proto_named(
        |this, args, ncx| {
            let obj = this
                .as_object()
                .ok_or_else(|| VmError::type_error("with called on non-object"))?;
            // Branding: verify it's a PlainMonthDay
            let ty = obj.get(&PropertyKey::string(SLOT_TEMPORAL_TYPE))
                .and_then(|v| v.as_string().map(|s| s.as_str().to_string()));
            if ty.as_deref() != Some("PlainMonthDay") {
                return Err(VmError::type_error("with called on non-PlainMonthDay"));
            }
            let cur_month = obj.get(&PropertyKey::string(SLOT_ISO_MONTH)).and_then(|v| v.as_int32()).unwrap_or(1);
            let cur_day = obj.get(&PropertyKey::string(SLOT_ISO_DAY)).and_then(|v| v.as_int32()).unwrap_or(1);
            let cur_year = obj.get(&PropertyKey::string(SLOT_ISO_YEAR)).and_then(|v| v.as_int32()).unwrap_or(1972);

            let item = args.first().cloned().unwrap_or(Value::undefined());
            // Argument must be an object (including Proxy)
            if item.is_undefined() || item.is_null() || item.is_boolean() || item.is_number()
                || item.is_string() || item.is_bigint() || item.as_symbol().is_some() {
                return Err(VmError::type_error("with argument must be an object"));
            }

            // Helper to get property from item (supports both Object and Proxy)
            let get_prop = |ncx: &mut NativeContext<'_>, item: &Value, key: &str| -> Result<Value, VmError> {
                if let Some(proxy) = item.as_proxy() {
                    proxy_get_property(ncx, proxy, item, key)
                } else if let Some(item_obj) = item.as_object() {
                    ncx.get_property(&item_obj, &PropertyKey::string(key))
                } else {
                    Ok(Value::undefined())
                }
            };

            // Reject if item is a Temporal type (PlainDate, PlainMonthDay, etc.)
            if let Some(item_obj) = item.as_object() {
                if let Some(item_ty) = item_obj.get(&PropertyKey::string(SLOT_TEMPORAL_TYPE))
                    .and_then(|v| v.as_string().map(|s| s.as_str().to_string())) {
                    if !item_ty.is_empty() {
                        return Err(VmError::type_error("with argument must be a partial object, not a Temporal type"));
                    }
                }
            }

            // Step 1: RejectObjectWithCalendarOrTimeZone — BEFORE field reads
            let cal_v = get_prop(ncx, &item, "calendar")?;
            if !cal_v.is_undefined() {
                return Err(VmError::type_error("calendar not allowed in PlainMonthDay.prototype.with"));
            }
            let tz_v = get_prop(ncx, &item, "timeZone")?;
            if !tz_v.is_undefined() {
                return Err(VmError::type_error("timeZone not allowed in PlainMonthDay.prototype.with"));
            }

            // Step 2: PrepareTemporalFields — get + IMMEDIATELY convert each field (alphabetical order)
            let day_v = get_prop(ncx, &item, "day")?;
            let has_day = !day_v.is_undefined();
            let day_num = if has_day {
                let n = ncx.to_number_value(&day_v)?;
                if n.is_infinite() { return Err(VmError::range_error("day property cannot be Infinity")); }
                Some(n as i32)
            } else { None };

            let month_v = get_prop(ncx, &item, "month")?;
            let has_month = !month_v.is_undefined();
            let month_num = if has_month {
                let n = ncx.to_number_value(&month_v)?;
                if n.is_infinite() { return Err(VmError::range_error("month property cannot be Infinity")); }
                Some(n as i32)
            } else { None };

            let month_code_v = get_prop(ncx, &item, "monthCode")?;
            let has_month_code = !month_code_v.is_undefined();
            let mc_str = if has_month_code {
                let mc = ncx.to_string_value(&month_code_v)?;
                Some(mc)
            } else { None };

            let year_v = get_prop(ncx, &item, "year")?;
            let has_year = !year_v.is_undefined();
            let year_num = if has_year {
                let n = ncx.to_number_value(&year_v)?;
                if n.is_infinite() { return Err(VmError::range_error("year property cannot be Infinity")); }
                Some(n as i32)
            } else { None };

            // Must have at least one known temporal field
            if !has_day && !has_month && !has_month_code && !has_year {
                return Err(VmError::type_error(
                    "with argument must have at least one recognized temporal property",
                ));
            }

            // Merge with current values
            let day = day_num.unwrap_or(cur_day);

            // CalendarResolveFields: reject below-minimum values BEFORE options
            if day < 1 { return Err(VmError::range_error(format!("day must be >= 1, got {}", day))); }
            if let Some(m) = month_num { if m < 1 { return Err(VmError::range_error(format!("month must be >= 1, got {}", m))); } }

            // Step 3: Read overflow from options — AFTER fields and basic below-min validation
            let options_val = args.get(1).cloned().unwrap_or(Value::undefined());
            let overflow = parse_overflow_option(ncx, &options_val)?;

            // monthCode validation AFTER options (per spec: options read before algorithmic validation)
            let month = if let Some(ref mc) = mc_str {
                validate_month_code_syntax(mc.as_str())?;
                let mc_month = validate_month_code_iso_suitability(mc.as_str())? as i32;
                if let Some(m) = month_num {
                    if m != mc_month {
                        return Err(VmError::range_error(format!("monthCode {} and month {} conflict", mc, m)));
                    }
                }
                mc_month
            } else if let Some(m) = month_num {
                m
            } else { cur_month };

            let year = year_num.unwrap_or(cur_year);

            // Build result using temporal_rs for validation with the user's year
            let ov = if overflow == Overflow::Reject { temporal_rs::options::Overflow::Reject } else { temporal_rs::options::Overflow::Constrain };
            if month < 0 || month > 255 { return Err(VmError::range_error(format!("month out of range: {}", month))); }
            if day < 0 || day > 255 { return Err(VmError::range_error(format!("day out of range: {}", day))); }
            // Validate with user's year to check day validity
            let pmd = temporal_rs::PlainMonthDay::new_with_overflow(
                month as u8, day as u8, temporal_rs::Calendar::default(), ov, Some(year),
            ).map_err(temporal_err)?;

            // Per spec, the result's reference year is always 1972 (ISO reference year)
            let ref_year = 1972;

            // Subclassing ignored — always use Temporal.PlainMonthDay constructor
            let temporal_ns = ncx.ctx.get_global("Temporal")
                .ok_or_else(|| VmError::type_error("Temporal namespace not found"))?;
            let temporal_obj = temporal_ns.as_object()
                .ok_or_else(|| VmError::type_error("Temporal namespace not found"))?;
            let pmd_ctor = temporal_obj.get(&PropertyKey::string("PlainMonthDay"))
                .ok_or_else(|| VmError::type_error("PlainMonthDay constructor not found"))?;
            create_plain_month_day_value(ncx, &pmd_ctor,
                pmd.month_code().to_month_integer() as i32,
                pmd.day() as i32,
                ref_year)
        },
        mm.clone(),
        fn_proto.clone(),
        "with",
        1,
    );
    proto.define_property(
        PropertyKey::string("with"),
        PropertyDescriptor::builtin_method(with_fn),
    );

    // .toPlainDate(yearLike) method
    let to_plain_date_fn = Value::native_function_with_proto_named(
        |this, args, ncx| {
            let obj = this
                .as_object()
                .ok_or_else(|| VmError::type_error("toPlainDate called on non-object"))?;
            // Branding check
            let ty = obj.get(&PropertyKey::string(SLOT_TEMPORAL_TYPE))
                .and_then(|v| v.as_string().map(|s| s.as_str().to_string()));
            if ty.as_deref() != Some("PlainMonthDay") {
                return Err(VmError::type_error("toPlainDate called on non-PlainMonthDay"));
            }
            let month = obj.get(&PropertyKey::string(SLOT_ISO_MONTH)).and_then(|v| v.as_int32()).unwrap_or(1);
            let day = obj.get(&PropertyKey::string(SLOT_ISO_DAY)).and_then(|v| v.as_int32()).unwrap_or(1);

            let year_like = args.first().cloned().unwrap_or(Value::undefined());
            if year_like.is_undefined() || year_like.is_null() || year_like.is_boolean()
                || year_like.is_number() || year_like.is_string() || year_like.is_bigint()
                || year_like.as_symbol().is_some() {
                return Err(VmError::type_error("toPlainDate requires an object argument with year"));
            }
            // Use observable get for year (supports both Object and Proxy)
            let year_val = if let Some(proxy) = year_like.as_proxy() {
                proxy_get_property(ncx, proxy, &year_like, "year")?
            } else if let Some(year_obj) = year_like.as_object() {
                ncx.get_property(&year_obj, &PropertyKey::string("year"))?
            } else {
                return Err(VmError::type_error("toPlainDate requires an object argument with year"));
            };
            if year_val.is_undefined() {
                return Err(VmError::type_error("year is required"));
            }
            let year_num = ncx.to_number_value(&year_val)?;
            if year_num.is_infinite() {
                return Err(VmError::range_error("year property cannot be Infinity"));
            }
            let year = year_num as i32;

            // Use temporal_rs with constrain overflow (spec default for toPlainDate)
            let pd = temporal_rs::PlainDate::new_with_overflow(
                year, month as u8, day as u8,
                temporal_rs::Calendar::default(),
                temporal_rs::options::Overflow::Constrain,
            ).map_err(temporal_err)?;

            // Create a PlainDate via Temporal.PlainDate constructor
            let temporal_ns = ncx.ctx.get_global("Temporal")
                .ok_or_else(|| VmError::type_error("Temporal namespace not found"))?;
            let temporal_obj = temporal_ns.as_object()
                .ok_or_else(|| VmError::type_error("Temporal namespace not found"))?;
            let pd_ctor = temporal_obj.get(&PropertyKey::string("PlainDate"))
                .ok_or_else(|| VmError::type_error("PlainDate constructor not found"))?;

            ncx.call_function_construct(
                &pd_ctor,
                Value::undefined(),
                &[Value::int32(pd.year()), Value::int32(pd.month() as i32), Value::int32(pd.day() as i32)],
            )
        },
        mm.clone(),
        fn_proto.clone(),
        "toPlainDate",
        1,
    );
    proto.define_property(
        PropertyKey::string("toPlainDate"),
        PropertyDescriptor::builtin_method(to_plain_date_fn),
    );

    // .toLocaleString() method
    let to_locale_string_fn = Value::native_function_with_proto_named(
        |this, _args, _ncx| {
            let obj = this
                .as_object()
                .ok_or_else(|| VmError::type_error("toLocaleString called on non-object"))?;
            let month = obj
                .get(&PropertyKey::string(SLOT_ISO_MONTH))
                .and_then(|v| v.as_int32())
                .ok_or_else(|| VmError::type_error("toLocaleString called on non-PlainMonthDay"))?;
            let day = obj
                .get(&PropertyKey::string(SLOT_ISO_DAY))
                .and_then(|v| v.as_int32())
                .ok_or_else(|| VmError::type_error("toLocaleString called on non-PlainMonthDay"))?;
            Ok(Value::string(JsString::intern(&format!("{:02}-{:02}", month, day))))
        },
        mm.clone(),
        fn_proto.clone(),
        "toLocaleString",
        0,
    );
    proto.define_property(
        PropertyKey::string("toLocaleString"),
        PropertyDescriptor::builtin_method(to_locale_string_fn),
    );

    // @@toStringTag
    proto.define_property(
        PropertyKey::Symbol(crate::intrinsics::well_known::to_string_tag_symbol()),
        PropertyDescriptor::data_with_attrs(
            Value::string(JsString::intern("Temporal.PlainMonthDay")),
            PropertyAttributes {
                writable: false,
                enumerable: false,
                configurable: true,
            },
        ),
    );
}

// ============================================================================
// PlainDate prototype methods
// ============================================================================

fn install_plain_date_prototype(
    proto: GcRef<JsObject>,
    fn_proto: GcRef<JsObject>,
    mm: &Arc<MemoryManager>,
) {
    // Helper macro-like for creating slot accessor getters
    let make_slot_getter = |slot: &'static str, name: &'static str, mm: &Arc<MemoryManager>, fn_proto: &GcRef<JsObject>| -> Value {
        Value::native_function_with_proto(
            move |this, _args, _ncx| {
                let obj = this.as_object().ok_or_else(|| {
                    VmError::type_error(&format!("{} called on non-object", name))
                })?;
                obj.get(&PropertyKey::string(slot))
                    .filter(|v| !v.is_undefined())
                    .ok_or_else(|| VmError::type_error(&format!("{} called on non-PlainDate", name)))
            },
            mm.clone(),
            fn_proto.clone(),
        )
    };

    // year, month, day getters
    for (slot, name) in &[
        (SLOT_ISO_YEAR, "year"),
        (SLOT_ISO_MONTH, "month"),
        (SLOT_ISO_DAY, "day"),
    ] {
        proto.define_property(
            PropertyKey::string(name),
            PropertyDescriptor::Accessor {
                get: Some(make_slot_getter(slot, name, mm, &fn_proto)),
                set: None,
                attributes: PropertyAttributes {
                    writable: false,
                    enumerable: false,
                    configurable: true,
                },
            },
        );
    }

    // monthCode getter
    proto.define_property(
        PropertyKey::string("monthCode"),
        PropertyDescriptor::Accessor {
            get: Some(Value::native_function_with_proto(
                |this, _args, _ncx| {
                    let obj = this.as_object().ok_or_else(|| {
                        VmError::type_error("monthCode called on non-object")
                    })?;
                    let month = obj.get(&PropertyKey::string(SLOT_ISO_MONTH))
                        .and_then(|v| v.as_int32())
                        .ok_or_else(|| VmError::type_error("monthCode called on non-PlainDate"))?;
                    Ok(Value::string(JsString::intern(&format_month_code(month as u32))))
                },
                mm.clone(),
                fn_proto.clone(),
            )),
            set: None,
            attributes: PropertyAttributes { writable: false, enumerable: false, configurable: true },
        },
    );

    // calendarId getter
    proto.define_property(
        PropertyKey::string("calendarId"),
        PropertyDescriptor::Accessor {
            get: Some(Value::native_function_with_proto(
                |this, _args, _ncx| {
                    // Branding: check it's a PlainDate
                    let obj = this.as_object().ok_or_else(|| {
                        VmError::type_error("calendarId called on non-object")
                    })?;
                    let _ = obj.get(&PropertyKey::string(SLOT_ISO_YEAR))
                        .and_then(|v| v.as_int32())
                        .ok_or_else(|| VmError::type_error("calendarId called on non-PlainDate"))?;
                    Ok(Value::string(JsString::intern("iso8601")))
                },
                mm.clone(),
                fn_proto.clone(),
            )),
            set: None,
            attributes: PropertyAttributes { writable: false, enumerable: false, configurable: true },
        },
    );

    // era getter — always undefined for ISO calendar
    proto.define_property(
        PropertyKey::string("era"),
        PropertyDescriptor::Accessor {
            get: Some(Value::native_function_with_proto(
                |this, _args, _ncx| {
                    let obj = this.as_object().ok_or_else(|| {
                        VmError::type_error("era called on non-object")
                    })?;
                    let _ = obj.get(&PropertyKey::string(SLOT_ISO_YEAR))
                        .and_then(|v| v.as_int32())
                        .ok_or_else(|| VmError::type_error("era called on non-PlainDate"))?;
                    Ok(Value::undefined())
                },
                mm.clone(),
                fn_proto.clone(),
            )),
            set: None,
            attributes: PropertyAttributes { writable: false, enumerable: false, configurable: true },
        },
    );

    // eraYear getter — always undefined for ISO calendar
    proto.define_property(
        PropertyKey::string("eraYear"),
        PropertyDescriptor::Accessor {
            get: Some(Value::native_function_with_proto(
                |this, _args, _ncx| {
                    let obj = this.as_object().ok_or_else(|| {
                        VmError::type_error("eraYear called on non-object")
                    })?;
                    let _ = obj.get(&PropertyKey::string(SLOT_ISO_YEAR))
                        .and_then(|v| v.as_int32())
                        .ok_or_else(|| VmError::type_error("eraYear called on non-PlainDate"))?;
                    Ok(Value::undefined())
                },
                mm.clone(),
                fn_proto.clone(),
            )),
            set: None,
            attributes: PropertyAttributes { writable: false, enumerable: false, configurable: true },
        },
    );

    // dayOfWeek, dayOfYear, weekOfYear, yearOfWeek, daysInWeek, daysInMonth, daysInYear, monthsInYear, inLeapYear
    proto.define_property(
        PropertyKey::string("dayOfWeek"),
        PropertyDescriptor::Accessor {
            get: Some(Value::native_function_with_proto(
                |this, _args, _ncx| {
                    let obj = this.as_object().ok_or_else(|| VmError::type_error("dayOfWeek called on non-object"))?;
                    let y = obj.get(&PropertyKey::string(SLOT_ISO_YEAR)).and_then(|v| v.as_int32())
                        .ok_or_else(|| VmError::type_error("dayOfWeek called on non-PlainDate"))?;
                    let m = obj.get(&PropertyKey::string(SLOT_ISO_MONTH)).and_then(|v| v.as_int32()).unwrap_or(1);
                    let d = obj.get(&PropertyKey::string(SLOT_ISO_DAY)).and_then(|v| v.as_int32()).unwrap_or(1);
                    // Zeller-like formula for ISO day of week (Monday=1, Sunday=7)
                    let dow = iso_day_of_week(y, m, d);
                    Ok(Value::int32(dow))
                },
                mm.clone(),
                fn_proto.clone(),
            )),
            set: None,
            attributes: PropertyAttributes { writable: false, enumerable: false, configurable: true },
        },
    );

    proto.define_property(
        PropertyKey::string("dayOfYear"),
        PropertyDescriptor::Accessor {
            get: Some(Value::native_function_with_proto(
                |this, _args, _ncx| {
                    let obj = this.as_object().ok_or_else(|| VmError::type_error("dayOfYear called on non-object"))?;
                    let y = obj.get(&PropertyKey::string(SLOT_ISO_YEAR)).and_then(|v| v.as_int32())
                        .ok_or_else(|| VmError::type_error("dayOfYear called on non-PlainDate"))?;
                    let m = obj.get(&PropertyKey::string(SLOT_ISO_MONTH)).and_then(|v| v.as_int32()).unwrap_or(1);
                    let d = obj.get(&PropertyKey::string(SLOT_ISO_DAY)).and_then(|v| v.as_int32()).unwrap_or(1);
                    let doy = iso_day_of_year(y, m, d);
                    Ok(Value::int32(doy))
                },
                mm.clone(),
                fn_proto.clone(),
            )),
            set: None,
            attributes: PropertyAttributes { writable: false, enumerable: false, configurable: true },
        },
    );

    proto.define_property(
        PropertyKey::string("daysInMonth"),
        PropertyDescriptor::Accessor {
            get: Some(Value::native_function_with_proto(
                |this, _args, _ncx| {
                    let obj = this.as_object().ok_or_else(|| VmError::type_error("daysInMonth"))?;
                    let y = obj.get(&PropertyKey::string(SLOT_ISO_YEAR)).and_then(|v| v.as_int32()).ok_or_else(|| VmError::type_error("daysInMonth"))?;
                    let m = obj.get(&PropertyKey::string(SLOT_ISO_MONTH)).and_then(|v| v.as_int32()).unwrap_or(1);
                    Ok(Value::int32(days_in_month(m as u32, y) as i32))
                },
                mm.clone(),
                fn_proto.clone(),
            )),
            set: None,
            attributes: PropertyAttributes { writable: false, enumerable: false, configurable: true },
        },
    );

    proto.define_property(
        PropertyKey::string("daysInYear"),
        PropertyDescriptor::Accessor {
            get: Some(Value::native_function_with_proto(
                |this, _args, _ncx| {
                    let obj = this.as_object().ok_or_else(|| VmError::type_error("daysInYear"))?;
                    let y = obj.get(&PropertyKey::string(SLOT_ISO_YEAR)).and_then(|v| v.as_int32()).ok_or_else(|| VmError::type_error("daysInYear"))?;
                    Ok(Value::int32(if is_leap_year(y) { 366 } else { 365 }))
                },
                mm.clone(),
                fn_proto.clone(),
            )),
            set: None,
            attributes: PropertyAttributes { writable: false, enumerable: false, configurable: true },
        },
    );

    proto.define_property(
        PropertyKey::string("monthsInYear"),
        PropertyDescriptor::Accessor {
            get: Some(Value::native_function_with_proto(
                |this, _args, _ncx| {
                    let obj = this.as_object().ok_or_else(|| VmError::type_error("monthsInYear"))?;
                    let _ = obj.get(&PropertyKey::string(SLOT_ISO_YEAR)).and_then(|v| v.as_int32()).ok_or_else(|| VmError::type_error("monthsInYear"))?;
                    Ok(Value::int32(12))
                },
                mm.clone(),
                fn_proto.clone(),
            )),
            set: None,
            attributes: PropertyAttributes { writable: false, enumerable: false, configurable: true },
        },
    );

    proto.define_property(
        PropertyKey::string("inLeapYear"),
        PropertyDescriptor::Accessor {
            get: Some(Value::native_function_with_proto(
                |this, _args, _ncx| {
                    let obj = this.as_object().ok_or_else(|| VmError::type_error("inLeapYear"))?;
                    let y = obj.get(&PropertyKey::string(SLOT_ISO_YEAR)).and_then(|v| v.as_int32()).ok_or_else(|| VmError::type_error("inLeapYear"))?;
                    Ok(Value::boolean(is_leap_year(y)))
                },
                mm.clone(),
                fn_proto.clone(),
            )),
            set: None,
            attributes: PropertyAttributes { writable: false, enumerable: false, configurable: true },
        },
    );

    proto.define_property(
        PropertyKey::string("daysInWeek"),
        PropertyDescriptor::Accessor {
            get: Some(Value::native_function_with_proto(
                |this, _args, _ncx| {
                    let obj = this.as_object().ok_or_else(|| VmError::type_error("daysInWeek"))?;
                    let _ = obj.get(&PropertyKey::string(SLOT_ISO_YEAR)).and_then(|v| v.as_int32()).ok_or_else(|| VmError::type_error("daysInWeek"))?;
                    Ok(Value::int32(7))
                },
                mm.clone(),
                fn_proto.clone(),
            )),
            set: None,
            attributes: PropertyAttributes { writable: false, enumerable: false, configurable: true },
        },
    );

    // toString()
    let to_string_fn = Value::native_function_with_proto_named(
        |this, _args, _ncx| {
            let obj = this.as_object().ok_or_else(|| VmError::type_error("toString on non-PlainDate"))?;
            let y = obj.get(&PropertyKey::string(SLOT_ISO_YEAR)).and_then(|v| v.as_int32()).ok_or_else(|| VmError::type_error("toString"))?;
            let m = obj.get(&PropertyKey::string(SLOT_ISO_MONTH)).and_then(|v| v.as_int32()).unwrap_or(1);
            let d = obj.get(&PropertyKey::string(SLOT_ISO_DAY)).and_then(|v| v.as_int32()).unwrap_or(1);
            if y < 0 || y > 9999 {
                Ok(Value::string(JsString::intern(&format!("{:+07}-{:02}-{:02}", y, m, d))))
            } else {
                Ok(Value::string(JsString::intern(&format!("{:04}-{:02}-{:02}", y, m, d))))
            }
        },
        mm.clone(),
        fn_proto.clone(),
        "toString",
        0,
    );
    proto.define_property(
        PropertyKey::string("toString"),
        PropertyDescriptor::builtin_method(to_string_fn),
    );

    // toJSON()
    let to_json_fn = Value::native_function_with_proto_named(
        |this, _args, _ncx| {
            let obj = this.as_object().ok_or_else(|| VmError::type_error("toJSON"))?;
            let y = obj.get(&PropertyKey::string(SLOT_ISO_YEAR)).and_then(|v| v.as_int32()).ok_or_else(|| VmError::type_error("toJSON"))?;
            let m = obj.get(&PropertyKey::string(SLOT_ISO_MONTH)).and_then(|v| v.as_int32()).unwrap_or(1);
            let d = obj.get(&PropertyKey::string(SLOT_ISO_DAY)).and_then(|v| v.as_int32()).unwrap_or(1);
            Ok(Value::string(JsString::intern(&format!("{:04}-{:02}-{:02}", y, m, d))))
        },
        mm.clone(),
        fn_proto.clone(),
        "toJSON",
        0,
    );
    proto.define_property(
        PropertyKey::string("toJSON"),
        PropertyDescriptor::builtin_method(to_json_fn),
    );

    // valueOf() — always throws
    let value_of_fn = Value::native_function_with_proto_named(
        |_this, _args, _ncx| {
            Err(VmError::type_error("use compare() or toString() to compare Temporal.PlainDate"))
        },
        mm.clone(),
        fn_proto.clone(),
        "valueOf",
        0,
    );
    proto.define_property(
        PropertyKey::string("valueOf"),
        PropertyDescriptor::builtin_method(value_of_fn),
    );

    // toLocaleString()
    let to_locale_string_fn = Value::native_function_with_proto_named(
        |this, _args, _ncx| {
            let obj = this.as_object().ok_or_else(|| VmError::type_error("toLocaleString"))?;
            let y = obj.get(&PropertyKey::string(SLOT_ISO_YEAR)).and_then(|v| v.as_int32()).ok_or_else(|| VmError::type_error("toLocaleString"))?;
            let m = obj.get(&PropertyKey::string(SLOT_ISO_MONTH)).and_then(|v| v.as_int32()).unwrap_or(1);
            let d = obj.get(&PropertyKey::string(SLOT_ISO_DAY)).and_then(|v| v.as_int32()).unwrap_or(1);
            Ok(Value::string(JsString::intern(&format!("{:04}-{:02}-{:02}", y, m, d))))
        },
        mm.clone(),
        fn_proto.clone(),
        "toLocaleString",
        0,
    );
    proto.define_property(
        PropertyKey::string("toLocaleString"),
        PropertyDescriptor::builtin_method(to_locale_string_fn),
    );

    // @@toStringTag
    proto.define_property(
        PropertyKey::Symbol(crate::intrinsics::well_known::to_string_tag_symbol()),
        PropertyDescriptor::data_with_attrs(
            Value::string(JsString::intern("Temporal.PlainDate")),
            PropertyAttributes { writable: false, enumerable: false, configurable: true },
        ),
    );
}

/// ISO day of week (Monday=1, Sunday=7)
fn iso_day_of_week(year: i32, month: i32, day: i32) -> i32 {
    // Tomohiko Sakamoto's algorithm
    let t = [0, 3, 2, 5, 0, 3, 5, 1, 4, 6, 2, 4];
    let mut y = year;
    if month < 3 { y -= 1; }
    let dow = (y + y / 4 - y / 100 + y / 400 + t[(month - 1) as usize] + day) % 7;
    // Convert 0=Sunday to ISO: Monday=1...Sunday=7
    if dow == 0 { 7 } else { dow }
}

/// Day of year for ISO calendar
fn iso_day_of_year(year: i32, month: i32, day: i32) -> i32 {
    let mut doy = day;
    for m in 1..month {
        doy += days_in_month(m as u32, year) as i32;
    }
    doy
}

// ============================================================================
// PlainDateTime prototype and constructor helpers
// ============================================================================

const SLOT_ISO_HOUR: &str = "__iso_hour";
const SLOT_ISO_MINUTE: &str = "__iso_minute";
const SLOT_ISO_SECOND: &str = "__iso_second";
const SLOT_ISO_MILLISECOND: &str = "__iso_millisecond";
const SLOT_ISO_MICROSECOND: &str = "__iso_microsecond";
const SLOT_ISO_NANOSECOND: &str = "__iso_nanosecond";

fn install_plain_date_time_prototype(
    proto: GcRef<JsObject>,
    fn_proto: GcRef<JsObject>,
    mm: &Arc<MemoryManager>,
) {
    // Getter with branding check: ensures SLOT_TEMPORAL_TYPE == "PlainDateTime"
    let make_branding_getter = |slot: &'static str, name: &'static str, mm: &Arc<MemoryManager>, fn_proto: &GcRef<JsObject>| -> Value {
        Value::native_function_with_proto(
            move |this, _args, _ncx| {
                let obj = this.as_object().ok_or_else(|| {
                    VmError::type_error(&format!("{} called on non-object", name))
                })?;
                // Branding check
                let ty = obj.get(&PropertyKey::string(SLOT_TEMPORAL_TYPE))
                    .and_then(|v| v.as_string().map(|s| s.as_str().to_string()));
                if ty.as_deref() != Some("PlainDateTime") {
                    return Err(VmError::type_error(&format!("{} called on non-PlainDateTime", name)));
                }
                obj.get(&PropertyKey::string(slot))
                    .filter(|v| !v.is_undefined())
                    .ok_or_else(|| VmError::type_error(&format!("{} called on non-PlainDateTime", name)))
            },
            mm.clone(),
            fn_proto.clone(),
        )
    };

    // Time getter with branding: checks brand, defaults to 0 if time slot missing
    let make_time_getter = |slot: &'static str, name: &'static str, mm: &Arc<MemoryManager>, fn_proto: &GcRef<JsObject>| -> Value {
        Value::native_function_with_proto(
            move |this, _args, _ncx| {
                let obj = this.as_object().ok_or_else(|| {
                    VmError::type_error(&format!("{} called on non-object", name))
                })?;
                // Branding check
                let ty = obj.get(&PropertyKey::string(SLOT_TEMPORAL_TYPE))
                    .and_then(|v| v.as_string().map(|s| s.as_str().to_string()));
                if ty.as_deref() != Some("PlainDateTime") {
                    return Err(VmError::type_error(&format!("{} called on non-PlainDateTime", name)));
                }
                Ok(obj.get(&PropertyKey::string(slot))
                    .filter(|v| !v.is_undefined())
                    .unwrap_or(Value::int32(0)))
            },
            mm.clone(),
            fn_proto.clone(),
        )
    };

    // Date getters (branded)
    for (slot, name) in &[
        (SLOT_ISO_YEAR, "year"),
        (SLOT_ISO_MONTH, "month"),
        (SLOT_ISO_DAY, "day"),
    ] {
        proto.define_property(
            PropertyKey::string(name),
            PropertyDescriptor::Accessor {
                get: Some(make_branding_getter(slot, name, mm, &fn_proto)),
                set: None,
                attributes: PropertyAttributes { writable: false, enumerable: false, configurable: true },
            },
        );
    }

    // Time getters (branded, default to 0)
    for (slot, name) in &[
        (SLOT_ISO_HOUR, "hour"),
        (SLOT_ISO_MINUTE, "minute"),
        (SLOT_ISO_SECOND, "second"),
        (SLOT_ISO_MILLISECOND, "millisecond"),
        (SLOT_ISO_MICROSECOND, "microsecond"),
        (SLOT_ISO_NANOSECOND, "nanosecond"),
    ] {
        proto.define_property(
            PropertyKey::string(name),
            PropertyDescriptor::Accessor {
                get: Some(make_time_getter(slot, name, mm, &fn_proto)),
                set: None,
                attributes: PropertyAttributes { writable: false, enumerable: false, configurable: true },
            },
        );
    }

    // monthCode
    proto.define_property(
        PropertyKey::string("monthCode"),
        PropertyDescriptor::Accessor {
            get: Some(Value::native_function_with_proto(
                |this, _args, _ncx| {
                    let obj = this.as_object().ok_or_else(|| VmError::type_error("monthCode"))?;
                    let m = obj.get(&PropertyKey::string(SLOT_ISO_MONTH)).and_then(|v| v.as_int32())
                        .ok_or_else(|| VmError::type_error("monthCode on non-PlainDateTime"))?;
                    Ok(Value::string(JsString::intern(&format_month_code(m as u32))))
                },
                mm.clone(),
                fn_proto.clone(),
            )),
            set: None,
            attributes: PropertyAttributes { writable: false, enumerable: false, configurable: true },
        },
    );

    // calendarId
    proto.define_property(
        PropertyKey::string("calendarId"),
        PropertyDescriptor::Accessor {
            get: Some(Value::native_function_with_proto(
                |this, _args, _ncx| {
                    let obj = this.as_object().ok_or_else(|| VmError::type_error("calendarId"))?;
                    let _ = obj.get(&PropertyKey::string(SLOT_ISO_YEAR)).and_then(|v| v.as_int32())
                        .ok_or_else(|| VmError::type_error("calendarId on non-PlainDateTime"))?;
                    Ok(Value::string(JsString::intern("iso8601")))
                },
                mm.clone(),
                fn_proto.clone(),
            )),
            set: None,
            attributes: PropertyAttributes { writable: false, enumerable: false, configurable: true },
        },
    );

    // era, eraYear — undefined for ISO
    for name in &["era", "eraYear"] {
        let n = *name;
        proto.define_property(
            PropertyKey::string(n),
            PropertyDescriptor::Accessor {
                get: Some(Value::native_function_with_proto(
                    move |this, _args, _ncx| {
                        let obj = this.as_object().ok_or_else(|| VmError::type_error(n))?;
                        let _ = obj.get(&PropertyKey::string(SLOT_ISO_YEAR)).and_then(|v| v.as_int32())
                            .ok_or_else(|| VmError::type_error(&format!("{} on non-PlainDateTime", n)))?;
                        Ok(Value::undefined())
                    },
                    mm.clone(),
                    fn_proto.clone(),
                )),
                set: None,
                attributes: PropertyAttributes { writable: false, enumerable: false, configurable: true },
            },
        );
    }

    // dayOfWeek, dayOfYear, daysInMonth, daysInYear, daysInWeek, monthsInYear, inLeapYear
    proto.define_property(
        PropertyKey::string("dayOfWeek"),
        PropertyDescriptor::Accessor {
            get: Some(Value::native_function_with_proto(
                |this, _args, _ncx| {
                    let obj = this.as_object().ok_or_else(|| VmError::type_error("dayOfWeek"))?;
                    let y = obj.get(&PropertyKey::string(SLOT_ISO_YEAR)).and_then(|v| v.as_int32()).ok_or_else(|| VmError::type_error("dayOfWeek"))?;
                    let m = obj.get(&PropertyKey::string(SLOT_ISO_MONTH)).and_then(|v| v.as_int32()).unwrap_or(1);
                    let d = obj.get(&PropertyKey::string(SLOT_ISO_DAY)).and_then(|v| v.as_int32()).unwrap_or(1);
                    Ok(Value::int32(iso_day_of_week(y, m, d)))
                },
                mm.clone(), fn_proto.clone(),
            )),
            set: None,
            attributes: PropertyAttributes { writable: false, enumerable: false, configurable: true },
        },
    );

    proto.define_property(
        PropertyKey::string("dayOfYear"),
        PropertyDescriptor::Accessor {
            get: Some(Value::native_function_with_proto(
                |this, _args, _ncx| {
                    let obj = this.as_object().ok_or_else(|| VmError::type_error("dayOfYear"))?;
                    let y = obj.get(&PropertyKey::string(SLOT_ISO_YEAR)).and_then(|v| v.as_int32()).ok_or_else(|| VmError::type_error("dayOfYear"))?;
                    let m = obj.get(&PropertyKey::string(SLOT_ISO_MONTH)).and_then(|v| v.as_int32()).unwrap_or(1);
                    let d = obj.get(&PropertyKey::string(SLOT_ISO_DAY)).and_then(|v| v.as_int32()).unwrap_or(1);
                    Ok(Value::int32(iso_day_of_year(y, m, d)))
                },
                mm.clone(), fn_proto.clone(),
            )),
            set: None,
            attributes: PropertyAttributes { writable: false, enumerable: false, configurable: true },
        },
    );

    for (name, val_fn) in &[
        ("daysInWeek", 7i32),
        ("monthsInYear", 12),
    ] {
        let v = *val_fn;
        proto.define_property(
            PropertyKey::string(name),
            PropertyDescriptor::Accessor {
                get: Some(Value::native_function_with_proto(
                    move |this, _args, _ncx| {
                        let obj = this.as_object().ok_or_else(|| VmError::type_error("getter"))?;
                        let _ = obj.get(&PropertyKey::string(SLOT_ISO_YEAR)).and_then(|vl| vl.as_int32()).ok_or_else(|| VmError::type_error("getter"))?;
                        Ok(Value::int32(v))
                    },
                    mm.clone(), fn_proto.clone(),
                )),
                set: None,
                attributes: PropertyAttributes { writable: false, enumerable: false, configurable: true },
            },
        );
    }

    proto.define_property(
        PropertyKey::string("daysInMonth"),
        PropertyDescriptor::Accessor {
            get: Some(Value::native_function_with_proto(
                |this, _args, _ncx| {
                    let obj = this.as_object().ok_or_else(|| VmError::type_error("daysInMonth"))?;
                    let y = obj.get(&PropertyKey::string(SLOT_ISO_YEAR)).and_then(|v| v.as_int32()).ok_or_else(|| VmError::type_error("daysInMonth"))?;
                    let m = obj.get(&PropertyKey::string(SLOT_ISO_MONTH)).and_then(|v| v.as_int32()).unwrap_or(1);
                    Ok(Value::int32(days_in_month(m as u32, y) as i32))
                },
                mm.clone(), fn_proto.clone(),
            )),
            set: None,
            attributes: PropertyAttributes { writable: false, enumerable: false, configurable: true },
        },
    );

    proto.define_property(
        PropertyKey::string("daysInYear"),
        PropertyDescriptor::Accessor {
            get: Some(Value::native_function_with_proto(
                |this, _args, _ncx| {
                    let obj = this.as_object().ok_or_else(|| VmError::type_error("daysInYear"))?;
                    let y = obj.get(&PropertyKey::string(SLOT_ISO_YEAR)).and_then(|v| v.as_int32()).ok_or_else(|| VmError::type_error("daysInYear"))?;
                    Ok(Value::int32(if is_leap_year(y) { 366 } else { 365 }))
                },
                mm.clone(), fn_proto.clone(),
            )),
            set: None,
            attributes: PropertyAttributes { writable: false, enumerable: false, configurable: true },
        },
    );

    proto.define_property(
        PropertyKey::string("inLeapYear"),
        PropertyDescriptor::Accessor {
            get: Some(Value::native_function_with_proto(
                |this, _args, _ncx| {
                    let obj = this.as_object().ok_or_else(|| VmError::type_error("inLeapYear"))?;
                    let y = obj.get(&PropertyKey::string(SLOT_ISO_YEAR)).and_then(|v| v.as_int32()).ok_or_else(|| VmError::type_error("inLeapYear"))?;
                    Ok(Value::boolean(is_leap_year(y)))
                },
                mm.clone(), fn_proto.clone(),
            )),
            set: None,
            attributes: PropertyAttributes { writable: false, enumerable: false, configurable: true },
        },
    );

    // toString
    let to_string_fn = Value::native_function_with_proto_named(
        |this, _args, _ncx| {
            let obj = this.as_object().ok_or_else(|| VmError::type_error("toString"))?;
            let y = obj.get(&PropertyKey::string(SLOT_ISO_YEAR)).and_then(|v| v.as_int32()).ok_or_else(|| VmError::type_error("toString"))?;
            let mo = obj.get(&PropertyKey::string(SLOT_ISO_MONTH)).and_then(|v| v.as_int32()).unwrap_or(1);
            let d = obj.get(&PropertyKey::string(SLOT_ISO_DAY)).and_then(|v| v.as_int32()).unwrap_or(1);
            let h = obj.get(&PropertyKey::string(SLOT_ISO_HOUR)).and_then(|v| v.as_int32()).unwrap_or(0);
            let mi = obj.get(&PropertyKey::string(SLOT_ISO_MINUTE)).and_then(|v| v.as_int32()).unwrap_or(0);
            let s = obj.get(&PropertyKey::string(SLOT_ISO_SECOND)).and_then(|v| v.as_int32()).unwrap_or(0);
            let ms = obj.get(&PropertyKey::string(SLOT_ISO_MILLISECOND)).and_then(|v| v.as_int32()).unwrap_or(0);
            let us = obj.get(&PropertyKey::string(SLOT_ISO_MICROSECOND)).and_then(|v| v.as_int32()).unwrap_or(0);
            let ns = obj.get(&PropertyKey::string(SLOT_ISO_NANOSECOND)).and_then(|v| v.as_int32()).unwrap_or(0);

            let date_part = if y < 0 || y > 9999 {
                format!("{:+07}-{:02}-{:02}", y, mo, d)
            } else {
                format!("{:04}-{:02}-{:02}", y, mo, d)
            };

            let sub = ns + us * 1000 + ms * 1_000_000;
            let time_part = if sub != 0 {
                let frac = format!("{:09}", sub);
                let trimmed = frac.trim_end_matches('0');
                format!("T{:02}:{:02}:{:02}.{}", h, mi, s, trimmed)
            } else if s != 0 {
                format!("T{:02}:{:02}:{:02}", h, mi, s)
            } else if mi != 0 || h != 0 {
                format!("T{:02}:{:02}:{:02}", h, mi, s)
            } else {
                "T00:00:00".to_string()
            };

            Ok(Value::string(JsString::intern(&format!("{}{}", date_part, time_part))))
        },
        mm.clone(),
        fn_proto.clone(),
        "toString",
        0,
    );
    proto.define_property(PropertyKey::string("toString"), PropertyDescriptor::builtin_method(to_string_fn));

    // toJSON
    let to_json_fn = Value::native_function_with_proto_named(
        |this, _args, ncx| {
            let obj = this.as_object().ok_or_else(|| VmError::type_error("toJSON called on non-PlainDateTime"))?;
            // Branding check
            let ty = obj.get(&PropertyKey::string(SLOT_TEMPORAL_TYPE))
                .and_then(|v| v.as_string().map(|s| s.as_str().to_string()));
            if ty.as_deref() != Some("PlainDateTime") {
                return Err(VmError::type_error("toJSON called on non-PlainDateTime"));
            }
            // Delegate to toString
            if let Some(ts) = obj.get(&PropertyKey::string("toString")) {
                return ncx.call_function(&ts, this.clone(), &[]);
            }
            Err(VmError::type_error("toJSON called on non-PlainDateTime"))
        },
        mm.clone(), fn_proto.clone(), "toJSON", 0,
    );
    proto.define_property(PropertyKey::string("toJSON"), PropertyDescriptor::builtin_method(to_json_fn));

    // valueOf — throws
    let value_of_fn = Value::native_function_with_proto_named(
        |_this, _args, _ncx| {
            Err(VmError::type_error("use compare() or toString() to compare Temporal.PlainDateTime"))
        },
        mm.clone(), fn_proto.clone(), "valueOf", 0,
    );
    proto.define_property(PropertyKey::string("valueOf"), PropertyDescriptor::builtin_method(value_of_fn));

    // toLocaleString
    let to_locale_string_fn = Value::native_function_with_proto_named(
        |this, args, ncx| {
            if let Some(obj) = this.as_object() {
                if let Some(ts) = obj.get(&PropertyKey::string("toString")) {
                    return ncx.call_function(&ts, this.clone(), &[]);
                }
            }
            Err(VmError::type_error("toLocaleString"))
        },
        mm.clone(), fn_proto.clone(), "toLocaleString", 0,
    );
    proto.define_property(PropertyKey::string("toLocaleString"), PropertyDescriptor::builtin_method(to_locale_string_fn));

    // toPlainDate
    let to_plain_date_fn = Value::native_function_with_proto_named(
        |this, _args, ncx| {
            let obj = this.as_object().ok_or_else(|| VmError::type_error("toPlainDate"))?;
            let y = obj.get(&PropertyKey::string(SLOT_ISO_YEAR)).and_then(|v| v.as_int32()).ok_or_else(|| VmError::type_error("toPlainDate"))?;
            let m = obj.get(&PropertyKey::string(SLOT_ISO_MONTH)).and_then(|v| v.as_int32()).unwrap_or(1);
            let d = obj.get(&PropertyKey::string(SLOT_ISO_DAY)).and_then(|v| v.as_int32()).unwrap_or(1);
            let temporal_ns = ncx.ctx.get_global("Temporal")
                .ok_or_else(|| VmError::type_error("Temporal not found"))?;
            let temporal_obj = temporal_ns.as_object()
                .ok_or_else(|| VmError::type_error("Temporal not found"))?;
            let pd_ctor = temporal_obj.get(&PropertyKey::string("PlainDate")).ok_or_else(|| VmError::type_error("PlainDate not found"))?;
            ncx.call_function_construct(&pd_ctor, Value::undefined(), &[Value::int32(y), Value::int32(m), Value::int32(d)])
        },
        mm.clone(), fn_proto.clone(), "toPlainDate", 0,
    );
    proto.define_property(PropertyKey::string("toPlainDate"), PropertyDescriptor::builtin_method(to_plain_date_fn));

    // with — PlainDateTime.prototype.with(temporalDateTimeLike [, options])
    let with_fn = Value::native_function_with_proto_named(
        |this, args, ncx| {
            let obj = this.as_object().ok_or_else(|| VmError::type_error("with called on non-PlainDateTime"))?;
            // Branding check
            let ty = obj.get(&PropertyKey::string(SLOT_TEMPORAL_TYPE))
                .and_then(|v| v.as_string().map(|s| s.as_str().to_string()));
            if ty.as_deref() != Some("PlainDateTime") {
                return Err(VmError::type_error("with called on non-PlainDateTime"));
            }

            // Get current values
            let cur_y = obj.get(&PropertyKey::string(SLOT_ISO_YEAR)).and_then(|v| v.as_int32()).unwrap_or(0);
            let cur_m = obj.get(&PropertyKey::string(SLOT_ISO_MONTH)).and_then(|v| v.as_int32()).unwrap_or(1);
            let cur_d = obj.get(&PropertyKey::string(SLOT_ISO_DAY)).and_then(|v| v.as_int32()).unwrap_or(1);
            let cur_h = obj.get(&PropertyKey::string(SLOT_ISO_HOUR)).and_then(|v| v.as_int32()).unwrap_or(0);
            let cur_mi = obj.get(&PropertyKey::string(SLOT_ISO_MINUTE)).and_then(|v| v.as_int32()).unwrap_or(0);
            let cur_s = obj.get(&PropertyKey::string(SLOT_ISO_SECOND)).and_then(|v| v.as_int32()).unwrap_or(0);
            let cur_ms = obj.get(&PropertyKey::string(SLOT_ISO_MILLISECOND)).and_then(|v| v.as_int32()).unwrap_or(0);
            let cur_us = obj.get(&PropertyKey::string(SLOT_ISO_MICROSECOND)).and_then(|v| v.as_int32()).unwrap_or(0);
            let cur_ns = obj.get(&PropertyKey::string(SLOT_ISO_NANOSECOND)).and_then(|v| v.as_int32()).unwrap_or(0);

            let item = args.first().cloned().unwrap_or(Value::undefined());

            // Helper: get property from object or proxy
            let get_prop = |ncx: &mut NativeContext<'_>, item: &Value, name: &str| -> Result<Value, VmError> {
                if let Some(proxy) = item.as_proxy() {
                    let key = PropertyKey::string(name);
                    let key_value = crate::proxy_operations::property_key_to_value_pub(&key);
                    crate::proxy_operations::proxy_get(ncx, proxy, &key, key_value, item.clone())
                } else if let Some(obj) = item.as_object() {
                    ncx.get_property(&obj, &PropertyKey::string(name))
                } else {
                    Err(VmError::type_error("with argument must be an object"))
                }
            };

            // Argument must be an object (including Proxy)
            if item.as_object().is_none() && item.as_proxy().is_none() {
                return Err(VmError::type_error("with argument must be an object"));
            }

            // Reject if item is a known Temporal type
            if let Some(item_obj) = item.as_object() {
                if let Some(item_ty) = item_obj.get(&PropertyKey::string(SLOT_TEMPORAL_TYPE))
                    .and_then(|v| v.as_string().map(|s| s.as_str().to_string())) {
                    if !item_ty.is_empty() {
                        return Err(VmError::type_error("with argument must be a partial object, not a Temporal type"));
                    }
                }
            }

            // Step 1: RejectObjectWithCalendarOrTimeZone — BEFORE field reads
            let cal_v = get_prop(ncx, &item, "calendar")?;
            if !cal_v.is_undefined() {
                return Err(VmError::type_error("calendar not allowed in with argument"));
            }
            let tz_v = get_prop(ncx, &item, "timeZone")?;
            if !tz_v.is_undefined() {
                return Err(VmError::type_error("timeZone not allowed in with argument"));
            }

            // Step 2: PrepareTemporalFields — get + IMMEDIATELY convert each field (alphabetical)
            // Each field: get → immediately convert via valueOf/toString
            let day_raw = get_prop(ncx, &item, "day")?;
            let day = if !day_raw.is_undefined() {
                let n = ncx.to_number_value(&day_raw)?;
                if n.is_infinite() { return Err(VmError::range_error("day property cannot be Infinity")); }
                n as i32
            } else { cur_d };

            let hour_raw = get_prop(ncx, &item, "hour")?;
            let hour = if !hour_raw.is_undefined() {
                let n = ncx.to_number_value(&hour_raw)?;
                if n.is_infinite() { return Err(VmError::range_error("hour property cannot be Infinity")); }
                n as i32
            } else { cur_h };

            let microsecond_raw = get_prop(ncx, &item, "microsecond")?;
            let microsecond = if !microsecond_raw.is_undefined() {
                let n = ncx.to_number_value(&microsecond_raw)?;
                if n.is_infinite() { return Err(VmError::range_error("microsecond property cannot be Infinity")); }
                n as i32
            } else { cur_us };

            let millisecond_raw = get_prop(ncx, &item, "millisecond")?;
            let millisecond = if !millisecond_raw.is_undefined() {
                let n = ncx.to_number_value(&millisecond_raw)?;
                if n.is_infinite() { return Err(VmError::range_error("millisecond property cannot be Infinity")); }
                n as i32
            } else { cur_ms };

            let minute_raw = get_prop(ncx, &item, "minute")?;
            let minute = if !minute_raw.is_undefined() {
                let n = ncx.to_number_value(&minute_raw)?;
                if n.is_infinite() { return Err(VmError::range_error("minute property cannot be Infinity")); }
                n as i32
            } else { cur_mi };

            let month_raw = get_prop(ncx, &item, "month")?;
            let month_n = if !month_raw.is_undefined() {
                let n = ncx.to_number_value(&month_raw)?;
                if n.is_infinite() { return Err(VmError::range_error("month property cannot be Infinity")); }
                Some(n as i32)
            } else { None };

            let month_code_raw = get_prop(ncx, &item, "monthCode")?;
            // Only read and convert monthCode here; validation happens AFTER options
            let mc_str = if !month_code_raw.is_undefined() {
                Some(ncx.to_string_value(&month_code_raw)?)
            } else { None };
            // Temporary month for basic below-min validation (monthCode validation deferred)
            let month_pre = if let Some(mn) = month_n { mn } else { cur_m };

            let nanosecond_raw = get_prop(ncx, &item, "nanosecond")?;
            let nanosecond = if !nanosecond_raw.is_undefined() {
                let n = ncx.to_number_value(&nanosecond_raw)?;
                if n.is_infinite() { return Err(VmError::range_error("nanosecond property cannot be Infinity")); }
                n as i32
            } else { cur_ns };

            let second_raw = get_prop(ncx, &item, "second")?;
            let second = if !second_raw.is_undefined() {
                let n = ncx.to_number_value(&second_raw)?;
                if n.is_infinite() { return Err(VmError::range_error("second property cannot be Infinity")); }
                n as i32
            } else { cur_s };

            let year_raw = get_prop(ncx, &item, "year")?;
            let year = if !year_raw.is_undefined() {
                let n = ncx.to_number_value(&year_raw)?;
                if n.is_infinite() { return Err(VmError::range_error("year property cannot be Infinity")); }
                n as i32
            } else { cur_y };

            // Check at least one field is defined
            let has_any = [&day_raw, &hour_raw, &microsecond_raw, &millisecond_raw, &minute_raw,
                &month_raw, &month_code_raw, &nanosecond_raw, &second_raw, &year_raw]
                .iter().any(|v| !v.is_undefined());
            if !has_any {
                return Err(VmError::type_error("with argument must have at least one recognized temporal property"));
            }

            // CalendarResolveFields: reject below-minimum values BEFORE options
            // (above-maximum values are handled by overflow constrain/reject after options)
            if month_pre < 1 { return Err(VmError::range_error(format!("month must be >= 1, got {}", month_pre))); }
            if day < 1 { return Err(VmError::range_error(format!("day must be >= 1, got {}", day))); }
            if hour < 0 { return Err(VmError::range_error(format!("hour must be >= 0, got {}", hour))); }
            if minute < 0 { return Err(VmError::range_error(format!("minute must be >= 0, got {}", minute))); }
            if second < 0 { return Err(VmError::range_error(format!("second must be >= 0, got {}", second))); }
            if millisecond < 0 { return Err(VmError::range_error(format!("millisecond must be >= 0, got {}", millisecond))); }
            if microsecond < 0 { return Err(VmError::range_error(format!("microsecond must be >= 0, got {}", microsecond))); }
            if nanosecond < 0 { return Err(VmError::range_error(format!("nanosecond must be >= 0, got {}", nanosecond))); }

            // Step 3: Read options — AFTER field reads and basic validation, BEFORE monthCode validation
            let options_val = args.get(1).cloned().unwrap_or(Value::undefined());
            let overflow = parse_overflow_option(ncx, &options_val)?;

            // Resolve month from monthCode AFTER options (per spec: options read before algorithmic validation)
            let month = if let Some(ref mc) = mc_str {
                validate_month_code_syntax(mc.as_str())?;
                let mc_month = validate_month_code_iso_suitability(mc.as_str())? as i32;
                if let Some(mn) = month_n {
                    if mn != mc_month {
                        return Err(VmError::range_error("month and monthCode must agree"));
                    }
                }
                mc_month
            } else { month_pre };

            // Use temporal_rs for full validation including calendar-specific checks
            let ov = if overflow == Overflow::Reject { temporal_rs::options::Overflow::Reject } else { temporal_rs::options::Overflow::Constrain };
            let pdt = temporal_rs::PlainDateTime::new_with_overflow(
                year, month as u8, day as u8,
                hour.clamp(0, 255) as u8, minute.clamp(0, 255) as u8, second.clamp(0, 255) as u8,
                millisecond.clamp(0, 65535) as u16, microsecond.clamp(0, 65535) as u16, nanosecond.clamp(0, 65535) as u16,
                temporal_rs::Calendar::default(), ov,
            ).map_err(temporal_err)?;

            // Subclassing ignored — always use Temporal.PlainDateTime constructor prototype
            let temporal_ns = ncx.ctx.get_global("Temporal")
                .ok_or_else(|| VmError::type_error("Temporal namespace not found"))?;
            let temporal_obj_ns = temporal_ns.as_object()
                .ok_or_else(|| VmError::type_error("Temporal namespace not found"))?;
            let pdt_ctor = temporal_obj_ns.get(&PropertyKey::string("PlainDateTime"))
                .ok_or_else(|| VmError::type_error("PlainDateTime constructor not found"))?;
            let pdt_ctor_obj = pdt_ctor.as_object()
                .ok_or_else(|| VmError::type_error("PlainDateTime is not a function"))?;
            let pdt_proto = pdt_ctor_obj.get(&PropertyKey::string("prototype"))
                .unwrap_or(Value::undefined());

            let result_obj = GcRef::new(JsObject::new(
                pdt_proto,
                ncx.ctx.memory_manager().clone(),
            ));
            result_obj.define_property(PropertyKey::string(SLOT_TEMPORAL_TYPE), PropertyDescriptor::data(Value::string(JsString::intern("PlainDateTime"))));
            result_obj.define_property(PropertyKey::string(SLOT_ISO_YEAR), PropertyDescriptor::data(Value::int32(pdt.year())));
            result_obj.define_property(PropertyKey::string(SLOT_ISO_MONTH), PropertyDescriptor::data(Value::int32(pdt.month() as i32)));
            result_obj.define_property(PropertyKey::string(SLOT_ISO_DAY), PropertyDescriptor::data(Value::int32(pdt.day() as i32)));
            result_obj.define_property(PropertyKey::string(SLOT_ISO_HOUR), PropertyDescriptor::data(Value::int32(pdt.hour() as i32)));
            result_obj.define_property(PropertyKey::string(SLOT_ISO_MINUTE), PropertyDescriptor::data(Value::int32(pdt.minute() as i32)));
            result_obj.define_property(PropertyKey::string(SLOT_ISO_SECOND), PropertyDescriptor::data(Value::int32(pdt.second() as i32)));
            result_obj.define_property(PropertyKey::string(SLOT_ISO_MILLISECOND), PropertyDescriptor::data(Value::int32(pdt.millisecond() as i32)));
            result_obj.define_property(PropertyKey::string(SLOT_ISO_MICROSECOND), PropertyDescriptor::data(Value::int32(pdt.microsecond() as i32)));
            result_obj.define_property(PropertyKey::string(SLOT_ISO_NANOSECOND), PropertyDescriptor::data(Value::int32(pdt.nanosecond() as i32)));
            Ok(Value::object(result_obj))
        },
        mm.clone(), fn_proto.clone(), "with", 1,
    );
    proto.define_property(PropertyKey::string("with"), PropertyDescriptor::builtin_method(with_fn));

    // toZonedDateTime — implementation for UTC and fixed-offset timezones
    let to_zoned_fn = Value::native_function_with_proto_named(
        |this, args, ncx| {
            let obj = this.as_object().ok_or_else(|| VmError::type_error("toZonedDateTime called on non-PlainDateTime"))?;
            // Branding check
            let ty = obj.get(&PropertyKey::string(SLOT_TEMPORAL_TYPE))
                .and_then(|v| v.as_string().map(|s| s.as_str().to_string()));
            if ty.as_deref() != Some("PlainDateTime") {
                return Err(VmError::type_error("toZonedDateTime called on non-PlainDateTime"));
            }

            // Get timeZone argument
            let tz_arg = args.first().cloned().unwrap_or(Value::undefined());

            // Type check: primitives → TypeError (except string)
            if tz_arg.is_undefined() || tz_arg.is_null() || tz_arg.is_boolean()
                || tz_arg.is_number() || tz_arg.is_bigint() {
                return Err(VmError::type_error(format!(
                    "{} is not a valid time zone",
                    if tz_arg.is_null() { "null" } else if tz_arg.is_undefined() { "undefined" } else { tz_arg.type_of() }
                )));
            }
            if tz_arg.as_symbol().is_some() {
                return Err(VmError::type_error("Cannot convert a Symbol value to a string"));
            }
            // Objects (non-string) → TypeError
            if tz_arg.as_object().is_some() || tz_arg.as_proxy().is_some() {
                return Err(VmError::type_error("object is not a valid time zone"));
            }

            let tz_str = ncx.to_string_value(&tz_arg)?;
            let tz_s = tz_str.as_str();

            // If it's an empty string, throw RangeError
            if tz_s.is_empty() {
                return Err(VmError::range_error("time zone string must not be empty"));
            }

            // Use temporal_rs for spec-compliant timezone parsing
            // This handles: "UTC", "+01:00", "2021-08-19T17:30Z", "2021-08-19T17:30-07:00[+01:46]", etc.
            let tz = temporal_rs::TimeZone::try_from_str(tz_s).map_err(temporal_err)?;
            let tz_identifier = tz.identifier().map_err(temporal_err)?;

            // Read disambiguation option
            let options_val = args.get(1).cloned().unwrap_or(Value::undefined());
            let disambiguation_str = if !options_val.is_undefined() {
                if options_val.is_null() || options_val.is_boolean() || options_val.is_number()
                    || options_val.is_string() || options_val.is_bigint() || options_val.as_symbol().is_some() {
                    return Err(VmError::type_error("options must be an object or undefined"));
                }
                if let Some(opts_obj) = options_val.as_object() {
                    let dis_val = ncx.get_property(&opts_obj, &PropertyKey::string("disambiguation"))?;
                    if !dis_val.is_undefined() {
                        let dis_str = ncx.to_string_value(&dis_val)?;
                        match dis_str.as_str() {
                            "compatible" | "earlier" | "later" | "reject" => dis_str.as_str().to_string(),
                            _ => return Err(VmError::range_error(format!("{} is not a valid value for disambiguation", dis_str))),
                        }
                    } else { "compatible".to_string() }
                } else if let Some(proxy) = options_val.as_proxy() {
                    let key = PropertyKey::string("disambiguation");
                    let key_value = crate::proxy_operations::property_key_to_value_pub(&key);
                    let dis_val = crate::proxy_operations::proxy_get(ncx, proxy, &key, key_value, options_val.clone())?;
                    if !dis_val.is_undefined() {
                        let dis_str = ncx.to_string_value(&dis_val)?;
                        match dis_str.as_str() {
                            "compatible" | "earlier" | "later" | "reject" => dis_str.as_str().to_string(),
                            _ => return Err(VmError::range_error(format!("{} is not a valid value for disambiguation", dis_str))),
                        }
                    } else { "compatible".to_string() }
                } else { "compatible".to_string() }
            } else { "compatible".to_string() };

            // Get the PlainDateTime components
            let y = obj.get(&PropertyKey::string(SLOT_ISO_YEAR)).and_then(|v| v.as_int32()).unwrap_or(0);
            let mo = obj.get(&PropertyKey::string(SLOT_ISO_MONTH)).and_then(|v| v.as_int32()).unwrap_or(1);
            let d = obj.get(&PropertyKey::string(SLOT_ISO_DAY)).and_then(|v| v.as_int32()).unwrap_or(1);
            let h = obj.get(&PropertyKey::string(SLOT_ISO_HOUR)).and_then(|v| v.as_int32()).unwrap_or(0);
            let mi = obj.get(&PropertyKey::string(SLOT_ISO_MINUTE)).and_then(|v| v.as_int32()).unwrap_or(0);
            let s = obj.get(&PropertyKey::string(SLOT_ISO_SECOND)).and_then(|v| v.as_int32()).unwrap_or(0);
            let ms_val = obj.get(&PropertyKey::string(SLOT_ISO_MILLISECOND)).and_then(|v| v.as_int32()).unwrap_or(0);
            let us_val = obj.get(&PropertyKey::string(SLOT_ISO_MICROSECOND)).and_then(|v| v.as_int32()).unwrap_or(0);
            let ns_val = obj.get(&PropertyKey::string(SLOT_ISO_NANOSECOND)).and_then(|v| v.as_int32()).unwrap_or(0);

            // Compute epoch nanoseconds from ISO date/time components
            // Epoch is 1970-01-01T00:00:00Z
            let days_from_epoch = iso_date_to_epoch_days(y, mo, d);
            let time_ns = (h as i128) * 3_600_000_000_000
                + (mi as i128) * 60_000_000_000
                + (s as i128) * 1_000_000_000
                + (ms_val as i128) * 1_000_000
                + (us_val as i128) * 1_000
                + (ns_val as i128);
            let local_epoch_ns = (days_from_epoch as i128) * 86_400_000_000_000 + time_ns;

            // Parse offset from timezone identifier
            let offset_ns = parse_tz_offset_ns(&tz_identifier)?;

            // For fixed-offset/UTC timezones, epoch_ns = local_epoch_ns - offset_ns
            let epoch_ns = local_epoch_ns - offset_ns;

            // Validate Instant range: ±10^8 days = ±8.64 × 10^21 nanoseconds
            let max_instant_ns: i128 = 8_640_000_000_000_000_000_000;
            if epoch_ns < -max_instant_ns || epoch_ns > max_instant_ns {
                return Err(VmError::range_error("resulting Instant is outside the allowed range"));
            }

            let epoch_ns_str = epoch_ns.to_string();

            // Create a ZonedDateTime object with proper prototype
            let temporal_ns_val = ncx.ctx.get_global("Temporal")
                .ok_or_else(|| VmError::type_error("Temporal namespace not found"))?;
            let temporal_obj = temporal_ns_val.as_object()
                .ok_or_else(|| VmError::type_error("Temporal namespace not found"))?;

            let zdt_proto = temporal_obj.get(&PropertyKey::string("ZonedDateTime"))
                .and_then(|ctor| ctor.as_object())
                .and_then(|ctor_obj| ctor_obj.get(&PropertyKey::string("prototype")))
                .unwrap_or(Value::undefined());

            let result = GcRef::new(JsObject::new(zdt_proto, ncx.ctx.memory_manager().clone()));
            result.define_property(PropertyKey::string(SLOT_TEMPORAL_TYPE),
                PropertyDescriptor::builtin_data(Value::string(JsString::intern("ZonedDateTime"))));
            result.define_property(PropertyKey::string("epochNanoseconds"),
                PropertyDescriptor::data(Value::bigint(epoch_ns_str.clone())));
            result.define_property(PropertyKey::string("calendarId"),
                PropertyDescriptor::data(Value::string(JsString::intern("iso8601"))));
            result.define_property(PropertyKey::string("timeZoneId"),
                PropertyDescriptor::data(Value::string(JsString::intern(&tz_identifier))));
            // Store the offset for this timezone
            result.define_property(PropertyKey::string("__tz_offset_ns__"),
                PropertyDescriptor::builtin_data(Value::string(JsString::intern(&offset_ns.to_string()))));
            Ok(Value::object(result))
        },
        mm.clone(), fn_proto.clone(), "toZonedDateTime", 1,
    );
    proto.define_property(PropertyKey::string("toZonedDateTime"), PropertyDescriptor::builtin_method(to_zoned_fn));

    // .equals(other) method
    let equals_fn = Value::native_function_with_proto_named(
        |this, args, ncx| {
            let obj = this.as_object().ok_or_else(|| VmError::type_error("equals called on non-object"))?;
            let ty = obj.get(&PropertyKey::string(SLOT_TEMPORAL_TYPE))
                .and_then(|v| v.as_string().map(|s| s.as_str().to_string()));
            if ty.as_deref() != Some("PlainDateTime") {
                return Err(VmError::type_error("equals called on non-PlainDateTime"));
            }
            let this_pdt = extract_pdt(&obj)?;

            let other = args.first().cloned().unwrap_or(Value::undefined());
            let other_pdt = to_temporal_datetime(ncx, &other)?;

            Ok(Value::boolean(this_pdt.compare_iso(&other_pdt) == std::cmp::Ordering::Equal))
        },
        mm.clone(), fn_proto.clone(), "equals", 1,
    );
    proto.define_property(PropertyKey::string("equals"), PropertyDescriptor::builtin_method(equals_fn));

    // Helper: extract temporal_rs::PlainDateTime from a JsObject with ISO slots
    fn extract_pdt(obj: &GcRef<JsObject>) -> Result<temporal_rs::PlainDateTime, VmError> {
        let y = obj.get(&PropertyKey::string(SLOT_ISO_YEAR)).and_then(|v| v.as_int32()).unwrap_or(0);
        let mo = obj.get(&PropertyKey::string(SLOT_ISO_MONTH)).and_then(|v| v.as_int32()).unwrap_or(1) as u8;
        let d = obj.get(&PropertyKey::string(SLOT_ISO_DAY)).and_then(|v| v.as_int32()).unwrap_or(1) as u8;
        let h = obj.get(&PropertyKey::string(SLOT_ISO_HOUR)).and_then(|v| v.as_int32()).unwrap_or(0) as u8;
        let mi = obj.get(&PropertyKey::string(SLOT_ISO_MINUTE)).and_then(|v| v.as_int32()).unwrap_or(0) as u8;
        let sec = obj.get(&PropertyKey::string(SLOT_ISO_SECOND)).and_then(|v| v.as_int32()).unwrap_or(0) as u8;
        let ms = obj.get(&PropertyKey::string(SLOT_ISO_MILLISECOND)).and_then(|v| v.as_int32()).unwrap_or(0) as u16;
        let us = obj.get(&PropertyKey::string(SLOT_ISO_MICROSECOND)).and_then(|v| v.as_int32()).unwrap_or(0) as u16;
        let ns = obj.get(&PropertyKey::string(SLOT_ISO_NANOSECOND)).and_then(|v| v.as_int32()).unwrap_or(0) as u16;
        temporal_rs::PlainDateTime::try_new(y, mo, d, h, mi, sec, ms, us, ns, temporal_rs::Calendar::default())
            .map_err(temporal_err)
    }

    // Helper: read a property from a Value (handles both JsObject and Proxy)
    fn get_val_property(ncx: &mut NativeContext<'_>, val: &Value, key: &str) -> Result<Value, VmError> {
        if let Some(obj) = val.as_object() {
            return ncx.get_property(&obj, &PropertyKey::string(key));
        }
        if let Some(proxy) = val.as_proxy() {
            return proxy_get_property(ncx, proxy, val, key);
        }
        Ok(Value::undefined())
    }

    // Helper: parse unit string to temporal_rs::options::Unit
    fn parse_temporal_unit(s: &str) -> Result<temporal_rs::options::Unit, VmError> {
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

    // Helper: parse rounding mode string to temporal_rs::options::RoundingMode
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
            _ => Err(VmError::range_error(format!("{} is not a valid rounding mode", s))),
        }
    }

    // Helper: validate calendar argument (ToTemporalCalendarIdentifier)
    fn validate_calendar_arg(ncx: &mut NativeContext<'_>, cal: &Value) -> Result<String, VmError> {
        if cal.is_undefined() {
            return Ok("iso8601".to_string());
        }
        // Symbol → TypeError
        if cal.as_symbol().is_some() {
            return Err(VmError::type_error("Cannot convert a Symbol value to a string"));
        }
        // Temporal objects with calendar → use internal calendar
        // Only PlainDate, PlainDateTime, PlainMonthDay, PlainYearMonth, ZonedDateTime have calendars
        // Duration and Instant do NOT have calendars → TypeError
        if let Some(obj) = cal.as_object() {
            let tt = obj.get(&PropertyKey::string(SLOT_TEMPORAL_TYPE))
                .and_then(|v| v.as_string().map(|s| s.as_str().to_string()));
            match tt.as_deref() {
                Some("PlainDate") | Some("PlainDateTime") | Some("PlainMonthDay") |
                Some("PlainYearMonth") | Some("ZonedDateTime") => {
                    return Ok("iso8601".to_string());
                }
                Some("Duration") | Some("Instant") => {
                    return Err(VmError::type_error(format!("{} instance is not a valid calendar", tt.unwrap())));
                }
                _ => {}
            }
        }
        // Non-string types → TypeError (per ToTemporalCalendarIdentifier)
        if !cal.is_string() {
            if cal.is_null() || cal.is_boolean() || cal.is_number() || cal.is_bigint() || cal.as_object().is_some() {
                return Err(VmError::type_error(format!("{} is not a valid calendar", ncx.to_string_value(cal).unwrap_or_default())));
            }
            return Err(VmError::type_error("calendar must be a string"));
        }
        let s = cal.as_string().unwrap().as_str().to_string();
        if s.is_empty() {
            return Err(VmError::range_error("empty string is not a valid calendar ID"));
        }
        // Validate calendar string: must be "iso8601" or a valid ISO string
        let lower = s.to_ascii_lowercase();
        if lower == "iso8601" {
            return Ok("iso8601".to_string());
        }
        // Try to parse as ISO date/datetime/time string
        if s.chars().any(|c| c.is_ascii_digit()) {
            if s.starts_with("-000000") || s.contains("-000000") {
                return Err(VmError::range_error("reject minus zero as extended year"));
            }
            if temporal_rs::PlainDateTime::from_utf8(s.as_bytes()).is_ok() {
                return Ok("iso8601".to_string());
            }
            if temporal_rs::PlainDate::from_utf8(s.as_bytes()).is_ok() {
                return Ok("iso8601".to_string());
            }
            if temporal_rs::PlainTime::from_utf8(s.as_bytes()).is_ok() {
                return Ok("iso8601".to_string());
            }
            if temporal_rs::PlainMonthDay::from_utf8(s.as_bytes()).is_ok() {
                return Ok("iso8601".to_string());
            }
            if temporal_rs::PlainYearMonth::from_utf8(s.as_bytes()).is_ok() {
                return Ok("iso8601".to_string());
            }
            return Err(VmError::range_error(format!("{} is not a valid calendar ID", s)));
        }
        Err(VmError::range_error(format!("{} is not a valid calendar ID", s)))
    }

    // Helper: read and validate a numeric temporal field, checking Infinity
    fn read_temporal_number(ncx: &mut NativeContext<'_>, val: &Value, field: &str) -> Result<f64, VmError> {
        let n = ncx.to_number_value(val)?;
        if n.is_infinite() {
            return Err(VmError::range_error(format!("{} property cannot be Infinity", field)));
        }
        Ok(n)
    }

    // Helper: ToTemporalDateTime — convert a Value to a temporal_rs::PlainDateTime
    // Handles: PlainDateTime objects, ZonedDateTime objects, property bags {year, month, day}, strings
    fn to_temporal_datetime(ncx: &mut NativeContext<'_>, item: &Value) -> Result<temporal_rs::PlainDateTime, VmError> {
        if item.is_string() {
            let s = ncx.to_string_value(item)?;
            reject_utc_designator_for_plain(s.as_str())?;
            return temporal_rs::PlainDateTime::from_utf8(s.as_bytes()).map_err(temporal_err);
        }

        if item.is_undefined() || item.is_null() || item.is_boolean() || item.is_number() || item.is_bigint() {
            return Err(VmError::type_error(format!("cannot convert {} to a PlainDateTime", item.type_of())));
        }

        if item.as_symbol().is_some() {
            return Err(VmError::type_error("Cannot convert a Symbol value to a string"));
        }

        // Handle both JsObject and Proxy
        if item.as_object().is_none() && item.as_proxy().is_none() {
            return Err(VmError::type_error("Expected an object or string"));
        }

        // Check if it's a Temporal type (only on real objects, not proxies)
        if let Some(obj) = item.as_object() {
            let temporal_type = obj.get(&PropertyKey::string(SLOT_TEMPORAL_TYPE))
                .and_then(|v| v.as_string().map(|s| s.as_str().to_string()));

            if temporal_type.as_deref() == Some("PlainDateTime") {
                return extract_pdt(&obj);
            }

            if temporal_type.as_deref() == Some("PlainDate") {
                let y = obj.get(&PropertyKey::string(SLOT_ISO_YEAR)).and_then(|v| v.as_int32()).unwrap_or(0);
                let mo = obj.get(&PropertyKey::string(SLOT_ISO_MONTH)).and_then(|v| v.as_int32()).unwrap_or(1);
                let d = obj.get(&PropertyKey::string(SLOT_ISO_DAY)).and_then(|v| v.as_int32()).unwrap_or(1);
                return temporal_rs::PlainDateTime::try_new(
                    y, mo as u8, d as u8, 0, 0, 0, 0, 0, 0,
                    temporal_rs::Calendar::default(),
                ).map_err(temporal_err);
            }

            if temporal_type.as_deref() == Some("ZonedDateTime") {
                let epoch_ns_val = obj.get(&PropertyKey::string("epochNanoseconds")).unwrap_or(Value::int32(0));
                let tz_id_val = obj.get(&PropertyKey::string("timeZoneId")).unwrap_or(Value::string(JsString::intern("UTC")));
                let tz_id = if let Some(s) = tz_id_val.as_string() { s.as_str().to_string() } else { "UTC".to_string() };

                let epoch_ns: i128 = if epoch_ns_val.is_bigint() {
                    let s = ncx.to_string_value(&epoch_ns_val)?;
                    let s = s.trim_end_matches('n');
                    s.parse::<i128>().unwrap_or(0)
                } else if let Some(n) = epoch_ns_val.as_number() { n as i128 } else { 0 };

                let offset_ns: i128 = parse_timezone_offset_ns(&tz_id);
                let wall_ns = epoch_ns + offset_ns;

                let ns_per_ms: i128 = 1_000_000;
                let ms_per_s: i128 = 1_000;

                let epoch_ms = wall_ns.div_euclid(ns_per_ms);
                let remainder_ns = wall_ns.rem_euclid(ns_per_ms);
                let us_part = (remainder_ns / 1000) as u16;
                let ns_part = (remainder_ns % 1000) as u16;

                let epoch_secs = epoch_ms.div_euclid(ms_per_s);
                let ms_rem = epoch_ms.rem_euclid(ms_per_s) as u16;

                let ndt = chrono::DateTime::from_timestamp(epoch_secs as i64, (ms_rem as u32) * 1_000_000)
                    .unwrap_or_else(|| chrono::DateTime::from_timestamp(0, 0).unwrap())
                    .naive_utc();

                return temporal_rs::PlainDateTime::try_new(
                    ndt.year(), ndt.month() as u8, ndt.day() as u8,
                    ndt.hour() as u8, ndt.minute() as u8, ndt.second() as u8,
                    ms_rem, us_part, ns_part,
                    temporal_rs::Calendar::default(),
                ).map_err(temporal_err);
            }
        }

        // Property bag — read AND convert each property in ALPHABETICAL order per spec
        // Order: calendar, day, hour, microsecond, millisecond, minute, month, monthCode, nanosecond, second, year
        // Each property is read then immediately converted (get → valueOf/toString) per spec observable ordering
        let calendar_val = get_val_property(ncx, item, "calendar")?;
        if !calendar_val.is_undefined() {
            validate_calendar_arg(ncx, &calendar_val)?;
        }

        // day — read and convert immediately
        let day_val = get_val_property(ncx, item, "day")?;
        let d = if !day_val.is_undefined() { read_temporal_number(ncx, &day_val, "day")? as i32 } else { -1 }; // -1 = missing

        // hour
        let hour_val = get_val_property(ncx, item, "hour")?;
        let h = if !hour_val.is_undefined() { read_temporal_number(ncx, &hour_val, "hour")? as i32 } else { 0 };

        // microsecond
        let us_val = get_val_property(ncx, item, "microsecond")?;
        let us = if !us_val.is_undefined() { read_temporal_number(ncx, &us_val, "microsecond")? as i32 } else { 0 };

        // millisecond
        let ms_val = get_val_property(ncx, item, "millisecond")?;
        let ms = if !ms_val.is_undefined() { read_temporal_number(ncx, &ms_val, "millisecond")? as i32 } else { 0 };

        // minute
        let minute_val = get_val_property(ncx, item, "minute")?;
        let mi = if !minute_val.is_undefined() { read_temporal_number(ncx, &minute_val, "minute")? as i32 } else { 0 };

        // month
        let month_val = get_val_property(ncx, item, "month")?;
        let month_num = if !month_val.is_undefined() { Some(read_temporal_number(ncx, &month_val, "month")? as i32) } else { None };

        // monthCode
        let month_code_val = get_val_property(ncx, item, "monthCode")?;
        let month_from_code = if !month_code_val.is_undefined() {
            let mc_str = ncx.to_string_value(&month_code_val)?;
            validate_month_code_syntax(&mc_str)?;
            Some(validate_month_code_iso_suitability(&mc_str)? as i32)
        } else { None };

        // nanosecond
        let ns_val = get_val_property(ncx, item, "nanosecond")?;
        let ns = if !ns_val.is_undefined() { read_temporal_number(ncx, &ns_val, "nanosecond")? as i32 } else { 0 };

        // second
        let second_val = get_val_property(ncx, item, "second")?;
        let sec = if !second_val.is_undefined() {
            let sv = read_temporal_number(ncx, &second_val, "second")? as i32;
            if sv == 60 { 59 } else { sv }
        } else { 0 };

        // year
        let year_val = get_val_property(ncx, item, "year")?;
        let y = if !year_val.is_undefined() { Some(read_temporal_number(ncx, &year_val, "year")? as i32) } else { None };

        // Check for required fields
        let y = match y {
            Some(y) => y,
            None => {
                if month_num.is_none() && month_from_code.is_none() && d == -1 {
                    return Err(VmError::type_error("plain object is not a valid property bag and does not convert to a string"));
                }
                return Err(VmError::type_error("year is required"));
            }
        };

        let month = if let Some(mc) = month_from_code {
            mc
        } else if let Some(m) = month_num {
            m
        } else {
            return Err(VmError::type_error("month or monthCode is required"));
        };

        if d == -1 {
            return Err(VmError::type_error("day is required"));
        }

        temporal_rs::PlainDateTime::try_new(
            y, month as u8, d as u8,
            h as u8, mi as u8, sec as u8,
            ms as u16, us as u16, ns as u16,
            temporal_rs::Calendar::default(),
        ).map_err(temporal_err)
    }

    // Helper: ToTemporalDuration — convert a Value to a temporal_rs::Duration
    fn to_temporal_duration(ncx: &mut NativeContext<'_>, item: &Value) -> Result<temporal_rs::Duration, VmError> {
        if item.is_string() {
            let s = ncx.to_string_value(item)?;
            return temporal_rs::Duration::from_utf8(s.as_bytes()).map_err(temporal_err);
        }
        if item.is_undefined() || item.is_null() || item.is_boolean() || item.is_number() || item.is_bigint() {
            return Err(VmError::type_error(format!("cannot convert {} to a Duration", item.type_of())));
        }
        if item.as_symbol().is_some() {
            return Err(VmError::type_error("Cannot convert a Symbol value to a string"));
        }
        // Handle both JsObject and Proxy
        if item.as_object().is_none() && item.as_proxy().is_none() {
            return Err(VmError::type_error("Expected an object or string for duration"));
        }

        // Check for array or function (not valid duration)
        if let Some(obj) = item.as_object() {
            if obj.is_array() {
                return Err(VmError::type_error("cannot convert array to a Duration"));
            }
        }
        if item.is_callable() {
            return Err(VmError::type_error("cannot convert function to a Duration"));
        }

        // If it's a Duration Temporal object, extract fields with 0 defaults (blank duration is valid)
        if let Some(obj) = item.as_object() {
            let temporal_type = obj.get(&PropertyKey::string(SLOT_TEMPORAL_TYPE))
                .and_then(|v| v.as_string().map(|s| s.as_str().to_string()));
            if temporal_type.as_deref() == Some("Duration") {
                // Duration object — read fields with 0 defaults, allowing all-zero
                let y = obj.get(&PropertyKey::string("years")).and_then(|v| v.as_number()).unwrap_or(0.0);
                let mo = obj.get(&PropertyKey::string("months")).and_then(|v| v.as_number()).unwrap_or(0.0);
                let w = obj.get(&PropertyKey::string("weeks")).and_then(|v| v.as_number()).unwrap_or(0.0);
                let d = obj.get(&PropertyKey::string("days")).and_then(|v| v.as_number()).unwrap_or(0.0);
                let h = obj.get(&PropertyKey::string("hours")).and_then(|v| v.as_number()).unwrap_or(0.0);
                let mi = obj.get(&PropertyKey::string("minutes")).and_then(|v| v.as_number()).unwrap_or(0.0);
                let s = obj.get(&PropertyKey::string("seconds")).and_then(|v| v.as_number()).unwrap_or(0.0);
                let ms = obj.get(&PropertyKey::string("milliseconds")).and_then(|v| v.as_number()).unwrap_or(0.0);
                let us = obj.get(&PropertyKey::string("microseconds")).and_then(|v| v.as_number()).unwrap_or(0.0);
                let ns = obj.get(&PropertyKey::string("nanoseconds")).and_then(|v| v.as_number()).unwrap_or(0.0);
                return temporal_rs::Duration::new(
                    y as i64, mo as i64, w as i64, d as i64,
                    h as i64, mi as i64, s as i64, ms as i64,
                    us as i128, ns as i128,
                ).map_err(temporal_err);
            }
        }

        // Read AND convert each duration field IMMEDIATELY in ALPHABETICAL order per spec:
        // days, hours, microseconds, milliseconds, minutes, months, nanoseconds, seconds, weeks, years
        // Each get is immediately followed by valueOf conversion for observable ordering
        fn read_dur_field(ncx: &mut NativeContext<'_>, item: &Value, field: &str) -> Result<(bool, f64), VmError> {
            let v = get_val_property(ncx, item, field)?;
            if v.is_undefined() {
                return Ok((false, 0.0));
            }
            let n = ncx.to_number_value(&v)?;
            if n.is_infinite() {
                return Err(VmError::range_error(format!("{} property cannot be Infinity", field)));
            }
            if n.is_nan() {
                return Err(VmError::range_error(format!("{} property cannot be NaN", field)));
            }
            if n != n.trunc() {
                return Err(VmError::range_error(format!("{} property must be an integer", field)));
            }
            Ok((true, n))
        }

        let (has_days, days) = read_dur_field(ncx, item, "days")?;
        let (has_hours, hours) = read_dur_field(ncx, item, "hours")?;
        let (has_us, microseconds) = read_dur_field(ncx, item, "microseconds")?;
        let (has_ms, milliseconds) = read_dur_field(ncx, item, "milliseconds")?;
        let (has_min, minutes) = read_dur_field(ncx, item, "minutes")?;
        let (has_mo, months) = read_dur_field(ncx, item, "months")?;
        let (has_ns, nanoseconds) = read_dur_field(ncx, item, "nanoseconds")?;
        let (has_sec, seconds) = read_dur_field(ncx, item, "seconds")?;
        let (has_wk, weeks) = read_dur_field(ncx, item, "weeks")?;
        let (has_yr, years) = read_dur_field(ncx, item, "years")?;

        let has_any = has_days || has_hours || has_us || has_ms || has_min || has_mo || has_ns || has_sec || has_wk || has_yr;

        if !has_any {
            return Err(VmError::type_error("duration object must have at least one temporal property"));
        }

        temporal_rs::Duration::new(
            years as i64, months as i64, weeks as i64, days as i64,
            hours as i64, minutes as i64, seconds as i64, milliseconds as i64,
            microseconds as i128, nanoseconds as i128,
        ).map_err(temporal_err)
    }

    // Helper: parse difference options in ALPHABETICAL order per spec:
    // largestUnit, roundingIncrement, roundingMode, smallestUnit
    fn parse_difference_settings(ncx: &mut NativeContext<'_>, options_val: &Value) -> Result<temporal_rs::options::DifferenceSettings, VmError> {
        let mut settings = temporal_rs::options::DifferenceSettings::default();
        if options_val.is_undefined() {
            return Ok(settings);
        }
        // Per GetOptionsObject: only undefined → default, only Object/Proxy → use it, else → TypeError
        if !options_val.is_object() && options_val.as_proxy().is_none() {
            return Err(VmError::type_error("options must be an object"));
        }
        // Read all options first, then validate — alphabetical order
        let lu_val = get_val_property(ncx, options_val, "largestUnit")?;
        let lu_parsed = if !lu_val.is_undefined() {
            let lu_str = ncx.to_string_value(&lu_val)?;
            Some(parse_temporal_unit(&lu_str)?)
        } else { None };

        let ri_val = get_val_property(ncx, options_val, "roundingIncrement")?;
        let ri_parsed = if !ri_val.is_undefined() {
            let ri_num = ncx.to_number_value(&ri_val)?;
            Some(temporal_rs::options::RoundingIncrement::try_from(ri_num).map_err(temporal_err)?)
        } else { None };

        let rm_val = get_val_property(ncx, options_val, "roundingMode")?;
        let rm_parsed = if !rm_val.is_undefined() {
            let rm_str = ncx.to_string_value(&rm_val)?;
            Some(parse_rounding_mode(&rm_str)?)
        } else { None };

        let su_val = get_val_property(ncx, options_val, "smallestUnit")?;
        let su_parsed = if !su_val.is_undefined() {
            let su_str = ncx.to_string_value(&su_val)?;
            Some(parse_temporal_unit(&su_str)?)
        } else { None };

        settings.largest_unit = lu_parsed;
        settings.smallest_unit = su_parsed;
        settings.rounding_mode = rm_parsed;
        settings.increment = ri_parsed;
        Ok(settings)
    }

    // .since(other, options) method
    let since_fn = Value::native_function_with_proto_named(
        move |this, args, ncx| {
            let obj = this.as_object().ok_or_else(|| VmError::type_error("since called on non-object"))?;
            let ty = obj.get(&PropertyKey::string(SLOT_TEMPORAL_TYPE))
                .and_then(|v| v.as_string().map(|s| s.as_str().to_string()));
            if ty.as_deref() != Some("PlainDateTime") {
                return Err(VmError::type_error("since called on non-PlainDateTime"));
            }
            let this_pdt = extract_pdt(&obj)?;

            let other_arg = args.first().cloned().unwrap_or(Value::undefined());
            let other_pdt = to_temporal_datetime(ncx, &other_arg)?;

            let options_val = args.get(1).cloned().unwrap_or(Value::undefined());
            let settings = parse_difference_settings(ncx, &options_val)?;

            let duration = this_pdt.since(&other_pdt, settings).map_err(temporal_err)?;

            // Create a Duration object via Temporal.Duration constructor
            let global = ncx.global();
            let dur_ctor = global.get(&PropertyKey::string("Temporal"))
                .and_then(|v| v.as_object())
                .and_then(|t| t.get(&PropertyKey::string("Duration")))
                .ok_or_else(|| VmError::type_error("Temporal.Duration not found"))?;

            ncx.call_function_construct(&dur_ctor, Value::undefined(), &[
                Value::number(duration.years() as f64),
                Value::number(duration.months() as f64),
                Value::number(duration.weeks() as f64),
                Value::number(duration.days() as f64),
                Value::number(duration.hours() as f64),
                Value::number(duration.minutes() as f64),
                Value::number(duration.seconds() as f64),
                Value::number(duration.milliseconds() as f64),
                Value::number(duration.microseconds() as f64),
                Value::number(duration.nanoseconds() as f64),
            ])
        },
        mm.clone(), fn_proto.clone(), "since", 1,
    );
    proto.define_property(PropertyKey::string("since"), PropertyDescriptor::builtin_method(since_fn));

    // .until(other, options) method
    let until_fn = Value::native_function_with_proto_named(
        move |this, args, ncx| {
            let obj = this.as_object().ok_or_else(|| VmError::type_error("until called on non-object"))?;
            let ty = obj.get(&PropertyKey::string(SLOT_TEMPORAL_TYPE))
                .and_then(|v| v.as_string().map(|s| s.as_str().to_string()));
            if ty.as_deref() != Some("PlainDateTime") {
                return Err(VmError::type_error("until called on non-PlainDateTime"));
            }
            let this_pdt = extract_pdt(&obj)?;

            let other_arg = args.first().cloned().unwrap_or(Value::undefined());
            let other_pdt = to_temporal_datetime(ncx, &other_arg)?;

            let options_val = args.get(1).cloned().unwrap_or(Value::undefined());
            let settings = parse_difference_settings(ncx, &options_val)?;

            let duration = this_pdt.until(&other_pdt, settings).map_err(temporal_err)?;

            let global = ncx.global();
            let dur_ctor = global.get(&PropertyKey::string("Temporal"))
                .and_then(|v| v.as_object())
                .and_then(|t| t.get(&PropertyKey::string("Duration")))
                .ok_or_else(|| VmError::type_error("Temporal.Duration not found"))?;

            ncx.call_function_construct(&dur_ctor, Value::undefined(), &[
                Value::number(duration.years() as f64),
                Value::number(duration.months() as f64),
                Value::number(duration.weeks() as f64),
                Value::number(duration.days() as f64),
                Value::number(duration.hours() as f64),
                Value::number(duration.minutes() as f64),
                Value::number(duration.seconds() as f64),
                Value::number(duration.milliseconds() as f64),
                Value::number(duration.microseconds() as f64),
                Value::number(duration.nanoseconds() as f64),
            ])
        },
        mm.clone(), fn_proto.clone(), "until", 1,
    );
    proto.define_property(PropertyKey::string("until"), PropertyDescriptor::builtin_method(until_fn));

    // .add(duration, options) method
    let add_fn = Value::native_function_with_proto_named(
        move |this, args, ncx| {
            let obj = this.as_object().ok_or_else(|| VmError::type_error("add called on non-object"))?;
            let ty = obj.get(&PropertyKey::string(SLOT_TEMPORAL_TYPE))
                .and_then(|v| v.as_string().map(|s| s.as_str().to_string()));
            if ty.as_deref() != Some("PlainDateTime") {
                return Err(VmError::type_error("add called on non-PlainDateTime"));
            }
            let this_pdt = extract_pdt(&obj)?;

            let dur_arg = args.first().cloned().unwrap_or(Value::undefined());
            let duration = to_temporal_duration(ncx, &dur_arg)?;

            let options_val = args.get(1).cloned().unwrap_or(Value::undefined());
            let overflow = parse_overflow_option(ncx, &options_val)?;

            let result = this_pdt.add(&duration, Some(overflow.to_temporal_rs())).map_err(temporal_err)?;

            let global = ncx.global();
            let pdt_ctor = global.get(&PropertyKey::string("Temporal"))
                .and_then(|v| v.as_object())
                .and_then(|t| t.get(&PropertyKey::string("PlainDateTime")))
                .ok_or_else(|| VmError::type_error("Temporal.PlainDateTime not found"))?;
            ncx.call_function_construct(&pdt_ctor, Value::undefined(), &[
                Value::int32(result.year()),
                Value::int32(result.month() as i32),
                Value::int32(result.day() as i32),
                Value::int32(result.hour() as i32),
                Value::int32(result.minute() as i32),
                Value::int32(result.second() as i32),
                Value::int32(result.millisecond() as i32),
                Value::int32(result.microsecond() as i32),
                Value::int32(result.nanosecond() as i32),
            ])
        },
        mm.clone(), fn_proto.clone(), "add", 1,
    );
    proto.define_property(PropertyKey::string("add"), PropertyDescriptor::builtin_method(add_fn));

    // .subtract(duration, options) method
    let subtract_fn = Value::native_function_with_proto_named(
        move |this, args, ncx| {
            let obj = this.as_object().ok_or_else(|| VmError::type_error("subtract called on non-object"))?;
            let ty = obj.get(&PropertyKey::string(SLOT_TEMPORAL_TYPE))
                .and_then(|v| v.as_string().map(|s| s.as_str().to_string()));
            if ty.as_deref() != Some("PlainDateTime") {
                return Err(VmError::type_error("subtract called on non-PlainDateTime"));
            }
            let this_pdt = extract_pdt(&obj)?;

            let dur_arg = args.first().cloned().unwrap_or(Value::undefined());
            let duration = to_temporal_duration(ncx, &dur_arg)?;

            let options_val = args.get(1).cloned().unwrap_or(Value::undefined());
            let overflow = parse_overflow_option(ncx, &options_val)?;

            let result = this_pdt.subtract(&duration, Some(overflow.to_temporal_rs())).map_err(temporal_err)?;

            let global = ncx.global();
            let pdt_ctor = global.get(&PropertyKey::string("Temporal"))
                .and_then(|v| v.as_object())
                .and_then(|t| t.get(&PropertyKey::string("PlainDateTime")))
                .ok_or_else(|| VmError::type_error("Temporal.PlainDateTime not found"))?;
            ncx.call_function_construct(&pdt_ctor, Value::undefined(), &[
                Value::int32(result.year()),
                Value::int32(result.month() as i32),
                Value::int32(result.day() as i32),
                Value::int32(result.hour() as i32),
                Value::int32(result.minute() as i32),
                Value::int32(result.second() as i32),
                Value::int32(result.millisecond() as i32),
                Value::int32(result.microsecond() as i32),
                Value::int32(result.nanosecond() as i32),
            ])
        },
        mm.clone(), fn_proto.clone(), "subtract", 1,
    );
    proto.define_property(PropertyKey::string("subtract"), PropertyDescriptor::builtin_method(subtract_fn));

    // .withCalendar(calendar) method
    let withcal_fn = Value::native_function_with_proto_named(
        |this, args, ncx| {
            let obj = this.as_object().ok_or_else(|| VmError::type_error("withCalendar called on non-object"))?;
            let ty = obj.get(&PropertyKey::string(SLOT_TEMPORAL_TYPE))
                .and_then(|v| v.as_string().map(|s| s.as_str().to_string()));
            if ty.as_deref() != Some("PlainDateTime") {
                return Err(VmError::type_error("withCalendar called on non-PlainDateTime"));
            }

            let cal_arg = args.first().cloned().unwrap_or(Value::undefined());
            if cal_arg.is_undefined() {
                return Err(VmError::type_error("missing calendar argument"));
            }
            // ToTemporalCalendarIdentifier
            validate_calendar_arg(ncx, &cal_arg)?;

            // Return new PlainDateTime with same ISO fields
            let global = ncx.global();
            let pdt_ctor = global.get(&PropertyKey::string("Temporal"))
                .and_then(|v| v.as_object())
                .and_then(|t| t.get(&PropertyKey::string("PlainDateTime")))
                .ok_or_else(|| VmError::type_error("Temporal.PlainDateTime not found"))?;
            let y = obj.get(&PropertyKey::string(SLOT_ISO_YEAR)).and_then(|v| v.as_int32()).unwrap_or(0);
            let mo = obj.get(&PropertyKey::string(SLOT_ISO_MONTH)).and_then(|v| v.as_int32()).unwrap_or(1);
            let d = obj.get(&PropertyKey::string(SLOT_ISO_DAY)).and_then(|v| v.as_int32()).unwrap_or(1);
            let h = obj.get(&PropertyKey::string(SLOT_ISO_HOUR)).and_then(|v| v.as_int32()).unwrap_or(0);
            let mi = obj.get(&PropertyKey::string(SLOT_ISO_MINUTE)).and_then(|v| v.as_int32()).unwrap_or(0);
            let sec = obj.get(&PropertyKey::string(SLOT_ISO_SECOND)).and_then(|v| v.as_int32()).unwrap_or(0);
            let ms = obj.get(&PropertyKey::string(SLOT_ISO_MILLISECOND)).and_then(|v| v.as_int32()).unwrap_or(0);
            let us = obj.get(&PropertyKey::string(SLOT_ISO_MICROSECOND)).and_then(|v| v.as_int32()).unwrap_or(0);
            let ns = obj.get(&PropertyKey::string(SLOT_ISO_NANOSECOND)).and_then(|v| v.as_int32()).unwrap_or(0);
            ncx.call_function_construct(&pdt_ctor, Value::undefined(), &[
                Value::int32(y), Value::int32(mo), Value::int32(d),
                Value::int32(h), Value::int32(mi), Value::int32(sec),
                Value::int32(ms), Value::int32(us), Value::int32(ns),
            ])
        },
        mm.clone(), fn_proto.clone(), "withCalendar", 1,
    );
    proto.define_property(PropertyKey::string("withCalendar"), PropertyDescriptor::builtin_method(withcal_fn));

    // @@toStringTag
    proto.define_property(
        PropertyKey::Symbol(crate::intrinsics::well_known::to_string_tag_symbol()),
        PropertyDescriptor::data_with_attrs(
            Value::string(JsString::intern("Temporal.PlainDateTime")),
            PropertyAttributes { writable: false, enumerable: false, configurable: true },
        ),
    );
}

/// Parse ISO datetime string into (year, month, day, hour, min, sec, ms, us, ns)
/// Uses temporal_rs for spec-compliant parsing.
fn parse_iso_datetime_string(s: &str) -> Result<(i32, u32, u32, i32, i32, i32, i32, i32, i32), VmError> {
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
// Install Temporal namespace
// ============================================================================

/// Create and install Temporal namespace on global object
pub fn install_temporal_namespace(
    global: GcRef<JsObject>,
    mm: &Arc<MemoryManager>,
) {
    let fn_proto_val = global
        .get(&PropertyKey::string("Function"))
        .and_then(|v| v.as_object())
        .and_then(|ctor| {
            ctor.get(&PropertyKey::string("prototype"))
                .and_then(|v| v.as_object())
        });

    let object_proto_val = global
        .get(&PropertyKey::string("Object"))
        .and_then(|v| v.as_object())
        .and_then(|ctor| {
            ctor.get(&PropertyKey::string("prototype"))
                .and_then(|v| v.as_object())
        });

    // Create main Temporal namespace object
    let temporal_obj = GcRef::new(JsObject::new(
        object_proto_val
            .map(Value::object)
            .unwrap_or(Value::null()),
        mm.clone(),
    ));

    // Tag it
    temporal_obj.define_property(
        PropertyKey::Symbol(crate::intrinsics::well_known::to_string_tag_symbol()),
        PropertyDescriptor::data_with_attrs(
            Value::string(JsString::intern("Temporal")),
            PropertyAttributes {
                writable: false,
                enumerable: false,
                configurable: true,
            },
        ),
    );

    let fn_proto = fn_proto_val.unwrap_or_else(|| {
        GcRef::new(JsObject::new(Value::null(), mm.clone()))
    });
    let obj_proto = object_proto_val.unwrap_or_else(|| {
        GcRef::new(JsObject::new(Value::null(), mm.clone()))
    });

    // ====================================================================
    // Temporal.Now (namespace object, not a constructor)
    // ====================================================================
    let temporal_now = GcRef::new(JsObject::new(Value::object(obj_proto.clone()), mm.clone()));
    temporal_now.define_property(
        PropertyKey::Symbol(crate::intrinsics::well_known::to_string_tag_symbol()),
        PropertyDescriptor::data_with_attrs(
            Value::string(JsString::intern("Temporal.Now")),
            PropertyAttributes {
                writable: false,
                enumerable: false,
                configurable: true,
            },
        ),
    );
    temporal_obj.define_property(
        PropertyKey::string("Now"),
        PropertyDescriptor::data_with_attrs(
            Value::object(temporal_now.clone()),
            PropertyAttributes::builtin_method(),
        ),
    );

    // ====================================================================
    // Temporal.PlainMonthDay
    // ====================================================================
    let pmd_proto =
        GcRef::new(JsObject::new(Value::object(obj_proto.clone()), mm.clone()));

    install_plain_month_day_prototype(pmd_proto.clone(), fn_proto.clone(), mm);

    let pmd_ctor_obj = GcRef::new(JsObject::new(Value::object(fn_proto.clone()), mm.clone()));

    // Wire constructor.prototype
    pmd_ctor_obj.define_property(
        PropertyKey::string("prototype"),
        PropertyDescriptor::data_with_attrs(
            Value::object(pmd_proto.clone()),
            PropertyAttributes {
                writable: false,
                enumerable: false,
                configurable: false,
            },
        ),
    );

    let pmd_ctor_fn = create_plain_month_day_constructor(pmd_proto.clone());
    let pmd_ctor_value = Value::native_function_with_proto_and_object(
        Arc::from(pmd_ctor_fn),
        mm.clone(),
        fn_proto.clone(),
        pmd_ctor_obj.clone(),
    );

    // Wire prototype.constructor
    pmd_proto.define_property(
        PropertyKey::string("constructor"),
        PropertyDescriptor::data_with_attrs(
            pmd_ctor_value.clone(),
            PropertyAttributes::constructor_link(),
        ),
    );

    // Set name and length
    pmd_ctor_obj.define_property(
        PropertyKey::string("name"),
        PropertyDescriptor::function_length(Value::string(JsString::intern("PlainMonthDay"))),
    );
    pmd_ctor_obj.define_property(
        PropertyKey::string("length"),
        PropertyDescriptor::function_length(Value::number(2.0)),
    );

    // PlainMonthDay.from() static method
    let pmd_ctor_for_from = pmd_ctor_value.clone();
    let from_fn = Value::native_function_with_proto_named(
        move |this, args, ncx| plain_month_day_from(pmd_ctor_for_from.clone(), this, args, ncx),
        mm.clone(),
        fn_proto.clone(),
        "from",
        1,
    );
    // Remove __non_constructor tag is set by default in native_function_with_proto_named
    // That's fine — .from() is not a constructor

    pmd_ctor_obj.define_property(
        PropertyKey::string("from"),
        PropertyDescriptor::builtin_method(from_fn),
    );

    temporal_obj.define_property(
        PropertyKey::string("PlainMonthDay"),
        PropertyDescriptor::data_with_attrs(
            pmd_ctor_value,
            PropertyAttributes::builtin_method(),
        ),
    );

    // ====================================================================
    // Stub constructors for other Temporal types (plain objects for now)
    // ====================================================================
    // ====================================================================
    // Temporal.PlainDate
    // ====================================================================
    {
        let pd_proto =
            GcRef::new(JsObject::new(Value::object(obj_proto.clone()), mm.clone()));

        install_plain_date_prototype(pd_proto.clone(), fn_proto.clone(), mm);

        let pd_ctor_obj = GcRef::new(JsObject::new(Value::object(fn_proto.clone()), mm.clone()));
        pd_ctor_obj.define_property(
            PropertyKey::string("prototype"),
            PropertyDescriptor::data_with_attrs(
                Value::object(pd_proto.clone()),
                PropertyAttributes {
                    writable: false,
                    enumerable: false,
                    configurable: false,
                },
            ),
        );
        pd_ctor_obj.define_property(
            PropertyKey::string("name"),
            PropertyDescriptor::function_length(Value::string(JsString::intern("PlainDate"))),
        );
        pd_ctor_obj.define_property(
            PropertyKey::string("length"),
            PropertyDescriptor::function_length(Value::number(3.0)),
        );
        let pd_ctor_fn: Box<
            dyn Fn(&Value, &[Value], &mut NativeContext<'_>) -> Result<Value, VmError> + Send + Sync,
        > = Box::new(|this, args, ncx| {
            let year = to_integer_with_truncation(ncx, &args.first().cloned().unwrap_or(Value::undefined()))? as i32;
            let month = to_integer_with_truncation(ncx, &args.get(1).cloned().unwrap_or(Value::undefined()))? as i32;
            let day = to_integer_with_truncation(ncx, &args.get(2).cloned().unwrap_or(Value::undefined()))? as i32;
            // Validate
            if month < 1 || month > 12 {
                return Err(VmError::range_error(format!("month must be 1-12, got {}", month)));
            }
            let max_day = days_in_month(month as u32, year);
            if day < 1 || day as u32 > max_day {
                return Err(VmError::range_error(format!("day must be 1-{}, got {}", max_day, day)));
            }
            if let Some(obj) = this.as_object() {
                obj.define_property(PropertyKey::string(SLOT_ISO_YEAR), PropertyDescriptor::builtin_data(Value::int32(year)));
                obj.define_property(PropertyKey::string(SLOT_ISO_MONTH), PropertyDescriptor::builtin_data(Value::int32(month)));
                obj.define_property(PropertyKey::string(SLOT_ISO_DAY), PropertyDescriptor::builtin_data(Value::int32(day)));
                obj.define_property(PropertyKey::string(SLOT_TEMPORAL_TYPE), PropertyDescriptor::builtin_data(Value::string(JsString::intern("PlainDate"))));
            }
            Ok(Value::undefined())
        });
        let pd_ctor_value = Value::native_function_with_proto_and_object(
            Arc::from(pd_ctor_fn),
            mm.clone(),
            fn_proto.clone(),
            pd_ctor_obj.clone(),
        );
        pd_proto.define_property(
            PropertyKey::string("constructor"),
            PropertyDescriptor::data_with_attrs(
                pd_ctor_value.clone(),
                PropertyAttributes::constructor_link(),
            ),
        );

        // PlainDate.from() static method
        let pd_ctor_for_from = pd_ctor_value.clone();
        let pd_from_fn = Value::native_function_with_proto_named(
            move |_this, args, ncx| {
                let item = args.first().cloned().unwrap_or(Value::undefined());
                if item.is_string() {
                    let s = ncx.to_string_value(&item)?;
                    let pd = temporal_rs::PlainDate::from_utf8(s.as_bytes()).map_err(temporal_err)?;
                    return ncx.call_function_construct(
                        &pd_ctor_for_from,
                        Value::undefined(),
                        &[
                            Value::int32(pd.year()),
                            Value::int32(pd.month() as i32),
                            Value::int32(pd.day() as i32),
                        ],
                    );
                }
                // Property bag
                if let Some(obj) = item.as_object() {
                    let y = obj.get(&PropertyKey::string("year")).filter(|v| !v.is_undefined());
                    let m = obj.get(&PropertyKey::string("month")).filter(|v| !v.is_undefined());
                    let d = obj.get(&PropertyKey::string("day")).filter(|v| !v.is_undefined());
                    if let (Some(yv), Some(mv), Some(dv)) = (y, m, d) {
                        let year = ncx.to_number_value(&yv)? as i32;
                        let month = ncx.to_number_value(&mv)? as i32;
                        let day = ncx.to_number_value(&dv)? as i32;
                        return ncx.call_function_construct(
                            &pd_ctor_for_from,
                            Value::undefined(),
                            &[Value::int32(year), Value::int32(month), Value::int32(day)],
                        );
                    }
                }
                Err(VmError::type_error("PlainDate.from: invalid argument"))
            },
            mm.clone(),
            fn_proto.clone(),
            "from",
            1,
        );
        pd_ctor_obj.define_property(
            PropertyKey::string("from"),
            PropertyDescriptor::builtin_method(pd_from_fn),
        );

        temporal_obj.define_property(
            PropertyKey::string("PlainDate"),
            PropertyDescriptor::data_with_attrs(
                pd_ctor_value,
                PropertyAttributes::builtin_method(),
            ),
        );
    }

    // ====================================================================
    // Temporal.PlainDateTime
    // ====================================================================
    {
        let pdt_proto = GcRef::new(JsObject::new(Value::object(obj_proto.clone()), mm.clone()));
        install_plain_date_time_prototype(pdt_proto.clone(), fn_proto.clone(), mm);

        let pdt_ctor_obj = GcRef::new(JsObject::new(Value::object(fn_proto.clone()), mm.clone()));
        pdt_ctor_obj.define_property(
            PropertyKey::string("prototype"),
            PropertyDescriptor::data_with_attrs(
                Value::object(pdt_proto.clone()),
                PropertyAttributes { writable: false, enumerable: false, configurable: false },
            ),
        );
        pdt_ctor_obj.define_property(
            PropertyKey::string("name"),
            PropertyDescriptor::function_length(Value::string(JsString::intern("PlainDateTime"))),
        );
        pdt_ctor_obj.define_property(
            PropertyKey::string("length"),
            PropertyDescriptor::function_length(Value::number(3.0)),
        );

        let pdt_proto_for_ctor = pdt_proto.clone();
        let pdt_ctor_fn: Box<
            dyn Fn(&Value, &[Value], &mut NativeContext<'_>) -> Result<Value, VmError> + Send + Sync,
        > = Box::new(move |this, args, ncx| {
            // Step 1: If NewTarget is undefined, throw TypeError
            let is_new_target = if let Some(obj) = this.as_object() {
                obj.prototype().as_object().map_or(false, |p| p.as_ptr() == pdt_proto_for_ctor.as_ptr())
            } else {
                false
            };
            if !is_new_target {
                return Err(VmError::type_error("Temporal.PlainDateTime constructor requires 'new'"));
            }

            // new Temporal.PlainDateTime(year, month, day [, hour, minute, second, ms, us, ns [, calendar]])
            let year = to_integer_with_truncation(ncx, &args.first().cloned().unwrap_or(Value::undefined()))? as i32;
            let month = to_integer_with_truncation(ncx, &args.get(1).cloned().unwrap_or(Value::undefined()))? as i32;
            let day = to_integer_with_truncation(ncx, &args.get(2).cloned().unwrap_or(Value::undefined()))? as i32;

            // Time fields default to 0 (undefined counts as missing)
            let get_time_arg = |idx: usize, ncx: &mut NativeContext<'_>| -> Result<i32, VmError> {
                match args.get(idx) {
                    Some(v) if !v.is_undefined() => Ok(to_integer_with_truncation(ncx, v)? as i32),
                    _ => Ok(0),
                }
            };
            let hour = get_time_arg(3, ncx)?;
            let minute = get_time_arg(4, ncx)?;
            let second = get_time_arg(5, ncx)?;
            let ms = get_time_arg(6, ncx)?;
            let us = get_time_arg(7, ncx)?;
            let ns = get_time_arg(8, ncx)?;

            // Calendar validation (arg 9)
            let calendar_val = args.get(9).cloned().unwrap_or(Value::undefined());
            if !calendar_val.is_undefined() {
                if calendar_val.is_null() || calendar_val.is_boolean() || calendar_val.is_number() || calendar_val.is_bigint() {
                    return Err(VmError::type_error(format!(
                        "{} is not a valid calendar",
                        if calendar_val.is_null() { "null".to_string() } else { calendar_val.type_of().to_string() }
                    )));
                }
                if calendar_val.as_symbol().is_some() {
                    return Err(VmError::type_error("Cannot convert a Symbol value to a string"));
                }
                let cal_str = ncx.to_string_value(&calendar_val)?;
                let lower = cal_str.as_str().to_ascii_lowercase();
                if lower != "iso8601" {
                    return Err(VmError::range_error(format!("Unknown calendar: {}", cal_str)));
                }
            }

            // Range-check before casting to narrower types
            if month < 1 || month > 12 { return Err(VmError::range_error(format!("month must be 1-12, got {}", month))); }
            if day < 1 || day > 31 { return Err(VmError::range_error(format!("day out of range: {}", day))); }
            if hour < 0 || hour > 23 { return Err(VmError::range_error(format!("hour must be 0-23, got {}", hour))); }
            if minute < 0 || minute > 59 { return Err(VmError::range_error(format!("minute must be 0-59, got {}", minute))); }
            if second < 0 || second > 59 { return Err(VmError::range_error(format!("second must be 0-59, got {}", second))); }
            if ms < 0 || ms > 999 { return Err(VmError::range_error(format!("millisecond must be 0-999, got {}", ms))); }
            if us < 0 || us > 999 { return Err(VmError::range_error(format!("microsecond must be 0-999, got {}", us))); }
            if ns < 0 || ns > 999 { return Err(VmError::range_error(format!("nanosecond must be 0-999, got {}", ns))); }

            // Use temporal_rs for full validation (handles limits, leap years, etc.)
            let _validated = temporal_rs::PlainDateTime::try_new_iso(
                year, month as u8, day as u8,
                hour as u8, minute as u8, second as u8,
                ms as u16, us as u16, ns as u16,
            ).map_err(temporal_err)?;

            if let Some(obj) = this.as_object() {
                obj.define_property(PropertyKey::string(SLOT_ISO_YEAR), PropertyDescriptor::builtin_data(Value::int32(year)));
                obj.define_property(PropertyKey::string(SLOT_ISO_MONTH), PropertyDescriptor::builtin_data(Value::int32(month)));
                obj.define_property(PropertyKey::string(SLOT_ISO_DAY), PropertyDescriptor::builtin_data(Value::int32(day)));
                obj.define_property(PropertyKey::string(SLOT_ISO_HOUR), PropertyDescriptor::builtin_data(Value::int32(hour)));
                obj.define_property(PropertyKey::string(SLOT_ISO_MINUTE), PropertyDescriptor::builtin_data(Value::int32(minute)));
                obj.define_property(PropertyKey::string(SLOT_ISO_SECOND), PropertyDescriptor::builtin_data(Value::int32(second)));
                obj.define_property(PropertyKey::string(SLOT_ISO_MILLISECOND), PropertyDescriptor::builtin_data(Value::int32(ms)));
                obj.define_property(PropertyKey::string(SLOT_ISO_MICROSECOND), PropertyDescriptor::builtin_data(Value::int32(us)));
                obj.define_property(PropertyKey::string(SLOT_ISO_NANOSECOND), PropertyDescriptor::builtin_data(Value::int32(ns)));
                obj.define_property(PropertyKey::string(SLOT_TEMPORAL_TYPE), PropertyDescriptor::builtin_data(Value::string(JsString::intern("PlainDateTime"))));
            }
            Ok(Value::undefined())
        });

        let pdt_ctor_value = Value::native_function_with_proto_and_object(
            Arc::from(pdt_ctor_fn),
            mm.clone(),
            fn_proto.clone(),
            pdt_ctor_obj.clone(),
        );

        pdt_proto.define_property(
            PropertyKey::string("constructor"),
            PropertyDescriptor::data_with_attrs(pdt_ctor_value.clone(), PropertyAttributes::constructor_link()),
        );

        // PlainDateTime.from()
        let pdt_ctor_for_from = pdt_ctor_value.clone();
        let pdt_from_fn = Value::native_function_with_proto_named(
            move |_this, args, ncx| {
                let item = args.first().cloned().unwrap_or(Value::undefined());
                let options_val = args.get(1).cloned().unwrap_or(Value::undefined());

                if item.is_string() {
                    let s = ncx.to_string_value(&item)?;
                    // Reject Z designator
                    reject_utc_designator_for_plain(s.as_str())?;
                    let (year, month, day, h, mi, sec, ms, us, ns) = parse_iso_datetime_string(s.as_str())?;
                    // Read options (for observable get, but we don't use the value for string inputs)
                    let _ = parse_overflow_option(ncx, &options_val)?;
                    return ncx.call_function_construct(
                        &pdt_ctor_for_from,
                        Value::undefined(),
                        &[
                            Value::int32(year), Value::int32(month as i32), Value::int32(day as i32),
                            Value::int32(h), Value::int32(mi), Value::int32(sec),
                            Value::int32(ms), Value::int32(us), Value::int32(ns),
                        ],
                    );
                }

                // Property bag (object or proxy)
                let is_proxy = item.as_proxy().is_some();
                if item.as_object().is_some() || is_proxy {
                    // Check for temporal type (only for real objects, not proxies)
                    let temporal_type = if let Some(obj) = item.as_object() {
                        obj.get(&PropertyKey::string(SLOT_TEMPORAL_TYPE))
                            .and_then(|v| v.as_string().map(|s| s.as_str().to_string()))
                    } else { None };
                    let obj = item.as_object(); // may be None for proxy

                    if temporal_type.as_deref() == Some("PlainDateTime") {
                        let o = obj.as_ref().unwrap();
                        // Read options first (for observable get ordering)
                        let _ = parse_overflow_option(ncx, &options_val)?;
                        // Copy from existing PlainDateTime
                        let y = o.get(&PropertyKey::string(SLOT_ISO_YEAR)).and_then(|v| v.as_int32()).unwrap_or(0);
                        let mo = o.get(&PropertyKey::string(SLOT_ISO_MONTH)).and_then(|v| v.as_int32()).unwrap_or(1);
                        let d = o.get(&PropertyKey::string(SLOT_ISO_DAY)).and_then(|v| v.as_int32()).unwrap_or(1);
                        let h = o.get(&PropertyKey::string(SLOT_ISO_HOUR)).and_then(|v| v.as_int32()).unwrap_or(0);
                        let mi = o.get(&PropertyKey::string(SLOT_ISO_MINUTE)).and_then(|v| v.as_int32()).unwrap_or(0);
                        let s = o.get(&PropertyKey::string(SLOT_ISO_SECOND)).and_then(|v| v.as_int32()).unwrap_or(0);
                        let ms = o.get(&PropertyKey::string(SLOT_ISO_MILLISECOND)).and_then(|v| v.as_int32()).unwrap_or(0);
                        let us = o.get(&PropertyKey::string(SLOT_ISO_MICROSECOND)).and_then(|v| v.as_int32()).unwrap_or(0);
                        let ns = o.get(&PropertyKey::string(SLOT_ISO_NANOSECOND)).and_then(|v| v.as_int32()).unwrap_or(0);
                        return ncx.call_function_construct(
                            &pdt_ctor_for_from, Value::undefined(),
                            &[Value::int32(y), Value::int32(mo), Value::int32(d),
                              Value::int32(h), Value::int32(mi), Value::int32(s),
                              Value::int32(ms), Value::int32(us), Value::int32(ns)],
                        );
                    }

                    if temporal_type.as_deref() == Some("PlainDate") {
                        let o = obj.as_ref().unwrap();
                        // PlainDate -> PlainDateTime with time 00:00:00
                        let _ = parse_overflow_option(ncx, &options_val)?;
                        let y = o.get(&PropertyKey::string(SLOT_ISO_YEAR)).and_then(|v| v.as_int32()).unwrap_or(0);
                        let mo = o.get(&PropertyKey::string(SLOT_ISO_MONTH)).and_then(|v| v.as_int32()).unwrap_or(1);
                        let d = o.get(&PropertyKey::string(SLOT_ISO_DAY)).and_then(|v| v.as_int32()).unwrap_or(1);
                        return ncx.call_function_construct(
                            &pdt_ctor_for_from, Value::undefined(),
                            &[Value::int32(y), Value::int32(mo), Value::int32(d)],
                        );
                    }

                    if temporal_type.as_deref() == Some("ZonedDateTime") {
                        let o = obj.as_ref().unwrap();
                        // ZonedDateTime → PlainDateTime: apply timezone offset
                        let _ = parse_overflow_option(ncx, &options_val)?;
                        let epoch_ns_val = o.get(&PropertyKey::string("epochNanoseconds"))
                            .unwrap_or(Value::int32(0));
                        let tz_id_val = o.get(&PropertyKey::string("timeZoneId"))
                            .unwrap_or(Value::string(JsString::intern("UTC")));
                        let tz_id = if let Some(s) = tz_id_val.as_string() { s.as_str().to_string() } else { "UTC".to_string() };

                        // Parse epoch nanoseconds from BigInt or number
                        let epoch_ns: i128 = if epoch_ns_val.is_bigint() {
                            // BigInt: convert to string, then parse
                            let s = ncx.to_string_value(&epoch_ns_val)?;
                            // Remove trailing 'n' if present
                            let s = s.trim_end_matches('n');
                            s.parse::<i128>().unwrap_or(0)
                        } else if let Some(n) = epoch_ns_val.as_number() {
                            n as i128
                        } else { 0 };

                        // Compute offset nanoseconds from timezone
                        let offset_ns: i128 = parse_timezone_offset_ns(&tz_id);

                        // Apply offset to get wall-clock nanoseconds
                        let wall_ns = epoch_ns + offset_ns;

                        // GetISOPartsFromEpoch using Euclidean division for correct floor behavior
                        let ns_per_ms: i128 = 1_000_000;
                        let ms_per_s: i128 = 1_000;

                        let epoch_ms = wall_ns.div_euclid(ns_per_ms);
                        let remainder_ns = wall_ns.rem_euclid(ns_per_ms);
                        let us_part = (remainder_ns / 1000) as i32;
                        let ns_part = (remainder_ns % 1000) as i32;

                        let epoch_secs = epoch_ms.div_euclid(ms_per_s);
                        let ms_rem = epoch_ms.rem_euclid(ms_per_s) as i32;

                        let ndt = chrono::DateTime::from_timestamp(epoch_secs as i64, (ms_rem as u32) * 1_000_000)
                            .unwrap_or_else(|| chrono::DateTime::from_timestamp(0, 0).unwrap())
                            .naive_utc();

                        return ncx.call_function_construct(
                            &pdt_ctor_for_from, Value::undefined(),
                            &[
                                Value::int32(ndt.year()),
                                Value::int32(ndt.month() as i32),
                                Value::int32(ndt.day() as i32),
                                Value::int32(ndt.hour() as i32),
                                Value::int32(ndt.minute() as i32),
                                Value::int32(ndt.second() as i32),
                                Value::int32(ms_rem),
                                Value::int32(us_part),
                                Value::int32(ns_part),
                            ],
                        );
                    }

                    // Helper for observable property get (supports both object and proxy)
                    let get_field = |ncx: &mut NativeContext<'_>, name: &str| -> Result<Value, VmError> {
                        if let Some(proxy) = item.as_proxy() {
                            let key = PropertyKey::string(name);
                            let key_value = crate::proxy_operations::property_key_to_value_pub(&key);
                            crate::proxy_operations::proxy_get(ncx, proxy, &key, key_value, item.clone())
                        } else if let Some(obj) = item.as_object() {
                            ncx.get_property(&obj, &PropertyKey::string(name))
                        } else {
                            Ok(Value::undefined())
                        }
                    };

                    // Validate calendar property if present
                    let calendar_val = get_field(ncx, "calendar")?;
                    if !calendar_val.is_undefined() {
                        resolve_calendar_from_property(ncx, &calendar_val)?;
                    }

                    // PrepareTemporalFields — get + IMMEDIATELY convert each field (alphabetical order)
                    // This ensures valueOf/toString is called right after each get
                    let day_val = get_field(ncx, "day")?;
                    let d = if !day_val.is_undefined() {
                        let n = ncx.to_number_value(&day_val)?;
                        if n.is_infinite() { return Err(VmError::range_error("day property cannot be Infinity")); }
                        Some(n as i32)
                    } else { None };

                    let hour_val = get_field(ncx, "hour")?;
                    let h = if !hour_val.is_undefined() {
                        let n = ncx.to_number_value(&hour_val)?;
                        if n.is_infinite() { return Err(VmError::range_error("hour property cannot be Infinity")); }
                        n as i32
                    } else { 0 };

                    let microsecond_val = get_field(ncx, "microsecond")?;
                    let us = if !microsecond_val.is_undefined() {
                        let n = ncx.to_number_value(&microsecond_val)?;
                        if n.is_infinite() { return Err(VmError::range_error("microsecond property cannot be Infinity")); }
                        n as i32
                    } else { 0 };

                    let millisecond_val = get_field(ncx, "millisecond")?;
                    let ms = if !millisecond_val.is_undefined() {
                        let n = ncx.to_number_value(&millisecond_val)?;
                        if n.is_infinite() { return Err(VmError::range_error("millisecond property cannot be Infinity")); }
                        n as i32
                    } else { 0 };

                    let minute_val = get_field(ncx, "minute")?;
                    let mi = if !minute_val.is_undefined() {
                        let n = ncx.to_number_value(&minute_val)?;
                        if n.is_infinite() { return Err(VmError::range_error("minute property cannot be Infinity")); }
                        n as i32
                    } else { 0 };

                    let month_val = get_field(ncx, "month")?;
                    let month_num = if !month_val.is_undefined() {
                        let n = ncx.to_number_value(&month_val)?;
                        if n.is_infinite() { return Err(VmError::range_error("month property cannot be Infinity")); }
                        Some(n as i32)
                    } else { None };

                    let month_code_val = get_field(ncx, "monthCode")?;
                    let mc_str = if !month_code_val.is_undefined() {
                        // monthCode: ToPrimitive(value, string) then RequireString
                        if month_code_val.as_symbol().is_some() {
                            return Err(VmError::type_error("Cannot convert a Symbol value to a string"));
                        }
                        // ToPrimitive for objects calls toString/valueOf
                        let primitive = if month_code_val.as_object().is_some() || month_code_val.as_proxy().is_some() {
                            ncx.to_primitive(&month_code_val, crate::interpreter::PreferredType::String)?
                        } else {
                            month_code_val.clone()
                        };
                        // RequireString: result must be a String
                        if !primitive.is_string() {
                            return Err(VmError::type_error(format!(
                                "monthCode must be a string, got {}",
                                primitive.type_of()
                            )));
                        }
                        let mc = primitive.as_string().unwrap().as_str().to_string();
                        // Syntax validation happens at read time (before other field conversions)
                        validate_month_code_syntax(&mc)?;
                        Some(mc)
                    } else { None };

                    let nanosecond_val = get_field(ncx, "nanosecond")?;
                    let ns = if !nanosecond_val.is_undefined() {
                        let n = ncx.to_number_value(&nanosecond_val)?;
                        if n.is_infinite() { return Err(VmError::range_error("nanosecond property cannot be Infinity")); }
                        n as i32
                    } else { 0 };

                    let second_val = get_field(ncx, "second")?;
                    let s = if !second_val.is_undefined() {
                        let n = ncx.to_number_value(&second_val)?;
                        if n.is_infinite() { return Err(VmError::range_error("second property cannot be Infinity")); }
                        n as i32
                    } else { 0 };

                    let year_val = get_field(ncx, "year")?;
                    let y = if !year_val.is_undefined() {
                        let n = ncx.to_number_value(&year_val)?;
                        if n.is_infinite() { return Err(VmError::range_error("year property cannot be Infinity")); }
                        Some(n as i32)
                    } else { None };

                    // Read options — overflow read comes AFTER field gets per spec
                    let overflow = parse_overflow_option(ncx, &options_val)?;

                    // CalendarResolveFields: check required fields FIRST (TypeError)
                    // before monthCode suitability validation (RangeError)
                    let y = y.ok_or_else(|| VmError::type_error("year is required"))?;
                    if mc_str.is_none() && month_num.is_none() {
                        return Err(VmError::type_error("month or monthCode is required"));
                    }
                    let d = d.ok_or_else(|| VmError::type_error("day is required"))?;

                    // Resolve month from monthCode and/or month
                    // (syntax already validated at read time; suitability validated here)
                    let m = if let Some(ref mc) = mc_str {
                        let mc_month = validate_month_code_iso_suitability(mc.as_str())? as i32;
                        if let Some(m_int) = month_num {
                            if m_int != mc_month {
                                return Err(VmError::range_error("month and monthCode must agree"));
                            }
                        }
                        mc_month
                    } else {
                        month_num.unwrap() // safe: checked above
                    };

                    // Use temporal_rs for validation with overflow
                    let ov = if overflow == Overflow::Reject { temporal_rs::options::Overflow::Reject } else { temporal_rs::options::Overflow::Constrain };
                    if m < 0 || m > 255 { return Err(VmError::range_error(format!("month out of range: {}", m))); }
                    if d < 0 || d > 255 { return Err(VmError::range_error(format!("day out of range: {}", d))); }
                    let pdt = temporal_rs::PlainDateTime::new_with_overflow(
                        y, m as u8, d as u8,
                        h.clamp(0, 255) as u8, mi.clamp(0, 255) as u8, s.clamp(0, 255) as u8,
                        ms.clamp(0, 65535) as u16, us.clamp(0, 65535) as u16, ns.clamp(0, 65535) as u16,
                        temporal_rs::Calendar::default(), ov,
                    ).map_err(temporal_err)?;

                    return ncx.call_function_construct(
                        &pdt_ctor_for_from, Value::undefined(),
                        &[Value::int32(pdt.year()), Value::int32(pdt.month() as i32), Value::int32(pdt.day() as i32),
                          Value::int32(pdt.hour() as i32), Value::int32(pdt.minute() as i32), Value::int32(pdt.second() as i32),
                          Value::int32(pdt.millisecond() as i32), Value::int32(pdt.microsecond() as i32), Value::int32(pdt.nanosecond() as i32)],
                    );
                }

                Err(VmError::type_error("PlainDateTime.from: invalid argument"))
            },
            mm.clone(),
            fn_proto.clone(),
            "from",
            1,
        );
        pdt_ctor_obj.define_property(
            PropertyKey::string("from"),
            PropertyDescriptor::builtin_method(pdt_from_fn),
        );

        // PlainDateTime.compare(one, two) — static method
        let pdt_compare_fn = Value::native_function_with_proto_named(
            |_this, args, ncx| {
                let one_arg = args.first().cloned().unwrap_or(Value::undefined());
                let two_arg = args.get(1).cloned().unwrap_or(Value::undefined());
                let one = to_temporal_datetime_standalone(ncx, &one_arg)?;
                let two = to_temporal_datetime_standalone(ncx, &two_arg)?;
                match temporal_rs::PlainDateTime::compare_iso(&one, &two) {
                    std::cmp::Ordering::Less => Ok(Value::int32(-1)),
                    std::cmp::Ordering::Equal => Ok(Value::int32(0)),
                    std::cmp::Ordering::Greater => Ok(Value::int32(1)),
                }
            },
            mm.clone(),
            fn_proto.clone(),
            "compare",
            2,
        );
        pdt_ctor_obj.define_property(
            PropertyKey::string("compare"),
            PropertyDescriptor::builtin_method(pdt_compare_fn),
        );

        temporal_obj.define_property(
            PropertyKey::string("PlainDateTime"),
            PropertyDescriptor::data_with_attrs(pdt_ctor_value, PropertyAttributes::builtin_method()),
        );
    }

    // Install Temporal.Now methods (after all constructors are defined)
    {
        let now_method = |name: &str, ctor_name: &'static str, arg_builder: fn() -> Vec<Value>| -> Value {
            Value::native_function_with_proto_named(
                move |_this, _args, ncx| {
                    let temporal_ns = ncx.ctx.get_global("Temporal")
                        .ok_or_else(|| VmError::type_error("Temporal namespace not found"))?;
                    let temporal_obj = temporal_ns.as_object()
                        .ok_or_else(|| VmError::type_error("Temporal namespace not found"))?;
                    let ctor = temporal_obj.get(&PropertyKey::string(ctor_name))
                        .ok_or_else(|| VmError::type_error(format!("{} constructor not found", ctor_name)))?;
                    let args = arg_builder();
                    ncx.call_function_construct(&ctor, Value::undefined(), &args)
                },
                mm.clone(),
                fn_proto.clone(),
                name,
                0,
            )
        };

        fn pdt_args() -> Vec<Value> {
            let now = chrono::Local::now();
            vec![
                Value::int32(now.year()),
                Value::int32(now.month() as i32),
                Value::int32(now.day() as i32),
                Value::int32(now.hour() as i32),
                Value::int32(now.minute() as i32),
                Value::int32(now.second() as i32),
                Value::int32((now.nanosecond() / 1_000_000) as i32),
                Value::int32(((now.nanosecond() % 1_000_000) / 1000) as i32),
                Value::int32((now.nanosecond() % 1000) as i32),
            ]
        }
        fn pd_args() -> Vec<Value> {
            let now = chrono::Local::now();
            vec![
                Value::int32(now.year()),
                Value::int32(now.month() as i32),
                Value::int32(now.day() as i32),
            ]
        }
        fn pt_args() -> Vec<Value> {
            let now = chrono::Local::now();
            vec![
                Value::int32(now.hour() as i32),
                Value::int32(now.minute() as i32),
                Value::int32(now.second() as i32),
                Value::int32((now.nanosecond() / 1_000_000) as i32),
                Value::int32(((now.nanosecond() % 1_000_000) / 1000) as i32),
                Value::int32((now.nanosecond() % 1000) as i32),
            ]
        }

        temporal_now.define_property(
            PropertyKey::string("plainDateTimeISO"),
            PropertyDescriptor::data_with_attrs(
                now_method("plainDateTimeISO", "PlainDateTime", pdt_args),
                PropertyAttributes::builtin_method(),
            ),
        );
        temporal_now.define_property(
            PropertyKey::string("plainDateISO"),
            PropertyDescriptor::data_with_attrs(
                now_method("plainDateISO", "PlainDate", pd_args),
                PropertyAttributes::builtin_method(),
            ),
        );
        temporal_now.define_property(
            PropertyKey::string("plainTimeISO"),
            PropertyDescriptor::data_with_attrs(
                now_method("plainTimeISO", "PlainTime", pt_args),
                PropertyAttributes::builtin_method(),
            ),
        );
    }

    let stub_types = [
        "Instant",
        "PlainTime",
        "PlainYearMonth",
        "ZonedDateTime",
        "Duration",
    ];

    for name in &stub_types {
        let stub_proto =
            GcRef::new(JsObject::new(Value::object(obj_proto.clone()), mm.clone()));
        let stub_ctor_obj = GcRef::new(JsObject::new(Value::object(fn_proto.clone()), mm.clone()));
        stub_ctor_obj.define_property(
            PropertyKey::string("prototype"),
            PropertyDescriptor::data_with_attrs(
                Value::object(stub_proto.clone()),
                PropertyAttributes {
                    writable: false,
                    enumerable: false,
                    configurable: false,
                },
            ),
        );
        stub_ctor_obj.define_property(
            PropertyKey::string("name"),
            PropertyDescriptor::function_length(Value::string(JsString::intern(name))),
        );
        stub_ctor_obj.define_property(
            PropertyKey::string("length"),
            PropertyDescriptor::function_length(Value::number(0.0)),
        );
        let name_owned = name.to_string();
        let stub_ctor_fn: Box<
            dyn Fn(&Value, &[Value], &mut NativeContext<'_>) -> Result<Value, VmError> + Send + Sync,
        > = Box::new(move |this, args, _ncx| {
            // Store temporal type on this
            if let Some(obj) = this.as_object() {
                obj.define_property(
                    PropertyKey::string(SLOT_TEMPORAL_TYPE),
                    PropertyDescriptor::builtin_data(Value::string(JsString::intern(&name_owned))),
                );
                // ZonedDateTime: store epochNanoseconds (arg0) and timeZoneId (arg1)
                if name_owned == "ZonedDateTime" {
                    if let Some(epoch_ns) = args.first() {
                        obj.define_property(
                            PropertyKey::string("epochNanoseconds"),
                            PropertyDescriptor::builtin_data(epoch_ns.clone()),
                        );
                    }
                    if let Some(tz_id) = args.get(1) {
                        obj.define_property(
                            PropertyKey::string("timeZoneId"),
                            PropertyDescriptor::builtin_data(tz_id.clone()),
                        );
                    }
                }
                // Instant: store epochNanoseconds (arg0)
                if name_owned == "Instant" {
                    if let Some(epoch_ns) = args.first() {
                        obj.define_property(
                            PropertyKey::string("epochNanoseconds"),
                            PropertyDescriptor::builtin_data(epoch_ns.clone()),
                        );
                    }
                }
                // Duration: store fields from args
                if name_owned == "Duration" {
                    let dur_fields = ["years","months","weeks","days","hours","minutes","seconds","milliseconds","microseconds","nanoseconds"];
                    for (i, field) in dur_fields.iter().enumerate() {
                        if let Some(val) = args.get(i) {
                            if !val.is_undefined() {
                                obj.define_property(
                                    PropertyKey::string(field),
                                    PropertyDescriptor::builtin_data(val.clone()),
                                );
                            }
                        }
                    }
                }
            }
            Ok(Value::undefined())
        });
        let stub_value = Value::native_function_with_proto_and_object(
            Arc::from(stub_ctor_fn),
            mm.clone(),
            fn_proto.clone(),
            stub_ctor_obj.clone(),
        );
        // Wire prototype.constructor
        stub_proto.define_property(
            PropertyKey::string("constructor"),
            PropertyDescriptor::data_with_attrs(
                stub_value.clone(),
                PropertyAttributes::constructor_link(),
            ),
        );

        // Add from() static method to stub types
        let from_ctor = stub_value.clone();
        let from_name = name.to_string();
        let from_fn = Value::native_function_with_proto_named(
            move |_this, args, ncx| {
                let item = args.first().cloned().unwrap_or(Value::undefined());
                if from_name == "Duration" {
                    // Duration.from: use ToTemporalDuration-like logic
                    if item.is_string() {
                        let s = ncx.to_string_value(&item)?;
                        let dur = temporal_rs::Duration::from_utf8(s.as_bytes()).map_err(temporal_err)?;
                        // Construct via constructor with extracted fields
                        let dur_args = vec![
                            Value::number(dur.years() as f64), Value::number(dur.months() as f64),
                            Value::number(dur.weeks() as f64), Value::number(dur.days() as f64),
                            Value::number(dur.hours() as f64), Value::number(dur.minutes() as f64),
                            Value::number(dur.seconds() as f64), Value::number(dur.milliseconds() as f64),
                            Value::number(dur.microseconds() as f64), Value::number(dur.nanoseconds() as f64),
                        ];
                        return ncx.call_function_construct(&from_ctor, Value::undefined(), &dur_args);
                    }
                    // For objects/property bags: read fields properly
                    if let Some(obj) = item.as_object() {
                        // Check if it's already a Duration instance
                        let tt = obj.get(&PropertyKey::string(SLOT_TEMPORAL_TYPE))
                            .and_then(|v| v.as_string().map(|s| s.as_str().to_string()));
                        if tt.as_deref() == Some("Duration") {
                            // Copy fields from existing Duration
                            let fields = ["years","months","weeks","days","hours","minutes","seconds","milliseconds","microseconds","nanoseconds"];
                            let dur_args: Vec<Value> = fields.iter().map(|f| {
                                obj.get(&PropertyKey::string(f)).unwrap_or(Value::int32(0))
                            }).collect();
                            return ncx.call_function_construct(&from_ctor, Value::undefined(), &dur_args);
                        }
                        // Generic property bag: read fields in alphabetical order
                        let field_names_alpha = ["days","hours","microseconds","milliseconds","minutes","months","nanoseconds","seconds","weeks","years"];
                        let field_names_ctor  = ["years","months","weeks","days","hours","minutes","seconds","milliseconds","microseconds","nanoseconds"];
                        let mut field_map = std::collections::HashMap::new();
                        for &f in &field_names_alpha {
                            let v = ncx.get_property(&obj, &PropertyKey::string(f))?;
                            if !v.is_undefined() {
                                let n = ncx.to_number_value(&v)?;
                                if n.is_infinite() { return Err(VmError::range_error(format!("{} cannot be Infinity", f))); }
                                if n.is_nan() { return Err(VmError::range_error(format!("{} cannot be NaN", f))); }
                                if n != n.trunc() { return Err(VmError::range_error(format!("{} must be an integer", f))); }
                                field_map.insert(f, n);
                            }
                        }
                        if field_map.is_empty() {
                            return Err(VmError::type_error("duration object must have at least one temporal property"));
                        }
                        let dur_args: Vec<Value> = field_names_ctor.iter().map(|f| {
                            Value::number(*field_map.get(f).unwrap_or(&0.0))
                        }).collect();
                        return ncx.call_function_construct(&from_ctor, Value::undefined(), &dur_args);
                    }
                    return Err(VmError::type_error("invalid argument for Duration.from"));
                }
                // Non-Duration types: just pass through to constructor
                ncx.call_function_construct(&from_ctor, Value::undefined(), &[item])
            },
            mm.clone(),
            fn_proto.clone(),
            "from",
            1,
        );
        stub_ctor_obj.define_property(
            PropertyKey::string("from"),
            PropertyDescriptor::data_with_attrs(from_fn, PropertyAttributes::builtin_method()),
        );

        // Duration-specific static and prototype methods
        if *name == "Duration" {
            // Duration.compare(d1, d2) — compares two durations by total nanoseconds
            let compare_fn = Value::native_function_with_proto_named(
                |_this, args, _ncx| {
                    let fields = ["years","months","weeks","days","hours","minutes","seconds","milliseconds","microseconds","nanoseconds"];
                    let d1 = args.first().and_then(|v| v.as_object()).ok_or_else(|| VmError::type_error("compare: first argument must be a Duration"))?;
                    let d2 = args.get(1).and_then(|v| v.as_object()).ok_or_else(|| VmError::type_error("compare: second argument must be a Duration"))?;
                    let mut v1 = [0f64; 10];
                    let mut v2 = [0f64; 10];
                    for (i, f) in fields.iter().enumerate() {
                        v1[i] = d1.get(&PropertyKey::string(f)).and_then(|v| v.as_number()).unwrap_or(0.0);
                        v2[i] = d2.get(&PropertyKey::string(f)).and_then(|v| v.as_number()).unwrap_or(0.0);
                    }
                    // total ns: ns + us*1e3 + ms*1e6 + s*1e9 + min*60e9 + h*3600e9 + d*86400e9
                    let ns1 = v1[9] + v1[8]*1e3 + v1[7]*1e6 + v1[6]*1e9 + v1[5]*60e9 + v1[4]*3600e9 + v1[3]*86400e9;
                    let ns2 = v2[9] + v2[8]*1e3 + v2[7]*1e6 + v2[6]*1e9 + v2[5]*60e9 + v2[4]*3600e9 + v2[3]*86400e9;
                    if ns1 < ns2 {
                        Ok(Value::int32(-1))
                    } else if ns1 > ns2 {
                        Ok(Value::int32(1))
                    } else {
                        Ok(Value::int32(0))
                    }
                },
                mm.clone(), fn_proto.clone(), "compare", 2,
            );
            stub_ctor_obj.define_property(
                PropertyKey::string("compare"),
                PropertyDescriptor::data_with_attrs(compare_fn, PropertyAttributes::builtin_method()),
            );

            // .negated() method
            let neg_ctor = stub_value.clone();
            let negated_fn = Value::native_function_with_proto_named(
                move |this, _args, ncx| {
                    let obj = this.as_object().ok_or_else(|| VmError::type_error("negated called on non-Duration"))?;
                    let dur_field_names = ["years","months","weeks","days","hours","minutes","seconds","milliseconds","microseconds","nanoseconds"];
                    let mut neg_args = Vec::with_capacity(10);
                    for field in &dur_field_names {
                        let v = obj.get(&PropertyKey::string(field)).and_then(|v| v.as_number()).unwrap_or(0.0);
                        // Avoid -0: negate only non-zero values
                        neg_args.push(if v == 0.0 { Value::number(0.0) } else { Value::number(-v) });
                    }
                    ncx.call_function_construct(&neg_ctor, Value::undefined(), &neg_args)
                },
                mm.clone(), fn_proto.clone(), "negated", 0,
            );
            stub_proto.define_property(PropertyKey::string("negated"), PropertyDescriptor::builtin_method(negated_fn));

            // .toString() method
            let tostring_fn = Value::native_function_with_proto_named(
                |this, _args, _ncx| {
                    let obj = this.as_object().ok_or_else(|| VmError::type_error("toString called on non-Duration"))?;
                    let dur_field_names = ["years","months","weeks","days","hours","minutes","seconds","milliseconds","microseconds","nanoseconds"];
                    let mut vals = [0i64; 10];
                    for (i, field) in dur_field_names.iter().enumerate() {
                        vals[i] = obj.get(&PropertyKey::string(field)).and_then(|v| v.as_number()).unwrap_or(0.0) as i64;
                    }
                    let [years, months, weeks, days, hours, minutes, seconds, milliseconds, microseconds, nanoseconds] = vals;
                    // Build ISO 8601 duration string
                    let sign = if [years,months,weeks,days,hours,minutes,seconds,milliseconds,microseconds,nanoseconds].iter().any(|&v| v < 0) {
                        // If any field is negative, all should be (per Temporal spec)
                        -1i64
                    } else { 1 };
                    let mut s = String::new();
                    if sign < 0 { s.push('-'); }
                    s.push('P');
                    let ay = years.unsigned_abs();
                    let amo = months.unsigned_abs();
                    let aw = weeks.unsigned_abs();
                    let ad = days.unsigned_abs();
                    if ay > 0 { s.push_str(&format!("{}Y", ay)); }
                    if amo > 0 { s.push_str(&format!("{}M", amo)); }
                    if aw > 0 { s.push_str(&format!("{}W", aw)); }
                    if ad > 0 { s.push_str(&format!("{}D", ad)); }
                    let ah = hours.unsigned_abs();
                    let ami = minutes.unsigned_abs();
                    // Balance seconds/ms/us/ns: compute total nanoseconds then extract seconds + frac
                    let total_ns_i128 = (seconds as i128) * 1_000_000_000
                        + (milliseconds as i128) * 1_000_000
                        + (microseconds as i128) * 1_000
                        + nanoseconds as i128;
                    let total_ns_abs = total_ns_i128.unsigned_abs();
                    let balanced_secs = total_ns_abs / 1_000_000_000;
                    let frac_ns = total_ns_abs % 1_000_000_000;
                    if ah > 0 || ami > 0 || balanced_secs > 0 || frac_ns > 0 {
                        s.push('T');
                        if ah > 0 { s.push_str(&format!("{}H", ah)); }
                        if ami > 0 { s.push_str(&format!("{}M", ami)); }
                        if balanced_secs > 0 || frac_ns > 0 {
                            if frac_ns > 0 {
                                let frac = format!("{:09}", frac_ns);
                                let frac = frac.trim_end_matches('0');
                                s.push_str(&format!("{}.{}S", balanced_secs, frac));
                            } else {
                                s.push_str(&format!("{}S", balanced_secs));
                            }
                        }
                    }
                    if s == "P" || s == "-P" { s = "PT0S".to_string(); }
                    Ok(Value::string(JsString::intern(&s)))
                },
                mm.clone(), fn_proto.clone(), "toString", 0,
            );
            stub_proto.define_property(PropertyKey::string("toString"), PropertyDescriptor::builtin_method(tostring_fn));

            // .total(options) method — returns total number of given unit
            let total_fn = Value::native_function_with_proto_named(
                |this, args, ncx| {
                    let obj = this.as_object().ok_or_else(|| VmError::type_error("total called on non-Duration"))?;
                    let dur_field_names = ["years","months","weeks","days","hours","minutes","seconds","milliseconds","microseconds","nanoseconds"];
                    let mut vals = [0f64; 10];
                    for (i, field) in dur_field_names.iter().enumerate() {
                        vals[i] = obj.get(&PropertyKey::string(field)).and_then(|v| v.as_number()).unwrap_or(0.0);
                    }
                    let [years, months, weeks, days, hours, minutes, seconds, milliseconds, microseconds, nanoseconds] = vals;

                    // Get unit from argument — can be a string or options object with "unit" property
                    let unit_arg = args.first().cloned().unwrap_or(Value::undefined());
                    let unit_str = if unit_arg.is_string() {
                        ncx.to_string_value(&unit_arg)?
                    } else if let Some(opts_obj) = unit_arg.as_object() {
                        let u = ncx.get_property(&opts_obj, &PropertyKey::string("unit"))?;
                        if u.is_undefined() {
                            return Err(VmError::range_error("unit is required"));
                        }
                        ncx.to_string_value(&u)?
                    } else {
                        return Err(VmError::type_error("total requires a unit string or options object"));
                    };

                    // Convert to total nanoseconds first, then divide
                    // For time-only durations (no date components), compute total directly
                    let total_ns = nanoseconds
                        + microseconds * 1e3
                        + milliseconds * 1e6
                        + seconds * 1e9
                        + minutes * 60e9
                        + hours * 3600e9
                        + days * 86400e9;

                    let result = match unit_str.as_str() {
                        "nanosecond" | "nanoseconds" => total_ns,
                        "microsecond" | "microseconds" => total_ns / 1e3,
                        "millisecond" | "milliseconds" => total_ns / 1e6,
                        "second" | "seconds" => total_ns / 1e9,
                        "minute" | "minutes" => total_ns / 60e9,
                        "hour" | "hours" => total_ns / 3600e9,
                        "day" | "days" => total_ns / 86400e9,
                        "week" | "weeks" => total_ns / (7.0 * 86400e9),
                        "month" | "months" => {
                            // Approximate — requires calendar context in full impl
                            months + years * 12.0
                        }
                        "year" | "years" => {
                            years + months / 12.0
                        }
                        _ => return Err(VmError::range_error(format!("{} is not a valid unit", unit_str))),
                    };
                    Ok(Value::number(result))
                },
                mm.clone(), fn_proto.clone(), "total", 1,
            );
            stub_proto.define_property(PropertyKey::string("total"), PropertyDescriptor::builtin_method(total_fn));

            // .add(other) method
            let add_dur_ctor = stub_value.clone();
            let add_fn = Value::native_function_with_proto_named(
                move |this, args, ncx| {
                    let obj = this.as_object().ok_or_else(|| VmError::type_error("add called on non-Duration"))?;
                    let dur_field_names = ["years","months","weeks","days","hours","minutes","seconds","milliseconds","microseconds","nanoseconds"];
                    let mut this_vals = [0f64; 10];
                    for (i, field) in dur_field_names.iter().enumerate() {
                        this_vals[i] = obj.get(&PropertyKey::string(field)).and_then(|v| v.as_number()).unwrap_or(0.0);
                    }
                    // Parse other duration
                    let other_arg = args.first().cloned().unwrap_or(Value::undefined());
                    let other_obj = if let Some(o) = other_arg.as_object() {
                        o
                    } else {
                        return Err(VmError::type_error("add requires a Duration argument"));
                    };
                    let mut other_vals = [0f64; 10];
                    for (i, field) in dur_field_names.iter().enumerate() {
                        other_vals[i] = other_obj.get(&PropertyKey::string(field)).and_then(|v| v.as_number()).unwrap_or(0.0);
                    }
                    let result_args: Vec<Value> = (0..10).map(|i| Value::number(this_vals[i] + other_vals[i])).collect();
                    ncx.call_function_construct(&add_dur_ctor, Value::undefined(), &result_args)
                },
                mm.clone(), fn_proto.clone(), "add", 1,
            );
            stub_proto.define_property(PropertyKey::string("add"), PropertyDescriptor::builtin_method(add_fn));

            // @@toStringTag for Duration
            stub_proto.define_property(
                PropertyKey::Symbol(crate::intrinsics::well_known::to_string_tag_symbol()),
                PropertyDescriptor::data_with_attrs(
                    Value::string(JsString::intern("Temporal.Duration")),
                    PropertyAttributes { writable: false, enumerable: false, configurable: true },
                ),
            );
        }

        temporal_obj.define_property(
            PropertyKey::string(name),
            PropertyDescriptor::data_with_attrs(
                stub_value,
                PropertyAttributes::builtin_method(),
            ),
        );
    }

    // Install Temporal on global as non-enumerable
    global.define_property(
        PropertyKey::string("Temporal"),
        PropertyDescriptor::data_with_attrs(
            Value::object(temporal_obj),
            PropertyAttributes::builtin_method(),
        ),
    );
}
