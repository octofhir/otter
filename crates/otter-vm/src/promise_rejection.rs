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
/// One notification's admitted Promise operation extent. The realm is owned
/// by this tracker; source and async context are captured at actual rejection.
#[derive(Debug, Clone)]
struct TrackedRejection {
    promise: crate::promise::JsPromiseHandle,
    context: Option<ExecutionContext>,
    async_context: Value,
}

#[derive(Debug, Default)]
pub(crate) struct RejectionTracker {
    /// Rejected while unhandled, awaiting the next checkpoint. Spec: the
    /// about-to-be-notified list.
    pending: Vec<TrackedRejection>,
    /// Reported as `unhandledrejection`, retained so a later handler fires
    /// `rejectionhandled`. Spec: the outstanding-rejected set.
    notified: Vec<TrackedRejection>,
}

impl RejectionTracker {
    /// Record a promise whose rejection had no reaction attached.
    pub(crate) fn note_rejected(
        &mut self,
        promise: crate::promise::JsPromiseHandle,
        context: Option<ExecutionContext>,
        async_context: Value,
    ) {
        self.pending.push(TrackedRejection {
            promise,
            context,
            async_context,
        });
    }

    /// Drop all tracked handles (bare realm with no reporter, or realm teardown).
    pub(crate) fn clear(&mut self) {
        self.pending.clear();
        self.notified.clear();
    }

    pub(crate) fn visit_function_ids(&self, visitor: &mut dyn FnMut(u32)) {
        for record in self.pending.iter().chain(&self.notified) {
            crate::code_liveness::visit_value(&record.async_context, visitor);
        }
    }

    /// Trace both handle lists as GC roots; a moving collection rewrites each
    /// slot in place.
    pub(crate) fn trace(&self, visitor: &mut SlotVisitor<'_>) {
        for promise in &self.pending {
            promise.promise.trace_value_slots(visitor);
            promise.async_context.trace_value_slots(visitor);
        }
        for promise in &self.notified {
            promise.promise.trace_value_slots(visitor);
            promise.async_context.trace_value_slots(visitor);
        }
    }
}

impl Interpreter {
    /// Feed a settle result's unhandled-rejection notification into the tracker.
    /// Called at every reject site right beside the job enqueue.
    pub(crate) fn note_settle_rejection(
        &mut self,
        jobs: &crate::promise::PromiseSettleJobs,
        context: Option<&ExecutionContext>,
    ) {
        if let Some(promise) = jobs.unhandled_rejection {
            let async_context = self.async_context();
            self.rejection_tracker
                .note_rejected(promise, context.cloned(), async_context);
        }
    }

    /// Track a promise created already-rejected (`Promise.reject`, born-rejected
    /// builders). Such a promise starts with `[[PromiseIsHandled]]` false, so it
    /// is always a candidate until a later reaction attaches — the checkpoint's
    /// live re-read suppresses it if one does.
    pub(crate) fn note_born_rejection(
        &mut self,
        promise: crate::promise::JsPromiseHandle,
        context: Option<&ExecutionContext>,
    ) {
        let async_context = self.async_context();
        self.rejection_tracker
            .note_rejected(promise, context.cloned(), async_context);
    }

    /// Whole-queue checkpoint visits every existing live realm in stable ID
    /// order. Reporters may enqueue follow-up jobs; an escaping fatal stops
    /// before any later realm reporter is invoked.
    pub(crate) fn run_all_promise_rejection_checkpoints(
        &mut self,
        report: &mut impl FnMut(&mut NativeCtx<'_>, &RunError) -> Result<bool, NativeError>,
    ) -> Result<(), RunError> {
        let mut realms = self
            .extra_realms
            .iter()
            .map(|realm| realm.id)
            .collect::<Vec<_>>();
        realms.push(self.active_realm_id);
        realms.sort_unstable();
        for realm in realms {
            if !self.job_realm_is_live(realm) {
                continue;
            }
            let outcome = self
                .with_host_realm_id(realm, |vm| Ok(vm.run_promise_rejection_checkpoint(report)));
            match outcome {
                Ok(outcome) => outcome?,
                Err(error) => {
                    return Err(RunError {
                        error,
                        frames: Vec::new(),
                        detail: self.take_error_detail(),
                    });
                }
            }
        }
        Ok(())
    }

    /// HTML "notify about rejected promises": run once the microtask queue is
    /// empty. Promises still unhandled fire `unhandledrejection`; previously
    /// reported promises that have since been handled fire `rejectionhandled`.
    pub(crate) fn run_promise_rejection_checkpoint(
        &mut self,
        report: &mut impl FnMut(&mut NativeCtx<'_>, &RunError) -> Result<bool, NativeError>,
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
            let record = self.rejection_tracker.pending[idx].clone();
            let promise = record.promise;
            if promise.is_handled(&self.gc_heap) {
                self.rejection_tracker.pending.remove(idx);
                continue;
            }
            self.rejection_tracker.pending.remove(idx);
            // Retain in `notified` (a GC root) before firing so the handle
            // survives any collection the reporter triggers.
            self.rejection_tracker.notified.push(record.clone());
            self.fire_promise_rejection(record, false, report)?;
        }

        // Notified → handled. A late `.then`/`.catch` flips the live flag.
        let mut jdx = 0;
        while jdx < self.rejection_tracker.notified.len() {
            let record = self.rejection_tracker.notified[jdx].clone();
            let promise = record.promise;
            if promise.is_handled(&self.gc_heap) {
                self.rejection_tracker.notified.remove(jdx);
                self.fire_promise_rejection(record, true, report)?;
                continue;
            }
            jdx += 1;
        }
        Ok(())
    }

    /// Invoke the JS reporter for one promise. `handled` selects the event type
    /// (`rejectionhandled` vs `unhandledrejection`).
    ///
    /// A reporter that throws is reporting that nothing took the rejection and
    /// the run is over — Node's default for a rejection nobody handled — so the
    /// throw escapes the drain rather than being swallowed. It is marked as
    /// coming from a rejection so the host can name that origin.
    fn fire_promise_rejection(
        &mut self,
        record: TrackedRejection,
        handled: bool,
        report: &mut impl FnMut(&mut NativeCtx<'_>, &RunError) -> Result<bool, NativeError>,
    ) -> Result<(), RunError> {
        self.with_handle_scope(|vm, scope| {
            let ambient = vm.async_context();
            let ambient = vm.scoped_value(scope, ambient);
            let promise = vm.scoped_value(scope, Value::promise(record.promise));
            let async_context = vm.scoped_value(scope, record.async_context);
            vm.set_async_context(vm.escape_scoped(async_context));
            let promise = vm
                .escape_scoped(promise)
                .as_promise()
                .expect("tracked promise");
            let outcome = vm.fire_promise_rejection_in_extent(
                promise,
                record.context.as_ref(),
                handled,
                report,
            );
            vm.set_async_context(vm.escape_scoped(ambient));
            outcome
        })
    }

    fn fire_promise_rejection_in_extent(
        &mut self,
        promise: crate::promise::JsPromiseHandle,
        admitted: Option<&ExecutionContext>,
        handled: bool,
        report: &mut impl FnMut(&mut NativeCtx<'_>, &RunError) -> Result<bool, NativeError>,
    ) -> Result<(), RunError> {
        // A notification is a fresh synchronous callback extent. Previously
        // handled job diagnostics must not become this hook's completion.
        let _ = self.take_pending_uncaught_throw();
        let _ = self.take_error_detail();
        self.clear_throw_provenance();
        self.uncaught_from_promise_rejection = false;
        let reason = match promise.state(&self.gc_heap) {
            crate::promise::PromiseState::Rejected(reason) => reason,
            // Only rejected promises are tracked; a settled-elsewhere handle is
            // stale bookkeeping, skip it.
            _ => return Ok(()),
        };
        let promise_value = Value::promise(promise);
        if let Some(hook) = self.promise_rejection_hook() {
            let outcome = NativeCtx::with_host_context(
                self,
                NativeCallInfo::default_call(),
                admitted,
                |ctx| hook.notify(ctx, promise_value, reason, handled),
            );
            let outcome = outcome.map_err(|error| {
                let error = crate::native_to_vm_error(self, error);
                RunError {
                    error,
                    frames: self.take_uncaught_frames(),
                    detail: self.take_error_detail(),
                }
            });
            return match outcome {
                Ok(()) => Ok(()),
                Err(error) => self.report_microtask_failure(admitted, error, report),
            };
        }

        // Re-fetch per call: the reporter Value is not rooted across the
        // reentrant dispatch a previous fire may have moved it through.
        let Some(reporter) = crate::object::get(self.global_this, &self.gc_heap, REPORTER_GLOBAL)
        else {
            return Ok(());
        };
        if !reporter.is_callable() {
            return Ok(());
        }
        let this = Value::object(self.global_this);
        let args: smallvec::SmallVec<[Value; 8]> =
            smallvec::smallvec![promise_value, reason, Value::boolean(handled)];
        let context = self
            .callable_context(admitted, reporter)
            .map_err(RunError::bare)?;
        let mut stack = ActivationStack::new();
        let outcome = self.with_runtime_turn(&mut stack, |turn| {
            let (interp, stack) = turn.into_parts();
            interp.run_callable_sync_rooted(stack, context.as_ref(), &reporter, this, args)
        });
        match outcome {
            Ok(_) => Ok(()),
            Err(error) => {
                self.uncaught_from_promise_rejection = true;
                let detail = self.take_error_detail();
                let error = RunError {
                    error,
                    frames: self.take_uncaught_frames(),
                    detail,
                };
                self.report_microtask_failure(context.as_ref(), error, report)
            }
        }
    }

    /// Whether the throw now surfacing came out of the rejection checkpoint,
    /// clearing the mark. Read once, where the host names the origin.
    #[must_use]
    pub fn take_uncaught_from_promise_rejection(&mut self) -> bool {
        std::mem::take(&mut self.uncaught_from_promise_rejection)
    }
}
