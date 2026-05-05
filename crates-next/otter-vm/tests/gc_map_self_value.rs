//! Map/Set GC regressions for task 79.
//!
//! `Map` and `Set` are GC-managed handles. Their entries can point
//! back to the collection itself, so mark-sweep must reclaim the
//! cycle once no runtime root retains it.

use otter_vm::Interpreter;
use otter_vm::Value;
use otter_vm::collections::{MAP_BODY_TYPE_TAG, SET_BODY_TYPE_TAG};

#[test]
fn map_self_value_reaped() {
    let mut interp = Interpreter::new();

    interp.force_gc();
    let baseline = interp.gc_heap_mut().gc_stats().by_type[MAP_BODY_TYPE_TAG as usize].live_bytes;

    let map = otter_vm::collections::alloc_map(interp.gc_heap_mut()).expect("alloc map");
    otter_vm::collections::map_set(
        map,
        interp.gc_heap_mut(),
        Value::Number(otter_vm::NumberValue::Smi(1)),
        Value::Map(map),
    )
    .expect("map self value");

    let with_map = interp.gc_heap_mut().gc_stats().by_type[MAP_BODY_TYPE_TAG as usize].live_bytes;
    assert!(
        with_map > baseline,
        "map allocation must bump live_bytes (baseline={baseline}, with_map={with_map})"
    );

    let _ = map;
    interp.force_gc();
    let after = interp.gc_heap_mut().gc_stats().by_type[MAP_BODY_TYPE_TAG as usize].live_bytes;
    assert_eq!(
        after, baseline,
        "self-referential map must be reaped by force_gc (baseline={baseline}, after={after})"
    );
}

#[test]
fn set_self_value_reaped() {
    let mut interp = Interpreter::new();

    interp.force_gc();
    let baseline = interp.gc_heap_mut().gc_stats().by_type[SET_BODY_TYPE_TAG as usize].live_bytes;

    let set = otter_vm::collections::alloc_set(interp.gc_heap_mut()).expect("alloc set");
    otter_vm::collections::set_add(set, interp.gc_heap_mut(), Value::Set(set))
        .expect("set self value");

    let with_set = interp.gc_heap_mut().gc_stats().by_type[SET_BODY_TYPE_TAG as usize].live_bytes;
    assert!(
        with_set > baseline,
        "set allocation must bump live_bytes (baseline={baseline}, with_set={with_set})"
    );

    let _ = set;
    interp.force_gc();
    let after = interp.gc_heap_mut().gc_stats().by_type[SET_BODY_TYPE_TAG as usize].live_bytes;
    assert_eq!(
        after, baseline,
        "self-referential set must be reaped by force_gc (baseline={baseline}, after={after})"
    );
}
