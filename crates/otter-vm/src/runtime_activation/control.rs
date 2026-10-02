//! Typed control, exception, and cold-deopt operations.
//!
//! # Contents
//! - [`BackedgePollOutcome`] — what a compiled back-edge does after its poll.
//! - [`RuntimeCall::backedge_poll`] — the cooperative loop checkpoint.
//!
//! # Invariants
//! - Identity branching stays in the VM boundary rather than leaking
//!   interpreter stack indices to the JIT.
//! - Only a baseline frame leaves for optimizing OSR: its complete register
//!   window is already rooted, and the loop header owns the tier transition.
//! - A poll from any other frame runs under an always-allocate scope: the
//!   frame's live registers are not rooted at the poll, so tier-up work that
//!   allocates must not start a collection.
//!
//! # See also
//! - [`crate::native_abi::Frame`] owns the canonical register window.

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
    /// Resume at the loop header to enter optimizing OSR.
    EnterOptimizing,
}

impl RuntimeCall<'_> {
    /// Poll interrupts and the work budget at one compiled back-edge.
    pub fn backedge_poll(&mut self) -> Result<BackedgePollOutcome, VmError> {
        // SAFETY: the branded call owns exclusive mutator access for this
        // short operation and retains only a raw descriptor afterwards.
        let context = &self.context;
        let vm = unsafe { &mut *self.vm.as_ptr() };
        // SAFETY: the polling frame stays published for this call.
        let header = unsafe { self.frame.as_ref() }.header;
        // An optimizing frame reaches this poll without a safepoint: its live
        // tagged registers are not rooted. Tier-up work the poll performs may
        // allocate, so it must not collect while that frame is suspended here.
        let _no_collection =
            (header.kind != NativeFrameKind::Baseline).then(|| vm.gc_heap.always_allocate_scope());
        let batch = vm.jit_backedge_fuel_window;
        let checkpoint = vm.jit_backedge_poll()?;
        if header.kind == NativeFrameKind::Baseline
            && vm.baseline_backedges_reach_osr(context, header.function_id, header.pc, batch)
        {
            return Ok(BackedgePollOutcome::EnterOptimizing);
        }
        Ok(if checkpoint == WorkBudgetCheckpoint::Yield {
            BackedgePollOutcome::Yield
        } else {
            BackedgePollOutcome::Continue
        })
    }
}
