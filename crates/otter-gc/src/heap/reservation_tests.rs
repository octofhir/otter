//! Noncollecting reservation admission over exact live LAB accounting.
//!
//! # Contents
//! - Ordinary admitted reservations preserve the current allocation buffer.
//! - Effective-cap admission refunds only unused LAB bytes before booking.
//! - Subsequent buffers charge anew and a full-cap failure forwards live roots.
//!
//! # Invariants
//! - Reservation/refund itself cannot collect or move any child.
//! - Refusal books no bytes; actual cap pressure uses the production collector.
//! - Alias and parent-edge observations use collector-rewritten handle slots.
//!
//! # See also
//! - `super::GcHeap::reserve_bytes_no_collect` owns admission/publication.

use super::*;
use crate::HandleScope;
use crate::test_support::{OpaqueLeaf, OpaquePair};

#[test]
fn admitted_reservation_keeps_lab_and_true_refusal_books_nothing() {
    let mut heap = GcHeap::with_max_heap_bytes(1024 * 1024).expect("heap");
    heap.set_gc_stress(0, false);
    heap.alloc(OpaqueLeaf { payload: 317 }).expect("fresh LAB");
    let before = (heap.lab.top, heap.lab.limit, heap.lab_charged);
    assert!(before.0 < before.1 && before.2 > 0);
    let tracked = heap.tracked_bytes();
    let cycles = heap.gc_cycle_counts();
    assert!(heap.tracked_bytes + 8 <= heap.max_heap_bytes());
    heap.reserve_bytes_no_collect(8).expect("within raw cap");
    assert_eq!((heap.lab.top, heap.lab.limit, heap.lab_charged), before);
    assert_eq!(heap.tracked_bytes(), tracked + 8);
    assert!(matches!(
        heap.reserve_bytes_no_collect(heap.max_heap_bytes()),
        Err(OutOfMemory::HeapCapExceeded { .. })
    ));
    assert_eq!((heap.lab.top, heap.lab.limit, heap.lab_charged), before);
    assert_eq!(heap.tracked_bytes(), tracked + 8);
    assert_eq!(heap.stats().reserved_bytes, 8);
    assert_eq!(heap.gc_cycle_counts(), cycles);
    heap.release_bytes(8);
    assert_eq!(heap.tracked_bytes(), tracked);
}

#[test]
fn effective_cap_retirement_refund_and_next_buffer_keep_actual_roots_current() {
    let cap = 64 * 1024;
    let mut heap = GcHeap::with_max_heap_bytes(cap).expect("heap");
    heap.set_gc_stress(0, false);
    // SAFETY: the handle stack outlives this scope and all handles below.
    let scope = unsafe { HandleScope::from_ptr(heap.handle_stack_ptr()) };
    let first = scope.local(heap.alloc(OpaqueLeaf { payload: 317 }).unwrap());
    let last = scope.local(heap.alloc(OpaqueLeaf { payload: 719 }).unwrap());
    let alias = scope.local(first.get());
    let owner = scope.local(
        heap.alloc(OpaquePair {
            first: first.get().raw(),
            second: last.get().raw(),
        })
        .unwrap(),
    );
    let before = [
        first.get().offset(),
        last.get().offset(),
        owner.get().offset(),
    ];
    assert_ne!(before[0], before[1]);
    let cycles = heap.gc_cycle_counts();
    let reserve = cap - heap.tracked_bytes();
    assert!(heap.tracked_bytes.saturating_add(reserve) > cap);
    heap.reserve_bytes_no_collect(reserve)
        .expect("unused charged tail must not reject effective-cap admission");
    assert_eq!(heap.lab.top, 0);
    assert_eq!(heap.lab.limit, 0);
    assert_eq!(heap.lab_charged, 0);
    assert_eq!(heap.tracked_bytes, cap);
    assert_eq!(heap.tracked_bytes(), cap);
    let filled = heap.stats();
    assert_eq!(filled.allocated_bytes as u64 + filled.reserved_bytes, cap);
    assert_eq!(heap.gc_cycle_counts(), cycles);
    assert_eq!(first.get().offset(), before[0]);
    assert_eq!(last.get().offset(), before[1]);
    assert!(
        heap.try_alloc_no_collect(OpaqueLeaf { payload: 997 })
            .is_none()
    );
    assert_eq!(heap.tracked_bytes(), cap);
    assert_eq!(heap.stats().reserved_bytes, reserve);
    let one_cell = align_up(
        std::mem::size_of::<GcHeader>() + std::mem::size_of::<OpaqueLeaf>(),
        CELL_SIZE,
    ) as u64;
    heap.release_bytes(one_cell);
    assert_eq!(heap.tracked_bytes(), cap - one_cell);
    let next = scope.local(
        heap.try_alloc_no_collect(OpaqueLeaf { payload: 997 })
            .expect("next LAB charges the refunded physical extent"),
    );
    assert_eq!(heap.tracked_bytes(), cap);
    assert_eq!(heap.lab_charged, one_cell);
    assert_eq!(heap.lab.remaining(), 0);
    assert_eq!(heap.stats().reserved_bytes, reserve - one_cell);
    assert_eq!(heap.gc_cycle_counts(), cycles);
    let error = heap
        .alloc(OpaqueLeaf { payload: 1009 })
        .expect_err("live cells and booked reservation exhaust the cap");
    assert!(matches!(error, OutOfMemory::HeapCapExceeded { .. }));
    assert_eq!(error.heap_limit_bytes(), cap);
    assert!(heap.gc_cycle_counts().1 > cycles.1);
    for (current, old) in [
        first.get().offset(),
        last.get().offset(),
        owner.get().offset(),
    ]
    .into_iter()
    .zip(before)
    {
        assert_ne!(
            current, old,
            "actual cap collection moved each fresh rooted cell"
        );
    }
    assert_eq!(first.get(), alias.get());
    heap.read_payload(owner.get(), |pair| {
        assert_eq!(pair.first, first.get().raw());
        assert_eq!(pair.second, last.get().raw());
    });
    heap.read_payload(first.get(), |leaf| assert_eq!(leaf.payload, 317));
    heap.read_payload(last.get(), |leaf| assert_eq!(leaf.payload, 719));
    heap.read_payload(next.get(), |leaf| assert_eq!(leaf.payload, 997));
    assert_eq!(heap.stats().reserved_bytes, reserve - one_cell);
    assert_eq!(heap.tracked_bytes(), cap);
    heap.release_bytes(reserve - one_cell);
    assert_eq!(heap.stats().reserved_bytes, 0);
    assert_eq!(heap.tracked_bytes(), heap.stats().allocated_bytes as u64);
}
