//! Cold materialization for legacy and inlined JIT side exits.
//!
//! Native interpreter entry normally keeps the canonical [`crate::NativeFrame`] and
//! its register window intact. This module is the narrow exception: legacy
//! frameless self-call exits and nested inlined deopts that still require a
//! `HoltStack` adapter materialize one only after the side exit has fired.
//!
//! # Contents
//! - [`Interpreter::jit_deopt_materialize_self_call`] — legacy self-call bail.
//! - [`Interpreter::jit_deopt_materialize_inline_frames`] — nested inline deopt.
//!
//! # Invariants
//! - These APIs are cold side-exit operations, never normal call entry or a
//!   native-to-interpreter tier transition.
//! - Register values are attached/copied exactly once after the side exit.
//! - Every temporary materialized frame and register window is removed before
//!   returning to compiled code.
//! - New interpreter/baseline/optimizer transitions must use
//!   [`crate::ActiveFrameMut`] over the existing [`NativeFrame`] instead.
//!
//! # See also
//! - [`crate::active_frame`] — canonical tier-neutral activation access.
//! - [`crate::jit::JitDeoptFrame`] — owned inline-deopt reconstruction input.
//! - [`crate::NativeFrame`] — canonical activation that these cold adapters replace
//!   only for legacy/deopt compatibility.

use crate::{ExecutionContext, Frame, HoltStack, Interpreter, Value, VmError, jit};

impl Interpreter {
    /// Materialize and finish a legacy frameless self-call after a compiled
    /// side exit.
    ///
    /// This is not the interpreter tier-entry path. It exists only for the old
    /// inline self-call sequence whose callee window has no canonical native
    /// frame to resume. The live top register window is attached to a cold
    /// `HoltStack` adapter, dispatched to completion, then removed.
    pub fn jit_deopt_materialize_self_call(
        &mut self,
        context: &ExecutionContext,
        stack: &mut HoltStack,
        caller_frame_index: usize,
        bail_pc: u32,
        register_count: usize,
    ) -> Result<Value, VmError> {
        let window = self.register_stack.top_window(register_count)?;
        let caller = stack
            .get(caller_frame_index)
            .ok_or(VmError::InvalidOperand)?;
        let function_id = caller.function_id;
        let upvalues = caller.upvalues.clone();
        let self_value = caller.self_value;

        self.note_jit_entry_bail(function_id);
        let function = context
            .exec_function(function_id)
            .ok_or(VmError::InvalidOperand)?;
        let mut frame = Frame::with_exec_return_upvalues_and_this(
            function,
            None,
            upvalues,
            Value::undefined(),
            window,
        );
        frame.self_value = self_value;
        frame.pc = bail_pc;

        let initial_stack_len = stack.len();
        stack.push(frame);
        let result = self.dispatch_loop(context, stack);
        while stack.len() > initial_stack_len {
            if let Some(mut frame) = stack.pop() {
                self.frame_release_cold(&mut frame);
                self.reclaim_registers(&mut frame);
            }
        }
        result
    }

    /// Materialize a nested inline deopt chain and run it to completion.
    ///
    /// `frames` is ordered outermost first. The outermost completion returns to
    /// compiled code; every younger adapter returns through its recorded parent
    /// destination. This owned reconstruction is reserved for inlined
    /// optimized exits that cannot resume one canonical native activation.
    pub fn jit_deopt_materialize_inline_frames(
        &mut self,
        context: &ExecutionContext,
        stack: &mut HoltStack,
        frames: &[jit::JitDeoptFrame],
    ) -> Result<Value, VmError> {
        let _window_rollback = self.register_window_rollback();
        let initial_stack_len = stack.len();
        let mut materialized: smallvec::SmallVec<[Frame; 4]> = smallvec::SmallVec::new();

        for (index, deopt) in frames.iter().enumerate() {
            if std::env::var_os("OTTER_JIT_TRACE").is_some() {
                eprintln!(
                    "[jit-trace] materialize inline deopt frame {index}/{} fid={} pc={}",
                    frames.len(),
                    deopt.callee_fid,
                    deopt.callee_pc
                );
            }
            let function = context
                .exec_function(deopt.callee_fid)
                .ok_or(VmError::InvalidOperand)?;
            let upvalues: crate::frame_state::UpvalueSpine =
                match deopt.closure.as_closure(&self.gc_heap) {
                    Some(closure) => closure.upvalues_snapshot(&self.gc_heap).into_boxed_slice(),
                    None => Vec::new().into_boxed_slice(),
                };
            let mut window = self.alloc_reg_window(deopt.registers.len())?;
            window.copy_from_slice(&deopt.registers);
            let return_register = (index != 0).then_some(deopt.return_register);
            let mut frame = Frame::with_exec_return_upvalues_and_this(
                function,
                return_register,
                upvalues,
                deopt.this,
                window,
            );
            frame.self_value = deopt.closure;
            frame.pc = deopt.callee_pc;
            materialized.push(frame);
        }

        self.enter_sync_reentry()?;
        for frame in materialized {
            stack.push(frame);
        }
        let result = self.dispatch_loop(context, stack);
        self.leave_sync_reentry();
        while stack.len() > initial_stack_len {
            if let Some(mut frame) = stack.pop() {
                self.frame_release_cold(&mut frame);
                self.reclaim_registers(&mut frame);
            }
        }
        result
    }
}
