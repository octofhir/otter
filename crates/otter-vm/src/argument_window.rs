//! Borrowed argument windows for bytecode call sites.
//!
//! Call opcodes name argument registers in the operand stream. A checked
//! window reads those values into owned inputs before native dispatch, keeping
//! prepared arguments stable after the Rust bytecode entry returns.
//!
//! # Contents
//! - [`BytecodeArgumentWindow`] for register-backed call arguments.
//! - Owned actual-argument inputs for the common call trampoline.
//!
//! # Invariants
//! - Windows never outlive the caller frame they borrow.
//! - Verified execution records expose their typed register words directly;
//!   focused malformed-operand tests retain a validating decoded view.
//! - Binding preserves the same clone/move semantics as the owned call path:
//!   parameters, rest arrays, and `arguments` snapshots each receive their own
//!   `Value` handle copy when required.
//!
//! # See also
//! - [`crate::call_ops`]
//! - [`crate::native_abi::Frame`]

use smallvec::SmallVec;

use crate::{CodeBlock, CodeBlockInstruction, Frame, Value, VmError, read_register};

#[cfg(test)]
use crate::{executable::OperandView, operand_decode::register_operand};

/// Operand source shared by variadic call and construct argument windows.
///
/// The interpreter dispatch path owns a verified dense execution record, so
/// it can read typed words without rebuilding and decoding
/// [`otter_bytecode::Operand`] values.
/// Focused malformed-operand tests retain the decoded view.
#[derive(Clone, Copy)]
pub(crate) enum ArgumentOperands<'code> {
    Execution {
        function: &'code CodeBlock,
        instruction: &'code CodeBlockInstruction,
    },
    #[cfg(test)]
    Decoded(OperandView<'code>),
}

impl<'code> ArgumentOperands<'code> {
    #[inline]
    #[must_use]
    pub(crate) const fn execution(
        function: &'code CodeBlock,
        instruction: &'code CodeBlockInstruction,
    ) -> Self {
        Self::Execution {
            function,
            instruction,
        }
    }

    #[cfg(test)]
    #[inline]
    #[must_use]
    pub(crate) const fn decoded(operands: OperandView<'code>) -> Self {
        Self::Decoded(operands)
    }

    /// Verified operand words of an execution record, in schema order.
    ///
    /// `None` only for the decoded test view, which keeps the validating
    /// per-operand path.
    #[inline]
    #[must_use]
    pub(crate) fn words(self) -> Option<&'code [u32]> {
        match self {
            Self::Execution {
                function,
                instruction,
            } => Some(function.operand_words(instruction)),
            #[cfg(test)]
            Self::Decoded(_) => None,
        }
    }

    #[inline]
    pub(crate) fn register(self, index: usize) -> Result<u16, VmError> {
        match self {
            Self::Execution {
                function,
                instruction,
            } => function
                .register(instruction, index)
                .ok_or(VmError::InvalidOperand),
            #[cfg(test)]
            Self::Decoded(operands) => register_operand(operands.get(index)),
        }
    }

    #[inline]
    pub(crate) fn const_index(self, index: usize) -> Result<u32, VmError> {
        match self {
            Self::Execution {
                function,
                instruction,
            } => function
                .const_index(instruction, index)
                .ok_or(VmError::InvalidOperand),
            #[cfg(test)]
            Self::Decoded(operands) => match operands.get(index) {
                Some(otter_bytecode::Operand::ConstIndex(value)) => Ok(value),
                _ => Err(VmError::InvalidOperand),
            },
        }
    }
}

/// Borrowed view over an opcode's argument-register operands.
pub(crate) struct BytecodeArgumentWindow<'frame, 'code> {
    caller: &'frame Frame,
    operands: ArgumentOperands<'code>,
    /// Argument register words of a verified execution record, already sliced
    /// to this window. `None` for the decoded test view and for any window whose
    /// declared length runs past the operand stream; both take the validating
    /// per-operand path.
    arg_words: Option<&'code [u32]>,
    first_arg_operand: usize,
    len: usize,
}

impl<'frame, 'code> BytecodeArgumentWindow<'frame, 'code> {
    #[must_use]
    pub(crate) fn from_operands(
        caller: &'frame Frame,
        operands: ArgumentOperands<'code>,
        first_arg_operand: usize,
        len: usize,
    ) -> Self {
        let arg_words = operands.words().and_then(|words| {
            let end = first_arg_operand.checked_add(len)?;
            words.get(first_arg_operand..end)
        });
        Self {
            caller,
            operands,
            arg_words,
            first_arg_operand,
            len,
        }
    }

    fn get(&self, index: usize) -> Result<&Value, VmError> {
        if index >= self.len {
            return Err(VmError::InvalidOperand);
        }
        if let Some(words) = self.arg_words {
            return read_register(self.caller, words[index] as u16);
        }
        let operand_index = self
            .first_arg_operand
            .checked_add(index)
            .ok_or(VmError::InvalidOperand)?;
        let register = self.operands.register(operand_index)?;
        read_register(self.caller, register)
    }

    pub(crate) fn to_smallvec8(&self) -> Result<SmallVec<[Value; 8]>, VmError> {
        let mut args = SmallVec::with_capacity(self.len);
        for index in 0..self.len {
            args.push(*self.get(index)?);
        }
        Ok(args)
    }
}
