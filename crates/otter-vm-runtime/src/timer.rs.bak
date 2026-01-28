//! Timer and Immediate implementations

use std::cmp::Ordering;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::{Duration, Instant};

/// Timer identifier
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TimerId(pub u64);

/// Immediate identifier
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ImmediateId(pub u64);

/// Callback type for timers
pub enum TimerCallback {
    /// One-shot callback (setTimeout)
    Once(Option<Box<dyn FnOnce() + Send>>),
    /// Repeating callback (setInterval)
    Repeating(Arc<dyn Fn() + Send + Sync>),
}

/// A scheduled timer (setTimeout/setInterval)
pub struct Timer {
    /// Unique ID
    pub id: TimerId,
    /// When to fire (absolute time)
    pub deadline: Instant,
    /// Callback to execute
    pub callback: TimerCallback,
    /// Interval for repeating timers (setInterval), None for setTimeout
    pub interval: Option<Duration>,
    /// Cancellation flag
    pub cancelled: Arc<AtomicBool>,
    /// Whether this timer keeps the event loop alive
    pub refed: Arc<AtomicBool>,
    /// HTML5 spec: timer nesting level
    pub nesting_level: u32,
}

/// Entry in the timer heap for O(log n) scheduling.
/// Uses reversed ordering for min-heap semantics (earliest deadline first).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TimerHeapEntry {
    /// When to fire
    pub deadline: Instant,
    /// Timer ID for lookup in HashMap
    pub id: u64,
}

impl Ord for TimerHeapEntry {
    fn cmp(&self, other: &Self) -> Ordering {
        // Reverse ordering: smaller deadline = higher priority (min-heap)
        // Break ties by ID (lower ID = higher priority for FIFO)
        other
            .deadline
            .cmp(&self.deadline)
            .then_with(|| other.id.cmp(&self.id))
    }
}

impl PartialOrd for TimerHeapEntry {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// A scheduled immediate (setImmediate)
pub struct Immediate {
    /// Unique ID
    pub id: ImmediateId,
    /// Callback to execute
    pub callback: Option<Box<dyn FnOnce() + Send>>,
    /// Cancellation flag
    pub cancelled: Arc<AtomicBool>,
    /// Whether this immediate keeps the event loop alive
    pub refed: Arc<AtomicBool>,
}
