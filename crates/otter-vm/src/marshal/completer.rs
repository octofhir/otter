//! The async-method protocol: pending promises settled by host work.
//!
//! An async binding body is a `Send` future producing plain Rust
//! data. The glue calls [`MarshalCx::promise_from_future`], which
//! polls the future once on the spot — an immediately-ready future
//! (a data-only method like `Blob.prototype.bytes`) settles through
//! the ordinary fulfilled/rejected-promise path with no executor
//! round-trip — and otherwise creates a pending promise, spawns the
//! future through the isolate's [`HostCompletionSink`], and settles
//! via a [`PromiseCompleter`] when it resolves.
//!
//! The completer is `Send` and carries no GC handle: the promise
//! travels as a persistent-root id and is re-resolved on the isolate
//! thread inside the completion job, where the `IntoJs` conversion of
//! the future's result also runs.
//!
//! # Contents
//! - [`PromiseCompleter`] — one-shot settle token for a pending
//!   promise.
//! - [`MarshalCx::promise_pending`] — pending promise + completer.
//! - [`MarshalCx::promise_from_future`] — the full protocol.
//!
//! # Invariants
//! - Nothing GC-touching crosses `.await`: the future's captures are
//!   `Send` Rust data, and the conversion to JS values happens inside
//!   the completion job on the mutator turn.
//! - The completer is one-shot; dropping it unsettled releases the
//!   promise's persistent root (the promise then simply never
//!   settles, the correct terminal behavior for abandoned work) and
//!   the liveness hold.
//! - A suspended completion materializes its JS result and drains reactions in
//!   the realm that created the promise. A disposed origin drops the root
//!   instead of settling into another realm.
//! - Rejection reasons materialize as real `TypeError` instances when
//!   the captured execution context allows constructor re-entry, and
//!   degrade to string reasons otherwise.
//!
//! # See also
//! - [`crate::host_completion`] — the sink contract this rides.
//! - `docs/site/src/content/docs/extensions/declarative-bindings.md`

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll, Waker};

use crate::host_completion::{
    HostCompletionAdmission, HostCompletionJob, HostCompletionOutcome, HostCompletionSink,
};
use crate::persistent_roots::PersistentRootId;
use crate::promise::JsPromise;
use crate::{ExecutionContext, Interpreter, NativeCallInfo, NativeCtx, Value};

use super::cx::MarshalCx;
use super::error::JsError;
use super::into_js::IntoJs;
use crate::handles::Local;

/// One-shot settle token for a pending promise created by
/// [`MarshalCx::promise_pending`]. `Send`; carries no GC handles.
pub struct PromiseCompleter {
    root: Option<PersistentRootId>,
    sink: Arc<dyn HostCompletionSink>,
    context: Option<ExecutionContext>,
    realm_id: u32,
    admission: Option<HostCompletionAdmission>,
}

impl std::fmt::Debug for PromiseCompleter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PromiseCompleter")
            .field("settled", &self.root.is_none())
            .finish_non_exhaustive()
    }
}

impl PromiseCompleter {
    /// Fulfill the promise with a Rust value, converted via `IntoJs`
    /// on the isolate thread.
    pub fn resolve<R: IntoJs + Send + 'static>(self, value: R) {
        self.finish::<R>(Ok(value));
    }

    /// Reject the promise.
    pub fn reject(self, error: JsError) {
        self.finish::<()>(Err(error));
    }

    fn finish<R: IntoJs + Send + 'static>(mut self, result: Result<R, JsError>) {
        let Some(root) = self.root.take() else { return };
        let admission = self
            .admission
            .take()
            .expect("a pending promise owns one completion admission");
        let context = self.context.clone();
        let realm_id = self.realm_id;
        if let Err(error) = self.sink.complete(
            admission,
            HostCompletionJob::new_with_cancel(
                move |interp| {
                    settle_from_root(interp, root, realm_id, context, result);
                },
                move |interp| {
                    interp.persistent_root_remove(root);
                },
            ),
            HostCompletionOutcome::Completed,
        ) {
            report_delivery_failure(&error);
        }
    }
}

impl Drop for PromiseCompleter {
    fn drop(&mut self) {
        // Abandoned without settling: release the promise's persistent
        // root on the isolate so it can be collected. The promise
        // never settles — the correct terminal state for dropped work.
        if let Some(root) = self.root.take() {
            let admission = self
                .admission
                .take()
                .expect("a pending promise owns one completion admission");
            if let Err(error) = self.sink.complete(
                admission,
                HostCompletionJob::new_with_cancel(
                    move |interp| {
                        interp.persistent_root_remove(root);
                    },
                    move |interp| {
                        interp.persistent_root_remove(root);
                    },
                ),
                HostCompletionOutcome::Cancelled,
            ) {
                report_delivery_failure(&error);
            }
        }
    }
}

/// A host contract violation has no safe VM re-entry path left. Report it
/// without panicking (including while unwinding an aborted future).
fn report_delivery_failure(error: &str) {
    use std::io::Write as _;

    let _ = writeln!(
        std::io::stderr().lock(),
        "otter: host completion delivery failed: {error}"
    );
}

/// Settle the promise parked at `root` with `result`, converting on
/// the isolate thread. Runs as a host completion job.
fn settle_from_root<R: IntoJs>(
    interp: &mut Interpreter,
    root: PersistentRootId,
    realm_id: u32,
    context: Option<ExecutionContext>,
    result: Result<R, JsError>,
) {
    let outcome = interp.with_host_realm_id(realm_id, move |interp| {
        settle_from_root_in_active_realm(interp, root, context, result);
        Ok(())
    });
    if outcome.is_err() {
        let _ = interp.persistent_root_remove(root);
    }
}

fn settle_from_root_in_active_realm<R: IntoJs>(
    interp: &mut Interpreter,
    root: PersistentRootId,
    context: Option<ExecutionContext>,
    result: Result<R, JsError>,
) {
    let Some(promise_value) = interp.persistent_root_remove(root) else {
        return;
    };
    if promise_value.as_promise().is_none() {
        return;
    }
    NativeCtx::with_host_context(
        interp,
        NativeCallInfo::default_call(),
        context.as_ref(),
        |ctx| {
            ctx.scope(|scope| {
                let mut cx = MarshalCx::new(scope);
                // The promise handle must survive the conversion allocations.
                let promise_handle = cx.park(promise_value);
                let settled = match result {
                    Ok(value) => value.into_js(&mut cx).map(|out| (out, true)),
                    Err(error) => reject_reason(&mut cx, &error).map(|out| (out, false)),
                };
                let (out, fulfil) = match settled {
                    Ok(pair) => pair,
                    Err(error) => match reject_reason(&mut cx, &error) {
                        Ok(out) => (out, false),
                        // Conversion of the failure itself failed (OOM-class);
                        // leave the promise unsettled rather than lie.
                        Err(_) => return,
                    },
                };
                let raw_out = cx.escape(out);
                let Some(promise) = cx.escape(promise_handle).as_promise() else {
                    return;
                };
                let jobs = if fulfil {
                    promise.fulfill(cx.heap_mut(), raw_out)
                } else {
                    promise.reject(cx.heap_mut(), raw_out)
                };
                let interp = cx.ctx().interp_mut();
                interp.note_settle_rejection(&jobs);
                for job in jobs.jobs {
                    interp.microtasks_mut().enqueue(job);
                }
            });
        },
    );
    // Drain with the settling context as the fallback: the reactions this
    // settlement unblocks can queue further microtasks of their own (an async
    // reaction that `await`s again — e.g. `(await fetch(...)).text()`), and
    // those continuation jobs have no origin context. Falling back to `None`
    // there aborts the drain mid-chain, stranding the tail; the settling
    // context lets it run to completion.
    let _ = interp.drain_microtasks_with_default(context);
}

/// Build the JS rejection reason for a binding error: a real error
/// instance when the captured context allows constructor re-entry,
/// else the rendered message string.
fn reject_reason<'s>(
    cx: &mut MarshalCx<'_, '_, 's>,
    error: &JsError,
) -> Result<Local<'s>, JsError> {
    let (ctor_name, message) = match error {
        JsError::Type(m) => ("TypeError", m.clone()),
        JsError::Range(m) => ("RangeError", m.clone()),
        JsError::Dom { name, message } => ("TypeError", format!("{name}: {message}")),
        JsError::Thrown(m) => ("Error", m.clone()),
    };
    if cx.ctx().execution_context().is_some()
        && let Some(ctor) = cx.ctx().global_value(ctor_name)
    {
        let ctor_handle = cx.park(ctor);
        let message_handle = cx.string(&message)?;
        let raw_ctor = cx.escape(ctor_handle);
        let raw_message = cx.escape(message_handle);
        if let Ok(instance) = cx.ctx().construct(raw_ctor, &[raw_message]) {
            return Ok(cx.park(instance));
        }
    }
    cx.string(&message)
}

impl<'rt, 'cx, 's> MarshalCx<'rt, 'cx, 's> {
    /// Create a pending promise plus its one-shot [`PromiseCompleter`].
    /// Requires the isolate's host completion sink (installed by the
    /// runtime layer); host-less embeddings get a `TypeError`.
    pub fn promise_pending(&mut self) -> Result<(Local<'s>, PromiseCompleter), JsError> {
        let Some(sink) = self.ctx().interp_mut().host_completion_sink() else {
            return Err(JsError::Type(
                "async host completions are not available in this embedding".to_string(),
            ));
        };
        let admission = sink.admit().map_err(JsError::Type)?;
        self.promise_pending_admitted(sink, admission)
    }

    fn promise_pending_admitted(
        &mut self,
        sink: Arc<dyn HostCompletionSink>,
        admission: HostCompletionAdmission,
    ) -> Result<(Local<'s>, PromiseCompleter), JsError> {
        let context = self.ctx().context_ref().cloned();
        let interp = self.ctx().interp_mut();
        let realm_id = interp.active_host_realm_id();
        let handle = crate::promise_dispatch::pending_runtime_rooted(interp, &[], &[])
            .map_err(|err| JsError::Type(err.to_string()))?;
        let promise_value = Value::promise(handle);
        let root = interp.persistent_root_insert(promise_value);
        let parked = self.park(promise_value);
        Ok((
            parked,
            PromiseCompleter {
                root: Some(root),
                sink,
                context,
                realm_id,
                admission: Some(admission),
            },
        ))
    }

    /// The full async-method protocol: admit and construct `future`, then poll
    /// it once on the spot. An already-ready result settles through the
    /// ordinary pre-settled promise path with no executor round-trip; otherwise
    /// the future is spawned on the host executor with a completer.
    pub fn promise_from_future<R, F, Make>(
        &mut self,
        make_future: Make,
    ) -> Result<Local<'s>, JsError>
    where
        R: IntoJs + Send + 'static,
        F: Future<Output = Result<R, JsError>> + Send + 'static,
        Make: FnOnce() -> F,
    {
        self.promise_from_future_with(|| ((), make_future()))
            .map(|(promise, ())| promise)
    }

    /// Variant of [`Self::promise_from_future`] whose admitted factory also
    /// returns an isolate-local sidecar, such as a cancellation handle.
    /// Admission still precedes both values' construction. An embedding with
    /// no completion sink fails before the factory is invoked.
    pub fn promise_from_future_with<R, F, Sidecar, Make>(
        &mut self,
        make: Make,
    ) -> Result<(Local<'s>, Sidecar), JsError>
    where
        R: IntoJs + Send + 'static,
        F: Future<Output = Result<R, JsError>> + Send + 'static,
        Make: FnOnce() -> (Sidecar, F),
    {
        let sink = self
            .ctx()
            .interp_mut()
            .host_completion_sink()
            .ok_or_else(|| {
                JsError::Type(
                    "async host completions are not available in this embedding".to_string(),
                )
            })?;
        // Constructing or polling a future may start I/O or register reactor
        // state. Reserve its terminal completion before either operation.
        let admission = sink.admit().map_err(JsError::Type)?;
        let (sidecar, future) = make();
        let mut pinned: Pin<Box<dyn Future<Output = Result<R, JsError>> + Send>> = Box::pin(future);
        // Eager first poll: an immediately-ready future (a data-only
        // method) settles with no executor round-trip. Reactor-backed
        // futures need the executor's context to register wakers, so
        // the poll runs through the sink when one is installed.
        let waker = Waker::noop();
        let mut poll_cx = Context::from_waker(waker);
        let mut ready: Option<Result<R, JsError>> = None;
        {
            let mut poll_once = || {
                if let Poll::Ready(result) = pinned.as_mut().poll(&mut poll_cx) {
                    ready = Some(result);
                }
            };
            sink.with_executor_context(&mut poll_once);
        }
        if let Some(result) = ready {
            let promise = match result {
                Ok(value) => value
                    .into_js(self)
                    .and_then(|out| self.promise_fulfilled(out)),
                Err(error) => {
                    reject_reason(self, &error).and_then(|reason| self.promise_rejected(reason))
                }
            };
            let outcome = if promise.is_ok() {
                HostCompletionOutcome::Completed
            } else {
                HostCompletionOutcome::Failed
            };
            if let Err(error) = sink.finish_inline(admission, outcome) {
                report_delivery_failure(&error);
            }
            return promise.map(|promise| (promise, sidecar));
        }
        let (promise, completer) = self.promise_pending_admitted(sink.clone(), admission)?;
        sink.spawn(Box::pin(drive(pinned, completer)));
        Ok((promise, sidecar))
    }
}

/// Drive an already-polled future to completion and settle.
async fn drive<R: IntoJs + Send + 'static>(
    future: Pin<Box<dyn Future<Output = Result<R, JsError>> + Send>>,
    completer: PromiseCompleter,
) {
    match future.await {
        Ok(value) => completer.resolve(value),
        Err(error) => completer.reject(error),
    }
}
