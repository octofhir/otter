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
//! - PromiseCall projects its native completion once at the published source;
//!   completed terminal failures bypass the ordinary source-error materializer.
//!
//! # See also
//! - [`crate::promise_dispatch`]

use crate::activation_stack::ActivationStack;
use otter_bytecode::Operand;
use smallvec::SmallVec;

use crate::{
    CommittedValueError, ExecutionContext, Frame, Interpreter, Microtask, Value, VmError,
    microtask,
    operand_decode::{const_operand, register_operand},
    promise_dispatch, read_register, write_register,
};

impl Interpreter {
    pub(crate) fn run_promise_fulfilled_of_regs(
        &mut self,
        context: &ExecutionContext,
        stack: &mut ActivationStack,
        top_idx: usize,
        dst: u16,
        src: u16,
    ) -> Result<(), VmError> {
        let value = *read_register(&stack[top_idx], src)?;
        let promise = promise_dispatch::PromiseBuilder::with_context(Some(context.clone()))
            .fulfilled_stack_rooted(self, stack, value, &[], &[])?;
        write_register(&mut stack[top_idx], dst, Value::promise(promise))?;
        stack[top_idx].advance_pc()?;
        Ok(())
    }

    pub(crate) fn run_queue_microtask_operands(
        &mut self,
        context: &ExecutionContext,
        frame: &mut Frame,
        operands: impl crate::executable::OperandSource,
    ) -> Result<(), VmError> {
        let callee_reg = register_operand(operands.first())?;
        let callee = *read_register(frame, callee_reg)?;
        if !self.is_callable_runtime(&callee) {
            return Err(VmError::NotCallable);
        }
        let args = collect_variadic_args(frame, operands, 1, 2)?;
        let realm_id = self.reaction_realm(Some(callee), self.active_realm_id)?;
        let source = self.callable_context(Some(context), callee)?;
        frame.advance_pc()?;
        self.microtasks.enqueue(Microtask {
            callee,
            this_value: Value::undefined(),
            args,
            context: source,
            realm_id,
            result_capability: None,
            kind: microtask::MicrotaskKind::Call,
            async_context: self.async_context(),
        });
        Ok(())
    }

    pub(crate) fn run_promise_new_operands(
        &mut self,
        context: &ExecutionContext,
        stack: &mut ActivationStack,
        operands: impl crate::executable::OperandSource,
    ) -> Result<(), VmError> {
        let dst = register_operand(operands.first())?;
        let executor_reg = register_operand(operands.get(1))?;
        let scratch_dst = register_operand(operands.get(2))?;
        let top_idx = stack.len() - 1;
        let executor = *read_register(&stack[top_idx], executor_reg)?;
        if !self.is_callable_runtime(&executor) {
            return Err(VmError::NotCallable);
        }
        let (handle, resolve, reject) =
            promise_dispatch::PromiseBuilder::with_context(Some(context.clone()))
                .construct_stack_rooted(self, stack, &[&executor], &[])?;
        write_register(&mut stack[top_idx], dst, Value::promise(handle))?;
        stack[top_idx].advance_pc()?;
        let mut args: SmallVec<[Value; 8]> = SmallVec::new();
        args.push(resolve);
        args.push(reject);
        self.invoke(
            stack,
            context,
            &executor,
            Value::undefined(),
            args,
            scratch_dst,
        )
    }

    pub(crate) fn run_promise_call_operands(
        &mut self,
        context: &ExecutionContext,
        stack: &mut ActivationStack,
        operands: impl crate::executable::OperandSource,
    ) -> Result<(), CommittedValueError> {
        let dst = register_operand(operands.first()).map_err(CommittedValueError::Fatal)?;
        let method_idx = const_operand(operands.get(1)).map_err(CommittedValueError::Fatal)?;
        let method = otter_bytecode::method_id::PromiseMethod::from_u32(method_idx)
            .ok_or(CommittedValueError::Fatal(VmError::InvalidOperand))?;
        let top_idx = stack.len() - 1;
        let args = collect_variadic_args(&stack[top_idx], operands, 2, 3)
            .map_err(CommittedValueError::Fatal)?;
        // This whole operation is synchronous. Keep its physical source PC
        // published through callbacks and the one native projection; only a
        // completed value advances the source instruction.
        let result = promise_dispatch::statics_call(
            self,
            stack,
            Some(context.clone()),
            None,
            method,
            args.as_slice(),
        )
        .map_err(|error| {
            crate::error_ops::native_error_to_committed_with_stack(
                self,
                stack,
                Some(context),
                error,
            )
        })?;
        let top_idx = stack.len() - 1;
        write_register(&mut stack[top_idx], dst, result).map_err(CommittedValueError::Fatal)?;
        stack[top_idx]
            .advance_pc()
            .map_err(CommittedValueError::Fatal)
    }
}

fn collect_variadic_args(
    frame: &Frame,
    operands: impl crate::executable::OperandSource,
    argc_pos: usize,
    args_start: usize,
) -> Result<SmallVec<[Value; 4]>, VmError> {
    let argc = match operands.get(argc_pos) {
        Some(Operand::ConstIndex(n)) => n as usize,
        _ => return Err(VmError::InvalidOperand),
    };
    let mut args: SmallVec<[Value; 4]> = SmallVec::with_capacity(argc);
    for i in 0..argc {
        let r = register_operand(operands.get(args_start + i))?;
        args.push(*read_register(frame, r)?);
    }
    Ok(args)
}
