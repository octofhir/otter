//! Direct-session timer and liveness operations.
//!
//! # Contents
//! Pending callbacks, timer cancellation and event-loop liveness.
//!
//! # Invariants
//! Each callback is associated with its owning realm.
//!
//! # See also
//! - [`crate::Runtime`] owns the isolate session.

use crate::*;

impl Runtime {
    /// `true` when the per-isolate `TimerCallbacks` table has any
    /// outstanding entries. The isolate runner uses this to decide
    /// whether the script's run loop needs to keep ticking the
    /// inbox after the synchronous body returns.
    #[must_use]
    pub fn has_pending_timer_callbacks(&self) -> bool {
        !self.interp.timer_callbacks().is_empty()
    }

    /// Cancel every JavaScript timer currently owned by this runtime.
    ///
    /// Callback roots are detached before the host scheduler is asked to
    /// cancel its deadlines, so a wake that loses the cancellation race is a
    /// harmless miss. Runtime-handle process finalization uses this boundary
    /// after exit listeners return; direct embedders may use it when ending
    /// their own hosted process lifecycle.
    ///
    /// Returns the number of callback entries detached from the VM.
    pub fn cancel_all_timers(&mut self) -> usize {
        self.interp.cancel_all_timers()
    }

    /// Cancel one JavaScript timer after a fatal handle-runner callback.
    pub(crate) fn cancel_timer_callback(&mut self, token: u64) -> bool {
        self.interp.cancel_timer(token)
    }

    /// `true` when the isolate still owes observable local work: queued
    /// microtasks or live timer entries.
    ///
    /// This is the idle predicate for an embedder that drives the
    /// isolate from its own event loop. A `false` result means the
    /// caller may block on its own wake sources without stranding JS work.
    /// Async native operations carry their own completion admission and
    /// liveness through the installed host sink rather than a raw Promise-id
    /// registry.
    ///
    /// Cheap: two counter reads, no heap walk. Safe to call every
    /// loop turn.
    #[must_use]
    pub fn has_pending_work(&self) -> bool {
        self.microtask_stats().pending || self.has_pending_timer_callbacks()
    }

    /// Fire the timer identified by `token`. Routes through
    /// the per-isolate `TimerCallbacks` table to recover the JS
    /// callable + extra arguments + execution context, invokes the
    /// callback as a top-level call against that context, and drains
    /// any microtasks the callback queued. Returns [`TimerFireOutcome`]
    /// so the isolate runner can update timer liveness without
    /// treating a cancelled timer as a fired task. Returns an
    /// [`OtterError`] when the callback raises an unhandled throw.
    ///
    /// One-shot (`setTimeout`) entries are removed from the table
    /// before invocation so a re-entrant `clearTimeout(token)`
    /// from inside the callback observes the entry as gone.
    /// Repeating (`setInterval`) entries stay in the table; the
    /// host scheduler re-arms them on its own.
    ///
    /// Embedders that drive the isolate from their own event loop call
    /// this when a deadline they scheduled from
    /// [`TimerScheduler::schedule`](otter_vm::TimerScheduler::schedule)
    /// elapses. The token is the one that `schedule` returned.
    pub fn fire_timer(&mut self, token: u64) -> Result<TimerFireOutcome, OtterError> {
        self.with_direct_timeout(move |runtime| runtime.fire_timer_unbounded(token))
    }

    pub(super) fn fire_timer_unbounded(
        &mut self,
        token: u64,
    ) -> Result<TimerFireOutcome, OtterError> {
        let entry = match self.interp.timer_callbacks().get(token).cloned() {
            Some(entry) => entry,
            None => return Ok(TimerFireOutcome::Missing),
        };
        let repeat = entry.repeat_ms.is_some();
        // A one-shot Timeout stays visible to `getActiveResourcesInfo()`
        // while its own callback runs — Node destroys it after the callback
        // returns. An Immediate is gone before its callback, also as in Node.
        let immediate = matches!(entry.kind, otter_vm::TimerKind::Immediate);
        if !repeat && immediate {
            self.interp.timer_callbacks_mut().remove(token);
        }
        // Entry values are rooted by timer_delivery before any callback or
        // handler can collect. The existing realm swap encloses the entire
        // callback/report extent and restores the caller realm on all outcomes.
        let delivered = self.interp.with_host_realm_id(entry.realm_id, |interp| {
            Ok(timer_delivery::invoke(interp, &entry))
        });
        if !repeat && !immediate {
            self.interp.timer_callbacks_mut().remove(token);
        }
        match delivered {
            Err(error) => return Err(map_vm_error(otter_vm::RunError::bare(error))),
            Ok(Err(error)) => {
                if let Some(code) = error.exit_code() {
                    self.pending_exit_code = Some(code);
                    return Ok(TimerFireOutcome::Fired { repeat });
                }
                return Err(map_native_error(error));
            }
            Ok(Ok(())) => {}
        }
        // The drain offers an uncaught task throw to `process` and any active
        // domain exactly as the callback's own throw above was offered.
        let outcome = self.drain_microtasks_dispatching_uncaught();
        match outcome {
            Ok(()) => Ok(TimerFireOutcome::Fired { repeat }),
            Err(err) => {
                if let otter_vm::VmError::Exit { code } = err.error {
                    self.pending_exit_code = Some(code);
                    return Ok(TimerFireOutcome::Fired { repeat });
                }
                Err(enrich_runtime_diagnostic_with_cause(
                    &mut self.interp,
                    map_vm_error(err),
                ))
            }
        }
    }
}
