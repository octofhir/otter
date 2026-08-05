//! Out-of-line property storage, allocated inside the GC heap.
//!
//! An object keeps its first [`super::INLINE_SLOT_CAP`] string-keyed
//! slots in the body itself. Past that it needs a growable array, and
//! where that array lives decides whether the object is self-contained.
//! A `Vec` would put it in malloc memory the collector does not own: the
//! object could not be captured into a page image, a restored copy would
//! alias the original buffer, and the tracer would hand the collector
//! slot addresses outside the heap.
//!
//! So the overflow slab is a GC body with its words in trailing storage
//! in the same cell — the shape V8 and JSC use for a property backing
//! store, for the same reason.
//!
//! # Contents
//!
//! - [`SlotSlabBody`] — capacity header followed by its words.
//! - [`SlotSlabHandle`] — handle type stored by an object body.
//! - [`alloc_slot_slab`] — allocate one, with the caller's roots live
//!   across the allocation.
//!
//! # Invariants
//!
//! - The trailing array holds exactly `capacity` words and is initialized to
//!   `Value::undefined()` before the slab becomes observable.
//! - A slab never shrinks in place and never reallocates itself: growth
//!   allocates a larger slab and copies, so a live `values_ptr` into the
//!   old slab is invalidated by exactly one event the object controls.
//! - Words are traced in place, so the collector rewrites the live slot
//!   rather than a copy — the same contract the inline array has.
//! - Old space: a backing store lives as long as the object that owns it,
//!   so a semispace copy of one is pure overhead.

use otter_gc::raw::SlotVisitor;

use crate::Value;

/// Reserved [`otter_gc::Traceable::TYPE_TAG`] for [`SlotSlabBody`].
pub const SLOT_SLAB_BODY_TYPE_TAG: u8 = 0x31;

/// Handle to an object's out-of-line property slab.
pub type SlotSlabHandle = otter_gc::Gc<SlotSlabBody>;

/// Capacity header for an out-of-line property slab. The words follow it
/// in the same cell.
#[repr(C, align(8))]
pub struct SlotSlabBody {
    /// Words the trailing array can hold.
    capacity: u32,
}

impl SlotSlabBody {
    /// Trailing bytes a slab of `capacity` words needs.
    #[must_use]
    pub fn trailing_bytes(capacity: usize) -> usize {
        capacity * std::mem::size_of::<Value>()
    }

    /// Header for a slab of `capacity` words.
    #[must_use]
    pub fn new(capacity: usize) -> Self {
        Self {
            capacity: u32::try_from(capacity).expect("slab capacity exceeds u32"),
        }
    }

    /// Words this slab can hold.
    #[must_use]
    pub fn capacity(&self) -> usize {
        self.capacity as usize
    }

    /// Base of the trailing word array.
    ///
    /// The object body caches this as its `values_ptr` so a slot read is
    /// one indexed load whether the slab is inline or out of line.
    #[must_use]
    pub fn words_ptr(&self) -> *mut Value {
        // SAFETY: the allocation reserved `trailing_bytes(capacity)`
        // immediately after this header, so the array starts one `Self`
        // past `self` and runs for `capacity` words.
        unsafe {
            (self as *const Self as *mut u8)
                .add(std::mem::size_of::<Self>())
                .cast()
        }
    }
}

impl otter_gc::SafeTraceable for SlotSlabBody {
    const TYPE_TAG: u8 = SLOT_SLAB_BODY_TYPE_TAG;

    fn trace_slots_safe(&mut self, v: &mut SlotVisitor<'_>) {
        let base = self.words_ptr();
        for index in 0..self.capacity() {
            // SAFETY: `index < capacity`, and the array is live for the
            // body's lifetime.
            let word = unsafe { base.add(index) };
            // SAFETY: same in-range word as above. `Value` owns the precise
            // cell/immediate discrimination and exposes its embedded moving
            // GC offset directly to the collector.
            unsafe { (*word).trace_value_slot_mut(v) };
        }
    }
}

/// Allocate a slab that can hold `capacity` words.
///
/// Every word is initialized to `undefined`, so a caller may treat the tail
/// beyond the live length as empty without writing it.
///
/// `external_visit` must yield every root the caller holds: this
/// allocation can collect, and an object waiting to receive the slab is
/// exactly the kind of handle a collection would otherwise move out from
/// under the caller.
///
/// # Errors
/// Propagates [`otter_gc::OutOfMemory`].
pub fn alloc_slot_slab(
    heap: &mut otter_gc::GcHeap,
    capacity: usize,
    external_visit: &mut otter_gc::heap::RootSlotVisitor<'_>,
) -> Result<SlotSlabHandle, otter_gc::OutOfMemory> {
    let slab = heap.alloc_variable_with_roots(
        SlotSlabBody::new(capacity),
        SlotSlabBody::trailing_bytes(capacity),
        external_visit,
    )?;
    heap.with_payload(slab, |body| {
        for index in 0..body.capacity() {
            // SAFETY: the allocation reserved exactly `capacity` trailing
            // `Value` words and no observer can see the slab before return.
            unsafe { *body.words_ptr().add(index) = Value::undefined() };
        }
    });
    Ok(slab)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Interpreter;

    #[test]
    fn a_fresh_slab_is_empty_and_sized() {
        let mut interp = Interpreter::new();
        let slab = alloc_slot_slab(interp.gc_heap_mut(), 12, &mut |_| {}).expect("slab");
        let capacity = interp.gc_heap().read_payload(slab, SlotSlabBody::capacity);
        assert_eq!(capacity, 12);
        for index in 0..capacity {
            let word = interp.gc_heap().read_payload(slab, |body| {
                // SAFETY: index is within the capacity just read.
                unsafe { *body.words_ptr().add(index) }
            });
            assert_eq!(word, Value::undefined(), "word {index} starts empty");
        }
    }

    #[test]
    fn words_round_trip_through_the_trailing_array() {
        let mut interp = Interpreter::new();
        let slab = alloc_slot_slab(interp.gc_heap_mut(), 4, &mut |_| {}).expect("slab");
        interp.gc_heap_mut().with_payload(slab, |body| {
            for index in 0..body.capacity() {
                // SAFETY: index is within the slab's capacity.
                unsafe { *body.words_ptr().add(index) = Value::number_i32(index as i32) };
            }
            true
        });
        for index in 0..4usize {
            let word = interp.gc_heap().read_payload(slab, |body| {
                // SAFETY: index is within the slab's capacity.
                unsafe { *body.words_ptr().add(index) }
            });
            assert_eq!(word, Value::number_i32(index as i32));
        }
    }
}
