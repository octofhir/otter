//! Event loop implementation for microtasks and timers.

use crate::bindings::*;
use crate::error::{JscError, JscResult};
use crate::value::extract_exception;
use parking_lot::Mutex;
use std::cell::{Cell, RefCell};
use std::collections::{BinaryHeap, HashMap, VecDeque};
use std::marker::PhantomData;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// HTML5 spec: timers nested more than this level get clamped to MIN_TIMEOUT_MS
/// Per spec: "If nesting level is greater than 5, and timeout is less than 4, then set timeout to 4."
const MAX_TIMER_NESTING_LEVEL: u32 = 5;
/// HTML5 spec: minimum timeout for deeply nested timers
const MIN_TIMEOUT_MS: u64 = 4;

thread_local! {
    /// Tracks timer nesting level for HTML5 spec compliance
    static TIMER_NESTING_LEVEL: Cell<u32> = const { Cell::new(0) };
}

struct TimerEntry {
    id: u64,
    callback: JSObjectRef,
    args: Vec<JSValueRef>,
    when: Instant,
    interval: Option<Duration>,
    /// Flag to mark timer as cancelled (for clearInterval inside callbacks)
    cancelled: AtomicBool,
    /// Whether this timer keeps the event loop alive.
    refed: AtomicBool,
    /// HTML5 spec: timer nesting level at creation time (inherited from creating task)
    nesting_level: u32,
}

impl std::fmt::Debug for TimerEntry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TimerEntry")
            .field("id", &self.id)
            .field("when", &self.when)
            .field("interval", &self.interval)
            .field("cancelled", &self.cancelled.load(Ordering::Relaxed))
            .field("refed", &self.refed.load(Ordering::Relaxed))
            .field("nesting_level", &self.nesting_level)
            .finish()
    }
}

/// Entry in the timer heap for O(log n) scheduling.
/// Uses Reverse for min-heap semantics (earliest `when` first).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct TimerHeapEntry {
    when: Instant,
    id: u64,
}

impl Ord for TimerHeapEntry {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        // Reverse ordering: smaller `when` = higher priority (min-heap)
        // Break ties by ID (lower ID = higher priority for FIFO)
        other
            .when
            .cmp(&self.when)
            .then_with(|| other.id.cmp(&self.id))
    }
}

impl PartialOrd for TimerHeapEntry {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

#[derive(Debug)]
struct MicrotaskEntry {
    callback: JSObjectRef,
}

#[derive(Debug)]
struct ImmediateEntry {
    id: u64,
    callback: JSObjectRef,
    args: Vec<JSValueRef>,
    cancelled: AtomicBool,
    refed: AtomicBool,
}

#[derive(Debug)]
struct ExecutingTimerState {
    cancelled: Arc<AtomicBool>,
    refed: Arc<AtomicBool>,
}

#[derive(Debug)]
struct ExecutingImmediateState {
    cancelled: Arc<AtomicBool>,
    refed: Arc<AtomicBool>,
}

pub(crate) struct EventLoop {
    ctx: JSContextRef,
    /// Timer storage by ID for O(1) lookup
    timers: Mutex<HashMap<u64, TimerEntry>>,
    /// Timer heap for O(log n) scheduling - min-heap ordered by `when`
    timer_heap: Mutex<BinaryHeap<TimerHeapEntry>>,
    microtasks: Mutex<VecDeque<MicrotaskEntry>>,
    immediates: Mutex<VecDeque<ImmediateEntry>>,
    next_timer_id: AtomicU64,
    next_immediate_id: AtomicU64,
    /// Tracks IDs of timers currently being executed (for clearInterval in callbacks)
    executing_timer_ids: Mutex<HashMap<u64, ExecutingTimerState>>,
    /// Tracks IDs of immediates currently being executed (for clearImmediate in callbacks)
    executing_immediate_ids: Mutex<HashMap<u64, ExecutingImmediateState>>,
    /// Marker to ensure EventLoop is !Send + !Sync.
    /// JSC contexts must be used only from the thread that created them.
    _marker: PhantomData<*mut ()>,
}

impl EventLoop {
    pub fn new(ctx: JSContextRef) -> Self {
        Self {
            ctx,
            timers: Mutex::new(HashMap::new()),
            timer_heap: Mutex::new(BinaryHeap::new()),
            microtasks: Mutex::new(VecDeque::new()),
            immediates: Mutex::new(VecDeque::new()),
            next_timer_id: AtomicU64::new(1),
            next_immediate_id: AtomicU64::new(1),
            executing_timer_ids: Mutex::new(HashMap::new()),
            executing_immediate_ids: Mutex::new(HashMap::new()),
            _marker: PhantomData,
        }
    }

    pub fn schedule_timer(
        &self,
        callback: JSObjectRef,
        delay: Duration,
        interval: Option<Duration>,
        args: Vec<JSValueRef>,
        refed: bool,
    ) -> JscResult<u64> {
        if unsafe { !JSObjectIsFunction(self.ctx, callback) } {
            return Err(JscError::type_error("function", "non-function"));
        }

        // HTML5 spec: Timer nesting level is inherited from the currently executing task.
        // The NEW timer's nesting level is the current level + 1.
        // If we're not inside a timer callback, current level is 0, so new timer gets level 1.
        let inherited_nesting = TIMER_NESTING_LEVEL.with(|level| level.get());
        let timer_nesting_level = inherited_nesting.saturating_add(1);

        // HTML5 spec: "If nesting level is greater than 5, and timeout is less than 4,
        // then set timeout to 4."
        let clamped_delay = if timer_nesting_level > MAX_TIMER_NESTING_LEVEL {
            delay.max(Duration::from_millis(MIN_TIMEOUT_MS))
        } else {
            delay
        };

        unsafe {
            JSValueProtect(self.ctx, callback as JSValueRef);
            for arg in &args {
                JSValueProtect(self.ctx, *arg);
            }
        }

        let id = self.next_timer_id.fetch_add(1, Ordering::Relaxed);
        let when = Instant::now() + clamped_delay;
        let entry = TimerEntry {
            id,
            callback,
            args,
            when,
            interval,
            cancelled: AtomicBool::new(false),
            refed: AtomicBool::new(refed),
            nesting_level: timer_nesting_level,
        };

        // Insert into HashMap for O(1) lookup by ID
        self.timers.lock().insert(id, entry);
        // Insert into heap for O(log n) scheduling
        self.timer_heap.lock().push(TimerHeapEntry { when, id });
        Ok(id)
    }

    /// Clear a timer by ID. Sets cancelled flag so it works even during callback execution.
    /// O(1) lookup thanks to HashMap storage.
    pub fn clear_timer(&self, id: u64) -> bool {
        // First check if timer is currently executing
        {
            let executing = self.executing_timer_ids.lock();
            if let Some(state) = executing.get(&id) {
                state.cancelled.store(true, Ordering::SeqCst);
                return true;
            }
        }

        // Then check the timer map - O(1) lookup
        let timers = self.timers.lock();
        if let Some(timer) = timers.get(&id) {
            timer.cancelled.store(true, Ordering::SeqCst);
            return true;
        }

        // Timer not found - might have already executed or been cleared
        false
    }

    /// Update whether a timer keeps the event loop alive.
    /// O(1) lookup thanks to HashMap storage.
    pub fn set_timer_ref(&self, id: u64, refed: bool) -> bool {
        {
            let executing = self.executing_timer_ids.lock();
            if let Some(state) = executing.get(&id) {
                state.refed.store(refed, Ordering::SeqCst);
                return true;
            }
        }

        // O(1) lookup in HashMap
        let timers = self.timers.lock();
        if let Some(timer) = timers.get(&id) {
            timer.refed.store(refed, Ordering::SeqCst);
            return true;
        }

        false
    }

    /// Schedule an immediate callback (setImmediate).
    pub fn schedule_immediate(
        &self,
        callback: JSObjectRef,
        args: Vec<JSValueRef>,
        refed: bool,
    ) -> JscResult<u64> {
        if unsafe { !JSObjectIsFunction(self.ctx, callback) } {
            return Err(JscError::type_error("function", "non-function"));
        }

        unsafe {
            JSValueProtect(self.ctx, callback as JSValueRef);
            for arg in &args {
                JSValueProtect(self.ctx, *arg);
            }
        }

        let id = self.next_immediate_id.fetch_add(1, Ordering::Relaxed);
        let entry = ImmediateEntry {
            id,
            callback,
            args,
            cancelled: AtomicBool::new(false),
            refed: AtomicBool::new(refed),
        };

        self.immediates.lock().push_back(entry);
        Ok(id)
    }

    /// Clear an immediate by ID. Sets cancelled flag so it works even during callback execution.
    pub fn clear_immediate(&self, id: u64) -> bool {
        {
            let executing = self.executing_immediate_ids.lock();
            if let Some(state) = executing.get(&id) {
                state.cancelled.store(true, Ordering::SeqCst);
                return true;
            }
        }

        let immediates = self.immediates.lock();
        if let Some(entry) = immediates.iter().find(|entry| entry.id == id) {
            entry.cancelled.store(true, Ordering::SeqCst);
            return true;
        }

        false
    }

    /// Update whether an immediate keeps the event loop alive.
    pub fn set_immediate_ref(&self, id: u64, refed: bool) -> bool {
        {
            let executing = self.executing_immediate_ids.lock();
            if let Some(state) = executing.get(&id) {
                state.refed.store(refed, Ordering::SeqCst);
                return true;
            }
        }

        let immediates = self.immediates.lock();
        if let Some(entry) = immediates.iter().find(|entry| entry.id == id) {
            entry.refed.store(refed, Ordering::SeqCst);
            return true;
        }

        false
    }

    /// Remove cancelled timers and clean up their resources
    fn cleanup_cancelled_timers(&self) {
        // Collect IDs of cancelled timers
        let cancelled_ids: Vec<u64> = {
            let timers = self.timers.lock();
            timers
                .iter()
                .filter(|(_, t)| t.cancelled.load(Ordering::SeqCst))
                .map(|(&id, _)| id)
                .collect()
        };

        // Remove and drop each cancelled timer
        for id in cancelled_ids {
            if let Some(timer) = self.timers.lock().remove(&id) {
                self.drop_timer(timer);
            }
        }
    }

    pub fn queue_microtask(&self, callback: JSObjectRef) -> JscResult<()> {
        if unsafe { !JSObjectIsFunction(self.ctx, callback) } {
            return Err(JscError::type_error("function", "non-function"));
        }

        unsafe {
            JSValueProtect(self.ctx, callback as JSValueRef);
        }

        self.microtasks
            .lock()
            .push_back(MicrotaskEntry { callback });
        Ok(())
    }

    pub fn poll(&self) -> JscResult<usize> {
        let mut executed = 0;
        executed += self.run_microtasks()?;
        executed += self.run_timers()?;
        executed += self.run_immediates()?;
        Ok(executed)
    }

    pub fn has_pending_tasks(&self) -> bool {
        if !self.microtasks.lock().is_empty() {
            return true;
        }
        // Only count non-cancelled, refed timers
        let timers = self.timers.lock();
        if timers
            .values()
            .any(|t| !t.cancelled.load(Ordering::Relaxed) && t.refed.load(Ordering::Relaxed))
        {
            return true;
        }

        let immediates = self.immediates.lock();
        immediates
            .iter()
            .any(|i| !i.cancelled.load(Ordering::Relaxed) && i.refed.load(Ordering::Relaxed))
    }

    pub fn next_timer_deadline(&self) -> Option<Instant> {
        let immediates = self.immediates.lock();
        if immediates
            .iter()
            .any(|i| !i.cancelled.load(Ordering::Relaxed) && i.refed.load(Ordering::Relaxed))
        {
            return Some(Instant::now());
        }

        let timers = self.timers.lock();
        timers
            .values()
            .filter(|t| !t.cancelled.load(Ordering::Relaxed) && t.refed.load(Ordering::Relaxed))
            .map(|timer| timer.when)
            .min()
    }

    fn run_microtasks(&self) -> JscResult<usize> {
        let mut ran = 0;
        loop {
            let task = {
                let mut microtasks = self.microtasks.lock();
                microtasks.pop_front()
            };

            let Some(task) = task else {
                break;
            };

            let result = self.call_function(task.callback, &[]);
            unsafe {
                JSValueUnprotect(self.ctx, task.callback as JSValueRef);
            }

            result?;
            ran += 1;
        }

        Ok(ran)
    }

    /// Run microtasks, logging errors but continuing to process the queue.
    /// This prevents a single failing microtask from jamming the entire queue.
    fn run_microtasks_continue_on_error(&self) -> usize {
        let mut ran = 0;
        loop {
            let task = {
                let mut microtasks = self.microtasks.lock();
                microtasks.pop_front()
            };

            let Some(task) = task else {
                break;
            };

            let result = self.call_function(task.callback, &[]);
            unsafe {
                JSValueUnprotect(self.ctx, task.callback as JSValueRef);
            }

            match result {
                Ok(()) => {}
                Err(e) => {
                    // Log error but continue processing - don't jam the queue
                    tracing::warn!("Microtask error: {}", e);
                }
            }
            ran += 1;
        }

        ran
    }

    fn run_timers(&self) -> JscResult<usize> {
        let now = Instant::now();
        let mut ran = 0;

        // Collect due timer IDs from heap - O(log n) per pop
        // We collect first to avoid holding locks during callback execution
        let mut due_ids = Vec::new();
        {
            let mut heap = self.timer_heap.lock();
            let timers = self.timers.lock();

            while let Some(&entry) = heap.peek() {
                if entry.when > now {
                    break; // No more due timers
                }

                heap.pop();

                // Check if this is a valid, non-cancelled timer
                if let Some(timer) = timers.get(&entry.id) {
                    // Skip if cancelled or if `when` doesn't match (stale heap entry)
                    if !timer.cancelled.load(Ordering::SeqCst) && timer.when == entry.when {
                        due_ids.push(entry.id);
                    }
                }
                // Stale entries (timer removed or rescheduled) are just discarded
            }
        }

        for timer_id in due_ids {
            // Look up timer info while it's STILL in self.timers (O(1) lookup)
            let timer_info = {
                let timers = self.timers.lock();
                timers.get(&timer_id).map(|t| {
                    (
                        t.callback,
                        t.args.clone(),
                        t.interval,
                        t.nesting_level,
                        t.refed.load(Ordering::Relaxed),
                        t.cancelled.load(Ordering::SeqCst),
                    )
                })
            };

            let Some((callback, args, interval, nesting_level, is_refed, is_cancelled)) =
                timer_info
            else {
                continue; // Timer was removed
            };

            if is_cancelled {
                self.remove_and_drop_timer(timer_id);
                continue;
            }

            // Register in executing_timer_ids
            let cancelled_flag = Arc::new(AtomicBool::new(false));
            let refed_flag = Arc::new(AtomicBool::new(is_refed));
            self.executing_timer_ids.lock().insert(
                timer_id,
                ExecutingTimerState {
                    cancelled: cancelled_flag.clone(),
                    refed: refed_flag.clone(),
                },
            );

            // Set nesting level and execute callback
            TIMER_NESTING_LEVEL.with(|level| level.set(nesting_level));
            let call_result = self.call_function(callback, &args);
            TIMER_NESTING_LEVEL.with(|level| level.set(0));

            match &call_result {
                Ok(()) => ran += 1,
                Err(e) => tracing::warn!("Timer {} callback error: {}", timer_id, e),
            }

            // Run microtasks while timer is still in executing_timer_ids
            let _ = self.run_microtasks_continue_on_error();

            // Check cancellation
            let was_cancelled = cancelled_flag.load(Ordering::SeqCst);
            let final_refed = refed_flag.load(Ordering::SeqCst);
            let timer_cancelled = {
                let timers = self.timers.lock();
                timers
                    .get(&timer_id)
                    .map(|t| t.cancelled.load(Ordering::SeqCst))
                    .unwrap_or(true)
            };

            self.executing_timer_ids.lock().remove(&timer_id);

            if was_cancelled || timer_cancelled {
                self.remove_and_drop_timer(timer_id);
                continue;
            }

            // Handle reschedule or cleanup
            if let Some(interval_duration) = interval {
                let clamped_interval = if nesting_level > MAX_TIMER_NESTING_LEVEL {
                    interval_duration.max(Duration::from_millis(MIN_TIMEOUT_MS))
                } else {
                    interval_duration
                };
                let new_when = Instant::now() + clamped_interval;

                // Update timer and push new heap entry
                {
                    let mut timers = self.timers.lock();
                    if let Some(timer) = timers.get_mut(&timer_id) {
                        timer.when = new_when;
                        timer.refed.store(final_refed, Ordering::SeqCst);
                        timer.cancelled.store(false, Ordering::SeqCst);
                    }
                }
                self.timer_heap
                    .lock()
                    .push(TimerHeapEntry { when: new_when, id: timer_id });
            } else {
                self.remove_and_drop_timer(timer_id);
            }
        }

        // Cleanup any remaining cancelled timers
        self.cleanup_cancelled_timers();

        Ok(ran)
    }

    /// Remove a timer by ID from the HashMap and drop it (unprotect JS values).
    fn remove_and_drop_timer(&self, timer_id: u64) {
        let timer = self.timers.lock().remove(&timer_id);

        if let Some(timer) = timer {
            self.drop_timer(timer);
        }
    }

    fn call_function(&self, callback: JSObjectRef, args: &[JSValueRef]) -> JscResult<()> {
        unsafe {
            let mut exception: JSValueRef = std::ptr::null_mut();
            let result = JSObjectCallAsFunction(
                self.ctx,
                callback,
                JSContextGetGlobalObject(self.ctx),
                args.len(),
                args.as_ptr(),
                &mut exception,
            );

            if !exception.is_null() || result.is_null() {
                return Err(extract_exception(self.ctx, exception).into());
            }
        }

        Ok(())
    }

    fn drop_timer(&self, timer: TimerEntry) {
        unsafe {
            JSValueUnprotect(self.ctx, timer.callback as JSValueRef);
            for arg in timer.args {
                JSValueUnprotect(self.ctx, arg);
            }
        }
    }

    fn run_immediates(&self) -> JscResult<usize> {
        // Step 1: Collect IDs of pending immediates WITHOUT removing them from storage.
        // This ensures clearImmediate(id) can still find them during execution of other immediates.
        let due_ids: Vec<u64> = {
            let queue = self.immediates.lock();
            queue.iter().map(|i| i.id).collect()
        };

        let mut ran = 0;

        for immediate_id in due_ids {
            // Step 2: Look up immediate info while it's STILL in self.immediates.
            let immediate_info = {
                let queue = self.immediates.lock();
                queue.iter().find(|i| i.id == immediate_id).map(|i| {
                    (
                        i.callback,
                        i.args.clone(),
                        i.refed.load(Ordering::Relaxed),
                        i.cancelled.load(Ordering::SeqCst),
                    )
                })
            };

            let Some((callback, args, is_refed, is_cancelled)) = immediate_info else {
                // Immediate was already removed (e.g., by clearImmediate from another callback)
                continue;
            };

            // Check if cancelled before execution
            if is_cancelled {
                self.remove_and_drop_immediate(immediate_id);
                continue;
            }

            // Step 3: Register in executing_immediate_ids.
            // Immediate is now findable in BOTH self.immediates AND executing_immediate_ids.
            let cancelled_flag = Arc::new(AtomicBool::new(false));
            let refed_flag = Arc::new(AtomicBool::new(is_refed));
            self.executing_immediate_ids.lock().insert(
                immediate_id,
                ExecutingImmediateState {
                    cancelled: cancelled_flag.clone(),
                    refed: refed_flag.clone(),
                },
            );

            // Step 4: Execute callback.
            let call_result = self.call_function(callback, &args);

            match &call_result {
                Ok(()) => {
                    ran += 1;
                }
                Err(e) => {
                    tracing::warn!("Immediate {} callback error: {}", immediate_id, e);
                }
            }

            // Step 5: Run microtasks WHILE immediate is still in executing_immediate_ids.
            let _ = self.run_microtasks_continue_on_error();

            // Step 6: Check cancellation.
            let was_cancelled = cancelled_flag.load(Ordering::SeqCst);

            // Also check the immediate's own cancelled flag
            let immediate_cancelled = {
                let queue = self.immediates.lock();
                queue
                    .iter()
                    .find(|i| i.id == immediate_id)
                    .map(|i| i.cancelled.load(Ordering::SeqCst))
                    .unwrap_or(true)
            };

            // Step 7: Remove from executing map.
            self.executing_immediate_ids.lock().remove(&immediate_id);

            // Step 8: Remove from queue (immediates don't reschedule like intervals).
            self.remove_and_drop_immediate(immediate_id);

            // Track if cancelled for logging purposes (immediate is already cleaned up)
            if was_cancelled || immediate_cancelled {
                continue;
            }
        }

        Ok(ran)
    }

    /// Remove an immediate by ID from the immediates queue and drop it (unprotect JS values).
    fn remove_and_drop_immediate(&self, immediate_id: u64) {
        let immediate = {
            let mut queue = self.immediates.lock();
            if let Some(idx) = queue.iter().position(|i| i.id == immediate_id) {
                Some(queue.remove(idx).unwrap())
            } else {
                None
            }
        };

        if let Some(immediate) = immediate {
            self.drop_immediate(immediate);
        }
    }

    fn drop_immediate(&self, immediate: ImmediateEntry) {
        unsafe {
            JSValueUnprotect(self.ctx, immediate.callback as JSValueRef);
            for arg in immediate.args {
                JSValueUnprotect(self.ctx, arg);
            }
        }
    }
}

thread_local! {
    static EVENT_LOOP_MAP: RefCell<HashMap<usize, Arc<EventLoop>>> =
        RefCell::new(HashMap::new());
}

pub(crate) fn register_context_event_loop(ctx: JSContextRef, event_loop: Arc<EventLoop>) {
    let ctx_key = ctx as usize;
    let global_key = unsafe { JSContextGetGlobalObject(ctx) as usize };
    EVENT_LOOP_MAP.with(|map| {
        let mut map = map.borrow_mut();
        map.insert(ctx_key, event_loop.clone());
        map.insert(global_key, event_loop);
    });
}

pub(crate) fn unregister_context_event_loop(ctx: JSContextRef) {
    let ctx_key = ctx as usize;
    let global_key = unsafe { JSContextGetGlobalObject(ctx) as usize };
    EVENT_LOOP_MAP.with(|map| {
        let mut map = map.borrow_mut();
        map.remove(&ctx_key);
        map.remove(&global_key);
    });
}

pub(crate) fn event_loop_for_context(ctx: JSContextRef) -> Option<Arc<EventLoop>> {
    let ctx_key = ctx as usize;
    let global_key = unsafe { JSContextGetGlobalObject(ctx) as usize };
    EVENT_LOOP_MAP.with(|map| {
        let map = map.borrow();
        map.get(&ctx_key)
            .cloned()
            .or_else(|| map.get(&global_key).cloned())
    })
}

pub(crate) fn get_function_arg(
    ctx: JSContextRef,
    arguments: *const JSValueRef,
    index: usize,
    argument_count: usize,
) -> JscResult<JSObjectRef> {
    if index >= argument_count {
        return Err(JscError::type_error("function", "missing"));
    }

    unsafe {
        let value = *arguments.add(index);
        let mut exception: JSValueRef = std::ptr::null_mut();
        let object = JSValueToObject(ctx, value, &mut exception);
        if !exception.is_null() || object.is_null() {
            return Err(JscError::type_error("function", "non-object"));
        }
        if !JSObjectIsFunction(ctx, object) {
            return Err(JscError::type_error("function", "non-function"));
        }
        Ok(object)
    }
}

pub(crate) fn get_delay_arg(
    ctx: JSContextRef,
    arguments: *const JSValueRef,
    index: usize,
    argument_count: usize,
) -> JscResult<Duration> {
    if index >= argument_count {
        return Ok(Duration::from_millis(0));
    }

    unsafe {
        let value = *arguments.add(index);
        let mut exception: JSValueRef = std::ptr::null_mut();
        let delay = JSValueToNumber(ctx, value, &mut exception);
        if !exception.is_null() {
            return Err(extract_exception(ctx, exception).into());
        }
        Ok(Duration::from_millis(delay.max(0.0) as u64))
    }
}

pub(crate) fn create_id_value(ctx: JSContextRef, id: u64) -> JSValueRef {
    unsafe { JSValueMakeNumber(ctx, id as f64) }
}

pub(crate) fn parse_id_arg(
    ctx: JSContextRef,
    arguments: *const JSValueRef,
    index: usize,
    argument_count: usize,
) -> JscResult<u64> {
    if index >= argument_count {
        return Err(JscError::type_error("timer id", "missing"));
    }

    unsafe {
        let value = *arguments.add(index);
        let mut exception: JSValueRef = std::ptr::null_mut();
        let id = JSValueToNumber(ctx, value, &mut exception);
        if !exception.is_null() {
            return Err(extract_exception(ctx, exception).into());
        }
        Ok(id as u64)
    }
}

pub(crate) fn collect_args(
    arguments: *const JSValueRef,
    start: usize,
    argument_count: usize,
) -> Vec<JSValueRef> {
    let mut args = Vec::new();
    for index in start..argument_count {
        unsafe {
            args.push(*arguments.add(index));
        }
    }
    args
}
