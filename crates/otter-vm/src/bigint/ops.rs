//! BigInt arithmetic, comparison, and bitwise primitives.
//!
//! These mirror the [`crate::number`] arithmetic surface but operate
//! on raw [`num_bigint::BigInt`] payloads borrowed from a
//! [`crate::bigint::BigIntValue`] body via
//! [`crate::bigint::BigIntValue::with_inner`]. Keeping the ops layer
//! heap-free means the same helpers compose with the bytecode
//! dispatcher (which owns `&GcHeap`) and ad-hoc paths in
//! `arithmetic_dispatch.rs` that need to fold computed `BigInt`
//! results back into fresh `BigIntValue` handles at the call site.
//!
//! # Contents
//! - Arithmetic: [`add`], [`sub`], [`mul`], [`div`], [`rem`],
//!   [`neg`], [`pow`].
//! - Bitwise: [`bitwise_and`], [`bitwise_or`], [`bitwise_xor`],
//!   [`bitwise_not`], [`shl`], [`shr`].
//! - Comparison: [`compare`], [`equals`].
//! - Mixed: [`compare_to_f64`], [`equals_f64`].
//! - [`OpError`] — failure modes the dispatcher converts to
//!   `VmError`.
//!
//! # Spec references
//! - ECMA-262 §6.1.6.2 — BigInt operations.
//! - `BigInt::divide`, `BigInt::remainder` raise `RangeError` on
//!   division by zero. The foundation surfaces those as
//!   [`OpError::DivisionByZero`].

use std::cmp::Ordering;

use num_bigint::BigInt;
use num_traits::{Signed, ToPrimitive, Zero};

/// Failure modes for BigInt operations.
#[derive(Debug, Clone, thiserror::Error)]
#[non_exhaustive]
pub enum OpError {
    /// `BigInt` divide / remainder by zero. Spec: `RangeError`.
    #[error("BigInt division by zero")]
    DivisionByZero,
    /// Negative exponent on `**`. Spec: `RangeError`.
    #[error("BigInt exponent must be non-negative")]
    NegativeExponent,
    /// Shift count is too large to fit in `u32` (the foundation
    /// caps shifts to `u32::MAX` bits, matching V8 / SpiderMonkey
    /// behaviour for spec correctness).
    #[error("BigInt shift count out of range")]
    ShiftOutOfRange,
}

/// `lhs + rhs`.
#[must_use]
pub fn add(lhs: &BigInt, rhs: &BigInt) -> BigInt {
    lhs + rhs
}

/// `lhs - rhs`.
#[must_use]
pub fn sub(lhs: &BigInt, rhs: &BigInt) -> BigInt {
    lhs - rhs
}

/// `lhs * rhs`.
#[must_use]
pub fn mul(lhs: &BigInt, rhs: &BigInt) -> BigInt {
    lhs * rhs
}

/// `lhs / rhs` — truncated toward zero (matches BigInt spec).
pub fn div(lhs: &BigInt, rhs: &BigInt) -> Result<BigInt, OpError> {
    if rhs.is_zero() {
        return Err(OpError::DivisionByZero);
    }
    Ok(lhs / rhs)
}

/// `lhs % rhs` — sign follows the dividend.
pub fn rem(lhs: &BigInt, rhs: &BigInt) -> Result<BigInt, OpError> {
    if rhs.is_zero() {
        return Err(OpError::DivisionByZero);
    }
    Ok(lhs % rhs)
}

/// Unary `-`.
#[must_use]
pub fn neg(value: &BigInt) -> BigInt {
    -value
}

/// `base ** exponent` — exponent must fit `u32` and be
/// non-negative per spec.
pub fn pow(base: &BigInt, exponent: &BigInt) -> Result<BigInt, OpError> {
    if exponent.is_negative() {
        return Err(OpError::NegativeExponent);
    }
    let exp_u32 = exponent.to_u32().ok_or(OpError::ShiftOutOfRange)?;
    Ok(base.pow(exp_u32))
}

/// `lhs & rhs`.
#[must_use]
pub fn bitwise_and(lhs: &BigInt, rhs: &BigInt) -> BigInt {
    lhs & rhs
}

/// `lhs | rhs`.
#[must_use]
pub fn bitwise_or(lhs: &BigInt, rhs: &BigInt) -> BigInt {
    lhs | rhs
}

/// `lhs ^ rhs`.
#[must_use]
pub fn bitwise_xor(lhs: &BigInt, rhs: &BigInt) -> BigInt {
    lhs ^ rhs
}

/// `~value`.
#[must_use]
pub fn bitwise_not(value: &BigInt) -> BigInt {
    !value
}

/// `lhs << rhs`. Negative shift counts shift right per spec.
pub fn shl(lhs: &BigInt, rhs: &BigInt) -> Result<BigInt, OpError> {
    if rhs.is_negative() {
        let abs = -rhs;
        let n = abs.to_u32().ok_or(OpError::ShiftOutOfRange)?;
        return Ok(lhs >> n);
    }
    let n = rhs.to_u32().ok_or(OpError::ShiftOutOfRange)?;
    Ok(lhs << n)
}

/// `lhs >> rhs` — arithmetic (sign-preserving) shift. There is
/// no `>>>` for BigInt (spec rejects it as a `TypeError`); the
/// dispatcher handles that at the call site.
pub fn shr(lhs: &BigInt, rhs: &BigInt) -> Result<BigInt, OpError> {
    if rhs.is_negative() {
        let abs = -rhs;
        let n = abs.to_u32().ok_or(OpError::ShiftOutOfRange)?;
        return Ok(lhs << n);
    }
    let n = rhs.to_u32().ok_or(OpError::ShiftOutOfRange)?;
    Ok(lhs >> n)
}

/// Three-way comparison.
#[must_use]
pub fn compare(lhs: &BigInt, rhs: &BigInt) -> Ordering {
    lhs.cmp(rhs)
}

/// `lhs === rhs` for two BigInts (just numeric equality).
#[must_use]
pub fn equals(lhs: &BigInt, rhs: &BigInt) -> bool {
    lhs == rhs
}

/// Compare a BigInt with a Number per spec §6.1.6.1.13's mixed
/// "less-than" rule. `f64::NAN` returns `None` (spec treats it as
/// undefined / unordered).
#[must_use]
pub fn compare_to_f64(lhs: &BigInt, rhs: f64) -> Option<Ordering> {
    if rhs.is_nan() {
        return None;
    }
    if rhs.is_infinite() {
        return Some(if rhs > 0.0 {
            Ordering::Less
        } else {
            Ordering::Greater
        });
    }
    let truncated = rhs.trunc();
    // Exact f64 → BigInt via bit decomposition — an `as i128` cast
    // saturates near 1.7e38 and corrupts comparisons against large
    // doubles (e.g. Number.MAX_VALUE ≈ 1.8e308).
    let rhs_int = num_traits::FromPrimitive::from_f64(truncated)?;
    match lhs.cmp(&rhs_int) {
        Ordering::Equal => {
            // BigInt vs non-integer Number: tie-break on the
            // fractional part of the original f64.
            if rhs > truncated {
                Some(Ordering::Less)
            } else if rhs < truncated {
                Some(Ordering::Greater)
            } else {
                Some(Ordering::Equal)
            }
        }
        other => Some(other),
    }
}

/// Equality with a Number per spec §7.2.13: integer-valued
/// numerics compare equal to BigInts of the same magnitude;
/// non-integer or NaN Numbers never equal a BigInt.
#[must_use]
pub fn equals_f64(lhs: &BigInt, rhs: f64) -> bool {
    matches!(compare_to_f64(lhs, rhs), Some(Ordering::Equal))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn b(n: i64) -> BigInt {
        BigInt::from(n)
    }

    #[test]
    fn add_beyond_max_safe_integer() {
        let a: BigInt = "9007199254740993".parse().unwrap();
        let r = add(&a, &b(1));
        assert_eq!(r.to_string(), "9007199254740994");
    }

    #[test]
    fn div_by_zero_errors() {
        assert!(matches!(div(&b(1), &b(0)), Err(OpError::DivisionByZero)));
    }

    #[test]
    fn pow_rejects_negative_exponent() {
        assert!(matches!(pow(&b(2), &b(-1)), Err(OpError::NegativeExponent)));
    }

    #[test]
    fn shifts_handle_negative_counts() {
        let four = b(4);
        // 4 << -1 == 4 >> 1 == 2
        let r = shl(&four, &b(-1)).unwrap();
        assert_eq!(r, b(2));
        // 4 >> -1 == 4 << 1 == 8
        let r = shr(&four, &b(-1)).unwrap();
        assert_eq!(r, b(8));
    }

    #[test]
    fn compare_to_f64_respects_fractional_tie_breaker() {
        assert_eq!(compare_to_f64(&b(2), 2.5), Some(Ordering::Less));
        assert_eq!(compare_to_f64(&b(2), 1.5), Some(Ordering::Greater));
        assert_eq!(compare_to_f64(&b(2), 2.0), Some(Ordering::Equal));
        assert_eq!(compare_to_f64(&b(2), f64::NAN), None);
    }

    #[test]
    fn equals_f64_matches_integer_only() {
        assert!(equals_f64(&b(7), 7.0));
        assert!(!equals_f64(&b(7), 7.5));
        assert!(!equals_f64(&b(7), f64::NAN));
    }
}
