//! Arithmetic primitives for [`NumberValue`].
//!
//! These four operators stay on the integer fast path whenever
//! both operands are `Smi` and the result is representable in
//! `i32`. The `Smi → Double` demotion is automatic via
//! [`NumberValue::canonicalize`].
//!
//! # Contents
//! - [`add`], [`sub`], [`mul`], [`div`], [`rem`], [`neg`].
//!
//! # See also
//! - [`super`] — module-level docs and the `NumberValue` shape.

use super::NumberValue;

/// `lhs + rhs`. Stays in the `Smi` path when both operands are
/// `Smi` and the result does not overflow.
#[must_use]
pub fn add(lhs: NumberValue, rhs: NumberValue) -> NumberValue {
    if let (NumberValue::Smi(a), NumberValue::Smi(b)) = (lhs, rhs)
        && let Some(r) = a.checked_add(b)
    {
        return NumberValue::Smi(r);
    }
    NumberValue::Double(lhs.as_f64() + rhs.as_f64()).canonicalize()
}

/// `lhs - rhs`.
#[must_use]
pub fn sub(lhs: NumberValue, rhs: NumberValue) -> NumberValue {
    if let (NumberValue::Smi(a), NumberValue::Smi(b)) = (lhs, rhs)
        && let Some(r) = a.checked_sub(b)
    {
        return NumberValue::Smi(r);
    }
    NumberValue::Double(lhs.as_f64() - rhs.as_f64()).canonicalize()
}

/// `lhs * rhs`.
#[must_use]
pub fn mul(lhs: NumberValue, rhs: NumberValue) -> NumberValue {
    if let (NumberValue::Smi(a), NumberValue::Smi(b)) = (lhs, rhs)
        && let Some(r) = a.checked_mul(b)
    {
        return NumberValue::Smi(r);
    }
    NumberValue::Double(lhs.as_f64() * rhs.as_f64()).canonicalize()
}

/// `lhs / rhs`. Always returns `Double` because integer division
/// rarely yields an exact integer; canonicalization promotes
/// integer-valued results back to `Smi`.
#[must_use]
pub fn div(lhs: NumberValue, rhs: NumberValue) -> NumberValue {
    NumberValue::Double(lhs.as_f64() / rhs.as_f64()).canonicalize()
}

/// `lhs % rhs` per IEEE-754 remainder semantics.
#[must_use]
pub fn rem(lhs: NumberValue, rhs: NumberValue) -> NumberValue {
    NumberValue::Double(lhs.as_f64() % rhs.as_f64()).canonicalize()
}

/// Unary `-`.
#[must_use]
pub fn neg(value: NumberValue) -> NumberValue {
    match value {
        NumberValue::Smi(0) => NumberValue::Double(-0.0),
        NumberValue::Smi(n) => match n.checked_neg() {
            Some(r) => NumberValue::Smi(r),
            None => NumberValue::Double(-f64::from(n)).canonicalize(),
        },
        NumberValue::Double(d) => NumberValue::Double(-d).canonicalize(),
    }
}

#[cfg(test)]
mod tests {
    use super::super::strict_equals;
    use super::*;

    #[test]
    fn smi_add_stays_smi() {
        assert_eq!(
            add(NumberValue::Smi(1), NumberValue::Smi(2)),
            NumberValue::Smi(3)
        );
    }

    #[test]
    fn smi_overflow_promotes_to_double() {
        let a = NumberValue::Smi(i32::MAX);
        let b = NumberValue::Smi(1);
        let r = add(a, b);
        match r {
            NumberValue::Double(d) => assert!((d - (i32::MAX as f64 + 1.0)).abs() < 1e-9),
            other => panic!("expected Double, got {other:?}"),
        }
    }

    #[test]
    fn negative_zero_round_trip() {
        let neg_zero = neg(NumberValue::Smi(0));
        assert!(neg_zero.is_negative_zero());
        assert!(strict_equals(neg_zero, NumberValue::Smi(0)));
    }

    #[test]
    fn division_by_zero_produces_infinity() {
        let r = div(NumberValue::Smi(1), NumberValue::Smi(0));
        assert!(r.is_infinite());
        let r = div(NumberValue::Smi(-1), NumberValue::Smi(0));
        assert!(r.is_infinite());
        let r = div(NumberValue::Smi(0), NumberValue::Smi(0));
        assert!(r.is_nan());
    }

    #[test]
    fn fractional_division_returns_double() {
        let r = div(NumberValue::Smi(1), NumberValue::Smi(2));
        match r {
            NumberValue::Double(d) => assert!((d - 0.5).abs() < 1e-12),
            other => panic!("expected Double, got {other:?}"),
        }
    }
}
