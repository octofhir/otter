//! Live argument windows for committed apply forwarding.
//!
//! # Contents
//! - Elided-arguments eligibility/count observation without allocation or JS.
//! - Copying a complete live argument list into an unpublished native callee.
//! - Shared mapped-parameter refresh for canonical and generated completion.
//!
//! # Invariants
//! - A materialized arguments object is never treated as an incoming window:
//!   its getters, length and mutations require canonical observable collection.
//! - Leaf copies read current mapped bindings, preserve extra actuals, and do
//!   not invent missing arguments. No GC or JS reentry occurs during a copy.
//! - The destination is unpublished and fully initialized. A rejected copy
//!   has no JavaScript effect; generated linkage discards that private frame.
//!
//! # See also
//! - [`crate::jit_spread_call_ops`] — committed runtime completion.
//! - [`crate::runtime_activation`] — compiled-frame access boundary.

use crate::{
    ActivationStack, ActiveFrameMut, ActiveFrameRef, CodeBlock, Interpreter, Value, VmError,
};
use otter_bytecode::{ArgumentBindingStorage, ArgumentsObjectKind};

impl Interpreter {
    pub(crate) fn elided_forward_argument_count(
        &self,
        stack: &ActivationStack,
        frame: &ActiveFrameRef<'_>,
        materialized: Option<usize>,
    ) -> Option<u32> {
        let cold = materialized
            .and_then(|index| stack.get(index))
            .and_then(|frame| self.frame_cold(frame));
        if cold.is_some_and(|cold| cold.arguments_object.is_some()) {
            return None;
        }
        let count = frame
            .incoming_argument_count()
            .or_else(|| cold.map(|cold| cold.incoming_args.len()))?;
        u32::try_from(count).ok()
    }

    pub(crate) fn copy_live_forwarded_arguments(
        &self,
        function: &CodeBlock,
        stack: &ActivationStack,
        source: &ActiveFrameRef<'_>,
        materialized: Option<usize>,
        destination: &mut ActiveFrameMut<'_>,
        parameter_count: u16,
    ) -> Result<bool, VmError> {
        let Some(count) = self.elided_forward_argument_count(stack, source, materialized) else {
            return Ok(false);
        };
        let count = count as usize;
        let incoming = destination.incoming_argument_count();
        if usize::from(parameter_count) > destination.register_count()
            || incoming.is_some_and(|length| length != count)
        {
            return Ok(false);
        }
        let cold = materialized
            .and_then(|index| stack.get(index))
            .and_then(|frame| self.frame_cold(frame));
        let native = source.incoming_argument_count().is_some();
        let mut write = |index: usize, value: Value| -> Result<(), VmError> {
            if index < usize::from(parameter_count) {
                destination.write(index as u16, value)?;
            }
            if incoming.is_some() {
                destination.write_incoming_argument(index, value)?;
            }
            Ok(())
        };
        let copied = if incoming.is_some() {
            count
        } else {
            count.min(usize::from(parameter_count))
        };
        for index in 0..copied {
            let value = if native {
                source.incoming_argument(index)?
            } else {
                *cold
                    .and_then(|cold| cold.incoming_args.get(index))
                    .ok_or(VmError::InvalidOperand)?
            };
            write(index, value)?;
        }
        if function.arguments_object_kind == ArgumentsObjectKind::Mapped {
            for binding in &function.mapped_argument_bindings {
                let index = binding.argument_index as usize;
                if index < count {
                    write(index, self.live_argument_binding(source, binding.storage)?)?;
                }
            }
        }
        Ok(true)
    }

    fn live_argument_binding(
        &self,
        frame: &ActiveFrameRef<'_>,
        storage: ArgumentBindingStorage,
    ) -> Result<Value, VmError> {
        Ok(match storage {
            ArgumentBindingStorage::Register { reg } => frame.read(reg)?,
            ArgumentBindingStorage::Upvalue { idx } => {
                crate::upvalue::read_upvalue(&self.gc_heap, frame.upvalue(u32::from(idx))?)
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
