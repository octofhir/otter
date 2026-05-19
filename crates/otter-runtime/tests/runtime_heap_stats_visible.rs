//! `Runtime::heap_stats()` reflects script allocations made through
//! the runtime-owned VM heap, and `Runtime::force_gc()` drives the
//! cycle counter without exposing raw heap access to embedders.
//!
//! # See also
//!
//! - GC architecture plan §7 ("Leak diagnosis").
//! - Task 74 — GC stats, heap snapshot, retained-size walker.

use otter_runtime::{Runtime, SourceInput};
use otter_vm::object::OBJECT_BODY_TYPE_TAG;

#[test]
fn runtime_heap_stats_and_force_gc_are_visible() {
    runtime_heap_stats_reflect_host_alloc_after_run_script();
    force_gc_resets_live_count_when_no_roots();
}

fn runtime_heap_stats_reflect_host_alloc_after_run_script() {
    let mut rt = Runtime::builder()
        .max_heap_bytes(64 * 1024 * 1024)
        .build()
        .expect("runtime");

    rt.run_script(
        SourceInput::from_javascript("const o = { answer: 42 }; o.answer;"),
        "<smoke>",
    )
    .expect("script ran");

    let stats = rt.heap_stats();
    assert!(
        stats.live_objects > 0,
        "expected at least one live object after script alloc"
    );
    assert!(stats.live_bytes > 0);
    assert!(
        stats.by_type[OBJECT_BODY_TYPE_TAG as usize].alloc_count_total >= 1,
        "per-type alloc counter not bumped"
    );
}

fn force_gc_resets_live_count_when_no_roots() {
    let mut rt = Runtime::builder().build().expect("runtime");
    rt.run_script(
        SourceInput::from_javascript("for (let i = 0; i < 10; i++) ({ i }); undefined;"),
        "<gc-smoke>",
    )
    .expect("script ran");
    rt.force_gc();
    let first_object = rt.heap_stats().by_type[OBJECT_BODY_TYPE_TAG as usize].live_bytes;

    rt.force_gc();
    let baseline_object = rt.heap_stats().by_type[OBJECT_BODY_TYPE_TAG as usize].live_bytes;
    let baseline_cycles = rt.heap_stats().gc_cycles;

    rt.force_gc();
    let stats = rt.heap_stats();
    assert!(
        baseline_object <= first_object,
        "object row should not grow after a forced GC with no new script allocations"
    );
    assert_eq!(
        stats.by_type[OBJECT_BODY_TYPE_TAG as usize].live_bytes, baseline_object,
        "object row must be stable after a GC with no new roots"
    );
    assert_eq!(stats.gc_cycles, baseline_cycles + 1);
}
