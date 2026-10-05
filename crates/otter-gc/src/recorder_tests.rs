//! Incomplete transfer and admission invariants of the bounded recorder.
//!
//! # Contents
//! - Active-token transfer/restart admission and reserved-buffer stability.
//!
//! # Invariants
//! - Nested service cannot append, grow storage, or invent a split phase.

use super::*;

#[test]
fn incomplete_transfer_cannot_restart_until_the_original_envelope_closes() {
    let mut recorder = GcPauseRecorder::default();
    recorder.start(2).unwrap();
    let token = recorder.begin(GcPauseKind::Full, GcPauseTrigger::Explicit, 0, 0);
    let capture = recorder.take().unwrap();
    assert!(capture.incomplete);
    assert!(capture.records.is_empty());
    assert_eq!(
        recorder.start(2),
        Err(GcPauseCaptureError::CollectionInProgress)
    );
    recorder.end(token, GcPauseOutcome::Completed, 1, 1);
    recorder.start(2).unwrap();
    let token = recorder.begin(GcPauseKind::Minor, GcPauseTrigger::Explicit, 1, 1);
    recorder.end(token, GcPauseOutcome::Completed, 1, 2);
    let capture = recorder.take().unwrap();
    assert!(!capture.incomplete);
    assert_eq!(capture.records.len(), 1);
    assert_eq!(capture.records[0].sequence, 1);
}

#[test]
fn admission_limits_and_reserved_storage_are_stable_across_nested_service() {
    let mut recorder = GcPauseRecorder::default();
    assert_eq!(recorder.start(0), Err(GcPauseCaptureError::InvalidCapacity));
    assert_eq!(
        recorder.start(MAX_RECORDS + 1),
        Err(GcPauseCaptureError::InvalidCapacity)
    );
    recorder.start(2).unwrap();
    let buffer = recorder.capture.as_ref().unwrap().records.as_ptr();
    let capacity = recorder.capture.as_ref().unwrap().records.capacity();
    let outer = recorder.begin(GcPauseKind::Full, GcPauseTrigger::Explicit, 0, 0);
    let inner = recorder.begin(GcPauseKind::SplitMark, GcPauseTrigger::Explicit, 0, 1);
    recorder.end(inner, GcPauseOutcome::Completed, 0, 1);
    recorder.end(outer, GcPauseOutcome::Completed, 1, 1);
    assert_eq!(recorder.capture.as_ref().unwrap().records.as_ptr(), buffer);
    assert_eq!(
        recorder.capture.as_ref().unwrap().records.capacity(),
        capacity
    );
    let capture = recorder.take().unwrap();
    assert_eq!(capture.records.len(), 1);
    assert!(!capture.contains_split_phases);
}
