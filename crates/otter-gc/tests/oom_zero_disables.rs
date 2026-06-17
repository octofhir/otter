//! Cap = 0 disables enforcement: the heap satisfies bulk
//! allocations limited only by the cage.
//!
//! Task 73 step 2.
//!
//! # See also
//!
//! - GC architecture plan §1.2 NF3.

use otter_gc::trace::{SlotVisitor, Traceable};
use otter_gc::{GcHeap, init_cage_with_size};

/// 200 KiB payload — > LARGE_OBJECT_THRESHOLD so each Big
/// occupies its own LOS page. 500 instances ≈ 100 MiB total,
/// well above the default informational cap a 0-cap test
/// represents.
struct Big {
    _payload: [u8; 200 * 1024],
}

impl Traceable for Big {
    const TYPE_TAG: u8 = 0x32;
    unsafe fn trace_slots(_this: *mut Self, _v: &mut SlotVisitor<'_>) {}
}

#[test]
fn cap_zero_allows_100mb_of_allocations() {
    // 256 MiB cage so the bulk allocations fit (each integration
    // test is its own binary so this init does not race other
    // tests).
    let _ = init_cage_with_size(256 * 1024 * 1024);
    let mut heap = GcHeap::with_max_heap_bytes(0).expect("heap");
    assert_eq!(heap.max_heap_bytes(), 0);
    let target_bytes: u64 = 100 * 1024 * 1024;
    let mut allocated_bytes: u64 = 0;
    while allocated_bytes < target_bytes {
        heap.alloc(Big {
            _payload: [0; 200 * 1024],
        })
        .expect("alloc must succeed when cap is disabled");
        allocated_bytes += 200 * 1024;
    }
    assert!(allocated_bytes >= target_bytes);
    // tracked_bytes is reserved for cap enforcement and is not
    // updated on the disabled-cap fast path; embedders observe
    // live bytes via stats() instead. Verify spaces saw the load
    // through the regular accounting: currently-live bytes plus the
    // cumulative full-GC reclaim must cover the total ever allocated
    // (the dead `Big`s may be reclaimed by repeated full GCs along
    // the way, so the per-GC `last_full_reclaimed` alone undercounts).
    let stats = heap.stats();
    assert!(stats.allocated_bytes + stats.total_full_reclaimed >= target_bytes as usize);
    // oom_flag is never set when the cap is disabled.
    assert!(!heap.oom_flag().load(std::sync::atomic::Ordering::Relaxed));
}
