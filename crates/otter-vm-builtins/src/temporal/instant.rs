//! Temporal.Instant - fixed point in time (nanosecond precision)

use otter_vm_core::error::VmError;
use otter_vm_core::string::JsString;
use otter_vm_core::value::Value;
use otter_vm_runtime::{Op, op_native};

pub fn ops() -> Vec<Op> {
    vec![
        op_native("__Temporal_Instant_from", instant_from),
        op_native(
            "__Temporal_Instant_fromEpochSeconds",
            instant_from_epoch_seconds,
        ),
        op_native(
            "__Temporal_Instant_fromEpochMilliseconds",
            instant_from_epoch_milliseconds,
        ),
        op_native(
            "__Temporal_Instant_fromEpochMicroseconds",
            instant_from_epoch_microseconds,
        ),
        op_native(
            "__Temporal_Instant_fromEpochNanoseconds",
            instant_from_epoch_nanoseconds,
        ),
        op_native("__Temporal_Instant_epochSeconds", instant_epoch_seconds),
        op_native(
            "__Temporal_Instant_epochMilliseconds",
            instant_epoch_milliseconds,
        ),
        op_native(
            "__Temporal_Instant_epochMicroseconds",
            instant_epoch_microseconds,
        ),
        op_native(
            "__Temporal_Instant_epochNanoseconds",
            instant_epoch_nanoseconds,
        ),
        op_native("__Temporal_Instant_add", instant_add),
        op_native("__Temporal_Instant_subtract", instant_subtract),
        op_native("__Temporal_Instant_until", instant_until),
        op_native("__Temporal_Instant_since", instant_since),
        op_native("__Temporal_Instant_round", instant_round),
        op_native("__Temporal_Instant_equals", instant_equals),
        op_native("__Temporal_Instant_toString", instant_to_string),
        op_native("__Temporal_Instant_toJSON", instant_to_json),
        op_native("__Temporal_Instant_valueOf", instant_value_of),
        op_native(
            "__Temporal_Instant_toZonedDateTimeISO",
            instant_to_zoned_date_time_iso,
        ),
    ]
}

/// Parse nanoseconds from string (Instant stores as string due to precision)
fn parse_nanos(args: &[Value]) -> Option<i128> {
    args.first().and_then(|v| {
        if let Some(s) = v.as_string() {
            s.as_str().parse::<i128>().ok()
        } else if let Some(n) = v.as_number() {
            Some(n as i128)
        } else {
            v.as_int32().map(|n| n as i128)
        }
    })
}

/// Temporal.Instant.from(thing) - create from ISO string or another Instant
fn instant_from(args: &[Value]) -> Result<Value, VmError> {
    let s = match args.first() {
        Some(v) if v.is_string() => v.as_string().unwrap().to_string(),
        _ => {
            return Err(VmError::type_error(
                "Temporal.Instant.from requires a string",
            ));
        }
    };

    // Parse ISO 8601 instant format: 2026-01-23T12:30:45.123456789Z
    if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(&s) {
        let nanos = dt.timestamp_nanos_opt().unwrap_or(0);
        return Ok(Value::string(JsString::intern(&nanos.to_string())));
    }

    // Try simpler formats
    if s.ends_with('Z') {
        let without_z = &s[..s.len() - 1];
        if let Ok(naive) = chrono::NaiveDateTime::parse_from_str(without_z, "%Y-%m-%dT%H:%M:%S%.f")
        {
            let nanos = naive.and_utc().timestamp_nanos_opt().unwrap_or(0);
            return Ok(Value::string(JsString::intern(&nanos.to_string())));
        }
    }

    Err(VmError::type_error(format!(
        "Invalid Instant string: {}",
        s
    )))
}

/// Temporal.Instant.fromEpochSeconds(epochSeconds)
fn instant_from_epoch_seconds(args: &[Value]) -> Result<Value, VmError> {
    let secs = args
        .first()
        .and_then(|v| v.as_number().or_else(|| v.as_int32().map(|n| n as f64)))
        .unwrap_or(0.0);

    let nanos = (secs * 1_000_000_000.0) as i128;
    Ok(Value::string(JsString::intern(&nanos.to_string())))
}

/// Temporal.Instant.fromEpochMilliseconds(epochMilliseconds)
fn instant_from_epoch_milliseconds(args: &[Value]) -> Result<Value, VmError> {
    let ms = args
        .first()
        .and_then(|v| v.as_number().or_else(|| v.as_int32().map(|n| n as f64)))
        .unwrap_or(0.0);

    let nanos = (ms * 1_000_000.0) as i128;
    Ok(Value::string(JsString::intern(&nanos.to_string())))
}

/// Temporal.Instant.fromEpochMicroseconds(epochMicroseconds)
fn instant_from_epoch_microseconds(args: &[Value]) -> Result<Value, VmError> {
    let us = args
        .first()
        .and_then(|v| v.as_string().and_then(|s| s.as_str().parse::<i128>().ok()))
        .or_else(|| args.first().and_then(|v| v.as_number().map(|n| n as i128)))
        .unwrap_or(0);

    let nanos = us * 1_000;
    Ok(Value::string(JsString::intern(&nanos.to_string())))
}

/// Temporal.Instant.fromEpochNanoseconds(epochNanoseconds)
fn instant_from_epoch_nanoseconds(args: &[Value]) -> Result<Value, VmError> {
    let nanos = args
        .first()
        .and_then(|v| v.as_string().and_then(|s| s.as_str().parse::<i128>().ok()))
        .or_else(|| args.first().and_then(|v| v.as_number().map(|n| n as i128)))
        .unwrap_or(0);

    Ok(Value::string(JsString::intern(&nanos.to_string())))
}

/// instant.epochSeconds
fn instant_epoch_seconds(args: &[Value]) -> Result<Value, VmError> {
    match parse_nanos(args) {
        Some(nanos) => Ok(Value::number((nanos / 1_000_000_000) as f64)),
        None => Err(VmError::type_error("Invalid Instant")),
    }
}

/// instant.epochMilliseconds
fn instant_epoch_milliseconds(args: &[Value]) -> Result<Value, VmError> {
    match parse_nanos(args) {
        Some(nanos) => Ok(Value::number((nanos / 1_000_000) as f64)),
        None => Err(VmError::type_error("Invalid Instant")),
    }
}

/// instant.epochMicroseconds (returns string for precision)
fn instant_epoch_microseconds(args: &[Value]) -> Result<Value, VmError> {
    match parse_nanos(args) {
        Some(nanos) => Ok(Value::string(JsString::intern(
            &(nanos / 1_000).to_string(),
        ))),
        None => Err(VmError::type_error("Invalid Instant")),
    }
}

/// instant.epochNanoseconds (returns string for precision)
fn instant_epoch_nanoseconds(args: &[Value]) -> Result<Value, VmError> {
    match parse_nanos(args) {
        Some(nanos) => Ok(Value::string(JsString::intern(&nanos.to_string()))),
        None => Err(VmError::type_error("Invalid Instant")),
    }
}

/// instant.add(duration) - returns new Instant
fn instant_add(args: &[Value]) -> Result<Value, VmError> {
    let nanos = parse_nanos(args).ok_or(VmError::type_error("Invalid Instant"))?;

    // Duration is passed as nanoseconds string in second arg
    let duration_nanos = args
        .get(1)
        .and_then(|v| v.as_string().and_then(|s| s.as_str().parse::<i128>().ok()))
        .or_else(|| args.get(1).and_then(|v| v.as_number().map(|n| n as i128)))
        .unwrap_or(0);

    let result = nanos + duration_nanos;
    Ok(Value::string(JsString::intern(&result.to_string())))
}

/// instant.subtract(duration) - returns new Instant
fn instant_subtract(args: &[Value]) -> Result<Value, VmError> {
    let nanos = parse_nanos(args).ok_or(VmError::type_error("Invalid Instant"))?;

    let duration_nanos = args
        .get(1)
        .and_then(|v| v.as_string().and_then(|s| s.as_str().parse::<i128>().ok()))
        .or_else(|| args.get(1).and_then(|v| v.as_number().map(|n| n as i128)))
        .unwrap_or(0);

    let result = nanos - duration_nanos;
    Ok(Value::string(JsString::intern(&result.to_string())))
}

/// instant.until(other) - returns Duration as nanoseconds string
fn instant_until(args: &[Value]) -> Result<Value, VmError> {
    let nanos = parse_nanos(args).ok_or(VmError::type_error("Invalid Instant"))?;

    let other_nanos = args
        .get(1)
        .and_then(|v| v.as_string().and_then(|s| s.as_str().parse::<i128>().ok()))
        .ok_or(VmError::type_error("Invalid target Instant"))?;

    let diff = other_nanos - nanos;
    Ok(Value::string(JsString::intern(&diff.to_string())))
}

/// instant.since(other) - returns Duration as nanoseconds string
fn instant_since(args: &[Value]) -> Result<Value, VmError> {
    let nanos = parse_nanos(args).ok_or(VmError::type_error("Invalid Instant"))?;

    let other_nanos = args
        .get(1)
        .and_then(|v| v.as_string().and_then(|s| s.as_str().parse::<i128>().ok()))
        .ok_or(VmError::type_error("Invalid target Instant"))?;

    let diff = nanos - other_nanos;
    Ok(Value::string(JsString::intern(&diff.to_string())))
}

/// instant.round(options) - round to nearest unit
fn instant_round(args: &[Value]) -> Result<Value, VmError> {
    let nanos = parse_nanos(args).ok_or(VmError::type_error("Invalid Instant"))?;

    // Get smallest unit from options (simplified - just support common units)
    let unit = args
        .get(1)
        .and_then(|v| v.as_string())
        .map(|s| s.to_string());

    let rounded = match unit.as_deref() {
        Some("hour") => (nanos / 3_600_000_000_000) * 3_600_000_000_000,
        Some("minute") => (nanos / 60_000_000_000) * 60_000_000_000,
        Some("second") => (nanos / 1_000_000_000) * 1_000_000_000,
        Some("millisecond") => (nanos / 1_000_000) * 1_000_000,
        Some("microsecond") => (nanos / 1_000) * 1_000,
        _ => nanos, // nanosecond or unknown - no rounding
    };

    Ok(Value::string(JsString::intern(&rounded.to_string())))
}

/// instant.equals(other)
fn instant_equals(args: &[Value]) -> Result<Value, VmError> {
    let nanos = parse_nanos(args);
    let other_nanos = args
        .get(1)
        .and_then(|v| v.as_string().and_then(|s| s.as_str().parse::<i128>().ok()));

    match (nanos, other_nanos) {
        (Some(a), Some(b)) => Ok(Value::boolean(a == b)),
        _ => Ok(Value::boolean(false)),
    }
}

/// instant.toString() - returns ISO 8601 string
fn instant_to_string(args: &[Value]) -> Result<Value, VmError> {
    let nanos = parse_nanos(args).ok_or(VmError::type_error("Invalid Instant"))?;

    let secs = nanos / 1_000_000_000;
    let sub_nanos = (nanos % 1_000_000_000) as u32;

    if let Some(dt) = chrono::DateTime::from_timestamp(secs as i64, sub_nanos) {
        let s = dt.format("%Y-%m-%dT%H:%M:%S%.9fZ").to_string();
        Ok(Value::string(JsString::intern(&s)))
    } else {
        Err(VmError::type_error("Invalid timestamp"))
    }
}

/// instant.toJSON() - same as toString
fn instant_to_json(args: &[Value]) -> Result<Value, VmError> {
    instant_to_string(args)
}

/// instant.valueOf() - throws TypeError (Temporal types are not comparable with <, >)
fn instant_value_of(_args: &[Value]) -> Result<Value, VmError> {
    Err(VmError::type_error(
        "Temporal.Instant cannot be converted to a primitive",
    ))
}

/// instant.toZonedDateTimeISO(timeZone) - convert to ZonedDateTime
fn instant_to_zoned_date_time_iso(args: &[Value]) -> Result<Value, VmError> {
    let nanos = parse_nanos(args).ok_or(VmError::type_error("Invalid Instant"))?;
    let tz = args
        .get(1)
        .and_then(|v| v.as_string())
        .map(|s| s.to_string())
        .unwrap_or_else(|| iana_time_zone::get_timezone().unwrap_or_else(|_| "UTC".to_string()));

    let secs = nanos / 1_000_000_000;
    let sub_nanos = (nanos % 1_000_000_000) as u32;

    if let Some(utc_dt) = chrono::DateTime::from_timestamp(secs as i64, sub_nanos) {
        // Parse timezone and convert
        if let Ok(tz_obj) = tz.parse::<chrono_tz::Tz>() {
            let dt = utc_dt.with_timezone(&tz_obj);
            let offset = dt.format("%:z").to_string();
            let s = format!(
                "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:09}{}[{}]",
                dt.year(),
                dt.month(),
                dt.day(),
                dt.hour(),
                dt.minute(),
                dt.second(),
                sub_nanos,
                offset,
                tz
            );
            return Ok(Value::string(JsString::intern(&s)));
        }
    }

    Err(VmError::type_error("Invalid timezone or timestamp"))
}

use chrono::{Datelike, Timelike};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_instant_from_epoch_milliseconds() {
        let args = vec![Value::number(1000.0)];
        let result = instant_from_epoch_milliseconds(&args).unwrap();
        let s = result.as_string().unwrap().to_string();
        assert_eq!(s, "1000000000"); // 1000ms = 1_000_000_000 ns
    }

    #[test]
    fn test_instant_epoch_seconds() {
        let args = vec![Value::string(JsString::intern("1000000000"))]; // 1 second in ns
        let result = instant_epoch_seconds(&args).unwrap();
        assert_eq!(result.as_number(), Some(1.0));
    }

    #[test]
    fn test_instant_equals() {
        let args = vec![
            Value::string(JsString::intern("1000")),
            Value::string(JsString::intern("1000")),
        ];
        let result = instant_equals(&args).unwrap();
        assert_eq!(result.as_boolean(), Some(true));
    }

    #[test]
    fn test_instant_add() {
        let args = vec![
            Value::string(JsString::intern("1000")),
            Value::string(JsString::intern("500")),
        ];
        let result = instant_add(&args).unwrap();
        let s = result.as_string().unwrap().to_string();
        assert_eq!(s, "1500");
    }
}
