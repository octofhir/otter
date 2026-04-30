//! `Date(...)` / `Date.<static>(...)` dispatcher per ECMA-262
//! §21.4.2 / §21.4.3. Routed through
//! [`crate::otter_bytecode::Op::DateCall`] by the compiler.
//!
//! # Contents
//! - [`call`] — single entry point used by the dispatch loop.
//!
//! # See also
//! - <https://tc39.es/ecma262/#sec-date-constructor>

use super::{JsDate, make_date};
use crate::number::NumberValue;
use crate::{Value, VmError};

/// Dispatch `Date(...)` / `Date.<name>(...)`. Empty `name` selects
/// the constructor.
///
/// # Errors
/// - [`VmError::TypeMismatch`] for malformed inputs.
/// - [`VmError::UnknownIntrinsic`] for unknown method names.
pub fn call(name: &str, args: &[Value]) -> Result<Value, VmError> {
    match name {
        // §21.4.2 — `new Date(...)`. Forms:
        //   - 0 args: now.
        //   - 1 arg (string): parse via Date.parse.
        //   - 1 arg (number / Date): epoch ms.
        //   - 2+ args: (year, month, day?, hr?, min?, sec?, ms?).
        "" => match args.len() {
            0 => Ok(Value::Date(JsDate::now())),
            1 => match &args[0] {
                Value::String(s) => Ok(Value::Date(JsDate::from_ms(parse_date(
                    &s.to_lossy_string(),
                )))),
                Value::Number(n) => Ok(Value::Date(JsDate::from_ms(n.as_f64()))),
                Value::Date(d) => Ok(Value::Date(JsDate::from_ms(d.time()))),
                _ => Ok(Value::Date(JsDate::invalid())),
            },
            _ => {
                let year = number_arg(args, 0);
                let month = number_arg(args, 1);
                let day = number_or(args, 2, 1.0);
                let hours = number_or(args, 3, 0.0);
                let minutes = number_or(args, 4, 0.0);
                let seconds = number_or(args, 5, 0.0);
                let ms = number_or(args, 6, 0.0);
                Ok(Value::Date(JsDate::from_ms(make_date(
                    year, month, day, hours, minutes, seconds, ms,
                ))))
            }
        },
        // §21.4.3.1 Date.now() — current epoch ms as a Number.
        "now" => Ok(Value::Number(NumberValue::from_f64(JsDate::now().time()))),
        // §21.4.3.2 Date.parse(str).
        "parse" => {
            let s = match args.first() {
                Some(Value::String(s)) => s.to_lossy_string(),
                _ => return Ok(Value::Number(NumberValue::from_f64(f64::NAN))),
            };
            Ok(Value::Number(NumberValue::from_f64(parse_date(&s))))
        }
        // §21.4.3.4 Date.UTC(year, month, day?, …).
        "UTC" => {
            if args.is_empty() {
                return Ok(Value::Number(NumberValue::from_f64(f64::NAN)));
            }
            let year = number_arg(args, 0);
            let month = number_or(args, 1, 0.0);
            let day = number_or(args, 2, 1.0);
            let hours = number_or(args, 3, 0.0);
            let minutes = number_or(args, 4, 0.0);
            let seconds = number_or(args, 5, 0.0);
            let ms = number_or(args, 6, 0.0);
            Ok(Value::Number(NumberValue::from_f64(make_date(
                year, month, day, hours, minutes, seconds, ms,
            ))))
        }
        _ => Err(VmError::UnknownIntrinsic {
            name: format!("Date.{name}"),
        }),
    }
}

fn number_arg(args: &[Value], idx: usize) -> f64 {
    match args.get(idx) {
        Some(Value::Number(n)) => n.as_f64(),
        Some(Value::Boolean(true)) => 1.0,
        Some(Value::Boolean(false)) | Some(Value::Null) => 0.0,
        _ => f64::NAN,
    }
}

fn number_or(args: &[Value], idx: usize, default: f64) -> f64 {
    if idx >= args.len() {
        return default;
    }
    number_arg(args, idx)
}

/// Parse an ISO 8601 / RFC 3339 date string per §21.4.1.18 — covers
/// the common `YYYY-MM-DDTHH:MM:SS[.sss][Z|±HH:MM]` shape and the
/// date-only form `YYYY-MM-DD`. Returns `NaN` for malformed input.
fn parse_date(input: &str) -> f64 {
    let s = input.trim();
    if s.is_empty() {
        return f64::NAN;
    }
    // Date portion: YYYY-MM-DD (year may be ±YYYYYY).
    let (date_part, rest) = split_at_first(s, &['T', ' ']);
    let (year, month, day) = match parse_date_components(date_part) {
        Some(v) => v,
        None => return f64::NAN,
    };
    let mut hour: f64 = 0.0;
    let mut minute: f64 = 0.0;
    let mut second: f64 = 0.0;
    let mut ms: f64 = 0.0;
    let mut offset_minutes: i64 = 0;
    if let Some(time_part) = rest {
        // Trim any trailing `Z` / `+HH:MM` / `-HH:MM` offset.
        let (time_body, offset) = split_offset(time_part);
        let parts: Vec<&str> = time_body.splitn(3, ':').collect();
        if parts.len() < 2 {
            return f64::NAN;
        }
        hour = parts[0].parse::<f64>().unwrap_or(f64::NAN);
        minute = parts[1].parse::<f64>().unwrap_or(f64::NAN);
        if let Some(sec_part) = parts.get(2) {
            // Seconds may include a `.fraction` for ms.
            let (sec_body, frac) = match sec_part.split_once('.') {
                Some((s, f)) => (s, Some(f)),
                None => (*sec_part, None),
            };
            second = sec_body.parse::<f64>().unwrap_or(f64::NAN);
            if let Some(f) = frac {
                let truncated: String = f.chars().take(3).collect();
                ms = format!("{:0<3}", truncated).parse::<f64>().unwrap_or(0.0);
            }
        }
        if let Some(offset_str) = offset {
            offset_minutes = match parse_offset(offset_str) {
                Some(m) => m,
                None => return f64::NAN,
            };
        }
    }
    let utc_ms = make_date(year, month - 1.0, day, hour, minute, second, ms);
    if !utc_ms.is_finite() {
        return f64::NAN;
    }
    utc_ms - (offset_minutes as f64) * 60_000.0
}

fn split_at_first<'a>(s: &'a str, seps: &[char]) -> (&'a str, Option<&'a str>) {
    for (i, c) in s.char_indices() {
        if seps.contains(&c) {
            return (&s[..i], Some(&s[i + c.len_utf8()..]));
        }
    }
    (s, None)
}

fn split_offset(time: &str) -> (&str, Option<&str>) {
    if let Some(stripped) = time.strip_suffix('Z') {
        return (stripped, Some("Z"));
    }
    // Find a `+` / `-` after position 0 (not the leading sign).
    for (i, c) in time.char_indices().rev() {
        if c == '+' || c == '-' {
            if i == 0 {
                continue;
            }
            return (&time[..i], Some(&time[i..]));
        }
    }
    (time, None)
}

fn parse_offset(s: &str) -> Option<i64> {
    if s == "Z" || s == "+00:00" || s == "-00:00" {
        return Some(0);
    }
    let (sign, body) = match s.chars().next()? {
        '+' => (1, &s[1..]),
        '-' => (-1, &s[1..]),
        _ => return None,
    };
    let (h, m) = match body.split_once(':') {
        Some((h, m)) => (h, m),
        None if body.len() == 4 => (&body[..2], &body[2..]),
        _ => return None,
    };
    let hours: i64 = h.parse().ok()?;
    let minutes: i64 = m.parse().ok()?;
    Some(sign * (hours * 60 + minutes))
}

fn parse_date_components(input: &str) -> Option<(f64, f64, f64)> {
    // `YYYY-MM-DD`, `YYYY-MM`, or `YYYY`.
    let parts: Vec<&str> = input.splitn(3, '-').collect();
    let year: f64 = parts.first()?.parse().ok()?;
    let month: f64 = parts.get(1).and_then(|s| s.parse().ok()).unwrap_or(1.0);
    let day: f64 = parts.get(2).and_then(|s| s.parse().ok()).unwrap_or(1.0);
    Some((year, month, day))
}
