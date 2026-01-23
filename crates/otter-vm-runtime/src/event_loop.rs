//! Event loop implementation

use crate::microtask::MicrotaskQueue;
use crate::timer::{Immediate, ImmediateId, Timer, TimerCallback, TimerHeapEntry, TimerId};
use parking_lot::Mutex;
use std::collections::{BinaryHeap, HashMap, HashSet, VecDeque};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// HTML5 spec: timers nested more than this level get clamped to MIN_TIMEOUT_MS
const MAX_TIMER_NESTING_LEVEL: u32 = 5;
/// HTML5 spec: minimum timeout for deeply nested timers (4ms)
const MIN_TIMEOUT_MS: u64 = 4;

thread_local! {
    /// Tracks timer nesting level for HTML5 spec compliance
    static TIMER_NESTING_LEVEL: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
}

/// Helper enum for extracting callbacks during timer execution
enum CallbackToRun {
    Once(Box<dyn FnOnce() + Send>),
    Repeating(Arc<dyn Fn() + Send + Sync>),
}

/// Event loop for executing async operations
pub struct EventLoop {
    /// Timer storage by ID for O(1) lookup
    timers: Mutex<HashMap<u64, Timer>>,
    /// Timer heap for O(log n) scheduling - min-heap ordered by deadline
    timer_heap: Mutex<BinaryHeap<TimerHeapEntry>>,
    /// Immediate queue (FIFO)
    immediates: Mutex<VecDeque<Immediate>>,
    /// Microtask queue
    microtasks: MicrotaskQueue,
    /// Next timer ID
    next_timer_id: AtomicU64,
    /// Next immediate ID
    next_immediate_id: AtomicU64,
    /// Is running
    running: AtomicBool,
    /// Tracks IDs of timers currently being executed (for clearInterval in callbacks)
    executing_timer_ids: Mutex<HashSet<u64>>,
    /// Tracks IDs of immediates currently being executed (for clearImmediate in callbacks)
    executing_immediate_ids: Mutex<HashSet<u64>>,
}

impl EventLoop {
    /// Create new event loop
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            timers: Mutex::new(HashMap::new()),
            timer_heap: Mutex::new(BinaryHeap::new()),
            immediates: Mutex::new(VecDeque::new()),
            microtasks: MicrotaskQueue::new(),
            next_timer_id: AtomicU64::new(1),
            next_immediate_id: AtomicU64::new(1),
            running: AtomicBool::new(false),
            executing_timer_ids: Mutex::new(HashSet::new()),
            executing_immediate_ids: Mutex::new(HashSet::new()),
        })
    }

    /// Schedule a one-shot timeout (setTimeout)
    pub fn set_timeout<F>(&self, callback: F, delay: Duration) -> TimerId
    where
        F: FnOnce() + Send + 'static,
    {
        self.schedule_timeout_internal(callback, delay, true)
    }

    /// Schedule a one-shot timeout with ref control
    pub fn schedule_timeout<F>(&self, callback: F, delay: Duration, refed: bool) -> TimerId
    where
        F: FnOnce() + Send + 'static,
    {
        self.schedule_timeout_internal(callback, delay, refed)
    }

    fn schedule_timeout_internal<F>(&self, callback: F, delay: Duration, refed: bool) -> TimerId
    where
        F: FnOnce() + Send + 'static,
    {
        let (id, deadline, nesting_level) = self.prepare_timer(delay);

        let timer = Timer {
            id,
            deadline,
            callback: TimerCallback::Once(Some(Box::new(callback))),
            interval: None,
            cancelled: Arc::new(AtomicBool::new(false)),
            refed: Arc::new(AtomicBool::new(refed)),
            nesting_level,
        };

        self.insert_timer(id, deadline, timer);
        id
    }

    /// Schedule a repeating interval (setInterval)
    pub fn set_interval<F>(&self, callback: F, interval: Duration) -> TimerId
    where
        F: Fn() + Send + Sync + 'static,
    {
        self.schedule_interval_internal(callback, interval, true)
    }

    /// Schedule a repeating interval with ref control
    pub fn schedule_interval<F>(&self, callback: F, interval: Duration, refed: bool) -> TimerId
    where
        F: Fn() + Send + Sync + 'static,
    {
        self.schedule_interval_internal(callback, interval, refed)
    }

    fn schedule_interval_internal<F>(&self, callback: F, interval: Duration, refed: bool) -> TimerId
    where
        F: Fn() + Send + Sync + 'static,
    {
        let (id, deadline, nesting_level) = self.prepare_timer(interval);

        let timer = Timer {
            id,
            deadline,
            callback: TimerCallback::Repeating(Arc::new(callback)),
            interval: Some(interval),
            cancelled: Arc::new(AtomicBool::new(false)),
            refed: Arc::new(AtomicBool::new(refed)),
            nesting_level,
        };

        self.insert_timer(id, deadline, timer);
        id
    }

    /// Prepare timer ID, deadline, and nesting level (shared logic)
    fn prepare_timer(&self, delay: Duration) -> (TimerId, Instant, u32) {
        let inherited_nesting = TIMER_NESTING_LEVEL.with(|level| level.get());
        let nesting_level = inherited_nesting.saturating_add(1);

        let clamped_delay = if nesting_level > MAX_TIMER_NESTING_LEVEL {
            delay.max(Duration::from_millis(MIN_TIMEOUT_MS))
        } else {
            delay
        };

        let id = TimerId(self.next_timer_id.fetch_add(1, Ordering::Relaxed));
        let deadline = Instant::now() + clamped_delay;

        (id, deadline, nesting_level)
    }

    /// Insert timer into storage and heap
    fn insert_timer(&self, id: TimerId, deadline: Instant, timer: Timer) {
        self.timers.lock().insert(id.0, timer);
        self.timer_heap
            .lock()
            .push(TimerHeapEntry { deadline, id: id.0 });
    }

    /// Cancel a timer by ID. Works even during callback execution.
    pub fn clear_timer(&self, id: TimerId) -> bool {
        // First check if timer is currently executing
        {
            let executing = self.executing_timer_ids.lock();
            if executing.contains(&id.0) {
                // Mark cancelled via the timer's flag
                if let Some(timer) = self.timers.lock().get(&id.0) {
                    timer.cancelled.store(true, Ordering::SeqCst);
                    return true;
                }
            }
        }

        // Then check the timer map - O(1) lookup
        let timers = self.timers.lock();
        if let Some(timer) = timers.get(&id.0) {
            timer.cancelled.store(true, Ordering::SeqCst);
            return true;
        }

        false
    }

    /// Cancel a timeout (alias for clear_timer)
    pub fn clear_timeout(&self, id: TimerId) {
        self.clear_timer(id);
    }

    /// Update whether a timer keeps the event loop alive
    pub fn set_timer_ref(&self, id: TimerId, refed: bool) -> bool {
        {
            let executing = self.executing_timer_ids.lock();
            if executing.contains(&id.0)
                && let Some(timer) = self.timers.lock().get(&id.0)
            {
                timer.refed.store(refed, Ordering::SeqCst);
                return true;
            }
        }

        let timers = self.timers.lock();
        if let Some(timer) = timers.get(&id.0) {
            timer.refed.store(refed, Ordering::SeqCst);
            return true;
        }

        false
    }

    /// Schedule an immediate callback (setImmediate)
    pub fn schedule_immediate<F>(&self, callback: F, refed: bool) -> ImmediateId
    where
        F: FnOnce() + Send + 'static,
    {
        let id = ImmediateId(self.next_immediate_id.fetch_add(1, Ordering::Relaxed));

        let immediate = Immediate {
            id,
            callback: Some(Box::new(callback)),
            cancelled: Arc::new(AtomicBool::new(false)),
            refed: Arc::new(AtomicBool::new(refed)),
        };

        self.immediates.lock().push_back(immediate);
        id
    }

    /// Cancel an immediate by ID
    pub fn clear_immediate(&self, id: ImmediateId) -> bool {
        {
            let executing = self.executing_immediate_ids.lock();
            if executing.contains(&id.0) {
                let immediates = self.immediates.lock();
                if let Some(imm) = immediates.iter().find(|i| i.id == id) {
                    imm.cancelled.store(true, Ordering::SeqCst);
                    return true;
                }
            }
        }

        let immediates = self.immediates.lock();
        if let Some(imm) = immediates.iter().find(|i| i.id == id) {
            imm.cancelled.store(true, Ordering::SeqCst);
            return true;
        }

        false
    }

    /// Update whether an immediate keeps the event loop alive
    pub fn set_immediate_ref(&self, id: ImmediateId, refed: bool) -> bool {
        {
            let executing = self.executing_immediate_ids.lock();
            if executing.contains(&id.0) {
                let immediates = self.immediates.lock();
                if let Some(imm) = immediates.iter().find(|i| i.id == id) {
                    imm.refed.store(refed, Ordering::SeqCst);
                    return true;
                }
            }
        }

        let immediates = self.immediates.lock();
        if let Some(imm) = immediates.iter().find(|i| i.id == id) {
            imm.refed.store(refed, Ordering::SeqCst);
            return true;
        }

        false
    }

    /// Queue a microtask
    pub fn queue_microtask<F>(&self, task: F)
    where
        F: FnOnce() + Send + 'static,
    {
        self.microtasks.enqueue(task);
    }

    /// Run the event loop until all tasks complete
    pub fn run_until_complete(&self) {
        self.running.store(true, Ordering::Release);

        while self.running.load(Ordering::Acquire) {
            // 1. Run all microtasks first (highest priority)
            self.drain_microtasks();

            // 2. Run ready timers
            self.run_timers();

            // 3. Run immediates
            self.run_immediates();

            // 4. Check if we should exit
            if !self.has_pending_tasks() {
                break;
            }

            // 5. Sleep until next timer if no immediate work
            if self.microtasks.is_empty()
                && self.immediates.lock().is_empty()
                && let Some(wait) = self.time_until_next_timer()
            {
                std::thread::sleep(wait.min(Duration::from_millis(10)));
            }
        }

        self.running.store(false, Ordering::Release);
    }

    /// Check if there are pending tasks that keep the loop alive
    pub fn has_pending_tasks(&self) -> bool {
        if !self.microtasks.is_empty() {
            return true;
        }

        // Only count non-cancelled, refed timers
        {
            let timers = self.timers.lock();
            if timers
                .values()
                .any(|t| !t.cancelled.load(Ordering::Relaxed) && t.refed.load(Ordering::Relaxed))
            {
                return true;
            }
        }

        // Only count non-cancelled, refed immediates
        {
            let immediates = self.immediates.lock();
            if immediates
                .iter()
                .any(|i| !i.cancelled.load(Ordering::Relaxed) && i.refed.load(Ordering::Relaxed))
            {
                return true;
            }
        }

        false
    }

    /// Drain all microtasks
    fn drain_microtasks(&self) {
        while let Some(task) = self.microtasks.dequeue() {
            task();
        }
    }

    /// Run all ready timers
    fn run_timers(&self) {
        let now = Instant::now();

        // Collect due timer IDs from heap
        let mut due_ids = Vec::new();
        {
            let mut heap = self.timer_heap.lock();
            let timers = self.timers.lock();

            while let Some(&entry) = heap.peek() {
                if entry.deadline > now {
                    break;
                }

                heap.pop();

                // Check if this is a valid, non-cancelled timer
                if let Some(timer) = timers.get(&entry.id)
                    && !timer.cancelled.load(Ordering::SeqCst)
                    && timer.deadline == entry.deadline
                {
                    due_ids.push(entry.id);
                }
            }
        }

        for timer_id in due_ids {
            // Extract callback and timer info
            let timer_info = {
                let mut timers = self.timers.lock();
                timers.get_mut(&timer_id).and_then(|t| {
                    if t.cancelled.load(Ordering::SeqCst) {
                        return None;
                    }
                    // Extract callback based on type
                    let callback = match &mut t.callback {
                        TimerCallback::Once(cb) => cb.take().map(CallbackToRun::Once),
                        TimerCallback::Repeating(cb) => Some(CallbackToRun::Repeating(cb.clone())),
                    };
                    Some((
                        callback,
                        t.interval,
                        t.nesting_level,
                        t.refed.clone(),
                        t.cancelled.clone(),
                    ))
                })
            };

            let Some((callback, interval, nesting_level, refed_flag, cancelled_flag)) = timer_info
            else {
                continue;
            };

            // Register as executing
            self.executing_timer_ids.lock().insert(timer_id);

            // Set nesting level and execute callback
            TIMER_NESTING_LEVEL.with(|level| level.set(nesting_level));
            if let Some(cb) = callback {
                match cb {
                    CallbackToRun::Once(f) => f(),
                    CallbackToRun::Repeating(f) => f(),
                }
            }
            TIMER_NESTING_LEVEL.with(|level| level.set(0));

            // Run microtasks after timer callback
            self.drain_microtasks();

            // Check if cancelled during execution
            let was_cancelled = cancelled_flag.load(Ordering::SeqCst);

            // Remove from executing
            self.executing_timer_ids.lock().remove(&timer_id);

            if was_cancelled {
                self.timers.lock().remove(&timer_id);
                continue;
            }

            // Handle reschedule for intervals
            if let Some(interval_duration) = interval {
                // Clamp interval for deeply nested timers
                let clamped_interval = if nesting_level > MAX_TIMER_NESTING_LEVEL {
                    interval_duration.max(Duration::from_millis(MIN_TIMEOUT_MS))
                } else {
                    interval_duration
                };

                let new_deadline = Instant::now() + clamped_interval;

                // Update timer deadline and re-add to heap
                let mut timers = self.timers.lock();
                if let Some(timer) = timers.get_mut(&timer_id) {
                    timer.deadline = new_deadline;
                    timer
                        .refed
                        .store(refed_flag.load(Ordering::SeqCst), Ordering::SeqCst);
                    timer.cancelled.store(false, Ordering::SeqCst);
                }

                self.timer_heap.lock().push(TimerHeapEntry {
                    deadline: new_deadline,
                    id: timer_id,
                });
            } else {
                // One-shot timer, remove it
                self.timers.lock().remove(&timer_id);
            }
        }

        // Cleanup cancelled timers
        let cancelled_ids: Vec<u64> = {
            let timers = self.timers.lock();
            timers
                .iter()
                .filter(|(_, t)| t.cancelled.load(Ordering::SeqCst))
                .map(|(&id, _)| id)
                .collect()
        };

        for id in cancelled_ids {
            self.timers.lock().remove(&id);
        }
    }

    /// Run all pending immediates
    fn run_immediates(&self) {
        // Collect IDs first
        let due_ids: Vec<u64> = {
            let queue = self.immediates.lock();
            queue.iter().map(|i| i.id.0).collect()
        };

        for immediate_id in due_ids {
            // Extract immediate info
            let immediate_info = {
                let mut queue = self.immediates.lock();
                let idx = queue.iter().position(|i| i.id.0 == immediate_id);
                idx.and_then(|i| {
                    let imm = queue.get_mut(i)?;
                    if imm.cancelled.load(Ordering::SeqCst) {
                        return None;
                    }
                    Some((imm.callback.take(), imm.cancelled.clone()))
                })
            };

            let Some((callback, cancelled_flag)) = immediate_info else {
                // Remove cancelled immediate
                let mut queue = self.immediates.lock();
                if let Some(idx) = queue.iter().position(|i| i.id.0 == immediate_id) {
                    queue.remove(idx);
                }
                continue;
            };

            // Register as executing
            self.executing_immediate_ids.lock().insert(immediate_id);

            // Execute callback
            if let Some(cb) = callback {
                cb();
            }

            // Run microtasks after immediate callback
            self.drain_microtasks();

            // Check if cancelled during execution
            let was_cancelled = cancelled_flag.load(Ordering::SeqCst);

            // Remove from executing
            self.executing_immediate_ids.lock().remove(&immediate_id);

            // Remove from queue (immediates don't repeat)
            let mut queue = self.immediates.lock();
            if let Some(idx) = queue.iter().position(|i| i.id.0 == immediate_id) {
                queue.remove(idx);
            }

            if was_cancelled {
                continue;
            }
        }
    }

    /// Get time until next timer
    fn time_until_next_timer(&self) -> Option<Duration> {
        let now = Instant::now();
        let timers = self.timers.lock();
        timers
            .values()
            .filter(|t| !t.cancelled.load(Ordering::Relaxed) && t.refed.load(Ordering::Relaxed))
            .map(|t| t.deadline.saturating_duration_since(now))
            .min()
    }

    /// Stop the event loop
    pub fn stop(&self) {
        self.running.store(false, Ordering::Release);
    }
}

impl Default for EventLoop {
    fn default() -> Self {
        // Note: new() returns Arc<Self>, but Default needs Self
        Self {
            timers: Mutex::new(HashMap::new()),
            timer_heap: Mutex::new(BinaryHeap::new()),
            immediates: Mutex::new(VecDeque::new()),
            microtasks: MicrotaskQueue::new(),
            next_timer_id: AtomicU64::new(1),
            next_immediate_id: AtomicU64::new(1),
            running: AtomicBool::new(false),
            executing_timer_ids: Mutex::new(HashSet::new()),
            executing_immediate_ids: Mutex::new(HashSet::new()),
        }
    }
}

impl EventLoop {
    /// Create new event loop wrapped in Arc (convenience method)
    pub fn new_arc() -> Arc<Self> {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    #[test]
    fn test_set_timeout() {
        let counter = Arc::new(AtomicUsize::new(0));
        let counter_clone = counter.clone();

        let event_loop = EventLoop::new();
        event_loop.set_timeout(
            move || {
                counter_clone.fetch_add(1, Ordering::Relaxed);
            },
            Duration::from_millis(10),
        );

        event_loop.run_until_complete();
        assert_eq!(counter.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn test_clear_timeout() {
        let counter = Arc::new(AtomicUsize::new(0));
        let counter_clone = counter.clone();

        let event_loop = EventLoop::new();
        let id = event_loop.set_timeout(
            move || {
                counter_clone.fetch_add(1, Ordering::Relaxed);
            },
            Duration::from_millis(100),
        );

        event_loop.clear_timeout(id);
        // Give it a moment to verify it doesn't fire
        std::thread::sleep(Duration::from_millis(150));

        assert_eq!(counter.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn test_microtask() {
        let counter = Arc::new(AtomicUsize::new(0));
        let counter_clone = counter.clone();

        let event_loop = EventLoop::new();
        event_loop.queue_microtask(move || {
            counter_clone.fetch_add(1, Ordering::Relaxed);
        });

        event_loop.run_until_complete();
        assert_eq!(counter.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn test_set_immediate() {
        let counter = Arc::new(AtomicUsize::new(0));
        let counter_clone = counter.clone();

        let event_loop = EventLoop::new();
        event_loop.schedule_immediate(
            move || {
                counter_clone.fetch_add(1, Ordering::Relaxed);
            },
            true,
        );

        event_loop.run_until_complete();
        assert_eq!(counter.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn test_clear_immediate() {
        let counter = Arc::new(AtomicUsize::new(0));
        let counter_clone = counter.clone();

        let event_loop = EventLoop::new();
        let id = event_loop.schedule_immediate(
            move || {
                counter_clone.fetch_add(1, Ordering::Relaxed);
            },
            true,
        );

        event_loop.clear_immediate(id);
        event_loop.run_until_complete();

        assert_eq!(counter.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn test_immediate_fires_after_timeout() {
        // Per Node.js semantics: timers phase runs before check (immediate) phase
        let order = Arc::new(Mutex::new(Vec::new()));

        let order1 = order.clone();
        let order2 = order.clone();

        let event_loop = EventLoop::new();

        // Schedule timeout first
        event_loop.set_timeout(
            move || {
                order1.lock().push("timeout");
            },
            Duration::from_millis(0),
        );

        // Schedule immediate second
        event_loop.schedule_immediate(
            move || {
                order2.lock().push("immediate");
            },
            true,
        );

        event_loop.run_until_complete();

        let result = order.lock();
        // Timers run before immediates (Node.js semantics)
        assert_eq!(*result, vec!["timeout", "immediate"]);
    }

    #[test]
    fn test_microtask_fires_before_immediate() {
        let order = Arc::new(Mutex::new(Vec::new()));

        let order1 = order.clone();
        let order2 = order.clone();

        let event_loop = EventLoop::new();

        // Schedule immediate first
        event_loop.schedule_immediate(
            move || {
                order1.lock().push("immediate");
            },
            true,
        );

        // Queue microtask second (should fire first)
        event_loop.queue_microtask(move || {
            order2.lock().push("microtask");
        });

        event_loop.run_until_complete();

        let result = order.lock();
        assert_eq!(*result, vec!["microtask", "immediate"]);
    }

    #[test]
    fn test_unrefed_timer_does_not_keep_loop_alive() {
        let counter = Arc::new(AtomicUsize::new(0));
        let counter_clone = counter.clone();

        let event_loop = EventLoop::new();

        // Schedule unrefed timer
        let id = event_loop.schedule_timeout(
            move || {
                counter_clone.fetch_add(1, Ordering::Relaxed);
            },
            Duration::from_millis(100),
            false, // unrefed
        );

        // Event loop should exit immediately since timer is unrefed
        event_loop.run_until_complete();

        // Timer should not have fired
        assert_eq!(counter.load(Ordering::Relaxed), 0);

        // Cleanup
        event_loop.clear_timer(id);
    }

    #[test]
    fn test_set_interval() {
        let counter = Arc::new(AtomicUsize::new(0));
        let event_loop = EventLoop::new();

        // Clone for interval callback
        let counter_interval = counter.clone();
        let event_loop_clone = event_loop.clone();

        // Set interval that stops itself after 3 fires
        let id_holder = Arc::new(Mutex::new(None::<TimerId>));
        let id_holder_clone = id_holder.clone();

        let id = event_loop.set_interval(
            move || {
                let count = counter_interval.fetch_add(1, Ordering::Relaxed) + 1;
                if count >= 3 {
                    // Stop after 3 fires
                    if let Some(id) = *id_holder_clone.lock() {
                        event_loop_clone.clear_timer(id);
                    }
                }
            },
            Duration::from_millis(5),
        );

        // Store ID so callback can clear it
        *id_holder.lock() = Some(id);

        // Run the event loop
        event_loop.run_until_complete();

        // Should have fired exactly 3 times
        assert_eq!(counter.load(Ordering::Relaxed), 3);
    }

    #[test]
    fn test_clear_interval() {
        let counter = Arc::new(AtomicUsize::new(0));
        let event_loop = EventLoop::new();

        let counter_interval = counter.clone();
        let event_loop_clone = event_loop.clone();
        let id_holder = Arc::new(Mutex::new(None::<TimerId>));
        let id_holder_clone = id_holder.clone();

        // Set interval that clears itself after first fire
        let id = event_loop.set_interval(
            move || {
                counter_interval.fetch_add(1, Ordering::Relaxed);
                // Clear immediately after first fire
                if let Some(id) = *id_holder_clone.lock() {
                    event_loop_clone.clear_timer(id);
                }
            },
            Duration::from_millis(5),
        );

        *id_holder.lock() = Some(id);

        event_loop.run_until_complete();

        // Should have fired exactly once (cleared after first fire)
        assert_eq!(counter.load(Ordering::Relaxed), 1);
    }
}
