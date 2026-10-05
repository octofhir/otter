//! Direct Runtime checkpoint admission after a script completion.
//!
//! # Contents
//! - The single fatal/escaping-OOM checkpoint predicate.
//! - Conditional checkpoint entry without consuming the original `RunError`.
//!
//! # Invariants
//! - Structural and control failures use the VM's canonical `is_fatal` policy.
//! - An escaping actual OOM also stops further JavaScript execution in this turn;
//!   a caught OOM whose script succeeds still permits the normal checkpoint.
//! - Catchable script errors still reach the checkpoint; the existing script
//!   error precedence is unchanged.
//! - No queue is cancelled or copied. Unexecuted jobs stay in the existing VM
//!   queue with their original context and roots. A deliberate later direct
//!   Runtime turn executes its script before resuming those jobs. Layer B process
//!   exit and task cancellation remain owned by the isolate runner.
//!
//! # See also
//! - [`otter_vm::VmError::is_fatal`]
//! - [`crate::Runtime::run_script`]

use otter_vm::{RunError, VmError};

pub(crate) fn stops(error: &VmError) -> bool {
    error.is_fatal() || matches!(error, VmError::OutOfMemory { .. })
}

/// Keep the prior completion untouched while deciding whether to enter JS jobs.
pub(crate) fn after_script<T>(
    script: &Result<T, RunError>,
    drain: impl FnOnce() -> Result<(), RunError>,
) -> Result<(), RunError> {
    if script
        .as_ref()
        .err()
        .is_some_and(|error| stops(&error.error))
    {
        return Ok(());
    }
    drain()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fatal_or_escaping_oom_preserves_the_prior_completion_without_entering_drain() {
        for error in [
            VmError::InvalidOperand,
            VmError::MissingReturn,
            VmError::Exit { code: 27 },
            VmError::Interrupted,
            VmError::BudgetExceeded,
            VmError::OutOfMemory {
                requested_bytes: 817,
                heap_limit_bytes: 16384,
            },
        ] {
            let script: Result<(), RunError> = Err(RunError::bare(error));
            after_script(&script, || panic!("fatal completion entered JS checkpoint"))
                .expect("prior completion skips checkpoint");
            assert_eq!(script.as_ref().unwrap_err().error, error);
        }
    }

    #[test]
    fn successful_and_catchable_completions_enter_the_existing_drain_once() {
        for script in [Ok(()), Err(RunError::bare(VmError::Uncaught))] {
            let mut entered = 0;
            let drained = after_script(&script, || {
                entered += 1;
                Err(RunError::bare(VmError::MissingReturn))
            });
            assert_eq!(entered, 1);
            assert_eq!(drained.unwrap_err().error, VmError::MissingReturn);
        }
    }
}
