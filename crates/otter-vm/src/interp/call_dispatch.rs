//! Interpreter entry and host entry into the common JavaScript stack.
//!
//! # Contents
//! - [`DispatchOutcome`] returns completion or a pending call to assembly.
//! - [`interpreter_entry`] executes one published activation.
//! - Host invocation publishes one shared execution context for its extent.
//!
//! # Invariants
//! An interpreter entry never dispatches its caller or child. Child requests
//! return to assembly after all Rust borrows end. A resumed payload is homed
//! before allocation. The native frame chain stays published throughout error
//! conversion, finally execution and suspension cleanup.
//!
//! # See also
//! - [`super::dispatch`] for bytecode execution.
//! - [`crate::native_abi::call_trampoline`] for activation ownership.

use crate::{
    ActivationFloor, ActivationStack, ExecutionContext, Interpreter, Value, VmError,
    jit::VmRuntimeActivation,
    native_abi::{
        JitCtx, NativeResultDomain, NativeResultPair, NativeResultStatus, VmThread, call_trampoline,
    },
};

pub(crate) enum DispatchOutcome {
    Returned(Value),
    Call,
    Tier(usize),
}

pub(crate) extern "C" fn interpreter_entry(ctx: *mut JitCtx) -> NativeResultPair {
    interpreter_turn(ctx, true)
}

pub(crate) extern "C" fn interpreter_resume_entry(ctx: *mut JitCtx) -> NativeResultPair {
    interpreter_turn(ctx, false)
}

fn tier_request(ctx: &mut JitCtx, entry: usize) -> NativeResultPair {
    ctx.pending_call = crate::native_abi::CallRequest::EMPTY;
    ctx.pending_call.entry = entry as u64;
    ctx.pending_call.header.flags = crate::native_abi::NativeFrameFlags::from_bits(
        crate::native_abi::NativeFrameFlags::TIER_ENTRY,
    );
    NativeResultPair::continue_execution()
}

fn interpreter_turn(ctx: *mut JitCtx, entering: bool) -> NativeResultPair {
    // SAFETY: the trampoline retains this context, its services and its frame.
    let ctx = unsafe { &mut *ctx };
    let Some(activation) = ctx.checked_activation().copied() else {
        return NativeResultPair::fatal_internal();
    };
    let Some(vm) = (unsafe { activation.vm.as_mut() }) else {
        return NativeResultPair::fatal_internal();
    };
    let Some(stack) = (unsafe { activation.stack.as_mut() }) else {
        return NativeResultPair::fatal_internal();
    };
    let Some(context) = (unsafe { activation.context.as_ref() }) else {
        return NativeResultPair::fatal_internal();
    };
    let fresh_entry = entering;
    if fresh_entry {
        stack.clear_completion();
    }
    // The trampoline copied any staged request span before this entry.
    stack.clear_staged();
    let resume = if fresh_entry && stack.has_prepared_call() {
        stack.consume_pending()
    } else {
        crate::prepared_call::ResumeInput::Normal
    };
    let frame = ctx.native_frame;
    let floor = ActivationFloor::at_depth(unsafe { (*frame).depth } as usize - 1);
    let completion = ctx.completion;
    let destination = ctx.completion_destination;
    let generation = ctx.completion_generation;
    ctx.completion = NativeResultPair::success(Value::UNDEFINED);
    ctx.completion_destination = u32::MAX;
    ctx.completion_generation = 0;
    let tier_completion = destination == crate::native_abi::TIER_COMPLETION_DESTINATION;
    if tier_completion && let Some(status) = completion.validate(NativeResultDomain::Compiled) {
        vm.jit_code_registry
            .note_completion(u64::from(generation), status);
    }
    if tier_completion
        && unsafe { (*frame).header.kind } == crate::native_abi::NativeFrameKind::Interpreter
    {
        // A cold inline deopt already resumed and completed this activation.
        // Its generated exit returns the committed completion to assembly.
        return if completion
            .validate(NativeResultDomain::Execution)
            .is_some_and(|status| {
                matches!(
                    status,
                    NativeResultStatus::Success
                        | NativeResultStatus::Throw
                        | NativeResultStatus::Fatal
                )
            }) {
            completion
        } else {
            NativeResultPair::fatal_internal()
        };
    }
    let resume_error = if tier_completion {
        match vm.complete_compiled_entry(stack, context, floor, completion) {
            Ok(Some(value)) => return NativeResultPair::success(value),
            Ok(None) => None,
            Err(error) => Some(error),
        }
    } else {
        match completion.validate(NativeResultDomain::Execution) {
            Some(NativeResultStatus::Success) => {
                if destination != u32::MAX {
                    let Ok(destination) = u16::try_from(destination) else {
                        return NativeResultPair::fatal_internal();
                    };
                    let Ok(mut active) = (unsafe { crate::ActiveFrameMut::from_ptr(frame) }) else {
                        return NativeResultPair::fatal_internal();
                    };
                    if active
                        .write(destination, completion.payload_value())
                        .is_err()
                    {
                        return NativeResultPair::fatal_internal();
                    }
                }
                None
            }
            Some(NativeResultStatus::Throw) => {
                vm.set_pending_uncaught_throw(completion.payload_value());
                Some(VmError::Uncaught)
            }
            Some(NativeResultStatus::Fatal) => Some(
                unsafe { ctx.error.as_mut() }
                    .and_then(Option::take)
                    .unwrap_or(VmError::InvalidOperand),
            ),
            _ => Some(VmError::InvalidOperand),
        }
    };
    let mut resume_error = resume_error;
    match resume {
        crate::prepared_call::ResumeInput::Normal => {}
        crate::prepared_call::ResumeInput::Throw(reason) => {
            // The thrown value unwinds the resumed body like any throw: a
            // handler or `finally` may consume or replace it, so only the
            // completion that escapes the body reaches the resumer.
            vm.set_pending_uncaught_throw(reason);
            resume_error = Some(VmError::Uncaught);
        }
        crate::prepared_call::ResumeInput::Return(value) => {
            match vm.return_running_finally_above(stack, floor, value) {
                Ok(Some(value)) => return NativeResultPair::success(value),
                Ok(None) => {}
                Err(error) => resume_error = Some(error),
            }
        }
    }
    // A first entry is the only interpreter entry that starts at PC zero:
    // every resumed caller has advanced past its call.
    if fresh_entry && !tier_completion && unsafe { (*frame).header.pc } == 0 {
        vm.record_runtime_bytecode_call();
    }
    if fresh_entry
        && resume_error.is_none()
        && let Err(error) = vm.begin_suspendable_activation(stack, context)
    {
        resume_error = Some(error);
    }
    if fresh_entry && resume_error.is_none() && !stack.has_prepared_call() && vm.jit_hook.is_some() {
        match vm.prepare_compiled_entry(stack, context) {
            Ok(Some(entry)) => return tier_request(ctx, entry),
            Ok(None) => {}
            Err(error) => resume_error = Some(error),
        }
    }
    match vm.dispatch_current_activation(context, stack, floor, resume_error) {
        Ok(DispatchOutcome::Call) => match stack.take_request().or_else(|| stack.pending_packet()) {
            Some(packet) => {
                ctx.pending_call = packet;
                NativeResultPair::continue_execution()
            }
            None => NativeResultPair::fatal_internal(),
        },
        Ok(DispatchOutcome::Tier(entry)) => tier_request(ctx, entry),
        Ok(DispatchOutcome::Returned(value)) => NativeResultPair::success(value),
        Err(VmError::Uncaught) => match vm.pending_uncaught_throw.take() {
            Some(exception) => NativeResultPair::throw_value(exception),
            None => NativeResultPair::fatal_internal(),
        },
        Err(error) => {
            vm.pending_uncaught_throw = None;
            vm.frame_release_cold(unsafe { &mut *frame });
            if let Some(slot) = unsafe { ctx.error.as_mut() } {
                *slot = Some(error);
            }
            NativeResultPair::fatal_internal()
        }
    }
}

impl Interpreter {
    pub(crate) fn execute_prepared_call(
        &mut self,
        context: &ExecutionContext,
        stack: &mut ActivationStack,
    ) -> Result<Value, VmError> {
        if !stack.is_runtime_rooted_by(self) {
            return Err(VmError::InvalidOperand);
        }
        let packet = stack
            .take_request()
            .or_else(|| stack.pending_packet())
            .ok_or(VmError::InvalidOperand)?;
        let mut activation = VmRuntimeActivation::new(self, stack, context);
        let mut thread = VmThread::empty();
        thread.runtime_context = std::ptr::from_mut(&mut activation) as u64;
        thread.code_registry = self.jit_code_registry_view_addr();
        thread.interrupt_cell = self.jit_interrupt_flag_ptr() as u64;
        thread.gc_heap = self.jit_gc_heap_ptr() as u64;
        thread.backedge_fuel_cell = self.jit_backedge_fuel_ptr() as u64;
        thread.global_lexical_epoch_cell = self.jit_global_lexical_epoch_addr() as u64;
        thread.marking_flag_cell = self.jit_marking_flag_ptr() as u64;
        thread.array_index_protector_cell = self.jit_array_index_protector_addr() as u64;
        thread.active_realm_cell = self.jit_active_realm_addr() as u64;
        thread.array_buffer_detach_protector_cell =
            self.jit_array_buffer_detach_protector_addr() as u64;
        let marker = 0_u8;
        let mut error = None;
        let mut ctx = JitCtx {
            thread: &mut thread,
            native_frame: self.jit_innermost_native_frame(),
            error: &mut error,
            generated_depth_limit: u64::from(self.max_stack_depth),
            global_this_offset: self.jit_global_this_offset_addr(),
            native_stack_limit: self.jit_native_stack_limit(std::ptr::from_ref(&marker).addr()),
            generated_feedback_clean: 1,
            alloc_window: self.jit_allocation_window(),
            runtime_stats: self.jit_runtime_stats_mut_ptr(),
            pending_call: packet,
            completion: NativeResultPair::success(Value::UNDEFINED),
            completion_destination: u32::MAX,
            completion_generation: 0,
        };
        unsafe { (*ctx.thread).frame_cell = std::ptr::from_mut(&mut ctx.native_frame) as u64 };
        let enclosing = self
            .jit_frame_cell
            .replace(std::ptr::NonNull::from(&mut ctx.native_frame).cast());
        let previous = unsafe { stack.bind_context(&mut ctx) };
        self.begin_work_budget_turn();
        let result = unsafe { call_trampoline(&mut ctx) };
        self.finish_work_budget_turn();
        stack.restore_context(previous);
        self.jit_frame_cell = enclosing;
        if let Some(call) = stack.take_pending() {
            self.release_prepared_inputs(call);
        }
        let result = self.finish_compiled_entry_transaction(
            context,
            result,
            ctx.generated_feedback_clean == 0,
        )?;
        match result.validate(NativeResultDomain::Execution) {
            Some(NativeResultStatus::Success) => Ok(result.payload_value()),
            Some(NativeResultStatus::Throw) => {
                self.set_pending_uncaught_throw(result.payload_value());
                Err(VmError::Uncaught)
            }
            Some(NativeResultStatus::Fatal) => Err(error.unwrap_or(VmError::InvalidOperand)),
            _ => Err(VmError::InvalidOperand),
        }
    }
}
