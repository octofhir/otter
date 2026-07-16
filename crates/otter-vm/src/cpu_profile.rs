//! Opt-in VM stack sampling for CPU-profile artifacts.
//!
//! The profiler samples the currently active VM stack every configured number
//! of bytecode dispatch ticks and stores owned [`StackFrameSnapshot`] frames.
//! It is intentionally passive: no signals, helper threads, or raw frame
//! pointers cross the runtime boundary.
//!
//! # Contents
//! - [`CpuProfiler`] — dispatch-loop sampler.
//! - [`CpuProfile`] — owned sample data returned to embedders.
//!
//! # Invariants
//! - Disabled profilers cost only an `Option` check in the dispatch loop.
//! - Samples contain owned frame metadata, never borrowed frames/registers.
//! - `time_deltas_us` has one entry per sample and uses wall-clock deltas between
//!   sample points so Chrome profile consumers can render a timeline.
//!
//! # See also
//! - [`crate::error_ops::snapshot_frames`]
//! - [`crate::run_control::StackFrameSnapshot`]

use serde::{Deserialize, Serialize};

use crate::activation_stack::ActivationStack;
use crate::error_ops::snapshot_frames;
use crate::{ExecutionContext, StackFrameSnapshot};

/// Owned VM stack profile captured during one run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CpuProfile {
    /// Bytecode dispatch ticks between sample attempts.
    pub interval: u64,
    /// Top-frame first stack samples.
    pub samples: Vec<Vec<StackFrameSnapshot>>,
    /// Wall-clock microseconds since the previous sample.
    pub time_deltas_us: Vec<u64>,
}

impl CpuProfile {
    /// Number of recorded samples.
    #[must_use]
    pub fn sample_count(&self) -> usize {
        self.samples.len()
    }
}

/// Dispatch-loop VM stack sampler.
#[derive(Debug)]
pub(crate) struct CpuProfiler {
    interval: u64,
    ticks_until_sample: u64,
    samples: Vec<Vec<StackFrameSnapshot>>,
    time_deltas_us: Vec<u64>,
    last_sample_at: std::time::Instant,
}

impl CpuProfiler {
    /// Create a profiler that samples every `interval` bytecode ticks.
    #[must_use]
    pub(crate) fn new(interval: u64) -> Self {
        let interval = interval.max(1);
        Self {
            interval,
            ticks_until_sample: interval,
            samples: Vec::new(),
            time_deltas_us: Vec::new(),
            last_sample_at: std::time::Instant::now(),
        }
    }

    /// Tick the sampler and capture a stack when the interval expires.
    pub(crate) fn maybe_sample(&mut self, context: &ExecutionContext, stack: &ActivationStack) {
        if self.ticks_until_sample > 1 {
            self.ticks_until_sample -= 1;
            return;
        }
        self.ticks_until_sample = self.interval;
        let now = std::time::Instant::now();
        let delta = now
            .saturating_duration_since(self.last_sample_at)
            .as_micros()
            .max(1) as u64;
        self.last_sample_at = now;
        self.samples.push(snapshot_frames(context, stack));
        self.time_deltas_us.push(delta);
    }

    /// Consume the sampler and return the owned profile.
    #[must_use]
    pub(crate) fn finish(self) -> CpuProfile {
        CpuProfile {
            interval: self.interval,
            samples: self.samples,
            time_deltas_us: self.time_deltas_us,
        }
    }
}
