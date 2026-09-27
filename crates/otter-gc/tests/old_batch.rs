//! Batch reservation preserves independent cell identity and tracing.
//!
//! # Contents
//! - Independent reclamation, page boundaries, cap refusal and moving children.
//!
//! # Invariants
//! - A reserved range is always a sequence of ordinary, individually live cells.
//! - Pending inputs and published outgoing edges survive moving collection.

use otter_gc::raw::{RawGc, SlotVisitor};
use otter_gc::{Gc, GcHeap, HandleScope, OutOfMemory, SafeTraceable};

#[derive(Clone, Copy)]
struct Leaf(u64);

impl SafeTraceable for Leaf {
    const TYPE_TAG: u8 = 0xf0;
    fn trace_slots_safe(&mut self, _: &mut SlotVisitor<'_>) {}
}

#[derive(Clone, Copy)]
struct Edge(Gc<Leaf>);

impl SafeTraceable for Edge {
    const TYPE_TAG: u8 = 0xf1;
    fn trace_slots_safe(&mut self, visitor: &mut SlotVisitor<'_>) {
        visitor((&raw mut self.0).cast::<RawGc>());
    }
}

#[test]
fn batch_cells_across_pages_are_independently_collectible() {
    let mut heap = GcHeap::new().unwrap();
    let mut cells = vec![Gc::null(); 20_000];
    heap.alloc_old_batch_with_roots(Leaf(73), &mut cells, &mut |_| {})
        .unwrap();
    let unique: std::collections::BTreeSet<_> = cells.iter().map(|cell| cell.offset()).collect();
    assert_eq!(unique.len(), cells.len());
    // SAFETY: the scope is dropped before its heap.
    let scope = unsafe { HandleScope::from_ptr(heap.handle_stack_ptr()) };
    let first = scope.local(cells[0]);
    let last = scope.local(cells[cells.len() - 1]);
    heap.with_payload(last.get(), |leaf| leaf.0 = 91);
    heap.collect_full(&mut |_| {}).unwrap();
    assert_eq!(heap.read_payload(first.get(), |leaf| leaf.0), 73);
    assert_eq!(heap.read_payload(last.get(), |leaf| leaf.0), 91);
    let stats = heap.gc_stats().by_type[0xf0];
    assert_eq!(stats.alloc_count_total, 20_000);
    assert_eq!(stats.alloc_bytes_total, 320_000);
    assert_eq!(stats.live_bytes, 32);
    assert_eq!(stats.free_count_total, 19_998);
}

#[test]
fn pending_batch_value_and_each_published_edge_are_moving_roots() {
    let mut heap = GcHeap::with_max_heap_bytes(4096).unwrap();
    let mut garbage = vec![Gc::null(); 200];
    heap.alloc_old_batch_with_roots(Leaf(0), &mut garbage, &mut |_| {})
        .unwrap();
    let child = heap.alloc(Leaf(73)).unwrap();
    let before = child.offset();
    let mut parents = vec![Gc::null(); 128];
    heap.alloc_old_batch_with_roots(Edge(child), &mut parents, &mut |_| {})
        .unwrap();
    assert!(
        heap.gc_stats().gc_cycles > 0,
        "batch budget must force full GC"
    );
    let moved = heap.read_payload(parents[0], |edge| edge.0);
    assert_ne!(moved.offset(), before);
    // SAFETY: the scope is dropped before its heap.
    let scope = unsafe { HandleScope::from_ptr(heap.handle_stack_ptr()) };
    let parent = scope.local(parents[127]);
    heap.collect_minor(otter_gc::EmptyRoots).unwrap();
    heap.collect_full(&mut |_| {}).unwrap();
    let child = heap.read_payload(parent.get(), |edge| edge.0);
    assert_eq!(heap.read_payload(child, |leaf| leaf.0), 73);
    assert_eq!(heap.gc_stats().by_type[0xf1].live_bytes, 16);
}

#[test]
fn cap_refusal_publishes_no_cell() {
    let mut heap = GcHeap::with_max_heap_bytes(128).unwrap();
    let mut output = [Gc::null(); 9];
    assert!(matches!(
        heap.alloc_old_batch_with_roots(Leaf(1), &mut output, &mut |_| {}),
        Err(OutOfMemory::HeapCapExceeded {
            requested_bytes: 144,
            ..
        })
    ));
    assert!(output.iter().all(|cell| cell.is_null()));
    assert_eq!(heap.gc_stats().by_type[0xf0].alloc_count_total, 0);
}

#[test]
fn batch_allocation_during_marking_starts_black() {
    let mut heap = GcHeap::new().unwrap();
    heap.start_incremental_mark_phase(&mut |_| {}).unwrap();
    let mut output = [Gc::null(); 3];
    heap.alloc_old_batch_with_roots(Leaf(1), &mut output, &mut |_| {})
        .unwrap();
    for cell in output {
        // SAFETY: the batch returned initialized live cells.
        assert_eq!(
            unsafe { (*cell.as_header_ptr()).mark_color() },
            otter_gc::header::MarkColor::Black
        );
    }
}
