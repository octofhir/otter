//! Arbitrary-precision integer values (`Value::BigInt`).
//!
//! ECMAScript `BigInt` is a primitive distinct from `Number`: every
//! arithmetic operator that mixes a `Number` with a `BigInt` is a
//! spec-mandated `TypeError`, so `Value` carries its own `BigInt` family
//! whose payload is a [`BigIntValue`] handle.
//!
//! # Contents
//! - [`BigIntValue`] — `Copy` 4-byte handle wrapping [`BigIntHandle`]
//!   (`Gc<BigIntBody>`). Reads route through `&GcHeap`.
//! - [`gc_body`] — the V8-layout body: sign, length, inline digits.
//! - [`digits`] — magnitude kernels writing into preallocated results.
//! - [`ops`] — the operators: each sizes, allocates and fills its result.
//! - [`dispatch`] — `BigInt(...)`, `BigInt.asIntN`, `BigInt.asUintN`.
//! - [`prototype`] — `BigInt.prototype.toString` / `valueOf`.
//!
//! # Invariants
//! - The wrapper holds **only** the GC handle (4 bytes, `Copy`); bodies own
//!   no memory outside the heap and are immutable once published.
//! - `num_bigint` appears only at cold edges — parsing, multi-digit radix
//!   rendering, conversions for embedders — and in the kernels' large-size
//!   multiplication.
//! - `Number` and `BigInt` are never equal under `===`. Loose equality
//!   across the two kinds checks numeric value.
//!
//! # Spec references
//! - ECMA-262 §6.1.6.2 (BigInt type).

use num_bigint::{BigInt, Sign};
use serde::{Deserialize, Serialize};

pub mod digits;
pub mod dispatch;
pub mod gc_body;
pub mod ops;
pub mod prototype;

pub use gc_body::{BIG_INT_BODY_TYPE_TAG, BigIntBody, BigIntHandle};

/// Heap handle for [`crate::Value::BigInt`].
///
/// `PartialEq` / `Eq` / `Hash` are **handle** identity. Spec `===` /
/// `SameValue` for BigInts is numeric equality, served by
/// [`BigIntValue::numeric_eq`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct BigIntValue {
    inner: BigIntHandle,
}

impl BigIntValue {
    /// A BigInt with the given sign and magnitude digits (least significant
    /// first; leading zeros allowed).
    ///
    /// # Errors
    /// Surfaces [`otter_gc::OutOfMemory`].
    pub fn from_magnitude(
        heap: &mut otter_gc::GcHeap,
        negative: bool,
        magnitude: &[u64],
    ) -> Result<Self, otter_gc::OutOfMemory> {
        Self::from_magnitude_with_roots(heap, negative, magnitude, &mut gc_body::no_roots)
    }

    /// [`Self::from_magnitude`] publishing the caller's live `roots` to a
    /// collection the allocation triggers.
    pub(crate) fn from_magnitude_with_roots(
        heap: &mut otter_gc::GcHeap,
        negative: bool,
        magnitude: &[u64],
        roots: &mut otter_gc::heap::RootSlotVisitor<'_>,
    ) -> Result<Self, otter_gc::OutOfMemory> {
        let len = digits::significant_len(magnitude);
        let body = gc_body::alloc_digits(heap, len, &mut [], roots)?;
        // SAFETY: no allocation while the slice is alive.
        unsafe { body.digits_mut().copy_from_slice(&magnitude[..len]) };
        Ok(Self::from_handle(body.publish(negative)))
    }

    /// Convert from a small integer.
    ///
    /// # Errors
    /// Surfaces [`otter_gc::OutOfMemory`].
    pub fn from_i32(heap: &mut otter_gc::GcHeap, n: i32) -> Result<Self, otter_gc::OutOfMemory> {
        Self::from_i64(heap, i64::from(n))
    }

    /// Convert from a signed 64-bit integer.
    ///
    /// # Errors
    /// Surfaces [`otter_gc::OutOfMemory`].
    pub fn from_i64(heap: &mut otter_gc::GcHeap, n: i64) -> Result<Self, otter_gc::OutOfMemory> {
        Self::from_magnitude(heap, n < 0, &[n.unsigned_abs()])
    }

    /// Convert from an unsigned 64-bit integer.
    ///
    /// # Errors
    /// Surfaces [`otter_gc::OutOfMemory`].
    pub fn from_u64(heap: &mut otter_gc::GcHeap, n: u64) -> Result<Self, otter_gc::OutOfMemory> {
        Self::from_magnitude(heap, false, &[n])
    }

    /// Convert from a 128-bit signed integer (Temporal epoch nanoseconds).
    ///
    /// # Errors
    /// Surfaces [`otter_gc::OutOfMemory`].
    pub fn from_i128(heap: &mut otter_gc::GcHeap, n: i128) -> Result<Self, otter_gc::OutOfMemory> {
        let magnitude = n.unsigned_abs();
        Self::from_magnitude(heap, n < 0, &[magnitude as u64, (magnitude >> 64) as u64])
    }

    /// Copy a `num_bigint` value onto the GC heap.
    ///
    /// # Errors
    /// Surfaces [`otter_gc::OutOfMemory`].
    pub fn from_num(
        heap: &mut otter_gc::GcHeap,
        value: &BigInt,
    ) -> Result<Self, otter_gc::OutOfMemory> {
        let magnitude: digits::Scratch = value.iter_u64_digits().collect();
        Self::from_magnitude(heap, value.sign() == Sign::Minus, &magnitude)
    }

    /// Parse a decimal-integer literal (no `n` suffix). Returns `None` when
    /// the string isn't a syntactically valid BigInt; returns `Some(Err(_))`
    /// when body allocation fails.
    pub fn from_decimal(
        heap: &mut otter_gc::GcHeap,
        text: &str,
    ) -> Option<Result<Self, otter_gc::OutOfMemory>> {
        text.parse::<BigInt>()
            .ok()
            .map(|big| Self::from_num(heap, &big))
    }

    /// Run `f` over the sign and significant magnitude digits.
    #[inline]
    pub fn read<R>(self, heap: &otter_gc::GcHeap, f: impl FnOnce(bool, &[u64]) -> R) -> R {
        heap.read_payload(self.inner, |body| f(body.is_negative(), body.digits()))
    }

    /// Copy the value out as a `num_bigint` integer.
    #[must_use]
    pub fn to_num(self, heap: &otter_gc::GcHeap) -> BigInt {
        self.read(heap, |negative, magnitude| {
            let sign = if negative { Sign::Minus } else { Sign::Plus };
            BigInt::from_biguint(sign, digits::to_biguint(magnitude))
        })
    }

    /// `true` when the value is negative.
    #[inline]
    #[must_use]
    pub fn is_negative(self, heap: &otter_gc::GcHeap) -> bool {
        self.read(heap, |negative, _| negative)
    }

    /// `true` iff the value is exactly zero.
    #[inline]
    #[must_use]
    pub fn is_zero(self, heap: &otter_gc::GcHeap) -> bool {
        self.read(heap, |_, magnitude| magnitude.is_empty())
    }

    /// `BigInt.asUintN(64, value)` as a native integer: the low 64 bits of
    /// the two's-complement value (ToBigUint64).
    #[must_use]
    pub fn to_u64_wrapping(self, heap: &otter_gc::GcHeap) -> u64 {
        self.read(heap, |negative, magnitude| {
            let low = magnitude.first().copied().unwrap_or(0);
            if negative { low.wrapping_neg() } else { low }
        })
    }

    /// `BigInt.asIntN(64, value)` as a native integer (ToBigInt64).
    #[must_use]
    pub fn to_i64_wrapping(self, heap: &otter_gc::GcHeap) -> i64 {
        self.to_u64_wrapping(heap) as i64
    }

    /// The value as an `i128`, or `None` past its range.
    #[must_use]
    pub fn to_i128(self, heap: &otter_gc::GcHeap) -> Option<i128> {
        self.read(heap, |negative, magnitude| {
            let magnitude = match *magnitude {
                [] => 0u128,
                [low] => u128::from(low),
                [low, high] => u128::from(low) | (u128::from(high) << 64),
                _ => return None,
            };
            if negative {
                0i128.checked_sub_unsigned(magnitude)
            } else {
                i128::try_from(magnitude).ok()
            }
        })
    }

    /// §21.2.5 Number(value): the nearest double, ties to even.
    #[must_use]
    pub fn to_f64(self, heap: &otter_gc::GcHeap) -> f64 {
        self.read(heap, |negative, magnitude| {
            let value = match *magnitude {
                [] => 0.0,
                [low] => low as f64,
                _ => num_traits::ToPrimitive::to_f64(&digits::to_biguint(magnitude))
                    .unwrap_or(f64::INFINITY),
            };
            if negative { -value } else { value }
        })
    }

    /// Spec rendering in `radix` (2..=36), without a trailing `n`.
    #[must_use]
    pub fn to_string_radix(self, heap: &otter_gc::GcHeap, radix: u32) -> String {
        self.read(heap, |negative, magnitude| {
            let mut text = match *magnitude {
                [] => return "0".to_owned(),
                [digit] => render_digit(digit, radix),
                _ => digits::to_biguint(magnitude).to_str_radix(radix),
            };
            if negative {
                text.insert(0, '-');
            }
            text
        })
    }

    /// Decimal rendering without a trailing `n` (`ToString`, display,
    /// JSON diagnostics).
    #[must_use]
    pub fn to_decimal_string(self, heap: &otter_gc::GcHeap) -> String {
        self.to_string_radix(heap, 10)
    }

    /// Spec `===` / SameValue for two BigInts: numeric equality.
    #[must_use]
    pub fn numeric_eq(self, other: Self, heap: &otter_gc::GcHeap) -> bool {
        self.inner == other.inner || self.compare(other, heap).is_eq()
    }

    /// Numeric three-way comparison.
    #[must_use]
    pub fn compare(self, other: Self, heap: &otter_gc::GcHeap) -> std::cmp::Ordering {
        self.read(heap, |a_negative, a| {
            other.read(heap, |b_negative, b| match (a_negative, b_negative) {
                (false, true) => std::cmp::Ordering::Greater,
                (true, false) => std::cmp::Ordering::Less,
                (false, false) => digits::compare(a, b),
                (true, true) => digits::compare(b, a),
            })
        })
    }

    /// A hash of the numeric value: equal BigInts hash equally.
    #[must_use]
    pub fn content_hash(self, heap: &otter_gc::GcHeap) -> u64 {
        self.read(heap, |negative, magnitude| {
            magnitude.iter().fold(u64::from(negative), |hash, &digit| {
                (hash.rotate_left(5) ^ digit).wrapping_mul(0x51_7cc1_b727_220a_95)
            })
        })
    }

    /// Raw GC handle — used by tracing and write barriers.
    #[doc(hidden)]
    #[inline]
    #[must_use]
    pub fn handle(self) -> BigIntHandle {
        self.inner
    }

    /// Trace this wrapper's body handle as a GC slot so a moving collector
    /// rewrites it in place, where a `BigIntValue` is stored outside a
    /// `Value` (a BigInt wrapper's `[[BigIntData]]`, a collection key).
    pub(crate) fn trace_handle_slot(&mut self, visitor: &mut otter_gc::raw::SlotVisitor<'_>) {
        if !self.inner.is_null() {
            visitor(std::ptr::addr_of_mut!(self.inner).cast::<otter_gc::raw::RawGc>());
        }
    }

    /// Rebuild a [`BigIntValue`] from a pre-existing [`BigIntHandle`].
    #[inline]
    #[must_use]
    pub fn from_handle(handle: BigIntHandle) -> Self {
        Self { inner: handle }
    }

    /// Identity comparison — handle equality. For spec-correct numeric
    /// equality use [`BigIntValue::numeric_eq`].
    #[must_use]
    pub fn ptr_eq(self, other: Self) -> bool {
        self.inner == other.inner
    }
}

/// One digit rendered in `radix`.
fn render_digit(mut digit: u64, radix: u32) -> String {
    let mut buffer = [0u8; 64];
    let mut at = buffer.len();
    while digit != 0 {
        at -= 1;
        buffer[at] = b"0123456789abcdefghijklmnopqrstuvwxyz"[(digit % u64::from(radix)) as usize];
        digit /= u64::from(radix);
    }
    String::from_utf8_lossy(&buffer[at..]).into_owned()
}

// `Serialize` only needs an identifier — the value model's serde
// path is debug-only and the matching `Deserialize` is intentionally
// unimplemented (see below). Production bytecode reaches BigInt
// constants through the dedicated `Constant::BigInt { decimal:
// String }` variant rather than this impl.
impl Serialize for BigIntValue {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_u32(self.inner.offset())
    }
}

/// Deserialization is intentionally unimplemented: reconstructing a
/// `BigIntValue` requires both the underlying numeric payload and a
/// live `GcHeap` to allocate the body, neither of which serde's
/// stateless `Deserialize` API can supply. Callers must use
/// `BigIntValue::from_decimal(heap, text)` directly.
impl<'de> Deserialize<'de> for BigIntValue {
    fn deserialize<D: serde::Deserializer<'de>>(_deserializer: D) -> Result<Self, D::Error> {
        Err(serde::de::Error::custom(
            "BigIntValue cannot be deserialised without a GcHeap; use \
             BigIntValue::from_decimal(heap, text) at the call site instead",
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fresh_heap() -> otter_gc::GcHeap {
        otter_gc::GcHeap::new().expect("gc heap")
    }

    #[test]
    fn from_decimal_round_trips() {
        let mut heap = fresh_heap();
        for text in [
            "9007199254740993",
            "-340282366920938463463374607431768211457",
            "0",
        ] {
            let v = BigIntValue::from_decimal(&mut heap, text).unwrap().unwrap();
            assert_eq!(v.to_decimal_string(&heap), text);
        }
    }

    #[test]
    fn numeric_eq_compares_value_not_handle() {
        let mut heap = fresh_heap();
        let a = BigIntValue::from_i32(&mut heap, 42).unwrap();
        let b = BigIntValue::from_i32(&mut heap, 42).unwrap();
        assert!(a.numeric_eq(b, &heap));
        assert!(!a.ptr_eq(b));
        assert_eq!(a.content_hash(&heap), b.content_hash(&heap));
    }

    #[test]
    fn rejects_invalid_literal() {
        let mut heap = fresh_heap();
        assert!(BigIntValue::from_decimal(&mut heap, "12.3").is_none());
        assert!(BigIntValue::from_decimal(&mut heap, "abc").is_none());
    }

    #[test]
    fn sign_zero_and_wrapping_reads() {
        let mut heap = fresh_heap();
        let zero = BigIntValue::from_i32(&mut heap, 0).unwrap();
        assert!(zero.is_zero(&heap) && !zero.is_negative(&heap));
        let neg = BigIntValue::from_i32(&mut heap, -7).unwrap();
        assert!(neg.is_negative(&heap));
        assert_eq!(neg.to_u64_wrapping(&heap), (-7i64) as u64);
        assert_eq!(neg.to_i128(&heap), Some(-7));
        assert_eq!(neg.to_string_radix(&heap, 2), "-111");
    }
}
