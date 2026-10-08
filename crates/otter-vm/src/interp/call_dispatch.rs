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
//! conversion and suspension cleanup. An activation waiting on a call it
//! staged stands at the staging instruction until the call completes.
//! A completed Fatal pair propagates without another Error allocation. Raw
//! prepublication StackOverflow is normalized once by the published interpreter
//! caller, because admission failed before the child had an execution extent.
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
    /// An operation already completed with a terminal failure. Detail and
    /// frames stay in the canonical interpreter owner; no Error is rebuilt.
    Fatal(VmError),
}

/// Realm global holding the host's asm.js linker. A realm without it runs
/// every `"use asm"` body as ordinary JavaScript.
const ASM_LINKER_GLOBAL: &str = "__otterAsmLink";

/// Offer the `"use asm"` module the published `frame` runs to the realm's
/// asm.js linker as `(sourceText, functionId, stdlib, foreign, heap)`:
/// `Some` exports when it linked the module, `None` to run the body as
/// ordinary JavaScript.
fn link_asm_module(
    vm: &mut Interpreter,
    stack: &mut crate::activation_stack::ActivationStack,
    context: &crate::ExecutionContext,
    frame: *mut crate::native_abi::Frame,
) -> Result<Option<Value>, VmError> {
    let Some(linker) = crate::object::get(vm.global_this, &vm.gc_heap, ASM_LINKER_GLOBAL)
        .filter(|linker| linker.is_callable())
    else {
        return Ok(None);
    };
    let function_id = unsafe { (*frame).header.function_id };
    let Some(source) = context.function_source_text(function_id) else {
        return Ok(None);
    };
    let source = crate::JsString::from_str(source, vm.gc_heap_mut()).map_err(crate::oom_to_vm)?;
    let active =
        unsafe { crate::ActiveFrameMut::from_ptr(frame) }.map_err(|_| VmError::InvalidOperand)?;
    let mut args: smallvec::SmallVec<[Value; 8]> = smallvec::SmallVec::new();
    args.push(Value::string(source));
    args.push(Value::number_i32(function_id as i32));
    for register in 0..3u16 {
        args.push(active.read(register).unwrap_or(Value::undefined()));
    }
    let exports =
        vm.run_callable_sync_rooted(stack, Some(context), &linker, Value::undefined(), args)?;
    Ok((!exports.is_undefined()).then_some(exports))
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
    let frame = ctx.native_frame;
    if frame.is_null() {
        return NativeResultPair::fatal_internal();
    }
    // Resolve the actual published bytecode owner before borrowing the mutator.
    let Some(context) = (unsafe { activation.owner_context((*frame).header.function_id) }) else {
        return NativeResultPair::fatal_internal();
    };
    let context = &context;
    let Some(vm) = (unsafe { activation.vm.as_mut() }) else {
        return NativeResultPair::fatal_internal();
    };
    let Some(stack) = (unsafe { activation.stack.as_mut() }) else {
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
    // A completed callee Fatal already passed its allocating throw boundary.
    // Preserving it prevents a failed Error allocation from being projected a
    // second time into a smaller RangeError. Prepublication stack admission is
    // the one raw-trampoline exception: its StackOverflow has no child extent
    // and still belongs to this published caller's ordinary catch semantics.
    if completion.validate(NativeResultDomain::Execution) == Some(NativeResultStatus::Fatal) {
        let error = unsafe { ctx.error.as_ref() }
            .and_then(Option::as_ref)
            .copied();
        if !matches!(error, Some(VmError::StackOverflow { .. })) {
            vm.pending_uncaught_throw = None;
            vm.frame_release_cold(unsafe { &mut *frame });
            return completion;
        }
    }
    // A call this activation staged has completed. The activation stands
    // at the staging instruction; a call instruction moves past it on a
    // normal completion. Only a frame waiting on its call carries the bit.
    let advance_on_resume = !tier_completion && {
        let header = unsafe { &mut (*frame).header };
        let staged_past = header
            .flags
            .contains(crate::native_abi::NativeFrameFlags::ADVANCE_ON_RESUME);
        header.flags = header
            .flags
            .without(crate::native_abi::NativeFrameFlags::ADVANCE_ON_RESUME);
        staged_past
    };
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
                if advance_on_resume {
                    let header = unsafe { &mut (*frame).header };
                    let Some(next) = header.pc.checked_add(1) else {
                        return NativeResultPair::fatal_internal();
                    };
                    header.pc = next;
                }
                None
            }
            Some(NativeResultStatus::Throw) => {
                vm.set_pending_uncaught_throw(completion.payload_value());
                vm.abandon_pending_ladders(unsafe { &mut *frame });
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
    let mut resume_site = crate::activation_stack::ThrowSite::Instruction;
    if let crate::prepared_call::ResumeInput::Throw(reason) = resume {
        // The thrown value unwinds the resumed body like any throw: a
        // handler may consume or replace it, so only the completion that
        // escapes the body reaches the resumer. The body is parked past its
        // `Await` or `GeneratorStart`.
        vm.set_pending_uncaught_throw(reason);
        resume_error = Some(VmError::Uncaught);
        resume_site = crate::activation_stack::ThrowSite::AfterCall;
    }
    // A first entry is the only interpreter entry that starts at PC zero:
    // every resumed caller stands at or past its call.
    if fresh_entry && !tier_completion && unsafe { (*frame).header.pc } == 0 {
        vm.record_runtime_bytecode_call();
    }
    if fresh_entry
        && resume_error.is_none()
        && let Err(error) = vm.begin_suspendable_activation(stack, context)
    {
        resume_error = Some(error);
    }
    if fresh_entry && !tier_completion && unsafe { (*frame).header.pc } == 0 {
        vm.begin_interpreted_retraining_activation(unsafe { &mut *frame });
    }
    // A `"use asm"` module offers itself to the host linker once per entry:
    // linked exports replace the body's completion (V8 instantiates asm.js
    // as WebAssembly the same way).
    if fresh_entry
        && !tier_completion
        && resume_error.is_none()
        && unsafe { (*frame).header.pc } == 0
        && context
            .exec_function(unsafe { (*frame).header.function_id })
            .is_some_and(|function| function.asm_module)
    {
        match link_asm_module(vm, stack, context, frame) {
            Ok(Some(exports)) => {
                vm.frame_release_cold(unsafe { &mut *frame });
                return NativeResultPair::success(exports);
            }
            Ok(None) => {}
            Err(error) => resume_error = Some(error),
        }
    }
    // A compiled body that side-exited at its entry resumes here at PC zero;
    // entering it again in the same turn would repeat that exit forever.
    if fresh_entry
        && !tier_completion
        && resume_error.is_none()
        && !stack.has_prepared_call()
        && vm.jit_hook.is_some()
    {
        match vm.prepare_compiled_entry(stack, context) {
            Ok(Some(entry)) => return tier_request(ctx, entry),
            Ok(None) => {}
            Err(error) => resume_error = Some(error),
        }
    }
    match vm.dispatch_current_activation(context, stack, floor, resume_error, resume_site) {
        Ok(DispatchOutcome::Call) => {
            match stack.take_request().or_else(|| stack.pending_packet()) {
                Some(packet) => {
                    ctx.pending_call = packet;
                    NativeResultPair::continue_execution()
                }
                None => NativeResultPair::fatal_internal(),
            }
        }
        Ok(DispatchOutcome::Tier(entry)) => tier_request(ctx, entry),
        Ok(DispatchOutcome::Returned(value)) => NativeResultPair::success(value),
        Err(VmError::Uncaught) => match vm.pending_uncaught_throw.take() {
            Some(exception) => NativeResultPair::throw_value(exception),
            None => NativeResultPair::fatal_internal(),
        },
        Ok(DispatchOutcome::Fatal(error)) | Err(error) => {
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
        context: Option<&ExecutionContext>,
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
        let enclosing = self.jit_context.replace(std::ptr::NonNull::from(&mut ctx));
        let previous = unsafe { stack.bind_context(&mut ctx) };
        self.begin_work_budget_turn();
        let result = unsafe { call_trampoline(&mut ctx) };
        self.finish_work_budget_turn();
        stack.restore_context(previous);
        self.jit_context = enclosing;
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
