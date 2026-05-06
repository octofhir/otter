//! Runtime scheduling boundary and Tokio default implementation.
//!
//! Task 85 introduces a product-level event-loop abstraction so public
//! handles can be `Send + Sync` while VM and GC internals stay owned by a
//! single isolate runner.
//!
//! # Contents
//!
//! - [`EventLoop`] — host scheduling trait.
//! - [`TokioEventLoop`] — default Tokio-backed implementation.
//! - Host-op, timer, wake, and liveness support types.
//!
//! # Invariants
//!
//! - Host futures are `Send + 'static` and carry only owned host data.
//! - The VM crate does not import Tokio types.
//! - Timer and host-op liveness metadata is explicit even before the
//!   corresponding JS APIs are exposed.
//!
//! # See also
//!
//! - [ADR-0005](../../../docs/new-engine/adr/0005-async-runtime-binding.md)
//! - [Task 85](../../../docs/new-engine/tasks/85-tokio-event-loop-runtime-handle.md)

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use tokio::sync::Mutex;
use tokio::task::JoinHandle;

/// Future shape accepted by [`EventLoop::spawn_host_op`].
pub type HostFuture = Pin<Box<dyn Future<Output = HostOpCompletion> + Send + 'static>>;

/// Liveness bit for runtime work.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuntimeLiveness {
    /// Keeps `run_until_idle` alive.
    Ref,
    /// May complete while the loop is already being driven but does
    /// not prevent idle shutdown.
    Unref,
}

/// Completion payload for a host operation.
#[derive(Debug, Clone)]
pub struct HostOpCompletion {
    /// Runtime-assigned operation id.
    pub id: u64,
    /// Human-readable operation kind.
    pub kind: String,
    /// Result payload. Foundation bridge keeps this owned and textual;
    /// future host APIs can widen it without exposing VM handles.
    pub result: Result<String, String>,
}

/// Abort handle returned for a spawned host operation.
#[derive(Clone)]
pub struct HostJoinHandle {
    abort: Arc<dyn Fn() + Send + Sync>,
}

impl std::fmt::Debug for HostJoinHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HostJoinHandle").finish_non_exhaustive()
    }
}

impl HostJoinHandle {
    /// Request best-effort cancellation of the host operation.
    pub fn abort(&self) {
        (self.abort)();
    }
}

/// Timer identifier.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TimerToken(pub u64);

/// Timer scheduling request.
#[derive(Debug, Clone)]
pub struct TimerRequest {
    /// Delay from now.
    pub delay: Duration,
    /// Optional interval repeat period.
    pub repeat: Option<Duration>,
    /// Ref/unref liveness.
    pub liveness: RuntimeLiveness,
    /// Diagnostic origin string.
    pub origin: String,
}

/// Wake request for the isolate runner.
#[derive(Debug, Clone)]
pub struct RuntimeWake {
    /// Wake origin.
    pub origin: String,
}

/// Scheduling boundary owned by `otter-runtime`.
pub trait EventLoop: Send + Sync + 'static {
    /// Spawn owned host work outside the isolate.
    fn spawn_host_op(&self, op: HostFuture) -> HostJoinHandle;

    /// Schedule a timer and return its token.
    fn schedule_timer(&self, request: TimerRequest) -> TimerToken;

    /// Cancel a scheduled timer.
    fn cancel_timer(&self, token: TimerToken) -> bool;

    /// Return the event-loop time source.
    fn now(&self) -> Instant;

    /// Wake the isolate runner.
    fn wake_runtime(&self, wake: RuntimeWake);
}

/// Tokio-backed default event loop.
#[derive(Clone)]
pub struct TokioEventLoop {
    handle: tokio::runtime::Handle,
    owned: Option<Arc<tokio::runtime::Runtime>>,
    next_timer: Arc<AtomicU64>,
    timers: Arc<Mutex<HashMap<TimerToken, JoinHandle<()>>>>,
}

impl std::fmt::Debug for TokioEventLoop {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TokioEventLoop")
            .field("owned", &self.owned.is_some())
            .finish_non_exhaustive()
    }
}

impl TokioEventLoop {
    /// Use the current Tokio runtime.
    ///
    /// # Panics
    /// Panics when called outside a Tokio runtime.
    #[must_use]
    pub fn current() -> Self {
        Self::from_handle(tokio::runtime::Handle::current())
    }

    /// Wrap an embedder-provided Tokio handle.
    #[must_use]
    pub fn from_handle(handle: tokio::runtime::Handle) -> Self {
        Self {
            handle,
            owned: None,
            next_timer: Arc::new(AtomicU64::new(1)),
            timers: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Create an owned multi-thread Tokio runtime.
    ///
    /// # Errors
    /// Returns [`std::io::Error`] if Tokio cannot create worker
    /// threads.
    pub fn owned() -> Result<Self, std::io::Error> {
        let runtime = Arc::new(tokio::runtime::Runtime::new()?);
        Ok(Self {
            handle: runtime.handle().clone(),
            owned: Some(runtime),
            next_timer: Arc::new(AtomicU64::new(1)),
            timers: Arc::new(Mutex::new(HashMap::new())),
        })
    }

    /// Use the current Tokio runtime when present; otherwise create
    /// an owned runtime.
    ///
    /// # Errors
    /// Returns [`std::io::Error`] if no current runtime exists and an
    /// owned runtime cannot be created.
    pub fn current_or_owned() -> Result<Self, std::io::Error> {
        match tokio::runtime::Handle::try_current() {
            Ok(handle) => Ok(Self::from_handle(handle)),
            Err(_) => Self::owned(),
        }
    }

    /// Block on a future using the backing Tokio runtime.
    ///
    /// This is intended for CLI and non-async embedders. Async callers
    /// should use the `async` methods on [`crate::Otter`] directly.
    pub fn block_on<F: Future>(&self, future: F) -> F::Output {
        if let Some(runtime) = &self.owned {
            return runtime.block_on(future);
        }
        tokio::task::block_in_place(|| self.handle.block_on(future))
    }

    /// Schedule a Tokio timer and invoke `on_fire` on the Tokio
    /// worker after the delay elapses.
    ///
    /// The timer task registry is intentionally Tokio-local. The
    /// generic [`crate::RuntimeHandle`] only sees owned timer tokens
    /// and inbox messages; it does not hold executor locks.
    #[must_use]
    pub fn schedule_timer_callback<F>(&self, request: TimerRequest, on_fire: F) -> TimerToken
    where
        F: FnOnce(TimerToken) + Send + 'static,
    {
        let token = TimerToken(self.next_timer.fetch_add(1, Ordering::Relaxed));
        let timers = self.timers.clone();
        let delay = request.delay;
        let join = self.handle.spawn(async move {
            tokio::time::sleep(delay).await;
            timers.lock().await.remove(&token);
            on_fire(token);
        });
        if let Ok(mut timers) = self.timers.try_lock() {
            timers.insert(token, join);
        }
        token
    }
}

impl EventLoop for TokioEventLoop {
    fn spawn_host_op(&self, op: HostFuture) -> HostJoinHandle {
        let join = self.handle.spawn(async move {
            let _ = op.await;
        });
        let abort = Arc::new(move || join.abort());
        HostJoinHandle { abort }
    }

    fn schedule_timer(&self, request: TimerRequest) -> TimerToken {
        self.schedule_timer_callback(request, |_| {})
    }

    fn cancel_timer(&self, token: TimerToken) -> bool {
        let removed = self
            .timers
            .try_lock()
            .ok()
            .and_then(|mut timers| timers.remove(&token));
        let Some(join) = removed else {
            return false;
        };
        join.abort();
        true
    }

    fn now(&self) -> Instant {
        Instant::now()
    }

    fn wake_runtime(&self, _wake: RuntimeWake) {}
}
