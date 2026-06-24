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
        let locale = *read_register(frame, locale_reg)?;
        let options = *read_register(frame, options_reg)?;
        let value = match intl::construct(class, &locale, &options, &mut self.gc_heap) {
            Ok(v) => v,
            Err(e) => return Err(intl_to_vm_error(self, e)),
        };
        write_register(frame, dst, value)?;
        frame.advance_pc(self.current_byte_len)?;
        Ok(())
    }
}

fn intl_to_vm_error(interp: &crate::Interpreter, err: intl::IntlError) -> VmError {
    match err {
        intl::IntlError::UnknownClass(name) => {
            interp.err_unknown_intrinsic(format!("Intl.{name}").into())
        }
        intl::IntlError::UnknownMember { class, method } => {
            interp.err_unknown_intrinsic(format!("Intl.{class}.prototype.{method}").into())
        }
        intl::IntlError::BadArgument { .. } => VmError::TypeMismatch,
        intl::IntlError::Range { message } => interp.err_range(message.into()),
        intl::IntlError::Engine { message, .. } => interp.err_uncaught((message).into()),
        intl::IntlError::OutOfMemory {
            requested_bytes,
            heap_limit_bytes,
        } => VmError::OutOfMemory {
            requested_bytes,
            heap_limit_bytes,
        },
    }
}
