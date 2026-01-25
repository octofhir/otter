//! # Otter Profiler
//!
//! CPU, memory, and async profiling for Otter VM.

#![warn(clippy::all)]
#![warn(missing_docs)]

pub mod async_trace;
pub mod cpu;
pub mod memory;
pub mod stats;

pub use async_trace::{AsyncSpan, AsyncTracer};
pub use cpu::{CpuProfile, CpuProfiler, StackFrame};
pub use memory::{
    HeapInfo, HeapInfoProvider, HeapSnapshot, HeapSnapshotDiff, MemoryProfiler, TypeStats,
};
pub use stats::{RuntimeStats, RuntimeStatsSnapshot};
