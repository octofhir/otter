//! Interpreter-owned scalar value-query and coercion helpers.
//!
//! # Contents
//! - Extracted single-implementation register helpers for `ToObject`,
//!   `ToPropertyKey`, `IsArray`, `ArrayLength`, and `LoadLength`, shared by
//!   interpreter dispatch.
//!
//! # Invariants
//! - Compiled activations use the typed representation-neutral implementation
//!   in [`crate::RuntimeCall::scalar_op`], not these materialized-frame helpers.
//! - `ToPropertyKey` coercion (`@@toPrimitive`/`valueOf`/`toString`) reenters JS
//!   through the shared path; a committed coercion is never replayed by an exact
//!   side exit.
//!
//! # See also
//! - [`crate::Interpreter::evaluate_to_primitive`]
//! - [`crate::Interpreter::run_typeof_regs`]

use crate::{
    ExecutionContext, Frame, Interpreter, JsString, Value, VmError, abstract_ops,
    activation_stack::ActivationStack, number::NumberValue, read_register, write_register,
};

impl Interpreter {
    /// §7.1.18 ToObject — wrap a primitive in its `%X.prototype%` body; objects
    /// pass through; `null`/`undefined` throw.
    pub(crate) fn run_to_object_reg(
        &mut self,
        stack: &mut ActivationStack,
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
        stack: &mut ActivationStack,
        top_idx: usize,
        dst: u16,
        src: u16,
    ) -> Result<(), VmError> {
        let value = *read_register(&stack[top_idx], src)?;
        let primitive = self.evaluate_to_primitive(
            stack,
            context,
            &value,
            abstract_ops::ToPrimitiveHint::String,
        )?;
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
            && self.realm_intrinsics.array_prototype() == Some(obj)
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
        write_register(frame, dst, Value::number_u32(s.len()))?;
        frame.advance_pc()?;
        Ok(())
    }
}
