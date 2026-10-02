//! Structured exceptions through the canonical compiled activation.
//!
//! # Contents
//! - [`RuntimeCall::exception_op`] raises a compiled `TdzError` and routes it.
//! - [`RuntimeCall::route_throw`] lands a committed throw in the frame's own
//!   handler or propagates its unchanged value.
//!
//! # Invariants
//! - The handler is the function's handler-table entry covering the PC the
//!   compiled frame published for the raising instruction, in every tier.
//! - A caught value is written to its handler register before the handler's
//!   PC is selected; the throwing operation is never replayed.
//! - Frame views never survive allocation or JavaScript reentry.
//!
//! # See also
//! - [`crate::CodeBlockControlFlowView::handler_at`]

use super::RuntimeCall;
use crate::{JitExceptionOutcome, Value, VmError};
use otter_bytecode::Op;

impl RuntimeCall<'_> {
    /// Raise the exception of a compiled exception opcode and route it.
    pub fn exception_op(
        &mut self,
        opcode: u8,
        arg0: u64,
        _arg1: u64,
        _arg2: u64,
    ) -> Result<JitExceptionOutcome, VmError> {
        if opcode != Op::TdzError as u8 {
            return Err(VmError::InvalidOperand);
        }
        let local_index = u32::try_from(arg0).map_err(|_| VmError::InvalidOperand)?;
        // SAFETY: the bound call owns exclusive mutator access; no frame view
        // is retained across the throwable allocation.
        let value = unsafe { &mut *self.vm.as_ptr() }.jit_tdz_throwable(
            &self.context,
            unsafe { &mut *self.stack.as_ptr() },
            local_index,
        )?;
        Ok(match self.route_throw(value)? {
            Some(pc) => JitExceptionOutcome::Resume(pc),
            None => JitExceptionOutcome::Throw(value),
        })
    }

    /// Route an already-committed pure throw. A handler result selects a new
    /// PC; `None` propagates the original exception without pending-throw
    /// storage.
    pub fn route_throw(&mut self, exception: Value) -> Result<Option<u32>, VmError> {
        let handler = {
            let function = self
                .context
                .exec_function(self.function_id())
                .ok_or(VmError::InvalidOperand)?;
            function.control_flow().handler_at(self.pc())
        };
        let Some(handler) = handler else {
            return Ok(None);
        };
        self.write(handler.exception, exception)?;
        // The published register now roots the value; acknowledgement neither
        // allocates nor changes the activation's language-visible state.
        unsafe { &mut *self.vm.as_ptr() }.jit_acknowledge_caught_throw();
        Ok(Some(handler.target))
    }
}
