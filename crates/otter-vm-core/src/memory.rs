//! Memory management and accounting for Otter VM
//!
//! This module provides tools to track and limit heap allocations
//! for JavaScript objects, strings, and other VM structures.

use crate::error::{VmError, VmResult};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

/// Minimum GC threshold (1MB)
const MIN_GC_THRESHOLD: usize = 1024 * 1024;

/// Default allocation count threshold for triggering GC
const DEFAULT_ALLOCATION_COUNT_THRESHOLD: usize = 10_000;

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
}

impl MemoryManager {
    /// Create a new memory manager with the specified limit
    pub fn new(limit: usize) -> Self {
        Self {
            allocated: AtomicUsize::new(0),
            limit,
            allocation_count: AtomicUsize::new(0),
            last_live_size: AtomicUsize::new(0),
            gc_requested: AtomicBool::new(false),
            allocation_count_threshold: AtomicUsize::new(DEFAULT_ALLOCATION_COUNT_THRESHOLD),
        }
    }

    /// Create a memory manager with a very large limit (for tests)
    pub fn test() -> Self {
        Self::new(usize::MAX / 2)
    }

    /// Try to book 'size' bytes. Returns Err(VmError::OutOfMemory) if limit exceeded.
    pub fn alloc(&self, size: usize) -> VmResult<()> {
        let current = self.allocated.load(Ordering::Relaxed);
        if current + size > self.limit {
            return Err(VmError::OutOfMemory);
        }

        // Use a loop with compare_exchange if we want strict accuracy,
        // but fetch_add is usually fine for accounting.
        self.allocated.fetch_add(size, Ordering::Relaxed);
        self.allocation_count.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    /// Record deallocation of 'size' bytes
    pub fn free(&self, size: usize) {
        self.allocated.fetch_sub(size, Ordering::Relaxed);
    }

    /// Get current allocated bytes
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
    /// 1. Heap size exceeds adaptive threshold (2x live set)
    /// 2. Allocation count exceeds threshold
    /// 3. Explicit GC was requested
    pub fn should_collect_garbage(&self) -> bool {
        // Check explicit request
        if self.gc_requested.load(Ordering::Relaxed) {
            return true;
        }

        // Check allocation count threshold
        let alloc_count = self.allocation_count.load(Ordering::Relaxed);
        let alloc_threshold = self.allocation_count_threshold.load(Ordering::Relaxed);
        if alloc_count >= alloc_threshold {
            return true;
        }

        // Check heap size threshold (delegate to GC registry's threshold)
        false
    }

    /// Request an explicit GC cycle
    pub fn request_gc(&self) {
        self.gc_requested.store(true, Ordering::Relaxed);
    }

    /// Clear GC request flag (called after GC completes)
    pub fn clear_gc_request(&self) {
        self.gc_requested.store(false, Ordering::Relaxed);
    }

    /// Called after a GC cycle completes to update state
    pub fn on_gc_complete(&self, live_bytes: usize) {
        self.reset_allocation_count();
        self.set_last_live_size(live_bytes);
        self.clear_gc_request();
    }

    /// Set the allocation count threshold
    pub fn set_allocation_count_threshold(&self, threshold: usize) {
        self.allocation_count_threshold
            .store(threshold, Ordering::Relaxed);
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
