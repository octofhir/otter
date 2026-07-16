//! Compiled-callee finish and abort lifecycle.
//!
//! # Contents
//! - Normal-return cleanup and destination commit.
//! - Bail-to-interpreter resumption for an already-published callee.
//! - Abort cleanup for throw or invalid nested-frame state.
//!
//! # Invariants
//! - Every path releases exactly the synchronous re-entry acquired by
//!   `prepare_jit_resolved_call`.
//! - A normal return removes exactly the prepared callee frame before committing
//!   its value to the caller.
//! - Native linkage releases the stable code-entry lease before invoking any
//!   finish path; the published VM frame then owns all live values.
//! - Abort reclaims every callee/nested register window after native entry has
//!   already released its generation lease.
//!
//! # See also
//! - [`super::frame`] for the paired prepare transaction and code pinning.
//! - [`crate::jit::JitPreparedDirectCall`] for the staged frame identity.

use crate::*;

impl Interpreter {
    /// Finish a direct compiled call that returned normally.
    ///
    /// Pops and reclaims the published callee frame, stores `value` into the
    /// caller destination register, and releases the sync-reentry guard held by
    /// [`Self::jit_prepare_direct_call`].
    pub fn jit_finish_direct_call_returned(
        &mut self,
        stack: &mut HoltStack,
        caller_frame_index: usize,
        callee_frame_index: usize,
        dst: u16,
        value: Value,
    ) -> Result<(), VmError> {
        if stack.len() != callee_frame_index + 1 {
            self.leave_sync_reentry();
            return Err(VmError::InvalidOperand);
        }
        if let Some(mut done) = stack.pop() {
            self.note_jit_entry_success(done.function_id);
            self.reclaim_registers(&mut done);
        }
        let caller = stack
            .get_mut(caller_frame_index)
            .ok_or(VmError::InvalidOperand)?;
        *caller
            .registers
            .get_mut(dst as usize)
            .ok_or(VmError::InvalidOperand)? = value;
        self.leave_sync_reentry();
        Ok(())
    }

    /// Finish a direct compiled call whose callee bailed to the interpreter.
    ///
    /// Resumes at `bail_pc` inside the already-published callee frame, stores
    /// the completion into the caller destination, and releases re-entry state.
    pub fn jit_finish_direct_call_bailed(
        &mut self,
        context: &ExecutionContext,
        stack: &mut HoltStack,
        caller_frame_index: usize,
        callee_frame_index: usize,
        dst: u16,
        bail_pc: u32,
    ) -> Result<(), VmError> {
        if callee_frame_index >= stack.len() {
            self.leave_sync_reentry();
            return Err(VmError::InvalidOperand);
        }
        self.note_jit_entry_bail(stack[callee_frame_index].function_id);
        stack[callee_frame_index].pc = bail_pc;
        match self.dispatch_loop(context, stack) {
            Ok(value) => {
                let caller = stack
                    .get_mut(caller_frame_index)
                    .ok_or(VmError::InvalidOperand)?;
                *caller
                    .registers
                    .get_mut(dst as usize)
                    .ok_or(VmError::InvalidOperand)? = value;
                self.leave_sync_reentry();
                Ok(())
            }
            Err(err) => {
                self.leave_sync_reentry();
                Err(err)
            }
        }
    }

    /// Abort a prepared direct call before normal return completion.
    ///
    /// Used by direct-call throw paths: drops the callee frame and any nested
    /// frames above it, releases their code anchors, and leaves sync re-entry.
    pub fn jit_abort_direct_call(&mut self, stack: &mut HoltStack, callee_frame_index: usize) {
        self.truncate_frame_stack_reclaiming(stack, callee_frame_index);
        self.leave_sync_reentry();
    }

    #[inline]
    fn truncate_frame_stack_reclaiming(&mut self, stack: &mut HoltStack, len: usize) {
        while stack.len() > len {
            if let Some(mut frame) = stack.pop() {
                self.reclaim_registers(&mut frame);
            }
        }
    }
}
