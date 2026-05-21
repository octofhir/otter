//! Intl opcode helpers.
//!
//! `Intl.*` construction is implemented by [`crate::intl`]. This module keeps
//! the bytecode operand glue out of the main dispatch loop.
//!
//! # Contents
//! - `NewIntl` executable operand decoding.
//! - Conversion from Intl dispatch errors into VM errors.
//!
//! # Invariants
//! - Inputs are decoded from executable operands.
//! - Helpers advance the current frame PC exactly once on success.
//!
//! # See also
//! - [`crate::intl`]
//! - [`crate::executable`]

use crate::{ExecutionContext, Frame, Interpreter, VmError, intl, read_register, write_register};

impl Interpreter {
    pub(crate) fn run_new_intl_regs(
        &mut self,
        context: &ExecutionContext,
        frame: &mut Frame,
        dst: u16,
        class_idx: u32,
        locale_reg: u16,
        options_reg: u16,
    ) -> Result<(), VmError> {
        let class = context
            .string_constant_str(class_idx)
            .ok_or(VmError::InvalidOperand)?;
        let locale = read_register(frame, locale_reg)?.clone();
        let options = read_register(frame, options_reg)?.clone();
        let value = intl::construct(class, &locale, &options, &mut self.gc_heap)
            .map_err(intl_to_vm_error)?;
        write_register(frame, dst, value)?;
        frame.pc += 1;
        Ok(())
    }
}

fn intl_to_vm_error(err: intl::IntlError) -> VmError {
    match err {
        intl::IntlError::UnknownClass(name) => VmError::UnknownIntrinsic {
            name: format!("Intl.{name}"),
        },
        intl::IntlError::UnknownMember { class, method } => VmError::UnknownIntrinsic {
            name: format!("Intl.{class}.prototype.{method}"),
        },
        intl::IntlError::BadArgument { .. } => VmError::TypeMismatch,
        intl::IntlError::Engine { message, .. } => VmError::Uncaught { value: message },
        intl::IntlError::OutOfMemory {
            requested_bytes,
            heap_limit_bytes,
        } => VmError::OutOfMemory {
            requested_bytes,
            heap_limit_bytes,
        },
    }
}
