//! Bounded direct-runtime driving of poll-mode Atomics.waitAsync waiters.
//!
//! # Contents
//! - Polling and ordinary reaction checkpoints on the owning isolate.
//! - Slice accounting across ready reaction chains and blocking waits.
//!
//! # Invariants
//! - Interruption stops before another settlement without cancelling parked work.
//! - The budget is checked between poll/checkpoint rounds, including ready rounds.
//! - A zero budget permits one nonblocking round; reactions are not preempted.
//! - Notify generation is captured before polling, so a concurrent wake is retained.
//! - Queue ownership, source contexts and settlement belong to the VM.
//!
//! # See also
//! - `otter_vm::Interpreter::poll_async_atomic_waits`.

use std::time::{Duration, Instant};

use crate::{OtterError, Runtime, enrich_runtime_diagnostic_with_cause, map_vm_error};

impl Runtime {
    /// Drive this isolate's poll-mode `Atomics.waitAsync` waiters until no
    /// waiter remains or `budget` elapses. Poll and drain each settlement's
    /// reactions, then wait for a notification or the nearest deadline.
    ///
    /// The budget bounds blocking waits and is checked between poll/checkpoint
    /// rounds, even when reactions immediately park more ready waiters. A zero
    /// budget performs one nonblocking round. Running reactions use the ordinary
    /// VM work budget and interrupt checks; this slice does not preempt them.
    /// Returns `true` when no poll-driven waiter remains.
    ///
    /// # Errors
    /// Propagates host interruption, settlement and microtask-drain failures.
    /// Interruption leaves pending work available for an explicit host resume.
    pub fn drive_pending_atomic_waits(&mut self, budget: Duration) -> Result<bool, OtterError> {
        let started = Instant::now();
        let interrupt = self.interp.interrupt_handle();
        loop {
            if interrupt.is_set() {
                return Err(OtterError::Interrupted);
            }
            let seen = otter_vm::atomics_wait::async_notify_generation();
            let (settled, pending) = self.interp.poll_async_atomic_waits().map_err(|error| {
                map_vm_error(otter_vm::RunError {
                    error,
                    frames: Vec::new(),
                    detail: self.interp.take_error_detail(),
                })
            })?;
            if settled > 0
                && let Err(err) = self.interp.drain_microtasks(|_, _| Ok(false))
                && !self.absorb_termination(&err)
            {
                return Err(enrich_runtime_diagnostic_with_cause(
                    &mut self.interp,
                    map_vm_error(err),
                ));
            }
            // A reaction may have parked another waiter after the poll counted
            // its queue. Inspect the live owner without settling another round.
            let pending = if settled > 0 {
                self.interp.has_pending_atomic_waits()
            } else {
                pending != 0
            };
            if !pending {
                return Ok(true);
            }
            let Some(mut wait) = budget
                .checked_sub(started.elapsed())
                .filter(|d| !d.is_zero())
            else {
                return Ok(false);
            };
            if settled > 0 {
                continue;
            }
            if let Some(deadline) = self.interp.next_atomic_wait_deadline() {
                wait = wait.min(deadline.saturating_duration_since(Instant::now()));
            }
            // Interrupt handles do not own the global notify condition variable.
            wait = wait.min(Duration::from_millis(50));
            let _ = otter_vm::atomics_wait::wait_for_async_notify(seen, wait);
        }
    }
}
