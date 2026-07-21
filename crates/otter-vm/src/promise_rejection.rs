//! HTML HostPromiseRejectionTracker bookkeeping and the post-drain
//! unhandled-rejection checkpoint.
//!
//! # Contents
//! - [`RejectionTracker`] — the two promise-handle lists the HTML algorithm
//!   keeps: `pending` (rejected, not yet reported) and `notified` (reported as
//!   `unhandledrejection`, retained so a late handler can fire
//!   `rejectionhandled`).
//! - [`PromiseRejectionHook`] — the embedder-owned callback used by browser
//!   hosts to materialize rejection events on the isolate thread.
//! - [`Interpreter::run_promise_rejection_checkpoint`] — run once each time the
//!   microtask queue drains empty. Re-reads each tracked promise's live
//!   `[[PromiseIsHandled]]` and dispatches through the Rust hook or JS reporter.
//!
//! # Invariants
//! - A promise enters `pending` only via [`RejectionTracker::note_rejected`],
//!   fed from [`crate::promise::PromiseSettleJobs::unhandled_rejection`] at every
//!   reject site. The reject-time `is_handled` gate suppresses promises already
//!   observed by a `.then`/`.catch`/`await`.
//! - The checkpoint always re-reads the live flag rather than trusting the
//!   reject-time snapshot: a handler attached between rejection and the
//!   checkpoint flips `is_handled`, and that promise must NOT be reported.
//! - Both lists are realm-owned GC roots (traced from the active or parked
//!   [`crate::RealmState`]); a tracked
//!   handle would otherwise be reclaimed while the reason is still pending
//!   report.
//! - Firing is a no-op (and both lists are cleared) when neither a Rust hook nor
//!   a JS reporter is installed — a bare VM realm has no event target, so
//!   accumulating handles there would leak.
//!
//! # See also
//! `crates/otter-web/src/web_bootstrap.js` (`__otterFirePromiseRejection`) — the
//! reporter that builds the `PromiseRejectionEvent`, invokes `globalThis.on*`,
//! and falls back to `reportError`.
use std::sync::Arc;

use crate::*;
use otter_gc::raw::SlotVisitor;

/// Rust-side observer for the HTML Promise rejection checkpoint.
///
/// The callback always runs on the isolate's owning thread. `promise` and
/// `reason` are raw values current at callback entry; a callback that allocates
/// must park both in `ctx.scope` first. Implementations must not retain either
/// value after returning.
pub trait PromiseRejectionHook: Send + Sync + 'static {
    /// Report one unhandled (`handled == false`) or later-handled
    /// (`handled == true`) rejection.
    fn notify(
        &self,
        ctx: &mut NativeCtx<'_>,
        promise: Value,
        reason: Value,
        handled: bool,
    ) -> Result<(), NativeError>;
}

/// Cloneable configured rejection hook.
#[derive(Clone)]
pub struct PromiseRejectionHookHandle(Arc<dyn PromiseRejectionHook>);

impl PromiseRejectionHookHandle {
    /// Wrap a hook implementation.
    #[must_use]
    pub fn new(hook: impl PromiseRejectionHook) -> Self {
        Self(Arc::new(hook))
    }

    fn notify(
        &self,
        ctx: &mut NativeCtx<'_>,
        promise: Value,
        reason: Value,
        handled: bool,
    ) -> Result<(), NativeError> {
        self.0.notify(ctx, promise, reason, handled)
    }
}

impl std::fmt::Debug for PromiseRejectionHookHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PromiseRejectionHookHandle")
            .finish_non_exhaustive()
    }
}

/// Global name of the JS reporter the web layer installs. The VM invokes it at
/// the checkpoint with `(promise, reason, wasHandled)`.
const REPORTER_GLOBAL: &str = "__otterFirePromiseRejection";

/// The HTML "about-to-be-notified rejected promises" and
/// "outstanding rejected promises" sets, kept per realm.
#[derive(Debug, Default)]
pub(crate) struct RejectionTracker {
    /// Rejected while unhandled, awaiting the next checkpoint. Spec: the
    /// about-to-be-notified list.
    pending: Vec<crate::promise::JsPromiseHandle>,
    /// Reported as `unhandledrejection`, retained so a later handler fires
    /// `rejectionhandled`. Spec: the outstanding-rejected set.
    notified: Vec<crate::promise::JsPromiseHandle>,
}

impl RejectionTracker {
    /// Record a promise whose rejection had no reaction attached.
    pub(crate) fn note_rejected(&mut self, promise: crate::promise::JsPromiseHandle) {
        self.pending.push(promise);
    }

    /// Whether either list holds work for the checkpoint to process.
    pub(crate) fn has_work(&self) -> bool {
        !self.pending.is_empty() || !self.notified.is_empty()
    }

    /// Drop all tracked handles (bare realm with no reporter, or realm teardown).
    pub(crate) fn clear(&mut self) {
        self.pending.clear();
        self.notified.clear();
    }

    /// Trace both handle lists as GC roots; a moving collection rewrites each
    /// slot in place.
    pub(crate) fn trace(&self, visitor: &mut SlotVisitor<'_>) {
        for promise in &self.pending {
            promise.trace_value_slots(visitor);
        }
        for promise in &self.notified {
            promise.trace_value_slots(visitor);
        }
    }
}

impl Interpreter {
    /// Feed a settle result's unhandled-rejection notification into the tracker.
    /// Called at every reject site right beside the job enqueue.
    pub(crate) fn note_settle_rejection(&mut self, jobs: &crate::promise::PromiseSettleJobs) {
        if let Some(promise) = jobs.unhandled_rejection {
            self.rejection_tracker.note_rejected(promise);
        }
    }

    /// Track a promise created already-rejected (`Promise.reject`, born-rejected
    /// builders). Such a promise starts with `[[PromiseIsHandled]]` false, so it
    /// is always a candidate until a later reaction attaches — the checkpoint's
    /// live re-read suppresses it if one does.
    pub(crate) fn note_born_rejection(&mut self, promise: crate::promise::JsPromiseHandle) {
        self.rejection_tracker.note_rejected(promise);
    }

    /// `true` while the tracker still has promises to classify.
    pub(crate) fn promise_rejections_need_checkpoint(&self) -> bool {
        self.rejection_tracker.has_work()
    }

    /// Discard all tracked rejections without firing (no reporter / no realm).
    pub(crate) fn clear_promise_rejection_tracking(&mut self) {
        self.rejection_tracker.clear();
    }

    /// HTML "notify about rejected promises": run once the microtask queue is
    /// empty. Promises still unhandled fire `unhandledrejection`; previously
    /// reported promises that have since been handled fire `rejectionhandled`.
    pub(crate) fn run_promise_rejection_checkpoint(
        &mut self,
        context: &ExecutionContext,
    ) -> Result<(), RunError> {
        // A Rust hook takes precedence over the compatibility JS reporter.
        // With neither installed there is no host to deliver to, so drop the
        // tracked handles rather than leak.
        let has_hook = self.promise_rejection_hook().is_some();
        let reporter = crate::object::get(self.global_this, &self.gc_heap, REPORTER_GLOBAL);
        if !has_hook && !reporter.is_some_and(|r| r.is_callable()) {
            self.rejection_tracker.clear();
            return Ok(());
        }

        // Pending → unhandled. Re-read the live flag: a handler attached since
        // the rejection suppresses the notification.
        let idx = 0;
        while idx < self.rejection_tracker.pending.len() {
            let promise = self.rejection_tracker.pending[idx];
            if promise.is_handled(&self.gc_heap) {
                self.rejection_tracker.pending.swap_remove(idx);
                continue;
            }
            self.rejection_tracker.pending.swap_remove(idx);
            // Retain in `notified` (a GC root) before firing so the handle
            // survives any collection the reporter triggers.
            self.rejection_tracker.notified.push(promise);
            self.fire_promise_rejection(context, promise, false);
        }

        // Notified → handled. A late `.then`/`.catch` flips the live flag.
        let mut jdx = 0;
        while jdx < self.rejection_tracker.notified.len() {
            let promise = self.rejection_tracker.notified[jdx];
            if promise.is_handled(&self.gc_heap) {
                self.rejection_tracker.notified.swap_remove(jdx);
                self.fire_promise_rejection(context, promise, true);
                continue;
            }
            jdx += 1;
        }
        Ok(())
    }

    /// Invoke the JS reporter for one promise. `handled` selects the event type
    /// (`rejectionhandled` vs `unhandledrejection`). Reporter errors are
    /// swallowed — a rejection notification must never abort the drain.
    fn fire_promise_rejection(
        &mut self,
        context: &ExecutionContext,
        promise: crate::promise::JsPromiseHandle,
        handled: bool,
    ) {
        let reason = match promise.state(&self.gc_heap) {
            crate::promise::PromiseState::Rejected(reason) => reason,
            // Only rejected promises are tracked; a settled-elsewhere handle is
            // stale bookkeeping, skip it.
            _ => return,
        };
        let promise_value = Value::promise(promise);
        if let Some(hook) = self.promise_rejection_hook() {
            let _ = NativeCtx::with_host_context(
                self,
                NativeCallInfo::default_call(),
                Some(context),
                |ctx| hook.notify(ctx, promise_value, reason, handled),
            );
            return;
        }

        // Re-fetch per call: the reporter Value is not rooted across the
        // reentrant dispatch a previous fire may have moved it through.
        let Some(reporter) = crate::object::get(self.global_this, &self.gc_heap, REPORTER_GLOBAL)
        else {
            return;
        };
        if !reporter.is_callable() {
            return;
        }
        let this = Value::object(self.global_this);
        let args: smallvec::SmallVec<[Value; 8]> =
            smallvec::smallvec![promise_value, reason, Value::boolean(handled)];
        let _ = self.run_callable_sync(context, &reporter, this, args);
    }
}
