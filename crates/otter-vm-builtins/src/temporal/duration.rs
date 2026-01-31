//! Temporal.Duration - represents a span of time

use otter_vm_core::error::VmError;
use otter_vm_core::string::JsString;
use otter_vm_core::value::Value;
use otter_vm_runtime::{Op, op_native};

pub fn ops() -> Vec<Op> {
    vec![
        op_native("__Temporal_Duration_from", duration_from),
        op_native("__Temporal_Duration_compare", duration_compare),
        op_native("__Temporal_Duration_years", duration_years),
        op_native("__Temporal_Duration_months", duration_months),
        op_native("__Temporal_Duration_weeks", duration_weeks),
        op_native("__Temporal_Duration_days", duration_days),
        op_native("__Temporal_Duration_hours", duration_hours),
        op_native("__Temporal_Duration_minutes", duration_minutes),
        op_native("__Temporal_Duration_seconds", duration_seconds),
        op_native("__Temporal_Duration_milliseconds", duration_milliseconds),
        op_native("__Temporal_Duration_microseconds", duration_microseconds),
        op_native("__Temporal_Duration_nanoseconds", duration_nanoseconds),
        op_native("__Temporal_Duration_sign", duration_sign),
        op_native("__Temporal_Duration_blank", duration_blank),
        op_native("__Temporal_Duration_negated", duration_negated),
        op_native("__Temporal_Duration_abs", duration_abs),
        op_native("__Temporal_Duration_add", duration_add),
        op_native("__Temporal_Duration_subtract", duration_subtract),
        op_native("__Temporal_Duration_round", duration_round),
        op_native("__Temporal_Duration_total", duration_total),
        op_native("__Temporal_Duration_toString", duration_to_string),
        op_native("__Temporal_Duration_toJSON", duration_to_json),
    ]
}

/// Duration components
#[derive(Clone, Default)]
struct DurationComponents {
    years: i32,
    months: i32,
    weeks: i32,
    days: i32,
    hours: i32,
    minutes: i32,
    seconds: i32,
    milliseconds: i32,
    microseconds: i32,
    nanoseconds: i64,
}

impl DurationComponents {
    fn total_nanoseconds(&self) -> i128 {
        let mut total: i128 = self.nanoseconds as i128;
        total += self.microseconds as i128 * 1_000;
        total += self.milliseconds as i128 * 1_000_000;
        total += self.seconds as i128 * 1_000_000_000;
        total += self.minutes as i128 * 60_000_000_000;
        total += self.hours as i128 * 3_600_000_000_000;
        total += self.days as i128 * 86_400_000_000_000;
        total += self.weeks as i128 * 604_800_000_000_000;
        // Note: months and years are calendar-dependent, simplified here
        total += self.months as i128 * 30 * 86_400_000_000_000;
        total += self.years as i128 * 365 * 86_400_000_000_000;
        total
    }

    fn sign(&self) -> i32 {
        let total = self.total_nanoseconds();
        if total > 0 {
            1
        } else if total < 0 {
            -1
        } else {
            0
        }
    }

    fn is_blank(&self) -> bool {
        self.total_nanoseconds() == 0
    }
}

fn parse_duration(s: &str) -> Option<DurationComponents> {
    // ISO 8601 duration format: P[n]Y[n]M[n]DT[n]H[n]M[n]S
    // Also accept: PT[n]H[n]M[n]S for time-only
    let s = s.trim();
    if !s.starts_with('P') && !s.starts_with('-') {
        return None;
    }

    let (negative, s) = if let Some(stripped) = s.strip_prefix('-') {
        (true, stripped)
    } else {
        (false, s)
    };

    let s = s.strip_prefix('P').unwrap_or(s);

    let mut d = DurationComponents::default();
    let mut current_num = String::new();
    let mut in_time = false;

    for c in s.chars() {
        if c.is_ascii_digit() || c == '.' || c == '-' {
            current_num.push(c);
        } else {
            let num: f64 = current_num.parse().unwrap_or(0.0);
            current_num.clear();

            match c {
                'Y' => d.years = num as i32,
                'M' if !in_time => d.months = num as i32,
                'W' => d.weeks = num as i32,
                'D' => d.days = num as i32,
                'T' => in_time = true,
                'H' => d.hours = num as i32,
                'M' if in_time => d.minutes = num as i32,
                'S' => {
                    d.seconds = num.trunc() as i32;
                    let frac = num.fract();
                    d.milliseconds = (frac * 1000.0).trunc() as i32;
                    d.microseconds = ((frac * 1_000_000.0).trunc() as i32) % 1000;
                    d.nanoseconds = ((frac * 1_000_000_000.0).trunc() as i64) % 1000;
                }
                _ => {}
            }
        }
    }

    if negative {
        d.years = -d.years;
        d.months = -d.months;
        d.weeks = -d.weeks;
        d.days = -d.days;
        d.hours = -d.hours;
        d.minutes = -d.minutes;
        d.seconds = -d.seconds;
        d.milliseconds = -d.milliseconds;
        d.microseconds = -d.microseconds;
        d.nanoseconds = -d.nanoseconds;
    }

    Some(d)
}

fn get_duration(args: &[Value]) -> Option<DurationComponents> {
    args.first()
        .and_then(|v| v.as_string())
        .and_then(|s| parse_duration(s.as_str()))
}

fn format_duration(d: &DurationComponents) -> String {
    let mut result = String::from("P");

    if d.years != 0 {
        result.push_str(&format!("{}Y", d.years.abs()));
    }
    if d.months != 0 {
        result.push_str(&format!("{}M", d.months.abs()));
    }
    if d.weeks != 0 {
        result.push_str(&format!("{}W", d.weeks.abs()));
    }
    if d.days != 0 {
        result.push_str(&format!("{}D", d.days.abs()));
    }

    let has_time = d.hours != 0
        || d.minutes != 0
        || d.seconds != 0
        || d.milliseconds != 0
        || d.microseconds != 0
        || d.nanoseconds != 0;

    if has_time {
        result.push('T');
        if d.hours != 0 {
            result.push_str(&format!("{}H", d.hours.abs()));
        }
        if d.minutes != 0 {
            result.push_str(&format!("{}M", d.minutes.abs()));
        }
        if d.seconds != 0 || d.milliseconds != 0 || d.microseconds != 0 || d.nanoseconds != 0 {
            let frac = (d.milliseconds.abs() as f64 / 1000.0)
                + (d.microseconds.abs() as f64 / 1_000_000.0)
                + (d.nanoseconds.abs() as f64 / 1_000_000_000.0);

            if frac > 0.0 {
                result.push_str(&format!("{}S", d.seconds.abs() as f64 + frac));
            } else {
                result.push_str(&format!("{}S", d.seconds.abs()));
            }
        }
    }

    if result == "P" {
        result = "PT0S".to_string();
    }

    if d.sign() < 0 {
        result = format!("-{}", result);
    }

    result
}

fn duration_from(args: &[Value]) -> Result<Value, VmError> {
    let s = args
        .first()
        .and_then(|v| v.as_string())
        .ok_or(VmError::type_error("Duration.from requires a string"))?;

    match parse_duration(s.as_str()) {
        Some(d) => Ok(Value::string(JsString::intern(&format_duration(&d)))),
        None => Err(VmError::type_error(format!(
            "Invalid Duration string: {}",
            s
        ))),
    }
}

fn duration_compare(args: &[Value]) -> Result<Value, VmError> {
    let d1 = get_duration(args);
    let d2 = args
        .get(1)
        .and_then(|v| v.as_string())
        .and_then(|s| parse_duration(s.as_str()));

    match (d1, d2) {
        (Some(a), Some(b)) => {
            let cmp = a.total_nanoseconds().cmp(&b.total_nanoseconds());
            Ok(Value::int32(match cmp {
                std::cmp::Ordering::Less => -1,
                std::cmp::Ordering::Equal => 0,
                std::cmp::Ordering::Greater => 1,
            }))
        }
        _ => Err(VmError::type_error("Invalid Duration for comparison")),
    }
}

fn duration_years(args: &[Value]) -> Result<Value, VmError> {
    get_duration(args)
        .map(|d| Value::int32(d.years))
        .ok_or_else(|| VmError::type_error("Invalid Duration"))
}

fn duration_months(args: &[Value]) -> Result<Value, VmError> {
    get_duration(args)
        .map(|d| Value::int32(d.months))
        .ok_or_else(|| VmError::type_error("Invalid Duration"))
}

fn duration_weeks(args: &[Value]) -> Result<Value, VmError> {
    get_duration(args)
        .map(|d| Value::int32(d.weeks))
        .ok_or_else(|| VmError::type_error("Invalid Duration"))
}

fn duration_days(args: &[Value]) -> Result<Value, VmError> {
    get_duration(args)
        .map(|d| Value::int32(d.days))
        .ok_or_else(|| VmError::type_error("Invalid Duration"))
}

fn duration_hours(args: &[Value]) -> Result<Value, VmError> {
    get_duration(args)
        .map(|d| Value::int32(d.hours))
        .ok_or_else(|| VmError::type_error("Invalid Duration"))
}

fn duration_minutes(args: &[Value]) -> Result<Value, VmError> {
    get_duration(args)
        .map(|d| Value::int32(d.minutes))
        .ok_or_else(|| VmError::type_error("Invalid Duration"))
}

fn duration_seconds(args: &[Value]) -> Result<Value, VmError> {
    get_duration(args)
        .map(|d| Value::int32(d.seconds))
        .ok_or_else(|| VmError::type_error("Invalid Duration"))
}

fn duration_milliseconds(args: &[Value]) -> Result<Value, VmError> {
    get_duration(args)
        .map(|d| Value::int32(d.milliseconds))
        .ok_or_else(|| VmError::type_error("Invalid Duration"))
}

fn duration_microseconds(args: &[Value]) -> Result<Value, VmError> {
    get_duration(args)
        .map(|d| Value::int32(d.microseconds))
        .ok_or_else(|| VmError::type_error("Invalid Duration"))
}

fn duration_nanoseconds(args: &[Value]) -> Result<Value, VmError> {
    get_duration(args)
        .map(|d| Value::int32(d.nanoseconds as i32))
        .ok_or_else(|| VmError::type_error("Invalid Duration"))
}

fn duration_sign(args: &[Value]) -> Result<Value, VmError> {
    get_duration(args)
        .map(|d| Value::int32(d.sign()))
        .ok_or_else(|| VmError::type_error("Invalid Duration"))
}

fn duration_blank(args: &[Value]) -> Result<Value, VmError> {
    get_duration(args)
        .map(|d| Value::boolean(d.is_blank()))
        .ok_or_else(|| VmError::type_error("Invalid Duration"))
}

fn duration_negated(args: &[Value]) -> Result<Value, VmError> {
    get_duration(args)
        .map(|d| {
            let neg = DurationComponents {
                years: -d.years,
                months: -d.months,
                weeks: -d.weeks,
                days: -d.days,
                hours: -d.hours,
                minutes: -d.minutes,
                seconds: -d.seconds,
                milliseconds: -d.milliseconds,
                microseconds: -d.microseconds,
                nanoseconds: -d.nanoseconds,
            };
            Value::string(JsString::intern(&format_duration(&neg)))
        })
        .ok_or_else(|| VmError::type_error("Invalid Duration"))
}

fn duration_abs(args: &[Value]) -> Result<Value, VmError> {
    get_duration(args)
        .map(|d| {
            let abs = DurationComponents {
                years: d.years.abs(),
                months: d.months.abs(),
                weeks: d.weeks.abs(),
                days: d.days.abs(),
                hours: d.hours.abs(),
                minutes: d.minutes.abs(),
                seconds: d.seconds.abs(),
                milliseconds: d.milliseconds.abs(),
                microseconds: d.microseconds.abs(),
                nanoseconds: d.nanoseconds.abs(),
            };
            Value::string(JsString::intern(&format_duration(&abs)))
        })
        .ok_or_else(|| VmError::type_error("Invalid Duration"))
}

fn duration_add(args: &[Value]) -> Result<Value, VmError> {
    let d1 = get_duration(args).ok_or(VmError::type_error("Invalid Duration"))?;
    let d2 = args
        .get(1)
        .and_then(|v| v.as_string())
        .and_then(|s| parse_duration(s.as_str()))
        .ok_or(VmError::type_error("Invalid Duration to add"))?;

    let sum = DurationComponents {
        years: d1.years + d2.years,
        months: d1.months + d2.months,
        weeks: d1.weeks + d2.weeks,
        days: d1.days + d2.days,
        hours: d1.hours + d2.hours,
        minutes: d1.minutes + d2.minutes,
        seconds: d1.seconds + d2.seconds,
        milliseconds: d1.milliseconds + d2.milliseconds,
        microseconds: d1.microseconds + d2.microseconds,
        nanoseconds: d1.nanoseconds + d2.nanoseconds,
    };

    Ok(Value::string(JsString::intern(&format_duration(&sum))))
}

fn duration_subtract(args: &[Value]) -> Result<Value, VmError> {
    let d1 = get_duration(args).ok_or(VmError::type_error("Invalid Duration"))?;
    let d2 = args
        .get(1)
        .and_then(|v| v.as_string())
        .and_then(|s| parse_duration(s.as_str()))
        .ok_or(VmError::type_error("Invalid Duration to subtract"))?;

    let diff = DurationComponents {
        years: d1.years - d2.years,
        months: d1.months - d2.months,
        weeks: d1.weeks - d2.weeks,
        days: d1.days - d2.days,
        hours: d1.hours - d2.hours,
        minutes: d1.minutes - d2.minutes,
        seconds: d1.seconds - d2.seconds,
        milliseconds: d1.milliseconds - d2.milliseconds,
        microseconds: d1.microseconds - d2.microseconds,
        nanoseconds: d1.nanoseconds - d2.nanoseconds,
    };

    Ok(Value::string(JsString::intern(&format_duration(&diff))))
}

fn duration_round(args: &[Value]) -> Result<Value, VmError> {
    let d = get_duration(args).ok_or(VmError::type_error("Invalid Duration"))?;
    let unit = args
        .get(1)
        .and_then(|v| v.as_string())
        .map(|s| s.to_string());

    // Simplified rounding - just return as-is for now
    // Full implementation would balance and round based on unit
    let _ = unit;
    Ok(Value::string(JsString::intern(&format_duration(&d))))
}

fn duration_total(args: &[Value]) -> Result<Value, VmError> {
    let d = get_duration(args).ok_or(VmError::type_error("Invalid Duration"))?;
    let unit = args
        .get(1)
        .and_then(|v| v.as_string())
        .map(|s| s.to_string());

    let total_ns = d.total_nanoseconds() as f64;

    let result = match unit.as_deref() {
        Some("nanoseconds") | Some("nanosecond") => total_ns,
        Some("microseconds") | Some("microsecond") => total_ns / 1_000.0,
        Some("milliseconds") | Some("millisecond") => total_ns / 1_000_000.0,
        Some("seconds") | Some("second") => total_ns / 1_000_000_000.0,
        Some("minutes") | Some("minute") => total_ns / 60_000_000_000.0,
        Some("hours") | Some("hour") => total_ns / 3_600_000_000_000.0,
        Some("days") | Some("day") => total_ns / 86_400_000_000_000.0,
        Some("weeks") | Some("week") => total_ns / 604_800_000_000_000.0,
        _ => total_ns / 1_000_000_000.0, // Default to seconds
    };

    Ok(Value::number(result))
}

fn duration_to_string(args: &[Value]) -> Result<Value, VmError> {
    get_duration(args)
        .map(|d| Value::string(JsString::intern(&format_duration(&d))))
        .ok_or_else(|| VmError::type_error("Invalid Duration"))
}

fn duration_to_json(args: &[Value]) -> Result<Value, VmError> {
    duration_to_string(args)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_duration_from() {
        let args = vec![Value::string(JsString::intern("P1Y2M3DT4H5M6S"))];
        let result = duration_from(&args).unwrap();
        let s = result.as_string().unwrap().to_string();
        assert!(s.contains("1Y"));
        assert!(s.contains("2M"));
        assert!(s.contains("3D"));
    }

    #[test]
    fn test_duration_days() {
        let args = vec![Value::string(JsString::intern("P5D"))];
        let result = duration_days(&args).unwrap();
        assert_eq!(result.as_int32(), Some(5));
    }

    #[test]
    fn test_duration_total() {
        let args = vec![
            Value::string(JsString::intern("PT1H")),
            Value::string(JsString::intern("minutes")),
        ];
        let result = duration_total(&args).unwrap();
        assert_eq!(result.as_number(), Some(60.0));
    }

    #[test]
    fn test_duration_add() {
        let args = vec![
            Value::string(JsString::intern("P1D")),
            Value::string(JsString::intern("P2D")),
        ];
        let result = duration_add(&args).unwrap();
        let s = result.as_string().unwrap().to_string();
        assert!(s.contains("3D"));
    }
}
