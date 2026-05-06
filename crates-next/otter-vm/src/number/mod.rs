//! Foundation numeric model + the surfaces built on top of it.
//!
//! Two-state representation:
//!
//! - [`NumberValue::Smi`] — small-integer immediate path
//!   (`i32`-range exact integers). Arithmetic on two `Smi`s
//!   stays in the integer path **unless** it overflows or yields
//!   a non-integer division remainder, in which case it
//!   demotes to [`NumberValue::Double`].
//! - [`NumberValue::Double`] — IEEE-754 fallback. Carries `NaN`,
//!   `±Infinity`, and `-0.0`. `-0.0` is **never** representable as
//!   `Smi`.
//!
//! # Invariants
//! - The value `0_i32` is always represented as `Smi(0)`, never
//!   `Double(+0.0)`. Negative zero is always `Double(-0.0)`.
//! - `NaN` is always `Double(f64::NAN)`.
//! - Conversions through [`NumberValue::canonicalize`] normalize
//!   `Double` payloads back to `Smi` when they hold an exact
//!   `i32`-range integer **and** are not `-0.0`.
//!
//! # Contents
//! - [`NumberValue`] — public number variant.
//! - [`NumericOrdering`], [`compare`], [`equals`], [`strict_equals`].
//! - Re-exports from submodules:
//!   - [`arith`] — `+ - * / % unary-`.
//!   - [`bitwise`] — bitwise operators + `**` + `ToInt32` / `ToUint32`.
//!   - [`parse`] — `String → Number` coercion subset.
//!   - [`prototype`] — `Number.prototype.{toString, toFixed}`.
//!
//! # See also
//! - <https://tc39.es/ecma262/#sec-ecmascript-language-types-number-type>

use std::cmp::Ordering;

use serde::{Deserialize, Serialize};

pub mod arith;
pub mod bitwise;
pub mod parse;
pub mod prototype;

pub use arith::{add, div, mul, neg, rem, sub};
pub use bitwise::{
    bitwise_and, bitwise_not, bitwise_or, bitwise_xor, pow, shl, shr_arith, shr_logical, to_int32,
    to_uint32,
};
pub use parse::{
    is_finite, is_integer, is_nan, is_safe_integer, parse_float, parse_int, to_number_from_string,
    to_number_value,
};
pub use prototype::lookup as prototype_lookup;

/// JavaScript Number value.
///
/// Storage form is `i32` when the value fits exactly and is not
/// `-0.0`, otherwise `f64`.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub enum NumberValue {
    /// Small integer fast path.
    Smi(i32),
    /// Double-precision float.
    Double(f64),
}

impl NumberValue {
    /// `Smi(n)` constructor.
    #[must_use]
    pub const fn from_i32(n: i32) -> Self {
        Self::Smi(n)
    }

    /// Construct from a `f64`. Normalizes integer-valued, non-`-0.0`
    /// doubles to `Smi` so equality semantics stay consistent.
    #[must_use]
    pub fn from_f64(n: f64) -> Self {
        Self::Double(n).canonicalize()
    }

    /// Normalize into the canonical representation:
    /// - integer-valued `Double` in `i32` range AND not `-0.0` →
    ///   `Smi`;
    /// - everything else stays `Double`.
    #[must_use]
    pub fn canonicalize(self) -> Self {
        match self {
            Self::Smi(_) => self,
            Self::Double(d) => {
                if d == 0.0 && d.is_sign_negative() {
                    // `-0.0` stays `Double` so it round-trips.
                    Self::Double(d)
                } else if d.is_nan() || d.is_infinite() {
                    Self::Double(d)
                } else if d.fract() == 0.0 && (i32::MIN as f64..=i32::MAX as f64).contains(&d) {
                    Self::Smi(d as i32)
                } else {
                    Self::Double(d)
                }
            }
        }
    }

    /// Produce the value as `f64` regardless of the underlying tag.
    #[must_use]
    pub fn as_f64(self) -> f64 {
        match self {
            Self::Smi(n) => f64::from(n),
            Self::Double(d) => d,
        }
    }

    /// Borrow as `i32` if the value is in the integer fast path.
    #[must_use]
    pub fn as_smi(self) -> Option<i32> {
        match self {
            Self::Smi(n) => Some(n),
            Self::Double(_) => None,
        }
    }

    /// `true` for `NaN`.
    #[must_use]
    pub fn is_nan(self) -> bool {
        matches!(self, Self::Double(d) if d.is_nan())
    }

    /// `true` for `±Infinity`.
    #[must_use]
    pub fn is_infinite(self) -> bool {
        matches!(self, Self::Double(d) if d.is_infinite())
    }

    /// `true` for `-0.0`.
    #[must_use]
    pub fn is_negative_zero(self) -> bool {
        matches!(self, Self::Double(d) if d == 0.0 && d.is_sign_negative())
    }

    /// JS `String(n)` rendering. Foundation subset: matches the
    /// spec for finite numbers via Rust's `Display`. `NaN`,
    /// `Infinity`, `-Infinity`, and `-0` are spelled per spec.
    #[must_use]
    pub fn to_display_string(self) -> String {
        match self {
            Self::Smi(n) => n.to_string(),
            Self::Double(d) => {
                if d.is_nan() {
                    return "NaN".to_string();
                }
                if d.is_infinite() {
                    return if d.is_sign_positive() {
                        "Infinity".to_string()
                    } else {
                        "-Infinity".to_string()
                    };
                }
                if d == 0.0 && d.is_sign_negative() {
                    return "0".to_string(); // Per ToString(-0) → "0".
                }
                if d.fract() == 0.0 && d.abs() < 1e21 {
                    format!("{}", d as i64)
                } else {
                    format!("{d}")
                }
            }
        }
    }
}

impl PartialEq for NumberValue {
    /// Spec `===` for two Number values: bitwise on `i32`, IEEE
    /// equality on `f64`. `NaN !== NaN`.
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Smi(a), Self::Smi(b)) => a == b,
            (Self::Smi(a), Self::Double(b)) | (Self::Double(b), Self::Smi(a)) => {
                f64::from(*a) == *b
            }
            (Self::Double(a), Self::Double(b)) => a == b,
        }
    }
}

/// Outcome of a numeric comparison. Matches `std::cmp::Ordering`
/// extended with an `Unordered` variant for `NaN`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NumericOrdering {
    /// `<`.
    Less,
    /// `==` (non-NaN).
    Equal,
    /// `>`.
    Greater,
    /// One operand is `NaN`; spec `<`, `<=`, `>`, `>=` all return
    /// `false` in this case.
    Unordered,
}

/// Spec `Number::lessThan` / `equal` family.
#[must_use]
pub fn compare(lhs: NumberValue, rhs: NumberValue) -> NumericOrdering {
    if lhs.is_nan() || rhs.is_nan() {
        return NumericOrdering::Unordered;
    }
    let l = lhs.as_f64();
    let r = rhs.as_f64();
    match l.partial_cmp(&r) {
        Some(Ordering::Less) => NumericOrdering::Less,
        Some(Ordering::Greater) => NumericOrdering::Greater,
        Some(Ordering::Equal) => NumericOrdering::Equal,
        None => NumericOrdering::Unordered,
    }
}

/// Spec `Number::equal` (`==` / `===` body for two numbers).
#[must_use]
pub fn equals(lhs: NumberValue, rhs: NumberValue) -> bool {
    if lhs.is_nan() || rhs.is_nan() {
        return false;
    }
    lhs == rhs
}

/// Spec `===` for two numbers.
#[must_use]
pub fn strict_equals(lhs: NumberValue, rhs: NumberValue) -> bool {
    equals(lhs, rhs)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nan_compares_unordered() {
        let nan = NumberValue::Double(f64::NAN);
        assert_eq!(
            compare(nan, NumberValue::Smi(1)),
            NumericOrdering::Unordered
        );
        assert!(!equals(nan, nan));
    }

    #[test]
    fn display_strings() {
        assert_eq!(NumberValue::Smi(42).to_display_string(), "42");
        assert_eq!(NumberValue::Smi(-7).to_display_string(), "-7");
        assert_eq!(NumberValue::Double(f64::NAN).to_display_string(), "NaN");
        assert_eq!(
            NumberValue::Double(f64::INFINITY).to_display_string(),
            "Infinity"
        );
        assert_eq!(NumberValue::Double(-0.0).to_display_string(), "0");
        assert_eq!(NumberValue::Double(1.5).to_display_string(), "1.5");
    }
}
