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
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::heap::{GcConfig, GcHeap};

/// A GC-managed object handle. 32-bit index, `Copy`, cheap.
///
/// Handles are only valid within the `TypedHeap` that created them.
/// After a collection, handles to freed objects return `None` on access.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Handle(pub u32);

/// Error returned by [`TypedHeap::reserve_bytes`] when a reservation would
/// cross the configured `max_heap_bytes` cap.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OutOfMemory;

impl std::fmt::Display for OutOfMemory {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("out of memory: heap limit exceeded")
    }
}

impl std::error::Error for OutOfMemory {}

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
///
/// # Memory budget
///
/// Live allocation tracking happens here, not on the underlying page-based
/// [`GcHeap`], because every concrete VM object (`HeapValue`, shapes,
/// strings, …) is allocated as a `Box<dyn TypeErasedObject>` stored in
/// `slots`. The page-based allocator in `GcHeap` is infrastructure for a
/// future migration and currently only tracks its own raw-page tests.
///
/// The `TypedHeap` owns a `tracked_bytes` counter fed by:
///
/// * shell allocations — `alloc<T>()` adds `size_of::<T>()`;
/// * explicit reservations — [`TypedHeap::reserve_bytes`] adds an arbitrary
///   number for container growth (e.g. `Vec::resize` on array elements).
///
/// When the counter crosses the configured `max_heap_bytes` cap, the heap
/// sets its shared OOM flag (owned by the underlying `GcHeap`) which the
/// interpreter polls at GC safepoints.
pub struct TypedHeap {
    /// Handle table: index → type-erased object.
    slots: Vec<Option<Slot>>,
    /// Free list of released slot indices.
    free_list: Vec<u32>,
    /// The underlying page-based GC heap (drives collection heuristics and
    /// owns the shared OOM flag).
    gc: GcHeap,
    /// Mark bitmap for full GC (parallel to slots).
    marks: Vec<bool>,
    /// Running total of tracked bytes (object shells + explicit reservations).
    tracked_bytes: usize,
    /// Cached hard cap for fast-path comparison (copied from
    /// [`GcConfig::max_heap_bytes`]). `None` = unlimited.
    max_heap_bytes: Option<usize>,
    /// Cached OOM flag from the underlying `GcHeap`, kept as a direct handle
    /// to avoid the indirection on every alloc.
    oom_flag: Arc<AtomicBool>,
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
        Self::with_config(GcConfig::default())
    }

    /// Creates a new typed heap with an explicit GC configuration.
    ///
    /// Use this to set a hard heap cap ([`GcConfig::max_heap_bytes`]) — the
    /// Otter analogue of Node.js's `--max-old-space-size`.
    pub fn with_config(config: GcConfig) -> Self {
        let max_heap_bytes = config.max_heap_bytes;
        let gc = GcHeap::new(config);
        let oom_flag = gc.oom_flag();
        Self {
            slots: Vec::with_capacity(1024),
            free_list: Vec::new(),
            gc,
            marks: Vec::new(),
            tracked_bytes: 0,
            max_heap_bytes,
            oom_flag,
        }
    }

    /// Creates a typed heap whose hard cap is `max_bytes`. All other GC
    /// parameters use their defaults.
    pub fn with_max_heap_bytes(max_bytes: usize) -> Self {
        Self::with_config(GcConfig {
            max_heap_bytes: Some(max_bytes),
            ..GcConfig::default()
        })
    }

    /// Returns a clone of the shared OOM signal flag.
    pub fn oom_flag(&self) -> Arc<AtomicBool> {
        self.oom_flag.clone()
    }

    /// Resets the OOM signal flag. Called by the runtime at script start.
    pub fn clear_oom_flag(&self) {
        self.oom_flag.store(false, Ordering::Relaxed);
    }

    /// Returns the configured hard cap on the heap size, if any.
    #[inline]
    pub fn max_heap_bytes(&self) -> Option<usize> {
        self.max_heap_bytes
    }

    /// Returns the currently tracked memory footprint in bytes.
    ///
    /// This is `sum(size_of::<T>())` for every live shell allocation plus
    /// any explicit reservations made via [`TypedHeap::reserve_bytes`]. The
    /// value is a lower bound — heap-allocated container internals (e.g. a
    /// `Vec<u8>` inside a payload) are only counted when the caller
    /// explicitly reserves them.
    #[inline]
    pub fn tracked_bytes(&self) -> usize {
        self.tracked_bytes
    }

    /// True when the tracked footprint plus `additional` would cross the
    /// configured hard cap. Cheap no-op when no limit is configured.
    #[inline]
    pub fn would_exceed_limit(&self, additional: usize) -> bool {
        match self.max_heap_bytes {
            Some(limit) => self.tracked_bytes.saturating_add(additional) > limit,
            None => false,
        }
    }

    /// Reserve `bytes` worth of off-slot memory (e.g. before growing a
    /// `Vec<RegisterValue>` inside an object). Returns `Err(OutOfMemory)`
    /// and raises the OOM flag when the reservation would exceed the cap.
    /// Callers must pair a successful reservation with [`release_bytes`]
    /// when the memory is freed.
    pub fn reserve_bytes(&mut self, bytes: usize) -> Result<(), OutOfMemory> {
        if self.would_exceed_limit(bytes) {
            self.oom_flag.store(true, Ordering::Relaxed);
            return Err(OutOfMemory);
        }
        self.tracked_bytes = self.tracked_bytes.saturating_add(bytes);
        Ok(())
    }

    /// Releases a previous reservation made via [`reserve_bytes`].
    pub fn release_bytes(&mut self, bytes: usize) {
        self.tracked_bytes = self.tracked_bytes.saturating_sub(bytes);
    }

    /// Allocates a new object, returning its handle.
    pub fn alloc<T: Traceable>(&mut self, value: T) -> Handle {
        let size = std::mem::size_of::<T>();
        // Shell accounting: if a hard cap is configured and we would cross
        // it, raise the OOM flag but still return a handle so callers can
        // decide when to fail (interpreter polls the flag at the next GC
        // safepoint). Failing inside `alloc` would require every call site
        // to handle the `Option<Handle>` contract, a much larger refactor.
        if self.would_exceed_limit(size) {
            self.oom_flag.store(true, Ordering::Relaxed);
        }
        self.tracked_bytes = self.tracked_bytes.saturating_add(size);

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
        self.mark_phase(roots);
        self.sweep_phase();
    }

    /// Runs only the mark phase (BFS from roots). Call `is_marked` afterwards
    /// to inspect results, then `mark_additional` for ephemeron values,
    /// and finally `sweep_phase` to free dead objects.
    ///
    /// This split allows the caller (ObjectHeap) to process ephemerons and
    /// clear dead weak entries between mark and sweep.
    pub fn run_mark_phase(&mut self, roots: &[Handle]) {
        self.mark_phase(roots);
    }

    /// Marks additional handles discovered during ephemeron processing.
    /// Call after `run_mark_phase` and before `run_sweep_phase`.
    pub fn run_mark_additional(&mut self, handles: &[Handle]) {
        self.mark_additional(handles);
    }

    /// Runs the sweep phase, freeing all unmarked objects.
    /// Call after mark phase and ephemeron processing are complete.
    pub fn run_sweep_phase(&mut self) {
        self.sweep_phase();
    }

    /// Returns whether a handle was marked during the current collection cycle.
    /// Valid between `run_mark_phase` and `run_sweep_phase`.
    #[must_use]
    pub fn is_marked(&self, handle: Handle) -> bool {
        let idx = handle.0 as usize;
        self.marks.get(idx).copied().unwrap_or(false)
    }

    /// Returns the mark bitmap as a slice. Valid between mark and sweep phases.
    /// Index `i` is `true` if `Handle(i)` was marked.
    #[must_use]
    pub fn marks(&self) -> &[bool] {
        &self.marks
    }

    /// Mark phase: BFS from roots, sets mark bits and traces children.
    fn mark_phase(&mut self, roots: &[Handle]) {
        self.marks.resize(self.slots.len(), false);
        self.marks.fill(false);

        let mut worklist: Vec<Handle> = roots.to_vec();
        self.drain_worklist(&mut worklist);
    }

    /// Marks additional handles and traces their children (used by ephemeron fixpoint).
    fn mark_additional(&mut self, handles: &[Handle]) {
        let mut worklist: Vec<Handle> = handles.to_vec();
        self.drain_worklist(&mut worklist);
    }

    /// Drains the mark worklist, marking and tracing each handle.
    fn drain_worklist(&mut self, worklist: &mut Vec<Handle>) {
        while let Some(handle) = worklist.pop() {
            let idx = handle.0 as usize;
            if idx >= self.marks.len() || self.marks[idx] {
                continue;
            }
            self.marks[idx] = true;

            if let Some(Some(slot)) = self.slots.get(idx) {
                slot.object.trace_handles(&mut |child| {
                    let cidx = child.0 as usize;
                    if cidx < self.marks.len() && !self.marks[cidx] {
                        worklist.push(child);
                    }
                });
            }
        }
    }

    /// Sweep phase: free all unmarked slots.
    fn sweep_phase(&mut self) {
        for (idx, slot_opt) in self.slots.iter_mut().enumerate() {
            if slot_opt.is_some() && !self.marks.get(idx).copied().unwrap_or(false) {
                if let Some(slot) = slot_opt.take() {
                    self.tracked_bytes = self.tracked_bytes.saturating_sub(slot.size);
                }
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
    fn tracked_bytes_grows_with_alloc() {
        let mut heap = TypedHeap::new();
        assert_eq!(heap.tracked_bytes(), 0);
        heap.alloc(Leaf(1));
        let first = heap.tracked_bytes();
        assert!(first >= std::mem::size_of::<Leaf>());
        heap.alloc(Leaf(2));
        assert!(heap.tracked_bytes() > first);
    }

    #[test]
    fn tracked_bytes_shrinks_after_sweep() {
        let mut heap = TypedHeap::new();
        let keep = heap.alloc(Leaf(1));
        heap.alloc(Leaf(2)); // unreachable
        heap.alloc(Leaf(3)); // unreachable
        let before = heap.tracked_bytes();
        heap.collect(&[keep]);
        assert!(
            heap.tracked_bytes() < before,
            "sweep should reclaim tracked bytes for dead slots"
        );
    }

    #[test]
    fn reserve_bytes_succeeds_under_limit() {
        let mut heap = TypedHeap::with_max_heap_bytes(1024);
        heap.reserve_bytes(256).expect("fits");
        assert_eq!(heap.tracked_bytes(), 256);
        heap.release_bytes(256);
        assert_eq!(heap.tracked_bytes(), 0);
    }

    #[test]
    fn reserve_bytes_fails_and_sets_oom_flag() {
        let mut heap = TypedHeap::with_max_heap_bytes(128);
        let err = heap.reserve_bytes(1024).unwrap_err();
        assert_eq!(err, OutOfMemory);
        assert!(heap.oom_flag().load(Ordering::Relaxed));
    }

    #[test]
    fn alloc_sets_oom_flag_when_shell_exhausts_cap() {
        // Cap below size_of<Leaf> so the very first alloc trips the flag.
        let tiny = std::mem::size_of::<Leaf>() / 2;
        let mut heap = TypedHeap::with_max_heap_bytes(tiny);
        heap.alloc(Leaf(1));
        assert!(heap.oom_flag().load(Ordering::Relaxed));
    }

    #[test]
    fn unlimited_heap_does_not_set_oom_flag() {
        let mut heap = TypedHeap::new();
        for i in 0..4_096 {
            heap.alloc(Leaf(i));
        }
        assert!(!heap.oom_flag().load(Ordering::Relaxed));
    }

    #[test]
    fn clear_oom_flag_resets() {
        let mut heap = TypedHeap::with_max_heap_bytes(8);
        let _ = heap.reserve_bytes(64);
        assert!(heap.oom_flag().load(Ordering::Relaxed));
        heap.clear_oom_flag();
        assert!(!heap.oom_flag().load(Ordering::Relaxed));
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
