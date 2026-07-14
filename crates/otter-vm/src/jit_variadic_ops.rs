//! Compiled variadic construction transitions.
//!
//! # Contents
//! - `ArrayConstruct`, `ArrayFrom`, `ArrayOf`, and `QueueMicrotask` completion
//!   through the VM's variadic helpers.
//!
//! # Invariants
//! - No variadic semantics are duplicated in JIT code; each opcode reconstructs
//!   its operand list from the packed argument tail and calls the same
//!   operand-based helper the interpreter dispatches.
//! - The lowering guarantees the argument count fits the four packed lanes; a
//!   larger list lowers to an exact pre-effect side exit and serves loop OSR.
//!
//! # See also
//! - [`crate::Interpreter::run_array_static_operands`]
//! - [`crate::Interpreter::run_queue_microtask_operands`]

use otter_bytecode::{Op, Operand};
use smallvec::SmallVec;

use crate::{ExecutionContext, Interpreter, VmError, holt_stack::HoltStack};

impl Interpreter {
    /// Complete one variadic construction opcode for a published compiled frame.
    /// `prefix` is the destination/callee register, `count` the argument count,
    /// and `packed_args` the argument registers (one per 16-bit lane).
    pub fn jit_runtime_variadic_op(
        &mut self,
        context: &ExecutionContext,
        stack: &mut HoltStack,
        frame_index: usize,
        opcode: u8,
        prefix: u64,
        count: u64,
        packed_args: u64,
    ) -> Result<(), VmError> {
        self.record_jit_runtime_stub_class(crate::native_abi::RuntimeStubClass::Reentrant);
        if frame_index + 1 != stack.len() {
            return Err(VmError::InvalidOperand);
        }
        let saved_pc = stack[frame_index].pc;
        let argc = count as usize;
        let mut ops: SmallVec<[Operand; 8]> = SmallVec::new();
        ops.push(Operand::Register(prefix as u16));
        ops.push(Operand::ConstIndex(count as u32));
        for i in 0..argc {
            ops.push(Operand::Register(
                ((packed_args >> (16 * i)) & 0xffff) as u16,
            ));
        }
        match opcode {
            value if value == Op::ArrayConstruct as u8 => {
                self.run_array_static_operands(Op::ArrayConstruct, context, stack, &ops[..])?;
            }
            value if value == Op::ArrayFrom as u8 => {
                self.run_array_static_operands(Op::ArrayFrom, context, stack, &ops[..])?;
            }
            value if value == Op::ArrayOf as u8 => {
                self.run_array_static_operands(Op::ArrayOf, context, stack, &ops[..])?;
            }
            value if value == Op::QueueMicrotask as u8 => {
                self.run_queue_microtask_operands(context, &mut stack[frame_index], &ops[..])?;
            }
            _ => return Err(VmError::InvalidOperand),
        }
        stack[frame_index].pc = saved_pc;
        Ok(())
    }
}
