//! Heap-stored BigInt payload with `i64` fast path.
//!
//! §6.1.6.2 The BigInt Type
//! <https://tc39.es/ecma262/#sec-ecmascript-language-types-bigint-type>
//!
//! Stores small magnitudes inline in an `i64` to avoid the per-op
//! `Vec<u32>` allocation that `num_bigint::BigInt` requires for every
//! arithmetic result. Promotes to a heap `BigInt` on overflow or on any
//! input that does not fit in `i64`. The split is invisible to callers
//! through [`BigIntPayload::as_bigint`], which returns a `Cow` so the
//! inline path constructs a `BigInt` only when the consumer demands one.

use num_bigint::BigInt;
use num_traits::{Signed, ToPrimitive, Zero};
use std::borrow::Cow;
use std::cmp::Ordering;

/// In-memory representation of a JavaScript BigInt heap value.
///
/// `Inline` carries the magnitude when it fits in `i64`; `Heap`
/// boxes the `BigInt` so the enum stays compact (16 B on 64-bit
/// hosts). Sign is encoded in the `i64` directly for `Inline`.
#[derive(Debug, Clone)]
pub enum BigIntPayload {
    Inline(i64),
    Heap(Box<BigInt>),
}

/// Returned by [`BigIntPayload::from_decimal_str`] when the input is not a
/// valid integer literal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ParseBigIntPayloadError;

impl std::fmt::Display for ParseBigIntPayloadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("invalid BigInt literal")
    }
}

impl std::error::Error for ParseBigIntPayloadError {}

impl BigIntPayload {
    /// Returns the canonical zero value.
    #[inline]
    pub const fn zero() -> Self {
        Self::Inline(0)
    }

    /// Constructs a BigInt from a signed 64-bit integer (always inline).
    #[inline]
    pub const fn from_i64(n: i64) -> Self {
        Self::Inline(n)
    }

    /// Constructs a BigInt from a signed 32-bit integer (always inline).
    #[inline]
    pub const fn from_i32(n: i32) -> Self {
        Self::Inline(n as i64)
    }

    /// Constructs a BigInt from an unsigned 64-bit integer.
    /// Inlines values up to `i64::MAX`; otherwise promotes.
    #[inline]
    pub fn from_u64(n: u64) -> Self {
        if n <= i64::MAX as u64 {
            Self::Inline(n as i64)
        } else {
            Self::Heap(Box::new(BigInt::from(n)))
        }
    }

    /// Constructs a BigInt from a `bool` (`true` → `1n`, `false` → `0n`).
    #[inline]
    pub const fn from_bool(b: bool) -> Self {
        Self::Inline(if b { 1 } else { 0 })
    }

    /// Adopts an existing `num_bigint::BigInt`, demoting to `Inline` when
    /// the value fits in `i64`.
    pub fn from_bigint(value: BigInt) -> Self {
        if let Some(n) = value.to_i64() {
            Self::Inline(n)
        } else {
            Self::Heap(Box::new(value))
        }
    }

    /// Parses a decimal string into a BigInt. Empty strings or strings
    /// containing non-digit characters are rejected with
    /// [`ParseBigIntPayloadError`] so callers can map to the spec-mandated
    /// `SyntaxError`.
    pub fn from_decimal_str(s: &str) -> Result<Self, ParseBigIntPayloadError> {
        let trimmed = s.trim();
        if let Ok(n) = trimmed.parse::<i64>() {
            return Ok(Self::Inline(n));
        }
        match trimmed.parse::<BigInt>() {
            Ok(b) => Ok(Self::from_bigint(b)),
            Err(_) => Err(ParseBigIntPayloadError),
        }
    }

    /// Returns the value as an owned-or-borrowed [`BigInt`]. The inline
    /// path constructs a fresh `BigInt` (cheap — single small `Vec`),
    /// the heap path borrows from the box.
    #[inline]
    pub fn as_bigint(&self) -> Cow<'_, BigInt> {
        match self {
            Self::Inline(n) => Cow::Owned(BigInt::from(*n)),
            Self::Heap(b) => Cow::Borrowed(b.as_ref()),
        }
    }

    /// Returns the magnitude as an owned [`BigInt`]. Always allocates for
    /// `Inline`; clones for `Heap`.
    #[inline]
    pub fn into_bigint(self) -> BigInt {
        match self {
            Self::Inline(n) => BigInt::from(n),
            Self::Heap(b) => *b,
        }
    }

    /// Tries to extract an `i64`. Returns `None` if the magnitude is too
    /// large to fit.
    #[inline]
    pub fn try_to_i64(&self) -> Option<i64> {
        match self {
            Self::Inline(n) => Some(*n),
            Self::Heap(b) => b.to_i64(),
        }
    }

    /// Tries to extract a `u64`. Returns `None` for negative values or
    /// magnitudes that do not fit.
    #[inline]
    pub fn try_to_u64(&self) -> Option<u64> {
        match self {
            Self::Inline(n) if *n >= 0 => Some(*n as u64),
            Self::Inline(_) => None,
            Self::Heap(b) => b.to_u64(),
        }
    }

    /// Tries to extract an `f64`. Inline values are exact; heap values
    /// fall back to `BigInt::to_f64`.
    #[inline]
    pub fn to_f64(&self) -> f64 {
        match self {
            Self::Inline(n) => *n as f64,
            Self::Heap(b) => b.to_f64().unwrap_or(f64::INFINITY),
        }
    }

    /// `true` iff this value is zero.
    #[inline]
    pub fn is_zero(&self) -> bool {
        match self {
            Self::Inline(n) => *n == 0,
            Self::Heap(b) => b.is_zero(),
        }
    }

    /// `true` iff this value is strictly negative.
    #[inline]
    pub fn is_negative(&self) -> bool {
        match self {
            Self::Inline(n) => *n < 0,
            Self::Heap(b) => b.is_negative(),
        }
    }

    /// Returns the canonical decimal string per §6.1.6.2.18
    /// `BigInt::toString`. The leading sign for zero is omitted.
    pub fn to_decimal_string(&self) -> String {
        match self {
            Self::Inline(n) => n.to_string(),
            Self::Heap(b) => b.to_string(),
        }
    }

    /// Returns the magnitude in the requested radix per
    /// §21.2.3.2 `BigInt.prototype.toString(radix)`.
    pub fn to_radix_string(&self, radix: u32) -> String {
        match self {
            Self::Inline(n) => {
                // i64 → radix string. Use BigInt::to_str_radix to get
                // identical output for negative values (sign prefix).
                BigInt::from(*n).to_str_radix(radix)
            }
            Self::Heap(b) => b.to_str_radix(radix),
        }
    }

    /// Total ordering: §6.1.6.2.13 BigInt::lessThan.
    fn compare(&self, other: &Self) -> Ordering {
        match (self, other) {
            (Self::Inline(a), Self::Inline(b)) => a.cmp(b),
            (Self::Heap(a), Self::Heap(b)) => a.as_ref().cmp(b.as_ref()),
            (a, b) => a.as_bigint().as_ref().cmp(b.as_bigint().as_ref()),
        }
    }

    // ── Arithmetic operations (§6.1.6.2.7..§6.1.6.2.12) ────────────────

    /// §6.1.6.2.7 BigInt::add(x, y).
    pub fn add(&self, other: &Self) -> Self {
        if let (Self::Inline(a), Self::Inline(b)) = (self, other)
            && let Some(sum) = a.checked_add(*b)
        {
            return Self::Inline(sum);
        }
        Self::from_bigint(self.as_bigint().as_ref() + other.as_bigint().as_ref())
    }

    /// §6.1.6.2.8 BigInt::subtract(x, y).
    pub fn sub(&self, other: &Self) -> Self {
        if let (Self::Inline(a), Self::Inline(b)) = (self, other)
            && let Some(diff) = a.checked_sub(*b)
        {
            return Self::Inline(diff);
        }
        Self::from_bigint(self.as_bigint().as_ref() - other.as_bigint().as_ref())
    }

    /// §6.1.6.2.9 BigInt::multiply(x, y).
    pub fn mul(&self, other: &Self) -> Self {
        if let (Self::Inline(a), Self::Inline(b)) = (self, other)
            && let Some(prod) = a.checked_mul(*b)
        {
            return Self::Inline(prod);
        }
        Self::from_bigint(self.as_bigint().as_ref() * other.as_bigint().as_ref())
    }

    /// §6.1.6.2.10 BigInt::divide(x, y) — truncating toward zero.
    /// Caller must guarantee `other` is non-zero; otherwise the call is UB
    /// for the inline path (rust panics on i64 division by zero).
    pub fn div_trunc(&self, other: &Self) -> Self {
        if let (Self::Inline(a), Self::Inline(b)) = (self, other) {
            // i64::MIN / -1 overflows; fall through to BigInt in that case.
            if let Some(q) = a.checked_div(*b) {
                return Self::Inline(q);
            }
        }
        Self::from_bigint(self.as_bigint().as_ref() / other.as_bigint().as_ref())
    }

    /// §6.1.6.2.11 BigInt::remainder(x, y) — truncating toward zero (sign
    /// follows dividend, matching ES `%` and `BigInt::remainder`).
    pub fn rem_trunc(&self, other: &Self) -> Self {
        if let (Self::Inline(a), Self::Inline(b)) = (self, other)
            && let Some(r) = a.checked_rem(*b)
        {
            return Self::Inline(r);
        }
        Self::from_bigint(self.as_bigint().as_ref() % other.as_bigint().as_ref())
    }

    /// §6.1.6.2.12 BigInt::exponentiate(base, exponent).
    /// Returns `None` if exponent is negative (caller throws RangeError).
    pub fn pow(&self, exp: &Self) -> Option<Self> {
        if exp.is_negative() {
            return None;
        }
        let exp_u32 = exp.try_to_u64()?.try_into().ok()?;
        match self {
            Self::Inline(n) => {
                if let Some(p) = n.checked_pow(exp_u32) {
                    return Some(Self::Inline(p));
                }
                Some(Self::from_bigint(BigInt::from(*n).pow(exp_u32)))
            }
            Self::Heap(b) => Some(Self::from_bigint(b.as_ref().pow(exp_u32))),
        }
    }

    /// Unary negation §6.1.6.2.4.
    pub fn neg(&self) -> Self {
        match self {
            Self::Inline(n) => match n.checked_neg() {
                Some(m) => Self::Inline(m),
                None => Self::Heap(Box::new(-BigInt::from(*n))),
            },
            Self::Heap(b) => Self::from_bigint(-b.as_ref().clone()),
        }
    }

    /// Bitwise NOT §6.1.6.2.2 — returns `-(x + 1)`.
    pub fn bitnot(&self) -> Self {
        match self {
            Self::Inline(n) => Self::Inline(!n),
            Self::Heap(b) => Self::from_bigint(!b.as_ref().clone()),
        }
    }

    /// §6.1.6.2.20 BigInt::bitwiseAND.
    pub fn bitand(&self, other: &Self) -> Self {
        if let (Self::Inline(a), Self::Inline(b)) = (self, other) {
            return Self::Inline(a & b);
        }
        Self::from_bigint(self.as_bigint().as_ref() & other.as_bigint().as_ref())
    }

    /// §6.1.6.2.21 BigInt::bitwiseOR.
    pub fn bitor(&self, other: &Self) -> Self {
        if let (Self::Inline(a), Self::Inline(b)) = (self, other) {
            return Self::Inline(a | b);
        }
        Self::from_bigint(self.as_bigint().as_ref() | other.as_bigint().as_ref())
    }

    /// §6.1.6.2.22 BigInt::bitwiseXOR.
    pub fn bitxor(&self, other: &Self) -> Self {
        if let (Self::Inline(a), Self::Inline(b)) = (self, other) {
            return Self::Inline(a ^ b);
        }
        Self::from_bigint(self.as_bigint().as_ref() ^ other.as_bigint().as_ref())
    }

    /// §6.1.6.2.5 BigInt::leftShift(x, y).
    pub fn shl(&self, other: &Self) -> Self {
        let shift_amount = other.try_to_i64();
        match shift_amount {
            Some(s) if s >= 0 => {
                let n = s as u32;
                if let Self::Inline(a) = self
                    && n < 63
                    && let Some(p) = a.checked_shl(n)
                {
                    return Self::Inline(p);
                }
                Self::from_bigint(self.as_bigint().as_ref() << (s as u128))
            }
            Some(s) => {
                // negative shift = arithmetic right shift
                let amount = (-s) as u128;
                Self::from_bigint(self.as_bigint().as_ref() >> amount)
            }
            None => {
                // Shift by huge magnitude. Negative → 0 (or -1 if neg);
                // positive → spec says throw RangeError, but at this layer
                // we just return a huge BigInt. The runtime layer enforces.
                if other.is_negative() {
                    if self.is_negative() {
                        Self::Inline(-1)
                    } else {
                        Self::zero()
                    }
                } else {
                    // Caller is expected to throw RangeError before this
                    // point; defensive fallback returns the original value.
                    self.clone()
                }
            }
        }
    }

    /// §6.1.6.2.5 BigInt::signedRightShift = leftShift(x, -y).
    pub fn shr(&self, other: &Self) -> Self {
        self.shl(&other.neg())
    }
}

impl PartialEq for BigIntPayload {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Inline(a), Self::Inline(b)) => a == b,
            (Self::Heap(a), Self::Heap(b)) => a == b,
            // Mixed: compare as BigInt (canonical normalisation guarantees
            // a Heap value cannot equal an i64 — `from_bigint` demotes to
            // Inline whenever the magnitude fits.)
            _ => false,
        }
    }
}

impl Eq for BigIntPayload {}

impl PartialOrd for BigIntPayload {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for BigIntPayload {
    fn cmp(&self, other: &Self) -> Ordering {
        self.compare(other)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inline_roundtrip() {
        let v = BigIntPayload::from_i64(42);
        assert_eq!(v.to_decimal_string(), "42");
        assert_eq!(v.try_to_i64(), Some(42));
    }

    #[test]
    fn from_decimal_inlines_small() {
        let v = BigIntPayload::from_decimal_str("123").unwrap();
        assert!(matches!(v, BigIntPayload::Inline(123)));
    }

    #[test]
    fn from_decimal_promotes_large() {
        let v = BigIntPayload::from_decimal_str("12345678901234567890").unwrap();
        assert!(matches!(v, BigIntPayload::Heap(_)));
        assert_eq!(v.to_decimal_string(), "12345678901234567890");
    }

    #[test]
    fn add_inline_fast_path() {
        let a = BigIntPayload::from_i64(2);
        let b = BigIntPayload::from_i64(3);
        let c = a.add(&b);
        assert!(matches!(c, BigIntPayload::Inline(5)));
    }

    #[test]
    fn add_overflow_promotes_to_heap() {
        let a = BigIntPayload::from_i64(i64::MAX);
        let b = BigIntPayload::from_i64(1);
        let c = a.add(&b);
        assert!(matches!(c, BigIntPayload::Heap(_)));
        assert_eq!(c.to_decimal_string(), "9223372036854775808");
    }

    #[test]
    fn add_demotes_back_to_inline() {
        // Heap + (-Heap) → 0 must collapse to Inline(0).
        let a = BigIntPayload::from_decimal_str("9223372036854775808").unwrap();
        let b = BigIntPayload::from_decimal_str("-9223372036854775808").unwrap();
        let c = a.add(&b);
        assert!(matches!(c, BigIntPayload::Inline(0)));
    }

    #[test]
    fn equality_across_representations_is_canonical() {
        // Constructor invariant: `from_bigint` demotes when magnitude fits,
        // so two equal values have the same variant — distinct variants
        // means distinct values.
        let a = BigIntPayload::from_decimal_str("1").unwrap();
        let b = BigIntPayload::from_i64(1);
        assert_eq!(a, b);
    }

    #[test]
    fn pow_inline_fast_path() {
        let a = BigIntPayload::from_i64(2);
        let b = BigIntPayload::from_i64(10);
        assert_eq!(a.pow(&b).unwrap().try_to_i64(), Some(1024));
    }

    #[test]
    fn pow_overflow_promotes() {
        let a = BigIntPayload::from_i64(2);
        let b = BigIntPayload::from_i64(100);
        let r = a.pow(&b).unwrap();
        assert!(matches!(r, BigIntPayload::Heap(_)));
        assert_eq!(
            r.to_decimal_string(),
            "1267650600228229401496703205376"
        );
    }

    #[test]
    fn pow_negative_exponent_returns_none() {
        let a = BigIntPayload::from_i64(2);
        let b = BigIntPayload::from_i64(-1);
        assert!(a.pow(&b).is_none());
    }

    #[test]
    fn neg_handles_i64_min() {
        let a = BigIntPayload::from_i64(i64::MIN);
        let n = a.neg();
        // i64::MIN cannot be negated as i64; must promote to Heap.
        assert!(matches!(n, BigIntPayload::Heap(_)));
        assert_eq!(n.to_decimal_string(), "9223372036854775808");
    }

    #[test]
    fn bitnot_inline() {
        let a = BigIntPayload::from_i64(5);
        assert_eq!(a.bitnot().try_to_i64(), Some(-6));
    }

    #[test]
    fn shl_inline_fast_path() {
        let a = BigIntPayload::from_i64(1);
        let b = BigIntPayload::from_i64(10);
        assert_eq!(a.shl(&b).try_to_i64(), Some(1024));
    }

    #[test]
    fn shl_promotes_on_overflow() {
        let a = BigIntPayload::from_i64(1);
        let b = BigIntPayload::from_i64(70);
        let r = a.shl(&b);
        assert!(matches!(r, BigIntPayload::Heap(_)));
    }

    #[test]
    fn radix_string_negative() {
        let a = BigIntPayload::from_i64(-255);
        assert_eq!(a.to_radix_string(16), "-ff");
    }

    #[test]
    fn ordering_across_variants() {
        let small = BigIntPayload::from_i64(100);
        let big = BigIntPayload::from_decimal_str("9223372036854775808").unwrap();
        assert!(small < big);
    }
}
