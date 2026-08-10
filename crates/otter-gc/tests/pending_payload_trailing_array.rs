//! A body's trailing array belongs to its heap cell, not to the body.
//!
//! `alloc_variable_with_roots` copies a stack-resident payload into a fresh
//! cell. If the heap cap forces a collection first, that payload has to be
//! traced while it is still on the stack — and at that moment its trailing
//! array does not exist. Tracing it the ordinary way walks `count` words past
//! the payload, straight into the caller's frame, and hands whatever is there
//! to the collector as root slots.
//!
//! `trace_pending_slots` is the answer: a body with a trailing array traces
//! nothing in its pending form. These tests hold that contract.

use otter_gc::raw::RawGc;
use otter_gc::test_support::OpaqueVector;
use otter_gc::trace::{SlotVisitor, Traceable};
use otter_gc::{GcHeap, OutOfMemory};

/// Count the slots a trace yields.
fn slots_from(trace: impl FnOnce(&mut SlotVisitor<'_>)) -> usize {
    let mut seen = 0usize;
    let mut visitor = |_slot: *mut RawGc| seen += 1;
    trace(&mut visitor);
    seen
}

#[test]
fn a_pending_body_with_a_trailing_array_traces_nothing() {
    // The count says a thousand elements; the stack holds only the count.
    let mut pending = OpaqueVector::new(1024);
    let visited = slots_from(|visitor| {
        // SAFETY: `pending` is a fully-constructed `OpaqueVector`; the
        // pending trace is defined not to read past `size_of::<Self>()`.
        unsafe { OpaqueVector::trace_pending_slots(&raw mut pending, visitor) };
    });
    assert_eq!(visited, 0, "the trailing array is not on the stack");
}

#[test]
fn the_same_body_traces_its_whole_array_once_it_is_in_the_heap() {
    let mut heap = GcHeap::new().expect("heap");
    let mut nothing = |_visitor: &mut dyn FnMut(*mut RawGc)| {};
    let handle = heap
        .alloc_variable_with_roots(
            OpaqueVector::new(8),
            OpaqueVector::trailing_bytes(8),
            &mut nothing,
        )
        .expect("allocation");
    let visited = heap.with_payload(handle, |body| {
        slots_from(|visitor| {
            // SAFETY: the body is in the heap, with its trailing array.
            unsafe { OpaqueVector::trace_slots(std::ptr::from_mut(body), visitor) };
        })
    });
    assert_eq!(visited, 8, "an in-heap body still traces every element");
}

#[test]
fn a_cap_triggered_collection_during_a_variable_allocation_is_survivable() {
    // A cap small enough that the allocation below overruns it, so the
    // allocation collects with the pending payload as a root — the exact
    // shape that used to walk the stack.
    let mut heap = GcHeap::with_max_heap_bytes(8 * 1024 * 1024).expect("heap");
    let mut nothing = |_visitor: &mut dyn FnMut(*mut RawGc)| {};
    let mut allocations = 0usize;
    // Elements enough that a stack walk of that many words would leave the
    // frame; repeated until the cap forces at least one collection.
    for _ in 0..64 {
        let elements = 4096;
        match heap.alloc_variable_with_roots(
            OpaqueVector::new(elements),
            OpaqueVector::trailing_bytes(elements),
            &mut nothing,
        ) {
            Ok(_) => allocations += 1,
            Err(OutOfMemory::HeapCapExceeded { .. }) => break,
            Err(other) => panic!("unexpected allocation failure: {other:?}"),
        }
    }
    assert!(allocations > 0, "at least one allocation should succeed");
}
