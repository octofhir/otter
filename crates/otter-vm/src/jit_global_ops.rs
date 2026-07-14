//! Compiled global-variable access transitions.
//!
//! # Contents
//! - `LoadGlobalThis`, `LoadGlobalOrUndefined`, `StoreGlobalBinding`, and
//!   `StoreGlobalChecked` completion through the VM's global environment-record
//!   helpers.
//! - Dynamic-scope `LoadDynamic`, `StoreDynamic`, and `TypeofDynamic` name
//!   resolution through the same environment helpers.
//!
//! # Invariants
//! - Every transition delegates to the same `run_*_reg` helper the interpreter
//!   dispatches, so accessor globals fire identical getters/setters and both
//!   tiers observe identical global-record state.
//! - Accessor getter/setter reentry runs through the shared HoltStack/VmThread
//!   path; a committed global effect is never replayed by an exact side exit.
//!
//! # See also
//! - [`crate::Interpreter::run_load_global_or_undefined_reg`]
//! - [`crate::Interpreter::run_store_global_binding_reg`]

use otter_bytecode::Op;

use crate::{ExecutionContext, Interpreter, VmError, holt_stack::HoltStack};

impl Interpreter {
    /// Complete one global-access opcode for a published compiled frame.
    ///
    /// Operand words are decoded by the template lowering: `arg0`/`arg1`/`arg2`
    /// name the destination/value register, the constant name index, and the
    /// opcode-specific strictness flag or `exists` register.
    pub fn jit_runtime_global_op(
        &mut self,
        context: &ExecutionContext,
        stack: &mut HoltStack,
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
            value if value == Op::LoadGlobalThis as u8 => {
                self.run_load_global_this_reg(frame, arg0 as u16)?;
            }
            value if value == Op::LoadGlobalOrUndefined as u8 => {
                self.run_load_global_or_undefined_reg(context, frame, arg0 as u16, arg1 as u32)?;
            }
            value if value == Op::StoreGlobalBinding as u8 => {
                self.run_store_global_binding_reg(
                    context,
                    frame,
                    arg0 as u16,
                    arg1 as u32,
                    arg2 != 0,
                )?;
            }
            value if value == Op::StoreGlobalChecked as u8 => {
                self.run_store_global_checked_reg(
                    context,
                    frame,
                    arg0 as u16,
                    arg1 as u32,
                    arg2 as u16,
                )?;
            }
            value if value == Op::LoadDynamic as u8 => {
                self.run_load_dynamic_reg(context, frame, arg0 as u16, arg1 as u32)?;
            }
            value if value == Op::StoreDynamic as u8 => {
                self.run_store_dynamic_reg(context, frame, arg0 as u16, arg1 as u32)?;
            }
            value if value == Op::TypeofDynamic as u8 => {
                self.run_typeof_dynamic_reg(context, frame, arg0 as u16, arg1 as u32)?;
            }
            _ => return Err(VmError::InvalidOperand),
        }
        stack[frame_index].pc = saved_pc;
        Ok(())
    }
}
