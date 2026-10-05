//! Async suspension and throw-unwind helpers.
//!
//! Async `await` and exception propagation are cold control-flow paths for the
//! dense interpreter. Keeping them out of `lib.rs` leaves the dispatch loop as
//! the opcode router while preserving the exact stack and microtask semantics.
//!
//! # Contents
//! - `Op::Await` parking for async functions and async generators.
//! - Microtask-driven async function and async-generator resume.
//! - Throw unwinding through each frame's handler table, and async rejection
//!   absorption.
//!
//! # Invariants
//! - A throw lands in the first handler-table entry covering the throwing
//!   instruction: the top frame's own instruction, or the call instruction
//!   of every caller.
//! - Await parking advances the frame PC before removing it from the active
//!   stack.
//! - Rejected awaits re-enter through the same throw-unwind path as
//!   synchronous `throw`.
//! - Async frames absorb unhandled JavaScript throws by rejecting their
//!   result promise. Completed terminal failures leave through the existing
//!   owned RunError without rejection projection.
//! - Every resume exit releases frames, cold records, and register windows back
//!   to the activation floor while that stack is still published as a GC root.
//!
//! # See also
//! - [`crate::microtask`]
//! - [`crate::promise_dispatch`]
//! - [`otter_bytecode::ExceptionHandler`]

use crate::activation_stack::{ActivationFloor, ActivationStack, ThrowSite};

use crate::promise::JsPromise;
use crate::runtime_activation::CommittedValueError;
use crate::{ExecutionContext, Frame, Interpreter, RunError, Value, VmError, promise_dispatch};

impl Interpreter {
    /// §27.7.5.3 Await step 2 — `PromiseResolve(%Promise%, value)`.
    ///
    /// A native promise is returned as-is; any other value (including
    /// a user-defined thenable) is settled through a fresh promise's
    /// resolve function so thenables are adopted (§27.2.1.3.2) rather
    /// than awaited as opaque values.
    fn await_promise_resolve(
        &mut self,
        context: &ExecutionContext,
        stack: &mut ActivationStack,
        value: Value,
    ) -> Result<crate::promise::JsPromiseHandle, CommittedValueError> {
        // §27.2.4.7 PromiseResolve — a promise is handed back as itself
        // only once its `constructor` proves to be this realm's; that read
        // is observable, and skipping it also skipped the ticks a
        // mismatched constructor costs.
        let promise = self.promise_resolve_value(stack, Some(context), value)?;
        promise
            .as_promise()
            .ok_or(CommittedValueError::Fatal(VmError::InvalidOperand))
    }

    /// Handle [`otter_bytecode::Op::Await`]: park the current
    /// async frame off the active stack and attach resume / reject
    /// reactions to the awaited promise.
    ///
    /// # Algorithm
    /// 1. Resolve the awaited value through [`Self::await_promise_resolve`]
    ///    (`PromiseResolve(%Promise%, v)`) so a thenable is adopted and a
    ///    plain value settles on the next microtask tick.
    /// 2. Advance the parked frame's pc past the `Await`
    ///    instruction so resumption continues with the next op.
    /// 3. Move the frame and cold state into the canonical managed parked
    ///    frame. The first settling reaction consumes it; its twin is a no-op.
    /// 4. Register resume reactions carrying the exact source, origin realm
    ///    and traced async context. The queued resume delivers the completion
    ///    into the parked frame's `dst` register.
    ///
    /// # Invariants
    /// - The frame at the top of `stack` MUST have async ownership in its cold
    ///   record; the compiler enforces
    ///   this. Violating it is a bytecode-malformation error and
    ///   surfaces as `VmError::InvalidOperand`.
    /// - On return, `stack` no longer contains the parked frame.
    ///   Callers that need to know whether the dispatch loop should
    ///   exit (because the parked frame was at the bottom) read
    ///   `stack.is_empty()` after this call.
    ///
    /// # Errors
    /// - [`VmError::InvalidOperand`] when called on a non-async
    ///   frame.
    pub(crate) fn do_await(
        &mut self,
        stack: &mut ActivationStack,
        context: &ExecutionContext,
        dst: u16,
        awaited: Value,
    ) -> Result<(), CommittedValueError> {
        self.with_handle_scope(|vm, scope| {
            let top_idx = stack.len() - 1;
            // §27.6 Async-generator body — the running frame has no
            // regular async-function state, but its cold record carries a
            // generator owner whose body was flagged
            // async. Park the frame on a dedicated resume native that
            // re-enters the generator body and either settles the
            // front queued request from a subsequent `Op::Yield` /
            // completion, or chains another `Op::Await`.
            if !vm.frame_has_async_state(&stack[top_idx]) {
                if let Some(owner) = vm.frame_generator_owner(&stack[top_idx])
                    && owner.is_async(&vm.gc_heap)
                {
                    return vm.do_await_async_gen(stack, context, dst, awaited, owner);
                }
                return Err(CommittedValueError::Fatal(VmError::InvalidOperand));
            }
            // Advance past the Await before parking so resumption
            // continues at the next instruction.
            stack[top_idx]
                .advance_pc()
                .map_err(|error| CommittedValueError::Fatal(error.into()))?;
            // The continuation belongs to the context that awaited, so capture it
            // before the frame parks.
            let async_context = vm.scoped_value(scope, vm.async_context());
            let realm_id = vm.active_host_realm_id();
            let promise = vm.await_promise_resolve(context, stack, awaited)?;
            // The capability and parked-frame allocations below move the awaited
            // promise; it rides an anchor slot and is re-read before the resume
            // reactions register, so they land on the live promise rather than
            // its vacated slot.
            let promise_slot = vm.push_iteration_anchor(Value::promise(promise)) - 1;
            let outcome = (|this: &mut Self| -> Result<(), CommittedValueError> {
                let capability =
                    promise_dispatch::PromiseBuilder::with_context(Some(context.clone()))
                        .capability_stack_rooted(this, stack, &[], &[])
                        .map_err(|error| CommittedValueError::JavaScript(error.into()))?;
                let capability_promise = this.scoped_value(scope, capability.promise);
                let capability_resolve = this.scoped_value(scope, capability.resolve);
                let capability_reject = this.scoped_value(scope, capability.reject);
                let parked = stack.pop().expect("top frame existed");
                let detached_cold = this.frame_detach_cold(parked);
                let parked = this.park_active_frame(parked);
                let parked =
                    crate::generator::alloc_parked_frame(&mut this.gc_heap, parked, detached_cold)
                        .map_err(|error| CommittedValueError::JavaScript(error.into()))?;
                let promise = this
                    .iteration_anchor(promise_slot)
                    .as_promise()
                    .expect("anchored awaited promise survives the park allocations");
                let capability = crate::promise::PromiseCapability {
                    promise: this.escape_scoped(capability_promise),
                    resolve: this.escape_scoped(capability_resolve),
                    reject: this.escape_scoped(capability_reject),
                    context: capability.context,
                };
                let async_context = this.escape_scoped(async_context);
                let outcome = promise.perform_async_resume_then(
                    &mut this.gc_heap,
                    parked,
                    dst,
                    capability,
                    None,
                    Some(context.clone()),
                    async_context,
                    realm_id,
                );
                if let Some(job) = outcome.immediate_job {
                    this.microtasks.enqueue(job);
                }
                Ok(())
            })(vm);
            vm.pop_iteration_anchors_to(promise_slot);
            outcome
        })
    }

    /// §27.6.3 — `Op::Await` inside an async-generator body. Parks
    /// the running frame and attaches resume / reject reactions
    /// that re-enter the body when the awaited promise settles. On
    /// resume, the generator's front request is settled by a
    /// subsequent `Op::Yield`, completion, or further `Op::Await`.
    fn do_await_async_gen(
        &mut self,
        stack: &mut ActivationStack,
        context: &ExecutionContext,
        dst: u16,
        awaited: Value,
        owner: crate::generator::JsGenerator,
    ) -> Result<(), CommittedValueError> {
        self.with_handle_scope(|vm, scope| {
            let owner_root = vm.scoped_value(scope, Value::generator(owner));
            let top_idx = stack.len() - 1;
            stack[top_idx]
                .advance_pc()
                .map_err(|error| CommittedValueError::Fatal(error.into()))?;
            // The continuation belongs to the context that awaited, so capture it
            // before the frame parks.
            let async_context = vm.scoped_value(scope, vm.async_context());
            let realm_id = vm.active_host_realm_id();
            let promise = vm.await_promise_resolve(context, stack, awaited)?;
            // Same anchoring as the regular-await path: the park allocations
            // below move the awaited promise.
            let promise_slot = vm.push_iteration_anchor(Value::promise(promise)) - 1;
            let capability =
                match promise_dispatch::PromiseBuilder::with_context(Some(context.clone()))
                    .capability_stack_rooted(vm, stack, &[], &[])
                {
                    Ok(capability) => capability,
                    Err(err) => {
                        vm.pop_iteration_anchors_to(promise_slot);
                        return Err(CommittedValueError::JavaScript(err.into()));
                    }
                };
            let capability_promise = vm.scoped_value(scope, capability.promise);
            let capability_resolve = vm.scoped_value(scope, capability.resolve);
            let capability_reject = vm.scoped_value(scope, capability.reject);
            let parked = stack.pop().expect("top frame existed");
            let detached_cold = vm.frame_detach_cold(parked);
            let parked = vm.park_active_frame(parked);
            let parked = match crate::generator::alloc_parked_frame(
                &mut vm.gc_heap,
                parked,
                detached_cold,
            ) {
                Ok(parked) => parked,
                Err(err) => {
                    vm.pop_iteration_anchors_to(promise_slot);
                    return Err(CommittedValueError::JavaScript(err.into()));
                }
            };
            let promise = vm
                .iteration_anchor(promise_slot)
                .as_promise()
                .expect("anchored awaited promise survives the park allocations");
            vm.pop_iteration_anchors_to(promise_slot);
            let capability = crate::promise::PromiseCapability {
                promise: vm.escape_scoped(capability_promise),
                resolve: vm.escape_scoped(capability_resolve),
                reject: vm.escape_scoped(capability_reject),
                context: capability.context,
            };
            let owner = vm
                .escape_scoped(owner_root)
                .as_generator()
                .ok_or(VmError::InvalidOperand)
                .map_err(|error| CommittedValueError::Fatal(error.into()))?;
            let async_context = vm.escape_scoped(async_context);
            let outcome = promise.perform_async_resume_then(
                &mut vm.gc_heap,
                parked,
                dst,
                capability,
                Some(owner),
                Some(context.clone()),
                async_context,
                realm_id,
            );
            // The body is parked on the awaited promise. The state is the ONLY
            // reliable awaiting signal: the request queue also holds the plain
            // `.next()` that is waiting on this turn, so queue-emptiness cannot
            // distinguish an await from a completed body.
            vm.escape_scoped(owner_root)
                .as_generator()
                .ok_or(VmError::InvalidOperand)
                .map_err(|error| CommittedValueError::Fatal(error.into()))?
                .set_async_state(
                    &mut vm.gc_heap,
                    crate::generator::AsyncGeneratorState::Awaiting,
                );
            if let Some(job) = outcome.immediate_job {
                vm.microtasks.enqueue(job);
            }
            Ok(())
        })
    }

    /// Resume an async-generator body whose `Op::Await` parked
    /// `frame`. Mirrors [`Self::run_async_resume`] but settles the
    /// generator's request queue on completion / unhandled
    /// throw rather than the frame's `async_state` promise.
    // Box keeps the common Microtask::Call variant compact while ownership
    // transfers from the queue into this single-shot resume path.
    #[allow(clippy::boxed_local)]
    pub(crate) fn run_async_gen_resume(
        &mut self,
        context: &ExecutionContext,
        frame: Box<crate::frame_state::ParkedFrameState>,
        cold: Option<Box<crate::cold_frame::ColdFrame>>,
        await_dst: u16,
        fulfilled: bool,
        value: Value,
        owner: crate::generator::JsGenerator,
    ) -> Result<(), RunError> {
        let mut frame = self.resume_parked_frame(*frame).map_err(RunError::bare)?;
        if let Some(c) = cold {
            self.prepared_attach_cold(&mut frame, *c);
        }
        let mut stack: ActivationStack = ActivationStack::new();
        let floor = stack.floor();
        stack.push(frame);
        // The value and owner remain live even after unwind pops the resumed
        // frame. The activation envelope publishes this anchor stack and the
        // local frame stack exactly once across both pre-dispatch rejection
        // injection and nested bytecode dispatch.
        let value_anchor = self.push_iteration_anchor(value) - 1;
        let owner_anchor = self.push_iteration_anchor(Value::generator(owner)) - 1;
        let result = self.with_runtime_turn(&mut stack, |turn| {
            let (interp, stack) = turn.into_parts();
            let result = (|| -> Result<(), RunError> {
                let value = interp.iteration_anchor(value_anchor);
                let owner = interp
                    .iteration_anchor(owner_anchor)
                    .as_generator()
                    .ok_or_else(|| RunError::bare(VmError::InvalidOperand))?;
                let call = stack
                    .pending_mut()
                    .ok_or_else(|| RunError::bare(VmError::InvalidOperand))?;
                if fulfilled {
                    call.seed_register(await_dst, value)
                        .map_err(RunError::bare)?;
                } else {
                    call.resume = crate::prepared_call::ResumeInput::Throw(value);
                }
                owner.set_async_state(
                    &mut interp.gc_heap,
                    crate::generator::AsyncGeneratorState::Executing,
                );
                match interp.dispatch_loop_above_rooted(context, stack, floor) {
                    Ok(value) => {
                        let yielded_already = owner.has_yielded(&interp.gc_heap);
                        if yielded_already {
                            // Op::Yield already settled the request and
                            // saved the frame back to the gen.
                            owner.take_yielded(&mut interp.gc_heap);
                            return Ok(());
                        }
                        // A further Op::Await re-parked the body: the frame
                        // left this stack alive, not completed. Settling the
                        // request here would answer `done: true` while user
                        // statements still wait to run.
                        if matches!(
                            owner.async_state(&interp.gc_heap),
                            crate::generator::AsyncGeneratorState::Awaiting
                        ) {
                            return Ok(());
                        }
                        // Body completed: settle the front request with
                        // the final return value as `done: true`.
                        interp
                            .async_generator_complete_step(Some(context), &owner, Ok(value), true)
                            .map_err(RunError::bare)?;
                        owner.mark_done(&mut interp.gc_heap);
                        interp
                            .async_generator_drain_done(stack, Some(context), &owner)
                            .map_err(|error| error.into_run_error(interp))?;
                        Ok(())
                    }
                    Err(error) => {
                        owner.mark_done(&mut interp.gc_heap);
                        let completion = CommittedValueError::completed_call(error);
                        let CommittedValueError::JavaScript(error) = completion else {
                            return Err(completion.into_run_error(interp));
                        };
                        let rejection = interp.vm_error_to_throwable_with_stack_roots(
                            Some(context),
                            stack,
                            &error,
                        );
                        match rejection {
                            Ok(reason) => {
                                interp
                                    .async_generator_complete_step(
                                        Some(context),
                                        &owner,
                                        Err(reason),
                                        true,
                                    )
                                    .map_err(RunError::bare)?;
                                interp
                                    .async_generator_drain_done(stack, Some(context), &owner)
                                    .map_err(|error| error.into_run_error(interp))?;
                                Ok(())
                            }
                            Err(error) => {
                                let frames = interp.snapshot_active_frames(context, usize::MAX);
                                Err(RunError {
                                    error,
                                    frames,
                                    detail: interp.take_error_detail(),
                                })
                            }
                        }
                    }
                }
            })();
            interp.release_frames_above(stack, floor);
            result
        });
        self.pop_iteration_anchors_to(value_anchor);
        result
    }

    /// Drive a [`crate::microtask::MicrotaskKind::AsyncResume`] task: re-push
    /// the parked async frame onto a fresh stack and run
    /// [`Self::dispatch_loop`] until it settles.
    ///
    /// # Algorithm
    /// 1. On the fulfillment path, write the resolved value into
    ///    the await's destination register and run dispatch.
    /// 2. On the rejection path, push the frame, then enter
    ///    dispatch by injecting an immediate throw via
    ///    [`Self::unwind_throw`]. If unwind eats the throw via an
    ///    in-frame handler, dispatch continues normally; if no
    ///    handler exists, unwind settles the result promise as
    ///    rejected and the stack is empty so the loop never starts.
    ///
    /// # Errors
    /// - Propagates any `VmError` raised inside the resumed body.
    ///   Async frames absorb their own throws via `async_state`,
    ///   so the only errors that escape are runtime-level (OOM,
    ///   stack overflow, interrupt).
    // See run_async_gen_resume: the box is a queue-layout decision, not an
    // allocation introduced by this call boundary.
    #[allow(clippy::boxed_local)]
    pub(crate) fn run_async_resume(
        &mut self,
        context: &ExecutionContext,
        frame: Box<crate::frame_state::ParkedFrameState>,
        cold: Option<Box<crate::cold_frame::ColdFrame>>,
        await_dst: u16,
        fulfilled: bool,
        value: Value,
    ) -> Result<(), RunError> {
        let mut frame = self.resume_parked_frame(*frame).map_err(RunError::bare)?;
        if let Some(c) = cold {
            self.prepared_attach_cold(&mut frame, *c);
        }
        let mut stack: ActivationStack = ActivationStack::new();
        let floor = stack.floor();
        stack.push(frame);
        let value_anchor = self.push_iteration_anchor(value) - 1;
        let result = self.with_runtime_turn(&mut stack, |turn| {
            let (interp, stack) = turn.into_parts();
            let result = (|| -> Result<(), RunError> {
                let value = interp.iteration_anchor(value_anchor);
                let call = stack
                    .pending_mut()
                    .ok_or_else(|| RunError::bare(VmError::InvalidOperand))?;
                if fulfilled {
                    call.seed_register(await_dst, value)
                        .map_err(RunError::bare)?;
                } else {
                    call.resume = crate::prepared_call::ResumeInput::Throw(value);
                }
                match interp.dispatch_loop_above_rooted(context, stack, floor) {
                    Ok(_) => Ok(()),
                    Err(error) => {
                        let frames = interp.snapshot_active_frames(context, usize::MAX);
                        Err(RunError {
                            error,
                            frames,
                            detail: interp.take_error_detail(),
                        })
                    }
                }
            })();
            interp.release_frames_above(stack, floor);
            result
        });
        self.pop_iteration_anchors_to(value_anchor);
        result
    }

    /// Walk the live frame stack looking for a handler that absorbs an
    /// in-flight throw.
    ///
    /// # Algorithm
    /// For each frame from the top, `site` first and every caller after it
    /// past its call instruction:
    /// - **Handler hit** — the first entry of the function's handler table
    ///   covering the throwing instruction receives the value in its
    ///   register and the frame continues at its target.
    /// - **No handler, async frame** — settle its result promise as
    ///   rejected, drain the resulting jobs into the microtask queue, pop
    ///   the frame, and stop: the caller already holds the promise.
    /// - **Otherwise** — pop the frame and continue.
    ///
    /// # Errors
    /// - [`VmError::Uncaught`] when the root execution region empties without
    ///   a handler and no async-frame absorbed the throw.
    #[cfg(test)]
    pub(crate) fn unwind_throw(
        &mut self,
        context: &ExecutionContext,
        stack: &mut ActivationStack,
        value: Value,
        site: ThrowSite,
    ) -> Result<(), VmError> {
        self.unwind_throw_above(context, stack, ActivationFloor::ROOT, value, site)
    }

    /// Unwind only frames owned by the execution region above `floor`.
    ///
    /// The caller-owned frame at `floor` is never inspected or popped. If no
    /// handler or async frame absorbs the throw before that boundary, the
    /// original thrown [`Value`] is retained for the caller and
    /// [`VmError::Uncaught`] is returned.
    pub(crate) fn unwind_throw_above(
        &mut self,
        context: &ExecutionContext,
        stack: &mut ActivationStack,
        floor: ActivationFloor,
        value: Value,
        site: ThrowSite,
    ) -> Result<(), VmError> {
        self.unwind_throw_with_uncaught_above(context, stack, floor, value, None, site)
    }

    /// Structured-error variant of [`Self::unwind_throw_above`].
    ///
    /// `uncaught_error` replaces [`VmError::Uncaught`] only after every frame
    /// above `floor` has been unwound. As on the root path, a structured error
    /// does not publish `value` through `pending_uncaught_throw`.
    ///
    /// # Throw-site provenance
    /// A throw that lands in a handler or an async frame costs no stack
    /// capture and clears `pending_uncaught_frames`. Raw sites of the whole
    /// published chain are captured only before the first handler-less frame
    /// is popped; they are resolved into owned snapshots, and the thrown value
    /// rendered, only when the throw leaves this region at `floor`. Provenance
    /// already pending from a nested throw that escaped an inner region stays
    /// authoritative.
    pub(crate) fn unwind_throw_with_uncaught_above(
        &mut self,
        context: &ExecutionContext,
        stack: &mut ActivationStack,
        floor: ActivationFloor,
        value: Value,
        mut uncaught_error: Option<VmError>,
        site: ThrowSite,
    ) -> Result<(), VmError> {
        if floor.depth() > stack.len() {
            return Err(VmError::InvalidOperand);
        }
        let inherited_provenance = self.pending_uncaught_frames.is_some();
        let mut unwound_sites: Option<Vec<crate::native_stack_snapshot::FrameSite>> = None;
        let mut value = value;
        let mut value_root = otter_gc::RootScope::new(&mut self.gc_heap);
        // SAFETY: `value` precedes the scope and stays in place until it
        // lands in a register, a pending-throw root, or a rejected promise;
        // settling a promise allocates.
        unsafe {
            crate::rooting::RootScopeExt::add_value(&mut value_root, &mut value);
        }
        let mut site = site;
        loop {
            if stack.is_at_floor(floor) {
                if !inherited_provenance {
                    let frames = match unwound_sites.take() {
                        Some(sites) => crate::native_stack_snapshot::resolve_frame_sites(
                            context,
                            &sites,
                            usize::MAX,
                        ),
                        None => self.snapshot_active_frames(context, usize::MAX),
                    };
                    self.pending_uncaught_frames = Some(frames);
                }
                if let Some(error) = uncaught_error.take() {
                    return Err(error);
                }
                let display = self.render_thrown(&value);
                self.pending_uncaught_throw = Some(value);
                return Err(self.err_uncaught(display.into()));
            }
            let frame = stack.last_mut().expect("frame present");
            if let Some(handler) = frame_handler(context, frame, site)? {
                frame.pc = handler.target;
                let slot = frame
                    .registers
                    .get_mut(usize::from(handler.exception))
                    .ok_or(VmError::InvalidOperand)?;
                *slot = value;
                self.pending_uncaught_frames = None;
                return Ok(());
            }
            site = ThrowSite::AfterCall;
            // Async frames absorb their own unhandled throws into the
            // result promise as a rejection — spec §27.7.5.3 step 1.h.iii.
            if self.frame_has_async_state(stack.last().expect("frame still present")) {
                let popped = stack.pop().expect("frame existed at last");
                self.complete_interpreted_retraining_activation(popped);
                let result_promise = self
                    .frame_take_async_state(popped)
                    .expect("async ownership checked just above")
                    .result_promise;
                self.frame_release_cold(popped);
                // The rejected promise is the activation's completion.
                self.completed_activation_result = Some(Value::promise(result_promise));
                let jobs = result_promise.reject(&mut self.gc_heap, value);
                self.note_settle_rejection(&jobs, Some(context));
                for j in jobs.jobs {
                    self.microtasks.enqueue(j);
                }
                self.pending_uncaught_frames = None;
                return Ok(());
            }
            if !inherited_provenance && unwound_sites.is_none() {
                unwound_sites = Some(self.capture_active_sites());
            }
            let popped = stack.pop().expect("frame still present");
            self.complete_interpreted_retraining_activation(popped);
            self.frame_release_cold(popped);
        }
    }
}

/// The handler a throw lands in within `frame`, whose PC stands at `site`.
fn frame_handler(
    context: &ExecutionContext,
    frame: &Frame,
    site: ThrowSite,
) -> Result<Option<otter_bytecode::ExceptionHandler>, VmError> {
    let resolved = context
        .for_function(frame.function_id)
        .map_err(|_| VmError::InvalidOperand)?;
    let function = resolved
        .exec_function(frame.function_id)
        .ok_or(VmError::InvalidOperand)?;
    if function.control_flow().handlers().is_empty() {
        return Ok(None);
    }
    let pc = match site {
        ThrowSite::Instruction => frame.pc,
        ThrowSite::AfterCall => frame.pc.checked_sub(1).ok_or(VmError::InvalidOperand)?,
    };
    Ok(function.control_flow().handler_at(pc))
}

#[cfg(test)]
mod tests {
    use super::*;
    use otter_bytecode::Function;

    use crate::frame_state::{AsyncFrameState, ParkedFrameState};

    fn empty_context() -> ExecutionContext {
        ExecutionContext::from_module(
            crate::test_support::minimal_bytecode_module("async-ops-test.js"),
            crate::source_registry::SourceRegistry::default(),
        )
        .expect("valid bytecode fixture")
    }

    fn function(registers: u16) -> Function {
        Function {
            locals: registers,
            ..Function::default()
        }
    }

    fn park_regular_async_frame(
        interp: &mut Interpreter,
        context: &ExecutionContext,
        mut frame: crate::test_support::FrameFixture,
    ) -> (
        Box<ParkedFrameState>,
        Option<Box<crate::cold_frame::ColdFrame>>,
    ) {
        let result_promise = promise_dispatch::PromiseBuilder::with_context(Some(context.clone()))
            .pending_runtime_rooted(interp, &[], &[])
            .unwrap();
        interp.frame_set_async_state(&mut frame, AsyncFrameState { result_promise });
        let cold = interp.frame_detach_cold(&mut frame);
        let parked = Box::new(interp.park_active_frame(&frame));
        (parked, cold)
    }

    fn park_async_generator_frame(
        interp: &mut Interpreter,
        frame: crate::test_support::FrameFixture,
    ) -> (
        Box<ParkedFrameState>,
        Option<Box<crate::cold_frame::ColdFrame>>,
        crate::generator::JsGenerator,
    ) {
        let parked = interp.park_active_frame(&frame);
        let owner = crate::generator::JsGenerator::new_with_prototype(
            &mut interp.gc_heap,
            parked,
            None,
            None,
        )
        .unwrap();
        owner.set_async(&mut interp.gc_heap, true);
        owner.install_owner_on_frame(&mut interp.gc_heap);
        let (parked, cold) = owner.take_frame(&mut interp.gc_heap).unwrap();
        (parked, cold, owner)
    }

    fn assert_invalid_resume_cleanup(interp: &Interpreter, error: RunError) {
        assert!(matches!(error.error, VmError::InvalidOperand));
        assert_eq!(interp.cold_frames.live_len(), 0);
    }

    #[test]
    fn async_resume_invalid_destination_releases_cold_record_and_window() {
        let mut interp = Interpreter::new().expect("fixture interpreter bootstrap");
        let context = empty_context();
        let frame = interp.test_frame_for_function(&function(1)).unwrap();
        let (parked, cold) = park_regular_async_frame(&mut interp, &context, frame);

        assert_eq!(interp.cold_frames.live_len(), 0);

        let error = interp
            .run_async_resume(&context, parked, cold, 1, true, Value::number_i32(7))
            .unwrap_err();

        assert_invalid_resume_cleanup(&interp, error);
    }

    #[test]
    fn async_generator_resume_invalid_destination_releases_cold_record_and_window() {
        let mut interp = Interpreter::new().expect("fixture interpreter bootstrap");
        let context = empty_context();
        let frame = interp.test_frame_for_function(&function(1)).unwrap();
        let (parked, cold, owner) = park_async_generator_frame(&mut interp, frame);

        assert_eq!(interp.cold_frames.live_len(), 0);

        let error = interp
            .run_async_gen_resume(&context, parked, cold, 1, true, Value::number_i32(7), owner)
            .unwrap_err();

        assert_invalid_resume_cleanup(&interp, error);
    }

    #[test]
    fn floor_unwind_preserves_caller_frame_window_and_thrown_identity() {
        let mut interp = Interpreter::new().expect("fixture interpreter bootstrap");
        let context = empty_context();
        let mut stack = crate::test_support::FrameChainFixture::new();

        let mut caller = interp.test_frame_for_function(&function(1)).unwrap();
        caller.pc = 19;
        caller.registers[0] = Value::number_i32(7);
        stack.push(caller);
        let floor = stack.floor();

        let mut nested = interp.test_frame_for_function(&function(2)).unwrap();
        nested.registers[0] = Value::number_i32(11);
        nested.registers[1] = Value::number_i32(13);
        stack.push(nested);

        let thrown = Value::number_i32(41);
        let error = interp
            .unwind_throw_above(&context, &mut stack, floor, thrown, ThrowSite::Instruction)
            .unwrap_err();

        assert!(matches!(error, VmError::Uncaught));
        assert_eq!(stack.len(), floor.depth());
        assert_eq!(stack.last().expect("caller retained").pc, 19);
        assert_eq!(
            stack.last().expect("caller retained").registers[0],
            Value::number_i32(7)
        );

        assert_eq!(interp.take_pending_uncaught_throw(), Some(thrown));

        let _caller = stack.pop().expect("caller retained");
    }
}
