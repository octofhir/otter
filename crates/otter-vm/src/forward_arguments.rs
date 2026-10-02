//! Live argument windows for committed apply forwarding.
//!
//! # Contents
//! - Elided-arguments eligibility/count observation without allocation or JS.
//! - Copying incoming actuals and captured aliases into a private native callee.
//! - Shared mapped-parameter refresh for canonical and generated completion.
//! - Immutable binding reads for native SSA operand and liveness construction.
//!
//! # Invariants
//! - A materialized arguments object is never treated as an incoming window:
//!   its getters, length and mutations require canonical observable collection.
//! - Leaf copies read current captured bindings; generated linkage patches
//!   register aliases from current homes. Together they preserve extra actuals and
//!   never invent missing arguments. No GC or JS reentry occurs during a copy.
//! - The destination is unpublished and fully initialized. A rejected copy
//!   has no JavaScript effect; generated linkage discards that private frame.
//!
//! # See also
//! - [`crate::jit_spread_call_ops`] — committed runtime completion.
//! - [`crate::runtime_activation`] — compiled-frame access boundary.

use crate::{ActiveFrameMut, ActiveFrameRef, CodeBlock, Interpreter, Value, VmError};
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

    pub(crate) fn copy_forwarded_argument_window(
        &self,
        function: &CodeBlock,
        source: &ActiveFrameRef<'_>,
        destination: &mut ActiveFrameMut<'_>,
        parameter_count: u16,
    ) -> Result<Option<u32>, VmError> {
        let Some(count) = self.elided_forward_argument_count(source) else {
            return Ok(None);
        };
        let count = count as usize;
        let incoming = destination.incoming_argument_count();
        if usize::from(parameter_count) > destination.register_count() || incoming != count {
            return Ok(None);
        }
        let mut write = |index: usize, value: Value| -> Result<(), VmError> {
            if index < usize::from(parameter_count) {
                destination.write(index as u16, value)?;
            }
            destination.write_incoming_argument(index, value)?;
            Ok(())
        };
        for index in 0..count {
            let value = source.incoming_argument(index)?;
            write(index, value)?;
        }
        for (argument_index, storage) in function.forwarded_argument_bindings() {
            let index = usize::from(argument_index);
            if index < count
                && let ArgumentBindingStorage::Context { .. } = storage
            {
                write(index, self.live_argument_binding(source, storage)?)?;
            }
        }
        // Register mappings belong to the generated caller's current value
        // homes, which may be SSA roots rather than the entry register window.
        // Generated linkage patches those values before publishing the callee.
        Ok(Some(count as u32))
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
