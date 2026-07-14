//! Compiled exception-region transitions.
//!
//! # Contents
//! - Frame-local try-handler installation and removal.
//! - Throw/finally resumption through the interpreter's canonical unwind code.
//! - Callee-throw delivery back into a live compiled caller.
//! - Abrupt jump/return completion without popping a live compiled frame.
//! - TDZ `ReferenceError` materialization through the same throwable builder as
//!   interpreter dispatch.
//!
//! # Invariants
//! - A transition that mutates cold-frame state never asks the interpreter to
//!   replay the originating opcode.
//! - The machine activation owns the live HoltStack frame until compiled code
//!   returns; helpers may select a continuation or return value, but never pop
//!   that frame underneath native code.
//! - Thrown values remain rooted in the published register/cold-frame graph,
//!   and reentry uses the existing HoltStack/VmThread activation ABI.
//!
//! # See also
//! - [`crate::Interpreter::unwind_throw`]
//! - [`crate::Interpreter::advance_abrupt_frame`]

use otter_bytecode::Op;

use crate::{
    ExecutionContext, Frame, Interpreter, TryHandler, Value, VmError,
    cold_frame::{AbruptFrameOutcome, AbruptKind, ParkedFinally},
    error_ops::snapshot_frames,
    holt_stack::HoltStack,
    read_register,
};

/// Result of a committed exception-region operation in compiled code.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum JitExceptionOutcome {
    /// Continue at the next emitted instruction.
    Continue,
    /// Resume the same frame at this canonical logical PC.
    Resume(u32),
    /// Return normally from the compiled frame.
    Return(Value),
}

impl Interpreter {
    /// Deliver a propagated compiled-callee throw into `frame_index` when that
    /// caller still owns an active structured-exception handler.
    ///
    /// Compiled call bridges use this before taking their shared throw
    /// epilogue. A successful unwind updates the caller's canonical frame PC;
    /// the bridge publishes that PC and bails so interpreter dispatch resumes
    /// at the selected catch/finally continuation without replaying the call.
    pub fn jit_resume_caller_throw(
        &mut self,
        context: &ExecutionContext,
        stack: &mut HoltStack,
        frame_index: usize,
    ) -> Result<Option<u32>, VmError> {
        let frame = stack.get(frame_index).ok_or(VmError::InvalidOperand)?;
        let has_handler = self
            .frame_cold(frame)
            .is_some_and(|cold| !cold.handlers.is_empty());
        if !has_handler {
            return Ok(None);
        }
        let Some(value) = self.pending_uncaught_throw.take() else {
            return Ok(None);
        };

        match self.unwind_throw(context, stack, value) {
            Ok(()) => {
                self.pending_uncaught_frames = None;
                let pc = stack.get(frame_index).ok_or(VmError::InvalidOperand)?.pc;
                Ok(Some(pc))
            }
            Err(err) => {
                self.pending_uncaught_throw = Some(value);
                Err(err)
            }
        }
    }

    /// Complete one structured-exception opcode for a published compiled frame.
    ///
    /// Arguments are opcode-specific scalar operands already validated by JIT
    /// lowering. Any successful mutation is reported as a committed outcome;
    /// errors are parked by the machine stub and surface as `STATUS_THREW`.
    #[allow(clippy::too_many_arguments)]
    pub fn jit_runtime_exception_op(
        &mut self,
        context: &ExecutionContext,
        stack: &mut HoltStack,
        frame_index: usize,
        opcode: u8,
        arg0: u64,
        arg1: u64,
        arg2: u64,
    ) -> Result<JitExceptionOutcome, VmError> {
        self.record_jit_runtime_stub_class(crate::native_abi::RuntimeStubClass::Reentrant);
        if frame_index + 1 != stack.len() {
            return Err(VmError::InvalidOperand);
        }
        let saved_pc = stack[frame_index].pc;
        match opcode {
            value if value == Op::EnterTry as u8 => {
                let decode_pc = |bits: u64| {
                    let pc = bits as u32;
                    (pc != u32::MAX).then_some(pc)
                };
                self.run_enter_try_handler(
                    &mut stack[frame_index],
                    TryHandler {
                        catch_pc: decode_pc(arg0),
                        finally_pc: decode_pc(arg1),
                        exc_register: arg2 as u16,
                    },
                )?;
                stack[frame_index].pc = saved_pc;
                Ok(JitExceptionOutcome::Continue)
            }
            value if value == Op::LeaveTry as u8 => {
                self.run_leave_try(&mut stack[frame_index])?;
                stack[frame_index].pc = saved_pc;
                Ok(JitExceptionOutcome::Continue)
            }
            value if value == Op::PopParkedFinally as u8 => {
                self.run_pop_parked_finally(&mut stack[frame_index], arg0 as usize)?;
                stack[frame_index].pc = saved_pc;
                Ok(JitExceptionOutcome::Continue)
            }
            value if value == Op::JumpViaFinally as u8 => self.jit_advance_abrupt(
                &mut stack[frame_index],
                AbruptKind::Jump(arg0 as u32),
                arg1 as u32,
            ),
            value if value == Op::Throw as u8 => {
                let value = *read_register(&stack[frame_index], arg0 as u16)?;
                self.jit_throw_from_compiled(context, stack, frame_index, value)
            }
            value if value == Op::TdzError as u8 => {
                let err = VmError::TemporalDeadZone {
                    local_index: arg0 as u32,
                };
                let value = self
                    .vm_error_to_throwable_with_stack_roots(Some(context), stack, &err)
                    .ok_or(err)?;
                self.jit_throw_from_compiled(context, stack, frame_index, value)
            }
            value if value == Op::EndFinally as u8 => {
                let parked = self
                    .frame_cold_mut(&mut stack[frame_index])
                    .and_then(|cold| cold.parked_finally.pop());
                match parked {
                    Some((ParkedFinally::Throw(value), _)) => {
                        self.jit_throw_from_compiled(context, stack, frame_index, value)
                    }
                    Some((ParkedFinally::Abrupt(completion, floor), _)) => {
                        self.jit_advance_abrupt(&mut stack[frame_index], completion, floor)
                    }
                    Some((ParkedFinally::Normal, _)) | None => Ok(JitExceptionOutcome::Continue),
                }
            }
            _ => Err(VmError::InvalidOperand),
        }
    }

    fn jit_advance_abrupt(
        &mut self,
        frame: &mut Frame,
        completion: AbruptKind,
        floor: u32,
    ) -> Result<JitExceptionOutcome, VmError> {
        match self.advance_abrupt_frame(frame, completion, floor)? {
            AbruptFrameOutcome::Resume => Ok(JitExceptionOutcome::Resume(frame.pc)),
            AbruptFrameOutcome::Return(value) => Ok(JitExceptionOutcome::Return(value)),
        }
    }

    fn jit_throw_from_compiled(
        &mut self,
        context: &ExecutionContext,
        stack: &mut HoltStack,
        frame_index: usize,
        value: Value,
    ) -> Result<JitExceptionOutcome, VmError> {
        let captured_frames = self.pending_uncaught_frames.is_none();
        if captured_frames {
            self.pending_uncaught_frames = Some(snapshot_frames(context, stack));
        }
        let has_handler = self
            .frame_cold(&stack[frame_index])
            .is_some_and(|cold| !cold.handlers.is_empty());
        if has_handler {
            self.unwind_throw(context, stack, value)?;
            if captured_frames {
                self.pending_uncaught_frames = None;
            }
            return Ok(JitExceptionOutcome::Resume(stack[frame_index].pc));
        }

        self.pending_uncaught_throw = Some(value);
        Err(self.err_uncaught(self.render_thrown(&value).into()))
    }
}
