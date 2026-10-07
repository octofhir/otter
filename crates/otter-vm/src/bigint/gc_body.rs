//! GC-managed body for [`crate::Value::BigInt`].
//!
//! V8's `BigInt` layout: a sign, a digit count, and the magnitude's
//! little-endian `u64` digits stored inline after the body. A body owns no
//! memory outside the heap, so it is born young, dies in a scavenge, and
//! moves with the collector like a string.
//!
//! # Contents
//!
//! - [`BigIntBody`] — sign, length, and the trailing digit array.
//! - [`BigIntHandle`] — 4-byte `Gc<BigIntBody>` handle, `Copy`.
//! - [`alloc_digits`] — allocate a zeroed body with room for its digits.
//! - [`BIG_INT_BODY_TYPE_TAG`] — reserved
//!   [`otter_gc::Traceable::TYPE_TAG`].
//!
//! # Invariants
//!
//! - A published body is canonical: `len` counts significant digits only and
//!   zero is `len == 0` with a clear sign. The allocation may hold spare
//!   digits past `len`; they are never read.
//! - Only the operation that allocated a body writes it, before the handle
//!   escapes. Published bodies are immutable.
//! - Trace impl is empty: a BigInt holds no GC references.
//!
//! # Spec
//!
//! - ECMA-262 §6.1.6.2 — The BigInt Type.

use otter_gc::GcHeap;
use otter_gc::OutOfMemory;
use otter_gc::heap::RootSlotVisitor;
use otter_gc::raw::SlotVisitor;

/// Reserved [`otter_gc::Traceable::TYPE_TAG`] for [`BigIntBody`].
pub const BIG_INT_BODY_TYPE_TAG: u8 = 0x25;

/// GC-allocated payload backing every `Value::BigInt`; the digits follow it.
#[derive(Debug)]
#[repr(C)]
pub struct BigIntBody {
    len: u32,
    negative: bool,
}

/// Byte offset of the length word inside the body; the sign byte follows it
/// at [`BIG_INT_NEGATIVE_OFFSET`] and the digits start after the body.
pub const BIG_INT_LEN_OFFSET: usize = std::mem::offset_of!(BigIntBody, len);
/// Byte offset of the sign byte inside the body.
pub const BIG_INT_NEGATIVE_OFFSET: usize = std::mem::offset_of!(BigIntBody, negative);

/// 4-byte compressed handle to a [`BigIntBody`]. `Copy`.
pub type BigIntHandle = otter_gc::Gc<BigIntBody>;

impl otter_gc::SafeTraceable for BigIntBody {
    const TYPE_TAG: u8 = BIG_INT_BODY_TYPE_TAG;

    fn trace_slots_safe(&mut self, _visitor: &mut SlotVisitor<'_>) {}
}

impl BigIntBody {
    /// Whether the value is negative.
    #[inline]
    #[must_use]
    pub fn is_negative(&self) -> bool {
        self.negative
    }

    /// The magnitude's significant digits, least significant first.
    #[inline]
    #[must_use]
    pub fn digits(&self) -> &[u64] {
        // SAFETY: the allocation reserved at least `len` digits right after
        // the body, and `len` only ever shrinks.
        unsafe { std::slice::from_raw_parts(self.digits_ptr(), self.len as usize) }
    }

    fn digits_ptr(&self) -> *const u64 {
        // SAFETY: the digits start one `Self` past the body; `Self` is
        // 8 bytes and the payload 8-aligned, so they are aligned.
        unsafe { (self as *const Self).add(1).cast::<u64>() }
    }
}

/// A freshly allocated body still being written: its full digit capacity is
/// writable, and [`Self::publish`] fixes the canonical length and sign.
pub(crate) struct UnpublishedBigInt {
    handle: BigIntHandle,
    capacity: usize,
}

impl UnpublishedBigInt {
    /// The writable digit capacity.
    ///
    /// # Safety
    /// No allocation may happen while the slice is alive, and it must not
    /// overlap any other body slice the caller holds (it cannot: the body
    /// is fresh).
    #[inline]
    pub(crate) unsafe fn digits_mut<'a>(&self) -> &'a mut [u64] {
        // SAFETY: per the contract above; the cell reserved `capacity` digits.
        unsafe {
            let body = body_ptr(self.handle);
            std::slice::from_raw_parts_mut((*body).digits_ptr().cast_mut(), self.capacity)
        }
    }

    /// Canonicalize the written digits and publish the body.
    #[inline]
    pub(crate) fn publish(self, negative: bool) -> BigIntHandle {
        // SAFETY: no allocation since `digits_mut`; the body is ours alone.
        unsafe {
            let digits = self.digits_mut();
            let len = super::digits::significant_len(digits);
            let body = body_ptr(self.handle);
            (*body).len = len as u32;
            (*body).negative = negative && len != 0;
        }
        self.handle
    }
}

/// Raw access to a body. The pointer is valid until the next allocation.
///
/// # Safety
/// `handle` names a live BigInt body.
#[inline]
pub(crate) unsafe fn body_ptr(handle: BigIntHandle) -> *mut BigIntBody {
    // SAFETY: the payload lives one header past the cell start.
    unsafe {
        handle
            .as_header_ptr()
            .cast::<u8>()
            .add(otter_gc::header::HEADER_SIZE)
            .cast::<BigIntBody>()
    }
}

/// A root visitor with nothing to add.
pub(crate) fn no_roots(_visitor: &mut dyn FnMut(*mut otter_gc::raw::RawGc)) {}

/// Allocate a zeroed body with room for `capacity` digits.
///
/// `handles` names the BigInt handles the caller still reads after the
/// allocation; a collection it triggers rewrites them in place. `roots`
/// publishes the caller's other live values (a compiled frame's spill slots)
/// and runs only if the allocation collects.
///
/// # Errors
/// Surfaces [`OutOfMemory`] verbatim.
pub(crate) fn alloc_digits(
    heap: &mut GcHeap,
    capacity: usize,
    handles: &mut [BigIntHandle],
    roots: &mut RootSlotVisitor<'_>,
) -> Result<UnpublishedBigInt, OutOfMemory> {
    let mut visit = |visitor: &mut dyn FnMut(*mut otter_gc::raw::RawGc)| {
        for handle in handles.iter_mut() {
            visitor(std::ptr::from_mut(handle).cast());
        }
        roots(visitor);
    };
    let external_visit: &mut RootSlotVisitor<'_> = &mut visit;
    let handle = heap.alloc_trailing_with_roots(
        BigIntBody {
            len: 0,
            negative: false,
        },
        capacity * std::mem::size_of::<u64>(),
        external_visit,
    )?;
    Ok(UnpublishedBigInt { handle, capacity })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn digits_round_trip_through_gc_heap() {
        let mut heap = GcHeap::new().expect("heap");
        let body = alloc_digits(&mut heap, 3, &mut [], &mut no_roots).expect("alloc");
        // SAFETY: no allocation while the slice is alive.
        unsafe { body.digits_mut().copy_from_slice(&[7, 9, 0]) };
        let handle = body.publish(true);
        heap.read_payload(handle, |body| {
            assert_eq!(body.digits(), &[7, 9]);
            assert!(body.is_negative());
        });
    }

    #[test]
    fn zero_publishes_without_sign() {
        let mut heap = GcHeap::new().expect("heap");
        let handle = alloc_digits(&mut heap, 2, &mut [], &mut no_roots)
            .expect("alloc")
            .publish(true);
        heap.read_payload(handle, |body| {
            assert!(body.digits().is_empty());
            assert!(!body.is_negative());
        });
    }
}
