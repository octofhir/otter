//! Arbitrary-precision integer values (`Value::BigInt`).
//!
//! ECMAScript `BigInt` is a primitive distinct from `Number`:
//! every arithmetic operator that mixes a `Number` with a `BigInt`
//! is a spec-mandated `TypeError`. The foundation slice models
//! that strict separation by giving `Value` its own `BigInt`
//! variant whose payload is a [`BigIntValue`] handle.
//!
//! # Contents
//! - [`BigIntValue`] — `Copy` 4-byte handle wrapping
//!   [`BigIntHandle`] (`Gc<BigIntBody>`). All reads route through
//!   `&GcHeap`; no off-heap cache. Matches the V8 / JSC /
//!   SpiderMonkey shape where a `BigInt` is a heap cell and
//!   `toString` is computed lazily.
//! - [`gc_body`] — `BigIntBody` GC payload with the canonical
//!   `num_bigint::BigInt`.
//! - [`ops`] — arithmetic, comparison, and bitwise primitives over
//!   `&num_bigint::BigInt`. Callers borrow operands through
//!   [`BigIntValue::with_inner`] and wrap results via
//!   [`BigIntValue::from_inner`].
//! - [`dispatch`] — `BigInt(...)`, `BigInt.asIntN`, `BigInt.asUintN`.
//! - [`prototype`] — `BigInt.prototype.toString` / `valueOf`.
//!
//! # Spec references
//! - ECMA-262 §6.1.6.2 (BigInt type).
//! - ECMA-262 §13.10 (Bitwise Operators) — BigInt path uses the
//!   integer rules without `ToInt32`-style truncation.
//!
//! # Invariants
//! - The wrapper holds **only** the GC handle (4 bytes, `Copy`).
//!   No `Rc` / `Arc` / `Box` lives in `BigIntValue` or
//!   `BigIntBody`.
//! - `Number` and `BigInt` are never equal under `===`. Loose
//!   equality across the two kinds checks numeric value.

use num_bigint::{BigInt, Sign};
use otter_gc::raw::SlotVisitor;
use serde::{Deserialize, Serialize};

pub mod dispatch;
pub mod gc_body;
pub mod ops;
pub mod prototype;

pub use gc_body::{BIG_INT_BODY_TYPE_TAG, BigIntBody, BigIntHandle, alloc_big_int};

/// Heap handle for [`crate::Value::BigInt`].
///
/// `Copy` 4-byte handle. All reads route through `&GcHeap`.
///
/// `PartialEq` / `Eq` / `Hash` are derived as **handle-offset**
/// equality (same body → equal). Spec `===` / `SameValue` for
/// BigInts is numeric equality, served by
/// [`BigIntValue::numeric_eq`] which reads both bodies through the
/// GC heap.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct BigIntValue {
    inner: BigIntHandle,
}

impl BigIntValue {
    /// Wrap an existing [`num_bigint::BigInt`] on the GC heap.
    ///
    /// # Errors
    /// Surfaces [`otter_gc::OutOfMemory`] verbatim from body
    /// allocation.
    pub fn from_inner(
        heap: &mut otter_gc::GcHeap,
        value: BigInt,
    ) -> Result<Self, otter_gc::OutOfMemory> {
        let inner = alloc_big_int(heap, value)?;
        Ok(Self { inner })
    }

    /// Convert from a small integer.
    ///
    /// # Errors
    /// Surfaces [`otter_gc::OutOfMemory`].
    pub fn from_i32(heap: &mut otter_gc::GcHeap, n: i32) -> Result<Self, otter_gc::OutOfMemory> {
        Self::from_inner(heap, BigInt::from(n))
    }

    /// Convert from a 128-bit signed integer (used by Temporal
    /// `epochNanoseconds` / `Instant.fromEpochMilliseconds`).
    ///
    /// # Errors
    /// Surfaces [`otter_gc::OutOfMemory`].
    pub fn from_i128(heap: &mut otter_gc::GcHeap, n: i128) -> Result<Self, otter_gc::OutOfMemory> {
        Self::from_inner(heap, BigInt::from(n))
    }

    /// Parse a decimal-integer literal (no `n` suffix). Returns
    /// `None` when the string isn't a syntactically valid BigInt;
    /// returns `Some(Err(_))` when body allocation fails.
    pub fn from_decimal(
        heap: &mut otter_gc::GcHeap,
        text: &str,
    ) -> Option<Result<Self, otter_gc::OutOfMemory>> {
        text.parse::<BigInt>()
            .ok()
            .map(|big| Self::from_inner(heap, big))
    }

    /// Run `f` against the underlying [`num_bigint::BigInt`] borrowed
    /// from the GC body. The closure receives `&BigInt`; the borrow
    /// does not escape so the call is sound under the
    /// single-mutator otter-gc contract.
    #[inline]
    #[must_use]
    pub fn with_inner<F, R>(self, heap: &otter_gc::GcHeap, f: F) -> R
    where
        F: FnOnce(&BigInt) -> R,
    {
        heap.read_payload(self.inner, |body| f(&body.inner))
    }

    /// Clone the underlying [`num_bigint::BigInt`] out of the GC body.
    /// Use sparingly — for cryptographic-sized BigInts this can be
    /// expensive. Prefer [`Self::with_inner`] when the consumer only
    /// needs a borrow.
    #[inline]
    #[must_use]
    pub fn clone_inner(self, heap: &otter_gc::GcHeap) -> BigInt {
        heap.read_payload(self.inner, |body| body.inner.clone())
    }

    /// Raw GC handle — used by tracing and write barriers.
    #[doc(hidden)]
    #[inline]
    #[must_use]
    pub fn handle(self) -> BigIntHandle {
        self.inner
    }

    /// Rebuild a [`BigIntValue`] from a pre-existing
    /// [`BigIntHandle`]. Heap-free; the wrapper carries no cached
    /// fields beyond the handle.
    #[inline]
    #[must_use]
    pub fn from_handle(handle: BigIntHandle) -> Self {
        Self { inner: handle }
    }

    /// Visit the embedded GC handle so the scavenger can rewrite the
    /// compressed offset in place if the body moves. Called from
    /// [`crate::Value::trace_value_slots`].
    pub(crate) fn trace_value_slots(&self, visitor: &mut SlotVisitor<'_>) {
        let p = &self.inner as *const BigIntHandle as *mut otter_gc::raw::RawGc;
        visitor(p);
    }

    /// Spec rendering: decimal digits **without** a trailing `n`.
    /// Computed on demand from the body's `BigInt` payload — V8 /
    /// JSC / SpiderMonkey all lazy-compute `toString` and the hot
    /// path on a `BigInt` is arithmetic, not display, so no
    /// off-heap cache is warranted. Used by
    /// `BigInt.prototype.toString(10)`, the CLI display path, hash
    /// keys, and JSON output.
    #[must_use]
    pub fn to_decimal_string(self, heap: &otter_gc::GcHeap) -> String {
        heap.read_payload(self.inner, |body| body.inner.to_string())
    }

    /// Sign of the value — single heap touch, O(1) within the body.
    #[inline]
    #[must_use]
    pub fn sign(self, heap: &otter_gc::GcHeap) -> Sign {
        heap.read_payload(self.inner, |body| body.inner.sign())
    }

    /// `true` iff the value is exactly zero. Reads the body's sign.
    #[inline]
    #[must_use]
    pub fn is_zero(self, heap: &otter_gc::GcHeap) -> bool {
        matches!(self.sign(heap), Sign::NoSign)
    }

    /// Identity comparison — handle-offset equality. Two clones of
    /// the same wrapper return `true`; distinct allocations of the
    /// same numeric value return `false`. For spec-correct numeric
    /// equality use [`BigIntValue::numeric_eq`].
    #[must_use]
    pub fn ptr_eq(self, other: Self) -> bool {
        self.inner == other.inner
    }

    /// Spec `===` for BigInt vs BigInt — reads both bodies and folds
    /// through `BigInt::eq`. Fast path: handle equality short-circuits
    /// to `true` without a heap read; differing signs short-circuit to
    /// `false` after two O(1) sign reads.
    #[must_use]
    pub fn numeric_eq(self, other: Self, heap: &otter_gc::GcHeap) -> bool {
        if self.inner == other.inner {
            return true;
        }
        if self.sign(heap) != other.sign(heap) {
            return false;
        }
        self.with_inner(heap, |a| other.with_inner(heap, |b| a == b))
    }
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
        let v = BigIntValue::from_decimal(&mut heap, "9007199254740993")
            .unwrap()
            .unwrap();
        assert_eq!(v.to_decimal_string(&heap), "9007199254740993");
    }

    #[test]
    fn numeric_eq_compares_value_not_handle() {
        let mut heap = fresh_heap();
        let a = BigIntValue::from_i32(&mut heap, 42).unwrap();
        let b = BigIntValue::from_i32(&mut heap, 42).unwrap();
        assert!(a.numeric_eq(b, &heap));
        assert!(!a.ptr_eq(b));
    }

    #[test]
    fn rejects_invalid_literal() {
        let mut heap = fresh_heap();
        assert!(BigIntValue::from_decimal(&mut heap, "12.3").is_none());
        assert!(BigIntValue::from_decimal(&mut heap, "abc").is_none());
    }

    #[test]
    fn sign_and_zero_flag() {
        let mut heap = fresh_heap();
        let zero = BigIntValue::from_i32(&mut heap, 0).unwrap();
        assert!(zero.is_zero(&heap));
        assert_eq!(zero.sign(&heap), Sign::NoSign);

        let neg = BigIntValue::from_i32(&mut heap, -7).unwrap();
        assert!(!neg.is_zero(&heap));
        assert_eq!(neg.sign(&heap), Sign::Minus);

        let pos = BigIntValue::from_i32(&mut heap, 7).unwrap();
        assert_eq!(pos.sign(&heap), Sign::Plus);
    }

    #[test]
    fn with_inner_borrows_body_payload() {
        let mut heap = fresh_heap();
        let v = BigIntValue::from_inner(&mut heap, BigInt::from(123_456_789_i64)).unwrap();
        let doubled: BigInt = v.with_inner(&heap, |b| b * 2);
        assert_eq!(doubled.to_string(), "246913578");
    }
}
