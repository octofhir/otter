//! Bounded collection-service capture at heap pause boundaries.
//!
//! # Contents
//! - Pause capture lifecycle and exact occupancy snapshots.
//! - Collection outcomes and trigger attribution.
//!
//! # Invariants
//! - Capture remains default-off and retains no heap values.
//! - Full service includes nested minor work without double-counting.
//!
//! # See also
//! - [`crate::observation`].

use super::GcHeap;
use crate::observation::{
    GcPauseCapture, GcPauseCaptureError, GcPauseKind, GcPauseOutcome, GcPauseTrigger,
};
use crate::oom::OutOfMemory;
use std::time::Instant;

impl GcHeap {
    /// Begin a bounded collection-service capture at a quiescent heap boundary.
    /// Storage is reserved now; collection never allocates observation records.
    pub fn start_gc_pause_capture(&mut self, capacity: usize) -> Result<(), GcPauseCaptureError> {
        if self.marking.is_marking() && !self.pause_recorder.is_capturing() {
            return Err(GcPauseCaptureError::CollectionInProgress);
        }
        self.pause_recorder.start(capacity)
    }

    /// Stop capture and move out owned scalar records, including overflow and
    /// incomplete/unsupported split-phase metadata. No heap walk is performed.
    pub fn take_gc_pause_capture(&mut self) -> Result<GcPauseCapture, GcPauseCaptureError> {
        self.pause_recorder.take()
    }

    pub(super) fn with_gc_pause<R>(
        &mut self,
        kind: GcPauseKind,
        trigger: GcPauseTrigger,
        work: impl FnOnce(&mut Self) -> (R, GcPauseOutcome),
    ) -> R {
        // No compiler thread reads the heap while it changes under a pause.
        let right = std::sync::Arc::clone(&self.read_right);
        let _exclusive = right.exclude();
        crate::compressed::age_retained_pages(Instant::now());
        let token = self.pause_recorder.begin(
            kind,
            trigger,
            self.gc_stats.gc_cycles,
            self.gc_stats.minor_gc_cycles,
            self.observed_occupancy(),
        );
        let (result, outcome) = work(self);
        let occupancy = self.observed_occupancy();
        self.pause_recorder.end(
            token,
            outcome,
            self.gc_stats.gc_cycles,
            self.gc_stats.minor_gc_cycles,
            occupancy,
        );
        result
    }

    /// Occupancy for a captured pause record; nothing is read while no
    /// capture runs.
    pub(super) fn observed_occupancy(&self) -> crate::observation::HeapOccupancy {
        if !self.pause_recorder.is_capturing() {
            return crate::observation::HeapOccupancy::default();
        }
        crate::observation::HeapOccupancy {
            old_bytes: self.old_generation_page_bytes(),
            old_allocated_bytes: (self.old_space.allocated_bytes()
                + self.large_space.allocated_bytes()) as u64,
            young_bytes: (self.new_space.allocated_bytes()
                + self.young_large_space.allocated_bytes()) as u64,
        }
    }

    /// Hand over the pause records captured so far and keep capturing.
    /// `None` when no capture runs or a collection is in progress.
    pub fn drain_gc_pause_capture(&mut self) -> Option<GcPauseCapture> {
        self.pause_recorder.drain()
    }

    pub(super) fn with_gc_pause_result<R>(
        &mut self,
        kind: GcPauseKind,
        trigger: GcPauseTrigger,
        work: impl FnOnce(&mut Self) -> Result<R, OutOfMemory>,
    ) -> Result<R, OutOfMemory> {
        self.with_gc_pause(kind, trigger, |heap| {
            let full_before = heap.gc_stats.gc_cycles;
            let result = work(heap);
            let outcome = if result.is_ok() {
                GcPauseOutcome::Completed
            } else if trigger == GcPauseTrigger::HeapCap
                && heap.gc_stats.gc_cycles > full_before
                && matches!(result, Err(OutOfMemory::HeapCapExceeded { .. }))
            {
                GcPauseOutcome::CompletedAllocationRefused
            } else {
                GcPauseOutcome::CollectionFailed
            };
            (result, outcome)
        })
    }
}
