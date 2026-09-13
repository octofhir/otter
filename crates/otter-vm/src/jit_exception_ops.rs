//! Compiled exception-region transitions.
//!
//! # Contents
//! - Frame-local try-handler installation and removal.
//! - Throw/finally resumption through the interpreter's canonical unwind code.
//! - Pure JavaScript-exception routing through a live compiled frame.
//! - Abrupt jump/return completion without popping a live compiled frame.
//! - TDZ `ReferenceError` materialization through the same throwable builder as
//!   interpreter dispatch.
//!
//! # Invariants
//! - A transition that mutates cold-frame state never asks the interpreter to
//!   replay the originating opcode.
//! - The machine activation owns the live ActivationStack frame until compiled code
//!   returns; helpers may select a continuation or return value, but never pop
//!   that frame underneath native code.
//! - Thrown values remain rooted in the published register/cold-frame graph,
//!   and reentry uses the existing ActivationStack/VmThread activation ABI.
//! - An unhandled compiled throw is returned as a pure [`Value`]; only
//!   diagnostic frame provenance remains on the VM until a local catch
//!   explicitly acknowledges consumption.
//!
//! # See also
//! - [`crate::Interpreter::unwind_throw`]
//! - [`crate::Interpreter::advance_abrupt_frame`]

use otter_bytecode::Op;

use crate::{
    ExecutionContext, Frame, Interpreter, TryHandler, Value, VmError,
    activation_stack::ActivationStack,
    cold_frame::{AbruptFrameOutcome, AbruptKind, ParkedFinally},
    snapshot_frames,
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
    /// Propagate one pure JavaScript exception value.
    Throw(Value),
}

impl Interpreter {
    /// Route one already-materialized JavaScript exception through a
    /// materialized compiled frame.
    ///
    /// `Some(pc)` means this frame's catch/finally handler consumed the value.
    /// `None` means the unchanged pure exception must propagate in the caller's
    /// result payload. No pending-throw side channel is populated.
    pub fn jit_route_throw(
        &mut self,
        context: &ExecutionContext,
        stack: &mut ActivationStack,
        frame_index: usize,
        mut value: Value,
    ) -> Result<Option<u32>, VmError> {
        let mut value_root = otter_gc::RootScope::new(&mut self.gc_heap);
        // SAFETY: `value` precedes the scope and remains stationary until
        // observable IteratorClose/unwind work completes.
        unsafe {
            crate::rooting::RootScopeExt::add_value(&mut value_root, &mut value);
        }
        match self.jit_throw_from_compiled(context, stack, frame_index, value) {
            Ok(JitExceptionOutcome::Resume(pc)) => Ok(Some(pc)),
            Ok(JitExceptionOutcome::Throw(propagated)) => {
                debug_assert_eq!(propagated, value);
                Ok(None)
            }
            Ok(JitExceptionOutcome::Continue | JitExceptionOutcome::Return(_)) => {
                Err(VmError::InvalidOperand)
            }
            Err(err) => Err(err),
        }
    }

    /// Acknowledge that a Machine local catch landing absorbed a pure throw.
    ///
    /// This is deliberately separate from throw extraction and propagation:
    /// nested getter/Proxy/callee frame provenance must survive every escaping
    /// compiled frame, but must not contaminate a later throw from the catch
    /// body itself.
    pub fn jit_acknowledge_caught_throw(&mut self) {
        self.pending_uncaught_frames = None;
        self.pending_uncaught_throw = None;
        let _ = self.take_error_detail();
    }

    /// Complete one structured-exception opcode for a published compiled frame.
    ///
    /// Arguments are opcode-specific scalar operands already validated by JIT
    /// lowering. Any successful mutation is reported as a committed outcome;
    /// the exception-transition boundary returns a pure JavaScript exception
    /// value or a structural fatal status.
    #[allow(clippy::too_many_arguments)]
    pub fn jit_runtime_exception_op(
        &mut self,
        context: &ExecutionContext,
        stack: &mut ActivationStack,
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
                self.materialized_enter_try_handler(
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
                self.materialized_leave_try(&mut stack[frame_index])?;
                stack[frame_index].pc = saved_pc;
                Ok(JitExceptionOutcome::Continue)
            }
            value if value == Op::PopParkedFinally as u8 => {
                self.materialized_pop_parked_finally(&mut stack[frame_index], arg0 as usize)?;
                stack[frame_index].pc = saved_pc;
                Ok(JitExceptionOutcome::Continue)
            }
            value if value == Op::JumpViaFinally as u8 => self.jit_advance_abrupt(
                &mut stack[frame_index],
                AbruptKind::Jump(arg0 as u32),
                arg1 as u32,
            ),
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
        stack: &mut ActivationStack,
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
            self.pending_uncaught_frames = None;
            return Ok(JitExceptionOutcome::Resume(stack[frame_index].pc));
        }

        Ok(JitExceptionOutcome::Throw(value))
    }
}
