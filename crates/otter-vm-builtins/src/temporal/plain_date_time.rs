//! Temporal.PlainDateTime - date and time without timezone

use chrono::{Datelike, NaiveDateTime, Timelike};
use otter_vm_core::string::JsString;
use otter_vm_core::value::Value;
use otter_vm_runtime::{Op, op_native};

pub fn ops() -> Vec<Op> {
    vec![
        op_native("__Temporal_PlainDateTime_from", plain_date_time_from),
        op_native("__Temporal_PlainDateTime_compare", plain_date_time_compare),
        op_native("__Temporal_PlainDateTime_year", plain_date_time_year),
        op_native("__Temporal_PlainDateTime_month", plain_date_time_month),
        op_native("__Temporal_PlainDateTime_day", plain_date_time_day),
        op_native("__Temporal_PlainDateTime_hour", plain_date_time_hour),
        op_native("__Temporal_PlainDateTime_minute", plain_date_time_minute),
        op_native("__Temporal_PlainDateTime_second", plain_date_time_second),
        op_native(
            "__Temporal_PlainDateTime_millisecond",
            plain_date_time_millisecond,
        ),
        op_native("__Temporal_PlainDateTime_add", plain_date_time_add),
        op_native(
            "__Temporal_PlainDateTime_subtract",
            plain_date_time_subtract,
        ),
        op_native("__Temporal_PlainDateTime_with", plain_date_time_with),
        op_native("__Temporal_PlainDateTime_equals", plain_date_time_equals),
        op_native(
            "__Temporal_PlainDateTime_toString",
            plain_date_time_to_string,
        ),
        op_native("__Temporal_PlainDateTime_toJSON", plain_date_time_to_json),
        op_native(
            "__Temporal_PlainDateTime_toPlainDate",
            plain_date_time_to_plain_date,
        ),
        op_native(
            "__Temporal_PlainDateTime_toPlainTime",
            plain_date_time_to_plain_time,
        ),
        op_native(
            "__Temporal_PlainDateTime_toZonedDateTime",
            plain_date_time_to_zoned_date_time,
        ),
    ]
}

fn parse_date_time(s: &str) -> Option<(NaiveDateTime, u32)> {
    // Handle ISO format: YYYY-MM-DDTHH:MM:SS.nnnnnnnnn
    let s = s.trim_end_matches('Z');
    let s = s.split('+').next().unwrap_or(s);
    let s = s.split('[').next().unwrap_or(s);

    let parts: Vec<&str> = s.split('.').collect();
    let dt_part = parts[0];

    let dt = NaiveDateTime::parse_from_str(dt_part, "%Y-%m-%dT%H:%M:%S")
        .ok()
        .or_else(|| NaiveDateTime::parse_from_str(dt_part, "%Y-%m-%d %H:%M:%S").ok());

    let extra_nanos = if parts.len() > 1 {
        let frac = parts[1];
        let padded = format!("{:0<9}", frac);
        padded[..9].parse::<u32>().unwrap_or(0)
    } else {
        0
    };

    dt.map(|d| (d, extra_nanos))
}

fn get_date_time(args: &[Value]) -> Option<(NaiveDateTime, u32)> {
    args.first()
        .and_then(|v| v.as_string())
        .and_then(|s| parse_date_time(s.as_str()))
}

fn format_date_time(dt: NaiveDateTime, nanos: u32) -> String {
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:09}",
        dt.year(),
        dt.month(),
        dt.day(),
        dt.hour(),
        dt.minute(),
        dt.second(),
        nanos
    )
}

fn plain_date_time_from(args: &[Value]) -> Result<Value, String> {
    let s = args
        .first()
        .and_then(|v| v.as_string())
        .ok_or("PlainDateTime.from requires a string")?;

    match parse_date_time(s.as_str()) {
        Some((dt, nanos)) => Ok(Value::string(JsString::intern(&format_date_time(
            dt, nanos,
        )))),
        None => Err(format!("Invalid PlainDateTime string: {}", s)),
    }
}

fn plain_date_time_compare(args: &[Value]) -> Result<Value, String> {
    let dt1 = get_date_time(args);
    let dt2 = args
        .get(1)
        .and_then(|v| v.as_string())
        .and_then(|s| parse_date_time(s.as_str()));

    match (dt1, dt2) {
        (Some(a), Some(b)) => {
            let cmp = a.cmp(&b);
            Ok(Value::int32(match cmp {
                std::cmp::Ordering::Less => -1,
                std::cmp::Ordering::Equal => 0,
                std::cmp::Ordering::Greater => 1,
            }))
        }
        _ => Err("Invalid PlainDateTime for comparison".to_string()),
    }
}

fn plain_date_time_year(args: &[Value]) -> Result<Value, String> {
    get_date_time(args)
        .map(|(dt, _)| Value::int32(dt.year()))
        .ok_or_else(|| "Invalid PlainDateTime".to_string())
}

fn plain_date_time_month(args: &[Value]) -> Result<Value, String> {
    get_date_time(args)
        .map(|(dt, _)| Value::int32(dt.month() as i32))
        .ok_or_else(|| "Invalid PlainDateTime".to_string())
}

fn plain_date_time_day(args: &[Value]) -> Result<Value, String> {
    get_date_time(args)
        .map(|(dt, _)| Value::int32(dt.day() as i32))
        .ok_or_else(|| "Invalid PlainDateTime".to_string())
}

fn plain_date_time_hour(args: &[Value]) -> Result<Value, String> {
    get_date_time(args)
        .map(|(dt, _)| Value::int32(dt.hour() as i32))
        .ok_or_else(|| "Invalid PlainDateTime".to_string())
}

fn plain_date_time_minute(args: &[Value]) -> Result<Value, String> {
    get_date_time(args)
        .map(|(dt, _)| Value::int32(dt.minute() as i32))
        .ok_or_else(|| "Invalid PlainDateTime".to_string())
}

fn plain_date_time_second(args: &[Value]) -> Result<Value, String> {
    get_date_time(args)
        .map(|(dt, _)| Value::int32(dt.second() as i32))
        .ok_or_else(|| "Invalid PlainDateTime".to_string())
}

fn plain_date_time_millisecond(args: &[Value]) -> Result<Value, String> {
    get_date_time(args)
        .map(|(_, nanos)| Value::int32((nanos / 1_000_000) as i32))
        .ok_or_else(|| "Invalid PlainDateTime".to_string())
}

fn plain_date_time_add(args: &[Value]) -> Result<Value, String> {
    let (dt, nanos) = get_date_time(args).ok_or("Invalid PlainDateTime")?;
    let add_days = args.get(1).and_then(|v| v.as_int32()).unwrap_or(0) as i64;

    let new_dt = dt + chrono::Duration::days(add_days);
    Ok(Value::string(JsString::intern(&format_date_time(
        new_dt, nanos,
    ))))
}

fn plain_date_time_subtract(args: &[Value]) -> Result<Value, String> {
    let (dt, nanos) = get_date_time(args).ok_or("Invalid PlainDateTime")?;
    let sub_days = args.get(1).and_then(|v| v.as_int32()).unwrap_or(0) as i64;

    let new_dt = dt - chrono::Duration::days(sub_days);
    Ok(Value::string(JsString::intern(&format_date_time(
        new_dt, nanos,
    ))))
}

fn plain_date_time_with(args: &[Value]) -> Result<Value, String> {
    let (dt, nanos) = get_date_time(args).ok_or("Invalid PlainDateTime")?;

    let year = args.get(1).and_then(|v| v.as_int32()).unwrap_or(dt.year());
    let month = args
        .get(2)
        .and_then(|v| v.as_int32())
        .map(|m| m as u32)
        .unwrap_or(dt.month());
    let day = args
        .get(3)
        .and_then(|v| v.as_int32())
        .map(|d| d as u32)
        .unwrap_or(dt.day());
    let hour = args
        .get(4)
        .and_then(|v| v.as_int32())
        .map(|h| h as u32)
        .unwrap_or(dt.hour());
    let minute = args
        .get(5)
        .and_then(|v| v.as_int32())
        .map(|m| m as u32)
        .unwrap_or(dt.minute());
    let second = args
        .get(6)
        .and_then(|v| v.as_int32())
        .map(|s| s as u32)
        .unwrap_or(dt.second());

    chrono::NaiveDate::from_ymd_opt(year, month, day)
        .and_then(|d| d.and_hms_opt(hour, minute, second))
        .map(|new_dt| Value::string(JsString::intern(&format_date_time(new_dt, nanos))))
        .ok_or_else(|| "Invalid datetime components".to_string())
}

fn plain_date_time_equals(args: &[Value]) -> Result<Value, String> {
    let dt1 = get_date_time(args);
    let dt2 = args
        .get(1)
        .and_then(|v| v.as_string())
        .and_then(|s| parse_date_time(s.as_str()));

    Ok(Value::boolean(dt1 == dt2))
}

fn plain_date_time_to_string(args: &[Value]) -> Result<Value, String> {
    get_date_time(args)
        .map(|(dt, nanos)| Value::string(JsString::intern(&format_date_time(dt, nanos))))
        .ok_or_else(|| "Invalid PlainDateTime".to_string())
}

fn plain_date_time_to_json(args: &[Value]) -> Result<Value, String> {
    plain_date_time_to_string(args)
}

fn plain_date_time_to_plain_date(args: &[Value]) -> Result<Value, String> {
    get_date_time(args)
        .map(|(dt, _)| Value::string(JsString::intern(&dt.format("%Y-%m-%d").to_string())))
        .ok_or_else(|| "Invalid PlainDateTime".to_string())
}

fn plain_date_time_to_plain_time(args: &[Value]) -> Result<Value, String> {
    get_date_time(args)
        .map(|(dt, nanos)| {
            Value::string(JsString::intern(&format!(
                "{:02}:{:02}:{:02}.{:09}",
                dt.hour(),
                dt.minute(),
                dt.second(),
                nanos
            )))
        })
        .ok_or_else(|| "Invalid PlainDateTime".to_string())
}

fn plain_date_time_to_zoned_date_time(args: &[Value]) -> Result<Value, String> {
    let (dt, nanos) = get_date_time(args).ok_or("Invalid PlainDateTime")?;
    let tz = args
        .get(1)
        .and_then(|v| v.as_string())
        .map(|s| s.to_string())
        .unwrap_or_else(|| iana_time_zone::get_timezone().unwrap_or_else(|_| "UTC".to_string()));

    // For simplicity, just append the timezone
    let s = format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:09}[{}]",
        dt.year(),
        dt.month(),
        dt.day(),
        dt.hour(),
        dt.minute(),
        dt.second(),
        nanos,
        tz
    );
    Ok(Value::string(JsString::intern(&s)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_plain_date_time_from() {
        let args = vec![Value::string(JsString::intern(
            "2026-01-23T12:30:45.123456789",
        ))];
        let result = plain_date_time_from(&args).unwrap();
        let s = result.as_string().unwrap().to_string();
        assert!(s.starts_with("2026-01-23T12:30:45"));
    }

    #[test]
    fn test_plain_date_time_year() {
        let args = vec![Value::string(JsString::intern(
            "2026-01-23T12:30:45.000000000",
        ))];
        let result = plain_date_time_year(&args).unwrap();
        assert_eq!(result.as_int32(), Some(2026));
    }
}
