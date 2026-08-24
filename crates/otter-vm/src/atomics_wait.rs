//! ECMA-262 §25.4 Atomics wait / notify parking registry.
//!
//! The global, process-wide [`Registry`] keys blocked agents by
//! `(shared_buffer_id, byte_index)` so that an `Atomics.notify` on
//! one host thread can wake an `Atomics.wait` blocked on another
//! host thread.
//!
//! # Contents
//! - [`WaitOutcome`] — one of `Ok` / `TimedOut` / cancellation
//!   outcomes (the `NotEqual` pre-check lives in the caller).
//! - [`WaitAgent`] and [`WaitAgentHandle`] — one non-reused ECMA agent
//!   identity and its cross-thread lifecycle cancellation handle.
//! - [`park_until_notified`] — block the caller on `(buf_id, idx)`
//!   until the deadline, a notify wakes it, or the owning runtime is
//!   interrupted.
//! - [`notify_waiters`] — wake up to `count` waiters parked on
//!   `(buf_id, idx)`; returns the number actually woken.
//! - [`cancel_all_waiters`] — process-global test-harness teardown;
//!   ordinary runtime shutdown cancels only its [`WaitAgent`].
//!
//! # Invariants
//! - Each blocked agent registers exactly one [`ParkSlot`]; if the
//!   wait returns (notify, timeout, interrupt, or cancellation), it
//!   removes its slot from the registry before returning.
//! - Agent ids are monotonic and never reused. A stale lifecycle handle
//!   therefore cannot cancel a waiter owned by a later interpreter.
//! - Cancelling an agent publishes the cancelled state before taking a
//!   registry lock. Registration checks that state while holding the same
//!   lock, so no waiter can slip in after cancellation and sleep forever.
//! - `notify_waiters` drains up to `count` slots under the
//!   registry lock then notifies them outside the lock, so woken
//!   agents do not contend with the registry while resuming.
//!
//! # See also
//! - <https://tc39.es/ecma262/#sec-atomics.wait>
//! - <https://tc39.es/ecma262/#sec-atomics.notify>
//! - <https://tc39.es/ecma262/#sec-atomics.waitasync>

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, LazyLock, Mutex};
use std::time::{Duration, Instant};

use crate::InterruptFlag;

const INTERRUPT_POLL_INTERVAL: Duration = Duration::from_millis(10);

static NEXT_WAIT_AGENT_ID: AtomicU64 = AtomicU64::new(1);

#[derive(Debug)]
struct WaitAgentState {
    id: u64,
    cancelled: AtomicBool,
}

/// Owner token for one interpreter's ECMA agent wait lifecycle.
///
/// The interpreter keeps this value for its whole lifetime. Dropping it
/// cancels every blocking and async waiter registered by that interpreter,
/// even when cloneable [`WaitAgentHandle`] values still exist on host threads.
/// Agent identities are never reused, so an old host handle cannot affect a
/// later interpreter.
#[derive(Debug)]
pub struct WaitAgent {
    state: Arc<WaitAgentState>,
}

impl WaitAgent {
    /// Create a fresh process-unique wait-agent identity.
    #[must_use]
    pub fn new() -> Self {
        let id = NEXT_WAIT_AGENT_ID
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
                current.checked_add(1)
            })
            .expect("Atomics wait agent id space exhausted");
        Self {
            state: Arc::new(WaitAgentState {
                id,
                cancelled: AtomicBool::new(false),
            }),
        }
    }

    /// Clone a cross-thread handle that can cancel only this agent's waits.
    #[must_use]
    pub fn handle(&self) -> WaitAgentHandle {
        WaitAgentHandle {
            state: Arc::clone(&self.state),
        }
    }
}

impl Default for WaitAgent {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for WaitAgent {
    fn drop(&mut self) {
        cancel_agent(&self.state);
    }
}

/// Cloneable cross-thread cancellation handle for one [`WaitAgent`].
///
/// Cancellation is permanent and idempotent for this agent lifecycle. It does
/// not interfere with `Atomics.notify`, which remains addressed exclusively by
/// shared-buffer identity and byte index.
#[derive(Clone, Debug)]
pub struct WaitAgentHandle {
    state: Arc<WaitAgentState>,
}

impl WaitAgentHandle {
    /// Cancel this agent's currently registered waits and reject later waits.
    ///
    /// Returns the number of blocking waits woken by this call. Async waiter
    /// bookkeeping owned by the same agent is removed but is not included in
    /// the count.
    pub fn cancel(&self) -> usize {
        cancel_agent(&self.state)
    }

    fn id(&self) -> u64 {
        self.state.id
    }

    fn is_cancelled(&self) -> bool {
        self.state.cancelled.load(Ordering::Acquire)
    }
}

/// One blocked agent waiting on `(buf_id, idx)`.
struct ParkSlot {
    agent_id: u64,
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
static ASYNC_REGISTRY: LazyLock<Mutex<HashMap<(u64, usize), VecDeque<u64>>>> =
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
    /// The owning ECMA agent lifecycle was cancelled before completion.
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
    agent: &WaitAgentHandle,
) -> WaitOutcome {
    if agent.is_cancelled() {
        return WaitOutcome::Cancelled;
    }
    let slot = Arc::new(ParkSlot {
        agent_id: agent.id(),
        state: Mutex::new(ParkState::default()),
        cv: Condvar::new(),
    });

    {
        let mut reg = REGISTRY.lock().expect("Atomics wait registry poisoned");
        if agent.is_cancelled() {
            return WaitOutcome::Cancelled;
        }
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
        if agent.is_cancelled() {
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
            if agent.is_cancelled() {
                break WaitOutcome::Cancelled;
            }
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
/// tracks wake counts here so `Atomics.notify` observes async waiters after its
/// blocking-waiter quota. Returns `false` when the owning agent was already
/// cancelled and no registration was made.
pub fn register_async_waiter(buf_id: u64, idx: usize, agent: &WaitAgentHandle) -> bool {
    let mut reg = ASYNC_REGISTRY
        .lock()
        .expect("Atomics async wait registry poisoned");
    if agent.is_cancelled() {
        return false;
    }
    reg.entry((buf_id, idx)).or_default().push_back(agent.id());
    true
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

fn cancel_agent(state: &WaitAgentState) -> usize {
    if state.cancelled.swap(true, Ordering::AcqRel) {
        return 0;
    }

    let drained: Vec<Arc<ParkSlot>> = {
        let mut reg = REGISTRY.lock().expect("Atomics wait registry poisoned");
        let mut drained = Vec::new();
        for slots in reg.values_mut() {
            slots.retain(|slot| {
                if slot.agent_id == state.id {
                    drained.push(Arc::clone(slot));
                    false
                } else {
                    true
                }
            });
        }
        reg.retain(|_, slots| !slots.is_empty());
        drained
    };

    {
        let mut reg = ASYNC_REGISTRY
            .lock()
            .expect("Atomics async wait registry poisoned");
        for waiters in reg.values_mut() {
            waiters.retain(|agent_id| *agent_id != state.id);
        }
        reg.retain(|_, waiters| !waiters.is_empty());
    }

    let cancelled = drained.len();
    for slot in drained {
        let mut slot_state = slot.state.lock().expect("Atomics wait slot poisoned");
        slot_state.cancelled = true;
        drop(slot_state);
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
        let agent = WaitAgent::new();
        let r = park_until_notified(42, 0, Some(Duration::ZERO), None, &agent.handle());
        assert_eq!(r, WaitOutcome::TimedOut);
    }

    #[test]
    fn notify_wakes_park() {
        let _guard = TEST_LOCK.lock().unwrap();
        static WOKEN: AtomicUsize = AtomicUsize::new(0);
        WOKEN.store(0, Ordering::Relaxed);
        let id: u64 = 0xfeed_beef;
        let agent = WaitAgent::new();
        let waiter_agent = agent.handle();

        let h = thread::spawn(move || {
            let r = park_until_notified(id, 7, Some(Duration::from_secs(5)), None, &waiter_agent);
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
        let agent = WaitAgent::new();
        let waiter_agent = agent.handle();
        let h = thread::spawn(move || {
            park_until_notified(77, 1, None, Some(&waiter_flag), &waiter_agent)
        });

        wait_for_registered(77, 1);

        flag.interrupt();
        assert_eq!(h.join().unwrap(), WaitOutcome::Interrupted);
    }

    #[test]
    fn cancel_all_waiters_breaks_infinite_wait() {
        let _guard = TEST_LOCK.lock().unwrap();
        let agent = WaitAgent::new();
        let waiter_agent = agent.handle();
        let h = thread::spawn(move || park_until_notified(88, 2, None, None, &waiter_agent));

        wait_for_registered(88, 2);

        assert_eq!(cancel_all_waiters(), 1);
        assert_eq!(h.join().unwrap(), WaitOutcome::Cancelled);
    }

    #[test]
    fn cancelling_one_agent_does_not_cancel_another() {
        let _guard = TEST_LOCK.lock().unwrap();
        let id = 0xcafe;
        let agent_a = WaitAgent::new();
        let agent_b = WaitAgent::new();
        let handle_a = agent_a.handle();
        let waiter_a = handle_a.clone();
        let waiter_b = agent_b.handle();
        let (done_a_tx, done_a_rx) = std::sync::mpsc::sync_channel(1);
        let (done_b_tx, done_b_rx) = std::sync::mpsc::sync_channel(1);

        let thread_a = thread::spawn(move || {
            let outcome = park_until_notified(id, 3, None, None, &waiter_a);
            done_a_tx.send(outcome).unwrap();
        });
        let thread_b = thread::spawn(move || {
            let outcome = park_until_notified(id, 3, None, None, &waiter_b);
            done_b_tx.send(outcome).unwrap();
        });
        wait_for_registered_count(id, 3, 2);

        assert_eq!(handle_a.cancel(), 1);
        assert_eq!(
            done_a_rx.recv_timeout(Duration::from_millis(250)).unwrap(),
            WaitOutcome::Cancelled
        );
        assert!(matches!(
            done_b_rx.try_recv(),
            Err(std::sync::mpsc::TryRecvError::Empty)
        ));

        assert_eq!(notify_waiters(id, 3, 1), 1);
        assert_eq!(
            done_b_rx.recv_timeout(Duration::from_secs(1)).unwrap(),
            WaitOutcome::Ok
        );
        thread_a.join().unwrap();
        thread_b.join().unwrap();
    }

    #[test]
    fn dropping_agent_owner_cancels_registered_wait() {
        let _guard = TEST_LOCK.lock().unwrap();
        let agent = WaitAgent::new();
        let waiter = agent.handle();
        let h = thread::spawn(move || park_until_notified(99, 4, None, None, &waiter));

        wait_for_registered(99, 4);
        drop(agent);

        assert_eq!(h.join().unwrap(), WaitOutcome::Cancelled);
    }

    #[test]
    fn cancelling_agent_removes_only_its_async_registration() {
        let _guard = TEST_LOCK.lock().unwrap();
        let agent_a = WaitAgent::new();
        let agent_b = WaitAgent::new();
        let handle_a = agent_a.handle();
        let handle_b = agent_b.handle();
        assert!(register_async_waiter(101, 5, &handle_a));
        assert!(register_async_waiter(101, 5, &handle_b));

        assert_eq!(handle_a.cancel(), 0);
        assert_eq!(notify_async_waiters(101, 5, usize::MAX), 1);
    }

    fn wait_for_registered(id: u64, idx: usize) {
        wait_for_registered_count(id, idx, 1);
    }

    fn wait_for_registered_count(id: u64, idx: usize, count: usize) {
        let deadline = Instant::now() + Duration::from_secs(1);
        while Instant::now() < deadline {
            let occupied = {
                let reg = REGISTRY.lock().unwrap();
                reg.get(&(id, idx)).is_some_and(|v| v.len() >= count)
            };
            if occupied {
                return;
            }
            thread::sleep(Duration::from_millis(10));
        }
        panic!("waiter did not register");
    }
}
