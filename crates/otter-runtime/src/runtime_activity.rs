//! Runtime event-loop activity accounting.
//!
//! Long-lived host resources such as servers, sockets, file watchers, and
//! request tasks participate in the runtime run-loop through ref/unref liveness
//! state. This module owns that generic accounting primitive; feature modules
//! should build on it instead of holding ad-hoc counters.
//!
//! # Contents
//! - [`RuntimeKeepAlive`] - idempotent liveness hold for a host resource.
//! - [`RuntimeActivityAccounting`] - runtime-internal accounting sink.
//!
//! # Invariants
//! - The primitive stores no VM values and never calls into JavaScript.
//! - Closing a hold is idempotent; dropping an open hold releases it as
//!   cancelled activity.
//! - Ref/unref semantics are generic runtime semantics, not HTTP-specific
//!   server state.
//!
//! # See also
//! - [`crate::event_loop::RuntimeLiveness`]
//! - [`crate::handle::RuntimeHandle`]

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use crate::event_loop::RuntimeLiveness;
use crate::{OtterError, Runtime};

/// Runtime-internal sink for event-loop activity counters.
pub(crate) trait RuntimeActivityAccounting: Send + Sync + 'static {
    /// Record one new host activity hold.
    fn retain_host_activity(&self, liveness: RuntimeLiveness);

    /// Record successful release of one host activity hold.
    fn complete_host_activity(&self, liveness: RuntimeLiveness);

    /// Record cancellation/drop release of one host activity hold.
    fn cancel_host_activity(&self, liveness: RuntimeLiveness);

    /// Move an open host activity hold between ref/unref classes.
    fn move_host_activity(&self, from: RuntimeLiveness, to: RuntimeLiveness);
}

/// Owned task that runs on the isolate event-loop thread.
///
/// Implementors must carry only owned, `Send` data. VM values and GC handles
/// must be reacquired on the isolate thread from runtime-managed roots.
pub trait RuntimeTask: Send + 'static {
    /// Execute this task during a runtime event-loop turn.
    fn run(self: Box<Self>, runtime: &mut Runtime) -> Result<(), OtterError>;
}

pub(crate) trait RuntimeTaskQueue: Send + Sync + 'static {
    fn enqueue_boxed(
        &self,
        task: Box<dyn RuntimeTask>,
        liveness: RuntimeLiveness,
    ) -> Result<(), OtterError>;
}

/// Cloneable sender for scheduling typed tasks onto the runtime event loop.
#[derive(Clone)]
pub struct RuntimeTaskSpawner {
    queue: Arc<dyn RuntimeTaskQueue>,
    accounting: Arc<dyn RuntimeActivityAccounting>,
    io_handle: Option<tokio::runtime::Handle>,
}

impl RuntimeTaskSpawner {
    pub(crate) fn new(
        queue: Arc<dyn RuntimeTaskQueue>,
        accounting: Arc<dyn RuntimeActivityAccounting>,
        io_handle: Option<tokio::runtime::Handle>,
    ) -> Self {
        Self {
            queue,
            accounting,
            io_handle,
        }
    }

    /// The shared Tokio runtime handle for host resources that own async IO
    /// (the HTTP server). `None` when the spawner was built without an event
    /// loop (unit tests). A server binds and accepts on this runtime so its
    /// connections are driven by the same executor as timers and fetch, keeping
    /// all VM re-entry on the isolate thread through [`Self::enqueue`].
    #[must_use]
    pub fn io_handle(&self) -> Option<tokio::runtime::Handle> {
        self.io_handle.clone()
    }

    /// Enqueue an owned task to run on the isolate event-loop thread.
    ///
    /// # Errors
    /// Returns [`OtterError`] when the runtime inbox is full or shutting down.
    pub fn enqueue(
        &self,
        task: impl RuntimeTask,
        liveness: RuntimeLiveness,
    ) -> Result<(), OtterError> {
        self.queue.enqueue_boxed(Box::new(task), liveness)
    }

    /// Retain one long-lived host resource in the runtime liveness counters.
    #[must_use]
    pub fn retain_keep_alive(&self, liveness: RuntimeLiveness) -> RuntimeKeepAlive {
        RuntimeKeepAlive::retain(self.accounting.clone(), liveness)
    }
}

impl std::fmt::Debug for RuntimeTaskSpawner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RuntimeTaskSpawner").finish_non_exhaustive()
    }
}

/// Idempotent liveness hold for an open host resource.
///
/// An individual resource is either live or closed, and `ref`/`unref`
/// accounting is applied exactly once.
#[derive(Clone)]
pub struct RuntimeKeepAlive {
    inner: Arc<RuntimeKeepAliveInner>,
}

struct RuntimeKeepAliveInner {
    accounting: Arc<dyn RuntimeActivityAccounting>,
    liveness: Mutex<RuntimeLiveness>,
    closed: AtomicBool,
}

impl RuntimeKeepAlive {
    pub(crate) fn retain(
        accounting: Arc<dyn RuntimeActivityAccounting>,
        liveness: RuntimeLiveness,
    ) -> Self {
        accounting.retain_host_activity(liveness);
        Self {
            inner: Arc::new(RuntimeKeepAliveInner {
                accounting,
                liveness: Mutex::new(liveness),
                closed: AtomicBool::new(false),
            }),
        }
    }

    /// Release this resource's liveness hold. Safe to call more than once.
    pub fn close(&self) {
        if !self.inner.closed.swap(true, Ordering::AcqRel) {
            let liveness = *self
                .inner
                .liveness
                .lock()
                .expect("runtime keep-alive liveness poisoned");
            self.inner.accounting.complete_host_activity(liveness);
        }
    }

    /// Switch this hold to referenced liveness. Idempotent.
    pub fn ref_(&self) {
        self.set_liveness(RuntimeLiveness::Ref);
    }

    /// Switch this hold to unreferenced liveness. Idempotent.
    pub fn unref(&self) {
        self.set_liveness(RuntimeLiveness::Unref);
    }

    fn set_liveness(&self, next: RuntimeLiveness) {
        if self.is_closed() {
            return;
        }
        let mut current = self
            .inner
            .liveness
            .lock()
            .expect("runtime keep-alive liveness poisoned");
        if *current == next || self.is_closed() {
            return;
        }
        self.inner.accounting.move_host_activity(*current, next);
        *current = next;
    }

    /// `true` once [`Self::close`] or drop released the liveness hold.
    #[must_use]
    pub fn is_closed(&self) -> bool {
        self.inner.closed.load(Ordering::Acquire)
    }

    /// The current ref/unref class of this hold.
    #[must_use]
    pub fn liveness(&self) -> RuntimeLiveness {
        *self
            .inner
            .liveness
            .lock()
            .expect("runtime keep-alive liveness poisoned")
    }
}

impl Drop for RuntimeKeepAliveInner {
    fn drop(&mut self) {
        if !self.closed.swap(true, Ordering::AcqRel) {
            let liveness = *self
                .liveness
                .lock()
                .expect("runtime keep-alive liveness poisoned");
            self.accounting.cancel_host_activity(liveness);
        }
    }
}

impl std::fmt::Debug for RuntimeKeepAlive {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RuntimeKeepAlive")
            .field("liveness", &self.liveness())
            .field("closed", &self.is_closed())
            .finish()
    }
}
