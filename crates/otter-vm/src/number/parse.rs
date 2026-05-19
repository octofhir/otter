//! Foundation `Number` parsing + predicates.
//!
//! Houses every spec algorithm that turns a string / Value into a
//! `NumberValue` or asks a yes/no question about a numeric value.
//! Both the global functions (`parseInt` / `parseFloat` / `isNaN` /
//! `isFinite`) and their `Number.<name>` static aliases reach for
//! this module — there is exactly one implementation of each.
//!
//! # Contents
//! - [`to_number_from_string`] — §7.1.4 string → Number coercion.
//! - [`to_number_value`] — §7.1.4 Value → Number coercion (used by
//!   global `isNaN` / `isFinite` after their ToNumber step).
//! - [`parse_int`] — §19.2.5 ParseInt(string, radix).
//! - [`parse_float`] — §19.2.4 ParseFloat(string).
//! - [`is_nan`] — §21.1.2.3 `Number.isNaN` (strict, no coercion).
//! - [`is_finite`] — §21.1.2.2 `Number.isFinite`.
//! - [`is_integer`] — §21.1.2.5 `Number.isInteger`.
//! - [`is_safe_integer`] — §21.1.2.6 `Number.isSafeInteger`.
//!
//! # See also
//! - <https://tc39.es/ecma262/#sec-tonumber>
//! - <https://tc39.es/ecma262/#sec-parseint-string-radix>
//! - <https://tc39.es/ecma262/#sec-parsefloat-string>

use super::NumberValue;
use crate::Value;

/// Foundation subset of `ToNumber(string)`.
///
/// Accepts:
/// - empty / whitespace-only strings → `+0`;
/// - `"Infinity"`, `"+Infinity"`, `"-Infinity"`;
/// - `"NaN"`;
/// - decimal-integer / decimal-float text (Rust `f64::from_str`).
///
/// Per §7.1.4.1 `StringToNumber`. Recognises decimal, hex (`0x` /
/// `0X`), binary (`0b` / `0B`), and octal (`0o` / `0O`) prefixed
/// integer literals as well as the decimal floating-point grammar
/// (`+/-`, integer/fraction/exponent). Unparseable strings → `NaN`.
#[must_use]
pub fn to_number_from_string(text: &str) -> NumberValue {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return NumberValue::Smi(0);
    }
    match trimmed {
        "NaN" => return NumberValue::Double(f64::NAN),
        "Infinity" | "+Infinity" => return NumberValue::Double(f64::INFINITY),
        "-Infinity" => return NumberValue::Double(f64::NEG_INFINITY),
        _ => {}
    }
    // §7.1.4.1.3 NonDecimalIntegerLiteral. Spec disallows a sign
    // prefix on hex / binary / octal forms (so `"-0x10"` → NaN).
    if let Some(rest) = trimmed
        .strip_prefix("0x")
        .or_else(|| trimmed.strip_prefix("0X"))
    {
        return parse_radix_digits(rest, 16);
    }
    if let Some(rest) = trimmed
        .strip_prefix("0b")
        .or_else(|| trimmed.strip_prefix("0B"))
    {
        return parse_radix_digits(rest, 2);
    }
    if let Some(rest) = trimmed
        .strip_prefix("0o")
        .or_else(|| trimmed.strip_prefix("0O"))
    {
        return parse_radix_digits(rest, 8);
    }
    // §7.1.4.1.3 — the StringNumericLiteral grammar accepts only the
    // exact tokens `Infinity` / `+Infinity` / `-Infinity` (matched
    // above) and the DecimalNumericLiteral form below. Rust's
    // `f64::parse` is case-insensitive and also accepts `inf` / `nan`,
    // so reject any case-folded "infinity" / "inf" / "nan" variant
    // that did not already match the spec-exact constants.
    let folded = trimmed.to_ascii_lowercase();
    let stripped = folded
        .strip_prefix('+')
        .or_else(|| folded.strip_prefix('-'))
        .unwrap_or(&folded);
    if matches!(stripped, "infinity" | "inf" | "nan") {
        return NumberValue::Double(f64::NAN);
    }
    match trimmed.parse::<f64>() {
        Ok(d) => NumberValue::Double(d).canonicalize(),
        Err(_) => NumberValue::Double(f64::NAN),
    }
}

fn parse_radix_digits(digits: &str, radix: u32) -> NumberValue {
    if digits.is_empty() {
        return NumberValue::Double(f64::NAN);
    }
    let mut value: f64 = 0.0;
    for ch in digits.chars() {
        let Some(d) = ch.to_digit(radix) else {
            return NumberValue::Double(f64::NAN);
        };
        value = value * f64::from(radix) + f64::from(d);
    }
    NumberValue::Double(value).canonicalize()
}

/// §7.1.4 ToNumber for an arbitrary `Value` — returns the raw
/// `f64` so callers can feed it into `isNaN` / `isFinite` /
/// numeric comparisons without going through the `NumberValue`
/// canonicalisation. The implementation lives here so the global
/// functions and the Number-namespace statics share it.
///
/// # See also
/// - <https://tc39.es/ecma262/#sec-tonumber>
#[must_use]
pub fn to_number_value(value: &Value) -> f64 {
    match value {
        Value::Number(n) => n.as_f64(),
        Value::Boolean(true) => 1.0,
        Value::Boolean(false) => 0.0,
        Value::Null => 0.0,
        Value::Undefined => f64::NAN,
        Value::String(s) => match to_number_from_string(&s.to_lossy_string()) {
            NumberValue::Smi(v) => v as f64,
            NumberValue::Double(d) => d,
        },
        _ => f64::NAN,
    }
}

/// §7.1.5 `ToIntegerOrInfinity(value)`. Coerces via [`to_number_value`]
/// then truncates toward zero; `NaN` collapses to `0`, infinities
/// pass through unchanged. Used by `Number.prototype.{toFixed,
/// toExponential, toPrecision}` for their fractional-digit /
/// precision arguments.
///
/// # See also
/// - <https://tc39.es/ecma262/#sec-tointegerorinfinity>
#[must_use]
pub fn to_integer_or_infinity(value: &Value) -> f64 {
    let n = to_number_value(value);
    if n.is_nan() {
        0.0
    } else if !n.is_finite() {
        n
    } else {
        n.trunc()
    }
}

/// Outcome of a strict `ToIntegerOrInfinity` coercion: distinguishes
/// the `Symbol` / `BigInt` arms (which spec-mandate a `TypeError`)
/// from the rest.
#[derive(Debug, Clone, Copy)]
pub enum IntegerCoercion {
    /// Successful coercion. Carries the truncated `f64`.
    Ok(f64),
    /// `Symbol` argument; `ToNumber(Symbol)` throws `TypeError`.
    SymbolNotConvertible,
    /// `BigInt` argument; `ToNumber(BigInt)` throws `TypeError`.
    BigIntNotConvertible,
}

/// Strict version of [`to_integer_or_infinity`] that surfaces the
/// `Symbol` / `BigInt` arms separately so the caller can throw a
/// spec-correct `TypeError` (per §7.1.4 ToNumber). Other inputs go
/// through [`to_number_value`] like the loose form.
#[must_use]
pub fn to_integer_or_infinity_strict(value: &Value) -> IntegerCoercion {
    match value {
        Value::Symbol(_) => IntegerCoercion::SymbolNotConvertible,
        Value::BigInt(_) => IntegerCoercion::BigIntNotConvertible,
        _ => IntegerCoercion::Ok(to_integer_or_infinity(value)),
    }
}

/// §19.2.5 ParseInt(string, radix). Strips whitespace + optional
/// sign, autodetects `0x` / `0X` when radix is 0 or 16, then walks
/// digits in `radix` (default 10). Returns `NaN` when no digit
/// parses.
///
/// # See also
/// - <https://tc39.es/ecma262/#sec-parseint-string-radix>
#[must_use]
pub fn parse_int(input: &str, mut radix: i32) -> NumberValue {
    let s = input.trim_start();
    let mut chars = s.chars().peekable();
    let mut negative = false;
    match chars.peek().copied() {
        Some('+') => {
            chars.next();
        }
        Some('-') => {
            chars.next();
            negative = true;
        }
        _ => {}
    }
    let rest: String = chars.clone().collect();
    let mut strip_prefix = false;
    if (radix == 0 || radix == 16) && (rest.starts_with("0x") || rest.starts_with("0X")) {
        strip_prefix = true;
        radix = 16;
    } else if radix == 0 {
        radix = 10;
    }
    if !(2..=36).contains(&radix) {
        return NumberValue::Double(f64::NAN);
    }
    let body: String = if strip_prefix {
        rest.chars().skip(2).collect()
    } else {
        rest
    };
    let mut digits = String::new();
    for c in body.chars() {
        let v = match c {
            '0'..='9' => c as u32 - '0' as u32,
            'a'..='z' => c as u32 - 'a' as u32 + 10,
            'A'..='Z' => c as u32 - 'A' as u32 + 10,
            _ => break,
        };
        if v as i32 >= radix {
            break;
        }
        digits.push(c);
    }
    if digits.is_empty() {
        return NumberValue::Double(f64::NAN);
    }
    let n = match i64::from_str_radix(&digits, radix as u32) {
        Ok(v) => v as f64,
        Err(_) => {
            // Overflow — manual reconstruction.
            let mut acc = 0.0f64;
            for c in digits.chars() {
                let v = c.to_digit(radix as u32).unwrap_or(0) as f64;
                acc = acc * radix as f64 + v;
            }
            acc
        }
    };
    let signed = if negative { -n } else { n };
    NumberValue::from_f64(signed)
}

/// §19.2.4 ParseFloat(string).
#[must_use]
pub fn parse_float(input: &str) -> NumberValue {
    let s = input.trim_start();
    if s.starts_with("Infinity") || s.starts_with("+Infinity") {
        return NumberValue::Double(f64::INFINITY);
    }
    if s.starts_with("-Infinity") {
        return NumberValue::Double(f64::NEG_INFINITY);
    }
    let mut end = 0usize;
    let mut seen_digit = false;
    let mut seen_dot = false;
    let mut seen_exp = false;
    for (i, c) in s.char_indices() {
        match c {
            '+' | '-' if i == end => {
                end = i + c.len_utf8();
            }
            '0'..='9' => {
                seen_digit = true;
                end = i + c.len_utf8();
            }
            '.' if !seen_dot && !seen_exp => {
                seen_dot = true;
                end = i + c.len_utf8();
            }
            'e' | 'E' if seen_digit && !seen_exp => {
                seen_exp = true;
                end = i + c.len_utf8();
            }
            '+' | '-' if seen_exp && s[..i].ends_with(['e', 'E']) => {
                end = i + c.len_utf8();
            }
            _ => break,
        }
    }
    if !seen_digit {
        return NumberValue::Double(f64::NAN);
    }
    match s[..end].parse::<f64>() {
        Ok(v) => NumberValue::from_f64(v),
        Err(_) => NumberValue::Double(f64::NAN),
    }
}

/// §21.1.2.3 `Number.isNaN(n)` — strict, no coercion. Pair with
/// [`to_number_value`] for the global `isNaN` semantics.
#[must_use]
pub fn is_nan(n: f64) -> bool {
    n.is_nan()
}

/// §21.1.2.2 `Number.isFinite(n)` — strict, no coercion.
#[must_use]
pub fn is_finite(n: f64) -> bool {
    n.is_finite()
}

/// §21.1.2.5 `Number.isInteger(value)`.
#[must_use]
pub fn is_integer(value: &Value) -> bool {
    match value {
        Value::Number(n) => {
            let v = n.as_f64();
            v.is_finite() && v.trunc() == v
        }
        _ => false,
    }
}

/// §21.1.2.6 `Number.isSafeInteger(value)`.
#[must_use]
pub fn is_safe_integer(value: &Value) -> bool {
    match value {
        Value::Number(n) => {
            let v = n.as_f64();
            v.is_finite() && v.trunc() == v && v.abs() <= 9_007_199_254_740_991.0
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn subset_round_trip() {
        assert_eq!(to_number_from_string("42"), NumberValue::Smi(42));
        assert_eq!(to_number_from_string("  17 "), NumberValue::Smi(17));
        assert!(to_number_from_string("NaN").is_nan());
        assert!(to_number_from_string("Infinity").is_infinite());
        assert!(to_number_from_string("-Infinity").is_infinite());
        assert_eq!(to_number_from_string(""), NumberValue::Smi(0));
        assert!(to_number_from_string("foo").is_nan());
        match to_number_from_string("1.5") {
            NumberValue::Double(d) => assert!((d - 1.5).abs() < 1e-12),
            other => panic!("expected Double, got {other:?}"),
        }
    }
}
