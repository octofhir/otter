//! Temporal.Now - utilities for getting current time

use chrono::{Local, Utc};
use otter_vm_core::string::JsString;
use otter_vm_core::value::Value;
use otter_vm_runtime::{Op, op_native};

pub fn ops() -> Vec<Op> {
    vec![
        op_native("__Temporal_Now_instant", now_instant),
        op_native("__Temporal_Now_timeZoneId", now_timezone_id),
        op_native("__Temporal_Now_zonedDateTimeISO", now_zoned_date_time_iso),
        op_native("__Temporal_Now_plainDateTimeISO", now_plain_date_time_iso),
        op_native("__Temporal_Now_plainDateISO", now_plain_date_iso),
        op_native("__Temporal_Now_plainTimeISO", now_plain_time_iso),
    ]
}

/// Temporal.Now.instant() - returns current time as epochNanoseconds string
fn now_instant(_args: &[Value]) -> Result<Value, String> {
    let now = Utc::now();
    let nanos = now.timestamp_nanos_opt().unwrap_or(0);
    // Return as string since i128 doesn't fit in f64
    Ok(Value::string(JsString::intern(&nanos.to_string())))
}

/// Temporal.Now.timeZoneId() - returns current timezone IANA name
fn now_timezone_id(_args: &[Value]) -> Result<Value, String> {
    // Get system timezone name
    let tz = iana_time_zone::get_timezone().unwrap_or_else(|_| "UTC".to_string());
    Ok(Value::string(JsString::intern(&tz)))
}

/// Temporal.Now.zonedDateTimeISO() - returns current ZonedDateTime in ISO calendar
fn now_zoned_date_time_iso(_args: &[Value]) -> Result<Value, String> {
    let now = Local::now();
    let tz = iana_time_zone::get_timezone().unwrap_or_else(|_| "UTC".to_string());

    // Format: 2026-01-23T15:30:45.123456789-05:00[America/New_York]
    let nanos = now.timestamp_subsec_nanos();
    let s = format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:09}{}[{}]",
        now.year(),
        now.month(),
        now.day(),
        now.hour(),
        now.minute(),
        now.second(),
        nanos,
        now.format("%:z"),
        tz
    );
    Ok(Value::string(JsString::intern(&s)))
}

/// Temporal.Now.plainDateTimeISO() - returns current PlainDateTime in ISO calendar
fn now_plain_date_time_iso(_args: &[Value]) -> Result<Value, String> {
    let now = Local::now();
    let nanos = now.timestamp_subsec_nanos();
    let s = format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:09}",
        now.year(),
        now.month(),
        now.day(),
        now.hour(),
        now.minute(),
        now.second(),
        nanos
    );
    Ok(Value::string(JsString::intern(&s)))
}

/// Temporal.Now.plainDateISO() - returns current PlainDate in ISO calendar
fn now_plain_date_iso(_args: &[Value]) -> Result<Value, String> {
    let now = Local::now();
    let s = format!("{:04}-{:02}-{:02}", now.year(), now.month(), now.day());
    Ok(Value::string(JsString::intern(&s)))
}

/// Temporal.Now.plainTimeISO() - returns current PlainTime in ISO calendar
fn now_plain_time_iso(_args: &[Value]) -> Result<Value, String> {
    let now = Local::now();
    let nanos = now.timestamp_subsec_nanos();
    let s = format!(
        "{:02}:{:02}:{:02}.{:09}",
        now.hour(),
        now.minute(),
        now.second(),
        nanos
    );
    Ok(Value::string(JsString::intern(&s)))
}

use chrono::{Datelike, Timelike};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_now_instant() {
        let result = now_instant(&[]).unwrap();
        let s = result.as_string().unwrap().to_string();
        // Should be a large number (nanoseconds since epoch)
        let nanos: i128 = s.parse().unwrap();
        assert!(nanos > 1_700_000_000_000_000_000); // After 2023
    }

    #[test]
    fn test_now_timezone_id() {
        let result = now_timezone_id(&[]).unwrap();
        let tz = result.as_string().unwrap().to_string();
        // Should be a valid IANA timezone
        assert!(!tz.is_empty());
    }

    #[test]
    fn test_now_plain_date_iso() {
        let result = now_plain_date_iso(&[]).unwrap();
        let s = result.as_string().unwrap().to_string();
        // Should match YYYY-MM-DD format
        assert!(s.len() == 10);
        assert!(s.contains('-'));
    }
}
