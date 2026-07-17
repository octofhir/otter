//! Compiled spread, explicit-receiver, arguments, and tail-call transitions.
//!
//! # Contents
//! - The shared `CollectArguments` register helper used by interpreter and JIT
//!   dispatch.
//! - Synchronous full-completion siblings for frame-pushing call/construct
//!   helpers.
//! - Packed-operand dispatch for the template-tier reentrant stub.
//!
//! # Invariants
//! - Calls and constructions append to the current rooted activation stack;
//!   JIT code owns no parallel JS call semantics.
//! - No frame borrow survives reentrant completion; the destination is
//!   re-borrowed from the published stack afterwards.
//! - Spread validation order matches interpreter dispatch.
//!
//! # See also
//! - [`crate::Interpreter::do_call_spread`]
//! - [`crate::Interpreter::do_construct_spread`]

use otter_bytecode::{ArgumentBindingStorage, ArgumentsObjectKind, Op};
use smallvec::SmallVec;

use crate::{
    ExecutionContext, Interpreter, Value, VmError, activation_stack::ActivationStack,
    interp::helpers::is_constructor_runtime, object, read_register, write_register,
};

impl Interpreter {
    /// §10.4.4 Arguments exotic object construction shared by interpreter and
    /// compiled dispatch.
    pub(crate) fn run_collect_arguments_reg(
        &mut self,
        context: &ExecutionContext,
        stack: &mut ActivationStack,
        frame_index: usize,
        dst: u16,
    ) -> Result<(), VmError> {
        let (elements, kind, mapped_entries, callee) = {
            let function_id = stack[frame_index].function_id;
            let function = context
                .exec_function(function_id)
                .ok_or(VmError::InvalidOperand)?;
            let frame = &mut stack[frame_index];
            let elements: SmallVec<[Value; 4]> = self
                .frame_cold_mut(frame)
                .map(|cold| std::mem::take(&mut cold.incoming_args))
                .unwrap_or_default();
            let mapped_entries = if function.arguments_object_kind == ArgumentsObjectKind::Mapped {
                function
                    .mapped_argument_bindings
                    .iter()
                    .filter_map(|binding| {
                        if binding.argument_index as usize >= elements.len() {
                            return None;
                        }
                        let ArgumentBindingStorage::Upvalue { idx } = binding.storage else {
                            return None;
                        };
                        let cell = *frame.upvalues.get(idx as usize)?;
                        Some(crate::object::MappedArgumentEntry {
                            key: binding.argument_index.to_string(),
                            cell,
                        })
                    })
                    .collect()
            } else {
                Vec::new()
            };
            let callee = frame.self_value;
            (
                elements,
                function.arguments_object_kind,
                mapped_entries,
                callee,
            )
        };
        let elements_len = elements.len();
        let callee_anchor = self.push_iteration_anchor(callee) - 1;
        let anchor_base = callee_anchor;
        let elements_start = callee_anchor + 1;
        for value in elements {
            self.push_iteration_anchor(value);
        }

        let collect = |interp: &mut Self| {
            let iterator_method = crate::object::get(interp.global_this, &interp.gc_heap, "Array")
                .and_then(|value| {
                    if let Some(ctor) = value.as_object() {
                        crate::object::get(ctor, &interp.gc_heap, "prototype")
                    } else if let Some(native) = value.as_native_function() {
                        native
                            .own_property_descriptor(&mut interp.gc_heap, "prototype")
                            .ok()
                            .flatten()
                            .and_then(|descriptor| match descriptor.kind {
                                crate::object::DescriptorKind::Data { value } => Some(value),
                                _ => None,
                            })
                    } else {
                        None
                    }
                })
                .and_then(|value| value.as_object())
                .and_then(|prototype| crate::object::get(prototype, &interp.gc_heap, "values"));
            let iterator_symbol = interp
                .well_known_symbols
                .get(crate::symbol::WellKnown::Iterator);
            let iterator_anchor =
                interp.push_iteration_anchor(iterator_method.unwrap_or(Value::undefined())) - 1;
            let obj = if kind == ArgumentsObjectKind::Mapped {
                let callee = interp.iteration_anchor(callee_anchor);
                let iterator_root = interp.iteration_anchor(iterator_anchor);
                let elements: SmallVec<[Value; 4]> = (elements_start
                    ..elements_start + elements_len)
                    .map(|index| interp.iteration_anchor(index))
                    .collect();
                let iterator_descriptor = iterator_method.map(|_| (iterator_symbol, iterator_root));
                let obj = interp.alloc_stack_rooted_object_with_value_roots(
                    stack,
                    &[&callee, &iterator_root],
                    &elements,
                )?;
                if let Some(proto) = interp.object_prototype_object_opt() {
                    object::set_prototype(obj, &mut interp.gc_heap, Some(proto));
                }
                crate::arguments_object::initialize_mapped(
                    obj,
                    &mut interp.gc_heap,
                    elements,
                    callee,
                    mapped_entries,
                    iterator_descriptor,
                )
            } else {
                let thrower = interp.restricted_throw_type_error()?;
                let iterator_root = interp.iteration_anchor(iterator_anchor);
                let elements: SmallVec<[Value; 4]> = (elements_start
                    ..elements_start + elements_len)
                    .map(|index| interp.iteration_anchor(index))
                    .collect();
                let iterator_descriptor = iterator_method.map(|_| (iterator_symbol, iterator_root));
                let obj = interp.alloc_stack_rooted_object_with_value_roots(
                    stack,
                    &[&thrower, &iterator_root],
                    &elements,
                )?;
                if let Some(proto) = interp.object_prototype_object_opt() {
                    object::set_prototype(obj, &mut interp.gc_heap, Some(proto));
                }
                crate::arguments_object::initialize_unmapped(
                    obj,
                    &mut interp.gc_heap,
                    elements,
                    thrower,
                    iterator_descriptor,
                )
            };
            let frame = &mut stack[frame_index];
            write_register(frame, dst, Value::object(obj))?;
            frame.advance_pc()?;
            Ok(())
        };
        let result = collect(self);
        self.pop_iteration_anchors_to(anchor_base);
        result
    }

    fn spread_arguments(
        &self,
        stack: &ActivationStack,
        frame_index: usize,
        args_reg: u16,
    ) -> Result<SmallVec<[Value; 8]>, VmError> {
        let args_array = read_register(&stack[frame_index], args_reg)?
            .as_array()
            .ok_or(VmError::TypeMismatch)?;
        Ok(crate::array::with_elements(
            args_array,
            &self.gc_heap,
            |elements| elements.iter().copied().collect(),
        ))
    }

    fn run_rooted_call_values(
        &mut self,
        stack: &mut ActivationStack,
        context: &ExecutionContext,
        callee: Value,
        this_value: Value,
        args: SmallVec<[Value; 8]>,
    ) -> Result<Value, VmError> {
        let args_len = args.len();
        let callee_anchor = self.push_iteration_anchor(callee) - 1;
        let anchor_base = callee_anchor;
        let this_anchor = self.push_iteration_anchor(this_value) - 1;
        let args_start = this_anchor + 1;
        for value in args {
            self.push_iteration_anchor(value);
        }
        let run = |interp: &mut Self, stack: &mut ActivationStack| {
            let callee = interp.iteration_anchor(callee_anchor);
            let this_value = interp.iteration_anchor(this_anchor);
            let mut rooted_args = SmallVec::with_capacity(args_len);
            for index in args_start..args_start + args_len {
                rooted_args.push(interp.iteration_anchor(index));
            }
            interp.run_callable_sync_rooted(stack, context, &callee, this_value, rooted_args)
        };
        let result = run(self, stack);
        self.pop_iteration_anchors_to(anchor_base);
        result
    }

    fn run_rooted_construct_values(
        &mut self,
        stack: &mut ActivationStack,
        context: &ExecutionContext,
        callee: Value,
        new_target: Value,
        args: SmallVec<[Value; 8]>,
    ) -> Result<Value, VmError> {
        let args_len = args.len();
        let callee_anchor = self.push_iteration_anchor(callee) - 1;
        let anchor_base = callee_anchor;
        let new_target_anchor = self.push_iteration_anchor(new_target) - 1;
        let args_start = new_target_anchor + 1;
        for value in args {
            self.push_iteration_anchor(value);
        }
        let run = |interp: &mut Self, stack: &mut ActivationStack| {
            let callee = interp.iteration_anchor(callee_anchor);
            let new_target = interp.iteration_anchor(new_target_anchor);
            let mut rooted_args = SmallVec::with_capacity(args_len);
            for index in args_start..args_start + args_len {
                rooted_args.push(interp.iteration_anchor(index));
            }
            interp.run_construct_sync_rooted(stack, context, &callee, new_target, rooted_args)
        };
        let result = run(self, stack);
        self.pop_iteration_anchors_to(anchor_base);
        result
    }

    #[allow(clippy::too_many_arguments)]
    fn run_call_spread_full_regs(
        &mut self,
        context: &ExecutionContext,
        stack: &mut ActivationStack,
        frame_index: usize,
        dst: u16,
        callee_reg: u16,
        this_reg: u16,
        args_reg: u16,
    ) -> Result<(), VmError> {
        let callee = *read_register(&stack[frame_index], callee_reg)?;
        let this_value = *read_register(&stack[frame_index], this_reg)?;
        let args = self.spread_arguments(stack, frame_index, args_reg)?;
        let result = self.run_rooted_call_values(stack, context, callee, this_value, args)?;
        let frame = &mut stack[frame_index];
        write_register(frame, dst, result)?;
        frame.advance_pc()?;
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn run_call_full_regs(
        &mut self,
        context: &ExecutionContext,
        stack: &mut ActivationStack,
        frame_index: usize,
        dst: u16,
        callee_reg: u16,
        this_reg: Option<u16>,
        arg_regs: &[u16],
    ) -> Result<(), VmError> {
        let callee = *read_register(&stack[frame_index], callee_reg)?;
        let this_value = match this_reg {
            Some(reg) => *read_register(&stack[frame_index], reg)?,
            None => Value::undefined(),
        };
        let mut args = SmallVec::with_capacity(arg_regs.len());
        for &reg in arg_regs {
            args.push(*read_register(&stack[frame_index], reg)?);
        }
        let result = self.run_rooted_call_values(stack, context, callee, this_value, args)?;
        let frame = &mut stack[frame_index];
        write_register(frame, dst, result)?;
        frame.advance_pc()?;
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn run_construct_spread_full_regs(
        &mut self,
        context: &ExecutionContext,
        stack: &mut ActivationStack,
        frame_index: usize,
        dst: u16,
        callee_reg: u16,
        args_reg: u16,
        super_construct: bool,
    ) -> Result<(), VmError> {
        let callee = *read_register(&stack[frame_index], callee_reg)?;
        if !is_constructor_runtime(&callee, context, &self.gc_heap) {
            return Err(VmError::NotCallable);
        }
        let new_target = if super_construct {
            self.frame_cold(&stack[frame_index])
                .and_then(|cold| cold.new_target)
                .unwrap_or(callee)
        } else {
            callee
        };
        let args = self.spread_arguments(stack, frame_index, args_reg)?;
        let result = self.run_rooted_construct_values(stack, context, callee, new_target, args)?;
        let frame = &mut stack[frame_index];
        write_register(frame, dst, result)?;
        frame.advance_pc()?;
        Ok(())
    }

    /// Complete one spread/call-family opcode for a published compiled frame.
    pub fn jit_runtime_spread_call_op(
        &mut self,
        context: &ExecutionContext,
        stack: &mut ActivationStack,
        frame_index: usize,
        opcode: u8,
        arg0: u64,
        arg1: u64,
        arg2: u64,
    ) -> Result<(), VmError> {
        self.record_jit_runtime_stub_class(crate::native_abi::RuntimeStubClass::Reentrant);
        if frame_index + 1 != stack.len() {
            return Err(VmError::InvalidOperand);
        }
        let saved_pc = stack[frame_index].pc;
        let lane = |packed: u64, index: usize| ((packed >> (index * 16)) & 0xffff) as u16;
        let packed_regs = |packed: u64, count: usize| {
            let mut regs = SmallVec::<[u16; 4]>::with_capacity(count);
            for index in 0..count {
                regs.push(lane(packed, index));
            }
            regs
        };
        match opcode {
            value if value == Op::CallSpread as u8 => {
                self.run_call_spread_full_regs(
                    context,
                    stack,
                    frame_index,
                    lane(arg0, 0),
                    lane(arg0, 1),
                    lane(arg0, 2),
                    lane(arg0, 3),
                )?;
            }
            value if value == Op::CallWithThis as u8 => {
                let regs = packed_regs(arg1, lane(arg0, 3) as usize);
                self.run_call_full_regs(
                    context,
                    stack,
                    frame_index,
                    lane(arg0, 0),
                    lane(arg0, 1),
                    Some(lane(arg0, 2)),
                    &regs,
                )?;
            }
            value if value == Op::CollectArguments as u8 => {
                self.run_collect_arguments_reg(context, stack, frame_index, arg0 as u16)?;
            }
            value if value == Op::NewSpread as u8 || value == Op::SuperConstructSpread as u8 => {
                self.run_construct_spread_full_regs(
                    context,
                    stack,
                    frame_index,
                    arg0 as u16,
                    arg1 as u16,
                    arg2 as u16,
                    value == Op::SuperConstructSpread as u8,
                )?;
            }
            _ => return Err(VmError::InvalidOperand),
        }
        stack[frame_index].pc = saved_pc;
        Ok(())
    }
}
