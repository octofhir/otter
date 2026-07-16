//! The bytecode dispatch loop.
//!
//! # Contents
//! `dispatch_loop_inner`: one `match` arm per opcode, inline caches,
//! the JIT tier-up/backedge hooks, and additive optimizing-tier back-edge
//! accounting. Deliberately a single function — splitting it would defeat the
//! dispatch-locality the interpreter depends on.
#![allow(unused_imports)]
use crate::*;

impl Interpreter {
    pub(crate) fn dispatch_loop_inner(
        &mut self,
        entry_context: &ExecutionContext,
        stack: &mut HoltStack,
    ) -> Result<Value, VmError> {
        // One stack can interleave frames from several code chunks
        // (closures escaped from `eval` / `new Function` / sibling
        // scripts), so each iteration dispatches against the chunk
        // owning the *top frame*: constants, atoms, and module
        // resolutions are chunk-local. The owned slot caches the last
        // foreign chunk so repeated foreign-frame ticks don't re-lock
        // the code-space registry.
        let mut foreign_context: Option<ExecutionContext> = None;
        // Hoisted once per turn: the budget config does not change mid-turn,
        // so the per-op checkpoint only needs to run when enforcement is on.
        // In the default Observe mode this collapses to a not-taken branch.
        let enforce_budget = self.runtime_budget.rejects_on_exceedance();
        // Like `enforce_budget`, these installation states are fixed for the
        // duration of a dispatch run — a JIT hook, CPU profiler, or step tracer
        // is attached between turns, never mid-loop. Hoisting the `is_some`
        // probes into loop-invariant bools turns three per-instruction
        // `self`-field loads into register-resident branches that collapse to
        // not-taken in the common (no hook / no profiler / no tracer) case.
        let jit_installed = self.jit_hook.is_some();
        let has_profiler = self.cpu_profiler.is_some();
        let has_tracer = self.tracer.is_some();
        // Per-frame dispatch cache. The owning chunk context, the executable
        // function body, and the dense instruction index are invariants of the
        // top frame — they change only when the frame does (call / return /
        // tail-call / unwind). The previous design re-derived all three on every
        // instruction: chunk resolution, an `exec_function` table lookup, and a
        // `byte_pc` → index map probe. This caches them keyed on `(function_id,
        // depth)`, which together pin the exact live frame: a straight-line tick
        // reuses the context + function pointer. `frame.pc` is already the
        // dense instruction index, so straight-line execution and branches both
        // fetch directly from the CodeBlock without a byte-PC lookup.
        //
        // SAFETY: `function` is a raw pointer into the chunk's `Arc`-owned
        // executable. Compiled code is never GC-managed and never moves, and the
        // owning context (`entry_context`, a borrow that outlives the loop, or
        // `foreign_context`, kept live below) stays alive while `function_id` is
        // unchanged — so the pointer is valid for every reuse.
        // Held as flat register-resident locals rather than an `Option<struct>`;
        // the chunk selection and function pointer are touched only on a miss.
        // `cache_function` is null until the
        // first resolution and is dereferenced solely on the hit path, which the
        // `(function_id, depth)` guard gates.
        let mut cache_valid = false;
        let mut cache_function_id: u32 = u32::MAX;
        let mut cache_depth: usize = 0;
        let mut cache_foreign = false;
        let mut cache_function: *const CodeBlock = std::ptr::null();
        loop {
            if self.interrupt.is_set() {
                return Err(VmError::Interrupted);
            }
            if stack.is_empty() {
                // Defensive: unwind paths (throw / finally) can
                // pop the last frame without writing back to a
                // caller register. Surface `undefined` so
                // the dispatch loop terminates cleanly instead of
                // panicking on the next `stack.len() - 1`. Tests
                // that rely on the throw escape will already have
                // flowed through `unwind_throw` and surfaced as
                // `VmError::Uncaught`; this guard catches the
                // residual "fell off the bottom" path and treats
                // it as completion.
                return Ok(Value::undefined());
            }
            let depth = stack.len();
            let top_idx = depth - 1;
            // SAFETY: the `is_empty()` guard above proves a live top frame this
            // tick; one unchecked read serves both fields.
            let (function_id, pc) = {
                let top = unsafe { stack.top_unchecked() };
                (top.function_id, top.pc)
            };
            // Reuse the cached frame state when the top frame is the same one as
            // the previous tick (same id *and* depth pin the exact live frame —
            // tail-call keeps the depth but swaps the id, recursion keeps the id
            // but changes the depth, so both guards are needed). The instruction
            // index is canonical and needs no coordinate conversion.
            let (context, function, idx): (&ExecutionContext, &CodeBlock, usize) =
                if cache_valid && cache_function_id == function_id && cache_depth == depth {
                    let context: &ExecutionContext = if cache_foreign {
                        foreign_context
                            .as_ref()
                            .ok_or_else(|| VmError::InvalidOperand)?
                    } else {
                        entry_context
                    };
                    // SAFETY: the pointer addresses never-moving compiled code in
                    // a still-live chunk context (see the cache comment); the
                    // `(function_id, depth)` guard proves it was filled.
                    let function: &CodeBlock = unsafe { &*cache_function };
                    let idx = usize::try_from(pc).map_err(|_| VmError::MissingReturn)?;
                    (context, function, idx)
                } else {
                    let mut foreign = false;
                    let context: &ExecutionContext = if entry_context.covers_function(function_id) {
                        entry_context
                    } else {
                        foreign = true;
                        let cached_covers = foreign_context
                            .as_ref()
                            .is_some_and(|c| c.covers_function(function_id));
                        if !cached_covers {
                            foreign_context = match entry_context.for_function(function_id) {
                                Some(code_space::ResolvedCtx::Owned(owned)) => {
                                    // Foreign chunks linked after this loop
                                    // started (eval during this turn) carry
                                    // IC sites past the entry chunk's range.
                                    self.ensure_property_ic_capacity(&owned);
                                    Some(owned)
                                }
                                _ => None,
                            };
                        }
                        foreign_context
                            .as_ref()
                            .ok_or_else(|| VmError::InvalidOperand)?
                    };
                    let function = context
                        .exec_function(function_id)
                        .ok_or_else(|| VmError::InvalidOperand)?;
                    let idx = usize::try_from(pc).map_err(|_| VmError::MissingReturn)?;
                    // The frame depth changes only across a frame transition,
                    // which is exactly what misses the cache (push deepens,
                    // return shallows, and the deepest frame is always freshly
                    // pushed → a miss). Sampling the max here instead of on every
                    // instruction keeps `maxStackDepthObserved` exact while taking
                    // the comparison off the straight-line hot path.
                    let depth32 = u32::try_from(depth).unwrap_or(u32::MAX);
                    if depth32 > self.runtime_budget_stats.max_stack_depth_observed {
                        self.runtime_budget_stats.max_stack_depth_observed = depth32;
                    }
                    // Refresh the miss-only fields of the cache.
                    cache_valid = true;
                    cache_function_id = function_id;
                    cache_depth = depth;
                    cache_foreign = foreign;
                    cache_function = function as *const CodeBlock;
                    (context, function, idx)
                };
            let instr = function.instr_at_index(idx).ok_or(VmError::MissingReturn)?;
            let op = function.op(instr);
            // Feedback is dense in the owning CodeBlock. Interpreter-only
            // execution keeps the cell untouched and pays no atomic update.
            let feedback = if jit_installed {
                function.feedback_recorder_at(idx)
            } else {
                None
            };
            // Per-instruction reduction metering. Reductions accumulate exactly
            // as before; the max-stack-depth sample moved to the frame-resolution
            // miss branch (depth changes only across frame transitions), and the
            // budget checkpoint below stays gated on `enforce_budget` (a not-taken
            // branch in the default Observe mode).
            self.runtime_budget_stats
                .record_reductions(runtime_budget::opcode_reductions(op));
            if enforce_budget {
                self.enforce_runtime_budget_checkpoint()?;
            }

            if has_profiler && let Some(profiler) = self.cpu_profiler.as_mut() {
                profiler.maybe_sample(context, stack);
            }

            // Step-trace hook. The hot path checks one loop-invariant bool
            // per instruction; the body only runs when an embedder installed
            // a tracer through `Interpreter::set_tracer`.
            if has_tracer {
                let function_name = context
                    .function(function_id)
                    .map(|f| f.name.as_str())
                    .unwrap_or("<unknown>");
                let register_window: &[Value] = &stack[top_idx].registers;
                let event = inspect::StepEvent {
                    frame_depth: stack.len(),
                    function_id,
                    function_name,
                    byte_pc: function
                        .instruction_byte_pc(idx)
                        .ok_or(VmError::MissingReturn)?,
                    op,
                    operands: function.operand_view(instr),
                    register_window,
                };
                if let Some(tracer) = self.tracer.as_deref_mut() {
                    tracer.on_step(&event);
                }
            }

            // Stack-modifying opcodes go first so we don't hold a
            // `&mut Frame` borrow while pushing / popping.
            match op {
                Op::ReturnValue | Op::Return => {
                    let src = register_operand(function.operand(instr, 0))?;
                    let value = stack[top_idx]
                        .registers
                        .get(src as usize)
                        .cloned()
                        .ok_or_else(|| VmError::InvalidOperand)?;
                    if let Some(popped) = self.return_running_finally(stack, value)? {
                        return Ok(popped);
                    }
                    continue;
                }
                Op::ReturnUndefined => {
                    if let Some(popped) = self.return_running_finally(stack, Value::undefined())? {
                        return Ok(popped);
                    }
                    continue;
                }
                Op::Call => {
                    let operands = function.operand_view(instr);
                    let depth_before = stack.len();
                    self.do_call(stack, context, operands)?;
                    // Tier-up hook: only when a bytecode callee frame was just
                    // pushed and a JIT is installed. Cheap (one bool) when off.
                    if jit_installed && stack.len() > depth_before {
                        // Record the observed callee for leaf-inlining feedback
                        // before the tier-up hook may consume the freshly pushed
                        // frame.
                        let callee_fid = stack[stack.len() - 1].function_id;
                        let transition = self.record_ordinary_call_feedback(
                            function,
                            instr.instruction_pc,
                            callee_fid,
                        );
                        if transition.evict_for_reopt() {
                            self.evict_compiled_for_reopt(function_id);
                        }
                        if let Some(Some(value)) = self.maybe_dispatch_jit(stack, context)? {
                            return Ok(value);
                        }
                    }
                    continue;
                }
                Op::TailCall => {
                    let operands = function.operand_view(instr);
                    self.do_tail_call(stack, context, operands)?;
                    continue;
                }
                Op::CallWithThis => {
                    let operands = function.operand_view(instr);
                    self.do_call_with_this(stack, context, operands)?;
                    continue;
                }
                Op::CallMethodValue => {
                    let operands = function.operand_view(instr);
                    let depth_before = stack.len();
                    let feedback_site = instr.property_ic_site();
                    // Capture the receiver/prototype layout before the call for
                    // method-inline feedback (the receiver register lives in the
                    // caller frame, which `do_call_method_value` leaves in place
                    // under the new callee frame; the receiver handle may move
                    // during the call, so the prototype shape and method slot are
                    // resolved here while it is still valid).
                    let method_site = if jit_installed
                        && feedback_site
                            .is_some_and(|site| !self.method_site_feedback_saturated(site))
                    {
                        register_operand(function.operand(instr, 1))
                            .ok()
                            .and_then(|r| {
                                stack
                                    .get(top_idx)
                                    .and_then(|f| f.registers.get(r as usize).copied())
                            })
                            .and_then(|recv| {
                                const_operand(function.operand(instr, 2)).ok().and_then(
                                    |name_idx| {
                                        self.method_site_for_receiver(
                                            context,
                                            function_id,
                                            name_idx,
                                            recv,
                                        )
                                    },
                                )
                            })
                    } else {
                        None
                    };
                    self.do_call_method_value(stack, context, operands)?;
                    // Tier-up hook, mirroring `Op::Call`: a bytecode method
                    // callee pushed via `invoke` lands as a fresh pc==0 frame.
                    if jit_installed && stack.len() > depth_before {
                        let method_fid = stack[stack.len() - 1].function_id;
                        if let (Some(feedback_site), Some(site)) = (feedback_site, method_site) {
                            self.note_method_target(feedback_site, method_fid, site);
                        }
                        if let Some(Some(value)) = self.maybe_dispatch_jit(stack, context)? {
                            return Ok(value);
                        }
                    }
                    continue;
                }
                Op::CallSpread => {
                    let operands = function.operand_view(instr);
                    self.do_call_spread(stack, context, operands)?;
                    continue;
                }
                Op::New => {
                    let operands = function.operand_view(instr);
                    let depth_before = stack.len();
                    self.do_construct(stack, context, operands)?;
                    // Tier-up hook, mirroring `Op::Call`: a bytecode
                    // constructor frame pushed by `new` can enter JIT at pc=0.
                    if jit_installed
                        && stack.len() > depth_before
                        && let Some(Some(value)) = self.maybe_dispatch_jit(stack, context)?
                    {
                        return Ok(value);
                    }
                    continue;
                }
                Op::NewSpread => {
                    let operands = function.operand_view(instr);
                    self.do_construct_spread(stack, context, operands)?;
                    continue;
                }
                Op::SuperConstructSpread => {
                    let operands = function.operand_view(instr);
                    self.do_super_construct_spread(stack, context, operands)?;
                    continue;
                }
                Op::BindThisValue => {
                    let src = register_operand(function.operand(instr, 0))?;
                    self.run_bind_this_value_reg(stack, top_idx, src)?;
                    continue;
                }
                Op::Throw => {
                    let src = register_operand(function.operand(instr, 0))?;
                    let value = stack[top_idx]
                        .registers
                        .get(src as usize)
                        .cloned()
                        .ok_or_else(|| VmError::InvalidOperand)?;
                    // Capture frames at the originating throw site
                    // before `unwind_throw` pops handler-less
                    // frames. If a catch absorbs the throw the
                    // unwind path clears `pending_uncaught_frames`
                    // through [`Self::clear_pending_uncaught_frames`].
                    self.pending_uncaught_frames = Some(snapshot_frames(context, stack));
                    let unwind = self.unwind_throw(context, stack, value);
                    if unwind.is_ok() {
                        self.pending_uncaught_frames = None;
                    } else {
                        // No handler in this dispatch stack — stash
                        // the thrown VALUE so outer loops / native
                        // boundaries keep identity instead of the
                        // rendered string.
                        self.pending_uncaught_throw = Some(value);
                    }
                    unwind?;
                    continue;
                }
                Op::EndFinally => {
                    let parked = self
                        .frame_cold_mut(&mut stack[top_idx])
                        .and_then(|c| c.parked_finally.pop());
                    match parked {
                        Some((crate::cold_frame::ParkedFinally::Throw(value), _)) => {
                            self.pending_uncaught_frames = Some(snapshot_frames(context, stack));
                            let unwind = self.unwind_throw(context, stack, value);
                            if unwind.is_ok() {
                                self.pending_uncaught_frames = None;
                            } else {
                                self.pending_uncaught_throw = Some(value);
                            }
                            unwind?;
                        }
                        Some((crate::cold_frame::ParkedFinally::Abrupt(completion, floor), _)) => {
                            // Resume the parked `return`/`break`/`continue`:
                            // run the next enclosing `finally`, or perform
                            // the completion when none remain.
                            if let Some(popped) = self.unwind_abrupt(stack, completion, floor)? {
                                return Ok(popped);
                            }
                        }
                        Some((crate::cold_frame::ParkedFinally::Normal, _)) | None => {
                            stack[top_idx].advance_pc()?;
                        }
                    }
                    continue;
                }
                Op::Await => {
                    let dst = register_operand(function.operand(instr, 0))?;
                    let src = register_operand(function.operand(instr, 1))?;
                    let awaited = *read_register(&stack[top_idx], src)?;
                    self.do_await(stack, context, dst, awaited)?;
                    if stack.is_empty() {
                        return Ok(Value::undefined());
                    }
                    continue;
                }
                // §27.5 generator suspension. Yield reads the value
                // operand, advances pc past itself, pops the frame
                // off the active stack, stashes it back onto the
                // owning [`crate::generator::JsGenerator`], records
                // the dst register so a future `.next(arg)` can
                // deposit `arg` there, and returns control to the
                // resume site (i.e. the enclosing
                // [`Self::resume_generator`] call).
                // <https://tc39.es/ecma262/#sec-yield>
                // §27.5.3.7 `yield*` delegating suspension — parks
                // the frame with the inner iterator result surfaced
                // verbatim; resume delivers (kind, value) into the
                // two destination registers without unwinding.
                Op::YieldDelegate => {
                    let (kind_dst, value_dst, src) = context
                        .exec_register3(instr)
                        .ok_or_else(|| VmError::InvalidOperand)?;
                    let yielded = *read_register(&stack[top_idx], src)?;
                    let owner = self
                        .frame_generator_owner(&stack[top_idx])
                        .ok_or(VmError::TypeMismatch)?;
                    let frame = stack.last_mut().ok_or_else(|| VmError::InvalidOperand)?;
                    frame.advance_pc()?;
                    let mut popped = stack.pop().expect("frame present");
                    let detached_cold = self.frame_detach_cold(&mut popped);
                    let popped = self.park_active_frame(popped);
                    owner.park_after_yield_delegate(
                        &mut self.gc_heap,
                        popped,
                        detached_cold,
                        kind_dst,
                        value_dst,
                        yielded,
                    );
                    return Ok(Value::undefined());
                }
                Op::Yield => {
                    let dst = register_operand(function.operand(instr, 0))?;
                    let src = register_operand(function.operand(instr, 1))?;
                    let yielded = *read_register(&stack[top_idx], src)?;
                    let owner = self
                        .frame_generator_owner(&stack[top_idx])
                        .ok_or(VmError::TypeMismatch)?;
                    let frame = stack.last_mut().ok_or_else(|| VmError::InvalidOperand)?;
                    frame.advance_pc()?;
                    let mut popped = stack.pop().expect("frame present");
                    let detached_cold = self.frame_detach_cold(&mut popped);
                    let popped = self.park_active_frame(popped);
                    owner.park_after_yield(&mut self.gc_heap, popped, detached_cold, dst, yielded);
                    // §27.6 — async-generator yield settles the
                    // outer `.next()` promise immediately with
                    // `{value, done: false}`. Sync generators bubble
                    // the yielded value out so the `resume_generator`
                    // caller can shape it.
                    if owner.is_async(&self.gc_heap) {
                        owner.set_async_state(
                            &mut self.gc_heap,
                            crate::generator::AsyncGeneratorState::SuspendedYield,
                        );
                        self.async_generator_complete_step(context, &owner, Ok(yielded), false)?;
                    }
                    return Ok(yielded);
                }
                Op::GeneratorStart => {
                    let owner = self
                        .frame_generator_owner(&stack[top_idx])
                        .ok_or(VmError::TypeMismatch)?;
                    let frame = stack.last_mut().ok_or_else(|| VmError::InvalidOperand)?;
                    frame.advance_pc()?;
                    let mut popped = stack.pop().expect("frame present");
                    let detached_cold = self.frame_detach_cold(&mut popped);
                    let popped = self.park_active_frame(popped);
                    owner.park_frame(&mut self.gc_heap, popped, detached_cold);
                    return Ok(Value::undefined());
                }
                // §7.1.4 ToNumber — the shared synchronous helper owns the
                // full ToPrimitive(number) ladder before committing `dst`.
                Op::ToNumber => {
                    let dst = function
                        .register(instr, 0)
                        .ok_or_else(|| VmError::InvalidOperand)?;
                    let src = function
                        .register(instr, 1)
                        .ok_or_else(|| VmError::InvalidOperand)?;
                    self.run_to_number_regs(context, stack, top_idx, dst, src)?;
                    continue;
                }
                // §7.1.1 `ToPrimitive` ladder. Each invocation of
                // the dispatch loop either advances pc with a
                // primitive in `dst` or pushes a frame for
                // `[Symbol.toPrimitive]` / `valueOf` / `toString`
                // and parks the ladder state on the running frame.
                // Stack-modifying so it has to happen before the
                // in-frame mutable borrow below. Always re-enters
                // the dispatch loop afterwards — the in-frame
                // match below has no arm for `Op::ToPrimitive`.
                Op::ToPrimitive => {
                    let operands = function.operand_view(instr);
                    // Hot fast path: an already-primitive source (the dominant
                    // case — numeric loop operands) is its own ToPrimitive
                    // result. Skip the hint-token decode and the parked-ladder
                    // resume check; a primitive operand never parks. Reading
                    // `src` (the original operand, not `dst`) keeps the object
                    // resume path — where `src` stays non-primitive — intact.
                    let dst = register_operand(operands.first())?;
                    let src = register_operand(operands.get(1))?;
                    let recv = *read_register(&stack[top_idx], src)?;
                    if abstract_ops::is_primitive(&recv) {
                        write_register(&mut stack[top_idx], dst, recv)?;
                        stack[top_idx].advance_pc()?;
                        continue;
                    }
                    self.drive_to_primitive(stack, context, operands)?;
                    continue;
                }
                // §7.4.3 `GetIterator`. Built-in iterables fall
                // through to the in-frame fast path; user objects
                // route through the call-frame ladder.
                // <https://tc39.es/ecma262/#sec-getiterator>
                Op::GetIterator => {
                    let operands = function.operand_view(instr);
                    if self.drive_get_iterator(stack, context, operands)? {
                        continue;
                    }
                    let dst = function
                        .register(instr, 0)
                        .ok_or_else(|| VmError::InvalidOperand)?;
                    let src = function
                        .register(instr, 1)
                        .ok_or_else(|| VmError::InvalidOperand)?;
                    self.run_get_iterator_regs(&mut *stack, top_idx, dst, src)?;
                    continue;
                }
                Op::GetAsyncIterator => {
                    let dst = function
                        .register(instr, 0)
                        .ok_or_else(|| VmError::InvalidOperand)?;
                    let src = function
                        .register(instr, 1)
                        .ok_or_else(|| VmError::InvalidOperand)?;
                    self.run_get_async_iterator_regs(context, &mut *stack, top_idx, dst, src)?;
                    continue;
                }
                // §7.4.5 `IteratorNext`. Built-in iterators step
                // synchronously; user iterators push a call to
                // `iter.next()` and resume to extract `value` /
                // `done`.
                // <https://tc39.es/ecma262/#sec-iteratornext>
                Op::IteratorNext => {
                    // §7.4.8 IteratorStep — if `next` throws, the
                    // iterator record is set `[[done]]` and IteratorClose
                    // is *not* run for it. Deregister the iterator from
                    // the §7.4.9 closer set before propagating so the
                    // throw-unwind does not invoke `[[return]]`.
                    let iter_reg = context
                        .exec_register(instr, 2)
                        .ok_or_else(|| VmError::InvalidOperand)?;
                    let iterator = *read_register(&stack[top_idx], iter_reg)?;
                    let operands = function.operand_view(instr);
                    match self.drive_iterator_next(stack, context, operands) {
                        Ok(true) => continue,
                        Ok(false) => {}
                        Err(e) => {
                            self.deregister_frame_iterator_closer(&mut stack[top_idx], iterator);
                            return Err(e);
                        }
                    }
                    let value_dst = context
                        .exec_register(instr, 0)
                        .ok_or_else(|| VmError::InvalidOperand)?;
                    let done_dst = context
                        .exec_register(instr, 1)
                        .ok_or_else(|| VmError::InvalidOperand)?;
                    let frame = &mut stack[top_idx];
                    if let Err(e) =
                        self.run_iterator_next_regs(frame, value_dst, done_dst, iter_reg)
                    {
                        self.deregister_frame_iterator_closer(&mut stack[top_idx], iterator);
                        return Err(e);
                    }
                    continue;
                }
                Op::IteratorClose => {
                    let iter_reg = context
                        .exec_register(instr, 0)
                        .ok_or_else(|| VmError::InvalidOperand)?;
                    let iterator = *read_register(&stack[top_idx], iter_reg)?;
                    // §7.4.9 — mark the iterator done *before* running its
                    // `[[return]]`: if `return` throws, the unwind must
                    // not close it again (it is already closing).
                    self.deregister_frame_iterator_closer(&mut stack[top_idx], iterator);
                    self.iterator_close_value_sync(context, iterator)?;
                    stack[top_idx].advance_pc()?;
                    continue;
                }
                Op::IteratorCloseStart => {
                    let iter_reg = context
                        .exec_register(instr, 0)
                        .ok_or_else(|| VmError::InvalidOperand)?;
                    let iterator = *read_register(&stack[top_idx], iter_reg)?;
                    // §7.4.9 — record the handler depth so throw-unwind
                    // can tell whether a catching handler sits inside or
                    // outside this iterator's region.
                    let handler_depth = self
                        .frame_cold(&stack[top_idx])
                        .map_or(0, |c| c.handlers.len() as u32);
                    self.frame_ensure_cold(&mut stack[top_idx])
                        .active_iterator_closers
                        .push((iterator, handler_depth));
                    stack[top_idx].advance_pc()?;
                    continue;
                }
                Op::IteratorCloseEnd => {
                    let iter_reg = context
                        .exec_register(instr, 0)
                        .ok_or_else(|| VmError::InvalidOperand)?;
                    let iterator = *read_register(&stack[top_idx], iter_reg)?;
                    if let Some(cold) = self.frame_cold_mut(&mut stack[top_idx])
                        && let Some(pos) = cold
                            .active_iterator_closers
                            .iter()
                            .rposition(|(value, _)| *value == iterator)
                    {
                        cold.active_iterator_closers.remove(pos);
                    }
                    stack[top_idx].advance_pc()?;
                    continue;
                }
                // §10.1.8 [[Get]] — when the resolved property is an
                // accessor descriptor at any depth in the prototype
                // chain, the runtime invokes the getter with `this`
                // bound to the original receiver. Stack-modifying so
                // it must run outside the in-frame mutable borrow
                // below.
                // <https://tc39.es/ecma262/#sec-ordinaryget>
                Op::LoadProperty => {
                    let operands = function.operand_view(instr);
                    if self.drive_load_property(stack, context, operands)? {
                        continue;
                    }
                    let dst = context
                        .exec_register(instr, 0)
                        .ok_or_else(|| VmError::InvalidOperand)?;
                    let obj_reg = context
                        .exec_register(instr, 1)
                        .ok_or_else(|| VmError::InvalidOperand)?;
                    let name_idx = context
                        .exec_const_index(instr, 2)
                        .ok_or_else(|| VmError::InvalidOperand)?;
                    let key = context
                        .property_atom(name_idx)
                        .ok_or_else(|| VmError::InvalidOperand)?;
                    self.run_load_property_reg(context, &mut *stack, top_idx, dst, obj_reg, key)?;
                    continue;
                }
                Op::LoadElement => {
                    let operands = function.operand_view(instr);
                    if let Some(recv_reg) = context.exec_register(instr, 1)
                        && let Ok(recv) = read_register(&stack[top_idx], recv_reg)
                    {
                        let recv = *recv;
                        let observed = match recv.as_typed_array(&self.gc_heap).map(|t| t.kind()) {
                            Some(crate::binary::TypedArrayKind::Float64) => {
                                Some(jit::JitElementLoadKind::Float64)
                            }
                            Some(crate::binary::TypedArrayKind::Int32) => {
                                Some(jit::JitElementLoadKind::Int32)
                            }
                            Some(_) => Some(jit::JitElementLoadKind::Any),
                            None => None,
                        };
                        if let Some(feedback) = feedback {
                            feedback.record_element_load(observed);
                        }
                    }
                    if self.drive_load_element(stack, context, operands)? {
                        continue;
                    }
                    let (dst, recv_reg, idx_reg) = context
                        .exec_register3(instr)
                        .ok_or_else(|| VmError::InvalidOperand)?;
                    let frame = &mut stack[top_idx];
                    self.run_load_element_regs(context, frame, dst, recv_reg, idx_reg)?;
                    continue;
                }
                Op::LoadSuperProperty => {
                    let dst = function
                        .register(instr, 0)
                        .ok_or_else(|| VmError::InvalidOperand)?;
                    let home_reg = context
                        .exec_register(instr, 1)
                        .ok_or_else(|| VmError::InvalidOperand)?;
                    let name_idx = context
                        .exec_const_index(instr, 2)
                        .ok_or_else(|| VmError::InvalidOperand)?;
                    let name = context
                        .property_atom(name_idx)
                        .ok_or_else(|| VmError::InvalidOperand)?
                        .name();
                    let home = *read_register(&stack[top_idx], home_reg)?;
                    self.run_load_super_property(
                        context,
                        stack,
                        top_idx,
                        dst,
                        home,
                        SuperReadKey::Resolved(VmPropertyKey::String(name)),
                    )?;
                    continue;
                }
                Op::LoadSuperElement => {
                    let (dst, home_reg, key_reg) = context
                        .exec_register3(instr)
                        .ok_or_else(|| VmError::InvalidOperand)?;
                    let home = *read_register(&stack[top_idx], home_reg)?;
                    let key_raw = *read_register(&stack[top_idx], key_reg)?;
                    self.run_load_super_property(
                        context,
                        stack,
                        top_idx,
                        dst,
                        home,
                        SuperReadKey::Computed(key_raw),
                    )?;
                    continue;
                }
                Op::SetSuperProperty => {
                    let home_reg = context
                        .exec_register(instr, 0)
                        .ok_or_else(|| VmError::InvalidOperand)?;
                    let name_idx = context
                        .exec_const_index(instr, 1)
                        .ok_or_else(|| VmError::InvalidOperand)?;
                    let value_reg = context
                        .exec_register(instr, 2)
                        .ok_or_else(|| VmError::InvalidOperand)?;
                    let name = context
                        .property_atom(name_idx)
                        .ok_or_else(|| VmError::InvalidOperand)?
                        .name();
                    let home = *read_register(&stack[top_idx], home_reg)?;
                    let value = *read_register(&stack[top_idx], value_reg)?;
                    let strict = context.function_is_strict(stack[top_idx].function_id);
                    self.run_store_super_property(
                        context,
                        stack,
                        top_idx,
                        home,
                        SuperReadKey::Resolved(VmPropertyKey::String(name)),
                        value,
                        strict,
                    )?;
                    continue;
                }
                Op::SetSuperElement => {
                    let (home_reg, key_reg, value_reg) = context
                        .exec_register3(instr)
                        .ok_or_else(|| VmError::InvalidOperand)?;
                    let home = *read_register(&stack[top_idx], home_reg)?;
                    let key_raw = *read_register(&stack[top_idx], key_reg)?;
                    let value = *read_register(&stack[top_idx], value_reg)?;
                    let strict = context.function_is_strict(stack[top_idx].function_id);
                    self.run_store_super_property(
                        context,
                        stack,
                        top_idx,
                        home,
                        SuperReadKey::Computed(key_raw),
                        value,
                        strict,
                    )?;
                    continue;
                }
                // §10.1.9 [[Set]] — accessor setter dispatch follows
                // the same pattern as `LoadProperty`. Non-writable
                // and non-extensible rejections surface here too.
                // <https://tc39.es/ecma262/#sec-ordinaryset>
                Op::StoreProperty => {
                    let operands = function.operand_view(instr);
                    if self.drive_store_property(stack, context, operands)? {
                        continue;
                    }
                    let obj_reg = context
                        .exec_register(instr, 0)
                        .ok_or_else(|| VmError::InvalidOperand)?;
                    let name_idx = context
                        .exec_const_index(instr, 1)
                        .ok_or_else(|| VmError::InvalidOperand)?;
                    let src = function
                        .register(instr, 2)
                        .ok_or_else(|| VmError::InvalidOperand)?;
                    let key = context
                        .property_atom(name_idx)
                        .ok_or_else(|| VmError::InvalidOperand)?;
                    self.run_store_property_reg(context, &mut *stack, top_idx, obj_reg, key, src)?;
                    continue;
                }
                Op::StoreElement => {
                    let operands = function.operand_view(instr);
                    let recv_reg = context
                        .exec_register(instr, 0)
                        .ok_or_else(|| VmError::InvalidOperand)?;
                    let idx_reg = context
                        .exec_register(instr, 1)
                        .ok_or_else(|| VmError::InvalidOperand)?;
                    let src_reg = context
                        .exec_register(instr, 2)
                        .ok_or_else(|| VmError::InvalidOperand)?;
                    if let Some(feedback) = feedback {
                        let value = *read_register(&stack[top_idx], src_reg)?;
                        feedback.record_arith(value, value);
                    }
                    if self.drive_store_element(stack, context, operands)? {
                        continue;
                    }
                    self.run_store_element_regs(
                        context,
                        &mut *stack,
                        top_idx,
                        recv_reg,
                        idx_reg,
                        src_reg,
                    )?;
                    continue;
                }
                Op::Instanceof => {
                    let operands = function.operand_view(instr);
                    if self.drive_instanceof(stack, context, operands)? {
                        continue;
                    }
                    let (dst, lhs, rhs) = function
                        .register3(instr)
                        .ok_or_else(|| VmError::InvalidOperand)?;
                    let frame = &mut stack[top_idx];
                    self.run_instanceof_legacy_regs(frame, dst, lhs, rhs)?;
                    continue;
                }
                // §28.2.4.7 / .10 Proxy.[[HasProperty]] /
                // [[Delete]] — invoke `has` / `deleteProperty`
                // traps when the receiver is a Proxy.
                Op::HasProperty => {
                    let operands = function.operand_view(instr);
                    if self.drive_has_property_proxy(stack, context, operands)? {
                        continue;
                    }
                    let (dst, lhs, rhs) = context
                        .exec_register3(instr)
                        .ok_or_else(|| VmError::InvalidOperand)?;
                    let frame = &mut stack[top_idx];
                    self.run_has_property_regs(frame, context, dst, lhs, rhs)?;
                    continue;
                }
                Op::DeleteProperty => {
                    let operands = function.operand_view(instr);
                    if self.drive_delete_property_proxy(stack, context, operands)? {
                        continue;
                    }
                    let dst = context
                        .exec_register(instr, 0)
                        .ok_or_else(|| VmError::InvalidOperand)?;
                    let obj_reg = context
                        .exec_register(instr, 1)
                        .ok_or_else(|| VmError::InvalidOperand)?;
                    let name_idx = context
                        .exec_const_index(instr, 2)
                        .ok_or_else(|| VmError::InvalidOperand)?;
                    let key = context
                        .property_atom(name_idx)
                        .ok_or_else(|| VmError::InvalidOperand)?;
                    let strict = context.function_is_strict(stack[top_idx].function_id);
                    // `delete` has an object fast path that bypasses the
                    // §28.3 MOP funnel; trigger deferred-namespace
                    // evaluation here (named delete is never symbol-like
                    // unless the key is "then").
                    let receiver = *read_register(&stack[top_idx], obj_reg)?;
                    if receiver.as_object().is_some_and(|o| {
                        crate::object::deferred_namespace_target(o, &self.gc_heap).is_some()
                    }) {
                        self.ensure_deferred_namespace_ready(
                            context,
                            &receiver,
                            key.name() != "then",
                        )?;
                    }
                    let frame = &mut stack[top_idx];
                    self.run_delete_property_reg(frame, dst, obj_reg, key, strict)?;
                    continue;
                }
                Op::DeleteElement => {
                    let operands = function.operand_view(instr);
                    if self.drive_delete_element_proxy(stack, context, operands)? {
                        continue;
                    }
                    let (dst, obj_reg, idx_reg) = context
                        .exec_register3(instr)
                        .ok_or_else(|| VmError::InvalidOperand)?;
                    let strict = context.function_is_strict(stack[top_idx].function_id);
                    let receiver = *read_register(&stack[top_idx], obj_reg)?;
                    if receiver.as_object().is_some_and(|o| {
                        crate::object::deferred_namespace_target(o, &self.gc_heap).is_some()
                    }) {
                        let key_val = *read_register(&stack[top_idx], idx_reg)?;
                        let symbol_like = key_val.as_symbol(&self.gc_heap).is_some()
                            || key_val
                                .as_string(&self.gc_heap)
                                .is_some_and(|s| s.to_lossy_string(&self.gc_heap) == "then");
                        self.ensure_deferred_namespace_ready(context, &receiver, !symbol_like)?;
                    }
                    let frame = &mut stack[top_idx];
                    self.run_delete_element_regs(frame, dst, obj_reg, idx_reg, strict)?;
                    continue;
                }
                // §28.2.4.1 / .2 Proxy.[[GetPrototypeOf]] /
                // [[SetPrototypeOf]] — invoke `getPrototypeOf` /
                // `setPrototypeOf` traps when the receiver is a
                // Proxy.
                Op::GetPrototype => {
                    let operands = function.operand_view(instr);
                    if self.drive_get_prototype_proxy(stack, context, operands)? {
                        continue;
                    }
                    let dst = context
                        .exec_register(instr, 0)
                        .ok_or_else(|| VmError::InvalidOperand)?;
                    let src = context
                        .exec_register(instr, 1)
                        .ok_or_else(|| VmError::InvalidOperand)?;
                    let frame = &mut stack[top_idx];
                    self.run_get_prototype_regs(frame, dst, src)?;
                    continue;
                }
                Op::SetPrototype => {
                    let operands = function.operand_view(instr);
                    if self.drive_set_prototype_proxy(stack, context, operands)? {
                        continue;
                    }
                    let obj_reg = context
                        .exec_register(instr, 0)
                        .ok_or_else(|| VmError::InvalidOperand)?;
                    let proto_reg = context
                        .exec_register(instr, 1)
                        .ok_or_else(|| VmError::InvalidOperand)?;
                    let frame = &mut stack[top_idx];
                    self.run_set_prototype_regs(context, frame, obj_reg, proto_reg)?;
                    continue;
                }
                // §19.4.1 indirect eval — recursively dispatches a
                // freshly compiled module on a sub-stack, then
                // writes the completion value into `dst`. Stack-
                // modifying so it has to run before the in-frame
                // borrow below.
                Op::Eval => {
                    let operands = function.operand_view(instr);
                    self.run_eval_operands(context, stack, operands)?;
                    continue;
                }
                // §20.2.1.1 — `new Function(args, body)` recurses
                // into the eval hook with a synthesised wrapper.
                Op::NewFunction => {
                    let operands = function.operand_view(instr);
                    self.run_new_function_operands(context, stack, operands)?;
                    continue;
                }
                Op::CollectArguments => {
                    let dst = register_operand(context.exec_operand(instr, 0))?;
                    self.run_collect_arguments_reg(context, stack, top_idx, dst)?;
                    continue;
                }
                Op::Nop => {
                    stack[top_idx].advance_pc()?;
                    continue;
                }
                Op::LoadUndefined => {
                    let dst = context
                        .exec_register(instr, 0)
                        .ok_or_else(|| VmError::InvalidOperand)?;
                    let frame = &mut stack[top_idx];
                    write_register(frame, dst, Value::undefined())?;
                    frame.advance_pc()?;
                    continue;
                }
                Op::LoadHole => {
                    let dst = context
                        .exec_register(instr, 0)
                        .ok_or_else(|| VmError::InvalidOperand)?;
                    let frame = &mut stack[top_idx];
                    write_register(frame, dst, Value::hole())?;
                    frame.advance_pc()?;
                    continue;
                }
                Op::LoadTrue => {
                    let dst = context
                        .exec_register(instr, 0)
                        .ok_or_else(|| VmError::InvalidOperand)?;
                    let frame = &mut stack[top_idx];
                    write_register(frame, dst, Value::boolean(true))?;
                    frame.advance_pc()?;
                    continue;
                }
                Op::LoadFalse => {
                    let dst = context
                        .exec_register(instr, 0)
                        .ok_or_else(|| VmError::InvalidOperand)?;
                    let frame = &mut stack[top_idx];
                    write_register(frame, dst, Value::boolean(false))?;
                    frame.advance_pc()?;
                    continue;
                }
                Op::LoadNull => {
                    let dst = context
                        .exec_register(instr, 0)
                        .ok_or_else(|| VmError::InvalidOperand)?;
                    let frame = &mut stack[top_idx];
                    write_register(frame, dst, Value::null())?;
                    frame.advance_pc()?;
                    continue;
                }
                Op::LoadInt32 => {
                    let dst = context
                        .exec_register(instr, 0)
                        .ok_or_else(|| VmError::InvalidOperand)?;
                    let imm = context
                        .exec_imm32(instr, 1)
                        .ok_or_else(|| VmError::InvalidOperand)?;
                    let frame = &mut stack[top_idx];
                    write_register(frame, dst, Value::number(NumberValue::Smi(imm)))?;
                    frame.advance_pc()?;
                    continue;
                }
                Op::LoadNumber => {
                    let dst = context
                        .exec_register(instr, 0)
                        .ok_or_else(|| VmError::InvalidOperand)?;
                    let idx = context
                        .exec_const_index(instr, 1)
                        .ok_or_else(|| VmError::InvalidOperand)?;
                    let bits = context
                        .number_constant_bits(idx)
                        .ok_or_else(|| VmError::InvalidOperand)?;
                    let value = NumberValue::from_f64(f64::from_bits(bits));
                    let frame = &mut stack[top_idx];
                    write_register(frame, dst, Value::number(value))?;
                    frame.advance_pc()?;
                    continue;
                }
                Op::LoadString => {
                    let dst = context
                        .exec_register(instr, 0)
                        .ok_or_else(|| VmError::InvalidOperand)?;
                    let idx = context
                        .exec_const_index(instr, 1)
                        .ok_or_else(|| VmError::InvalidOperand)?;
                    let value = self.load_string_constant_value(context, idx)?;
                    let frame = &mut stack[top_idx];
                    write_register(frame, dst, value)?;
                    frame.advance_pc()?;
                    continue;
                }
                Op::LoadLength => {
                    let dst = context
                        .exec_register(instr, 0)
                        .ok_or_else(|| VmError::InvalidOperand)?;
                    let src = context
                        .exec_register(instr, 1)
                        .ok_or_else(|| VmError::InvalidOperand)?;
                    self.run_load_length_reg(&mut stack[top_idx], dst, src)?;
                    continue;
                }
                Op::LogicalNot => {
                    let dst = context
                        .exec_register(instr, 0)
                        .ok_or_else(|| VmError::InvalidOperand)?;
                    let src = context
                        .exec_register(instr, 1)
                        .ok_or_else(|| VmError::InvalidOperand)?;
                    let frame = &mut stack[top_idx];
                    let truthy = read_register(frame, src)?.to_boolean(&self.gc_heap);
                    write_register(frame, dst, Value::boolean(!truthy))?;
                    frame.advance_pc()?;
                    continue;
                }
                Op::ToBoolean => {
                    let dst = context
                        .exec_register(instr, 0)
                        .ok_or_else(|| VmError::InvalidOperand)?;
                    let src = context
                        .exec_register(instr, 1)
                        .ok_or_else(|| VmError::InvalidOperand)?;
                    let frame = &mut stack[top_idx];
                    let truthy = read_register(frame, src)?.to_boolean(&self.gc_heap);
                    write_register(frame, dst, Value::boolean(truthy))?;
                    frame.advance_pc()?;
                    continue;
                }
                Op::GetStringIndex => {
                    let (dst, recv, idx) = context
                        .exec_register3(instr)
                        .ok_or_else(|| VmError::InvalidOperand)?;
                    let frame = &mut stack[top_idx];
                    self.run_get_string_index_regs(frame, dst, recv, idx)?;
                    continue;
                }
                Op::TypeOf => {
                    let dst = context
                        .exec_register(instr, 0)
                        .ok_or_else(|| VmError::InvalidOperand)?;
                    let src = context
                        .exec_register(instr, 1)
                        .ok_or_else(|| VmError::InvalidOperand)?;
                    let frame = &mut stack[top_idx];
                    self.run_typeof_regs(frame, dst, src)?;
                    continue;
                }
                Op::LoadThis => {
                    let dst = context
                        .exec_register(instr, 0)
                        .ok_or_else(|| VmError::InvalidOperand)?;
                    // Legacy arrows created before a derived constructor binds
                    // `this` carry the hole snapshot. Resolve that sidecar-only
                    // inheritance once, then enter the same kernel native
                    // activations use.
                    let this_value = self.materialized_this_binding(&*stack, top_idx)?;
                    let mut frame = ActiveFrameMut::materialized(&mut stack[top_idx]);
                    frame.set_this_value(this_value);
                    self.frame_load_this(&mut frame, dst)?;
                    frame.advance_pc()?;
                    continue;
                }
                Op::LoadNewTarget => {
                    let dst = context
                        .exec_register(instr, 0)
                        .ok_or_else(|| VmError::InvalidOperand)?;
                    let new_target = self
                        .frame_cold(&stack[top_idx])
                        .and_then(|cold| cold.new_target)
                        .unwrap_or(Value::undefined());
                    let mut frame = ActiveFrameMut::materialized_with_new_target(
                        &mut stack[top_idx],
                        new_target,
                    );
                    self.frame_load_new_target(&mut frame, dst)?;
                    frame.advance_pc()?;
                    continue;
                }
                Op::NewObject => {
                    let dst = context
                        .exec_register(instr, 0)
                        .ok_or_else(|| VmError::InvalidOperand)?;
                    self.run_new_object_reg(&mut *stack, top_idx, dst)?;
                    continue;
                }
                Op::NewArray => {
                    let operands = function.operand_view(instr);
                    self.run_new_array_operands(&mut *stack, top_idx, operands)?;
                    continue;
                }
                Op::LoadRegExp => {
                    let dst = context
                        .exec_register(instr, 0)
                        .ok_or_else(|| VmError::InvalidOperand)?;
                    let idx = context
                        .exec_const_index(instr, 1)
                        .ok_or_else(|| VmError::InvalidOperand)?;
                    let frame = &mut stack[top_idx];
                    self.run_load_regexp_reg(context, frame, dst, idx)?;
                    continue;
                }
                Op::LoadBigInt => {
                    let dst = context
                        .exec_register(instr, 0)
                        .ok_or_else(|| VmError::InvalidOperand)?;
                    let idx = context
                        .exec_const_index(instr, 1)
                        .ok_or_else(|| VmError::InvalidOperand)?;
                    let frame = &mut stack[top_idx];
                    self.run_load_bigint_reg(context, frame, dst, idx)?;
                    continue;
                }
                Op::LoadUpvalue => {
                    let dst = context
                        .exec_register(instr, 0)
                        .ok_or_else(|| VmError::InvalidOperand)?;
                    let idx = context
                        .exec_imm32(instr, 1)
                        .ok_or_else(|| VmError::InvalidOperand)?;
                    let mut frame = ActiveFrameMut::materialized(&mut stack[top_idx]);
                    self.frame_load_upvalue(&mut frame, dst, idx)?;
                    frame.advance_pc()?;
                    continue;
                }
                Op::FreshUpvalue => {
                    let idx = context
                        .exec_imm32(instr, 0)
                        .ok_or_else(|| VmError::InvalidOperand)?;
                    let mut frame = ActiveFrameMut::materialized(&mut stack[top_idx]);
                    self.frame_fresh_upvalue(&mut frame, idx)?;
                    frame.advance_pc()?;
                    continue;
                }
                Op::StoreUpvalue => {
                    let src = context
                        .exec_register(instr, 0)
                        .ok_or_else(|| VmError::InvalidOperand)?;
                    let idx = context
                        .exec_imm32(instr, 1)
                        .ok_or_else(|| VmError::InvalidOperand)?;
                    let mut frame = ActiveFrameMut::materialized(&mut stack[top_idx]);
                    self.frame_store_upvalue(&mut frame, src, idx)?;
                    frame.advance_pc()?;
                    continue;
                }
                Op::StoreUpvalueChecked => {
                    let src = context
                        .exec_register(instr, 0)
                        .ok_or_else(|| VmError::InvalidOperand)?;
                    let idx = context
                        .exec_imm32(instr, 1)
                        .ok_or_else(|| VmError::InvalidOperand)?;
                    let mut frame = ActiveFrameMut::materialized(&mut stack[top_idx]);
                    self.frame_store_upvalue_checked(&mut frame, src, idx)?;
                    frame.advance_pc()?;
                    continue;
                }
                Op::CollectRest => {
                    let dst = context
                        .exec_register(instr, 0)
                        .ok_or_else(|| VmError::InvalidOperand)?;
                    self.materialized_collect_rest(&mut *stack, top_idx, dst)?;
                    stack[top_idx].advance_pc()?;
                    continue;
                }
                Op::MakeFunction => {
                    let dst = context
                        .exec_register(instr, 0)
                        .ok_or_else(|| VmError::InvalidOperand)?;
                    let idx = context
                        .exec_const_index(instr, 1)
                        .ok_or_else(|| VmError::InvalidOperand)?;
                    let frame = &mut stack[top_idx];
                    self.run_make_function_reg(context, frame, dst, idx)?;
                    continue;
                }
                Op::MakeClass => {
                    let dst = context
                        .exec_register(instr, 0)
                        .ok_or_else(|| VmError::InvalidOperand)?;
                    let ctor_reg = context
                        .exec_register(instr, 1)
                        .ok_or_else(|| VmError::InvalidOperand)?;
                    let proto_reg = context
                        .exec_register(instr, 2)
                        .ok_or_else(|| VmError::InvalidOperand)?;
                    let statics_reg = context
                        .exec_register(instr, 3)
                        .ok_or_else(|| VmError::InvalidOperand)?;
                    // Operand 4 (parent class value) — absent in
                    // pre-existing bytecode; `undefined` = base class.
                    let parent_reg = context.exec_register(instr, 4);
                    self.run_make_class_regs(
                        &mut *stack,
                        top_idx,
                        dst,
                        ctor_reg,
                        proto_reg,
                        statics_reg,
                        parent_reg,
                    )?;
                    continue;
                }
                Op::NewError => {
                    let dst = context
                        .exec_register(instr, 0)
                        .ok_or_else(|| VmError::InvalidOperand)?;
                    let msg_reg = context
                        .exec_register(instr, 1)
                        .ok_or_else(|| VmError::InvalidOperand)?;
                    self.run_new_error_regs(context, &mut *stack, top_idx, dst, msg_reg)?;
                    continue;
                }
                Op::NewBuiltinError => {
                    let dst = context
                        .exec_register(instr, 0)
                        .ok_or_else(|| VmError::InvalidOperand)?;
                    let kind_idx = context
                        .exec_const_index(instr, 1)
                        .ok_or_else(|| VmError::InvalidOperand)?;
                    let msg_reg = context
                        .exec_register(instr, 2)
                        .ok_or_else(|| VmError::InvalidOperand)?;
                    self.run_new_builtin_error_regs(
                        context,
                        &mut *stack,
                        top_idx,
                        dst,
                        kind_idx,
                        msg_reg,
                    )?;
                    continue;
                }
                Op::LoadBuiltinError => {
                    let dst = context
                        .exec_register(instr, 0)
                        .ok_or_else(|| VmError::InvalidOperand)?;
                    let kind_idx = context
                        .exec_const_index(instr, 1)
                        .ok_or_else(|| VmError::InvalidOperand)?;
                    let frame = &mut stack[top_idx];
                    self.run_load_builtin_error_reg(context, frame, dst, kind_idx)?;
                    continue;
                }
                Op::LoadGlobalThis => {
                    let dst = context
                        .exec_register(instr, 0)
                        .ok_or_else(|| VmError::InvalidOperand)?;
                    let frame = &mut stack[top_idx];
                    self.run_load_global_this_reg(frame, dst)?;
                    continue;
                }
                Op::LoadGlobalOrThrow => {
                    let dst = context
                        .exec_register(instr, 0)
                        .ok_or_else(|| VmError::InvalidOperand)?;
                    let name_idx = context
                        .exec_const_index(instr, 1)
                        .ok_or_else(|| VmError::InvalidOperand)?;
                    let frame = &mut stack[top_idx];
                    self.run_load_global_or_throw_reg(context, frame, dst, name_idx)?;
                    continue;
                }
                Op::LoadGlobalOrUndefined => {
                    let dst = context
                        .exec_register(instr, 0)
                        .ok_or_else(|| VmError::InvalidOperand)?;
                    let name_idx = context
                        .exec_const_index(instr, 1)
                        .ok_or_else(|| VmError::InvalidOperand)?;
                    let frame = &mut stack[top_idx];
                    self.run_load_global_or_undefined_reg(context, frame, dst, name_idx)?;
                    continue;
                }
                Op::DeclareGlobalVar => {
                    let name_idx = context
                        .exec_const_index(instr, 0)
                        .ok_or_else(|| VmError::InvalidOperand)?;
                    let configurable = context.exec_imm32(instr, 1).unwrap_or(0) != 0;
                    let frame = &mut stack[top_idx];
                    self.run_declare_global_var_reg(context, frame, name_idx, configurable)?;
                    continue;
                }
                // §13.2.8.4 GetTemplateObject — realm-cached frozen
                // template-strings object per tagged-template site.
                Op::GetTemplateObject => {
                    let dst = context
                        .exec_register(instr, 0)
                        .ok_or_else(|| VmError::InvalidOperand)?;
                    let site_idx = context
                        .exec_const_index(instr, 1)
                        .ok_or_else(|| VmError::InvalidOperand)?;
                    self.run_get_template_object_reg(context, stack, top_idx, dst, site_idx)?;
                    continue;
                }
                // §9.1 — captured-binding read in a frame whose
                // function contains a direct eval: an
                // eval-introduced var of the same name shadows the
                // capture.
                Op::LoadShadowedUpvalue => {
                    let dst = context
                        .exec_register(instr, 0)
                        .ok_or_else(|| VmError::InvalidOperand)?;
                    let name_idx = context
                        .exec_const_index(instr, 1)
                        .ok_or_else(|| VmError::InvalidOperand)?;
                    let uv_idx = context.exec_imm32(instr, 2).unwrap_or(0) as usize;
                    let frame = &mut stack[top_idx];
                    self.run_load_shadowed_upvalue_reg(context, frame, dst, name_idx, uv_idx)?;
                    continue;
                }
                Op::LoadDynamic => {
                    let dst = context
                        .exec_register(instr, 0)
                        .ok_or_else(|| VmError::InvalidOperand)?;
                    let name_idx = context
                        .exec_const_index(instr, 1)
                        .ok_or_else(|| VmError::InvalidOperand)?;
                    let frame = &mut stack[top_idx];
                    self.run_load_dynamic_reg(context, frame, dst, name_idx)?;
                    continue;
                }
                Op::StoreDynamic => {
                    let value_reg = context
                        .exec_register(instr, 0)
                        .ok_or_else(|| VmError::InvalidOperand)?;
                    let name_idx = context
                        .exec_const_index(instr, 1)
                        .ok_or_else(|| VmError::InvalidOperand)?;
                    let frame = &mut stack[top_idx];
                    self.run_store_dynamic_reg(context, frame, value_reg, name_idx)?;
                    continue;
                }
                Op::TypeofDynamic => {
                    let dst = context
                        .exec_register(instr, 0)
                        .ok_or_else(|| VmError::InvalidOperand)?;
                    let name_idx = context
                        .exec_const_index(instr, 1)
                        .ok_or_else(|| VmError::InvalidOperand)?;
                    let frame = &mut stack[top_idx];
                    self.run_typeof_dynamic_reg(context, frame, dst, name_idx)?;
                    continue;
                }
                Op::DeleteDynamic => {
                    let dst = context
                        .exec_register(instr, 0)
                        .ok_or_else(|| VmError::InvalidOperand)?;
                    let name_idx = context
                        .exec_const_index(instr, 1)
                        .ok_or_else(|| VmError::InvalidOperand)?;
                    let frame = &mut stack[top_idx];
                    self.run_delete_dynamic_reg(context, frame, dst, name_idx)?;
                    continue;
                }
                // §6.2.12 — mint a Private Name carrier; the marker
                // keeps it out of Proxy traps and arms the §7.3.28
                // extensibility check on adds.
                Op::NewPrivateName => {
                    let dst = context
                        .exec_register(instr, 0)
                        .ok_or_else(|| VmError::InvalidOperand)?;
                    let desc_idx = context
                        .exec_const_index(instr, 1)
                        .ok_or_else(|| VmError::InvalidOperand)?;
                    self.run_new_private_name_reg(context, stack, top_idx, dst, desc_idx)?;
                    continue;
                }
                Op::DefineGlobalFunction => {
                    let name_idx = context
                        .exec_const_index(instr, 0)
                        .ok_or_else(|| VmError::InvalidOperand)?;
                    let value_reg = context
                        .exec_register(instr, 1)
                        .ok_or_else(|| VmError::InvalidOperand)?;
                    let deletable = context.exec_imm32(instr, 2).unwrap_or(0) != 0;
                    let frame = &mut stack[top_idx];
                    self.run_define_global_function_reg(
                        context, frame, name_idx, value_reg, deletable,
                    )?;
                    continue;
                }
                Op::DeclareGlobalLex => {
                    let name_idx = context
                        .exec_const_index(instr, 0)
                        .ok_or_else(|| VmError::InvalidOperand)?;
                    let is_const = context.exec_imm32(instr, 1).unwrap_or(0) != 0;
                    let frame = &mut stack[top_idx];
                    self.run_declare_global_lex_reg(context, frame, name_idx, is_const)?;
                    continue;
                }
                Op::StoreGlobalBinding => {
                    let value_reg = context
                        .exec_register(instr, 0)
                        .ok_or_else(|| VmError::InvalidOperand)?;
                    let name_idx = context
                        .exec_const_index(instr, 1)
                        .ok_or_else(|| VmError::InvalidOperand)?;
                    let strict = context.exec_imm32(instr, 2).unwrap_or(0) != 0;
                    let frame = &mut stack[top_idx];
                    self.run_store_global_binding_reg(context, frame, value_reg, name_idx, strict)?;
                    continue;
                }
                Op::InitGlobalLex => {
                    let value_reg = context
                        .exec_register(instr, 0)
                        .ok_or_else(|| VmError::InvalidOperand)?;
                    let name_idx = context
                        .exec_const_index(instr, 1)
                        .ok_or_else(|| VmError::InvalidOperand)?;
                    let frame = &mut stack[top_idx];
                    self.run_init_global_lex_reg(context, frame, value_reg, name_idx)?;
                    continue;
                }
                // §15.7.14 class-definition validation: heritage
                // IsConstructor / static computed key != "prototype".
                Op::ClassCheck => {
                    let kind = context.exec_imm32(instr, 0).unwrap_or(0);
                    let reg = context
                        .exec_register(instr, 1)
                        .ok_or_else(|| VmError::InvalidOperand)?;
                    self.run_class_check_reg(context, stack, top_idx, kind as u32, reg)?;
                    continue;
                }
                // §7.3.7 CreateDataPropertyOrThrow — object literal
                // property definition; never consults inherited
                // setters (unlike StoreProperty's Set semantics).
                Op::DefineDataProperty => {
                    let (obj_reg, key_reg, value_reg) = context
                        .exec_register3(instr)
                        .ok_or_else(|| VmError::InvalidOperand)?;
                    self.run_define_data_property_regs(
                        context, stack, top_idx, obj_reg, key_reg, value_reg,
                    )?;
                    stack[top_idx].advance_pc()?;
                    continue;
                }
                // §10.2.10 SetFunctionName — names an anonymous
                // function from a run-time property key.
                Op::SetFunctionName => {
                    let fn_reg = context
                        .exec_register(instr, 0)
                        .ok_or_else(|| VmError::InvalidOperand)?;
                    let key_reg = context
                        .exec_register(instr, 1)
                        .ok_or_else(|| VmError::InvalidOperand)?;
                    let prefix_idx = context
                        .exec_const_index(instr, 2)
                        .ok_or_else(|| VmError::InvalidOperand)?;
                    self.run_set_function_name_reg(
                        context, stack, top_idx, fn_reg, key_reg, prefix_idx,
                    )?;
                    continue;
                }
                // §7.3.31 PrivateGet — brand check (absent name
                // throws), accessor-without-getter throws, accessor
                // invokes its getter with the receiver as `this`.
                Op::PrivateGet => {
                    let (dst, obj_reg, key_reg) = context
                        .exec_register3(instr)
                        .ok_or_else(|| VmError::InvalidOperand)?;
                    self.run_private_get_reg(context, stack, top_idx, dst, obj_reg, key_reg)?;
                    continue;
                }
                // §7.3.32 PrivateSet — brand check, private methods
                // are not writable, accessor-without-setter throws,
                // an own field writes in place preserving attributes.
                Op::PrivateSet => {
                    let (obj_reg, key_reg, value_reg) = context
                        .exec_register3(instr)
                        .ok_or_else(|| VmError::InvalidOperand)?;
                    self.run_private_set_reg(context, stack, top_idx, obj_reg, key_reg, value_reg)?;
                    continue;
                }
                // §7.1.3 ToNumeric on an already-primitive operand:
                // Number / BigInt pass through, Symbol throws, the
                // rest convert via ToNumber. Emitted between the two
                // ToPrimitive coercions of a numeric binary operator
                // so ToNumeric(lhs) throws before rhs `valueOf` runs.
                Op::ToNumeric => {
                    let dst = context
                        .exec_register(instr, 0)
                        .ok_or_else(|| VmError::InvalidOperand)?;
                    let src = context
                        .exec_register(instr, 1)
                        .ok_or_else(|| VmError::InvalidOperand)?;
                    let value = *read_register(&stack[top_idx], src)?;
                    let result = if value.is_number() || value.is_big_int() {
                        value
                    } else if value.is_symbol() {
                        return Err(self.err_type(
                            ("Cannot convert a Symbol value to a number".to_string()).into(),
                        ));
                    } else {
                        Value::number(crate::number::NumberValue::from_f64(
                            crate::number::parse::to_number_value(&value, &self.gc_heap),
                        ))
                    };
                    let frame = &mut stack[top_idx];
                    write_register(frame, dst, result)?;
                    frame.advance_pc()?;
                    continue;
                }
                // §7.1.18 ToObject — wrap a primitive in its
                // `%X.prototype%` body; objects pass through;
                // `null` / `undefined` throw a TypeError. Emitted by
                // the `with` statement lowering (§14.11.2 step 2).
                Op::ToObject => {
                    let dst = context
                        .exec_register(instr, 0)
                        .ok_or_else(|| VmError::InvalidOperand)?;
                    let src = context
                        .exec_register(instr, 1)
                        .ok_or_else(|| VmError::InvalidOperand)?;
                    self.run_to_object_reg(stack, top_idx, dst, src)?;
                    continue;
                }
                // §7.1.19 ToPropertyKey with full user coercion —
                // class field definitions canonicalize their
                // computed names at class-definition time.
                Op::ToPropertyKey => {
                    let dst = context
                        .exec_register(instr, 0)
                        .ok_or_else(|| VmError::InvalidOperand)?;
                    let src = context
                        .exec_register(instr, 1)
                        .ok_or_else(|| VmError::InvalidOperand)?;
                    self.run_to_property_key_reg(context, stack, top_idx, dst, src)?;
                    continue;
                }
                // §7.3.31 PrivateElementFind own-only — private
                // methods / accessors require the class brand marker
                // as an OWN property of the receiver (installed after
                // super() returns); the prototype-side method lookup
                // alone must not satisfy access before that.
                Op::PrivateBrandCheck => {
                    let obj_reg = context
                        .exec_register(instr, 0)
                        .ok_or_else(|| VmError::InvalidOperand)?;
                    let brand_reg = context
                        .exec_register(instr, 1)
                        .ok_or_else(|| VmError::InvalidOperand)?;
                    self.run_private_brand_check_reg(context, stack, top_idx, obj_reg, brand_reg)?;
                    continue;
                }
                // §13.4.2 UpdateExpression numeric step — ToNumeric
                // then ±1, preserving the BigInt type (§6.1.6.2.7).
                Op::Increment => {
                    let dst = context
                        .exec_register(instr, 0)
                        .ok_or_else(|| VmError::InvalidOperand)?;
                    let src = context
                        .exec_register(instr, 1)
                        .ok_or_else(|| VmError::InvalidOperand)?;
                    let delta = context.exec_imm32(instr, 2).unwrap_or(1);
                    let frame = &mut stack[top_idx];
                    self.run_increment_regs(context, frame, dst, src, delta, feedback)?;
                    continue;
                }
                Op::ValidateGlobalDecl => {
                    let name_idx = context
                        .exec_const_index(instr, 0)
                        .ok_or_else(|| VmError::InvalidOperand)?;
                    let kind = context.exec_imm32(instr, 1).unwrap_or(0);
                    let frame = &mut stack[top_idx];
                    self.run_validate_global_decl_reg(context, frame, name_idx, kind)?;
                    continue;
                }
                Op::DefineGlobalVar => {
                    let name_idx = context
                        .exec_const_index(instr, 0)
                        .ok_or_else(|| VmError::InvalidOperand)?;
                    let value_reg = context
                        .exec_register(instr, 1)
                        .ok_or_else(|| VmError::InvalidOperand)?;
                    let frame = &mut stack[top_idx];
                    self.run_define_global_var_reg(context, frame, name_idx, value_reg)?;
                    continue;
                }
                Op::ImportNamespace => {
                    let dst = context
                        .exec_register(instr, 0)
                        .ok_or_else(|| VmError::InvalidOperand)?;
                    let spec_idx = context
                        .exec_const_index(instr, 1)
                        .ok_or_else(|| VmError::InvalidOperand)?;
                    self.run_import_namespace_reg(context, stack, top_idx, dst, spec_idx)?;
                    continue;
                }
                Op::ImportNamespaceDeferred => {
                    let dst = context
                        .exec_register(instr, 0)
                        .ok_or_else(|| VmError::InvalidOperand)?;
                    let spec_idx = context
                        .exec_const_index(instr, 1)
                        .ok_or_else(|| VmError::InvalidOperand)?;
                    self.run_import_namespace_deferred_reg(context, stack, top_idx, dst, spec_idx)?;
                    continue;
                }
                Op::ModuleNamespaceObject => {
                    let dst = context
                        .exec_register(instr, 0)
                        .ok_or_else(|| VmError::InvalidOperand)?;
                    let spec_idx = context
                        .exec_const_index(instr, 1)
                        .ok_or_else(|| VmError::InvalidOperand)?;
                    self.run_module_namespace_object_reg(context, stack, top_idx, dst, spec_idx)?;
                    continue;
                }
                Op::LoadImportBinding => {
                    let dst = context
                        .exec_register(instr, 0)
                        .ok_or_else(|| VmError::InvalidOperand)?;
                    let url_idx = context
                        .exec_const_index(instr, 1)
                        .ok_or_else(|| VmError::InvalidOperand)?;
                    let name_idx = context
                        .exec_const_index(instr, 2)
                        .ok_or_else(|| VmError::InvalidOperand)?;
                    self.run_load_import_binding_reg(
                        context, stack, top_idx, dst, url_idx, name_idx,
                    )?;
                    continue;
                }
                Op::EvaluateModule => {
                    let dst = context
                        .exec_register(instr, 0)
                        .ok_or_else(|| VmError::InvalidOperand)?;
                    let url_idx = context
                        .exec_const_index(instr, 1)
                        .ok_or_else(|| VmError::InvalidOperand)?;
                    self.run_evaluate_module_const(context, stack, top_idx, dst, url_idx)?;
                    continue;
                }
                Op::MarkModuleEvaluated => {
                    let url_idx = context
                        .exec_const_index(instr, 0)
                        .ok_or_else(|| VmError::InvalidOperand)?;
                    self.run_mark_module_evaluated_const(context, stack, top_idx, url_idx)?;
                    continue;
                }
                Op::ImportMetaResolve => {
                    let dst = context
                        .exec_register(instr, 0)
                        .ok_or_else(|| VmError::InvalidOperand)?;
                    let spec_reg = context
                        .exec_register(instr, 1)
                        .ok_or_else(|| VmError::InvalidOperand)?;
                    self.run_import_meta_resolve_regs(context, stack, top_idx, dst, spec_reg)?;
                    continue;
                }
                Op::PromiseFulfilledOf => {
                    let dst = context
                        .exec_register(instr, 0)
                        .ok_or_else(|| VmError::InvalidOperand)?;
                    let src = context
                        .exec_register(instr, 1)
                        .ok_or_else(|| VmError::InvalidOperand)?;
                    self.run_promise_fulfilled_of_regs(context, stack, top_idx, dst, src)?;
                    continue;
                }
                Op::ArrayPush => {
                    let arr_reg = context
                        .exec_register(instr, 0)
                        .ok_or_else(|| VmError::InvalidOperand)?;
                    let value_reg = context
                        .exec_register(instr, 1)
                        .ok_or_else(|| VmError::InvalidOperand)?;
                    self.run_array_push_regs(&mut *stack, top_idx, arr_reg, value_reg)?;
                    continue;
                }
                Op::NewWeakRef => {
                    let dst = context
                        .exec_register(instr, 0)
                        .ok_or_else(|| VmError::InvalidOperand)?;
                    let target_reg = context
                        .exec_register(instr, 1)
                        .ok_or_else(|| VmError::InvalidOperand)?;
                    self.run_new_weak_ref_regs(&mut *stack, top_idx, dst, target_reg)?;
                    continue;
                }
                Op::NewFinalizationRegistry => {
                    let dst = context
                        .exec_register(instr, 0)
                        .ok_or_else(|| VmError::InvalidOperand)?;
                    let callback_reg = context
                        .exec_register(instr, 1)
                        .ok_or_else(|| VmError::InvalidOperand)?;
                    self.run_new_finalization_registry_regs(
                        context,
                        &mut *stack,
                        top_idx,
                        dst,
                        callback_reg,
                    )?;
                    continue;
                }
                Op::NewCollection => {
                    let dst = context
                        .exec_register(instr, 0)
                        .ok_or_else(|| VmError::InvalidOperand)?;
                    let kind_idx = context
                        .exec_const_index(instr, 1)
                        .ok_or_else(|| VmError::InvalidOperand)?;
                    let iter_reg = context
                        .exec_register(instr, 2)
                        .ok_or_else(|| VmError::InvalidOperand)?;
                    self.run_new_collection_regs(
                        context,
                        &mut *stack,
                        top_idx,
                        dst,
                        kind_idx,
                        iter_reg,
                    )?;
                    continue;
                }
                Op::MathLoad => {
                    let dst = context
                        .exec_register(instr, 0)
                        .ok_or_else(|| VmError::InvalidOperand)?;
                    let name_idx = context
                        .exec_const_index(instr, 1)
                        .ok_or_else(|| VmError::InvalidOperand)?;
                    let frame = &mut stack[top_idx];
                    self.run_math_load_reg(context, frame, dst, name_idx)?;
                    continue;
                }
                Op::MathCall => {
                    let operands = function.operand_view(instr);
                    self.do_math_call(stack, context, operands)?;
                    continue;
                }
                Op::SymbolLoad => {
                    let dst = context
                        .exec_register(instr, 0)
                        .ok_or_else(|| VmError::InvalidOperand)?;
                    let name_idx = context
                        .exec_const_index(instr, 1)
                        .ok_or_else(|| VmError::InvalidOperand)?;
                    let frame = &mut stack[top_idx];
                    self.run_symbol_load_reg(context, frame, dst, name_idx)?;
                    continue;
                }
                Op::TemporalLoad => {
                    let dst = context
                        .exec_register(instr, 0)
                        .ok_or_else(|| VmError::InvalidOperand)?;
                    let name_idx = context
                        .exec_const_index(instr, 1)
                        .ok_or_else(|| VmError::InvalidOperand)?;
                    let frame = &mut stack[top_idx];
                    self.run_temporal_load_reg(context, frame, dst, name_idx)?;
                    continue;
                }
                Op::EnterTry => {
                    let region = context
                        .exec_exception_region(instr)
                        .ok_or_else(|| VmError::InvalidOperand)?;
                    let frame = &mut stack[top_idx];
                    self.materialized_enter_try_region(frame, region)?;
                    frame.advance_pc()?;
                    continue;
                }
                Op::LeaveTry => {
                    let frame = &mut stack[top_idx];
                    self.materialized_leave_try(frame)?;
                    frame.advance_pc()?;
                    continue;
                }
                Op::GlobalBindingExists => {
                    let dst = register_operand(context.exec_operand(instr, 0))?;
                    let name_idx = const_operand(context.exec_operand(instr, 1))?;
                    let frame = &mut stack[top_idx];
                    self.run_global_binding_exists_reg(context, frame, dst, name_idx)?;
                    continue;
                }
                Op::StoreGlobalChecked => {
                    let value_reg = register_operand(context.exec_operand(instr, 0))?;
                    let name_idx = const_operand(context.exec_operand(instr, 1))?;
                    let exists_reg = register_operand(context.exec_operand(instr, 2))?;
                    let frame = &mut stack[top_idx];
                    self.run_store_global_checked_reg(
                        context, frame, value_reg, name_idx, exists_reg,
                    )?;
                    continue;
                }
                Op::PopParkedFinally => {
                    // §14.15.3 — a break/continue leaving `count`
                    // finally bodies abandons the completions those
                    // finallys parked (innermost on top).
                    let count = context
                        .exec_imm32(instr, 0)
                        .ok_or_else(|| VmError::InvalidOperand)?
                        .max(0) as usize;
                    self.materialized_pop_parked_finally(&mut stack[top_idx], count)?;
                    stack[top_idx].advance_pc()?;
                    continue;
                }
                Op::JumpViaFinally => {
                    // §14.15.3 — `break`/`continue` crossing `finally`
                    // blocks: run them (down to `floor`), then jump.
                    let offset = context
                        .exec_imm32(instr, 0)
                        .ok_or_else(|| VmError::InvalidOperand)?;
                    let floor = context
                        .exec_imm32(instr, 1)
                        .ok_or_else(|| VmError::InvalidOperand)?
                        as u32;
                    let next_pc = (stack[top_idx].pc as i64 + 1).saturating_add(offset as i64);
                    if !(0..=u32::MAX as i64).contains(&next_pc) {
                        return Err(VmError::InvalidOperand);
                    }
                    if let Some(popped) = self.unwind_abrupt(
                        stack,
                        crate::cold_frame::AbruptKind::Jump(next_pc as u32),
                        floor,
                    )? {
                        return Ok(popped);
                    }
                    continue;
                }
                Op::Jump => {
                    let offset = context
                        .exec_imm32(instr, 0)
                        .ok_or_else(|| VmError::InvalidOperand)?;
                    apply_branch(&mut stack[top_idx], offset, &self.interrupt)?;
                    if offset < 0
                        && let Some(Some(value)) =
                            self.note_backedge_and_maybe_osr(stack, context, top_idx)?
                    {
                        return Ok(value);
                    }
                    continue;
                }
                Op::JumpIfTrue => {
                    let offset = context
                        .exec_imm32(instr, 0)
                        .ok_or_else(|| VmError::InvalidOperand)?;
                    let cond = context
                        .exec_register(instr, 1)
                        .ok_or_else(|| VmError::InvalidOperand)?;
                    let frame = &mut stack[top_idx];
                    let taken = read_register(frame, cond)?.to_boolean(&self.gc_heap);
                    if jit_installed && let Some(cell) = feedback {
                        cell.record_branch(taken);
                    }
                    if taken {
                        apply_branch(frame, offset, &self.interrupt)?;
                        if offset < 0
                            && let Some(Some(value)) =
                                self.note_backedge_and_maybe_osr(stack, context, top_idx)?
                        {
                            return Ok(value);
                        }
                    } else {
                        frame.advance_pc()?;
                    }
                    continue;
                }
                Op::JumpIfFalse => {
                    let offset = context
                        .exec_imm32(instr, 0)
                        .ok_or_else(|| VmError::InvalidOperand)?;
                    let cond = context
                        .exec_register(instr, 1)
                        .ok_or_else(|| VmError::InvalidOperand)?;
                    let frame = &mut stack[top_idx];
                    let taken = !read_register(frame, cond)?.to_boolean(&self.gc_heap);
                    if jit_installed && let Some(cell) = feedback {
                        cell.record_branch(taken);
                    }
                    if taken {
                        apply_branch(frame, offset, &self.interrupt)?;
                        if offset < 0
                            && let Some(Some(value)) =
                                self.note_backedge_and_maybe_osr(stack, context, top_idx)?
                        {
                            return Ok(value);
                        }
                    } else {
                        frame.advance_pc()?;
                    }
                    continue;
                }
                Op::JumpIfNullish => {
                    let offset = context
                        .exec_imm32(instr, 0)
                        .ok_or_else(|| VmError::InvalidOperand)?;
                    let cond = context
                        .exec_register(instr, 1)
                        .ok_or_else(|| VmError::InvalidOperand)?;
                    let frame = &mut stack[top_idx];
                    if read_register(frame, cond)?.is_nullish() {
                        apply_branch(frame, offset, &self.interrupt)?;
                    } else {
                        frame.advance_pc()?;
                    }
                    continue;
                }
                Op::LoadLocal => {
                    let dst = context
                        .exec_register(instr, 0)
                        .ok_or_else(|| VmError::InvalidOperand)?;
                    let idx = context
                        .exec_imm32(instr, 1)
                        .ok_or_else(|| VmError::InvalidOperand)?;
                    let frame = &mut stack[top_idx];
                    let value = *read_register(frame, idx as u16)?;
                    write_register(frame, dst, value)?;
                    frame.advance_pc()?;
                    continue;
                }
                Op::StoreLocal => {
                    let src = context
                        .exec_register(instr, 0)
                        .ok_or_else(|| VmError::InvalidOperand)?;
                    let idx = context
                        .exec_imm32(instr, 1)
                        .ok_or_else(|| VmError::InvalidOperand)?;
                    let frame = &mut stack[top_idx];
                    let value = *read_register(frame, src)?;
                    write_register(frame, idx as u16, value)?;
                    frame.advance_pc()?;
                    continue;
                }
                Op::TdzError => {
                    let local_index = context
                        .exec_imm32(instr, 0)
                        .ok_or_else(|| VmError::InvalidOperand)?
                        as u32;
                    return Err(VmError::TemporalDeadZone { local_index });
                }
                Op::Add => {
                    let (dst, lhs, rhs) = context
                        .exec_register3(instr)
                        .ok_or_else(|| VmError::InvalidOperand)?;
                    let frame = &mut stack[top_idx];
                    self.run_add_regs(frame, dst, lhs, rhs, feedback)?;
                    continue;
                }
                Op::Sub => {
                    let (dst, lhs, rhs) = context
                        .exec_register3(instr)
                        .ok_or_else(|| VmError::InvalidOperand)?;
                    let frame = &mut stack[top_idx];
                    self.run_numeric_regs(
                        frame,
                        dst,
                        lhs,
                        rhs,
                        number::sub,
                        bigint_sub_op,
                        feedback,
                    )?;
                    continue;
                }
                Op::Mul => {
                    let (dst, lhs, rhs) = context
                        .exec_register3(instr)
                        .ok_or_else(|| VmError::InvalidOperand)?;
                    let frame = &mut stack[top_idx];
                    self.run_numeric_regs(
                        frame,
                        dst,
                        lhs,
                        rhs,
                        number::mul,
                        bigint_mul_op,
                        feedback,
                    )?;
                    continue;
                }
                Op::Div => {
                    let (dst, lhs, rhs) = context
                        .exec_register3(instr)
                        .ok_or_else(|| VmError::InvalidOperand)?;
                    let frame = &mut stack[top_idx];
                    self.run_numeric_regs(
                        frame,
                        dst,
                        lhs,
                        rhs,
                        number::div,
                        bigint::ops::div,
                        feedback,
                    )?;
                    continue;
                }
                Op::Rem => {
                    let (dst, lhs, rhs) = context
                        .exec_register3(instr)
                        .ok_or_else(|| VmError::InvalidOperand)?;
                    let frame = &mut stack[top_idx];
                    self.run_numeric_regs(
                        frame,
                        dst,
                        lhs,
                        rhs,
                        number::rem,
                        bigint::ops::rem,
                        feedback,
                    )?;
                    continue;
                }
                Op::Pow => {
                    let (dst, lhs, rhs) = context
                        .exec_register3(instr)
                        .ok_or_else(|| VmError::InvalidOperand)?;
                    let frame = &mut stack[top_idx];
                    self.run_numeric_regs(
                        frame,
                        dst,
                        lhs,
                        rhs,
                        number::pow,
                        bigint::ops::pow,
                        feedback,
                    )?;
                    continue;
                }
                Op::BitwiseAnd => {
                    let (dst, lhs, rhs) = context
                        .exec_register3(instr)
                        .ok_or_else(|| VmError::InvalidOperand)?;
                    let frame = &mut stack[top_idx];
                    self.run_numeric_regs(
                        frame,
                        dst,
                        lhs,
                        rhs,
                        number::bitwise_and,
                        bigint_and_op,
                        feedback,
                    )?;
                    continue;
                }
                Op::BitwiseOr => {
                    let (dst, lhs, rhs) = context
                        .exec_register3(instr)
                        .ok_or_else(|| VmError::InvalidOperand)?;
                    let frame = &mut stack[top_idx];
                    self.run_numeric_regs(
                        frame,
                        dst,
                        lhs,
                        rhs,
                        number::bitwise_or,
                        bigint_or_op,
                        feedback,
                    )?;
                    continue;
                }
                Op::BitwiseXor => {
                    let (dst, lhs, rhs) = context
                        .exec_register3(instr)
                        .ok_or_else(|| VmError::InvalidOperand)?;
                    let frame = &mut stack[top_idx];
                    self.run_numeric_regs(
                        frame,
                        dst,
                        lhs,
                        rhs,
                        number::bitwise_xor,
                        bigint_xor_op,
                        feedback,
                    )?;
                    continue;
                }
                Op::Shl => {
                    let (dst, lhs, rhs) = context
                        .exec_register3(instr)
                        .ok_or_else(|| VmError::InvalidOperand)?;
                    let frame = &mut stack[top_idx];
                    self.run_numeric_regs(
                        frame,
                        dst,
                        lhs,
                        rhs,
                        number::shl,
                        bigint::ops::shl,
                        feedback,
                    )?;
                    continue;
                }
                Op::Shr => {
                    let (dst, lhs, rhs) = context
                        .exec_register3(instr)
                        .ok_or_else(|| VmError::InvalidOperand)?;
                    let frame = &mut stack[top_idx];
                    self.run_numeric_regs(
                        frame,
                        dst,
                        lhs,
                        rhs,
                        number::shr_arith,
                        bigint::ops::shr,
                        feedback,
                    )?;
                    continue;
                }
                Op::LessThan | Op::LessEq | Op::GreaterThan | Op::GreaterEq => {
                    let (dst, lhs, rhs) = context
                        .exec_register3(instr)
                        .ok_or_else(|| VmError::InvalidOperand)?;
                    let frame = &mut stack[top_idx];
                    self.run_compare_regs(frame, dst, lhs, rhs, op, feedback)?;
                    continue;
                }
                Op::Ushr => {
                    let (dst, lhs, rhs) = context
                        .exec_register3(instr)
                        .ok_or_else(|| VmError::InvalidOperand)?;
                    let frame = &mut stack[top_idx];
                    self.run_ushr_regs(frame, dst, lhs, rhs, feedback)?;
                    continue;
                }
                Op::Neg => {
                    let dst = context
                        .exec_register(instr, 0)
                        .ok_or_else(|| VmError::InvalidOperand)?;
                    let src = context
                        .exec_register(instr, 1)
                        .ok_or_else(|| VmError::InvalidOperand)?;
                    let frame = &mut stack[top_idx];
                    self.run_neg_regs(frame, dst, src)?;
                    continue;
                }
                Op::BitwiseNot => {
                    let dst = context
                        .exec_register(instr, 0)
                        .ok_or_else(|| VmError::InvalidOperand)?;
                    let src = context
                        .exec_register(instr, 1)
                        .ok_or_else(|| VmError::InvalidOperand)?;
                    let frame = &mut stack[top_idx];
                    self.run_bitwise_not_regs(frame, dst, src)?;
                    continue;
                }
                Op::Equal | Op::NotEqual | Op::LooseEqual | Op::LooseNotEqual | Op::SameValue => {
                    let (dst, lhs, rhs) = context
                        .exec_register3(instr)
                        .ok_or_else(|| VmError::InvalidOperand)?;
                    let frame = &mut stack[top_idx];
                    match op {
                        Op::Equal => self.run_equal_regs(frame, dst, lhs, rhs, false, feedback)?,
                        Op::NotEqual => {
                            self.run_equal_regs(frame, dst, lhs, rhs, true, feedback)?
                        }
                        Op::LooseEqual => {
                            self.run_loose_equal_regs(
                                context, frame, dst, lhs, rhs, false, feedback,
                            )?;
                        }
                        Op::LooseNotEqual => {
                            self.run_loose_equal_regs(
                                context, frame, dst, lhs, rhs, true, feedback,
                            )?;
                        }
                        Op::SameValue => self.run_same_value_regs(frame, dst, lhs, rhs)?,
                        _ => unreachable!("equality opcode group"),
                    }
                    continue;
                }
                Op::ArrayLength => {
                    let dst = context
                        .exec_register(instr, 0)
                        .ok_or_else(|| VmError::InvalidOperand)?;
                    let src = context
                        .exec_register(instr, 1)
                        .ok_or_else(|| VmError::InvalidOperand)?;
                    self.run_array_length_reg(&mut stack[top_idx], dst, src)?;
                    continue;
                }
                Op::IsArray => {
                    let dst = context
                        .exec_register(instr, 0)
                        .ok_or_else(|| VmError::InvalidOperand)?;
                    let src = context
                        .exec_register(instr, 1)
                        .ok_or_else(|| VmError::InvalidOperand)?;
                    self.run_is_array_reg(&mut stack[top_idx], dst, src)?;
                    continue;
                }
                Op::IsEvalIntrinsic => {
                    let dst = context
                        .exec_register(instr, 0)
                        .ok_or(VmError::InvalidOperand)?;
                    let src = context
                        .exec_register(instr, 1)
                        .ok_or(VmError::InvalidOperand)?;
                    self.run_is_eval_intrinsic_reg(stack, top_idx, dst, src)?;
                    continue;
                }
                Op::MakeClosure => {
                    let operands = function.operand_view(instr);
                    let frame = &mut stack[top_idx];
                    self.run_make_closure_operands(context, frame, operands)?;
                    continue;
                }
                Op::ArrayBufferCall => {
                    let operands = function.operand_view(instr);
                    self.run_array_buffer_static_call_operands(stack, operands)?;
                    continue;
                }
                Op::SharedArrayBufferCall => {
                    let operands = function.operand_view(instr);
                    self.run_shared_array_buffer_static_call_operands(stack, operands)?;
                    continue;
                }
                Op::BigIntCall | Op::DataViewCall => {
                    let operands = function.operand_view(instr);
                    let frame = &mut stack[top_idx];
                    self.run_static_call_operands(op, context, frame, operands)?;
                    continue;
                }
                Op::ArrayConstruct | Op::ArrayFrom | Op::ArrayOf => {
                    let operands = function.operand_view(instr);
                    self.run_array_static_operands(op, context, stack, operands)?;
                    continue;
                }
                Op::ForInKeys => {
                    let operands = function.operand_view(instr);
                    self.run_for_in_keys_operands(context, stack, operands)?;
                    continue;
                }
                Op::CopyDataProperties => {
                    let operands = function.operand_view(instr);
                    self.run_copy_data_properties_operands(context, stack, operands)?;
                    continue;
                }
                Op::StarReexport => {
                    let operands = function.operand_view(instr);
                    self.run_star_reexport_operands(context, stack, operands)?;
                    continue;
                }
                Op::DefineOwnProperty => {
                    let operands = function.operand_view(instr);
                    self.run_define_own_property_operands(context, stack, operands)?;
                    continue;
                }
                Op::QueueMicrotask => {
                    let operands = function.operand_view(instr);
                    let frame = &mut stack[top_idx];
                    self.run_queue_microtask_operands(context, frame, operands)?;
                    continue;
                }
                Op::PromiseNew => {
                    let operands = function.operand_view(instr);
                    self.run_promise_new_operands(context, stack, operands)?;
                    continue;
                }
                Op::PromiseCall => {
                    let operands = function.operand_view(instr);
                    self.run_promise_call_operands(context, stack, operands)?;
                    continue;
                }
                Op::ImportNamespaceDynamic => {
                    let operands = function.operand_view(instr);
                    self.run_import_namespace_dynamic_operands(context, stack, top_idx, operands)?;
                    continue;
                }
                Op::BindFunction => {
                    let operands = function.operand_view(instr);
                    self.drive_bind_function(stack, context, operands)?;
                    continue;
                }
            }
        }
    }
}
