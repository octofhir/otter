//! Compiled structural object transitions.
//!
//! # Contents
//! - `ForInKeys` and `CopyDataProperties` completion through the VM's structural
//!   helpers.
//!
//! # Invariants
//! - No structural semantics are duplicated in JIT code; each opcode rebuilds
//!   its register operands and calls the same operand-based helper the
//!   interpreter dispatches.
//! - `CopyDataProperties` may invoke a Proxy `ownKeys`/`getOwnPropertyDescriptor`
//!   trap through the shared reentry path; a committed copy is never replayed by
//!   an exact side exit.
//!
//! # See also
//! - [`crate::Interpreter::run_for_in_keys_operands`]
//! - [`crate::Interpreter::run_copy_data_properties_operands`]

use otter_bytecode::{Op, Operand};

use crate::{ExecutionContext, Interpreter, VmError, activation_stack::ActivationStack};

impl Interpreter {
    /// Complete one structural object opcode for a published compiled frame.
    /// `arg0`/`arg1` name the destination/target and source registers.
    pub fn jit_runtime_structural_op(
        &mut self,
        context: &ExecutionContext,
        stack: &mut ActivationStack,
        frame_index: usize,
        opcode: u8,
        arg0: u64,
        arg1: u64,
    ) -> Result<(), VmError> {
        self.record_jit_runtime_stub_class(crate::native_abi::RuntimeStubClass::Reentrant);
        if frame_index + 1 != stack.len() {
            return Err(VmError::InvalidOperand);
        }
        let saved_pc = stack[frame_index].pc;
        let ops = [
            Operand::Register(arg0 as u16),
            Operand::Register(arg1 as u16),
        ];
        match opcode {
            value if value == Op::ForInKeys as u8 => {
                self.run_for_in_keys_operands(context, stack, &ops)?;
            }
            value if value == Op::CopyDataProperties as u8 => {
                self.run_copy_data_properties_operands(context, stack, &ops)?;
            }
            _ => return Err(VmError::InvalidOperand),
        }
        stack[frame_index].pc = saved_pc;
        Ok(())
    }
}
