//! Compiled scalar value-query and coercion transitions.
//!
//! # Contents
//! - Extracted single-implementation register helpers for `ToObject`,
//!   `ToPropertyKey`, `IsArray`, `ArrayLength`, and `LoadLength`, shared by the
//!   interpreter dispatch and the compiled transition.
//! - The reentrant scalar transition dispatching those plus `TypeOf`,
//!   `LoadNewTarget`, and `SameValue`.
//!
//! # Invariants
//! - No scalar semantics are duplicated in JIT code; each opcode calls the same
//!   VM register helper the interpreter dispatches.
//! - `ToPropertyKey` coercion (`@@toPrimitive`/`valueOf`/`toString`) reenters JS
//!   through the shared path; a committed coercion is never replayed by an exact
//!   side exit.
//!
//! # See also
//! - [`crate::Interpreter::evaluate_to_primitive`]
//! - [`crate::Interpreter::run_typeof_regs`]

use otter_bytecode::Op;

use crate::{
    ActiveFrameMut, ExecutionContext, Frame, Interpreter, JsString, Value, VmError, abstract_ops,
    holt_stack::HoltStack, number::NumberValue, read_register, write_register,
};

impl Interpreter {
    /// §7.1.18 ToObject — wrap a primitive in its `%X.prototype%` body; objects
    /// pass through; `null`/`undefined` throw.
    pub(crate) fn run_to_object_reg(
        &mut self,
        stack: &mut HoltStack,
        top_idx: usize,
        dst: u16,
        src: u16,
    ) -> Result<(), VmError> {
        let value = *read_register(&stack[top_idx], src)?;
        if value.is_nullish() {
            return Err(VmError::TypeMismatch);
        }
        let boxed = self.box_sloppy_this_primitive_stack_rooted(stack, value, &[])?;
        let frame = &mut stack[top_idx];
        write_register(frame, dst, boxed)?;
        frame.advance_pc()?;
        Ok(())
    }

    /// §7.1.19 ToPropertyKey with full user coercion.
    pub(crate) fn run_to_property_key_reg(
        &mut self,
        context: &ExecutionContext,
        stack: &mut HoltStack,
        top_idx: usize,
        dst: u16,
        src: u16,
    ) -> Result<(), VmError> {
        let value = *read_register(&stack[top_idx], src)?;
        let primitive =
            self.evaluate_to_primitive(context, &value, abstract_ops::ToPrimitiveHint::String)?;
        let key = if primitive.as_symbol(&self.gc_heap).is_some()
            || primitive.as_string(&self.gc_heap).is_some()
        {
            primitive
        } else {
            let text = primitive.display_string(&self.gc_heap);
            Value::string(JsString::from_str(&text, &mut self.gc_heap)?)
        };
        let frame = &mut stack[top_idx];
        write_register(frame, dst, key)?;
        frame.advance_pc()?;
        Ok(())
    }

    /// §23.1.3.1 `Array.isArray`, including the Proxy-target unwrap and the
    /// realm `Array.prototype` identity.
    pub(crate) fn run_is_array_reg(
        &mut self,
        frame: &mut Frame,
        dst: u16,
        src: u16,
    ) -> Result<(), VmError> {
        let value = *read_register(frame, src)?;
        let mut result = abstract_ops::is_array(&self.gc_heap, &value)?;
        if !result
            && let Some(obj) = value.as_object()
            && self.realm_intrinsics.array_prototype == Some(obj)
        {
            result = true;
        }
        write_register(frame, dst, Value::boolean(result))?;
        frame.advance_pc()?;
        Ok(())
    }

    /// Dense array length read for the `ArrayLength` fast opcode.
    pub(crate) fn run_array_length_reg(
        &self,
        frame: &mut Frame,
        dst: u16,
        src: u16,
    ) -> Result<(), VmError> {
        let arr = read_register(frame, src)?
            .as_array()
            .ok_or(VmError::TypeMismatch)?;
        let n = NumberValue::from_f64(crate::array::len(arr, &self.gc_heap) as f64);
        write_register(frame, dst, Value::number(n))?;
        frame.advance_pc()?;
        Ok(())
    }

    /// String length read for the `LoadLength` fast opcode.
    pub(crate) fn run_load_length_reg(
        &self,
        frame: &mut Frame,
        dst: u16,
        src: u16,
    ) -> Result<(), VmError> {
        let s = read_register(frame, src)?
            .as_string(&self.gc_heap)
            .ok_or(VmError::TypeMismatch)?;
        let len = NumberValue::from_i32(s.len() as i32);
        write_register(frame, dst, Value::number(len))?;
        frame.advance_pc()?;
        Ok(())
    }

    /// Complete one scalar value-query/coercion opcode for a published compiled
    /// frame. `arg0`/`arg1`/`arg2` name the destination and source (or
    /// left/right) registers per opcode.
    pub fn jit_runtime_scalar_op(
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
        let dst = arg0 as u16;
        let src = arg1 as u16;
        match opcode {
            value if value == Op::ToObject as u8 => {
                self.run_to_object_reg(stack, frame_index, dst, src)?;
            }
            value if value == Op::ToPropertyKey as u8 => {
                self.run_to_property_key_reg(context, stack, frame_index, dst, src)?;
            }
            value if value == Op::TypeOf as u8 => {
                self.run_typeof_regs(&mut stack[frame_index], dst, src)?;
            }
            value if value == Op::LoadNewTarget as u8 => {
                let new_target = self
                    .frame_cold(&stack[frame_index])
                    .and_then(|cold| cold.new_target)
                    .unwrap_or(Value::undefined());
                let mut frame = ActiveFrameMut::materialized_with_new_target(
                    &mut stack[frame_index],
                    new_target,
                );
                self.frame_load_new_target(&mut frame, dst)?;
            }
            value if value == Op::SameValue as u8 => {
                self.run_same_value_regs(&mut stack[frame_index], dst, src, arg2 as u16)?;
            }
            value if value == Op::IsArray as u8 => {
                self.run_is_array_reg(&mut stack[frame_index], dst, src)?;
            }
            value if value == Op::ArrayLength as u8 => {
                self.run_array_length_reg(&mut stack[frame_index], dst, src)?;
            }
            value if value == Op::LoadLength as u8 => {
                self.run_load_length_reg(&mut stack[frame_index], dst, src)?;
            }
            _ => return Err(VmError::InvalidOperand),
        }
        stack[frame_index].pc = saved_pc;
        Ok(())
    }
}
