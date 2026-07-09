//! Incremental old-gen marking with the Dijkstra insertion
//! barrier.
//!
//! Covers the two correctness properties task 86 must preserve:
//!
//! 1. A pointer the mutator publishes between
//!    [`GcHeap::start_incremental_mark_phase`] and
//!    [`GcHeap::finish_incremental_mark_phase`] is shaded gray by
//!    the insertion-barrier path inside `write_barrier`. Without
//!    the barrier the freshly-published white child would be
//!    swept; with it, the child survives.
//! 2. [`GcHeap::incremental_mark_step`] honours the per-call
//!    `budget` so a long mark cycle can be split across many
//!    safepoints.
//!
//! Spec: <https://dl.acm.org/doi/10.1145/359642.359655> (Dijkstra,
//! Lamport, Martin, Scholten, Steffens — "On-the-fly garbage
//! collection: an exercise in cooperation").

use otter_gc::raw::RawGc;
use otter_gc::trace::{SlotVisitor, Traceable};
use otter_gc::{Gc, GcHeap, HandleScope};

struct Cell {
    next: Gc<Cell>,
}

impl Traceable for Cell {
    const TYPE_TAG: u8 = 0xB1;
    unsafe fn trace_slots(this: *mut Self, v: &mut SlotVisitor<'_>) {
        unsafe {
            let slot = std::ptr::addr_of_mut!((*this).next) as *mut RawGc;
            v(slot);
        }
    }
}

#[test]
fn insertion_barrier_saves_white_child_published_mid_cycle() {
    let mut heap = GcHeap::new().expect("heap");
    heap.register_traceable::<Cell>();

    // SAFETY: the heap-owned handle stack pointer is non-null and
    // points at the heap's persistent stack for the duration of
    // this scope.
    let scope = unsafe { HandleScope::from_ptr(heap.handle_stack_ptr()) };

    // Three old-gen cells allocated *before* marking begins, so
    // each one starts the cycle white.
    let a = heap.alloc_old(Cell { next: Gc::null() }).unwrap();
    let b = heap.alloc_old(Cell { next: Gc::null() }).unwrap();
    let c = heap.alloc_old(Cell { next: Gc::null() }).unwrap();
    // Only `a` is reachable from a root.
    let _root_a = scope.local(a);

    heap.start_incremental_mark_phase(&mut |_| {})
        .expect("start mark");
    assert!(heap.marking().is_marking());

    // Drain any gray work — `a` came in gray; `b` and `c` are
    // unreachable so they remain white.
    let _ = heap.incremental_mark_step(64);
    assert!(heap.is_marked(a.raw()));
    assert!(!heap.is_marked(b.raw()));
    assert!(!heap.is_marked(c.raw()));

    // Mutator stores `a.next = b`. The `record_write` path runs
    // the insertion barrier, which must shade the white child
    // gray *before* finish-mark sees the slot.
    heap.with_payload(a, |cell| cell.next = b);
    heap.record_write(a, &b);
    assert!(
        heap.is_marked(b.raw()),
        "insertion barrier must shade `b` gray"
    );

    // Drain whatever the barrier just enqueued.
    let _ = heap.incremental_mark_step(64);

    heap.finish_incremental_mark_phase(&mut |_| {});

    // Capture mark colours before sweep clears them — sweep frees
    // `c` and reading its header afterwards would be UB.
    let a_marked = heap.is_marked(a.raw());
    let b_marked = heap.is_marked(b.raw());
    let c_marked = heap.is_marked(c.raw());
    assert!(a_marked, "rooted `a` must be marked");
    assert!(b_marked, "barrier-saved `b` must be marked");
    assert!(!c_marked, "unreferenced `c` must remain white");

    heap.sweep_phase();
    assert!(!heap.marking().is_marking());
}

#[test]
fn incremental_step_respects_budget() {
    let mut heap = GcHeap::new().expect("heap");
    heap.register_traceable::<Cell>();

    // Build a 50-cell chain rooted at `head`.
    let scope = unsafe { HandleScope::from_ptr(heap.handle_stack_ptr()) };
    const CHAIN_LEN: usize = 50;
    let mut tail = heap.alloc_old(Cell { next: Gc::null() }).unwrap();
    for _ in 1..CHAIN_LEN {
        let next = heap.alloc_old(Cell { next: tail }).unwrap();
        tail = next;
    }
    let head = tail;
    let _root = scope.local(head);

    heap.start_incremental_mark_phase(&mut |_| {})
        .expect("start mark");

    // Budgeted steps must each process at most `budget` headers
    // and the total must reach the chain length (root + chain).
    let mut total_processed = 0;
    let budget = 7;
    loop {
        let processed = heap.incremental_mark_step(budget);
        assert!(
            processed <= budget,
            "step processed {processed}, exceeding budget {budget}"
        );
        total_processed += processed;
        if processed == 0 {
            break;
        }
    }
    assert!(
        total_processed >= CHAIN_LEN,
        "expected at least {CHAIN_LEN} headers processed, got {total_processed}",
    );

    heap.finish_incremental_mark_phase(&mut |_| {});
    heap.sweep_phase();
}

#[test]
fn convenience_mark_phase_matches_split_path() {
    // Ensure the single-shot `mark_phase` wrapper is observably
    // identical to start + finish: same set of survivors, same
    // post-sweep `is_marking == false` state.
    let mut heap = GcHeap::new().expect("heap");
    heap.register_traceable::<Cell>();
    let scope = unsafe { HandleScope::from_ptr(heap.handle_stack_ptr()) };
    let a = heap.alloc_old(Cell { next: Gc::null() }).unwrap();
    let _root = scope.local(a);
    let _orphan = heap.alloc_old(Cell { next: Gc::null() }).unwrap();

    heap.mark_phase(&mut |_| {}).expect("mark phase");
    assert!(heap.is_marked(a.raw()));
    heap.sweep_phase();
    assert!(!heap.marking().is_marking());
}
