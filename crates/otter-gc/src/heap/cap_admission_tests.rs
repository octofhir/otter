//! Exact cap admission for alternating old and young cells.
//!
//! # Contents
//! - Raw-admitted old allocations keep a live nursery LAB.
//! - Exact mixed-generation budgets refund only unused charged LAB tails.
//! - True exhaustion collects and forwards young children through old parents.
//!
//! # Invariants
//! - Admission never retires an ordinarily admitted buffer or collects on an
//!   effective-cap hit. Refusal retains its actual typed allocator cause.
//! - Distinct children, aliases and parent edges use collector-traced handles.
//! - Exact physical cell footprints account for every admitted byte.
//!
//! # See also
//! - `super::GcHeap::account_or_collect_with_roots` owns cap admission.

use super::*;
use crate::HandleScope;
use crate::test_support::{OpaqueLeaf, OpaquePair};

fn cell_bytes<T>() -> u64 {
    align_up(
        std::mem::size_of::<GcHeader>() + std::mem::size_of::<T>(),
        CELL_SIZE,
    ) as u64
}

#[test]
fn raw_admitted_old_cell_preserves_live_lab_and_young_aliases() {
    let mut heap = GcHeap::with_max_heap_bytes(1024 * 1024).expect("heap");
    heap.set_gc_stress(0, false);
    // SAFETY: the heap-owned handle stack outlives every handle below.
    let scope = unsafe { HandleScope::from_ptr(heap.handle_stack_ptr()) };
    let child = scope.local(heap.alloc(OpaqueLeaf { payload: 317 }).unwrap());
    let alias = scope.local(child.get());
    let before = (heap.lab.top, heap.lab.limit, heap.lab_charged);
    let raw = heap.tracked_bytes;
    let cycles = heap.gc_cycle_counts();
    assert!(before.0 < before.1 && before.2 > 0);
    assert!(raw + cell_bytes::<OpaquePair>() <= heap.max_heap_bytes());
    let parent = scope.local(
        heap.alloc_old(OpaquePair {
            first: child.get().raw(),
            second: child.get().raw(),
        })
        .expect("ordinary raw-admitted old allocation"),
    );
    assert_eq!((heap.lab.top, heap.lab.limit, heap.lab_charged), before);
    assert_eq!(heap.tracked_bytes, raw + cell_bytes::<OpaquePair>());
    assert_eq!(heap.gc_cycle_counts(), cycles);
    assert_eq!(child.get(), alias.get());
    heap.read_payload(parent.get(), |body| {
        assert_eq!(body.first, child.get().raw());
        assert_eq!(body.second, child.get().raw());
    });
    heap.read_payload(child.get(), |body| assert_eq!(body.payload, 317));
}

#[test]
fn exact_alternating_old_young_budget_defers_collection_until_true_exhaustion() {
    let cap = 64 * 1024;
    let mut heap = GcHeap::with_max_heap_bytes(cap).expect("heap");
    heap.set_gc_stress(0, false);
    // SAFETY: the heap-owned handle stack outlives every handle below.
    let scope = unsafe { HandleScope::from_ptr(heap.handle_stack_ptr()) };
    let input_bytes = 2 * (cell_bytes::<OpaquePair>() + cell_bytes::<OpaqueLeaf>());
    let reserved = cap - input_bytes - 1;
    heap.reserve_bytes_no_collect(reserved)
        .expect("exact physical input budget");
    let admitted = heap.stats();
    let cycles = heap.gc_cycle_counts();
    let parent = scope.local(
        heap.alloc_old(OpaquePair {
            first: RawGc::NULL,
            second: RawGc::NULL,
        })
        .unwrap(),
    );
    let first = scope.local(heap.alloc(OpaqueLeaf { payload: 317 }).unwrap());
    let alias = scope.local(first.get());
    let first_offset = first.get().offset();
    let before_old = (heap.lab.top, heap.lab.limit, heap.lab_charged);
    let old_bytes = cell_bytes::<OpaquePair>();
    assert!(before_old.0 < before_old.1 && before_old.2 > 0);
    assert!(heap.tracked_bytes.saturating_add(old_bytes) > cap);
    assert!(heap.effective_tracked_bytes().saturating_add(old_bytes) <= cap);
    let second_parent = scope.local(
        heap.alloc_old(OpaquePair {
            first: first.get().raw(),
            second: first.get().raw(),
        })
        .expect("unused nursery tail cannot deny the old cell"),
    );
    assert_eq!((heap.lab.top, heap.lab.limit, heap.lab_charged), (0, 0, 0));
    assert_eq!(heap.gc_cycle_counts(), cycles);
    assert_eq!(first.get().offset(), first_offset);
    let last = scope.local(heap.alloc(OpaqueLeaf { payload: 719 }).unwrap());
    let last_alias = scope.local(last.get());
    assert_ne!(first.get().offset(), last.get().offset());
    heap.with_payload(parent.get(), |body| {
        body.first = first.get().raw();
        body.second = last.get().raw();
    });
    heap.record_write(parent.get(), &first.get());
    heap.record_write(parent.get(), &last.get());
    heap.with_payload(second_parent.get(), |body| body.second = last.get().raw());
    heap.record_write(second_parent.get(), &last.get());
    assert_eq!(
        heap.gc_cycle_counts(),
        cycles,
        "every fresh input stays young"
    );
    assert_eq!(
        heap.stats().allocated_bytes - admitted.allocated_bytes,
        input_bytes as usize
    );
    assert_eq!(heap.tracked_bytes(), cap - 1);
    assert_eq!(heap.stats().reserved_bytes, reserved);
    let before = [first.get().offset(), last.get().offset()];
    let error = heap
        .alloc_old(OpaqueLeaf { payload: 997 })
        .expect_err("one spare byte cannot fund another physical cell");
    assert!(matches!(error, OutOfMemory::HeapCapExceeded { .. }));
    assert_eq!(error.requested_bytes(), cell_bytes::<OpaqueLeaf>());
    assert_eq!(error.heap_limit_bytes(), cap);
    assert!(heap.gc_cycle_counts().1 > cycles.1);
    for (current, previous) in [first.get().offset(), last.get().offset()]
        .into_iter()
        .zip(before)
    {
        assert_ne!(
            current, previous,
            "true cap collection moves each distinct young child"
        );
    }
    assert_eq!(first.get(), alias.get());
    assert_eq!(last.get(), last_alias.get());
    for parent in [parent, second_parent] {
        heap.read_payload(parent.get(), |body| {
            assert_eq!(body.first, first.get().raw());
            assert_eq!(body.second, last.get().raw());
        });
    }
    heap.read_payload(first.get(), |body| assert_eq!(body.payload, 317));
    heap.read_payload(last.get(), |body| assert_eq!(body.payload, 719));
    assert_eq!(heap.stats().reserved_bytes, reserved);
    assert_eq!(heap.tracked_bytes(), cap - 1);
    heap.release_bytes(reserved);
    assert_eq!(heap.stats().reserved_bytes, 0);
    assert_eq!(heap.tracked_bytes(), heap.stats().allocated_bytes as u64);
}
