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

use std::collections::HashMap;
use std::future::Future;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::task::JoinHandle;

use crate::host_services::{HttpsModuleFetchSink, HttpsModuleFetcher, HttpsModuleFetcherHandle};

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
    /// Wrap an embedder-provided Tokio handle.
    #[must_use]
    pub(crate) fn from_handle(handle: tokio::runtime::Handle) -> Self {
        Self {
            handle,
            owned: None,
            http_client: reqwest::Client::new(),
            next_timer: Arc::new(AtomicU64::new(1)),
            timers: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Create an owned multi-thread Tokio runtime.
    ///
    /// # Errors
    /// Returns [`std::io::Error`] if Tokio cannot create worker
    /// threads.
    pub(crate) fn owned() -> Result<Self, std::io::Error> {
        let runtime = Arc::new(tokio::runtime::Runtime::new()?);
        Ok(Self {
            handle: runtime.handle().clone(),
            owned: Some(runtime),
            http_client: reqwest::Client::new(),
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

    /// Build a narrow HTTPS module fetch service backed by this
    /// event loop's Tokio handle.
    #[must_use]
    pub(crate) fn https_module_fetcher(&self) -> HttpsModuleFetcherHandle {
        Arc::new(TokioHttpsModuleFetcher {
            handle: self.handle.clone(),
            client: self.http_client.clone(),
        })
    }

    /// Blocking adapter used by the synchronous module-graph builder.
    ///
    /// Layer B runs that builder on Tokio's blocking pool, so HTTP/file reads,
    /// parsing, compilation, and linking do not park the isolate. Direct Layer
    /// A callers may still choose this adapter for synchronous preparation.
    pub(crate) fn blocking_module_fetcher(
        &self,
    ) -> Arc<dyn crate::module_loader::RemoteModuleFetch> {
        Arc::new(TokioBlockingModuleFetcher {
            handle: self.handle.clone(),
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

    /// Schedule a Tokio timer and notify `wake` from the Tokio
    /// worker after the delay elapses.
    ///
    /// The timer task registry is intentionally Tokio-local. The
    /// generic [`crate::RuntimeHandle`] only sees owned timer tokens
    /// and inbox messages; it does not hold executor locks.
    fn schedule_timer_task(&self, request: TimerRequest, wake: Arc<dyn TimerWake>) -> TimerToken {
        let token = TimerToken(self.next_timer.fetch_add(1, Ordering::Relaxed));
        let timers = self.timers.clone();
        let delay = request.delay;
        let repeat = request
            .repeat
            .map(|period| period.max(Duration::from_millis(1)));
        let (start_tx, start_rx) = tokio::sync::oneshot::channel::<()>();
        let join = self.handle.spawn(async move {
            if start_rx.await.is_err() {
                return;
            }
            match repeat {
                Some(period) => {
                    tokio::time::sleep(delay).await;
                    loop {
                        wake.timer_fired(token);
                        tokio::time::sleep(period).await;
                    }
                }
                None => {
                    tokio::time::sleep(delay).await;
                    timers
                        .lock()
                        .expect("timer registry poisoned")
                        .remove(&token);
                    wake.timer_fired(token);
                }
            }
        });
        self.timers
            .lock()
            .expect("timer registry poisoned")
            .insert(token, join);
        let _ = start_tx.send(());
        token
    }
}

#[derive(Debug)]
struct TokioHttpsModuleFetcher {
    handle: tokio::runtime::Handle,
    client: reqwest::Client,
}

#[derive(Debug)]
struct TokioBlockingModuleFetcher {
    handle: tokio::runtime::Handle,
    client: reqwest::Client,
}

impl crate::module_loader::RemoteModuleFetch for TokioBlockingModuleFetcher {
    fn fetch(&self, url: &str) -> Result<crate::module_loader::RemoteModuleSource, String> {
        let client = self.client.clone();
        let url = url.to_string();
        // Drive the async request to completion on the shared Tokio runtime.
        // `block_in_place` yields the current worker so the request's own I/O
        // task can run when this is called from inside the runtime; from the
        // plain isolate thread it is a direct `handle.block_on`.
        tokio::task::block_in_place(|| {
            self.handle.block_on(async move {
                let resp = client
                    .get(&url)
                    .send()
                    .await
                    .map_err(|e| format!("HTTPS request failed: {e}"))?;
                if !resp.status().is_success() {
                    return Err(format!("HTTPS status {} for \"{url}\"", resp.status()));
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
                    .map_err(|e| format!("HTTPS body read failed: {e}"))?;
                Ok(crate::module_loader::RemoteModuleSource {
                    source,
                    content_type,
                    final_url,
                })
            })
        })
    }
}

impl HttpsModuleFetcher for TokioHttpsModuleFetcher {
    fn fetch_utf8(&self, url: String, sink: Arc<dyn HttpsModuleFetchSink>) {
        let client = self.client.clone();
        self.handle.spawn(async move {
            let result = async {
                let resp = client
                    .get(&url)
                    .send()
                    .await
                    .map_err(|e| format!("dynamic import: HTTPS request failed: {e}"))?;
                if !resp.status().is_success() {
                    return Err(format!(
                        "dynamic import: HTTPS status {} for \"{url}\"",
                        resp.status()
                    ));
                }
                resp.text()
                    .await
                    .map_err(|e| format!("dynamic import: HTTPS body read failed: {e}"))
            }
            .await;
            sink.fetched(result);
        });
    }
}

impl EventLoop for TokioEventLoop {
    fn schedule_timer(&self, request: TimerRequest, wake: Arc<dyn TimerWake>) -> TimerToken {
        self.schedule_timer_task(request, wake)
    }

    fn cancel_timer(&self, token: TimerToken) -> bool {
        let removed = self
            .timers
            .lock()
            .expect("timer registry poisoned")
            .remove(&token);
        let Some(join) = removed else {
            return false;
        };
        join.abort();
        true
    }
}
