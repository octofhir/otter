//! Foundation `String → Number` coercion subset.
//!
//! # Contents
//! - [`to_number_from_string`].
//!
//! # See also
//! - ECMA-262 §7.1.4 ("ToNumber") for the full algorithm. The
//!   foundation slice covers the decimal subset; hex / binary /
//!   octal numeric literals arrive with a later string-method
//!   slice.

use super::NumberValue;

/// Foundation subset of `ToNumber(string)`.
///
/// Accepts:
/// - empty / whitespace-only strings → `+0`;
/// - `"Infinity"`, `"+Infinity"`, `"-Infinity"`;
/// - `"NaN"`;
/// - decimal-integer / decimal-float text (Rust `f64::from_str`).
///
/// Any other shape → `NaN`. Hex / binary / octal `StringNumeric`
/// literals are deferred to a later slice.
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
    match trimmed.parse::<f64>() {
        Ok(d) => NumberValue::Double(d).canonicalize(),
        Err(_) => NumberValue::Double(f64::NAN),
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
