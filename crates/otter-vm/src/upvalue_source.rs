//! Allocation-neutral views over stable upvalue-cell spines.
//!
//! # Contents
//! - [`UpvalueSource`] — a compact pointer/count descriptor with short raw-slot
//!   operations for tracing, copying, and native-frame publication.
//!
//! # Invariants
//! - A source points only at initialized [`crate::UpvalueCell`] entries whose
//!   backing allocation never moves or resizes for the source's live extent.
//! - The owner of a borrowed source keeps that backing allocation alive. For a
//!   closure spine this means rooting the exact closure value; for an owned
//!   frame spine this means retaining its box.
//! - No Rust slice is retained across a collection. Tracing and copying use
//!   short raw-slot operations so the moving collector may rewrite cells in
//!   place without violating a live shared borrow.
//!
//! # See also
//! - [`crate::closure::ClosureCallHeader`] publishes the stable closure spine.
//! - [`crate::native_abi::NativeFrame`] publishes the active upvalue window.

use std::ptr::NonNull;

use otter_gc::raw::RawGc;

use crate::{UpvalueCell, VmError, frame_state::UpvalueSpine};

/// Pointer/count view of one initialized, allocation-stable upvalue spine.
///
/// The descriptor is intentionally lifetime-free because it crosses the VM ↔
/// native staging boundary. Construction is unsafe; safe operations rely on
/// the constructor's backing-lifetime guarantee.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct UpvalueSource {
    base: NonNull<UpvalueCell>,
    len: u32,
}

impl UpvalueSource {
    /// Empty source; it owns no allocation and publishes a null ABI base.
    #[must_use]
    pub(crate) const fn empty() -> Self {
        Self {
            base: NonNull::dangling(),
            len: 0,
        }
    }

    /// Describe one stable initialized allocation.
    ///
    /// # Safety
    /// When `len != 0`, `base` must remain valid for `len` initialized cells
    /// and must not be resized or freed until every copy of the returned source
    /// is dead. The backing owner must remain GC-reachable whenever collection
    /// can occur.
    pub(crate) unsafe fn from_raw_parts(base: *mut UpvalueCell, len: u32) -> Result<Self, VmError> {
        if len == 0 {
            return Ok(Self::empty());
        }
        let base = NonNull::new(base).ok_or(VmError::InvalidOperand)?;
        Ok(Self { base, len })
    }

    /// Describe a stable Rust-owned slice.
    ///
    /// # Safety
    /// The slice's backing allocation must satisfy the lifetime/stability
    /// contract of [`Self::from_raw_parts`].
    pub(crate) unsafe fn from_stable_slice(cells: &[UpvalueCell]) -> Result<Self, VmError> {
        let len = u32::try_from(cells.len()).map_err(|_| VmError::InvalidOperand)?;
        // SAFETY: forwarded from this function's contract. `as_ptr` is
        // non-null for a non-empty slice; the empty case is canonicalized.
        unsafe { Self::from_raw_parts(cells.as_ptr().cast_mut(), len) }
    }

    /// Number of initialized cells.
    #[inline]
    #[must_use]
    pub(crate) const fn len(self) -> usize {
        self.len as usize
    }

    /// Whether the source is empty.
    #[inline]
    #[must_use]
    pub(crate) const fn is_empty(self) -> bool {
        self.len == 0
    }

    /// Stable base pointer, null for an empty source.
    #[cfg(test)]
    #[inline]
    #[must_use]
    pub(crate) const fn base_ptr_or_null(self) -> *mut UpvalueCell {
        if self.len == 0 {
            std::ptr::null_mut()
        } else {
            self.base.as_ptr()
        }
    }

    /// Read one current handle without creating a long-lived slice.
    pub(crate) fn read(self, index: usize) -> Option<UpvalueCell> {
        if index >= self.len() {
            return None;
        }
        // SAFETY: construction guarantees an initialized stable allocation and
        // the bounds check limits this operation to one cell.
        Some(unsafe { *self.base.as_ptr().add(index) })
    }

    /// Visit every exact source slot so a moving collection can rewrite it.
    pub(crate) fn trace_slots(self, visitor: &mut dyn FnMut(*mut RawGc)) {
        for index in 0..self.len() {
            // SAFETY: construction guarantees initialized stable slots. No
            // Rust reference/slice exists while the visitor mutates the cell.
            let slot = unsafe { self.base.as_ptr().add(index) };
            visitor(slot.cast::<RawGc>());
        }
    }

    /// Copy the current handles into an existing owned destination.
    pub(crate) fn copy_into(self, destination: &mut [UpvalueCell]) -> Result<(), VmError> {
        if destination.len() != self.len() {
            return Err(VmError::InvalidOperand);
        }
        for (index, destination) in destination.iter_mut().enumerate() {
            *destination = self.read(index).ok_or(VmError::InvalidOperand)?;
        }
        Ok(())
    }

    /// Materialize one owned spine. This performs one host allocation and no
    /// GC allocation; callers use it only when an interpreter frame is needed.
    #[must_use]
    pub(crate) fn copy_owned(self) -> UpvalueSpine {
        let mut cells = Vec::with_capacity(self.len());
        for index in 0..self.len() {
            // Bounds are established by the loop.
            cells.push(self.read(index).expect("upvalue source index in bounds"));
        }
        cells.into_boxed_slice()
    }
}

const _: [(); 16] = [(); std::mem::size_of::<UpvalueSource>()];
const _: [(); 8] = [(); std::mem::align_of::<UpvalueSource>()];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stable_source_copies_without_retaining_a_slice() {
        let mut heap = otter_gc::GcHeap::new().unwrap();
        let cells = vec![
            crate::alloc_upvalue(&mut heap, crate::Value::undefined()).unwrap(),
            crate::alloc_upvalue(&mut heap, crate::Value::null()).unwrap(),
        ];
        // SAFETY: `cells` stays allocated and unmodified for the source's use.
        let source = unsafe { UpvalueSource::from_stable_slice(&cells) }.unwrap();
        assert_eq!(source.len(), 2);
        assert_eq!(source.base_ptr_or_null(), cells.as_ptr().cast_mut());
        assert_eq!(source.copy_owned().as_ref(), cells.as_slice());
    }

    #[test]
    fn empty_source_has_null_native_base() {
        let source = UpvalueSource::empty();
        assert!(source.is_empty());
        assert!(source.base_ptr_or_null().is_null());
        assert!(source.copy_owned().is_empty());
    }

    #[test]
    fn raw_slot_trace_relocates_values_reachable_through_borrowed_cells() {
        let mut heap = otter_gc::GcHeap::new().unwrap();
        let cell = crate::alloc_upvalue(&mut heap, crate::Value::undefined()).unwrap();
        let mut no_roots = |_visitor: &mut dyn FnMut(*mut RawGc)| {};
        let object = crate::object::alloc_object_with_roots(&mut heap, &mut no_roots).unwrap();
        crate::store_upvalue(&mut heap, cell, crate::Value::object(object));
        let before = crate::read_upvalue(&heap, cell)
            .as_raw_gc()
            .expect("young object handle");
        let cells = [cell];
        // SAFETY: the array remains initialized and live through collection.
        let source = unsafe { UpvalueSource::from_stable_slice(&cells) }.unwrap();

        heap.collect_minor_with_roots(&mut |visitor| source.trace_slots(visitor))
            .unwrap();

        let after = crate::read_upvalue(&heap, source.read(0).unwrap())
            .as_raw_gc()
            .expect("relocated object handle");
        assert_ne!(before, after);
    }
}
