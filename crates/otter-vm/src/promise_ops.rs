//! Promise opcode helpers.
//!
//! Promise and microtask opcode helpers.
//!
//! Fixed-width promise helpers and variadic promise/microtask call glue stay
//! out of the main dispatch loop while preserving the compact executable
//! operand path.
//!
//! # Contents
//! - Wrap a value in an already-fulfilled promise.
//! - Construct a promise with an executor.
//! - Dispatch promise static methods.
//! - Enqueue `queueMicrotask` callbacks.
//!
//! # Invariants
//! - The produced promise carries the current execution context for reaction
//!   jobs.
//! - `PromiseNew` advances the caller PC before invoking the executor.
//! - Variadic helpers read executable operands directly.
//!
//! # See also
//! - [`crate::promise_dispatch`]

use otter_bytecode::Operand;
use smallvec::SmallVec;

use crate::{
    ExecutionContext, Frame, Interpreter, Microtask, Value, VmError, microtask, native_to_vm_error,
    operand_decode::{const_operand, register_operand},
    promise_dispatch, read_register, write_register,
};

impl Interpreter {
    pub(crate) fn run_promise_fulfilled_of_regs(
        &mut self,
        context: &ExecutionContext,
        stack: &mut SmallVec<[Frame; 8]>,
        top_idx: usize,
        dst: u16,
        src: u16,
    ) -> Result<(), VmError> {
        let value = read_register(&stack[top_idx], src)?.clone();
        let promise = promise_dispatch::PromiseBuilder::with_context(context.clone())
            .fulfilled_stack_rooted(self, stack, value, &[], &[])?;
        write_register(&mut stack[top_idx], dst, Value::Promise(promise))?;
        stack[top_idx].pc += 1;
        Ok(())
    }

    pub(crate) fn run_queue_microtask_operands(
        &mut self,
        context: &ExecutionContext,
        frame: &mut Frame,
        operands: &[Operand],
    ) -> Result<(), VmError> {
        let callee_reg = register_operand(operands.first())?;
        let callee = read_register(frame, callee_reg)?.clone();
        if !self.is_callable_runtime(&callee) {
            return Err(VmError::NotCallable);
        }
        let args = collect_variadic_args(frame, operands, 1, 2)?;
        frame.pc += 1;
        self.microtasks.enqueue(Microtask {
            callee,
            this_value: Value::Undefined,
            args,
            context: Some(context.clone()),
            result_capability: None,
            kind: microtask::MicrotaskKind::Call,
        });
        Ok(())
    }

    pub(crate) fn run_promise_new_operands(
        &mut self,
        context: &ExecutionContext,
        stack: &mut SmallVec<[Frame; 8]>,
        operands: &[Operand],
    ) -> Result<(), VmError> {
        let dst = register_operand(operands.first())?;
        let executor_reg = register_operand(operands.get(1))?;
        let scratch_dst = register_operand(operands.get(2))?;
        let top_idx = stack.len() - 1;
        let executor = read_register(&stack[top_idx], executor_reg)?.clone();
        if !self.is_callable_runtime(&executor) {
            return Err(VmError::NotCallable);
        }
        let (handle, resolve, reject) =
            promise_dispatch::PromiseBuilder::with_context(context.clone())
                .construct_stack_rooted(self, stack, &[&executor], &[])?;
        write_register(&mut stack[top_idx], dst, Value::Promise(handle))?;
        stack[top_idx].pc += 1;
        let mut args: SmallVec<[Value; 8]> = SmallVec::new();
        args.push(resolve);
        args.push(reject);
        self.invoke(
            stack,
            context,
            &executor,
            Value::Undefined,
            args,
            scratch_dst,
        )
    }

    pub(crate) fn run_promise_call_operands(
        &mut self,
        context: &ExecutionContext,
        stack: &mut SmallVec<[Frame; 8]>,
        operands: &[Operand],
    ) -> Result<(), VmError> {
        let dst = register_operand(operands.first())?;
        let method_idx = const_operand(operands.get(1))?;
        let method = otter_bytecode::method_id::PromiseMethod::from_u32(method_idx)
            .ok_or(VmError::InvalidOperand)?;
        let top_idx = stack.len() - 1;
        let args = collect_variadic_args(&stack[top_idx], operands, 2, 3)?;
        stack[top_idx].pc += 1;
        let result =
            promise_dispatch::statics_call(self, Some(context.clone()), method, args.as_slice())
                .map_err(native_to_vm_error)?;
        let top_idx = stack.len() - 1;
        write_register(&mut stack[top_idx], dst, result)
    }
}

fn collect_variadic_args(
    frame: &Frame,
    operands: &[Operand],
    argc_pos: usize,
    args_start: usize,
) -> Result<SmallVec<[Value; 4]>, VmError> {
    let argc = match operands.get(argc_pos) {
        Some(&Operand::ConstIndex(n)) => n as usize,
        _ => return Err(VmError::InvalidOperand),
    };
    let mut args: SmallVec<[Value; 4]> = SmallVec::with_capacity(argc);
    for i in 0..argc {
        let r = register_operand(operands.get(args_start + i))?;
        args.push(read_register(frame, r)?.clone());
    }
    Ok(args)
}
