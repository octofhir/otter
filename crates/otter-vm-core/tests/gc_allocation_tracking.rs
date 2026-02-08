//! Validation tests for unified GC allocation tracking
//!
//! Verifies that MemoryManager accurately tracks all GC allocations
//! including strings, BigInt, Symbol, and objects.

use otter_vm_core::context::VmContext;
use otter_vm_core::gc::GcRef;
use otter_vm_core::memory::MemoryManager;
use otter_vm_core::object::JsObject;
use otter_vm_core::string::JsString;
use otter_vm_core::value::Value;
use std::sync::Arc;

#[test]
fn test_memorymanager_tracks_string_allocations() {
    let mm = Arc::new(MemoryManager::new(10_000_000));
    let global = GcRef::new(JsObject::new(Value::null(), mm.clone()));
    let _ctx = VmContext::new(global, mm.clone());

    let initial_allocated = mm.allocated();

    // Allocate strings through GcRef::new
    let s1 = JsString::intern("test string 1");
    let s2 = JsString::intern("test string 2");
    let s3 = JsString::new_gc("non-interned string");

    // MemoryManager should track these allocations
    let after_strings = mm.allocated();
    assert!(
        after_strings > initial_allocated,
        "MemoryManager should track string allocations: {} vs {}",
        after_strings,
        initial_allocated
    );

    // Touch the strings to prevent compiler optimization
    let _ = (s1, s2, s3);
}

#[test]
fn test_memorymanager_tracks_object_allocations() {
    let mm = Arc::new(MemoryManager::new(10_000_000));
    let global = GcRef::new(JsObject::new(Value::null(), mm.clone()));
    let _ctx = VmContext::new(global, mm.clone());

    let initial_allocated = mm.allocated();

    // Allocate JsObjects
    let o1 = GcRef::new(JsObject::new(Value::null(), mm.clone()));
    let o2 = GcRef::new(JsObject::new(Value::null(), mm.clone()));
    let o3 = GcRef::new(JsObject::new(Value::null(), mm.clone()));

    let after_objects = mm.allocated();
    assert!(
        after_objects > initial_allocated,
        "MemoryManager should track object allocations: {} vs {}",
        after_objects,
        initial_allocated
    );

    let _ = (o1, o2, o3);
}

#[test]
fn test_memorymanager_reconciles_after_gc() {
    let mm = Arc::new(MemoryManager::new(10_000_000));
    let global = GcRef::new(JsObject::new(Value::null(), mm.clone()));
    let ctx = VmContext::new(global, mm.clone());

    // Allocate a bunch of objects
    for i in 0..1000 {
        let _s = JsString::intern(&format!("string_{}", i));
    }

    let before_gc_allocated = mm.allocated();
    let before_gc_count = mm.allocation_count();

    // Trigger GC
    let reclaimed = ctx.collect_garbage();

    let after_gc_allocated = mm.allocated();
    let after_gc_count = mm.allocation_count();

    println!("Before GC: {} bytes, {} allocations", before_gc_allocated, before_gc_count);
    println!("After GC:  {} bytes, {} allocations", after_gc_allocated, after_gc_count);
    println!("Reclaimed: {} bytes", reclaimed);

    // After GC, allocation count should be reset
    assert_eq!(
        after_gc_count, 0,
        "Allocation count should be reset after GC"
    );

    // After GC, allocated bytes should match GC registry
    let gc_total_bytes = otter_vm_gc::global_registry().total_bytes();
    assert_eq!(
        after_gc_allocated, gc_total_bytes,
        "MemoryManager allocated should match GC registry after reconciliation: {} vs {}",
        after_gc_allocated, gc_total_bytes
    );
}

#[test]
fn test_memorymanager_gc_accuracy() {
    let mm = Arc::new(MemoryManager::new(10_000_000));
    let global = GcRef::new(JsObject::new(Value::null(), mm.clone()));
    let ctx = VmContext::new(global, mm.clone());

    // Allocate many different types
    for i in 0..100 {
        let _s = JsString::intern(&format!("test_{}", i));
        let _obj = GcRef::new(JsObject::new(Value::null(), mm.clone()));
    }

    // Before GC: MemoryManager accumulates allocations
    // It may be higher than GC registry due to not yet freed objects
    let mm_before_gc = mm.allocated();
    let gc_before_gc = otter_vm_gc::global_registry().total_bytes();

    println!("Before GC: MM={}, GC={}", mm_before_gc, gc_before_gc);

    // MemoryManager should be tracking SOMETHING (not zero)
    assert!(
        mm_before_gc > 0,
        "MemoryManager should track allocations before GC"
    );

    // Trigger GC - this reconciles MemoryManager with GC reality
    ctx.collect_garbage();

    // After GC, they should match exactly (reconciliation happened)
    let mm_allocated_after = mm.allocated();
    let gc_total_bytes_after = otter_vm_gc::global_registry().total_bytes();

    println!("After GC:  MM={}, GC={}", mm_allocated_after, gc_total_bytes_after);

    assert_eq!(
        mm_allocated_after, gc_total_bytes_after,
        "After GC, MemoryManager and GC registry should match exactly: {} vs {}",
        mm_allocated_after, gc_total_bytes_after
    );
}

#[test]
fn test_thread_local_memorymanager_isolation() {
    use std::thread;

    // Thread 1: Has its own VmContext
    let handle1 = thread::spawn(|| {
        let mm = Arc::new(MemoryManager::new(10_000_000));
        let global = GcRef::new(JsObject::new(Value::null(), mm.clone()));
        let _ctx = VmContext::new(global, mm.clone());

        // This thread should see its own MemoryManager
        assert!(
            MemoryManager::current().is_some(),
            "Thread 1 should have thread-local MM"
        );

        let _s = JsString::intern("thread1 string");
        mm.allocated()
    });

    // Thread 2: Has its own VmContext
    let handle2 = thread::spawn(|| {
        let mm = Arc::new(MemoryManager::new(10_000_000));
        let global = GcRef::new(JsObject::new(Value::null(), mm.clone()));
        let _ctx = VmContext::new(global, mm.clone());

        // This thread should see its own MemoryManager
        assert!(
            MemoryManager::current().is_some(),
            "Thread 2 should have thread-local MM"
        );

        let _s = JsString::intern("thread2 string");
        mm.allocated()
    });

    // Main thread: No VmContext
    assert!(
        MemoryManager::current().is_none(),
        "Main thread without VmContext should not have thread-local MM"
    );

    let allocated1 = handle1.join().unwrap();
    let allocated2 = handle2.join().unwrap();

    // Each thread tracked its own allocations independently
    assert!(allocated1 > 0, "Thread 1 should have allocations");
    assert!(allocated2 > 0, "Thread 2 should have allocations");
}

#[test]
fn test_memorymanager_cleared_on_drop() {
    // Create a scope where VmContext exists
    {
        let mm = Arc::new(MemoryManager::new(10_000_000));
        let global = GcRef::new(JsObject::new(Value::null(), mm.clone()));
        let _ctx = VmContext::new(global, mm.clone());

        assert!(
            MemoryManager::current().is_some(),
            "MemoryManager should be set while VmContext exists"
        );
    }

    // After VmContext drops, thread-local should be cleared
    assert!(
        MemoryManager::current().is_none(),
        "MemoryManager should be cleared after VmContext drops"
    );
}

#[test]
fn test_stress_allocation_tracking() {
    let mm = Arc::new(MemoryManager::new(100_000_000)); // 100MB limit
    let global = GcRef::new(JsObject::new(Value::null(), mm.clone()));
    let ctx = VmContext::new(global, mm.clone());

    // Allocate 10,000 objects of mixed types
    for i in 0..10_000 {
        let _s = JsString::intern(&format!("stress_test_{}", i));

        if i % 3 == 0 {
            let _obj = GcRef::new(JsObject::new(Value::null(), mm.clone()));
        }

        // Trigger GC every 1000 allocations
        if i % 1000 == 0 && i > 0 {
            ctx.collect_garbage();

            // Verify reconciliation
            let mm_allocated = mm.allocated();
            let gc_total = otter_vm_gc::global_registry().total_bytes();
            assert_eq!(
                mm_allocated, gc_total,
                "Iteration {}: MM and GC should match after collection: {} vs {}",
                i, mm_allocated, gc_total
            );
        }
    }

    println!("Stress test completed successfully");
    println!("Final MM allocated: {} bytes", mm.allocated());
    println!("Final GC total: {} bytes", otter_vm_gc::global_registry().total_bytes());
}
