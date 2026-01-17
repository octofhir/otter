//! WTFTimer implementation for bun-webkit integration.
//!
//! bun-webkit expects these timer functions to be provided by the embedder.
//! This implementation provides the C ABI functions that WebKit's WTF RunLoop
//! calls to schedule timers.

// Allow unsafe operations in unsafe functions (Rust 2024 compatibility)
#![allow(unsafe_op_in_unsafe_fn)]

use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Timer state
struct WTFTimerInner {
    /// Callback function pointer provided by WTF
    callback: Option<unsafe extern "C" fn(*mut c_void)>,
    /// User data for callback
    user_data: *mut c_void,
    /// Whether the timer is active
    active: AtomicBool,
    /// Whether this is a repeating timer
    repeat: AtomicBool,
    /// Fire time (if scheduled)
    fire_time: Mutex<Option<Instant>>,
    /// Repeat interval
    interval: AtomicU64, // stored as nanoseconds
}

// SAFETY: user_data is managed by WTF and must be thread-safe
unsafe impl Send for WTFTimerInner {}
unsafe impl Sync for WTFTimerInner {}

/// Opaque timer handle
pub struct WTFTimer {
    inner: Arc<WTFTimerInner>,
}

impl WTFTimer {
    fn new(callback: unsafe extern "C" fn(*mut c_void), user_data: *mut c_void) -> Self {
        Self {
            inner: Arc::new(WTFTimerInner {
                callback: Some(callback),
                user_data,
                active: AtomicBool::new(false),
                repeat: AtomicBool::new(false),
                fire_time: Mutex::new(None),
                interval: AtomicU64::new(0),
            }),
        }
    }

    fn update(&self, delay_seconds: f64, repeat: bool) {
        let delay = Duration::from_secs_f64(delay_seconds.max(0.0));
        let fire_time = Instant::now() + delay;

        *self.inner.fire_time.lock().unwrap() = Some(fire_time);
        self.inner
            .interval
            .store(delay.as_nanos() as u64, Ordering::SeqCst);
        self.inner.repeat.store(repeat, Ordering::SeqCst);
        self.inner.active.store(true, Ordering::SeqCst);

        // Schedule on the global timer thread
        schedule_timer(self.inner.clone(), delay);
    }

    fn cancel(&self) {
        self.inner.active.store(false, Ordering::SeqCst);
        *self.inner.fire_time.lock().unwrap() = None;
    }

    fn is_active(&self) -> bool {
        self.inner.active.load(Ordering::SeqCst)
    }

    fn seconds_until_fire(&self) -> f64 {
        if let Some(fire_time) = *self.inner.fire_time.lock().unwrap() {
            let now = Instant::now();
            if fire_time > now {
                return (fire_time - now).as_secs_f64();
            }
            return 0.0;
        }
        f64::INFINITY
    }
}

// Global timer scheduling
use std::collections::BinaryHeap;
use std::sync::OnceLock;
use std::thread;

struct ScheduledTimer {
    fire_time: Instant,
    timer: Arc<WTFTimerInner>,
}

impl PartialEq for ScheduledTimer {
    fn eq(&self, other: &Self) -> bool {
        self.fire_time == other.fire_time
    }
}

impl Eq for ScheduledTimer {}

impl PartialOrd for ScheduledTimer {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for ScheduledTimer {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        // Reverse order for min-heap behavior
        other.fire_time.cmp(&self.fire_time)
    }
}

struct TimerQueue {
    timers: Mutex<BinaryHeap<ScheduledTimer>>,
    notify: std::sync::Condvar,
}

static TIMER_QUEUE: OnceLock<TimerQueue> = OnceLock::new();

fn get_timer_queue() -> &'static TimerQueue {
    TIMER_QUEUE.get_or_init(|| {
        let queue = TimerQueue {
            timers: Mutex::new(BinaryHeap::new()),
            notify: std::sync::Condvar::new(),
        };

        // Spawn timer thread
        thread::Builder::new()
            .name("wtf-timer".into())
            .spawn(|| timer_thread_main())
            .expect("Failed to spawn timer thread");

        queue
    })
}

fn schedule_timer(timer: Arc<WTFTimerInner>, delay: Duration) {
    let queue = get_timer_queue();
    let fire_time = Instant::now() + delay;

    {
        let mut timers = queue.timers.lock().unwrap();
        timers.push(ScheduledTimer { fire_time, timer });
    }

    queue.notify.notify_one();
}

fn timer_thread_main() {
    let queue = get_timer_queue();

    loop {
        let mut timers = queue.timers.lock().unwrap();

        // Wait for timers or notification
        while timers.is_empty() {
            timers = queue.notify.wait(timers).unwrap();
        }

        // Check the next timer
        if let Some(next) = timers.peek() {
            let now = Instant::now();
            if next.fire_time <= now {
                // Timer is ready to fire
                let scheduled = timers.pop().unwrap();
                drop(timers); // Release lock before callback

                fire_timer(&scheduled.timer);
            } else {
                // Wait until fire time or new timer
                let wait_duration = next.fire_time - now;
                let _ = queue.notify.wait_timeout(timers, wait_duration);
            }
        }
    }
}

fn fire_timer(timer: &Arc<WTFTimerInner>) {
    if !timer.active.load(Ordering::SeqCst) {
        return;
    }

    // Call the callback
    if let Some(callback) = timer.callback {
        unsafe {
            callback(timer.user_data);
        }
    }

    // Reschedule if repeating
    if timer.repeat.load(Ordering::SeqCst) && timer.active.load(Ordering::SeqCst) {
        let interval_nanos = timer.interval.load(Ordering::SeqCst);
        let delay = Duration::from_nanos(interval_nanos);
        *timer.fire_time.lock().unwrap() = Some(Instant::now() + delay);
        schedule_timer(timer.clone(), delay);
    } else {
        timer.active.store(false, Ordering::SeqCst);
    }
}

// C ABI exports for bun-webkit

/// Create a new WTF timer.
///
/// # Safety
/// - `callback` must be a valid function pointer
/// - `user_data` must remain valid for the lifetime of the timer
#[unsafe(no_mangle)]
pub unsafe extern "C" fn WTFTimer__create(
    callback: unsafe extern "C" fn(*mut c_void),
    user_data: *mut c_void,
) -> *mut WTFTimer {
    let timer = Box::new(WTFTimer::new(callback, user_data));
    Box::into_raw(timer)
}

/// Update/schedule a timer.
///
/// # Safety
/// - `timer` must be a valid pointer from `WTFTimer__create`
#[unsafe(no_mangle)]
pub unsafe extern "C" fn WTFTimer__update(timer: *mut WTFTimer, delay_seconds: f64, repeat: bool) {
    if timer.is_null() {
        return;
    }
    let timer = &*timer;
    timer.update(delay_seconds, repeat);
}

/// Cancel a timer.
///
/// # Safety
/// - `timer` must be a valid pointer from `WTFTimer__create`
#[unsafe(no_mangle)]
pub unsafe extern "C" fn WTFTimer__cancel(timer: *mut WTFTimer) {
    if timer.is_null() {
        return;
    }
    let timer = &*timer;
    timer.cancel();
}

/// Check if timer is active.
///
/// # Safety
/// - `timer` must be a valid pointer from `WTFTimer__create`
#[unsafe(no_mangle)]
pub unsafe extern "C" fn WTFTimer__isActive(timer: *mut WTFTimer) -> bool {
    if timer.is_null() {
        return false;
    }
    let timer = &*timer;
    timer.is_active()
}

/// Get seconds until timer fires.
///
/// # Safety
/// - `timer` must be a valid pointer from `WTFTimer__create`
#[unsafe(no_mangle)]
pub unsafe extern "C" fn WTFTimer__secondsUntilTimer(timer: *mut WTFTimer) -> f64 {
    if timer.is_null() {
        return f64::INFINITY;
    }
    let timer = &*timer;
    timer.seconds_until_fire()
}

/// Deallocate a timer.
///
/// # Safety
/// - `timer` must be a valid pointer from `WTFTimer__create`
/// - Timer must not be used after this call
#[unsafe(no_mangle)]
pub unsafe extern "C" fn WTFTimer__deinit(timer: *mut WTFTimer) {
    if timer.is_null() {
        return;
    }
    // Cancel first
    let timer_ref = &*timer;
    timer_ref.cancel();

    // Then deallocate
    let _ = Box::from_raw(timer);
}

/// Run imminent timers (called by WTF for GC scheduling).
///
/// # Safety
/// This function is safe to call from any thread.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn WTFTimer__runIfImminent() {
    // For now, this is a no-op as we handle timers in the timer thread
    // In a more sophisticated implementation, this would check for
    // zero-delay timers and run them synchronously
}
