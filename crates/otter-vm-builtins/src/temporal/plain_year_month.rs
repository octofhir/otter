//! Temporal.PlainYearMonth - year and month only

use chrono::NaiveDate;
use otter_vm_core::string::JsString;
use otter_vm_core::value::Value;
use otter_vm_runtime::{Op, op_native};

pub fn ops() -> Vec<Op> {
    vec![
        op_native("__Temporal_PlainYearMonth_from", plain_year_month_from),
        op_native(
            "__Temporal_PlainYearMonth_compare",
            plain_year_month_compare,
        ),
        op_native("__Temporal_PlainYearMonth_year", plain_year_month_year),
        op_native("__Temporal_PlainYearMonth_month", plain_year_month_month),
        op_native(
            "__Temporal_PlainYearMonth_monthCode",
            plain_year_month_month_code,
        ),
        op_native(
            "__Temporal_PlainYearMonth_daysInMonth",
            plain_year_month_days_in_month,
        ),
        op_native(
            "__Temporal_PlainYearMonth_daysInYear",
            plain_year_month_days_in_year,
        ),
        op_native(
            "__Temporal_PlainYearMonth_monthsInYear",
            plain_year_month_months_in_year,
        ),
        op_native(
            "__Temporal_PlainYearMonth_inLeapYear",
            plain_year_month_in_leap_year,
        ),
        op_native("__Temporal_PlainYearMonth_add", plain_year_month_add),
        op_native(
            "__Temporal_PlainYearMonth_subtract",
            plain_year_month_subtract,
        ),
        op_native("__Temporal_PlainYearMonth_equals", plain_year_month_equals),
        op_native(
            "__Temporal_PlainYearMonth_toString",
            plain_year_month_to_string,
        ),
        op_native("__Temporal_PlainYearMonth_toJSON", plain_year_month_to_json),
        op_native(
            "__Temporal_PlainYearMonth_toPlainDate",
            plain_year_month_to_plain_date,
        ),
    ]
}

fn parse_year_month(s: &str) -> Option<(i32, u32)> {
    // Format: YYYY-MM
    let parts: Vec<&str> = s.split('-').collect();
    if parts.len() >= 2 {
        let year = parts[0].parse::<i32>().ok()?;
        let month = parts[1].parse::<u32>().ok()?;
        if (1..=12).contains(&month) {
            return Some((year, month));
        }
    }
    None
}

fn get_year_month(args: &[Value]) -> Option<(i32, u32)> {
    args.first()
        .and_then(|v| v.as_string())
        .and_then(|s| parse_year_month(s.as_str()))
}

fn plain_year_month_from(args: &[Value]) -> Result<Value, String> {
    let s = args
        .first()
        .and_then(|v| v.as_string())
        .ok_or("PlainYearMonth.from requires a string")?;

    // Extract YYYY-MM from various formats
    let ym_str = s.as_str().split('T').next().unwrap_or(s.as_str());
    let parts: Vec<&str> = ym_str.split('-').collect();

    if parts.len() >= 2 {
        let year: i32 = parts[0].parse().map_err(|_| "Invalid year")?;
        let month: u32 = parts[1].parse().map_err(|_| "Invalid month")?;
        if (1..=12).contains(&month) {
            return Ok(Value::string(JsString::intern(&format!(
                "{:04}-{:02}",
                year, month
            ))));
        }
    }

    Err(format!("Invalid PlainYearMonth string: {}", s))
}

fn plain_year_month_compare(args: &[Value]) -> Result<Value, String> {
    let ym1 = get_year_month(args);
    let ym2 = args
        .get(1)
        .and_then(|v| v.as_string())
        .and_then(|s| parse_year_month(s.as_str()));

    match (ym1, ym2) {
        (Some(a), Some(b)) => {
            let cmp = a.cmp(&b);
            Ok(Value::int32(match cmp {
                std::cmp::Ordering::Less => -1,
                std::cmp::Ordering::Equal => 0,
                std::cmp::Ordering::Greater => 1,
            }))
        }
        _ => Err("Invalid PlainYearMonth for comparison".to_string()),
    }
}

fn plain_year_month_year(args: &[Value]) -> Result<Value, String> {
    get_year_month(args)
        .map(|(year, _)| Value::int32(year))
        .ok_or_else(|| "Invalid PlainYearMonth".to_string())
}

fn plain_year_month_month(args: &[Value]) -> Result<Value, String> {
    get_year_month(args)
        .map(|(_, month)| Value::int32(month as i32))
        .ok_or_else(|| "Invalid PlainYearMonth".to_string())
}

fn plain_year_month_month_code(args: &[Value]) -> Result<Value, String> {
    get_year_month(args)
        .map(|(_, month)| Value::string(JsString::intern(&format!("M{:02}", month))))
        .ok_or_else(|| "Invalid PlainYearMonth".to_string())
}

fn plain_year_month_days_in_month(args: &[Value]) -> Result<Value, String> {
    get_year_month(args)
        .and_then(|(year, month)| {
            let first = NaiveDate::from_ymd_opt(year, month, 1)?;
            let next = if month == 12 {
                NaiveDate::from_ymd_opt(year + 1, 1, 1)?
            } else {
                NaiveDate::from_ymd_opt(year, month + 1, 1)?
            };
            Some(Value::int32((next - first).num_days() as i32))
        })
        .ok_or_else(|| "Invalid PlainYearMonth".to_string())
}

fn plain_year_month_days_in_year(args: &[Value]) -> Result<Value, String> {
    get_year_month(args)
        .and_then(|(year, _)| {
            NaiveDate::from_ymd_opt(year, 1, 1)
                .map(|d| Value::int32(if d.leap_year() { 366 } else { 365 }))
        })
        .ok_or_else(|| "Invalid PlainYearMonth".to_string())
}

fn plain_year_month_months_in_year(_args: &[Value]) -> Result<Value, String> {
    Ok(Value::int32(12))
}

fn plain_year_month_in_leap_year(args: &[Value]) -> Result<Value, String> {
    get_year_month(args)
        .and_then(|(year, _)| {
            NaiveDate::from_ymd_opt(year, 1, 1).map(|d| Value::boolean(d.leap_year()))
        })
        .ok_or_else(|| "Invalid PlainYearMonth".to_string())
}

fn plain_year_month_add(args: &[Value]) -> Result<Value, String> {
    let (year, month) = get_year_month(args).ok_or("Invalid PlainYearMonth")?;
    let add_months = args.get(1).and_then(|v| v.as_int32()).unwrap_or(0);

    let total_months = year * 12 + (month as i32 - 1) + add_months;
    let new_year = total_months.div_euclid(12);
    let new_month = (total_months.rem_euclid(12) + 1) as u32;

    Ok(Value::string(JsString::intern(&format!(
        "{:04}-{:02}",
        new_year, new_month
    ))))
}

fn plain_year_month_subtract(args: &[Value]) -> Result<Value, String> {
    let (year, month) = get_year_month(args).ok_or("Invalid PlainYearMonth")?;
    let sub_months = args.get(1).and_then(|v| v.as_int32()).unwrap_or(0);

    let total_months = year * 12 + (month as i32 - 1) - sub_months;
    let new_year = total_months.div_euclid(12);
    let new_month = (total_months.rem_euclid(12) + 1) as u32;

    Ok(Value::string(JsString::intern(&format!(
        "{:04}-{:02}",
        new_year, new_month
    ))))
}

fn plain_year_month_equals(args: &[Value]) -> Result<Value, String> {
    let ym1 = get_year_month(args);
    let ym2 = args
        .get(1)
        .and_then(|v| v.as_string())
        .and_then(|s| parse_year_month(s.as_str()));

    Ok(Value::boolean(ym1 == ym2))
}

fn plain_year_month_to_string(args: &[Value]) -> Result<Value, String> {
    get_year_month(args)
        .map(|(year, month)| Value::string(JsString::intern(&format!("{:04}-{:02}", year, month))))
        .ok_or_else(|| "Invalid PlainYearMonth".to_string())
}

fn plain_year_month_to_json(args: &[Value]) -> Result<Value, String> {
    plain_year_month_to_string(args)
}

fn plain_year_month_to_plain_date(args: &[Value]) -> Result<Value, String> {
    let (year, month) = get_year_month(args).ok_or("Invalid PlainYearMonth")?;
    let day = args.get(1).and_then(|v| v.as_int32()).unwrap_or(1) as u32;

    NaiveDate::from_ymd_opt(year, month, day)
        .map(|d| Value::string(JsString::intern(&d.format("%Y-%m-%d").to_string())))
        .ok_or_else(|| "Invalid day for month".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_plain_year_month_from() {
        let args = vec![Value::string(JsString::intern("2026-01"))];
        let result = plain_year_month_from(&args).unwrap();
        let s = result.as_string().unwrap().to_string();
        assert_eq!(s, "2026-01");
    }

    #[test]
    fn test_plain_year_month_add() {
        let args = vec![Value::string(JsString::intern("2026-11")), Value::int32(3)];
        let result = plain_year_month_add(&args).unwrap();
        let s = result.as_string().unwrap().to_string();
        assert_eq!(s, "2027-02");
    }
}
