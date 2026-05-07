//! Weak collection GC regressions for task 80.
//!
//! `WeakMap` / `WeakSet` entries are ephemerons: the collection
//! table can stay live without keeping its keys alive. `WeakMap`
//! values become strong only after their key is marked through
//! another path.

use otter_gc::raw::RawGc;
use otter_vm::Value;
use otter_vm::collections::{
    alloc_weak_map, alloc_weak_set, run_ephemeron_fixpoint, weak_map_get, weak_map_has,
    weak_map_set, weak_set_add, weak_set_has,
};
use otter_vm::object::OBJECT_BODY_TYPE_TAG;

fn collect_with_roots(heap: &mut otter_gc::GcHeap, roots: &mut [&mut RawGc]) {
    let mut visit = |sv: &mut dyn FnMut(*mut RawGc)| {
        for root in roots.iter_mut() {
            sv(*root as *mut RawGc);
        }
    };
    heap.mark_phase(&mut visit);
    run_ephemeron_fixpoint(heap);
    heap.sweep_phase();
}

#[test]
fn weakmap_dead_key_entry_is_pruned_and_value_reaped() {
    let mut heap = otter_gc::GcHeap::new().expect("gc heap");
    let wm = alloc_weak_map(&mut heap).expect("weak map");
    let key = otter_vm::object::alloc_object(&mut heap).expect("key");
    let value = otter_vm::object::alloc_object(&mut heap).expect("value");

    weak_map_set(wm, &mut heap, Value::Object(key), Value::Object(value)).expect("weak map set");
    let mut wm_root = wm.raw();
    collect_with_roots(&mut heap, &mut [&mut wm_root]);

    assert_eq!(
        weak_map_get(wm, &heap, &Value::Object(key)).expect("weak map get"),
        None
    );
    assert_eq!(
        heap.gc_stats().by_type[OBJECT_BODY_TYPE_TAG as usize].live_bytes,
        0,
        "dead weak-map key and value must not be retained"
    );
}

#[test]
fn weakmap_live_key_marks_value_through_fixpoint() {
    let mut heap = otter_gc::GcHeap::new().expect("gc heap");
    let wm = alloc_weak_map(&mut heap).expect("weak map");
    let key = otter_vm::object::alloc_object(&mut heap).expect("key");
    let value = otter_vm::object::alloc_object(&mut heap).expect("value");

    weak_map_set(wm, &mut heap, Value::Object(key), Value::Object(value)).expect("weak map set");
    let mut wm_root = wm.raw();
    let mut key_root = key.raw();
    collect_with_roots(&mut heap, &mut [&mut wm_root, &mut key_root]);

    assert!(weak_map_has(wm, &heap, &Value::Object(key)).expect("weak map has"));
    assert!(
        heap.gc_stats().by_type[OBJECT_BODY_TYPE_TAG as usize].live_bytes > 0,
        "live weak-map key must mark its value"
    );
}

#[test]
fn weakmap_ephemeron_chain_reaches_fixpoint() {
    let mut heap = otter_gc::GcHeap::new().expect("gc heap");
    let wm = alloc_weak_map(&mut heap).expect("weak map");
    let k1 = otter_vm::object::alloc_object(&mut heap).expect("k1");
    let k2 = otter_vm::object::alloc_object(&mut heap).expect("k2");
    let value = otter_vm::object::alloc_object(&mut heap).expect("value");

    weak_map_set(wm, &mut heap, Value::Object(k1), Value::Object(k2)).expect("weak map k1");
    weak_map_set(wm, &mut heap, Value::Object(k2), Value::Object(value)).expect("weak map k2");
    let mut wm_root = wm.raw();
    let mut k1_root = k1.raw();
    collect_with_roots(&mut heap, &mut [&mut wm_root, &mut k1_root]);

    assert!(weak_map_has(wm, &heap, &Value::Object(k2)).expect("weak map has k2"));
    assert!(
        heap.gc_stats().by_type[OBJECT_BODY_TYPE_TAG as usize].live_bytes > 0,
        "ephemeron chain must mark through k1 -> k2 -> value"
    );
}

#[test]
fn weakmap_dead_ephemeron_chain_is_reaped() {
    let mut heap = otter_gc::GcHeap::new().expect("gc heap");
    let wm = alloc_weak_map(&mut heap).expect("weak map");
    let k1 = otter_vm::object::alloc_object(&mut heap).expect("k1");
    let k2 = otter_vm::object::alloc_object(&mut heap).expect("k2");
    let value = otter_vm::object::alloc_object(&mut heap).expect("value");

    weak_map_set(wm, &mut heap, Value::Object(k1), Value::Object(k2)).expect("weak map k1");
    weak_map_set(wm, &mut heap, Value::Object(k2), Value::Object(value)).expect("weak map k2");
    let mut wm_root = wm.raw();
    collect_with_roots(&mut heap, &mut [&mut wm_root]);

    assert!(!weak_map_has(wm, &heap, &Value::Object(k1)).expect("weak map has k1"));
    assert!(!weak_map_has(wm, &heap, &Value::Object(k2)).expect("weak map has k2"));
    assert_eq!(
        heap.gc_stats().by_type[OBJECT_BODY_TYPE_TAG as usize].live_bytes,
        0,
        "dead ephemeron chain must not retain k2 or value"
    );
}

#[test]
fn weakmap_self_reference_does_not_keep_key_alive() {
    let mut heap = otter_gc::GcHeap::new().expect("gc heap");
    let wm = alloc_weak_map(&mut heap).expect("weak map");
    let obj = otter_vm::object::alloc_object(&mut heap).expect("obj");

    weak_map_set(wm, &mut heap, Value::Object(obj), Value::Object(obj)).expect("weak map set");
    let mut wm_root = wm.raw();
    collect_with_roots(&mut heap, &mut [&mut wm_root]);

    assert!(!weak_map_has(wm, &heap, &Value::Object(obj)).expect("weak map has"));
    assert_eq!(
        heap.gc_stats().by_type[OBJECT_BODY_TYPE_TAG as usize].live_bytes,
        0,
        "WeakMap[obj] = obj must not create a retaining cycle"
    );
}

#[test]
fn weakmap_replacement_and_delete_do_not_leave_stale_ephemerons() {
    let mut heap = otter_gc::GcHeap::new().expect("gc heap");
    let wm = alloc_weak_map(&mut heap).expect("weak map");
    let key = otter_vm::object::alloc_object(&mut heap).expect("key");
    let old_value = otter_vm::object::alloc_object(&mut heap).expect("old value");
    let new_value = otter_vm::object::alloc_object(&mut heap).expect("new value");

    weak_map_set(wm, &mut heap, Value::Object(key), Value::Object(old_value))
        .expect("weak map old");
    weak_map_set(wm, &mut heap, Value::Object(key), Value::Object(new_value))
        .expect("weak map replacement");
    assert!(
        otter_vm::collections::weak_map_delete(wm, &mut heap, &Value::Object(key))
            .expect("weak map delete")
    );
    let mut wm_root = wm.raw();
    collect_with_roots(&mut heap, &mut [&mut wm_root]);

    assert_eq!(
        heap.gc_stats().by_type[OBJECT_BODY_TYPE_TAG as usize].live_bytes,
        0,
        "replacement and delete must not retain stale ephemeron values"
    );
}

#[test]
fn dropped_weakmap_prunes_registry_and_does_not_retain_values() {
    let mut heap = otter_gc::GcHeap::new().expect("gc heap");
    let key = otter_vm::object::alloc_object(&mut heap).expect("key");
    let key_bytes = {
        let mut key_raw = key.raw();
        collect_with_roots(&mut heap, &mut [&mut key_raw]);
        heap.gc_stats().by_type[OBJECT_BODY_TYPE_TAG as usize].live_bytes
    };
    let wm = alloc_weak_map(&mut heap).expect("weak map");
    let value = otter_vm::object::alloc_object(&mut heap).expect("value");

    weak_map_set(wm, &mut heap, Value::Object(key), Value::Object(value)).expect("weak map set");
    assert_eq!(heap.ephemeron_table_count(), 1);

    let mut key_root = key.raw();
    collect_with_roots(&mut heap, &mut [&mut key_root]);

    assert_eq!(
        heap.ephemeron_table_count(),
        0,
        "dead weak-map table must be pruned from the collector registry"
    );
    assert_eq!(
        heap.gc_stats().by_type[OBJECT_BODY_TYPE_TAG as usize].live_bytes,
        key_bytes,
        "dead weak-map registry metadata must not retain the value"
    );
}

#[test]
fn allocation_during_mark_phase_registers_ephemerons_after_active_snapshot() {
    let mut heap = otter_gc::GcHeap::new().expect("gc heap");
    let existing = alloc_weak_map(&mut heap).expect("existing weak map");
    let mut existing_root = existing.raw();
    let mut visit = |sv: &mut dyn FnMut(*mut RawGc)| {
        sv(&mut existing_root as *mut RawGc);
    };

    heap.mark_phase(&mut visit);
    let snapshot_before = heap.ephemeron_tables_snapshot();
    let late = alloc_weak_map(&mut heap).expect("late weak map");
    let key = otter_vm::object::alloc_object(&mut heap).expect("late key");
    let value = otter_vm::object::alloc_object(&mut heap).expect("late value");
    weak_map_set(late, &mut heap, Value::Object(key), Value::Object(value))
        .expect("late weak map set");

    assert!(
        !snapshot_before.contains(&late.raw()),
        "allocation during a mark phase must not mutate an already-taken ephemeron snapshot"
    );
    assert!(
        heap.is_marked(late.raw()),
        "allocation during an active mark phase must use black allocation"
    );

    run_ephemeron_fixpoint(&mut heap);
    heap.sweep_phase();

    assert_eq!(
        heap.ephemeron_table_count(),
        2,
        "late weak-map table should be appended to registry after the active snapshot"
    );
}

#[test]
fn weakset_dead_key_entry_is_pruned() {
    let mut heap = otter_gc::GcHeap::new().expect("gc heap");
    let ws = alloc_weak_set(&mut heap).expect("weak set");
    let key = otter_vm::object::alloc_object(&mut heap).expect("key");

    weak_set_add(ws, &mut heap, Value::Object(key)).expect("weak set add");
    let mut ws_root = ws.raw();
    collect_with_roots(&mut heap, &mut [&mut ws_root]);

    assert!(!weak_set_has(ws, &heap, &Value::Object(key)).expect("weak set has"));
    assert_eq!(
        heap.gc_stats().by_type[OBJECT_BODY_TYPE_TAG as usize].live_bytes,
        0,
        "dead weak-set key must not be retained"
    );
}
