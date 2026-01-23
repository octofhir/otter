//! Temporal.PlainDate - calendar date without time or timezone

use chrono::{Datelike, NaiveDate};
use otter_vm_core::string::JsString;
use otter_vm_core::value::Value;
use otter_vm_runtime::{Op, op_native};

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
        op_native("__Temporal_PlainDate_weekOfYear", plain_date_week_of_year),
        op_native("__Temporal_PlainDate_daysInMonth", plain_date_days_in_month),
        op_native("__Temporal_PlainDate_daysInYear", plain_date_days_in_year),
        op_native(
            "__Temporal_PlainDate_monthsInYear",
            plain_date_months_in_year,
        ),
        op_native("__Temporal_PlainDate_inLeapYear", plain_date_in_leap_year),
        op_native("__Temporal_PlainDate_add", plain_date_add),
        op_native("__Temporal_PlainDate_subtract", plain_date_subtract),
        op_native("__Temporal_PlainDate_until", plain_date_until),
        op_native("__Temporal_PlainDate_since", plain_date_since),
        op_native("__Temporal_PlainDate_with", plain_date_with),
        op_native("__Temporal_PlainDate_equals", plain_date_equals),
        op_native("__Temporal_PlainDate_toString", plain_date_to_string),
        op_native("__Temporal_PlainDate_toJSON", plain_date_to_json),
        op_native(
            "__Temporal_PlainDate_toPlainDateTime",
            plain_date_to_plain_date_time,
        ),
        op_native(
            "__Temporal_PlainDate_toPlainYearMonth",
            plain_date_to_plain_year_month,
        ),
        op_native(
            "__Temporal_PlainDate_toPlainMonthDay",
            plain_date_to_plain_month_day,
        ),
    ]
}

/// Parse date string (YYYY-MM-DD) to NaiveDate
fn parse_date(s: &str) -> Option<NaiveDate> {
    NaiveDate::parse_from_str(s, "%Y-%m-%d").ok()
}

/// Parse date from Value (string format YYYY-MM-DD)
fn get_date(args: &[Value]) -> Option<NaiveDate> {
    args.first()
        .and_then(|v| v.as_string())
        .and_then(|s| parse_date(s.as_str()))
}

/// Temporal.PlainDate.from(thing)
fn plain_date_from(args: &[Value]) -> Result<Value, String> {
    let s = args
        .first()
        .and_then(|v| v.as_string())
        .ok_or("PlainDate.from requires a string")?;

    // Parse YYYY-MM-DD or ISO date part
    let date_str = s.as_str().split('T').next().unwrap_or(s.as_str());

    match parse_date(date_str) {
        Some(d) => Ok(Value::string(JsString::intern(
            &d.format("%Y-%m-%d").to_string(),
        ))),
        None => Err(format!("Invalid date string: {}", s)),
    }
}

/// Temporal.PlainDate.compare(one, two)
fn plain_date_compare(args: &[Value]) -> Result<Value, String> {
    let d1 = args
        .first()
        .and_then(|v| v.as_string())
        .and_then(|s| parse_date(s.as_str()));
    let d2 = args
        .get(1)
        .and_then(|v| v.as_string())
        .and_then(|s| parse_date(s.as_str()));

    match (d1, d2) {
        (Some(a), Some(b)) => {
            let cmp = a.cmp(&b);
            Ok(Value::int32(match cmp {
                std::cmp::Ordering::Less => -1,
                std::cmp::Ordering::Equal => 0,
                std::cmp::Ordering::Greater => 1,
            }))
        }
        _ => Err("Invalid dates for comparison".to_string()),
    }
}

fn plain_date_year(args: &[Value]) -> Result<Value, String> {
    get_date(args)
        .map(|d| Value::int32(d.year()))
        .ok_or_else(|| "Invalid PlainDate".to_string())
}

fn plain_date_month(args: &[Value]) -> Result<Value, String> {
    get_date(args)
        .map(|d| Value::int32(d.month() as i32))
        .ok_or_else(|| "Invalid PlainDate".to_string())
}

fn plain_date_month_code(args: &[Value]) -> Result<Value, String> {
    get_date(args)
        .map(|d| Value::string(JsString::intern(&format!("M{:02}", d.month()))))
        .ok_or_else(|| "Invalid PlainDate".to_string())
}

fn plain_date_day(args: &[Value]) -> Result<Value, String> {
    get_date(args)
        .map(|d| Value::int32(d.day() as i32))
        .ok_or_else(|| "Invalid PlainDate".to_string())
}

fn plain_date_day_of_week(args: &[Value]) -> Result<Value, String> {
    get_date(args)
        .map(|d| Value::int32(d.weekday().num_days_from_monday() as i32 + 1)) // 1=Mon, 7=Sun
        .ok_or_else(|| "Invalid PlainDate".to_string())
}

fn plain_date_day_of_year(args: &[Value]) -> Result<Value, String> {
    get_date(args)
        .map(|d| Value::int32(d.ordinal() as i32))
        .ok_or_else(|| "Invalid PlainDate".to_string())
}

fn plain_date_week_of_year(args: &[Value]) -> Result<Value, String> {
    get_date(args)
        .map(|d| Value::int32(d.iso_week().week() as i32))
        .ok_or_else(|| "Invalid PlainDate".to_string())
}

fn plain_date_days_in_month(args: &[Value]) -> Result<Value, String> {
    get_date(args)
        .map(|d| {
            let days = if d.month() == 12 {
                NaiveDate::from_ymd_opt(d.year() + 1, 1, 1)
            } else {
                NaiveDate::from_ymd_opt(d.year(), d.month() + 1, 1)
            }
            .map(|next| {
                next.signed_duration_since(NaiveDate::from_ymd_opt(d.year(), d.month(), 1).unwrap())
                    .num_days()
            })
            .unwrap_or(30);
            Value::int32(days as i32)
        })
        .ok_or_else(|| "Invalid PlainDate".to_string())
}

fn plain_date_days_in_year(args: &[Value]) -> Result<Value, String> {
    get_date(args)
        .map(|d| {
            let days = if d.leap_year() { 366 } else { 365 };
            Value::int32(days)
        })
        .ok_or_else(|| "Invalid PlainDate".to_string())
}

fn plain_date_months_in_year(_args: &[Value]) -> Result<Value, String> {
    Ok(Value::int32(12)) // ISO calendar always has 12 months
}

fn plain_date_in_leap_year(args: &[Value]) -> Result<Value, String> {
    get_date(args)
        .map(|d| Value::boolean(d.leap_year()))
        .ok_or_else(|| "Invalid PlainDate".to_string())
}

/// plainDate.add(duration)
fn plain_date_add(args: &[Value]) -> Result<Value, String> {
    let date = get_date(args).ok_or("Invalid PlainDate")?;

    // Duration passed as JSON object with years, months, days
    let days = args.get(1).and_then(|v| v.as_int32()).unwrap_or(0);

    let new_date = date + chrono::Duration::days(days as i64);
    Ok(Value::string(JsString::intern(
        &new_date.format("%Y-%m-%d").to_string(),
    )))
}

/// plainDate.subtract(duration)
fn plain_date_subtract(args: &[Value]) -> Result<Value, String> {
    let date = get_date(args).ok_or("Invalid PlainDate")?;
    let days = args.get(1).and_then(|v| v.as_int32()).unwrap_or(0);

    let new_date = date - chrono::Duration::days(days as i64);
    Ok(Value::string(JsString::intern(
        &new_date.format("%Y-%m-%d").to_string(),
    )))
}

/// plainDate.until(other)
fn plain_date_until(args: &[Value]) -> Result<Value, String> {
    let d1 = get_date(args).ok_or("Invalid PlainDate")?;
    let d2 = args
        .get(1)
        .and_then(|v| v.as_string())
        .and_then(|s| parse_date(s.as_str()))
        .ok_or("Invalid target date")?;

    let days = d2.signed_duration_since(d1).num_days();
    Ok(Value::int32(days as i32))
}

/// plainDate.since(other)
fn plain_date_since(args: &[Value]) -> Result<Value, String> {
    let d1 = get_date(args).ok_or("Invalid PlainDate")?;
    let d2 = args
        .get(1)
        .and_then(|v| v.as_string())
        .and_then(|s| parse_date(s.as_str()))
        .ok_or("Invalid target date")?;

    let days = d1.signed_duration_since(d2).num_days();
    Ok(Value::int32(days as i32))
}

/// plainDate.with(partialPlainDate)
fn plain_date_with(args: &[Value]) -> Result<Value, String> {
    let date = get_date(args).ok_or("Invalid PlainDate")?;

    // Get replacement values (simplified - expect year, month, day as separate args)
    let year = args
        .get(1)
        .and_then(|v| v.as_int32())
        .unwrap_or(date.year());
    let month = args
        .get(2)
        .and_then(|v| v.as_int32())
        .map(|m| m as u32)
        .unwrap_or(date.month());
    let day = args
        .get(3)
        .and_then(|v| v.as_int32())
        .map(|d| d as u32)
        .unwrap_or(date.day());

    NaiveDate::from_ymd_opt(year, month, day)
        .map(|d| Value::string(JsString::intern(&d.format("%Y-%m-%d").to_string())))
        .ok_or_else(|| "Invalid date components".to_string())
}

fn plain_date_equals(args: &[Value]) -> Result<Value, String> {
    let d1 = get_date(args);
    let d2 = args
        .get(1)
        .and_then(|v| v.as_string())
        .and_then(|s| parse_date(s.as_str()));

    Ok(Value::boolean(d1 == d2))
}

fn plain_date_to_string(args: &[Value]) -> Result<Value, String> {
    get_date(args)
        .map(|d| Value::string(JsString::intern(&d.format("%Y-%m-%d").to_string())))
        .ok_or_else(|| "Invalid PlainDate".to_string())
}

fn plain_date_to_json(args: &[Value]) -> Result<Value, String> {
    plain_date_to_string(args)
}

/// plainDate.toPlainDateTime(plainTime?)
fn plain_date_to_plain_date_time(args: &[Value]) -> Result<Value, String> {
    let date = get_date(args).ok_or("Invalid PlainDate")?;
    let time = args
        .get(1)
        .and_then(|v| v.as_string())
        .map(|s| s.to_string())
        .unwrap_or_else(|| "00:00:00".to_string());

    Ok(Value::string(JsString::intern(&format!(
        "{}T{}",
        date.format("%Y-%m-%d"),
        time
    ))))
}

/// plainDate.toPlainYearMonth()
fn plain_date_to_plain_year_month(args: &[Value]) -> Result<Value, String> {
    get_date(args)
        .map(|d| Value::string(JsString::intern(&d.format("%Y-%m").to_string())))
        .ok_or_else(|| "Invalid PlainDate".to_string())
}

/// plainDate.toPlainMonthDay()
fn plain_date_to_plain_month_day(args: &[Value]) -> Result<Value, String> {
    get_date(args)
        .map(|d| {
            Value::string(JsString::intern(&format!(
                "--{:02}-{:02}",
                d.month(),
                d.day()
            )))
        })
        .ok_or_else(|| "Invalid PlainDate".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_plain_date_from() {
        let args = vec![Value::string(JsString::intern("2026-01-23"))];
        let result = plain_date_from(&args).unwrap();
        let s = result.as_string().unwrap().to_string();
        assert_eq!(s, "2026-01-23");
    }

    #[test]
    fn test_plain_date_year() {
        let args = vec![Value::string(JsString::intern("2026-01-23"))];
        let result = plain_date_year(&args).unwrap();
        assert_eq!(result.as_int32(), Some(2026));
    }

    #[test]
    fn test_plain_date_compare() {
        let args = vec![
            Value::string(JsString::intern("2026-01-23")),
            Value::string(JsString::intern("2026-01-24")),
        ];
        let result = plain_date_compare(&args).unwrap();
        assert_eq!(result.as_int32(), Some(-1));
    }

    #[test]
    fn test_plain_date_add() {
        let args = vec![
            Value::string(JsString::intern("2026-01-23")),
            Value::int32(7),
        ];
        let result = plain_date_add(&args).unwrap();
        let s = result.as_string().unwrap().to_string();
        assert_eq!(s, "2026-01-30");
    }
}
