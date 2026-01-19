//! Event loop implementation for microtasks and timers.

use crate::bindings::*;
use crate::error::{JscError, JscResult};
use crate::value::extract_exception;
use parking_lot::Mutex;
use std::cell::{Cell, RefCell};
use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// HTML5 spec: timers nested more than this level get clamped to MIN_TIMEOUT_MS
const MAX_TIMER_NESTING_LEVEL: u32 = 4;
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
}

impl std::fmt::Debug for TimerEntry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TimerEntry")
            .field("id", &self.id)
            .field("when", &self.when)
            .field("interval", &self.interval)
            .field("cancelled", &self.cancelled.load(Ordering::Relaxed))
            .field("refed", &self.refed.load(Ordering::Relaxed))
            .finish()
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

pub(crate) struct EventLoop {
    ctx: JSContextRef,
    timers: Mutex<Vec<TimerEntry>>,
    microtasks: Mutex<VecDeque<MicrotaskEntry>>,
    immediates: Mutex<VecDeque<ImmediateEntry>>,
    next_timer_id: AtomicU64,
    next_immediate_id: AtomicU64,
    /// Tracks IDs of timers currently being executed (for clearInterval in callbacks)
    executing_timer_ids: Mutex<HashMap<u64, ExecutingTimerState>>,
    /// Tracks IDs of immediates currently being executed (for clearImmediate in callbacks)
    executing_immediate_ids: Mutex<HashMap<u64, Arc<AtomicBool>>>,
}

impl EventLoop {
    pub fn new(ctx: JSContextRef) -> Self {
        Self {
            ctx,
            timers: Mutex::new(Vec::new()),
            microtasks: Mutex::new(VecDeque::new()),
            immediates: Mutex::new(VecDeque::new()),
            next_timer_id: AtomicU64::new(1),
            next_immediate_id: AtomicU64::new(1),
            executing_timer_ids: Mutex::new(HashMap::new()),
            executing_immediate_ids: Mutex::new(HashMap::new()),
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

        // HTML5 spec: Apply nested timer clamping
        let clamped_delay = TIMER_NESTING_LEVEL.with(|level| {
            if level.get() > MAX_TIMER_NESTING_LEVEL {
                delay.max(Duration::from_millis(MIN_TIMEOUT_MS))
            } else {
                delay
            }
        });

        unsafe {
            JSValueProtect(self.ctx, callback as JSValueRef);
            for arg in &args {
                JSValueProtect(self.ctx, *arg);
            }
        }

        let id = self.next_timer_id.fetch_add(1, Ordering::Relaxed);
        let entry = TimerEntry {
            id,
            callback,
            args,
            when: Instant::now() + clamped_delay,
            interval,
            cancelled: AtomicBool::new(false),
            refed: AtomicBool::new(refed),
        };

        self.timers.lock().push(entry);
        Ok(id)
    }

    /// Clear a timer by ID. Sets cancelled flag so it works even during callback execution.
    pub fn clear_timer(&self, id: u64) -> bool {
        // First check if timer is currently executing
        {
            let executing = self.executing_timer_ids.lock();
            if let Some(state) = executing.get(&id) {
                state.cancelled.store(true, Ordering::SeqCst);
                return true;
            }
        }

        // Then check the timer queue
        let timers = self.timers.lock();
        if let Some(timer) = timers.iter().find(|timer| timer.id == id) {
            timer.cancelled.store(true, Ordering::SeqCst);
            return true;
        }

        // Timer not found - might have already executed or been cleared
        false
    }

    /// Update whether a timer keeps the event loop alive.
    pub fn set_timer_ref(&self, id: u64, refed: bool) -> bool {
        {
            let executing = self.executing_timer_ids.lock();
            if let Some(state) = executing.get(&id) {
                state.refed.store(refed, Ordering::SeqCst);
                return true;
            }
        }

        let timers = self.timers.lock();
        if let Some(timer) = timers.iter().find(|timer| timer.id == id) {
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
            if let Some(cancelled_flag) = executing.get(&id) {
                cancelled_flag.store(true, Ordering::SeqCst);
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
                state.store(refed, Ordering::SeqCst);
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
        let mut timers = self.timers.lock();
        let mut i = 0;
        while i < timers.len() {
            if timers[i].cancelled.load(Ordering::SeqCst) {
                let timer = timers.remove(i);
                // Drop lock before calling drop_timer to avoid potential issues
                drop(timers);
                self.drop_timer(timer);
                timers = self.timers.lock();
                // Don't increment i - we removed an element
            } else {
                i += 1;
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
        // Only count non-cancelled timers
        let timers = self.timers.lock();
        if timers.iter().any(|t| {
            !t.cancelled.load(Ordering::Relaxed) && t.refed.load(Ordering::Relaxed)
        }) {
            return true;
        }

        let immediates = self.immediates.lock();
        immediates.iter().any(|i| {
            !i.cancelled.load(Ordering::Relaxed) && i.refed.load(Ordering::Relaxed)
        })
    }

    pub fn next_timer_deadline(&self) -> Option<Instant> {
        let immediates = self.immediates.lock();
        if immediates.iter().any(|i| {
            !i.cancelled.load(Ordering::Relaxed) && i.refed.load(Ordering::Relaxed)
        }) {
            return Some(Instant::now());
        }

        let timers = self.timers.lock();
        timers
            .iter()
            .filter(|t| {
                !t.cancelled.load(Ordering::Relaxed) && t.refed.load(Ordering::Relaxed)
            })
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

    fn run_timers(&self) -> JscResult<usize> {
        let now = Instant::now();

        // Extract due timers (including cancelled ones - we'll check later)
        let due = {
            let mut timers = self.timers.lock();
            let mut due = Vec::new();
            let mut index = 0;
            while index < timers.len() {
                if timers[index].when <= now {
                    due.push(timers.remove(index));
                } else {
                    index += 1;
                }
            }
            due
        };

        let mut ran = 0;

        for mut timer in due {
            // Check cancelled BEFORE execution (might have been cancelled by previous timer)
            if timer.cancelled.load(Ordering::SeqCst) {
                self.drop_timer(timer);
                continue;
            }

            let timer_id = timer.id;
            let is_interval = timer.interval.is_some();

            // Create shared cancelled flag for this executing timer
            let cancelled_flag = Arc::new(AtomicBool::new(false));
            let refed_flag = Arc::new(AtomicBool::new(timer.refed.load(Ordering::Relaxed)));
            self.executing_timer_ids
                .lock()
                .insert(
                    timer_id,
                    ExecutingTimerState {
                        cancelled: cancelled_flag.clone(),
                        refed: refed_flag.clone(),
                    },
                );

            // Increment nesting level for HTML5 spec compliance
            TIMER_NESTING_LEVEL.with(|level| {
                level.set(level.get().saturating_add(1));
            });

            // Execute callback - continue on error (browser behavior)
            let call_result = self.call_function(timer.callback, &timer.args);

            // Decrement nesting level
            TIMER_NESTING_LEVEL.with(|level| {
                level.set(level.get().saturating_sub(1));
            });

            // Remove from executing map
            self.executing_timer_ids.lock().remove(&timer_id);

            // Check if cancelled during execution
            let was_cancelled = cancelled_flag.load(Ordering::SeqCst);
            let is_refed = refed_flag.load(Ordering::SeqCst);

            match call_result {
                Ok(()) => {
                    ran += 1;
                    // Run microtasks after each timer (ignore microtask errors too)
                    let _ = self.run_microtasks();
                }
                Err(e) => {
                    // Log error but continue event loop (browser behavior)
                    tracing::warn!("Timer {} callback error: {}", timer_id, e);
                }
            }

            // Check cancelled AFTER execution (clearInterval might have been called in callback)
            if was_cancelled || timer.cancelled.load(Ordering::SeqCst) {
                self.drop_timer(timer);
                continue;
            }

            // Reschedule interval timers, cleanup one-shot timers
            if is_interval {
                timer.refed.store(is_refed, Ordering::SeqCst);
                // Apply clamping for rescheduled intervals too
                let interval = timer.interval.unwrap();
                let clamped_interval = TIMER_NESTING_LEVEL.with(|level| {
                    if level.get() > MAX_TIMER_NESTING_LEVEL {
                        interval.max(Duration::from_millis(MIN_TIMEOUT_MS))
                    } else {
                        interval
                    }
                });
                timer.when = Instant::now() + clamped_interval;
                // Reset cancelled flag for next iteration
                timer.cancelled.store(false, Ordering::SeqCst);
                self.timers.lock().push(timer);
            } else {
                self.drop_timer(timer);
            }
        }

        // Periodically cleanup cancelled timers that are still in the queue
        self.cleanup_cancelled_timers();

        Ok(ran)
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
        let immediates = {
            let mut queue = self.immediates.lock();
            let mut due = Vec::new();
            while let Some(entry) = queue.pop_front() {
                due.push(entry);
            }
            due
        };

        let mut ran = 0;

        for immediate in immediates {
            if immediate.cancelled.load(Ordering::SeqCst) {
                self.drop_immediate(immediate);
                continue;
            }

            let id = immediate.id;
            let cancelled_flag = Arc::new(AtomicBool::new(false));
            self.executing_immediate_ids
                .lock()
                .insert(id, cancelled_flag.clone());

            let call_result = self.call_function(immediate.callback, &immediate.args);
            self.executing_immediate_ids.lock().remove(&id);

            let was_cancelled = cancelled_flag.load(Ordering::SeqCst);

            match call_result {
                Ok(()) => {
                    ran += 1;
                    let _ = self.run_microtasks();
                }
                Err(e) => {
                    tracing::warn!("Immediate {} callback error: {}", id, e);
                }
            }

            if was_cancelled {
                self.drop_immediate(immediate);
                continue;
            }

            self.drop_immediate(immediate);
        }

        Ok(ran)
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
