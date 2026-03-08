//! Memory management and accounting for Otter VM
//!
//! This module provides tools to track and limit heap allocations
//! for JavaScript objects, strings, and other VM structures.
//!
//! # Separation of Concerns
//!
//! The MemoryManager is NOT a garbage collector - it's a lightweight
//! accounting layer that decides *when* to trigger GC. The actual
//! mark-sweep collection happens in the `otter-vm-gc` crate.
//!
//! | Component | Responsibility |
//! |-----------|---------------|
//! | MemoryManager | Track bytes, decide when to GC |
//! | otter-vm-gc | Perform mark-sweep collection |
//! | HandleScope | Provide safe GC boundaries |
//!
//! ## GC Decision Criteria
//!
//! GC is triggered when ANY of these conditions are met:
//! 1. Explicit GC request (e.g., from Test262 harness via `gc_requested` flag)
//! 2. Allocation count exceeds threshold (default 10,000 allocations)
//! 3. Heap size exceeds adaptive threshold (2x live set size, minimum 1MB)
//!
//! ## Why MemoryManager is Separate from otter-vm-gc
//!
//! **Performance:** The interpreter checks `should_collect_garbage()` every
//! ~10,000 instructions. This check must be extremely fast:
//! - MemoryManager uses atomic operations (no locks)
//! - Simple integer comparisons
//! - No expensive registry lookups
//!
//! **otter-vm-gc** uses:
//! - RwLock for AllocationRegistry
//! - Per-object metadata (GcHeader)
//! - Type-erased drop/trace function pointers
//! - Tri-color marking algorithm
//!
//! Mixing these concerns would make the fast path (GC decision) slow.
//!
//! ## Adaptive Threshold
//!
//! The GC threshold adapts based on the live set size:
//! - After GC completes, threshold = max(2 × live_bytes, MIN_GC_THRESHOLD)
//! - Prevents thrashing when heap is small
//! - Reduces GC frequency when heap is large and stable
//!
//! ## Usage Pattern
//!
//! ```ignore
//! let mm = MemoryManager::new(10_000_000); // 10MB limit
//!
//! // Fast path: check if GC needed (called frequently)
//! if mm.should_collect_garbage() {
//!     // Slow path: run actual collection (otter-vm-gc)
//!     let reclaimed = collect_garbage();
//!     mm.on_gc_complete(live_bytes);
//! }
//!
//! // Book memory for allocation
//! mm.alloc(size)?;
//! ```
//!
//! ## Architecture
//!
//! ```text
//! VmContext (interpreter loop)
//!     ↓ every ~10k instructions
//! MemoryManager::should_collect_garbage() (fast atomic checks)
//!     ↓ if true
//! otter-vm-gc::collect() (expensive mark-sweep)
//!     ↓ returns live_bytes
//! MemoryManager::on_gc_complete() (update stats, reset threshold)
//! ```

use crate::error::{VmError, VmResult};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

thread_local! {
    static THREAD_MEMORY_MANAGER: std::cell::RefCell<Option<Arc<MemoryManager>>> = const { std::cell::RefCell::new(None) };
}

/// Minimum GC threshold (1MB)
const MIN_GC_THRESHOLD: usize = 1024 * 1024;

/// Default allocation count threshold for triggering GC
/// Increased from 1,000 to reduce GC frequency (GC was taking 43% CPU)
const DEFAULT_ALLOCATION_COUNT_THRESHOLD: usize = 10_000;

/// Allocation category counters for profiling.
///
/// Only active when the `profiling` feature is enabled; otherwise a zero-size struct.
#[cfg(feature = "profiling")]
#[derive(Debug, Default)]
pub struct AllocationCategoryStats {
    /// Object allocations
    pub object_count: AtomicUsize,
    /// Object bytes allocated
    pub object_bytes: AtomicUsize,
    /// Array allocations
    pub array_count: AtomicUsize,
    /// String allocations
    pub string_count: AtomicUsize,
    /// Closure/function allocations
    pub closure_count: AtomicUsize,
    /// Other allocations (BigInt, Symbol, etc.)
    pub other_count: AtomicUsize,
}

#[cfg(feature = "profiling")]
impl AllocationCategoryStats {
    fn new() -> Self {
        Self::default()
    }

    /// Record an allocation by category tag.
    /// Tags: OBJECT=3, STRING=1, ARRAY=2, CLOSURE=5, FUNCTION=4
    #[inline]
    pub fn record(&self, tag: u8, bytes: usize) {
        match tag {
            3 => {
                self.object_count.fetch_add(1, Ordering::Relaxed);
                self.object_bytes.fetch_add(bytes, Ordering::Relaxed);
            }
            1 => { self.string_count.fetch_add(1, Ordering::Relaxed); }
            2 => { self.array_count.fetch_add(1, Ordering::Relaxed); }
            4 | 5 => { self.closure_count.fetch_add(1, Ordering::Relaxed); }
            _ => { self.other_count.fetch_add(1, Ordering::Relaxed); }
        }
    }

    /// Record an array allocation specifically.
    #[inline]
    pub fn record_array(&self) {
        self.array_count.fetch_add(1, Ordering::Relaxed);
    }

    /// Snapshot the current values.
    pub fn snapshot(&self) -> AllocationCategorySnapshot {
        AllocationCategorySnapshot {
            object_count: self.object_count.load(Ordering::Relaxed) as u64,
            object_bytes: self.object_bytes.load(Ordering::Relaxed) as u64,
            array_count: self.array_count.load(Ordering::Relaxed) as u64,
            string_count: self.string_count.load(Ordering::Relaxed) as u64,
            closure_count: self.closure_count.load(Ordering::Relaxed) as u64,
            other_count: self.other_count.load(Ordering::Relaxed) as u64,
        }
    }
}

/// Snapshot of allocation category stats (for reporting).
#[cfg(feature = "profiling")]
#[derive(Debug, Clone)]
pub struct AllocationCategorySnapshot {
    /// Object allocations
    pub object_count: u64,
    /// Object bytes allocated
    pub object_bytes: u64,
    /// Array allocations
    pub array_count: u64,
    /// String allocations
    pub string_count: u64,
    /// Closure/function allocations
    pub closure_count: u64,
    /// Other allocations
    pub other_count: u64,
}

/// Manages memory limits and accounting for a VM instance
pub struct MemoryManager {
    /// Total bytes currently allocated
    allocated: AtomicUsize,
    /// Maximum bytes allowed for this VM
    limit: usize,
    /// Number of allocations since last GC
    allocation_count: AtomicUsize,
    /// Live set size after last GC (bytes)
    last_live_size: AtomicUsize,
    /// Explicit GC request flag
    gc_requested: AtomicBool,
    /// Allocation count threshold for triggering GC
    allocation_count_threshold: AtomicUsize,
    /// Cached GC threshold (updated after each GC)
    cached_gc_threshold: AtomicUsize,
    /// GC stress mode: collect on every `should_collect_garbage()` check.
    /// Enable with `GC_STRESS=1` environment variable.
    gc_stress: bool,
    /// Per-category allocation counters (profiling only)
    #[cfg(feature = "profiling")]
    pub category_stats: AllocationCategoryStats,
}

impl MemoryManager {
    /// Create a new memory manager with the specified limit
    pub fn new(limit: usize) -> Self {
        let gc_stress = std::env::var("GC_STRESS")
            .map(|v| v == "1")
            .unwrap_or(false);
        Self {
            allocated: AtomicUsize::new(0),
            limit,
            allocation_count: AtomicUsize::new(0),
            last_live_size: AtomicUsize::new(0),
            gc_requested: AtomicBool::new(false),
            allocation_count_threshold: AtomicUsize::new(DEFAULT_ALLOCATION_COUNT_THRESHOLD),
            cached_gc_threshold: AtomicUsize::new(MIN_GC_THRESHOLD),
            gc_stress,
            #[cfg(feature = "profiling")]
            category_stats: AllocationCategoryStats::new(),
        }
    }

    /// Create a memory manager with a very large limit (for tests)
    pub fn test() -> Self {
        Self::new(usize::MAX / 2)
    }

    /// Try to book 'size' bytes. Returns Err(VmError::OutOfMemory) if limit exceeded.
    #[inline]
    pub fn alloc(&self, size: usize) -> VmResult<()> {
        // Fast path: check limit (common case: allocations succeed)
        let current = self.allocated.load(Ordering::Relaxed);
        if current + size > self.limit {
            return Err(VmError::OutOfMemory);
        }

        // Update counters with relaxed ordering for performance
        self.allocated.fetch_add(size, Ordering::Relaxed);
        self.allocation_count.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    /// Record deallocation of 'size' bytes
    #[inline]
    pub fn free(&self, size: usize) {
        self.allocated.fetch_sub(size, Ordering::Relaxed);
    }

    /// Get current allocated bytes
    #[inline]
    pub fn allocated(&self) -> usize {
        self.allocated.load(Ordering::Relaxed)
    }

    /// Get memory limit
    pub fn limit(&self) -> usize {
        self.limit
    }

    /// Get the number of allocations since last GC
    pub fn allocation_count(&self) -> usize {
        self.allocation_count.load(Ordering::Relaxed)
    }

    /// Reset allocation count (called after GC)
    pub fn reset_allocation_count(&self) {
        self.allocation_count.store(0, Ordering::Relaxed);
    }

    /// Get the live set size after last GC
    pub fn last_live_size(&self) -> usize {
        self.last_live_size.load(Ordering::Relaxed)
    }

    /// Update live set size (called after GC)
    pub fn set_last_live_size(&self, size: usize) {
        self.last_live_size.store(size, Ordering::Relaxed);
    }

    /// Compute adaptive GC threshold based on live set size
    ///
    /// Returns 2x the live set size, with a minimum of MIN_GC_THRESHOLD (1MB)
    pub fn gc_threshold(&self) -> usize {
        let live_size = self.last_live_size.load(Ordering::Relaxed);
        usize::max(MIN_GC_THRESHOLD, live_size.saturating_mul(2))
    }

    /// Check if GC should be triggered based on memory pressure
    ///
    /// Returns true if any of these conditions are met:
    /// 1. Explicit GC was requested
    /// 2. Allocation count exceeds threshold
    /// 3. Heap size exceeds adaptive threshold (2x live set)
    ///
    /// Optimized with early exits and cached threshold for performance.
    #[inline]
    pub fn should_collect_garbage(&self) -> bool {
        // GC stress mode: always collect
        if self.gc_stress {
            return true;
        }

        // Fast path: check explicit request first (single atomic load, most urgent)
        if self.gc_requested.load(Ordering::Relaxed) {
            return true;
        }

        // Fast path: check allocation count (single load, cheap comparison)
        let alloc_count = self.allocation_count.load(Ordering::Relaxed);
        if alloc_count >= self.allocation_count_threshold.load(Ordering::Relaxed) {
            return true;
        }

        // Slower path: check heap size against cached threshold
        let allocated = self.allocated.load(Ordering::Relaxed);
        let threshold = self.cached_gc_threshold.load(Ordering::Relaxed);
        allocated >= threshold
    }

    /// Request an explicit GC cycle
    pub fn request_gc(&self) {
        self.gc_requested.store(true, Ordering::Relaxed);
    }

    /// Clear GC request flag (called after GC completes)
    pub fn clear_gc_request(&self) {
        self.gc_requested.store(false, Ordering::Relaxed);
    }

    /// Called after a GC cycle completes to update state.
    /// Reconciles allocated bytes with actual GC live set size.
    pub fn on_gc_complete(&self, live_bytes: usize) {
        self.reset_allocation_count();
        self.set_last_live_size(live_bytes);
        self.clear_gc_request();
        // Reconcile: sync allocated with actual live bytes from GC
        self.allocated.store(live_bytes, Ordering::Relaxed);
        // Update cached threshold
        let new_threshold = usize::max(MIN_GC_THRESHOLD, live_bytes.saturating_mul(2));
        self.cached_gc_threshold
            .store(new_threshold, Ordering::Relaxed);
    }

    /// Set the allocation count threshold
    pub fn set_allocation_count_threshold(&self, threshold: usize) {
        self.allocation_count_threshold
            .store(threshold, Ordering::Relaxed);
    }

    /// Set the thread-local default MemoryManager.
    /// Called by VmContext::with_config() when a VM is created.
    pub fn set_thread_default(mm: Arc<MemoryManager>) {
        THREAD_MEMORY_MANAGER.with(|cell| {
            *cell.borrow_mut() = Some(mm);
        });
    }

    /// Get the thread-local MemoryManager, if one is set.
    pub fn current() -> Option<Arc<MemoryManager>> {
        THREAD_MEMORY_MANAGER.with(|cell| cell.borrow().clone())
    }

    /// Clear the thread-local MemoryManager.
    /// Called by VmContext::drop().
    pub fn clear_thread_default() {
        THREAD_MEMORY_MANAGER.with(|cell| {
            *cell.borrow_mut() = None;
        });
    }

    /// Clear the thread-local MemoryManager only if it matches the given one.
    /// Used by VmRuntime::drop() to avoid clearing another runtime's MM.
    pub fn clear_thread_default_if(mm: &Arc<MemoryManager>) {
        THREAD_MEMORY_MANAGER.with(|cell| {
            let mut guard = cell.borrow_mut();
            if let Some(current) = guard.as_ref() {
                if Arc::ptr_eq(current, mm) {
                    *guard = None;
                }
            }
        });
    }
}

/// A wrapper around a type that records its size in a MemoryManager on drop
pub struct Tracked<T> {
    inner: T,
    size: usize,
    manager: Arc<MemoryManager>,
}

impl<T> Tracked<T> {
    pub fn new(inner: T, size: usize, manager: Arc<MemoryManager>) -> VmResult<Self> {
        manager.alloc(size)?;
        Ok(Self {
            inner,
            size,
            manager,
        })
    }

    pub fn inner(&self) -> &T {
        &self.inner
    }

    pub fn inner_mut(&mut self) -> &mut T {
        &mut self.inner
    }
}

impl<T> std::ops::Deref for Tracked<T> {
    type Target = T;
    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

impl<T> Drop for Tracked<T> {
    fn drop(&mut self) {
        self.manager.free(self.size);
    }
}
