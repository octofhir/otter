//! The bytecode dispatch loop.
//!
//! # Contents
//! `dispatch_loop_inner`: one `match` arm per opcode, inline caches,
//! the JIT tier-up/backedge hooks, and additive optimizing-tier back-edge
//! accounting. Deliberately a single function — splitting it would defeat the
//! dispatch-locality the interpreter depends on.
#![allow(unused_imports)]
use super::call_dispatch::DispatchOutcome;
use crate::*;

impl Interpreter {
    /// Deliver one step event to an installed tracer.
    ///
    /// Out of line by design: the dispatch loop guards this with a
    /// loop-invariant bool, and building the event inline would spend hot-loop
    /// registers and instruction cache on every run that installs no tracer.
    #[inline(never)]
    fn emit_step_trace(
        &mut self,
        context: &ExecutionContext,
        stack: &ActivationStack,
        top_idx: usize,
        function: &CodeBlock,
        function_id: u32,
        idx: usize,
        instr: &CodeBlockInstruction,
    ) -> Result<(), VmError> {
        let function_name = context
            .function(function_id)
            .map(|f| f.name.as_str())
            .unwrap_or("<unknown>");
        let event = inspect::StepEvent {
            frame_depth: stack.len(),
            function_id,
            function_name,
            byte_pc: function
                .instruction_byte_pc(idx)
                .ok_or(VmError::MissingReturn)?,
            op: function.op(instr),
            operands: function.operand_view(instr),
            register_window: &stack[top_idx].registers,
        };
        if let Some(tracer) = self.tracer.as_deref_mut() {
            tracer.on_step(&event);
        }
        Ok(())
    }

    /// Run the activation above `floor` until it returns, stages a call, or
    /// fails.
    ///
    /// On a failure the activation's PC is the failing instruction, and
    /// while a staged call runs it is the staging instruction, whatever the
    /// instruction had already done to the PC: the handler table is keyed by
    /// those instructions.
    pub(crate) fn dispatch_loop_inner(
        &mut self,
        entry_context: &ExecutionContext,
        stack: &mut ActivationStack,
        floor: ActivationFloor,
    ) -> Result<DispatchOutcome, VmError> {
        let mut tick_pc = u32::MAX;
        let outcome = self.dispatch_ticks(entry_context, stack, floor, &mut tick_pc);
        let staged = matches!(outcome, Ok(DispatchOutcome::Call));
        if (staged || outcome.is_err())
            && tick_pc != u32::MAX
            && !stack.is_at_floor(floor)
            && let Some(frame) = stack.last_mut()
            && frame.pc != tick_pc
        {
            if staged {
                frame.header.flags = frame
                    .header
                    .flags
                    .with(crate::native_abi::NativeFrameFlags::ADVANCE_ON_RESUME);
            }
            frame.pc = tick_pc;
        }
        outcome
    }

    fn dispatch_ticks(
        &mut self,
        entry_context: &ExecutionContext,
        stack: &mut ActivationStack,
        floor: ActivationFloor,
        tick_pc: &mut u32,
    ) -> Result<DispatchOutcome, VmError> {
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
        let enforce_budget = self.work_budget.enforces_on_exceedance();
        // Like `enforce_budget`, these installation states are fixed for the
        // duration of a dispatch run — a JIT hook, CPU profiler, or step tracer
        // is attached between turns, never mid-loop. Hoisting the `is_some`
        // probes into loop-invariant bools turns three per-instruction
        // `self`-field loads into register-resident branches that collapse to
        // not-taken in the common (no hook / no profiler / no tracer) case.
        let jit_installed = self.jit_hook.is_some();
        let has_profiler = self.cpu_profiler.is_some();
        let has_tracer = self.tracer.is_some();
        // Union of the per-instruction hooks, tested once per dispatched
        // instruction in place of three separate not-taken branches.
        let has_hooks = enforce_budget || has_profiler || has_tracer;
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
        // Cooperative cancellation is polled where execution can repeat: this
        // entry, every back-edge (`apply_branch`), and `Op::TailCall`. A run
        // that reaches none of them is bounded by the instruction stream and the
        // stack-depth limit, so the per-instruction poll it replaces bought no
        // extra termination guarantee.
        if self.interrupt.is_set() {
            return Err(VmError::Interrupted);
        }
        let mut cache_valid = false;
        let mut cache_function_id: u32 = u32::MAX;
        let mut cache_depth: usize = 0;
        let mut cache_foreign = false;
        let mut cache_function: *const CodeBlock = std::ptr::null();
        loop {
            if stack.has_pending_call() {
                return Ok(DispatchOutcome::Call);
            }
            if stack.is_at_floor(floor) {
                // Defensive: unwind paths (throw / finally) can
                // pop the last frame without writing back to a
                // caller register. Surface `undefined` so
                // the dispatch loop terminates cleanly instead of
                // panicking on the next `stack.len() - 1`. Tests
                // that rely on the throw escape will already have
                // flowed through `unwind_throw` and surfaced as
                // `VmError::Uncaught`; this guard catches the
                // residual "fell off the bottom" path and treats
                // it as completion. An async activation that absorbed its
                // throw completes with its rejected result promise.
                let result = self
                    .completed_activation_result
                    .take()
                    .unwrap_or_else(Value::undefined);
                return Ok(DispatchOutcome::Returned(result));
            }
            let depth = stack.len();
            let top_idx = depth - 1;
            // SAFETY: the `is_empty()` guard above proves a live top frame this
            // tick; one unchecked read serves both fields.
            let (function_id, pc) = {
                let top = unsafe { stack.top_unchecked() };
                (top.function_id, top.pc)
            };
            *tick_pc = pc;
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
                                Ok(code_space::ResolvedCtx::Owned(owned)) => {
                                    // Foreign chunks linked after this loop
                                    // started (eval during this turn) carry
                                    // IC sites past the entry chunk's range.
                                    self.ensure_method_feedback_context(&owned);
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
                    if depth32 > self.work_budget_stats.max_stack_depth_observed {
                        self.work_budget_stats.max_stack_depth_observed = depth32;
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
            // Every opcode has a static base work charge. The max-stack-depth
            // sample lives on the frame-resolution miss branch (depth changes
            // only across frame transitions), and the checkpoint stays gated
            // on `enforce_budget` in the default Observe mode.
            self.work_budget_stats.record_work(instr.reductions());
            // Budget enforcement, CPU sampling, and step tracing are each
            // installed between turns, never mid-loop. Testing their union once
            // keeps the common no-hook instruction at a single not-taken branch
            // instead of three; the individual tests only run when some hook is
            // actually present.
            if has_hooks {
                if enforce_budget {
                    self.enforce_work_budget_checkpoint()?;
                }
                if has_profiler && let Some(mut profiler) = self.cpu_profiler.take() {
                    profiler.maybe_sample(self, context);
                    self.cpu_profiler = Some(profiler);
                }
                // The body is kept out of line so building the event costs the
                // hot loop nothing.
                if has_tracer {
                    self.emit_step_trace(
                        context,
                        stack,
                        top_idx,
                        function,
                        function_id,
                        idx,
                        instr,
                    )?;
                }
            }

            // Stack-modifying opcodes go first so we don't hold a
            // `&mut Frame` borrow while pushing / popping.
            match op {
                Op::ReturnValue | Op::Return => {
                    let src = instr.reg(0);
                    let value = stack[top_idx]
                        .registers
                        .get(src as usize)
                        .cloned()
                        .ok_or_else(|| VmError::InvalidOperand)?;
                    if let Some(popped) = self.pop_frame_above(stack, floor, value, None)? {
                        return Ok(DispatchOutcome::Returned(popped));
                    }
                    continue;
                }
                Op::ReturnUndefined => {
                    if let Some(popped) =
                        self.pop_frame_above(stack, floor, Value::undefined(), None)?
                    {
                        return Ok(DispatchOutcome::Returned(popped));
                    }
                    continue;
                }
                // §10.2.2 [[Construct]] steps 10–12 for a derived constructor:
                // the completion reads `this` from the `DerivedThis` slot.
                Op::ReturnDerived => {
                    let (value_reg, ctx_reg) = (instr.reg(0), instr.reg(1));
                    let coord = otter_bytecode::ContextCoord::from_imm32(instr.imm(2))
                        .ok_or(VmError::InvalidOperand)?;
                    let frame = &stack[top_idx];
                    let value = *read_register(frame, value_reg)?;
                    let ctx = *read_register(frame, ctx_reg)?;
                    let derived_this = self.derived_this_slot_value(ctx, coord)?;
                    if let Some(popped) =
                        self.pop_frame_above(stack, floor, value, Some(derived_this))?
                    {
                        return Ok(DispatchOutcome::Returned(popped));
                    }
                    continue;
                }
                Op::Call | Op::CallWithThis => {
                    // Both forms already own the loaded callable. Record its
                    // exact native identity before execution can collect;
                    // bytecode dispatch still supplies the resolved frame id.
                    if jit_installed {
                        self.record_call_attempt_feedback(
                            function,
                            instr.instruction_pc,
                            function_id,
                        );
                    }
                    // `f.call(this, ...)` loaded as a value runs `f`: the site
                    // records that function, read before the call can collect.
                    let function_call_target = (jit_installed && op == Op::CallWithThis)
                        .then(|| {
                            let frame = stack.get(top_idx)?;
                            let register = |index| {
                                register_operand(function.operand(instr, index))
                                    .ok()
                                    .and_then(|r| frame.registers.get(r as usize).copied())
                            };
                            self.function_prototype_call_target(register(1)?, register(2)?)
                        })
                        .flatten();
                    let static_native_target = if jit_installed {
                        register_operand(function.operand(instr, 1))
                            .ok()
                            .and_then(|register| {
                                stack
                                    .get(top_idx)
                                    .and_then(|frame| frame.registers.get(register as usize))
                                    .copied()
                            })
                            .and_then(Value::as_native_function)
                            .and_then(|native| {
                                crate::jit_static_native::jit_static_call_target(
                                    native,
                                    &self.gc_heap,
                                )
                            })
                    } else {
                        None
                    };
                    if op == Op::CallWithThis {
                        self.do_call_with_this_exec(stack, function, instr)?;
                    } else {
                        self.do_call_exec(stack, function, instr)?;
                    }
                    // Record the resolved typed target before a tier-up hook
                    // consumes a newly pushed bytecode frame. Static natives
                    // complete synchronously and therefore leave stack depth
                    // unchanged.
                    let staged_target = self.staged_bytecode_target(stack);
                    if jit_installed
                        && let Some(target) = if function_call_target.is_some() {
                            function_call_target
                        } else if let Some(function_id) = staged_target {
                            Some(crate::feedback::OrdinaryCallTarget::Bytecode(function_id))
                        } else {
                            static_native_target.map(|declaration| {
                                crate::feedback::OrdinaryCallTarget::StaticNative(
                                    declaration.leaf_stub_id,
                                )
                            })
                        }
                    {
                        let transition = self.record_ordinary_call_feedback(
                            function,
                            instr.instruction_pc,
                            target,
                        );
                        if transition.evict_for_reopt() {
                            self.evict_compiled_for_reopt(function_id);
                        }
                    }
                    continue;
                }
                Op::TailCall => {
                    // Cooperative cancellation is polled at back-edges
                    // (`apply_branch`) and here: unbounded execution needs
                    // either a back-edge or an unbounded tail-call chain, and
                    // every other shape terminates against the instruction
                    // stream or the stack-depth limit.
                    if self.interrupt.is_set() {
                        return Err(VmError::Interrupted);
                    }
                    if jit_installed {
                        self.record_call_attempt_feedback(
                            function,
                            instr.instruction_pc,
                            function_id,
                        );
                    }
                    self.do_tail_call_exec(stack, function, instr)?;
                    if jit_installed && let Some(function_id) = self.staged_bytecode_target(stack) {
                        let target = crate::feedback::OrdinaryCallTarget::Bytecode(function_id);
                        let transition = self.record_ordinary_call_feedback(
                            function,
                            instr.instruction_pc,
                            target,
                        );
                        if transition.evict_for_reopt() {
                            self.evict_compiled_for_reopt(function_id);
                        }
                    }
                    continue;
                }
                Op::CallForwardArguments => {
                    if jit_installed {
                        self.record_call_attempt_feedback(
                            function,
                            instr.instruction_pc,
                            function_id,
                        );
                    }
                    self.do_call_forward_arguments_exec(stack, context, function, instr)?;
                    if jit_installed && let Some(function_id) = self.staged_bytecode_target(stack) {
                        let target = crate::feedback::OrdinaryCallTarget::Bytecode(function_id);
                        let transition = self.record_ordinary_call_feedback(
                            function,
                            instr.instruction_pc,
                            target,
                        );
                        if transition.evict_for_reopt() {
                            self.evict_compiled_for_reopt(function_id);
                        }
                    }
                    continue;
                }
                Op::CallMethodValue => {
                    if jit_installed {
                        self.record_call_attempt_feedback(
                            function,
                            instr.instruction_pc,
                            function_id,
                        );
                    }
                    let feedback_site = instr.property_ic_site();
                    // Capture the receiver/prototype layout before the call for
                    // method-inline feedback (the receiver register lives in the
                    // caller frame, which `do_call_method_value` leaves in place
                    // under the new callee frame; the receiver handle may move
                    // during the call, so the prototype shape and method slot are
                    // resolved here while it is still valid).
                    let capture = jit_installed
                        && feedback_site
                            .is_some_and(|site| !self.method_site_feedback_saturated(site));
                    let receiver_value = jit_installed
                        .then(|| register_operand(function.operand(instr, 1)).ok())
                        .flatten()
                        .and_then(|r| {
                            stack
                                .get(top_idx)
                                .and_then(|f| f.registers.get(r as usize).copied())
                        });
                    let mut receiver = receiver_value.filter(|_| capture);
                    let name_idx = const_operand(function.operand(instr, 2)).ok();
                    // The receiver is refreshed in place: resolving the site can
                    // migrate a dictionary-mode receiver onto the shaped path,
                    // and that allocation may relocate it.
                    let method_site = match (receiver.as_mut(), name_idx) {
                        (Some(recv), Some(name_idx)) => {
                            self.method_site_for_receiver(context, function_id, name_idx, recv)
                        }
                        _ => None,
                    };
                    // A declared native leaf completes synchronously and pushes
                    // no frame, so its identity has to be classified here, while
                    // the receiver is still live, or the site records nothing.
                    let native_leaf = match (receiver, name_idx, method_site.as_ref()) {
                        (Some(recv), Some(name_idx), Some(_)) => {
                            const_operand(function.operand(instr, 3))
                                .ok()
                                .and_then(|argc| {
                                    self.method_slot_native_leaf(
                                        context,
                                        function_id,
                                        name_idx,
                                        argc as usize,
                                        recv,
                                    )
                                })
                        }
                        _ => None,
                    };
                    self.do_call_method_value_exec(stack, context, function, instr)?;
                    // Feedback comes from the staged request. `f.call(...)` stages `%Function.prototype.call%` with `f`
                    // as its receiver; the site records the function it runs.
                    let function_call_target = jit_installed
                        .then(|| {
                            stack
                                .staged_request_mut()
                                .map(|request| (request.callee, request.receiver))
                        })
                        .flatten()
                        .and_then(|(callee, receiver)| {
                            self.function_prototype_call_target(callee, receiver)
                        });
                    if let Some(target) = function_call_target {
                        let transition = self.record_ordinary_call_feedback(
                            function,
                            instr.instruction_pc,
                            target,
                        );
                        if transition.evict_for_reopt() {
                            self.evict_compiled_for_reopt(function_id);
                        }
                    } else if jit_installed
                        && let Some(method_fid) = self.staged_bytecode_target(stack)
                    {
                        if let (Some(feedback_site), Some(site)) = (feedback_site, method_site) {
                            let changed = self.note_method_target(feedback_site, method_fid, site);
                            self.commit_method_call_feedback_transition(
                                function,
                                function_id,
                                changed,
                            );
                        }
                    } else if jit_installed
                        && let (Some(feedback_site), Some(site), Some(stub_id)) =
                            (feedback_site, method_site, native_leaf)
                    {
                        let changed =
                            self.record_method_native_leaf_feedback(feedback_site, stub_id, site);
                        self.commit_method_call_feedback_transition(function, function_id, changed);
                    }
                    continue;
                }
                Op::CallSpread => {
                    let operands = function.operand_view(instr);
                    self.do_call_spread(stack, operands)?;
                    if jit_installed && let Some(function_id) = self.staged_bytecode_target(stack) {
                        let transition = self.record_ordinary_call_feedback(
                            function,
                            instr.instruction_pc,
                            crate::feedback::OrdinaryCallTarget::Bytecode(function_id),
                        );
                        if transition.evict_for_reopt() {
                            self.evict_compiled_for_reopt(function_id);
                        }
                    }
                    continue;
                }
                Op::New => {
                    if jit_installed {
                        self.record_call_attempt_feedback(
                            function,
                            instr.instruction_pc,
                            function_id,
                        );
                    }
                    let direct_construct_fid = if jit_installed {
                        register_operand(function.operand(instr, 1))
                            .ok()
                            .and_then(|register| {
                                stack
                                    .get(top_idx)
                                    .and_then(|frame| frame.registers.get(register as usize))
                                    .copied()
                            })
                            .and_then(|value| {
                                value
                                    .as_function()
                                    .or_else(|| {
                                        value
                                            .as_closure(&self.gc_heap)
                                            .map(|closure| closure.function_id())
                                    })
                                    .or_else(|| {
                                        value.as_class_constructor().and_then(|class| {
                                            let ctor = class.ctor(&self.gc_heap);
                                            ctor.as_function().or_else(|| {
                                                ctor.as_closure(&self.gc_heap)
                                                    .map(|closure| closure.function_id())
                                            })
                                        })
                                    })
                            })
                    } else {
                        None
                    };
                    self.do_construct_exec(stack, function, instr)?;
                    // Tier-up hook, mirroring `Op::Call`: a bytecode
                    // constructor frame pushed by `new` can enter JIT at pc=0.
                    if jit_installed
                        && let Some(function_id) = self.staged_bytecode_target(stack)
                        && direct_construct_fid == Some(function_id)
                    {
                        let transition = self.record_ordinary_call_feedback(
                            function,
                            instr.instruction_pc,
                            crate::feedback::OrdinaryCallTarget::Bytecode(function_id),
                        );
                        if transition.evict_for_reopt() {
                            self.evict_compiled_for_reopt(function_id);
                        }
                    }

                    continue;
                }
                Op::SuperConstruct => {
                    let direct_construct_fid = if jit_installed {
                        register_operand(function.operand(instr, 1))
                            .ok()
                            .and_then(|register| {
                                stack
                                    .get(top_idx)
                                    .and_then(|frame| frame.registers.get(register as usize))
                                    .copied()
                            })
                            .and_then(|value| {
                                value
                                    .as_function()
                                    .or_else(|| {
                                        value
                                            .as_closure(&self.gc_heap)
                                            .map(|closure| closure.function_id())
                                    })
                                    .or_else(|| {
                                        value.as_class_constructor().and_then(|class| {
                                            let ctor = class.ctor(&self.gc_heap);
                                            ctor.as_function().or_else(|| {
                                                ctor.as_closure(&self.gc_heap)
                                                    .map(|closure| closure.function_id())
                                            })
                                        })
                                    })
                            })
                    } else {
                        None
                    };
                    self.do_super_construct_exec(stack, function, instr)?;
                    if jit_installed
                        && let Some(function_id) = self.staged_bytecode_target(stack)
                        && direct_construct_fid == Some(function_id)
                    {
                        let transition = self.record_ordinary_call_feedback(
                            function,
                            instr.instruction_pc,
                            crate::feedback::OrdinaryCallTarget::Bytecode(function_id),
                        );
                        if transition.evict_for_reopt() {
                            self.evict_compiled_for_reopt(function_id);
                        }
                    }

                    continue;
                }
                Op::NewSpread => {
                    let direct_construct_fid = if jit_installed {
                        register_operand(function.operand(instr, 1))
                            .ok()
                            .and_then(|register| stack[top_idx].registers.get(register as usize))
                            .copied()
                            .and_then(|value| {
                                value
                                    .as_function()
                                    .or_else(|| {
                                        value
                                            .as_closure(&self.gc_heap)
                                            .map(|closure| closure.function_id())
                                    })
                                    .or_else(|| {
                                        value.as_class_constructor().and_then(|class| {
                                            let ctor = class.ctor(&self.gc_heap);
                                            ctor.as_function().or_else(|| {
                                                ctor.as_closure(&self.gc_heap)
                                                    .map(|closure| closure.function_id())
                                            })
                                        })
                                    })
                            })
                    } else {
                        None
                    };
                    let operands = function.operand_view(instr);
                    self.do_construct_spread(stack, operands)?;
                    if jit_installed
                        && let Some(function_id) = self.staged_bytecode_target(stack)
                        && direct_construct_fid == Some(function_id)
                    {
                        let transition = self.record_ordinary_call_feedback(
                            function,
                            instr.instruction_pc,
                            crate::feedback::OrdinaryCallTarget::Bytecode(function_id),
                        );
                        if transition.evict_for_reopt() {
                            self.evict_compiled_for_reopt(function_id);
                        }
                    }

                    continue;
                }
                Op::SuperConstructSpread => {
                    let direct_construct_fid = if jit_installed {
                        register_operand(function.operand(instr, 1))
                            .ok()
                            .and_then(|register| stack[top_idx].registers.get(register as usize))
                            .copied()
                            .and_then(|value| {
                                value
                                    .as_function()
                                    .or_else(|| {
                                        value
                                            .as_closure(&self.gc_heap)
                                            .map(|closure| closure.function_id())
                                    })
                                    .or_else(|| {
                                        value.as_class_constructor().and_then(|class| {
                                            let ctor = class.ctor(&self.gc_heap);
                                            ctor.as_function().or_else(|| {
                                                ctor.as_closure(&self.gc_heap)
                                                    .map(|closure| closure.function_id())
                                            })
                                        })
                                    })
                            })
                    } else {
                        None
                    };
                    let operands = function.operand_view(instr);
                    self.do_super_construct_spread(stack, operands)?;
                    if jit_installed
                        && let Some(function_id) = self.staged_bytecode_target(stack)
                        && direct_construct_fid == Some(function_id)
                    {
                        let transition = self.record_ordinary_call_feedback(
                            function,
                            instr.instruction_pc,
                            crate::feedback::OrdinaryCallTarget::Bytecode(function_id),
                        );
                        if transition.evict_for_reopt() {
                            self.evict_compiled_for_reopt(function_id);
                        }
                    }

                    continue;
                }
                Op::BindThisValue => {
                    let src = instr.reg(0);
                    self.run_bind_this_value_reg(stack, top_idx, src)?;
                    continue;
                }
                Op::Throw => {
                    let src = instr.reg(0);
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
                    if self.pending_uncaught_frames.is_none() {
                        self.pending_uncaught_frames =
                            Some(self.snapshot_active_frames(context, usize::MAX));
                    }
                    let unwind = self.unwind_throw_above(
                        context,
                        stack,
                        floor,
                        value,
                        crate::activation_stack::ThrowSite::Instruction,
                    );
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
                Op::Await => {
                    let dst = instr.reg(0);
                    let src = instr.reg(1);
                    let awaited = *read_register(&stack[top_idx], src)?;
                    // A suspended async activation completes with its result
                    // promise, rooted across the await's allocations.
                    let result = self
                        .frame_cold(&stack[top_idx])
                        .and_then(|cold| cold.async_state.as_ref())
                        .map_or(Value::undefined(), |state| {
                            Value::promise(state.result_promise)
                        });
                    let anchor = self.push_iteration_anchor(result) - 1;
                    let awaited_result = self.do_await(stack, context, dst, awaited);
                    let result = self.iteration_anchor(anchor);
                    self.pop_iteration_anchors_to(anchor);
                    awaited_result?;
                    if stack.is_at_floor(floor) {
                        return Ok(DispatchOutcome::Returned(result));
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
                    let (kind_dst, value_dst, src) = instr.reg3();
                    let yielded = *read_register(&stack[top_idx], src)?;
                    let owner = self
                        .frame_generator_owner(&stack[top_idx])
                        .ok_or(VmError::TypeMismatch)?;
                    let frame = stack.last_mut().ok_or_else(|| VmError::InvalidOperand)?;
                    frame.advance_pc()?;
                    let popped = stack.pop().expect("frame present");
                    let detached_cold = self.frame_detach_cold(popped);
                    let popped = self.park_active_frame(popped);
                    owner.park_after_yield_delegate(
                        &mut self.gc_heap,
                        popped,
                        detached_cold,
                        kind_dst,
                        value_dst,
                        yielded,
                    );
                    // §27.6.3.8 AsyncGeneratorYield — a delegating
                    // suspension settles the outer request with the inner
                    // result's value AS IS: only the plain-yield evaluation
                    // awaits its operand first, so `yield*` never unwraps a
                    // promise handed back by a manual async iterator. A sync
                    // delegation bubbles the inner record to
                    // `resume_generator`.
                    if owner.is_async(&self.gc_heap) {
                        owner.set_async_state(
                            &mut self.gc_heap,
                            crate::generator::AsyncGeneratorState::SuspendedYield,
                        );
                        self.async_generator_complete_step(context, &owner, Ok(yielded), false)?;
                        self.async_generator_resume_next(stack, context, &owner)?;
                    }
                    return Ok(DispatchOutcome::Returned(Value::undefined()));
                }
                Op::Yield => {
                    let (kind_dst, value_dst, src) = instr.reg3();
                    let yielded = *read_register(&stack[top_idx], src)?;
                    let owner = self
                        .frame_generator_owner(&stack[top_idx])
                        .ok_or(VmError::TypeMismatch)?;
                    let frame = stack.last_mut().ok_or_else(|| VmError::InvalidOperand)?;
                    frame.advance_pc()?;
                    let popped = stack.pop().expect("frame present");
                    let detached_cold = self.frame_detach_cold(popped);
                    let popped = self.park_active_frame(popped);
                    owner.park_after_yield(
                        &mut self.gc_heap,
                        popped,
                        detached_cold,
                        kind_dst,
                        value_dst,
                        yielded,
                    );
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
                        self.async_generator_yield_awaited(stack, context, &owner, yielded)?;
                    }
                    return Ok(DispatchOutcome::Returned(yielded));
                }
                Op::GeneratorStart => {
                    let owner = self
                        .frame_generator_owner(&stack[top_idx])
                        .ok_or(VmError::TypeMismatch)?;
                    let callee = stack[top_idx].self_value;
                    let frame = stack.last_mut().ok_or_else(|| VmError::InvalidOperand)?;
                    frame.advance_pc()?;
                    let popped = stack.pop().expect("frame present");
                    let detached_cold = self.frame_detach_cold(popped);
                    let popped = self.park_active_frame(popped);
                    owner.park_frame(&mut self.gc_heap, popped, detached_cold);
                    // §27.5.1 step 3: the prototype is read after
                    // FunctionDeclarationInstantiation; the generator object is
                    // this call's completion.
                    let generator =
                        self.resolve_started_generator(stack, context, owner, callee, function_id)?;
                    return Ok(DispatchOutcome::Returned(generator));
                }
                // §7.1.4 ToNumber — the shared synchronous helper owns the
                // full ToPrimitive(number) ladder before committing `dst`.
                Op::ToNumber => {
                    let dst = instr.reg(0);
                    let src = instr.reg(1);
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
                    // Hot fast path: an already-primitive source (the dominant
                    // case — numeric loop operands) is its own ToPrimitive
                    // result. Skip the hint-token decode and the parked-ladder
                    // resume check; a primitive operand never parks. Reading
                    // `src` (the original operand, not `dst`) keeps the object
                    // resume path — where `src` stays non-primitive — intact.
                    let dst = instr.reg(0);
                    let src = instr.reg(1);
                    let recv = *read_register(&stack[top_idx], src)?;
                    if recv.is_primitive() {
                        write_register(&mut stack[top_idx], dst, recv)?;
                        stack[top_idx].advance_pc()?;
                        continue;
                    }
                    self.drive_to_primitive(stack, context, function.operand_view(instr))?;
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
                    let dst = instr.reg(0);
                    let src = instr.reg(1);
                    self.run_get_iterator_regs(&mut *stack, top_idx, dst, src)?;
                    continue;
                }
                Op::GetAsyncIterator => {
                    let dst = instr.reg(0);
                    let src = instr.reg(1);
                    self.run_get_async_iterator_regs(context, &mut *stack, top_idx, dst, src)?;
                    continue;
                }
                // §7.4.5 `IteratorNext`. Built-in iterators step
                // synchronously; user iterators push a call to
                // `iter.next()` and resume to extract `value` /
                // `done`.
                // <https://tc39.es/ecma262/#sec-iteratornext>
                Op::IteratorNext => {
                    // §7.4.8 IteratorStepValue — a throw out of `next` or out
                    // of the result's getters sets the record's [[Done]], so
                    // a handler's IteratorClose leaves it alone.
                    let iter_reg = instr.reg(2);
                    let iterator = *read_register(&stack[top_idx], iter_reg)?;
                    let operands = function.operand_view(instr);
                    match self.drive_iterator_next(stack, context, operands) {
                        Ok(true) => continue,
                        Ok(false) => {}
                        Err(e) => {
                            self.iterator_mark_done(iterator);
                            return Err(e);
                        }
                    }
                    let value_dst = instr.reg(0);
                    let done_dst = instr.reg(1);
                    let frame = &mut stack[top_idx];
                    if let Err(e) =
                        self.run_iterator_next_regs(frame, value_dst, done_dst, iter_reg)
                    {
                        self.iterator_mark_done(iterator);
                        return Err(e);
                    }
                    continue;
                }
                // §7.4.11 IteratorClose for a normal completion.
                Op::IteratorClose => {
                    let iterator = *read_register(&stack[top_idx], instr.reg(0))?;
                    self.iterator_close_value_sync(stack, context, iterator)?;
                    stack[top_idx].advance_pc()?;
                    continue;
                }
                // §7.4.11 IteratorClose for a throw completion: the handler
                // rethrows its own value next.
                Op::IteratorCloseThrow => {
                    let iterator = *read_register(&stack[top_idx], instr.reg(0))?;
                    self.iterator_close_for_throw(stack, context, iterator)?;
                    stack[top_idx].advance_pc()?;
                    continue;
                }
                Op::AsyncIteratorReturn => {
                    let result_reg = instr.reg(0);
                    let called_reg = instr.reg(1);
                    let iter_reg = instr.reg(2);
                    let iterator = *read_register(&stack[top_idx], iter_reg)?;
                    // §7.4.11 steps 3-5: an iterator without a `return`
                    // is already closed, and the caller skips the await.
                    let outcome = self.async_iterator_return_call(stack, context, iterator)?;
                    let frame = &mut stack[top_idx];
                    match outcome {
                        Some(result) => {
                            write_register(frame, result_reg, result)?;
                            write_register(frame, called_reg, Value::boolean(true))?;
                        }
                        None => {
                            write_register(frame, result_reg, Value::undefined())?;
                            write_register(frame, called_reg, Value::boolean(false))?;
                        }
                    }
                    frame.advance_pc()?;
                    continue;
                }
                Op::CheckIteratorResult => {
                    let value_reg = instr.reg(0);
                    let value = *read_register(&stack[top_idx], value_reg)?;
                    // §7.4.11 step 8 — the awaited `return` result is an
                    // Object or the close is a TypeError.
                    if !value.is_object() && !value.is_proxy() {
                        return Err(self.err_type(
                            ("iterator `return` did not yield an object".to_string()).into(),
                        ));
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
                    let dst = instr.reg(0);
                    let obj_reg = instr.reg(1);
                    let name_idx = instr.const_word(2);
                    let key = context
                        .property_atom(name_idx)
                        .ok_or_else(|| VmError::InvalidOperand)?;
                    let slot = function
                        .property_feedback_at(
                            instr.instruction_pc as usize,
                            crate::property_ic::PropertyIcKind::Load,
                        )
                        .ok_or(VmError::InvalidOperand)?;
                    if self.drive_load_property(stack, context, dst, obj_reg, key, slot)? {
                        continue;
                    }
                    self.run_load_property_reg(context, &mut *stack, top_idx, dst, obj_reg, key)?;
                    continue;
                }
                Op::LoadElement => {
                    let operands = function.operand_view(instr);
                    if jit_installed
                        && let Some(recv_reg) = function.register(instr, 1)
                        && let Ok(recv) = read_register(&stack[top_idx], recv_reg)
                    {
                        let recv = *recv;
                        self.record_element_family_feedback(
                            function,
                            instr.instruction_pc,
                            function_id,
                            recv,
                        );
                    }
                    if self.drive_load_element(stack, context, operands)? {
                        continue;
                    }
                    let (dst, recv_reg, idx_reg) = instr.reg3();
                    self.run_load_element_regs(context, stack, top_idx, dst, recv_reg, idx_reg)?;
                    continue;
                }
                Op::LoadSuperProperty => {
                    let dst = instr.reg(0);
                    let home_reg = instr.reg(1);
                    let name_idx = instr.const_word(2);
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
                    let (dst, home_reg, key_reg) = instr.reg3();
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
                    let home_reg = instr.reg(0);
                    let name_idx = instr.const_word(1);
                    let value_reg = instr.reg(2);
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
                    let (home_reg, key_reg, value_reg) = instr.reg3();
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
                    if self.drive_store_property(stack, context, operands, false)? {
                        continue;
                    }
                    let obj_reg = instr.reg(0);
                    let name_idx = instr.const_word(1);
                    let src = instr.reg(2);
                    let key = context
                        .property_atom(name_idx)
                        .ok_or_else(|| VmError::InvalidOperand)?;
                    self.run_store_property_reg(
                        context,
                        &mut *stack,
                        top_idx,
                        obj_reg,
                        key,
                        src,
                        false,
                    )?;
                    continue;
                }
                // §15.7.1 — class heritage / computed-key stores keep
                // strict PutValue semantics inside a sloppy frame.
                Op::StorePropertyStrict => {
                    let operands = function.operand_view(instr);
                    if self.drive_store_property(stack, context, operands, true)? {
                        continue;
                    }
                    let obj_reg = instr.reg(0);
                    let name_idx = instr.const_word(1);
                    let src = instr.reg(2);
                    let key = context
                        .property_atom(name_idx)
                        .ok_or_else(|| VmError::InvalidOperand)?;
                    self.run_store_property_reg(
                        context,
                        &mut *stack,
                        top_idx,
                        obj_reg,
                        key,
                        src,
                        true,
                    )?;
                    continue;
                }
                // §15.7.1 — `StoreElementStrict` keeps strict PutValue
                // semantics inside a sloppy frame; both forms record the
                // same feedback.
                Op::StoreElement | Op::StoreElementStrict => {
                    let strict = op == Op::StoreElementStrict;
                    let recv_reg = instr.reg(0);
                    let idx_reg = instr.reg(1);
                    let src_reg = instr.reg(2);
                    if let Some(feedback) = feedback {
                        let value = *read_register(&stack[top_idx], src_reg)?;
                        feedback.record_arith(value, value);
                    }
                    if jit_installed {
                        let recv = *read_register(&stack[top_idx], recv_reg)?;
                        self.record_element_family_feedback(
                            function,
                            instr.instruction_pc,
                            function_id,
                            recv,
                        );
                    }
                    // Copy the operands through a short representation-neutral
                    // view, then end the frame borrow before `[[Set]]` can
                    // allocate, collect, or synchronously re-enter JavaScript.
                    let (function_id, receiver, key, value) = {
                        let frame = ActiveFrameRef::from_frame(&stack[top_idx]);
                        (
                            frame.function_id(),
                            frame.read(recv_reg)?,
                            frame.read(idx_reg)?,
                            frame.read(src_reg)?,
                        )
                    };
                    self.store_element_values(
                        stack,
                        context,
                        function_id,
                        receiver,
                        key,
                        value,
                        strict,
                    )?;
                    // The synchronous helper completed the full effect (or
                    // threw), so publish exactly one resume increment.
                    stack[top_idx].advance_pc()?;
                    continue;
                }
                Op::Instanceof => {
                    let (dst, lhs, rhs) = instr.reg3();
                    let lhs = *read_register(&stack[top_idx], lhs)?;
                    let rhs = *read_register(&stack[top_idx], rhs)?;
                    let result = self.object_protocol_value(
                        stack,
                        context,
                        crate::ObjectProtocolValueOp::Instanceof,
                        lhs,
                        rhs,
                    )?;
                    let frame = &mut stack[top_idx];
                    write_register(frame, dst, result)?;
                    frame.advance_pc()?;
                    continue;
                }
                // §28.2.4.7 / .10 Proxy.[[HasProperty]] /
                // [[Delete]] — invoke `has` / `deleteProperty`
                // traps when the receiver is a Proxy.
                Op::HasProperty => {
                    let (dst, lhs, rhs) = instr.reg3();
                    let lhs = *read_register(&stack[top_idx], lhs)?;
                    let rhs = *read_register(&stack[top_idx], rhs)?;
                    let result = self.object_protocol_value(
                        stack,
                        context,
                        crate::ObjectProtocolValueOp::HasProperty,
                        lhs,
                        rhs,
                    )?;
                    let frame = &mut stack[top_idx];
                    write_register(frame, dst, result)?;
                    frame.advance_pc()?;
                    continue;
                }
                Op::DeleteProperty => {
                    let operands = function.operand_view(instr);
                    if self.drive_delete_property_proxy(stack, context, operands)? {
                        continue;
                    }
                    let dst = instr.reg(0);
                    let obj_reg = instr.reg(1);
                    let name_idx = instr.const_word(2);
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
                            stack,
                            context,
                            &receiver,
                            key.name() != "then",
                        )?;
                    }
                    let frame = &mut stack[top_idx];
                    self.run_delete_property_reg(context, frame, dst, obj_reg, key, strict)?;
                    continue;
                }
                Op::DeleteElement => {
                    let operands = function.operand_view(instr);
                    if self.drive_delete_element_proxy(stack, context, operands)? {
                        continue;
                    }
                    let (dst, obj_reg, idx_reg) = instr.reg3();
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
                        self.ensure_deferred_namespace_ready(
                            stack,
                            context,
                            &receiver,
                            !symbol_like,
                        )?;
                    }
                    self.run_delete_element_regs(
                        context, stack, top_idx, dst, obj_reg, idx_reg, strict,
                    )?;
                    continue;
                }
                // §28.2.4.1 / .2 Proxy.[[GetPrototypeOf]] /
                // [[SetPrototypeOf]] — invoke `getPrototypeOf` /
                // `setPrototypeOf` traps when the receiver is a
                // Proxy.
                Op::GetPrototype => {
                    let dst = instr.reg(0);
                    let src = instr.reg(1);
                    let source = *read_register(&stack[top_idx], src)?;
                    let result = self.object_protocol_value(
                        stack,
                        context,
                        crate::ObjectProtocolValueOp::GetPrototype,
                        source,
                        Value::undefined(),
                    )?;
                    let frame = &mut stack[top_idx];
                    write_register(frame, dst, result)?;
                    frame.advance_pc()?;
                    continue;
                }
                Op::SetPrototype => {
                    let obj_reg = instr.reg(0);
                    let proto_reg = instr.reg(1);
                    let object = *read_register(&stack[top_idx], obj_reg)?;
                    let prototype = *read_register(&stack[top_idx], proto_reg)?;
                    self.object_protocol_value(
                        stack,
                        context,
                        crate::ObjectProtocolValueOp::SetPrototype,
                        object,
                        prototype,
                    )?;
                    stack[top_idx].advance_pc()?;
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
                Op::LoadArgumentsLength | Op::LoadArgumentsElement => {
                    let dst = instr.reg(0);
                    let key = (op == Op::LoadArgumentsElement)
                        .then(|| read_register(&stack[top_idx], instr.reg(1)).copied())
                        .transpose()?;
                    let value = self.read_frame_arguments(context, stack, top_idx, key)?;
                    write_register(&mut stack[top_idx], dst, value)?;
                    stack[top_idx].advance_pc()?;
                    continue;
                }
                Op::CollectArguments => {
                    let (dst, ctx) = (instr.reg(0), instr.reg(1));
                    self.run_collect_arguments_reg(context, stack, top_idx, dst, ctx)?;
                    continue;
                }
                Op::Nop => {
                    stack[top_idx].advance_pc()?;
                    continue;
                }
                Op::LoadUndefined => {
                    let dst = instr.reg(0);
                    let frame = &mut stack[top_idx];
                    write_register(frame, dst, Value::undefined())?;
                    frame.advance_pc()?;
                    continue;
                }
                Op::LoadHole => {
                    let dst = instr.reg(0);
                    let frame = &mut stack[top_idx];
                    write_register(frame, dst, Value::hole())?;
                    frame.advance_pc()?;
                    continue;
                }
                Op::LoadTrue => {
                    let dst = instr.reg(0);
                    let frame = &mut stack[top_idx];
                    write_register(frame, dst, Value::boolean(true))?;
                    frame.advance_pc()?;
                    continue;
                }
                Op::LoadFalse => {
                    let dst = instr.reg(0);
                    let frame = &mut stack[top_idx];
                    write_register(frame, dst, Value::boolean(false))?;
                    frame.advance_pc()?;
                    continue;
                }
                Op::LoadNull => {
                    let dst = instr.reg(0);
                    let frame = &mut stack[top_idx];
                    write_register(frame, dst, Value::null())?;
                    frame.advance_pc()?;
                    continue;
                }
                Op::LoadInt32 => {
                    let dst = instr.reg(0);
                    let imm = instr.imm(1);
                    let frame = &mut stack[top_idx];
                    write_register(frame, dst, Value::number(NumberValue::Smi(imm)))?;
                    frame.advance_pc()?;
                    continue;
                }
                Op::LoadNumber => {
                    let dst = instr.reg(0);
                    let idx = instr.const_word(1);
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
                    let dst = instr.reg(0);
                    let idx = instr.const_word(1);
                    let value = self.load_string_constant_value(context, idx)?;
                    let frame = &mut stack[top_idx];
                    write_register(frame, dst, value)?;
                    frame.advance_pc()?;
                    continue;
                }
                Op::LoadLength => {
                    let dst = instr.reg(0);
                    let src = instr.reg(1);
                    let source = *read_register(&stack[top_idx], src)?;
                    let result = self.scalar_value(
                        stack,
                        context,
                        crate::ScalarValueOp::LoadLength,
                        source,
                        Value::undefined(),
                        Value::undefined(),
                    )?;
                    let frame = &mut stack[top_idx];
                    write_register(frame, dst, result)?;
                    frame.advance_pc()?;
                    continue;
                }
                Op::LogicalNot => {
                    let dst = instr.reg(0);
                    let src = instr.reg(1);
                    let frame = &mut stack[top_idx];
                    let truthy = read_register(frame, src)?.to_boolean(&self.gc_heap);
                    write_register(frame, dst, Value::boolean(!truthy))?;
                    frame.advance_pc()?;
                    continue;
                }
                Op::ToBoolean => {
                    let dst = instr.reg(0);
                    let src = instr.reg(1);
                    let frame = &mut stack[top_idx];
                    let truthy = read_register(frame, src)?.to_boolean(&self.gc_heap);
                    write_register(frame, dst, Value::boolean(truthy))?;
                    frame.advance_pc()?;
                    continue;
                }
                Op::GetStringIndex => {
                    let (dst, recv, idx) = instr.reg3();
                    let frame = &mut stack[top_idx];
                    self.run_get_string_index_regs(frame, dst, recv, idx)?;
                    continue;
                }
                Op::TypeOf => {
                    let dst = instr.reg(0);
                    let src = instr.reg(1);
                    let source = *read_register(&stack[top_idx], src)?;
                    let result = self.scalar_value(
                        stack,
                        context,
                        crate::ScalarValueOp::TypeOf,
                        source,
                        Value::undefined(),
                        Value::undefined(),
                    )?;
                    let frame = &mut stack[top_idx];
                    write_register(frame, dst, result)?;
                    frame.advance_pc()?;
                    continue;
                }
                Op::TestTypeOf => {
                    let dst = instr.reg(0);
                    let src = instr.reg(1);
                    let test = function
                        .imm32(instr, 2)
                        .and_then(otter_bytecode::TypeOfTest::decode)
                        .ok_or(VmError::InvalidOperand)?;
                    let frame = &mut stack[top_idx];
                    let kind = read_register(frame, src)?.typeof_kind_with_heap(&self.gc_heap);
                    write_register(
                        frame,
                        dst,
                        Value::boolean((kind == test.kind) != test.negate),
                    )?;
                    frame.advance_pc()?;
                    continue;
                }
                Op::LoadThis => {
                    let dst = instr.reg(0);
                    let mut frame = ActiveFrameMut::from_frame(&mut stack[top_idx]);
                    self.frame_load_this(&mut frame, dst)?;
                    frame.advance_pc()?;
                    continue;
                }
                Op::LoadNewTarget => {
                    let dst = instr.reg(0);
                    let new_target = stack[top_idx].new_target();
                    let result = self.scalar_value(
                        stack,
                        context,
                        crate::ScalarValueOp::LoadNewTarget,
                        Value::undefined(),
                        Value::undefined(),
                        new_target,
                    )?;
                    let frame = &mut stack[top_idx];
                    write_register(frame, dst, result)?;
                    frame.advance_pc()?;
                    continue;
                }
                Op::NewObject => {
                    let dst = instr.reg(0);
                    self.run_new_object_reg(&mut *stack, top_idx, dst)?;
                    continue;
                }
                Op::NewArray => {
                    let operands = function.operand_view(instr);
                    self.run_new_array_operands(&mut *stack, top_idx, operands)?;
                    continue;
                }
                Op::NewObjectLiteral => {
                    let operands = function.operand_view(instr);
                    self.run_new_object_literal_operands(&mut *stack, top_idx, context, operands)?;
                    continue;
                }
                Op::LoadRegExp => {
                    let dst = instr.reg(0);
                    let idx = instr.const_word(1);
                    let frame = &mut stack[top_idx];
                    self.run_load_regexp_reg(context, frame, dst, idx)?;
                    continue;
                }
                Op::LoadBigInt => {
                    let dst = instr.reg(0);
                    let idx = instr.const_word(1);
                    let frame = &mut stack[top_idx];
                    self.run_load_bigint_reg(context, frame, dst, idx)?;
                    continue;
                }
                Op::LoadClosureContext => {
                    let dst = instr.reg(0);
                    let mut frame = ActiveFrameMut::from_frame(&mut stack[top_idx]);
                    self.frame_load_closure_context(&mut frame, dst)?;
                    frame.advance_pc()?;
                    continue;
                }
                Op::LoadSelf => {
                    let dst = instr.reg(0);
                    let mut frame = ActiveFrameMut::from_frame(&mut stack[top_idx]);
                    self.frame_load_self(&mut frame, dst)?;
                    frame.advance_pc()?;
                    continue;
                }
                Op::CreateContext => {
                    let (dst, parent) = (instr.reg(0), instr.reg(1));
                    let scope = instr.imm(2) as u32;
                    let mut frame = ActiveFrameMut::from_frame(&mut stack[top_idx]);
                    self.frame_create_context(context, &mut frame, dst, parent, scope)?;
                    frame.advance_pc()?;
                    continue;
                }
                Op::CopyContext => {
                    let (dst, src) = (instr.reg(0), instr.reg(1));
                    let mut frame = ActiveFrameMut::from_frame(&mut stack[top_idx]);
                    self.frame_copy_context(&mut frame, dst, src)?;
                    frame.advance_pc()?;
                    continue;
                }
                Op::LoadContextSlot | Op::LoadContextSlotChecked => {
                    let (dst, ctx) = (instr.reg(0), instr.reg(1));
                    let coord = instr.imm(2);
                    let frame = &mut stack[top_idx];
                    let value = self.load_context_slot_value(
                        context,
                        *read_register(frame, ctx)?,
                        coord,
                        op == Op::LoadContextSlotChecked,
                    )?;
                    write_register(frame, dst, value)?;
                    frame.advance_pc()?;
                    continue;
                }
                Op::StoreContextSlot | Op::StoreContextSlotChecked => {
                    let (src, ctx) = (instr.reg(0), instr.reg(1));
                    let coord = instr.imm(2);
                    let frame = &mut stack[top_idx];
                    let (value, target) =
                        (*read_register(frame, src)?, *read_register(frame, ctx)?);
                    self.store_context_slot_value(
                        context,
                        target,
                        coord,
                        value,
                        op == Op::StoreContextSlotChecked,
                    )?;
                    stack[top_idx].advance_pc()?;
                    continue;
                }
                Op::BindThisContextSlot => {
                    let (src, ctx) = (instr.reg(0), instr.reg(1));
                    let coord = instr.imm(2);
                    let frame = &mut stack[top_idx];
                    let (value, target) =
                        (*read_register(frame, src)?, *read_register(frame, ctx)?);
                    self.bind_this_context_slot_value(target, coord, value)?;
                    stack[top_idx].advance_pc()?;
                    continue;
                }
                Op::CollectRest => {
                    let dst = instr.reg(0);
                    self.collect_rest(&mut *stack, top_idx, function.param_count, dst)?;
                    stack[top_idx].advance_pc()?;
                    continue;
                }
                Op::MakeFunction => {
                    let dst = instr.reg(0);
                    let idx = instr.const_word(1);
                    let frame = &mut stack[top_idx];
                    self.run_make_function_reg(context, frame, dst, idx)?;
                    continue;
                }
                Op::MakeClass => {
                    let dst = function.reg(instr, 0);
                    let ctor_reg = function.reg(instr, 1);
                    let proto_reg = function.reg(instr, 2);
                    let statics_reg = function.reg(instr, 3);
                    // Operand 4 (parent class value) — absent in
                    // pre-existing bytecode; `undefined` = base class.
                    let parent_reg = function.register(instr, 4);
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
                    let dst = instr.reg(0);
                    let msg_reg = instr.reg(1);
                    self.run_new_error_regs(context, &mut *stack, top_idx, dst, msg_reg)?;
                    continue;
                }
                Op::NewBuiltinError => {
                    let dst = instr.reg(0);
                    let kind_idx = instr.const_word(1);
                    let msg_reg = instr.reg(2);
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
                    let dst = instr.reg(0);
                    let kind_idx = instr.const_word(1);
                    let frame = &mut stack[top_idx];
                    self.run_load_builtin_error_reg(context, frame, dst, kind_idx)?;
                    continue;
                }
                Op::LoadGlobalThis => {
                    let dst = instr.reg(0);
                    let frame = &mut stack[top_idx];
                    self.run_load_global_this_reg(frame, dst)?;
                    continue;
                }
                Op::LoadGlobalOrThrow => {
                    let dst = instr.reg(0);
                    let name_idx = instr.const_word(1);
                    self.run_load_global_or_throw_reg(context, stack, top_idx, dst, name_idx)?;
                    continue;
                }
                Op::LoadGlobalOrUndefined => {
                    let dst = instr.reg(0);
                    let name_idx = instr.const_word(1);
                    self.run_load_global_or_undefined_reg(context, stack, top_idx, dst, name_idx)?;
                    continue;
                }
                Op::DeclareGlobalVar => {
                    let name_idx = instr.const_word(0);
                    let configurable = function.imm32(instr, 1).unwrap_or(0) != 0;
                    let frame = &mut stack[top_idx];
                    self.run_declare_global_var_reg(context, frame, name_idx, configurable)?;
                    continue;
                }
                // §13.2.8.4 GetTemplateObject — realm-cached frozen
                // template-strings object per tagged-template site.
                Op::GetTemplateObject => {
                    let dst = instr.reg(0);
                    let site_idx = instr.const_word(1);
                    self.run_get_template_object_reg(context, stack, top_idx, dst, site_idx)?;
                    continue;
                }
                // §9.1.2.1 GetIdentifierReference through contexts a sloppy
                // direct eval may have extended: extensions at hops
                // `[0, depth)` first, then the static slot or the global
                // Environment Record.
                Op::LoadLookupSlot => {
                    let (dst, ctx) = (instr.reg(0), instr.reg(1));
                    let (name_idx, coord) = (instr.const_word(2), instr.imm(3));
                    let frame = &mut stack[top_idx];
                    let value = self.load_lookup_slot_value(
                        context,
                        function_id,
                        *read_register(frame, ctx)?,
                        name_idx,
                        coord,
                    )?;
                    write_register(frame, dst, value)?;
                    frame.advance_pc()?;
                    continue;
                }
                Op::StoreLookupSlot => {
                    let value_reg = function.reg(instr, 0);
                    let ctx = function.reg(instr, 1);
                    let name_idx = function
                        .const_index(instr, 2)
                        .ok_or(VmError::InvalidOperand)?;
                    let coord = function.imm32(instr, 3).ok_or(VmError::InvalidOperand)?;
                    let fallback = function.imm32(instr, 4).ok_or(VmError::InvalidOperand)?;
                    let frame = &stack[top_idx];
                    let (value, target) = (
                        *read_register(frame, value_reg)?,
                        *read_register(frame, ctx)?,
                    );
                    self.store_lookup_slot_value(
                        context,
                        function_id,
                        target,
                        name_idx,
                        coord,
                        fallback,
                        value,
                    )?;
                    stack[top_idx].advance_pc()?;
                    continue;
                }
                Op::DeleteLookupSlot => {
                    let (dst, ctx) = (instr.reg(0), instr.reg(1));
                    let (name_idx, depth) = (instr.const_word(2), instr.imm(3));
                    let target = *read_register(&stack[top_idx], ctx)?;
                    let value = self.delete_lookup_slot_value(
                        context,
                        function_id,
                        target,
                        name_idx,
                        depth,
                    )?;
                    let frame = &mut stack[top_idx];
                    write_register(frame, dst, value)?;
                    frame.advance_pc()?;
                    continue;
                }
                Op::LoadLookupGlobal | Op::TypeofLookupGlobal => {
                    let (dst, ctx) = (instr.reg(0), instr.reg(1));
                    let (name_idx, depth) = (instr.const_word(2), instr.imm(3));
                    let target = *read_register(&stack[top_idx], ctx)?;
                    let missing = if op == Op::LoadLookupGlobal {
                        otter_bytecode::opcode_schema::BindingMissing::Throw
                    } else {
                        otter_bytecode::opcode_schema::BindingMissing::Undefined
                    };
                    let value = self.load_lookup_global_value(
                        context,
                        stack,
                        function_id,
                        target,
                        name_idx,
                        depth,
                        missing,
                    )?;
                    let frame = &mut stack[top_idx];
                    write_register(frame, dst, value)?;
                    frame.advance_pc()?;
                    continue;
                }
                Op::StoreLookupGlobal => {
                    let (value_reg, ctx) = (instr.reg(0), instr.reg(1));
                    let (name_idx, mode) = (instr.const_word(2), instr.imm(3));
                    let frame = &stack[top_idx];
                    let (value, target) = (
                        *read_register(frame, value_reg)?,
                        *read_register(frame, ctx)?,
                    );
                    self.store_lookup_global_value(
                        context,
                        stack,
                        function_id,
                        target,
                        name_idx,
                        mode,
                        value,
                    )?;
                    stack[top_idx].advance_pc()?;
                    continue;
                }
                Op::DeleteLookupGlobal => {
                    let (dst, ctx) = (instr.reg(0), instr.reg(1));
                    let (name_idx, depth) = (instr.const_word(2), instr.imm(3));
                    let target = *read_register(&stack[top_idx], ctx)?;
                    let value = self.delete_lookup_global_value(
                        context,
                        function_id,
                        target,
                        name_idx,
                        depth,
                    )?;
                    let frame = &mut stack[top_idx];
                    write_register(frame, dst, value)?;
                    frame.advance_pc()?;
                    continue;
                }
                // §13.15.2 — the assignment target's reference base resolves
                // before the right-hand side; `StoreRef` writes through it.
                Op::ResolveLookupRef => {
                    let (dst, ctx) = (instr.reg(0), instr.reg(1));
                    let (name_idx, target_imm) = (instr.const_word(2), instr.imm(3));
                    let frame = &mut stack[top_idx];
                    let reference = self.resolve_lookup_ref_value(
                        context,
                        function_id,
                        *read_register(frame, ctx)?,
                        name_idx,
                        target_imm,
                    )?;
                    write_register(frame, dst, reference)?;
                    frame.advance_pc()?;
                    continue;
                }
                Op::StoreRef => {
                    let (value_reg, reference_reg) = (instr.reg(0), instr.reg(1));
                    let (name_idx, mode) = (instr.const_word(2), instr.imm(3));
                    let frame = &stack[top_idx];
                    let (value, reference) = (
                        *read_register(frame, value_reg)?,
                        *read_register(frame, reference_reg)?,
                    );
                    self.store_ref_value(
                        context,
                        stack,
                        function_id,
                        reference,
                        name_idx,
                        mode,
                        value,
                    )?;
                    stack[top_idx].advance_pc()?;
                    continue;
                }
                // §19.2.1.3 step 16 — a sloppy eval's `var` with no static
                // slot becomes a deletable binding of the var scope.
                Op::DeclareEvalVar => {
                    let ctx = instr.reg(0);
                    let (name_idx, var_depth) = (instr.const_word(1), instr.imm(2));
                    let target = *read_register(&stack[top_idx], ctx)?;
                    self.declare_eval_var_value(context, function_id, target, name_idx, var_depth)?;
                    stack[top_idx].advance_pc()?;
                    continue;
                }
                Op::StoreVarScope => {
                    let (value_reg, ctx) = (instr.reg(0), instr.reg(1));
                    let (name_idx, var_depth) = (instr.const_word(2), instr.imm(3));
                    let frame = &stack[top_idx];
                    let (value, target) = (
                        *read_register(frame, value_reg)?,
                        *read_register(frame, ctx)?,
                    );
                    self.store_var_scope_value(
                        context,
                        function_id,
                        target,
                        name_idx,
                        var_depth,
                        value,
                    )?;
                    stack[top_idx].advance_pc()?;
                    continue;
                }
                // §6.2.12 — mint a Private Name carrier; the marker
                // keeps it out of Proxy traps and arms the §7.3.28
                // extensibility check on adds.
                Op::NewPrivateName => {
                    let dst = instr.reg(0);
                    let desc_idx = instr.const_word(1);
                    self.run_new_private_name_reg(context, stack, top_idx, dst, desc_idx)?;
                    continue;
                }
                Op::DefineGlobalFunction => {
                    let name_idx = instr.const_word(0);
                    let value_reg = instr.reg(1);
                    let deletable = function.imm32(instr, 2).unwrap_or(0) != 0;
                    let frame = &mut stack[top_idx];
                    self.run_define_global_function_reg(
                        context, frame, name_idx, value_reg, deletable,
                    )?;
                    continue;
                }
                Op::DeclareGlobalLex => {
                    let name_idx = instr.const_word(0);
                    let is_const = function.imm32(instr, 1).unwrap_or(0) != 0;
                    let frame = &mut stack[top_idx];
                    self.run_declare_global_lex_reg(context, frame, name_idx, is_const)?;
                    continue;
                }
                Op::StoreGlobalBinding => {
                    let value_reg = instr.reg(0);
                    let name_idx = instr.const_word(1);
                    let strict = function.imm32(instr, 2).unwrap_or(0) != 0;
                    self.run_store_global_binding_reg(
                        context, stack, top_idx, value_reg, name_idx, strict,
                    )?;
                    continue;
                }
                Op::InitGlobalLex => {
                    let value_reg = instr.reg(0);
                    let name_idx = instr.const_word(1);
                    let frame = &mut stack[top_idx];
                    self.run_init_global_lex_reg(context, frame, value_reg, name_idx)?;
                    continue;
                }
                // §15.7.14 class-definition validation: heritage
                // IsConstructor / static computed key != "prototype".
                Op::ClassCheck => {
                    let kind = function.imm32(instr, 0).unwrap_or(0);
                    let reg = instr.reg(1);
                    self.run_class_check_reg(context, stack, top_idx, kind as u32, reg)?;
                    continue;
                }
                // §7.3.7 CreateDataPropertyOrThrow — object literal
                // property definition; never consults inherited
                // setters (unlike StoreProperty's Set semantics).
                Op::DefineDataProperty => {
                    let (obj_reg, key_reg, value_reg) = instr.reg3();
                    self.run_define_data_property_regs(
                        context, stack, top_idx, obj_reg, key_reg, value_reg,
                    )?;
                    stack[top_idx].advance_pc()?;
                    continue;
                }
                // §10.2.10 SetFunctionName — names an anonymous
                // function from a run-time property key.
                Op::SetFunctionName => {
                    let fn_reg = instr.reg(0);
                    let key_reg = instr.reg(1);
                    let prefix_idx = instr.const_word(2);
                    self.run_set_function_name_reg(
                        context, stack, top_idx, fn_reg, key_reg, prefix_idx,
                    )?;
                    continue;
                }
                // §7.3.31 PrivateGet — brand check (absent name
                // throws), accessor-without-getter throws, accessor
                // invokes its getter with the receiver as `this`.
                Op::PrivateGet => {
                    let (dst, obj_reg, key_reg) = instr.reg3();
                    self.run_private_get_reg(context, stack, top_idx, dst, obj_reg, key_reg)?;
                    continue;
                }
                // §7.3.32 PrivateSet — brand check, private methods
                // are not writable, accessor-without-setter throws,
                // an own field writes in place preserving attributes.
                Op::PrivateSet => {
                    let (obj_reg, key_reg, value_reg) = instr.reg3();
                    self.run_private_set_reg(context, stack, top_idx, obj_reg, key_reg, value_reg)?;
                    continue;
                }
                // §7.1.3 ToNumeric: Number / BigInt pass through, an
                // object runs ToPrimitive(number) first, Symbol throws,
                // the rest convert via ToNumber. A postfix update keeps
                // this as its result value.
                Op::ToNumeric => {
                    let dst = instr.reg(0);
                    let src = instr.reg(1);
                    let value = *read_register(&stack[top_idx], src)?;
                    let result = if value.is_number() {
                        value
                    } else {
                        crate::coerce::to_numeric_or_throw(self, stack, context, &value)?
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
                    let dst = instr.reg(0);
                    let src = instr.reg(1);
                    let source = *read_register(&stack[top_idx], src)?;
                    let result = self.scalar_value(
                        stack,
                        context,
                        crate::ScalarValueOp::ToObject,
                        source,
                        Value::undefined(),
                        Value::undefined(),
                    )?;
                    let frame = &mut stack[top_idx];
                    write_register(frame, dst, result)?;
                    frame.advance_pc()?;
                    continue;
                }
                // §7.1.19 ToPropertyKey with full user coercion —
                // class field definitions canonicalize their
                // computed names at class-definition time.
                Op::ToPropertyKey => {
                    let dst = instr.reg(0);
                    let src = instr.reg(1);
                    let source = *read_register(&stack[top_idx], src)?;
                    let result = self.scalar_value(
                        stack,
                        context,
                        crate::ScalarValueOp::ToPropertyKey,
                        source,
                        Value::undefined(),
                        Value::undefined(),
                    )?;
                    let frame = &mut stack[top_idx];
                    write_register(frame, dst, result)?;
                    frame.advance_pc()?;
                    continue;
                }
                // §7.3.31 PrivateElementFind own-only — private
                // methods / accessors require the class brand marker
                // as an OWN property of the receiver (installed after
                // super() returns); the prototype-side method lookup
                // alone must not satisfy access before that.
                Op::PrivateBrandCheck => {
                    let obj_reg = instr.reg(0);
                    let brand_reg = instr.reg(1);
                    self.run_private_brand_check_reg(context, stack, top_idx, obj_reg, brand_reg)?;
                    continue;
                }
                // §13.4.2 UpdateExpression numeric step — ToNumeric
                // then ±1, preserving the BigInt type (§6.1.6.2.7).
                Op::Increment => {
                    let dst = instr.reg(0);
                    let src = instr.reg(1);
                    let delta = function.imm32(instr, 2).unwrap_or(1);
                    self.run_increment_regs(stack, context, top_idx, dst, src, delta, feedback)?;
                    continue;
                }
                Op::ValidateGlobalDecl => {
                    let name_idx = instr.const_word(0);
                    let kind = function.imm32(instr, 1).unwrap_or(0);
                    let frame = &mut stack[top_idx];
                    self.run_validate_global_decl_reg(context, frame, name_idx, kind)?;
                    continue;
                }
                Op::DefineGlobalVar => {
                    let name_idx = instr.const_word(0);
                    let value_reg = instr.reg(1);
                    let frame = &mut stack[top_idx];
                    self.run_define_global_var_reg(context, frame, name_idx, value_reg)?;
                    continue;
                }
                Op::ImportNamespace => {
                    let dst = instr.reg(0);
                    let spec_idx = instr.const_word(1);
                    self.run_import_namespace_reg(context, stack, top_idx, dst, spec_idx)?;
                    continue;
                }
                Op::ImportNamespaceDeferred => {
                    let dst = instr.reg(0);
                    let spec_idx = instr.const_word(1);
                    self.run_import_namespace_deferred_reg(context, stack, top_idx, dst, spec_idx)?;
                    continue;
                }
                Op::ModuleNamespaceObject => {
                    let dst = instr.reg(0);
                    let spec_idx = instr.const_word(1);
                    self.run_module_namespace_object_reg(context, stack, top_idx, dst, spec_idx)?;
                    continue;
                }
                Op::LoadImportBinding => {
                    let dst = instr.reg(0);
                    let url_idx = instr.const_word(1);
                    let name_idx = instr.const_word(2);
                    self.run_load_import_binding_reg(
                        context, stack, top_idx, dst, url_idx, name_idx,
                    )?;
                    continue;
                }
                Op::EvaluateModule => {
                    let dst = instr.reg(0);
                    let url_idx = instr.const_word(1);
                    self.run_evaluate_module_const(context, stack, top_idx, dst, url_idx)?;
                    continue;
                }
                Op::MarkModuleEvaluated => {
                    let url_idx = instr.const_word(0);
                    self.run_mark_module_evaluated_const(context, stack, top_idx, url_idx)?;
                    continue;
                }
                Op::ImportMetaResolve => {
                    let dst = instr.reg(0);
                    let spec_reg = instr.reg(1);
                    self.run_import_meta_resolve_regs(context, stack, top_idx, dst, spec_reg)?;
                    continue;
                }
                Op::PromiseFulfilledOf => {
                    let dst = instr.reg(0);
                    let src = instr.reg(1);
                    self.run_promise_fulfilled_of_regs(context, stack, top_idx, dst, src)?;
                    continue;
                }
                Op::ArrayPush => {
                    let arr_reg = instr.reg(0);
                    let value_reg = instr.reg(1);
                    self.run_array_push_regs(&mut *stack, top_idx, arr_reg, value_reg)?;
                    continue;
                }
                Op::NewWeakRef => {
                    let dst = instr.reg(0);
                    let target_reg = instr.reg(1);
                    self.run_new_weak_ref_regs(&mut *stack, top_idx, dst, target_reg)?;
                    continue;
                }
                Op::NewFinalizationRegistry => {
                    let dst = instr.reg(0);
                    let callback_reg = instr.reg(1);
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
                    let dst = instr.reg(0);
                    let kind_idx = instr.const_word(1);
                    let iter_reg = instr.reg(2);
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
                    let dst = instr.reg(0);
                    let name_idx = instr.const_word(1);
                    let frame = &mut stack[top_idx];
                    self.run_math_load_reg(context, frame, dst, name_idx)?;
                    continue;
                }
                Op::SymbolLoad => {
                    let dst = instr.reg(0);
                    let name_idx = instr.const_word(1);
                    let frame = &mut stack[top_idx];
                    self.run_symbol_load_reg(context, frame, dst, name_idx)?;
                    continue;
                }
                Op::TemporalLoad => {
                    let dst = instr.reg(0);
                    let name_idx = instr.const_word(1);
                    let frame = &mut stack[top_idx];
                    self.run_temporal_load_reg(context, frame, dst, name_idx)?;
                    continue;
                }
                Op::GlobalBindingExists => {
                    let dst = instr.reg(0);
                    let name_idx = const_operand(function.operand(instr, 1))?;
                    self.run_global_binding_exists_reg(context, stack, top_idx, dst, name_idx)?;
                    continue;
                }
                Op::StoreGlobalChecked => {
                    let value_reg = instr.reg(0);
                    let name_idx = const_operand(function.operand(instr, 1))?;
                    let exists_reg = instr.reg(2);
                    self.run_store_global_checked_reg(
                        context, stack, top_idx, value_reg, name_idx, exists_reg,
                    )?;
                    continue;
                }
                Op::Jump => {
                    let offset = instr.imm(0);
                    // `top_idx` is `stack.len() - 1` this tick and no frame was
                    // pushed, so the top frame is the executing one.
                    // SAFETY: the `is_at_floor` guard proved a live top frame.
                    apply_branch(
                        unsafe { stack.top_unchecked_mut() },
                        offset,
                        &self.interrupt,
                    )?;
                    if jit_installed
                        && offset < 0
                        && let Some(entry) =
                            self.note_backedge_and_maybe_osr(stack, context, top_idx)?
                    {
                        return Ok(DispatchOutcome::Tier(entry));
                    }
                    continue;
                }
                Op::JumpIfTrue => {
                    let offset = instr.imm(0);
                    let cond = instr.reg(1);
                    // SAFETY: live top frame (see the `is_at_floor` guard); it is
                    // the executing frame this tick.
                    let frame = unsafe { stack.top_unchecked_mut() };
                    // `cond` is a schema register; SAFETY: see `read_register_unchecked`.
                    let taken =
                        unsafe { read_register_unchecked(frame, cond) }.to_boolean(&self.gc_heap);
                    if jit_installed && let Some(cell) = feedback {
                        cell.record_branch(taken);
                    }
                    if taken {
                        apply_branch(frame, offset, &self.interrupt)?;
                        if jit_installed
                            && offset < 0
                            && let Some(entry) =
                                self.note_backedge_and_maybe_osr(stack, context, top_idx)?
                        {
                            return Ok(DispatchOutcome::Tier(entry));
                        }
                    } else {
                        frame.advance_pc_fast();
                    }
                    continue;
                }
                Op::JumpIfFalse => {
                    let offset = instr.imm(0);
                    let cond = instr.reg(1);
                    // SAFETY: live top frame (see the `is_at_floor` guard); it is
                    // the executing frame this tick.
                    let frame = unsafe { stack.top_unchecked_mut() };
                    // `cond` is a schema register; SAFETY: see `read_register_unchecked`.
                    let taken =
                        !unsafe { read_register_unchecked(frame, cond) }.to_boolean(&self.gc_heap);
                    if jit_installed && let Some(cell) = feedback {
                        cell.record_branch(taken);
                    }
                    if taken {
                        apply_branch(frame, offset, &self.interrupt)?;
                        if jit_installed
                            && offset < 0
                            && let Some(entry) =
                                self.note_backedge_and_maybe_osr(stack, context, top_idx)?
                        {
                            return Ok(DispatchOutcome::Tier(entry));
                        }
                    } else {
                        frame.advance_pc_fast();
                    }
                    continue;
                }
                Op::JumpIfNullish => {
                    let offset = instr.imm(0);
                    let cond = instr.reg(1);
                    // SAFETY: live top frame (see the `is_at_floor` guard); it is
                    // the executing frame this tick.
                    let frame = unsafe { stack.top_unchecked_mut() };
                    // SAFETY: see `read_register_unchecked`.
                    if unsafe { read_register_unchecked(frame, cond) }.is_nullish() {
                        apply_branch(frame, offset, &self.interrupt)?;
                    } else {
                        frame.advance_pc_fast();
                    }
                    continue;
                }
                Op::LoadLocal => {
                    let dst = instr.reg(0);
                    let idx = instr.imm(1) as u16;
                    // SAFETY: live top frame (see the `is_at_floor` guard); it is
                    // the executing frame this tick.
                    let frame = unsafe { stack.top_unchecked_mut() };
                    // Both operands are schema registers (the `Imm32` is a local
                    // index), bounded by the build-time verifier.
                    // SAFETY: see `read_register_unchecked`.
                    unsafe {
                        let value = read_register_unchecked(frame, idx);
                        write_register_unchecked(frame, dst, value);
                    }
                    frame.advance_pc_fast();
                    continue;
                }
                Op::StoreLocal => {
                    let src = instr.reg(0);
                    let idx = instr.imm(1) as u16;
                    // SAFETY: live top frame (see the `is_at_floor` guard); it is
                    // the executing frame this tick.
                    let frame = unsafe { stack.top_unchecked_mut() };
                    // SAFETY: see `Op::LoadLocal`.
                    unsafe {
                        let value = read_register_unchecked(frame, src);
                        write_register_unchecked(frame, idx, value);
                    }
                    frame.advance_pc_fast();
                    continue;
                }
                Op::TdzError => {
                    let local_index = instr.imm(0) as u32;
                    return Err(VmError::TemporalDeadZone { local_index });
                }
                Op::Add => {
                    let (dst, lhs, rhs) = instr.reg3();
                    self.run_add_regs(stack, context, top_idx, dst, lhs, rhs, feedback)?;
                    continue;
                }
                Op::AddImm => {
                    let (dst, lhs) = (instr.reg(0), instr.reg(1));
                    let imm = instr.imm(2);
                    self.run_add_imm(stack, context, top_idx, dst, lhs, imm, feedback)?;
                    continue;
                }
                Op::SubImm => {
                    let (dst, lhs) = (instr.reg(0), instr.reg(1));
                    let imm = instr.imm(2);
                    self.run_numeric_imm(
                        stack,
                        context,
                        top_idx,
                        dst,
                        lhs,
                        imm,
                        number::sub,
                        bigint_sub_op,
                        feedback,
                    )?;
                    continue;
                }
                Op::BitwiseAndImm => {
                    let (dst, lhs) = (instr.reg(0), instr.reg(1));
                    let imm = instr.imm(2);
                    self.run_numeric_imm(
                        stack,
                        context,
                        top_idx,
                        dst,
                        lhs,
                        imm,
                        number::bitwise_and,
                        bigint_and_op,
                        feedback,
                    )?;
                    continue;
                }
                Op::LessThanImm => {
                    let (dst, lhs) = (instr.reg(0), instr.reg(1));
                    let imm = instr.imm(2);
                    self.run_less_than_imm(stack, context, top_idx, dst, lhs, imm, feedback)?;
                    continue;
                }
                Op::EqualImm => {
                    let (dst, lhs) = (instr.reg(0), instr.reg(1));
                    let imm = instr.imm(2);
                    // SAFETY: live top frame (see the `is_at_floor` guard).
                    self.run_equal_imm(
                        unsafe { stack.top_unchecked_mut() },
                        dst,
                        lhs,
                        imm,
                        false,
                        feedback,
                    )?;
                    continue;
                }
                Op::NotEqualImm => {
                    let (dst, lhs) = (instr.reg(0), instr.reg(1));
                    let imm = instr.imm(2);
                    // SAFETY: live top frame (see the `is_at_floor` guard).
                    self.run_equal_imm(
                        unsafe { stack.top_unchecked_mut() },
                        dst,
                        lhs,
                        imm,
                        true,
                        feedback,
                    )?;
                    continue;
                }
                Op::Sub | Op::Mul | Op::Div | Op::Rem | Op::Pow => {
                    let (dst, lhs, rhs) = instr.reg3();
                    let operation = otter_bytecode::scalar_semantics::numeric_binary_semantics(op)
                        .expect("numeric dispatch group has shared semantics")
                        .operation;
                    self.run_numeric_binary_regs(
                        stack,
                        context,
                        top_idx,
                        [dst, lhs, rhs],
                        operation,
                        feedback,
                    )?;
                    continue;
                }
                Op::BitwiseAnd => {
                    let (dst, lhs, rhs) = instr.reg3();
                    self.run_numeric_regs(
                        stack,
                        context,
                        top_idx,
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
                    let (dst, lhs, rhs) = instr.reg3();
                    self.run_numeric_regs(
                        stack,
                        context,
                        top_idx,
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
                    let (dst, lhs, rhs) = instr.reg3();
                    self.run_numeric_regs(
                        stack,
                        context,
                        top_idx,
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
                    let (dst, lhs, rhs) = instr.reg3();
                    self.run_numeric_regs(
                        stack,
                        context,
                        top_idx,
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
                    let (dst, lhs, rhs) = instr.reg3();
                    self.run_numeric_regs(
                        stack,
                        context,
                        top_idx,
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
                    let (dst, lhs, rhs) = instr.reg3();
                    self.run_compare_regs(stack, context, top_idx, dst, lhs, rhs, op, feedback)?;
                    continue;
                }
                Op::Ushr => {
                    let (dst, lhs, rhs) = instr.reg3();
                    self.run_ushr_regs(stack, context, top_idx, dst, lhs, rhs, feedback)?;
                    continue;
                }
                Op::Neg => {
                    let dst = instr.reg(0);
                    let src = instr.reg(1);
                    self.run_neg_regs(stack, context, top_idx, dst, src, feedback)?;
                    continue;
                }
                Op::BitwiseNot => {
                    let dst = instr.reg(0);
                    let src = instr.reg(1);
                    self.run_bitwise_not_regs(stack, context, top_idx, dst, src)?;
                    continue;
                }
                Op::Equal | Op::NotEqual | Op::LooseEqual | Op::LooseNotEqual | Op::SameValue => {
                    let (dst, lhs, rhs) = instr.reg3();
                    match op {
                        // SAFETY (frame-based arms): live top frame (see the
                        // `is_at_floor` guard); it is the executing frame.
                        Op::Equal => self.run_equal_regs(
                            unsafe { stack.top_unchecked_mut() },
                            dst,
                            lhs,
                            rhs,
                            false,
                            feedback,
                        )?,
                        Op::NotEqual => self.run_equal_regs(
                            unsafe { stack.top_unchecked_mut() },
                            dst,
                            lhs,
                            rhs,
                            true,
                            feedback,
                        )?,
                        Op::LooseEqual => {
                            self.run_loose_equal_regs(
                                stack, context, top_idx, dst, lhs, rhs, false, feedback,
                            )?;
                        }
                        Op::LooseNotEqual => {
                            self.run_loose_equal_regs(
                                stack, context, top_idx, dst, lhs, rhs, true, feedback,
                            )?;
                        }
                        Op::SameValue => {
                            let lhs = *read_register(&stack[top_idx], lhs)?;
                            let rhs = *read_register(&stack[top_idx], rhs)?;
                            let result = self.scalar_value(
                                stack,
                                context,
                                crate::ScalarValueOp::SameValue,
                                lhs,
                                rhs,
                                Value::undefined(),
                            )?;
                            let frame = &mut stack[top_idx];
                            write_register(frame, dst, result)?;
                            frame.advance_pc_fast();
                        }
                        _ => unreachable!("equality opcode group"),
                    }
                    continue;
                }
                Op::ArrayLength => {
                    let dst = instr.reg(0);
                    let src = instr.reg(1);
                    let source = *read_register(&stack[top_idx], src)?;
                    let result = self.scalar_value(
                        stack,
                        context,
                        crate::ScalarValueOp::ArrayLength,
                        source,
                        Value::undefined(),
                        Value::undefined(),
                    )?;
                    let frame = &mut stack[top_idx];
                    write_register(frame, dst, result)?;
                    frame.advance_pc()?;
                    continue;
                }
                Op::IsArray => {
                    let dst = instr.reg(0);
                    let src = instr.reg(1);
                    let source = *read_register(&stack[top_idx], src)?;
                    let result = self.scalar_value(
                        stack,
                        context,
                        crate::ScalarValueOp::IsArray,
                        source,
                        Value::undefined(),
                        Value::undefined(),
                    )?;
                    let frame = &mut stack[top_idx];
                    write_register(frame, dst, result)?;
                    frame.advance_pc()?;
                    continue;
                }
                Op::IsEvalIntrinsic => {
                    let dst = instr.reg(0);
                    let src = instr.reg(1);
                    self.run_is_eval_intrinsic_reg(stack, top_idx, dst, src)?;
                    continue;
                }
                Op::MakeClosure => {
                    let dst = instr.reg(0);
                    let function_index = instr.const_word(1);
                    let ctx = instr.reg(2);
                    let frame = &mut stack[top_idx];
                    self.run_make_closure_regs(context, frame, dst, function_index, ctx)?;
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
