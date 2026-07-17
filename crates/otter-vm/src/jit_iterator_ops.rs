//! Compiled iterator-lifecycle transitions.
//!
//! # Contents
//! - Synchronous `GetIterator`/`GetAsyncIterator` acquisition, including user
//!   `[Symbol.iterator]()` methods driven through the shared reentrant path.
//! - Full `IteratorNext` completion through the VM iterator engine.
//! - Iterator-close and closer-registry lifetime transitions.
//!
//! # Invariants
//! - Every successful transition has committed its source opcode; the
//!   generated caller only falls through and never replays it.
//! - Reentrant `next` calls temporarily disarm an already-active closer, so a
//!   throwing `next` observes IteratorStep's no-close rule; a live iterator is
//!   re-armed exactly once on success.
//! - All user callbacks run through the existing ActivationStack/VmThread reentry
//!   path and values remain rooted by the published frame.
//!
//! # See also
//! - [`crate::Interpreter::get_iterator_full`]
//! - [`crate::Interpreter::iterator_next_full`]
//! - [`crate::Interpreter::iterator_close_value_sync`]

use otter_bytecode::Op;

use crate::{
    ExecutionContext, Interpreter, VmError, activation_stack::ActivationStack, read_register,
    write_register,
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
    ) -> Result<(), VmError> {
        self.record_jit_runtime_stub_class(crate::native_abi::RuntimeStubClass::Reentrant);
        if frame_index + 1 != stack.len() {
            return Err(VmError::InvalidOperand);
        }
        let saved_pc = stack[frame_index].pc;
        match opcode {
            value if value == Op::IteratorNext as u8 => {
                let value_dst = arg0 as u16;
                let done_dst = arg1 as u16;
                let iter_reg = arg2 as u16;
                let iterator = *read_register(&stack[frame_index], iter_reg)?;
                if iterator.as_iterator().is_none() {
                    return Err(VmError::TypeMismatch);
                }

                // `IteratorNext` normally leaves an active closer alone for
                // builtin steps.  A full completion may call user code, so
                // disarm only an already-registered closer for that span;
                // otherwise a throw from `next` would incorrectly run
                // IteratorClose during unwind.
                let was_registered = self.frame_cold(&stack[frame_index]).is_some_and(|cold| {
                    cold.active_iterator_closers
                        .iter()
                        .any(|(value, _)| *value == iterator)
                });
                if was_registered {
                    self.deregister_frame_iterator_closer(&mut stack[frame_index], iterator);
                }

                let (value, done) = match iterator.as_iterator() {
                    Some(handle) => self.iterator_next_full(context, stack, &handle),
                    None => Err(VmError::TypeMismatch),
                }?;
                if was_registered && !done {
                    let iterator = *read_register(&stack[frame_index], iter_reg)?;
                    self.register_frame_iterator_closer(&mut stack[frame_index], iterator);
                }
                write_register(&mut stack[frame_index], value_dst, value)?;
                write_register(
                    &mut stack[frame_index],
                    done_dst,
                    crate::Value::boolean(done),
                )?;
                stack[frame_index].pc = saved_pc;
                Ok(())
            }
            value if value == Op::IteratorClose as u8 => {
                let iterator = *read_register(&stack[frame_index], arg0 as u16)?;
                self.deregister_frame_iterator_closer(&mut stack[frame_index], iterator);
                self.iterator_close_value_sync(stack, context, iterator)?;
                stack[frame_index].pc = saved_pc;
                Ok(())
            }
            value if value == Op::IteratorCloseStart as u8 => {
                let iterator = *read_register(&stack[frame_index], arg0 as u16)?;
                self.register_frame_iterator_closer(&mut stack[frame_index], iterator);
                stack[frame_index].pc = saved_pc;
                Ok(())
            }
            value if value == Op::IteratorCloseEnd as u8 => {
                let iterator = *read_register(&stack[frame_index], arg0 as u16)?;
                self.deregister_frame_iterator_closer(&mut stack[frame_index], iterator);
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
            _ => Err(VmError::InvalidOperand),
        }
    }
}
