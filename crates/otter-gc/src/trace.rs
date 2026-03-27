//! Object tracing infrastructure — type-dispatched visitor for the mark phase.
//!
//! Every GC-managed object type registers a [`TraceFn`] in the global trace
//! table, indexed by the object's `type_tag` from [`GcHeader`]. During
//! marking, the collector reads the tag, looks up the trace function in O(1),
//! and calls it to discover child pointers.
//!
//! **No `dyn Any`, no trait objects on the hot path.** The trace table is a
//! fixed-size array of function pointers — a single indexed load + indirect
//! call, same as V8's `BodyDescriptor::IterateBody` dispatch.

use crate::header::GcHeader;

/// Maximum number of distinct object type tags.
pub const MAX_TYPE_TAGS: usize = 256;

/// Pointer visitor callback type — a plain function pointer.
///
/// The argument is a *mutable* pointer to the slot containing the child
/// pointer. This allows the scavenger/compactor to update the slot
/// in-place when an object is moved.
///
/// For cases that need captured state (e.g., the scavenger's evacuate loop),
/// use [`TraceTable::trace_object_mut`] which accepts `&mut dyn FnMut`.
pub type VisitFn = fn(slot: *mut *const GcHeader);

/// Trace function signature. Given a pointer to the object's header,
/// calls `visit` for each GC-pointer slot within the object's payload.
///
/// Uses `&mut dyn FnMut` to allow the visitor to capture mutable state
/// (e.g., the scavenger needs to evacuate children into to-space).
/// This matches V8's pattern where the visitor is a virtual method call
/// on a `ObjectVisitor` base class.
pub type TraceFn = fn(header: *const GcHeader, visit: &mut dyn FnMut(*mut *const GcHeader));

/// Global trace function table — one entry per type tag.
///
/// Initialized at startup via [`register_trace_fn`]. Entries that are `None`
/// indicate leaf objects with no GC-pointer fields (e.g., raw strings,
/// number boxes) — the collector skips tracing for them entirely.
pub struct TraceTable {
    entries: [Option<TraceFn>; MAX_TYPE_TAGS],
}

impl TraceTable {
    /// Creates an empty trace table (all entries `None`).
    pub const fn new() -> Self {
        Self {
            entries: [None; MAX_TYPE_TAGS],
        }
    }

    /// Registers a trace function for the given type tag.
    ///
    /// # Panics
    ///
    /// Panics if `type_tag` already has a registered trace function
    /// (double registration is a bug).
    pub fn register(&mut self, type_tag: u8, trace_fn: TraceFn) {
        let slot = &mut self.entries[type_tag as usize];
        assert!(
            slot.is_none(),
            "trace function already registered for type_tag {type_tag}"
        );
        *slot = Some(trace_fn);
    }

    /// Looks up the trace function for the given type tag.
    /// Returns `None` for leaf objects (no GC pointers to trace).
    #[inline]
    pub fn get(&self, type_tag: u8) -> Option<TraceFn> {
        self.entries[type_tag as usize]
    }

    /// Traces an object by looking up its trace function and calling it.
    /// No-op for leaf objects (returns without calling visit).
    ///
    /// The visitor is a `&mut dyn FnMut` so it can capture mutable state
    /// (e.g., the scavenger's evacuation context).
    ///
    /// # Safety
    ///
    /// `header` must point to a valid, live GC object with a correctly
    /// initialized type tag.
    #[inline]
    pub unsafe fn trace_object(
        &self,
        header: *const GcHeader,
        visit: &mut dyn FnMut(*mut *const GcHeader),
    ) {
        let tag = unsafe { (*header).type_tag() };
        if let Some(trace_fn) = self.entries[tag as usize] {
            trace_fn(header, visit);
        }
    }
}

impl Default for TraceTable {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::header::GcHeader;

    const TAG_LEAF: u8 = 0;
    const TAG_WITH_CHILD: u8 = 1;

    /// A fake object layout: header + one pointer slot.
    #[repr(C)]
    struct FakeNode {
        header: GcHeader,
        child: *const GcHeader,
    }

    fn trace_fake_node(header: *const GcHeader, visit: &mut dyn FnMut(*mut *const GcHeader)) {
        let node = header as *const FakeNode;
        let child_slot = unsafe { &raw const (*node).child } as *mut *const GcHeader;
        visit(child_slot);
    }

    #[test]
    fn empty_table_returns_none() {
        let table = TraceTable::new();
        assert!(table.get(0).is_none());
        assert!(table.get(255).is_none());
    }

    #[test]
    fn register_and_lookup() {
        let mut table = TraceTable::new();
        table.register(TAG_WITH_CHILD, trace_fake_node);

        assert!(table.get(TAG_LEAF).is_none());
        assert!(table.get(TAG_WITH_CHILD).is_some());
    }

    #[test]
    #[should_panic(expected = "already registered")]
    fn double_register_panics() {
        let mut table = TraceTable::new();
        table.register(TAG_WITH_CHILD, trace_fake_node);
        table.register(TAG_WITH_CHILD, trace_fake_node); // Should panic
    }

    #[test]
    fn trace_object_visits_child_slots() {
        let mut table = TraceTable::new();
        table.register(TAG_WITH_CHILD, trace_fake_node);

        let child = GcHeader::new(TAG_LEAF, 8);
        let node = FakeNode {
            header: GcHeader::new(TAG_WITH_CHILD, std::mem::size_of::<FakeNode>() as u32),
            child: &child as *const GcHeader,
        };

        let mut visited_slots: Vec<*const GcHeader> = Vec::new();

        unsafe {
            table.trace_object(&node.header as *const GcHeader, &mut |slot| {
                let ptr = slot.read();
                visited_slots.push(ptr);
            });
        };

        assert_eq!(visited_slots.len(), 1);
        assert_eq!(visited_slots[0], &child as *const GcHeader);
    }

    #[test]
    fn trace_leaf_is_noop() {
        let table = TraceTable::new();
        // TAG_LEAF not registered → trace_object should be a no-op.
        let leaf = GcHeader::new(TAG_LEAF, 8);

        // This should not panic or call any visit function.
        unsafe {
            table.trace_object(&leaf as *const GcHeader, &mut |_| {
                panic!("should not be called for leaf");
            });
        };
    }
}
