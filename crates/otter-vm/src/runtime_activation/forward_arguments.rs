//! Argument forwarding from a published compiled caller.
//!
//! # Contents
//! - Staging from the current exact actual list and mapped parameter bindings.
//! - Committed boxed-value completion with pure exception results.
//!
//! # Invariants
//! - Committed completion roots explicit operands before allocation or reentry.
//! - Frame and window pointers remain engine-private and are validated before use.
//! - Callee frame construction and missing formals belong to its entry; forwarding
//!   never creates or initializes a private callee window.
//!
//! # See also
//! - [`crate::forward_arguments`] — source semantics and mapped bindings.

use super::RuntimeCall;
use crate::ActiveFrameRef;
use crate::native_abi::CommittedValueError;

impl RuntimeCall<'_> {
    /// Resolve one forwarded call from `[method, callee, receiver, register
    /// bindings…, formals context]` into its callee, receiver and actuals.
    /// Register bindings follow the immutable CodeBlock mapping order; the
    /// trailing context word is present exactly when a mapped formal is
    /// context-held. The caller stages the result before any allocation.
    pub fn stage_forward_values(
        &mut self,
        values: &[crate::Value],
    ) -> Result<
        (
            crate::Value,
            crate::Value,
            smallvec::SmallVec<[crate::Value; 8]>,
        ),
        CommittedValueError,
    > {
        // SAFETY: the bound activation owns the context and published frame;
        // the checked source borrows no managed slice across reentrant work.
        let context = &self.context;
        let function = context
            .exec_function(self.function_id())
            .ok_or(crate::VmError::InvalidOperand)
            .map_err(|error| CommittedValueError::Fatal(error.into()))?;
        let instruction = function
            .instr_at_index(self.pc() as usize)
            .ok_or(crate::VmError::InvalidOperand)
            .map_err(|error| CommittedValueError::Fatal(error.into()))?;
        let bindings = function
            .forwarded_argument_bindings()
            .filter(|(_, storage)| {
                matches!(
                    storage,
                    otter_bytecode::ArgumentBindingStorage::Register { .. }
                )
            })
            .count();
        let context_words = usize::from(function.forwarded_formals_context().is_some());
        if function.op(instruction) != otter_bytecode::Op::CallForwardArguments
            || values.len() != bindings + 3 + context_words
            || self.frame_index().is_err()
        {
            return Err(CommittedValueError::Fatal(crate::VmError::InvalidOperand));
        }
        let source = unsafe { ActiveFrameRef::from_ptr(self.frame.as_ptr()) }
            .map_err(|_| crate::VmError::InvalidOperand)
            .map_err(CommittedValueError::Fatal)?;
        let vm = unsafe { &mut *self.vm.as_ptr() };
        let stack = unsafe { &mut *self.stack.as_ptr() };
        vm.jit_runtime_forward_request(context, stack, &source, values)
    }
}
