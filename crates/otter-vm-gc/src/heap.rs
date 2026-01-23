//! GC Heap management

use parking_lot::RwLock;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

/// GC configuration
#[derive(Debug, Clone)]
pub struct GcConfig {
    /// Young generation size per thread (default: 1MB)
    pub young_size: usize,
    /// Old generation initial size (default: 16MB)
    pub old_size: usize,
    /// Large object threshold (default: 8KB)
    pub large_threshold: usize,
    /// GC trigger ratio (default: 0.75)
    pub gc_trigger_ratio: f64,
}

impl Default for GcConfig {
    fn default() -> Self {
        Self {
            young_size: 1024 * 1024,    // 1MB
            old_size: 16 * 1024 * 1024, // 16MB
            large_threshold: 8 * 1024,  // 8KB
            gc_trigger_ratio: 0.75,
        }
    }
}

/// Main GC heap - shared between threads
pub struct GcHeap {
    config: GcConfig,
    /// Total bytes allocated
    allocated: AtomicUsize,
    /// Old generation
    old_gen: RwLock<OldGeneration>,
    /// Large object space
    large_objects: RwLock<LargeObjectSpace>,
}

impl GcHeap {
    /// Create new heap with default config
    pub fn new() -> Arc<Self> {
        Self::with_config(GcConfig::default())
    }

    /// Create new heap with custom config
    pub fn with_config(config: GcConfig) -> Arc<Self> {
        Arc::new(Self {
            old_gen: RwLock::new(OldGeneration::new(config.old_size)),
            large_objects: RwLock::new(LargeObjectSpace::new()),
            allocated: AtomicUsize::new(0),
            config,
        })
    }

    /// Get current allocated bytes
    pub fn allocated(&self) -> usize {
        self.allocated.load(Ordering::Relaxed)
    }

    /// Check if GC should be triggered
    pub fn should_gc(&self) -> bool {
        let allocated = self.allocated() as f64;
        let threshold = self.config.old_size as f64 * self.config.gc_trigger_ratio;
        allocated > threshold
    }

    /// Allocate in old generation
    pub fn allocate_old(&self, size: usize) -> Option<*mut u8> {
        // Align to 8 bytes for tracking
        let aligned_size = (size + 7) & !7;
        let mut old = self.old_gen.write();
        let ptr = old.allocate(size)?;
        self.allocated.fetch_add(aligned_size, Ordering::Relaxed);
        Some(ptr)
    }

    /// Allocate large object
    pub fn allocate_large(&self, size: usize) -> Option<*mut u8> {
        let mut large = self.large_objects.write();
        let ptr = large.allocate(size)?;
        self.allocated.fetch_add(size, Ordering::Relaxed);
        Some(ptr)
    }

    /// Get config
    pub fn config(&self) -> &GcConfig {
        &self.config
    }
}

// Safety: GcHeap is designed to be shared between threads
unsafe impl Send for GcHeap {}
unsafe impl Sync for GcHeap {}

/// Old generation heap region
struct OldGeneration {
    /// Memory region
    memory: Vec<u8>,
    /// Free pointer
    free: usize,
}

impl OldGeneration {
    fn new(size: usize) -> Self {
        Self {
            memory: vec![0u8; size],
            free: 0,
        }
    }

    fn allocate(&mut self, size: usize) -> Option<*mut u8> {
        // Align to 8 bytes
        let aligned_size = (size + 7) & !7;

        if self.free + aligned_size > self.memory.len() {
            return None;
        }

        let ptr = self.memory.as_mut_ptr().wrapping_add(self.free);
        self.free += aligned_size;
        Some(ptr)
    }
}

/// Large object space
struct LargeObjectSpace {
    /// Large allocations
    allocations: Vec<Box<[u8]>>,
}

impl LargeObjectSpace {
    fn new() -> Self {
        Self {
            allocations: Vec::new(),
        }
    }

    fn allocate(&mut self, size: usize) -> Option<*mut u8> {
        let mut allocation = vec![0u8; size].into_boxed_slice();
        let ptr = allocation.as_mut_ptr();
        self.allocations.push(allocation);
        Some(ptr)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_heap_creation() {
        let heap = GcHeap::new();
        assert_eq!(heap.allocated(), 0);
    }

    #[test]
    fn test_allocate_old() {
        let heap = GcHeap::new();
        let ptr = heap.allocate_old(100);
        assert!(ptr.is_some());
        assert_eq!(heap.allocated(), 104); // Aligned to 8
    }

    #[test]
    fn test_allocate_large() {
        let heap = GcHeap::new();
        let ptr = heap.allocate_large(10000);
        assert!(ptr.is_some());
        assert_eq!(heap.allocated(), 10000);
    }
}
