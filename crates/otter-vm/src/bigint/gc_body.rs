//! GC-managed body for [`crate::Value::BigInt`].
//!
//! # Contents
//!
//! - [`BigIntBody`] — owns a [`num_bigint::BigInt`] payload directly.
//! - [`BigIntHandle`] — 4-byte `Gc<BigIntBody>` handle, `Copy`.
//! - [`alloc_big_int`] — allocator routed through
//!   [`otter_gc::GcHeap::alloc_old`].
//! - [`BIG_INT_BODY_TYPE_TAG`] — reserved
//!   [`otter_gc::Traceable::TYPE_TAG`].
//!
//! # Invariants
//!
//! - The `num_bigint::BigInt` payload is `Drop`-managed by the GC
//!   body's Rust drop.
//! - `Gc::offset == 0` is reserved as null.
//! - Trace impl is empty: `BigInt` holds no GC references.
//!
//! # Spec
//!
//! - ECMA-262 §6.1.6.2 — The BigInt Type.

use num_bigint::BigInt;

use otter_gc::GcHeap;
use otter_gc::OutOfMemory;
use otter_gc::raw::SlotVisitor;

/// Reserved [`otter_gc::Traceable::TYPE_TAG`] for [`BigIntBody`].
pub const BIG_INT_BODY_TYPE_TAG: u8 = 0x25;

/// GC-allocated payload backing every `Value::BigInt`.
#[derive(Debug, Clone)]
pub struct BigIntBody {
    /// Underlying arbitrary-precision integer.
    pub inner: BigInt,
}

impl otter_gc::SafeTraceable for BigIntBody {
    const TYPE_TAG: u8 = BIG_INT_BODY_TYPE_TAG;

    /// No outgoing GC slots — `BigInt` is plain numeric data.
    fn trace_slots_safe(&self, _visitor: &mut SlotVisitor<'_>) {}
}

/// 4-byte compressed handle to a [`BigIntBody`]. `Copy`.
pub type BigIntHandle = otter_gc::Gc<BigIntBody>;

/// Allocate a BigInt body on the GC heap.
///
/// Routes through [`GcHeap::alloc_old`]; the `Vec<BigDigit>` limb
/// storage is already out-of-line so young-space allocation gives no
/// locality benefit.
///
/// # Errors
///
/// Surfaces [`OutOfMemory`] verbatim.
pub fn alloc_big_int(heap: &mut GcHeap, value: BigInt) -> Result<BigIntHandle, OutOfMemory> {
    heap.alloc_old(BigIntBody { inner: value })
}

#[cfg(test)]
mod tests {
    use super::*;
    use num_bigint::Sign;

    #[test]
    fn round_trips_through_gc_heap() {
        let mut heap = GcHeap::new().expect("heap");
        let big = BigInt::from(2_i128.pow(70));
        let handle = alloc_big_int(&mut heap, big.clone()).expect("alloc");
        heap.read_payload(handle, |body| {
            assert_eq!(body.inner, big);
        });
    }

    #[test]
    fn negative_round_trip() {
        let mut heap = GcHeap::new().expect("heap");
        let big = BigInt::from_biguint(Sign::Minus, num_bigint::BigUint::from(42u32));
        let handle = alloc_big_int(&mut heap, big.clone()).expect("alloc");
        heap.read_payload(handle, |body| {
            assert_eq!(body.inner, big);
        });
    }

    #[test]
    fn type_tag_matches_traceable_const() {
        assert_eq!(
            <BigIntBody as otter_gc::SafeTraceable>::TYPE_TAG,
            BIG_INT_BODY_TYPE_TAG
        );
    }
}
