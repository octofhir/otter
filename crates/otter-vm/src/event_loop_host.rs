//! Event loop host interface — the contract between the VM and the embedder.
//!
//! The VM defines [`EventLoopHost`] as an abstraction over timer scheduling
//! and I/O polling. The default implementation ([`TokioEventLoop`] in
//! `event_loop.rs`) uses tokio and works everywhere. Embedders with special
//! needs (WASM, game engines) can provide their own implementation.

use std::time::Duration;

use crate::object::ObjectHandle;
use crate::value::RegisterValue;

/// Opaque timer identifier returned by `set_timeout` / `set_interval`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TimerId(pub u32);

/// What completed during an event loop poll cycle.
#[derive(Debug, Clone)]
pub enum CompletedEvent {
    /// A timer fired.
    Timer {
        id: TimerId,
        callback: ObjectHandle,
        this_value: RegisterValue,
    },
    // Future extensions:
    // IoReady { rid: u32, callback: ObjectHandle },
    // Signal { signum: i32, callback: ObjectHandle },
}

/// Event loop host interface.
///
/// The VM ships `TokioEventLoop` as the default. Override only for radically
/// different execution models (WASM, game loops, embedded systems).
///
/// # Contract
///
/// - `poll_next()` blocks (or yields in async context) until at least one
///   event completes. Returns empty vec when no pending work remains.
/// - After each callback execution, the caller (event loop driver) drains
///   the microtask queue.
/// - Timer scheduling follows HTML5 §8.6: nesting level > 5 → min 4ms interval.
pub trait EventLoopHost {
    /// Schedules a one-shot timer. Returns a timer ID for cancellation.
    fn set_timeout(
        &mut self,
        callback: ObjectHandle,
        this_value: RegisterValue,
        delay: Duration,
    ) -> TimerId;

    /// Schedules a repeating timer. Returns a timer ID for cancellation.
    fn set_interval(
        &mut self,
        callback: ObjectHandle,
        this_value: RegisterValue,
        interval: Duration,
    ) -> TimerId;

    /// Cancels a timer by ID. No-op if already fired or cancelled.
    fn clear_timer(&mut self, id: TimerId);

    /// Blocks until at least one pending event completes, then returns all
    /// completed events. Returns empty vec if no pending work.
    fn poll_next(&mut self) -> Vec<CompletedEvent>;

    /// Whether there are pending timers, I/O, or other async work.
    fn has_pending_work(&self) -> bool;
}

/// Minimum timer interval when nesting exceeds threshold (HTML5 spec §8.6).
pub const MIN_TIMER_INTERVAL: Duration = Duration::from_millis(4);

/// Maximum nesting level before the minimum interval clamp applies.
pub const MAX_TIMER_NESTING_BEFORE_CLAMP: u8 = 5;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timer_id_equality() {
        assert_eq!(TimerId(1), TimerId(1));
        assert_ne!(TimerId(1), TimerId(2));
    }

    #[test]
    fn completed_event_debug_format() {
        let event = CompletedEvent::Timer {
            id: TimerId(42),
            callback: ObjectHandle(7),
            this_value: RegisterValue::undefined(),
        };
        let debug = format!("{event:?}");
        assert!(debug.contains("Timer"));
        assert!(debug.contains("42"));
    }

    #[test]
    fn min_timer_interval_is_4ms() {
        assert_eq!(MIN_TIMER_INTERVAL, Duration::from_millis(4));
    }
}
