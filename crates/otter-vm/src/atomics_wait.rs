//! ECMA-262 §25.4 Atomics wait / notify parking registry.
//!
//! Slice 19b (workers phase 2) infrastructure: the global, process
//! wide [`WaitRegistry`] keys parked threads by `(shared_buffer_id,
//! byte_index)` so that an `Atomics.notify` on one host thread can
//! wake an `Atomics.wait` blocked on another host thread.
//!
//! Single-thread foundation behaviour: when the engine runs on a
//! single host thread, no other thread can call notify while the
//! main thread is parked. The registry still implements the spec
//! `Atomics.wait` algorithm faithfully — it parks the calling
//! thread on `std::thread::park_timeout` until either a `notify`
//! arrives from another thread (impossible without 19c agents,
//! tracked in `docs/workers-262-plan.md`) or the timeout expires.
//!
//! # Contents
//! - [`WaitOutcome`] — one of `Ok` / `TimedOut` (the `NotEqual`
//!   pre-check lives in the caller).
//! - [`park_until_notified`] — block the caller on `(buf_id, idx)`
//!   until the deadline or a notify wakes it.
//! - [`notify_waiters`] — wake up to `count` waiters parked on
//!   `(buf_id, idx)`; returns the number actually woken.
//!
//! # Invariants
//! - Each parked thread registers exactly one [`ParkSlot`]; if the
//!   thread wakes (notify or timeout), it removes its slot from
//!   the registry before returning.
//! - `notify_waiters` drains up to `count` slots under the
//!   registry lock then unparks them outside the lock, so the
//!   notified threads do not contend with the registry while
//!   waking.
//!
//! # See also
//! - <https://tc39.es/ecma262/#sec-atomics.wait>
//! - <https://tc39.es/ecma262/#sec-atomics.notify>
//! - <https://tc39.es/ecma262/#sec-atomics.waitasync>

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock, Mutex};
use std::thread::{self, Thread};
use std::time::{Duration, Instant};

/// One parked thread waiting on `(buf_id, idx)`.
struct ParkSlot {
    /// Thread to unpark on notify.
    handle: Thread,
    /// Flips to `true` when a notify targets this slot. The waker
    /// stores `true` before calling `handle.unpark()`, and the
    /// parked thread reads the flag to distinguish spurious
    /// `park_timeout` wakeups from genuine notifications.
    notified: AtomicBool,
}

/// Global wait registry. Keyed by `(buf_id, idx)` so wakes target
/// the same byte address as the `Atomics.wait` call.
type Registry = HashMap<(u64, usize), Vec<Arc<ParkSlot>>>;

static REGISTRY: LazyLock<Mutex<Registry>> = LazyLock::new(|| Mutex::new(HashMap::new()));

/// Outcome of [`park_until_notified`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WaitOutcome {
    /// Thread was woken by a matching [`notify_waiters`] call.
    Ok,
    /// Deadline expired before any notify reached this slot.
    TimedOut,
}

/// Park the calling thread on `(buf_id, idx)` until either a
/// notify wakes it or the deadline elapses.
///
/// `timeout = None` is infinite wait per spec; the caller is
/// responsible for honouring the spec "+∞ means no timeout"
/// mapping before invoking this function. A `Duration::ZERO`
/// caller will return [`WaitOutcome::TimedOut`] immediately if no
/// notify is already queued (matches d8 semantics).
pub fn park_until_notified(buf_id: u64, idx: usize, timeout: Option<Duration>) -> WaitOutcome {
    let slot = Arc::new(ParkSlot {
        handle: thread::current(),
        notified: AtomicBool::new(false),
    });

    // Register this slot in the global registry.
    {
        let mut reg = REGISTRY.lock().expect("Atomics wait registry poisoned");
        reg.entry((buf_id, idx))
            .or_default()
            .push(Arc::clone(&slot));
    }

    let deadline = timeout.map(|t| Instant::now().checked_add(t).unwrap_or_else(Instant::now));
    let outcome = loop {
        match deadline {
            Some(d) => {
                let now = Instant::now();
                if now >= d {
                    break WaitOutcome::TimedOut;
                }
                thread::park_timeout(d - now);
            }
            None => thread::park(),
        }
        if slot.notified.load(Ordering::Acquire) {
            break WaitOutcome::Ok;
        }
        // Spurious wakeup — keep looping until deadline or notify.
    };

    // Remove our slot from the registry. On Ok we may have been
    // drained already; on TimedOut we need to evict ourselves so a
    // future notify does not target a dead slot.
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
        slot.notified.store(true, Ordering::Release);
        slot.handle.unpark();
    }
    woken
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;
    use std::thread;
    use std::time::Duration;

    #[test]
    fn park_with_zero_timeout_returns_timed_out() {
        let r = park_until_notified(42, 0, Some(Duration::ZERO));
        assert_eq!(r, WaitOutcome::TimedOut);
    }

    #[test]
    fn notify_wakes_park() {
        static WOKEN: AtomicUsize = AtomicUsize::new(0);
        WOKEN.store(0, Ordering::Relaxed);
        let id: u64 = 0xfeed_beef;

        let h = thread::spawn(move || {
            let r = park_until_notified(id, 7, Some(Duration::from_secs(5)));
            if r == WaitOutcome::Ok {
                WOKEN.fetch_add(1, Ordering::Relaxed);
            }
        });

        // Give the parker time to register itself.
        let deadline = Instant::now() + Duration::from_secs(1);
        while Instant::now() < deadline {
            let occupied = {
                let reg = REGISTRY.lock().unwrap();
                reg.get(&(id, 7)).is_some_and(|v| !v.is_empty())
            };
            if occupied {
                break;
            }
            thread::sleep(Duration::from_millis(10));
        }

        let n = notify_waiters(id, 7, 1);
        assert_eq!(n, 1);
        h.join().unwrap();
        assert_eq!(WOKEN.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn notify_on_empty_returns_zero() {
        let n = notify_waiters(0xdead, 0, 999);
        assert_eq!(n, 0);
    }
}
