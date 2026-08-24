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
//! - Timer tokens stay within JavaScript's exact integer range and are unique
//!   among currently armed timers on a shared host.
//! - The timer drain task owns only its private driver state. Dropping the last
//!   host/event-loop owner closes that state and wakes the task so it can exit.
//! - Cancelled timer heap entries are compacted relative to the live registry;
//!   cancellation cannot grow an unbounded far-future tombstone backlog.
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
    // Drop the timer owner before the optional owned runtime. Its final drop
    // wakes the drain task, allowing runtime shutdown to join it promptly.
    timers: Arc<TimerDriverOwner>,
    owned: Option<Arc<tokio::runtime::Runtime>>,
    http_client: reqwest::Client,
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
        let timers = TimerDriverOwner::spawn(&handle);
        Self {
            handle,
            timers,
            owned: None,
            http_client: remote_http_client(),
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
        let timers = TimerDriverOwner::spawn(&handle);
        Ok(Self {
            handle,
            timers,
            owned: Some(runtime),
            http_client: remote_http_client(),
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

/// One armed timer as ordered inside [`TimerDriverInner`].
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
    closed: bool,
}

impl TimerDriverState {
    /// Bound cancelled heap tombstones while preserving every current entry.
    fn compact_cancelled_entries(&mut self) {
        let live = self.records.len();
        if live == 0 {
            self.heap.clear();
            return;
        }

        let compact_above = live.saturating_mul(2).saturating_add(64);
        if self.heap.len() <= compact_above {
            return;
        }

        let Self { heap, records, .. } = self;
        heap.retain(|Reverse(entry)| {
            records
                .get(&entry.token)
                .is_some_and(|record| record.seq == entry.seq)
        });
    }
}

/// Single-task timer wheel behind [`TokioEventLoop`].
///
/// Every host timer lives in one deadline-ordered heap drained by one Tokio
/// task, so the inbox receives `TimerFired` messages in deadline order and, on
/// a tie, in arming order. Sleeping each timer in its own spawned task cannot
/// promise that: two `sleep(1ms)` tasks wake on different workers and race to
/// the inbox.
struct TimerDriverInner {
    state: Mutex<TimerDriverState>,
    /// Rung whenever the earliest deadline may have moved closer.
    wakeup: Notify,
}

/// Externally owned lifetime guard for one timer driver.
///
/// The drain task deliberately retains only [`TimerDriverInner`]. Therefore
/// the last shared host/event-loop owner can close the driver even while that
/// task is asleep on a far-future deadline.
struct TimerDriverOwner {
    inner: Arc<TimerDriverInner>,
}

const MAX_SAFE_TIMER_TOKEN: u64 = (1u64 << 53) - 1;

impl TimerDriverOwner {
    /// Build a driver and put its drain loop on `handle`.
    fn spawn(handle: &tokio::runtime::Handle) -> Arc<Self> {
        let inner = Arc::new(TimerDriverInner {
            state: Mutex::new(TimerDriverState {
                next_token: 1,
                ..TimerDriverState::default()
            }),
            wakeup: Notify::new(),
        });
        handle.spawn(TimerDriverInner::drain(Arc::clone(&inner)));
        Arc::new(Self { inner })
    }

    fn arm(&self, request: &TimerRequest, wake: Arc<dyn TimerWake>) -> TimerToken {
        self.inner.arm(request, wake)
    }

    fn disarm(&self, token: TimerToken) -> bool {
        self.inner.disarm(token)
    }
}

impl Drop for TimerDriverOwner {
    fn drop(&mut self) {
        self.inner.close();
    }
}

impl TimerDriverInner {
    /// Arm a timer and return the token the VM stores its callback under.
    fn arm(&self, request: &TimerRequest, wake: Arc<dyn TimerWake>) -> TimerToken {
        let deadline = Instant::now() + request.delay;
        let repeat = request
            .repeat
            .map(|period| period.max(Duration::from_millis(1)));
        let token = {
            let mut state = self.state.lock().expect("timer driver poisoned");
            debug_assert!(!state.closed, "closed timer driver cannot be armed");
            let token = loop {
                let token = TimerToken(state.next_token);
                state.next_token = if state.next_token == MAX_SAFE_TIMER_TOKEN {
                    1
                } else {
                    state.next_token + 1
                };
                if !state.records.contains_key(&token) {
                    break token;
                }
            };
            let seq = state.next_seq;
            state.next_seq += 1;
            state
                .records
                .insert(token, TimerRecord { wake, repeat, seq });
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

    /// Drop a timer's record and compact cancelled heap entries when needed.
    fn disarm(&self, token: TimerToken) -> bool {
        let removed = {
            let mut state = self.state.lock().expect("timer driver poisoned");
            let removed = state.records.remove(&token).is_some();
            if removed {
                state.compact_cancelled_entries();
            }
            removed
        };
        if removed {
            self.wakeup.notify_one();
        }
        removed
    }

    /// Take everything due, in order, and report when the next timer is due.
    fn take_due(&self) -> Option<(Vec<(Arc<dyn TimerWake>, TimerToken)>, Option<Instant>)> {
        let mut state = self.state.lock().expect("timer driver poisoned");
        if state.closed {
            return None;
        }
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
        state.compact_cancelled_entries();
        let next = state.heap.peek().map(|Reverse(entry)| entry.deadline);
        Some((due, next))
    }

    /// Close the driver and release every callback retained by armed timers.
    fn close(&self) {
        let should_wake = {
            let mut state = self.state.lock().expect("timer driver poisoned");
            if state.closed {
                false
            } else {
                state.closed = true;
                state.records.clear();
                state.heap.clear();
                true
            }
        };
        if should_wake {
            // There is one drain task. `notify_one` stores a permit if close
            // races the task between reading state and polling `notified()`.
            self.wakeup.notify_one();
        }
    }

    /// Fire due timers forever, sleeping until the earliest deadline.
    async fn drain(driver: Arc<Self>) {
        loop {
            // Registered before the heap is read so an arm racing this pass
            // still wakes the sleep below.
            let wakeup = driver.wakeup.notified();
            tokio::pin!(wakeup);

            let Some((due, next)) = driver.take_due() else {
                return;
            };
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

fn remote_http_client() -> reqwest::Client {
    // Redirect authorization is request-specific, so the shared pooled client
    // must return each 3xx to `fetch_remote_module` before another connection
    // can be opened.
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .expect("reqwest client construction must succeed")
}

fn is_followed_redirect(status: reqwest::StatusCode) -> bool {
    matches!(
        status,
        reqwest::StatusCode::MOVED_PERMANENTLY
            | reqwest::StatusCode::FOUND
            | reqwest::StatusCode::SEE_OTHER
            | reqwest::StatusCode::TEMPORARY_REDIRECT
            | reqwest::StatusCode::PERMANENT_REDIRECT
    )
}

async fn fetch_remote_module_hop(
    client: reqwest::Client,
    request: crate::module_loader::RemoteModuleRequest,
) -> Result<crate::module_loader::RemoteModuleResponse, crate::module_loader::RemoteModuleError> {
    let requested_url = request.url.clone();
    let request_account = request.account.clone();
    let current = reqwest::Url::parse(&requested_url).map_err(|error| {
        crate::module_loader::RemoteModuleError::Fetch {
            url: requested_url.clone(),
            message: format!("invalid remote module URL: {error}"),
        }
    })?;
    let response = client.get(current.clone()).send().await.map_err(|error| {
        crate::module_loader::RemoteModuleError::Fetch {
            url: current.to_string(),
            message: format!("HTTP request failed: {error}"),
        }
    })?;
    if is_followed_redirect(response.status()) {
        let location = response
            .headers()
            .get(reqwest::header::LOCATION)
            .ok_or_else(|| crate::module_loader::RemoteModuleError::Fetch {
                url: current.to_string(),
                message: format!(
                    "redirect status {} has no Location header",
                    response.status()
                ),
            })?
            .to_str()
            .map_err(|error| crate::module_loader::RemoteModuleError::Fetch {
                url: current.to_string(),
                message: format!("redirect Location is not valid text: {error}"),
            })?
            .to_string();
        return Ok(crate::module_loader::RemoteModuleResponse::Redirect { location });
    }
    if !response.status().is_success() {
        return Err(crate::module_loader::RemoteModuleError::Fetch {
            url: current.to_string(),
            message: format!("HTTP status {}", response.status()),
        });
    }
    let content_type = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .map(str::to_string);
    // Stream the body in chunks, charging each chunk to the request's
    // ledger before it is retained. A rejected byte budget aborts the
    // download instead of collecting an unbounded body first.
    let mut response = response;
    let mut builder = otter_resource::SharedSourceBuilder::new(&request_account);
    loop {
        let chunk = response.chunk().await.map_err(|error| {
            crate::module_loader::RemoteModuleError::Fetch {
                url: current.to_string(),
                message: format!("HTTP body read failed: {error}"),
            }
        })?;
        let Some(chunk) = chunk else { break };
        builder.push_bytes(&chunk).map_err(|error| {
            crate::module_loader::RemoteModuleError::Fetch {
                url: current.to_string(),
                message: format!("bounded body read failed: {error}"),
            }
        })?;
    }
    let source =
        builder
            .finish_utf8()
            .map_err(|error| crate::module_loader::RemoteModuleError::Fetch {
                url: current.to_string(),
                message: format!("HTTP body is not valid UTF-8 source: {error}"),
            })?;
    Ok(crate::module_loader::RemoteModuleResponse::Source {
        source,
        content_type,
    })
}

impl crate::module_loader::RemoteModuleProvider for TokioRemoteModuleProvider {
    fn fetch(
        &self,
        request: crate::module_loader::RemoteModuleRequest,
    ) -> crate::module_loader::RemoteModuleFuture {
        let client = self.client.clone();
        Box::pin(async move {
            let cancellation = request.cancellation.clone();
            tokio::select! {
                () = cancellation.cancelled() => {
                    Err(crate::module_loader::RemoteModuleError::Cancelled)
                }
                result = fetch_remote_module_hop(client, request) => result,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug)]
    struct NoopWake;

    impl TimerWake for NoopWake {
        fn timer_fired(&self, _token: TimerToken) {}
    }

    fn idle_driver() -> TimerDriverInner {
        TimerDriverInner {
            state: Mutex::new(TimerDriverState {
                next_token: 1,
                ..TimerDriverState::default()
            }),
            wakeup: Notify::new(),
        }
    }

    #[test]
    fn cancelling_ten_thousand_far_future_timers_bounds_heap_tombstones() {
        let driver = idle_driver();
        let wake: Arc<dyn TimerWake> = Arc::new(NoopWake);
        let far_future = TimerRequest {
            delay: Duration::from_secs(24 * 60 * 60),
            repeat: None,
        };

        let live = (0..16)
            .map(|_| driver.arm(&far_future, Arc::clone(&wake)))
            .collect::<Vec<_>>();
        let cancelled = (0..10_000)
            .map(|_| driver.arm(&far_future, Arc::clone(&wake)))
            .collect::<Vec<_>>();
        for token in cancelled {
            assert!(driver.disarm(token));
        }

        {
            let state = driver.state.lock().expect("timer driver poisoned");
            assert_eq!(state.records.len(), live.len());
            assert!(
                state.heap.len() <= state.records.len().saturating_mul(2).saturating_add(64),
                "heap retained {} entries for {} live timers",
                state.heap.len(),
                state.records.len()
            );
        }

        for token in live {
            assert!(driver.disarm(token));
        }
        let state = driver.state.lock().expect("timer driver poisoned");
        assert!(state.records.is_empty());
        assert!(state.heap.is_empty());
    }

    #[test]
    fn compaction_preserves_mixed_live_timer_order() {
        let driver = idle_driver();
        let wake: Arc<dyn TimerWake> = Arc::new(NoopWake);
        let due_now = TimerRequest {
            delay: Duration::ZERO,
            repeat: None,
        };
        let far_future = TimerRequest {
            delay: Duration::from_secs(24 * 60 * 60),
            repeat: None,
        };

        let first = driver.arm(&due_now, Arc::clone(&wake));
        let cancelled_due = driver.arm(&due_now, Arc::clone(&wake));
        let second = driver.arm(&due_now, Arc::clone(&wake));
        let cancelled = (0..512)
            .map(|_| driver.arm(&far_future, Arc::clone(&wake)))
            .collect::<Vec<_>>();
        let third = driver.arm(&due_now, Arc::clone(&wake));

        assert!(driver.disarm(cancelled_due));
        for token in cancelled {
            assert!(driver.disarm(token));
        }

        let (due, next) = driver.take_due().expect("driver remains open");
        let tokens = due.into_iter().map(|(_, token)| token).collect::<Vec<_>>();
        assert_eq!(tokens, vec![first, second, third]);
        assert!(next.is_none());

        let state = driver.state.lock().expect("timer driver poisoned");
        assert!(state.records.is_empty());
        assert!(state.heap.is_empty());
    }

    #[tokio::test]
    async fn dropping_external_handle_owners_stops_and_releases_driver() {
        let host = TokioRuntimeHost::from_handle(tokio::runtime::Handle::current());
        let event_loop = host.event_loop();
        let owner = Arc::downgrade(&event_loop.timers);
        let inner = Arc::downgrade(&event_loop.timers.inner);
        let callback = Arc::new(NoopWake);
        let callback_weak = Arc::downgrade(&callback);

        event_loop.schedule_timer(
            TimerRequest {
                delay: Duration::from_secs(24 * 60 * 60),
                repeat: None,
            },
            callback.clone(),
        );
        drop(callback);

        drop(host);
        assert!(
            owner.upgrade().is_some(),
            "event-loop clone keeps owner alive"
        );
        assert!(callback_weak.upgrade().is_some());

        drop(event_loop);
        assert!(
            owner.upgrade().is_none(),
            "last external owner was released"
        );
        assert!(
            callback_weak.upgrade().is_none(),
            "close releases armed callbacks"
        );
        if let Some(driver) = inner.upgrade() {
            assert!(
                driver.state.lock().expect("timer driver poisoned").closed,
                "owner drop marks the inner driver closed"
            );
        }

        tokio::time::timeout(Duration::from_secs(1), async {
            while inner.upgrade().is_some() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("timer drain task retained its inner state after close");
    }
}
