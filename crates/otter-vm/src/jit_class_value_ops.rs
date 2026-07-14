//! Compiled class creation and dynamic-value transitions.
//!
//! # Contents
//! - Shared register helpers for template objects, private names, and the
//!   direct-eval identity probe.
//! - Packed compiled dispatch for class creation, dynamic functions, eval,
//!   and full `ToNumber` coercion.
//!
//! # Invariants
//! - Interpreter and JIT dispatch call the same VM helpers; no JavaScript
//!   semantics are implemented in machine-code glue.
//! - Constant and template-site indices resolve through the compiled frame's
//!   owning chunk, never the ambient caller chunk.
//! - Reentrant helpers publish no frame borrow or raw source register across
//!   synchronous JavaScript completion.
//!
//! # See also
//! - [`crate::Interpreter::run_make_class_regs`]
//! - [`crate::Interpreter::run_eval_operands`]
//! - [`crate::coerce::to_number_or_throw`]

use otter_bytecode::{Op, Operand};
use smallvec::SmallVec;

use crate::{
    ExecutionContext, Interpreter, JsString, Value, VmError, holt_stack::HoltStack, read_register,
    write_register,
};

impl Interpreter {
    /// §13.2.8.4 GetTemplateObject with a frame-owning chunk lookup.
    pub(crate) fn run_get_template_object_reg(
        &mut self,
        context: &ExecutionContext,
        stack: &mut HoltStack,
        frame_index: usize,
        dst: u16,
        site_idx: u32,
    ) -> Result<(), VmError> {
        let function_id = stack[frame_index].function_id;
        let chunk_base = context
            .function_base_for_function(function_id)
            .ok_or(VmError::InvalidOperand)?;
        let key = (chunk_base, site_idx);
        let value = match self.template_objects.get(&key) {
            Some(value) => *value,
            None => {
                let built = self.build_template_object(context, stack, function_id, site_idx)?;
                self.template_objects.insert(key, built);
                built
            }
        };
        let frame = &mut stack[frame_index];
        write_register(frame, dst, value)?;
        frame.advance_pc()?;
        Ok(())
    }

    /// §6.2.12 NewPrivateName using the frame's owning constant pool.
    pub(crate) fn run_new_private_name_reg(
        &mut self,
        context: &ExecutionContext,
        stack: &mut HoltStack,
        frame_index: usize,
        dst: u16,
        desc_idx: u32,
    ) -> Result<(), VmError> {
        let function_id = stack[frame_index].function_id;
        let description = context
            .string_constant_str_for_function(function_id, desc_idx)
            .ok_or(VmError::InvalidOperand)?
            .to_string();
        let description = JsString::from_str(&description, &mut self.gc_heap)?;
        let symbol = crate::symbol::JsSymbol::new_private(&mut self.gc_heap, Some(description))?;
        let frame = &mut stack[frame_index];
        write_register(frame, dst, Value::symbol(symbol))?;
        frame.advance_pc()?;
        Ok(())
    }

    /// §13.3.6.2 direct-eval identity probe (`SameValue(func, %eval%)`).
    pub(crate) fn run_is_eval_intrinsic_reg(
        &mut self,
        stack: &mut HoltStack,
        frame_index: usize,
        dst: u16,
        src: u16,
    ) -> Result<(), VmError> {
        let value = *read_register(&stack[frame_index], src)?;
        let is_eval = value.as_native_function().is_some_and(|native| {
            native.is_static_fn(&self.gc_heap, crate::intrinsics::number::global_eval)
        });
        let frame = &mut stack[frame_index];
        write_register(frame, dst, Value::boolean(is_eval))?;
        frame.advance_pc()?;
        Ok(())
    }

    /// Complete one class/value-family opcode for a published compiled frame.
    pub fn jit_runtime_class_value_op(
        &mut self,
        context: &ExecutionContext,
        stack: &mut HoltStack,
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
        match opcode {
            value if value == Op::MakeClass as u8 => {
                self.run_make_class_regs(
                    stack,
                    frame_index,
                    lane(arg0, 0),
                    lane(arg0, 1),
                    lane(arg0, 2),
                    lane(arg0, 3),
                    Some(arg1 as u16),
                )?;
            }
            value if value == Op::NewFunction as u8 => {
                let argc = lane(arg0, 1) as usize;
                let mut operands: SmallVec<[Operand; 6]> = SmallVec::with_capacity(argc + 2);
                operands.push(Operand::Register(lane(arg0, 0)));
                operands.push(Operand::ConstIndex(argc as u32));
                for index in 0..argc {
                    operands.push(Operand::Register(lane(arg1, index)));
                }
                self.run_new_function_operands(context, stack, operands.as_slice())?;
            }
            value if value == Op::NewPrivateName as u8 => {
                self.run_new_private_name_reg(
                    context,
                    stack,
                    frame_index,
                    arg0 as u16,
                    arg1 as u32,
                )?;
            }
            value if value == Op::GetTemplateObject as u8 => {
                self.run_get_template_object_reg(
                    context,
                    stack,
                    frame_index,
                    arg0 as u16,
                    arg1 as u32,
                )?;
            }
            value if value == Op::Eval as u8 => {
                let operands = [
                    Operand::Register(lane(arg0, 0)),
                    Operand::Register(lane(arg0, 1)),
                    Operand::Imm32(arg1 as u32 as i32),
                ];
                self.run_eval_operands(context, stack, operands.as_slice())?;
            }
            value if value == Op::IsEvalIntrinsic as u8 => {
                self.run_is_eval_intrinsic_reg(stack, frame_index, lane(arg0, 0), lane(arg0, 1))?;
            }
            value if value == Op::ToNumber as u8 => {
                self.run_to_number_regs(context, stack, frame_index, lane(arg0, 0), lane(arg0, 1))?;
            }
            _ => return Err(VmError::InvalidOperand),
        }
        stack[frame_index].pc = saved_pc;
        let _ = arg2;
        Ok(())
    }
}
