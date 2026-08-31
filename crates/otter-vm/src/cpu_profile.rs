//! Bounded opt-in VM stack sampling for CPU-profile artifacts.
//!
//! The profiler samples the active VM stack every configured number of
//! bytecode dispatch ticks. It owns no signals, helper threads, raw frame
//! pointers, or user-configurable memory knobs.
//!
//! # Contents
//! - [`CpuProfiler`] — dispatch-loop sampler and lifetime admission state.
//! - [`CpuProfile`] — owned sample batches returned to embedders.
//!
//! # Invariants
//! - Disabled profilers cost only an `Option` check in the dispatch loop.
//! - One profiler lifetime retains at most [`CPU_PROFILE_SAMPLE_LIMIT`] samples
//!   and [`CPU_PROFILE_RETAINED_BYTE_LIMIT`] logical sample bytes.
//! - One sample owns at most [`CPU_PROFILE_MAX_FRAMES`] frames and
//!   [`CPU_PROFILE_SAMPLE_BYTE_LIMIT`] bytes. Frame strings are inspected and
//!   admitted before fallible copies are allocated.
//! - Draining transfers samples without resetting lifetime admission; callers
//!   cannot bypass the limits by retaining several drained batches.
//! - `time_deltas_us` has one entry per retained sample. Dropped attempts do not
//!   advance the recorded-sample clock.
//!
//! # See also
//! - [`crate::stack_snapshot::visit_frame_snapshots`]
//! - [`crate::run_control::StackFrameSnapshot`]

use serde::{Deserialize, Serialize};

use crate::activation_stack::ActivationStack;
use crate::stack_snapshot::visit_frame_snapshots;
use crate::{ExecutionContext, StackFrameSnapshot};

/// Maximum retained samples across one installed profiler's lifetime.
const CPU_PROFILE_SAMPLE_LIMIT: u64 = 65_536;
/// Maximum logical bytes retained across one installed profiler's lifetime.
const CPU_PROFILE_RETAINED_BYTE_LIMIT: usize = 64 * 1024 * 1024;
/// Maximum logical bytes retained by one sample.
const CPU_PROFILE_SAMPLE_BYTE_LIMIT: usize = 1024 * 1024;
/// Maximum JavaScript frames owned by one sample, matching V8's stack bound.
const CPU_PROFILE_MAX_FRAMES: usize = 255;
/// Conservative outer-vector and allocator overhead charged to every sample.
const CPU_PROFILE_SAMPLE_OVERHEAD_BYTES: usize = 128;

/// Owned VM stack profile captured during one run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CpuProfile {
    /// Bytecode dispatch ticks between sample attempts.
    pub interval: u64,
    /// Top-frame first stack samples.
    pub samples: Vec<Vec<StackFrameSnapshot>>,
    /// Wall-clock microseconds since the previous retained sample.
    pub time_deltas_us: Vec<u64>,
    /// Sample attempts discarded by count, memory, or allocation admission.
    pub dropped_samples: u64,
    /// Frames omitted from retained samples by the per-sample stack bound.
    pub truncated_frames: u64,
}

impl CpuProfile {
    /// Number of retained samples.
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
    recorded_samples: u64,
    retained_bytes: usize,
    dropped_samples: u64,
    truncated_frames: u64,
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
            recorded_samples: 0,
            retained_bytes: 0,
            dropped_samples: 0,
            truncated_frames: 0,
        }
    }

    /// Tick the sampler and capture a stack when the interval expires.
    ///
    /// Kept out of line: the dispatch loop calls this per instruction behind a
    /// loop-invariant "profiler installed" branch, and inlining the sampling
    /// body there costs hot-loop registers and instruction cache.
    #[inline(never)]
    pub(crate) fn maybe_sample(&mut self, context: &ExecutionContext, stack: &ActivationStack) {
        if self.ticks_until_sample > 1 {
            self.ticks_until_sample -= 1;
            return;
        }
        self.ticks_until_sample = self.interval;

        let Some(remaining) = CPU_PROFILE_RETAINED_BYTE_LIMIT.checked_sub(self.retained_bytes)
        else {
            self.record_drop();
            return;
        };
        if self.recorded_samples >= CPU_PROFILE_SAMPLE_LIMIT
            || remaining < CPU_PROFILE_SAMPLE_OVERHEAD_BYTES
        {
            self.record_drop();
            return;
        }
        let frame_budget = remaining
            .min(CPU_PROFILE_SAMPLE_BYTE_LIMIT)
            .saturating_sub(CPU_PROFILE_SAMPLE_OVERHEAD_BYTES);
        let capture = match capture_sample(context, stack, frame_budget) {
            Ok(capture) => capture,
            Err(()) => {
                self.record_drop();
                return;
            }
        };
        let _ = self.try_record_sample(capture, std::time::Instant::now());
    }

    fn try_record_sample(&mut self, capture: CapturedSample, now: std::time::Instant) -> bool {
        let Some(sample_bytes) = capture
            .retained_bytes
            .checked_add(CPU_PROFILE_SAMPLE_OVERHEAD_BYTES)
        else {
            self.record_drop();
            return false;
        };
        let admitted = self.recorded_samples < CPU_PROFILE_SAMPLE_LIMIT
            && sample_bytes <= CPU_PROFILE_SAMPLE_BYTE_LIMIT
            && self
                .retained_bytes
                .checked_add(sample_bytes)
                .is_some_and(|total| total <= CPU_PROFILE_RETAINED_BYTE_LIMIT);
        if !admitted
            || self.samples.try_reserve_exact(1).is_err()
            || self.time_deltas_us.try_reserve_exact(1).is_err()
        {
            self.record_drop();
            return false;
        }

        let delta = now
            .saturating_duration_since(self.last_sample_at)
            .as_micros()
            .max(1);
        let delta = u64::try_from(delta).unwrap_or(u64::MAX);
        self.last_sample_at = now;
        self.recorded_samples += 1;
        self.retained_bytes += sample_bytes;
        self.truncated_frames = self
            .truncated_frames
            .saturating_add(capture.truncated_frames);
        self.samples.push(capture.frames);
        self.time_deltas_us.push(delta);
        true
    }

    fn record_drop(&mut self) {
        self.dropped_samples = self.dropped_samples.saturating_add(1);
    }

    /// Hand back everything sampled so far and keep sampling.
    ///
    /// A long run can report several batches (module evaluation, then the
    /// drained event loop). Lifetime count/byte admission deliberately remains
    /// in this sampler after the batch is drained.
    #[must_use]
    pub(crate) fn drain(&mut self) -> CpuProfile {
        CpuProfile {
            interval: self.interval,
            samples: std::mem::take(&mut self.samples),
            time_deltas_us: std::mem::take(&mut self.time_deltas_us),
            dropped_samples: std::mem::take(&mut self.dropped_samples),
            truncated_frames: std::mem::take(&mut self.truncated_frames),
        }
    }

    /// Consume the sampler and return the final owned batch.
    #[must_use]
    pub(crate) fn finish(self) -> CpuProfile {
        CpuProfile {
            interval: self.interval,
            samples: self.samples,
            time_deltas_us: self.time_deltas_us,
            dropped_samples: self.dropped_samples,
            truncated_frames: self.truncated_frames,
        }
    }
}

#[derive(Debug)]
struct CapturedSample {
    frames: Vec<StackFrameSnapshot>,
    retained_bytes: usize,
    truncated_frames: u64,
}

fn capture_sample(
    context: &ExecutionContext,
    stack: &ActivationStack,
    byte_limit: usize,
) -> Result<CapturedSample, ()> {
    let frame_count = stack.len().min(CPU_PROFILE_MAX_FRAMES);
    let minimum_bytes = frame_count
        .checked_mul(std::mem::size_of::<StackFrameSnapshot>())
        .ok_or(())?;
    if minimum_bytes > byte_limit {
        return Err(());
    }
    let mut frames = Vec::new();
    frames.try_reserve_exact(frame_count).map_err(|_| ())?;
    let mut retained_bytes = frames
        .capacity()
        .checked_mul(std::mem::size_of::<StackFrameSnapshot>())
        .ok_or(())?;
    if retained_bytes > byte_limit {
        return Err(());
    }

    let mut failed = false;
    visit_frame_snapshots(context, stack, frame_count, |frame| {
        let Some(requested) = frame
            .function_name
            .len()
            .checked_add(frame.module.len())
            .and_then(|strings| retained_bytes.checked_add(strings))
        else {
            failed = true;
            return false;
        };
        if requested > byte_limit {
            failed = true;
            return false;
        }
        let Ok(function_name) = try_copy_string(frame.function_name) else {
            failed = true;
            return false;
        };
        let Ok(module) = try_copy_string(frame.module) else {
            failed = true;
            return false;
        };
        let Some(next_bytes) = retained_bytes
            .checked_add(function_name.capacity())
            .and_then(|bytes| bytes.checked_add(module.capacity()))
        else {
            failed = true;
            return false;
        };
        if next_bytes > byte_limit {
            failed = true;
            return false;
        }
        retained_bytes = next_bytes;
        frames.push(StackFrameSnapshot {
            function_id: frame.function_id,
            function_name,
            module,
            span: frame.span,
        });
        true
    });
    if failed || frames.len() != frame_count {
        return Err(());
    }
    Ok(CapturedSample {
        frames,
        retained_bytes,
        truncated_frames: u64::try_from(stack.len().saturating_sub(frame_count))
            .unwrap_or(u64::MAX),
    })
}

fn try_copy_string(value: &str) -> Result<String, std::collections::TryReserveError> {
    let mut owned = String::new();
    owned.try_reserve_exact(value.len())?;
    owned.push_str(value);
    Ok(owned)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn empty_capture() -> CapturedSample {
        CapturedSample {
            frames: Vec::new(),
            retained_bytes: 0,
            truncated_frames: 0,
        }
    }

    #[test]
    fn lifetime_sample_limit_survives_batch_drains() {
        let mut profiler = CpuProfiler::new(1);
        profiler.recorded_samples = CPU_PROFILE_SAMPLE_LIMIT - 1;
        assert!(profiler.try_record_sample(empty_capture(), std::time::Instant::now()));
        let accepted = profiler.drain();
        assert_eq!(accepted.sample_count(), 1);
        assert_eq!(accepted.dropped_samples, 0);

        assert!(!profiler.try_record_sample(empty_capture(), std::time::Instant::now()));
        let rejected = profiler.drain();
        assert_eq!(rejected.sample_count(), 0);
        assert_eq!(rejected.dropped_samples, 1);
    }

    #[test]
    fn lifetime_byte_limit_accepts_the_boundary_then_drops() {
        let mut profiler = CpuProfiler::new(1);
        profiler.retained_bytes =
            CPU_PROFILE_RETAINED_BYTE_LIMIT - CPU_PROFILE_SAMPLE_OVERHEAD_BYTES;
        assert!(profiler.try_record_sample(empty_capture(), std::time::Instant::now()));
        assert_eq!(profiler.retained_bytes, CPU_PROFILE_RETAINED_BYTE_LIMIT);
        assert!(!profiler.try_record_sample(empty_capture(), std::time::Instant::now()));
        assert_eq!(profiler.dropped_samples, 1);
    }

    #[test]
    fn oversized_single_sample_is_dropped_without_retention() {
        let mut profiler = CpuProfiler::new(1);
        let oversized = CapturedSample {
            frames: Vec::new(),
            retained_bytes: CPU_PROFILE_SAMPLE_BYTE_LIMIT,
            truncated_frames: 7,
        };
        assert!(!profiler.try_record_sample(oversized, std::time::Instant::now()));
        let profile = profiler.finish();
        assert!(profile.samples.is_empty());
        assert_eq!(profile.dropped_samples, 1);
        assert_eq!(profile.truncated_frames, 0);
    }
}
