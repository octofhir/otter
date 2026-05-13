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

use crate::{ExecutionContext, Frame, Interpreter, Value, VmError, write_register};

impl Interpreter {
    pub(crate) fn run_load_global_this_reg(
        &self,
        frame: &mut Frame,
        dst: u16,
    ) -> Result<(), VmError> {
        write_register(frame, dst, Value::Object(self.global_this))?;
        frame.pc += 1;
        Ok(())
    }

    pub(crate) fn run_load_global_or_throw_reg(
        &self,
        context: &ExecutionContext,
        frame: &mut Frame,
        dst: u16,
        name_idx: u32,
    ) -> Result<(), VmError> {
        let name = context
            .string_constant_str(name_idx)
            .ok_or(VmError::InvalidOperand)?;
        if let Some(value) = crate::object::get(self.global_this, &self.gc_heap, name) {
            write_register(frame, dst, value)?;
            frame.pc += 1;
            Ok(())
        } else {
            Err(VmError::UndefinedIdentifier {
                name: name.to_string(),
            })
        }
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
            crate::object::get(self.global_this, &self.gc_heap, name).unwrap_or(Value::Undefined);
        write_register(frame, dst, value)?;
        frame.pc += 1;
        Ok(())
    }
}
