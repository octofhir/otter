//! Arbitrary-precision integer values (`Value::BigInt`).
//!
//! ECMAScript `BigInt` is a primitive distinct from `Number`:
//! every arithmetic operator that mixes a `Number` with a `BigInt`
//! is a spec-mandated `TypeError`. The foundation slice models
//! that strict separation by giving `Value` its own `BigInt`
//! variant whose payload is a [`BigIntValue`] handle.
//!
//! # Contents
//! - [`BigIntValue`] — `Rc`-shared wrapper around a
//!   [`num_bigint::BigInt`]; cloning is cheap.
//! - Constructors and conversions: [`from_i32`], [`from_decimal`],
//!   [`as_inner`].
//! - [`ops`] — arithmetic, comparison, and bitwise primitives. The
//!   VM dispatcher routes `Op::Add` / `Op::Sub` / `Op::BitwiseAnd`
//!   / etc. through the helpers there when both operands are
//!   `BigInt`.
//!
//! # Spec references
//! - ECMA-262 §6.1.6.2 (BigInt type).
//! - ECMA-262 §13.10 (Bitwise Operators) — BigInt path uses the
//!   integer rules without `ToInt32`-style truncation.
//!
//! # Invariants
//! - `BigIntValue` always holds a normalised `BigInt` (no
//!   redundant leading zeros) — `num_bigint` guarantees that.
//! - `Number` and `BigInt` are never equal under `===`. Loose
//!   equality across the two kinds checks numeric value.

use std::rc::Rc;

use num_bigint::BigInt;
use serde::{Deserialize, Serialize};

pub mod dispatch;
pub mod ops;
pub mod prototype;

/// Heap-shared arbitrary-precision integer. Cheap to clone.
#[derive(Debug, Clone)]
pub struct BigIntValue {
    inner: Rc<BigInt>,
}

impl BigIntValue {
    /// Wrap an existing [`num_bigint::BigInt`].
    #[must_use]
    pub fn from_inner(value: BigInt) -> Self {
        Self {
            inner: Rc::new(value),
        }
    }

    /// Convert from a small integer.
    #[must_use]
    pub fn from_i32(n: i32) -> Self {
        Self::from_inner(BigInt::from(n))
    }

    /// Convert from a 128-bit signed integer (used by Temporal
    /// `epochNanoseconds` / `Instant.fromEpochMilliseconds`).
    #[must_use]
    pub fn from_i128(n: i128) -> Self {
        Self::from_inner(BigInt::from(n))
    }

    /// Parse a decimal-integer literal (no `n` suffix). Returns
    /// `None` when the string isn't a syntactically valid BigInt.
    #[must_use]
    pub fn from_decimal(text: &str) -> Option<Self> {
        text.parse::<BigInt>().ok().map(Self::from_inner)
    }

    /// Borrow the underlying `num_bigint::BigInt`.
    #[must_use]
    pub fn as_inner(&self) -> &BigInt {
        &self.inner
    }

    /// Identity comparison (true iff both handles share the same
    /// `Rc` allocation).
    #[must_use]
    pub fn ptr_eq(&self, other: &Self) -> bool {
        Rc::ptr_eq(&self.inner, &other.inner)
    }

    /// Spec rendering: decimal digits **without** a trailing `n`.
    /// Use this for both `BigInt.prototype.toString()` and the
    /// CLI display path.
    #[must_use]
    pub fn to_decimal_string(&self) -> String {
        self.inner.to_string()
    }
}

impl PartialEq for BigIntValue {
    fn eq(&self, other: &Self) -> bool {
        self.inner == other.inner
    }
}

impl Eq for BigIntValue {}

// `Serialize`/`Deserialize` for the wire form just round-trip the
// decimal rendering — it's lossless and human-readable in JSON
// dumps. Tests rarely (if ever) rely on this; production
// bytecode dumps surface BigInt constants via the dedicated
// `Constant::BigInt { decimal: String }` variant rather than
// reaching for a Value's serde impl, but having it satisfies
// `Value: Serialize` blanket bounds.
impl Serialize for BigIntValue {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.to_decimal_string())
    }
}

impl<'de> Deserialize<'de> for BigIntValue {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        BigIntValue::from_decimal(&s).ok_or_else(|| {
            serde::de::Error::custom(format!("invalid BigInt decimal literal `{s}`"))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_decimal_round_trips() {
        let v = BigIntValue::from_decimal("9007199254740993").unwrap();
        assert_eq!(v.to_decimal_string(), "9007199254740993");
    }

    #[test]
    fn equality_compares_value_not_handle() {
        let a = BigIntValue::from_i32(42);
        let b = BigIntValue::from_i32(42);
        assert_eq!(a, b);
        assert!(!a.ptr_eq(&b));
    }

    #[test]
    fn rejects_invalid_literal() {
        assert!(BigIntValue::from_decimal("12.3").is_none());
        assert!(BigIntValue::from_decimal("abc").is_none());
    }
}
