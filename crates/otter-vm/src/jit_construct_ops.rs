//! Compiled allocating-construction transitions.
//!
//! # Contents
//! - `CollectRest`, `NewError`, `NewBuiltinError`, `ArrayPush`, `NewWeakRef`,
//!   `NewFinalizationRegistry`, and `NewCollection` completion through the VM's
//!   allocating construction helpers.
//!
//! # Invariants
//! - No construction semantics are duplicated in JIT code; each opcode calls the
//!   same `run_*` helper the interpreter dispatches.
//! - The published frame is the moving-GC root for the array/error allocation
//!   each helper performs.
//!
//! # See also
//! - [`crate::Interpreter::run_new_error_regs`]
//! - [`crate::Interpreter::run_array_push_regs`]

use otter_bytecode::Op;

use crate::{ExecutionContext, Interpreter, VmError, holt_stack::HoltStack};

impl Interpreter {
    /// Complete one allocating-construction opcode for a published compiled
    /// frame. `arg0`/`arg1`/`arg2` name the destination plus source/value
    /// registers or a constant kind index per opcode.
    pub fn jit_runtime_construct_op(
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
        match opcode {
            value if value == Op::CollectRest as u8 => {
                self.materialized_collect_rest(stack, frame_index, arg0 as u16)?;
            }
            value if value == Op::NewError as u8 => {
                self.run_new_error_regs(context, stack, frame_index, arg0 as u16, arg1 as u16)?;
            }
            value if value == Op::NewBuiltinError as u8 => {
                self.run_new_builtin_error_regs(
                    context,
                    stack,
                    frame_index,
                    arg0 as u16,
                    arg1 as u32,
                    arg2 as u16,
                )?;
            }
            value if value == Op::ArrayPush as u8 => {
                self.run_array_push_regs(stack, frame_index, arg0 as u16, arg1 as u16)?;
            }
            value if value == Op::NewWeakRef as u8 => {
                self.run_new_weak_ref_regs(stack, frame_index, arg0 as u16, arg1 as u16)?;
            }
            value if value == Op::NewFinalizationRegistry as u8 => {
                self.run_new_finalization_registry_regs(
                    context,
                    stack,
                    frame_index,
                    arg0 as u16,
                    arg1 as u16,
                )?;
            }
            value if value == Op::NewCollection as u8 => {
                self.run_new_collection_regs(
                    context,
                    stack,
                    frame_index,
                    arg0 as u16,
                    arg1 as u32,
                    arg2 as u16,
                )?;
            }
            value if value == Op::PromiseFulfilledOf as u8 => {
                self.run_promise_fulfilled_of_regs(
                    context,
                    stack,
                    frame_index,
                    arg0 as u16,
                    arg1 as u16,
                )?;
            }
            _ => return Err(VmError::InvalidOperand),
        }
        stack[frame_index].pc = saved_pc;
        Ok(())
    }
}
