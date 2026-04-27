//! Bitwise operators and exponentiation for [`NumberValue`].
//!
//! All bitwise operators in ECMAScript convert their operands
//! through `ToInt32` (signed) or `ToUint32` (the right-hand side
//! of `>>>`), perform the integer operation, then return a
//! `Number`. We model that with the [`to_int32`] / [`to_uint32`]
//! helpers and a small set of operator wrappers so the dispatcher
//! arms are one-liners.
//!
//! # Contents
//! - [`to_int32`] — spec ToInt32.
//! - [`to_uint32`] — spec ToUint32.
//! - [`bitwise_and`] / [`bitwise_or`] / [`bitwise_xor`] /
//!   [`bitwise_not`] — `& | ^ ~`.
//! - [`shl`] / [`shr_arith`] / [`shr_logical`] — `<< >> >>>`.
//! - [`pow`] — `**` / `Math.pow` core (foundation lowers through
//!   `f64::powf`).
//!
//! # Spec references
//! - ECMA-262 §7.1.6 (ToInt32), §7.1.7 (ToUint32).
//! - ECMA-262 §13.10 (Bitwise Operators), §13.11
//!   (Exponentiation).

use super::NumberValue;

/// Spec `ToInt32(value)` — round toward zero, take low 32 bits as
/// a signed integer. `NaN` and infinities map to `0`.
#[must_use]
pub fn to_int32(value: NumberValue) -> i32 {
    if let NumberValue::Smi(n) = value {
        return n;
    }
    let f = value.as_f64();
    if !f.is_finite() {
        return 0;
    }
    // Step 4 of ToInt32: truncate toward zero.
    let trunc = f.trunc();
    // Step 5: modulo 2^32.
    let modulo = trunc.rem_euclid(4_294_967_296.0);
    // Step 6: bring into [-2^31, 2^31) signed range.
    let signed = if modulo >= 2_147_483_648.0 {
        modulo - 4_294_967_296.0
    } else {
        modulo
    };
    signed as i32
}

/// Spec `ToUint32(value)` — same as `ToInt32` but interprets the
/// 32-bit result as unsigned.
#[must_use]
pub fn to_uint32(value: NumberValue) -> u32 {
    to_int32(value) as u32
}

/// `lhs & rhs` per spec.
#[must_use]
pub fn bitwise_and(lhs: NumberValue, rhs: NumberValue) -> NumberValue {
    NumberValue::Smi(to_int32(lhs) & to_int32(rhs))
}

/// `lhs | rhs` per spec.
#[must_use]
pub fn bitwise_or(lhs: NumberValue, rhs: NumberValue) -> NumberValue {
    NumberValue::Smi(to_int32(lhs) | to_int32(rhs))
}

/// `lhs ^ rhs` per spec.
#[must_use]
pub fn bitwise_xor(lhs: NumberValue, rhs: NumberValue) -> NumberValue {
    NumberValue::Smi(to_int32(lhs) ^ to_int32(rhs))
}

/// `~value` per spec.
#[must_use]
pub fn bitwise_not(value: NumberValue) -> NumberValue {
    NumberValue::Smi(!to_int32(value))
}

/// `lhs << rhs` per spec — shift count masked to its low 5 bits.
#[must_use]
pub fn shl(lhs: NumberValue, rhs: NumberValue) -> NumberValue {
    let shift = to_uint32(rhs) & 0x1f;
    NumberValue::Smi(to_int32(lhs).wrapping_shl(shift))
}

/// `lhs >> rhs` per spec — arithmetic (sign-preserving) shift.
#[must_use]
pub fn shr_arith(lhs: NumberValue, rhs: NumberValue) -> NumberValue {
    let shift = to_uint32(rhs) & 0x1f;
    NumberValue::Smi(to_int32(lhs).wrapping_shr(shift))
}

/// `lhs >>> rhs` per spec — logical (zero-fill) shift. The result
/// fits in `u32` so we promote to `Double` whenever it exceeds
/// `i32::MAX` (e.g., `(-1 >>> 0) === 4294967295`).
#[must_use]
pub fn shr_logical(lhs: NumberValue, rhs: NumberValue) -> NumberValue {
    let shift = to_uint32(rhs) & 0x1f;
    let result = to_uint32(lhs).wrapping_shr(shift);
    if result <= i32::MAX as u32 {
        NumberValue::Smi(result as i32)
    } else {
        NumberValue::Double(f64::from(result))
    }
}

/// `base ** exponent` per spec / `Math.pow(base, exponent)`.
/// Foundation drops to `f64::powf`; integer-power overflow falls
/// back to the double path (canonicalized into `Smi` when exact).
#[must_use]
pub fn pow(base: NumberValue, exponent: NumberValue) -> NumberValue {
    NumberValue::Double(base.as_f64().powf(exponent.as_f64())).canonicalize()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn to_int32_truncates_and_wraps() {
        assert_eq!(to_int32(NumberValue::Smi(7)), 7);
        assert_eq!(to_int32(NumberValue::Double(7.7)), 7);
        assert_eq!(to_int32(NumberValue::Double(-7.7)), -7);
        // 2^32 wraps to 0.
        assert_eq!(to_int32(NumberValue::Double(4_294_967_296.0)), 0);
        // 2^31 wraps to -2^31.
        assert_eq!(to_int32(NumberValue::Double(2_147_483_648.0)), i32::MIN);
        // NaN / infinity map to 0.
        assert_eq!(to_int32(NumberValue::Double(f64::NAN)), 0);
        assert_eq!(to_int32(NumberValue::Double(f64::INFINITY)), 0);
    }

    #[test]
    fn shifts_match_spec() {
        let one = NumberValue::Smi(1);
        let three = NumberValue::Smi(3);
        assert_eq!(shl(one, three), NumberValue::Smi(8));
        // Shift count modulo 32.
        assert_eq!(shl(one, NumberValue::Smi(33)), NumberValue::Smi(2));
        // -1 >>> 0 → 0xFFFFFFFF, which exceeds i32::MAX → Double.
        let r = shr_logical(NumberValue::Smi(-1), NumberValue::Smi(0));
        assert_eq!(r.as_f64(), 4_294_967_295.0);
    }

    #[test]
    fn pow_handles_int_and_float() {
        assert_eq!(
            pow(NumberValue::Smi(2), NumberValue::Smi(10)),
            NumberValue::Smi(1024)
        );
        match pow(NumberValue::Smi(2), NumberValue::Double(0.5)) {
            NumberValue::Double(d) => assert!((d - 2f64.sqrt()).abs() < 1e-12),
            other => panic!("expected Double, got {other:?}"),
        }
    }
}
