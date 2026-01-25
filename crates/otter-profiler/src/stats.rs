//! Runtime statistics collection
//!
//! Provides atomic counters for real-time performance monitoring.

use serde::Serialize;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

/// Runtime statistics - atomic counters for thread-safe access
pub struct RuntimeStats {
    /// Total instructions executed
    pub instructions_executed: AtomicU64,
    /// Total allocations count
    pub allocations: AtomicU64,
    /// Total bytes allocated
    pub allocation_bytes: AtomicU64,
    /// GC collection count
    pub gc_collections: AtomicU64,
    /// Total GC pause time in nanoseconds
    pub gc_pause_time_ns: AtomicU64,
    /// Start time
    start_time: Instant,
}

/// Snapshot of runtime stats (for reporting)
#[derive(Debug, Clone, Serialize)]
pub struct RuntimeStatsSnapshot {
    /// Duration since profiling started (microseconds)
    pub duration_us: u64,
    /// Total instructions executed
    pub instructions_executed: u64,
    /// Total allocations count
    pub allocations: u64,
    /// Total bytes allocated
    pub allocation_bytes: u64,
    /// GC collection count
    pub gc_collections: u64,
    /// Total GC pause time (microseconds)
    pub gc_pause_time_us: u64,
    /// Instructions per second
    pub instructions_per_sec: f64,
    /// Allocations per second
    pub allocations_per_sec: f64,
    /// Average GC pause (microseconds)
    pub avg_gc_pause_us: f64,
}

impl RuntimeStats {
    /// Create new stats counter
    pub fn new() -> Self {
        Self {
            instructions_executed: AtomicU64::new(0),
            allocations: AtomicU64::new(0),
            allocation_bytes: AtomicU64::new(0),
            gc_collections: AtomicU64::new(0),
            gc_pause_time_ns: AtomicU64::new(0),
            start_time: Instant::now(),
        }
    }

    /// Record an instruction execution
    #[inline]
    pub fn record_instruction(&self) {
        self.instructions_executed.fetch_add(1, Ordering::Relaxed);
    }

    /// Record an allocation
    #[inline]
    pub fn record_allocation(&self, bytes: usize) {
        self.allocations.fetch_add(1, Ordering::Relaxed);
        self.allocation_bytes
            .fetch_add(bytes as u64, Ordering::Relaxed);
    }

    /// Record a GC collection
    #[inline]
    pub fn record_gc(&self, pause_ns: u64) {
        self.gc_collections.fetch_add(1, Ordering::Relaxed);
        self.gc_pause_time_ns.fetch_add(pause_ns, Ordering::Relaxed);
    }

    /// Take a snapshot of current stats
    pub fn snapshot(&self) -> RuntimeStatsSnapshot {
        let duration = self.start_time.elapsed();
        let duration_us = duration.as_micros() as u64;
        let duration_secs = duration.as_secs_f64();

        let instructions = self.instructions_executed.load(Ordering::Relaxed);
        let allocations = self.allocations.load(Ordering::Relaxed);
        let allocation_bytes = self.allocation_bytes.load(Ordering::Relaxed);
        let gc_collections = self.gc_collections.load(Ordering::Relaxed);
        let gc_pause_time_ns = self.gc_pause_time_ns.load(Ordering::Relaxed);

        RuntimeStatsSnapshot {
            duration_us,
            instructions_executed: instructions,
            allocations,
            allocation_bytes,
            gc_collections,
            gc_pause_time_us: gc_pause_time_ns / 1000,
            instructions_per_sec: if duration_secs > 0.0 {
                instructions as f64 / duration_secs
            } else {
                0.0
            },
            allocations_per_sec: if duration_secs > 0.0 {
                allocations as f64 / duration_secs
            } else {
                0.0
            },
            avg_gc_pause_us: if gc_collections > 0 {
                (gc_pause_time_ns / 1000) as f64 / gc_collections as f64
            } else {
                0.0
            },
        }
    }

    /// Reset all counters
    pub fn reset(&self) {
        self.instructions_executed.store(0, Ordering::Relaxed);
        self.allocations.store(0, Ordering::Relaxed);
        self.allocation_bytes.store(0, Ordering::Relaxed);
        self.gc_collections.store(0, Ordering::Relaxed);
        self.gc_pause_time_ns.store(0, Ordering::Relaxed);
    }
}

impl Default for RuntimeStats {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_instruction_counting() {
        let stats = RuntimeStats::new();
        stats.record_instruction();
        stats.record_instruction();

        let snapshot = stats.snapshot();
        assert_eq!(snapshot.instructions_executed, 2);
    }

    #[test]
    fn test_allocation_tracking() {
        let stats = RuntimeStats::new();
        stats.record_allocation(100);
        stats.record_allocation(250);

        let snapshot = stats.snapshot();
        assert_eq!(snapshot.allocations, 2);
        assert_eq!(snapshot.allocation_bytes, 350);
    }

    #[test]
    fn test_gc_tracking() {
        let stats = RuntimeStats::new();
        stats.record_gc(1_000_000); // 1ms
        stats.record_gc(2_000_000); // 2ms

        let snapshot = stats.snapshot();
        assert_eq!(snapshot.gc_collections, 2);
        assert_eq!(snapshot.gc_pause_time_us, 3000);
        assert_eq!(snapshot.avg_gc_pause_us, 1500.0);
    }

    #[test]
    fn test_reset() {
        let stats = RuntimeStats::new();
        stats.record_instruction();
        stats.record_allocation(100);
        stats.reset();

        let snapshot = stats.snapshot();
        assert_eq!(snapshot.instructions_executed, 0);
        assert_eq!(snapshot.allocations, 0);
    }
}
