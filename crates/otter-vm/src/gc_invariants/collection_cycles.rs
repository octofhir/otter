//! Map/Set GC invariants.
//!
//! `Map` and `Set` are GC-managed handles. Their entries can point
//! back to the collection itself, so mark-sweep must reclaim the
//! cycle once no runtime root retains it.

use crate::Interpreter;
use crate::Value;
use crate::collections::{MAP_BODY_TYPE_TAG, SET_BODY_TYPE_TAG};

fn assert_map_self_value_reaped() {
    let mut interp = Interpreter::new();

    interp.force_gc();
    let baseline = interp.gc_heap_mut().gc_stats().by_type[MAP_BODY_TYPE_TAG as usize].live_bytes;

    let map = crate::collections::alloc_map(interp.gc_heap_mut()).expect("alloc map");
    crate::collections::map_set(
        map,
        interp.gc_heap_mut(),
        Value::number_i32(1),
        Value::map(map),
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

fn assert_set_self_value_reaped() {
    let mut interp = Interpreter::new();

    interp.force_gc();
    let baseline = interp.gc_heap_mut().gc_stats().by_type[SET_BODY_TYPE_TAG as usize].live_bytes;

    let set = crate::collections::alloc_set(interp.gc_heap_mut()).expect("alloc set");
    crate::collections::set_add(set, interp.gc_heap_mut(), Value::set(set))
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

#[test]
fn map_set_self_value_reaped() {
    assert_map_self_value_reaped();
    assert_set_self_value_reaped();
}
