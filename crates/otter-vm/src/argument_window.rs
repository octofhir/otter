//! Borrowed argument windows for bytecode call sites.
//!
//! Call opcodes carry their argument registers in the instruction operand
//! stream. This module lets the VM bind ordinary bytecode callees directly
//! from the caller's register window instead of first materialising an owned
//! `SmallVec<Value>`.
//!
//! # Contents
//! - [`BytecodeArgumentWindow`] for register-backed call arguments.
//! - Binding helpers for parameter, rest, and `arguments` frame slots.
//! - Owned materialisation for fallback call paths.
//!
//! # Invariants
//! - Windows never outlive the caller frame they borrow.
//! - Operand decoding is validated before each register read.
//! - Binding preserves the same clone/move semantics as the owned call path:
//!   parameters, rest arrays, and `arguments` snapshots each receive their own
//!   `Value` handle copy when required.
//!
//! # See also
//! - [`crate::call_ops`]
//! - [`crate::frame_state::Frame`]

use otter_bytecode::Operand;
use smallvec::SmallVec;

use crate::{
    ExecutableFunction, Frame, Value, VmError, operand_decode::register_operand, read_register,
};

/// Borrowed view over an opcode's argument-register operands.
pub(crate) struct BytecodeArgumentWindow<'a> {
    caller: &'a Frame,
    operands: &'a [Operand],
    first_arg_operand: usize,
    len: usize,
}

impl<'a> BytecodeArgumentWindow<'a> {
    #[must_use]
    pub(crate) fn new(
        caller: &'a Frame,
        operands: &'a [Operand],
        first_arg_operand: usize,
        len: usize,
    ) -> Self {
        Self {
            caller,
            operands,
            first_arg_operand,
            len,
        }
    }

    fn get(&self, index: usize) -> Result<&Value, VmError> {
        if index >= self.len {
            return Err(VmError::InvalidOperand);
        }
        let operand_index = self
            .first_arg_operand
            .checked_add(index)
            .ok_or(VmError::InvalidOperand)?;
        let register = register_operand(self.operands.get(operand_index))?;
        read_register(self.caller, register)
    }

    pub(crate) fn contiguous_slice(&self) -> Result<Option<&'a [Value]>, VmError> {
        if self.len == 0 {
            return Ok(Some(&self.caller.registers[0..0]));
        }
        let first = register_operand(self.operands.get(self.first_arg_operand))? as usize;
        let end = first.checked_add(self.len).ok_or(VmError::InvalidOperand)?;
        if end > self.caller.registers.len() {
            return Err(VmError::InvalidOperand);
        }
        for index in 1..self.len {
            let operand_index = self
                .first_arg_operand
                .checked_add(index)
                .ok_or(VmError::InvalidOperand)?;
            let register = register_operand(self.operands.get(operand_index))? as usize;
            if register != first + index {
                return Ok(None);
            }
        }
        Ok(Some(&self.caller.registers[first..end]))
    }

    pub(crate) fn to_smallvec8(&self) -> Result<SmallVec<[Value; 8]>, VmError> {
        let mut args = SmallVec::with_capacity(self.len);
        for index in 0..self.len {
            args.push(self.get(index)?.clone());
        }
        Ok(args)
    }

    pub(crate) fn bind_into(
        &self,
        function: &ExecutableFunction,
        frame: &mut Frame,
    ) -> Result<(), VmError> {
        let bind_count = (function.param_count as usize).min(self.len);
        for index in 0..self.len {
            let value = self.get(index)?.clone();
            if function.needs_arguments {
                frame.incoming_args.push(value.clone());
            }
            if index < bind_count {
                let slot = frame
                    .registers
                    .get_mut(index)
                    .ok_or(VmError::InvalidOperand)?;
                *slot = value;
            } else if function.has_rest {
                frame.rest_args.push(value);
            }
        }
        Ok(())
    }
}
