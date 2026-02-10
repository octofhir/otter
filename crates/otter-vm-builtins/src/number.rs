//! Number built-in
//!
//! Provides Number static methods and Number.prototype methods:
//! - isFinite, isInteger, isNaN, isSafeInteger
//! - parseFloat, parseInt
//! - toFixed, toExponential, toPrecision
//! - toString, toLocaleString, valueOf

use otter_vm_core::error::VmError;
use otter_vm_core::memory;
use otter_vm_core::string::JsString;
use otter_vm_core::value::Value;
use otter_vm_runtime::{Op, op_native_with_mm as op_native};
use std::sync::Arc;

/// Get Number ops for extension registration
pub fn ops() -> Vec<Op> {
    vec![
        // Static methods
        op_native("__Number_isFinite", number_is_finite),
        op_native("__Number_isInteger", number_is_integer),
        op_native("__Number_isNaN", number_is_nan),
        op_native("__Number_isSafeInteger", number_is_safe_integer),
        op_native("__Number_parseFloat", number_parse_float),
        op_native("__Number_parseInt", number_parse_int),
        // Instance methods
        op_native("__Number_toFixed", number_to_fixed),
        op_native("__Number_toExponential", number_to_exponential),
        op_native("__Number_toPrecision", number_to_precision),
        op_native("__Number_toString", number_to_string),
        op_native("__Number_toLocaleString", number_to_locale_string),
        op_native("__Number_valueOf", number_value_of),
    ]
}

// =============================================================================
// Helper functions
// =============================================================================

fn to_number(val: &Value) -> f64 {
    if let Some(n) = val.as_number() {
        n
    } else if let Some(n) = val.as_int32() {
        n as f64
    } else if val.is_undefined() || val.is_null() {
        f64::NAN
    } else if let Some(b) = val.as_boolean() {
        if b { 1.0 } else { 0.0 }
    } else if let Some(s) = val.as_string() {
        s.as_str().parse::<f64>().unwrap_or(f64::NAN)
    } else {
        f64::NAN
    }
}

fn get_arg_number(args: &[Value], idx: usize) -> f64 {
    args.get(idx).map(to_number).unwrap_or(f64::NAN)
}

fn get_arg_int(args: &[Value], idx: usize) -> Option<i64> {
    args.get(idx).and_then(|v| {
        if let Some(n) = v.as_int32() {
            Some(n as i64)
        } else if let Some(n) = v.as_number() {
            if n.is_finite() {
                Some(n.trunc() as i64)
            } else {
                None
            }
        } else {
            None
        }
    })
}

// =============================================================================
// Static methods
// =============================================================================

/// Number.isFinite() - determines whether the passed value is a finite number
fn number_is_finite(args: &[Value], _mm: Arc<memory::MemoryManager>) -> Result<Value, VmError> {
    let val = args.first();
    match val {
        Some(v) if v.is_number() => {
            let n = v.as_number().unwrap();
            Ok(Value::boolean(n.is_finite()))
        }
        Some(v) if v.is_int32() => {
            // Integers are always finite
            Ok(Value::boolean(true))
        }
        _ => Ok(Value::boolean(false)), // Non-numbers return false
    }
}

/// Number.isInteger() - determines whether the passed value is an integer
fn number_is_integer(args: &[Value], _mm: Arc<memory::MemoryManager>) -> Result<Value, VmError> {
    let val = args.first();
    match val {
        Some(v) if v.is_int32() => Ok(Value::boolean(true)),
        Some(v) if v.is_number() => {
            let n = v.as_number().unwrap();
            Ok(Value::boolean(n.is_finite() && n.trunc() == n))
        }
        _ => Ok(Value::boolean(false)),
    }
}

/// Number.isNaN() - determines whether the passed value is NaN
fn number_is_nan(args: &[Value], _mm: Arc<memory::MemoryManager>) -> Result<Value, VmError> {
    let val = args.first();
    match val {
        Some(v) if v.is_number() => {
            let n = v.as_number().unwrap();
            Ok(Value::boolean(n.is_nan()))
        }
        Some(v) if v.is_int32() => Ok(Value::boolean(false)), // Integers are never NaN
        _ => Ok(Value::boolean(false)),                       // Non-numbers return false
    }
}

/// Number.isSafeInteger() - determines whether the provided value is a safe integer
fn number_is_safe_integer(
    args: &[Value],
    _mm: Arc<memory::MemoryManager>,
) -> Result<Value, VmError> {
    const MAX_SAFE_INTEGER: f64 = 9007199254740991.0; // 2^53 - 1
    const MIN_SAFE_INTEGER: f64 = -9007199254740991.0;

    let val = args.first();
    match val {
        Some(v) if v.is_int32() => {
            // i32 is always a safe integer
            Ok(Value::boolean(true))
        }
        Some(v) if v.is_number() => {
            let n = v.as_number().unwrap();
            let is_safe = n.is_finite()
                && n.trunc() == n
                && (MIN_SAFE_INTEGER..=MAX_SAFE_INTEGER).contains(&n);
            Ok(Value::boolean(is_safe))
        }
        _ => Ok(Value::boolean(false)),
    }
}

/// Number.parseFloat() - parses a string argument and returns a floating point number
fn number_parse_float(args: &[Value], _mm: Arc<memory::MemoryManager>) -> Result<Value, VmError> {
    let s = match args.first() {
        Some(v) if v.is_string() => v.as_string().unwrap().to_string(),
        Some(v) => {
            // ToString conversion
            if let Some(n) = v.as_number() {
                return Ok(Value::number(n));
            } else if let Some(n) = v.as_int32() {
                return Ok(Value::number(n as f64));
            } else {
                String::new()
            }
        }
        None => return Ok(Value::number(f64::NAN)),
    };

    let trimmed = s.trim_start();
    if trimmed.is_empty() {
        return Ok(Value::number(f64::NAN));
    }

    // Check for Infinity
    if let Some(rest) = trimmed.strip_prefix("Infinity")
        && (rest.is_empty() || !rest.chars().next().unwrap().is_alphanumeric())
    {
        return Ok(Value::number(f64::INFINITY));
    }
    if let Some(rest) = trimmed.strip_prefix("+Infinity")
        && (rest.is_empty() || !rest.chars().next().unwrap().is_alphanumeric())
    {
        return Ok(Value::number(f64::INFINITY));
    }
    if let Some(rest) = trimmed.strip_prefix("-Infinity")
        && (rest.is_empty() || !rest.chars().next().unwrap().is_alphanumeric())
    {
        return Ok(Value::number(f64::NEG_INFINITY));
    }

    // Find the longest valid float prefix
    let mut end = 0;
    let mut has_dot = false;
    let mut has_exp = false;
    let chars: Vec<char> = trimmed.chars().collect();

    // Handle leading sign
    if !chars.is_empty() && (chars[0] == '+' || chars[0] == '-') {
        end = 1;
    }

    while end < chars.len() {
        let c = chars[end];
        if c.is_ascii_digit() {
            end += 1;
        } else if c == '.' && !has_dot && !has_exp {
            has_dot = true;
            end += 1;
        } else if (c == 'e' || c == 'E') && !has_exp && end > 0 {
            has_exp = true;
            end += 1;
            // Handle exponent sign
            if end < chars.len() && (chars[end] == '+' || chars[end] == '-') {
                end += 1;
            }
        } else {
            break;
        }
    }

    if end == 0 || (end == 1 && (chars[0] == '+' || chars[0] == '-')) {
        return Ok(Value::number(f64::NAN));
    }

    let num_str: String = chars[..end].iter().collect();
    match num_str.parse::<f64>() {
        Ok(n) => Ok(Value::number(n)),
        Err(_) => Ok(Value::number(f64::NAN)),
    }
}

/// Number.parseInt() - parses a string argument and returns an integer
fn number_parse_int(args: &[Value], _mm: Arc<memory::MemoryManager>) -> Result<Value, VmError> {
    let s = match args.first() {
        Some(v) if v.is_string() => v.as_string().unwrap().to_string(),
        Some(v) if v.is_number() => {
            let n = v.as_number().unwrap();
            if n.is_nan() || n.is_infinite() {
                return Ok(Value::number(f64::NAN));
            }
            n.trunc().to_string()
        }
        Some(v) if v.is_int32() => v.as_int32().unwrap().to_string(),
        _ => return Ok(Value::number(f64::NAN)),
    };

    let radix = get_arg_int(args, 1).map(|r| r as i32);

    let trimmed = s.trim_start();
    if trimmed.is_empty() {
        return Ok(Value::number(f64::NAN));
    }

    let (negative, rest) = if let Some(stripped) = trimmed.strip_prefix('-') {
        (true, stripped)
    } else if let Some(stripped) = trimmed.strip_prefix('+') {
        (false, stripped)
    } else {
        (false, trimmed)
    };

    // Determine radix
    let (actual_radix, num_str) = match radix {
        Some(0) | None => {
            if rest.starts_with("0x") || rest.starts_with("0X") {
                (16, &rest[2..])
            } else if rest.starts_with("0o") || rest.starts_with("0O") {
                (8, &rest[2..])
            } else if rest.starts_with("0b") || rest.starts_with("0B") {
                (2, &rest[2..])
            } else {
                (10, rest)
            }
        }
        Some(16) if rest.starts_with("0x") || rest.starts_with("0X") => (16, &rest[2..]),
        Some(r) if (2..=36).contains(&r) => (r as u32, rest),
        _ => return Ok(Value::number(f64::NAN)),
    };

    // Find valid digits
    let valid_chars: String = num_str
        .chars()
        .take_while(|c| c.is_digit(actual_radix))
        .collect();

    if valid_chars.is_empty() {
        return Ok(Value::number(f64::NAN));
    }

    match i64::from_str_radix(&valid_chars, actual_radix) {
        Ok(n) => {
            let result = if negative { -n } else { n };
            Ok(Value::number(result as f64))
        }
        Err(_) => {
            // Try as u64 for very large numbers
            match u64::from_str_radix(&valid_chars, actual_radix) {
                Ok(n) => {
                    let result = if negative { -(n as f64) } else { n as f64 };
                    Ok(Value::number(result))
                }
                Err(_) => Ok(Value::number(f64::NAN)),
            }
        }
    }
}

// =============================================================================
// Instance methods
// =============================================================================

/// Number.prototype.toFixed() - formats a number using fixed-point notation
fn number_to_fixed(args: &[Value], _mm: Arc<memory::MemoryManager>) -> Result<Value, VmError> {
    let num = get_arg_number(args, 0);
    let digits = get_arg_int(args, 1).unwrap_or(0) as usize;

    if digits > 100 {
        return Err(VmError::type_error(
            "toFixed() digits argument must be between 0 and 100",
        ));
    }

    if num.is_nan() {
        return Ok(Value::string(JsString::intern("NaN")));
    }
    if num.is_infinite() {
        return Ok(Value::string(JsString::intern(if num.is_sign_positive() {
            "Infinity"
        } else {
            "-Infinity"
        })));
    }

    Ok(Value::string(JsString::intern(&format!(
        "{:.precision$}",
        num,
        precision = digits
    ))))
}

/// Number.prototype.toExponential() - returns a string in exponential notation
fn number_to_exponential(
    args: &[Value],
    _mm: Arc<memory::MemoryManager>,
) -> Result<Value, VmError> {
    let num = get_arg_number(args, 0);
    let fraction_digits = get_arg_int(args, 1);

    if num.is_nan() {
        return Ok(Value::string(JsString::intern("NaN")));
    }
    if num.is_infinite() {
        return Ok(Value::string(JsString::intern(if num.is_sign_positive() {
            "Infinity"
        } else {
            "-Infinity"
        })));
    }

    match fraction_digits {
        Some(digits) if !(0..=100).contains(&digits) => Err(VmError::range_error(
            "toExponential() argument must be between 0 and 100",
        )),
        Some(digits) => Ok(Value::string(JsString::intern(&format!(
            "{:.precision$e}",
            num,
            precision = digits as usize
        )))),
        None => Ok(Value::string(JsString::intern(&format!("{:e}", num)))),
    }
}

/// Number.prototype.toPrecision() - returns a string representing the number to a specified precision
fn number_to_precision(args: &[Value], _mm: Arc<memory::MemoryManager>) -> Result<Value, VmError> {
    let num = get_arg_number(args, 0);
    let precision = get_arg_int(args, 1);

    if num.is_nan() {
        return Ok(Value::string(JsString::intern("NaN")));
    }
    if num.is_infinite() {
        return Ok(Value::string(JsString::intern(if num.is_sign_positive() {
            "Infinity"
        } else {
            "-Infinity"
        })));
    }

    match precision {
        None => Ok(Value::string(JsString::intern(&num.to_string()))),
        Some(p) if !(1..=100).contains(&p) => Err(VmError::range_error(
            "toPrecision() argument must be between 1 and 100",
        )),
        Some(p) => {
            let p = p as usize;
            if num == 0.0 {
                if p == 1 {
                    return Ok(Value::string(JsString::intern("0")));
                }
                let zeros = "0".repeat(p - 1);
                return Ok(Value::string(JsString::intern(&format!("0.{}", zeros))));
            }

            let abs_num = num.abs();
            let exp = abs_num.log10().floor() as i32;

            if exp < -6 || exp >= p as i32 {
                // Use exponential notation
                let mantissa_digits = p - 1;
                let formatted = format!("{:.precision$e}", num, precision = mantissa_digits);
                Ok(Value::string(JsString::intern(&formatted)))
            } else {
                // Use fixed notation
                let decimal_places = if exp >= 0 {
                    (p as i32 - exp - 1).max(0) as usize
                } else {
                    p + (-exp - 1) as usize
                };
                let formatted = format!("{:.precision$}", num, precision = decimal_places);
                Ok(Value::string(JsString::intern(&formatted)))
            }
        }
    }
}

/// Number.prototype.toString() - returns a string representing the number
fn number_to_string(args: &[Value], _mm: Arc<memory::MemoryManager>) -> Result<Value, VmError> {
    let num = get_arg_number(args, 0);
    let radix = get_arg_int(args, 1).unwrap_or(10) as u32;

    if !(2..=36).contains(&radix) {
        return Err(VmError::type_error(
            "toString() radix must be between 2 and 36",
        ));
    }

    if num.is_nan() {
        return Ok(Value::string(JsString::intern("NaN")));
    }
    if num.is_infinite() {
        return Ok(Value::string(JsString::intern(if num.is_sign_positive() {
            "Infinity"
        } else {
            "-Infinity"
        })));
    }

    if radix == 10 {
        return Ok(Value::string(JsString::intern(&num.to_string())));
    }

    // For non-decimal radix, handle integer part
    if num.trunc() == num && num.abs() < i64::MAX as f64 {
        let int_val = num as i64;
        let result = if int_val >= 0 {
            format_radix(int_val as u64, radix)
        } else {
            format!("-{}", format_radix((-int_val) as u64, radix))
        };
        return Ok(Value::string(JsString::intern(&result)));
    }

    // For non-integer values with non-decimal radix, simplified
    let int_part = num.trunc() as i64;
    let result = if int_part >= 0 {
        format_radix(int_part as u64, radix)
    } else {
        format!("-{}", format_radix((-int_part) as u64, radix))
    };
    Ok(Value::string(JsString::intern(&result)))
}

fn format_radix(mut n: u64, radix: u32) -> String {
    if n == 0 {
        return "0".to_string();
    }

    let digits: Vec<char> = "0123456789abcdefghijklmnopqrstuvwxyz".chars().collect();
    let mut result = Vec::new();

    while n > 0 {
        result.push(digits[(n % radix as u64) as usize]);
        n /= radix as u64;
    }

    result.iter().rev().collect()
}

/// Number.prototype.toLocaleString() - returns a string with locale-sensitive representation
fn number_to_locale_string(
    args: &[Value],
    _mm: Arc<memory::MemoryManager>,
) -> Result<Value, VmError> {
    let num = get_arg_number(args, 0);
    // Simplified: just return toString result (no locale support yet)
    if num.is_nan() {
        return Ok(Value::string(JsString::intern("NaN")));
    }
    if num.is_infinite() {
        return Ok(Value::string(JsString::intern(if num.is_sign_positive() {
            "Infinity"
        } else {
            "-Infinity"
        })));
    }
    Ok(Value::string(JsString::intern(&num.to_string())))
}

/// Number.prototype.valueOf() - returns the primitive value of the number
fn number_value_of(args: &[Value], _mm: Arc<memory::MemoryManager>) -> Result<Value, VmError> {
    match args.first() {
        Some(v) if v.is_number() => Ok(v.clone()),
        Some(v) if v.is_int32() => Ok(v.clone()),
        _ => Err(VmError::type_error("valueOf requires a number")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn str_val(s: &str) -> Value {
        Value::string(JsString::intern(s))
    }

    fn assert_str_result(result: &Value, expected: &str) {
        let s = result.as_string().expect("expected string value");
        assert_eq!(s.as_str(), expected);
    }

    #[test]
    fn test_is_finite() {
        let mm = Arc::new(memory::MemoryManager::test());
        assert_eq!(
            number_is_finite(&[Value::number(42.0)], mm.clone())
                .unwrap()
                .as_boolean(),
            Some(true)
        );
        assert_eq!(
            number_is_finite(&[Value::int32(42)], mm.clone())
                .unwrap()
                .as_boolean(),
            Some(true)
        );
        assert_eq!(
            number_is_finite(&[Value::number(f64::INFINITY)], mm.clone())
                .unwrap()
                .as_boolean(),
            Some(false)
        );
        assert_eq!(
            number_is_finite(&[Value::number(f64::NEG_INFINITY)], mm.clone())
                .unwrap()
                .as_boolean(),
            Some(false)
        );
        assert_eq!(
            number_is_finite(&[Value::number(f64::NAN)], mm.clone())
                .unwrap()
                .as_boolean(),
            Some(false)
        );
        assert_eq!(
            number_is_finite(&[str_val("42")], mm).unwrap().as_boolean(),
            Some(false)
        );
    }

    #[test]
    fn test_is_integer() {
        let mm = Arc::new(memory::MemoryManager::test());
        assert_eq!(
            number_is_integer(&[Value::number(42.0)], mm.clone())
                .unwrap()
                .as_boolean(),
            Some(true)
        );
        assert_eq!(
            number_is_integer(&[Value::number(42.5)], mm.clone())
                .unwrap()
                .as_boolean(),
            Some(false)
        );
        assert_eq!(
            number_is_integer(&[Value::int32(42)], mm)
                .unwrap()
                .as_boolean(),
            Some(true)
        );
    }

    #[test]
    fn test_is_nan() {
        let mm = Arc::new(memory::MemoryManager::test());
        assert_eq!(
            number_is_nan(&[Value::number(f64::NAN)], mm.clone())
                .unwrap()
                .as_boolean(),
            Some(true)
        );
        assert_eq!(
            number_is_nan(&[Value::number(42.0)], mm.clone())
                .unwrap()
                .as_boolean(),
            Some(false)
        );
        assert_eq!(
            number_is_nan(&[Value::number(f64::INFINITY)], mm)
                .unwrap()
                .as_boolean(),
            Some(false)
        );
    }

    #[test]
    fn test_is_safe_integer() {
        let mm = Arc::new(memory::MemoryManager::test());
        assert_eq!(
            number_is_safe_integer(&[Value::number(42.0)], mm.clone())
                .unwrap()
                .as_boolean(),
            Some(true)
        );
        assert_eq!(
            number_is_safe_integer(&[Value::number(9007199254740991.0)], mm.clone())
                .unwrap()
                .as_boolean(),
            Some(true)
        );
        assert_eq!(
            number_is_safe_integer(&[Value::number(9007199254740992.0)], mm)
                .unwrap()
                .as_boolean(),
            Some(false)
        );
    }

    #[test]
    fn test_parse_float() {
        let mm = Arc::new(memory::MemoryManager::test());
        assert_eq!(
            number_parse_float(&[str_val("2.75")], mm.clone())
                .unwrap()
                .as_number(),
            Some(2.75)
        );
        assert_eq!(
            number_parse_float(&[str_val("Infinity")], mm.clone())
                .unwrap()
                .as_number(),
            Some(f64::INFINITY)
        );
        assert_eq!(
            number_parse_float(&[str_val("-Infinity")], mm)
                .unwrap()
                .as_number(),
            Some(f64::NEG_INFINITY)
        );
    }

    #[test]
    fn test_parse_int() {
        let mm = Arc::new(memory::MemoryManager::test());
        assert_eq!(
            number_parse_int(&[str_val("42")], mm.clone())
                .unwrap()
                .as_number(),
            Some(42.0)
        );
        assert_eq!(
            number_parse_int(&[str_val("0xff"), Value::int32(16)], mm)
                .unwrap()
                .as_number(),
            Some(255.0)
        );
    }

    #[test]
    fn test_to_fixed() {
        let mm = Arc::new(memory::MemoryManager::test());
        let result =
            number_to_fixed(&[Value::number(1.23456), Value::int32(2)], mm.clone()).unwrap();
        assert_str_result(&result, "1.23");

        let result =
            number_to_fixed(&[Value::number(f64::NAN), Value::int32(2)], mm.clone()).unwrap();
        assert_str_result(&result, "NaN");

        let result = number_to_fixed(&[Value::number(f64::INFINITY), Value::int32(2)], mm).unwrap();
        assert_str_result(&result, "Infinity");
    }

    #[test]
    fn test_to_string() {
        let mm = Arc::new(memory::MemoryManager::test());
        let result = number_to_string(&[Value::number(42.0)], mm.clone()).unwrap();
        assert_str_result(&result, "42");

        let result =
            number_to_string(&[Value::number(255.0), Value::int32(16)], mm.clone()).unwrap();
        assert_str_result(&result, "ff");

        let result = number_to_string(&[Value::number(f64::NAN)], mm.clone()).unwrap();
        assert_str_result(&result, "NaN");

        let result = number_to_string(&[Value::number(f64::INFINITY)], mm.clone()).unwrap();
        assert_str_result(&result, "Infinity");

        let result = number_to_string(&[Value::number(f64::NEG_INFINITY)], mm).unwrap();
        assert_str_result(&result, "-Infinity");
    }

    #[test]
    fn test_value_of() {
        let mm = Arc::new(memory::MemoryManager::test());
        assert_eq!(
            number_value_of(&[Value::number(42.0)], mm.clone())
                .unwrap()
                .as_number(),
            Some(42.0)
        );
        assert_eq!(
            number_value_of(&[Value::int32(42)], mm).unwrap().as_int32(),
            Some(42)
        );
    }
}
