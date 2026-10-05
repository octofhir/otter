//! Collection-service capture contracts exercised through the actual heap.
//!
//! # Contents
//! - Nested full/minor suppression, pressure provenance, overflow and phases.
//! - Completed collection followed by an allocation refusal.
//!
//! # Invariants
//! - Allocations, root scans, counters and cap decisions use the production GC.
//! - Every asserted capture is complete; absent events never imply zero pauses.

use otter_gc::{
    EmptyRoots, GcHeap, GcPauseCaptureError, GcPauseKind, GcPauseOutcome, GcPauseTrigger,
    test_support::OpaqueLeaf,
};

fn heap() -> GcHeap {
    let mut heap = GcHeap::new().expect("heap");
    heap.set_gc_stress(0, false);
    heap
}

#[test]
fn full_service_records_one_outer_pause_after_roots_and_postmark() {
    let mut heap = heap();
    heap.alloc(OpaqueLeaf { payload: 17 })
        .expect("young allocation");
    heap.set_post_mark_processor(|heap| {
        assert_eq!(
            heap.start_gc_pause_capture(4),
            Err(GcPauseCaptureError::AlreadyCapturing),
        );
    });
    let mut root_visits = 0;
    heap.start_gc_pause_capture(4).expect("capture");
    heap.collect_full(&mut |_| root_visits += 1)
        .expect("full GC");
    let completed = heap.gc_stats().clone();
    let capture = heap.take_gc_pause_capture().expect("take");
    assert!(root_visits > 0);
    assert!(!capture.incomplete && !capture.contains_split_phases);
    assert_eq!(capture.dropped_records, 0);
    assert_eq!(capture.records.len(), 1);
    let event = capture.records[0];
    assert_eq!(event.kind, GcPauseKind::Full);
    assert_eq!(event.trigger, GcPauseTrigger::Explicit);
    assert_eq!(event.outcome, GcPauseOutcome::Completed);
    assert_eq!(event.full_cycles_after, event.full_cycles_before + 1);
    assert!(event.minor_cycles_after > event.minor_cycles_before);
    assert_eq!(event.full_cycles_after, completed.gc_cycles);
    assert_eq!(event.minor_cycles_after, completed.minor_gc_cycles);
}

#[test]
fn natural_nursery_pressure_has_exact_provenance_and_cycle_coverage() {
    let mut heap = heap();
    let before = heap.gc_stats().minor_gc_cycles;
    heap.start_gc_pause_capture(64).expect("capture");
    for i in 0..2_100_000u64 {
        heap.alloc(OpaqueLeaf { payload: i })
            .expect("dead leaf traffic");
    }
    let after = heap.gc_stats().minor_gc_cycles;
    let capture = heap.take_gc_pause_capture().expect("take");
    assert!(after > before, "actual pressure must collect");
    assert!(!capture.incomplete && !capture.contains_split_phases);
    assert_eq!(capture.dropped_records, 0);
    assert_eq!(capture.records.len() as u64, after - before);
    for (index, record) in capture.records.iter().enumerate() {
        assert_eq!(record.sequence, index as u64 + 1);
        assert_eq!(record.kind, GcPauseKind::Minor);
        assert_eq!(record.trigger, GcPauseTrigger::NurseryCapacity);
        assert_eq!(record.outcome, GcPauseOutcome::Completed);
        assert_eq!(record.minor_cycles_after, record.minor_cycles_before + 1);
        assert_eq!(record.full_cycles_before, record.full_cycles_after);
    }
}

#[test]
fn natural_major_pressure_includes_nested_minor_and_accounting_in_one_event() {
    let mut heap = heap();
    heap.start_gc_pause_capture(32).expect("capture");
    let before = heap.gc_stats().gc_cycles;
    // Each dead large cell consumes an actual old/large page. This crosses the
    // production growth floor; there is no manual collection or stress tick.
    for i in 0..80u64 {
        heap.alloc_trailing_with_roots(OpaqueLeaf { payload: i }, 200 * 1024, &mut |_| {})
            .expect("dead large traffic");
    }
    let after = heap.gc_stats().gc_cycles;
    let capture = heap.take_gc_pause_capture().expect("take");
    assert!(after > before, "actual page pressure must run major GC");
    assert!(!capture.incomplete && !capture.contains_split_phases);
    assert_eq!(capture.dropped_records, 0);
    assert_eq!(capture.records.len() as u64, after - before);
    for event in capture.records {
        assert_eq!(event.kind, GcPauseKind::Full);
        assert_eq!(event.trigger, GcPauseTrigger::GrowthBudget);
        assert_eq!(event.outcome, GcPauseOutcome::Completed);
        assert_eq!(event.full_cycles_after, event.full_cycles_before + 1);
        assert!(event.minor_cycles_after > event.minor_cycles_before);
    }
}

#[test]
fn disabled_collection_cannot_start_capture_from_inside_postmark() {
    let mut heap = heap();
    heap.set_post_mark_processor(|heap| {
        assert_eq!(
            heap.start_gc_pause_capture(4),
            Err(GcPauseCaptureError::CollectionInProgress)
        );
    });
    heap.collect_full(&mut |_| {})
        .expect("full GC with disabled recorder");
    assert_eq!(
        heap.take_gc_pause_capture().unwrap_err(),
        GcPauseCaptureError::NotCapturing
    );
}

#[test]
fn overflow_retains_first_event_and_counts_every_later_outer_collection() {
    let mut heap = heap();
    heap.start_gc_pause_capture(1).expect("capture");
    for _ in 0..3 {
        heap.collect_minor(EmptyRoots).expect("minor");
    }
    let capture = heap.take_gc_pause_capture().expect("take");
    assert_eq!(capture.records.len(), 1);
    assert_eq!(capture.records[0].sequence, 1);
    assert_eq!(capture.dropped_records, 2);
    assert!(!capture.incomplete);
}

#[test]
fn split_service_is_explicitly_unscoreable_as_whole_collection_pauses() {
    let mut heap = heap();
    heap.start_gc_pause_capture(8).expect("capture");
    heap.start_incremental_mark_phase(&mut |_| {})
        .expect("start");
    let _ = heap.incremental_mark_step(1);
    heap.finish_incremental_mark_phase(&mut |_| {});
    heap.run_post_mark_processing();
    heap.sweep_phase();
    let capture = heap.take_gc_pause_capture().expect("take");
    assert!(capture.contains_split_phases && !capture.incomplete);
    assert_eq!(capture.records.len(), 5);
    assert_eq!(capture.records[0].kind, GcPauseKind::SplitMark);
    assert_eq!(capture.records[1].kind, GcPauseKind::SplitStep);
    assert_eq!(capture.records[2].kind, GcPauseKind::SplitMark);
    assert_eq!(capture.records[3].kind, GcPauseKind::SplitWeak);
    assert_eq!(capture.records[4].kind, GcPauseKind::SplitSweep);
}

#[test]
fn capture_cannot_start_inside_an_already_active_incremental_cycle() {
    let mut heap = heap();
    heap.start_incremental_mark_phase(&mut |_| {})
        .expect("start without capture");
    assert_eq!(
        heap.start_gc_pause_capture(4),
        Err(GcPauseCaptureError::CollectionInProgress)
    );
    heap.finish_incremental_mark_phase(&mut |_| {});
    heap.run_post_mark_processing();
    heap.sweep_phase();
    heap.start_gc_pause_capture(4)
        .expect("quiescent after sweep");
    heap.run_post_mark_processing();
    let capture = heap
        .take_gc_pause_capture()
        .expect("standalone weak service");
    assert!(capture.contains_split_phases);
    assert_eq!(capture.records.len(), 1);
    assert_eq!(capture.records[0].kind, GcPauseKind::SplitWeak);
}

#[test]
fn cap_refusal_follows_completed_collection_and_keeps_its_outcome() {
    let mut heap = GcHeap::with_max_heap_bytes(64).expect("capped heap");
    heap.set_gc_stress(0, false);
    let retained = heap
        .reserve_external(64)
        .expect("existing live reservation");
    heap.start_gc_pause_capture(4).expect("capture");
    // This request is individually within the cap; its live reservation sum
    // requires actual full collection before the projected total is refused.
    assert!(heap.reserve_external(32).is_err());
    let capture = heap.take_gc_pause_capture().expect("take");
    assert_eq!(capture.records.len(), 1);
    let event = capture.records[0];
    assert_eq!(event.kind, GcPauseKind::Full);
    assert_eq!(event.trigger, GcPauseTrigger::HeapCap);
    assert_eq!(event.outcome, GcPauseOutcome::CompletedAllocationRefused);
    assert_eq!(event.full_cycles_after, event.full_cycles_before + 1);
    assert_eq!(heap.stats().reserved_bytes, 64);
    drop(retained);
}
