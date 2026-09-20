//! Structured exceptions through the canonical compiled activation.
//!
//! # Contents
//! - [`RuntimeCall::exception_op`] completes materialized exception operations
//!   and the static catch-only subset for generated stack windows.
//! - [`RuntimeCall::route_throw`] consumes a committed throw in the innermost
//!   local catch or propagates its unchanged value.
//!
//! # Invariants
//! - Stack-owned catch state is the verified CodeBlock's active-region table,
//!   not a second mutable handler stack. Exact deopt reconstructs that table.
//! - Finally and dynamic completion operations exit before effects.
//! - A caught value is written to its published destination before selecting
//!   the catch PC; the throwing operation is never replayed.
//! - Frame views never survive allocation or JavaScript reentry.
//!
//! # See also
//! - [`crate::CodeBlockControlFlowView::active_catch_regions`]
//! - [`crate::Interpreter::jit_rebuild_materialized_catch_handlers`]

use super::{RuntimeCall, RuntimeFrameIdentity};
use crate::{JitExceptionOutcome, Value, VmError};
use otter_bytecode::Op;

impl RuntimeCall<'_> {
    /// Complete an exception opcode using the activation's physical owner.
    pub fn exception_op(
        &mut self,
        opcode: u8,
        arg0: u64,
        arg1: u64,
        arg2: u64,
    ) -> Result<JitExceptionOutcome, VmError> {
        if let RuntimeFrameIdentity::Materialized(index) = self.identity {
            // SAFETY: the bound call owns exclusive mutator access; no native
            // frame view is retained across canonical unwind or allocation.
            return unsafe { &mut *self.vm.as_ptr() }.jit_runtime_exception_op(
                &self.context,
                unsafe { &mut *self.stack.as_ptr() },
                index,
                opcode,
                arg0,
                arg1,
                arg2,
            );
        }
        // Count the declared transition once even when the static handler
        // operation needs no materialized sidecar or JavaScript reentry.
        unsafe { &mut *self.vm.as_ptr() }
            .record_jit_runtime_stub_class(crate::native_abi::RuntimeStubClass::Reentrant);
        let (function_id, pc) = (self.function_id(), self.pc());
        let context = &self.context;
        let function = context
            .exec_function(function_id)
            .ok_or(VmError::InvalidOperand)?;
        let instruction = function
            .instr_at_index(pc as usize)
            .ok_or(VmError::InvalidOperand)?;
        if function.op(instruction) as u8 != opcode {
            return Err(VmError::InvalidOperand);
        }
        match opcode {
            value if value == Op::EnterTry as u8 => {
                let region = function
                    .control_flow()
                    .exception_region(pc)
                    .ok_or(VmError::InvalidOperand)?;
                if region
                    .catch_pc
                    .map(u64::from)
                    .unwrap_or(u64::from(u32::MAX))
                    != arg0
                    || region
                        .finally_pc
                        .map(u64::from)
                        .unwrap_or(u64::from(u32::MAX))
                        != arg1
                    || u64::from(region.exception_register) != arg2
                {
                    return Err(VmError::InvalidOperand);
                }
                if function
                    .control_flow()
                    .active_catch_regions(pc.checked_add(1).ok_or(VmError::InvalidOperand)?)
                    .is_ok()
                {
                    return Ok(JitExceptionOutcome::Continue);
                }
            }
            value
                if value == Op::LeaveTry as u8
                    && function.control_flow().active_catch_regions(pc).is_ok() =>
            {
                return Ok(JitExceptionOutcome::Continue);
            }
            _ => {}
        }
        Ok(JitExceptionOutcome::Resume(pc))
    }

    /// Route an already-committed pure throw. A catch result selects a new PC;
    /// `None` propagates the original exception without pending-throw storage.
    pub fn route_throw(&mut self, exception: Value) -> Result<Option<u32>, VmError> {
        if let RuntimeFrameIdentity::Materialized(index) = self.identity {
            // SAFETY: no frame/register borrow survives canonical unwind.
            return unsafe { &mut *self.vm.as_ptr() }.jit_route_throw(
                &self.context,
                unsafe { &mut *self.stack.as_ptr() },
                index,
                exception,
            );
        }
        let handler = {
            // SAFETY: metadata inspection cannot collect or reenter JavaScript.
            let (function_id, pc) = (self.function_id(), self.pc());
            let context = &self.context;
            let function = context
                .exec_function(function_id)
                .ok_or(VmError::InvalidOperand)?;
            function
                .control_flow()
                .active_catch_regions(pc)
                .map_err(|_| VmError::InvalidOperand)?
                .last()
        };
        let Some(handler) = handler else {
            return Ok(None);
        };
        let catch_pc = handler.catch_pc.ok_or(VmError::InvalidOperand)?;
        self.write(handler.exception_register, exception)?;
        // The published register now roots the value; acknowledgement neither
        // allocates nor changes the activation's language-visible state.
        unsafe { &mut *self.vm.as_ptr() }.jit_acknowledge_caught_throw();
        Ok(Some(catch_pc))
    }
}
