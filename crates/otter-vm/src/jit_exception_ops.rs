//! Compiled-code exception transitions.
//!
//! # Contents
//! - [`JitExceptionOutcome`] — a raised exception either lands in the
//!   frame's own handler or propagates.
//! - Consumption acknowledgement for a pure exception a compiled frame's
//!   handler absorbed.
//! - TDZ `ReferenceError` materialization through the same throwable builder
//!   as interpreter dispatch.
//!
//! # Invariants
//! - A compiled frame publishes the PC of the instruction that raised, so its
//!   handler is found in the function's handler table exactly as the
//!   interpreter finds it.
//! - An unhandled compiled throw is returned as a pure [`Value`]; only
//!   diagnostic frame provenance remains on the VM until a local handler
//!   explicitly acknowledges consumption.
//!
//! # See also
//! - [`crate::Interpreter::unwind_throw`]
//! - [`crate::RuntimeCall::route_throw`]

use crate::{ActivationStack, ExecutionContext, Interpreter, Value, VmError};

/// Where a compiled frame continues after raising an exception.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum JitExceptionOutcome {
    /// The frame's handler at this canonical logical PC holds the value.
    Resume(u32),
    /// Propagate one pure JavaScript exception value.
    Throw(Value),
}

impl Interpreter {
    /// Acknowledge that a compiled frame's handler absorbed a pure throw.
    ///
    /// This is deliberately separate from throw extraction and propagation:
    /// nested getter/Proxy/callee frame provenance must survive every escaping
    /// compiled frame, but must not contaminate a later throw from the
    /// handler itself.
    pub fn jit_acknowledge_caught_throw(&mut self) {
        self.clear_throw_provenance();
        self.pending_uncaught_throw = None;
        let _ = self.take_error_detail();
    }

    /// Materialize the TDZ `ReferenceError` a compiled `TdzError` raises.
    pub(crate) fn jit_tdz_throwable(
        &mut self,
        context: &ExecutionContext,
        stack: &mut ActivationStack,
        local_index: u32,
    ) -> Result<Value, VmError> {
        self.record_jit_runtime_stub_class(crate::native_abi::RuntimeStubClass::Reentrant);
        let err = VmError::TemporalDeadZone { local_index };
        let value = self.vm_error_to_throwable_with_stack_roots(Some(context), stack, &err)?;
        self.record_throw_site();
        Ok(value)
    }
}
