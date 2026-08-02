//! A closure's captured upvalue spine, allocated inside the GC heap.
//!
//! A closure captures a fixed number of [`UpvalueCell`] handles, decided
//! when the closure is created and never changed. Where that array lives
//! decides whether the closure is self-contained. A `Vec` would put it in
//! malloc memory the collector does not own: the closure could not be
//! captured into a page image, a restored copy would alias the original
//! buffer, and the tracer would hand the collector slot addresses outside
//! the heap.
//!
//! So the spine is a GC body with its handles in trailing storage in the
//! same cell — V8's `Context`, reached from a `JSFunction` by handle, for
//! the same reason.
//!
//! # Contents
//!
//! - [`UpvalueSpineBody`] — count header followed by its cells.
//! - [`UpvalueSpineHandle`] — handle type stored by a closure body.
//! - [`alloc_upvalue_spine`] — allocate one from a caller's cells.
//!
//! # Invariants
//!
//! - The trailing array holds exactly `count` cells and is fully written
//!   by [`alloc_upvalue_spine`] before the handle is returned.
//! - A spine never resizes. That is what lets
//!   [`crate::closure::ClosureCallHeader::upvalue_base`] publish its
//!   address to compiled code for the closure's lifetime.
//! - Old space: a spine lives exactly as long as the closure that owns
//!   it, so a semispace copy of one is pure overhead — and old-space
//!   bodies do not move, which is what keeps the published base valid
//!   across a scavenge. A restore does move them, and the closure's
//!   tracer recomputes the base from the relocated handle.
//! - Cells are traced in place, so the collector rewrites the live entry
//!   rather than a copy.
//!
//! # See also
//!
//! - [`crate::closure`] — the owner, and where the base is republished.
//! - [`crate::object::slot_slab`] — the same shape for property storage.

use otter_gc::raw::{RawGc, SlotVisitor};

use crate::UpvalueCell;

/// Reserved [`otter_gc::Traceable::TYPE_TAG`] for [`UpvalueSpineBody`].
pub const UPVALUE_SPINE_BODY_TYPE_TAG: u8 = 0x32;

/// Handle to a closure's captured upvalue spine.
pub type UpvalueSpineHandle = otter_gc::Gc<UpvalueSpineBody>;

/// Count header for a captured upvalue spine. The cells follow it in the
/// same cell.
#[repr(C)]
pub struct UpvalueSpineBody {
    /// Cells the trailing array holds.
    count: u32,
}

impl UpvalueSpineBody {
    /// Trailing bytes a spine of `count` cells needs.
    #[must_use]
    pub fn trailing_bytes(count: usize) -> usize {
        count * std::mem::size_of::<UpvalueCell>()
    }

    /// Header for a spine of `count` cells.
    #[must_use]
    pub fn new(count: usize) -> Self {
        Self {
            count: u32::try_from(count).expect("closure upvalue spine exceeds the u32 call ABI"),
        }
    }

    /// Cells this spine holds.
    #[must_use]
    pub fn count(&self) -> usize {
        self.count as usize
    }

    /// Base of the trailing cell array.
    ///
    /// The closure's call header publishes this to compiled code as
    /// `upvalue_base`.
    #[must_use]
    pub fn cells_ptr(&self) -> *mut UpvalueCell {
        // SAFETY: the allocation reserved `trailing_bytes(count)`
        // immediately after this header, so the array starts one `Self`
        // past `self` and runs for `count` cells.
        unsafe {
            (self as *const Self as *mut u8)
                .add(std::mem::size_of::<Self>())
                .cast()
        }
    }

    /// The captured cells.
    #[must_use]
    pub fn cells(&self) -> &[UpvalueCell] {
        // SAFETY: the trailing array holds exactly `count` initialized
        // cells, written before the spine handle was published.
        unsafe { std::slice::from_raw_parts(self.cells_ptr(), self.count()) }
    }
}

/// Process address of `spine`'s first cell, or zero for a null handle.
///
/// This is what [`crate::closure::ClosureCallHeader::upvalue_base`]
/// publishes to compiled code, and the one place that decodes a spine
/// handle without going through the heap — the closure's tracer needs it
/// while it already holds the body.
#[must_use]
pub fn cells_base_address(spine: UpvalueSpineHandle) -> u64 {
    if spine.is_null() {
        return 0;
    }
    let header = spine.as_header_ptr();
    // SAFETY: a non-null handle names a live cell whose payload is an
    // `UpvalueSpineBody` one header past the start; `cells_ptr` only does
    // address arithmetic on it.
    unsafe {
        let body = header
            .cast::<u8>()
            .add(std::mem::size_of::<otter_gc::GcHeader>())
            .cast::<UpvalueSpineBody>();
        (*body).cells_ptr() as usize as u64
    }
}

impl otter_gc::SafeTraceable for UpvalueSpineBody {
    const TYPE_TAG: u8 = UPVALUE_SPINE_BODY_TYPE_TAG;

    fn trace_slots_safe(&mut self, v: &mut SlotVisitor<'_>) {
        let base = self.cells_ptr();
        for index in 0..self.count() {
            // SAFETY: `index < count`, and the array is live for the
            // body's lifetime. An `UpvalueCell` is a bare compressed
            // handle, so the entry is itself a slot the collector
            // rewrites in place.
            let cell = unsafe { base.add(index) };
            // SAFETY: same in-range entry, read only to skip nulls.
            if unsafe { (*cell).is_null() } {
                continue;
            }
            v(cell.cast::<RawGc>());
        }
    }
}

/// Allocate a spine holding `cells`.
///
/// `cells` is taken mutably and rooted across the allocation: a
/// collection here would otherwise move the cells out from under the
/// caller's copies. Every entry is written into the spine after the
/// allocation, so the spine receives the post-collection handles.
///
/// `external_visit` must yield every other root the caller holds.
///
/// # Errors
/// Propagates [`otter_gc::OutOfMemory`].
pub fn alloc_upvalue_spine(
    heap: &mut otter_gc::GcHeap,
    cells: &mut [UpvalueCell],
    external_visit: &mut otter_gc::heap::RootSlotVisitor<'_>,
) -> Result<UpvalueSpineHandle, otter_gc::OutOfMemory> {
    let count = cells.len();
    let cells_base = cells.as_mut_ptr();
    let mut visit = |visitor: &mut dyn FnMut(*mut RawGc)| {
        external_visit(visitor);
        for index in 0..count {
            // SAFETY: `index < count` and the caller's slice outlives
            // this call, so the entry is a live slot to rewrite.
            let cell = unsafe { cells_base.add(index) };
            // SAFETY: same in-range entry.
            if unsafe { (*cell).is_null() } {
                continue;
            }
            visitor(cell.cast::<RawGc>());
        }
    };
    let spine = heap.alloc_variable_with_roots(
        UpvalueSpineBody::new(count),
        UpvalueSpineBody::trailing_bytes(count),
        &mut visit,
    )?;
    heap.with_payload(spine, |body| {
        let base = body.cells_ptr();
        for (index, cell) in cells.iter().enumerate() {
            // SAFETY: `index < count`, the capacity the spine was sized
            // for.
            unsafe { *base.add(index) = *cell };
        }
        true
    });
    // The cells may be younger than the old-space spine that now names
    // them, so record the edge the mutator barrier would have.
    for cell in cells.iter() {
        if !cell.is_null() {
            heap.record_write(spine, cell);
        }
    }
    Ok(spine)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Value, alloc_upvalue};

    #[test]
    fn a_spine_holds_the_cells_it_was_built_from() {
        let mut heap = otter_gc::GcHeap::new().expect("heap");
        let a = alloc_upvalue(&mut heap, Value::undefined()).expect("cell a");
        let b = alloc_upvalue(&mut heap, Value::null()).expect("cell b");
        let mut cells = [a, b];
        let spine = alloc_upvalue_spine(&mut heap, &mut cells, &mut |_| {}).expect("spine");
        let stored = heap.read_payload(spine, |body| body.cells().to_vec());
        assert_eq!(stored, vec![a, b]);
    }

    #[test]
    fn an_empty_spine_is_still_a_body_with_no_cells() {
        let mut heap = otter_gc::GcHeap::new().expect("heap");
        let spine = alloc_upvalue_spine(&mut heap, &mut [], &mut |_| {}).expect("spine");
        assert_eq!(heap.read_payload(spine, UpvalueSpineBody::count), 0);
        assert!(heap.read_payload(spine, |body| body.cells().is_empty()));
    }

    #[test]
    fn the_published_base_addresses_the_trailing_array() {
        let mut heap = otter_gc::GcHeap::new().expect("heap");
        let cell = alloc_upvalue(&mut heap, Value::undefined()).expect("cell");
        let mut cells = [cell];
        let spine = alloc_upvalue_spine(&mut heap, &mut cells, &mut |_| {}).expect("spine");
        let (base, first) = heap.read_payload(spine, |body| {
            // SAFETY: the spine holds one cell.
            (body.cells_ptr(), unsafe { *body.cells_ptr() })
        });
        assert!(!base.is_null());
        assert_eq!(first, cell);
    }
}
