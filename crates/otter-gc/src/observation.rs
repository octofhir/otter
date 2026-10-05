//! Bounded observations of synchronous collection service on one heap.
//!
//! # Contents
//! - Owned request, pause records, completion and trigger metadata.
//! - A private recorder with one outer envelope for nested collections.
//!
//! # Invariants
//! - Capture is explicit; disabled collection entry never reads a clock.
//! - Storage is reserved before capture. Collection never grows it, formats
//!   output, walks heap objects or invokes an observation callback.
//! - Nested minors belong to their enclosing full collection or accounting
//!   envelope. Failed and incomplete observations remain distinguishable.
//! - Times describe collection service, including its trigger accounting tail;
//!   they do not describe allocation latency or concurrent collector work.
//!
//! # See also
//! - `crate::heap::GcHeap` owns the actual collection and accounting envelopes.

use std::time::Instant;

#[cfg(test)]
#[path = "recorder_tests.rs"]
mod tests;

/// A synchronous collector service operation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GcPauseKind {
    /// One copying collection with all root and relocation work.
    Minor,
    /// One outer full service, including nested copying collections.
    Full,
    /// A standalone initial/final mark or additional marking operation.
    SplitMark,
    /// One incremental marking step, possibly separated by mutator execution.
    SplitStep,
    /// Standalone old-space sweep and accounting.
    SplitSweep,
    /// Standalone ephemeron, weak-reference, or post-mark processing.
    SplitWeak,
}

/// The authoritative reason the collector was entered.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GcPauseTrigger {
    /// Direct host/runtime collection request.
    Explicit,
    /// Configured diagnostic allocation stress.
    Stress,
    /// Young allocation could not fit after normal nursery growth.
    NurseryCapacity,
    /// Admission needed collection before a heap-cap decision.
    HeapCap,
    /// Actual old/large allocation growth crossed the collection budget.
    GrowthBudget,
    /// Runtime code reclamation requested its rooted reachability collection.
    CodeReclamation,
}

/// Result of an observed service operation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GcPauseOutcome {
    /// Collector and the trigger's enclosed accounting completed successfully.
    Completed,
    /// Collector service returned an allocation/resource failure.
    CollectionFailed,
    /// Full collection completed, but the original allocation remained over cap.
    CompletedAllocationRefused,
}

/// One owned scalar outer-service observation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GcPauseRecord {
    /// One-based outer-service order within this capture.
    pub sequence: u64,
    /// Monotonic nanoseconds since this capture's local origin.
    pub start_ns: u64,
    /// Enclosed synchronous service duration, in nanoseconds.
    pub duration_ns: u64,
    /// Service boundary owned by the collector entry.
    pub kind: GcPauseKind,
    /// Actual trigger supplied by the entry owner.
    pub trigger: GcPauseTrigger,
    /// Collector completion or enclosed post-collection refusal.
    pub outcome: GcPauseOutcome,
    /// Full-cycle counter immediately before the outer service.
    pub full_cycles_before: u64,
    /// Full-cycle counter after all enclosed work and accounting.
    pub full_cycles_after: u64,
    /// Minor-cycle counter immediately before the outer service.
    pub minor_cycles_before: u64,
    /// Minor-cycle counter afterward, including full service's nested minors.
    pub minor_cycles_after: u64,
}

/// Complete owned transfer of one bounded capture, including coverage gaps.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GcPauseCapture {
    /// Every successfully closed outer record that fit the reserved buffer.
    pub records: Vec<GcPauseRecord>,
    /// Outer services omitted because the reserved record buffer was full.
    pub dropped_records: u64,
    /// Transfer occurred while a service token remained active.
    pub incomplete: bool,
    /// Standalone mark/step/weak/sweep service cannot represent whole cycles.
    pub contains_split_phases: bool,
}

/// Explicit capture admission errors.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GcPauseCaptureError {
    /// A capture already owns the heap's bounded recorder.
    AlreadyCapturing,
    /// Collection or an incremental cycle is already in progress.
    CollectionInProgress,
    /// No capture exists to transfer.
    NotCapturing,
    /// Requested capacity is zero or exceeds the supported maximum.
    InvalidCapacity,
    /// Native storage reservation failed before capture began.
    BufferAllocation {
        /// Exact record-buffer bytes requested from the native allocator.
        requested_bytes: u64,
    },
}

const MAX_RECORDS: usize = 65_536;

struct Capture {
    origin: Instant,
    capacity: usize,
    records: Vec<GcPauseRecord>,
    sequence: u64,
    dropped_records: u64,
    contains_split_phases: bool,
}

/// One recorder owned directly by the heap, with no observation side channel.
#[derive(Default)]
pub(crate) struct GcPauseRecorder {
    capture: Option<Box<Capture>>,
    depth: u32,
}

/// Stack-owned envelope data never borrows the heap during collection.
pub(crate) struct PauseToken {
    entered: bool,
    start: Option<(Instant, GcPauseRecord)>,
}

impl GcPauseRecorder {
    pub(crate) fn is_capturing(&self) -> bool {
        self.capture.is_some()
    }

    pub(crate) fn start(&mut self, capacity: usize) -> Result<(), GcPauseCaptureError> {
        if self.capture.is_some() {
            return Err(GcPauseCaptureError::AlreadyCapturing);
        }
        if self.depth != 0 {
            return Err(GcPauseCaptureError::CollectionInProgress);
        }
        if capacity == 0 || capacity > MAX_RECORDS {
            return Err(GcPauseCaptureError::InvalidCapacity);
        }
        let mut records = Vec::new();
        records
            .try_reserve_exact(capacity)
            .map_err(|_| GcPauseCaptureError::BufferAllocation {
                requested_bytes: (capacity * std::mem::size_of::<GcPauseRecord>()) as u64,
            })?;
        self.capture = Some(Box::new(Capture {
            origin: Instant::now(),
            capacity,
            records,
            sequence: 0,
            dropped_records: 0,
            contains_split_phases: false,
        }));
        Ok(())
    }

    pub(crate) fn take(&mut self) -> Result<GcPauseCapture, GcPauseCaptureError> {
        let capture = self
            .capture
            .take()
            .ok_or(GcPauseCaptureError::NotCapturing)?;
        Ok(GcPauseCapture {
            records: capture.records,
            dropped_records: capture.dropped_records,
            incomplete: self.depth != 0,
            contains_split_phases: capture.contains_split_phases,
        })
    }

    pub(crate) fn begin(
        &mut self,
        kind: GcPauseKind,
        trigger: GcPauseTrigger,
        full_cycles: u64,
        minor_cycles: u64,
    ) -> PauseToken {
        let outermost = self.depth == 0;
        self.depth = self
            .depth
            .checked_add(1)
            .expect("bounded GC envelope depth");
        let Some(capture) = self.capture.as_mut() else {
            return PauseToken {
                entered: true,
                start: None,
            };
        };
        capture.contains_split_phases |= outermost
            && matches!(
                kind,
                GcPauseKind::SplitMark
                    | GcPauseKind::SplitStep
                    | GcPauseKind::SplitSweep
                    | GcPauseKind::SplitWeak
            );
        let start = if !outermost {
            None
        } else {
            capture.sequence = capture.sequence.saturating_add(1);
            if capture.records.len() == capture.capacity {
                capture.dropped_records = capture.dropped_records.saturating_add(1);
                None
            } else {
                let start = Instant::now();
                Some((
                    start,
                    GcPauseRecord {
                        sequence: capture.sequence,
                        start_ns: start.duration_since(capture.origin).as_nanos() as u64,
                        duration_ns: 0,
                        kind,
                        trigger,
                        outcome: GcPauseOutcome::Completed,
                        full_cycles_before: full_cycles,
                        full_cycles_after: full_cycles,
                        minor_cycles_before: minor_cycles,
                        minor_cycles_after: minor_cycles,
                    },
                ))
            }
        };
        PauseToken {
            entered: true,
            start,
        }
    }

    pub(crate) fn end(
        &mut self,
        token: PauseToken,
        outcome: GcPauseOutcome,
        full_cycles: u64,
        minor_cycles: u64,
    ) {
        if !token.entered {
            return;
        }
        self.depth = self.depth.checked_sub(1).expect("matching GC envelope");
        if let Some(capture) = self.capture.as_mut()
            && let Some((start, mut record)) = token.start
        {
            record.duration_ns = start.elapsed().as_nanos() as u64;
            record.outcome = outcome;
            record.full_cycles_after = full_cycles;
            record.minor_cycles_after = minor_cycles;
            // The outer token owns a reserved slot; nested envelopes never
            // append and capture cannot restart while a token is active.
            debug_assert!(capture.records.len() < capture.capacity);
            capture.records.push(record);
        }
    }
}
