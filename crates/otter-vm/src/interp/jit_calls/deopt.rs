//! Cold materialization for JIT side exits.
//!
//! Native execution normally keeps the canonical [`crate::NativeFrame`] and its
//! register window intact. This module is the narrow exception: a generated
//! stack-owned call or nested inline exit copies/materializes an
//! [`crate::ActivationStack`] frame only after the side exit has fired.
//!
//! # Contents
//! - [`Interpreter::jit_deopt_materialize_stack_call`] — standard generated
//!   stack-call deopt, including exact-generation diagnostics and policy.
//! - [`Interpreter::jit_deopt_materialize_inline_frames`] — nested inline deopt.
//!
//! # Invariants
//! - These APIs are cold side-exit operations, never normal call entry or a
//!   native-to-interpreter tier transition.
//! - Register values are attached/copied exactly once after the side exit.
//! - Every temporary materialized frame and register window is removed before
//!   returning to compiled code.
//! - Nested dispatch stops at the caller's activation floor and reuses the
//!   already-published runtime-turn root provider.
//! - A stack-owned frame stays published while copying and dispatching, so its
//!   machine-stack values remain precise roots until generated code unpublishes
//!   the activation after this API returns.
//! - Generated code owns synchronous-depth and native-stack-byte counters.
//!   Stack-call deopt neither enters nor leaves synchronous re-entry. Its
//!   logical-depth slot is transferred temporarily to the materialized frame
//!   so interpreter stack checks count native outer frames exactly once.
//! - New interpreter/baseline/optimizer transitions must use
//!   [`crate::ActiveFrameMut`] over the existing [`NativeFrame`] instead.
//!
//! # See also
//! - [`crate::active_frame`] — canonical tier-neutral activation access.
//! - [`crate::jit::JitDeoptFrame`] — owned inline-deopt reconstruction input.
//! - [`crate::NativeFrame`] — canonical activation reconstructed only after a
//!   cold side exit.

use crate::{
    ActivationStack, ActiveFrameRef, ExecutionContext, Frame, Interpreter, NativeFrame,
    NativeFrameFlags, Value, VmError, jit,
};

impl Interpreter {
    /// Copy one generated stack-owned activation into cold interpreter storage
    /// and run it from its exact published resume PC.
    ///
    /// `native` is a scalar snapshot of the still-published stack frame.
    /// Generated code retains ownership of its synchronous-depth and
    /// native-stack-byte reservations across this call and releases them only
    /// after the interpreter continuation returns.
    pub fn jit_deopt_materialize_stack_call(
        &mut self,
        context: &ExecutionContext,
        stack: &mut ActivationStack,
        native: NativeFrame,
        caller_function_id: u32,
        caller_call_pc: u32,
        callee_code_object_id: u64,
        caller_code_object_id: u64,
        call_kind: jit::JitDirectCallKind,
    ) -> Result<Value, VmError> {
        if !native
            .header
            .flags
            .contains(NativeFrameFlags::STACK_REGISTERS)
            || native.header.flags.contains(NativeFrameFlags::MATERIALIZED)
        {
            return Err(VmError::InvalidOperand);
        }
        self.note_generated_call_deopt(
            caller_function_id,
            caller_call_pc,
            caller_code_object_id,
            callee_code_object_id,
            call_kind,
            native,
        )?;

        // SAFETY: generated code keeps the original NativeFrame and both
        // windows initialized, published, and stable across this cold call.
        // The copied descriptor retains the exact same raw window addresses.
        let active = unsafe { ActiveFrameRef::from_native_ptr(&native) }
            .map_err(|_| VmError::InvalidOperand)?;
        let function = context
            .exec_function(native.header.function_id)
            .ok_or(VmError::InvalidOperand)?;
        let register_count = active.register_count();
        if register_count != usize::from(function.register_count)
            || active.upvalue_count() < usize::from(function.own_upvalue_count)
        {
            return Err(VmError::InvalidOperand);
        }

        // Copy all scalar and tagged inputs before building the materialized
        // frame. Host Vec/register-arena growth cannot collect; once pushed,
        // ordinary runtime-turn tracing owns the new interpreter storage.
        let self_value = active.self_value();
        let this_value = active.this_value();
        let mut upvalues = Vec::with_capacity(active.upvalue_count());
        for index in 0..active.upvalue_count() {
            let index = u32::try_from(index).map_err(|_| VmError::InvalidOperand)?;
            upvalues.push(active.upvalue(index)?);
        }

        let _window_rollback = self.register_window_rollback();
        let mut window = self.alloc_reg_window(register_count)?;
        for index in 0..register_count {
            let register = u16::try_from(index).map_err(|_| VmError::InvalidOperand)?;
            window[index] = active.read(register)?;
        }
        let mut frame = Frame::with_exec_return_upvalues_and_this(
            function,
            None,
            upvalues.into_boxed_slice(),
            this_value,
            window,
        );
        frame.self_value = self_value;
        frame.pc = native.header.pc;

        self.with_materialized_generated_call_depth(|interp| {
            let floor = stack.floor();
            stack.push(frame);
            let result = interp.dispatch_loop_above_rooted(context, stack, floor);
            interp.release_frames_above(stack, floor);
            result
        })?
    }

    /// Materialize a nested inline deopt chain and run it to completion.
    ///
    /// `frames` is ordered outermost first. The outermost completion returns to
    /// compiled code; every younger frame returns through its recorded parent
    /// destination. This owned reconstruction is reserved for inlined
    /// optimized exits that cannot resume one canonical native activation.
    pub fn jit_deopt_materialize_inline_frames(
        &mut self,
        context: &ExecutionContext,
        stack: &mut ActivationStack,
        frames: &[jit::JitDeoptFrame],
    ) -> Result<Value, VmError> {
        let _window_rollback = self.register_window_rollback();
        let floor = stack.floor();
        let mut materialized: smallvec::SmallVec<[Frame; 4]> = smallvec::SmallVec::new();

        for (index, deopt) in frames.iter().enumerate() {
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
            self.record_jit_debug_event(|| crate::JitDebugEvent::InlineDeoptFrame {
                index: u32::try_from(index).unwrap_or(u32::MAX),
                total: u32::try_from(frames.len()).unwrap_or(u32::MAX),
                function_id: deopt.callee_fid,
                resume_pc: deopt.callee_pc,
            });
        }

        self.enter_sync_reentry()?;
        for frame in materialized {
            stack.push(frame);
        }
        let result = self.dispatch_loop_above_rooted(context, stack, floor);
        self.leave_sync_reentry();
        self.release_frames_above(stack, floor);
        result
    }
}
