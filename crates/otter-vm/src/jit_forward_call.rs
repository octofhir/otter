//! Committed forwarding from explicit boxed call operands.
//!
//! # Contents
//! - Live argument selection for materialized and stack-owned activations.
//! - One rooted full-completion call after the apply lookup has committed.
//!
//! # Invariants
//! - Register aliases and the formals context come from explicit current
//!   packet values, never stale frame slots.
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
    /// Resolve the callee, receiver and actual list of one admitted forwarding
    /// site. The caller stages them before any further allocation.
    pub(crate) fn jit_runtime_forward_request(
        &mut self,
        context: &ExecutionContext,
        stack: &mut ActivationStack,
        source: &ActiveFrameRef<'_>,
        values: &[Value],
    ) -> Result<(Value, Value, SmallVec<[Value; 8]>), VmError> {
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
                let existing = source.native_arguments_object().map(Value::object);
                let arguments = if let Some(object) = existing {
                    self.create_list_from_array_like(stack, context, object)?
                } else {
                    let count = self
                        .elided_forward_argument_count(source)
                        .ok_or(VmError::InvalidOperand)? as usize;
                    let mut arguments = SmallVec::<[Value; 8]>::with_capacity(count);
                    for index in 0..count {
                        let value = source.incoming_argument(index)?;
                        arguments.push(value);
                    }
                    let register_words = function
                        .forwarded_argument_bindings()
                        .filter(|(_, storage)| {
                            matches!(storage, ArgumentBindingStorage::Register { .. })
                        })
                        .count();
                    let context_word = base + 3 + register_words;
                    let mut register_word = base + 3;
                    for (index, storage) in function.forwarded_argument_bindings() {
                        let value = match storage {
                            ArgumentBindingStorage::Register { .. } => {
                                let value = self.iteration_anchor(register_word);
                                register_word += 1;
                                value
                            }
                            ArgumentBindingStorage::Context { slot, .. } => {
                                if usize::from(index) >= count {
                                    continue;
                                }
                                let context = self
                                    .iteration_anchor(context_word)
                                    .as_context()
                                    .ok_or(VmError::InvalidOperand)?;
                                crate::context::read_slot(&self.gc_heap, context, slot)
                                    .ok_or(VmError::InvalidOperand)?
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
                let index = stack
                    .iter()
                    .position(|frame| {
                        frame.function_id == source.function_id()
                            && frame.registers.as_ptr() == source.register_base_ptr()
                    })
                    .ok_or(VmError::InvalidOperand)?;
                // Publish current SSA aliases before observable arguments creation.
                // Every write ends before allocation; the canonical window roots
                // the live context and register bindings through moving collection.
                let mut word = base + 3;
                for (_, storage) in function.forwarded_argument_bindings() {
                    if let ArgumentBindingStorage::Register { reg } = storage {
                        *stack[index]
                            .registers
                            .get_mut(usize::from(reg))
                            .ok_or(VmError::InvalidOperand)? = self.iteration_anchor(word);
                        word += 1;
                    }
                }
                if let Some(register) = function.forwarded_formals_context() {
                    *stack[index]
                        .registers
                        .get_mut(usize::from(register))
                        .ok_or(VmError::InvalidOperand)? = self.iteration_anchor(word);
                }
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
            self.record_resolved_call_feedback(
                function,
                call_pc,
                function_id,
                callee,
                Value::undefined(),
            );
            Ok((callee, receiver, arguments))
        })();
        self.pop_iteration_anchors_to(base);
        result
    }
}
