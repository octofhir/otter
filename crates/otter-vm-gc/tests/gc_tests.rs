//! GC correctness tests
//!
//! These tests verify that the stop-the-world mark/sweep garbage collector
//! correctly handles various scenarios.

use otter_vm_gc::{AllocationRegistry, GcHeader, GcTraceable, gc_alloc_in};

/// Simple test object for GC testing
struct TestObject {
    value: i32,
    /// Optional reference to another object's GcHeader
    reference: Option<*const GcHeader>,
}

impl GcTraceable for TestObject {
    const NEEDS_TRACE: bool = true;

    fn trace(&self, tracer: &mut dyn FnMut(*const GcHeader)) {
        if let Some(ptr) = self.reference {
            tracer(ptr);
        }
    }
}

/// Get header pointer from a value pointer
unsafe fn header_from_ptr<T>(ptr: *const T) -> *const GcHeader {
    // SAFETY: ptr points to the value after the GcHeader
    unsafe {
        (ptr as *const u8).sub(std::mem::offset_of!(
            otter_vm_gc::object::GcAllocation<T>,
            value
        )) as *const GcHeader
    }
}

#[test]
fn test_collect_simple_garbage() {
    let registry = AllocationRegistry::new();

    // Create object without rooting
    unsafe {
        let _ = gc_alloc_in(
            &registry,
            TestObject {
                value: 42,
                reference: None,
            },
        );
    }

    // Verify allocation
    assert_eq!(registry.allocation_count(), 1);
    let initial_size = registry.total_bytes();
    assert!(initial_size > 0);

    // Force GC with no roots
    let reclaimed = registry.collect(&[]);

    // Object should be collected
    assert!(reclaimed > 0);
    assert_eq!(registry.allocation_count(), 0);
    assert_eq!(registry.total_bytes(), 0);
}

#[test]
fn test_circular_references_collected() {
    let registry = AllocationRegistry::new();

    // Create two objects
    let obj1 = unsafe {
        gc_alloc_in(
            &registry,
            TestObject {
                value: 1,
                reference: None,
            },
        )
    };
    let header1 = unsafe { header_from_ptr(obj1) };

    let obj2 = unsafe {
        gc_alloc_in(
            &registry,
            TestObject {
                value: 2,
                reference: Some(header1), // obj2 -> obj1
            },
        )
    };
    let header2 = unsafe { header_from_ptr(obj2) };

    // Complete the cycle: obj1 -> obj2
    unsafe {
        (*obj1).reference = Some(header2);
    }

    assert_eq!(registry.allocation_count(), 2);
    let initial_size = registry.total_bytes();
    assert!(initial_size > 0);

    // Force GC with NO roots - both objects should be collected
    let reclaimed = registry.collect(&[]);

    // Cycle should be collected
    assert!(reclaimed > 0);
    assert_eq!(registry.allocation_count(), 0);
    assert_eq!(registry.total_bytes(), 0);
}

#[test]
fn test_heap_growth_bounded() {
    let registry = AllocationRegistry::with_threshold(1024); // Small threshold

    // Allocate many temporary objects (not rooted)
    for i in 0..100 {
        unsafe {
            let _ = gc_alloc_in(
                &registry,
                TestObject {
                    value: i,
                    reference: None,
                },
            );
        }

        // Trigger GC periodically to keep heap bounded
        if i % 10 == 9 {
            registry.collect(&[]);
        }
    }

    // Final collection
    registry.collect(&[]);

    // Heap should be empty (no roots)
    assert_eq!(registry.total_bytes(), 0);
    assert_eq!(registry.allocation_count(), 0);
}

#[test]
fn test_gc_statistics() {
    let registry = AllocationRegistry::new();

    // Initial stats
    let stats = registry.stats();
    assert_eq!(stats.collection_count, 0);
    assert_eq!(stats.total_bytes, 0);
    assert_eq!(stats.allocation_count, 0);

    // Allocate
    unsafe {
        let _ = gc_alloc_in(
            &registry,
            TestObject {
                value: 42,
                reference: None,
            },
        );
    }

    let stats = registry.stats();
    assert_eq!(stats.allocation_count, 1);
    assert!(stats.total_bytes > 0);

    // Collect
    registry.collect(&[]);

    let stats = registry.stats();
    assert_eq!(stats.collection_count, 1);
    assert_eq!(stats.allocation_count, 0);
    assert!(stats.last_reclaimed > 0);
}

#[test]
fn test_should_gc_threshold() {
    let registry = AllocationRegistry::with_threshold(200);

    // Should not trigger GC initially
    assert!(!registry.should_gc());

    // Allocate until threshold is exceeded
    for _ in 0..10 {
        unsafe {
            let _ = gc_alloc_in(
                &registry,
                TestObject {
                    value: 0,
                    reference: None,
                },
            );
        }
    }

    // Should trigger GC now
    assert!(registry.should_gc());

    // After collection, should not trigger
    registry.collect(&[]);
    assert!(!registry.should_gc());
}
