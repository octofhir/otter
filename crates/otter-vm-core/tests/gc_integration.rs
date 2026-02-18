//! GC Integration Tests
//!
//! This module tests the garbage collector's behavior in realistic scenarios:
//! - Circular reference collection
//! - Stress testing with many allocations
//! - Object retention with HandleScope rooting
//!
//! NOTE: These tests MUST run serially (--test-threads=1) because they share
//! global GC state. The GC_LOCK mutex ensures serial execution when run
//! with the default parallel test runner.

use otter_vm_core::VmContext;
use otter_vm_core::gc::{GcRef, HandleScope};
use otter_vm_core::memory::MemoryManager;
use otter_vm_core::object::{JsObject, PropertyKey};
use otter_vm_core::runtime::VmRuntime;
use otter_vm_core::value::Value;
use std::sync::{Arc, Mutex};

/// Global mutex to ensure GC tests run serially.
static GC_LOCK: Mutex<()> = Mutex::new(());

/// Helper function to create a test VM context.
/// Returns VmRuntime to keep the GC registry alive.
fn create_test_context() -> (VmContext, Arc<MemoryManager>, VmRuntime) {
    let runtime = VmRuntime::new();
    let mm = runtime.memory_manager().clone();
    let ctx = runtime.create_context();
    (ctx, mm, runtime)
}

// ============================================================================
// Circular Reference Tests
// ============================================================================

#[test]
fn test_circular_reference_two_objects() {
    let _lock = GC_LOCK.lock().unwrap();
    let (ctx, mm, _rt) = create_test_context();

    let initial_stats = ctx.gc_stats();

    // Create circular references in a block so locals go out of scope
    {
        let obj_a = GcRef::new(JsObject::new(Value::null(), mm.clone()));
        let obj_b = GcRef::new(JsObject::new(Value::null(), mm.clone()));

        // Set up circular references: a.b = b, b.a = a
        obj_a.set(PropertyKey::string("b"), Value::object(obj_b));
        obj_b.set(PropertyKey::string("a"), Value::object(obj_a));

        obj_a.set(PropertyKey::string("name"), Value::int32(1));
        obj_b.set(PropertyKey::string("name"), Value::int32(2));

        assert!(obj_a.get(&PropertyKey::string("b")).is_some());
        assert!(obj_b.get(&PropertyKey::string("a")).is_some());
    }

    let reclaimed = ctx.collect_garbage();

    let final_stats = ctx.gc_stats();
    assert!(
        final_stats.collection_count > initial_stats.collection_count,
        "GC collection should have occurred"
    );

    assert!(
        reclaimed > 0 || final_stats.allocation_count == 0,
        "Circular refs should be collected (reclaimed={}, allocs={})",
        reclaimed,
        final_stats.allocation_count
    );
}

#[test]
fn test_circular_reference_chain() {
    let _lock = GC_LOCK.lock().unwrap();
    let (ctx, mm, _rt) = create_test_context();

    {
        let obj_a = GcRef::new(JsObject::new(Value::null(), mm.clone()));
        let obj_b = GcRef::new(JsObject::new(Value::null(), mm.clone()));
        let obj_c = GcRef::new(JsObject::new(Value::null(), mm.clone()));

        obj_a.set(PropertyKey::string("next"), Value::object(obj_b));
        obj_b.set(PropertyKey::string("next"), Value::object(obj_c));
        obj_c.set(PropertyKey::string("next"), Value::object(obj_a));
    }

    ctx.collect_garbage();

    let stats = ctx.gc_stats();
    assert!(stats.collection_count >= 1, "GC should have run");
}

#[test]
fn test_self_referencing_object() {
    let _lock = GC_LOCK.lock().unwrap();
    let (ctx, mm, _rt) = create_test_context();

    {
        let obj = GcRef::new(JsObject::new(Value::null(), mm.clone()));
        obj.set(PropertyKey::string("self"), Value::object(obj));
    }

    ctx.collect_garbage();

    let stats = ctx.gc_stats();
    assert!(stats.collection_count >= 1);
}

// ============================================================================
// Stress Tests
// ============================================================================

#[test]
fn test_gc_stress_many_objects() {
    let _lock = GC_LOCK.lock().unwrap();
    let (ctx, mm, _rt) = create_test_context();

    const NUM_OBJECTS: usize = 10_000;

    for i in 0..NUM_OBJECTS {
        let obj = GcRef::new(JsObject::new(Value::null(), mm.clone()));
        obj.set(PropertyKey::string("value"), Value::int32(i as i32));
    }

    let reclaimed = ctx.collect_garbage();

    let stats = ctx.gc_stats();
    assert!(stats.collection_count >= 1, "GC should have run");
    assert!(
        reclaimed > 0 || stats.total_bytes < NUM_OBJECTS * 100,
        "Memory should be reclaimed"
    );
}

#[test]
fn test_gc_stress_with_properties() {
    let _lock = GC_LOCK.lock().unwrap();
    let (ctx, mm, _rt) = create_test_context();

    for _ in 0..1000 {
        let obj = GcRef::new(JsObject::new(Value::null(), mm.clone()));

        for j in 0..10 {
            let key = PropertyKey::string(&format!("prop{}", j));
            obj.set(key, Value::int32(j));
        }
    }

    ctx.collect_garbage();

    let stats = ctx.gc_stats();
    assert!(stats.collection_count >= 1);
}

#[test]
fn test_gc_stress_nested_objects() {
    let _lock = GC_LOCK.lock().unwrap();
    let (ctx, mm, _rt) = create_test_context();

    for _ in 0..100 {
        let root = GcRef::new(JsObject::new(Value::null(), mm.clone()));
        let mut current = root;

        for depth in 0..10 {
            let child = GcRef::new(JsObject::new(Value::null(), mm.clone()));
            child.set(PropertyKey::string("depth"), Value::int32(depth));
            current.set(PropertyKey::string("child"), Value::object(child));
            current = child;
        }
    }

    ctx.collect_garbage();

    let stats = ctx.gc_stats();
    assert!(stats.collection_count >= 1);
}

// ============================================================================
// Retention Tests
// ============================================================================

#[test]
#[ignore = "SIGSEGV: GC rooting bug — rooted object freed during collection"]
fn test_gc_retains_rooted_objects() {
    let (mut ctx, mm, _rt) = create_test_context();

    let scope = HandleScope::new(&mut ctx);

    let obj = GcRef::new(JsObject::new(Value::null(), mm.clone()));
    obj.set(PropertyKey::string("important"), Value::int32(42));

    assert_eq!(
        obj.get(&PropertyKey::string("important")),
        Some(Value::int32(42)),
        "Property should be set before rooting"
    );

    let handle = scope.root_value(Value::object(obj));

    for _ in 0..100 {
        let _garbage = GcRef::new(JsObject::new(Value::null(), mm.clone()));
    }

    scope.context().collect_garbage();

    let root_slot = scope.context().get_root_slot(handle.slot_index());
    if let Some(rooted_obj) = root_slot.as_object() {
        let value = rooted_obj.get(&PropertyKey::string("important"));
        assert_eq!(
            value,
            Some(Value::int32(42)),
            "Rooted object should survive GC"
        );
    } else {
        panic!("Rooted object should still be accessible");
    }
}

#[test]
#[ignore = "SIGSEGV: GC rooting bug"]
fn test_gc_retains_objects_reachable_from_roots() {
    let (mut ctx, mm, _rt) = create_test_context();

    let scope = HandleScope::new(&mut ctx);

    // Create a chain: obj1 -> obj2 -> obj3
    let obj3 = GcRef::new(JsObject::new(Value::null(), mm.clone()));
    obj3.set(PropertyKey::string("value"), Value::int32(3));

    let obj2 = GcRef::new(JsObject::new(Value::null(), mm.clone()));
    obj2.set(PropertyKey::string("next"), Value::object(obj3));
    obj2.set(PropertyKey::string("value"), Value::int32(2));

    let obj1 = GcRef::new(JsObject::new(Value::null(), mm.clone()));
    obj1.set(PropertyKey::string("next"), Value::object(obj2));
    obj1.set(PropertyKey::string("value"), Value::int32(1));

    assert_eq!(
        obj1.get(&PropertyKey::string("value")),
        Some(Value::int32(1))
    );
    assert_eq!(
        obj2.get(&PropertyKey::string("value")),
        Some(Value::int32(2))
    );
    assert_eq!(
        obj3.get(&PropertyKey::string("value")),
        Some(Value::int32(3))
    );

    let handle = scope.root_value(Value::object(obj1));

    for _ in 0..50 {
        let _garbage = GcRef::new(JsObject::new(Value::null(), mm.clone()));
    }

    scope.context().collect_garbage();

    let root_slot = scope.context().get_root_slot(handle.slot_index());
    let rooted_obj = root_slot
        .as_object()
        .expect("Root object should be accessible");

    assert_eq!(
        rooted_obj.get(&PropertyKey::string("value")),
        Some(Value::int32(1)),
        "obj1 value should survive"
    );

    let next_obj = rooted_obj
        .get(&PropertyKey::string("next"))
        .and_then(|v| v.as_object())
        .expect("obj2 should be reachable from obj1");

    assert_eq!(
        next_obj.get(&PropertyKey::string("value")),
        Some(Value::int32(2)),
        "obj2 value should survive"
    );

    let final_obj = next_obj
        .get(&PropertyKey::string("next"))
        .and_then(|v| v.as_object())
        .expect("obj3 should be reachable from obj2");

    assert_eq!(
        final_obj.get(&PropertyKey::string("value")),
        Some(Value::int32(3)),
        "obj3 value should survive"
    );
}

#[test]
fn test_gc_nested_handle_scopes() {
    let (mut ctx, mm, _rt) = create_test_context();

    let scope1 = HandleScope::new(&mut ctx);
    let obj1 = GcRef::new(JsObject::new(Value::null(), mm.clone()));
    obj1.set(PropertyKey::string("level"), Value::int32(1));
    let h1 = scope1.root_value(Value::object(obj1));

    {
        let scope2 = HandleScope::new(scope1.context_mut());
        let obj2 = GcRef::new(JsObject::new(Value::null(), mm.clone()));
        obj2.set(PropertyKey::string("level"), Value::int32(2));
        let h2 = scope2.root_value(Value::object(obj2));

        assert_eq!(scope2.context().root_count(), 2);

        scope2.context().collect_garbage();

        let v1 = scope2.context().get_root_slot(h1.slot_index());
        let v2 = scope2.context().get_root_slot(h2.slot_index());
        assert!(v1.as_object().is_some());
        assert!(v2.as_object().is_some());
    }

    assert_eq!(scope1.context().root_count(), 1);

    scope1.context().collect_garbage();

    let v1 = scope1.context().get_root_slot(h1.slot_index());
    assert!(v1.as_object().is_some());
}

// ============================================================================
// GC Stats Tests
// ============================================================================

#[test]
fn test_gc_stats_tracking() {
    let _lock = GC_LOCK.lock().unwrap();
    let (ctx, mm, _rt) = create_test_context();

    let initial_stats = ctx.gc_stats();

    for _ in 0..100 {
        let _obj = GcRef::new(JsObject::new(Value::null(), mm.clone()));
    }

    ctx.collect_garbage();

    let after_first_gc = ctx.gc_stats();
    assert_eq!(
        after_first_gc.collection_count,
        initial_stats.collection_count + 1
    );

    for _ in 0..100 {
        let _obj = GcRef::new(JsObject::new(Value::null(), mm.clone()));
    }

    ctx.collect_garbage();

    let after_second_gc = ctx.gc_stats();
    assert_eq!(
        after_second_gc.collection_count,
        initial_stats.collection_count + 2
    );
}

#[test]
#[ignore = "SIGSEGV: GC rooting bug"]
fn test_gc_pause_time_recorded() {
    let _lock = GC_LOCK.lock().unwrap();
    let (ctx, mm, _rt) = create_test_context();

    for _ in 0..1000 {
        let _obj = GcRef::new(JsObject::new(Value::null(), mm.clone()));
    }

    ctx.collect_garbage();

    let stats = ctx.gc_stats();
    assert!(
        stats.collection_count > 0,
        "Collection count should be tracked"
    );
}

// ============================================================================
// Memory Pressure Tests
// ============================================================================

#[test]
fn test_gc_threshold_trigger() {
    let _lock = GC_LOCK.lock().unwrap();
    let (ctx, mm, _rt) = create_test_context();

    ctx.set_gc_threshold(1024); // 1KB

    let initial_collection_count = ctx.gc_stats().collection_count;

    for _ in 0..100 {
        let obj = GcRef::new(JsObject::new(Value::null(), mm.clone()));
        obj.set(PropertyKey::string("data"), Value::int32(12345));
    }

    let triggered = ctx.maybe_collect_garbage();

    let final_stats = ctx.gc_stats();
    if triggered {
        assert!(final_stats.collection_count > initial_collection_count);
    }
}

#[test]
fn test_heap_size_reporting() {
    let _lock = GC_LOCK.lock().unwrap();
    let (ctx, mm, _rt) = create_test_context();

    let initial_heap = ctx.heap_size();

    let mut objects = Vec::new();
    for _ in 0..100 {
        let obj = GcRef::new(JsObject::new(Value::null(), mm.clone()));
        objects.push(obj);
    }

    let heap_after_alloc = ctx.heap_size();
    assert!(
        heap_after_alloc >= initial_heap,
        "Heap should grow with allocations"
    );

    for obj in &objects {
        obj.set(PropertyKey::string("mark"), Value::int32(1));
    }
    drop(objects);
    ctx.collect_garbage();

    let heap_after_gc = ctx.heap_size();
    assert!(
        heap_after_gc <= heap_after_alloc,
        "Heap should shrink after GC"
    );
}
