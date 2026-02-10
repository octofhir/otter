//! Event loop implementation
//!
//! Async-only event loop with tokio integration for HTTP server support.

use crate::microtask::{JsJobQueue, MicrotaskQueue, MicrotaskSequencer};
use crate::timer::{Immediate, ImmediateId, Timer, TimerCallback, TimerHeapEntry, TimerId};
use parking_lot::Mutex;
use serde_json::Value as JsonValue;
use std::collections::{BinaryHeap, HashMap, HashSet, VecDeque};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

/// HTTP event for server request dispatch
#[derive(Debug, Clone)]
pub struct HttpEvent {
    /// Server ID that received the request
    pub server_id: u64,
    /// Request ID for this specific request
    pub request_id: u64,
}

/// WebSocket event for server dispatch
#[derive(Debug, Clone)]
pub enum WsEvent {
    Open {
        server_id: u64,
        socket_id: u64,
        data: Option<JsonValue>,
        remote_addr: Option<String>,
    },
    Message {
        server_id: u64,
        socket_id: u64,
        data: Vec<u8>,
        is_text: bool,
    },
    Close {
        server_id: u64,
        socket_id: u64,
        code: u16,
        reason: String,
    },
    Drain {
        server_id: u64,
        socket_id: u64,
    },
    Ping {
        server_id: u64,
        socket_id: u64,
        data: Vec<u8>,
    },
    Pong {
        server_id: u64,
        socket_id: u64,
        data: Vec<u8>,
    },
    Error {
        server_id: u64,
        socket_id: u64,
        message: String,
    },
}

/// Active server count - shared between event loop and HTTP extension
pub type ActiveServerCount = Arc<AtomicU64>;

/// HTTP event dispatcher callback type
pub type HttpDispatcher = Box<dyn Fn(u64, u64) + Send + Sync>;
/// WebSocket event dispatcher callback type
pub type WsDispatcher = Box<dyn Fn(WsEvent) + Send + Sync>;

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
    /// Microtask queue (Rust closures)
    microtasks: MicrotaskQueue,
    /// JS callback job queue (JavaScript functions)
    js_jobs: Arc<JsJobQueue>,
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

    // === HTTP support fields ===
    /// HTTP events receiver (from HTTP server)
    http_events_rx: Mutex<Option<mpsc::UnboundedReceiver<HttpEvent>>>,
    /// Active HTTP servers count
    active_http_servers: Arc<AtomicU64>,
    /// HTTP event dispatcher callback
    http_dispatcher: Mutex<Option<HttpDispatcher>>,
    /// WebSocket events receiver (from WS server)
    ws_events_rx: Mutex<Option<mpsc::UnboundedReceiver<WsEvent>>>,
    /// WebSocket event dispatcher callback
    ws_dispatcher: Mutex<Option<WsDispatcher>>,

    // === Async operations support ===
    /// Pending async operations count
    pending_async_ops: Arc<AtomicU64>,
    /// Handles for spawned async tasks (for cancellation if needed)
    async_task_handles: Mutex<Vec<JoinHandle<()>>>,
}

impl EventLoop {
    /// Create a new event loop
    pub fn new() -> Arc<Self> {
        let sequencer = MicrotaskSequencer::new();
        Arc::new(Self {
            timers: Mutex::new(HashMap::new()),
            timer_heap: Mutex::new(BinaryHeap::new()),
            immediates: Mutex::new(VecDeque::new()),
            microtasks: MicrotaskQueue::with_sequencer(sequencer.clone()),
            js_jobs: Arc::new(JsJobQueue::with_sequencer(sequencer)),
            next_timer_id: AtomicU64::new(1),
            next_immediate_id: AtomicU64::new(1),
            running: AtomicBool::new(false),
            executing_timer_ids: Mutex::new(HashSet::new()),
            executing_immediate_ids: Mutex::new(HashSet::new()),
            // HTTP support
            http_events_rx: Mutex::new(None),
            active_http_servers: Arc::new(AtomicU64::new(0)),
            http_dispatcher: Mutex::new(None),
            ws_events_rx: Mutex::new(None),
            ws_dispatcher: Mutex::new(None),
            // Async ops support
            pending_async_ops: Arc::new(AtomicU64::new(0)),
            async_task_handles: Mutex::new(Vec::new()),
        })
    }

    /// Get the pending async ops counter (for sharing with async op handlers)
    pub fn get_pending_async_ops_count(&self) -> Arc<AtomicU64> {
        Arc::clone(&self.pending_async_ops)
    }

    /// Check if there are pending async operations
    pub fn has_pending_async_ops(&self) -> bool {
        self.pending_async_ops.load(Ordering::Relaxed) > 0
    }

    /// Register a spawned async task handle
    pub fn register_async_task(&self, handle: JoinHandle<()>) {
        self.async_task_handles.lock().push(handle);
    }

    /// Clean up completed task handles
    fn cleanup_completed_tasks(&self) {
        let mut handles = self.async_task_handles.lock();
        handles.retain(|h| !h.is_finished());
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
                // Mark canceled via the timer's flag
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

    /// Get access to the microtask queue
    ///
    /// This allows external code to enqueue microtasks for promise callbacks.
    pub fn microtask_queue(&self) -> &MicrotaskQueue {
        &self.microtasks
    }

    /// Get access to the JS job queue
    ///
    /// This allows Promise callbacks to enqueue JavaScript function calls.
    pub fn js_job_queue(&self) -> &Arc<JsJobQueue> {
        &self.js_jobs
    }

    /// Check if there are pending tasks that keep the loop alive
    pub fn has_pending_tasks(&self) -> bool {
        if !self.microtasks.is_empty() || !self.js_jobs.is_empty() {
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

        // Collect due timer IDs from a heap
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

    // === HTTP support methods ===

    /// Set the HTTP events receiver channel
    pub fn set_http_receiver(&self, rx: mpsc::UnboundedReceiver<HttpEvent>) {
        *self.http_events_rx.lock() = Some(rx);
    }

    /// Set the HTTP event dispatcher callback
    ///
    /// The callback receives (server_id, request_id) and should call the JS handler
    pub fn set_http_dispatcher<F>(&self, dispatcher: F)
    where
        F: Fn(u64, u64) + Send + Sync + 'static,
    {
        *self.http_dispatcher.lock() = Some(Box::new(dispatcher));
    }

    /// Set the WebSocket events receiver channel
    pub fn set_ws_receiver(&self, rx: mpsc::UnboundedReceiver<WsEvent>) {
        *self.ws_events_rx.lock() = Some(rx);
    }

    /// Set the WebSocket event dispatcher callback
    pub fn set_ws_dispatcher<F>(&self, dispatcher: F)
    where
        F: Fn(WsEvent) + Send + Sync + 'static,
    {
        *self.ws_dispatcher.lock() = Some(Box::new(dispatcher));
    }

    /// Get the active HTTP servers counter
    ///
    /// This is shared with the HTTP extension to track when servers start/stop
    pub fn get_active_server_count(&self) -> ActiveServerCount {
        Arc::clone(&self.active_http_servers)
    }

    /// Check if there are active HTTP servers
    pub fn has_active_http_servers(&self) -> bool {
        self.active_http_servers.load(Ordering::Relaxed) > 0
    }

    /// Poll HTTP events and dispatch them
    ///
    /// Collects all pending events first, then dispatches them to avoid
    /// holding locks during JS execution.
    fn poll_http_events(&self) {
        // Collect pending events without holding lock during dispatch
        let events: Vec<HttpEvent> = {
            let mut rx_guard = self.http_events_rx.lock();
            if let Some(rx) = rx_guard.as_mut() {
                let mut events = Vec::with_capacity(16); // Pre-allocate for common case
                while let Ok(event) = rx.try_recv() {
                    events.push(event);
                }
                events
            } else {
                return;
            }
        };

        if events.is_empty() {
            return;
        }

        // Dispatch each event and run microtasks after
        for event in events {
            // Get dispatcher for each event (allows it to be changed during execution)
            let dispatcher = self.http_dispatcher.lock();
            if let Some(ref dispatch_fn) = *dispatcher {
                dispatch_fn(event.server_id, event.request_id);
            } else {
                break;
            }
            // Drop lock before running microtasks
            drop(dispatcher);
            // Run microtasks after each dispatch for proper Promise resolution
            self.drain_microtasks();
        }
    }

    /// Poll WebSocket events and dispatch them
    fn poll_ws_events(&self) {
        let events: Vec<WsEvent> = {
            let mut rx_guard = self.ws_events_rx.lock();
            if let Some(rx) = rx_guard.as_mut() {
                let mut events = Vec::with_capacity(16);
                while let Ok(event) = rx.try_recv() {
                    events.push(event);
                }
                events
            } else {
                return;
            }
        };

        if events.is_empty() {
            return;
        }

        for event in events {
            let dispatcher = self.ws_dispatcher.lock();
            if let Some(ref dispatch_fn) = *dispatcher {
                dispatch_fn(event);
            } else {
                break;
            }
            drop(dispatcher);
            self.drain_microtasks();
        }
    }

    /// Take all pending HTTP events (for external dispatch)
    ///
    /// Returns collected events and clears the queue. This is used when
    /// the runtime needs to dispatch events with access to VmContext.
    pub fn take_http_events(&self) -> Vec<HttpEvent> {
        let mut rx_guard = self.http_events_rx.lock();
        if let Some(rx) = rx_guard.as_mut() {
            let mut events = Vec::with_capacity(16);
            while let Ok(event) = rx.try_recv() {
                events.push(event);
            }
            events
        } else {
            Vec::new()
        }
    }

    /// Take all pending WebSocket events
    pub fn take_ws_events(&self) -> Vec<WsEvent> {
        let mut rx_guard = self.ws_events_rx.lock();
        if let Some(rx) = rx_guard.as_mut() {
            let mut events = Vec::with_capacity(16);
            while let Ok(event) = rx.try_recv() {
                events.push(event);
            }
            events
        } else {
            Vec::new()
        }
    }

    /// Run the event loop asynchronously with tokio
    ///
    /// This version supports HTTP server events and integrates with tokio for I/O.
    /// Use this when running HTTP servers or other async operations.
    pub async fn run_until_complete_async(&self) {
        self.running.store(true, Ordering::Release);

        loop {
            if !self.running.load(Ordering::Acquire) {
                break;
            }

            // 1. Poll HTTP events (non-blocking)
            self.poll_http_events();
            // 1b. Poll WebSocket events (non-blocking)
            self.poll_ws_events();

            // 2. Run all microtasks first (highest priority)
            self.drain_microtasks();

            // 3. Run ready timers
            self.run_timers();

            // 4. Run immediates
            self.run_immediates();

            // 5. Clean up completed async task handles
            self.cleanup_completed_tasks();

            // 6. Check if we should exit
            let has_tasks = self.has_pending_tasks();
            let has_servers = self.has_active_http_servers();
            let has_async_ops = self.has_pending_async_ops();

            if !has_tasks && !has_servers && !has_async_ops {
                break;
            }

            // 6. Yield to tokio for I/O operations
            // This allows HTTP server tasks and other async ops to progress
            tokio::task::yield_now().await;

            // 7. Small sleep if nothing is immediately ready to prevent busy-loop
            if self.microtasks.is_empty()
                && self.js_jobs.is_empty()
                && self.immediates.lock().is_empty()
                && !has_servers
                && let Some(wait) = self.time_until_next_timer()
            {
                // Use tokio sleep instead of std::thread::sleep for async compatibility
                tokio::time::sleep(wait.min(Duration::from_millis(10))).await;
            }
        }

        self.running.store(false, Ordering::Release);
    }
}

impl Default for EventLoop {
    fn default() -> Self {
        // Note: new() returns Arc<Self>, but Default needs Self
        let sequencer = MicrotaskSequencer::new();
        Self {
            timers: Mutex::new(HashMap::new()),
            timer_heap: Mutex::new(BinaryHeap::new()),
            immediates: Mutex::new(VecDeque::new()),
            microtasks: MicrotaskQueue::with_sequencer(sequencer.clone()),
            js_jobs: Arc::new(JsJobQueue::with_sequencer(sequencer)),
            next_timer_id: AtomicU64::new(1),
            next_immediate_id: AtomicU64::new(1),
            running: AtomicBool::new(false),
            executing_timer_ids: Mutex::new(HashSet::new()),
            executing_immediate_ids: Mutex::new(HashSet::new()),
            // HTTP support
            http_events_rx: Mutex::new(None),
            active_http_servers: Arc::new(AtomicU64::new(0)),
            http_dispatcher: Mutex::new(None),
            // WebSocket support
            ws_events_rx: Mutex::new(None),
            ws_dispatcher: Mutex::new(None),
            // Async ops support
            pending_async_ops: Arc::new(AtomicU64::new(0)),
            async_task_handles: Mutex::new(Vec::new()),
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

    #[tokio::test]
    async fn test_set_timeout() {
        let counter = Arc::new(AtomicUsize::new(0));
        let counter_clone = counter.clone();

        let event_loop = EventLoop::new();
        event_loop.set_timeout(
            move || {
                counter_clone.fetch_add(1, Ordering::Relaxed);
            },
            Duration::from_millis(10),
        );

        event_loop.run_until_complete_async().await;
        assert_eq!(counter.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn test_clear_timeout() {
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
        tokio::time::sleep(Duration::from_millis(150)).await;

        assert_eq!(counter.load(Ordering::Relaxed), 0);
    }

    #[tokio::test]
    async fn test_microtask() {
        let counter = Arc::new(AtomicUsize::new(0));
        let counter_clone = counter.clone();

        let event_loop = EventLoop::new();
        event_loop.queue_microtask(move || {
            counter_clone.fetch_add(1, Ordering::Relaxed);
        });

        event_loop.run_until_complete_async().await;
        assert_eq!(counter.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn test_set_immediate() {
        let counter = Arc::new(AtomicUsize::new(0));
        let counter_clone = counter.clone();

        let event_loop = EventLoop::new();
        event_loop.schedule_immediate(
            move || {
                counter_clone.fetch_add(1, Ordering::Relaxed);
            },
            true,
        );

        event_loop.run_until_complete_async().await;
        assert_eq!(counter.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn test_clear_immediate() {
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
        event_loop.run_until_complete_async().await;

        assert_eq!(counter.load(Ordering::Relaxed), 0);
    }

    #[tokio::test]
    async fn test_immediate_fires_after_timeout() {
        // Per spec: timers phase runs before check (immediate) phase
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

        event_loop.run_until_complete_async().await;

        let result = order.lock();
        // Timers run before immediates
        assert_eq!(*result, vec!["timeout", "immediate"]);
    }

    #[tokio::test]
    async fn test_microtask_fires_before_immediate() {
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

        event_loop.run_until_complete_async().await;

        let result = order.lock();
        assert_eq!(*result, vec!["microtask", "immediate"]);
    }

    #[tokio::test]
    async fn test_unrefed_timer_does_not_keep_loop_alive() {
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
        event_loop.run_until_complete_async().await;

        // Timer should not have fired
        assert_eq!(counter.load(Ordering::Relaxed), 0);

        // Cleanup
        event_loop.clear_timer(id);
    }

    #[tokio::test]
    async fn test_set_interval() {
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
        event_loop.run_until_complete_async().await;

        // Should have fired exactly 3 times
        assert_eq!(counter.load(Ordering::Relaxed), 3);
    }

    #[tokio::test]
    async fn test_clear_interval() {
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

        event_loop.run_until_complete_async().await;

        // Should have fired exactly once (cleared after first fire)
        assert_eq!(counter.load(Ordering::Relaxed), 1);
    }
}
