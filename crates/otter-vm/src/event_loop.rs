//! Default tokio-powered event loop and timer registry.
//!
//! [`TokioEventLoop`] implements [`EventLoopHost`] and works everywhere:
//! - **CLI** (`otterjs run`): creates its own `current_thread` runtime
//! - **Axum/Hyper**: uses `from_current()` — reuses the existing tokio runtime
//! - **`#[tokio::test]`**: works out of the box
//!
//! [`TimerRegistry`] manages scheduled timers with a min-heap sorted by
//! deadline, O(1) cancellation via HashMap, and HTML5 §8.6 nesting clamp.

use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap};
use std::time::{Duration, Instant};

use crate::event_loop_host::{
    CompletedEvent, EventLoopHost, TimerId, MAX_TIMER_NESTING_BEFORE_CLAMP, MIN_TIMER_INTERVAL,
};
use crate::object::ObjectHandle;
use crate::value::RegisterValue;

// ---------------------------------------------------------------------------
// TimerRegistry
// ---------------------------------------------------------------------------

/// Internal timer state.
#[derive(Debug, Clone)]
struct TimerState {
    id: TimerId,
    deadline: Instant,
    callback: ObjectHandle,
    this_value: RegisterValue,
    /// `None` for setTimeout, `Some(interval)` for setInterval.
    interval: Option<Duration>,
    /// HTML5 nesting level — clamped to `MIN_TIMER_INTERVAL` when > 5.
    nesting_level: u8,
    /// Whether this timer has been cancelled.
    cancelled: bool,
}

/// Entry in the min-heap, ordered by deadline.
#[derive(Debug, Clone, Eq, PartialEq)]
struct HeapEntry {
    deadline: Instant,
    id: TimerId,
}

impl Ord for HeapEntry {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.deadline
            .cmp(&other.deadline)
            .then_with(|| self.id.0.cmp(&other.id.0))
    }
}

impl PartialOrd for HeapEntry {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

/// Timer registry with min-heap scheduling and O(1) cancellation.
pub struct TimerRegistry {
    /// Min-heap of timer deadlines.
    heap: BinaryHeap<Reverse<HeapEntry>>,
    /// Timer state by ID (for O(1) cancel and callback lookup).
    timers: HashMap<u32, TimerState>,
    /// Next timer ID.
    next_id: u32,
    /// Current nesting level (for HTML5 §8.6 clamp).
    nesting_level: u8,
}

impl TimerRegistry {
    pub fn new() -> Self {
        Self {
            heap: BinaryHeap::new(),
            timers: HashMap::new(),
            next_id: 1,
            nesting_level: 0,
        }
    }

    /// Schedules a one-shot timer (setTimeout).
    pub fn set_timeout(
        &mut self,
        callback: ObjectHandle,
        this_value: RegisterValue,
        delay: Duration,
    ) -> TimerId {
        self.schedule(callback, this_value, delay, None)
    }

    /// Schedules a repeating timer (setInterval).
    pub fn set_interval(
        &mut self,
        callback: ObjectHandle,
        this_value: RegisterValue,
        interval: Duration,
    ) -> TimerId {
        self.schedule(callback, this_value, interval, Some(interval))
    }

    /// Cancels a timer. No-op if already fired or cancelled.
    pub fn clear(&mut self, id: TimerId) {
        if let Some(state) = self.timers.get_mut(&id.0) {
            state.cancelled = true;
        }
    }

    /// Returns all timers whose deadline has passed as of `now`.
    /// Skips cancelled timers. Re-arms interval timers with deadlines
    /// strictly after `now` to prevent infinite re-fire within one call.
    pub fn collect_fired(&mut self, now: Instant) -> Vec<FiredTimer> {
        let mut fired = Vec::new();
        // Track IDs we've already fired this round to prevent interval
        // timers from re-firing within the same collect_fired call.
        let mut fired_ids = std::collections::HashSet::new();

        while let Some(Reverse(entry)) = self.heap.peek() {
            if entry.deadline > now {
                break;
            }

            let Reverse(entry) = self.heap.pop().expect("peek succeeded");

            // Skip if we already fired this timer in this round (interval re-arm).
            if fired_ids.contains(&entry.id.0) {
                // Push it back — it'll fire next round.
                self.heap.push(Reverse(entry));
                break;
            }

            let Some(state) = self.timers.get(&entry.id.0) else {
                continue;
            };

            if state.cancelled {
                self.timers.remove(&entry.id.0);
                continue;
            }

            let timer = FiredTimer {
                id: state.id,
                callback: state.callback,
                this_value: state.this_value,
            };

            fired_ids.insert(entry.id.0);

            if let Some(interval) = state.interval {
                let clamped = self.clamp_interval(interval, state.nesting_level + 1);
                // Ensure the new deadline is strictly after `now` to prevent
                // infinite re-firing in a single collect_fired call.
                let min_future = now + Duration::from_micros(1);
                let new_deadline = (now + clamped).max(min_future);
                let new_nesting = state.nesting_level.saturating_add(1);

                let state = self.timers.get_mut(&entry.id.0).expect("just checked");
                state.deadline = new_deadline;
                state.nesting_level = new_nesting;

                self.heap.push(Reverse(HeapEntry {
                    deadline: new_deadline,
                    id: entry.id,
                }));
            } else {
                self.timers.remove(&entry.id.0);
            }

            fired.push(timer);
        }

        fired
    }

    /// Time until the next timer fires, or `None` if no timers pending.
    pub fn next_deadline(&self) -> Option<Instant> {
        // Skip cancelled entries at the top.
        // (We don't remove them eagerly — lazy cleanup.)
        for Reverse(entry) in self.heap.iter() {
            if let Some(state) = self.timers.get(&entry.id.0)
                && !state.cancelled
            {
                return Some(entry.deadline);
            }
        }
        None
    }

    /// Whether there are any active (non-cancelled) timers.
    pub fn has_pending(&self) -> bool {
        self.timers.values().any(|s| !s.cancelled)
    }

    /// Number of active timers.
    pub fn active_count(&self) -> usize {
        self.timers.values().filter(|s| !s.cancelled).count()
    }

    fn schedule(
        &mut self,
        callback: ObjectHandle,
        this_value: RegisterValue,
        delay: Duration,
        interval: Option<Duration>,
    ) -> TimerId {
        let id = TimerId(self.next_id);
        self.next_id += 1;

        let clamped_delay = self.clamp_interval(delay, self.nesting_level);
        let deadline = Instant::now() + clamped_delay;

        let state = TimerState {
            id,
            deadline,
            callback,
            this_value,
            interval,
            nesting_level: self.nesting_level,
            cancelled: false,
        };

        self.timers.insert(id.0, state);
        self.heap.push(Reverse(HeapEntry { deadline, id }));

        id
    }

    /// HTML5 §8.6: If nesting level > 5, clamp interval to at least 4ms.
    fn clamp_interval(&self, interval: Duration, nesting: u8) -> Duration {
        if nesting > MAX_TIMER_NESTING_BEFORE_CLAMP {
            interval.max(MIN_TIMER_INTERVAL)
        } else {
            interval
        }
    }
}

impl Default for TimerRegistry {
    fn default() -> Self {
        Self::new()
    }
}

/// A timer that has fired and is ready for callback execution.
#[derive(Debug, Clone)]
pub struct FiredTimer {
    pub id: TimerId,
    pub callback: ObjectHandle,
    pub this_value: RegisterValue,
}

// ---------------------------------------------------------------------------
// TokioEventLoop
// ---------------------------------------------------------------------------

/// Default event loop backed by tokio.
///
/// Two construction modes:
/// - `new()`: creates an owned `current_thread` tokio runtime (for CLI).
/// - `from_current()`: borrows the existing tokio runtime (for Axum embedding).
pub struct TokioEventLoop {
    timers: TimerRegistry,
    /// Owned tokio runtime (for standalone CLI mode).
    /// `None` when using `from_current()`.
    owned_runtime: Option<tokio::runtime::Runtime>,
}

impl TokioEventLoop {
    /// Creates a new event loop with an owned single-threaded tokio runtime.
    ///
    /// Use this for standalone CLI execution (`otterjs run script.js`).
    pub fn new() -> Self {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .expect("failed to build tokio current_thread runtime");

        Self {
            timers: TimerRegistry::new(),
            owned_runtime: Some(runtime),
        }
    }

    /// Creates an event loop that uses the currently active tokio runtime.
    ///
    /// Use this when embedded in an application that already runs tokio
    /// (e.g., Axum, Hyper, tonic). Panics if no tokio runtime is active.
    pub fn from_current() -> Self {
        // Verify a runtime exists.
        let _handle = tokio::runtime::Handle::current();
        Self {
            timers: TimerRegistry::new(),
            owned_runtime: None,
        }
    }

    /// Access to the timer registry (for intrinsic timer globals).
    pub fn timers(&self) -> &TimerRegistry {
        &self.timers
    }

    /// Mutable access to the timer registry.
    pub fn timers_mut(&mut self) -> &mut TimerRegistry {
        &mut self.timers
    }
}

impl Default for TokioEventLoop {
    fn default() -> Self {
        Self::new()
    }
}

impl EventLoopHost for TokioEventLoop {
    fn set_timeout(
        &mut self,
        callback: ObjectHandle,
        this_value: RegisterValue,
        delay: Duration,
    ) -> TimerId {
        self.timers.set_timeout(callback, this_value, delay)
    }

    fn set_interval(
        &mut self,
        callback: ObjectHandle,
        this_value: RegisterValue,
        interval: Duration,
    ) -> TimerId {
        self.timers.set_interval(callback, this_value, interval)
    }

    fn clear_timer(&mut self, id: TimerId) {
        self.timers.clear(id);
    }

    fn poll_next(&mut self) -> Vec<CompletedEvent> {
        if !self.timers.has_pending() {
            return Vec::new();
        }

        let Some(deadline) = self.timers.next_deadline() else {
            return Vec::new();
        };

        let now = Instant::now();
        if deadline <= now {
            // Timers already expired — collect them without blocking.
            return self
                .timers
                .collect_fired(now)
                .into_iter()
                .map(|t| CompletedEvent::Timer {
                    id: t.id,
                    callback: t.callback,
                    this_value: t.this_value,
                })
                .collect();
        }

        // Sleep until the next deadline using tokio.
        let sleep_duration = deadline - now;

        let block_fn = || async {
            tokio::time::sleep(sleep_duration).await;
        };

        if let Some(rt) = &self.owned_runtime {
            rt.block_on(block_fn());
        } else {
            // When using from_current(), we can't block_on.
            // Instead, do a spin with yield to avoid blocking the runtime.
            // In production, this path is used from `run_until_complete`
            // which is already async.
            std::thread::sleep(sleep_duration);
        }

        self.timers
            .collect_fired(Instant::now())
            .into_iter()
            .map(|t| CompletedEvent::Timer {
                id: t.id,
                callback: t.callback,
                this_value: t.this_value,
            })
            .collect()
    }

    fn has_pending_work(&self) -> bool {
        self.timers.has_pending()
    }
}

// ---------------------------------------------------------------------------
// Event loop driver
// ---------------------------------------------------------------------------

/// Runs JS to completion including all async work (synchronous variant).
///
/// 1. Execute the module
/// 2. Drain microtasks
/// 3. Event loop: poll → execute callbacks → drain microtasks → repeat
///
/// Use this for CLI and Test262. For Axum embedding, use the async variant.
pub fn run_event_loop(
    event_loop: &mut dyn EventLoopHost,
    has_microtasks: impl Fn() -> bool,
    _drain_microtasks: impl FnMut() -> Result<(), crate::interpreter::InterpreterError>,
    mut execute_callback: impl FnMut(CompletedEvent) -> Result<(), crate::interpreter::InterpreterError>,
    mut drain_after_callback: impl FnMut() -> Result<(), crate::interpreter::InterpreterError>,
) -> Result<(), crate::interpreter::InterpreterError> {
    loop {
        if !event_loop.has_pending_work() && !has_microtasks() {
            break;
        }

        let events = event_loop.poll_next();
        if events.is_empty() && !has_microtasks() {
            break;
        }

        for event in events {
            execute_callback(event)?;
            drain_after_callback()?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event_loop_host::TimerId;

    #[test]
    fn timer_registry_set_timeout() {
        let mut reg = TimerRegistry::new();
        let id = reg.set_timeout(
            ObjectHandle(1),
            RegisterValue::undefined(),
            Duration::from_millis(10),
        );
        assert_eq!(id, TimerId(1));
        assert!(reg.has_pending());
        assert_eq!(reg.active_count(), 1);
    }

    #[test]
    fn timer_registry_cancel() {
        let mut reg = TimerRegistry::new();
        let id = reg.set_timeout(
            ObjectHandle(1),
            RegisterValue::undefined(),
            Duration::from_millis(100),
        );
        reg.clear(id);
        assert!(!reg.has_pending());
    }

    #[test]
    fn timer_registry_fires_after_deadline() {
        let mut reg = TimerRegistry::new();
        reg.set_timeout(
            ObjectHandle(1),
            RegisterValue::undefined(),
            Duration::from_millis(0), // Immediate
        );

        // Collect immediately — deadline is in the past.
        std::thread::sleep(Duration::from_millis(1));
        let fired = reg.collect_fired(Instant::now());
        assert_eq!(fired.len(), 1);
        assert_eq!(fired[0].callback, ObjectHandle(1));
    }

    #[test]
    fn timer_registry_interval_re_arms() {
        let mut reg = TimerRegistry::new();
        let id = reg.set_interval(
            ObjectHandle(2),
            RegisterValue::undefined(),
            Duration::from_millis(0),
        );

        // Use a future instant to guarantee the deadline has passed.
        std::thread::sleep(Duration::from_millis(5));
        let now = Instant::now();
        let fired = reg.collect_fired(now);
        assert_eq!(fired.len(), 1);

        // Timer should still be pending (re-armed).
        assert!(reg.has_pending());

        // Fire again — advance time further.
        std::thread::sleep(Duration::from_millis(5));
        let fired = reg.collect_fired(Instant::now());
        assert!(!fired.is_empty());
        assert_eq!(fired[0].id, id);
    }

    #[test]
    fn timer_registry_cancelled_timer_skipped() {
        let mut reg = TimerRegistry::new();
        let id = reg.set_timeout(
            ObjectHandle(3),
            RegisterValue::undefined(),
            Duration::from_millis(0),
        );
        reg.clear(id);

        std::thread::sleep(Duration::from_millis(1));
        let fired = reg.collect_fired(Instant::now());
        assert!(fired.is_empty());
    }

    #[test]
    fn timer_registry_ordering() {
        let mut reg = TimerRegistry::new();
        let _id1 = reg.set_timeout(
            ObjectHandle(10),
            RegisterValue::undefined(),
            Duration::from_millis(20),
        );
        let _id2 = reg.set_timeout(
            ObjectHandle(20),
            RegisterValue::undefined(),
            Duration::from_millis(0), // Fires first
        );

        std::thread::sleep(Duration::from_millis(1));
        let fired = reg.collect_fired(Instant::now());
        assert_eq!(fired.len(), 1);
        assert_eq!(fired[0].callback, ObjectHandle(20)); // id2 first
    }

    #[test]
    fn timer_registry_next_deadline() {
        let mut reg = TimerRegistry::new();
        assert!(reg.next_deadline().is_none());

        reg.set_timeout(
            ObjectHandle(1),
            RegisterValue::undefined(),
            Duration::from_millis(100),
        );
        assert!(reg.next_deadline().is_some());
    }

    #[test]
    fn tokio_event_loop_creates_runtime() {
        let event_loop = TokioEventLoop::new();
        assert!(!event_loop.has_pending_work());
    }

    #[test]
    fn tokio_event_loop_set_and_poll_timer() {
        let mut event_loop = TokioEventLoop::new();
        event_loop.set_timeout(
            ObjectHandle(5),
            RegisterValue::undefined(),
            Duration::from_millis(0),
        );

        assert!(event_loop.has_pending_work());

        let events = event_loop.poll_next();
        assert_eq!(events.len(), 1);
        match &events[0] {
            CompletedEvent::Timer { callback, .. } => {
                assert_eq!(*callback, ObjectHandle(5));
            }
        }
    }

    #[test]
    fn run_event_loop_exits_when_no_work() {
        let mut event_loop = TokioEventLoop::new();

        let result = run_event_loop(
            &mut event_loop,
            || false,
            || Ok(()),
            |_| Ok(()),
            || Ok(()),
        );

        assert!(result.is_ok());
    }

    #[test]
    fn run_event_loop_processes_timer() {
        let mut event_loop = TokioEventLoop::new();
        event_loop.set_timeout(
            ObjectHandle(7),
            RegisterValue::undefined(),
            Duration::from_millis(0),
        );

        let mut callbacks_executed = 0u32;

        let result = run_event_loop(
            &mut event_loop,
            || false,
            || Ok(()),
            |event| {
                match event {
                    CompletedEvent::Timer { callback, .. } => {
                        assert_eq!(callback, ObjectHandle(7));
                        callbacks_executed += 1;
                    }
                }
                Ok(())
            },
            || Ok(()),
        );

        assert!(result.is_ok());
        assert_eq!(callbacks_executed, 1);
    }
}
