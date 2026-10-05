//! Owned collection-service observations at the runtime boundary.
//!
//! # Contents
//! - Quiescent capture admission and transfer of the collector's scalar records.
//! - Mapping capture admission onto the runtime's one public error model.
//!
//! # Invariants
//! - The collector owns the only record format and recorder.
//! - These methods neither walk objects nor expose VM handles.
//! - All formatting occurs outside collection service.
//!
//! # See also
//! - `otter_gc::observation` defines the observed service boundaries.

pub use otter_gc::{GcPauseCapture, GcPauseKind, GcPauseOutcome, GcPauseRecord, GcPauseTrigger};

use crate::{OtterError, Runtime};

impl Runtime {
    /// Start an optional bounded collection-service capture.
    ///
    /// Reserve space for `capacity` records (1 through 65,536) before entering
    /// JavaScript. The disabled recorder performs no observation clock reads.
    /// Capture overflow and split service phases remain visible on transfer.
    ///
    /// # Errors
    /// Returns a usage error for invalid capacity or an active capture/service;
    /// returns an allocation error if the native observation buffer is refused.
    pub fn start_gc_pause_capture(&mut self, capacity: usize) -> Result<(), OtterError> {
        self.interp
            .gc_heap_mut()
            .start_gc_pause_capture(capacity)
            .map_err(capture_error)
    }

    /// Stop capture and transfer the same owned collector records.
    ///
    /// Nanoseconds use a monotonic origin local to this capture. Full service
    /// includes its nested minors; split API calls are separately labeled.
    /// These intervals are neither whole allocation latency nor a measure of
    /// concurrent collector work.
    ///
    /// # Errors
    /// Returns a usage error when no capture is active.
    pub fn take_gc_pause_capture(&mut self) -> Result<GcPauseCapture, OtterError> {
        self.interp
            .gc_heap_mut()
            .take_gc_pause_capture()
            .map_err(capture_error)
    }
}

fn capture_error(error: otter_gc::GcPauseCaptureError) -> OtterError {
    use otter_gc::GcPauseCaptureError;
    let message = match error {
        GcPauseCaptureError::AlreadyCapturing => "GC pause capture is already active",
        GcPauseCaptureError::CollectionInProgress => "GC service is currently in progress",
        GcPauseCaptureError::NotCapturing => "GC pause capture is not active",
        GcPauseCaptureError::InvalidCapacity => "GC pause capacity must be between 1 and 65536",
        GcPauseCaptureError::BufferAllocation { requested_bytes } => {
            return OtterError::OutOfMemory {
                requested_bytes,
                heap_limit_bytes: 0,
            };
        }
    };
    OtterError::Usage {
        message: message.into(),
    }
}
