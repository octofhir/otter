//! Temporal.Duration - represents a span of time
//!
//! Backed by `temporal_rs::Duration` for spec-compliant behavior.

use otter_vm_core::error::VmError;
use otter_vm_core::string::JsString;
use otter_vm_core::value::Value;
use otter_vm_runtime::{Op, op_native};
use temporal_rs::Duration;
use temporal_rs::options::ToStringRoundingOptions;

pub fn ops() -> Vec<Op> {
    vec![
        op_native("__Temporal_Duration_from", duration_from),
        op_native("__Temporal_Duration_compare", duration_compare),
        op_native("__Temporal_Duration_years", duration_years),
        op_native("__Temporal_Duration_months", duration_months),
        op_native("__Temporal_Duration_weeks", duration_weeks),
        op_native("__Temporal_Duration_days", duration_days),
        op_native("__Temporal_Duration_hours", duration_hours),
        op_native("__Temporal_Duration_minutes", duration_minutes),
        op_native("__Temporal_Duration_seconds", duration_seconds),
        op_native("__Temporal_Duration_milliseconds", duration_milliseconds),
        op_native("__Temporal_Duration_microseconds", duration_microseconds),
        op_native("__Temporal_Duration_nanoseconds", duration_nanoseconds),
        op_native("__Temporal_Duration_sign", duration_sign),
        op_native("__Temporal_Duration_blank", duration_blank),
        op_native("__Temporal_Duration_negated", duration_negated),
        op_native("__Temporal_Duration_abs", duration_abs),
        op_native("__Temporal_Duration_add", duration_add),
        op_native("__Temporal_Duration_subtract", duration_subtract),
        op_native("__Temporal_Duration_round", duration_round),
        op_native("__Temporal_Duration_total", duration_total),
        op_native("__Temporal_Duration_toString", duration_to_string),
        op_native("__Temporal_Duration_toJSON", duration_to_json),
    ]
}

fn parse_duration(args: &[Value]) -> Result<Duration, VmError> {
    let s = args
        .first()
        .and_then(|v| v.as_string())
        .ok_or_else(|| VmError::type_error("Invalid Duration"))?;
    Duration::from_utf8(s.as_str().as_bytes())
        .map_err(|e| VmError::type_error(format!("Invalid Duration string: {e}")))
}

fn format_duration(d: &Duration) -> String {
    d.as_temporal_string(ToStringRoundingOptions::default())
        .unwrap_or_else(|_| "PT0S".to_string())
}

fn duration_from(args: &[Value]) -> Result<Value, VmError> {
    let d = parse_duration(args)?;
    Ok(Value::string(JsString::intern(&format_duration(&d))))
}

fn duration_compare(args: &[Value]) -> Result<Value, VmError> {
    let d1 = parse_duration(args)?;
    let d2 = args
        .get(1)
        .ok_or_else(|| VmError::type_error("Invalid Duration for comparison"))
        .and_then(|v| parse_duration(&[v.clone()]))?;

    // Compare total nanoseconds for time-only durations
    let ns1 = d1.days() as i128 * 86_400_000_000_000
        + d1.hours() as i128 * 3_600_000_000_000
        + d1.minutes() as i128 * 60_000_000_000
        + d1.seconds() as i128 * 1_000_000_000
        + d1.milliseconds() as i128 * 1_000_000
        + d1.microseconds() as i128 * 1_000
        + d1.nanoseconds() as i128;
    let ns2 = d2.days() as i128 * 86_400_000_000_000
        + d2.hours() as i128 * 3_600_000_000_000
        + d2.minutes() as i128 * 60_000_000_000
        + d2.seconds() as i128 * 1_000_000_000
        + d2.milliseconds() as i128 * 1_000_000
        + d2.microseconds() as i128 * 1_000
        + d2.nanoseconds() as i128;

    Ok(Value::int32(match ns1.cmp(&ns2) {
        std::cmp::Ordering::Less => -1,
        std::cmp::Ordering::Equal => 0,
        std::cmp::Ordering::Greater => 1,
    }))
}

fn duration_years(args: &[Value]) -> Result<Value, VmError> {
    parse_duration(args).map(|d| Value::int32(d.years() as i32))
}

fn duration_months(args: &[Value]) -> Result<Value, VmError> {
    parse_duration(args).map(|d| Value::int32(d.months() as i32))
}

fn duration_weeks(args: &[Value]) -> Result<Value, VmError> {
    parse_duration(args).map(|d| Value::int32(d.weeks() as i32))
}

fn duration_days(args: &[Value]) -> Result<Value, VmError> {
    parse_duration(args).map(|d| Value::int32(d.days() as i32))
}

fn duration_hours(args: &[Value]) -> Result<Value, VmError> {
    parse_duration(args).map(|d| Value::int32(d.hours() as i32))
}

fn duration_minutes(args: &[Value]) -> Result<Value, VmError> {
    parse_duration(args).map(|d| Value::int32(d.minutes() as i32))
}

fn duration_seconds(args: &[Value]) -> Result<Value, VmError> {
    parse_duration(args).map(|d| Value::int32(d.seconds() as i32))
}

fn duration_milliseconds(args: &[Value]) -> Result<Value, VmError> {
    parse_duration(args).map(|d| Value::int32(d.milliseconds() as i32))
}

fn duration_microseconds(args: &[Value]) -> Result<Value, VmError> {
    parse_duration(args).map(|d| Value::int32(d.microseconds() as i32))
}

fn duration_nanoseconds(args: &[Value]) -> Result<Value, VmError> {
    parse_duration(args).map(|d| Value::int32(d.nanoseconds() as i32))
}

fn duration_sign(args: &[Value]) -> Result<Value, VmError> {
    parse_duration(args).map(|d| Value::int32(d.sign() as i8 as i32))
}

fn duration_blank(args: &[Value]) -> Result<Value, VmError> {
    parse_duration(args).map(|d| Value::boolean(d.is_zero()))
}

fn duration_negated(args: &[Value]) -> Result<Value, VmError> {
    let d = parse_duration(args)?;
    let neg = d.negated();
    Ok(Value::string(JsString::intern(&format_duration(&neg))))
}

fn duration_abs(args: &[Value]) -> Result<Value, VmError> {
    let d = parse_duration(args)?;
    let abs = d.abs();
    Ok(Value::string(JsString::intern(&format_duration(&abs))))
}

fn duration_add(args: &[Value]) -> Result<Value, VmError> {
    let d1 = parse_duration(args)?;
    let d2 = args
        .get(1)
        .ok_or_else(|| VmError::type_error("Invalid Duration to add"))
        .and_then(|v| parse_duration(&[v.clone()]))?;

    let result = Duration::new(
        d1.years() + d2.years(),
        d1.months() + d2.months(),
        d1.weeks() + d2.weeks(),
        d1.days() + d2.days(),
        d1.hours() + d2.hours(),
        d1.minutes() + d2.minutes(),
        d1.seconds() + d2.seconds(),
        d1.milliseconds() + d2.milliseconds(),
        d1.microseconds() + d2.microseconds(),
        d1.nanoseconds() + d2.nanoseconds(),
    )
    .map_err(|e| VmError::type_error(format!("Duration add error: {e}")))?;
    Ok(Value::string(JsString::intern(&format_duration(&result))))
}

fn duration_subtract(args: &[Value]) -> Result<Value, VmError> {
    let d1 = parse_duration(args)?;
    let d2 = args
        .get(1)
        .ok_or_else(|| VmError::type_error("Invalid Duration to subtract"))
        .and_then(|v| parse_duration(&[v.clone()]))?;

    let result = Duration::new(
        d1.years() - d2.years(),
        d1.months() - d2.months(),
        d1.weeks() - d2.weeks(),
        d1.days() - d2.days(),
        d1.hours() - d2.hours(),
        d1.minutes() - d2.minutes(),
        d1.seconds() - d2.seconds(),
        d1.milliseconds() - d2.milliseconds(),
        d1.microseconds() - d2.microseconds(),
        d1.nanoseconds() - d2.nanoseconds(),
    )
    .map_err(|e| VmError::type_error(format!("Duration subtract error: {e}")))?;
    Ok(Value::string(JsString::intern(&format_duration(&result))))
}

fn duration_round(args: &[Value]) -> Result<Value, VmError> {
    // Duration.round() requires relativeTo for calendar units;
    // for now return as-is (same as old impl)
    let d = parse_duration(args)?;
    Ok(Value::string(JsString::intern(&format_duration(&d))))
}

fn duration_total(args: &[Value]) -> Result<Value, VmError> {
    let d = parse_duration(args)?;
    let unit = args
        .get(1)
        .and_then(|v| v.as_string())
        .map(|s| s.to_string());

    let total_ns = d.days() as f64 * 86_400_000_000_000.0
        + d.hours() as f64 * 3_600_000_000_000.0
        + d.minutes() as f64 * 60_000_000_000.0
        + d.seconds() as f64 * 1_000_000_000.0
        + d.milliseconds() as f64 * 1_000_000.0
        + d.microseconds() as f64 * 1_000.0
        + d.nanoseconds() as f64;

    let result = match unit.as_deref() {
        Some("nanoseconds") | Some("nanosecond") => total_ns,
        Some("microseconds") | Some("microsecond") => total_ns / 1_000.0,
        Some("milliseconds") | Some("millisecond") => total_ns / 1_000_000.0,
        Some("seconds") | Some("second") => total_ns / 1_000_000_000.0,
        Some("minutes") | Some("minute") => total_ns / 60_000_000_000.0,
        Some("hours") | Some("hour") => total_ns / 3_600_000_000_000.0,
        Some("days") | Some("day") => total_ns / 86_400_000_000_000.0,
        Some("weeks") | Some("week") => total_ns / 604_800_000_000_000.0,
        _ => total_ns / 1_000_000_000.0,
    };

    Ok(Value::number(result))
}

fn duration_to_string(args: &[Value]) -> Result<Value, VmError> {
    let d = parse_duration(args)?;
    Ok(Value::string(JsString::intern(&format_duration(&d))))
}

fn duration_to_json(args: &[Value]) -> Result<Value, VmError> {
    duration_to_string(args)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_duration_from() {
        let args = vec![Value::string(JsString::intern("P1Y2M3DT4H5M6S"))];
        let result = duration_from(&args).unwrap();
        let s = result.as_string().unwrap().to_string();
        assert!(s.contains("1Y"));
        assert!(s.contains("2M"));
        assert!(s.contains("3D"));
    }

    #[test]
    fn test_duration_days() {
        let args = vec![Value::string(JsString::intern("P5D"))];
        let result = duration_days(&args).unwrap();
        assert_eq!(result.as_int32(), Some(5));
    }

    #[test]
    fn test_duration_total() {
        let args = vec![
            Value::string(JsString::intern("PT1H")),
            Value::string(JsString::intern("minutes")),
        ];
        let result = duration_total(&args).unwrap();
        assert_eq!(result.as_number(), Some(60.0));
    }

    #[test]
    fn test_duration_add() {
        let args = vec![
            Value::string(JsString::intern("P1D")),
            Value::string(JsString::intern("P2D")),
        ];
        let result = duration_add(&args).unwrap();
        let s = result.as_string().unwrap().to_string();
        assert!(s.contains("3D"));
    }
}
