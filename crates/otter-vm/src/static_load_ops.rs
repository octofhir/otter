//! Static namespace load opcode helpers.
//!
//! These helpers cover fixed-width loads from built-in namespaces where the
//! compiler has already encoded the requested property name as a string
//! constant.
//!
//! # Contents
//! - `Math.<constant>` loads.
//! - `Symbol.<static>` loads.
//! - `Temporal.<static>` loads.
//!
//! # Invariants
//! - Names are decoded once from the executable context's pre-decoded string
//!   constants.
//!
//! # See also
//! - [`crate::execution_context`]

use crate::{
    ExecutionContext, Frame, Interpreter, VmError, math, symbol_dispatch, symbol_to_vm_error,
    temporal, temporal_to_vm_error, write_register,
};

impl Interpreter {
    pub(crate) fn run_math_load_reg(
        &self,
        context: &ExecutionContext,
        frame: &mut Frame,
        dst: u16,
        name_idx: u32,
    ) -> Result<(), VmError> {
        let name = context
            .string_constant_str(name_idx)
            .ok_or(VmError::InvalidOperand)?;
        let value = math::load_constant(name).ok_or_else(|| VmError::UnknownIntrinsic {
            name: format!("Math.{name}"),
        })?;
        write_register(frame, dst, value)?;
        frame.advance_pc(1)?;
        Ok(())
    }

    pub(crate) fn run_symbol_load_reg(
        &mut self,
        context: &ExecutionContext,
        frame: &mut Frame,
        dst: u16,
        name_idx: u32,
    ) -> Result<(), VmError> {
        let name = context
            .string_constant_str(name_idx)
            .ok_or(VmError::InvalidOperand)?;
        let value = symbol_dispatch::load_static(self, name).map_err(symbol_to_vm_error)?;
        write_register(frame, dst, value)?;
        frame.advance_pc(1)?;
        Ok(())
    }

    pub(crate) fn run_temporal_load_reg(
        &self,
        context: &ExecutionContext,
        frame: &mut Frame,
        dst: u16,
        name_idx: u32,
    ) -> Result<(), VmError> {
        let name = context
            .string_constant_str(name_idx)
            .ok_or(VmError::InvalidOperand)?;
        let value = temporal::load_static(name).map_err(temporal_to_vm_error)?;
        write_register(frame, dst, value)?;
        frame.advance_pc(1)?;
        Ok(())
    }
}
