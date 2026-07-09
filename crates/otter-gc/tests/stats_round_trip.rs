//! Per-type live count returns to baseline after force GC.
//!
//! Allocates 100 of `Cell` (a leaf type), confirms the counter
//! sees them, then drops the handle scope and forces a full GC
//! to assert the counter returns to zero.
//!
//! # See also
//!
//! - GC architecture plan §7 ("Leak diagnosis").
//! - Task 74 — GC stats, heap snapshot, retained-size walker.

use otter_gc::trace::{SlotVisitor, Traceable};
use otter_gc::{GcHeap, HandleScope};

#[derive(Debug)]
struct Cell {
    _value: u64,
}

impl Traceable for Cell {
    const TYPE_TAG: u8 = 0x40;
    unsafe fn trace_slots(_this: *mut Self, _v: &mut SlotVisitor<'_>) {}
}

#[test]
fn live_count_returns_to_zero_after_full_gc() {
    let mut heap = GcHeap::new().expect("heap");
    heap.register_traceable::<Cell>();

    // After alloc, live_objects accumulates and per-type
    // alloc_count_total bumps in lock step. The handle scope
    // pins them only for the body.
    {
        let scope = unsafe { HandleScope::from_ptr(heap.handle_stack_ptr()) };
        for i in 0..100u64 {
            let g = heap.alloc(Cell { _value: i }).unwrap();
            let _local = scope.local(g);
        }
        let stats = heap.gc_stats();
        assert_eq!(stats.live_objects, 100);
        assert_eq!(
            stats.by_type[Cell::TYPE_TAG as usize].alloc_count_total,
            100
        );
        assert!(stats.by_type[Cell::TYPE_TAG as usize].live_bytes > 0);
    }

    heap.collect_full(&mut |_| {}).expect("full GC");

    let stats = heap.gc_stats();
    assert_eq!(stats.live_objects, 0, "expected baseline after GC");
    assert_eq!(stats.live_bytes, 0);
    assert_eq!(stats.gc_cycles, 1);
    let row = &stats.by_type[Cell::TYPE_TAG as usize];
    assert_eq!(row.live_bytes, 0);
    assert_eq!(row.alloc_count_total, 100);
    assert_eq!(row.free_count_total, 100);
}
