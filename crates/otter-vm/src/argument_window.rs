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

use smallvec::SmallVec;

use crate::{
    CodeBlock, Frame, Value, VmError, executable::OperandView, operand_decode::register_operand,
    read_register,
};

/// Borrowed view over an opcode's argument-register operands.
pub(crate) struct BytecodeArgumentWindow<'frame, 'code> {
    caller: &'frame Frame,
    operands: OperandView<'code>,
    first_arg_operand: usize,
    len: usize,
}

impl<'frame, 'code> BytecodeArgumentWindow<'frame, 'code> {
    #[must_use]
    pub(crate) fn new(
        caller: &'frame Frame,
        operands: OperandView<'code>,
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

    pub(crate) fn to_smallvec8(&self) -> Result<SmallVec<[Value; 8]>, VmError> {
        let mut args = SmallVec::with_capacity(self.len);
        for index in 0..self.len {
            args.push(*self.get(index)?);
        }
        Ok(args)
    }

    /// Bind the window into the callee `frame`'s register window and
    /// return any rest / incoming-args side records the caller must
    /// install into the frame's cold slot. Splitting it this way
    /// avoids passing a `&mut Interpreter` through every argument-
    /// window call site.
    pub(crate) fn bind_into(
        &self,
        function: &CodeBlock,
        frame: &mut Frame,
    ) -> Result<BoundExtras, VmError> {
        let bind_count = (function.param_count as usize).min(self.len);
        let mut rest_args: SmallVec<[Value; 4]> = SmallVec::new();
        let mut incoming_args: SmallVec<[Value; 4]> = SmallVec::new();
        for index in 0..self.len {
            let value = *self.get(index)?;
            if function.needs_arguments {
                incoming_args.push(value);
            }
            if index < bind_count {
                let slot = frame
                    .registers
                    .get_mut(index)
                    .ok_or(VmError::InvalidOperand)?;
                *slot = value;
            } else if function.has_rest {
                rest_args.push(value);
            }
        }
        Ok(BoundExtras {
            rest_args,
            incoming_args,
        })
    }
}

/// Side records produced by [`BytecodeArgumentWindow::bind_into`] and
/// installed into the callee frame's cold slot by the caller.
pub(crate) struct BoundExtras {
    pub rest_args: SmallVec<[Value; 4]>,
    pub incoming_args: SmallVec<[Value; 4]>,
}

impl BoundExtras {
    #[must_use]
    pub(crate) fn is_empty(&self) -> bool {
        self.rest_args.is_empty() && self.incoming_args.is_empty()
    }
}
