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
//! - [`HostCompletionSink`] — admit and spawn futures, then post their
//!   completion jobs.
//! - [`HostCompletionJob`] — a `Send` closure run with full
//!   interpreter access on the isolate thread.
//! - [`HostCompletionAdmission`] — opaque, unique runtime credit carried from
//!   the synchronous prologue through terminal dispatch.
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

use std::any::Any;
use std::future::Future;
use std::pin::Pin;

use crate::Interpreter;

/// A completion job: runs on the isolate thread with full interpreter
/// access. Built by the marshalling layer; carries only owned `Send`
/// data.
pub struct HostCompletionJob {
    run: Option<Box<dyn FnOnce(&mut Interpreter) + Send>>,
    cancel: Option<Box<dyn FnOnce(&mut Interpreter) + Send>>,
}

impl HostCompletionJob {
    /// Wrap a closure as a completion job.
    #[must_use]
    pub fn new(job: impl FnOnce(&mut Interpreter) + Send + 'static) -> Self {
        Self {
            run: Some(Box::new(job)),
            cancel: None,
        }
    }

    /// Wrap a completion plus the isolate-local cleanup to run if dispatch is
    /// suppressed after process exit.
    #[must_use]
    pub fn new_with_cancel(
        job: impl FnOnce(&mut Interpreter) + Send + 'static,
        cancel: impl FnOnce(&mut Interpreter) + Send + 'static,
    ) -> Self {
        Self {
            run: Some(Box::new(job)),
            cancel: Some(Box::new(cancel)),
        }
    }

    /// Run the job against the isolate's interpreter.
    pub fn run(mut self, interp: &mut Interpreter) {
        if let Some(job) = self.run.take() {
            job(interp);
        }
    }

    /// Suppress settlement and run only the job's isolate-local cleanup.
    pub fn cancel(mut self, interp: &mut Interpreter) {
        if let Some(cancel) = self.cancel.take() {
            cancel(interp);
        }
    }
}

/// Terminal accounting for an admitted future that completed synchronously,
/// before a pending Promise or isolate-inbox job was needed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostCompletionOutcome {
    /// The Rust result was converted into a fulfilled or rejected JS Promise.
    Completed,
    /// Promise construction or Rust-to-JS conversion failed synchronously.
    Failed,
    /// The host future was dropped or aborted before producing a result.
    Cancelled,
}

/// Unique runtime-owned admission for one asynchronous host completion.
///
/// The VM treats the payload as opaque and only moves it back to the sink that
/// created it. Dropping the carrier before handoff cancels the admission by
/// dropping the runtime's RAII guard.
pub struct HostCompletionAdmission(Option<Box<dyn Any + Send>>);

impl HostCompletionAdmission {
    /// Wrap one embedder-owned admission guard.
    #[must_use]
    pub fn new(token: Box<dyn Any + Send>) -> Self {
        Self(Some(token))
    }

    /// Recover a guard of the expected concrete type in the sink that created
    /// it. A carrier from another sink is returned intact instead of panicking.
    pub fn try_into_inner<T: Any + Send>(mut self) -> Result<Box<T>, Self> {
        let Some(token) = self.0.take() else {
            return Err(self);
        };
        match token.downcast::<T>() {
            Ok(token) => Ok(token),
            Err(token) => {
                self.0 = Some(token);
                Err(self)
            }
        }
    }
}

impl std::fmt::Debug for HostCompletionAdmission {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("HostCompletionAdmission")
            .field("live", &self.0.is_some())
            .finish()
    }
}

impl std::fmt::Debug for HostCompletionJob {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HostCompletionJob").finish_non_exhaustive()
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
    fn complete(
        &self,
        admission: HostCompletionAdmission,
        job: HostCompletionJob,
        outcome: HostCompletionOutcome,
    ) -> Result<(), String>;

    /// Consume an admission whose future completed during its eager first poll.
    /// No isolate-inbox job is needed, but origin and completion statistics must
    /// still reach one explicit terminal state rather than looking cancelled.
    fn finish_inline(
        &self,
        admission: HostCompletionAdmission,
        outcome: HostCompletionOutcome,
    ) -> Result<(), String>;

    /// Admit one terminal completion before allocating a pending Promise or
    /// starting its host future.
    fn admit(&self) -> Result<HostCompletionAdmission, String>;

    /// Run `f` inside the host executor's context. The marshalling
    /// layer's eager first poll runs through this so reactor-backed
    /// futures (timers, sockets) can register their wakers; the
    /// default is a plain call for embeddings whose futures never
    /// touch a reactor.
    fn with_executor_context(&self, f: &mut dyn FnMut()) {
        f();
    }
}
