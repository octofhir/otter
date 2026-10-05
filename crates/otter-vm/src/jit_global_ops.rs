//! Compiled global-variable access transitions.
//!
//! # Contents
//! - `LoadGlobalThis`, `LoadGlobalOrUndefined`, `StoreGlobalBinding`, and
//!   `StoreGlobalChecked` completion through the VM's global environment-record
//!   helpers.
//!
//! # Invariants
//! - Every transition delegates to the interpreter's register helper or its
//!   shared value-level kernel, so accessor globals fire identical
//!   getters/setters and both tiers observe identical global-record state.
//! - Names a sloppy direct eval may have introduced resolve through the
//!   `Lookup*` binding family (`crate::context_ops`), not through this
//!   transition.
//! - Accessor getter/setter reentry runs through the shared ActivationStack/VmThread
//!   path; a committed global effect is never replayed by an exact side exit.
//!
//! # See also
//! - [`crate::Interpreter::run_load_global_or_undefined_reg`]
//! - [`crate::Interpreter::run_store_global_binding_reg`]

use crate::native_abi::CommittedValueError;
use otter_bytecode::Op;

use crate::{
    ActiveFrameMut, ExecutionContext, Interpreter, VmError, activation_stack::ActivationStack,
};

impl Interpreter {
    /// Complete one global-access opcode for a published compiled frame.
    ///
    /// Operand words are decoded by the template lowering: `arg0`/`arg1`/`arg2`
    /// name the destination/value register, the constant name index, and the
    /// opcode-specific strictness flag or `exists` register.
    pub(crate) fn jit_runtime_global_op(
        &mut self,
        context: &ExecutionContext,
        stack: &mut ActivationStack,
        frame_index: usize,
        frame: &mut ActiveFrameMut<'_>,
        opcode: u8,
        arg0: u64,
        arg1: u64,
        arg2: u64,
    ) -> Result<(), CommittedValueError> {
        self.record_jit_runtime_stub_class(crate::native_abi::RuntimeStubClass::Reentrant);
        if frame_index + 1 != stack.len() {
            return Err(CommittedValueError::Fatal(VmError::InvalidOperand));
        }
        let saved_pc = stack[frame_index].pc;
        let saved_native_pc = frame.pc();
        match opcode {
            value if value == Op::LoadGlobalThis as u8 => {
                self.run_load_global_this_reg(&mut stack[frame_index], arg0 as u16)
                    .map_err(|error| CommittedValueError::JavaScript(error.into()))?;
            }
            value if value == Op::LoadGlobalOrUndefined as u8 => {
                self.run_load_global_or_undefined_reg(
                    context,
                    stack,
                    frame_index,
                    arg0 as u16,
                    arg1 as u32,
                )?;
            }
            value if value == Op::StoreGlobalBinding as u8 => {
                self.run_store_global_binding_reg(
                    context,
                    stack,
                    frame_index,
                    arg0 as u16,
                    arg1 as u32,
                    arg2 != 0,
                )?;
            }
            value if value == Op::StoreGlobalChecked as u8 => {
                self.run_store_global_checked_reg(
                    context,
                    stack,
                    frame_index,
                    arg0 as u16,
                    arg1 as u32,
                    arg2 as u16,
                )?;
            }
            value if value == Op::DeclareGlobalVar as u8 => {
                self.run_declare_global_var_reg(
                    context,
                    &mut stack[frame_index],
                    arg1 as u32,
                    arg2 != 0,
                )
                .map_err(|error| CommittedValueError::JavaScript(error.into()))?;
            }
            value if value == Op::DeclareGlobalLex as u8 => {
                self.run_declare_global_lex_reg(
                    context,
                    &mut stack[frame_index],
                    arg1 as u32,
                    arg2 != 0,
                )
                .map_err(|error| CommittedValueError::JavaScript(error.into()))?;
            }
            value if value == Op::ValidateGlobalDecl as u8 => {
                self.run_validate_global_decl_reg(
                    context,
                    &mut stack[frame_index],
                    arg1 as u32,
                    arg2 as u32 as i32,
                )
                .map_err(|error| CommittedValueError::JavaScript(error.into()))?;
            }
            value if value == Op::DefineGlobalVar as u8 => {
                self.run_define_global_var_reg(
                    context,
                    stack,
                    frame_index,
                    arg1 as u32,
                    arg0 as u16,
                )?;
            }
            value if value == Op::DefineGlobalFunction as u8 => {
                self.run_define_global_function_reg(
                    context,
                    &mut stack[frame_index],
                    arg1 as u32,
                    arg0 as u16,
                    arg2 != 0,
                )
                .map_err(|error| CommittedValueError::JavaScript(error.into()))?;
            }
            value if value == Op::InitGlobalLex as u8 => {
                self.run_init_global_lex_reg(
                    context,
                    &mut stack[frame_index],
                    arg0 as u16,
                    arg1 as u32,
                )
                .map_err(|error| CommittedValueError::JavaScript(error.into()))?;
            }
            value if value == Op::GlobalBindingExists as u8 => {
                self.run_global_binding_exists_reg(
                    context,
                    stack,
                    frame_index,
                    arg0 as u16,
                    arg1 as u32,
                )?;
            }
            _ => return Err(CommittedValueError::Fatal(VmError::InvalidOperand)),
        }
        stack[frame_index].pc = saved_pc;
        frame.set_pc(saved_native_pc);
        Ok(())
    }
}
