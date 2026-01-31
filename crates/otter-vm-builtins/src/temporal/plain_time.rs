//! Temporal.PlainTime - time without date or timezone

use chrono::NaiveTime;
use otter_vm_core::value::Value;
use otter_vm_core::{VmError, string::JsString};
use otter_vm_runtime::{Op, op_native};

pub fn ops() -> Vec<Op> {
    vec![
        op_native("__Temporal_PlainTime_from", plain_time_from),
        op_native("__Temporal_PlainTime_compare", plain_time_compare),
        op_native("__Temporal_PlainTime_hour", plain_time_hour),
        op_native("__Temporal_PlainTime_minute", plain_time_minute),
        op_native("__Temporal_PlainTime_second", plain_time_second),
        op_native("__Temporal_PlainTime_millisecond", plain_time_millisecond),
        op_native("__Temporal_PlainTime_microsecond", plain_time_microsecond),
        op_native("__Temporal_PlainTime_nanosecond", plain_time_nanosecond),
        op_native("__Temporal_PlainTime_add", plain_time_add),
        op_native("__Temporal_PlainTime_subtract", plain_time_subtract),
        op_native("__Temporal_PlainTime_until", plain_time_until),
        op_native("__Temporal_PlainTime_since", plain_time_since),
        op_native("__Temporal_PlainTime_with", plain_time_with),
        op_native("__Temporal_PlainTime_round", plain_time_round),
        op_native("__Temporal_PlainTime_equals", plain_time_equals),
        op_native("__Temporal_PlainTime_toString", plain_time_to_string),
        op_native("__Temporal_PlainTime_toJSON", plain_time_to_json),
        op_native(
            "__Temporal_PlainTime_toPlainDateTime",
            plain_time_to_plain_date_time,
        ),
    ]
}

/// Parse time string to NaiveTime and extra nanoseconds
fn parse_time(s: &str) -> Option<(NaiveTime, u32)> {
    // Handle formats: HH:MM:SS.nnnnnnnnn, HH:MM:SS, HH:MM
    let parts: Vec<&str> = s.split('.').collect();
    let time_part = parts[0];

    let time = if time_part.len() == 5 {
        // HH:MM format
        NaiveTime::parse_from_str(time_part, "%H:%M").ok()
    } else {
        NaiveTime::parse_from_str(time_part, "%H:%M:%S").ok()
    };

    let extra_nanos = if parts.len() > 1 {
        // Parse fractional seconds (up to 9 digits for nanoseconds)
        let frac = parts[1];
        let padded = format!("{:0<9}", frac);
        padded[..9].parse::<u32>().unwrap_or(0)
    } else {
        0
    };

    time.map(|t| (t, extra_nanos))
}

fn get_time(args: &[Value]) -> Option<(NaiveTime, u32)> {
    args.first()
        .and_then(|v| v.as_string())
        .and_then(|s| parse_time(s.as_str()))
}

fn format_time(t: NaiveTime, nanos: u32) -> String {
    format!(
        "{:02}:{:02}:{:02}.{:09}",
        t.hour(),
        t.minute(),
        t.second(),
        nanos
    )
}

fn plain_time_from(args: &[Value]) -> Result<Value, VmError> {
    let s = args
        .first()
        .and_then(|v| v.as_string())
        .ok_or(VmError::type_error("PlainTime.from requires a string"))?;

    // Extract time part if ISO datetime
    let time_str = if s.as_str().contains('T') {
        s.as_str().split('T').nth(1).unwrap_or(s.as_str())
    } else {
        s.as_str()
    };

    // Remove timezone info if present
    let time_str = time_str.split('+').next().unwrap_or(time_str);
    let time_str = time_str.split('-').next().unwrap_or(time_str);
    let time_str = time_str.trim_end_matches('Z');

    match parse_time(time_str) {
        Some((t, nanos)) => Ok(Value::string(JsString::intern(&format_time(t, nanos)))),
        None => Err(VmError::type_error(format!("Invalid time string: {}", s))),
    }
}

fn plain_time_compare(args: &[Value]) -> Result<Value, VmError> {
    let t1 = get_time(args);
    let t2 = args
        .get(1)
        .and_then(|v| v.as_string())
        .and_then(|s| parse_time(s.as_str()));

    match (t1, t2) {
        (Some((a, an)), Some((b, bn))) => {
            let cmp = (a, an).cmp(&(b, bn));
            Ok(Value::int32(match cmp {
                std::cmp::Ordering::Less => -1,
                std::cmp::Ordering::Equal => 0,
                std::cmp::Ordering::Greater => 1,
            }))
        }
        _ => Err(VmError::type_error("Invalid times for comparison")),
    }
}

fn plain_time_hour(args: &[Value]) -> Result<Value, VmError> {
    get_time(args)
        .map(|(t, _)| Value::int32(t.hour() as i32))
        .ok_or_else(|| VmError::type_error("Invalid PlainTime"))
}

fn plain_time_minute(args: &[Value]) -> Result<Value, VmError> {
    get_time(args)
        .map(|(t, _)| Value::int32(t.minute() as i32))
        .ok_or_else(|| VmError::type_error("Invalid PlainTime"))
}

fn plain_time_second(args: &[Value]) -> Result<Value, VmError> {
    get_time(args)
        .map(|(t, _)| Value::int32(t.second() as i32))
        .ok_or_else(|| VmError::type_error("Invalid PlainTime"))
}

fn plain_time_millisecond(args: &[Value]) -> Result<Value, VmError> {
    get_time(args)
        .map(|(_, nanos)| Value::int32((nanos / 1_000_000) as i32))
        .ok_or_else(|| VmError::type_error("Invalid PlainTime"))
}

fn plain_time_microsecond(args: &[Value]) -> Result<Value, VmError> {
    get_time(args)
        .map(|(_, nanos)| Value::int32(((nanos / 1_000) % 1_000) as i32))
        .ok_or_else(|| VmError::type_error("Invalid PlainTime"))
}

fn plain_time_nanosecond(args: &[Value]) -> Result<Value, VmError> {
    get_time(args)
        .map(|(_, nanos)| Value::int32((nanos % 1_000) as i32))
        .ok_or_else(|| VmError::type_error("Invalid PlainTime"))
}

fn plain_time_add(args: &[Value]) -> Result<Value, VmError> {
    let (time, nanos) = get_time(args).ok_or(VmError::type_error("Invalid PlainTime"))?;
    let add_nanos = args.get(1).and_then(|v| v.as_int32()).unwrap_or(0) as i64;

    let total_nanos =
        time.num_seconds_from_midnight() as i64 * 1_000_000_000 + nanos as i64 + add_nanos;
    let total_nanos = total_nanos.rem_euclid(86_400_000_000_000); // Wrap around midnight

    let secs = (total_nanos / 1_000_000_000) as u32;
    let new_nanos = (total_nanos % 1_000_000_000) as u32;
    let new_time = NaiveTime::from_num_seconds_from_midnight_opt(secs, 0).unwrap();

    Ok(Value::string(JsString::intern(&format_time(
        new_time, new_nanos,
    ))))
}

fn plain_time_subtract(args: &[Value]) -> Result<Value, VmError> {
    let (time, nanos) = get_time(args).ok_or(VmError::type_error("Invalid PlainTime"))?;
    let sub_nanos = args.get(1).and_then(|v| v.as_int32()).unwrap_or(0) as i64;

    let total_nanos =
        time.num_seconds_from_midnight() as i64 * 1_000_000_000 + nanos as i64 - sub_nanos;
    let total_nanos = total_nanos.rem_euclid(86_400_000_000_000);

    let secs = (total_nanos / 1_000_000_000) as u32;
    let new_nanos = (total_nanos % 1_000_000_000) as u32;
    let new_time = NaiveTime::from_num_seconds_from_midnight_opt(secs, 0).unwrap();

    Ok(Value::string(JsString::intern(&format_time(
        new_time, new_nanos,
    ))))
}

fn plain_time_until(args: &[Value]) -> Result<Value, VmError> {
    let (t1, n1) = get_time(args).ok_or(VmError::type_error("Invalid PlainTime"))?;
    let (t2, n2) = args
        .get(1)
        .and_then(|v| v.as_string())
        .and_then(|s| parse_time(s.as_str()))
        .ok_or(VmError::type_error("Invalid target time"))?;

    let nanos1 = t1.num_seconds_from_midnight() as i64 * 1_000_000_000 + n1 as i64;
    let nanos2 = t2.num_seconds_from_midnight() as i64 * 1_000_000_000 + n2 as i64;

    Ok(Value::string(JsString::intern(
        &(nanos2 - nanos1).to_string(),
    )))
}

fn plain_time_since(args: &[Value]) -> Result<Value, VmError> {
    let (t1, n1) = get_time(args).ok_or(VmError::type_error("Invalid PlainTime"))?;
    let (t2, n2) = args
        .get(1)
        .and_then(|v| v.as_string())
        .and_then(|s| parse_time(s.as_str()))
        .ok_or(VmError::type_error("Invalid target time"))?;

    let nanos1 = t1.num_seconds_from_midnight() as i64 * 1_000_000_000 + n1 as i64;
    let nanos2 = t2.num_seconds_from_midnight() as i64 * 1_000_000_000 + n2 as i64;

    Ok(Value::string(JsString::intern(
        &(nanos1 - nanos2).to_string(),
    )))
}

fn plain_time_with(args: &[Value]) -> Result<Value, VmError> {
    let (time, nanos) = get_time(args).ok_or(VmError::type_error("Invalid PlainTime"))?;

    let hour = args
        .get(1)
        .and_then(|v| v.as_int32())
        .map(|h| h as u32)
        .unwrap_or(time.hour());
    let minute = args
        .get(2)
        .and_then(|v| v.as_int32())
        .map(|m| m as u32)
        .unwrap_or(time.minute());
    let second = args
        .get(3)
        .and_then(|v| v.as_int32())
        .map(|s| s as u32)
        .unwrap_or(time.second());
    let new_nanos = args
        .get(4)
        .and_then(|v| v.as_int32())
        .map(|n| n as u32)
        .unwrap_or(nanos);

    NaiveTime::from_hms_opt(hour, minute, second)
        .map(|t| Value::string(JsString::intern(&format_time(t, new_nanos))))
        .ok_or_else(|| VmError::type_error("Invalid time components"))
}

fn plain_time_round(args: &[Value]) -> Result<Value, VmError> {
    let (time, nanos) = get_time(args).ok_or(VmError::type_error("Invalid PlainTime"))?;
    let unit = args
        .get(1)
        .and_then(|v| v.as_string())
        .map(|s| s.to_string());

    let total_nanos = time.num_seconds_from_midnight() as i64 * 1_000_000_000 + nanos as i64;

    let rounded = match unit.as_deref() {
        Some("hour") => (total_nanos / 3_600_000_000_000) * 3_600_000_000_000,
        Some("minute") => (total_nanos / 60_000_000_000) * 60_000_000_000,
        Some("second") => (total_nanos / 1_000_000_000) * 1_000_000_000,
        Some("millisecond") => (total_nanos / 1_000_000) * 1_000_000,
        Some("microsecond") => (total_nanos / 1_000) * 1_000,
        _ => total_nanos,
    };

    let secs = (rounded / 1_000_000_000) as u32;
    let new_nanos = (rounded % 1_000_000_000) as u32;
    let new_time = NaiveTime::from_num_seconds_from_midnight_opt(secs % 86400, 0).unwrap();

    Ok(Value::string(JsString::intern(&format_time(
        new_time, new_nanos,
    ))))
}

fn plain_time_equals(args: &[Value]) -> Result<Value, VmError> {
    let t1 = get_time(args);
    let t2 = args
        .get(1)
        .and_then(|v| v.as_string())
        .and_then(|s| parse_time(s.as_str()));

    Ok(Value::boolean(t1 == t2))
}

fn plain_time_to_string(args: &[Value]) -> Result<Value, VmError> {
    get_time(args)
        .map(|(t, nanos)| Value::string(JsString::intern(&format_time(t, nanos))))
        .ok_or_else(|| VmError::type_error("Invalid PlainTime"))
}

fn plain_time_to_json(args: &[Value]) -> Result<Value, VmError> {
    plain_time_to_string(args)
}

fn plain_time_to_plain_date_time(args: &[Value]) -> Result<Value, VmError> {
    let (time, nanos) = get_time(args).ok_or(VmError::type_error("Invalid PlainTime"))?;
    let date = args
        .get(1)
        .and_then(|v| v.as_string())
        .map(|s| s.to_string())
        .unwrap_or_else(|| "1970-01-01".to_owned());

    Ok(Value::string(JsString::intern(&format!(
        "{}T{}",
        date,
        format_time(time, nanos)
    ))))
}

use chrono::Timelike;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_plain_time_from() {
        let args = vec![Value::string(JsString::intern("12:30:45.123456789"))];
        let result = plain_time_from(&args).unwrap();
        let s = result.as_string().unwrap().to_string();
        assert!(s.starts_with("12:30:45"));
    }

    #[test]
    fn test_plain_time_hour() {
        let args = vec![Value::string(JsString::intern("12:30:45.000000000"))];
        let result = plain_time_hour(&args).unwrap();
        assert_eq!(result.as_int32(), Some(12));
    }

    #[test]
    fn test_plain_time_compare() {
        let args = vec![
            Value::string(JsString::intern("12:30:00.000000000")),
            Value::string(JsString::intern("12:30:01.000000000")),
        ];
        let result = plain_time_compare(&args).unwrap();
        assert_eq!(result.as_int32(), Some(-1));
    }
}
