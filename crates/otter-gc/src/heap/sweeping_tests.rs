//! A throughput collection's sweep behind its pause.
//!
//! # Contents
//! - Dead cells' drops run, kept cells survive and cells allocated while the
//!   helpers sweep stay live through the cycle's end and the next collection.
//!
//! # Invariants
//! - Every retained cell is reachable from one of the heap's embedder slots.

use std::sync::Arc;

use crate::compressed::RawGc;
use crate::observation::GcPauseTrigger;
use crate::trace::{SlotVisitor, Traceable};
use crate::{FullGcGoal, GcHeap};

struct Cell {
    next: RawGc,
    marker: u64,
    _witness: Option<Arc<()>>,
}

impl Traceable for Cell {
    const TYPE_TAG: u8 = 0xed;

    unsafe fn trace_slots(this: *mut Self, visitor: &mut SlotVisitor<'_>) {
        // SAFETY: the collector supplies an initialized Cell body.
        unsafe { visitor(std::ptr::addr_of_mut!((*this).next)) };
    }
}

fn push(heap: &mut GcHeap, root: usize, marker: u64) {
    let cell = heap
        .alloc_old(Cell {
            next: heap.embedder_root(root),
            marker,
            _witness: None,
        })
        .expect("old cell");
    heap.set_embedder_root(root, cell.raw());
}

/// The markers of the chain at `root`, newest first.
fn chain(heap: &GcHeap, root: usize) -> Vec<u64> {
    let mut markers = Vec::new();
    let mut raw = heap.embedder_root(root);
    while !raw.is_null() {
        // SAFETY: every chain link is a Cell handle.
        let cell = unsafe { raw.cast::<Cell>() };
        raw = heap.read_payload(cell, |body| {
            markers.push(body.marker);
            body.next
        });
    }
    markers
}

#[test]
fn a_sweep_behind_its_pause_keeps_kept_and_new_cells_and_drops_the_dead() {
    let mut heap = GcHeap::new().expect("heap");
    heap.set_gc_stress(0, false);
    let witness = Arc::new(());
    const KEPT: u64 = 40_000;
    for marker in 0..KEPT {
        push(&mut heap, 0, marker);
        heap.alloc_old(Cell {
            next: RawGc::NULL,
            marker,
            _witness: Some(Arc::clone(&witness)),
        })
        .expect("dead cell");
    }
    assert_eq!(Arc::strong_count(&witness), KEPT as usize + 1);
    heap.collect_full_observed(
        &mut |_| {},
        GcPauseTrigger::Explicit,
        FullGcGoal::Throughput,
    )
    .expect("full collection");
    let behind = heap.is_sweeping();
    assert!(
        behind || std::thread::available_parallelism().map_or(1, |n| n.get()) == 1,
        "a throughput collection over many pages sweeps behind its pause"
    );
    // Old allocation while the helpers sweep reaches only fresh pages or
    // pages whose walk was applied.
    const FRESH: u64 = 10_000;
    for marker in 0..FRESH {
        push(&mut heap, 1, KEPT + marker);
    }
    heap.finish_sweeping();
    assert!(!heap.is_sweeping());
    assert_eq!(
        Arc::strong_count(&witness),
        1,
        "every dead cell dropped once"
    );
    let expected_kept: Vec<u64> = (0..KEPT).rev().collect();
    let expected_fresh: Vec<u64> = (KEPT..KEPT + FRESH).rev().collect();
    assert_eq!(chain(&heap, 0), expected_kept);
    assert_eq!(chain(&heap, 1), expected_fresh);
    // The next collection finds both chains whole.
    heap.collect_full(&mut |_| {}).expect("second collection");
    assert_eq!(chain(&heap, 0), expected_kept);
    assert_eq!(chain(&heap, 1), expected_fresh);
    assert!(heap.gc_stats().gc_cycles >= 2);
}
