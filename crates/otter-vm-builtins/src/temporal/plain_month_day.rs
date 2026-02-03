//! Temporal.PlainMonthDay - month and day only

use otter_vm_core::value::Value;
use otter_vm_core::{VmError, string::JsString};
use otter_vm_runtime::{Op, op_native};
use temporal_rs::PlainMonthDay;
use temporal_rs::options::DisplayCalendar;

pub fn ops() -> Vec<Op> {
    vec![
        op_native("__Temporal_PlainMonthDay_from", plain_month_day_from),
        op_native("__Temporal_PlainMonthDay_month", plain_month_day_month),
        op_native("__Temporal_PlainMonthDay_monthCode", plain_month_day_month_code),
        op_native("__Temporal_PlainMonthDay_day", plain_month_day_day),
        op_native("__Temporal_PlainMonthDay_equals", plain_month_day_equals),
        op_native("__Temporal_PlainMonthDay_toString", plain_month_day_to_string),
        op_native("__Temporal_PlainMonthDay_toJSON", plain_month_day_to_json),
        op_native("__Temporal_PlainMonthDay_toPlainDate", plain_month_day_to_plain_date),
    ]
}

fn parse_month_day(s: &str) -> Option<PlainMonthDay> {
    PlainMonthDay::from_utf8(s.as_bytes()).ok()
}

fn get_month_day(args: &[Value]) -> Option<PlainMonthDay> {
    args.first()
        .and_then(|v| v.as_string())
        .and_then(|s| parse_month_day(s.as_str()))
}

fn format_month_day(md: &PlainMonthDay) -> String {
    md.to_ixdtf_string(DisplayCalendar::Auto)
}

fn plain_month_day_from(args: &[Value]) -> Result<Value, VmError> {
    let s = args
        .first()
        .and_then(|v| v.as_string())
        .ok_or(VmError::type_error("PlainMonthDay.from requires a string"))?;

    match parse_month_day(s.as_str()) {
        Some(md) => Ok(Value::string(JsString::intern(&format_month_day(&md)))),
        None => Err(VmError::type_error(format!("Invalid PlainMonthDay string: {}", s))),
    }
}

fn plain_month_day_month(args: &[Value]) -> Result<Value, VmError> {
    get_month_day(args)
        .map(|md| Value::int32(md.month_code().to_month_integer() as i32))
        .ok_or_else(|| VmError::type_error("Invalid PlainMonthDay"))
}

fn plain_month_day_month_code(args: &[Value]) -> Result<Value, VmError> {
    get_month_day(args)
        .map(|md| Value::string(JsString::intern(md.month_code().as_str())))
        .ok_or_else(|| VmError::type_error("Invalid PlainMonthDay"))
}

fn plain_month_day_day(args: &[Value]) -> Result<Value, VmError> {
    get_month_day(args)
        .map(|md| Value::int32(md.day() as i32))
        .ok_or_else(|| VmError::type_error("Invalid PlainMonthDay"))
}

fn plain_month_day_equals(args: &[Value]) -> Result<Value, VmError> {
    let md1 = get_month_day(args);
    let md2 = args.get(1)
        .and_then(|v| v.as_string())
        .and_then(|s| parse_month_day(s.as_str()));

    match (md1, md2) {
        (Some(a), Some(b)) => Ok(Value::boolean(a == b)),
        _ => Ok(Value::boolean(false)),
    }
}

fn plain_month_day_to_string(args: &[Value]) -> Result<Value, VmError> {
    get_month_day(args)
        .map(|md| Value::string(JsString::intern(&format_month_day(&md))))
        .ok_or_else(|| VmError::type_error("Invalid PlainMonthDay"))
}

fn plain_month_day_to_json(args: &[Value]) -> Result<Value, VmError> {
    plain_month_day_to_string(args)
}

fn plain_month_day_to_plain_date(args: &[Value]) -> Result<Value, VmError> {
    use temporal_rs::fields::CalendarFields;

    let md = get_month_day(args).ok_or(VmError::type_error("Invalid PlainMonthDay"))?;
    let year = args.get(1).and_then(|v| v.as_int32()).unwrap_or(2000);

    let fields = CalendarFields::new().with_year(year);

    let date = md.to_plain_date(Some(fields))
        .map_err(|e| VmError::type_error(format!("toPlainDate failed: {:?}", e)))?;

    let s = date.to_ixdtf_string(DisplayCalendar::Auto);
    Ok(Value::string(JsString::intern(&s)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_plain_month_day_from() {
        let args = vec![Value::string(JsString::intern("--01-23"))];
        let result = plain_month_day_from(&args).unwrap();
        let s = result.as_string().unwrap().to_string();
        assert!(s.contains("01") && s.contains("23"));
    }

    #[test]
    fn test_plain_month_day_day() {
        let args = vec![Value::string(JsString::intern("--12-25"))];
        let result = plain_month_day_day(&args).unwrap();
        assert_eq!(result.as_int32(), Some(25));
    }
}
