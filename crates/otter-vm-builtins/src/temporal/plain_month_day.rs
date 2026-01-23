//! Temporal.PlainMonthDay - month and day only (e.g., birthdays, holidays)

use chrono::{Datelike, NaiveDate};
use otter_vm_core::string::JsString;
use otter_vm_core::value::Value;
use otter_vm_runtime::{Op, op_native};

pub fn ops() -> Vec<Op> {
    vec![
        op_native("__Temporal_PlainMonthDay_from", plain_month_day_from),
        op_native("__Temporal_PlainMonthDay_month", plain_month_day_month),
        op_native(
            "__Temporal_PlainMonthDay_monthCode",
            plain_month_day_month_code,
        ),
        op_native("__Temporal_PlainMonthDay_day", plain_month_day_day),
        op_native("__Temporal_PlainMonthDay_equals", plain_month_day_equals),
        op_native(
            "__Temporal_PlainMonthDay_toString",
            plain_month_day_to_string,
        ),
        op_native("__Temporal_PlainMonthDay_toJSON", plain_month_day_to_json),
        op_native(
            "__Temporal_PlainMonthDay_toPlainDate",
            plain_month_day_to_plain_date,
        ),
    ]
}

fn parse_month_day(s: &str) -> Option<(u32, u32)> {
    // Format: --MM-DD or MM-DD
    let s = s.trim_start_matches('-');
    let parts: Vec<&str> = s.split('-').collect();

    if parts.len() >= 2 {
        let month = parts[parts.len() - 2].parse::<u32>().ok()?;
        let day = parts[parts.len() - 1].parse::<u32>().ok()?;
        if (1..=12).contains(&month) && (1..=31).contains(&day) {
            // Validate the day is valid for the month (use 2000 as reference year for Feb)
            if NaiveDate::from_ymd_opt(2000, month, day).is_some() {
                return Some((month, day));
            }
        }
    }
    None
}

fn get_month_day(args: &[Value]) -> Option<(u32, u32)> {
    args.first()
        .and_then(|v| v.as_string())
        .and_then(|s| parse_month_day(s.as_str()))
}

fn plain_month_day_from(args: &[Value]) -> Result<Value, String> {
    let s = args
        .first()
        .and_then(|v| v.as_string())
        .ok_or("PlainMonthDay.from requires a string")?;

    match parse_month_day(s.as_str()) {
        Some((month, day)) => Ok(Value::string(JsString::intern(&format!(
            "--{:02}-{:02}",
            month, day
        )))),
        None => Err(format!("Invalid PlainMonthDay string: {}", s)),
    }
}

fn plain_month_day_month(args: &[Value]) -> Result<Value, String> {
    get_month_day(args)
        .map(|(month, _)| Value::int32(month as i32))
        .ok_or_else(|| "Invalid PlainMonthDay".to_string())
}

fn plain_month_day_month_code(args: &[Value]) -> Result<Value, String> {
    get_month_day(args)
        .map(|(month, _)| Value::string(JsString::intern(&format!("M{:02}", month))))
        .ok_or_else(|| "Invalid PlainMonthDay".to_string())
}

fn plain_month_day_day(args: &[Value]) -> Result<Value, String> {
    get_month_day(args)
        .map(|(_, day)| Value::int32(day as i32))
        .ok_or_else(|| "Invalid PlainMonthDay".to_string())
}

fn plain_month_day_equals(args: &[Value]) -> Result<Value, String> {
    let md1 = get_month_day(args);
    let md2 = args
        .get(1)
        .and_then(|v| v.as_string())
        .and_then(|s| parse_month_day(s.as_str()));

    Ok(Value::boolean(md1 == md2))
}

fn plain_month_day_to_string(args: &[Value]) -> Result<Value, String> {
    get_month_day(args)
        .map(|(month, day)| Value::string(JsString::intern(&format!("--{:02}-{:02}", month, day))))
        .ok_or_else(|| "Invalid PlainMonthDay".to_string())
}

fn plain_month_day_to_json(args: &[Value]) -> Result<Value, String> {
    plain_month_day_to_string(args)
}

fn plain_month_day_to_plain_date(args: &[Value]) -> Result<Value, String> {
    let (month, day) = get_month_day(args).ok_or("Invalid PlainMonthDay")?;
    let year = args
        .get(1)
        .and_then(|v| v.as_int32())
        .unwrap_or(chrono::Local::now().year());

    NaiveDate::from_ymd_opt(year, month, day)
        .map(|d| Value::string(JsString::intern(&d.format("%Y-%m-%d").to_string())))
        .ok_or_else(|| format!("Invalid date: {}-{:02}-{:02}", year, month, day))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_plain_month_day_from() {
        let args = vec![Value::string(JsString::intern("--01-23"))];
        let result = plain_month_day_from(&args).unwrap();
        let s = result.as_string().unwrap().to_string();
        assert_eq!(s, "--01-23");
    }

    #[test]
    fn test_plain_month_day_from_without_prefix() {
        let args = vec![Value::string(JsString::intern("01-23"))];
        let result = plain_month_day_from(&args).unwrap();
        let s = result.as_string().unwrap().to_string();
        assert_eq!(s, "--01-23");
    }

    #[test]
    fn test_plain_month_day_month() {
        let args = vec![Value::string(JsString::intern("--12-25"))];
        let result = plain_month_day_month(&args).unwrap();
        assert_eq!(result.as_int32(), Some(12));
    }

    #[test]
    fn test_plain_month_day_to_plain_date() {
        let args = vec![
            Value::string(JsString::intern("--01-23")),
            Value::int32(2026),
        ];
        let result = plain_month_day_to_plain_date(&args).unwrap();
        let s = result.as_string().unwrap().to_string();
        assert_eq!(s, "2026-01-23");
    }
}
