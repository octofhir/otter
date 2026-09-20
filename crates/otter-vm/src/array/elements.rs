//! GC-owned dense storage for ordinary JavaScript arrays.
//!
//! Numeric arrays keep IEEE-754 payloads unboxed. A side bitmap distinguishes
//! holes from every possible double bit pattern, including NaN and negative
//! zero. Storage widens monotonically to tagged `Value` words when a
//! non-number is written; it never narrows again.
//!
//! # Contents
//! - [`DenseElementKind`] — the JIT-visible storage classification.
//! - [`ElementSlabBody`] — fixed header followed by element words and,
//!   for numeric slabs, a hole bitmap.
//! - Root-aware allocation and raw-handle accessors used by `array`.
//!
//! # Invariants
//! - Every element word is eight bytes for every kind, so numeric-to-tagged
//!   widening is allocation-free and preserves the cached element base.
//! - `PackedDouble` has no holes in the live prefix. `HoleyDouble` uses the
//!   bitmap; no floating-point payload is reserved as a sentinel.
//! - Only `Tagged` slabs trace their live prefix. The kind is published only
//!   after every converted `Value` word has been initialized.
//! - Full tracing visits the complete tagged prefix. Minor remembered-parent
//!   tracing visits a conservative dirty interval and retains any slots whose
//!   targets remain young after relocation. Every write that can introduce or
//!   move a GC edge updates the interval; generated primitive stores need not.
//! - Slabs live in old space and never resize. Growth allocates a replacement
//!   and the owning array republishes its cached base.
//!
//! # See also
//! - [`crate::array::ArrayBody`]
//! - [`crate::value_slab`] — tagged-only storage used by native captures.

use crate::Value;
use otter_gc::heap::RootSlotVisitor;
use otter_gc::raw::SlotVisitor;

#[cfg(test)]
mod remembered_tests;

/// Reserved GC type tag for ordinary-array element slabs.
pub(crate) const ELEMENT_SLAB_BODY_TYPE_TAG: u8 = 0x3f;

/// Physical representation of an ordinary array's dense prefix.
///
/// The discriminants are part of the native JIT contract and are therefore
/// explicit. `Empty` exists only in the array body's cache; a materialized
/// slab always carries one of the other three kinds.
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DenseElementKind {
    /// No dense slab has been allocated.
    Empty = 0,
    /// Every live element is an unboxed IEEE-754 double.
    PackedDouble = 1,
    /// Live elements are doubles or holes selected by the side bitmap.
    HoleyDouble = 2,
    /// Every live word is a tagged [`Value`], including the hole sentinel.
    Tagged = 3,
}

impl DenseElementKind {
    #[inline]
    pub(crate) const fn from_raw(raw: u8) -> Option<Self> {
        match raw {
            0 => Some(Self::Empty),
            1 => Some(Self::PackedDouble),
            2 => Some(Self::HoleyDouble),
            3 => Some(Self::Tagged),
            _ => None,
        }
    }

    #[inline]
    pub(crate) const fn is_numeric(self) -> bool {
        matches!(self, Self::PackedDouble | Self::HoleyDouble)
    }
}

/// Handle to one old-space element slab.
pub(crate) type ElementSlabHandle = otter_gc::Gc<ElementSlabBody>;

/// Header followed by `capacity` eight-byte element words and, for numeric
/// kinds, `ceil(capacity / 64)` bitmap words.
#[repr(C, align(8))]
pub(crate) struct ElementSlabBody {
    capacity: u32,
    len: u32,
    hole_count: u32,
    kind: DenseElementKind,
    reserved: [u8; 3],
    /// Conservative interval of writes not yet proven free of young edges.
    /// Empty is represented by `dirty_start >= dirty_end`. Keeping indices,
    /// rather than slot addresses, also makes copied heap images self-contained.
    dirty_start: u32,
    dirty_end: u32,
}

const _: () = assert!(std::mem::size_of::<ElementSlabBody>() == 24);
const _: () = assert!(std::mem::align_of::<ElementSlabBody>() == 8);

impl ElementSlabBody {
    #[must_use]
    pub(crate) fn new(capacity: usize, kind: DenseElementKind) -> Self {
        assert!(
            kind != DenseElementKind::Empty,
            "materialized slab cannot be empty"
        );
        Self {
            capacity: u32::try_from(capacity).expect("array element capacity exceeds u32"),
            len: 0,
            hole_count: 0,
            kind,
            reserved: [0; 3],
            dirty_start: u32::MAX,
            dirty_end: 0,
        }
    }

    #[must_use]
    pub(crate) fn trailing_bytes(capacity: usize, kind: DenseElementKind) -> usize {
        let data = capacity
            .checked_mul(std::mem::size_of::<u64>())
            .expect("array element storage size overflow");
        if kind == DenseElementKind::Tagged {
            data
        } else {
            data.checked_add(Self::bitmap_words_for(capacity) * std::mem::size_of::<u64>())
                .expect("array hole bitmap size overflow")
        }
    }

    #[inline]
    #[must_use]
    pub(crate) const fn capacity(&self) -> usize {
        self.capacity as usize
    }

    #[inline]
    #[must_use]
    pub(crate) const fn len(&self) -> usize {
        self.len as usize
    }

    #[inline]
    #[must_use]
    pub(crate) const fn kind(&self) -> DenseElementKind {
        self.kind
    }

    #[inline]
    #[must_use]
    pub(crate) fn data_ptr(&self) -> *mut u8 {
        // SAFETY: allocation reserves the trailing storage immediately after
        // this fixed, eight-byte-aligned header.
        unsafe { (self as *const Self as *mut u8).add(std::mem::size_of::<Self>()) }
    }

    #[inline]
    fn values_ptr(&self) -> *mut Value {
        self.data_ptr().cast()
    }

    pub(crate) fn visit_function_ids(&self, visitor: &mut dyn FnMut(u32)) {
        if self.kind != DenseElementKind::Tagged {
            return;
        }
        for index in 0..self.len() {
            // SAFETY: tagged values in the live prefix are initialized before
            // `len` is published.
            crate::code_liveness::visit_value(unsafe { &*self.values_ptr().add(index) }, visitor);
        }
    }

    #[inline]
    fn doubles_ptr(&self) -> *mut f64 {
        self.data_ptr().cast()
    }

    #[inline]
    fn bitmap_ptr(&self) -> *mut u64 {
        debug_assert!(self.kind.is_numeric());
        // SAFETY: numeric allocation reserves `capacity * 8` data bytes before
        // the bitmap and both regions are eight-byte aligned.
        unsafe { self.data_ptr().add(self.capacity() * 8).cast() }
    }

    #[inline]
    const fn bitmap_words_for(capacity: usize) -> usize {
        capacity.div_ceil(64)
    }

    #[inline]
    fn hole_bit(&self, index: usize) -> bool {
        debug_assert!(self.kind.is_numeric());
        debug_assert!(index < self.capacity());
        let word = index / 64;
        let bit = index % 64;
        // SAFETY: `word < ceil(capacity / 64)`.
        unsafe { (*self.bitmap_ptr().add(word) & (1_u64 << bit)) != 0 }
    }

    #[inline]
    fn set_hole_bit(&mut self, index: usize) {
        debug_assert!(self.kind.is_numeric());
        debug_assert!(index < self.capacity());
        let word = index / 64;
        let mask = 1_u64 << (index % 64);
        // SAFETY: `word < ceil(capacity / 64)`.
        let slot = unsafe { &mut *self.bitmap_ptr().add(word) };
        if *slot & mask == 0 {
            *slot |= mask;
            self.hole_count = self.hole_count.saturating_add(1);
        }
        self.kind = DenseElementKind::HoleyDouble;
    }

    #[inline]
    fn clear_hole_bit(&mut self, index: usize) {
        debug_assert!(self.kind.is_numeric());
        debug_assert!(index < self.capacity());
        let word = index / 64;
        let mask = 1_u64 << (index % 64);
        // SAFETY: `word < ceil(capacity / 64)`.
        let slot = unsafe { &mut *self.bitmap_ptr().add(word) };
        if *slot & mask != 0 {
            *slot &= !mask;
            self.hole_count -= 1;
        }
        if self.hole_count == 0 {
            self.kind = DenseElementKind::PackedDouble;
        }
    }

    /// Read one initialized slot in its ordinary tagged form.
    #[inline]
    #[must_use]
    pub(crate) fn get(&self, index: usize) -> Option<Value> {
        if index >= self.len() {
            return None;
        }
        Some(match self.kind {
            DenseElementKind::Empty => unreachable!("materialized slab cannot be empty"),
            DenseElementKind::Tagged => {
                // SAFETY: `index < len <= capacity`, and tagged slots in the
                // live prefix are initialized before `len` is published.
                unsafe { *self.values_ptr().add(index) }
            }
            DenseElementKind::PackedDouble => {
                // SAFETY: as above, with a raw `f64` word.
                Value::number(crate::number::NumberValue::from_f64(unsafe {
                    *self.doubles_ptr().add(index)
                }))
            }
            DenseElementKind::HoleyDouble => {
                if self.hole_bit(index) {
                    Value::hole()
                } else {
                    // SAFETY: a clear live bitmap bit names an initialized
                    // double word.
                    Value::number(crate::number::NumberValue::from_f64(unsafe {
                        *self.doubles_ptr().add(index)
                    }))
                }
            }
        })
    }

    /// Write one slot, widening numeric storage to tagged in place when the
    /// value is neither a Number nor the internal hole sentinel.
    pub(crate) fn set(&mut self, index: usize, value: Value) {
        assert!(index < self.capacity(), "dense write exceeds slab capacity");
        if self.kind.is_numeric() {
            if value.is_hole() {
                self.set_hole_bit(index);
                return;
            }
            if let Some(number) = value.as_f64() {
                // SAFETY: `index < capacity`; numeric data words are `f64`.
                unsafe { *self.doubles_ptr().add(index) = number };
                self.clear_hole_bit(index);
                return;
            }
            self.convert_to_tagged();
        }
        debug_assert_eq!(self.kind, DenseElementKind::Tagged);
        // SAFETY: `index < capacity`; tagged data words are `Value`.
        unsafe { *self.values_ptr().add(index) = value };
        self.mark_dirty_range(index, index + 1);
    }

    /// Mark mutations using element indices, without allocating or retaining
    /// host pointers. Appends leave only the newly written suffix dirty;
    /// disjoint writes conservatively include the intervening slots.
    fn mark_dirty_range(&mut self, start: usize, end: usize) {
        debug_assert!(end <= self.capacity());
        if start < end {
            self.dirty_start = self.dirty_start.min(start as u32);
            self.dirty_end = self.dirty_end.max(end as u32);
        }
    }

    /// Publish a fully initialized live prefix.
    pub(crate) fn set_len(&mut self, len: usize) {
        assert!(len <= self.capacity());
        self.len = u32::try_from(len).expect("array dense length exceeds u32");
        if self.kind.is_numeric() && self.hole_count == 0 {
            self.kind = DenseElementKind::PackedDouble;
        }
    }

    /// Remove the suffix `[len, old_len)` and stop tracing/marking it live.
    pub(crate) fn truncate(&mut self, len: usize) {
        let current = self.len();
        if len >= current {
            return;
        }
        match self.kind {
            DenseElementKind::Empty => unreachable!("materialized slab cannot be empty"),
            DenseElementKind::Tagged => {
                for index in len..current {
                    // SAFETY: the removed slot is inside the initialized prefix.
                    unsafe { *self.values_ptr().add(index) = Value::undefined() };
                }
            }
            DenseElementKind::PackedDouble => {}
            DenseElementKind::HoleyDouble => {
                for index in len..current {
                    self.clear_hole_bit(index);
                }
            }
        }
        self.set_len(len);
        self.dirty_end = self.dirty_end.min(len as u32);
        if self.dirty_start >= self.dirty_end {
            self.dirty_start = u32::MAX;
            self.dirty_end = 0;
        }
    }

    /// Convert every live numeric word to a tagged Number/hole word, then
    /// publish the terminal tagged kind. No allocation or safepoint occurs.
    pub(crate) fn convert_to_tagged(&mut self) {
        if self.kind == DenseElementKind::Tagged {
            return;
        }
        debug_assert!(self.kind.is_numeric());
        let was_holey = self.kind == DenseElementKind::HoleyDouble;
        for index in 0..self.len() {
            let value = if was_holey && self.hole_bit(index) {
                Value::hole()
            } else {
                // SAFETY: every non-hole live numeric word is initialized.
                Value::number(crate::number::NumberValue::from_f64(unsafe {
                    *self.doubles_ptr().add(index)
                }))
            };
            // SAFETY: every representation uses the same eight-byte data word.
            unsafe { *self.values_ptr().add(index) = value };
        }
        self.hole_count = 0;
        // Publication is last: the tracer must never see Tagged while any live
        // word still contains raw double bits.
        self.kind = DenseElementKind::Tagged;
    }

    #[must_use]
    pub(crate) fn values_vec(&self) -> Vec<Value> {
        (0..self.len())
            .map(|index| self.get(index).expect("live dense slot"))
            .collect()
    }

    #[must_use]
    pub(crate) fn tagged_slice(&self) -> Option<&[Value]> {
        (self.kind == DenseElementKind::Tagged).then(|| {
            // SAFETY: the complete tagged live prefix is initialized.
            unsafe { std::slice::from_raw_parts(self.values_ptr().cast_const(), self.len()) }
        })
    }

    #[must_use]
    pub(crate) fn tagged_slice_mut(&mut self) -> Option<&mut [Value]> {
        if self.kind != DenseElementKind::Tagged {
            return None;
        }
        // A slice caller can rewrite any slot, including by swapping existing
        // young edges. Publish the complete interval before exposing it.
        self.mark_dirty_range(0, self.len());
        // SAFETY: the complete tagged live prefix is initialized and the
        // exclusive body borrow rules out another array mutation.
        Some(unsafe { std::slice::from_raw_parts_mut(self.values_ptr(), self.len()) })
    }
}

impl otter_gc::SafeTraceable for ElementSlabBody {
    const TYPE_TAG: u8 = ELEMENT_SLAB_BODY_TYPE_TAG;

    fn trace_slots_safe(&mut self, visitor: &mut SlotVisitor<'_>) {
        if self.kind != DenseElementKind::Tagged {
            return;
        }
        for index in 0..self.len() {
            // SAFETY: the tagged live prefix is initialized and lives in this
            // GC cell, so the collector may rewrite each slot in place.
            unsafe { (&mut *self.values_ptr().add(index)).trace_value_slot_mut(visitor) };
        }
    }

    fn trace_pending_slots_safe(&mut self, _visitor: &mut SlotVisitor<'_>) {
        // A stack-resident header has no trailing storage. Callers root pending
        // source values explicitly until they have been copied into the slab.
    }

    fn trace_remembered_slots_safe(&mut self, visitor: &mut SlotVisitor<'_>) {
        let start = std::mem::replace(&mut self.dirty_start, u32::MAX).min(self.len);
        let end = std::mem::replace(&mut self.dirty_end, 0).min(self.len);
        if self.kind != DenseElementKind::Tagged {
            return;
        }
        for index in start..end {
            // SAFETY: the interval is clipped to the initialized tagged prefix.
            let value = unsafe { &mut *self.values_ptr().add(index as usize) };
            value.trace_value_slot_mut(visitor);
            // The root walk precedes remembered parents. A child it already
            // copied to young to-space will stay young on this visit, so its
            // slot must remain dirty even though this pass rewrote it.
            if let Some(raw) = value.as_raw_gc()
                // SAFETY: the visitor has returned a current, live cell offset.
                && unsafe { (*raw.as_header_ptr()).is_young() }
            {
                self.mark_dirty_range(index as usize, index as usize + 1);
            }
        }
    }
}

/// Decode a live slab handle without borrowing the heap.
#[must_use]
pub(crate) fn body_of(slab: ElementSlabHandle) -> Option<*mut ElementSlabBody> {
    if slab.is_null() {
        return None;
    }
    let header = slab.as_header_ptr();
    // SAFETY: a non-null handle names a live slab payload one GC header past
    // the cell start.
    Some(unsafe {
        header
            .cast::<u8>()
            .add(std::mem::size_of::<otter_gc::GcHeader>())
            .cast::<ElementSlabBody>()
    })
}

#[must_use]
pub(crate) fn data_base(slab: ElementSlabHandle) -> *mut u8 {
    body_of(slab).map_or(std::ptr::null_mut(), |body| {
        // SAFETY: `body_of` returned a live payload.
        unsafe { (*body).data_ptr() }
    })
}

#[must_use]
pub(crate) fn capacity_of(slab: ElementSlabHandle) -> usize {
    body_of(slab).map_or(0, |body| {
        // SAFETY: `body_of` returned a live payload.
        unsafe { (*body).capacity() }
    })
}

#[must_use]
#[cfg(any(debug_assertions, test))]
pub(crate) fn len_of(slab: ElementSlabHandle) -> usize {
    body_of(slab).map_or(0, |body| {
        // SAFETY: `body_of` returned a live payload.
        unsafe { (*body).len() }
    })
}

#[must_use]
pub(crate) fn kind_of(slab: ElementSlabHandle) -> DenseElementKind {
    body_of(slab).map_or(DenseElementKind::Empty, |body| {
        // SAFETY: `body_of` returned a live payload.
        unsafe { (*body).kind() }
    })
}

/// Allocate an empty old-space slab. The caller initializes words and then
/// publishes its live length without another allocation.
pub(crate) fn alloc_element_slab(
    heap: &mut otter_gc::GcHeap,
    capacity: usize,
    kind: DenseElementKind,
    external_visit: &mut RootSlotVisitor<'_>,
) -> Result<ElementSlabHandle, otter_gc::OutOfMemory> {
    debug_assert!(capacity != 0);
    let slab = heap.alloc_variable_with_roots(
        ElementSlabBody::new(capacity, kind),
        ElementSlabBody::trailing_bytes(capacity, kind),
        external_visit,
    )?;
    if kind.is_numeric() {
        let body = body_of(slab).expect("fresh element slab");
        // Do not make bitmap initialization depend on allocator zero-fill:
        // read-modify-write hole updates require every word to be known zero.
        // SAFETY: the fresh slab is not yet reachable except through this
        // handle, and its numeric tail reserves exactly this many words.
        unsafe {
            std::ptr::write_bytes(
                (*body).bitmap_ptr(),
                0,
                ElementSlabBody::bitmap_words_for(capacity),
            )
        };
    }
    Ok(slab)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Interpreter;

    #[test]
    fn holes_do_not_alias_nan_or_negative_zero() {
        let mut interp = Interpreter::new();
        let slab = alloc_element_slab(
            interp.gc_heap_mut(),
            4,
            DenseElementKind::HoleyDouble,
            &mut |_| {},
        )
        .expect("slab");
        let body = body_of(slab).expect("body");
        // SAFETY: fresh slab is exclusively owned by this test.
        unsafe {
            (*body).set(0, Value::number_f64(f64::NAN));
            (*body).set(1, Value::number_f64(-0.0));
            (*body).set(2, Value::hole());
            (*body).set(3, Value::number_i32(7));
            (*body).set_len(4);
            assert!((*body).get(0).expect("NaN").as_f64().unwrap().is_nan());
            assert!(
                (*body)
                    .get(1)
                    .expect("-0")
                    .as_f64()
                    .unwrap()
                    .is_sign_negative()
            );
            assert!((*body).get(2).expect("hole").is_hole());
            assert_eq!((*body).get(3).expect("seven").as_f64(), Some(7.0));
            assert_eq!((*body).kind(), DenseElementKind::HoleyDouble);
        }
    }

    #[test]
    fn filling_last_hole_promotes_to_packed_and_non_number_widens() {
        let mut interp = Interpreter::new();
        let slab = alloc_element_slab(
            interp.gc_heap_mut(),
            2,
            DenseElementKind::HoleyDouble,
            &mut |_| {},
        )
        .expect("slab");
        let body = body_of(slab).expect("body");
        // SAFETY: fresh slab is exclusively owned by this test.
        unsafe {
            (*body).set(0, Value::hole());
            (*body).set(1, Value::number_i32(2));
            (*body).set_len(2);
            assert_eq!((*body).kind(), DenseElementKind::HoleyDouble);
            (*body).set(0, Value::number_i32(1));
            assert_eq!((*body).kind(), DenseElementKind::PackedDouble);
            (*body).set(1, Value::boolean(true));
            assert_eq!((*body).kind(), DenseElementKind::Tagged);
            assert_eq!((*body).get(0).and_then(Value::as_f64), Some(1.0));
            assert_eq!((*body).get(1), Some(Value::boolean(true)));
        }
    }

    #[test]
    fn widened_slab_traces_a_cell_through_full_collection() {
        let mut interp = Interpreter::new();
        let array = interp
            .array_from_elements_host_rooted([1.25, 2.5, 3.75].map(Value::number_f64), &[], &[])
            .expect("numeric array");
        let root = interp.persistent_root_insert(Value::array(array));
        let mut child = interp
            .alloc_host_object_with_roots(&[], &[])
            .expect("child object");
        crate::object::set(
            &mut child,
            interp.gc_heap_mut(),
            "marker",
            Value::number_i32(73),
        );

        let array = interp
            .persistent_root_get(root)
            .and_then(Value::as_array)
            .expect("rooted array");
        crate::array::set(array, interp.gc_heap_mut(), 1, Value::object(child))
            .expect("widen array");
        assert_eq!(
            crate::array::dense_element_kind(array, interp.gc_heap()),
            DenseElementKind::Tagged
        );

        interp.force_gc().expect("full collection");
        let array = interp
            .persistent_root_get(root)
            .and_then(Value::as_array)
            .expect("rooted array after collection");
        let child = crate::array::get(array, interp.gc_heap(), 1)
            .as_object()
            .expect("traced child");
        assert_eq!(
            crate::object::get(child, interp.gc_heap(), "marker").and_then(Value::as_i32),
            Some(73)
        );
    }
}
