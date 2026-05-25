//! Global binding load opcode helpers.
//!
//! These are fixed-width global environment reads that can dispatch directly
//! from executable operands.
//!
//! # Contents
//! - `globalThis` load.
//! - Throwing global binding lookup for ordinary identifier reads.
//! - Undefined-returning global lookup for `typeof`.
//!
//! # Invariants
//! - Global properties live on the interpreter's `global_this` object.
//! - Missing throwing lookups surface as `UndefinedIdentifier` so the normal
//!   error path can synthesize a `ReferenceError`.
//!
//! # See also
//! - [`crate::executable`]
//! - [`crate::object`]

use smallvec::SmallVec;

use crate::{
    ExecutionContext, Frame, Interpreter, Value, VmError, VmGetOutcome, VmPropertyKey, object,
    write_register,
};

impl Interpreter {
    pub(crate) fn run_load_global_this_reg(
        &self,
        frame: &mut Frame,
        dst: u16,
    ) -> Result<(), VmError> {
        write_register(frame, dst, Value::object(self.global_this))?;
        frame.advance_pc(self.current_byte_len)?;
        Ok(())
    }

    pub(crate) fn run_load_global_or_throw_reg(
        &mut self,
        context: &ExecutionContext,
        frame: &mut Frame,
        dst: u16,
        name_idx: u32,
    ) -> Result<(), VmError> {
        let name = context
            .string_constant_str(name_idx)
            .ok_or(VmError::InvalidOperand)?;
        let receiver = Value::object(self.global_this);
        let key = VmPropertyKey::String(name);
        if !self.ordinary_has_property_value(context, receiver, &key, 0)? {
            return Err(VmError::UndefinedIdentifier {
                name: name.to_string(),
            });
        }
        let value = match self.ordinary_get_value(context, receiver, receiver, &key, 0)? {
            VmGetOutcome::Value(value) => value,
            VmGetOutcome::InvokeGetter { getter } => {
                self.run_callable_sync(context, &getter, receiver, SmallVec::new())?
            }
        };
        write_register(frame, dst, value)?;
        frame.advance_pc(self.current_byte_len)?;
        Ok(())
    }

    pub(crate) fn run_load_global_or_undefined_reg(
        &self,
        context: &ExecutionContext,
        frame: &mut Frame,
        dst: u16,
        name_idx: u32,
    ) -> Result<(), VmError> {
        let name = context
            .string_constant_str(name_idx)
            .ok_or(VmError::InvalidOperand)?;
        let value =
            crate::object::get(self.global_this, &self.gc_heap, name).unwrap_or(Value::undefined());
        write_register(frame, dst, value)?;
        frame.advance_pc(self.current_byte_len)?;
        Ok(())
    }

    pub(crate) fn run_define_global_var_reg(
        &mut self,
        context: &ExecutionContext,
        frame: &mut Frame,
        name_idx: u32,
        value_reg: u16,
    ) -> Result<(), VmError> {
        let name = context
            .string_constant_str(name_idx)
            .ok_or(VmError::InvalidOperand)?;
        let value = *crate::read_register(frame, value_reg)?;
        let descriptor = object::PartialPropertyDescriptor {
            value: Some(value),
            writable: Some(true),
            enumerable: Some(true),
            configurable: Some(true),
            ..Default::default()
        };
        if !object::define_own_property_partial(
            self.global_this,
            &mut self.gc_heap,
            name,
            descriptor,
        ) {
            return Err(VmError::TypeError {
                message: format!("Cannot declare global var '{name}'"),
            });
        }
        frame.advance_pc(self.current_byte_len)?;
        Ok(())
    }
}
