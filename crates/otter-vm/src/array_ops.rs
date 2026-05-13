//! Array static opcode helpers.
//!
//! Array constructor/static bytecodes are variadic, so their argument registers
//! still live in the executable side-operand slice. This module keeps their
//! decode and call glue out of the main interpreter loop.
//!
//! # Contents
//! - `Array(...)` / `new Array(...)` construction.
//! - `Array.from(...)` and `Array.of(...)` static calls.
//!
//! # Invariants
//! - The current frame PC is advanced before running `Array.from` so any
//!   synchronous iterator/property callbacks observe the post-call PC.
//! - Arguments are read from executable operands, not cloned bytecode DTOs.
//!
//! # See also
//! - [`crate::array_statics`]
//! - [`crate::executable`]

use otter_bytecode::{Op, Operand};
use smallvec::SmallVec;

use crate::{
    ExecutionContext, Frame, Interpreter, Value, VmError, array_statics,
    operand_decode::register_operand, read_register, write_register,
};

impl Interpreter {
    pub(crate) fn run_array_static_operands(
        &mut self,
        op: Op,
        context: &ExecutionContext,
        stack: &mut SmallVec<[Frame; 8]>,
        operands: &[Operand],
    ) -> Result<(), VmError> {
        let dst = register_operand(operands.first())?;
        let top_idx = stack.len() - 1;
        let args = collect_array_args(&stack[top_idx], operands)?;

        let pc = stack[top_idx].pc;
        stack[top_idx].pc = pc.checked_add(1).ok_or(VmError::InvalidOperand)?;
        let result = match op {
            Op::ArrayConstruct => array_statics::construct(&args, &mut self.gc_heap)?,
            Op::ArrayFrom => self.array_from_sync(context, &args)?,
            Op::ArrayOf => array_statics::of(&args, &mut self.gc_heap)?,
            _ => return Err(VmError::InvalidOperand),
        };

        let frame = stack.last_mut().ok_or(VmError::InvalidOperand)?;
        write_register(frame, dst, result)
    }
}

fn collect_array_args(
    frame: &Frame,
    operands: &[Operand],
) -> Result<SmallVec<[Value; 4]>, VmError> {
    let argc = match operands.get(1) {
        Some(&Operand::ConstIndex(n)) => n as usize,
        _ => return Err(VmError::InvalidOperand),
    };
    let mut args: SmallVec<[Value; 4]> = SmallVec::with_capacity(argc);
    for i in 0..argc {
        let r = register_operand(operands.get(2 + i))?;
        args.push(read_register(frame, r)?.clone());
    }
    Ok(args)
}
