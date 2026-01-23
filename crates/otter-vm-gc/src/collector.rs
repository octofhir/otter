//! Mark-sweep garbage collector

use crate::barrier::{RememberedSet, WriteBarrierBuffer};
use crate::heap::GcHeap;
use crate::object::{GcHeader, MarkColor};
use std::collections::VecDeque;
use std::sync::Arc;

/// Garbage collector
pub struct Collector {
    heap: Arc<GcHeap>,
    /// Gray worklist for incremental marking
    worklist: VecDeque<*const GcHeader>,
    /// Statistics
    stats: GcStats,
    /// Write barrier buffer for concurrent mutation
    barrier_buffer: Arc<WriteBarrierBuffer>,
    /// Remembered set for generational collection
    remembered_set: Arc<RememberedSet>,
}

/// GC statistics
#[derive(Debug, Default, Clone)]
pub struct GcStats {
    /// Number of collections
    pub collections: u64,
    /// Total time spent in GC (nanoseconds)
    pub total_time_ns: u64,
    /// Bytes reclaimed in last collection
    pub last_reclaimed: usize,
    /// Objects marked in last collection
    pub last_marked: usize,
}

impl Collector {
    /// Create new collector
    pub fn new(heap: Arc<GcHeap>) -> Self {
        Self {
            heap,
            worklist: VecDeque::new(),
            stats: GcStats::default(),
            barrier_buffer: Arc::new(WriteBarrierBuffer::new()),
            remembered_set: Arc::new(RememberedSet::new()),
        }
    }

    /// Create collector with custom barrier buffer and remembered set
    pub fn with_barriers(
        heap: Arc<GcHeap>,
        barrier_buffer: Arc<WriteBarrierBuffer>,
        remembered_set: Arc<RememberedSet>,
    ) -> Self {
        Self {
            heap,
            worklist: VecDeque::new(),
            stats: GcStats::default(),
            barrier_buffer,
            remembered_set,
        }
    }

    /// Get the write barrier buffer for mutation tracking
    pub fn barrier_buffer(&self) -> &Arc<WriteBarrierBuffer> {
        &self.barrier_buffer
    }

    /// Get the remembered set for generational collection
    pub fn remembered_set(&self) -> &Arc<RememberedSet> {
        &self.remembered_set
    }

    /// Run a full GC cycle
    pub fn collect(&mut self, roots: &[*const GcHeader]) {
        let start = std::time::Instant::now();

        // Phase 1: Mark (includes barrier buffer and remembered set)
        self.mark(roots);

        // Phase 2: Sweep
        let reclaimed = self.sweep();

        // Update stats
        self.stats.collections += 1;
        self.stats.total_time_ns += start.elapsed().as_nanos() as u64;
        self.stats.last_reclaimed = reclaimed;
    }

    /// Run a young generation collection (minor GC)
    ///
    /// Only collects young generation, using remembered set as additional roots
    pub fn collect_young(&mut self, roots: &[*const GcHeader]) {
        let start = std::time::Instant::now();

        // Combine roots with remembered set entries
        let mut all_roots: Vec<*const GcHeader> = roots.to_vec();
        all_roots.extend(self.remembered_set.roots());

        // Mark from combined roots
        self.mark(&all_roots);

        // Sweep young generation only
        let reclaimed = self.sweep_young();

        // Update stats
        self.stats.collections += 1;
        self.stats.total_time_ns += start.elapsed().as_nanos() as u64;
        self.stats.last_reclaimed = reclaimed;
    }

    /// Mark phase - trace from roots
    fn mark(&mut self, roots: &[*const GcHeader]) {
        self.stats.last_marked = 0;

        // Add roots to worklist
        for &root in roots {
            if !root.is_null() {
                unsafe {
                    let header = &*root;
                    if header.mark() == MarkColor::White {
                        header.set_mark(MarkColor::Gray);
                        self.worklist.push_back(root);
                    }
                }
            }
        }

        // Also process write barrier buffer entries
        let barrier_entries = self.barrier_buffer.drain();
        for entry in barrier_entries {
            if !entry.is_null() {
                unsafe {
                    let header = &*entry;
                    // Barrier entries are already gray, add to worklist
                    if header.mark() == MarkColor::Gray {
                        self.worklist.push_back(entry);
                    }
                }
            }
        }

        // Process worklist
        while let Some(obj_ptr) = self.worklist.pop_front() {
            unsafe {
                let header = &*obj_ptr;

                // Trace object's references
                // This requires knowing the object type and layout
                self.trace_object(obj_ptr);

                // Mark as black (fully scanned)
                header.set_mark(MarkColor::Black);
                self.stats.last_marked += 1;
            }
        }
    }

    /// Trace an object's references
    unsafe fn trace_object(&mut self, _obj_ptr: *const GcHeader) {
        // This will be implemented based on object type
        // For now, it's a no-op placeholder

        // Example implementation:
        // let obj = &*obj_ptr;
        // match obj.tag() {
        //     tags::OBJECT => self.trace_js_object(obj_ptr),
        //     tags::ARRAY => self.trace_array(obj_ptr),
        //     tags::FUNCTION => self.trace_function(obj_ptr),
        //     _ => {}
        // }
    }

    /// Mark a reference (called during tracing)
    ///
    /// # Safety
    /// The pointer must be valid and point to a live GcHeader.
    pub unsafe fn mark_reference(&mut self, ptr: *const GcHeader) {
        if ptr.is_null() {
            return;
        }

        // SAFETY: Caller guarantees ptr is valid
        let header = unsafe { &*ptr };
        if header.mark() == MarkColor::White {
            header.set_mark(MarkColor::Gray);
            self.worklist.push_back(ptr);
        }
    }

    /// Sweep phase - reclaim white objects
    fn sweep(&mut self) -> usize {
        // For now, return 0 as we don't have proper object tracking yet
        // Full implementation will iterate over all allocated objects
        // and free those still marked white
        0
    }

    /// Sweep young generation only
    fn sweep_young(&mut self) -> usize {
        // For now, return 0 as we don't have proper young generation tracking
        // Full implementation will iterate over young generation objects
        // and promote survivors to old generation
        0
    }

    /// Process write barrier buffer
    ///
    /// Call this during incremental marking to handle mutations
    pub fn process_barrier_buffer(&mut self) {
        let entries = self.barrier_buffer.drain();
        for entry in entries {
            if !entry.is_null() {
                unsafe {
                    let header = &*entry;
                    // Re-scan objects that were modified
                    if header.mark() == MarkColor::Gray {
                        self.worklist.push_back(entry);
                    }
                }
            }
        }
    }

    /// Clear the remembered set after full GC
    pub fn clear_remembered_set(&self) {
        self.remembered_set.clear();
    }

    /// Get statistics
    pub fn stats(&self) -> &GcStats {
        &self.stats
    }

    /// Get heap reference
    pub fn heap(&self) -> &Arc<GcHeap> {
        &self.heap
    }

    /// Reset all marks to white (prepare for next GC)
    #[allow(dead_code)]
    fn reset_marks(&mut self) {
        // Will iterate over all objects and reset marks
    }
}

/// Write barrier for concurrent/incremental GC
///
/// # Safety
/// Both pointers must be valid and point to live GcHeaders.
pub unsafe fn write_barrier(from: *const GcHeader, to: *const GcHeader) {
    if from.is_null() || to.is_null() {
        return;
    }

    // SAFETY: Caller guarantees pointers are valid
    let from_header = unsafe { &*from };
    let to_header = unsafe { &*to };

    // If writing a white reference into a black object,
    // we need to gray the reference to maintain invariant
    if from_header.mark() == MarkColor::Black && to_header.mark() == MarkColor::White {
        to_header.set_mark(MarkColor::Gray);
        // Note: In a real implementation, we'd also add to a write barrier buffer
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_collector_creation() {
        let heap = GcHeap::new();
        let collector = Collector::new(heap);
        assert_eq!(collector.stats().collections, 0);
    }

    #[test]
    fn test_collect_empty() {
        let heap = GcHeap::new();
        let mut collector = Collector::new(heap);
        collector.collect(&[]);
        assert_eq!(collector.stats().collections, 1);
    }

    #[test]
    fn test_mark_single_root() {
        use crate::object::tags;

        let heap = GcHeap::new();
        let mut collector = Collector::new(heap);

        // Create a header on the stack for testing
        let header = GcHeader::new(tags::OBJECT);
        let root = &header as *const GcHeader;

        collector.collect(&[root]);

        assert_eq!(collector.stats().collections, 1);
        assert_eq!(collector.stats().last_marked, 1);
        assert_eq!(header.mark(), MarkColor::Black);
    }

    #[test]
    fn test_write_barrier() {
        use crate::object::tags;

        let from = GcHeader::new(tags::OBJECT);
        let to = GcHeader::new(tags::OBJECT);

        // Set from to black
        from.set_mark(MarkColor::Black);

        // to is white by default
        assert_eq!(to.mark(), MarkColor::White);

        // Write barrier should gray 'to'
        // SAFETY: Both pointers are valid references to headers on the stack
        unsafe { write_barrier(&from as *const _, &to as *const _) };
        assert_eq!(to.mark(), MarkColor::Gray);
    }
}
