//! NaN-box value ↔ `GcRef<T>` bridge.
//!
//! Strategy B of the GC migration replaces `Handle(u32)` with raw
//! [`GcRef<T>`] pointers in the VM's `Value` NaN-box layout. The VM
//! crate (`otter-vm`) forbids `unsafe`, so the integer ↔ pointer
//! conversions live here, where `unsafe` is allowed under documented
//! `// SAFETY:` reasoning.
//!
//! The two conversions are:
//!
//! * [`gc_ref_to_payload`] — pack a `GcRef<T>` into the low 48 bits
//!   of a `u64`. Always safe because we are merely reading an existing
//!   valid pointer as an integer.
//! * [`payload_to_gc_ref`] — recover a `GcRef<T>` from those 48 bits.
//!   The callsite is *not* marked `unsafe`, but it carries a contract:
//!   the payload must have been produced by [`gc_ref_to_payload`] for
//!   a `GcRef<T>` whose underlying allocation is still alive. This
//!   contract is the same rooting discipline that every `GcRef`
//!   accessor already requires; the function simply expresses it via
//!   documentation rather than the type system, so VM code can call it
//!   under `forbid(unsafe_code)`. Misuse is undefined behaviour, just
//!   as it would be for `GcRef::payload` on a stale ref.
//!
//! # Why a 48-bit pointer is enough
//!
//! All major 64-bit architectures Otter targets (x86-64, AArch64,
//! RISC-V) restrict valid heap addresses to canonical 47–48-bit
//! ranges. The page-based [`crate::heap::GcHeap`] allocates pages
//! through the system allocator; those pages live well below the
//! 48-bit boundary. The NaN-box layout in `otter-vm` reserves the
//! top 16 bits for the tag word and uses the bottom 48 bits for the
//! payload, which is exactly enough for a `*mut GcHeader`.
//!
//! On platforms where this assumption ever fails (5-level paging
//! enabled with > 48-bit virtual addresses), the constructors will
//! still succeed at the integer level but the resulting pointer will
//! be incorrect. The VM debug-asserts the upper-16-bits-zero invariant
//! at NaN-box construction time so the failure surfaces immediately.

use std::ptr::NonNull;

use crate::gc_ref::GcRef;
use crate::header::GcHeader;

/// Mask covering the low 48 bits of a `u64`. Matches the
/// `OBJECT_PAYLOAD_MASK` constant in `otter-vm`'s `value.rs`.
pub const PAYLOAD_MASK: u64 = 0x0000_FFFF_FFFF_FFFF;

/// Packs a [`GcRef<T>`] into a 48-bit NaN-box payload.
///
/// The high 16 bits of the returned `u64` are zero. Combine with the
/// VM-side tag bits via `tag | gc_ref_to_payload(r)`.
#[inline]
pub fn gc_ref_to_payload<T>(r: GcRef<T>) -> u64 {
    let bits = r.as_ptr().as_ptr() as u64;
    debug_assert_eq!(
        bits & !PAYLOAD_MASK,
        0,
        "GcRef pointer exceeds the 48-bit NaN-box payload range — \
         platform may use > 48-bit canonical addresses",
    );
    bits & PAYLOAD_MASK
}

/// Recovers a [`GcRef<T>`] from a 48-bit NaN-box payload.
///
/// Returns `None` if `payload` is zero (interpreted as a null
/// pointer, which `GcRef` cannot represent).
///
/// # Contract
///
/// `payload` must have been produced by [`gc_ref_to_payload`] for a
/// `GcRef<T>` whose underlying allocation is still alive at this
/// call. Violating this contract is undefined behaviour the moment
/// the returned `GcRef` is dereferenced, exactly as it would be for
/// any other stale GC reference. This contract is enforced through
/// documentation only — the function signature is safe so VM code
/// (which forbids `unsafe`) can call it directly.
#[inline]
pub fn payload_to_gc_ref<T>(payload: u64) -> Option<GcRef<T>> {
    let raw = payload & PAYLOAD_MASK;
    let ptr = NonNull::new(raw as *mut GcHeader)?;
    // SAFETY: per the function-level contract, `payload` was produced
    // by `gc_ref_to_payload` for a still-live `GcRef<T>`. The pointer
    // is therefore a valid `[GcHeader | T]` allocation. Callers carry
    // the same rooting discipline as for any `GcRef` accessor.
    Some(unsafe { GcRef::<T>::from_raw_unchecked(ptr) })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gc_ref::type_tag;
    use crate::heap::{GcConfig, GcHeap};
    use crate::local::HandleScope;
    use crate::types::{self, string::JsStringGc};
    use std::sync::atomic::{AtomicU8, AtomicU32};

    fn fresh_heap() -> GcHeap {
        let mut heap = GcHeap::new(GcConfig {
            young_gen_size: 1024 * 1024,
            old_gen_threshold: 512 * 1024,
            ..GcConfig::default()
        });
        types::register_all(&mut heap);
        heap
    }

    fn alloc_string<'gc>(
        scope: &mut HandleScope<'gc>,
    ) -> GcRef<JsStringGc> {
        scope
            .alloc_typed(
                type_tag::STRING,
                JsStringGc {
                    length: 0,
                    hash: AtomicU32::new(0),
                    flags: AtomicU8::new(0),
                    _padding: [0; 3],
                    repr: crate::types::string::JsStringRepr::SeqOneByte(Box::new([])),
                },
            )
            .expect("alloc")
            .as_ref()
    }

    #[test]
    fn round_trips_through_payload() {
        let mut heap = fresh_heap();
        let mut scope = HandleScope::new(&mut heap);
        let r = alloc_string(&mut scope);

        let payload = gc_ref_to_payload(r);
        // High 16 bits must be zero — the VM tag occupies them.
        assert_eq!(payload & !PAYLOAD_MASK, 0);

        let recovered: GcRef<JsStringGc> =
            payload_to_gc_ref(payload).expect("non-null payload");
        assert_eq!(r, recovered);
    }

    #[test]
    fn null_payload_returns_none() {
        let r: Option<GcRef<JsStringGc>> = payload_to_gc_ref(0);
        assert!(r.is_none());
    }

    #[test]
    fn payload_high_bits_dont_leak_into_pointer() {
        let mut heap = fresh_heap();
        let mut scope = HandleScope::new(&mut heap);
        let r = alloc_string(&mut scope);

        // Simulate the VM tag occupying the high bits.
        let tag: u64 = 0x7FFE_0000_0000_0000;
        let payload = gc_ref_to_payload(r);
        let combined = tag | payload;
        // The recovery function masks the high bits back off.
        let recovered: GcRef<JsStringGc> =
            payload_to_gc_ref(combined).expect("non-null");
        assert_eq!(r, recovered);
    }
}
