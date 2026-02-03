//! Temporal.ZonedDateTime - date, time, and timezone combined

use otter_vm_core::value::Value;
use otter_vm_core::{VmError, string::JsString};
use otter_vm_runtime::{Op, op_native};
use temporal_rs::ZonedDateTime;
use temporal_rs::options::{Disambiguation, DisplayCalendar, DisplayOffset, DisplayTimeZone, OffsetDisambiguation, ToStringRoundingOptions};
use temporal_rs::provider::COMPILED_TZ_PROVIDER;

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
        op_native("__Temporal_ZonedDateTime_millisecond", zoned_date_time_millisecond),
        op_native("__Temporal_ZonedDateTime_microsecond", zoned_date_time_microsecond),
        op_native("__Temporal_ZonedDateTime_nanosecond", zoned_date_time_nanosecond),
        op_native("__Temporal_ZonedDateTime_timeZoneId", zoned_date_time_timezone_id),
        op_native("__Temporal_ZonedDateTime_offset", zoned_date_time_offset),
        op_native("__Temporal_ZonedDateTime_epochSeconds", zoned_date_time_epoch_seconds),
        op_native("__Temporal_ZonedDateTime_epochMilliseconds", zoned_date_time_epoch_milliseconds),
        op_native("__Temporal_ZonedDateTime_epochNanoseconds", zoned_date_time_epoch_nanoseconds),
        op_native("__Temporal_ZonedDateTime_add", zoned_date_time_add),
        op_native("__Temporal_ZonedDateTime_subtract", zoned_date_time_subtract),
        op_native("__Temporal_ZonedDateTime_withTimeZone", zoned_date_time_with_timezone),
        op_native("__Temporal_ZonedDateTime_equals", zoned_date_time_equals),
        op_native("__Temporal_ZonedDateTime_toString", zoned_date_time_to_string),
        op_native("__Temporal_ZonedDateTime_toJSON", zoned_date_time_to_json),
        op_native("__Temporal_ZonedDateTime_toInstant", zoned_date_time_to_instant),
        op_native("__Temporal_ZonedDateTime_toPlainDateTime", zoned_date_time_to_plain_date_time),
        op_native("__Temporal_ZonedDateTime_toPlainDate", zoned_date_time_to_plain_date),
        op_native("__Temporal_ZonedDateTime_toPlainTime", zoned_date_time_to_plain_time),
    ]
}

fn parse_zoned_date_time(s: &str) -> Option<ZonedDateTime> {
    ZonedDateTime::from_utf8(
        s.as_bytes(),
        Disambiguation::Compatible,
        OffsetDisambiguation::Reject,
    ).ok()
}

fn get_zoned_date_time(args: &[Value]) -> Option<ZonedDateTime> {
    args.first()
        .and_then(|v| v.as_string())
        .and_then(|s| parse_zoned_date_time(s.as_str()))
}

fn format_zoned_date_time(zdt: &ZonedDateTime) -> String {
    zdt.to_ixdtf_string_with_provider(
        DisplayOffset::Auto,
        DisplayTimeZone::Auto,
        DisplayCalendar::Auto,
        ToStringRoundingOptions::default(),
        &*COMPILED_TZ_PROVIDER,
    ).unwrap_or_else(|_| format!("{:?}", zdt))
}

fn zoned_date_time_from(args: &[Value]) -> Result<Value, VmError> {
    let s = args
        .first()
        .and_then(|v| v.as_string())
        .ok_or(VmError::type_error("ZonedDateTime.from requires a string"))?;

    match parse_zoned_date_time(s.as_str()) {
        Some(zdt) => Ok(Value::string(JsString::intern(&format_zoned_date_time(&zdt)))),
        None => Err(VmError::type_error(format!("Invalid ZonedDateTime string: {}", s))),
    }
}

fn zoned_date_time_compare(args: &[Value]) -> Result<Value, VmError> {
    let zdt1 = get_zoned_date_time(args);
    let zdt2 = args.get(1)
        .and_then(|v| v.as_string())
        .and_then(|s| parse_zoned_date_time(s.as_str()));

    match (zdt1, zdt2) {
        (Some(a), Some(b)) => {
            let ns1 = a.epoch_nanoseconds();
            let ns2 = b.epoch_nanoseconds();
            Ok(Value::int32(match ns1.cmp(&ns2) {
                std::cmp::Ordering::Less => -1,
                std::cmp::Ordering::Equal => 0,
                std::cmp::Ordering::Greater => 1,
            }))
        }
        _ => Err(VmError::type_error("Invalid ZonedDateTime for comparison")),
    }
}

fn zoned_date_time_year(args: &[Value]) -> Result<Value, VmError> {
    get_zoned_date_time(args)
        .map(|zdt| Value::int32(zdt.year()))
        .ok_or_else(|| VmError::type_error("Invalid ZonedDateTime"))
}

fn zoned_date_time_month(args: &[Value]) -> Result<Value, VmError> {
    get_zoned_date_time(args)
        .map(|zdt| Value::int32(zdt.month() as i32))
        .ok_or_else(|| VmError::type_error("Invalid ZonedDateTime"))
}

fn zoned_date_time_day(args: &[Value]) -> Result<Value, VmError> {
    get_zoned_date_time(args)
        .map(|zdt| Value::int32(zdt.day() as i32))
        .ok_or_else(|| VmError::type_error("Invalid ZonedDateTime"))
}

fn zoned_date_time_hour(args: &[Value]) -> Result<Value, VmError> {
    get_zoned_date_time(args)
        .map(|zdt| Value::int32(zdt.hour() as i32))
        .ok_or_else(|| VmError::type_error("Invalid ZonedDateTime"))
}

fn zoned_date_time_minute(args: &[Value]) -> Result<Value, VmError> {
    get_zoned_date_time(args)
        .map(|zdt| Value::int32(zdt.minute() as i32))
        .ok_or_else(|| VmError::type_error("Invalid ZonedDateTime"))
}

fn zoned_date_time_second(args: &[Value]) -> Result<Value, VmError> {
    get_zoned_date_time(args)
        .map(|zdt| Value::int32(zdt.second() as i32))
        .ok_or_else(|| VmError::type_error("Invalid ZonedDateTime"))
}

fn zoned_date_time_millisecond(args: &[Value]) -> Result<Value, VmError> {
    get_zoned_date_time(args)
        .map(|zdt| Value::int32(zdt.millisecond() as i32))
        .ok_or_else(|| VmError::type_error("Invalid ZonedDateTime"))
}

fn zoned_date_time_microsecond(args: &[Value]) -> Result<Value, VmError> {
    get_zoned_date_time(args)
        .map(|zdt| Value::int32(zdt.microsecond() as i32))
        .ok_or_else(|| VmError::type_error("Invalid ZonedDateTime"))
}

fn zoned_date_time_nanosecond(args: &[Value]) -> Result<Value, VmError> {
    get_zoned_date_time(args)
        .map(|zdt| Value::int32(zdt.nanosecond() as i32))
        .ok_or_else(|| VmError::type_error("Invalid ZonedDateTime"))
}

fn zoned_date_time_timezone_id(args: &[Value]) -> Result<Value, VmError> {
    get_zoned_date_time(args)
        .and_then(|zdt| {
            zdt.time_zone()
                .identifier_with_provider(&*COMPILED_TZ_PROVIDER)
                .ok()
        })
        .map(|tz_id| Value::string(JsString::intern(&tz_id)))
        .ok_or_else(|| VmError::type_error("Invalid ZonedDateTime"))
}

fn zoned_date_time_offset(args: &[Value]) -> Result<Value, VmError> {
    get_zoned_date_time(args)
        .map(|zdt| Value::string(JsString::intern(&zdt.offset())))
        .ok_or_else(|| VmError::type_error("Invalid ZonedDateTime"))
}

fn zoned_date_time_epoch_seconds(args: &[Value]) -> Result<Value, VmError> {
    get_zoned_date_time(args)
        .map(|zdt| Value::number((zdt.epoch_milliseconds() / 1000) as f64))
        .ok_or_else(|| VmError::type_error("Invalid ZonedDateTime"))
}

fn zoned_date_time_epoch_milliseconds(args: &[Value]) -> Result<Value, VmError> {
    get_zoned_date_time(args)
        .map(|zdt| Value::number(zdt.epoch_milliseconds() as f64))
        .ok_or_else(|| VmError::type_error("Invalid ZonedDateTime"))
}

fn zoned_date_time_epoch_nanoseconds(args: &[Value]) -> Result<Value, VmError> {
    get_zoned_date_time(args)
        .map(|zdt| Value::string(JsString::intern(&zdt.epoch_nanoseconds().as_i128().to_string())))
        .ok_or_else(|| VmError::type_error("Invalid ZonedDateTime"))
}

fn zoned_date_time_add(args: &[Value]) -> Result<Value, VmError> {
    let zdt = get_zoned_date_time(args).ok_or(VmError::type_error("Invalid ZonedDateTime"))?;
    let duration_str = args.get(1)
        .and_then(|v| v.as_string())
        .ok_or(VmError::type_error("Duration required"))?;

    let duration = temporal_rs::Duration::from_utf8(duration_str.as_str().as_bytes())
        .map_err(|e| VmError::type_error(format!("Invalid duration: {:?}", e)))?;

    let new_zdt = zdt.add_with_provider(&duration, None, &*COMPILED_TZ_PROVIDER)
        .map_err(|e| VmError::type_error(format!("Add failed: {:?}", e)))?;

    Ok(Value::string(JsString::intern(&format_zoned_date_time(&new_zdt))))
}

fn zoned_date_time_subtract(args: &[Value]) -> Result<Value, VmError> {
    let zdt = get_zoned_date_time(args).ok_or(VmError::type_error("Invalid ZonedDateTime"))?;
    let duration_str = args.get(1)
        .and_then(|v| v.as_string())
        .ok_or(VmError::type_error("Duration required"))?;

    let duration = temporal_rs::Duration::from_utf8(duration_str.as_str().as_bytes())
        .map_err(|e| VmError::type_error(format!("Invalid duration: {:?}", e)))?;

    let new_zdt = zdt.subtract_with_provider(&duration, None, &*COMPILED_TZ_PROVIDER)
        .map_err(|e| VmError::type_error(format!("Subtract failed: {:?}", e)))?;

    Ok(Value::string(JsString::intern(&format_zoned_date_time(&new_zdt))))
}

fn zoned_date_time_with_timezone(args: &[Value]) -> Result<Value, VmError> {
    let zdt = get_zoned_date_time(args).ok_or(VmError::type_error("Invalid ZonedDateTime"))?;
    let new_tz_str = args.get(1)
        .and_then(|v| v.as_string())
        .ok_or(VmError::type_error("withTimeZone requires a timezone string"))?;

    let new_tz = temporal_rs::TimeZone::try_from_str_with_provider(new_tz_str.as_str(), &*COMPILED_TZ_PROVIDER)
        .map_err(|e| VmError::type_error(format!("Invalid timezone: {:?}", e)))?;

    let new_zdt = zdt.with_timezone(new_tz)
        .map_err(|e| VmError::type_error(format!("withTimeZone failed: {:?}", e)))?;

    Ok(Value::string(JsString::intern(&format_zoned_date_time(&new_zdt))))
}

fn zoned_date_time_equals(args: &[Value]) -> Result<Value, VmError> {
    let zdt1 = get_zoned_date_time(args);
    let zdt2 = args.get(1)
        .and_then(|v| v.as_string())
        .and_then(|s| parse_zoned_date_time(s.as_str()));

    match (zdt1, zdt2) {
        (Some(a), Some(b)) => Ok(Value::boolean(a.epoch_nanoseconds() == b.epoch_nanoseconds())),
        _ => Ok(Value::boolean(false)),
    }
}

fn zoned_date_time_to_string(args: &[Value]) -> Result<Value, VmError> {
    get_zoned_date_time(args)
        .map(|zdt| Value::string(JsString::intern(&format_zoned_date_time(&zdt))))
        .ok_or_else(|| VmError::type_error("Invalid ZonedDateTime"))
}

fn zoned_date_time_to_json(args: &[Value]) -> Result<Value, VmError> {
    zoned_date_time_to_string(args)
}

fn zoned_date_time_to_instant(args: &[Value]) -> Result<Value, VmError> {
    get_zoned_date_time(args)
        .map(|zdt| {
            let instant = zdt.to_instant();
            Value::string(JsString::intern(&instant.epoch_nanoseconds().as_i128().to_string()))
        })
        .ok_or_else(|| VmError::type_error("Invalid ZonedDateTime"))
}

fn zoned_date_time_to_plain_date_time(args: &[Value]) -> Result<Value, VmError> {
    get_zoned_date_time(args)
        .map(|zdt| {
            let dt = zdt.to_plain_date_time();
            let s = dt.to_ixdtf_string(ToStringRoundingOptions::default(), DisplayCalendar::Auto)
                .unwrap_or_else(|_| format!("{:?}", dt));
            Value::string(JsString::intern(&s))
        })
        .ok_or_else(|| VmError::type_error("Invalid ZonedDateTime"))
}

fn zoned_date_time_to_plain_date(args: &[Value]) -> Result<Value, VmError> {
    get_zoned_date_time(args)
        .map(|zdt| {
            let date = zdt.to_plain_date();
            let s = date.to_ixdtf_string(DisplayCalendar::Auto);
            Value::string(JsString::intern(&s))
        })
        .ok_or_else(|| VmError::type_error("Invalid ZonedDateTime"))
}

fn zoned_date_time_to_plain_time(args: &[Value]) -> Result<Value, VmError> {
    get_zoned_date_time(args)
        .and_then(|zdt| {
            let time = zdt.to_plain_time();
            time.to_ixdtf_string(ToStringRoundingOptions::default()).ok()
        })
        .map(|s| Value::string(JsString::intern(&s)))
        .ok_or_else(|| VmError::type_error("Invalid ZonedDateTime"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_zoned_date_time_from() {
        let args = vec![Value::string(JsString::intern(
            "2026-01-23T12:30:45+00:00[UTC]",
        ))];
        let result = zoned_date_time_from(&args).unwrap();
        let s = result.as_string().unwrap().to_string();
        assert!(s.contains("2026-01-23"));
        assert!(s.contains("UTC"));
    }

    #[test]
    fn test_zoned_date_time_year() {
        let args = vec![Value::string(JsString::intern(
            "2026-01-23T12:30:45+00:00[UTC]",
        ))];
        let result = zoned_date_time_year(&args).unwrap();
        assert_eq!(result.as_int32(), Some(2026));
    }
}
