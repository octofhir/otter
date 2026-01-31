//! Temporal.ZonedDateTime - date, time, and timezone combined

use chrono::{Datelike, TimeZone, Timelike};
use chrono_tz::Tz;
use otter_vm_core::value::Value;
use otter_vm_core::{VmError, string::JsString};
use otter_vm_runtime::{Op, op_native};

pub fn ops() -> Vec<Op> {
    vec![
        op_native("__Temporal_ZonedDateTime_from", zoned_date_time_from),
        op_native("__Temporal_ZonedDateTime_compare", zoned_date_time_compare),
        op_native("__Temporal_ZonedDateTime_year", zoned_date_time_year),
        op_native("__Temporal_ZonedDateTime_month", zoned_date_time_month),
        op_native("__Temporal_ZonedDateTime_day", zoned_date_time_day),
        op_native("__Temporal_ZonedDateTime_hour", zoned_date_time_hour),
        op_native("__Temporal_ZonedDateTime_minute", zoned_date_time_minute),
        op_native("__Temporal_ZonedDateTime_second", zoned_date_time_second),
        op_native(
            "__Temporal_ZonedDateTime_millisecond",
            zoned_date_time_millisecond,
        ),
        op_native(
            "__Temporal_ZonedDateTime_timeZoneId",
            zoned_date_time_timezone_id,
        ),
        op_native("__Temporal_ZonedDateTime_offset", zoned_date_time_offset),
        op_native(
            "__Temporal_ZonedDateTime_epochSeconds",
            zoned_date_time_epoch_seconds,
        ),
        op_native(
            "__Temporal_ZonedDateTime_epochMilliseconds",
            zoned_date_time_epoch_milliseconds,
        ),
        op_native(
            "__Temporal_ZonedDateTime_epochNanoseconds",
            zoned_date_time_epoch_nanoseconds,
        ),
        op_native("__Temporal_ZonedDateTime_add", zoned_date_time_add),
        op_native(
            "__Temporal_ZonedDateTime_subtract",
            zoned_date_time_subtract,
        ),
        op_native("__Temporal_ZonedDateTime_with", zoned_date_time_with),
        op_native(
            "__Temporal_ZonedDateTime_withTimeZone",
            zoned_date_time_with_timezone,
        ),
        op_native("__Temporal_ZonedDateTime_equals", zoned_date_time_equals),
        op_native(
            "__Temporal_ZonedDateTime_toString",
            zoned_date_time_to_string,
        ),
        op_native("__Temporal_ZonedDateTime_toJSON", zoned_date_time_to_json),
        op_native(
            "__Temporal_ZonedDateTime_toInstant",
            zoned_date_time_to_instant,
        ),
        op_native(
            "__Temporal_ZonedDateTime_toPlainDateTime",
            zoned_date_time_to_plain_date_time,
        ),
        op_native(
            "__Temporal_ZonedDateTime_toPlainDate",
            zoned_date_time_to_plain_date,
        ),
        op_native(
            "__Temporal_ZonedDateTime_toPlainTime",
            zoned_date_time_to_plain_time,
        ),
    ]
}

/// Parsed ZonedDateTime components
struct ZonedDateTimeComponents {
    year: i32,
    month: u32,
    day: u32,
    hour: u32,
    minute: u32,
    second: u32,
    nanos: u32,
    timezone: String,
    offset: String,
}

fn parse_zoned_date_time(s: &str) -> Option<ZonedDateTimeComponents> {
    // Format: 2026-01-23T12:30:45.123456789-05:00[America/New_York]
    let tz_start = s.find('[')?;
    let tz_end = s.find(']')?;
    let timezone = s[tz_start + 1..tz_end].to_string();

    let dt_part = &s[..tz_start];

    // Parse offset
    let offset_idx = dt_part.rfind('+').or_else(|| {
        // Find - after the date part (not in date)
        let t_idx = dt_part.find('T').unwrap_or(0);
        dt_part[t_idx..].rfind('-').map(|i| t_idx + i)
    });

    let (dt_str, offset) = match offset_idx {
        Some(idx) if idx > 10 => (&dt_part[..idx], dt_part[idx..].to_string()),
        _ => (dt_part, "+00:00".to_string()),
    };

    // Parse date and time
    let parts: Vec<&str> = dt_str.split('.').collect();
    let main_part = parts[0];

    let nanos = if parts.len() > 1 {
        let frac = parts[1];
        let padded = format!("{:0<9}", frac);
        padded[..9].parse::<u32>().unwrap_or(0)
    } else {
        0
    };

    let dt = chrono::NaiveDateTime::parse_from_str(main_part, "%Y-%m-%dT%H:%M:%S").ok()?;

    Some(ZonedDateTimeComponents {
        year: dt.year(),
        month: dt.month(),
        day: dt.day(),
        hour: dt.hour(),
        minute: dt.minute(),
        second: dt.second(),
        nanos,
        timezone,
        offset,
    })
}

fn get_zoned_date_time(args: &[Value]) -> Option<ZonedDateTimeComponents> {
    args.first()
        .and_then(|v| v.as_string())
        .and_then(|s| parse_zoned_date_time(s.as_str()))
}

fn format_zoned_date_time(c: &ZonedDateTimeComponents) -> String {
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:09}{}[{}]",
        c.year, c.month, c.day, c.hour, c.minute, c.second, c.nanos, c.offset, c.timezone
    )
}

fn zoned_date_time_from(args: &[Value]) -> Result<Value, VmError> {
    let s = args
        .first()
        .and_then(|v| v.as_string())
        .ok_or(VmError::type_error("ZonedDateTime.from requires a string"))?;

    match parse_zoned_date_time(s.as_str()) {
        Some(c) => Ok(Value::string(JsString::intern(&format_zoned_date_time(&c)))),
        None => Err(VmError::type_error(format!(
            "Invalid ZonedDateTime string: {}",
            s
        ))),
    }
}

fn zoned_date_time_compare(args: &[Value]) -> Result<Value, VmError> {
    // Compare by epoch nanoseconds
    let ns1 = get_epoch_nanos(args);
    let ns2 = args
        .get(1)
        .and_then(|v| v.as_string())
        .and_then(|s| parse_zoned_date_time(s.as_str()))
        .and_then(|c| compute_epoch_nanos(&c));

    match (ns1, ns2) {
        (Some(a), Some(b)) => Ok(Value::int32(match a.cmp(&b) {
            std::cmp::Ordering::Less => -1,
            std::cmp::Ordering::Equal => 0,
            std::cmp::Ordering::Greater => 1,
        })),
        _ => Err(VmError::type_error("Invalid ZonedDateTime for comparison")),
    }
}

fn get_epoch_nanos(args: &[Value]) -> Option<i128> {
    get_zoned_date_time(args).and_then(|c| compute_epoch_nanos(&c))
}

fn compute_epoch_nanos(c: &ZonedDateTimeComponents) -> Option<i128> {
    let tz: Tz = c.timezone.parse().ok()?;
    let naive = chrono::NaiveDate::from_ymd_opt(c.year, c.month, c.day)?
        .and_hms_nano_opt(c.hour, c.minute, c.second, c.nanos)?;
    let dt = tz.from_local_datetime(&naive).single()?;
    Some(dt.timestamp_nanos_opt()? as i128)
}

fn zoned_date_time_year(args: &[Value]) -> Result<Value, VmError> {
    get_zoned_date_time(args)
        .map(|c| Value::int32(c.year))
        .ok_or_else(|| VmError::type_error("Invalid ZonedDateTime"))
}

fn zoned_date_time_month(args: &[Value]) -> Result<Value, VmError> {
    get_zoned_date_time(args)
        .map(|c| Value::int32(c.month as i32))
        .ok_or_else(|| VmError::type_error("Invalid ZonedDateTime"))
}

fn zoned_date_time_day(args: &[Value]) -> Result<Value, VmError> {
    get_zoned_date_time(args)
        .map(|c| Value::int32(c.day as i32))
        .ok_or_else(|| VmError::type_error("Invalid ZonedDateTime"))
}

fn zoned_date_time_hour(args: &[Value]) -> Result<Value, VmError> {
    get_zoned_date_time(args)
        .map(|c| Value::int32(c.hour as i32))
        .ok_or_else(|| VmError::type_error("Invalid ZonedDateTime"))
}

fn zoned_date_time_minute(args: &[Value]) -> Result<Value, VmError> {
    get_zoned_date_time(args)
        .map(|c| Value::int32(c.minute as i32))
        .ok_or_else(|| VmError::type_error("Invalid ZonedDateTime"))
}

fn zoned_date_time_second(args: &[Value]) -> Result<Value, VmError> {
    get_zoned_date_time(args)
        .map(|c| Value::int32(c.second as i32))
        .ok_or_else(|| VmError::type_error("Invalid ZonedDateTime"))
}

fn zoned_date_time_millisecond(args: &[Value]) -> Result<Value, VmError> {
    get_zoned_date_time(args)
        .map(|c| Value::int32((c.nanos / 1_000_000) as i32))
        .ok_or_else(|| VmError::type_error("Invalid ZonedDateTime"))
}

fn zoned_date_time_timezone_id(args: &[Value]) -> Result<Value, VmError> {
    get_zoned_date_time(args)
        .map(|c| Value::string(JsString::intern(&c.timezone)))
        .ok_or_else(|| VmError::type_error("Invalid ZonedDateTime"))
}

fn zoned_date_time_offset(args: &[Value]) -> Result<Value, VmError> {
    get_zoned_date_time(args)
        .map(|c| Value::string(JsString::intern(&c.offset)))
        .ok_or_else(|| VmError::type_error("Invalid ZonedDateTime"))
}

fn zoned_date_time_epoch_seconds(args: &[Value]) -> Result<Value, VmError> {
    get_epoch_nanos(args)
        .map(|ns| Value::number((ns / 1_000_000_000) as f64))
        .ok_or_else(|| VmError::type_error("Invalid ZonedDateTime"))
}

fn zoned_date_time_epoch_milliseconds(args: &[Value]) -> Result<Value, VmError> {
    get_epoch_nanos(args)
        .map(|ns| Value::number((ns / 1_000_000) as f64))
        .ok_or_else(|| VmError::type_error("Invalid ZonedDateTime"))
}

fn zoned_date_time_epoch_nanoseconds(args: &[Value]) -> Result<Value, VmError> {
    get_epoch_nanos(args)
        .map(|ns| Value::string(JsString::intern(&ns.to_string())))
        .ok_or_else(|| VmError::type_error("Invalid ZonedDateTime"))
}

fn zoned_date_time_add(args: &[Value]) -> Result<Value, VmError> {
    let c = get_zoned_date_time(args).ok_or(VmError::type_error("Invalid ZonedDateTime"))?;
    let add_days = args.get(1).and_then(|v| v.as_int32()).unwrap_or(0) as i64;

    let tz: Tz = c.timezone.parse().map_err(|_| "Invalid timezone")?;
    let naive = chrono::NaiveDate::from_ymd_opt(c.year, c.month, c.day)
        .and_then(|d| d.and_hms_nano_opt(c.hour, c.minute, c.second, c.nanos))
        .ok_or(VmError::type_error("Invalid date"))?;

    let dt = tz
        .from_local_datetime(&naive)
        .single()
        .ok_or(VmError::type_error("Ambiguous local time"))?;
    let new_dt = dt + chrono::Duration::days(add_days);

    let new_c = ZonedDateTimeComponents {
        year: new_dt.year(),
        month: new_dt.month(),
        day: new_dt.day(),
        hour: new_dt.hour(),
        minute: new_dt.minute(),
        second: new_dt.second(),
        nanos: new_dt.nanosecond(),
        timezone: c.timezone,
        offset: new_dt.format("%:z").to_string(),
    };

    Ok(Value::string(JsString::intern(&format_zoned_date_time(
        &new_c,
    ))))
}

fn zoned_date_time_subtract(args: &[Value]) -> Result<Value, VmError> {
    let c = get_zoned_date_time(args).ok_or(VmError::type_error("Invalid ZonedDateTime"))?;
    let sub_days = args.get(1).and_then(|v| v.as_int32()).unwrap_or(0) as i64;

    let tz: Tz = c.timezone.parse().map_err(|_| "Invalid timezone")?;
    let naive = chrono::NaiveDate::from_ymd_opt(c.year, c.month, c.day)
        .and_then(|d| d.and_hms_nano_opt(c.hour, c.minute, c.second, c.nanos))
        .ok_or(VmError::type_error("Invalid date"))?;

    let dt = tz
        .from_local_datetime(&naive)
        .single()
        .ok_or(VmError::type_error("Ambiguous local time"))?;
    let new_dt = dt - chrono::Duration::days(sub_days);

    let new_c = ZonedDateTimeComponents {
        year: new_dt.year(),
        month: new_dt.month(),
        day: new_dt.day(),
        hour: new_dt.hour(),
        minute: new_dt.minute(),
        second: new_dt.second(),
        nanos: new_dt.nanosecond(),
        timezone: c.timezone,
        offset: new_dt.format("%:z").to_string(),
    };

    Ok(Value::string(JsString::intern(&format_zoned_date_time(
        &new_c,
    ))))
}

fn zoned_date_time_with(args: &[Value]) -> Result<Value, VmError> {
    let c = get_zoned_date_time(args).ok_or(VmError::type_error("Invalid ZonedDateTime"))?;

    let year = args.get(1).and_then(|v| v.as_int32()).unwrap_or(c.year);
    let month = args
        .get(2)
        .and_then(|v| v.as_int32())
        .map(|m| m as u32)
        .unwrap_or(c.month);
    let day = args
        .get(3)
        .and_then(|v| v.as_int32())
        .map(|d| d as u32)
        .unwrap_or(c.day);

    let new_c = ZonedDateTimeComponents {
        year,
        month,
        day,
        hour: c.hour,
        minute: c.minute,
        second: c.second,
        nanos: c.nanos,
        timezone: c.timezone,
        offset: c.offset,
    };

    Ok(Value::string(JsString::intern(&format_zoned_date_time(
        &new_c,
    ))))
}

fn zoned_date_time_with_timezone(args: &[Value]) -> Result<Value, VmError> {
    let c = get_zoned_date_time(args).ok_or(VmError::type_error("Invalid ZonedDateTime"))?;
    let new_tz = args
        .get(1)
        .and_then(|v| v.as_string())
        .ok_or(VmError::type_error(
            "withTimeZone requires a timezone string",
        ))?;

    // Convert to instant and then to new timezone
    let old_tz: Tz = c
        .timezone
        .parse()
        .map_err(|_| "Invalid original timezone")?;
    let new_tz_obj: Tz = new_tz
        .as_str()
        .parse()
        .map_err(|_| "Invalid new timezone")?;

    let naive = chrono::NaiveDate::from_ymd_opt(c.year, c.month, c.day)
        .and_then(|d| d.and_hms_nano_opt(c.hour, c.minute, c.second, c.nanos))
        .ok_or(VmError::type_error("Invalid date"))?;

    let old_dt = old_tz
        .from_local_datetime(&naive)
        .single()
        .ok_or(VmError::type_error("Ambiguous local time"))?;
    let new_dt = old_dt.with_timezone(&new_tz_obj);

    let new_c = ZonedDateTimeComponents {
        year: new_dt.year(),
        month: new_dt.month(),
        day: new_dt.day(),
        hour: new_dt.hour(),
        minute: new_dt.minute(),
        second: new_dt.second(),
        nanos: new_dt.nanosecond(),
        timezone: new_tz.to_string(),
        offset: new_dt.format("%:z").to_string(),
    };

    Ok(Value::string(JsString::intern(&format_zoned_date_time(
        &new_c,
    ))))
}

fn zoned_date_time_equals(args: &[Value]) -> Result<Value, VmError> {
    let ns1 = get_epoch_nanos(args);
    let ns2 = args
        .get(1)
        .and_then(|v| v.as_string())
        .and_then(|s| parse_zoned_date_time(s.as_str()))
        .and_then(|c| compute_epoch_nanos(&c));

    Ok(Value::boolean(ns1 == ns2))
}

fn zoned_date_time_to_string(args: &[Value]) -> Result<Value, VmError> {
    get_zoned_date_time(args)
        .map(|c| Value::string(JsString::intern(&format_zoned_date_time(&c))))
        .ok_or_else(|| VmError::type_error("Invalid ZonedDateTime"))
}

fn zoned_date_time_to_json(args: &[Value]) -> Result<Value, VmError> {
    zoned_date_time_to_string(args)
}

fn zoned_date_time_to_instant(args: &[Value]) -> Result<Value, VmError> {
    get_epoch_nanos(args)
        .map(|ns| Value::string(JsString::intern(&ns.to_string())))
        .ok_or_else(|| VmError::type_error("Invalid ZonedDateTime"))
}

fn zoned_date_time_to_plain_date_time(args: &[Value]) -> Result<Value, VmError> {
    get_zoned_date_time(args)
        .map(|c| {
            Value::string(JsString::intern(&format!(
                "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:09}",
                c.year, c.month, c.day, c.hour, c.minute, c.second, c.nanos
            )))
        })
        .ok_or_else(|| VmError::type_error("Invalid ZonedDateTime"))
}

fn zoned_date_time_to_plain_date(args: &[Value]) -> Result<Value, VmError> {
    get_zoned_date_time(args)
        .map(|c| {
            Value::string(JsString::intern(&format!(
                "{:04}-{:02}-{:02}",
                c.year, c.month, c.day
            )))
        })
        .ok_or_else(|| VmError::type_error("Invalid ZonedDateTime"))
}

fn zoned_date_time_to_plain_time(args: &[Value]) -> Result<Value, VmError> {
    get_zoned_date_time(args)
        .map(|c| {
            Value::string(JsString::intern(&format!(
                "{:02}:{:02}:{:02}.{:09}",
                c.hour, c.minute, c.second, c.nanos
            )))
        })
        .ok_or_else(|| VmError::type_error("Invalid ZonedDateTime"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_zoned_date_time_from() {
        let args = vec![Value::string(JsString::intern(
            "2026-01-23T12:30:45.123456789-05:00[America/New_York]",
        ))];
        let result = zoned_date_time_from(&args).unwrap();
        let s = result.as_string().unwrap().to_string();
        assert!(s.contains("2026-01-23"));
        assert!(s.contains("America/New_York"));
    }

    #[test]
    fn test_zoned_date_time_year() {
        let args = vec![Value::string(JsString::intern(
            "2026-01-23T12:30:45.000000000+00:00[UTC]",
        ))];
        let result = zoned_date_time_year(&args).unwrap();
        assert_eq!(result.as_int32(), Some(2026));
    }

    #[test]
    fn test_zoned_date_time_timezone_id() {
        let args = vec![Value::string(JsString::intern(
            "2026-01-23T12:30:45.000000000-05:00[America/New_York]",
        ))];
        let result = zoned_date_time_timezone_id(&args).unwrap();
        let s = result.as_string().unwrap().to_string();
        assert_eq!(s, "America/New_York");
    }
}
