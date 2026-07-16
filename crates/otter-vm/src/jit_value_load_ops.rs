//! Compiled static value-load transitions.
//!
//! # Contents
//! - `MathLoad`, `SymbolLoad`, `TemporalLoad`, `LoadBigInt`, and
//!   `GetStringIndex` completion through the VM's load helpers.
//!
//! # Invariants
//! - No load semantics are duplicated in JIT code; each opcode calls the same
//!   `run_*` helper the interpreter dispatches.
//! - The published frame is the moving-GC root for any allocation the helper
//!   performs (BigInt constant, single-code-unit string).
//!
//! # See also
//! - [`crate::Interpreter::run_math_load_reg`]
//! - [`crate::Interpreter::run_get_string_index_regs`]

use otter_bytecode::Op;

use crate::{ExecutionContext, Interpreter, VmError, activation_stack::ActivationStack};

impl Interpreter {
    /// Complete one static value-load opcode for a published compiled frame.
    /// `arg0`/`arg1`/`arg2` name the destination register plus a constant name
    /// index (loads) or the receiver/index registers (`GetStringIndex`).
    pub fn jit_runtime_value_load_op(
        &mut self,
        context: &ExecutionContext,
        stack: &mut ActivationStack,
        frame_index: usize,
        opcode: u8,
        arg0: u64,
        arg1: u64,
        arg2: u64,
    ) -> Result<(), VmError> {
        self.record_jit_runtime_stub_class(crate::native_abi::RuntimeStubClass::Reentrant);
        if frame_index + 1 != stack.len() {
            return Err(VmError::InvalidOperand);
        }
        let saved_pc = stack[frame_index].pc;
        let frame = &mut stack[frame_index];
        match opcode {
            value if value == Op::MathLoad as u8 => {
                self.run_math_load_reg(context, frame, arg0 as u16, arg1 as u32)?;
            }
            value if value == Op::SymbolLoad as u8 => {
                self.run_symbol_load_reg(context, frame, arg0 as u16, arg1 as u32)?;
            }
            value if value == Op::TemporalLoad as u8 => {
                self.run_temporal_load_reg(context, frame, arg0 as u16, arg1 as u32)?;
            }
            value if value == Op::LoadBigInt as u8 => {
                self.run_load_bigint_reg(context, frame, arg0 as u16, arg1 as u32)?;
            }
            value if value == Op::GetStringIndex as u8 => {
                self.run_get_string_index_regs(frame, arg0 as u16, arg1 as u16, arg2 as u16)?;
            }
            _ => return Err(VmError::InvalidOperand),
        }
        stack[frame_index].pc = saved_pc;
        Ok(())
    }
}
