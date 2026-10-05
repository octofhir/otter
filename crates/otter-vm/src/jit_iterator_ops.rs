//! Compiled iterator-lifecycle transitions.
//!
//! # Contents
//! - Synchronous `GetIterator`/`GetAsyncIterator` acquisition, including user
//!   `[Symbol.iterator]()` methods driven through the shared reentrant path.
//! - Full `IteratorNext` completion through the VM iterator engine.
//! - Iterator close for normal and throw completions.
//!
//! # Invariants
//! - Every successful transition has committed its source opcode; the
//!   generated caller only falls through and never replays it.
//! - A throw out of `next` sets the record's `[[Done]]`, so a handler's
//!   close leaves it alone (§7.4.8).
//! - A completed terminal failure retains `Fatal`; the status-only stub never
//!   sends it through JavaScript throwable materialization again.
//! - All user callbacks run through the existing ActivationStack/VmThread reentry
//!   path and values remain rooted by the published frame.
//!
//! # See also
//! - [`crate::Interpreter::get_iterator_full`]
//! - [`crate::Interpreter::iterator_next_full`]
//! - [`crate::Interpreter::iterator_close_value_sync`]

use otter_bytecode::Op;

use crate::{
    CommittedValueError, ExecutionContext, Interpreter, VmError, activation_stack::ActivationStack,
    read_register, write_register,
};

impl Interpreter {
    /// Complete one iterator-lifecycle opcode for a published compiled frame.
    ///
    /// The operand words are decoded by the template lowering and name frame
    /// registers.  This is deliberately a single VM-owned completion path:
    /// it delegates user iterators, generators, and iterator helpers to the
    /// same full semantic helpers used by the interpreter.
    #[allow(clippy::too_many_arguments)]
    pub fn jit_runtime_iterator_op(
        &mut self,
        context: &ExecutionContext,
        stack: &mut ActivationStack,
        frame_index: usize,
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
        match opcode {
            value if value == Op::IteratorNext as u8 => {
                let value_dst = arg0 as u16;
                let done_dst = arg1 as u16;
                let iter_reg = arg2 as u16;
                let iterator = *read_register(&stack[frame_index], iter_reg)
                    .map_err(CommittedValueError::Fatal)?;
                if iterator.as_iterator().is_none() {
                    return Err(CommittedValueError::JavaScript(VmError::TypeMismatch));
                }

                let (value, done) = match iterator.as_iterator() {
                    Some(handle) => self.iterator_next_full(context, stack, &handle),
                    None => Err(CommittedValueError::JavaScript(VmError::TypeMismatch)),
                }
                .inspect_err(|_| self.iterator_mark_done(iterator))?;
                write_register(&mut stack[frame_index], value_dst, value)
                    .map_err(CommittedValueError::Fatal)?;
                write_register(
                    &mut stack[frame_index],
                    done_dst,
                    crate::Value::boolean(done),
                )
                .map_err(CommittedValueError::Fatal)?;
                stack[frame_index].pc = saved_pc;
                Ok(())
            }
            value if value == Op::IteratorClose as u8 => {
                let iterator = *read_register(&stack[frame_index], arg0 as u16)
                    .map_err(CommittedValueError::Fatal)?;
                self.iterator_close_value_sync(stack, Some(context), iterator)?;
                stack[frame_index].pc = saved_pc;
                Ok(())
            }
            value if value == Op::IteratorCloseThrow as u8 => {
                let iterator = *read_register(&stack[frame_index], arg0 as u16)
                    .map_err(CommittedValueError::Fatal)?;
                self.iterator_close_for_throw(stack, Some(context), iterator)?;
                stack[frame_index].pc = saved_pc;
                Ok(())
            }
            value if value == Op::GetIterator as u8 => {
                self.get_iterator_full(context, stack, frame_index, arg0 as u16, arg1 as u16)?;
                stack[frame_index].pc = saved_pc;
                Ok(())
            }
            value if value == Op::GetAsyncIterator as u8 => {
                self.run_get_async_iterator_regs(
                    context,
                    stack,
                    frame_index,
                    arg0 as u16,
                    arg1 as u16,
                )?;
                stack[frame_index].pc = saved_pc;
                Ok(())
            }
            _ => Err(CommittedValueError::Fatal(VmError::InvalidOperand)),
        }
    }
}
