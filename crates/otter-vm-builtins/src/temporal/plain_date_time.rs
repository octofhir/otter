//! Temporal.PlainDateTime - date and time without timezone

use otter_vm_core::value::Value;
use otter_vm_core::{VmError, string::JsString};
use otter_vm_runtime::{Op, op_native};
use temporal_rs::PlainDateTime;
use temporal_rs::options::{DisplayCalendar, ToStringRoundingOptions};
use temporal_rs::provider::COMPILED_TZ_PROVIDER;

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
        op_native(
            "__Temporal_PlainDateTime_microsecond",
            plain_date_time_microsecond,
        ),
        op_native(
            "__Temporal_PlainDateTime_nanosecond",
            plain_date_time_nanosecond,
        ),
        op_native("__Temporal_PlainDateTime_add", plain_date_time_add),
        op_native(
            "__Temporal_PlainDateTime_subtract",
            plain_date_time_subtract,
        ),
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

fn parse_date_time(s: &str) -> Option<PlainDateTime> {
    PlainDateTime::from_utf8(s.as_bytes()).ok()
}

fn get_date_time(args: &[Value]) -> Option<PlainDateTime> {
    args.first()
        .and_then(|v| v.as_string())
        .and_then(|s| parse_date_time(s.as_str()))
}

fn format_date_time(dt: &PlainDateTime) -> String {
    dt.to_ixdtf_string(ToStringRoundingOptions::default(), DisplayCalendar::Auto)
        .unwrap_or_else(|_| {
            format!(
                "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}",
                dt.year(),
                dt.month(),
                dt.day(),
                dt.hour(),
                dt.minute(),
                dt.second()
            )
        })
}

fn plain_date_time_from(args: &[Value]) -> Result<Value, VmError> {
    let s = args
        .first()
        .and_then(|v| v.as_string())
        .ok_or(VmError::type_error("PlainDateTime.from requires a string"))?;

    match parse_date_time(s.as_str()) {
        Some(dt) => Ok(Value::string(JsString::intern(&format_date_time(&dt)))),
        None => Err(VmError::type_error(format!(
            "Invalid PlainDateTime string: {}",
            s
        ))),
    }
}

fn plain_date_time_compare(args: &[Value]) -> Result<Value, VmError> {
    let dt1 = get_date_time(args);
    let dt2 = args
        .get(1)
        .and_then(|v| v.as_string())
        .and_then(|s| parse_date_time(s.as_str()));

    match (dt1, dt2) {
        (Some(a), Some(b)) => Ok(Value::int32(a.compare_iso(&b) as i8 as i32)),
        _ => Err(VmError::type_error("Invalid PlainDateTime for comparison")),
    }
}

fn plain_date_time_year(args: &[Value]) -> Result<Value, VmError> {
    get_date_time(args)
        .map(|dt| Value::int32(dt.year()))
        .ok_or_else(|| VmError::type_error("Invalid PlainDateTime"))
}

fn plain_date_time_month(args: &[Value]) -> Result<Value, VmError> {
    get_date_time(args)
        .map(|dt| Value::int32(dt.month() as i32))
        .ok_or_else(|| VmError::type_error("Invalid PlainDateTime"))
}

fn plain_date_time_day(args: &[Value]) -> Result<Value, VmError> {
    get_date_time(args)
        .map(|dt| Value::int32(dt.day() as i32))
        .ok_or_else(|| VmError::type_error("Invalid PlainDateTime"))
}

fn plain_date_time_hour(args: &[Value]) -> Result<Value, VmError> {
    get_date_time(args)
        .map(|dt| Value::int32(dt.hour() as i32))
        .ok_or_else(|| VmError::type_error("Invalid PlainDateTime"))
}

fn plain_date_time_minute(args: &[Value]) -> Result<Value, VmError> {
    get_date_time(args)
        .map(|dt| Value::int32(dt.minute() as i32))
        .ok_or_else(|| VmError::type_error("Invalid PlainDateTime"))
}

fn plain_date_time_second(args: &[Value]) -> Result<Value, VmError> {
    get_date_time(args)
        .map(|dt| Value::int32(dt.second() as i32))
        .ok_or_else(|| VmError::type_error("Invalid PlainDateTime"))
}

fn plain_date_time_millisecond(args: &[Value]) -> Result<Value, VmError> {
    get_date_time(args)
        .map(|dt| Value::int32(dt.millisecond() as i32))
        .ok_or_else(|| VmError::type_error("Invalid PlainDateTime"))
}

fn plain_date_time_microsecond(args: &[Value]) -> Result<Value, VmError> {
    get_date_time(args)
        .map(|dt| Value::int32(dt.microsecond() as i32))
        .ok_or_else(|| VmError::type_error("Invalid PlainDateTime"))
}

fn plain_date_time_nanosecond(args: &[Value]) -> Result<Value, VmError> {
    get_date_time(args)
        .map(|dt| Value::int32(dt.nanosecond() as i32))
        .ok_or_else(|| VmError::type_error("Invalid PlainDateTime"))
}

fn plain_date_time_add(args: &[Value]) -> Result<Value, VmError> {
    let dt = get_date_time(args).ok_or(VmError::type_error("Invalid PlainDateTime"))?;
    let duration_str = args
        .get(1)
        .and_then(|v| v.as_string())
        .ok_or(VmError::type_error("Duration required"))?;

    let duration = temporal_rs::Duration::from_utf8(duration_str.as_str().as_bytes())
        .map_err(|e| VmError::type_error(format!("Invalid duration: {:?}", e)))?;

    let new_dt = dt
        .add(&duration, None)
        .map_err(|e| VmError::type_error(format!("Add failed: {:?}", e)))?;

    Ok(Value::string(JsString::intern(&format_date_time(&new_dt))))
}

fn plain_date_time_subtract(args: &[Value]) -> Result<Value, VmError> {
    let dt = get_date_time(args).ok_or(VmError::type_error("Invalid PlainDateTime"))?;
    let duration_str = args
        .get(1)
        .and_then(|v| v.as_string())
        .ok_or(VmError::type_error("Duration required"))?;

    let duration = temporal_rs::Duration::from_utf8(duration_str.as_str().as_bytes())
        .map_err(|e| VmError::type_error(format!("Invalid duration: {:?}", e)))?;

    let new_dt = dt
        .subtract(&duration, None)
        .map_err(|e| VmError::type_error(format!("Subtract failed: {:?}", e)))?;

    Ok(Value::string(JsString::intern(&format_date_time(&new_dt))))
}

fn plain_date_time_equals(args: &[Value]) -> Result<Value, VmError> {
    let dt1 = get_date_time(args);
    let dt2 = args
        .get(1)
        .and_then(|v| v.as_string())
        .and_then(|s| parse_date_time(s.as_str()));

    match (dt1, dt2) {
        (Some(a), Some(b)) => Ok(Value::boolean(a == b)),
        _ => Ok(Value::boolean(false)),
    }
}

fn plain_date_time_to_string(args: &[Value]) -> Result<Value, VmError> {
    get_date_time(args)
        .map(|dt| Value::string(JsString::intern(&format_date_time(&dt))))
        .ok_or_else(|| VmError::type_error("Invalid PlainDateTime"))
}

fn plain_date_time_to_json(args: &[Value]) -> Result<Value, VmError> {
    plain_date_time_to_string(args)
}

fn plain_date_time_to_plain_date(args: &[Value]) -> Result<Value, VmError> {
    get_date_time(args)
        .map(|dt| {
            let date = dt.to_plain_date();
            let s = date.to_ixdtf_string(DisplayCalendar::Auto);
            Value::string(JsString::intern(&s))
        })
        .ok_or_else(|| VmError::type_error("Invalid PlainDateTime"))
}

fn plain_date_time_to_plain_time(args: &[Value]) -> Result<Value, VmError> {
    get_date_time(args)
        .and_then(|dt| {
            let time = dt.to_plain_time();
            time.to_ixdtf_string(ToStringRoundingOptions::default())
                .ok()
        })
        .map(|s| Value::string(JsString::intern(&s)))
        .ok_or_else(|| VmError::type_error("Invalid PlainDateTime"))
}

fn plain_date_time_to_zoned_date_time(args: &[Value]) -> Result<Value, VmError> {
    let dt = get_date_time(args).ok_or(VmError::type_error("Invalid PlainDateTime"))?;
    let tz_str = args.get(1).and_then(|v| v.as_string());

    let tz = if let Some(tz_id) = tz_str {
        temporal_rs::TimeZone::try_from_str_with_provider(tz_id.as_str(), &*COMPILED_TZ_PROVIDER)
            .map_err(|e| VmError::type_error(format!("Invalid timezone: {:?}", e)))?
    } else {
        temporal_rs::Temporal::now()
            .time_zone_with_provider(&*COMPILED_TZ_PROVIDER)
            .map_err(|e| VmError::type_error(format!("Failed to get system timezone: {:?}", e)))?
    };

    let zdt = dt
        .to_zoned_date_time_with_provider(
            tz,
            temporal_rs::options::Disambiguation::Compatible,
            &*COMPILED_TZ_PROVIDER,
        )
        .map_err(|e| VmError::type_error(format!("toZonedDateTime failed: {:?}", e)))?;

    let s = zdt
        .to_ixdtf_string_with_provider(
            temporal_rs::options::DisplayOffset::Auto,
            temporal_rs::options::DisplayTimeZone::Auto,
            DisplayCalendar::Auto,
            ToStringRoundingOptions::default(),
            &*COMPILED_TZ_PROVIDER,
        )
        .map_err(|e| VmError::type_error(format!("toString failed: {:?}", e)))?;

    Ok(Value::string(JsString::intern(&s)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_plain_date_time_from() {
        let args = vec![Value::string(JsString::intern("2026-01-23T12:30:45"))];
        let result = plain_date_time_from(&args).unwrap();
        let s = result.as_string().unwrap().to_string();
        assert!(s.starts_with("2026-01-23T12:30:45"));
    }

    #[test]
    fn test_plain_date_time_year() {
        let args = vec![Value::string(JsString::intern("2026-01-23T12:30:45"))];
        let result = plain_date_time_year(&args).unwrap();
        assert_eq!(result.as_int32(), Some(2026));
    }
}
