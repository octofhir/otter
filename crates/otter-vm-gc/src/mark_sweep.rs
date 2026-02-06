//! Stop-the-World Mark/Sweep Garbage Collector
//!
//! This module implements a simple stop-the-world mark/sweep collector
//! that can collect circular references.
//!
//! ## Design
//!
//! - **Object Tracking**: All GC-managed allocations are tracked in a central registry
//! - **Tri-color Marking**: Uses white/gray/black marking for cycle detection
//! - **Iterative Marking**: Uses a gray worklist to avoid stack overflow
//! - **Sweep**: Frees all white (unreachable) objects after marking

use parking_lot::RwLock;
use std::cell::Cell;
use std::collections::{HashSet, VecDeque};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

thread_local! {
    /// Flag indicating that `dealloc_all` is in progress on this thread.
    /// When true, Drop impls should NOT access other GC-managed objects
    /// because they may have already been freed.
    static GC_DEALLOC_IN_PROGRESS: Cell<bool> = const { Cell::new(false) };
}

/// Returns true if the GC is currently freeing all allocations on this thread.
/// Drop impls should check this and skip any access to other GC objects.
pub fn is_dealloc_in_progress() -> bool {
    GC_DEALLOC_IN_PROGRESS.with(|f| f.get())
}
use std::time::{Duration, Instant};

use crate::object::{GcHeader, MarkColor};

/// Type-erased drop function for cleaning up allocations
type DropFn = unsafe fn(*mut u8);

/// Type-erased trace function for marking references
type TraceFn = unsafe fn(*const u8, &mut dyn FnMut(*const GcHeader));

/// Allocation entry in the registry
struct AllocationEntry {
    /// Pointer to the GcHeader at the start of the allocation
    header: *mut GcHeader,
    /// Size of the allocation (header + value)
    size: usize,
    /// Drop function for this allocation
    drop_fn: DropFn,
    /// Trace function for this allocation
    trace_fn: Option<TraceFn>,
}

// SAFETY: AllocationEntry contains raw pointers but they are managed exclusively
// by the AllocationRegistry which is protected by RwLock
unsafe impl Send for AllocationEntry {}
unsafe impl Sync for AllocationEntry {}

/// Central registry tracking all GC-managed allocations
pub struct AllocationRegistry {
    /// All tracked allocations
    allocations: RwLock<Vec<AllocationEntry>>,
    /// Total bytes allocated
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
}

impl AllocationRegistry {
    /// Create a new allocation registry
    pub fn new() -> Self {
        Self {
            allocations: RwLock::new(Vec::with_capacity(1024)),
            total_bytes: AtomicUsize::new(0),
            gc_threshold: AtomicUsize::new(1024 * 1024), // 1MB default
            collection_count: AtomicUsize::new(0),
            last_reclaimed: AtomicUsize::new(0),
            total_pause_nanos: AtomicU64::new(0),
            last_pause_nanos: AtomicU64::new(0),
        }
    }

    /// Create a new registry with a custom GC threshold
    pub fn with_threshold(threshold: usize) -> Self {
        let registry = Self::new();
        registry.gc_threshold.store(threshold, Ordering::Relaxed);
        registry
    }

    /// Register a new allocation
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
        let entry = AllocationEntry {
            header,
            size,
            drop_fn,
            trace_fn,
        };
        self.allocations.write().push(entry);
        self.total_bytes.fetch_add(size, Ordering::Relaxed);
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

    /// Get the number of allocations
    pub fn allocation_count(&self) -> usize {
        self.allocations.read().len()
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
    ///
    /// # Arguments
    /// - `roots`: Pointers to GcHeaders that are roots (live references)
    ///
    /// # Returns
    /// Number of bytes reclaimed
    pub fn collect(&self, roots: &[*const GcHeader]) -> usize {
        let start = Instant::now();

        #[cfg(feature = "gc_logging")]
        let initial_bytes = self.total_bytes.load(Ordering::Relaxed);
        #[cfg(feature = "gc_logging")]
        let initial_count = self.allocations.read().len();

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
        self.last_pause_nanos.store(elapsed_nanos, Ordering::Relaxed);

        #[cfg(feature = "gc_logging")]
        {
            let final_bytes = self.total_bytes.load(Ordering::Relaxed);
            let final_count = self.allocations.read().len();

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
        let allocations = self.allocations.read();
        for entry in allocations.iter() {
            unsafe {
                (*entry.header).set_mark(MarkColor::White);
            }
        }
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

        // Process the worklist until empty
        let allocations = self.allocations.read();
        while let Some(ptr) = worklist.pop_front() {
            unsafe {
                let header = &*ptr;

                // Skip if already black (fully processed)
                if header.mark() == MarkColor::Black {
                    continue;
                }

                // Find the allocation entry for this header to get the trace function
                if let Some(entry) = allocations.iter().find(|e| std::ptr::eq(e.header, ptr))
                    && let Some(trace_fn) = entry.trace_fn
                {
                    // Trace the object's references
                    let data_ptr = (entry.header as *const u8)
                        .add(std::mem::size_of::<GcHeader>());
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
        let mut allocations = self.allocations.write();
        let mut reclaimed: usize = 0;

        // Partition: keep marked objects, collect unmarked for freeing
        let mut live_allocations = Vec::with_capacity(allocations.len());
        let mut dead_allocations = Vec::new();

        for entry in allocations.drain(..) {
            unsafe {
                if (*entry.header).mark() == MarkColor::White {
                    // Unmarked - will be freed
                    reclaimed += entry.size;
                    dead_allocations.push(entry);
                } else {
                    // Marked - reset to white for next GC cycle
                    (*entry.header).set_mark(MarkColor::White);
                    live_allocations.push(entry);
                }
            }
        }

        // Replace with live allocations
        *allocations = live_allocations;

        // Update total bytes
        self.total_bytes.fetch_sub(reclaimed, Ordering::Relaxed);

        // Drop the write lock before deallocation.
        // This is safe because collect_lock serializes entire GC cycles,
        // so no other thread can be marking headers while we deallocate.
        drop(allocations);

        for entry in dead_allocations {
            unsafe {
                (entry.drop_fn)(entry.header as *mut u8);
            }
        }

        reclaimed
    }

    /// Deallocate ALL tracked allocations without marking.
    ///
    /// Use this when tearing down an engine/isolate to reclaim all memory.
    /// After calling this, no GcRef pointers from this registry are valid.
    pub fn dealloc_all(&self) -> usize {
        let mut allocations = self.allocations.write();
        let mut reclaimed: usize = 0;

        let entries: Vec<AllocationEntry> = allocations.drain(..).collect();
        let total = self.total_bytes.load(Ordering::Relaxed);
        self.total_bytes.store(0, Ordering::Relaxed);
        drop(allocations);

        // Set the dealloc-in-progress flag so that Drop impls skip
        // accessing other GC objects (which may already be freed).
        GC_DEALLOC_IN_PROGRESS.with(|f| f.set(true));

        for entry in entries {
            reclaimed += entry.size;
            unsafe {
                (entry.drop_fn)(entry.header as *mut u8);
            }
        }

        GC_DEALLOC_IN_PROGRESS.with(|f| f.set(false));

        // reclaimed may differ from total due to race conditions; use actual total
        let _ = reclaimed;
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

/// Thread-local allocation registry for the GC.
///
/// Each thread gets its own registry so that GC collections in one thread
/// (with that thread's roots) don't sweep objects belonging to another thread.
/// This prevents use-after-free when multiple VmContexts run in parallel
/// (e.g. in test suites).
///
/// The registry is leaked (Box::leak) to produce a `&'static` reference that
/// matches the existing API. Each thread leaks exactly one AllocationRegistry
/// for the lifetime of the process — a bounded, negligible leak.
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
    // Allocate memory for GcHeader + T
    let layout = std::alloc::Layout::new::<(GcHeader, T)>();
    // SAFETY: Layout is valid and non-zero sized
    let ptr = unsafe { std::alloc::alloc(layout) as *mut (GcHeader, T) };

    if ptr.is_null() {
        std::alloc::handle_alloc_error(layout);
    }

    // SAFETY: ptr is non-null and properly aligned
    unsafe {
        // Initialize header and value
        std::ptr::write(&mut (*ptr).0, GcHeader::new(0));
        std::ptr::write(&mut (*ptr).1, value);
    }

    // Register with the GC
    let header_ptr = ptr as *mut GcHeader;
    let drop_fn: DropFn = drop_gc_box::<T>;
    let trace_fn: Option<TraceFn> = if T::NEEDS_TRACE {
        Some(trace_gc_box::<T>)
    } else {
        None
    };

    // SAFETY: header_ptr is valid, drop_fn and trace_fn are correct for the type
    unsafe {
        registry.register(header_ptr, layout.size(), drop_fn, trace_fn);
    }

    // Return pointer to the value (after the header)
    // SAFETY: ptr is valid and points to initialized memory
    unsafe { &mut (*ptr).1 as *mut T }
}

/// Drop function for GC boxes
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
        let header_ptr = unsafe {
            (ptr as *const u8).sub(std::mem::size_of::<GcHeader>()) as *const GcHeader
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
            gc_alloc_in(&registry, Node {
                value: 2,
                next_header: None,
            })
        };
        let node2_header = unsafe {
            (node2 as *const u8).sub(std::mem::size_of::<GcHeader>()) as *const GcHeader
        };

        let node1 = unsafe {
            gc_alloc_in(&registry, Node {
                value: 1,
                next_header: Some(node2_header),
            })
        };
        let node1_header = unsafe {
            (node1 as *const u8).sub(std::mem::size_of::<GcHeader>()) as *const GcHeader
        };

        // Also create an unreachable node
        unsafe {
            let _ = gc_alloc_in(&registry, Node {
                value: 999,
                next_header: None,
            });
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
            gc_alloc_in(&registry, Node {
                value: 1,
                next_header: None, // Will be set after node2 is created
            })
        };
        let node1_header = unsafe {
            (node1 as *const u8).sub(std::mem::size_of::<GcHeader>()) as *const GcHeader
        };

        let node2 = unsafe {
            gc_alloc_in(&registry, Node {
                value: 2,
                next_header: Some(node1_header), // Points back to node1
            })
        };
        let node2_header = unsafe {
            (node2 as *const u8).sub(std::mem::size_of::<GcHeader>()) as *const GcHeader
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
        // Each i64 allocation is 8 bytes + GcHeader (8 bytes) = 16 bytes minimum
        for i in 0..10 {
            unsafe {
                let _ = gc_alloc_in(&registry, i as i64);
            }
        }

        assert!(registry.should_gc());
    }
}
