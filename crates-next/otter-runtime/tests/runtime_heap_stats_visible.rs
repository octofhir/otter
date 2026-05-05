//! `Runtime::heap_stats()` reflects host-side allocations made
//! through the runtime's GC heap, and `Runtime::force_gc()`
//! drives the cycle counter.
//!
//! Phase-1 interpreter still routes its values through the
//! `Rc`-based VM, so the JS body does not allocate against the
//! GC heap directly. Per-type GC migrations (tasks 76+) widen
//! the surface populated by script execution; this test pins
//! the API shape today using
//! [`otter_gc::test_support::OpaqueLeaf`] for the host-side
//! allocations.
//!
//! # See also
//!
//! - GC architecture plan §7 ("Leak diagnosis").
//! - Task 74 — GC stats, heap snapshot, retained-size walker.

use otter_gc::Traceable;
use otter_gc::test_support::OpaqueLeaf;
use otter_runtime::{Runtime, SourceInput};

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

    rt.run_script(SourceInput::from_javascript("undefined;"), "<smoke>")
        .expect("script ran");

    rt.gc_heap_mut().register_traceable::<OpaqueLeaf>();
    let _g = rt
        .gc_heap_mut()
        .alloc(OpaqueLeaf { payload: 0xABCD })
        .expect("alloc");

    let stats = rt.heap_stats();
    assert!(
        stats.live_objects > 0,
        "expected at least one live object after host alloc"
    );
    assert!(stats.live_bytes > 0);
    assert!(
        stats.by_type[OpaqueLeaf::TYPE_TAG as usize].alloc_count_total >= 1,
        "per-type alloc counter not bumped"
    );
}

fn force_gc_resets_live_count_when_no_roots() {
    let mut rt = Runtime::builder().build().expect("runtime");
    rt.gc_heap_mut().register_traceable::<OpaqueLeaf>();
    // Establish a baseline AFTER intrinsics + globalThis are
    // wired so we measure only the OpaqueLeaf row, which is
    // unaffected by the post-task-77 root walker.
    rt.force_gc();
    let baseline_leaf = rt.heap_stats().by_type[OpaqueLeaf::TYPE_TAG as usize].live_bytes;
    let baseline_cycles = rt.heap_stats().gc_cycles;
    for i in 0..10u64 {
        let _ = rt
            .gc_heap_mut()
            .alloc(OpaqueLeaf { payload: i })
            .expect("alloc");
    }
    rt.force_gc();
    let stats = rt.heap_stats();
    assert_eq!(
        stats.by_type[OpaqueLeaf::TYPE_TAG as usize].live_bytes,
        baseline_leaf,
        "unrooted OpaqueLeaf row must return to baseline"
    );
    assert_eq!(stats.gc_cycles, baseline_cycles + 1);
}
