//! Committed forwarding from explicit boxed call operands.
//!
//! # Contents
//! - Live argument selection for materialized and stack-owned activations.
//! - One rooted full-completion call after the apply lookup has committed.
//!
//! # Invariants
//! - Register aliases come from explicit current values, never stale entry slots.
//! - Input anchors survive arguments-object allocation and observable getters.
//! - A separate leaf admission handles pre-effect caller materialization exits;
//!   this boundary returns only a completed value or an exception/error.
//! - Existing arguments objects retain canonical array-like semantics.
//!
//! # See also
//! - [`crate::forward_arguments`] — native window copying and binding metadata.
//! - [`crate::runtime_activation`] — validated engine-private operand boundary.

use crate::{ActivationStack, ActiveFrameRef, ExecutionContext, Interpreter, Value, VmError};
use otter_bytecode::ArgumentBindingStorage;
use smallvec::SmallVec;

impl Interpreter {
    pub(crate) fn jit_runtime_forward_values(
        &mut self,
        context: &ExecutionContext,
        stack: &mut ActivationStack,
        source: &ActiveFrameRef<'_>,
        materialized: Option<usize>,
        values: &[Value],
    ) -> Result<Value, VmError> {
        let function_id = source.function_id();
        let call_pc = source.pc();
        let function = context
            .exec_function(function_id)
            .ok_or(VmError::InvalidOperand)?;
        self.record_call_attempt_feedback(function, call_pc, function_id);
        self.record_jit_runtime_stub_class(crate::native_abi::RuntimeStubClass::Reentrant);
        let intrinsic = crate::method_ops::is_function_prototype_intrinsic_value(
            values[0],
            &self.gc_heap,
            crate::native_function::VmIntrinsicFunction::FunctionPrototypeApply,
        );
        let base = self.push_iteration_anchor(values[0]) - 1;
        for &value in &values[1..] {
            self.push_iteration_anchor(value);
        }
        let result = (|| {
            let (callee, receiver, arguments) = if intrinsic {
                if !self.is_callable_runtime(&self.iteration_anchor(base + 1)) {
                    return Err(VmError::NotCallable);
                }
                let existing = materialized
                    .and_then(|index| stack.get(index))
                    .and_then(|frame| self.frame_cold(frame))
                    .and_then(|cold| cold.arguments_object);
                let arguments = if let Some(object) = existing {
                    self.create_list_from_array_like(stack, context, object)?
                } else {
                    let count = self
                        .elided_forward_argument_count(stack, source, materialized)
                        .ok_or(VmError::InvalidOperand)? as usize;
                    let mut arguments = SmallVec::<[Value; 8]>::with_capacity(count);
                    for index in 0..count {
                        let value = if source.incoming_argument_count().is_some() {
                            source.incoming_argument(index)?
                        } else {
                            *materialized
                                .and_then(|index| stack.get(index))
                                .and_then(|frame| self.frame_cold(frame))
                                .and_then(|cold| cold.incoming_args.get(index))
                                .ok_or(VmError::InvalidOperand)?
                        };
                        arguments.push(value);
                    }
                    let mut register_word = base + 3;
                    for (index, storage) in function.forwarded_argument_bindings() {
                        let value = match storage {
                            ArgumentBindingStorage::Register { .. } => {
                                let value = self.iteration_anchor(register_word);
                                register_word += 1;
                                value
                            }
                            ArgumentBindingStorage::Upvalue { idx } => {
                                if usize::from(index) >= count {
                                    continue;
                                }
                                crate::upvalue::read_upvalue(
                                    &self.gc_heap,
                                    source.upvalue(u32::from(idx))?,
                                )
                            }
                        };
                        if let Some(slot) = arguments.get_mut(usize::from(index)) {
                            *slot = value;
                        }
                    }
                    arguments
                };
                (
                    self.iteration_anchor(base + 1),
                    self.iteration_anchor(base + 2),
                    arguments,
                )
            } else {
                let index = materialized.ok_or(VmError::InvalidOperand)?;
                let object = self.materialize_frame_arguments_object(context, stack, index)?;
                (
                    self.iteration_anchor(base),
                    self.iteration_anchor(base + 1),
                    smallvec::smallvec![self.iteration_anchor(base + 2), object],
                )
            };
            self.jit_runtime_stats.jit_to_rust_call_transitions = self
                .jit_runtime_stats
                .jit_to_rust_call_transitions
                .saturating_add(1);
            self.record_resolved_call_feedback(function, call_pc, function_id, callee);
            self.run_rooted_call_values(stack, context, callee, receiver, arguments)
        })();
        self.pop_iteration_anchors_to(base);
        result
    }
}
