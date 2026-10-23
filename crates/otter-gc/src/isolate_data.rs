//! Per-isolate embedder state the heap owns and roots.
//!
//! Some isolate state has to be reachable from code that holds only the
//! heap: native leaves called from generated code, heap-only define paths
//! and the GC's own root walk. The heap therefore owns one embedder value,
//! typed by the embedder, and hands it back by type through `&GcHeap` and
//! `&mut GcHeap`. The VM keeps its property-name interner and the canonical
//! key strings there.
//!
//! # Contents
//! - [`IsolateData`] — the embedder value's contract: its strong old-space
//!   handles and pre-sweep weak maintenance.
//! - [`IsolateDataSlot`] — the heap's owned slot: the value plus the type it
//!   was installed as.
//!
//! # Invariants
//! - A heap holds at most one value, installed once; it lives exactly as
//!   long as the heap.
//! - The value is boxed, so its address and the addresses of the boxes it
//!   owns are stable for the heap's life: generated code may embed them.
//! - A typed read answers only for the type the value was installed as.
//! - Weak maintenance runs once at the physical sweep boundary, after final marking.
//! - The value names old-space cells only. Old space never moves, so its
//!   handles are visited as roots by every marking and by no scavenge.
//!
//! # See also
//! - [`crate::heap::GcHeap::install_isolate_data`]
//! - [`crate::heap::GcHeap::embedder_root`] — the fixed-slot sibling for
//!   single handles.

use std::any::{Any, TypeId};

use crate::compressed::RawGc;

/// Embedder state a [`crate::heap::GcHeap`] owns for its isolate.
pub trait IsolateData: Any {
    /// Visit the slot of every strong handle the value holds. Each one names
    /// an old-space cell, which never moves, so the visitor never rewrites
    /// a slot.
    fn visit_old_roots(&self, visitor: &mut dyn FnMut(*mut RawGc));

    /// Forget old-space weak entries after marking and before physical sweep.
    ///
    /// The hook must not allocate, mark, reenter the mutator or retain `heap`.
    /// It runs for full and split mark/sweep collections, independently of
    /// temporary extra-root registrations. Old-space handles never move.
    fn sweep_weak(&self, _heap: &crate::heap::GcHeap) {}
}

/// The heap's owned embedder value and the type it was installed as.
pub(crate) struct IsolateDataSlot {
    type_id: TypeId,
    data: Box<dyn IsolateData>,
}

impl IsolateDataSlot {
    pub(crate) fn new<T: IsolateData>(data: T) -> Self {
        Self {
            type_id: TypeId::of::<T>(),
            data: Box::new(data),
        }
    }

    /// The value as `T`, when it was installed as `T`.
    #[inline]
    pub(crate) fn get<T: IsolateData>(&self) -> Option<&T> {
        if self.type_id != TypeId::of::<T>() {
            return None;
        }
        // SAFETY: `type_id` was recorded from the concrete type boxed into
        // `data`, and it equals `T`'s, so the box holds a `T`.
        Some(unsafe { &*std::ptr::from_ref::<dyn IsolateData>(&*self.data).cast::<T>() })
    }

    /// [`Self::get`] for mutation.
    #[inline]
    pub(crate) fn get_mut<T: IsolateData>(&mut self) -> Option<&mut T> {
        if self.type_id != TypeId::of::<T>() {
            return None;
        }
        // SAFETY: as in `get`; the unique borrow of the slot is the unique
        // borrow of its value.
        Some(unsafe { &mut *std::ptr::from_mut::<dyn IsolateData>(&mut *self.data).cast::<T>() })
    }

    pub(crate) fn sweep_weak(&self, heap: &crate::heap::GcHeap) {
        self.data.sweep_weak(heap);
    }

    pub(crate) fn visit_old_roots(&self, visitor: &mut dyn FnMut(*mut RawGc)) {
        self.data.visit_old_roots(visitor);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Counter(u32);

    impl IsolateData for Counter {
        fn visit_old_roots(&self, _visitor: &mut dyn FnMut(*mut RawGc)) {}
    }

    struct Other;

    impl IsolateData for Other {
        fn visit_old_roots(&self, _visitor: &mut dyn FnMut(*mut RawGc)) {}
    }

    #[test]
    fn typed_reads_answer_only_for_the_installed_type() {
        let mut slot = IsolateDataSlot::new(Counter(7));
        assert_eq!(slot.get::<Counter>().map(|counter| counter.0), Some(7));
        assert!(slot.get::<Other>().is_none());
        slot.get_mut::<Counter>().expect("installed type").0 = 9;
        assert_eq!(slot.get::<Counter>().map(|counter| counter.0), Some(9));
        assert!(slot.get_mut::<Other>().is_none());
    }
}
