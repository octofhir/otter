//! Write barriers for generational and incremental GC.
//!
//! Every pointer store in the heap must go through a write barrier to maintain
//! two invariants:
//!
//! 1. **Generational invariant**: The remembered set tracks all old→young
//!    pointers so the young-gen scavenger doesn't miss live young objects
//!    reachable only from old space.
//!
//! 2. **Incremental marking invariant** (Dijkstra insertion barrier): During
//!    incremental marking, if a black object stores a pointer to a white
//!    object, the white object must be shaded gray. Otherwise the marker
//!    (which already finished scanning the black object) would miss it.
//!
//! # Usage
//!
//! The interpreter calls [`WriteBarrier::record`] after every pointer store
//! (`SetProperty`, `SetUpvalue`, `SetIndex`, array element write, etc.).
//! The barrier is a no-op when:
//! - The source is young (young→anything needs no barrier — scavenger scans all young).
//! - Incremental marking is not active AND source is old, target is old.
//!
//! The hot-path check is 2-3 loads + 1-2 branches — ~2ns overhead per store.

use crate::header::GcHeader;

/// Remembered set: tracks old→young pointer slots for the scavenger.
///
/// Stores raw `*mut *const GcHeader` pointers — the slot addresses within
/// old-space objects that contain pointers to young-space objects. During
/// scavenge, these slots are treated as additional roots into from-space.
///
/// V8 uses a per-page two-level bitmap (SlotSet). We start with a flat Vec
/// and will optimize to a page-indexed bitmap if profiling shows contention.
pub struct RememberedSet {
    /// Slot addresses (pointer-to-pointer) that contain old→young refs.
    slots: Vec<*mut *const GcHeader>,
}

impl RememberedSet {
    pub fn new() -> Self {
        Self {
            slots: Vec::with_capacity(256),
        }
    }

    /// Records one old→young slot.
    #[inline]
    pub fn insert(&mut self, slot: *mut *const GcHeader) {
        self.slots.push(slot);
    }

    /// Returns all recorded slots for root scanning during scavenge.
    pub fn slots(&self) -> &[*mut *const GcHeader] {
        &self.slots
    }

    /// Clears all recorded slots (called after scavenge completes).
    pub fn clear(&mut self) {
        self.slots.clear();
    }

    /// Number of recorded slots.
    pub fn len(&self) -> usize {
        self.slots.len()
    }

    /// Whether the set is empty.
    pub fn is_empty(&self) -> bool {
        self.slots.is_empty()
    }
}

impl Default for RememberedSet {
    fn default() -> Self {
        Self::new()
    }
}

// Send: slot pointers are heap addresses, transferable between threads
// when isolate ownership transfers.
unsafe impl Send for RememberedSet {}

/// Combined generational + incremental write barrier.
///
/// Owns the remembered set and holds a flag indicating whether incremental
/// marking is active. The interpreter holds a `&mut WriteBarrier` and calls
/// [`record`] on every pointer store.
pub struct WriteBarrier {
    /// Remembered set for old→young pointers.
    pub remembered_set: RememberedSet,
    /// Whether incremental marking is currently active.
    /// When true, the Dijkstra insertion barrier fires on black→white stores.
    pub marking_active: bool,
    /// Gray worklist push function — called by the insertion barrier to shade
    /// a white target gray. Set by the marking subsystem when marking begins.
    /// `None` when marking is not active.
    gray_push: Option<fn(*mut GcHeader)>,
}

impl WriteBarrier {
    pub fn new() -> Self {
        Self {
            remembered_set: RememberedSet::new(),
            marking_active: false,
            gray_push: None,
        }
    }

    /// Activates the incremental marking barrier with the given gray-push callback.
    pub fn activate_marking_barrier(&mut self, push: fn(*mut GcHeader)) {
        self.marking_active = true;
        self.gray_push = Some(push);
    }

    /// Deactivates the incremental marking barrier.
    pub fn deactivate_marking_barrier(&mut self) {
        self.marking_active = false;
        self.gray_push = None;
    }

    /// Combined write barrier — called after every pointer store.
    ///
    /// `source` is the object being written to (the container).
    /// `slot` is the address of the pointer field that was just written.
    /// `target` is the new value written to the slot (the pointee).
    ///
    /// # Safety
    ///
    /// `source` and `target` must point to valid, live GC objects (or target
    /// may be null, in which case the barrier is a no-op).
    #[inline]
    pub unsafe fn record(
        &mut self,
        source: *const GcHeader,
        slot: *mut *const GcHeader,
        target: *const GcHeader,
    ) {
        if target.is_null() {
            return;
        }

        let source_h = unsafe { &*source };
        let target_h = unsafe { &*target };

        // Generational barrier: old→young ⇒ record in remembered set.
        // Young→anything: no barrier needed (scavenger scans all of young space).
        if !source_h.is_young() && target_h.is_young() {
            self.remembered_set.insert(slot);
        }

        // Incremental marking barrier (Dijkstra insertion):
        // If source is black and target is white, shade target gray.
        if self.marking_active {
            use crate::header::MarkColor;
            if source_h.mark_color() == MarkColor::Black
                && target_h.mark_color() == MarkColor::White
                && let Some(push) = self.gray_push
            {
                push(target as *mut GcHeader);
            }
        }
    }
}

impl Default for WriteBarrier {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::header::{GcHeader, MarkColor};
    use std::cell::RefCell;

    #[test]
    fn generational_barrier_records_old_to_young() {
        let mut barrier = WriteBarrier::new();

        let source = GcHeader::new(0, 16); // Old (default)
        let target = GcHeader::new_young(0, 16); // Young

        let mut slot: *const GcHeader = &target;
        let slot_ptr: *mut *const GcHeader = &mut slot;

        unsafe {
            barrier.record(&source, slot_ptr, &target);
        }

        assert_eq!(barrier.remembered_set.len(), 1);
    }

    #[test]
    fn generational_barrier_skips_young_to_young() {
        let mut barrier = WriteBarrier::new();

        let source = GcHeader::new_young(0, 16);
        let target = GcHeader::new_young(0, 16);

        let mut slot: *const GcHeader = &target;
        let slot_ptr: *mut *const GcHeader = &mut slot;

        unsafe {
            barrier.record(&source, slot_ptr, &target);
        }

        assert!(barrier.remembered_set.is_empty());
    }

    #[test]
    fn generational_barrier_skips_old_to_old() {
        let mut barrier = WriteBarrier::new();

        let source = GcHeader::new(0, 16);
        let target = GcHeader::new(0, 16);

        let mut slot: *const GcHeader = &target;
        let slot_ptr: *mut *const GcHeader = &mut slot;

        unsafe {
            barrier.record(&source, slot_ptr, &target);
        }

        assert!(barrier.remembered_set.is_empty());
    }

    #[test]
    fn generational_barrier_skips_null_target() {
        let mut barrier = WriteBarrier::new();
        let source = GcHeader::new(0, 16);

        let mut slot: *const GcHeader = std::ptr::null();
        let slot_ptr: *mut *const GcHeader = &mut slot;

        unsafe {
            barrier.record(&source, slot_ptr, std::ptr::null());
        }

        assert!(barrier.remembered_set.is_empty());
    }

    // For the incremental barrier test, we use a thread-local to capture pushes
    // since the push callback is a plain function pointer.
    std::thread_local! {
        static GRAY_PUSHES: RefCell<Vec<*mut GcHeader>> = const { RefCell::new(Vec::new()) };
    }

    fn test_gray_push(header: *mut GcHeader) {
        GRAY_PUSHES.with(|v| v.borrow_mut().push(header));
    }

    #[test]
    fn marking_barrier_shades_white_target_gray() {
        let mut barrier = WriteBarrier::new();
        barrier.activate_marking_barrier(test_gray_push);

        let source = GcHeader::new(0, 16);
        source.set_mark_color(MarkColor::Black);

        let target = GcHeader::new(0, 16); // White

        let mut slot: *const GcHeader = &target;
        let slot_ptr: *mut *const GcHeader = &mut slot;

        GRAY_PUSHES.with(|v| v.borrow_mut().clear());
        unsafe {
            barrier.record(&source, slot_ptr, &target);
        }

        GRAY_PUSHES.with(|v| {
            assert_eq!(v.borrow().len(), 1);
        });
    }

    #[test]
    fn marking_barrier_skips_non_black_source() {
        let mut barrier = WriteBarrier::new();
        barrier.activate_marking_barrier(test_gray_push);

        let source = GcHeader::new(0, 16);
        source.set_mark_color(MarkColor::Gray); // Not black

        let target = GcHeader::new(0, 16); // White

        let mut slot: *const GcHeader = &target;
        let slot_ptr: *mut *const GcHeader = &mut slot;

        GRAY_PUSHES.with(|v| v.borrow_mut().clear());
        unsafe {
            barrier.record(&source, slot_ptr, &target);
        }

        GRAY_PUSHES.with(|v| {
            assert!(v.borrow().is_empty());
        });
    }

    #[test]
    fn marking_barrier_skips_already_marked_target() {
        let mut barrier = WriteBarrier::new();
        barrier.activate_marking_barrier(test_gray_push);

        let source = GcHeader::new(0, 16);
        source.set_mark_color(MarkColor::Black);

        let target = GcHeader::new(0, 16);
        target.set_mark_color(MarkColor::Gray); // Already gray

        let mut slot: *const GcHeader = &target;
        let slot_ptr: *mut *const GcHeader = &mut slot;

        GRAY_PUSHES.with(|v| v.borrow_mut().clear());
        unsafe {
            barrier.record(&source, slot_ptr, &target);
        }

        GRAY_PUSHES.with(|v| {
            assert!(v.borrow().is_empty());
        });
    }

    #[test]
    fn marking_barrier_inactive_when_not_marking() {
        let mut barrier = WriteBarrier::new();
        // NOT activated

        let source = GcHeader::new(0, 16);
        source.set_mark_color(MarkColor::Black);

        let target = GcHeader::new(0, 16); // White

        let mut slot: *const GcHeader = &target;
        let slot_ptr: *mut *const GcHeader = &mut slot;

        GRAY_PUSHES.with(|v| v.borrow_mut().clear());
        unsafe {
            barrier.record(&source, slot_ptr, &target);
        }

        GRAY_PUSHES.with(|v| {
            assert!(v.borrow().is_empty());
        });
    }

    #[test]
    fn remembered_set_clear() {
        let mut rs = RememberedSet::new();
        let header = GcHeader::new(0, 16);
        let mut slot: *const GcHeader = &header;
        rs.insert(&mut slot);
        assert_eq!(rs.len(), 1);

        rs.clear();
        assert!(rs.is_empty());
    }
}
