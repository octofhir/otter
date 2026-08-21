//! Runtime scheduling boundary and Tokio default implementation.
//!
//! The runtime exposes a product-level event-loop abstraction so public handles
//! can be `Send + Sync` while VM and GC internals stay owned by a single
//! isolate runner.
//!
//! # Contents
//!
//! - [`EventLoop`] — host scheduling trait.
//! - [`TokioRuntimeHost`] — public, shareable Tokio-backed host services.
//! - [`TokioEventLoop`] — isolate-facing implementation behind that host.
//! - Timer sink and HTTPS host-service wiring support types.
//!
//! # Invariants
//!
//! - The VM crate does not import Tokio types.
//! - Tokio workers only emit timer tokens or owned host-service
//!   results; JS callback dispatch stays on the isolate runner.
//!
//! # See also
//!
//! - [Event loop](../../../docs/book/src/engine/event-loop.md)
//! - [Runtime architecture](../../../docs/book/src/engine/architecture.md)

use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap};
use std::future::Future;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::sync::Notify;

/// Shareable Tokio-backed host services for one application process.
///
/// A browser normally constructs one host and clones it into every per-page
/// [`crate::RuntimeBuilder`]. The resulting isolates retain separate heaps,
/// globals, microtask queues, and capability state while sharing the executor,
/// HTTP client, and timer registry. A CLI can use the same host with one
/// isolate.
///
/// This is a ready-made implementation, not a requirement for direct
/// embedders. Layer A can instead install custom timer and completion sinks
/// driven by another event loop.
#[derive(Clone)]
pub struct TokioRuntimeHost {
    event_loop: TokioEventLoop,
}

impl TokioRuntimeHost {
    /// Create a host that owns a new multi-thread Tokio runtime.
    ///
    /// # Errors
    /// Returns [`std::io::Error`] when Tokio cannot create its worker threads.
    pub fn new() -> Result<Self, std::io::Error> {
        Ok(Self {
            event_loop: TokioEventLoop::owned()?,
        })
    }

    /// Wrap an embedder-owned Tokio runtime.
    ///
    /// The embedder must keep the runtime alive for at least as long as every
    /// isolate built from this host.
    #[must_use]
    pub fn from_handle(handle: tokio::runtime::Handle) -> Self {
        Self {
            event_loop: TokioEventLoop::from_handle(handle),
        }
    }

    /// Reuse the current Tokio runtime, or own a new one when called outside
    /// an executor context.
    ///
    /// # Errors
    /// Returns [`std::io::Error`] when fallback runtime creation fails.
    pub fn current_or_new() -> Result<Self, std::io::Error> {
        Ok(Self {
            event_loop: TokioEventLoop::current_or_owned()?,
        })
    }

    /// The shared executor handle for host integrations that perform owned,
    /// non-GC async work.
    #[must_use]
    pub fn handle(&self) -> tokio::runtime::Handle {
        self.event_loop.handle()
    }

    pub(crate) fn event_loop(&self) -> TokioEventLoop {
        self.event_loop.clone()
    }
}

impl std::fmt::Debug for TokioRuntimeHost {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TokioRuntimeHost")
            .field("owns_runtime", &self.event_loop.owned.is_some())
            .finish_non_exhaustive()
    }
}

/// Runtime-side sink notified when a host timer fires.
///
/// Implementations should only ship the opaque [`TimerToken`] back
/// to the isolate/runtime boundary. They must not retain VM or GC
/// state.
pub(crate) trait TimerWake: Send + Sync + 'static {
    /// Notify the runtime that `token` fired.
    fn timer_fired(&self, token: TimerToken);
}

/// Liveness bit for runtime work.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuntimeLiveness {
    /// Keeps `run_until_idle` alive.
    Ref,
    /// May complete while the loop is already being driven but does
    /// not prevent idle shutdown.
    Unref,
}

/// Timer identifier.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct TimerToken(pub u64);

/// Timer scheduling request.
#[derive(Debug, Clone)]
pub(crate) struct TimerRequest {
    /// Delay from now.
    pub delay: Duration,
    /// Optional interval repeat period.
    pub repeat: Option<Duration>,
}

/// Scheduling boundary owned by `otter-runtime`.
pub(crate) trait EventLoop: Send + Sync + 'static {
    /// Schedule a timer and return its token.
    fn schedule_timer(&self, request: TimerRequest, wake: Arc<dyn TimerWake>) -> TimerToken;

    /// Cancel a scheduled timer.
    fn cancel_timer(&self, token: TimerToken) -> bool;
}

/// Tokio-backed default event loop.
#[derive(Clone)]
pub(crate) struct TokioEventLoop {
    handle: tokio::runtime::Handle,
    owned: Option<Arc<tokio::runtime::Runtime>>,
    http_client: reqwest::Client,
    timers: Arc<TimerDriver>,
}

impl std::fmt::Debug for TokioEventLoop {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TokioEventLoop")
            .field("owned", &self.owned.is_some())
            .finish_non_exhaustive()
    }
}

impl TokioEventLoop {
    /// Wrap an embedder-provided Tokio handle.
    #[must_use]
    pub(crate) fn from_handle(handle: tokio::runtime::Handle) -> Self {
        let timers = TimerDriver::spawn(&handle);
        Self {
            handle,
            owned: None,
            http_client: reqwest::Client::new(),
            timers,
        }
    }

    /// Create an owned multi-thread Tokio runtime.
    ///
    /// # Errors
    /// Returns [`std::io::Error`] if Tokio cannot create worker
    /// threads.
    pub(crate) fn owned() -> Result<Self, std::io::Error> {
        let runtime = Arc::new(tokio::runtime::Runtime::new()?);
        let handle = runtime.handle().clone();
        let timers = TimerDriver::spawn(&handle);
        Ok(Self {
            handle,
            owned: Some(runtime),
            http_client: reqwest::Client::new(),
            timers,
        })
    }

    /// Use the current Tokio runtime when present; otherwise create
    /// an owned runtime.
    ///
    /// # Errors
    /// Returns [`std::io::Error`] if no current runtime exists and an
    /// owned runtime cannot be created.
    pub(crate) fn current_or_owned() -> Result<Self, std::io::Error> {
        match tokio::runtime::Handle::try_current() {
            Ok(handle) => Ok(Self::from_handle(handle)),
            Err(_) => Self::owned(),
        }
    }

    /// The backing Tokio runtime handle. Host resources that own
    /// long-lived async IO (the HTTP server, future `node:net`) spawn
    /// their accept/serve loops onto it so all IO shares the one runtime.
    #[must_use]
    pub(crate) fn handle(&self) -> tokio::runtime::Handle {
        self.handle.clone()
    }

    /// Async provider used by the remote graph fetch/prefetch phase.
    pub(crate) fn remote_module_provider(
        &self,
    ) -> Arc<dyn crate::module_loader::RemoteModuleProvider> {
        Arc::new(TokioRemoteModuleProvider {
            client: self.http_client.clone(),
        })
    }

    /// Block on a future using the backing Tokio runtime.
    ///
    /// This is intended for CLI and non-async embedders. Async callers
    /// should use the `async` methods on [`crate::Otter`] directly.
    pub(crate) fn block_on<F: Future>(&self, future: F) -> F::Output {
        if let Some(runtime) = &self.owned {
            return runtime.block_on(future);
        }
        tokio::task::block_in_place(|| self.handle.block_on(future))
    }

}

/// One armed timer as ordered inside [`TimerDriver`].
///
/// Ordering is `(deadline, seq)`: earliest deadline first, and among timers
/// due at the same instant the one armed first. `seq` is what gives
/// same-millisecond timers their arming order, the way libuv compares
/// `(timeout, id)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ArmedTimer {
    deadline: Instant,
    seq: u64,
    token: TimerToken,
}

impl Ord for ArmedTimer {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.deadline
            .cmp(&other.deadline)
            .then_with(|| self.seq.cmp(&other.seq))
    }
}

impl PartialOrd for ArmedTimer {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

/// What the driver needs to fire a token, kept out of the heap so a
/// cancellation is a single map removal.
struct TimerRecord {
    wake: Arc<dyn TimerWake>,
    /// `Some(period)` for `setInterval`; the driver re-arms itself.
    repeat: Option<Duration>,
    /// Sequence of the heap entry currently standing for this token. A popped
    /// entry whose `seq` no longer matches was superseded by a re-arm.
    seq: u64,
}

#[derive(Default)]
struct TimerDriverState {
    heap: BinaryHeap<Reverse<ArmedTimer>>,
    records: HashMap<TimerToken, TimerRecord>,
    next_seq: u64,
    next_token: u64,
}

/// Single-task timer wheel behind [`TokioEventLoop`].
///
/// Every host timer lives in one deadline-ordered heap drained by one Tokio
/// task, so the inbox receives `TimerFired` messages in deadline order and, on
/// a tie, in arming order. Sleeping each timer in its own spawned task cannot
/// promise that: two `sleep(1ms)` tasks wake on different workers and race to
/// the inbox.
struct TimerDriver {
    state: Mutex<TimerDriverState>,
    /// Rung whenever the earliest deadline may have moved closer.
    wakeup: Notify,
}

impl TimerDriver {
    /// Build a driver and put its drain loop on `handle`.
    fn spawn(handle: &tokio::runtime::Handle) -> Arc<Self> {
        let driver = Arc::new(Self {
            state: Mutex::new(TimerDriverState {
                next_token: 1,
                ..TimerDriverState::default()
            }),
            wakeup: Notify::new(),
        });
        handle.spawn(Self::drain(Arc::clone(&driver)));
        driver
    }

    /// Arm a timer and return the token the VM stores its callback under.
    fn arm(&self, request: &TimerRequest, wake: Arc<dyn TimerWake>) -> TimerToken {
        let deadline = Instant::now() + request.delay;
        let repeat = request
            .repeat
            .map(|period| period.max(Duration::from_millis(1)));
        let token = {
            let mut state = self.state.lock().expect("timer driver poisoned");
            let token = TimerToken(state.next_token);
            state.next_token += 1;
            let seq = state.next_seq;
            state.next_seq += 1;
            state.records.insert(token, TimerRecord { wake, repeat, seq });
            state.heap.push(Reverse(ArmedTimer {
                deadline,
                seq,
                token,
            }));
            token
        };
        self.wakeup.notify_one();
        token
    }

    /// Drop a timer's record. Its heap entry is skipped when it surfaces.
    fn disarm(&self, token: TimerToken) -> bool {
        let removed = {
            let mut state = self.state.lock().expect("timer driver poisoned");
            state.records.remove(&token).is_some()
        };
        if removed {
            self.wakeup.notify_one();
        }
        removed
    }

    /// Take everything due, in order, and report when the next timer is due.
    fn take_due(&self) -> (Vec<(Arc<dyn TimerWake>, TimerToken)>, Option<Instant>) {
        let mut state = self.state.lock().expect("timer driver poisoned");
        let now = Instant::now();
        let mut due: Vec<(Arc<dyn TimerWake>, TimerToken)> = Vec::new();
        while let Some(&Reverse(entry)) = state.heap.peek() {
            if entry.deadline > now {
                break;
            }
            state.heap.pop();
            let (wake, repeat) = match state.records.get(&entry.token) {
                Some(record) if record.seq == entry.seq => {
                    (Arc::clone(&record.wake), record.repeat)
                }
                // Cancelled, or superseded by a re-arm.
                _ => continue,
            };
            match repeat {
                Some(period) => {
                    let seq = state.next_seq;
                    state.next_seq += 1;
                    if let Some(record) = state.records.get_mut(&entry.token) {
                        record.seq = seq;
                    }
                    state.heap.push(Reverse(ArmedTimer {
                        deadline: now + period,
                        seq,
                        token: entry.token,
                    }));
                }
                None => {
                    state.records.remove(&entry.token);
                }
            }
            due.push((wake, entry.token));
        }
        let next = state.heap.peek().map(|Reverse(entry)| entry.deadline);
        (due, next)
    }

    /// Fire due timers forever, sleeping until the earliest deadline.
    async fn drain(driver: Arc<Self>) {
        loop {
            // Registered before the heap is read so an arm racing this pass
            // still wakes the sleep below.
            let wakeup = driver.wakeup.notified();
            tokio::pin!(wakeup);

            let (due, next) = driver.take_due();
            for (wake, token) in due {
                wake.timer_fired(token);
            }

            match next {
                Some(deadline) => {
                    let sleep = tokio::time::sleep_until(tokio::time::Instant::from_std(deadline));
                    tokio::pin!(sleep);
                    tokio::select! {
                        () = &mut sleep => {}
                        () = &mut wakeup => {}
                    }
                }
                None => wakeup.await,
            }
        }
    }
}

#[derive(Debug)]
struct TokioRemoteModuleProvider {
    client: reqwest::Client,
}

impl crate::module_loader::RemoteModuleProvider for TokioRemoteModuleProvider {
    fn fetch(
        &self,
        request: crate::module_loader::RemoteModuleRequest,
    ) -> crate::module_loader::RemoteModuleFuture {
        let client = self.client.clone();
        Box::pin(async move {
            let url = request.url;
            let cancellation = request.cancellation;
            tokio::select! {
                () = cancellation.cancelled() => {
                    Err(crate::module_loader::RemoteModuleError::Cancelled)
                }
                result = async move {
                let resp = client
                    .get(&url)
                    .send()
                    .await
                    .map_err(|error| crate::module_loader::RemoteModuleError::Fetch {
                        url: url.clone(),
                        message: format!("HTTPS request failed: {error}"),
                    })?;
                if !resp.status().is_success() {
                    return Err(crate::module_loader::RemoteModuleError::Fetch {
                        url: url.clone(),
                        message: format!("HTTPS status {}", resp.status()),
                    });
                }
                let final_url = resp.url().to_string();
                let content_type = resp
                    .headers()
                    .get(reqwest::header::CONTENT_TYPE)
                    .and_then(|v| v.to_str().ok())
                    .map(str::to_string);
                let source = resp
                    .text()
                    .await
                    .map_err(|error| crate::module_loader::RemoteModuleError::Fetch {
                        url: url.clone(),
                        message: format!("HTTPS body read failed: {error}"),
                    })?;
                Ok(crate::module_loader::RemoteModuleSource {
                    source,
                    content_type,
                    final_url,
                })
                } => result,
            }
        })
    }
}

impl EventLoop for TokioEventLoop {
    fn schedule_timer(&self, request: TimerRequest, wake: Arc<dyn TimerWake>) -> TimerToken {
        self.timers.arm(&request, wake)
    }

    fn cancel_timer(&self, token: TimerToken) -> bool {
        self.timers.disarm(token)
    }
}
