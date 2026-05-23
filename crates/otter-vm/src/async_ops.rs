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

use smallvec::SmallVec;

use crate::promise::JsPromise;
use crate::{
    ExecutionContext, Frame, Interpreter, RunError, Value, VmError, promise_dispatch,
    render_thrown_value, snapshot_frames,
};

impl Interpreter {
    /// Handle [`otter_bytecode::Op::Await`]: park the current
    /// async frame off the active stack and attach resume / reject
    /// reactions to the awaited promise.
    ///
    /// # Algorithm
    /// 1. Wrap a non-promise value with `Promise.resolve(v)` per
    ///    spec §27.7.5.3 step 1.b (an `Await` of a non-thenable
    ///    settles immediately on the next microtask tick).
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
    /// - The frame at the top of `stack` MUST be an async frame
    ///   (its `async_state.is_some()`); the compiler enforces
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
        stack: &mut SmallVec<[Frame; 8]>,
        context: &ExecutionContext,
        dst: u16,
        awaited: Value,
    ) -> Result<(), VmError> {
        let top_idx = stack.len() - 1;
        // §27.6 Async-generator body — the running frame has no
        // `async_state` (it isn't a regular async-function frame),
        // but it carries a `generator_owner` whose body was flagged
        // async. Park the frame on a dedicated resume native that
        // re-enters the generator body and either settles the
        // outer `pending_request` from a subsequent `Op::Yield` /
        // completion, or chains another `Op::Await`.
        if stack[top_idx].async_state.is_none() {
            if let Some(owner) = stack[top_idx].generator_owner
                && owner.is_async(&self.gc_heap)
            {
                return self.do_await_async_gen(stack, context, dst, awaited, owner);
            }
            return Err(VmError::InvalidOperand);
        }
        // Advance past the Await before parking so resumption
        // continues at the next instruction.
        stack[top_idx].pc = stack[top_idx]
            .pc
            .checked_add(1)
            .ok_or(VmError::InvalidOperand)?;
        let promise =
            if let Some(p) = awaited.as_promise() {
                p
            } else {
                promise_dispatch::PromiseBuilder::with_context(context.clone())
                    .fulfilled_stack_rooted(self, stack, awaited, &[], &[])?
            };
        let promise_value = Value::promise(promise);
        let capability = promise_dispatch::PromiseBuilder::with_context(context.clone())
            .capability_stack_rooted(self, stack, &[&promise_value], &[])?;
        let parked = stack.pop().expect("top frame existed");
        let parked = crate::generator::alloc_parked_frame(&mut self.gc_heap, parked)?;
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
    /// resume, the generator's `pending_request` is settled by a
    /// subsequent `Op::Yield`, completion, or further `Op::Await`.
    fn do_await_async_gen(
        &mut self,
        stack: &mut SmallVec<[Frame; 8]>,
        context: &ExecutionContext,
        dst: u16,
        awaited: Value,
        owner: crate::generator::JsGenerator,
    ) -> Result<(), VmError> {
        let top_idx = stack.len() - 1;
        stack[top_idx].pc = stack[top_idx]
            .pc
            .checked_add(1)
            .ok_or(VmError::InvalidOperand)?;
        let promise =
            if let Some(p) = awaited.as_promise() {
                p
            } else {
                promise_dispatch::PromiseBuilder::with_context(context.clone())
                    .fulfilled_stack_rooted(self, stack, awaited, &[], &[])?
            };
        let promise_value = Value::promise(promise);
        let capability = promise_dispatch::PromiseBuilder::with_context(context.clone())
            .capability_stack_rooted(self, stack, &[&promise_value], &[])?;
        let parked = stack.pop().expect("top frame existed");
        let parked = crate::generator::alloc_parked_frame(&mut self.gc_heap, parked)?;
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
    /// generator's `pending_request` on completion / unhandled
    /// throw rather than the frame's `async_state` promise.
    // `Box<Frame>` is intentional: the parked frame travels heap-owned
    // through the microtask queue. Inlining it would require copying
    // the whole frame on every async-resume dispatch tick.
    #[allow(clippy::boxed_local)]
    pub(crate) fn run_async_gen_resume(
        &mut self,
        context: &ExecutionContext,
        mut frame: Box<Frame>,
        await_dst: u16,
        fulfilled: bool,
        value: Value,
        owner: crate::generator::JsGenerator,
    ) -> Result<(), RunError> {
        if fulfilled {
            if let Some(slot) = frame.registers.get_mut(await_dst as usize) {
                *slot = value;
            } else {
                return Err(RunError {
                    error: VmError::InvalidOperand,
                    frames: Vec::new(),
                });
            }
        }
        let mut stack: SmallVec<[Frame; 8]> = SmallVec::new();
        stack.push(*frame);
        if !fulfilled {
            if let Err(error) = self.unwind_throw(&mut stack, value) {
                let frames = snapshot_frames(context, &stack);
                return Err(RunError { error, frames });
            }
            if stack.is_empty() {
                // Throw drained out of the gen body; settle the
                // pending request as rejected.
                let req = owner.take_pending_request(&mut self.gc_heap);
                if let Some(req) = req {
                    let request_context = req.context.clone().unwrap_or_else(|| context.clone());
                    if let Err(error) = self.run_callable_sync(
                        &request_context,
                        &req.reject,
                        Value::undefined(),
                        smallvec::smallvec![value],
                    ) {
                        return Err(RunError {
                            error,
                            frames: Vec::new(),
                        });
                    }
                }
                owner.mark_done(&mut self.gc_heap);
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
                // Body completed: settle the pending request with
                // the final return value as `done: true`.
                let req = owner.take_pending_request(&mut self.gc_heap);
                if let Some(req) = req {
                    let record = self
                        .make_runtime_rooted_iter_result(value, true, &[&req.resolve], &[])
                        .map_err(RunError::bare)?;
                    let request_context = req.context.clone().unwrap_or_else(|| context.clone());
                    if let Err(error) = self.run_callable_sync(
                        &request_context,
                        &req.resolve,
                        Value::undefined(),
                        smallvec::smallvec![record],
                    ) {
                        return Err(RunError {
                            error,
                            frames: Vec::new(),
                        });
                    }
                }
                owner.mark_done(&mut self.gc_heap);
                Ok(())
            }
            Err(error) => {
                let frames = snapshot_frames(context, &stack);
                Err(RunError { error, frames })
            }
        }
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
    // Box<Frame>: parked frame travels heap-owned through the
    // microtask queue; inlining would copy the whole frame on every
    // tick.
    #[allow(clippy::boxed_local)]
    pub(crate) fn run_async_resume(
        &mut self,
        context: &ExecutionContext,
        mut frame: Box<Frame>,
        await_dst: u16,
        fulfilled: bool,
        value: Value,
    ) -> Result<(), RunError> {
        if fulfilled {
            if let Some(slot) = frame.registers.get_mut(await_dst as usize) {
                *slot = value;
            } else {
                return Err(RunError {
                    error: VmError::InvalidOperand,
                    frames: Vec::new(),
                });
            }
        }
        let mut stack: SmallVec<[Frame; 8]> = SmallVec::new();
        stack.push(*frame);
        if !fulfilled {
            // Inject the rejection as a throw so the parked frame
            // observes it through its `try`/`catch`/`finally`
            // structure exactly as a synchronous throw would.
            if let Err(error) = self.unwind_throw(&mut stack, value) {
                let frames = snapshot_frames(context, &stack);
                return Err(RunError { error, frames });
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
                Err(RunError { error, frames })
            }
        }
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
    ///    - **Finally-only handler hit** — park the value on
    ///      `frame.pending_throw`, jump pc to the finally entry,
    ///      pop the handler, return `Ok(())`.
    ///      [`otter_bytecode::Op::EndFinally`] re-throws.
    ///    - **No handler in this frame** — if the frame is async
    ///      (`async_state.is_some()`), settle its result promise
    ///      as rejected, drain the resulting jobs into the
    ///      microtask queue, pop the frame, and stop unwinding.
    ///      The caller is in a different "logical thread" — its pc
    ///      was advanced past the call site at entry and the
    ///      result promise was already in its register.
    ///    - **Otherwise** — pop the frame and continue.
    ///
    /// # Errors
    /// - [`VmError::Uncaught`] when the frame stack empties without
    ///   a handler and no async-frame absorbed the throw.
    pub(crate) fn unwind_throw(
        &mut self,
        stack: &mut SmallVec<[Frame; 8]>,
        value: Value,
    ) -> Result<(), VmError> {
        self.unwind_throw_with_uncaught(stack, value, None)
    }

    /// Same as [`Self::unwind_throw`], but returns
    /// `uncaught_error` if the frame stack empties without a
    /// handler. Heap-cap failures use this path so script code can
    /// catch a real `RangeError`, while embedders still receive
    /// structured [`VmError::OutOfMemory`] when the error is
    /// unhandled.
    pub(crate) fn unwind_throw_with_uncaught(
        &mut self,
        stack: &mut SmallVec<[Frame; 8]>,
        value: Value,
        mut uncaught_error: Option<VmError>,
    ) -> Result<(), VmError> {
        let display = render_thrown_value(&value, &self.gc_heap);
        let payload = value;
        loop {
            let Some(frame) = stack.last_mut() else {
                if uncaught_error.is_none() {
                    self.pending_uncaught_throw = Some(payload);
                }
                return Err(uncaught_error
                    .take()
                    .unwrap_or(VmError::Uncaught { value: display }));
            };
            let popped_handler = self
                .frame_cold_mut(frame)
                .and_then(|c| c.handlers.pop());
            // Re-borrow `frame` after the helper's `&mut self` borrow.
            let frame = stack.last_mut().expect("frame still present");
            let Some(handler) = popped_handler else {
                // No in-frame try-handler. Async frames absorb
                // their own unhandled throws into the result
                // promise as a rejection — synthesised in spec
                // §27.7.5.3 step 1.h.iii.
                if frame.async_state.is_some() {
                    let popped = stack.pop().expect("frame existed at last_mut");
                    let result_promise = popped
                        .async_state
                        .expect("async_state checked just above")
                        .result_promise;
                    let jobs = result_promise.reject(&mut self.gc_heap, payload);
                    for j in jobs.jobs {
                        self.microtasks.enqueue(j);
                    }
                    return Ok(());
                }
                stack.pop();
                continue;
            };
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
            self.frame_ensure_cold(frame).pending_throw = Some(payload);
            return Ok(());
        }
    }
}
