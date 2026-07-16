//! Async suspension and throw-unwind helpers.
//!
//! Async `await` and exception propagation are cold control-flow paths for the
//! dense interpreter. Keeping them out of `lib.rs` leaves the dispatch loop as
//! the opcode router while preserving the exact stack and microtask semantics.
//!
//! # Contents
//! - `Op::Await` parking for async functions and async generators.
//! - Microtask-driven async function and async-generator resume.
//! - Try/catch/finally throw unwinding and async rejection absorption.
//!
//! # Invariants
//! - Await parking advances the frame PC before removing it from the active
//!   stack.
//! - Rejected awaits re-enter through the same throw-unwind path as
//!   synchronous `throw`.
//! - Async frames absorb unhandled throws by rejecting their result promise.
//!
//! # See also
//! - [`crate::microtask`]
//! - [`crate::promise_dispatch`]

use crate::activation_stack::{ActivationFloor, ActivationStack};
use smallvec::SmallVec;

use crate::promise::JsPromise;
use crate::{
    ExecutionContext, Frame, Interpreter, RunError, Value, VmError, promise_dispatch,
    snapshot_frames,
};

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
        stack: &ActivationStack,
        value: Value,
    ) -> Result<crate::promise::JsPromiseHandle, VmError> {
        if let Some(p) = value.as_promise() {
            return Ok(p);
        }
        let cap = promise_dispatch::PromiseBuilder::with_context(context.clone())
            .capability_stack_rooted(self, stack, &[&value], &[])?;
        let resolve = cap.resolve;
        self.run_callable_sync(
            context,
            &resolve,
            Value::undefined(),
            smallvec::smallvec![value],
        )?;
        Ok(cap
            .promise
            .as_promise()
            .expect("promise capability holds a promise"))
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
    /// 3. Pop the frame off the active stack and box it; share the
    ///    box between the resume / reject closures via an
    ///    `Rc<Cell<Option<_>>>` so whichever reaction fires first
    ///    consumes the parked frame and the other reaction falls
    ///    through as a no-op (matching spec idempotency for
    ///    `then`'s twin reactions).
    /// 4. Build native `resume_fulfill` / `resume_reject` closures
    ///    that enqueue a [`crate::microtask::MicrotaskKind::AsyncResume`]
    ///    microtask when invoked. Attach them with `perform_then` so the
    ///    drain delivers the awaited value into the parked frame's
    ///    `dst` register on resume.
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
    ) -> Result<(), VmError> {
        let top_idx = stack.len() - 1;
        // §27.6 Async-generator body — the running frame has no
        // regular async-function state, but its cold record carries a
        // generator owner whose body was flagged
        // async. Park the frame on a dedicated resume native that
        // re-enters the generator body and either settles the
        // front queued request from a subsequent `Op::Yield` /
        // completion, or chains another `Op::Await`.
        if !self.frame_has_async_state(&stack[top_idx]) {
            if let Some(owner) = self.frame_generator_owner(&stack[top_idx])
                && owner.is_async(&self.gc_heap)
            {
                return self.do_await_async_gen(stack, context, dst, awaited, owner);
            }
            return Err(VmError::InvalidOperand);
        }
        // Advance past the Await before parking so resumption
        // continues at the next instruction.
        stack[top_idx].advance_pc()?;
        let promise = self.await_promise_resolve(context, stack, awaited)?;
        let promise_value = Value::promise(promise);
        let capability = promise_dispatch::PromiseBuilder::with_context(context.clone())
            .capability_stack_rooted(self, stack, &[&promise_value], &[])?;
        let mut parked = stack.pop().expect("top frame existed");
        let detached_cold = self.frame_detach_cold(&mut parked);
        let parked = self.park_active_frame(parked);
        let parked =
            crate::generator::alloc_parked_frame(&mut self.gc_heap, parked, detached_cold)?;
        let outcome = promise.perform_async_resume_then_with_context(
            &mut self.gc_heap,
            parked,
            dst,
            capability,
            None,
            Some(context.clone()),
        );
        if let Some(job) = outcome.immediate_job {
            self.microtasks.enqueue(job);
        }
        Ok(())
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
    ) -> Result<(), VmError> {
        let top_idx = stack.len() - 1;
        stack[top_idx].advance_pc()?;
        let promise = self.await_promise_resolve(context, stack, awaited)?;
        let promise_value = Value::promise(promise);
        let capability = promise_dispatch::PromiseBuilder::with_context(context.clone())
            .capability_stack_rooted(self, stack, &[&promise_value], &[])?;
        let mut parked = stack.pop().expect("top frame existed");
        let detached_cold = self.frame_detach_cold(&mut parked);
        let parked = self.park_active_frame(parked);
        let parked =
            crate::generator::alloc_parked_frame(&mut self.gc_heap, parked, detached_cold)?;
        let outcome = promise.perform_async_resume_then_with_context(
            &mut self.gc_heap,
            parked,
            dst,
            capability,
            Some(owner),
            Some(context.clone()),
        );
        if let Some(job) = outcome.immediate_job {
            self.microtasks.enqueue(job);
        }
        Ok(())
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
        let _window_rollback = self.register_window_rollback();
        let mut frame = self.resume_parked_frame(*frame).map_err(RunError::bare)?;
        if let Some(c) = cold {
            self.frame_attach_cold(&mut frame, c);
        }
        if fulfilled {
            if let Some(slot) = frame.registers.get_mut(await_dst as usize) {
                *slot = value;
            } else {
                return Err(RunError {
                    error: VmError::InvalidOperand,
                    frames: Vec::new(),
                    detail: self.take_error_detail(),
                });
            }
        }
        let mut stack: ActivationStack = ActivationStack::new();
        stack.push(frame);
        owner.set_async_state(
            &mut self.gc_heap,
            crate::generator::AsyncGeneratorState::Executing,
        );
        // Same rooting contract as [`Self::run_async_resume`]: the
        // resumed frame, the settlement value, and the owning
        // generator must be GC roots across the allocating
        // unwind / completion steps that run outside
        // `dispatch_loop`'s own provider scope.
        let anchor_depth = self.push_iteration_anchor(value);
        self.push_iteration_anchor(Value::generator(owner));
        let frame_roots = otter_gc::RawFrameRoots::new(
            &stack as *const ActivationStack,
            &self.cold_frames as *const crate::cold_frame::ColdFramePool,
            crate::ActivationStack::trace_roots,
        );
        let frame_root_provider: &dyn otter_gc::FrameRoots = &frame_roots;
        let frame_roots_guard = self
            .gc_heap
            .register_frame_roots(frame_root_provider as *const dyn otter_gc::FrameRoots);
        let result = (|| -> Result<(), RunError> {
            if !fulfilled {
                if let Err(error) = self.unwind_throw(context, &mut stack, value) {
                    // Unhandled anywhere in the parked gen body —
                    // §27.6.3 AsyncGenerator resumption settles the
                    // front request as rejected instead of letting the
                    // throw escape the dispatch tick.
                    if matches!(error, VmError::Uncaught) {
                        let reason = self.take_pending_uncaught_throw().unwrap_or(value);
                        owner.mark_done(&mut self.gc_heap);
                        self.async_generator_complete_step(context, &owner, Err(reason), true)
                            .map_err(RunError::bare)?;
                        self.async_generator_drain_done(context, &owner)
                            .map_err(RunError::bare)?;
                        return Ok(());
                    }
                    let frames = snapshot_frames(context, &stack);
                    return Err(RunError {
                        error,
                        frames,
                        detail: self.take_error_detail(),
                    });
                }
                if stack.is_empty() {
                    // Throw drained out of the gen body; settle the
                    // front request as rejected.
                    self.async_generator_complete_step(context, &owner, Err(value), true)
                        .map_err(RunError::bare)?;
                    owner.mark_done(&mut self.gc_heap);
                    self.async_generator_drain_done(context, &owner)
                        .map_err(RunError::bare)?;
                    return Ok(());
                }
            }
            match self.dispatch_loop(context, &mut stack) {
                Ok(value) => {
                    let yielded_already = owner.has_yielded(&self.gc_heap);
                    if yielded_already {
                        // Op::Yield already settled the request and
                        // saved the frame back to the gen.
                        owner.take_yielded(&mut self.gc_heap);
                        return Ok(());
                    }
                    // Body completed: settle the front request with
                    // the final return value as `done: true`.
                    self.async_generator_complete_step(context, &owner, Ok(value), true)
                        .map_err(RunError::bare)?;
                    owner.mark_done(&mut self.gc_heap);
                    self.async_generator_drain_done(context, &owner)
                        .map_err(RunError::bare)?;
                    Ok(())
                }
                Err(error) => {
                    owner.mark_done(&mut self.gc_heap);
                    if matches!(error, VmError::MissingReturn) {
                        self.async_generator_drain_done(context, &owner)
                            .map_err(RunError::bare)?;
                        return Ok(());
                    }
                    let rejection = if let Some(thrown) = self.pending_uncaught_throw.take() {
                        Some(thrown)
                    } else {
                        self.vm_error_to_throwable_with_stack_roots(Some(context), &stack, &error)
                    };
                    if let Some(reason) = rejection {
                        self.async_generator_complete_step(context, &owner, Err(reason), true)
                            .map_err(RunError::bare)?;
                        self.async_generator_drain_done(context, &owner)
                            .map_err(RunError::bare)?;
                        Ok(())
                    } else {
                        let frames = snapshot_frames(context, &stack);
                        Err(RunError {
                            error,
                            frames,
                            detail: self.take_error_detail(),
                        })
                    }
                }
            }
        })();
        drop(frame_roots_guard);
        self.pop_iteration_anchors_to(anchor_depth - 1);
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
        let _window_rollback = self.register_window_rollback();
        let mut frame = self.resume_parked_frame(*frame).map_err(RunError::bare)?;
        if let Some(c) = cold {
            self.frame_attach_cold(&mut frame, c);
        }
        if fulfilled {
            if let Some(slot) = frame.registers.get_mut(await_dst as usize) {
                *slot = value;
            } else {
                return Err(RunError {
                    error: VmError::InvalidOperand,
                    frames: Vec::new(),
                    detail: self.take_error_detail(),
                });
            }
        }
        let mut stack: ActivationStack = ActivationStack::new();
        stack.push(frame);
        // The resumed frame and the settlement value must be GC
        // roots *before* `dispatch_loop` registers its own provider:
        // the rejection path below allocates (thrown-value
        // rendering, promise rejection jobs) while the frame only
        // lives on this local stack.
        let anchor_depth = self.push_iteration_anchor(value);
        let frame_roots = otter_gc::RawFrameRoots::new(
            &stack as *const ActivationStack,
            &self.cold_frames as *const crate::cold_frame::ColdFramePool,
            crate::ActivationStack::trace_roots,
        );
        let frame_root_provider: &dyn otter_gc::FrameRoots = &frame_roots;
        let frame_roots_guard = self
            .gc_heap
            .register_frame_roots(frame_root_provider as *const dyn otter_gc::FrameRoots);
        let result = (|| -> Result<(), RunError> {
            if !fulfilled {
                // Inject the rejection as a throw so the parked frame
                // observes it through its `try`/`catch`/`finally`
                // structure exactly as a synchronous throw would.
                if let Err(error) = self.unwind_throw(context, &mut stack, value) {
                    let frames = snapshot_frames(context, &stack);
                    return Err(RunError {
                        error,
                        frames,
                        detail: self.take_error_detail(),
                    });
                }
                if stack.is_empty() {
                    // The rejection drained through the async frame's
                    // result promise — nothing left to dispatch.
                    return Ok(());
                }
            }
            match self.dispatch_loop(context, &mut stack) {
                Ok(_) => Ok(()),
                Err(error) => {
                    let frames = snapshot_frames(context, &stack);
                    Err(RunError {
                        error,
                        frames,
                        detail: self.take_error_detail(),
                    })
                }
            }
        })();
        drop(frame_roots_guard);
        self.pop_iteration_anchors_to(anchor_depth - 1);
        result
    }

    /// Walk the live frame stack looking for a try-handler that
    /// can absorb an in-flight throw.
    ///
    /// # Algorithm
    /// 1. Inspect the top frame:
    ///    - **Catch handler hit** — write the thrown value into
    ///      the handler's `exc_register`, jump pc to the catch
    ///      entry, pop the handler, return `Ok(())` so dispatch
    ///      resumes in that frame.
    ///    - **Finally-only handler hit** — park the value on the
    ///      frame's `parked_finally` stack (tagged with the handler
    ///      depth), jump pc to the finally entry, pop the handler,
    ///      return `Ok(())`. [`otter_bytecode::Op::EndFinally`]
    ///      re-throws unless a later unwind discarded the entry.
    ///    - **No handler in this frame** — if the cold record owns an async
    ///      result promise, settle it
    ///      as rejected, drain the resulting jobs into the
    ///      microtask queue, pop the frame, and stop unwinding.
    ///      The caller is in a different "logical thread" — its pc
    ///      was advanced past the call site at entry and the
    ///      result promise was already in its register.
    ///    - **Otherwise** — pop the frame and continue.
    ///
    /// # Errors
    /// - [`VmError::Uncaught`] when the root execution region empties without
    ///   a handler and no async-frame absorbed the throw.
    pub(crate) fn unwind_throw(
        &mut self,
        context: &ExecutionContext,
        stack: &mut ActivationStack,
        value: Value,
    ) -> Result<(), VmError> {
        self.unwind_throw_above(context, stack, ActivationFloor::ROOT, value)
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
    ) -> Result<(), VmError> {
        self.unwind_throw_with_uncaught_above(context, stack, floor, value, None)
    }

    /// Structured-error variant of [`Self::unwind_throw_above`].
    ///
    /// `uncaught_error` replaces [`VmError::Uncaught`] only after every frame
    /// above `floor` has been unwound. As on the root path, a structured error
    /// does not publish `value` through `pending_uncaught_throw`.
    pub(crate) fn unwind_throw_with_uncaught_above(
        &mut self,
        context: &ExecutionContext,
        stack: &mut ActivationStack,
        floor: ActivationFloor,
        value: Value,
        mut uncaught_error: Option<VmError>,
    ) -> Result<(), VmError> {
        if floor.depth() > stack.len() {
            return Err(VmError::InvalidOperand);
        }

        let display = self.render_thrown(&value);
        let payload = value;
        loop {
            if stack.is_at_floor(floor) {
                if uncaught_error.is_none() {
                    self.pending_uncaught_throw = Some(payload);
                }
                return Err(uncaught_error
                    .take()
                    .unwrap_or(self.err_uncaught((display).into())));
            }
            let popped_handler = {
                let frame = stack.last_mut().expect("frame present");
                self.frame_cold_mut(frame).and_then(|c| c.handlers.pop())
            };
            let Some(handler) = popped_handler else {
                // No in-frame try-handler: this frame's entire body is
                // exited, so §7.4.9 IteratorClose runs for every
                // iterator it left open (floor `-1` takes all depths)
                // before the frame is discarded.
                let closers = self.take_frame_closers_above(stack.last_mut().expect("frame"), -1);
                self.close_unwind_iterators(context, closers);
                // Async frames absorb their own unhandled throws into the
                // result promise as a rejection — spec §27.7.5.3
                // step 1.h.iii.
                if self.frame_has_async_state(stack.last().expect("frame still present")) {
                    let mut popped = stack.pop().expect("frame existed at last");
                    let result_promise = self
                        .frame_take_async_state(&mut popped)
                        .expect("async ownership checked just above")
                        .result_promise;
                    self.frame_release_cold(&mut popped);
                    self.reclaim_registers(&mut popped);
                    let jobs = result_promise.reject(&mut self.gc_heap, payload);
                    self.note_settle_rejection(&jobs);
                    for j in jobs.jobs {
                        self.microtasks.enqueue(j);
                    }
                    return Ok(());
                }
                let mut popped = stack.pop().expect("frame still present");
                self.frame_release_cold(&mut popped);
                self.reclaim_registers(&mut popped);
                continue;
            };
            // Landing in this frame's catch / finally: §7.4.9 closes the
            // iterators whose region sits inside the handler just popped
            // (registered at a deeper handler depth than the remaining
            // floor). An iterator opened *outside* the matched handler —
            // e.g. a `try`/`catch` nested inside the loop body — stays
            // open so iteration can resume.
            let handler_floor = self
                .frame_cold(stack.last().expect("frame"))
                .map_or(0, |c| c.handlers.len() as i64);
            // §14.15.3 — completions parked by `finally` blocks this
            // throw abandons (their handler depth sits above the
            // landing handler) are replaced by the new throw.
            if let Some(cold) = self.frame_cold_mut(stack.last_mut().expect("frame")) {
                cold.parked_finally
                    .retain(|(_, depth)| i64::from(*depth) <= handler_floor);
            }
            let closers =
                self.take_frame_closers_above(stack.last_mut().expect("frame"), handler_floor);
            self.close_unwind_iterators(context, closers);
            let frame = stack.last_mut().expect("frame still present");
            if let Some(catch_pc) = handler.catch_pc {
                frame.pc = catch_pc;
                let slot = frame
                    .registers
                    .get_mut(handler.exc_register as usize)
                    .ok_or(VmError::InvalidOperand)?;
                *slot = payload;
                return Ok(());
            }
            let finally_pc = handler.finally_pc.ok_or(VmError::InvalidOperand)?;
            frame.pc = finally_pc;
            let cold = self.frame_ensure_cold(frame);
            let depth = cold.handlers.len() as u32;
            cold.parked_finally
                .push((crate::cold_frame::ParkedFinally::Throw(payload), depth));
            return Ok(());
        }
    }

    /// Drain the top frame's iterator closers whose recorded handler
    /// depth is strictly greater than `floor`, returning them
    /// innermost-first (last registered runs first). Used by
    /// [`Self::unwind_throw_with_uncaught`] to decide which §7.4.9
    /// IteratorClose hooks the in-flight throw crosses.
    fn take_frame_closers_above(&mut self, frame: &mut Frame, floor: i64) -> SmallVec<[Value; 2]> {
        let mut out: SmallVec<[Value; 2]> = SmallVec::new();
        if let Some(cold) = self.frame_cold_mut(frame) {
            let mut i = 0;
            while i < cold.active_iterator_closers.len() {
                if i64::from(cold.active_iterator_closers[i].1) > floor {
                    out.push(cold.active_iterator_closers.remove(i).0);
                } else {
                    i += 1;
                }
            }
        }
        out.reverse();
        out
    }

    /// Run `[[return]]` on each iterator crossed by an in-flight throw.
    /// A secondary throw from `return` is swallowed — §7.4.9 keeps the
    /// original (throw) completion.
    fn close_unwind_iterators(
        &mut self,
        context: &ExecutionContext,
        closers: SmallVec<[Value; 2]>,
    ) {
        for iterator in closers {
            let _ = self.iterator_close_value_sync(context, iterator);
        }
    }

    /// Drop `iterator` from the top frame's §7.4.9 closer registry.
    /// Called when the iterator becomes `[[Done]]` — its `next`
    /// returned `done: true`, or an explicit `Op::IteratorClose`
    /// already ran — so a later throw-unwind does not invoke
    /// `[[return]]` a second time (IteratorClose is a no-op on a done
    /// iterator).
    pub(crate) fn deregister_frame_iterator_closer(&mut self, frame: &mut Frame, iterator: Value) {
        if let Some(cold) = self.frame_cold_mut(frame)
            && let Some(pos) = cold
                .active_iterator_closers
                .iter()
                .rposition(|(v, _)| *v == iterator)
        {
            cold.active_iterator_closers.remove(pos);
        }
    }

    /// Re-arm `iterator` in the top frame's §7.4.9 closer registry at
    /// the current handler depth. Called after a user `next` returns
    /// `done: false` (the closer was dropped for the span of the call
    /// so a throwing `next` would not trigger IteratorClose). A no-op
    /// when the iterator is already registered.
    pub(crate) fn register_frame_iterator_closer(&mut self, frame: &mut Frame, iterator: Value) {
        let depth = self
            .frame_cold(frame)
            .map_or(0, |c| c.handlers.len() as u32);
        let cold = self.frame_ensure_cold(frame);
        if !cold
            .active_iterator_closers
            .iter()
            .any(|(v, _)| *v == iterator)
        {
            cold.active_iterator_closers.push((iterator, depth));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use otter_bytecode::{BytecodeModule, Function, SourceKind};

    fn empty_context() -> ExecutionContext {
        ExecutionContext::from_module(BytecodeModule {
            module: "async-ops-test.js".to_string(),
            template_sites: Vec::new(),
            source_kind: SourceKind::JavaScript,
            functions: Vec::new(),
            constants: Vec::new(),
            module_resolutions: Vec::new(),
            module_inits: Vec::new(),
        })
    }

    fn function(registers: u16) -> Function {
        Function {
            locals: registers,
            ..Function::default()
        }
    }

    #[test]
    fn floor_unwind_preserves_caller_frame_window_and_thrown_identity() {
        let mut interp = Interpreter::new();
        let context = empty_context();
        let mut stack = ActivationStack::new();

        let mut caller = interp.test_frame_for_function(&function(1)).unwrap();
        caller.pc = 19;
        caller.registers[0] = Value::number_i32(7);
        stack.push(caller);
        let floor = stack.floor();

        let mut nested = interp.test_frame_for_function(&function(2)).unwrap();
        nested.registers[0] = Value::number_i32(11);
        nested.registers[1] = Value::number_i32(13);
        stack.push(nested);
        assert_eq!(interp.register_stack.checkpoint(), 3);

        let thrown = Value::number_i32(41);
        let error = interp
            .unwind_throw_above(&context, &mut stack, floor, thrown)
            .unwrap_err();

        assert!(matches!(error, VmError::Uncaught));
        assert_eq!(stack.len(), floor.depth());
        assert_eq!(stack.last().expect("caller retained").pc, 19);
        assert_eq!(
            stack.last().expect("caller retained").registers[0],
            Value::number_i32(7)
        );
        assert_eq!(interp.register_stack.checkpoint(), 1);
        assert_eq!(interp.take_pending_uncaught_throw(), Some(thrown));

        let mut caller = stack.pop().expect("caller retained");
        interp.reclaim_registers(&mut caller);
        assert_eq!(interp.register_stack.checkpoint(), 0);
    }
}
