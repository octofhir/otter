//! Temporal.PlainTime - wall-clock time without date or timezone

use otter_vm_core::value::Value;
use otter_vm_core::{VmError, string::JsString};
use otter_vm_runtime::{Op, op_native};
use temporal_rs::PlainTime;
use temporal_rs::options::ToStringRoundingOptions;

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
        op_native("__Temporal_PlainTime_equals", plain_time_equals),
        op_native("__Temporal_PlainTime_toString", plain_time_to_string),
        op_native("__Temporal_PlainTime_toJSON", plain_time_to_json),
    ]
}

fn parse_time(s: &str) -> Option<PlainTime> {
    PlainTime::from_utf8(s.as_bytes()).ok()
}

fn get_time(args: &[Value]) -> Option<PlainTime> {
    args.first()
        .and_then(|v| v.as_string())
        .and_then(|s| parse_time(s.as_str()))
}

fn format_time(t: &PlainTime) -> String {
    t.to_ixdtf_string(ToStringRoundingOptions::default())
        .unwrap_or_else(|_| format!("{:02}:{:02}:{:02}", t.hour(), t.minute(), t.second()))
}

fn plain_time_from(args: &[Value]) -> Result<Value, VmError> {
    let s = args
        .first()
        .and_then(|v| v.as_string())
        .ok_or(VmError::type_error("PlainTime.from requires a string"))?;

    match parse_time(s.as_str()) {
        Some(t) => Ok(Value::string(JsString::intern(&format_time(&t)))),
        None => Err(VmError::type_error(format!("Invalid PlainTime string: {}", s))),
    }
}

fn plain_time_compare(args: &[Value]) -> Result<Value, VmError> {
    let t1 = get_time(args);
    let t2 = args.get(1)
        .and_then(|v| v.as_string())
        .and_then(|s| parse_time(s.as_str()));

    match (t1, t2) {
        (Some(a), Some(b)) => {
            let cmp = a.cmp(&b);
            Ok(Value::int32(match cmp {
                std::cmp::Ordering::Less => -1,
                std::cmp::Ordering::Equal => 0,
                std::cmp::Ordering::Greater => 1,
            }))
        }
        _ => Err(VmError::type_error("Invalid PlainTime for comparison")),
    }
}

fn plain_time_hour(args: &[Value]) -> Result<Value, VmError> {
    get_time(args)
        .map(|t| Value::int32(t.hour() as i32))
        .ok_or_else(|| VmError::type_error("Invalid PlainTime"))
}

fn plain_time_minute(args: &[Value]) -> Result<Value, VmError> {
    get_time(args)
        .map(|t| Value::int32(t.minute() as i32))
        .ok_or_else(|| VmError::type_error("Invalid PlainTime"))
}

fn plain_time_second(args: &[Value]) -> Result<Value, VmError> {
    get_time(args)
        .map(|t| Value::int32(t.second() as i32))
        .ok_or_else(|| VmError::type_error("Invalid PlainTime"))
}

fn plain_time_millisecond(args: &[Value]) -> Result<Value, VmError> {
    get_time(args)
        .map(|t| Value::int32(t.millisecond() as i32))
        .ok_or_else(|| VmError::type_error("Invalid PlainTime"))
}

fn plain_time_microsecond(args: &[Value]) -> Result<Value, VmError> {
    get_time(args)
        .map(|t| Value::int32(t.microsecond() as i32))
        .ok_or_else(|| VmError::type_error("Invalid PlainTime"))
}

fn plain_time_nanosecond(args: &[Value]) -> Result<Value, VmError> {
    get_time(args)
        .map(|t| Value::int32(t.nanosecond() as i32))
        .ok_or_else(|| VmError::type_error("Invalid PlainTime"))
}

fn plain_time_add(args: &[Value]) -> Result<Value, VmError> {
    let t = get_time(args).ok_or(VmError::type_error("Invalid PlainTime"))?;
    let duration_str = args.get(1)
        .and_then(|v| v.as_string())
        .ok_or(VmError::type_error("Duration required"))?;

    let duration = temporal_rs::Duration::from_utf8(duration_str.as_str().as_bytes())
        .map_err(|e| VmError::type_error(format!("Invalid duration: {:?}", e)))?;

    let new_t = t.add(&duration)
        .map_err(|e| VmError::type_error(format!("Add failed: {:?}", e)))?;

    Ok(Value::string(JsString::intern(&format_time(&new_t))))
}

fn plain_time_subtract(args: &[Value]) -> Result<Value, VmError> {
    let t = get_time(args).ok_or(VmError::type_error("Invalid PlainTime"))?;
    let duration_str = args.get(1)
        .and_then(|v| v.as_string())
        .ok_or(VmError::type_error("Duration required"))?;

    let duration = temporal_rs::Duration::from_utf8(duration_str.as_str().as_bytes())
        .map_err(|e| VmError::type_error(format!("Invalid duration: {:?}", e)))?;

    let new_t = t.subtract(&duration)
        .map_err(|e| VmError::type_error(format!("Subtract failed: {:?}", e)))?;

    Ok(Value::string(JsString::intern(&format_time(&new_t))))
}

fn plain_time_equals(args: &[Value]) -> Result<Value, VmError> {
    let t1 = get_time(args);
    let t2 = args.get(1)
        .and_then(|v| v.as_string())
        .and_then(|s| parse_time(s.as_str()));

    match (t1, t2) {
        (Some(a), Some(b)) => Ok(Value::boolean(a == b)),
        _ => Ok(Value::boolean(false)),
    }
}

fn plain_time_to_string(args: &[Value]) -> Result<Value, VmError> {
    get_time(args)
        .map(|t| Value::string(JsString::intern(&format_time(&t))))
        .ok_or_else(|| VmError::type_error("Invalid PlainTime"))
}

fn plain_time_to_json(args: &[Value]) -> Result<Value, VmError> {
    plain_time_to_string(args)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_plain_time_from() {
        let args = vec![Value::string(JsString::intern("12:30:45"))];
        let result = plain_time_from(&args).unwrap();
        let s = result.as_string().unwrap().to_string();
        assert!(s.starts_with("12:30:45"));
    }

    #[test]
    fn test_plain_time_hour() {
        let args = vec![Value::string(JsString::intern("12:30:45"))];
        let result = plain_time_hour(&args).unwrap();
        assert_eq!(result.as_int32(), Some(12));
    }
}
