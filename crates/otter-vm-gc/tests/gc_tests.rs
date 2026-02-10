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
    unsafe { (ptr as *const u8).sub(std::mem::size_of::<GcHeader>()) as *const GcHeader }
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
fn test_rooted_objects_survive() {
    let registry = AllocationRegistry::new();

    // Allocate and keep a root
    let obj = unsafe {
        gc_alloc_in(
            &registry,
            TestObject {
                value: 42,
                reference: None,
            },
        )
    };

    // Get the header for rooting
    let header = unsafe { header_from_ptr(obj) };

    let initial_size = registry.total_bytes();

    // Force GC with this object as root
    let reclaimed = registry.collect(&[header]);

    // Rooted object should survive
    assert_eq!(reclaimed, 0);
    assert_eq!(registry.allocation_count(), 1);
    assert_eq!(registry.total_bytes(), initial_size);

    // Value should still be accessible
    unsafe {
        assert_eq!((*obj).value, 42);
    }
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
fn test_circular_references_survive_when_rooted() {
    let registry = AllocationRegistry::new();

    // Create a circular structure
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
                reference: Some(header1),
            },
        )
    };
    let header2 = unsafe { header_from_ptr(obj2) };

    // Complete the cycle
    unsafe {
        (*obj1).reference = Some(header2);
    }

    // Root only obj1
    let reclaimed = registry.collect(&[header1]);

    // Both should survive (obj2 is reachable through obj1)
    assert_eq!(reclaimed, 0);
    assert_eq!(registry.allocation_count(), 2);

    // Both values should still be accessible
    unsafe {
        assert_eq!((*obj1).value, 1);
        assert_eq!((*obj2).value, 2);
    }
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
fn test_transitive_reachability() {
    let registry = AllocationRegistry::new();

    // Create a chain: root -> obj1 -> obj2 -> obj3
    let obj3 = unsafe {
        gc_alloc_in(
            &registry,
            TestObject {
                value: 3,
                reference: None,
            },
        )
    };
    let header3 = unsafe { header_from_ptr(obj3) };

    let obj2 = unsafe {
        gc_alloc_in(
            &registry,
            TestObject {
                value: 2,
                reference: Some(header3),
            },
        )
    };
    let header2 = unsafe { header_from_ptr(obj2) };

    let obj1 = unsafe {
        gc_alloc_in(
            &registry,
            TestObject {
                value: 1,
                reference: Some(header2),
            },
        )
    };
    let header1 = unsafe { header_from_ptr(obj1) };

    // Also create an unreachable object
    unsafe {
        let _ = gc_alloc_in(
            &registry,
            TestObject {
                value: 999,
                reference: None,
            },
        );
    }

    assert_eq!(registry.allocation_count(), 4);

    // Root only obj1 - entire chain should survive
    let reclaimed = registry.collect(&[header1]);

    // The unreachable object should be freed
    assert!(reclaimed > 0);
    assert_eq!(registry.allocation_count(), 3);

    // All reachable objects should still be valid
    unsafe {
        assert_eq!((*obj1).value, 1);
        assert_eq!((*obj2).value, 2);
        assert_eq!((*obj3).value, 3);
    }
}

#[test]
fn test_multiple_roots() {
    let registry = AllocationRegistry::new();

    // Create multiple independent object graphs
    let obj_a = unsafe {
        gc_alloc_in(
            &registry,
            TestObject {
                value: 1,
                reference: None,
            },
        )
    };
    let header_a = unsafe { header_from_ptr(obj_a) };

    let obj_b = unsafe {
        gc_alloc_in(
            &registry,
            TestObject {
                value: 2,
                reference: None,
            },
        )
    };
    let header_b = unsafe { header_from_ptr(obj_b) };

    // Unreachable object
    unsafe {
        let _ = gc_alloc_in(
            &registry,
            TestObject {
                value: 999,
                reference: None,
            },
        );
    }

    assert_eq!(registry.allocation_count(), 3);

    // Root both independent objects
    let reclaimed = registry.collect(&[header_a, header_b]);

    // Only unreachable object should be freed
    assert!(reclaimed > 0);
    assert_eq!(registry.allocation_count(), 2);
}

#[test]
fn test_multiple_gc_cycles() {
    let registry = AllocationRegistry::new();

    // First cycle
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

    unsafe {
        let _ = gc_alloc_in(
            &registry,
            TestObject {
                value: 2,
                reference: None,
            },
        ); // unreachable
    }

    registry.collect(&[header1]);
    assert_eq!(registry.allocation_count(), 1);

    // Second cycle - add more objects
    let obj3 = unsafe {
        gc_alloc_in(
            &registry,
            TestObject {
                value: 3,
                reference: Some(header1),
            },
        )
    };
    let header3 = unsafe { header_from_ptr(obj3) };

    unsafe {
        let _ = gc_alloc_in(
            &registry,
            TestObject {
                value: 4,
                reference: None,
            },
        ); // unreachable
    }

    registry.collect(&[header3]);
    assert_eq!(registry.allocation_count(), 2); // obj1 and obj3 survive

    // Third cycle - drop all roots
    registry.collect(&[]);
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

#[test]
fn test_self_referential_object() {
    let registry = AllocationRegistry::new();

    // Create an object that references itself
    let obj = unsafe {
        gc_alloc_in(
            &registry,
            TestObject {
                value: 42,
                reference: None,
            },
        )
    };
    let header = unsafe { header_from_ptr(obj) };

    // Make it self-referential
    unsafe {
        (*obj).reference = Some(header);
    }

    assert_eq!(registry.allocation_count(), 1);

    // Without rooting, it should be collected (despite self-reference)
    registry.collect(&[]);
    assert_eq!(registry.allocation_count(), 0);

    // Create another self-referential object and root it
    let obj2 = unsafe {
        gc_alloc_in(
            &registry,
            TestObject {
                value: 100,
                reference: None,
            },
        )
    };
    let header2 = unsafe { header_from_ptr(obj2) };
    unsafe {
        (*obj2).reference = Some(header2);
    }

    // With rooting, it should survive
    registry.collect(&[header2]);
    assert_eq!(registry.allocation_count(), 1);

    unsafe {
        assert_eq!((*obj2).value, 100);
    }
}
