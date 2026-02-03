//! Temporal.PlainDate - calendar date without time or timezone

use otter_vm_core::value::Value;
use otter_vm_core::{VmError, string::JsString};
use otter_vm_runtime::{Op, op_native};
use temporal_rs::PlainDate;
use temporal_rs::options::DisplayCalendar;

pub fn ops() -> Vec<Op> {
    vec![
        op_native("__Temporal_PlainDate_from", plain_date_from),
        op_native("__Temporal_PlainDate_compare", plain_date_compare),
        op_native("__Temporal_PlainDate_year", plain_date_year),
        op_native("__Temporal_PlainDate_month", plain_date_month),
        op_native("__Temporal_PlainDate_monthCode", plain_date_month_code),
        op_native("__Temporal_PlainDate_day", plain_date_day),
        op_native("__Temporal_PlainDate_dayOfWeek", plain_date_day_of_week),
        op_native("__Temporal_PlainDate_dayOfYear", plain_date_day_of_year),
        op_native("__Temporal_PlainDate_daysInMonth", plain_date_days_in_month),
        op_native("__Temporal_PlainDate_daysInYear", plain_date_days_in_year),
        op_native("__Temporal_PlainDate_inLeapYear", plain_date_in_leap_year),
        op_native("__Temporal_PlainDate_add", plain_date_add),
        op_native("__Temporal_PlainDate_subtract", plain_date_subtract),
        op_native("__Temporal_PlainDate_equals", plain_date_equals),
        op_native("__Temporal_PlainDate_toString", plain_date_to_string),
        op_native("__Temporal_PlainDate_toJSON", plain_date_to_json),
        op_native("__Temporal_PlainDate_toPlainDateTime", plain_date_to_plain_date_time),
    ]
}

fn parse_date(s: &str) -> Option<PlainDate> {
    PlainDate::from_utf8(s.as_bytes()).ok()
}

fn get_date(args: &[Value]) -> Option<PlainDate> {
    args.first()
        .and_then(|v| v.as_string())
        .and_then(|s| parse_date(s.as_str()))
}

fn format_date(d: &PlainDate) -> String {
    d.to_ixdtf_string(DisplayCalendar::Auto)
}

fn plain_date_from(args: &[Value]) -> Result<Value, VmError> {
    let s = args
        .first()
        .and_then(|v| v.as_string())
        .ok_or(VmError::type_error("PlainDate.from requires a string"))?;

    match parse_date(s.as_str()) {
        Some(d) => Ok(Value::string(JsString::intern(&format_date(&d)))),
        None => Err(VmError::type_error(format!("Invalid PlainDate string: {}", s))),
    }
}

fn plain_date_compare(args: &[Value]) -> Result<Value, VmError> {
    let d1 = get_date(args);
    let d2 = args.get(1)
        .and_then(|v| v.as_string())
        .and_then(|s| parse_date(s.as_str()));

    match (d1, d2) {
        (Some(a), Some(b)) => {
            Ok(Value::int32(a.compare_iso(&b) as i8 as i32))
        }
        _ => Err(VmError::type_error("Invalid PlainDate for comparison")),
    }
}

fn plain_date_year(args: &[Value]) -> Result<Value, VmError> {
    get_date(args)
        .map(|d| Value::int32(d.year()))
        .ok_or_else(|| VmError::type_error("Invalid PlainDate"))
}

fn plain_date_month(args: &[Value]) -> Result<Value, VmError> {
    get_date(args)
        .map(|d| Value::int32(d.month() as i32))
        .ok_or_else(|| VmError::type_error("Invalid PlainDate"))
}

fn plain_date_month_code(args: &[Value]) -> Result<Value, VmError> {
    get_date(args)
        .map(|d| Value::string(JsString::intern(d.month_code().as_str())))
        .ok_or_else(|| VmError::type_error("Invalid PlainDate"))
}

fn plain_date_day(args: &[Value]) -> Result<Value, VmError> {
    get_date(args)
        .map(|d| Value::int32(d.day() as i32))
        .ok_or_else(|| VmError::type_error("Invalid PlainDate"))
}

fn plain_date_day_of_week(args: &[Value]) -> Result<Value, VmError> {
    get_date(args)
        .map(|d| Value::int32(d.day_of_week() as i32))
        .ok_or_else(|| VmError::type_error("Invalid PlainDate"))
}

fn plain_date_day_of_year(args: &[Value]) -> Result<Value, VmError> {
    get_date(args)
        .map(|d| Value::int32(d.day_of_year() as i32))
        .ok_or_else(|| VmError::type_error("Invalid PlainDate"))
}

fn plain_date_days_in_month(args: &[Value]) -> Result<Value, VmError> {
    get_date(args)
        .map(|d| Value::int32(d.days_in_month() as i32))
        .ok_or_else(|| VmError::type_error("Invalid PlainDate"))
}

fn plain_date_days_in_year(args: &[Value]) -> Result<Value, VmError> {
    get_date(args)
        .map(|d| Value::int32(d.days_in_year() as i32))
        .ok_or_else(|| VmError::type_error("Invalid PlainDate"))
}

fn plain_date_in_leap_year(args: &[Value]) -> Result<Value, VmError> {
    get_date(args)
        .map(|d| Value::boolean(d.in_leap_year()))
        .ok_or_else(|| VmError::type_error("Invalid PlainDate"))
}

fn plain_date_add(args: &[Value]) -> Result<Value, VmError> {
    let d = get_date(args).ok_or(VmError::type_error("Invalid PlainDate"))?;
    let duration_str = args.get(1)
        .and_then(|v| v.as_string())
        .ok_or(VmError::type_error("Duration required"))?;

    let duration = temporal_rs::Duration::from_utf8(duration_str.as_str().as_bytes())
        .map_err(|e| VmError::type_error(format!("Invalid duration: {:?}", e)))?;

    let new_d = d.add(&duration, None)
        .map_err(|e| VmError::type_error(format!("Add failed: {:?}", e)))?;

    Ok(Value::string(JsString::intern(&format_date(&new_d))))
}

fn plain_date_subtract(args: &[Value]) -> Result<Value, VmError> {
    let d = get_date(args).ok_or(VmError::type_error("Invalid PlainDate"))?;
    let duration_str = args.get(1)
        .and_then(|v| v.as_string())
        .ok_or(VmError::type_error("Duration required"))?;

    let duration = temporal_rs::Duration::from_utf8(duration_str.as_str().as_bytes())
        .map_err(|e| VmError::type_error(format!("Invalid duration: {:?}", e)))?;

    let new_d = d.subtract(&duration, None)
        .map_err(|e| VmError::type_error(format!("Subtract failed: {:?}", e)))?;

    Ok(Value::string(JsString::intern(&format_date(&new_d))))
}

fn plain_date_equals(args: &[Value]) -> Result<Value, VmError> {
    let d1 = get_date(args);
    let d2 = args.get(1)
        .and_then(|v| v.as_string())
        .and_then(|s| parse_date(s.as_str()));

    match (d1, d2) {
        (Some(a), Some(b)) => Ok(Value::boolean(a == b)),
        _ => Ok(Value::boolean(false)),
    }
}

fn plain_date_to_string(args: &[Value]) -> Result<Value, VmError> {
    get_date(args)
        .map(|d| Value::string(JsString::intern(&format_date(&d))))
        .ok_or_else(|| VmError::type_error("Invalid PlainDate"))
}

fn plain_date_to_json(args: &[Value]) -> Result<Value, VmError> {
    plain_date_to_string(args)
}

fn plain_date_to_plain_date_time(args: &[Value]) -> Result<Value, VmError> {
    let d = get_date(args).ok_or(VmError::type_error("Invalid PlainDate"))?;
    let time_str = args.get(1).and_then(|v| v.as_string());

    let time = if let Some(ts) = time_str {
        temporal_rs::PlainTime::from_utf8(ts.as_str().as_bytes())
            .map_err(|e| VmError::type_error(format!("Invalid time: {:?}", e)))?
    } else {
        temporal_rs::PlainTime::default()
    };

    let dt = d.to_plain_date_time(Some(time))
        .map_err(|e| VmError::type_error(format!("toPlainDateTime failed: {:?}", e)))?;

    let s = dt.to_ixdtf_string(temporal_rs::options::ToStringRoundingOptions::default(), DisplayCalendar::Auto)
        .map_err(|e| VmError::type_error(format!("toString failed: {:?}", e)))?;

    Ok(Value::string(JsString::intern(&s)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_plain_date_from() {
        let args = vec![Value::string(JsString::intern("2026-01-23"))];
        let result = plain_date_from(&args).unwrap();
        let s = result.as_string().unwrap().to_string();
        assert!(s.starts_with("2026-01-23"));
    }

    #[test]
    fn test_plain_date_year() {
        let args = vec![Value::string(JsString::intern("2026-01-23"))];
        let result = plain_date_year(&args).unwrap();
        assert_eq!(result.as_int32(), Some(2026));
    }
}
