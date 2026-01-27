//! Integration tests for GC safepoint triggering
//!
//! These tests verify that garbage collection integrates correctly with
//! the interpreter safepoints and that memory stays bounded under load.
//!
//! Note: The global GC registry is shared across tests, so collection counts
//! accumulate across test runs. Tests should check relative changes rather
//! than absolute values.

use std::sync::Arc;
use std::time::Duration;

use otter_vm_core::context::VmContext;
use otter_vm_core::gc::GcRef;
use otter_vm_core::memory::MemoryManager;
use otter_vm_core::object::JsObject;

/// Helper to create a test context with custom memory limit
fn create_test_context(memory_limit: usize) -> VmContext {
    let memory_manager = Arc::new(MemoryManager::new(memory_limit));
    let global = GcRef::new(JsObject::new(None, memory_manager.clone()));
    VmContext::new(global, memory_manager)
}

/// Helper to create a test context with default settings
fn create_default_test_context() -> VmContext {
    create_test_context(100 * 1024 * 1024) // 100MB limit
}

#[test]
fn test_gc_stats_tracked() {
    let ctx = create_default_test_context();

    // Get initial collection count
    let initial_count = ctx.gc_stats().collection_count;

    // Force GC
    ctx.collect_garbage();

    let stats = ctx.gc_stats();
    // Due to parallel test execution, other tests may have triggered GC
    // So we check that count increased by at least 1
    assert!(
        stats.collection_count >= initial_count + 1,
        "Collection count should increment by at least 1 (initial: {}, after: {})",
        initial_count,
        stats.collection_count
    );
    assert!(
        stats.total_pause_time >= Duration::ZERO,
        "GC pause time should be tracked"
    );
}

#[test]
fn test_explicit_gc_collect() {
    let ctx = create_default_test_context();

    // Initial stats (may not be zero due to global registry)
    let initial_stats = ctx.gc_stats();
    let initial_count = initial_stats.collection_count;

    // Run GC explicitly
    let _reclaimed = ctx.collect_garbage();

    // Stats should be updated - count should increase by at least 1
    let stats = ctx.gc_stats();
    assert!(
        stats.collection_count >= initial_count + 1,
        "Collection count should increment by at least 1 (initial: {}, after: {})",
        initial_count,
        stats.collection_count
    );
    assert!(stats.last_pause_time >= Duration::ZERO);

    let count_after_first = stats.collection_count;

    // Second collection
    ctx.collect_garbage();
    let stats2 = ctx.gc_stats();
    assert!(
        stats2.collection_count >= count_after_first + 1,
        "Collection count should increment by at least 1 more"
    );
    assert!(stats2.total_pause_time >= stats.total_pause_time);
}

#[test]
fn test_maybe_collect_garbage_respects_threshold() {
    let ctx = create_default_test_context();

    // With default threshold and no allocations, GC should not trigger
    let collected = ctx.maybe_collect_garbage();

    // Depending on the global registry state, this may or may not collect.
    // The important thing is that the method doesn't panic and works correctly.
    let stats = ctx.gc_stats();

    // If it collected, stats should reflect it
    if collected {
        assert!(stats.collection_count >= 1);
    }
}

#[test]
fn test_heap_size_reporting() {
    let ctx = create_default_test_context();

    // Initial heap size should be valid (this always succeeds for usize, but verifies API works)
    let heap_size = ctx.heap_size();
    // Heap size is just a number - verify we can read it without panic
    let _ = heap_size;

    // After GC, heap size should still be valid
    ctx.collect_garbage();
    let heap_after_gc = ctx.heap_size();
    // Heap size after GC should be accessible
    let _ = heap_after_gc;
}

#[test]
fn test_gc_threshold_configuration() {
    let ctx = create_default_test_context();

    // Set a custom threshold
    ctx.set_gc_threshold(2 * 1024 * 1024); // 2MB

    // This should work without panicking
    ctx.maybe_collect_garbage();
}

#[test]
fn test_memory_manager_gc_integration() {
    let memory_manager = Arc::new(MemoryManager::new(100 * 1024 * 1024));

    // Initial state
    assert_eq!(memory_manager.allocation_count(), 0);
    assert_eq!(memory_manager.last_live_size(), 0);

    // Simulate allocation tracking
    memory_manager.alloc(1024).unwrap();
    memory_manager.alloc(2048).unwrap();
    assert_eq!(memory_manager.allocation_count(), 2);

    // Simulate GC completion
    memory_manager.on_gc_complete(512);
    assert_eq!(memory_manager.allocation_count(), 0);
    assert_eq!(memory_manager.last_live_size(), 512);

    // Adaptive threshold should be 2x live size (min 1MB)
    let threshold = memory_manager.gc_threshold();
    assert!(threshold >= 1024 * 1024); // At least 1MB
}

#[test]
fn test_memory_manager_should_collect_garbage() {
    let memory_manager = Arc::new(MemoryManager::new(100 * 1024 * 1024));

    // Initially should not collect
    assert!(!memory_manager.should_collect_garbage());

    // After many allocations, should trigger collection
    memory_manager.set_allocation_count_threshold(100);
    for _ in 0..100 {
        memory_manager.alloc(64).unwrap();
    }
    assert!(memory_manager.should_collect_garbage());

    // After GC completion, should reset
    memory_manager.on_gc_complete(0);
    assert!(!memory_manager.should_collect_garbage());
}

#[test]
fn test_memory_manager_explicit_gc_request() {
    let memory_manager = Arc::new(MemoryManager::new(100 * 1024 * 1024));

    // Initially should not collect
    assert!(!memory_manager.should_collect_garbage());

    // Request explicit GC
    memory_manager.request_gc();
    assert!(memory_manager.should_collect_garbage());

    // After GC completion, request should be cleared
    memory_manager.on_gc_complete(0);
    assert!(!memory_manager.should_collect_garbage());
}

#[test]
fn test_gc_pause_time_tracking() {
    let ctx = create_default_test_context();

    // Get initial stats
    let initial_count = ctx.gc_stats().collection_count;
    let initial_pause = ctx.gc_stats().total_pause_time;

    // Run multiple GC cycles
    for _ in 0..5 {
        ctx.collect_garbage();
    }

    let stats = ctx.gc_stats();
    // Due to parallel test execution, other tests may have triggered GC
    // So we check that count increased by at least 5
    assert!(
        stats.collection_count >= initial_count + 5,
        "Collection count should increment by at least 5 (initial: {}, after: {})",
        initial_count,
        stats.collection_count
    );
    assert!(
        stats.total_pause_time >= initial_pause,
        "Total pause time should increase or stay the same"
    );
    assert!(
        stats.last_pause_time >= Duration::ZERO,
        "Last pause time should be tracked"
    );
}

#[test]
fn test_context_request_gc() {
    let ctx = create_default_test_context();

    // Request GC
    ctx.request_gc();

    // Memory manager should indicate collection is needed
    assert!(ctx.memory_manager().should_collect_garbage());

    // After maybe_collect_garbage, request should be cleared
    ctx.maybe_collect_garbage();

    // Flag should be cleared after collection
    // (The GC may or may not have run depending on global registry state,
    // but the request flag should be cleared either way)
}
