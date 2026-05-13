//! Frame-local opcode helpers.
//!
//! These opcodes only read the active frame and write registers. Keeping them
//! out of the fallback interpreter body helps `lib.rs` shrink while preserving
//! the dense executable operand path.
//!
//! # Contents
//! - `this` and `new.target` register loads.
//! - Upvalue load/store register operations.
//!
//! # Invariants
//! - Inputs are decoded from the executable instruction format before reaching
//!   these helpers.
//! - Helpers never mutate the call stack shape.
//!
//! # See also
//! - [`crate::Frame`]
//! - [`crate::executable`]

use crate::{
    Frame, Interpreter, Value, VmError, read_register, read_upvalue, store_upvalue, write_register,
};

impl Interpreter {
    pub(crate) fn run_load_this_reg(&self, frame: &mut Frame, dst: u16) -> Result<(), VmError> {
        let value = frame.this_value.clone();
        write_register(frame, dst, value)?;
        frame.pc += 1;
        Ok(())
    }

    pub(crate) fn run_load_new_target_reg(
        &self,
        frame: &mut Frame,
        dst: u16,
    ) -> Result<(), VmError> {
        let value = frame.new_target.clone().unwrap_or(Value::Undefined);
        write_register(frame, dst, value)?;
        frame.pc += 1;
        Ok(())
    }

    pub(crate) fn run_load_upvalue_reg(
        &self,
        frame: &mut Frame,
        dst: u16,
        idx: i32,
    ) -> Result<(), VmError> {
        if idx < 0 {
            return Err(VmError::InvalidOperand);
        }
        let cell = *frame
            .upvalues
            .get(idx as usize)
            .ok_or(VmError::InvalidOperand)?;
        let value = read_upvalue(&self.gc_heap, cell);
        write_register(frame, dst, value)?;
        frame.pc += 1;
        Ok(())
    }

    pub(crate) fn run_store_upvalue_reg(
        &mut self,
        frame: &mut Frame,
        src: u16,
        idx: i32,
    ) -> Result<(), VmError> {
        if idx < 0 {
            return Err(VmError::InvalidOperand);
        }
        let value = read_register(frame, src)?.clone();
        let cell = *frame
            .upvalues
            .get(idx as usize)
            .ok_or(VmError::InvalidOperand)?;
        store_upvalue(&mut self.gc_heap, cell, value);
        frame.pc += 1;
        Ok(())
    }
}
