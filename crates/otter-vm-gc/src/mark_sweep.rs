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

use rustc_hash::FxHashMap;

use crate::marked_block::{
    BlockDirectory, DropFn, LARGE_OBJECT_THRESHOLD, NUM_SIZE_CLASSES, TraceFn,
    size_class_cell_size, size_class_index,
};
use crate::object::{GcHeader, MarkColor, bump_mark_version};

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

/// Push a grayed object to the thread-local write barrier buffer.
///
/// Called from insertion barriers when GC is in Marking phase.
/// The buffer is drained into the mark worklist during `incremental_mark_step()`.
pub fn barrier_push(ptr: *const GcHeader) {
    WRITE_BARRIER_BUF.with(|buf| buf.borrow_mut().push(ptr));
}

/// Drain the thread-local write barrier buffer.
///
/// Returns all accumulated barrier entries and empties the buffer.
fn barrier_drain() -> Vec<*const GcHeader> {
    WRITE_BARRIER_BUF.with(|buf| std::mem::take(&mut *buf.borrow_mut()))
}

/// Returns true if the GC is currently freeing all allocations on this thread.
/// Drop impls should check this and skip any access to other GC objects.
pub fn is_dealloc_in_progress() -> bool {
    GC_DEALLOC_IN_PROGRESS.with(|f| f.get())
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
    /// Trace function for this allocation
    trace_fn: Option<TraceFn>,
}

// SAFETY: LargeAllocation contains raw pointers but they are managed exclusively
// by the AllocationRegistry on a single thread (thread_local storage).
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
    /// Per-size-class block directories for small objects.
    /// Index = size class index (0..NUM_SIZE_CLASSES).
    directories: Vec<BlockDirectory>,
    /// Large objects tracked individually.
    large_objects: RefCell<Vec<LargeAllocation>>,
    /// Total bytes allocated (across blocks + large objects).
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

    // --- Incremental marking state ---
    /// Current GC phase (Idle or Marking)
    gc_phase: Cell<GcPhase>,
    /// Persistent worklist for incremental marking (gray objects to process)
    mark_worklist: RefCell<VecDeque<*const GcHeader>>,
    /// Visited set to prevent re-adding objects to worklist
    mark_visited: RefCell<HashSet<usize>>,
    /// Cached trace function lookup table (built once when marking starts)
    trace_lookup: RefCell<Option<FxHashMap<usize, Option<TraceFn>>>>,
    /// Timestamp when incremental marking started (for pause time tracking)
    mark_start: Cell<Option<Instant>>,
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
            gc_phase: Cell::new(GcPhase::Idle),
            mark_worklist: RefCell::new(VecDeque::new()),
            mark_visited: RefCell::new(HashSet::new()),
            trace_lookup: RefCell::new(None),
            mark_start: Cell::new(None),
        }
    }

    /// Create a new registry with a custom GC threshold
    pub fn with_threshold(threshold: usize) -> Self {
        let registry = Self::new();
        registry.gc_threshold.store(threshold, Ordering::Relaxed);
        registry
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
    pub unsafe fn register(
        &self,
        header: *mut GcHeader,
        size: usize,
        drop_fn: DropFn,
        trace_fn: Option<TraceFn>,
    ) {
        // This is the legacy path for objects allocated externally (via alloc::alloc).
        // New allocations should go through allocate_in_block() directly.
        // For backwards compatibility, we register large objects here.
        let large = LargeAllocation {
            header,
            size,
            _layout: std::alloc::Layout::from_size_align(size, 8).unwrap(),
            drop_fn,
            trace_fn,
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
    pub fn allocate_in_block(
        &self,
        actual_size: usize,
        drop_fn: DropFn,
        trace_fn: Option<TraceFn>,
    ) -> *mut u8 {
        let sc_idx =
            size_class_index(actual_size).expect("allocate_in_block called for large object");
        let ptr = self.directories[sc_idx].allocate(actual_size, drop_fn, trace_fn);
        self.total_bytes.fetch_add(actual_size, Ordering::Relaxed);
        ptr
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
        block_count + large_count
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
        let start = Instant::now();

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

        // Phase 3: Sweep unmarked objects
        let reclaimed = self.sweep();

        // Measure pause time
        let elapsed = start.elapsed();
        let elapsed_nanos = elapsed.as_nanos() as u64;

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
        let start = Instant::now();

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

        // Phase 5: Sweep unmarked objects
        let reclaimed = self.sweep();

        let elapsed = start.elapsed();
        let elapsed_nanos = elapsed.as_nanos() as u64;

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
    fn build_trace_lookup(&self) -> FxHashMap<usize, Option<TraceFn>> {
        let mut map = FxHashMap::default();

        // Add all block-allocated objects
        for dir in &self.directories {
            dir.for_each_allocated(|header_ptr, trace_fn| {
                map.insert(header_ptr as usize, trace_fn);
            });
        }

        // Add large objects
        let large_objects = self.large_objects.borrow();
        for entry in large_objects.iter() {
            map.insert(entry.header as usize, entry.trace_fn);
        }

        map
    }

    /// Mark phase: trace from roots and mark all reachable objects
    fn mark(&self, roots: &[*const GcHeader]) {
        let mut worklist: VecDeque<*const GcHeader> = VecDeque::new();
        let mut visited: HashSet<usize> = HashSet::new();

        // Add all roots to the worklist (mark as gray)
        for &root in roots {
            if !root.is_null() {
                let addr = root as usize;
                if visited.insert(addr) {
                    unsafe {
                        (*root).set_mark(MarkColor::Gray);
                    }
                    worklist.push_back(root);
                }
            }
        }

        // Build trace function lookup table (O(n) build, O(1) per lookup)
        let trace_lookup = self.build_trace_lookup();

        // Process the worklist until empty
        while let Some(ptr) = worklist.pop_front() {
            unsafe {
                let header = &*ptr;

                // Skip if already black (fully processed)
                if header.mark() == MarkColor::Black {
                    continue;
                }

                // Look up trace function for this header (O(1))
                if let Some(Some(trace_fn)) = trace_lookup.get(&(ptr as usize)) {
                    // Trace the object's references
                    let data_ptr = (ptr as *const u8).add(std::mem::size_of::<GcHeader>());
                    trace_fn(data_ptr, &mut |child_header| {
                        if !child_header.is_null() {
                            let child_addr = child_header as usize;
                            if visited.insert(child_addr) {
                                (*child_header).set_mark(MarkColor::Gray);
                                worklist.push_back(child_header);
                            }
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

        // Sweep all block directories
        for dir in &self.directories {
            reclaimed += dir.sweep();
        }

        // Sweep large objects
        {
            let mut large_objects = self.large_objects.borrow_mut();
            let mut live = Vec::with_capacity(large_objects.len());
            let mut dead = Vec::new();

            for entry in large_objects.drain(..) {
                unsafe {
                    if (*entry.header).mark() == MarkColor::White {
                        reclaimed += entry.size;
                        dead.push(entry);
                    } else {
                        live.push(entry);
                    }
                }
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
        let mut visited = self.mark_visited.borrow_mut();
        visited.clear();

        for &root in roots {
            if !root.is_null() {
                let addr = root as usize;
                if visited.insert(addr) {
                    unsafe {
                        (*root).set_mark(MarkColor::Gray);
                    }
                    worklist.push_back(root);
                }
            }
        }

        // Build trace lookup (once per cycle)
        *self.trace_lookup.borrow_mut() = Some(self.build_trace_lookup());

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
        let barrier_entries = barrier_drain();
        if !barrier_entries.is_empty() {
            let mut worklist = self.mark_worklist.borrow_mut();
            let mut visited = self.mark_visited.borrow_mut();
            for ptr in barrier_entries {
                if !ptr.is_null() {
                    let addr = ptr as usize;
                    if visited.insert(addr) {
                        worklist.push_back(ptr);
                    }
                }
            }
        }

        let trace_lookup = self.trace_lookup.borrow();
        let lookup = match trace_lookup.as_ref() {
            Some(l) => l,
            None => return true, // No lookup means marking wasn't started properly
        };

        let mut worklist = self.mark_worklist.borrow_mut();
        let mut visited = self.mark_visited.borrow_mut();
        let mut processed = 0;

        while processed < budget {
            let ptr = match worklist.pop_front() {
                Some(p) => p,
                None => break, // Worklist empty
            };

            unsafe {
                let header = &*ptr;

                // Skip if already black (fully processed)
                if header.mark() == MarkColor::Black {
                    continue;
                }

                // Look up trace function for this header (O(1))
                if let Some(Some(trace_fn)) = lookup.get(&(ptr as usize)) {
                    let data_ptr = (ptr as *const u8).add(std::mem::size_of::<GcHeader>());
                    trace_fn(data_ptr, &mut |child_header| {
                        if !child_header.is_null() {
                            let child_addr = child_header as usize;
                            if visited.insert(child_addr) {
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
        let reclaimed = self.sweep();

        // Measure total time (from start_incremental_gc to finish_gc)
        if let Some(start) = self.mark_start.get() {
            let elapsed_nanos = start.elapsed().as_nanos() as u64;
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
        self.mark_visited.borrow_mut().clear();
        *self.trace_lookup.borrow_mut() = None;
        self.mark_start.set(None);
        self.gc_phase.set(GcPhase::Idle);

        reclaimed
    }

    /// Deallocate ALL tracked allocations without marking.
    ///
    /// Use this when tearing down an engine/isolate to reclaim all memory.
    pub fn dealloc_all(&self) -> usize {
        let total = self.total_bytes.load(Ordering::Relaxed);

        GC_DEALLOC_IN_PROGRESS.with(|f| f.set(true));

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

        GC_DEALLOC_IN_PROGRESS.with(|f| f.set(false));

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

// Thread-local allocation registry for the GC.
//
// Each thread gets its own registry so that GC collections in one thread
// (with that thread's roots) don't sweep objects belonging to another thread.
// This prevents use-after-free when multiple VmContexts run in parallel
// (e.g. in test suites).
//
// The registry is leaked (Box::leak) to produce a `&'static` reference that
// matches the existing API. Each thread leaks exactly one AllocationRegistry
// for the lifetime of the process — a bounded, negligible leak.
thread_local! {
    static THREAD_REGISTRY: &'static AllocationRegistry = Box::leak(Box::new(AllocationRegistry::new()));
}

/// Get the thread-local allocation registry
pub fn global_registry() -> &'static AllocationRegistry {
    THREAD_REGISTRY.with(|r| *r)
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
    let layout = std::alloc::Layout::new::<(GcHeader, T)>();
    let alloc_size = layout.size();

    let trace_fn: Option<TraceFn> = if T::NEEDS_TRACE {
        Some(trace_gc_box::<T>)
    } else {
        None
    };

    let ptr: *mut (GcHeader, T);

    if alloc_size <= LARGE_OBJECT_THRESHOLD {
        // Small object: allocate from block directory.
        // Use drop_gc_box_in_block which only drops in-place (no dealloc —
        // the block owns the memory).
        let drop_fn: DropFn = drop_gc_box_in_block::<T>;
        let cell_ptr = registry.allocate_in_block(alloc_size, drop_fn, trace_fn);
        ptr = cell_ptr as *mut (GcHeader, T);
    } else {
        // Large object: allocate individually via global allocator.
        // Use drop_gc_box which also calls dealloc.
        let drop_fn: DropFn = drop_gc_box::<T>;
        // SAFETY: Layout is valid and non-zero sized
        let raw = unsafe { std::alloc::alloc(layout) as *mut (GcHeader, T) };
        if raw.is_null() {
            std::alloc::handle_alloc_error(layout);
        }
        ptr = raw;

        // Register with the large object tracker
        let header_ptr = ptr as *mut GcHeader;
        unsafe {
            registry.register(header_ptr, alloc_size, drop_fn, trace_fn);
        }
    }

    // Initialize header and value
    // SAFETY: ptr is non-null and properly aligned (from block or alloc)
    unsafe {
        std::ptr::write(&mut (*ptr).0, GcHeader::new(0));
        std::ptr::write(&mut (*ptr).1, value);

        // Black allocation: objects allocated during marking are pre-marked Black.
        // This prevents newly allocated objects from being swept in the ongoing cycle.
        if registry.is_marking() {
            (*ptr).0.set_mark(MarkColor::Black);
        }
    }

    // Return pointer to the value (after the header)
    unsafe { &mut (*ptr).1 as *mut T }
}

/// Drop function for block-allocated GC cells.
///
/// Only drops the value in-place (no dealloc — the block owns the memory).
unsafe fn drop_gc_box_in_block<T>(ptr: *mut u8) {
    let box_ptr = ptr as *mut (GcHeader, T);
    // SAFETY: ptr is valid and points to an initialized (GcHeader, T)
    unsafe {
        std::ptr::drop_in_place(&mut (*box_ptr).1);
        // Do NOT call dealloc — the block owns this memory
    }
}

/// Drop function for large GC boxes (individually allocated).
unsafe fn drop_gc_box<T>(ptr: *mut u8) {
    let layout = std::alloc::Layout::new::<(GcHeader, T)>();
    let box_ptr = ptr as *mut (GcHeader, T);
    // SAFETY: ptr is valid and points to an initialized (GcHeader, T)
    unsafe {
        std::ptr::drop_in_place(&mut (*box_ptr).1);
        std::alloc::dealloc(ptr, layout);
    }
}

/// Trace function for GC boxes
unsafe fn trace_gc_box<T: GcTraceable>(ptr: *const u8, tracer: &mut dyn FnMut(*const GcHeader)) {
    let value_ptr = ptr as *const T;
    // SAFETY: ptr is valid and points to an initialized T
    unsafe {
        (*value_ptr).trace(tracer);
    }
}

/// Trait for types that can be traced by the GC
pub trait GcTraceable {
    /// Whether this type contains GC references that need tracing
    const NEEDS_TRACE: bool;

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
    fn trace(&self, _tracer: &mut dyn FnMut(*const GcHeader)) {}
}

impl GcTraceable for bool {
    const NEEDS_TRACE: bool = false;
    fn trace(&self, _tracer: &mut dyn FnMut(*const GcHeader)) {}
}

impl GcTraceable for i32 {
    const NEEDS_TRACE: bool = false;
    fn trace(&self, _tracer: &mut dyn FnMut(*const GcHeader)) {}
}

impl GcTraceable for i64 {
    const NEEDS_TRACE: bool = false;
    fn trace(&self, _tracer: &mut dyn FnMut(*const GcHeader)) {}
}

impl GcTraceable for f64 {
    const NEEDS_TRACE: bool = false;
    fn trace(&self, _tracer: &mut dyn FnMut(*const GcHeader)) {}
}

impl GcTraceable for String {
    const NEEDS_TRACE: bool = false;
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
        let header_ptr =
            unsafe { (ptr as *const u8).sub(std::mem::size_of::<GcHeader>()) as *const GcHeader };

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
                let _ = gc_alloc_in(&registry, i as i32);
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
        let node2_header =
            unsafe { (node2 as *const u8).sub(std::mem::size_of::<GcHeader>()) as *const GcHeader };

        let node1 = unsafe {
            gc_alloc_in(
                &registry,
                Node {
                    value: 1,
                    next_header: Some(node2_header),
                },
            )
        };
        let node1_header =
            unsafe { (node1 as *const u8).sub(std::mem::size_of::<GcHeader>()) as *const GcHeader };

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
        let node1_header =
            unsafe { (node1 as *const u8).sub(std::mem::size_of::<GcHeader>()) as *const GcHeader };

        let node2 = unsafe {
            gc_alloc_in(
                &registry,
                Node {
                    value: 2,
                    next_header: Some(node1_header),
                },
            )
        };
        let node2_header =
            unsafe { (node2 as *const u8).sub(std::mem::size_of::<GcHeader>()) as *const GcHeader };

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
        let header_ptr =
            unsafe { (ptr as *const u8).sub(std::mem::size_of::<GcHeader>()) as *const GcHeader };

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
        let header_ptr =
            unsafe { (ptr as *const u8).sub(std::mem::size_of::<GcHeader>()) as *const GcHeader };

        // Allocate a second object (not in initial roots)
        let ptr2 = unsafe { gc_alloc_in(&registry, 99i32) };
        let header_ptr2 =
            unsafe { (ptr2 as *const u8).sub(std::mem::size_of::<GcHeader>()) as *const GcHeader };

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
}
