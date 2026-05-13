//! Reflect opcode helpers.
//!
//! `ReflectCall` is a variadic static-dispatch bytecode. The underlying
//! semantics live in [`crate::reflect`]; this module owns executable operand
//! decoding and the PC/writeback protocol.
//!
//! # Contents
//! - `Reflect.<method>(...)` executable operand handling.
//!
//! # Invariants
//! - The current frame PC is advanced before invoking Reflect semantics because
//!   `Reflect.apply` / `Reflect.construct` may synchronously call into JS.
//! - Arguments are read from executable operands.
//!
//! # See also
//! - [`crate::reflect`]
//! - [`crate::executable`]

use otter_bytecode::Operand;
use smallvec::SmallVec;

use crate::{
    ExecutionContext, Frame, Interpreter, Value, VmError, operand_decode::const_operand,
    operand_decode::register_operand, read_register, reflect, write_register,
};

impl Interpreter {
    pub(crate) fn run_reflect_call_operands(
        &mut self,
        context: &ExecutionContext,
        stack: &mut SmallVec<[Frame; 8]>,
        operands: &[Operand],
    ) -> Result<(), VmError> {
        let dst = register_operand(operands.first())?;
        let method_idx = const_operand(operands.get(1))?;
        let method = otter_bytecode::method_id::ReflectMethod::from_u32(method_idx)
            .ok_or(VmError::InvalidOperand)?;
        let top_idx = stack.len() - 1;
        let args = collect_reflect_args(&stack[top_idx], operands)?;

        let pc = stack[top_idx].pc;
        stack[top_idx].pc = pc.checked_add(1).ok_or(VmError::InvalidOperand)?;
        let heap = self.string_heap.clone();
        let result = reflect::call(self, context, method, &args, &heap)?;
        let frame = stack.last_mut().ok_or(VmError::InvalidOperand)?;
        write_register(frame, dst, result)
    }
}

fn collect_reflect_args(
    frame: &Frame,
    operands: &[Operand],
) -> Result<SmallVec<[Value; 4]>, VmError> {
    let argc = match operands.get(2) {
        Some(&Operand::ConstIndex(n)) => n as usize,
        _ => return Err(VmError::InvalidOperand),
    };
    let mut args: SmallVec<[Value; 4]> = SmallVec::with_capacity(argc);
    for i in 0..argc {
        let r = register_operand(operands.get(3 + i))?;
        args.push(read_register(frame, r)?.clone());
    }
    Ok(args)
}
