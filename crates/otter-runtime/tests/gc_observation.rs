//! Owned runtime observation across real rooted full collections.
//!
//! # Contents
//! - Actual collector counter reconciliation and immutable owned record transfer.
//! - Admission errors and overflow remain explicit at the runtime boundary.
//!
//! # Invariants
//! - Collection uses the runtime's production root visitor and successful API.
//! - Capture never relies on a heap handle, elapsed threshold, or absent event.

use otter_runtime::{
    GcPauseCapture, GcPauseKind, GcPauseOutcome, GcPauseTrigger, OtterError, Runtime, SourceInput,
};

#[test]
fn runtime_transfers_owned_capture_after_real_rooted_collection() {
    fn send_sync<T: Send + Sync>() {}
    send_sync::<GcPauseCapture>();
    let mut runtime = Runtime::builder().build().expect("runtime");
    let setup = runtime
        .run_script(
            SourceInput::from_javascript("globalThis.retainedProbe = {child: {value: 42}}; 42;"),
            "gc-observation-setup.js",
        )
        .expect("rooted setup");
    assert_eq!(setup.completion_string(), "42");
    drop(setup);
    let before = runtime.execution_stats();
    runtime.start_gc_pause_capture(4).expect("capture");
    runtime.force_gc().expect("first rooted full collection");
    runtime.force_gc().expect("second rooted full collection");
    let after = runtime.execution_stats();
    let capture = runtime.take_gc_pause_capture().expect("owned capture");
    assert!(!capture.incomplete && !capture.contains_split_phases);
    assert_eq!(capture.dropped_records, 0);
    assert_eq!(capture.records.len(), 2);
    let mut full = before.gc_cycles;
    let mut minor = before.gc_minor_cycles;
    let mut previous_end = 0;
    for (index, record) in capture.records.iter().enumerate() {
        assert_eq!(record.sequence, index as u64 + 1);
        assert_eq!(record.kind, GcPauseKind::Full);
        assert_eq!(record.trigger, GcPauseTrigger::Explicit);
        assert_eq!(record.outcome, GcPauseOutcome::Completed);
        assert_eq!(record.full_cycles_before, full);
        assert_eq!(record.minor_cycles_before, minor);
        assert_eq!(record.full_cycles_after, full + 1);
        assert!(record.minor_cycles_after > minor);
        assert!(record.start_ns >= previous_end);
        previous_end = record.start_ns.checked_add(record.duration_ns).unwrap();
        full = record.full_cycles_after;
        minor = record.minor_cycles_after;
    }
    assert_eq!(full, after.gc_cycles);
    assert_eq!(minor, after.gc_minor_cycles);
    let check = runtime
        .run_script(
            SourceInput::from_javascript("retainedProbe.child.value;"),
            "gc-observation-check.js",
        )
        .expect("live child after collection");
    assert_eq!(check.completion_string(), "42");
    assert_eq!(
        capture.records.len(),
        2,
        "later runtime work cannot mutate owned records"
    );
    assert!(matches!(
        runtime.take_gc_pause_capture(),
        Err(OtterError::Usage { .. })
    ));
}

#[test]
fn runtime_keeps_admission_and_overflow_distinct_from_complete_capture() {
    let mut runtime = Runtime::builder().build().expect("runtime");
    assert!(matches!(
        runtime.start_gc_pause_capture(0),
        Err(OtterError::Usage { .. })
    ));
    runtime
        .start_gc_pause_capture(1)
        .expect("single record capture");
    assert!(matches!(
        runtime.start_gc_pause_capture(2),
        Err(OtterError::Usage { .. })
    ));
    runtime.force_gc().expect("first full collection");
    runtime.force_gc().expect("second full collection");
    let capture = runtime.take_gc_pause_capture().expect("overflow capture");
    assert_eq!(capture.records.len(), 1);
    assert_eq!(capture.records[0].sequence, 1);
    assert_eq!(capture.dropped_records, 1);
    assert!(!capture.incomplete && !capture.contains_split_phases);
    runtime
        .start_gc_pause_capture(1)
        .expect("restart only after take");
    let empty = runtime
        .take_gc_pause_capture()
        .expect("complete empty interval");
    assert!(empty.records.is_empty());
    assert_eq!(empty.dropped_records, 0);
    assert!(!empty.incomplete && !empty.contains_split_phases);
}
