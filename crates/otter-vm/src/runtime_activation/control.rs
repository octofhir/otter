//! Typed control, exception, and cold-deopt operations.
//!
//! # Contents
//! - [`BackedgePollOutcome`] — what a compiled back-edge does after its poll.
//! - [`RuntimeCall::backedge_poll`] — the cooperative loop checkpoint.
//!
//! # Invariants
//! - Identity branching stays in the VM boundary rather than leaking
//!   interpreter stack indices to the JIT.
//! - Only a baseline frame is asked to relink: its registers already live in
//!   the interpreter window, so resuming at the loop header replays nothing.
//!
//! # See also
//! - `Interpreter::take_backedge_relink` owns the relink decision.

use crate::VmError;
use crate::native_abi::NativeFrameKind;
use crate::work_budget::WorkBudgetCheckpoint;

use super::RuntimeCall;

/// What a compiled back-edge does after its cooperative poll.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackedgePollOutcome {
    /// Continue the loop in native code.
    Continue,
    /// The work budget rotated its slice; the loop still continues.
    Yield,
    /// Resume the interpreter at the loop header. The running baseline body
    /// was compiled before some of its call targets owned entry code; the VM
    /// discarded it so the header recompiles with generated linkage.
    Relink,
}

impl RuntimeCall<'_> {
    /// Poll interrupts and the work budget at one compiled back-edge.
    pub fn backedge_poll(&mut self) -> Result<BackedgePollOutcome, VmError> {
        // SAFETY: the branded call owns exclusive mutator access for this
        // short operation and retains only a raw descriptor afterwards.
        let context = &self.context;
        let vm = unsafe { &mut *self.vm.as_ptr() };
        let checkpoint = vm.jit_backedge_poll(context)?;
        // SAFETY: the polling frame stays published for this call.
        let header = unsafe { self.frame.as_ref() }.header;
        if header.kind == NativeFrameKind::Baseline
            && vm.take_backedge_relink(context, header.function_id)
        {
            return Ok(BackedgePollOutcome::Relink);
        }
        Ok(if checkpoint == WorkBudgetCheckpoint::Yield {
            BackedgePollOutcome::Yield
        } else {
            BackedgePollOutcome::Continue
        })
    }
}
