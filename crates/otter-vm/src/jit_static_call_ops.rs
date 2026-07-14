//! Compiled static intrinsic-call transitions.
//!
//! # Contents
//! - `ArrayBufferCall`, `SharedArrayBufferCall`, `BigIntCall`, and
//!   `DataViewCall` completion through the VM's static-call helpers.
//!
//! # Invariants
//! - These opcodes carry a distinct operand layout — `dst`, a method-id
//!   constant, an argument count, then the argument registers — so the
//!   transition rebuilds exactly that shape before calling the same helper the
//!   interpreter dispatches. No static-call semantics are duplicated in JIT
//!   code.
//! - The lowering guarantees the argument count fits the four packed lanes; a
//!   larger list lowers to an exact pre-effect side exit and serves loop OSR.
//!
//! # See also
//! - [`crate::Interpreter::run_static_call_operands`]
//! - [`crate::Interpreter::run_array_buffer_static_call_operands`]

use otter_bytecode::{Op, Operand};
use smallvec::SmallVec;

use crate::{ExecutionContext, Interpreter, VmError, holt_stack::HoltStack};

impl Interpreter {
    /// Complete one static intrinsic-call opcode for a published compiled frame.
    /// `packed_head` is `dst | argc<<16`; `method` is the method-id constant;
    /// `packed_args` holds the argument registers, one per 16-bit lane.
    pub fn jit_runtime_static_call_op(
        &mut self,
        context: &ExecutionContext,
        stack: &mut HoltStack,
        frame_index: usize,
        opcode: u8,
        packed_head: u64,
        method: u64,
        packed_args: u64,
    ) -> Result<(), VmError> {
        self.record_jit_runtime_stub_class(crate::native_abi::RuntimeStubClass::Reentrant);
        if frame_index + 1 != stack.len() {
            return Err(VmError::InvalidOperand);
        }
        let saved_pc = stack[frame_index].pc;
        let dst = (packed_head & 0xffff) as u16;
        let argc = ((packed_head >> 16) & 0xffff) as usize;
        // Rebuild the interpreter operand layout: dst, method-const, argc-const,
        // then the argument registers.
        let mut ops: SmallVec<[Operand; 8]> = SmallVec::new();
        ops.push(Operand::Register(dst));
        ops.push(Operand::ConstIndex(method as u32));
        ops.push(Operand::ConstIndex(argc as u32));
        for i in 0..argc {
            ops.push(Operand::Register(
                ((packed_args >> (16 * i)) & 0xffff) as u16,
            ));
        }
        match opcode {
            value if value == Op::ArrayBufferCall as u8 => {
                self.run_array_buffer_static_call_operands(stack, &ops[..])?;
            }
            value if value == Op::SharedArrayBufferCall as u8 => {
                self.run_shared_array_buffer_static_call_operands(stack, &ops[..])?;
            }
            value if value == Op::BigIntCall as u8 => {
                self.run_static_call_operands(
                    Op::BigIntCall,
                    context,
                    &mut stack[frame_index],
                    &ops[..],
                )?;
            }
            value if value == Op::DataViewCall as u8 => {
                self.run_static_call_operands(
                    Op::DataViewCall,
                    context,
                    &mut stack[frame_index],
                    &ops[..],
                )?;
            }
            _ => return Err(VmError::InvalidOperand),
        }
        stack[frame_index].pc = saved_pc;
        Ok(())
    }
}
