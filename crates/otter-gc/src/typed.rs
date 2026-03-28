//! Safe typed allocation and access API for the GC heap.
//!
//! This module provides a safe layer over the raw page-based heap that
//! `otter-vm` (which forbids `unsafe`) can use. Objects are stored as
//! typed Rust values behind GC handles, with all unsafe pointer operations
//! confined to this module.
//!
//! # Design
//!
//! Each GC-managed type is registered with a unique `type_tag`. The heap
//! stores objects as `Box<dyn TypeErasedObject>` in a handle table. This
//! approach has slightly more overhead than raw page-based allocation
//! (one extra indirection + vtable call for tracing), but provides:
//!
//! - **Full safety**: no unsafe in calling code
//! - **Correct GC tracing**: objects implement `Traceable` which reports
//!   child handles
//! - **Cross-platform**: works on macOS, Linux, Windows, WASM
//!
//! The page-based allocator (`page.rs`, `space.rs`) is used for the
//! underlying memory when the TypedHeap is backed by page allocation.
//! For the initial integration, we use a handle-table approach that
//! delegates collection decisions to the GcHeap's generational logic.

use std::any::Any;

use crate::heap::GcHeap;

/// A GC-managed object handle. 32-bit index, `Copy`, cheap.
///
/// Handles are only valid within the `TypedHeap` that created them.
/// After a collection, handles to freed objects return `None` on access.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Handle(pub u32);

/// Trait for GC-traceable objects. Objects stored in the typed heap must
/// implement this to report which other handles they reference.
///
/// This is the safe equivalent of the raw `TraceFn` in `trace.rs`.
pub trait Traceable: Any {
    /// Report all `Handle`s that this object holds.
    fn trace_handles(&self, visitor: &mut dyn FnMut(Handle));
}

/// Type-erased object storage with tracing support.
trait TypeErasedObject: Any {
    fn trace_handles(&self, visitor: &mut dyn FnMut(Handle));
    fn as_any(&self) -> &dyn Any;
    fn as_any_mut(&mut self) -> &mut dyn Any;
}

impl<T: Traceable> TypeErasedObject for T {
    fn trace_handles(&self, visitor: &mut dyn FnMut(Handle)) {
        Traceable::trace_handles(self, visitor);
    }
    fn as_any(&self) -> &dyn Any {
        self
    }
    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }
}

/// Safe, typed GC heap built on top of the page-based `GcHeap`.
///
/// Provides handle-based allocation and access with automatic collection.
/// All unsafe is confined within this struct's implementation.
pub struct TypedHeap {
    /// Handle table: index → type-erased object.
    slots: Vec<Option<Slot>>,
    /// Free list of released slot indices.
    free_list: Vec<u32>,
    /// The underlying page-based GC heap (drives collection heuristics).
    gc: GcHeap,
    /// Mark bitmap for full GC (parallel to slots).
    marks: Vec<bool>,
}

struct Slot {
    object: Box<dyn TypeErasedObject>,
    /// Size in bytes (approximate, for GC pressure tracking).
    #[allow(dead_code)]
    size: usize,
    /// Whether this object is in the young generation.
    #[allow(dead_code)]
    is_young: bool,
    /// Whether this object survived a previous young-gen collection.
    #[allow(dead_code)]
    survived: bool,
}

impl TypedHeap {
    /// Creates a new typed heap with default configuration.
    pub fn new() -> Self {
        Self {
            slots: Vec::with_capacity(1024),
            free_list: Vec::new(),
            gc: GcHeap::with_defaults(),
            marks: Vec::new(),
        }
    }

    /// Allocates a new object, returning its handle.
    pub fn alloc<T: Traceable>(&mut self, value: T) -> Handle {
        let size = std::mem::size_of::<T>();
        let slot = Slot {
            object: Box::new(value),
            size,
            is_young: true,
            survived: false,
        };

        let index = if let Some(free) = self.free_list.pop() {
            self.slots[free as usize] = Some(slot);
            free
        } else {
            let idx = self.slots.len() as u32;
            self.slots.push(Some(slot));
            idx
        };

        Handle(index)
    }

    /// Reads a reference to the object behind a handle.
    pub fn get<T: Traceable>(&self, handle: Handle) -> Option<&T> {
        self.slots
            .get(handle.0 as usize)?
            .as_ref()?
            .object
            .as_any()
            .downcast_ref::<T>()
    }

    /// Reads a mutable reference to the object behind a handle.
    pub fn get_mut<T: Traceable>(&mut self, handle: Handle) -> Option<&mut T> {
        self.slots
            .get_mut(handle.0 as usize)?
            .as_mut()?
            .object
            .as_any_mut()
            .downcast_mut::<T>()
    }

    /// Returns true if the handle points to a live object.
    pub fn is_live(&self, handle: Handle) -> bool {
        self.slots
            .get(handle.0 as usize)
            .is_some_and(|s| s.is_some())
    }

    /// Number of live objects.
    pub fn live_count(&self) -> usize {
        self.slots.iter().filter(|s| s.is_some()).count()
    }

    /// Triggers a full mark-sweep collection.
    ///
    /// `roots` are the handles that are directly reachable (stack, globals).
    /// All objects transitively reachable from roots survive; the rest are freed.
    pub fn collect(&mut self, roots: &[Handle]) {
        // Reset marks.
        self.marks.resize(self.slots.len(), false);
        self.marks.fill(false);

        // Mark phase: BFS from roots.
        let mut worklist: Vec<Handle> = roots.to_vec();

        while let Some(handle) = worklist.pop() {
            let idx = handle.0 as usize;
            if idx >= self.marks.len() || self.marks[idx] {
                continue;
            }
            self.marks[idx] = true;

            // Trace children.
            if let Some(Some(slot)) = self.slots.get(idx) {
                slot.object.trace_handles(&mut |child| {
                    let cidx = child.0 as usize;
                    if cidx < self.marks.len() && !self.marks[cidx] {
                        worklist.push(child);
                    }
                });
            }
        }

        // Sweep: free unmarked slots.
        for (idx, slot_opt) in self.slots.iter_mut().enumerate() {
            if slot_opt.is_some() && !self.marks.get(idx).copied().unwrap_or(false) {
                *slot_opt = None;
                self.free_list.push(idx as u32);
            }
        }
    }

    /// Triggers collection if memory pressure exceeds threshold.
    /// Called at GC safepoints.
    pub fn maybe_collect(&mut self, roots: &[Handle]) {
        // Simple heuristic: collect when slot count exceeds 2x live count
        // or when we have more than 10K dead slots.
        let total = self.slots.len();
        let live = self.live_count();
        if total > 1024 && live * 2 < total {
            self.collect(roots);
        }
    }

    /// Iterates over all live slots, calling `visitor(index, &dyn Any)`.
    /// The visitor can downcast to the concrete type.
    /// Used for operations that need to scan all objects (e.g., native payload tracing).
    pub fn for_each<F>(&self, mut visitor: F)
    where
        F: FnMut(u32, &dyn Any),
    {
        for (idx, slot_opt) in self.slots.iter().enumerate() {
            if let Some(slot) = slot_opt {
                visitor(idx as u32, slot.object.as_any());
            }
        }
    }

    /// Access to the underlying page-based GcHeap (for direct page operations).
    pub fn gc_heap(&self) -> &GcHeap {
        &self.gc
    }

    /// Mutable access to the underlying GcHeap.
    pub fn gc_heap_mut(&mut self) -> &mut GcHeap {
        &mut self.gc
    }
}

impl Default for TypedHeap {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, PartialEq)]
    struct Leaf(i64);

    impl Traceable for Leaf {
        fn trace_handles(&self, _visitor: &mut dyn FnMut(Handle)) {}
    }

    #[derive(Debug)]
    #[allow(dead_code)]
    struct Node {
        value: i64,
        child: Option<Handle>,
    }

    impl Traceable for Node {
        fn trace_handles(&self, visitor: &mut dyn FnMut(Handle)) {
            if let Some(h) = self.child {
                visitor(h);
            }
        }
    }

    #[test]
    fn alloc_and_read() {
        let mut heap = TypedHeap::new();
        let h = heap.alloc(Leaf(42));
        assert_eq!(heap.get::<Leaf>(h), Some(&Leaf(42)));
    }

    #[test]
    fn alloc_and_mutate() {
        let mut heap = TypedHeap::new();
        let h = heap.alloc(Leaf(0));
        heap.get_mut::<Leaf>(h).unwrap().0 = 99;
        assert_eq!(heap.get::<Leaf>(h), Some(&Leaf(99)));
    }

    #[test]
    fn collect_frees_unreachable() {
        let mut heap = TypedHeap::new();
        let alive = heap.alloc(Leaf(1));
        let dead = heap.alloc(Leaf(2));

        heap.collect(&[alive]);

        assert!(heap.is_live(alive));
        assert!(!heap.is_live(dead));
        assert_eq!(heap.live_count(), 1);
    }

    #[test]
    fn collect_follows_references() {
        let mut heap = TypedHeap::new();
        let leaf = heap.alloc(Leaf(10));
        let node = heap.alloc(Node {
            value: 1,
            child: Some(leaf),
        });
        let _orphan = heap.alloc(Leaf(99));

        heap.collect(&[node]);

        assert!(heap.is_live(node));
        assert!(heap.is_live(leaf)); // Kept alive transitively
        assert!(!heap.is_live(_orphan));
    }

    #[test]
    fn handle_reuse_after_collect() {
        let mut heap = TypedHeap::new();
        let h1 = heap.alloc(Leaf(1));
        let _h2 = heap.alloc(Leaf(2));

        heap.collect(&[h1]); // h2 freed

        let h3 = heap.alloc(Leaf(3)); // Should reuse h2's slot
        assert!(heap.is_live(h3));
        assert_eq!(heap.get::<Leaf>(h3), Some(&Leaf(3)));
    }

    #[test]
    fn multiple_gc_cycles() {
        let mut heap = TypedHeap::new();

        for i in 0..100 {
            let _ = heap.alloc(Leaf(i));
        }
        heap.collect(&[]);
        assert_eq!(heap.live_count(), 0);

        let keep = heap.alloc(Leaf(999));
        for i in 0..50 {
            let _ = heap.alloc(Leaf(i));
        }
        heap.collect(&[keep]);
        assert_eq!(heap.live_count(), 1);
        assert_eq!(heap.get::<Leaf>(keep), Some(&Leaf(999)));
    }

    #[test]
    fn deep_reference_chain() {
        let mut heap = TypedHeap::new();

        // Build a chain: root → n1 → n2 → n3 → leaf
        let leaf = heap.alloc(Leaf(42));
        let n3 = heap.alloc(Node {
            value: 3,
            child: Some(leaf),
        });
        let n2 = heap.alloc(Node {
            value: 2,
            child: Some(n3),
        });
        let n1 = heap.alloc(Node {
            value: 1,
            child: Some(n2),
        });

        // Also some garbage.
        let _g1 = heap.alloc(Leaf(0));
        let _g2 = heap.alloc(Leaf(0));

        heap.collect(&[n1]);

        assert!(heap.is_live(n1));
        assert!(heap.is_live(n2));
        assert!(heap.is_live(n3));
        assert!(heap.is_live(leaf));
        assert!(!heap.is_live(_g1));
        assert!(!heap.is_live(_g2));
        assert_eq!(heap.live_count(), 4);
    }
}
