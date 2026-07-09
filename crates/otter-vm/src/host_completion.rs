//! Host completion sink: how async native work re-enters the isolate.
//!
//! An async native method runs in three phases: a sync prologue on
//! the isolate (argument extraction, pending-promise creation), a
//! `Send` future on the host executor, and a completion job back on
//! the isolate that converts the future's Rust result into a JS value
//! and settles the promise. The [`HostCompletionSink`] is the
//! isolate's connection to phases two and three: the runtime layer
//! installs one per interpreter (exactly like the timer scheduler),
//! backed by its event loop and inbox.
//!
//! # Contents
//! - [`HostCompletionSink`] — spawn futures, post completion jobs,
//!   hold liveness.
//! - [`HostCompletionJob`] — a `Send` closure run with full
//!   interpreter access on the isolate thread.
//! - [`HostKeepAlive`] — opaque liveness token; dropping it releases
//!   the hold that keeps the event loop from going idle.
//!
//! # Invariants
//! - The sink is per-interpreter state installed by the embedder —
//!   never a process global or thread-local.
//! - A [`HostCompletionJob`] carries only owned `Send` data; every
//!   GC value it needs must travel as a persistent-root id and be
//!   re-resolved on the isolate thread.
//!
//! # See also
//! - [`crate::marshal`] — `PromiseCompleter` / `promise_from_future`,
//!   the typed surface over this sink.
//! - `crates/otter-runtime/src/handle.rs` — the inbox-backed
//!   implementation.

use std::future::Future;
use std::pin::Pin;

use crate::Interpreter;

/// A completion job: runs on the isolate thread with full interpreter
/// access. Built by the marshalling layer; carries only owned `Send`
/// data.
pub struct HostCompletionJob(Box<dyn FnOnce(&mut Interpreter) + Send>);

impl HostCompletionJob {
    /// Wrap a closure as a completion job.
    #[must_use]
    pub fn new(job: impl FnOnce(&mut Interpreter) + Send + 'static) -> Self {
        Self(Box::new(job))
    }

    /// Run the job against the isolate's interpreter.
    pub fn run(self, interp: &mut Interpreter) {
        (self.0)(interp);
    }
}

impl std::fmt::Debug for HostCompletionJob {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HostCompletionJob").finish_non_exhaustive()
    }
}

/// Opaque liveness token. While held, the embedder's event loop must
/// not consider the isolate idle (a completion is still expected);
/// dropping releases the hold.
pub struct HostKeepAlive(Option<Box<dyn std::any::Any + Send>>);

impl HostKeepAlive {
    /// Wrap an embedder-owned liveness guard.
    #[must_use]
    pub fn new(token: Box<dyn std::any::Any + Send>) -> Self {
        Self(Some(token))
    }

    /// A no-op token for embeddings without liveness accounting.
    #[must_use]
    pub fn noop() -> Self {
        Self(None)
    }
}

impl std::fmt::Debug for HostKeepAlive {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HostKeepAlive")
            .field("held", &self.0.is_some())
            .finish()
    }
}

/// The isolate's host-async connection: spawn `Send` futures on the
/// embedder's executor and post completion jobs back to the isolate.
pub trait HostCompletionSink: Send + Sync {
    /// Spawn a future on the host executor. The future owns its data
    /// and reports back exclusively through [`Self::complete`].
    fn spawn(&self, future: Pin<Box<dyn Future<Output = ()> + Send>>);

    /// Post a completion job to run on the isolate thread at the next
    /// checkpoint.
    fn complete(&self, job: HostCompletionJob);

    /// Acquire a liveness hold that keeps the event loop alive until
    /// the matching completion arrives (released on drop).
    fn keep_alive(&self) -> HostKeepAlive;

    /// Run `f` inside the host executor's context. The marshalling
    /// layer's eager first poll runs through this so reactor-backed
    /// futures (timers, sockets) can register their wakers; the
    /// default is a plain call for embeddings whose futures never
    /// touch a reactor.
    fn with_executor_context(&self, f: &mut dyn FnMut()) {
        f();
    }
}
