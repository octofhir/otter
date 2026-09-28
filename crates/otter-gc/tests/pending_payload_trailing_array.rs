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

#[test]
fn young_trailing_allocation_uses_pending_trace_before_publication() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    static PENDING: AtomicUsize = AtomicUsize::new(0);
    struct Tail {
        published: bool,
    }
    impl otter_gc::SafeTraceable for Tail {
        const TYPE_TAG: u8 = 0xF8;
        fn trace_slots_safe(&mut self, _visitor: &mut SlotVisitor<'_>) {
            assert!(
                self.published,
                "heap tracer received an unpublished stack body"
            );
        }
        fn trace_pending_slots_safe(&mut self, _visitor: &mut SlotVisitor<'_>) {
            assert!(!self.published);
            PENDING.fetch_add(1, Ordering::Relaxed);
        }
    }
    let mut heap = GcHeap::with_max_heap_bytes(4096).expect("heap");
    for _ in 0..64 {
        let handle = heap
            .alloc_trailing_with_roots(Tail { published: false }, 1024, &mut |_| {})
            .expect("young trailing allocation");
        heap.with_payload(handle, |body| body.published = true);
    }
    assert!(heap.gc_stats().gc_cycles > 0);
    assert!(PENDING.load(Ordering::Relaxed) > 0);
}

/// Allocate an [`OpaqueVector`] of `len` elements with every element set to
/// `leaf` by the allocation's initializer. `leaf` rides through the
/// allocation as an external root, the way a VM kernel roots the values it
/// copies into a new context.
fn initialized_vector(
    heap: &mut GcHeap,
    len: usize,
    leaf: &mut RawGc,
) -> otter_gc::Gc<OpaqueVector> {
    let leaf_slot: *mut RawGc = leaf;
    let mut roots = |visitor: &mut dyn FnMut(*mut RawGc)| visitor(leaf_slot);
    heap.alloc_trailing_with_roots_initialized(
        OpaqueVector::new(len),
        OpaqueVector::trailing_bytes(len),
        &mut roots,
        |body| {
            for index in 0..len {
                // SAFETY: the root visitor above kept `leaf_slot` current
                // across any collection this allocation triggered.
                body.set(index, unsafe { *leaf_slot });
            }
        },
    )
    .expect("initialized trailing allocation")
}

#[test]
fn an_initialized_young_tail_survives_scavenges_and_full_collections() {
    let mut heap = GcHeap::new().expect("heap");
    let mut leaf = heap
        .alloc(otter_gc::test_support::OpaqueLeaf { payload: 0xC0FFEE })
        .expect("leaf")
        .raw();
    let mut vector = initialized_vector(&mut heap, 5, &mut leaf).raw();
    for round in 0..3 {
        let vector_slot: *mut RawGc = &mut vector;
        let mut roots = |visitor: &mut dyn FnMut(*mut RawGc)| visitor(vector_slot);
        if round == 2 {
            heap.collect_full(&mut roots).expect("full collection");
        } else {
            heap.collect_minor_with_roots(&mut roots).expect("scavenge");
        }
    }
    let handle = heap
        .cast_raw_if_type::<OpaqueVector>(vector)
        .expect("vector survives");
    let elements = heap.with_payload(handle, |body| {
        (0..body.len())
            .map(|index| body.get(index))
            .collect::<Vec<_>>()
    });
    assert_eq!(elements.len(), 5);
    let first = elements[0];
    assert!(elements.iter().all(|element| *element == first));
    let leaf = heap
        .cast_raw_if_type::<otter_gc::test_support::OpaqueLeaf>(first)
        .expect("tail element still names the leaf");
    assert_eq!(heap.with_payload(leaf, |body| body.payload), 0xC0FFEE);
}

#[test]
fn an_initialized_tail_under_bootstrap_tenuring_is_barriered_to_its_young_children() {
    let mut heap = GcHeap::new().expect("heap");
    let mut leaf = heap
        .alloc(otter_gc::test_support::OpaqueLeaf { payload: 7 })
        .expect("young leaf")
        .raw();
    heap.set_tenure_all(true);
    let mut vector = initialized_vector(&mut heap, 3, &mut leaf).raw();
    heap.set_tenure_all(false);
    // Only the old vector roots the young leaf: the scavenge finds it
    // through the card the initializer's barrier scan recorded.
    let vector_slot: *mut RawGc = &mut vector;
    let mut roots = |visitor: &mut dyn FnMut(*mut RawGc)| visitor(vector_slot);
    heap.collect_minor_with_roots(&mut roots).expect("scavenge");
    let handle = heap
        .cast_raw_if_type::<OpaqueVector>(vector)
        .expect("vector survives");
    let element = heap.with_payload(handle, |body| body.get(2));
    let leaf = heap
        .cast_raw_if_type::<otter_gc::test_support::OpaqueLeaf>(element)
        .expect("tail element rewritten to the evacuated leaf");
    assert_eq!(heap.with_payload(leaf, |body| body.payload), 7);
}
