//! Eval and dynamic function constructor opcode helpers.
//!
//! `eval` and `new Function(...)` recurse through the VM compiler/runtime path,
//! so their dispatch has to run before the dense in-frame match borrows the
//! current frame.
//!
//! # Contents
//! - Indirect eval execution and writeback.
//! - `Function` constructor argument collection.
//!
//! # Invariants
//! - Helpers advance the current frame PC exactly once on success.
//! - Arguments are read from executable operands.
//! - Strict-mode eval inherits the caller function strictness.
//!
//! # See also
//! - [`crate::executable`]
//! - [`crate::ExecutionContext`]

use otter_bytecode::Operand;
use smallvec::SmallVec;

use crate::{
    ExecutionContext, Frame, Interpreter, Value, VmError, operand_decode::register_operand,
    read_register, write_register,
};

impl Interpreter {
    pub(crate) fn run_eval_operands(
        &mut self,
        context: &ExecutionContext,
        stack: &mut SmallVec<[Frame; 8]>,
        operands: &[Operand],
    ) -> Result<(), VmError> {
        let dst = register_operand(operands.first())?;
        let src_reg = register_operand(operands.get(1))?;
        let top_idx = stack.len() - 1;
        let value = read_register(&stack[top_idx], src_reg)?.clone();
        let force_strict = context.function_is_strict(stack[top_idx].function_id);
        let result = self.run_eval(&value, force_strict)?;
        let frame = stack.last_mut().ok_or(VmError::InvalidOperand)?;
        write_register(frame, dst, result)?;
        frame.pc = frame.pc.checked_add(1).ok_or(VmError::InvalidOperand)?;
        Ok(())
    }

    pub(crate) fn run_new_function_operands(
        &mut self,
        context: &ExecutionContext,
        stack: &mut SmallVec<[Frame; 8]>,
        operands: &[Operand],
    ) -> Result<(), VmError> {
        let dst = register_operand(operands.first())?;
        let top_idx = stack.len() - 1;
        let args = collect_new_function_args(&stack[top_idx], operands)?;
        let result = self.build_function_constructor(context, &args)?;
        let frame = stack.last_mut().ok_or(VmError::InvalidOperand)?;
        write_register(frame, dst, result)?;
        frame.pc = frame.pc.checked_add(1).ok_or(VmError::InvalidOperand)?;
        Ok(())
    }
}

fn collect_new_function_args(
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
