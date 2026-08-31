//! Flat `Value` storage allocated inside the GC heap.
//!
//! A backing store of JS values was a `Vec<Value>` or a `SmallVec`:
//! malloc memory the collector does not own. That made its owner
//! unrestorable — a page image could not carry the buffer, a restored
//! copy would alias the original, and the tracer handed the collector
//! slot addresses outside the heap.
//!
//! So the values live in a GC body with the payload in trailing storage
//! in the same cell — V8's `FixedArray`, reached from the owner by
//! handle. An array's dense elements and a native function's captures
//! are the two current owners.
//!
//! # Contents
//!
//! - [`ValueSlabBody`] — capacity header followed by its values.
//! - [`ValueSlabHandle`] — handle type stored by an array body.
//! - [`alloc_value_slab`] — allocate one, with the caller's roots live
//!   across the allocation.
//!
//! # Invariants
//!
//! - The trailing array is 8-byte aligned and can hold `capacity`
//!   values, of which the first `len` are initialised. Only that prefix
//!   is traced: capacity a swept-and-reused cell handed over is not
//!   guaranteed to be zero, so tracing it would hand the collector
//!   whatever the previous tenant left behind.
//! - A slab never resizes itself: growth allocates a larger slab and
//!   copies, so a live `elements_ptr` into the old slab is invalidated by
//!   exactly one event the array controls.
//! - Values are traced in place, so the collector rewrites the live
//!   element rather than a copy.
//! - Old space: a backing store lives as long as the array that owns it,
//!   so a semispace copy of one is pure overhead — and old-space bodies
//!   do not move, which is what keeps the cached `elements_ptr` valid
//!   across a scavenge.
//!
//! # See also
//!
//! - [`crate::object::slot_slab`] — the same shape for property storage.

use otter_gc::raw::SlotVisitor;

use crate::Value;

/// Reserved [`otter_gc::Traceable::TYPE_TAG`] for [`ValueSlabBody`].
pub const VALUE_SLAB_BODY_TYPE_TAG: u8 = 0x33;

/// Handle to an array's dense element storage.
pub type ValueSlabHandle = otter_gc::Gc<ValueSlabBody>;

/// Capacity header for dense element storage. The values follow it in
/// the same cell.
///
/// Aligned to 8 so the trailing array of [`Value`] starts on an 8-byte
/// boundary: the header is padded to a full `Value` slot rather than the
/// four bytes the capacity needs, because an unaligned element read is
/// undefined behaviour.
#[repr(C, align(8))]
pub struct ValueSlabBody {
    /// Values the trailing array can hold.
    capacity: u32,
    /// Values actually initialised, and therefore traced. The array body
    /// mirrors this in its `dense_len` cache for compiled bounds checks;
    /// every mutation writes both.
    len: u32,
}

impl ValueSlabBody {
    /// Trailing bytes a slab of `capacity` values needs.
    #[must_use]
    pub fn trailing_bytes(capacity: usize) -> usize {
        capacity * std::mem::size_of::<Value>()
    }

    /// Header for an empty slab that can hold `capacity` values.
    #[must_use]
    pub fn new(capacity: usize) -> Self {
        Self {
            capacity: u32::try_from(capacity).expect("array element capacity exceeds u32"),
            len: 0,
        }
    }

    /// Values initialised so far.
    #[must_use]
    pub fn len(&self) -> usize {
        self.len as usize
    }

    /// `true` when no value has been initialised yet.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Set how many leading values are initialised.
    pub fn set_len(&mut self, len: usize) {
        debug_assert!(len <= self.capacity());
        self.len = u32::try_from(len).expect("array dense length exceeds u32");
    }

    /// Values this slab can hold.
    #[must_use]
    pub fn capacity(&self) -> usize {
        self.capacity as usize
    }

    /// Base of the trailing value array.
    ///
    /// The array body caches this as its `elements_ptr` so an element
    /// read is one indexed load for compiled code and for the runtime
    /// alike.
    #[must_use]
    pub fn values_ptr(&self) -> *mut Value {
        // SAFETY: the allocation reserved `trailing_bytes(capacity)`
        // immediately after this header, so the array starts one `Self`
        // past `self` and runs for `capacity` values.
        unsafe {
            (self as *const Self as *mut u8)
                .add(std::mem::size_of::<Self>())
                .cast()
        }
    }

    pub(crate) fn visit_function_ids(&self, visitor: &mut dyn FnMut(u32)) {
        let base = self.values_ptr();
        for index in 0..self.len() {
            // SAFETY: the live prefix is initialized before `len` is published.
            crate::code_liveness::visit_value(unsafe { &*base.add(index) }, visitor);
        }
    }
}

// The trailing values must land on their own alignment, so the header has
// to be a whole number of `Value` slots wide.
const _: () =
    assert!(std::mem::size_of::<ValueSlabBody>().is_multiple_of(std::mem::align_of::<Value>()));

impl otter_gc::SafeTraceable for ValueSlabBody {
    const TYPE_TAG: u8 = VALUE_SLAB_BODY_TYPE_TAG;

    fn trace_slots_safe(&mut self, visitor: &mut SlotVisitor<'_>) {
        let base = self.values_ptr();
        for index in 0..self.len() {
            // SAFETY: `index < len <= capacity`, and every value in that
            // prefix was written before the slab became reachable.
            let value = unsafe { &mut *base.add(index) };
            value.trace_value_slot_mut(visitor);
        }
    }

    /// The trailing array lives in the heap cell, not in this body, so a
    /// pending copy on the stack has nothing to trace: everything
    /// `trace_slots_safe` walks is storage that does not exist yet.
    fn trace_pending_slots_safe(&mut self, _visitor: &mut SlotVisitor<'_>) {}
}

/// Process address of `slab`'s first value, or null for a null handle.
///
/// This is what an array body caches as `elements_ptr`, and the one
/// place that decodes a slab handle without going through the heap — the
/// array's tracer needs it while it already holds the body.
#[must_use]
pub fn values_base(slab: ValueSlabHandle) -> *mut Value {
    if slab.is_null() {
        return std::ptr::null_mut();
    }
    // SAFETY: `body_of` returns a pointer to a live slab payload, and
    // `values_ptr` only does address arithmetic on it.
    body_of(slab).map_or(std::ptr::null_mut(), |body| unsafe { (*body).values_ptr() })
}

/// Values `slab` can hold, or zero for a null handle.
#[must_use]
pub fn capacity_of(slab: ValueSlabHandle) -> usize {
    body_of(slab).map_or(0, |body| {
        // SAFETY: `body_of` returns a pointer to a live slab payload.
        unsafe { (*body).capacity() }
    })
}

/// The slab payload behind `slab`, or `None` for a null handle.
///
/// The array body reaches its slab this way while holding a payload
/// borrow, where there is no heap to ask.
#[must_use]
pub fn body_of(slab: ValueSlabHandle) -> Option<*mut ValueSlabBody> {
    if slab.is_null() {
        return None;
    }
    let header = slab.as_header_ptr();
    // SAFETY: a non-null handle names a live cell whose payload is an
    // `ValueSlabBody` one header past the start.
    Some(unsafe {
        header
            .cast::<u8>()
            .add(std::mem::size_of::<otter_gc::GcHeader>())
            .cast::<ValueSlabBody>()
    })
}

/// Allocate an empty slab that can hold `capacity` values.
///
/// The slab starts with `len` zero, so nothing is traced until the
/// caller writes values and publishes the length.
///
/// `external_visit` must yield every root the caller holds: this
/// allocation can collect, and an array waiting to receive the slab is
/// exactly the kind of handle a collection would otherwise move out from
/// under the caller.
///
/// # Errors
/// Propagates [`otter_gc::OutOfMemory`].
pub fn alloc_value_slab(
    heap: &mut otter_gc::GcHeap,
    capacity: usize,
    external_visit: &mut otter_gc::heap::RootSlotVisitor<'_>,
) -> Result<ValueSlabHandle, otter_gc::OutOfMemory> {
    heap.alloc_variable_with_roots(
        ValueSlabBody::new(capacity),
        ValueSlabBody::trailing_bytes(capacity),
        external_visit,
    )
}

/// Allocate a slab holding exactly `values`, in order.
///
/// The pending values are rooted across the allocation — they are
/// ordinary `Value`s on the caller's stack, and a collection here would
/// otherwise leave the copies naming pre-move objects. The copy happens
/// behind the mutator's back, so every old→young edge it creates is
/// remembered against the slab before this returns.
///
/// # Errors
/// Propagates [`otter_gc::OutOfMemory`].
pub fn slab_from_values(
    heap: &mut otter_gc::GcHeap,
    values: &mut [Value],
    external_visit: &mut otter_gc::heap::RootSlotVisitor<'_>,
) -> Result<ValueSlabHandle, otter_gc::OutOfMemory> {
    if values.is_empty() {
        return Ok(ValueSlabHandle::null());
    }
    let count = values.len();
    let base = values.as_mut_ptr();
    let mut visit = |visitor: &mut dyn FnMut(*mut otter_gc::raw::RawGc)| {
        external_visit(visitor);
        for index in 0..count {
            // SAFETY: `index < count` and the caller's slice outlives
            // this call, so the entry is a live slot to rewrite.
            let value = unsafe { &mut *base.add(index) };
            value.trace_value_slot_mut(visitor);
        }
    };
    let slab = alloc_value_slab(heap, count, &mut visit)?;
    let slab_base = values_base(slab);
    for index in 0..count {
        // SAFETY: `index < count = capacity`; source and destination do
        // not overlap.
        unsafe { *slab_base.add(index) = *base.add(index) };
    }
    // SAFETY: the handle names the slab just allocated.
    unsafe { (*body_of(slab).expect("fresh slab")).set_len(count) };
    for index in 0..count {
        // SAFETY: `index < count`.
        let value = unsafe { *base.add(index) };
        heap.record_write(slab, &value);
    }
    Ok(slab)
}

/// The initialised values of `slab` as a slice, empty for a null handle.
///
/// # Safety
/// The caller must keep the slab reachable for `'a` — in practice, the
/// owner naming it must be rooted for the duration.
pub unsafe fn live_slice<'a>(slab: ValueSlabHandle) -> &'a [Value] {
    let Some(body) = body_of(slab) else {
        return &[];
    };
    // SAFETY: caller keeps the slab live; the first `len` values were
    // initialised before the slab became reachable.
    unsafe { std::slice::from_raw_parts((*body).values_ptr().cast_const(), (*body).len()) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Interpreter;

    #[test]
    fn a_fresh_slab_is_empty_and_sized() {
        let mut interp = Interpreter::new();
        let slab = alloc_value_slab(interp.gc_heap_mut(), 8, &mut |_| {}).expect("slab");
        assert_eq!(capacity_of(slab), 8);
        assert_eq!(interp.gc_heap().read_payload(slab, ValueSlabBody::len), 0);
    }

    #[test]
    fn values_round_trip_through_the_trailing_array() {
        let mut interp = Interpreter::new();
        let slab = alloc_value_slab(interp.gc_heap_mut(), 4, &mut |_| {}).expect("slab");
        let base = values_base(slab);
        // SAFETY: the slab is live and has room for four values.
        unsafe { (*body_of(slab).expect("slab body")).set_len(4) };
        for index in 0..4usize {
            // SAFETY: index is within the slab's capacity.
            unsafe {
                *base.add(index) = Value::number(crate::number::NumberValue::Double(index as f64))
            };
        }
        for index in 0..4usize {
            // SAFETY: index is within the slab's capacity.
            let value = unsafe { *base.add(index) };
            assert_eq!(value.as_number().map(|n| n.as_f64()), Some(index as f64));
        }
    }
}
