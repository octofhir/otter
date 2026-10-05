//! Failed copying service retains an exact closed observation and unchanged root.
//!
//! # Contents
//! - A separate binary exhausts a small cage before promotion preflight.
//!
//! # Invariants
//! - The only rooted young cell survives once before the old tail is filled.
//! - Failure is an actual collector error, never a synthesized result.

use otter_gc::{
    EmptyRoots, GcHeap, GcPauseKind, GcPauseOutcome, GcPauseTrigger, HandleScope, OutOfMemory,
    PAGE_SIZE, init_cage_with_size, test_support::OpaqueLeaf,
};

#[test]
fn failed_promotion_records_error_without_rewriting_the_live_root() {
    let pages = otter_gc::space::DEFAULT_NEW_SPACE_PAGES * 2 + 2;
    init_cage_with_size(PAGE_SIZE * pages).expect("isolated small cage");
    let mut heap = GcHeap::new().expect("heap");
    heap.set_gc_stress(0, false);
    // SAFETY: the scope outlives every local and drops before this heap;
    // its pointer denotes the heap's stable, owned handle stack.
    let scope = unsafe { HandleScope::from_ptr(heap.handle_stack_ptr()) };
    let young = scope.local(heap.alloc(OpaqueLeaf { payload: 17 }).expect("young cell"));
    heap.collect_minor(EmptyRoots).expect("first survival");
    let before = young.get();
    let mut allocated = 0;
    loop {
        match heap.alloc_old(OpaqueLeaf { payload: allocated }) {
            Ok(_) => allocated += 1,
            Err(error) => {
                assert_eq!(error, OutOfMemory::CageExhausted);
                break;
            }
        }
        assert!(
            allocated < PAGE_SIZE as u64,
            "one old page must be exhausted"
        );
    }
    assert!(allocated > 0);
    heap.start_gc_pause_capture(4).expect("capture");
    assert_eq!(
        heap.collect_minor(EmptyRoots),
        Err(OutOfMemory::CageExhausted)
    );
    let capture = heap
        .take_gc_pause_capture()
        .expect("closed capture after error");
    assert!(!capture.incomplete && !capture.contains_split_phases);
    assert_eq!(capture.dropped_records, 0);
    assert_eq!(capture.records.len(), 1);
    let event = capture.records[0];
    assert_eq!(event.kind, GcPauseKind::Minor);
    assert_eq!(event.trigger, GcPauseTrigger::Explicit);
    assert_eq!(event.outcome, GcPauseOutcome::CollectionFailed);
    assert_eq!(event.minor_cycles_before, event.minor_cycles_after);
    assert_eq!(young.get(), before);
    assert_eq!(heap.read_payload(young.get(), |body| body.payload), 17);
}
