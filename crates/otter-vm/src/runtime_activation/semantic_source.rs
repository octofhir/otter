//! Source identity for boxed semantics from physical and inlined activations.
//!
//! # Contents
//! - Source function/PC from the active safepoint recipe.
//! - Immutable source opcode and constant operand lookup.
//!
//! # Invariants
//! A published safepoint owns its suspended call PC and logical descendants.
//! The physical frame PC is the exact side-exit resume field. A boundary with
//! no suspended edge uses that canonical frame PC. An active Template helper
//! uses its current PC even if an earlier stage left a source-bearing record. Logical descendants name their
//! own operation without materializing or replaying a caller.
//! Registry borrows end before exclusive VM access, allocation or reentry.
//!
//! # See also
//! `bindings` and `value_ops` validate source opcodes and complete boxed semantics.

use super::RuntimeCall;
use crate::{
    ExecutionContext, Interpreter, VmError,
    native_abi::{Frame, NO_CALL_PC},
};
use otter_bytecode::Op;

pub(crate) fn frame_semantic_source(
    vm: &Interpreter,
    context: &ExecutionContext,
    frame: &Frame,
) -> Result<(u32, u32), VmError> {
    let anchor = vm.jit_frame_return_pc(frame);
    let Some(record) = vm.jit_anchored_safepoint(frame, anchor)? else {
        return Ok((frame.header.function_id, frame.header.pc));
    };
    if record.inline_frames.is_empty() {
        let pc = vm.jit_frame_source_pc(frame, anchor, Some(record));
        return Ok((frame.header.function_id, pc));
    }
    if record.call_pc == NO_CALL_PC {
        return Err(VmError::InvalidOperand);
    }
    let source = record.inline_frames.last().ok_or(VmError::InvalidOperand)?;
    let owner = context
        .for_function(source.function_id)
        .map_err(|_| VmError::InvalidOperand)?;
    let function = owner
        .exec_function(source.function_id)
        .ok_or(VmError::InvalidOperand)?;
    let pc = function
        .instruction_at_byte_pc(source.byte_pc)
        .and_then(|pc| u32::try_from(pc).ok())
        .ok_or(VmError::InvalidOperand)?;
    Ok((source.function_id, pc))
}

impl RuntimeCall<'_> {
    pub(super) fn semantic_source(&self) -> Result<(u32, u32), VmError> {
        // SAFETY: RuntimeCall retains these published records. Metadata reads
        // end before any exclusive VM borrow, allocation or JavaScript reentry.
        frame_semantic_source(unsafe { self.vm.as_ref() }, &self.context, unsafe {
            self.frame.as_ref()
        })
    }
}

impl RuntimeCall<'_> {
    pub(super) fn published_opcode(&self) -> Result<Op, VmError> {
        let (function_id, instruction_pc) = self.semantic_source()?;
        let context = self
            .context
            .for_function(function_id)
            .map_err(|_| VmError::InvalidOperand)?;
        let function = context
            .exec_function(function_id)
            .ok_or(VmError::InvalidOperand)?;
        let instruction = function
            .instr_at_index(instruction_pc as usize)
            .ok_or(VmError::InvalidOperand)?;
        if instruction.instruction_pc != instruction_pc {
            return Err(VmError::InvalidOperand);
        }
        Ok(function.op(instruction))
    }

    /// The published instruction's string-constant operand at `operand`.
    pub(super) fn published_const_index(&self, operand: u8) -> Result<u32, VmError> {
        let (function_id, instruction_pc) = self.semantic_source()?;
        let context = self
            .context
            .for_function(function_id)
            .map_err(|_| VmError::InvalidOperand)?;
        let function = context
            .exec_function(function_id)
            .ok_or(VmError::InvalidOperand)?;
        let instruction = function
            .instr_at_index(instruction_pc as usize)
            .ok_or(VmError::InvalidOperand)?;
        if instruction.instruction_pc != instruction_pc {
            return Err(VmError::InvalidOperand);
        }
        // Binding opcodes may carry more than the inline operand words (the
        // snapshot forms have five), so resolve through the owning CodeBlock,
        // which consults the overflow table.
        function
            .const_index(instruction, usize::from(operand))
            .ok_or(VmError::InvalidOperand)
    }
}
