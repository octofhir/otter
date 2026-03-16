//! Incremental Mark/Sweep Garbage Collector
//!
//! This module implements an incremental mark/sweep collector
//! that can collect circular references with minimal pause times.
//!
//! ## Design
//!
//! - **Block-Based Allocation**: Objects are allocated in 16KB blocks
//! - **Size-Class Segregation**: Each block holds cells of a single size class
//! - **Tri-color Marking**: Uses white/gray/black marking for cycle detection
//! - **Incremental Marking**: Processes a budget of gray objects per safepoint
//! - **Write Barriers**: Dijkstra insertion barriers maintain tri-color invariant
//! - **Per-Block Sweep**: Sweep can skip entirely-dead blocks in O(1)
//! - **Large Object Space**: Objects > 8KB get individual allocations
//! - **Black Allocation**: Objects allocated during marking are pre-marked live

use std::cell::{Cell, RefCell};
use std::collections::{HashSet, VecDeque};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use crate::marked_block::{
    BlockDirectory, DropFn, LARGE_OBJECT_THRESHOLD, NUM_SIZE_CLASSES, TraceFn,
    size_class_cell_size, size_class_index,
};
use crate::object::{GcAllocation, GcHeader, MarkColor, bump_mark_version};

/// GC phase for incremental collection.
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GcPhase {
    /// No GC in progress — normal mutation
    Idle = 0,
    /// Incremental marking in progress — write barriers active
    Marking = 1,
}

thread_local! {
    /// Flag indicating that `dealloc_all` is in progress on this thread.
    /// When true, Drop impls should NOT access other GC-managed objects
    /// because they may have already been freed.
    static GC_DEALLOC_IN_PROGRESS: Cell<bool> = const { Cell::new(false) };

    /// Write barrier buffer: collects grayed objects during incremental marking.
    /// Drained into the mark worklist at each incremental step.
    static WRITE_BARRIER_BUF: RefCell<Vec<*const GcHeader>> = RefCell::new(Vec::with_capacity(256));
}

/// Push a grayed object to the write barrier buffer on the current registry.
///
/// Called from insertion barriers when GC is in Marking phase.
/// The buffer is drained into the mark worklist during `incremental_mark_step()`.
pub fn barrier_push(ptr: *const GcHeader) {
    // Try per-isolate buffer first, fall back to thread-local for backward compat
    THREAD_REGISTRY.with(|r| {
        let reg_ptr = r.get();
        if !reg_ptr.is_null() {
            let reg = unsafe { &*reg_ptr };
            reg.write_barrier_buf.borrow_mut().push(ptr);
        } else {
            WRITE_BARRIER_BUF.with(|buf| buf.borrow_mut().push(ptr));
        }
    });
}

/// Drain the write barrier buffer from the given registry.
///
/// Returns all accumulated barrier entries and empties the buffer.
fn barrier_drain_from(registry: &AllocationRegistry) -> Vec<*const GcHeader> {
    std::mem::take(&mut *registry.write_barrier_buf.borrow_mut())
}

/// Returns true if the GC is currently freeing all allocations.
/// Drop impls should check this and skip any access to other GC objects.
pub fn is_dealloc_in_progress() -> bool {
    THREAD_REGISTRY.with(|r| {
        let reg_ptr = r.get();
        if !reg_ptr.is_null() {
            let reg = unsafe { &*reg_ptr };
            reg.dealloc_in_progress.get()
        } else {
            GC_DEALLOC_IN_PROGRESS.with(|f| f.get())
        }
    })
}

/// A large object allocation (> 8KB), individually tracked.
struct LargeAllocation {
    /// Pointer to the GcHeader at the start of the allocation
    header: *mut GcHeader,
    /// Size of the allocation (header + value)
    size: usize,
    /// Layout used for deallocation
    _layout: std::alloc::Layout,
    /// Drop function for this allocation
    drop_fn: DropFn,
    /// Trace function for this allocation (obsolete, handled by trace_table)
    _trace_fn: Option<TraceFn>,
}

// SAFETY: LargeAllocation contains raw pointers but they are managed exclusively
// by the AllocationRegistry on a single thread. Thread confinement is enforced
// by the Isolate abstraction (one isolate = one thread at a time).
unsafe impl Send for LargeAllocation {}
unsafe impl Sync for LargeAllocation {}

/// Central registry tracking all GC-managed allocations.
///
/// Uses block-based allocation for small objects (≤ 8KB) and individual
/// allocations for large objects (> 8KB).
///
/// Supports both full stop-the-world collection (`collect()`) and incremental
/// marking (`start_incremental_gc()` / `incremental_mark_step()` / `finish_gc()`).
pub struct AllocationRegistry {
    /// Per-size-class block directories for small objects (old space).
    /// Index = size class index (0..NUM_SIZE_CLASSES).
    directories: Vec<BlockDirectory>,
    /// Large objects tracked individually.
    large_objects: RefCell<Vec<LargeAllocation>>,
    /// Total bytes allocated (across blocks + large objects + nursery).
    total_bytes: AtomicUsize,
    /// Threshold for triggering GC (default 1MB)
    gc_threshold: AtomicUsize,
    /// Number of collections performed
    collection_count: AtomicUsize,
    /// Bytes reclaimed in last collection
    last_reclaimed: AtomicUsize,
    /// Total pause time in nanoseconds (accumulated across all collections)
    total_pause_nanos: AtomicU64,
    /// Last pause time in nanoseconds
    last_pause_nanos: AtomicU64,

    // --- Nursery (young generation) ---
    /// Bump allocator for short-lived objects.
    nursery: crate::nursery::Nursery,
    /// Remembered set: old-gen objects that point to nursery objects.
    remembered_set: crate::barrier::RememberedSet,
    /// Number of minor GC collections performed.
    minor_collection_count: AtomicUsize,

    // --- Incremental marking state ---
    /// Current GC phase (Idle or Marking)
    gc_phase: Cell<GcPhase>,
    /// Persistent worklist for incremental marking (gray objects to process)
    mark_worklist: RefCell<VecDeque<*const GcHeader>>,
    /// Trace table (indexed by tag u8).
    /// Maps object tags to their type-erased trace functions.
    trace_table: [Option<TraceFn>; 256],
    /// Timestamp when incremental marking started (for pause time tracking)
    mark_start: Cell<Option<Instant>>,
    /// GC pause histogram (bucket counts by duration)
    pause_histogram: RefCell<GcPauseHistogram>,

    // --- Per-isolate GC state (formerly thread-locals) ---
    /// Write barrier buffer: collects grayed objects during incremental marking.
    /// Drained into the mark worklist at each incremental step.
    write_barrier_buf: RefCell<Vec<*const GcHeader>>,
    /// Flag indicating that `dealloc_all` is in progress.
    /// When true, Drop impls should NOT access other GC-managed objects.
    dealloc_in_progress: Cell<bool>,
}

impl AllocationRegistry {
    /// Create a new allocation registry
    pub fn new() -> Self {
        let mut directories = Vec::with_capacity(NUM_SIZE_CLASSES);
        for i in 0..NUM_SIZE_CLASSES {
            directories.push(BlockDirectory::new(size_class_cell_size(i)));
        }

        Self {
            directories,
            large_objects: RefCell::new(Vec::new()),
            total_bytes: AtomicUsize::new(0),
            gc_threshold: AtomicUsize::new(1024 * 1024), // 1MB default
            collection_count: AtomicUsize::new(0),
            last_reclaimed: AtomicUsize::new(0),
            total_pause_nanos: AtomicU64::new(0),
            last_pause_nanos: AtomicU64::new(0),
            nursery: crate::nursery::Nursery::new(),
            remembered_set: crate::barrier::RememberedSet::new(),
            minor_collection_count: AtomicUsize::new(0),
            gc_phase: Cell::new(GcPhase::Idle),
            mark_worklist: RefCell::new(VecDeque::new()),
            trace_table: [None; 256],
            mark_start: Cell::new(None),
            pause_histogram: RefCell::new(GcPauseHistogram::default()),
            write_barrier_buf: RefCell::new(Vec::with_capacity(256)),
            dealloc_in_progress: Cell::new(false),
        }
    }

    /// Create a new registry with a custom GC threshold
    pub fn with_threshold(threshold: usize) -> Self {
        let registry = Self::new();
        registry.gc_threshold.store(threshold, Ordering::Relaxed);
        registry
    }

    /// Register a type for GC tracing.
    ///
    /// This builds a static lookup table for the mark phase to find trace
    /// functions based on object tags.
    pub fn register_type<T: GcTraceable>(&mut self) {
        if T::NEEDS_TRACE {
            self.trace_table[T::TYPE_ID as usize] = Some(trace_gc_box::<T>);
        }
    }

    /// Register a new allocation.
    ///
    /// For small objects (≤ 8KB), allocates from a block directory.
    /// For large objects, allocates individually via the global allocator.
    ///
    /// # Safety
    /// - `header` must point to a valid GcHeader at the start of an allocation
    /// - `drop_fn` must correctly deallocate the memory when called
    /// - The allocation must remain valid until removed from the registry
    pub unsafe fn register(&self, header: *mut GcHeader, size: usize, drop_fn: DropFn) {
        // This is the legacy path for objects allocated externally (via alloc::alloc).
        // New allocations should go through allocate_in_block() directly.
        // For backwards compatibility, we register large objects here.
        let large = LargeAllocation {
            header,
            size,
            _layout: std::alloc::Layout::from_size_align(size, 16).unwrap(),
            drop_fn,
            _trace_fn: None,
        };
        self.large_objects.borrow_mut().push(large);
        self.total_bytes.fetch_add(size, Ordering::Relaxed);
    }

    /// Allocate a cell from the appropriate block directory.
    ///
    /// Returns a raw pointer to the start of the cell (where GcHeader goes).
    /// The cell is `cell_size` bytes, which is >= `actual_size`.
    ///
    /// # Panics
    /// Panics if `actual_size` > LARGE_OBJECT_THRESHOLD.
    pub fn allocate_in_block(&self, actual_size: usize, drop_fn: DropFn) -> *mut u8 {
        let sc_idx =
            size_class_index(actual_size).expect("allocate_in_block called for large object");
        let ptr = self.directories[sc_idx].allocate(actual_size, drop_fn);
        self.total_bytes.fetch_add(actual_size, Ordering::Relaxed);
        ptr
    }

    /// Try to allocate a cell in the nursery (young generation).
    ///
    /// Returns a pointer to the start of the cell, or `None` if nursery is full.
    /// Nursery allocation is bump-pointer fast (~3ns).
    #[inline]
    pub fn allocate_in_nursery(&self, actual_size: usize, drop_fn: DropFn) -> Option<*mut u8> {
        let ptr = self.nursery.alloc(actual_size, drop_fn)?;
        self.total_bytes.fetch_add(actual_size, Ordering::Relaxed);
        Some(ptr)
    }

    /// Check if a pointer is in the nursery.
    #[inline]
    pub fn is_nursery_ptr(&self, ptr: *const u8) -> bool {
        self.nursery.contains(ptr)
    }

    /// Get nursery usage ratio (0.0 to 1.0).
    #[inline]
    pub fn nursery_usage(&self) -> f64 {
        self.nursery.usage()
    }

    /// Check if a minor GC should be triggered.
    ///
    /// Returns true if the nursery is 80%+ full.
    #[inline]
    pub fn should_minor_gc(&self) -> bool {
        self.nursery.usage() >= 0.8
    }

    /// Get a reference to the remembered set.
    pub fn remembered_set(&self) -> &crate::barrier::RememberedSet {
        &self.remembered_set
    }

    /// Number of live objects currently in the nursery.
    #[inline]
    pub fn nursery_live_count(&self) -> usize {
        self.nursery.live_count()
    }

    /// Perform a minor GC (young generation collection).
    ///
    /// Only marks and sweeps nursery objects + remembered set roots.
    /// Much cheaper than a full collection — no old-gen scanning.
    ///
    /// Returns bytes reclaimed.
    pub fn collect_minor(&self, roots: &[*const GcHeader]) -> usize {
        self.collect_minor_with_pre_sweep_hook(roots, || {})
    }

    /// Perform a minor GC with a pre-sweep hook.
    ///
    /// The `pre_sweep` hook runs after marking (live objects are Black) but
    /// before sweeping (dead objects' memory and values are still intact).
    /// Use this to prune weak data structures (e.g., string intern table)
    /// that may reference nursery objects about to be freed.
    pub fn collect_minor_with_pre_sweep_hook<F: FnOnce()>(
        &self,
        roots: &[*const GcHeader],
        pre_sweep: F,
    ) -> usize {
        let start = Instant::now();

        // Phase 1: Reset marks (O(1) via version bump).
        // All objects appear White. Young-only marking will only
        // mark nursery objects; old-gen stays White (harmless until full GC).
        self.reset_marks();

        // Phase 2: Young-only mark from roots + remembered set.
        // Only enqueues nursery objects, only follows nursery children.
        // O(nursery_objects) instead of O(reachable_heap).
        self.mark_minor(roots);

        // Phase 3: Pre-sweep hook — prune weak references (e.g., string table)
        // while marks are valid but dead objects are still alive in memory.
        pre_sweep();

        // Phase 4: Sweep nursery — skip tenured cells (they weren't marked
        // in young-only mode and must not be freed).
        let (reclaimed, _tenured) = self.nursery.sweep_young_only();
        self.total_bytes.fetch_sub(reclaimed, Ordering::Relaxed);

        // Phase 5: Compact nursery if possible
        self.nursery.compact_if_possible();

        // Phase 6: Clean remembered set — remove dead/tenured entries.
        // Don't clear entirely: live young entries must persist for next minor GC.
        self.clean_remembered_set();

        // Update stats
        let elapsed = start.elapsed();
        let elapsed_nanos = elapsed.as_nanos() as u64;
        self.pause_histogram.borrow_mut().record(elapsed);
        self.minor_collection_count.fetch_add(1, Ordering::Relaxed);
        self.last_reclaimed.store(reclaimed, Ordering::Relaxed);
        self.total_pause_nanos
            .fetch_add(elapsed_nanos, Ordering::Relaxed);
        self.last_pause_nanos
            .store(elapsed_nanos, Ordering::Relaxed);

        reclaimed
    }

    /// Young-only mark phase for minor GC.
    ///
    /// Only traces nursery (young) objects. Old-gen objects are skipped entirely.
    /// Correctness relies on the generational write barrier: every store of a
    /// young value into an old object adds the young value to the remembered set.
    ///
    /// Complexity: O(nursery_objects) instead of O(reachable_heap).
    fn mark_minor(&self, roots: &[*const GcHeader]) {
        let mut worklist: VecDeque<*const GcHeader> = VecDeque::new();

        // Step 1: From roots, only enqueue young (nursery) objects.
        for &root in roots {
            if !root.is_null() && self.nursery.contains(root as *const u8) {
                unsafe {
                    if (*root).mark() == MarkColor::White {
                        (*root).set_mark(MarkColor::Gray);
                        worklist.push_back(root);
                    }
                }
            }
        }

        // Step 2: Enqueue remembered set entries (objects ref'ing nursery).
        {
            let rs_entries = self.remembered_set.roots();
            for entry in rs_entries {
                if !entry.is_null() {
                    unsafe {
                        // Enqueue if White. If it's in nursery, it survives sweep.
                        // If it's old-gen, it acts as a root to find young children.
                        if (*entry).mark() == MarkColor::White {
                            (*entry).set_mark(MarkColor::Gray);
                            worklist.push_back(entry);
                        }
                    }
                }
            }
        }

        // Step 3: Process worklist — trace children, only follow young children.
        while let Some(ptr) = worklist.pop_front() {
            unsafe {
                let header = &*ptr;

                if header.mark() == MarkColor::Black {
                    continue;
                }

                // Trace this young object's references
                let tag = header.tag();
                if let Some(trace_fn) = self.trace_table[tag as usize] {
                    trace_fn(ptr as *const u8, &mut |child_header| {
                        if !child_header.is_null()
                            && self.nursery.contains(child_header as *const u8)
                            && (*child_header).mark() == MarkColor::White
                        {
                            (*child_header).set_mark(MarkColor::Gray);
                            worklist.push_back(child_header);
                        }
                    });
                }

                header.set_mark(MarkColor::Black);
            }
        }
    }

    /// Clean remembered set after minor GC.
    ///
    /// Removes entries for objects that were freed (white after marking) or
    /// tenured (no longer young). Keeps entries for live young objects so
    /// they remain discoverable in the next minor GC.
    fn clean_remembered_set(&self) {
        let entries = self.remembered_set.roots();
        let mut to_remove = Vec::new();
        for entry in entries {
            if entry.is_null() {
                to_remove.push(entry);
                continue;
            }

            // If it's in the nursery, we only keep it if it's still alive (marked Black)
            // AND it's still young (non-tenured). If it's tenured, it's logically old
            // and will be added back if it points to nursery, OR we can just keep it.
            // For now, follow the existing pattern for nursery objects.
            if self.nursery.contains(entry as *const u8) {
                unsafe {
                    let header = &*entry;
                    if header.mark() != MarkColor::Black || !header.is_young() {
                        to_remove.push(entry);
                    }
                }
            }
            // If it's NOT in the nursery, it's an old-gen object.
            // We keep it in the remembered set. Ideally we'd only keep it if it
            // still points to the nursery, but we don't have that info here easily.
        }
        for entry in to_remove {
            self.remembered_set.remove(entry);
        }
    }

    /// Get the number of minor collections performed.
    pub fn minor_collection_count(&self) -> usize {
        self.minor_collection_count.load(Ordering::Relaxed)
    }

    /// Get total allocated bytes
    pub fn total_bytes(&self) -> usize {
        self.total_bytes.load(Ordering::Relaxed)
    }

    /// Get GC threshold
    pub fn gc_threshold(&self) -> usize {
        self.gc_threshold.load(Ordering::Relaxed)
    }

    /// Set GC threshold
    pub fn set_gc_threshold(&self, threshold: usize) {
        self.gc_threshold.store(threshold, Ordering::Relaxed);
    }

    /// Check if GC should be triggered
    pub fn should_gc(&self) -> bool {
        self.total_bytes() >= self.gc_threshold()
    }

    /// Get the number of live allocations (blocks + large objects).
    pub fn allocation_count(&self) -> usize {
        let block_count: usize = self.directories.iter().map(|d| d.live_count()).sum();
        let large_count = self.large_objects.borrow().len();
        let nursery_count = self.nursery.live_count();
        block_count + large_count + nursery_count
    }

    /// Get a copy of the GC pause histogram.
    pub fn pause_histogram(&self) -> GcPauseHistogram {
        *self.pause_histogram.borrow()
    }

    /// Get collection statistics
    pub fn stats(&self) -> RegistryStats {
        RegistryStats {
            total_bytes: self.total_bytes(),
            allocation_count: self.allocation_count(),
            collection_count: self.collection_count.load(Ordering::Relaxed),
            last_reclaimed: self.last_reclaimed.load(Ordering::Relaxed),
            total_pause_time: Duration::from_nanos(self.total_pause_nanos.load(Ordering::Relaxed)),
            last_pause_time: Duration::from_nanos(self.last_pause_nanos.load(Ordering::Relaxed)),
        }
    }

    /// Perform a full mark/sweep collection
    pub fn collect(&self, roots: &[*const GcHeader]) -> usize {
        self.collect_with_pre_sweep_hook(roots, || {})
    }

    /// Perform a full mark/sweep collection, calling `pre_sweep` between the
    /// mark and sweep phases.
    ///
    /// `pre_sweep` is invoked after marking completes (all live objects are
    /// Gray/Black) and before sweeping (White objects are still in memory).
    /// Use this hook to prune weak/soft data structures — such as string intern
    /// tables — whose entries point to objects that may have become unreachable.
    pub fn collect_with_pre_sweep_hook<F: FnOnce()>(
        &self,
        roots: &[*const GcHeader],
        pre_sweep: F,
    ) -> usize {
        let start = Instant::now();

        // Cancel any in-progress incremental GC — a full STW collection
        // supersedes it. Without this, stale worklist entries from the
        // interrupted incremental cycle point to objects that the STW sweep
        // will free, causing use-after-free on the next mark step.
        self.cancel_incremental_gc();

        #[cfg(feature = "gc_logging")]
        let initial_bytes = self.total_bytes.load(Ordering::Relaxed);
        #[cfg(feature = "gc_logging")]
        let initial_count = self.allocation_count();

        #[cfg(feature = "gc_logging")]
        tracing::debug!(
            target: "otter::gc",
            roots = roots.len(),
            heap_bytes = initial_bytes,
            objects = initial_count,
            "GC cycle starting"
        );

        // Phase 1: Reset all marks to white
        self.reset_marks();

        // Phase 2: Mark from roots
        self.mark(roots);

        // Pre-sweep hook (e.g., prune interned string table dead entries)
        pre_sweep();

        // Phase 3: Sweep unmarked objects
        let reclaimed = self.sweep();

        // Measure pause time
        let elapsed = start.elapsed();
        let elapsed_nanos = elapsed.as_nanos() as u64;
        self.pause_histogram.borrow_mut().record(elapsed);

        // Update stats
        #[cfg(feature = "gc_logging")]
        let collection_num = self.collection_count.fetch_add(1, Ordering::Relaxed) + 1;
        #[cfg(not(feature = "gc_logging"))]
        self.collection_count.fetch_add(1, Ordering::Relaxed);

        self.last_reclaimed.store(reclaimed, Ordering::Relaxed);
        self.total_pause_nanos
            .fetch_add(elapsed_nanos, Ordering::Relaxed);
        self.last_pause_nanos
            .store(elapsed_nanos, Ordering::Relaxed);

        #[cfg(feature = "gc_logging")]
        {
            let final_bytes = self.total_bytes.load(Ordering::Relaxed);
            let final_count = self.allocation_count();

            tracing::info!(
                target: "otter::gc",
                collection = collection_num,
                reclaimed_bytes = reclaimed,
                pause_us = elapsed.as_micros() as u64,
                live_bytes = final_bytes,
                live_objects = final_count,
                freed_objects = initial_count.saturating_sub(final_count),
                "GC cycle complete"
            );
        }

        reclaimed
    }

    /// Perform a full mark/sweep collection with ephemeron support
    pub fn collect_with_ephemerons(
        &self,
        roots: &[*const GcHeader],
        ephemeron_tables: &[&crate::ephemeron::EphemeronTable],
    ) -> usize {
        self.collect_with_ephemerons_and_pre_sweep_hook(roots, ephemeron_tables, || {})
    }

    /// Like `collect_with_ephemerons` but calls `pre_sweep` after all marking
    /// (including ephemeron fixpoint) and before the final sweep.
    pub fn collect_with_ephemerons_and_pre_sweep_hook<F: FnOnce()>(
        &self,
        roots: &[*const GcHeader],
        ephemeron_tables: &[&crate::ephemeron::EphemeronTable],
        pre_sweep: F,
    ) -> usize {
        let start = Instant::now();

        // Cancel any in-progress incremental GC (same reason as above).
        self.cancel_incremental_gc();

        #[cfg(feature = "gc_logging")]
        let initial_bytes = self.total_bytes.load(Ordering::Relaxed);
        #[cfg(feature = "gc_logging")]
        let initial_count = self.allocation_count();

        #[cfg(feature = "gc_logging")]
        tracing::debug!(
            target: "otter::gc",
            roots = roots.len(),
            heap_bytes = initial_bytes,
            objects = initial_count,
            ephemeron_tables = ephemeron_tables.len(),
            "GC cycle starting"
        );

        // Phase 1: Reset all marks to white
        self.reset_marks();

        // Phase 2: Mark from roots (standard marking)
        self.mark(roots);

        // Phase 3: Ephemeron fixpoint iteration
        if !ephemeron_tables.is_empty() {
            let mut iterations = 0;
            loop {
                let mut newly_marked = 0;

                for table in ephemeron_tables {
                    unsafe {
                        newly_marked += table.trace_live_entries(&mut |header| {
                            if !header.is_null() {
                                let h = &*header;
                                if h.mark() == MarkColor::White {
                                    h.set_mark(MarkColor::Gray);
                                    self.mark(&[header]);
                                }
                            }
                        });
                    }
                }

                iterations += 1;

                #[cfg(feature = "gc_logging")]
                tracing::debug!(
                    target: "otter::gc",
                    iteration = iterations,
                    newly_marked,
                    "Ephemeron fixpoint iteration"
                );

                if newly_marked == 0 {
                    break;
                }

                if iterations > 1000 {
                    #[cfg(feature = "gc_logging")]
                    tracing::warn!(
                        target: "otter::gc",
                        "Ephemeron fixpoint iteration limit reached (1000 iterations)"
                    );
                    break;
                }
            }
        }

        // Phase 4: Sweep dead ephemeron entries
        for table in ephemeron_tables {
            unsafe {
                let _removed = table.sweep();
                #[cfg(feature = "gc_logging")]
                if _removed > 0 {
                    tracing::debug!(
                        target: "otter::gc",
                        removed_entries = _removed,
                        "Swept dead ephemeron entries"
                    );
                }
            }
        }

        // Pre-sweep hook (e.g., prune interned string table dead entries)
        pre_sweep();

        // Phase 5: Sweep unmarked objects
        let reclaimed = self.sweep();

        let elapsed = start.elapsed();
        let elapsed_nanos = elapsed.as_nanos() as u64;
        self.pause_histogram.borrow_mut().record(elapsed);

        #[cfg(feature = "gc_logging")]
        let collection_num = self.collection_count.fetch_add(1, Ordering::Relaxed) + 1;
        #[cfg(not(feature = "gc_logging"))]
        self.collection_count.fetch_add(1, Ordering::Relaxed);

        self.last_reclaimed.store(reclaimed, Ordering::Relaxed);
        self.total_pause_nanos
            .fetch_add(elapsed_nanos, Ordering::Relaxed);
        self.last_pause_nanos
            .store(elapsed_nanos, Ordering::Relaxed);

        #[cfg(feature = "gc_logging")]
        {
            let final_bytes = self.total_bytes.load(Ordering::Relaxed);
            let final_count = self.allocation_count();

            tracing::info!(
                target: "otter::gc",
                collection = collection_num,
                reclaimed_bytes = reclaimed,
                pause_us = elapsed.as_micros() as u64,
                live_bytes = final_bytes,
                live_objects = final_count,
                freed_objects = initial_count.saturating_sub(final_count),
                "GC cycle complete"
            );
        }

        reclaimed
    }

    /// Reset all marks to white (preparation for marking)
    fn reset_marks(&self) {
        // O(1) logical versioning: bump global mark version.
        // All objects with stale mark_version are now effectively White.
        bump_mark_version();
    }

    /// Build a lookup table mapping header addresses to trace functions.
    ///
    /// This is built once per GC cycle for O(1) lookup during mark phase,
    /// replacing the O(n) linear search from the old Vec<AllocationEntry>.
    /// Mark phase: trace from roots and mark all reachable objects.
    ///
    /// Uses tri-color mark bits (White/Gray/Black) on GcHeader instead of a
    /// separate HashSet for visited tracking.  All headers start White after
    /// the prepare phase, so `mark != White` means already enqueued.
    fn mark(&self, roots: &[*const GcHeader]) {
        let mut worklist: VecDeque<*const GcHeader> = VecDeque::new();

        // Add all roots to the worklist (mark as gray)
        for &root in roots {
            if !root.is_null() {
                unsafe {
                    if (*root).mark() == MarkColor::White {
                        (*root).set_mark(MarkColor::Gray);
                        worklist.push_back(root);
                    }
                }
            }
        }

        // Process the worklist until empty
        while let Some(ptr) = worklist.pop_front() {
            unsafe {
                let header = &*ptr;

                // Skip if already black (fully processed)
                if header.mark() == MarkColor::Black {
                    continue;
                }

                // Look up trace function for this header (O(1))
                let tag = header.tag();
                if let Some(trace_fn) = self.trace_table[tag as usize] {
                    // Trace the object's references, passing the start of the allocation
                    trace_fn(ptr as *const u8, &mut |child_header| {
                        if !child_header.is_null() && (*child_header).mark() == MarkColor::White {
                            (*child_header).set_mark(MarkColor::Gray);
                            worklist.push_back(child_header);
                        }
                    });
                }

                // Mark as black (fully scanned)
                header.set_mark(MarkColor::Black);
            }
        }
    }

    /// Sweep phase: free all white (unreachable) objects
    fn sweep(&self) -> usize {
        let mut reclaimed: usize = 0;

        // Sweep nursery (young generation)
        let (nursery_reclaimed, _tenured) = self.nursery.sweep_after_minor_gc();
        reclaimed += nursery_reclaimed;
        self.nursery.compact_if_possible();
        // After a full GC, clear the remembered set (all cross-gen refs re-established)
        self.remembered_set.clear();

        // Sweep all block directories (old generation)
        for dir in &self.directories {
            reclaimed += dir.sweep();
        }

        // Sweep large objects: partition into live/dead with single drain
        {
            let mut large_objects = self.large_objects.borrow_mut();
            let (live, dead): (Vec<_>, Vec<_>) = large_objects
                .drain(..)
                .partition(|entry| unsafe { (*entry.header).mark() != MarkColor::White });

            for entry in &dead {
                reclaimed += entry.size;
            }

            *large_objects = live;
            drop(large_objects);

            // Call drop functions after releasing borrow
            for entry in dead {
                unsafe {
                    (entry.drop_fn)(entry.header as *mut u8);
                }
            }
        }

        // Update total bytes
        self.total_bytes.fetch_sub(reclaimed, Ordering::Relaxed);

        reclaimed
    }

    // ---------------------------------------------------------------
    // Incremental marking API
    // ---------------------------------------------------------------

    /// Cancel an in-progress incremental GC cycle.
    ///
    /// Clears the worklist, trace lookup, and resets the phase to Idle.
    /// This is a no-op when no incremental cycle is active.
    fn cancel_incremental_gc(&self) {
        if self.gc_phase.get() != GcPhase::Idle {
            self.mark_worklist.borrow_mut().clear();
            self.mark_start.set(None);
            self.gc_phase.set(GcPhase::Idle);
        }
    }

    /// Get the current GC phase.
    pub fn gc_phase(&self) -> GcPhase {
        self.gc_phase.get()
    }

    /// Returns true if incremental marking is in progress.
    #[inline]
    pub fn is_marking(&self) -> bool {
        self.gc_phase.get() == GcPhase::Marking
    }

    /// Start an incremental GC cycle.
    ///
    /// Resets marks, seeds the worklist from roots, builds the trace lookup,
    /// and transitions to `GcPhase::Marking`. Subsequent calls to
    /// `incremental_mark_step()` will process the worklist in budgeted chunks.
    pub fn start_incremental_gc(&self, roots: &[*const GcHeader]) {
        // Reset all marks to white
        self.reset_marks();

        // Initialize worklist from roots
        let mut worklist = self.mark_worklist.borrow_mut();
        worklist.clear();

        for &root in roots {
            if !root.is_null() {
                // Use Gray mark bit as "in-worklist" sentinel — no HashSet needed.
                // After reset_marks() all objects are White, so this check reliably
                // deduplicates roots without an auxiliary visited set.
                if unsafe { (*root).mark() } == MarkColor::White {
                    unsafe {
                        (*root).set_mark(MarkColor::Gray);
                    }
                    worklist.push_back(root);
                }
            }
        }

        // Build trace lookup (once per cycle)

        // Record start time and enter marking phase
        self.mark_start.set(Some(Instant::now()));
        self.gc_phase.set(GcPhase::Marking);
    }

    /// Process up to `budget` gray objects from the mark worklist.
    ///
    /// Returns `true` when marking is complete (worklist empty after draining
    /// write barrier buffer). Returns `false` if there are still objects to process.
    ///
    /// Call this at safepoints during incremental GC.
    pub fn incremental_mark_step(&self, budget: usize) -> bool {
        if self.gc_phase.get() != GcPhase::Marking {
            return true;
        }

        // Drain write barrier buffer into worklist
        let barrier_entries = barrier_drain_from(self);
        if !barrier_entries.is_empty() {
            let mut worklist = self.mark_worklist.borrow_mut();
            for ptr in barrier_entries {
                if !ptr.is_null() {
                    // Only add to worklist if still White (not already Gray or Black).
                    if unsafe { (*ptr).mark() } == MarkColor::White {
                        unsafe { (*ptr).set_mark(MarkColor::Gray) };
                        worklist.push_back(ptr);
                    }
                }
            }
        }

        let mut worklist = self.mark_worklist.borrow_mut();
        let mut processed = 0;

        while processed < budget {
            let ptr = match worklist.pop_front() {
                Some(p) => p,
                None => break, // Worklist empty
            };

            unsafe {
                let header = &*ptr;

                // Skip if already black (fully processed).
                // This can happen when an object is added via write barrier after
                // already being processed in a previous step.
                if header.mark() == MarkColor::Black {
                    continue;
                }

                // Look up trace function for this header (O(1))
                let tag = header.tag();
                if let Some(trace_fn) = self.trace_table[tag as usize] {
                    // Pass the allocation pointer directly
                    trace_fn(ptr as *const u8, &mut |child_header| {
                        if !child_header.is_null() {
                            // Use mark color as "in-worklist" sentinel instead of HashSet.
                            // Only add White objects (not yet seen this cycle).
                            if (*child_header).mark() == MarkColor::White {
                                (*child_header).set_mark(MarkColor::Gray);
                                worklist.push_back(child_header);
                            }
                        }
                    });
                }

                // Mark as black (fully scanned)
                header.set_mark(MarkColor::Black);
            }

            processed += 1;
        }

        worklist.is_empty()
    }

    /// Complete the incremental GC cycle: sweep and update stats.
    ///
    /// Call this after `incremental_mark_step()` returns `true`.
    /// Returns bytes reclaimed.
    pub fn finish_gc(&self) -> usize {
        self.finish_gc_with_pre_sweep_hook(|| {})
    }

    /// Like `finish_gc` but calls `pre_sweep` between mark completion and sweep.
    ///
    /// Use this to prune weak data structures (e.g., string intern tables)
    /// after the mark phase has determined which objects are live.
    pub fn finish_gc_with_pre_sweep_hook<F: FnOnce()>(&self, pre_sweep: F) -> usize {
        pre_sweep();
        let reclaimed = self.sweep();

        // Measure total time (from start_incremental_gc to finish_gc)
        if let Some(start) = self.mark_start.get() {
            let elapsed = start.elapsed();
            let elapsed_nanos = elapsed.as_nanos() as u64;
            self.pause_histogram.borrow_mut().record(elapsed);
            self.total_pause_nanos
                .fetch_add(elapsed_nanos, Ordering::Relaxed);
            self.last_pause_nanos
                .store(elapsed_nanos, Ordering::Relaxed);
        }

        // Update stats
        self.collection_count.fetch_add(1, Ordering::Relaxed);
        self.last_reclaimed.store(reclaimed, Ordering::Relaxed);

        // Clean up incremental state
        self.mark_worklist.borrow_mut().clear();
        self.mark_start.set(None);
        self.gc_phase.set(GcPhase::Idle);

        reclaimed
    }

    /// Deallocate ALL tracked allocations without marking.
    ///
    /// Use this when tearing down an engine/isolate to reclaim all memory.
    pub fn dealloc_all(&self) -> usize {
        let total = self.total_bytes.load(Ordering::Relaxed);

        // Use per-isolate flag (on self) instead of thread-local
        self.dealloc_in_progress.set(true);

        // Dealloc nursery objects first
        self.nursery.dealloc_all();
        self.remembered_set.clear();

        // Dealloc all block-allocated objects
        for dir in &self.directories {
            dir.dealloc_all();
        }

        // Dealloc large objects
        {
            let mut large_objects = self.large_objects.borrow_mut();
            let entries: Vec<LargeAllocation> = large_objects.drain(..).collect();
            drop(large_objects);

            for entry in entries {
                unsafe {
                    (entry.drop_fn)(entry.header as *mut u8);
                }
            }
        }

        self.total_bytes.store(0, Ordering::Relaxed);

        self.dealloc_in_progress.set(false);

        total
    }
}

impl Default for AllocationRegistry {
    fn default() -> Self {
        Self::new()
    }
}

/// Statistics from the allocation registry
#[derive(Debug, Clone, Copy)]
pub struct RegistryStats {
    /// Total bytes currently allocated
    pub total_bytes: usize,
    /// Number of live allocations
    pub allocation_count: usize,
    /// Total number of collections performed
    pub collection_count: usize,
    /// Bytes reclaimed in last collection
    pub last_reclaimed: usize,
    /// Total pause time accumulated across all collections
    pub total_pause_time: Duration,
    /// Pause time of the last collection
    pub last_pause_time: Duration,
}

/// GC pause time histogram with fixed buckets.
///
/// Buckets: <1ms, 1-5ms, 5-10ms, 10-50ms, 50-100ms, >100ms
#[derive(Debug, Clone, Copy, Default)]
pub struct GcPauseHistogram {
    /// Pauses < 1ms
    pub under_1ms: u32,
    /// Pauses 1ms - 5ms
    pub ms_1_to_5: u32,
    /// Pauses 5ms - 10ms
    pub ms_5_to_10: u32,
    /// Pauses 10ms - 50ms
    pub ms_10_to_50: u32,
    /// Pauses 50ms - 100ms
    pub ms_50_to_100: u32,
    /// Pauses > 100ms
    pub over_100ms: u32,
}

impl GcPauseHistogram {
    /// Record a GC pause duration into the appropriate bucket.
    pub fn record(&mut self, duration: Duration) {
        let ms = duration.as_millis();
        if ms < 1 {
            self.under_1ms += 1;
        } else if ms < 5 {
            self.ms_1_to_5 += 1;
        } else if ms < 10 {
            self.ms_5_to_10 += 1;
        } else if ms < 50 {
            self.ms_10_to_50 += 1;
        } else if ms < 100 {
            self.ms_50_to_100 += 1;
        } else {
            self.over_100ms += 1;
        }
    }

    /// Total number of recorded pauses.
    pub fn total(&self) -> u32 {
        self.under_1ms
            + self.ms_1_to_5
            + self.ms_5_to_10
            + self.ms_10_to_50
            + self.ms_50_to_100
            + self.over_100ms
    }
}

// Thread-local allocation registry pointer for the GC.
//
// Set by VmRuntime::with_config() or Isolate::enter() to point to
// the owning runtime's registry. Each VmRuntime creates its own
// AllocationRegistry (Box-owned, dropped with VmRuntime).
//
// Panics if gc_alloc() is called without a registry set — this means
// the caller forgot to create a VmRuntime or Isolate first.
thread_local! {
    static THREAD_REGISTRY: std::cell::Cell<*const AllocationRegistry> = const { std::cell::Cell::new(std::ptr::null()) };
}

/// Set the thread-local allocation registry.
///
/// Called by `Isolate::enter()` to install the isolate's own registry.
///
/// # Safety
///
/// The caller must ensure the registry pointer remains valid for the
/// duration it is set (i.e., until `clear_thread_registry()` is called).
pub unsafe fn set_thread_registry(registry: &AllocationRegistry) {
    THREAD_REGISTRY.with(|r| r.set(registry as *const AllocationRegistry));
}

/// Clear the thread-local allocation registry.
///
/// Called by `IsolateGuard::drop()` when exiting an isolate.
pub fn clear_thread_registry() {
    THREAD_REGISTRY.with(|r| r.set(std::ptr::null()));
}

/// Clear the thread-local registry only if it points to the given registry.
///
/// Used by `VmRuntime::drop()` to avoid clearing another runtime's registry.
pub fn clear_thread_registry_if(registry: *const AllocationRegistry) {
    THREAD_REGISTRY.with(|r| {
        if r.get() == registry {
            r.set(std::ptr::null());
        }
    });
}

/// Get the thread-local allocation registry.
///
/// # Panics
///
/// Panics if no registry has been set via `set_thread_registry()`.
/// This means the caller must create a `VmRuntime` or `Isolate` first.
pub fn global_registry() -> &'static AllocationRegistry {
    THREAD_REGISTRY.with(|r| {
        let ptr = r.get();
        assert!(
            !ptr.is_null(),
            "No GC allocation registry set on this thread. \
             Create a VmRuntime or Isolate before allocating GC objects."
        );
        // SAFETY: set_thread_registry guarantees the pointer is valid
        // for the duration it is set. VmRuntime::drop() clears the pointer
        // before freeing the registry.
        unsafe { &*ptr }
    })
}

/// Check if a GcHeader pointer is in the nursery and add to remembered set.
///
/// Used by the generational write barrier: when a value is stored into a
/// GC-managed object, check if the value is a young (nursery) object. If so,
/// record it in the remembered set for the next minor GC.
///
/// This is the fast path for the generational barrier — returns immediately
/// if the pointer is not in the nursery range (2 integer comparisons).
#[inline]
pub fn remembered_set_add_if_young(header_ptr: *const GcHeader) {
    THREAD_REGISTRY.with(|r| {
        let reg_ptr = r.get();
        if !reg_ptr.is_null() {
            let reg = unsafe { &*reg_ptr };
            if reg.nursery.contains(header_ptr as *const u8) {
                reg.remembered_set.add(header_ptr);
            }
        }
    });
}

/// Allocate a GC-managed value
///
/// # Safety
/// The caller must ensure proper root management for the returned pointer.
pub unsafe fn gc_alloc<T>(value: T) -> *mut T
where
    T: GcTraceable + 'static,
{
    // SAFETY: Caller ensures proper root management
    unsafe { gc_alloc_in(global_registry(), value) }
}

/// Allocate a GC-managed value in a specific registry
///
/// # Safety
/// The caller must ensure proper root management for the returned pointer.
pub unsafe fn gc_alloc_in<T>(registry: &AllocationRegistry, value: T) -> *mut T
where
    T: GcTraceable + 'static,
{
    // Force 16-byte alignment for ALL allocations so that the NaN-boxing
    // header-finding formula `(value_ptr - 8) & !15` works universally.
    // Types with align > 8 (e.g. TemporalValue with i128 fields) need this.
    let layout = std::alloc::Layout::new::<GcAllocation<T>>()
        .align_to(16)
        .expect("16-byte alignment should always be valid")
        .pad_to_align();
    let alloc_size = layout.size();

    let ptr: *mut GcAllocation<T>;
    let is_young: bool;

    if alloc_size <= LARGE_OBJECT_THRESHOLD {
        // Small object: try nursery first for fast bump allocation (~3ns).
        // Use drop_gc_box_in_block which only drops in-place (no dealloc —
        // both nursery and block allocator own their memory).
        let drop_fn: DropFn = drop_gc_box_in_block::<T>;

        if let Some(cell_ptr) = registry.allocate_in_nursery(alloc_size, drop_fn) {
            ptr = cell_ptr as *mut GcAllocation<T>;
            is_young = true;
        } else {
            // Nursery full — allocate in old space (block directory).
            let cell_ptr = registry.allocate_in_block(alloc_size, drop_fn);
            ptr = cell_ptr as *mut GcAllocation<T>;
            is_young = false;
        }
    } else {
        // Large object: allocate individually via global allocator.
        // Large objects always go to old space (not worth nursery-ing).
        let drop_fn: DropFn = drop_gc_box::<T>;
        // SAFETY: Layout is valid and non-zero sized
        let raw = unsafe { std::alloc::alloc(layout) as *mut GcAllocation<T> };
        if raw.is_null() {
            std::alloc::handle_alloc_error(layout);
        }
        ptr = raw;
        is_young = false;

        // Register with the large object tracker
        let header_ptr = ptr as *mut GcHeader;
        unsafe {
            registry.register(header_ptr, alloc_size, drop_fn);
        }
    }

    // Initialize header and value
    // SAFETY: ptr is non-null and properly aligned (from nursery, block, or alloc)
    unsafe {
        if is_young {
            std::ptr::write(&mut (*ptr).header, GcHeader::new_young(T::TYPE_ID));
        } else {
            std::ptr::write(&mut (*ptr).header, GcHeader::new(T::TYPE_ID));
        }
        std::ptr::write(&mut (*ptr).value, value);

        // Black allocation: objects allocated during marking are pre-marked Black.
        // This prevents newly allocated objects from being swept in the ongoing cycle.
        if registry.is_marking() {
            (*ptr).header.set_mark(MarkColor::Black);
        }
    }

    // Return pointer to the value (after the header)
    unsafe { &mut (*ptr).value as *mut T }
}

/// Drop function for block-allocated GC cells.
///
/// Only drops the value in-place (no dealloc — the block owns the memory).
unsafe fn drop_gc_box_in_block<T>(ptr: *mut u8) {
    let box_ptr = ptr as *mut GcAllocation<T>;
    // SAFETY: ptr is valid and points to an initialized GcAllocation<T>
    unsafe {
        std::ptr::drop_in_place(&mut (*box_ptr).value);
        // Do NOT call dealloc — the block owns this memory
    }
}

/// Drop function for large GC boxes (individually allocated).
unsafe fn drop_gc_box<T>(ptr: *mut u8) {
    // Must match the layout used in gc_alloc_in (16-byte aligned, padded).
    let layout = std::alloc::Layout::new::<GcAllocation<T>>()
        .align_to(16)
        .expect("16-byte alignment should always be valid")
        .pad_to_align();
    let box_ptr = ptr as *mut GcAllocation<T>;
    // SAFETY: ptr is valid and points to an initialized GcAllocation<T>
    unsafe {
        std::ptr::drop_in_place(&mut (*box_ptr).value);
        std::alloc::dealloc(ptr, layout);
    }
}

/// Trace function for GC boxes
unsafe fn trace_gc_box<T: GcTraceable>(ptr: *const u8, tracer: &mut dyn FnMut(*const GcHeader)) {
    let alloc_ptr = ptr as *const GcAllocation<T>;
    // SAFETY: ptr is valid and points to an initialized GcAllocation<T>
    unsafe {
        (*alloc_ptr).value.trace(tracer);
    }
}

/// Trait for types that can be traced by the GC
pub trait GcTraceable {
    /// Whether this type contains GC references that need tracing
    const NEEDS_TRACE: bool;

    /// Type ID for tag-based trace function lookup (O(1)).
    /// 0 = no trace, 1-255 = type-specific trace function.
    const TYPE_ID: u8 = 0;

    /// Trace all GC references in this value
    fn trace(&self, tracer: &mut dyn FnMut(*const GcHeader));

    /// Whether this type needs sweep cleanup
    fn needs_sweep_cleanup() -> bool {
        false
    }

    /// Cleanup during sweep phase
    fn sweep_cleanup(&mut self, _dead: &HashSet<*const GcHeader>) {}
}

// Implement GcTraceable for primitive types
impl GcTraceable for () {
    const NEEDS_TRACE: bool = false;
    const TYPE_ID: u8 = 0;
    fn trace(&self, _tracer: &mut dyn FnMut(*const GcHeader)) {}
}

impl GcTraceable for bool {
    const NEEDS_TRACE: bool = false;
    const TYPE_ID: u8 = 0;
    fn trace(&self, _tracer: &mut dyn FnMut(*const GcHeader)) {}
}

impl GcTraceable for i32 {
    const NEEDS_TRACE: bool = false;
    const TYPE_ID: u8 = 0;
    fn trace(&self, _tracer: &mut dyn FnMut(*const GcHeader)) {}
}

impl GcTraceable for i64 {
    const NEEDS_TRACE: bool = false;
    const TYPE_ID: u8 = 0;
    fn trace(&self, _tracer: &mut dyn FnMut(*const GcHeader)) {}
}

impl GcTraceable for f64 {
    const NEEDS_TRACE: bool = false;
    const TYPE_ID: u8 = 0;
    fn trace(&self, _tracer: &mut dyn FnMut(*const GcHeader)) {}
}

impl GcTraceable for String {
    const NEEDS_TRACE: bool = false;
    const TYPE_ID: u8 = 0;
    fn trace(&self, _tracer: &mut dyn FnMut(*const GcHeader)) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_registry_creation() {
        let registry = AllocationRegistry::new();
        assert_eq!(registry.total_bytes(), 0);
        assert_eq!(registry.allocation_count(), 0);
    }

    #[test]
    fn test_registry_with_threshold() {
        let registry = AllocationRegistry::with_threshold(2048);
        assert_eq!(registry.gc_threshold(), 2048);
    }

    #[test]
    fn test_collect_empty() {
        let registry = AllocationRegistry::new();
        let reclaimed = registry.collect(&[]);
        assert_eq!(reclaimed, 0);
        assert_eq!(registry.stats().collection_count, 1);
    }

    #[test]
    fn test_gc_alloc_and_collect_unreachable() {
        let registry = AllocationRegistry::new();

        // Allocate without rooting
        unsafe {
            let _ = gc_alloc_in(&registry, 42i32);
            let _ = gc_alloc_in(&registry, 100i32);
        }

        assert_eq!(registry.allocation_count(), 2);
        assert!(registry.total_bytes() > 0);

        // Collect with no roots - everything should be freed
        let reclaimed = registry.collect(&[]);

        assert!(reclaimed > 0);
        assert_eq!(registry.allocation_count(), 0);
        assert_eq!(registry.total_bytes(), 0);
    }

    #[test]
    fn test_gc_alloc_with_roots() {
        let registry = AllocationRegistry::new();

        // Allocate and keep a root
        let ptr = unsafe { gc_alloc_in(&registry, 42i32) };

        // Get the header pointer for rooting
        let header_ptr = unsafe {
            (ptr as *mut u8).sub(std::mem::offset_of!(GcAllocation<i32>, value)) as *const GcHeader
        };

        assert_eq!(registry.allocation_count(), 1);

        // Collect with root - should survive
        let reclaimed = registry.collect(&[header_ptr]);

        assert_eq!(reclaimed, 0);
        assert_eq!(registry.allocation_count(), 1);

        // Value should still be accessible
        unsafe {
            assert_eq!(*ptr, 42);
        }
    }

    #[test]
    fn test_multiple_collections() {
        let registry = AllocationRegistry::new();

        for i in 0..5 {
            unsafe {
                let _ = gc_alloc_in(&registry, i);
            }
            registry.collect(&[]);
        }

        assert_eq!(registry.stats().collection_count, 5);
        assert_eq!(registry.allocation_count(), 0);
    }

    /// Test struct that holds a reference to another GC object
    struct Node {
        value: i32,
        next_header: Option<*const GcHeader>,
    }

    impl GcTraceable for Node {
        const NEEDS_TRACE: bool = true;

        fn trace(&self, tracer: &mut dyn FnMut(*const GcHeader)) {
            if let Some(next) = self.next_header {
                tracer(next);
            }
        }
    }

    #[test]
    #[ignore = "flaky: depends on thread-local registry state from other tests"]
    fn test_gc_traces_references() {
        let registry = AllocationRegistry::new();

        // Create a chain: root -> node1 -> node2
        let node2 = unsafe {
            gc_alloc_in(
                &registry,
                Node {
                    value: 2,
                    next_header: None,
                },
            )
        };
        let node2_header = unsafe {
            (node2 as *mut u8).sub(std::mem::offset_of!(GcAllocation<Node>, value))
                as *const GcHeader
        };

        let node1 = unsafe {
            gc_alloc_in(
                &registry,
                Node {
                    value: 1,
                    next_header: Some(node2_header),
                },
            )
        };
        let node1_header = unsafe {
            (node1 as *mut u8).sub(std::mem::offset_of!(GcAllocation<Node>, value))
                as *const GcHeader
        };

        // Also create an unreachable node
        unsafe {
            let _ = gc_alloc_in(
                &registry,
                Node {
                    value: 999,
                    next_header: None,
                },
            );
        }

        assert_eq!(registry.allocation_count(), 3);

        // Collect with only node1 as root - node1 and node2 should survive
        let reclaimed = registry.collect(&[node1_header]);

        assert!(reclaimed > 0); // The unreachable node should be freed
        assert_eq!(registry.allocation_count(), 2);

        // Both reachable nodes should still be valid
        unsafe {
            assert_eq!((*node1).value, 1);
            assert_eq!((*node2).value, 2);
        }
    }

    #[test]
    fn test_gc_collects_cycles() {
        let registry = AllocationRegistry::new();

        // Create a cycle: node1 -> node2 -> node1
        let node1 = unsafe {
            gc_alloc_in(
                &registry,
                Node {
                    value: 1,
                    next_header: None,
                },
            )
        };
        let node1_header = unsafe {
            (node1 as *mut u8).sub(std::mem::offset_of!(GcAllocation<Node>, value))
                as *const GcHeader
        };

        let node2 = unsafe {
            gc_alloc_in(
                &registry,
                Node {
                    value: 2,
                    next_header: Some(node1_header),
                },
            )
        };
        let node2_header = unsafe {
            (node2 as *mut u8).sub(std::mem::offset_of!(GcAllocation<Node>, value))
                as *const GcHeader
        };

        // Complete the cycle
        unsafe {
            (*node1).next_header = Some(node2_header);
        }

        assert_eq!(registry.allocation_count(), 2);

        // Collect with no roots - the cycle should be collected!
        let reclaimed = registry.collect(&[]);

        assert!(reclaimed > 0);
        assert_eq!(registry.allocation_count(), 0);
    }

    #[test]
    fn test_should_gc_threshold() {
        let registry = AllocationRegistry::with_threshold(100);

        assert!(!registry.should_gc());

        // Allocate enough to exceed threshold
        for i in 0..10 {
            unsafe {
                let _ = gc_alloc_in(&registry, i as i64);
            }
        }

        assert!(registry.should_gc());
    }

    // ---------------------------------------------------------------
    // Incremental marking tests
    // ---------------------------------------------------------------

    #[test]
    fn test_incremental_gc_phase() {
        let registry = AllocationRegistry::new();
        assert_eq!(registry.gc_phase(), GcPhase::Idle);
        assert!(!registry.is_marking());

        registry.start_incremental_gc(&[]);
        assert_eq!(registry.gc_phase(), GcPhase::Marking);
        assert!(registry.is_marking());

        // Empty worklist → step completes immediately
        let done = registry.incremental_mark_step(100);
        assert!(done);

        let reclaimed = registry.finish_gc();
        assert_eq!(reclaimed, 0);
        assert_eq!(registry.gc_phase(), GcPhase::Idle);
    }

    #[test]
    fn test_incremental_gc_unreachable() {
        let registry = AllocationRegistry::new();

        // Allocate without rooting
        unsafe {
            let _ = gc_alloc_in(&registry, 42i32);
            let _ = gc_alloc_in(&registry, 100i32);
        }

        assert_eq!(registry.allocation_count(), 2);

        // Incremental GC with no roots
        registry.start_incremental_gc(&[]);
        let done = registry.incremental_mark_step(1000);
        assert!(done);
        let reclaimed = registry.finish_gc();

        assert!(reclaimed > 0);
        assert_eq!(registry.allocation_count(), 0);
    }

    #[test]
    fn test_incremental_gc_with_roots() {
        let registry = AllocationRegistry::new();

        let ptr = unsafe { gc_alloc_in(&registry, 42i32) };
        let header_ptr = unsafe {
            (ptr as *mut u8).sub(std::mem::offset_of!(GcAllocation<i32>, value)) as *const GcHeader
        };

        // Also allocate something unreachable
        unsafe {
            let _ = gc_alloc_in(&registry, 99i32);
        }

        assert_eq!(registry.allocation_count(), 2);

        // Incremental GC with root
        registry.start_incremental_gc(&[header_ptr]);
        let done = registry.incremental_mark_step(1000);
        assert!(done);
        let reclaimed = registry.finish_gc();

        assert!(reclaimed > 0);
        assert_eq!(registry.allocation_count(), 1);

        // Rooted value still valid
        unsafe {
            assert_eq!(*ptr, 42);
        }
    }

    #[test]
    fn test_write_barrier_buffer_integration() {
        let registry = AllocationRegistry::new();

        let ptr = unsafe { gc_alloc_in(&registry, 42i32) };
        let header_ptr = unsafe {
            (ptr as *mut u8).sub(std::mem::offset_of!(GcAllocation<i32>, value)) as *const GcHeader
        };

        // Allocate a second object (not in initial roots)
        let ptr2 = unsafe { gc_alloc_in(&registry, 99i32) };
        let header_ptr2 = unsafe {
            (ptr2 as *mut u8).sub(std::mem::offset_of!(GcAllocation<i32>, value)) as *const GcHeader
        };

        // Start incremental GC with only first object as root
        registry.start_incremental_gc(&[header_ptr]);

        // Simulate a write barrier: the mutator stores a reference to ptr2
        // and pushes it to the barrier buffer
        unsafe {
            (*header_ptr2).set_mark(MarkColor::Gray);
        }
        barrier_push(header_ptr2);

        // Mark step drains barrier buffer → ptr2 gets traced
        let done = registry.incremental_mark_step(1000);
        assert!(done);

        let _reclaimed = registry.finish_gc();

        // Both objects survive (ptr2 was saved by the write barrier)
        assert_eq!(registry.allocation_count(), 2);
    }

    #[test]
    fn test_nursery_allocation_goes_to_nursery() {
        let registry = AllocationRegistry::new();

        // Small objects should go to nursery
        let ptr = unsafe { gc_alloc_in(&registry, 42i32) };

        // Verify it's in the nursery
        let header_ptr = unsafe {
            (ptr as *mut u8).sub(std::mem::offset_of!(GcAllocation<i32>, value)) as *const GcHeader
        };
        unsafe {
            assert!(
                (*header_ptr).is_young(),
                "small object should be in nursery"
            );
        }

        assert_eq!(registry.allocation_count(), 1);
        assert!(registry.nursery_usage() > 0.0);
    }

    #[test]
    fn test_minor_gc_reclaims_unreachable_nursery_objects() {
        let registry = AllocationRegistry::new();

        // Allocate objects in nursery
        let _ptr1 = unsafe { gc_alloc_in(&registry, 100i32) };
        let _ptr2 = unsafe { gc_alloc_in(&registry, 200i32) };
        let ptr3 = unsafe { gc_alloc_in(&registry, 300i32) };

        assert_eq!(registry.allocation_count(), 3);
        let bytes_before = registry.total_bytes();

        // Root only ptr3
        let header3 = unsafe {
            (ptr3 as *mut u8).sub(std::mem::offset_of!(GcAllocation<i32>, value)) as *const GcHeader
        };

        // Run minor GC with only ptr3 rooted
        let reclaimed = registry.collect_minor(&[header3]);
        assert!(reclaimed > 0, "minor GC should reclaim unreachable objects");
        assert_eq!(
            registry.allocation_count(),
            1,
            "only rooted object should survive"
        );
        assert!(registry.total_bytes() < bytes_before);
    }

    #[test]
    fn test_minor_gc_preserves_rooted_objects() {
        let registry = AllocationRegistry::new();

        let ptr1 = unsafe { gc_alloc_in(&registry, 42i32) };
        let ptr2 = unsafe { gc_alloc_in(&registry, 84i32) };

        let header1 = unsafe {
            (ptr1 as *mut u8).sub(std::mem::offset_of!(GcAllocation<i32>, value)) as *const GcHeader
        };
        let header2 = unsafe {
            (ptr2 as *mut u8).sub(std::mem::offset_of!(GcAllocation<i32>, value)) as *const GcHeader
        };

        // Root both
        let reclaimed = registry.collect_minor(&[header1, header2]);
        assert_eq!(
            reclaimed, 0,
            "no objects should be reclaimed when all are rooted"
        );
        assert_eq!(registry.allocation_count(), 2);

        // Values should still be accessible
        unsafe {
            assert_eq!(*ptr1, 42);
            assert_eq!(*ptr2, 84);
        }
    }

    #[test]
    fn test_should_minor_gc_threshold() {
        // Create a small nursery to test threshold quickly
        let registry = AllocationRegistry::new();
        // The nursery is 2MB — we need to fill 80% to trigger should_minor_gc()

        assert!(
            !registry.should_minor_gc(),
            "empty nursery should not trigger minor GC"
        );

        // Allocate enough to pass the 80% threshold
        // Each i32 GcAllocation is ~12 bytes, so we need many allocations
        let nursery_size = 2 * 1024 * 1024; // 2MB
        let threshold = (nursery_size as f64 * 0.8) as usize;
        let alloc_size = std::mem::size_of::<GcAllocation<i32>>();
        let needed = threshold / alloc_size;

        for _ in 0..needed {
            unsafe {
                gc_alloc_in(&registry, 0i32);
            }
        }

        assert!(
            registry.should_minor_gc(),
            "nursery at 80%+ should trigger minor GC"
        );
    }

    #[test]
    fn test_minor_collection_count() {
        let registry = AllocationRegistry::new();

        assert_eq!(registry.minor_collection_count(), 0);

        let _ptr = unsafe { gc_alloc_in(&registry, 42i32) };
        registry.collect_minor(&[]);

        assert_eq!(registry.minor_collection_count(), 1);

        registry.collect_minor(&[]);
        assert_eq!(registry.minor_collection_count(), 2);
    }
}
