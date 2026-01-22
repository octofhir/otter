//! Thread-local allocator for young generation

use crate::heap::GcHeap;
use std::sync::Arc;

/// Thread-local bump allocator for young generation
pub struct Allocator {
    /// Shared heap reference
    heap: Arc<GcHeap>,
    /// Young generation buffer
    young: Vec<u8>,
    /// Allocation pointer
    ptr: usize,
}

impl Allocator {
    /// Create new allocator for a heap
    pub fn new(heap: Arc<GcHeap>) -> Self {
        let young_size = heap.config().young_size;
        Self {
            heap,
            young: vec![0u8; young_size],
            ptr: 0,
        }
    }

    /// Allocate memory (returns None if GC needed)
    pub fn allocate(&mut self, size: usize) -> Option<*mut u8> {
        // Align to 8 bytes
        let aligned_size = (size + 7) & !7;

        // Try young generation first
        if self.ptr + aligned_size <= self.young.len() {
            let ptr = self.young.as_mut_ptr().wrapping_add(self.ptr);
            self.ptr += aligned_size;
            return Some(ptr);
        }

        // Young gen full - check if large object
        if aligned_size > self.heap.config().large_threshold {
            return self.heap.allocate_large(aligned_size);
        }

        // Need minor GC or promotion to old gen
        // For now, just allocate in old gen
        self.heap.allocate_old(aligned_size)
    }

    /// Reset young generation (after minor GC)
    pub fn reset_young(&mut self) {
        self.ptr = 0;
    }

    /// Get heap reference
    pub fn heap(&self) -> &Arc<GcHeap> {
        &self.heap
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_allocator_young() {
        let heap = GcHeap::new();
        let mut alloc = Allocator::new(heap);

        let ptr1 = alloc.allocate(100);
        let ptr2 = alloc.allocate(200);

        assert!(ptr1.is_some());
        assert!(ptr2.is_some());
        assert_ne!(ptr1, ptr2);
    }

    #[test]
    fn test_allocator_reset() {
        let heap = GcHeap::new();
        let mut alloc = Allocator::new(heap);

        let ptr1 = alloc.allocate(100);
        alloc.reset_young();
        let ptr2 = alloc.allocate(100);

        // After reset, should allocate from same position
        assert_eq!(ptr1, ptr2);
    }
}
