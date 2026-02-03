//! Temporal.PlainYearMonth - year and month only

use otter_vm_core::value::Value;
use otter_vm_core::{VmError, string::JsString};
use otter_vm_runtime::{Op, op_native};
use temporal_rs::PlainYearMonth;
use temporal_rs::options::DisplayCalendar;

pub fn ops() -> Vec<Op> {
    vec![
        op_native("__Temporal_PlainYearMonth_from", plain_year_month_from),
        op_native("__Temporal_PlainYearMonth_compare", plain_year_month_compare),
        op_native("__Temporal_PlainYearMonth_year", plain_year_month_year),
        op_native("__Temporal_PlainYearMonth_month", plain_year_month_month),
        op_native("__Temporal_PlainYearMonth_monthCode", plain_year_month_month_code),
        op_native("__Temporal_PlainYearMonth_daysInMonth", plain_year_month_days_in_month),
        op_native("__Temporal_PlainYearMonth_daysInYear", plain_year_month_days_in_year),
        op_native("__Temporal_PlainYearMonth_monthsInYear", plain_year_month_months_in_year),
        op_native("__Temporal_PlainYearMonth_inLeapYear", plain_year_month_in_leap_year),
        op_native("__Temporal_PlainYearMonth_add", plain_year_month_add),
        op_native("__Temporal_PlainYearMonth_subtract", plain_year_month_subtract),
        op_native("__Temporal_PlainYearMonth_equals", plain_year_month_equals),
        op_native("__Temporal_PlainYearMonth_toString", plain_year_month_to_string),
        op_native("__Temporal_PlainYearMonth_toJSON", plain_year_month_to_json),
        op_native("__Temporal_PlainYearMonth_toPlainDate", plain_year_month_to_plain_date),
    ]
}

fn parse_year_month(s: &str) -> Option<PlainYearMonth> {
    PlainYearMonth::from_utf8(s.as_bytes()).ok()
}

fn get_year_month(args: &[Value]) -> Option<PlainYearMonth> {
    args.first()
        .and_then(|v| v.as_string())
        .and_then(|s| parse_year_month(s.as_str()))
}

fn format_year_month(ym: &PlainYearMonth) -> String {
    ym.to_ixdtf_string(DisplayCalendar::Auto)
}

fn plain_year_month_from(args: &[Value]) -> Result<Value, VmError> {
    let s = args
        .first()
        .and_then(|v| v.as_string())
        .ok_or(VmError::type_error("PlainYearMonth.from requires a string"))?;

    match parse_year_month(s.as_str()) {
        Some(ym) => Ok(Value::string(JsString::intern(&format_year_month(&ym)))),
        None => Err(VmError::type_error(format!("Invalid PlainYearMonth string: {}", s))),
    }
}

fn plain_year_month_compare(args: &[Value]) -> Result<Value, VmError> {
    let ym1 = get_year_month(args);
    let ym2 = args.get(1)
        .and_then(|v| v.as_string())
        .and_then(|s| parse_year_month(s.as_str()));

    match (ym1, ym2) {
        (Some(a), Some(b)) => {
            let cmp = a.compare_iso(&b);
            Ok(Value::int32(cmp as i8 as i32))
        }
        _ => Err(VmError::type_error("Invalid PlainYearMonth for comparison")),
    }
}

fn plain_year_month_year(args: &[Value]) -> Result<Value, VmError> {
    get_year_month(args)
        .map(|ym| Value::int32(ym.year()))
        .ok_or_else(|| VmError::type_error("Invalid PlainYearMonth"))
}

fn plain_year_month_month(args: &[Value]) -> Result<Value, VmError> {
    get_year_month(args)
        .map(|ym| Value::int32(ym.month() as i32))
        .ok_or_else(|| VmError::type_error("Invalid PlainYearMonth"))
}

fn plain_year_month_month_code(args: &[Value]) -> Result<Value, VmError> {
    get_year_month(args)
        .map(|ym| Value::string(JsString::intern(ym.month_code().as_str())))
        .ok_or_else(|| VmError::type_error("Invalid PlainYearMonth"))
}

fn plain_year_month_days_in_month(args: &[Value]) -> Result<Value, VmError> {
    get_year_month(args)
        .map(|ym| Value::int32(ym.days_in_month() as i32))
        .ok_or_else(|| VmError::type_error("Invalid PlainYearMonth"))
}

fn plain_year_month_days_in_year(args: &[Value]) -> Result<Value, VmError> {
    get_year_month(args)
        .map(|ym| Value::int32(ym.days_in_year() as i32))
        .ok_or_else(|| VmError::type_error("Invalid PlainYearMonth"))
}

fn plain_year_month_months_in_year(args: &[Value]) -> Result<Value, VmError> {
    get_year_month(args)
        .map(|ym| Value::int32(ym.months_in_year() as i32))
        .ok_or_else(|| VmError::type_error("Invalid PlainYearMonth"))
}

fn plain_year_month_in_leap_year(args: &[Value]) -> Result<Value, VmError> {
    get_year_month(args)
        .map(|ym| Value::boolean(ym.in_leap_year()))
        .ok_or_else(|| VmError::type_error("Invalid PlainYearMonth"))
}

fn plain_year_month_add(args: &[Value]) -> Result<Value, VmError> {
    use temporal_rs::options::Overflow;

    let ym = get_year_month(args).ok_or(VmError::type_error("Invalid PlainYearMonth"))?;
    let duration_str = args.get(1)
        .and_then(|v| v.as_string())
        .ok_or(VmError::type_error("Duration required"))?;

    let duration = temporal_rs::Duration::from_utf8(duration_str.as_str().as_bytes())
        .map_err(|e| VmError::type_error(format!("Invalid duration: {:?}", e)))?;

    let new_ym = ym.add(&duration, Overflow::Constrain)
        .map_err(|e| VmError::type_error(format!("Add failed: {:?}", e)))?;

    Ok(Value::string(JsString::intern(&format_year_month(&new_ym))))
}

fn plain_year_month_subtract(args: &[Value]) -> Result<Value, VmError> {
    use temporal_rs::options::Overflow;

    let ym = get_year_month(args).ok_or(VmError::type_error("Invalid PlainYearMonth"))?;
    let duration_str = args.get(1)
        .and_then(|v| v.as_string())
        .ok_or(VmError::type_error("Duration required"))?;

    let duration = temporal_rs::Duration::from_utf8(duration_str.as_str().as_bytes())
        .map_err(|e| VmError::type_error(format!("Invalid duration: {:?}", e)))?;

    let new_ym = ym.subtract(&duration, Overflow::Constrain)
        .map_err(|e| VmError::type_error(format!("Subtract failed: {:?}", e)))?;

    Ok(Value::string(JsString::intern(&format_year_month(&new_ym))))
}

fn plain_year_month_equals(args: &[Value]) -> Result<Value, VmError> {
    let ym1 = get_year_month(args);
    let ym2 = args.get(1)
        .and_then(|v| v.as_string())
        .and_then(|s| parse_year_month(s.as_str()));

    match (ym1, ym2) {
        (Some(a), Some(b)) => Ok(Value::boolean(a == b)),
        _ => Ok(Value::boolean(false)),
    }
}

fn plain_year_month_to_string(args: &[Value]) -> Result<Value, VmError> {
    get_year_month(args)
        .map(|ym| Value::string(JsString::intern(&format_year_month(&ym))))
        .ok_or_else(|| VmError::type_error("Invalid PlainYearMonth"))
}

fn plain_year_month_to_json(args: &[Value]) -> Result<Value, VmError> {
    plain_year_month_to_string(args)
}

fn plain_year_month_to_plain_date(args: &[Value]) -> Result<Value, VmError> {
    use temporal_rs::fields::CalendarFields;

    let ym = get_year_month(args).ok_or(VmError::type_error("Invalid PlainYearMonth"))?;
    let day = args.get(1).and_then(|v| v.as_int32()).unwrap_or(1) as u8;

    let fields = CalendarFields::new().with_day(day);

    let date = ym.to_plain_date(Some(fields))
        .map_err(|e| VmError::type_error(format!("toPlainDate failed: {:?}", e)))?;

    let s = date.to_ixdtf_string(DisplayCalendar::Auto);
    Ok(Value::string(JsString::intern(&s)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_plain_year_month_from() {
        let args = vec![Value::string(JsString::intern("2026-01"))];
        let result = plain_year_month_from(&args).unwrap();
        let s = result.as_string().unwrap().to_string();
        assert!(s.contains("2026") && s.contains("01"));
    }

    #[test]
    fn test_plain_year_month_year() {
        let args = vec![Value::string(JsString::intern("2026-01"))];
        let result = plain_year_month_year(&args).unwrap();
        assert_eq!(result.as_int32(), Some(2026));
    }
}
