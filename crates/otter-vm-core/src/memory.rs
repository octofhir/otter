//! Memory management and accounting for Otter VM
//!
//! This module provides tools to track and limit heap allocations
//! for JavaScript objects, strings, and other VM structures.

use crate::error::{VmError, VmResult};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

/// Manages memory limits and accounting for a VM instance
pub struct MemoryManager {
    /// Total bytes currently allocated
    allocated: AtomicUsize,
    /// Maximum bytes allowed for this VM
    limit: usize,
}

impl MemoryManager {
    /// Create a new memory manager with the specified limit
    pub fn new(limit: usize) -> Self {
        Self {
            allocated: AtomicUsize::new(0),
            limit,
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
