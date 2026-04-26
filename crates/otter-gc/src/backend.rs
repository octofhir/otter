//! `HeapBackend` — abstract object-heap interface used by `otter-vm`.
//!
//! Phase 1 of the GC migration introduces this seam so the rest of the
//! VM stops binding directly to the concrete `TypedHeap` type. Phase 2
//! adds a second implementation (`PagedHeap`) backed by the page-based
//! `GcHeap` and switches `ObjectHeap` to use it. Phase 6 retires
//! `TypedHeap` entirely.
//!
//! The trait intentionally mirrors the public surface of `TypedHeap`
//! one-to-one so the migration is a drop-in field type swap, not a
//! redesign of every call site in `otter-vm`. Methods that take a
//! generic `T: Traceable` (alloc / get / get_mut) are not object-safe
//! — that's fine, every call site uses static dispatch.
//!
//! Performance contract: every method on this trait must be a thin
//! wrapper over the underlying inherent method. Implementations are
//! expected to be `#[inline]` so the trait dispatch disappears at the
//! call site.

use std::any::Any;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use crate::typed::{Handle, OutOfMemory, Traceable};

/// Object-heap interface used by `otter-vm`.
///
/// See module docs for the rationale and migration plan.
pub trait HeapBackend {
    // -----------------------------------------------------------------
    // Allocation / accessors
    // -----------------------------------------------------------------

    /// Allocates a fresh object. Returns its [`Handle`] or
    /// [`OutOfMemory`] when the configured hard cap is exceeded.
    fn alloc<T: Traceable>(&mut self, value: T) -> Result<Handle, OutOfMemory>;

    /// Returns a reference to the object behind a handle.
    fn get<T: Traceable>(&self, handle: Handle) -> Option<&T>;

    /// Returns a mutable reference to the object behind a handle.
    fn get_mut<T: Traceable>(&mut self, handle: Handle) -> Option<&mut T>;

    /// Returns `true` if the handle still points at a live object.
    fn is_live(&self, handle: Handle) -> bool;

    /// Returns the number of live objects on the heap.
    fn live_count(&self) -> usize;

    /// Iterates over every live object as `(handle_index, &dyn Any)`.
    /// Used by reflection / native-payload tracing where the VM needs
    /// to walk all heap objects of a given concrete type.
    fn for_each(&self, visitor: &mut dyn FnMut(u32, &dyn Any));

    // -----------------------------------------------------------------
    // Collection — synchronous full mark/sweep
    // -----------------------------------------------------------------

    /// Triggers a full mark-sweep collection from the given roots.
    fn collect(&mut self, roots: &[Handle]);

    /// Runs only the mark phase. Used together with
    /// [`HeapBackend::run_mark_additional`] and
    /// [`HeapBackend::run_sweep_phase`] to interleave ephemeron
    /// processing between mark and sweep.
    fn run_mark_phase(&mut self, roots: &[Handle]);

    /// Marks an additional set of handles after [`run_mark_phase`].
    /// Used by the WeakMap/WeakSet ephemeron fix-point loop.
    fn run_mark_additional(&mut self, handles: &[Handle]);

    /// Runs the sweep phase that frees every unmarked object.
    fn run_sweep_phase(&mut self);

    /// Returns whether a handle was marked during the current cycle.
    /// Valid between [`run_mark_phase`] and [`run_sweep_phase`].
    fn is_marked(&self, handle: Handle) -> bool;

    /// Returns the mark bitmap as a slice.
    fn marks(&self) -> &[bool];

    /// Triggers collection if memory pressure exceeds the configured
    /// threshold. Called at GC safepoints.
    fn maybe_collect(&mut self, roots: &[Handle]);

    // -----------------------------------------------------------------
    // Heap-budget reservations
    // -----------------------------------------------------------------

    /// Reserves `bytes` worth of off-slot memory (e.g. before growing
    /// a `Vec<u8>` payload inside an object). Pairs with
    /// [`release_bytes`].
    #[must_use = "allocation reservations must handle possible heap-limit failures"]
    fn reserve_bytes(&mut self, bytes: usize) -> Result<(), OutOfMemory>;

    /// Releases a reservation made via [`reserve_bytes`].
    fn release_bytes(&mut self, bytes: usize);

    /// Returns `true` when `tracked_bytes() + additional` would cross
    /// the configured hard cap.
    fn would_exceed_limit(&self, additional: usize) -> bool;

    /// Returns the current tracked footprint in bytes.
    fn tracked_bytes(&self) -> usize;

    /// Returns the configured hard cap in bytes, if any.
    fn max_heap_bytes(&self) -> Option<usize>;

    // -----------------------------------------------------------------
    // OOM signal flag
    // -----------------------------------------------------------------

    /// Returns a clone of the shared OOM signal flag.
    fn oom_flag(&self) -> Arc<AtomicBool>;

    /// Resets the OOM signal flag.
    fn clear_oom_flag(&self);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::typed::TypedHeap;

    #[derive(Debug, PartialEq)]
    struct Leaf(i64);

    impl Traceable for Leaf {
        fn trace_handles(&self, _visitor: &mut dyn FnMut(Handle)) {}
    }

    /// Smoke test that `TypedHeap` actually satisfies `HeapBackend`
    /// once the `impl` lands. Phase 1's blanket impl in `typed.rs`
    /// is what makes this compile.
    #[test]
    fn typed_heap_implements_heap_backend() {
        fn assert_backend<B: HeapBackend>() {}
        assert_backend::<TypedHeap>();
    }

    #[test]
    fn round_trip_through_trait() {
        let mut heap: TypedHeap = TypedHeap::new();
        let h = HeapBackend::alloc(&mut heap, Leaf(42)).expect("alloc");
        assert_eq!(HeapBackend::get::<Leaf>(&heap, h), Some(&Leaf(42)));
        assert!(HeapBackend::is_live(&heap, h));
        HeapBackend::collect(&mut heap, &[]);
        assert!(!HeapBackend::is_live(&heap, h));
    }
}
