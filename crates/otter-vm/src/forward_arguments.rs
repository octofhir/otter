//! Live argument windows for committed apply forwarding.
//!
//! # Contents
//! - Elided-arguments eligibility/count observation without allocation or JS.
//! - Shared mapped-parameter refresh for canonical and generated completion.
//! - Immutable binding reads for native SSA operand and liveness construction.
//!
//! # Invariants
//! - A materialized arguments object is never treated as an incoming window:
//!   its getters, length and mutations require canonical observable collection.
//! - Forwarding preserves extra actuals and refreshes only mapped arguments
//!   present in that exact actual list. It never invents missing arguments.
//! - Native callee entries own window construction and missing-formal values;
//!   forwarding copies no formal register window.
//!
//! # See also
//! - [`crate::jit_spread_call_ops`] — committed runtime completion.
//! - [`crate::runtime_activation`] — compiled-frame access boundary.

use crate::{ActiveFrameRef, CodeBlock, Interpreter, Value, VmError};
use otter_bytecode::{ArgumentBindingStorage, ArgumentsObjectKind};

impl CodeBlock {
    /// Immutable mapped bindings consumed by elided argument forwarding.
    ///
    /// The argument index is not a register index: duplicate formals and captured
    /// parameters retain the compiler's exact storage mapping. Unmapped bodies
    /// consume only their original actual window and expose no binding reads.
    pub fn forwarded_argument_bindings(
        &self,
    ) -> impl Iterator<Item = (u16, ArgumentBindingStorage)> + '_ {
        self.mapped_argument_bindings
            .iter()
            .filter(|_| self.arguments_object_kind == ArgumentsObjectKind::Mapped)
            .map(|binding| (binding.argument_index, binding.storage))
    }

    /// Frame register holding the context of the context-held mapped formals,
    /// when a forwarded argument list reads any. Every such formal names the
    /// same parameter-scope context register.
    ///
    /// A committed forward packet carries this context as its trailing word:
    /// `[method, callee, receiver, register bindings…, formals context]`.
    #[must_use]
    pub fn forwarded_formals_context(&self) -> Option<u16> {
        self.forwarded_argument_bindings()
            .find_map(|(_, storage)| match storage {
                ArgumentBindingStorage::Context { reg, .. } => Some(reg),
                ArgumentBindingStorage::Register { .. } => None,
            })
    }
}

impl Interpreter {
    pub(crate) fn elided_forward_argument_count(&self, frame: &ActiveFrameRef<'_>) -> Option<u32> {
        if frame.native_arguments_object().is_some() {
            return None;
        }
        u32::try_from(frame.incoming_argument_count()).ok()
    }

    fn live_argument_binding(
        &self,
        frame: &ActiveFrameRef<'_>,
        storage: ArgumentBindingStorage,
    ) -> Result<Value, VmError> {
        Ok(match storage {
            ArgumentBindingStorage::Register { reg } => frame.read(reg)?,
            ArgumentBindingStorage::Context { reg, slot } => {
                let context = frame
                    .read(reg)?
                    .as_context()
                    .ok_or(VmError::InvalidOperand)?;
                crate::context::read_slot(&self.gc_heap, context, slot)
                    .ok_or(VmError::InvalidOperand)?
            }
        })
    }

    /// Refresh the mapped prefix of an elided arguments list from the same
    /// parameter storage an arguments exotic object would read. Extra actuals,
    /// strict/unmapped parameters and absent actuals keep their incoming values.
    /// No allocation or JavaScript reentry occurs while the frame is borrowed.
    pub(crate) fn refresh_mapped_argument_values(
        &self,
        function: &crate::executable::CodeBlock,
        frame: &crate::ActiveFrameRef<'_>,
        arguments: &mut [Value],
    ) -> Result<(), VmError> {
        if function.arguments_object_kind != ArgumentsObjectKind::Mapped {
            return Ok(());
        }
        for binding in &function.mapped_argument_bindings {
            let Some(value) = arguments.get_mut(binding.argument_index as usize) else {
                continue;
            };
            *value = self.live_argument_binding(frame, binding.storage)?;
        }
        Ok(())
    }
}
