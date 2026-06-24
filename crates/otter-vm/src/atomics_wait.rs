//! ECMA-262 §25.4 Atomics wait / notify parking registry.
//!
//! The global, process-wide [`WaitRegistry`] keys blocked agents by
//! `(shared_buffer_id, byte_index)` so that an `Atomics.notify` on
//! one host thread can wake an `Atomics.wait` blocked on another
//! host thread.
//!
//! # Contents
//! - [`WaitOutcome`] — one of `Ok` / `TimedOut` / cancellation
//!   outcomes (the `NotEqual` pre-check lives in the caller).
//! - [`park_until_notified`] — block the caller on `(buf_id, idx)`
//!   until the deadline, a notify wakes it, or the owning runtime is
//!   interrupted.
//! - [`notify_waiters`] — wake up to `count` waiters parked on
//!   `(buf_id, idx)`; returns the number actually woken.
//! - [`cancel_all_waiters`] — wake all blocked agents during host
//!   shutdown / test harness cancellation.
//!
//! # Invariants
//! - Each blocked agent registers exactly one [`ParkSlot`]; if the
//!   wait returns (notify, timeout, interrupt, or cancellation), it
//!   removes its slot from the registry before returning.
//! - `notify_waiters` drains up to `count` slots under the
//!   registry lock then notifies them outside the lock, so woken
//!   agents do not contend with the registry while resuming.
//!
//! # See also
//! - <https://tc39.es/ecma262/#sec-atomics.wait>
//! - <https://tc39.es/ecma262/#sec-atomics.notify>
//! - <https://tc39.es/ecma262/#sec-atomics.waitasync>

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Condvar, LazyLock, Mutex};
use std::time::{Duration, Instant};

use crate::InterruptFlag;

const INTERRUPT_POLL_INTERVAL: Duration = Duration::from_millis(10);

/// One blocked agent waiting on `(buf_id, idx)`.
struct ParkSlot {
    state: Mutex<ParkState>,
    cv: Condvar,
}

#[derive(Debug, Default)]
struct ParkState {
    notified: bool,
    cancelled: bool,
}

/// Global wait registry. Keyed by `(buf_id, idx)` so wakes target
/// the same byte address as the `Atomics.wait` call.
type Registry = HashMap<(u64, usize), Vec<Arc<ParkSlot>>>;

static REGISTRY: LazyLock<Mutex<Registry>> = LazyLock::new(|| Mutex::new(HashMap::new()));
static ASYNC_REGISTRY: LazyLock<Mutex<HashMap<(u64, usize), VecDeque<()>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Outcome of [`park_until_notified`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WaitOutcome {
    /// Thread was woken by a matching [`notify_waiters`] call.
    Ok,
    /// Deadline expired before any notify reached this slot.
    TimedOut,
    /// The owning runtime was interrupted while blocked.
    Interrupted,
    /// Host shutdown cancelled the wait before it could complete.
    Cancelled,
}

/// Park the calling thread on `(buf_id, idx)` until either a
/// notify wakes it, the deadline elapses, or the runtime interrupt
/// flag is tripped.
///
/// `timeout = None` is infinite wait per spec; the caller is
/// responsible for honouring the spec "+∞ means no timeout"
/// mapping before invoking this function. A `Duration::ZERO`
/// caller will return [`WaitOutcome::TimedOut`] immediately if no
/// notify is already queued (matches d8 semantics).
pub fn park_until_notified(
    buf_id: u64,
    idx: usize,
    timeout: Option<Duration>,
    interrupt: Option<&InterruptFlag>,
) -> WaitOutcome {
    let slot = Arc::new(ParkSlot {
        state: Mutex::new(ParkState::default()),
        cv: Condvar::new(),
    });

    {
        let mut reg = REGISTRY.lock().expect("Atomics wait registry poisoned");
        reg.entry((buf_id, idx))
            .or_default()
            .push(Arc::clone(&slot));
    }

    let deadline = timeout.map(|t| Instant::now().checked_add(t).unwrap_or_else(Instant::now));
    let mut state = slot.state.lock().expect("Atomics wait slot poisoned");
    let outcome = loop {
        if state.notified {
            break WaitOutcome::Ok;
        }
        if state.cancelled {
            break WaitOutcome::Cancelled;
        }
        if interrupt.is_some_and(InterruptFlag::is_set) {
            break WaitOutcome::Interrupted;
        }

        let wait_for = match deadline {
            Some(d) => match d.checked_duration_since(Instant::now()) {
                Some(remaining) => remaining.min(INTERRUPT_POLL_INTERVAL),
                None => break WaitOutcome::TimedOut,
            },
            None => INTERRUPT_POLL_INTERVAL,
        };

        let (next, timeout_result) = slot
            .cv
            .wait_timeout(state, wait_for)
            .expect("Atomics wait slot poisoned");
        state = next;
        if timeout_result.timed_out()
            && deadline.is_some_and(|d| Instant::now() >= d)
            && !state.notified
            && !state.cancelled
        {
            if interrupt.is_some_and(InterruptFlag::is_set) {
                break WaitOutcome::Interrupted;
            }
            break WaitOutcome::TimedOut;
        }
    };
    drop(state);

    {
        let mut reg = REGISTRY.lock().expect("Atomics wait registry poisoned");
        if let Some(slots) = reg.get_mut(&(buf_id, idx)) {
            slots.retain(|s| !Arc::ptr_eq(s, &slot));
            if slots.is_empty() {
                reg.remove(&(buf_id, idx));
            }
        }
    }
    outcome
}

/// Wake up to `count` threads parked on `(buf_id, idx)`. Returns
/// the number actually woken. `count = usize::MAX` means "all
/// waiters" per spec defaulting.
pub fn notify_waiters(buf_id: u64, idx: usize, count: usize) -> usize {
    if count == 0 {
        return 0;
    }
    let drained: Vec<Arc<ParkSlot>> = {
        let mut reg = REGISTRY.lock().expect("Atomics wait registry poisoned");
        let Some(slots) = reg.get_mut(&(buf_id, idx)) else {
            return 0;
        };
        let n = count.min(slots.len());
        let drained: Vec<_> = slots.drain(..n).collect();
        if slots.is_empty() {
            reg.remove(&(buf_id, idx));
        }
        drained
    };
    let woken = drained.len();
    for slot in drained {
        let mut state = slot.state.lock().expect("Atomics wait slot poisoned");
        state.notified = true;
        drop(state);
        slot.cv.notify_one();
    }
    woken
}

/// Register a non-blocking `Atomics.waitAsync` waiter. The current foundation
/// tracks wake counts here so `Atomics.notify` observes async waiters in the
/// same waiter list as blocking waiters.
pub fn register_async_waiter(buf_id: u64, idx: usize) {
    let mut reg = ASYNC_REGISTRY
        .lock()
        .expect("Atomics async wait registry poisoned");
    reg.entry((buf_id, idx)).or_default().push_back(());
}

/// Wake async waiters registered through [`register_async_waiter`].
pub fn notify_async_waiters(buf_id: u64, idx: usize, count: usize) -> usize {
    if count == 0 {
        return 0;
    }
    let mut reg = ASYNC_REGISTRY
        .lock()
        .expect("Atomics async wait registry poisoned");
    let Some(waiters) = reg.get_mut(&(buf_id, idx)) else {
        return 0;
    };
    let n = count.min(waiters.len());
    for _ in 0..n {
        waiters.pop_front();
    }
    if waiters.is_empty() {
        reg.remove(&(buf_id, idx));
    }
    n
}

/// Cancel every currently blocked waiter and wake its owning host
/// thread. This is a host lifecycle hook, not an ECMAScript
/// operation: Test262 uses it when a per-test watchdog fires or when
/// it tears down leftover `$262.agent` workers.
pub fn cancel_all_waiters() -> usize {
    let drained: Vec<Arc<ParkSlot>> = {
        let mut reg = REGISTRY.lock().expect("Atomics wait registry poisoned");
        reg.drain().flat_map(|(_, slots)| slots).collect()
    };
    ASYNC_REGISTRY
        .lock()
        .expect("Atomics async wait registry poisoned")
        .clear();
    let cancelled = drained.len();
    for slot in drained {
        let mut state = slot.state.lock().expect("Atomics wait slot poisoned");
        state.cancelled = true;
        drop(state);
        slot.cv.notify_one();
    }
    cancelled
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{LazyLock, Mutex};
    use std::thread;

    static TEST_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

    #[test]
    fn park_with_zero_timeout_returns_timed_out() {
        let _guard = TEST_LOCK.lock().unwrap();
        let r = park_until_notified(42, 0, Some(Duration::ZERO), None);
        assert_eq!(r, WaitOutcome::TimedOut);
    }

    #[test]
    fn notify_wakes_park() {
        let _guard = TEST_LOCK.lock().unwrap();
        static WOKEN: AtomicUsize = AtomicUsize::new(0);
        WOKEN.store(0, Ordering::Relaxed);
        let id: u64 = 0xfeed_beef;

        let h = thread::spawn(move || {
            let r = park_until_notified(id, 7, Some(Duration::from_secs(5)), None);
            if r == WaitOutcome::Ok {
                WOKEN.fetch_add(1, Ordering::Relaxed);
            }
        });

        wait_for_registered(id, 7);

        let n = notify_waiters(id, 7, 1);
        assert_eq!(n, 1);
        h.join().unwrap();
        assert_eq!(WOKEN.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn notify_on_empty_returns_zero() {
        let _guard = TEST_LOCK.lock().unwrap();
        let n = notify_waiters(0xdead, 0, 999);
        assert_eq!(n, 0);
    }

    #[test]
    fn interrupt_breaks_infinite_wait() {
        let _guard = TEST_LOCK.lock().unwrap();
        let flag = InterruptFlag::new();
        let waiter_flag = flag.clone();
        let h = thread::spawn(move || park_until_notified(77, 1, None, Some(&waiter_flag)));

        wait_for_registered(77, 1);

        flag.interrupt();
        assert_eq!(h.join().unwrap(), WaitOutcome::Interrupted);
    }

    #[test]
    fn cancel_all_waiters_breaks_infinite_wait() {
        let _guard = TEST_LOCK.lock().unwrap();
        let h = thread::spawn(move || park_until_notified(88, 2, None, None));

        wait_for_registered(88, 2);

        assert_eq!(cancel_all_waiters(), 1);
        assert_eq!(h.join().unwrap(), WaitOutcome::Cancelled);
    }

    fn wait_for_registered(id: u64, idx: usize) {
        let deadline = Instant::now() + Duration::from_secs(1);
        while Instant::now() < deadline {
            let occupied = {
                let reg = REGISTRY.lock().unwrap();
                reg.get(&(id, idx)).is_some_and(|v| !v.is_empty())
            };
            if occupied {
                return;
            }
            thread::sleep(Duration::from_millis(10));
        }
        panic!("waiter did not register");
    }
}
