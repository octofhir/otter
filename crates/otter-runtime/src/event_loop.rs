//! Event loop implementation for microtasks and timers.

use crate::bindings::*;
use crate::error::{JscError, JscResult};
use crate::value::extract_exception;
use parking_lot::Mutex;
use std::cell::RefCell;
use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

#[derive(Debug)]
struct TimerEntry {
    id: u64,
    callback: JSObjectRef,
    args: Vec<JSValueRef>,
    when: Instant,
    interval: Option<Duration>,
}

#[derive(Debug)]
struct MicrotaskEntry {
    callback: JSObjectRef,
}

pub(crate) struct EventLoop {
    ctx: JSContextRef,
    timers: Mutex<Vec<TimerEntry>>,
    microtasks: Mutex<VecDeque<MicrotaskEntry>>,
    next_timer_id: AtomicU64,
}

impl EventLoop {
    pub fn new(ctx: JSContextRef) -> Self {
        Self {
            ctx,
            timers: Mutex::new(Vec::new()),
            microtasks: Mutex::new(VecDeque::new()),
            next_timer_id: AtomicU64::new(1),
        }
    }

    pub fn schedule_timer(
        &self,
        callback: JSObjectRef,
        delay: Duration,
        interval: Option<Duration>,
        args: Vec<JSValueRef>,
    ) -> JscResult<u64> {
        if unsafe { !JSObjectIsFunction(self.ctx, callback) } {
            return Err(JscError::TypeError {
                expected: "function".to_string(),
                actual: "non-function".to_string(),
            });
        }

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
            when: Instant::now() + delay,
            interval,
        };

        self.timers.lock().push(entry);
        Ok(id)
    }

    pub fn clear_timer(&self, id: u64) -> bool {
        let mut timers = self.timers.lock();
        if let Some(index) = timers.iter().position(|timer| timer.id == id) {
            let timer = timers.remove(index);
            self.drop_timer(timer);
            true
        } else {
            false
        }
    }

    pub fn queue_microtask(&self, callback: JSObjectRef) -> JscResult<()> {
        if unsafe { !JSObjectIsFunction(self.ctx, callback) } {
            return Err(JscError::TypeError {
                expected: "function".to_string(),
                actual: "non-function".to_string(),
            });
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
        Ok(executed)
    }

    pub fn has_pending_tasks(&self) -> bool {
        if !self.microtasks.lock().is_empty() {
            return true;
        }
        !self.timers.lock().is_empty()
    }

    pub fn next_timer_deadline(&self) -> Option<Instant> {
        let timers = self.timers.lock();
        timers.iter().map(|timer| timer.when).min()
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
            self.call_function(timer.callback, &timer.args)?;
            ran += 1;

            self.run_microtasks()?;

            if let Some(interval) = timer.interval {
                timer.when = Instant::now() + interval;
                self.timers.lock().push(timer);
            } else {
                self.drop_timer(timer);
            }
        }

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
                return Err(extract_exception(self.ctx, exception));
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
}

thread_local! {
    static EVENT_LOOP_MAP: RefCell<HashMap<usize, Arc<EventLoop>>> =
        RefCell::new(HashMap::new());
}

pub(crate) fn register_context_event_loop(ctx: JSContextRef, event_loop: Arc<EventLoop>) {
    EVENT_LOOP_MAP.with(|map| {
        map.borrow_mut().insert(ctx as usize, event_loop);
    });
}

pub(crate) fn unregister_context_event_loop(ctx: JSContextRef) {
    EVENT_LOOP_MAP.with(|map| {
        map.borrow_mut().remove(&(ctx as usize));
    });
}

pub(crate) fn event_loop_for_context(ctx: JSContextRef) -> Option<Arc<EventLoop>> {
    EVENT_LOOP_MAP.with(|map| map.borrow().get(&(ctx as usize)).cloned())
}

pub(crate) fn get_function_arg(
    ctx: JSContextRef,
    arguments: *const JSValueRef,
    index: usize,
    argument_count: usize,
) -> JscResult<JSObjectRef> {
    if index >= argument_count {
        return Err(JscError::TypeError {
            expected: "function".to_string(),
            actual: "missing".to_string(),
        });
    }

    unsafe {
        let value = *arguments.add(index);
        let mut exception: JSValueRef = std::ptr::null_mut();
        let object = JSValueToObject(ctx, value, &mut exception);
        if !exception.is_null() || object.is_null() {
            return Err(JscError::TypeError {
                expected: "function".to_string(),
                actual: "non-object".to_string(),
            });
        }
        if !JSObjectIsFunction(ctx, object) {
            return Err(JscError::TypeError {
                expected: "function".to_string(),
                actual: "non-function".to_string(),
            });
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
            return Err(extract_exception(ctx, exception));
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
        return Err(JscError::TypeError {
            expected: "timer id".to_string(),
            actual: "missing".to_string(),
        });
    }

    unsafe {
        let value = *arguments.add(index);
        let mut exception: JSValueRef = std::ptr::null_mut();
        let id = JSValueToNumber(ctx, value, &mut exception);
        if !exception.is_null() {
            return Err(extract_exception(ctx, exception));
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
