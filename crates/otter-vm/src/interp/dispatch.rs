//! The bytecode dispatch loop.
//!
//! # Contents
//! `dispatch_loop_inner`: one `match` arm per opcode, inline caches,
//! the JIT tier-up/backedge hooks, and additive optimizing-tier back-edge
//! accounting. Deliberately a single function — splitting it would defeat the
//! dispatch-locality the interpreter depends on.
//!
//! # Invariants
//! One run dispatches one activation: its record, owning chunk and CodeBlock
//! are resolved once, and the record's `COMPLETED` bit is the only frame test
//! between instructions. Arms that can stage a call or complete the
//! activation continue at `'activation`, which tests for both; arms that only
//! compute in registers and the PC continue at `'dispatch`. Opcode work and
//! source work are metered in registers and published when the run leaves
//! (source work also before every back-edge decision).
//! Retraining charges only fetched interpreter opcodes, never static loop
//! spans or compiled execution. Activation-local evidence survives staged calls.
//! Completed terminal semantic failures leave through `DispatchOutcome::Fatal`;
//! ordinary source errors retain the existing `VmError` handler path.
//!
//! # See also
//! - [`super::jit_retraining`] for deopt replacement admission.
#![allow(unused_imports)]
use super::call_dispatch::{DispatchOutcome, EnteredActivation};
use crate::*;

/// Source opcodes an interpreter run entered but has not yet charged to its
/// CodeBlock's tier-policy cell. Publishing on drop covers every way a run
/// leaves; the cell outlives the run because its CodeBlock does.
struct PendingSourceWork<'a> {
    cell: &'a crate::native_abi::SourceWork,
    pending: u64,
}

impl<'a> PendingSourceWork<'a> {
    #[inline]
    fn new(cell: &'a crate::native_abi::SourceWork) -> Self {
        Self { cell, pending: 0 }
    }

    #[inline]
    fn count(&mut self) {
        self.pending += 1;
    }

    #[inline]
    fn publish(&mut self) {
        if self.pending != 0 {
            self.cell.charge(std::mem::take(&mut self.pending));
        }
    }
}

impl Drop for PendingSourceWork<'_> {
    #[inline]
    fn drop(&mut self) {
        self.publish();
    }
}

/// PC of the instruction an interpreter run last fetched, kept in a register
/// and published to the caller however the run leaves.
struct TickPc<'a> {
    out: &'a mut u32,
    pc: u32,
}

impl Drop for TickPc<'_> {
    #[inline]
    fn drop(&mut self) {
        *self.out = self.pc;
    }
}

/// Budget weight an interpreter run metered in a register, added to the
/// caller's count however the run leaves.
struct PendingBudgetWork<'a> {
    out: &'a mut u64,
    units: u64,
}

impl Drop for PendingBudgetWork<'_> {
    #[inline]
    fn drop(&mut self) {
        *self.out = self.out.wrapping_add(self.units);
    }
}

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
        instr: CodeBlockInstruction<'_>,
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
    /// `entered` is the activation as its entry already resolved it in
    /// `entry_context`.
    #[inline(always)]
    pub(crate) fn dispatch_loop_inner(
        &mut self,
        entry_context: &ExecutionContext,
        entered: Option<EnteredActivation<'_>>,
        stack: &mut ActivationStack,
        floor: ActivationFloor,
    ) -> Result<DispatchOutcome, VmError> {
        let mut tick_pc = u32::MAX;
        let mut work_units = 0;
        let outcome = self.dispatch_ticks(
            entry_context,
            entered,
            stack,
            floor,
            &mut tick_pc,
            &mut work_units,
        );
        self.work_budget_stats.record_work(work_units);
        let staged = matches!(outcome, Ok(DispatchOutcome::Call));
        if (staged || outcome.is_err() || matches!(outcome, Ok(DispatchOutcome::Fatal(_))))
            && tick_pc != u32::MAX
        {
            let (frame, depth) = stack.top_with_depth();
            // SAFETY: a live chain above the floor has this live top record.
            if depth > floor.depth()
                && let Some(frame) = unsafe { frame.as_mut() }
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
        }
        outcome
    }

    fn dispatch_ticks(
        &mut self,
        entry_context: &ExecutionContext,
        entered: Option<EnteredActivation<'_>>,
        stack: &mut ActivationStack,
        floor: ActivationFloor,
        tick_pc: &mut u32,
        work_units: &mut u64,
    ) -> Result<DispatchOutcome, VmError> {
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
        // Cooperative cancellation is polled where execution can repeat: this
        // entry, every back-edge (`apply_branch`), and `Op::TailCall`. A run
        // that reaches none of them is bounded by the instruction stream and the
        // stack-depth limit, so the per-instruction poll it replaces bought no
        // extra termination guarantee.
        if self.interrupt.is_set() {
            return Err(VmError::Interrupted);
        }
        // An interpreter turn dispatches exactly one activation: its floor is
        // the activation's caller, a call stages a request and leaves, and a
        // nested synchronous run restores the published frame before its
        // handler returns. The top frame, the chunk that owns its function and
        // the executable CodeBlock are therefore invariants of this whole run
        // and are resolved here once. The frame leaves the chain only by
        // completing (return, unwind past its handlers, suspension), which
        // sets its `COMPLETED` bit; that bit is the only per-instruction frame
        // test.
        let (top_ptr, depth, entered_function) = match entered {
            // SAFETY: the entry's published record is live for its whole turn.
            Some(entered)
                if !unsafe { &*entered.frame }
                    .header
                    .flags
                    .contains(crate::native_abi::NativeFrameFlags::COMPLETED) =>
            {
                // SAFETY: as above.
                let depth = unsafe { crate::native_abi::Frame::logical_depth(entered.frame) };
                (entered.frame, depth as usize, Some(entered.function))
            }
            _ => {
                let (top_ptr, depth) = stack.top_with_depth();
                (top_ptr, depth, None)
            }
        };
        debug_assert!(
            entered_function.is_none() || stack.top_with_depth() == (top_ptr, depth),
            "an entry dispatches its own published activation"
        );
        if depth <= floor.depth() {
            // Unwind paths (throw / finally) can complete the activation
            // without writing back to a caller register. An async activation
            // that absorbed its throw completes with its rejected result
            // promise.
            let result = self
                .completed_activation_result
                .take()
                .unwrap_or_else(Value::undefined);
            return Ok(DispatchOutcome::Returned(result));
        }
        #[allow(unused_macros)]
        macro_rules! top_frame {
            () => {
                // SAFETY: `top_ptr` is this run's live activation (see above);
                // each use is a fresh, short borrow at the same point the
                // handler used to index the stack.
                unsafe { &mut *top_ptr }
            };
        }
        let top_idx = depth - 1;
        let function_id = top_frame!().function_id;
        // One stack can interleave frames from several code chunks (closures
        // escaped from `eval` / `new Function` / sibling scripts): constants,
        // atoms, and module resolutions are chunk-local, so the activation
        // dispatches against the chunk owning its function.
        let foreign_context: Option<ExecutionContext>;
        let context: &ExecutionContext =
            if entered_function.is_some() || entry_context.covers_function(function_id) {
                entry_context
            } else {
                foreign_context = match entry_context.for_function(function_id) {
                    Ok(code_space::ResolvedCtx::Owned(owned)) => {
                        // Foreign chunks linked after this loop started (eval
                        // during this turn) carry IC sites past the entry chunk's
                        // range.
                        self.ensure_method_feedback_context(&owned);
                        Some(owned)
                    }
                    _ => None,
                };
                foreign_context
                    .as_ref()
                    .ok_or_else(|| VmError::InvalidOperand)?
            };
        // SAFETY: compiled code is never GC-managed and never moves, and the
        // owning context outlives this run, so the CodeBlock borrow is valid
        // for every instruction of it.
        let function: &CodeBlock = match entered_function {
            Some(function) if function.id == function_id => function,
            _ => context
                .exec_function(function_id)
                .ok_or_else(|| VmError::InvalidOperand)?,
        };
        // Each activation is entered at a fresh depth, so the maximum is
        // sampled once per run rather than on every instruction.
        let depth32 = u32::try_from(depth).unwrap_or(u32::MAX);
        if depth32 > self.work_budget_stats.max_stack_depth_observed {
            self.work_budget_stats.max_stack_depth_observed = depth32;
        }
        // Opcode work is metered in registers and published when the run
        // leaves: the budget weight through `work_units`, which the caller
        // records, and the tier policy's source work through this charge,
        // which also publishes before every back-edge decision. Only a
        // rejecting budget reads the ledger mid-run, and its checkpoint
        // publishes first.
        let mut source_work = PendingSourceWork::new(function.source_work());
        let mut budget_work = PendingBudgetWork {
            out: work_units,
            units: 0,
        };
        let mut tick = TickPc {
            out: tick_pc,
            pc: u32::MAX,
        };
        // A schema register of this activation; the build-time verifier
        // bounded it against the window.
        macro_rules! register {
            ($index:expr) => {{
                let frame: &Frame = top_frame!();
                // SAFETY: see `read_register_unchecked`.
                unsafe { read_register_unchecked(frame, $index) }
            }};
        }
        // Number arms commit their inline fold and continue; any other operand
        // falls through to the arm's full semantic path. Feedback mirrors that
        // path: arithmetic also records the result's representation, an
        // immediate form and a comparison only their operands.
        macro_rules! commit_number_fold {
            ($next:lifetime, $feedback:expr, $fold:path, $op:expr, $dst:expr, $lhs:expr, $rhs:expr, $records_result:expr) => {{
                let (lhs, rhs): (Value, Value) = ($lhs, $rhs);
                if let Some(result) = $fold($op, lhs, rhs) {
                    if let Some(feedback) = $feedback {
                        feedback.record_arith(lhs, rhs);
                        if $records_result {
                            feedback.record_arith(result, result);
                        }
                    }
                    let frame = top_frame!();
                    // SAFETY: `dst` is a schema register.
                    unsafe { write_register_unchecked(frame, $dst, result) };
                    frame.advance_pc_fast();
                    continue $next;
                }
            }};
        }
        // An arm that can stage a call or complete the activation continues
        // at `'activation`, which tests both; an arm that only computes in
        // registers and the PC (number folds, branches, locals, constants)
        // continues straight at `'dispatch`.
        'activation: loop {
            if stack.has_pending_call() {
                return Ok(DispatchOutcome::Call);
            }
            if top_frame!()
                .header
                .flags
                .contains(crate::native_abi::NativeFrameFlags::COMPLETED)
            {
                let result = self
                    .completed_activation_result
                    .take()
                    .unwrap_or_else(Value::undefined);
                return Ok(DispatchOutcome::Returned(result));
            }
            'dispatch: loop {
                debug_assert_eq!(
                    stack.top_with_depth(),
                    (top_ptr, depth),
                    "an interpreter run dispatches one activation"
                );
                let pc = top_frame!().pc;
                tick.pc = pc;
                let idx = pc as usize;
                let instr = function.instr_at_index(idx).ok_or(VmError::MissingReturn)?;
                let op = function.op(instr);
                // Feedback is dense in the owning CodeBlock. Interpreter-only
                // execution keeps the cell untouched and pays no atomic update.
                let feedback = if jit_installed {
                    source_work.count();
                    function.feedback_recorder_at(idx)
                } else {
                    None
                };
                // Every opcode has a static base work charge; the checkpoint stays
                // gated on `enforce_budget` in the default Observe mode.
                budget_work.units = budget_work.units.wrapping_add(instr.reductions());
                // Budget enforcement, CPU sampling, and step tracing are each
                // installed between turns, never mid-loop. Testing their union once
                // keeps the common no-hook instruction at a single not-taken branch
                // instead of three; the individual tests only run when some hook is
                // actually present.
                if has_hooks {
                    if enforce_budget {
                        self.work_budget_stats
                            .record_work(std::mem::take(&mut budget_work.units));
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
                        let value = top_frame!()
                            .registers
                            .get(src as usize)
                            .cloned()
                            .ok_or_else(|| VmError::InvalidOperand)?;
                        if let Some(popped) =
                            self.complete_activation(stack, top_ptr, depth, floor, value, None)?
                        {
                            return Ok(DispatchOutcome::Returned(popped));
                        }
                        continue 'activation;
                    }
                    Op::ReturnUndefined => {
                        if let Some(popped) = self.complete_activation(
                            stack,
                            top_ptr,
                            depth,
                            floor,
                            Value::undefined(),
                            None,
                        )? {
                            return Ok(DispatchOutcome::Returned(popped));
                        }
                        continue 'activation;
                    }
                    // §10.2.2 [[Construct]] steps 10–12 for a derived constructor:
                    // the completion reads `this` from the `DerivedThis` slot.
                    Op::ReturnDerived => {
                        let (value_reg, ctx_reg) = (instr.reg(0), instr.reg(1));
                        let coord = otter_bytecode::ContextCoord::from_imm32(instr.imm(2))
                            .ok_or(VmError::InvalidOperand)?;
                        let frame = &(*top_frame!());
                        let value = *read_register(frame, value_reg)?;
                        let ctx = *read_register(frame, ctx_reg)?;
                        let derived_this = self.derived_this_slot_value(ctx, coord)?;
                        if let Some(popped) =
                            self.pop_frame_above(stack, floor, value, Some(derived_this))?
                        {
                            return Ok(DispatchOutcome::Returned(popped));
                        }
                        continue 'activation;
                    }
                    Op::Call | Op::CallWithThis => {
                        // Both forms already own the loaded callable. Record its
                        // exact native identity before execution can collect;
                        // bytecode dispatch still supplies the resolved frame id.
                        if jit_installed {
                            self.record_call_attempt_feedback(function, instr.instruction_pc);
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
                        let native_target = if jit_installed {
                            register_operand(function.operand(instr, 1))
                                .ok()
                                .and_then(|register| {
                                    stack
                                        .get(top_idx)
                                        .and_then(|frame| frame.registers.get(register as usize))
                                        .copied()
                                })
                                .and_then(|callee| self.native_call_target(callee))
                        } else {
                            None
                        };
                        if op == Op::CallWithThis {
                            self.do_call_with_this_exec(stack, function, instr)?;
                        } else {
                            self.do_call_exec(stack, top_ptr, function, instr)?;
                        }
                        // Record the resolved typed target before a tier-up hook
                        // consumes a newly pushed bytecode frame. Static natives
                        // complete synchronously and therefore leave stack depth
                        // unchanged. A native callee is the site's target even when
                        // it forwarded to the function it staged.
                        if jit_installed
                            && let Some(target) = if function_call_target.is_some() {
                                function_call_target
                            } else if native_target.is_some() {
                                native_target
                            } else {
                                self.staged_bytecode_target(stack)
                                    .map(crate::feedback::OrdinaryCallTarget::Bytecode)
                            }
                        {
                            let _ = self.record_ordinary_call_feedback(
                                function,
                                instr.instruction_pc,
                                target,
                            );
                        }
                        continue 'activation;
                    }
                    Op::TailCall => {
                        let native_target = jit_installed
                            .then(|| {
                                function
                                    .register(instr, 1)
                                    .and_then(|register| {
                                        top_frame!().registers.get(register as usize).copied()
                                    })
                                    .and_then(|callee| self.native_call_target(callee))
                            })
                            .flatten();
                        // Cooperative cancellation is polled at back-edges
                        // (`apply_branch`) and here: unbounded execution needs
                        // either a back-edge or an unbounded tail-call chain, and
                        // every other shape terminates against the instruction
                        // stream or the stack-depth limit.
                        if self.interrupt.is_set() {
                            return Err(VmError::Interrupted);
                        }
                        if jit_installed {
                            self.record_call_attempt_feedback(function, instr.instruction_pc);
                        }
                        self.do_tail_call_exec(stack, function, instr)?;
                        if jit_installed
                            && let Some(target) = self
                                .staged_bytecode_target(stack)
                                .map(crate::feedback::OrdinaryCallTarget::Bytecode)
                                .or(native_target)
                        {
                            let _ = self.record_ordinary_call_feedback(
                                function,
                                instr.instruction_pc,
                                target,
                            );
                        }
                        continue 'activation;
                    }
                    Op::CallForwardArguments => {
                        let native_target = jit_installed
                        .then(|| {
                            let frame = &(*top_frame!());
                            let operand = |index| {
                                function.register(instr, index).and_then(|register| {
                                    frame.registers.get(register as usize).copied()
                                })
                            };
                            let method = operand(1)?;
                            let callee = if crate::method_ops::is_function_prototype_intrinsic_value(
                                method,
                                &self.gc_heap,
                                crate::native_function::VmIntrinsicFunction::FunctionPrototypeApply,
                            ) {
                                operand(2)?
                            } else {
                                method
                            };
                            self.native_call_target(callee)
                        })
                        .flatten();
                        if jit_installed {
                            self.record_call_attempt_feedback(function, instr.instruction_pc);
                        }
                        (match self.do_call_forward_arguments_exec(stack, context, function, instr)
                        {
                            Ok(value) => value,
                            Err(CommittedValueError::JavaScript(error)) => return Err(error),
                            Err(CommittedValueError::Fatal(error)) => {
                                return Ok(DispatchOutcome::Fatal(error));
                            }
                        });
                        if jit_installed
                            && let Some(target) = self
                                .staged_bytecode_target(stack)
                                .map(crate::feedback::OrdinaryCallTarget::Bytecode)
                                .or(native_target)
                        {
                            let _ = self.record_ordinary_call_feedback(
                                function,
                                instr.instruction_pc,
                                target,
                            );
                        }
                        continue 'activation;
                    }
                    Op::CallMethodValue => {
                        if jit_installed {
                            self.record_call_attempt_feedback(function, instr.instruction_pc);
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
                        if let (Some(recv), Some(name_idx), Some(site)) =
                            (receiver, name_idx, feedback_site)
                            && let Ok(argc) = const_operand(function.operand(instr, 3))
                            && self
                                .recorded_method_call(
                                    context,
                                    function_id,
                                    name_idx,
                                    site,
                                    context.property_feedback_slot(
                                        function_id,
                                        instr.instruction_pc,
                                        crate::property_ic::PropertyIcKind::Load,
                                    ),
                                    recv,
                                    argc as usize,
                                )
                                .is_some()
                        {
                            receiver = None;
                        }
                        // The receiver is refreshed in place: resolving the site can
                        // migrate a dictionary-mode receiver onto the shaped path,
                        // and that allocation may relocate it.
                        let method_site = match (receiver.as_mut(), name_idx) {
                            (Some(recv), Some(name_idx)) => {
                                self.method_site_for_receiver(context, function_id, name_idx, recv)
                            }
                            _ => None,
                        };
                        // An object receiver no guard can describe gets no record;
                        // without saturation every later call would repeat the
                        // capture. A primitive receiver costs no capture work.
                        if receiver.is_some_and(|recv| recv.as_object().is_some())
                            && method_site.is_none()
                            && let Some(site) = feedback_site
                        {
                            self.saturate_method_site_feedback(site);
                        }
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
                        match self.do_call_method_value_exec(stack, context, function, instr) {
                            Ok(()) => {}
                            Err(CommittedValueError::JavaScript(error)) => return Err(error),
                            Err(CommittedValueError::Fatal(error)) => {
                                return Ok(DispatchOutcome::Fatal(error));
                            }
                        }
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
                            let _ = self.record_ordinary_call_feedback(
                                function,
                                instr.instruction_pc,
                                target,
                            );
                        } else if jit_installed
                            && let Some(method_fid) = self.staged_bytecode_target(stack)
                        {
                            if let (Some(feedback_site), Some(site)) = (feedback_site, method_site)
                            {
                                let changed =
                                    self.note_method_target(feedback_site, method_fid, site);
                                self.commit_method_call_feedback_transition(function, changed);
                            }
                        } else if jit_installed
                            && let (Some(feedback_site), Some(site), Some(stub_id)) =
                                (feedback_site, method_site, native_leaf)
                        {
                            let changed = self.record_method_native_leaf_feedback(
                                feedback_site,
                                stub_id,
                                site,
                            );
                            self.commit_method_call_feedback_transition(function, changed);
                        }
                        continue 'activation;
                    }
                    Op::CallSpread => {
                        let native_target = jit_installed
                            .then(|| {
                                function
                                    .register(instr, 1)
                                    .and_then(|register| {
                                        top_frame!().registers.get(register as usize).copied()
                                    })
                                    .and_then(|callee| self.native_call_target(callee))
                            })
                            .flatten();
                        let operands = function.operand_view(instr);
                        self.do_call_spread(stack, operands)?;
                        if jit_installed
                            && let Some(target) = self
                                .staged_bytecode_target(stack)
                                .map(crate::feedback::OrdinaryCallTarget::Bytecode)
                                .or(native_target)
                        {
                            let _ = self.record_ordinary_call_feedback(
                                function,
                                instr.instruction_pc,
                                target,
                            );
                        }
                        continue 'activation;
                    }
                    Op::New => {
                        let native_target = jit_installed
                            .then(|| {
                                function
                                    .register(instr, 1)
                                    .and_then(|register| {
                                        top_frame!().registers.get(register as usize).copied()
                                    })
                                    .and_then(|callee| self.native_construct_target(callee))
                            })
                            .flatten();
                        if jit_installed {
                            self.record_call_attempt_feedback(function, instr.instruction_pc);
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
                            let _ = self.record_ordinary_call_feedback(
                                function,
                                instr.instruction_pc,
                                crate::feedback::OrdinaryCallTarget::Bytecode(function_id),
                            );
                        }

                        if let Some(target) = native_target {
                            let _ = self.record_ordinary_call_feedback(
                                function,
                                instr.instruction_pc,
                                target,
                            );
                        }
                        continue 'activation;
                    }
                    Op::SuperConstruct => {
                        let native_target = jit_installed
                            .then(|| {
                                function
                                    .register(instr, 1)
                                    .and_then(|register| {
                                        top_frame!().registers.get(register as usize).copied()
                                    })
                                    .and_then(|callee| {
                                        callee
                                            .as_native_function()
                                            .map(|_| crate::feedback::OrdinaryCallTarget::Native)
                                    })
                            })
                            .flatten();
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
                            let _ = self.record_ordinary_call_feedback(
                                function,
                                instr.instruction_pc,
                                crate::feedback::OrdinaryCallTarget::Bytecode(function_id),
                            );
                        }

                        if let Some(target) = native_target {
                            let _ = self.record_ordinary_call_feedback(
                                function,
                                instr.instruction_pc,
                                target,
                            );
                        }
                        continue 'activation;
                    }
                    Op::NewSpread => {
                        let native_target = jit_installed
                            .then(|| {
                                function
                                    .register(instr, 1)
                                    .and_then(|register| {
                                        top_frame!().registers.get(register as usize).copied()
                                    })
                                    .and_then(|callee| {
                                        callee
                                            .as_native_function()
                                            .map(|_| crate::feedback::OrdinaryCallTarget::Native)
                                    })
                            })
                            .flatten();
                        let direct_construct_fid = if jit_installed {
                            register_operand(function.operand(instr, 1))
                                .ok()
                                .and_then(|register| top_frame!().registers.get(register as usize))
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
                            let _ = self.record_ordinary_call_feedback(
                                function,
                                instr.instruction_pc,
                                crate::feedback::OrdinaryCallTarget::Bytecode(function_id),
                            );
                        }

                        if let Some(target) = native_target {
                            let _ = self.record_ordinary_call_feedback(
                                function,
                                instr.instruction_pc,
                                target,
                            );
                        }
                        continue 'activation;
                    }
                    Op::SuperConstructSpread => {
                        let native_target = jit_installed
                            .then(|| {
                                function
                                    .register(instr, 1)
                                    .and_then(|register| {
                                        top_frame!().registers.get(register as usize).copied()
                                    })
                                    .and_then(|callee| {
                                        callee
                                            .as_native_function()
                                            .map(|_| crate::feedback::OrdinaryCallTarget::Native)
                                    })
                            })
                            .flatten();
                        let direct_construct_fid = if jit_installed {
                            register_operand(function.operand(instr, 1))
                                .ok()
                                .and_then(|register| top_frame!().registers.get(register as usize))
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
                            let _ = self.record_ordinary_call_feedback(
                                function,
                                instr.instruction_pc,
                                crate::feedback::OrdinaryCallTarget::Bytecode(function_id),
                            );
                        }

                        if let Some(target) = native_target {
                            let _ = self.record_ordinary_call_feedback(
                                function,
                                instr.instruction_pc,
                                target,
                            );
                        }
                        continue 'activation;
                    }
                    Op::BindThisValue => {
                        let src = instr.reg(0);
                        self.run_bind_this_value_reg(stack, top_idx, src)?;
                        continue 'activation;
                    }
                    Op::Throw => {
                        let src = instr.reg(0);
                        let value = top_frame!()
                            .registers
                            .get(src as usize)
                            .cloned()
                            .ok_or_else(|| VmError::InvalidOperand)?;
                        // The unwind owns throw-site provenance: it captures
                        // frames only when this throw leaves the region.
                        let unwind = self.unwind_throw_above(
                            context,
                            stack,
                            floor,
                            value,
                            crate::activation_stack::ThrowSite::Instruction,
                        );
                        if unwind.is_err() {
                            // No handler in this dispatch stack — stash
                            // the thrown VALUE so outer loops / native
                            // boundaries keep identity instead of the
                            // rendered string.
                            self.pending_uncaught_throw = Some(value);
                        }
                        unwind?;
                        continue 'activation;
                    }
                    Op::Await => {
                        let dst = instr.reg(0);
                        let src = instr.reg(1);
                        let awaited = *read_register(&(*top_frame!()), src)?;
                        // A suspended async activation completes with its result
                        // promise, rooted across the await's allocations.
                        let result = self
                            .frame_cold(&(*top_frame!()))
                            .and_then(|cold| cold.async_state.as_ref())
                            .map_or(Value::undefined(), |state| {
                                Value::promise(state.result_promise)
                            });
                        let anchor = self.push_iteration_anchor(result) - 1;
                        let awaited_result = self.do_await(stack, context, dst, awaited);
                        let result = self.iteration_anchor(anchor);
                        self.pop_iteration_anchors_to(anchor);
                        match awaited_result {
                            Ok(()) => {}
                            Err(CommittedValueError::JavaScript(error)) => return Err(error),
                            Err(CommittedValueError::Fatal(error)) => {
                                return Ok(DispatchOutcome::Fatal(error));
                            }
                        }
                        if stack.is_at_floor(floor) {
                            return Ok(DispatchOutcome::Returned(result));
                        }
                        continue 'activation;
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
                        let yielded = *read_register(&(*top_frame!()), src)?;
                        let owner = self
                            .frame_generator_owner(&(*top_frame!()))
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
                            self.async_generator_complete_step(
                                Some(context),
                                &owner,
                                Ok(yielded),
                                false,
                            )?;
                            match self.async_generator_resume_next(stack, Some(context), &owner) {
                                Ok(()) => {}
                                Err(CommittedValueError::JavaScript(error)) => return Err(error),
                                Err(CommittedValueError::Fatal(error)) => {
                                    return Ok(DispatchOutcome::Fatal(error));
                                }
                            }
                        }
                        return Ok(DispatchOutcome::Returned(Value::undefined()));
                    }
                    Op::Yield => {
                        let (kind_dst, value_dst, src) = instr.reg3();
                        let yielded = *read_register(&(*top_frame!()), src)?;
                        let owner = self
                            .frame_generator_owner(&(*top_frame!()))
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
                            (match self
                                .async_generator_yield_awaited(stack, context, &owner, yielded)
                            {
                                Ok(value) => value,
                                Err(CommittedValueError::JavaScript(error)) => return Err(error),
                                Err(CommittedValueError::Fatal(error)) => {
                                    return Ok(DispatchOutcome::Fatal(error));
                                }
                            });
                        }
                        return Ok(DispatchOutcome::Returned(yielded));
                    }
                    Op::GeneratorStart => {
                        let owner = self
                            .frame_generator_owner(&(*top_frame!()))
                            .ok_or(VmError::TypeMismatch)?;
                        let callee = top_frame!().self_value;
                        let frame = stack.last_mut().ok_or_else(|| VmError::InvalidOperand)?;
                        frame.advance_pc()?;
                        let popped = stack.pop().expect("frame present");
                        let detached_cold = self.frame_detach_cold(popped);
                        let popped = self.park_active_frame(popped);
                        owner.park_frame(&mut self.gc_heap, popped, detached_cold);
                        // §27.5.1 step 3: the prototype is read after
                        // FunctionDeclarationInstantiation; the generator object is
                        // this call's completion.
                        let generator = match self.resolve_started_generator(
                            stack,
                            context,
                            owner,
                            callee,
                            function_id,
                        ) {
                            Ok(value) => value,
                            Err(CommittedValueError::JavaScript(error)) => return Err(error),
                            Err(CommittedValueError::Fatal(error)) => {
                                return Ok(DispatchOutcome::Fatal(error));
                            }
                        };
                        return Ok(DispatchOutcome::Returned(generator));
                    }
                    // §7.1.4 ToNumber — the shared synchronous helper owns the
                    // full ToPrimitive(number) ladder before committing `dst`.
                    Op::ToNumber => {
                        let dst = instr.reg(0);
                        let src = instr.reg(1);
                        (match self.run_to_number_regs(context, stack, top_idx, dst, src) {
                            Ok(value) => value,
                            Err(CommittedValueError::JavaScript(error)) => return Err(error),
                            Err(CommittedValueError::Fatal(error)) => {
                                return Ok(DispatchOutcome::Fatal(error));
                            }
                        });
                        continue 'activation;
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
                        let recv = *read_register(&(*top_frame!()), src)?;
                        if recv.is_primitive() {
                            write_register(&mut (*top_frame!()), dst, recv)?;
                            top_frame!().advance_pc()?;
                            continue 'activation;
                        }
                        (match self.drive_to_primitive(stack, context, function.operand_view(instr))
                        {
                            Ok(value) => value,
                            Err(CommittedValueError::JavaScript(error)) => return Err(error),
                            Err(CommittedValueError::Fatal(error)) => {
                                return Ok(DispatchOutcome::Fatal(error));
                            }
                        });
                        continue 'activation;
                    }
                    // §7.4.3 `GetIterator`. Built-in iterables fall
                    // through to the in-frame fast path; user objects
                    // route through the call-frame ladder.
                    // <https://tc39.es/ecma262/#sec-getiterator>
                    Op::GetIterator => {
                        let operands = function.operand_view(instr);
                        match self.drive_get_iterator(stack, context, operands) {
                            Ok(()) => {}
                            Err(CommittedValueError::JavaScript(error)) => return Err(error),
                            Err(CommittedValueError::Fatal(error)) => {
                                return Ok(DispatchOutcome::Fatal(error));
                            }
                        }
                        continue 'activation;
                    }
                    Op::GetAsyncIterator => {
                        let dst = instr.reg(0);
                        let src = instr.reg(1);
                        (match self.run_get_async_iterator_regs(
                            context,
                            &mut *stack,
                            top_idx,
                            dst,
                            src,
                        ) {
                            Ok(value) => value,
                            Err(CommittedValueError::JavaScript(error)) => return Err(error),
                            Err(CommittedValueError::Fatal(error)) => {
                                return Ok(DispatchOutcome::Fatal(error));
                            }
                        });
                        continue 'activation;
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
                        let cursor =
                            crate::iterator_record::RecordCursor::read(&(*top_frame!()), iter_reg)?;
                        if cursor != crate::iterator_record::RecordCursor::Generic {
                            if let crate::iterator_record::RecordCursor::Fast(_) = cursor {
                                let array = *read_register(&(*top_frame!()), iter_reg)?;
                                self.record_element_family_feedback(
                                    function,
                                    instr.instruction_pc,
                                    array,
                                );
                            }
                            let (value, done) = match self
                                .step_fast_array_record(context, stack, top_idx, iter_reg)
                            {
                                Ok(step) => step,
                                Err(CommittedValueError::JavaScript(error)) => return Err(error),
                                Err(CommittedValueError::Fatal(error)) => {
                                    return Ok(DispatchOutcome::Fatal(error));
                                }
                            };
                            let frame = &mut (*top_frame!());
                            write_register(frame, instr.reg(0), value)?;
                            write_register(frame, instr.reg(1), Value::boolean(done))?;
                            frame.advance_pc()?;
                            continue 'activation;
                        }
                        let iterator = *read_register(&(*top_frame!()), iter_reg)?;
                        let operands = function.operand_view(instr);
                        match self.drive_iterator_next(stack, context, operands) {
                            Ok(true) => continue 'activation,
                            Ok(false) => {}
                            Err(error) => {
                                self.iterator_mark_done(iterator);
                                match error {
                                    CommittedValueError::JavaScript(error) => return Err(error),
                                    CommittedValueError::Fatal(error) => {
                                        return Ok(DispatchOutcome::Fatal(error));
                                    }
                                }
                            }
                        }
                        let value_dst = instr.reg(0);
                        let done_dst = instr.reg(1);
                        let frame = &mut (*top_frame!());
                        if let Err(e) =
                            self.run_iterator_next_regs(frame, value_dst, done_dst, iter_reg)
                        {
                            self.iterator_mark_done(iterator);
                            return Err(e);
                        }
                        continue 'activation;
                    }
                    // §7.4.11 IteratorClose for a normal completion.
                    Op::IteratorClose => {
                        match self.close_iterator_record(
                            context,
                            stack,
                            top_idx,
                            instr.reg(0),
                            false,
                        ) {
                            Ok(()) => {}
                            Err(CommittedValueError::JavaScript(error)) => return Err(error),
                            Err(CommittedValueError::Fatal(error)) => {
                                return Ok(DispatchOutcome::Fatal(error));
                            }
                        }
                        top_frame!().advance_pc()?;
                        continue 'activation;
                    }
                    // §7.4.11 IteratorClose for a throw completion: the handler
                    // rethrows its own value next.
                    Op::IteratorCloseThrow => {
                        match self.close_iterator_record(
                            context,
                            stack,
                            top_idx,
                            instr.reg(0),
                            true,
                        ) {
                            Ok(()) => {}
                            Err(CommittedValueError::JavaScript(error)) => return Err(error),
                            Err(CommittedValueError::Fatal(error)) => {
                                return Ok(DispatchOutcome::Fatal(error));
                            }
                        }
                        top_frame!().advance_pc()?;
                        continue 'activation;
                    }
                    Op::AsyncIteratorReturn => {
                        let result_reg = instr.reg(0);
                        let called_reg = instr.reg(1);
                        let iter_reg = instr.reg(2);
                        let iterator = *read_register(&(*top_frame!()), iter_reg)?;
                        // §7.4.11 steps 3-5: an iterator without a `return`
                        // is already closed, and the caller skips the await.
                        let outcome =
                            match self.async_iterator_return_call(stack, context, iterator) {
                                Ok(value) => value,
                                Err(CommittedValueError::JavaScript(error)) => return Err(error),
                                Err(CommittedValueError::Fatal(error)) => {
                                    return Ok(DispatchOutcome::Fatal(error));
                                }
                            };
                        let frame = &mut (*top_frame!());
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
                        continue 'activation;
                    }
                    Op::CheckIteratorResult => {
                        let value_reg = instr.reg(0);
                        let value = *read_register(&(*top_frame!()), value_reg)?;
                        // §7.4.11 step 8 — the awaited `return` result is an
                        // Object or the close is a TypeError.
                        if !value.is_object() && !value.is_proxy() {
                            return Err(self.err_type(
                                ("iterator `return` did not yield an object".to_string()).into(),
                            ));
                        }
                        top_frame!().advance_pc()?;
                        continue 'activation;
                    }
                    // §10.1.8 [[Get]] — when the resolved property is an
                    // accessor descriptor at any depth in the prototype
                    // chain, the runtime invokes the getter with `this`
                    // bound to the original receiver. Stack-modifying so
                    // it must run outside the in-frame mutable borrow
                    // below.
                    // <https://tc39.es/ecma262/#sec-ordinaryget>
                    Op::HasNamedProperty => {
                        let key = context
                            .property_atom(instr.const_word(2))
                            .ok_or(VmError::InvalidOperand)?;
                        let slot = function.property_feedback_at(
                            instr.instruction_pc as usize,
                            crate::property_ic::PropertyIcKind::Load,
                        );
                        (match self.run_has_named_property_reg(
                            context,
                            stack,
                            top_idx,
                            instr.reg(0),
                            instr.reg(1),
                            key,
                            slot,
                        ) {
                            Ok(value) => value,
                            Err(CommittedValueError::JavaScript(error)) => return Err(error),
                            Err(CommittedValueError::Fatal(error)) => {
                                return Ok(DispatchOutcome::Fatal(error));
                            }
                        });
                        continue 'activation;
                    }
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
                        if match self.drive_load_property(stack, context, dst, obj_reg, key, slot) {
                            Ok(value) => value,
                            Err(CommittedValueError::JavaScript(error)) => return Err(error),
                            Err(CommittedValueError::Fatal(error)) => {
                                return Ok(DispatchOutcome::Fatal(error));
                            }
                        } {
                            continue 'activation;
                        }
                        (match self.run_load_property_reg(
                            context,
                            &mut *stack,
                            top_idx,
                            dst,
                            obj_reg,
                            key,
                        ) {
                            Ok(value) => value,
                            Err(CommittedValueError::JavaScript(error)) => return Err(error),
                            Err(CommittedValueError::Fatal(error)) => {
                                return Ok(DispatchOutcome::Fatal(error));
                            }
                        });
                        continue 'activation;
                    }
                    Op::LoadElement => {
                        let operands = function.operand_view(instr);
                        if jit_installed
                            && let Some(recv_reg) = function.register(instr, 1)
                            && let Ok(recv) = read_register(&(*top_frame!()), recv_reg)
                        {
                            let recv = *recv;
                            self.record_element_family_feedback(
                                function,
                                instr.instruction_pc,
                                recv,
                            );
                        }
                        if match self.drive_load_element(stack, context, operands) {
                            Ok(value) => value,
                            Err(CommittedValueError::JavaScript(error)) => return Err(error),
                            Err(CommittedValueError::Fatal(error)) => {
                                return Ok(DispatchOutcome::Fatal(error));
                            }
                        } {
                            continue 'activation;
                        }
                        let (dst, recv_reg, idx_reg) = instr.reg3();
                        (match self
                            .run_load_element_regs(context, stack, top_idx, dst, recv_reg, idx_reg)
                        {
                            Ok(value) => value,
                            Err(CommittedValueError::JavaScript(error)) => return Err(error),
                            Err(CommittedValueError::Fatal(error)) => {
                                return Ok(DispatchOutcome::Fatal(error));
                            }
                        });
                        continue 'activation;
                    }
                    Op::LoadSuperProperty => {
                        let dst = instr.reg(0);
                        let home_reg = instr.reg(1);
                        let name_idx = instr.const_word(2);
                        let name = context
                            .property_atom(name_idx)
                            .ok_or_else(|| VmError::InvalidOperand)?
                            .name();
                        let home = *read_register(&(*top_frame!()), home_reg)?;
                        (match self.run_load_super_property(
                            context,
                            stack,
                            top_idx,
                            dst,
                            home,
                            SuperReadKey::Resolved(VmPropertyKey::String(name)),
                        ) {
                            Ok(value) => value,
                            Err(CommittedValueError::JavaScript(error)) => return Err(error),
                            Err(CommittedValueError::Fatal(error)) => {
                                return Ok(DispatchOutcome::Fatal(error));
                            }
                        });
                        continue 'activation;
                    }
                    Op::LoadSuperElement => {
                        let (dst, home_reg, key_reg) = instr.reg3();
                        let home = *read_register(&(*top_frame!()), home_reg)?;
                        let key_raw = *read_register(&(*top_frame!()), key_reg)?;
                        (match self.run_load_super_property(
                            context,
                            stack,
                            top_idx,
                            dst,
                            home,
                            SuperReadKey::Computed(key_raw),
                        ) {
                            Ok(value) => value,
                            Err(CommittedValueError::JavaScript(error)) => return Err(error),
                            Err(CommittedValueError::Fatal(error)) => {
                                return Ok(DispatchOutcome::Fatal(error));
                            }
                        });
                        continue 'activation;
                    }
                    Op::SetSuperProperty => {
                        let home_reg = instr.reg(0);
                        let name_idx = instr.const_word(1);
                        let value_reg = instr.reg(2);
                        let name = context
                            .property_atom(name_idx)
                            .ok_or_else(|| VmError::InvalidOperand)?
                            .name();
                        let home = *read_register(&(*top_frame!()), home_reg)?;
                        let value = *read_register(&(*top_frame!()), value_reg)?;
                        let strict = context.function_is_strict(top_frame!().function_id);
                        (match self.run_store_super_property(
                            context,
                            stack,
                            top_idx,
                            home,
                            SuperReadKey::Resolved(VmPropertyKey::String(name)),
                            value,
                            strict,
                        ) {
                            Ok(value) => value,
                            Err(CommittedValueError::JavaScript(error)) => return Err(error),
                            Err(CommittedValueError::Fatal(error)) => {
                                return Ok(DispatchOutcome::Fatal(error));
                            }
                        });
                        continue 'activation;
                    }
                    Op::SetSuperElement => {
                        let (home_reg, key_reg, value_reg) = instr.reg3();
                        let home = *read_register(&(*top_frame!()), home_reg)?;
                        let key_raw = *read_register(&(*top_frame!()), key_reg)?;
                        let value = *read_register(&(*top_frame!()), value_reg)?;
                        let strict = context.function_is_strict(top_frame!().function_id);
                        (match self.run_store_super_property(
                            context,
                            stack,
                            top_idx,
                            home,
                            SuperReadKey::Computed(key_raw),
                            value,
                            strict,
                        ) {
                            Ok(value) => value,
                            Err(CommittedValueError::JavaScript(error)) => return Err(error),
                            Err(CommittedValueError::Fatal(error)) => {
                                return Ok(DispatchOutcome::Fatal(error));
                            }
                        });
                        continue 'activation;
                    }
                    // §10.1.9 [[Set]] — accessor setter dispatch follows
                    // the same pattern as `LoadProperty`. Non-writable
                    // and non-extensible rejections surface here too.
                    // <https://tc39.es/ecma262/#sec-ordinaryset>
                    Op::StoreProperty => {
                        let operands = function.operand_view(instr);
                        if match self.drive_store_property(stack, context, operands, false) {
                            Ok(value) => value,
                            Err(CommittedValueError::JavaScript(error)) => return Err(error),
                            Err(CommittedValueError::Fatal(error)) => {
                                return Ok(DispatchOutcome::Fatal(error));
                            }
                        } {
                            continue 'activation;
                        }
                        let obj_reg = instr.reg(0);
                        let name_idx = instr.const_word(1);
                        let src = instr.reg(2);
                        let key = context
                            .property_atom(name_idx)
                            .ok_or_else(|| VmError::InvalidOperand)?;
                        (match self.run_store_property_reg(
                            context,
                            &mut *stack,
                            top_idx,
                            obj_reg,
                            key,
                            src,
                            false,
                        ) {
                            Ok(value) => value,
                            Err(CommittedValueError::JavaScript(error)) => return Err(error),
                            Err(CommittedValueError::Fatal(error)) => {
                                return Ok(DispatchOutcome::Fatal(error));
                            }
                        });
                        continue 'activation;
                    }
                    // §15.7.1 — class heritage / computed-key stores keep
                    // strict PutValue semantics inside a sloppy frame.
                    Op::StorePropertyStrict => {
                        let operands = function.operand_view(instr);
                        if match self.drive_store_property(stack, context, operands, true) {
                            Ok(value) => value,
                            Err(CommittedValueError::JavaScript(error)) => return Err(error),
                            Err(CommittedValueError::Fatal(error)) => {
                                return Ok(DispatchOutcome::Fatal(error));
                            }
                        } {
                            continue 'activation;
                        }
                        let obj_reg = instr.reg(0);
                        let name_idx = instr.const_word(1);
                        let src = instr.reg(2);
                        let key = context
                            .property_atom(name_idx)
                            .ok_or_else(|| VmError::InvalidOperand)?;
                        (match self.run_store_property_reg(
                            context,
                            &mut *stack,
                            top_idx,
                            obj_reg,
                            key,
                            src,
                            true,
                        ) {
                            Ok(value) => value,
                            Err(CommittedValueError::JavaScript(error)) => return Err(error),
                            Err(CommittedValueError::Fatal(error)) => {
                                return Ok(DispatchOutcome::Fatal(error));
                            }
                        });
                        continue 'activation;
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
                            let value = *read_register(&(*top_frame!()), src_reg)?;
                            feedback.record_arith(value, value);
                        }
                        if jit_installed {
                            let recv = *read_register(&(*top_frame!()), recv_reg)?;
                            let key = *read_register(&(*top_frame!()), idx_reg)?;
                            self.record_element_family_feedback(
                                function,
                                instr.instruction_pc,
                                recv,
                            );
                            self.record_element_bounds_feedback(
                                function,
                                instr.instruction_pc,
                                recv,
                                key,
                            );
                        }
                        // Copy the operands through a short representation-neutral
                        // view, then end the frame borrow before `[[Set]]` can
                        // allocate, collect, or synchronously re-enter JavaScript.
                        let (function_id, receiver, key, value) = {
                            let frame = ActiveFrameRef::from_frame(&(*top_frame!()));
                            (
                                frame.function_id(),
                                frame.read(recv_reg)?,
                                frame.read(idx_reg)?,
                                frame.read(src_reg)?,
                            )
                        };
                        (match self.store_element_values(
                            stack,
                            context,
                            function_id,
                            receiver,
                            key,
                            value,
                            strict,
                        ) {
                            Ok(value) => value,
                            Err(CommittedValueError::JavaScript(error)) => return Err(error),
                            Err(CommittedValueError::Fatal(error)) => {
                                return Ok(DispatchOutcome::Fatal(error));
                            }
                        });
                        // The synchronous helper completed the full effect (or
                        // threw), so publish exactly one resume increment.
                        top_frame!().advance_pc()?;
                        continue 'activation;
                    }
                    Op::Instanceof => {
                        let (dst, lhs, rhs) = instr.reg3();
                        let lhs = *read_register(&(*top_frame!()), lhs)?;
                        let rhs = *read_register(&(*top_frame!()), rhs)?;
                        let result = match self.object_protocol_value(
                            stack,
                            context,
                            crate::ObjectProtocolValueOp::Instanceof,
                            lhs,
                            rhs,
                        ) {
                            Ok(value) => value,
                            Err(CommittedValueError::JavaScript(error)) => return Err(error),
                            Err(CommittedValueError::Fatal(error)) => {
                                return Ok(DispatchOutcome::Fatal(error));
                            }
                        };
                        let frame = &mut (*top_frame!());
                        write_register(frame, dst, result)?;
                        frame.advance_pc()?;
                        continue 'activation;
                    }
                    // §28.2.4.7 / .10 Proxy.[[HasProperty]] /
                    // [[Delete]] — invoke `has` / `deleteProperty`
                    // traps when the receiver is a Proxy.
                    Op::HasProperty => {
                        let (dst, lhs, rhs) = instr.reg3();
                        let lhs = *read_register(&(*top_frame!()), lhs)?;
                        let rhs = *read_register(&(*top_frame!()), rhs)?;
                        let result = match self.object_protocol_value(
                            stack,
                            context,
                            crate::ObjectProtocolValueOp::HasProperty,
                            lhs,
                            rhs,
                        ) {
                            Ok(value) => value,
                            Err(CommittedValueError::JavaScript(error)) => return Err(error),
                            Err(CommittedValueError::Fatal(error)) => {
                                return Ok(DispatchOutcome::Fatal(error));
                            }
                        };
                        let frame = &mut (*top_frame!());
                        write_register(frame, dst, result)?;
                        frame.advance_pc()?;
                        continue 'activation;
                    }
                    Op::DeleteProperty => {
                        let operands = function.operand_view(instr);
                        if match self.drive_delete_property_proxy(stack, context, operands) {
                            Ok(value) => value,
                            Err(CommittedValueError::JavaScript(error)) => return Err(error),
                            Err(CommittedValueError::Fatal(error)) => {
                                return Ok(DispatchOutcome::Fatal(error));
                            }
                        } {
                            continue 'activation;
                        }
                        let dst = instr.reg(0);
                        let obj_reg = instr.reg(1);
                        let name_idx = instr.const_word(2);
                        let key = context
                            .property_atom(name_idx)
                            .ok_or_else(|| VmError::InvalidOperand)?;
                        let strict = context.function_is_strict(top_frame!().function_id);
                        // `delete` has an object fast path that bypasses the
                        // §28.3 MOP funnel; trigger deferred-namespace
                        // evaluation here (named delete is never symbol-like
                        // unless the key is "then").
                        let receiver = *read_register(&(*top_frame!()), obj_reg)?;
                        if receiver.as_object().is_some_and(|o| {
                            crate::object::deferred_namespace_target(o, &self.gc_heap).is_some()
                        }) {
                            (match self.ensure_deferred_namespace_ready(
                                stack,
                                context,
                                &receiver,
                                key.name() != "then",
                            ) {
                                Ok(value) => value,
                                Err(CommittedValueError::JavaScript(error)) => return Err(error),
                                Err(CommittedValueError::Fatal(error)) => {
                                    return Ok(DispatchOutcome::Fatal(error));
                                }
                            });
                        }
                        let frame = &mut (*top_frame!());
                        self.run_delete_property_reg(context, frame, dst, obj_reg, key, strict)?;
                        continue 'activation;
                    }
                    Op::DeleteElement => {
                        let operands = function.operand_view(instr);
                        if match self.drive_delete_element_proxy(stack, context, operands) {
                            Ok(value) => value,
                            Err(CommittedValueError::JavaScript(error)) => return Err(error),
                            Err(CommittedValueError::Fatal(error)) => {
                                return Ok(DispatchOutcome::Fatal(error));
                            }
                        } {
                            continue 'activation;
                        }
                        let (dst, obj_reg, idx_reg) = instr.reg3();
                        let strict = context.function_is_strict(top_frame!().function_id);
                        let receiver = *read_register(&(*top_frame!()), obj_reg)?;
                        if receiver.as_object().is_some_and(|o| {
                            crate::object::deferred_namespace_target(o, &self.gc_heap).is_some()
                        }) {
                            let key_val = *read_register(&(*top_frame!()), idx_reg)?;
                            let symbol_like = key_val.as_symbol(&self.gc_heap).is_some()
                                || key_val
                                    .as_string(&self.gc_heap)
                                    .is_some_and(|s| s.to_lossy_string(&self.gc_heap) == "then");
                            (match self.ensure_deferred_namespace_ready(
                                stack,
                                context,
                                &receiver,
                                !symbol_like,
                            ) {
                                Ok(value) => value,
                                Err(CommittedValueError::JavaScript(error)) => return Err(error),
                                Err(CommittedValueError::Fatal(error)) => {
                                    return Ok(DispatchOutcome::Fatal(error));
                                }
                            });
                        }
                        (match self.run_delete_element_regs(
                            context, stack, top_idx, dst, obj_reg, idx_reg, strict,
                        ) {
                            Ok(value) => value,
                            Err(CommittedValueError::JavaScript(error)) => return Err(error),
                            Err(CommittedValueError::Fatal(error)) => {
                                return Ok(DispatchOutcome::Fatal(error));
                            }
                        });
                        continue 'activation;
                    }
                    // §28.2.4.1 / .2 Proxy.[[GetPrototypeOf]] /
                    // [[SetPrototypeOf]] — invoke `getPrototypeOf` /
                    // `setPrototypeOf` traps when the receiver is a
                    // Proxy.
                    Op::GetPrototype => {
                        let dst = instr.reg(0);
                        let src = instr.reg(1);
                        let source = *read_register(&(*top_frame!()), src)?;
                        let result = match self.object_protocol_value(
                            stack,
                            context,
                            crate::ObjectProtocolValueOp::GetPrototype,
                            source,
                            Value::undefined(),
                        ) {
                            Ok(value) => value,
                            Err(CommittedValueError::JavaScript(error)) => return Err(error),
                            Err(CommittedValueError::Fatal(error)) => {
                                return Ok(DispatchOutcome::Fatal(error));
                            }
                        };
                        let frame = &mut (*top_frame!());
                        write_register(frame, dst, result)?;
                        frame.advance_pc()?;
                        continue 'activation;
                    }
                    Op::SetPrototype => {
                        let obj_reg = instr.reg(0);
                        let proto_reg = instr.reg(1);
                        let object = *read_register(&(*top_frame!()), obj_reg)?;
                        let prototype = *read_register(&(*top_frame!()), proto_reg)?;
                        match self.object_protocol_value(
                            stack,
                            context,
                            crate::ObjectProtocolValueOp::SetPrototype,
                            object,
                            prototype,
                        ) {
                            Ok(value) => value,
                            Err(CommittedValueError::JavaScript(error)) => return Err(error),
                            Err(CommittedValueError::Fatal(error)) => {
                                return Ok(DispatchOutcome::Fatal(error));
                            }
                        };
                        top_frame!().advance_pc()?;
                        continue 'activation;
                    }
                    // §19.4.1 indirect eval — recursively dispatches a
                    // freshly compiled module on a sub-stack, then
                    // writes the completion value into `dst`. Stack-
                    // modifying so it has to run before the in-frame
                    // borrow below.
                    Op::Eval => {
                        let operands = function.operand_view(instr);
                        self.run_eval_operands(context, stack, operands)?;
                        continue 'activation;
                    }
                    Op::LoadArgumentsLength | Op::LoadArgumentsElement => {
                        let dst = instr.reg(0);
                        let key = (op == Op::LoadArgumentsElement)
                            .then(|| read_register(&(*top_frame!()), instr.reg(1)).copied())
                            .transpose()?;
                        let value = match self.read_frame_arguments(context, stack, top_idx, key) {
                            Ok(value) => value,
                            Err(CommittedValueError::JavaScript(error)) => return Err(error),
                            Err(CommittedValueError::Fatal(error)) => {
                                return Ok(DispatchOutcome::Fatal(error));
                            }
                        };
                        write_register(&mut (*top_frame!()), dst, value)?;
                        top_frame!().advance_pc()?;
                        continue 'activation;
                    }
                    Op::CollectArguments => {
                        let (dst, ctx) = (instr.reg(0), instr.reg(1));
                        self.run_collect_arguments_reg(context, stack, top_idx, dst, ctx)?;
                        continue 'activation;
                    }
                    Op::Nop => {
                        top_frame!().advance_pc()?;
                        continue 'dispatch;
                    }
                    Op::LoadUndefined => {
                        let dst = instr.reg(0);
                        let frame = &mut (*top_frame!());
                        write_register(frame, dst, Value::undefined())?;
                        frame.advance_pc()?;
                        continue 'dispatch;
                    }
                    Op::LoadHole => {
                        let dst = instr.reg(0);
                        let frame = &mut (*top_frame!());
                        write_register(frame, dst, Value::hole())?;
                        frame.advance_pc()?;
                        continue 'dispatch;
                    }
                    Op::LoadTrue => {
                        let dst = instr.reg(0);
                        let frame = &mut (*top_frame!());
                        write_register(frame, dst, Value::boolean(true))?;
                        frame.advance_pc()?;
                        continue 'dispatch;
                    }
                    Op::LoadFalse => {
                        let dst = instr.reg(0);
                        let frame = &mut (*top_frame!());
                        write_register(frame, dst, Value::boolean(false))?;
                        frame.advance_pc()?;
                        continue 'dispatch;
                    }
                    Op::LoadNull => {
                        let dst = instr.reg(0);
                        let frame = &mut (*top_frame!());
                        write_register(frame, dst, Value::null())?;
                        frame.advance_pc()?;
                        continue 'dispatch;
                    }
                    Op::LoadInt32 => {
                        let dst = instr.reg(0);
                        let imm = instr.imm(1);
                        let frame = &mut (*top_frame!());
                        write_register(frame, dst, Value::number(NumberValue::Smi(imm)))?;
                        frame.advance_pc()?;
                        continue 'dispatch;
                    }
                    Op::LoadNumber => {
                        let dst = instr.reg(0);
                        let idx = instr.const_word(1);
                        let bits = context
                            .number_constant_bits(idx)
                            .ok_or_else(|| VmError::InvalidOperand)?;
                        let value = NumberValue::from_f64(f64::from_bits(bits));
                        let frame = &mut (*top_frame!());
                        write_register(frame, dst, Value::number(value))?;
                        frame.advance_pc()?;
                        continue 'activation;
                    }
                    Op::LoadString => {
                        let dst = instr.reg(0);
                        let idx = instr.const_word(1);
                        let value = self.load_string_constant_value(context, idx)?;
                        let frame = &mut (*top_frame!());
                        write_register(frame, dst, value)?;
                        frame.advance_pc()?;
                        continue 'activation;
                    }
                    Op::LoadLength => {
                        let dst = instr.reg(0);
                        let src = instr.reg(1);
                        let source = *read_register(&(*top_frame!()), src)?;
                        let result = match self.scalar_value(
                            stack,
                            context,
                            crate::ScalarValueOp::LoadLength,
                            source,
                            Value::undefined(),
                            Value::undefined(),
                        ) {
                            Ok(value) => value,
                            Err(CommittedValueError::JavaScript(error)) => return Err(error),
                            Err(CommittedValueError::Fatal(error)) => {
                                return Ok(DispatchOutcome::Fatal(error));
                            }
                        };
                        let frame = &mut (*top_frame!());
                        write_register(frame, dst, result)?;
                        frame.advance_pc()?;
                        continue 'activation;
                    }
                    Op::LogicalNot => {
                        let dst = instr.reg(0);
                        let src = instr.reg(1);
                        let frame = &mut (*top_frame!());
                        let truthy = read_register(frame, src)?.to_boolean(&self.gc_heap);
                        write_register(frame, dst, Value::boolean(!truthy))?;
                        frame.advance_pc()?;
                        continue 'activation;
                    }
                    Op::ToBoolean => {
                        let dst = instr.reg(0);
                        let src = instr.reg(1);
                        let frame = &mut (*top_frame!());
                        let truthy = read_register(frame, src)?.to_boolean(&self.gc_heap);
                        write_register(frame, dst, Value::boolean(truthy))?;
                        frame.advance_pc()?;
                        continue 'activation;
                    }
                    Op::GetStringIndex => {
                        let (dst, recv, idx) = instr.reg3();
                        let frame = &mut (*top_frame!());
                        self.run_get_string_index_regs(frame, dst, recv, idx)?;
                        continue 'activation;
                    }
                    Op::TypeOf => {
                        let dst = instr.reg(0);
                        let src = instr.reg(1);
                        let source = *read_register(&(*top_frame!()), src)?;
                        let result = match self.scalar_value(
                            stack,
                            context,
                            crate::ScalarValueOp::TypeOf,
                            source,
                            Value::undefined(),
                            Value::undefined(),
                        ) {
                            Ok(value) => value,
                            Err(CommittedValueError::JavaScript(error)) => return Err(error),
                            Err(CommittedValueError::Fatal(error)) => {
                                return Ok(DispatchOutcome::Fatal(error));
                            }
                        };
                        let frame = &mut (*top_frame!());
                        write_register(frame, dst, result)?;
                        frame.advance_pc()?;
                        continue 'activation;
                    }
                    Op::TestTypeOf => {
                        let dst = instr.reg(0);
                        let src = instr.reg(1);
                        let test = function
                            .imm32(instr, 2)
                            .and_then(otter_bytecode::TypeOfTest::decode)
                            .ok_or(VmError::InvalidOperand)?;
                        let frame = &mut (*top_frame!());
                        let kind = read_register(frame, src)?.typeof_kind_with_heap(&self.gc_heap);
                        write_register(
                            frame,
                            dst,
                            Value::boolean((kind == test.kind) != test.negate),
                        )?;
                        frame.advance_pc()?;
                        continue 'activation;
                    }
                    Op::LoadThis => {
                        let dst = instr.reg(0);
                        let mut frame = ActiveFrameMut::from_frame(&mut (*top_frame!()));
                        self.frame_load_this(&mut frame, dst)?;
                        frame.advance_pc()?;
                        continue 'activation;
                    }
                    Op::LoadNewTarget => {
                        let dst = instr.reg(0);
                        let new_target = top_frame!().new_target();
                        let result = match self.scalar_value(
                            stack,
                            context,
                            crate::ScalarValueOp::LoadNewTarget,
                            Value::undefined(),
                            Value::undefined(),
                            new_target,
                        ) {
                            Ok(value) => value,
                            Err(CommittedValueError::JavaScript(error)) => return Err(error),
                            Err(CommittedValueError::Fatal(error)) => {
                                return Ok(DispatchOutcome::Fatal(error));
                            }
                        };
                        let frame = &mut (*top_frame!());
                        write_register(frame, dst, result)?;
                        frame.advance_pc()?;
                        continue 'activation;
                    }
                    Op::NewObject => {
                        let dst = instr.reg(0);
                        self.run_new_object_reg(&mut *stack, top_idx, dst)?;
                        continue 'activation;
                    }
                    Op::NewArray => {
                        let operands = function.operand_view(instr);
                        let placement =
                            self.literal_site_placement(function.allocation_site_at(idx));
                        self.run_new_array_operands(&mut *stack, top_idx, placement, operands)?;
                        continue 'activation;
                    }
                    Op::NewArrayBoilerplate => {
                        let dst = instr.reg(0);
                        let boilerplate = instr.const_word(1);
                        let placement =
                            self.literal_site_placement(function.allocation_site_at(idx));
                        let frame = &mut (*top_frame!());
                        self.run_new_array_boilerplate_reg(
                            context,
                            frame,
                            dst,
                            boilerplate,
                            placement,
                        )?;
                        continue 'activation;
                    }
                    Op::NewObjectLiteral => {
                        let operands = function.operand_view(instr);
                        let placement =
                            self.literal_site_placement(function.allocation_site_at(idx));
                        self.run_new_object_literal_operands(
                            &mut *stack,
                            top_idx,
                            context,
                            placement,
                            operands,
                        )?;
                        continue 'activation;
                    }
                    Op::LoadRegExp => {
                        let dst = instr.reg(0);
                        let idx = instr.const_word(1);
                        let frame = &mut (*top_frame!());
                        self.run_load_regexp_reg(context, frame, dst, idx)?;
                        continue 'activation;
                    }
                    Op::LoadBigInt => {
                        let dst = instr.reg(0);
                        let idx = instr.const_word(1);
                        let frame = &mut (*top_frame!());
                        self.run_load_bigint_reg(context, frame, dst, idx)?;
                        continue 'activation;
                    }
                    Op::LoadClosureContext => {
                        let dst = instr.reg(0);
                        let mut frame = ActiveFrameMut::from_frame(&mut (*top_frame!()));
                        self.frame_load_closure_context(&mut frame, dst)?;
                        frame.advance_pc()?;
                        continue 'activation;
                    }
                    Op::LoadSelf => {
                        let dst = instr.reg(0);
                        let mut frame = ActiveFrameMut::from_frame(&mut (*top_frame!()));
                        self.frame_load_self(&mut frame, dst)?;
                        frame.advance_pc()?;
                        continue 'activation;
                    }
                    Op::CreateContext => {
                        let (dst, parent) = (instr.reg(0), instr.reg(1));
                        let scope = instr.imm(2) as u32;
                        let mut frame = ActiveFrameMut::from_frame(&mut (*top_frame!()));
                        self.frame_create_context(context, &mut frame, dst, parent, scope)?;
                        frame.advance_pc()?;
                        continue 'activation;
                    }
                    Op::CopyContext => {
                        let (dst, src) = (instr.reg(0), instr.reg(1));
                        let mut frame = ActiveFrameMut::from_frame(&mut (*top_frame!()));
                        self.frame_copy_context(&mut frame, dst, src)?;
                        frame.advance_pc()?;
                        continue 'activation;
                    }
                    Op::LoadContextSlot | Op::LoadContextSlotChecked => {
                        let (dst, ctx) = (instr.reg(0), instr.reg(1));
                        let coord = instr.imm(2);
                        let frame = &mut (*top_frame!());
                        let value = self.load_context_slot_value(
                            context,
                            *read_register(frame, ctx)?,
                            coord,
                            op == Op::LoadContextSlotChecked,
                        )?;
                        write_register(frame, dst, value)?;
                        frame.advance_pc()?;
                        continue 'activation;
                    }
                    Op::StoreContextSlot | Op::StoreContextSlotChecked => {
                        let (src, ctx) = (instr.reg(0), instr.reg(1));
                        let coord = instr.imm(2);
                        let frame = &mut (*top_frame!());
                        let (value, target) =
                            (*read_register(frame, src)?, *read_register(frame, ctx)?);
                        self.store_context_slot_value(
                            context,
                            target,
                            coord,
                            value,
                            op == Op::StoreContextSlotChecked,
                        )?;
                        top_frame!().advance_pc()?;
                        continue 'activation;
                    }
                    Op::BindThisContextSlot => {
                        let (src, ctx) = (instr.reg(0), instr.reg(1));
                        let coord = instr.imm(2);
                        let frame = &mut (*top_frame!());
                        let (value, target) =
                            (*read_register(frame, src)?, *read_register(frame, ctx)?);
                        self.bind_this_context_slot_value(target, coord, value)?;
                        top_frame!().advance_pc()?;
                        continue 'activation;
                    }
                    Op::CollectRest => {
                        let dst = instr.reg(0);
                        self.collect_rest(&mut *stack, top_idx, function.param_count, dst)?;
                        top_frame!().advance_pc()?;
                        continue 'activation;
                    }
                    Op::MakeFunction => {
                        let dst = instr.reg(0);
                        let idx = instr.const_word(1);
                        let frame = &mut (*top_frame!());
                        self.run_make_function_reg(context, frame, dst, idx)?;
                        continue 'activation;
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
                        continue 'activation;
                    }
                    Op::NewError => {
                        let dst = instr.reg(0);
                        let kind_idx = instr.const_word(1);
                        let msg_reg = instr.reg(2);
                        (match self.run_new_error_regs(
                            context,
                            &mut *stack,
                            top_idx,
                            dst,
                            kind_idx,
                            msg_reg,
                        ) {
                            Ok(value) => value,
                            Err(CommittedValueError::JavaScript(error)) => return Err(error),
                            Err(CommittedValueError::Fatal(error)) => {
                                return Ok(DispatchOutcome::Fatal(error));
                            }
                        });
                        continue 'activation;
                    }
                    Op::LoadGlobalThis => {
                        let dst = instr.reg(0);
                        let frame = &mut (*top_frame!());
                        self.run_load_global_this_reg(frame, dst)?;
                        continue 'activation;
                    }
                    Op::LoadGlobalOrThrow => {
                        let dst = instr.reg(0);
                        let name_idx = instr.const_word(1);
                        (match self
                            .run_load_global_or_throw_reg(context, stack, top_idx, dst, name_idx)
                        {
                            Ok(value) => value,
                            Err(CommittedValueError::JavaScript(error)) => return Err(error),
                            Err(CommittedValueError::Fatal(error)) => {
                                return Ok(DispatchOutcome::Fatal(error));
                            }
                        });
                        continue 'activation;
                    }
                    Op::LoadGlobalOrUndefined => {
                        let dst = instr.reg(0);
                        let name_idx = instr.const_word(1);
                        (match self.run_load_global_or_undefined_reg(
                            context, stack, top_idx, dst, name_idx,
                        ) {
                            Ok(value) => value,
                            Err(CommittedValueError::JavaScript(error)) => return Err(error),
                            Err(CommittedValueError::Fatal(error)) => {
                                return Ok(DispatchOutcome::Fatal(error));
                            }
                        });
                        continue 'activation;
                    }
                    Op::DeclareGlobalVar => {
                        let name_idx = instr.const_word(0);
                        let configurable = function.imm32(instr, 1).unwrap_or(0) != 0;
                        let frame = &mut (*top_frame!());
                        self.run_declare_global_var_reg(context, frame, name_idx, configurable)?;
                        continue 'activation;
                    }
                    // §13.2.8.4 GetTemplateObject — realm-cached frozen
                    // template-strings object per tagged-template site.
                    Op::GetTemplateObject => {
                        let dst = instr.reg(0);
                        let site_idx = instr.const_word(1);
                        self.run_get_template_object_reg(context, stack, top_idx, dst, site_idx)?;
                        continue 'activation;
                    }
                    // §9.1.2.1 GetIdentifierReference through contexts a sloppy
                    // direct eval may have extended: extensions at hops
                    // `[0, depth)` first, then the static slot or the global
                    // Environment Record.
                    Op::LoadLookupSlot => {
                        let (dst, ctx) = (instr.reg(0), instr.reg(1));
                        let (name_idx, coord) = (instr.const_word(2), instr.imm(3));
                        let frame = &mut (*top_frame!());
                        let value = self.load_lookup_slot_value(
                            context,
                            function_id,
                            *read_register(frame, ctx)?,
                            name_idx,
                            coord,
                        )?;
                        write_register(frame, dst, value)?;
                        frame.advance_pc()?;
                        continue 'activation;
                    }
                    Op::StoreLookupSlot => {
                        let value_reg = function.reg(instr, 0);
                        let ctx = function.reg(instr, 1);
                        let name_idx = function
                            .const_index(instr, 2)
                            .ok_or(VmError::InvalidOperand)?;
                        let coord = function.imm32(instr, 3).ok_or(VmError::InvalidOperand)?;
                        let fallback = function.imm32(instr, 4).ok_or(VmError::InvalidOperand)?;
                        let frame = &(*top_frame!());
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
                        top_frame!().advance_pc()?;
                        continue 'activation;
                    }
                    Op::DeleteLookupSlot => {
                        let (dst, ctx) = (instr.reg(0), instr.reg(1));
                        let (name_idx, depth) = (instr.const_word(2), instr.imm(3));
                        let target = *read_register(&(*top_frame!()), ctx)?;
                        let value = self.delete_lookup_slot_value(
                            context,
                            function_id,
                            target,
                            name_idx,
                            depth,
                        )?;
                        let frame = &mut (*top_frame!());
                        write_register(frame, dst, value)?;
                        frame.advance_pc()?;
                        continue 'activation;
                    }
                    Op::LoadLookupGlobal | Op::TypeofLookupGlobal => {
                        let (dst, ctx) = (instr.reg(0), instr.reg(1));
                        let (name_idx, depth) = (instr.const_word(2), instr.imm(3));
                        let target = *read_register(&(*top_frame!()), ctx)?;
                        let missing = if op == Op::LoadLookupGlobal {
                            otter_bytecode::opcode_schema::BindingMissing::Throw
                        } else {
                            otter_bytecode::opcode_schema::BindingMissing::Undefined
                        };
                        let value = match self.load_lookup_global_value(
                            context,
                            stack,
                            function_id,
                            target,
                            name_idx,
                            depth,
                            missing,
                        ) {
                            Ok(value) => value,
                            Err(CommittedValueError::JavaScript(error)) => return Err(error),
                            Err(CommittedValueError::Fatal(error)) => {
                                return Ok(DispatchOutcome::Fatal(error));
                            }
                        };
                        let frame = &mut (*top_frame!());
                        write_register(frame, dst, value)?;
                        frame.advance_pc()?;
                        continue 'activation;
                    }
                    Op::StoreLookupGlobal => {
                        let (value_reg, ctx) = (instr.reg(0), instr.reg(1));
                        let (name_idx, mode) = (instr.const_word(2), instr.imm(3));
                        let frame = &(*top_frame!());
                        let (value, target) = (
                            *read_register(frame, value_reg)?,
                            *read_register(frame, ctx)?,
                        );
                        (match self.store_lookup_global_value(
                            context,
                            stack,
                            function_id,
                            target,
                            name_idx,
                            mode,
                            value,
                        ) {
                            Ok(value) => value,
                            Err(CommittedValueError::JavaScript(error)) => return Err(error),
                            Err(CommittedValueError::Fatal(error)) => {
                                return Ok(DispatchOutcome::Fatal(error));
                            }
                        });
                        top_frame!().advance_pc()?;
                        continue 'activation;
                    }
                    Op::DeleteLookupGlobal => {
                        let (dst, ctx) = (instr.reg(0), instr.reg(1));
                        let (name_idx, depth) = (instr.const_word(2), instr.imm(3));
                        let target = *read_register(&(*top_frame!()), ctx)?;
                        let value = self.delete_lookup_global_value(
                            context,
                            function_id,
                            target,
                            name_idx,
                            depth,
                        )?;
                        let frame = &mut (*top_frame!());
                        write_register(frame, dst, value)?;
                        frame.advance_pc()?;
                        continue 'activation;
                    }
                    // §13.15.2 — the assignment target's reference base resolves
                    // before the right-hand side; `StoreRef` writes through it.
                    Op::ResolveLookupRef => {
                        let (dst, ctx) = (instr.reg(0), instr.reg(1));
                        let (name_idx, target_imm) = (instr.const_word(2), instr.imm(3));
                        let frame = &mut (*top_frame!());
                        let reference = self.resolve_lookup_ref_value(
                            context,
                            function_id,
                            *read_register(frame, ctx)?,
                            name_idx,
                            target_imm,
                        )?;
                        write_register(frame, dst, reference)?;
                        frame.advance_pc()?;
                        continue 'activation;
                    }
                    Op::StoreRef => {
                        let (value_reg, reference_reg) = (instr.reg(0), instr.reg(1));
                        let (name_idx, mode) = (instr.const_word(2), instr.imm(3));
                        let frame = &(*top_frame!());
                        let (value, reference) = (
                            *read_register(frame, value_reg)?,
                            *read_register(frame, reference_reg)?,
                        );
                        (match self.store_ref_value(
                            context,
                            stack,
                            function_id,
                            reference,
                            name_idx,
                            mode,
                            value,
                        ) {
                            Ok(value) => value,
                            Err(CommittedValueError::JavaScript(error)) => return Err(error),
                            Err(CommittedValueError::Fatal(error)) => {
                                return Ok(DispatchOutcome::Fatal(error));
                            }
                        });
                        top_frame!().advance_pc()?;
                        continue 'activation;
                    }
                    // §19.2.1.3 step 16 — a sloppy eval's `var` with no static
                    // slot becomes a deletable binding of the var scope.
                    Op::DeclareEvalVar => {
                        let ctx = instr.reg(0);
                        let (name_idx, var_depth) = (instr.const_word(1), instr.imm(2));
                        let target = *read_register(&(*top_frame!()), ctx)?;
                        self.declare_eval_var_value(
                            context,
                            function_id,
                            target,
                            name_idx,
                            var_depth,
                        )?;
                        top_frame!().advance_pc()?;
                        continue 'activation;
                    }
                    Op::StoreVarScope => {
                        let (value_reg, ctx) = (instr.reg(0), instr.reg(1));
                        let (name_idx, var_depth) = (instr.const_word(2), instr.imm(3));
                        let frame = &(*top_frame!());
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
                        top_frame!().advance_pc()?;
                        continue 'activation;
                    }
                    // §6.2.12 — mint a Private Name carrier; the marker
                    // keeps it out of Proxy traps and arms the §7.3.28
                    // extensibility check on adds.
                    Op::NewPrivateName => {
                        let dst = instr.reg(0);
                        let desc_idx = instr.const_word(1);
                        self.run_new_private_name_reg(context, stack, top_idx, dst, desc_idx)?;
                        continue 'activation;
                    }
                    Op::DefineGlobalFunction => {
                        let name_idx = instr.const_word(0);
                        let value_reg = instr.reg(1);
                        let deletable = function.imm32(instr, 2).unwrap_or(0) != 0;
                        let frame = &mut (*top_frame!());
                        self.run_define_global_function_reg(
                            context, frame, name_idx, value_reg, deletable,
                        )?;
                        continue 'activation;
                    }
                    Op::DeclareGlobalLex => {
                        let name_idx = instr.const_word(0);
                        let is_const = function.imm32(instr, 1).unwrap_or(0) != 0;
                        let frame = &mut (*top_frame!());
                        self.run_declare_global_lex_reg(context, frame, name_idx, is_const)?;
                        continue 'activation;
                    }
                    Op::StoreGlobalBinding => {
                        let value_reg = instr.reg(0);
                        let name_idx = instr.const_word(1);
                        let strict = function.imm32(instr, 2).unwrap_or(0) != 0;
                        (match self.run_store_global_binding_reg(
                            context, stack, top_idx, value_reg, name_idx, strict,
                        ) {
                            Ok(value) => value,
                            Err(CommittedValueError::JavaScript(error)) => return Err(error),
                            Err(CommittedValueError::Fatal(error)) => {
                                return Ok(DispatchOutcome::Fatal(error));
                            }
                        });
                        continue 'activation;
                    }
                    Op::InitGlobalLex => {
                        let value_reg = instr.reg(0);
                        let name_idx = instr.const_word(1);
                        let frame = &mut (*top_frame!());
                        self.run_init_global_lex_reg(context, frame, value_reg, name_idx)?;
                        continue 'activation;
                    }
                    // §15.7.14 class-definition validation: heritage
                    // IsConstructor / static computed key != "prototype".
                    Op::ClassCheck => {
                        let kind = function.imm32(instr, 0).unwrap_or(0);
                        let reg = instr.reg(1);
                        self.run_class_check_reg(context, stack, top_idx, kind as u32, reg)?;
                        continue 'activation;
                    }
                    // §7.3.7 CreateDataPropertyOrThrow — object literal
                    // property definition; never consults inherited
                    // setters (unlike StoreProperty's Set semantics).
                    Op::DefineDataProperty => {
                        let (obj_reg, key_reg, value_reg) = instr.reg3();
                        (match self.run_define_data_property_regs(
                            context, stack, top_idx, obj_reg, key_reg, value_reg,
                        ) {
                            Ok(value) => value,
                            Err(CommittedValueError::JavaScript(error)) => return Err(error),
                            Err(CommittedValueError::Fatal(error)) => {
                                return Ok(DispatchOutcome::Fatal(error));
                            }
                        });
                        top_frame!().advance_pc()?;
                        continue 'activation;
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
                        continue 'activation;
                    }
                    // §7.3.31 PrivateGet — brand check (absent name
                    // throws), accessor-without-getter throws, accessor
                    // invokes its getter with the receiver as `this`.
                    Op::PrivateGet => {
                        let (dst, obj_reg, key_reg) = instr.reg3();
                        (match self
                            .run_private_get_reg(context, stack, top_idx, dst, obj_reg, key_reg)
                        {
                            Ok(value) => value,
                            Err(CommittedValueError::JavaScript(error)) => return Err(error),
                            Err(CommittedValueError::Fatal(error)) => {
                                return Ok(DispatchOutcome::Fatal(error));
                            }
                        });
                        continue 'activation;
                    }
                    // §7.3.32 PrivateSet — brand check, private methods
                    // are not writable, accessor-without-setter throws,
                    // an own field writes in place preserving attributes.
                    Op::PrivateSet => {
                        let (obj_reg, key_reg, value_reg) = instr.reg3();
                        (match self.run_private_set_reg(
                            context, stack, top_idx, obj_reg, key_reg, value_reg,
                        ) {
                            Ok(value) => value,
                            Err(CommittedValueError::JavaScript(error)) => return Err(error),
                            Err(CommittedValueError::Fatal(error)) => {
                                return Ok(DispatchOutcome::Fatal(error));
                            }
                        });
                        continue 'activation;
                    }
                    // §7.1.3 ToNumeric: Number / BigInt pass through, an
                    // object runs ToPrimitive(number) first, Symbol throws,
                    // the rest convert via ToNumber. A postfix update keeps
                    // this as its result value.
                    Op::ToNumeric => {
                        let dst = instr.reg(0);
                        let src = instr.reg(1);
                        let value = *read_register(&(*top_frame!()), src)?;
                        let result = if value.is_number() {
                            value
                        } else {
                            match crate::coerce::to_numeric_or_throw(self, stack, context, &value) {
                                Ok(value) => value,
                                Err(CommittedValueError::JavaScript(error)) => return Err(error),
                                Err(CommittedValueError::Fatal(error)) => {
                                    return Ok(DispatchOutcome::Fatal(error));
                                }
                            }
                        };
                        let frame = &mut (*top_frame!());
                        write_register(frame, dst, result)?;
                        frame.advance_pc()?;
                        continue 'activation;
                    }
                    // §7.1.18 ToObject — wrap a primitive in its
                    // `%X.prototype%` body; objects pass through;
                    // `null` / `undefined` throw a TypeError. Emitted by
                    // the `with` statement lowering (§14.11.2 step 2).
                    Op::ToObject => {
                        let dst = instr.reg(0);
                        let src = instr.reg(1);
                        let source = *read_register(&(*top_frame!()), src)?;
                        let result = match self.scalar_value(
                            stack,
                            context,
                            crate::ScalarValueOp::ToObject,
                            source,
                            Value::undefined(),
                            Value::undefined(),
                        ) {
                            Ok(value) => value,
                            Err(CommittedValueError::JavaScript(error)) => return Err(error),
                            Err(CommittedValueError::Fatal(error)) => {
                                return Ok(DispatchOutcome::Fatal(error));
                            }
                        };
                        let frame = &mut (*top_frame!());
                        write_register(frame, dst, result)?;
                        frame.advance_pc()?;
                        continue 'activation;
                    }
                    // §7.1.19 ToPropertyKey with full user coercion —
                    // class field definitions canonicalize their
                    // computed names at class-definition time.
                    Op::ToPropertyKey => {
                        let dst = instr.reg(0);
                        let src = instr.reg(1);
                        let source = *read_register(&(*top_frame!()), src)?;
                        let result = match self.scalar_value(
                            stack,
                            context,
                            crate::ScalarValueOp::ToPropertyKey,
                            source,
                            Value::undefined(),
                            Value::undefined(),
                        ) {
                            Ok(value) => value,
                            Err(CommittedValueError::JavaScript(error)) => return Err(error),
                            Err(CommittedValueError::Fatal(error)) => {
                                return Ok(DispatchOutcome::Fatal(error));
                            }
                        };
                        let frame = &mut (*top_frame!());
                        write_register(frame, dst, result)?;
                        frame.advance_pc()?;
                        continue 'activation;
                    }
                    // §7.3.31 PrivateElementFind own-only — private
                    // methods / accessors require the class brand marker
                    // as an OWN property of the receiver (installed after
                    // super() returns); the prototype-side method lookup
                    // alone must not satisfy access before that.
                    Op::PrivateBrandCheck => {
                        let obj_reg = instr.reg(0);
                        let brand_reg = instr.reg(1);
                        (match self.run_private_brand_check_reg(
                            context, stack, top_idx, obj_reg, brand_reg,
                        ) {
                            Ok(value) => value,
                            Err(CommittedValueError::JavaScript(error)) => return Err(error),
                            Err(CommittedValueError::Fatal(error)) => {
                                return Ok(DispatchOutcome::Fatal(error));
                            }
                        });
                        continue 'activation;
                    }
                    // §13.4.2 UpdateExpression numeric step — ToNumeric
                    // then ±1, preserving the BigInt type (§6.1.6.2.7).
                    Op::Increment => {
                        let dst = instr.reg(0);
                        let src = instr.reg(1);
                        let delta = function.imm32(instr, 2).unwrap_or(1);
                        let value = register!(src);
                        if let Some(result) = crate::arithmetic_dispatch::step_number(value, delta)
                        {
                            if let Some(feedback) = feedback {
                                feedback.record_arith(value, Value::number_i32(delta));
                                feedback.record_arith(result, result);
                            }
                            let frame = top_frame!();
                            // SAFETY: `dst` is a schema register.
                            unsafe { write_register_unchecked(frame, dst, result) };
                            frame.advance_pc_fast();
                            continue 'dispatch;
                        }
                        (match self
                            .run_increment_regs(stack, context, top_ptr, dst, src, delta, feedback)
                        {
                            Ok(value) => value,
                            Err(CommittedValueError::JavaScript(error)) => return Err(error),
                            Err(CommittedValueError::Fatal(error)) => {
                                return Ok(DispatchOutcome::Fatal(error));
                            }
                        });
                        continue 'activation;
                    }
                    Op::ValidateGlobalDecl => {
                        let name_idx = instr.const_word(0);
                        let kind = function.imm32(instr, 1).unwrap_or(0);
                        let frame = &mut (*top_frame!());
                        self.run_validate_global_decl_reg(context, frame, name_idx, kind)?;
                        continue 'activation;
                    }
                    Op::DefineGlobalVar => {
                        let name_idx = instr.const_word(0);
                        let value_reg = instr.reg(1);
                        (match self
                            .run_define_global_var_reg(context, stack, top_idx, name_idx, value_reg)
                        {
                            Ok(value) => value,
                            Err(CommittedValueError::JavaScript(error)) => return Err(error),
                            Err(CommittedValueError::Fatal(error)) => {
                                return Ok(DispatchOutcome::Fatal(error));
                            }
                        });
                        continue 'activation;
                    }
                    Op::ImportNamespace => {
                        let dst = instr.reg(0);
                        let spec_idx = instr.const_word(1);
                        self.run_import_namespace_reg(context, stack, top_idx, dst, spec_idx)?;
                        continue 'activation;
                    }
                    Op::ImportNamespaceDeferred => {
                        let dst = instr.reg(0);
                        let spec_idx = instr.const_word(1);
                        self.run_import_namespace_deferred_reg(
                            context, stack, top_idx, dst, spec_idx,
                        )?;
                        continue 'activation;
                    }
                    Op::ModuleNamespaceObject => {
                        let dst = instr.reg(0);
                        let spec_idx = instr.const_word(1);
                        self.run_module_namespace_object_reg(
                            context, stack, top_idx, dst, spec_idx,
                        )?;
                        continue 'activation;
                    }
                    Op::LoadImportBinding => {
                        let dst = instr.reg(0);
                        let url_idx = instr.const_word(1);
                        let name_idx = instr.const_word(2);
                        self.run_load_import_binding_reg(
                            context, stack, top_idx, dst, url_idx, name_idx,
                        )?;
                        continue 'activation;
                    }
                    Op::EvaluateModule => {
                        let dst = instr.reg(0);
                        let url_idx = instr.const_word(1);
                        self.run_evaluate_module_const(context, stack, top_idx, dst, url_idx)?;
                        continue 'activation;
                    }
                    Op::MarkModuleEvaluated => {
                        let url_idx = instr.const_word(0);
                        self.run_mark_module_evaluated_const(context, stack, top_idx, url_idx)?;
                        continue 'activation;
                    }
                    Op::ImportMetaResolve => {
                        let dst = instr.reg(0);
                        let spec_reg = instr.reg(1);
                        self.run_import_meta_resolve_regs(context, stack, top_idx, dst, spec_reg)?;
                        continue 'activation;
                    }
                    Op::PromiseFulfilledOf => {
                        let dst = instr.reg(0);
                        let src = instr.reg(1);
                        self.run_promise_fulfilled_of_regs(context, stack, top_idx, dst, src)?;
                        continue 'activation;
                    }
                    Op::ArrayPush => {
                        let arr_reg = instr.reg(0);
                        let value_reg = instr.reg(1);
                        self.run_array_push_regs(&mut *stack, top_idx, arr_reg, value_reg)?;
                        continue 'activation;
                    }
                    Op::SpreadAppend => {
                        let (array_reg, iterable_reg) = (instr.reg(0), instr.reg(1));
                        match self.spread_append(context, stack, top_idx, array_reg, iterable_reg) {
                            Ok(()) => {}
                            Err(CommittedValueError::JavaScript(error)) => return Err(error),
                            Err(CommittedValueError::Fatal(error)) => {
                                return Ok(DispatchOutcome::Fatal(error));
                            }
                        }
                        top_frame!().advance_pc()?;
                        continue 'activation;
                    }
                    Op::NewWeakRef => {
                        let dst = instr.reg(0);
                        let target_reg = instr.reg(1);
                        self.run_new_weak_ref_regs(&mut *stack, top_idx, dst, target_reg)?;
                        continue 'activation;
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
                        continue 'activation;
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
                        continue 'activation;
                    }
                    Op::SymbolLoad => {
                        let dst = instr.reg(0);
                        let name_idx = instr.const_word(1);
                        let frame = &mut (*top_frame!());
                        self.run_symbol_load_reg(context, frame, dst, name_idx)?;
                        continue 'activation;
                    }
                    Op::TemporalLoad => {
                        let dst = instr.reg(0);
                        let name_idx = instr.const_word(1);
                        let frame = &mut (*top_frame!());
                        self.run_temporal_load_reg(context, frame, dst, name_idx)?;
                        continue 'activation;
                    }
                    Op::GlobalBindingExists => {
                        let dst = instr.reg(0);
                        let name_idx = const_operand(function.operand(instr, 1))?;
                        (match self
                            .run_global_binding_exists_reg(context, stack, top_idx, dst, name_idx)
                        {
                            Ok(value) => value,
                            Err(CommittedValueError::JavaScript(error)) => return Err(error),
                            Err(CommittedValueError::Fatal(error)) => {
                                return Ok(DispatchOutcome::Fatal(error));
                            }
                        });
                        continue 'activation;
                    }
                    Op::StoreGlobalChecked => {
                        let value_reg = instr.reg(0);
                        let name_idx = const_operand(function.operand(instr, 1))?;
                        let exists_reg = instr.reg(2);
                        (match self.run_store_global_checked_reg(
                            context, stack, top_idx, value_reg, name_idx, exists_reg,
                        ) {
                            Ok(value) => value,
                            Err(CommittedValueError::JavaScript(error)) => return Err(error),
                            Err(CommittedValueError::Fatal(error)) => {
                                return Ok(DispatchOutcome::Fatal(error));
                            }
                        });
                        continue 'activation;
                    }
                    Op::Jump => {
                        let offset = instr.imm(0);
                        // `top_idx` is `stack.len() - 1` this tick and no frame was
                        // pushed, so the top frame is the executing one.
                        // SAFETY: the `is_at_floor` guard proved a live top frame.
                        apply_branch(top_frame!(), offset, &self.interrupt)?;
                        if jit_installed
                            && offset < 0
                            && let Some(entry) = {
                                source_work.publish();
                                self.note_backedge_and_maybe_osr(stack, context, top_idx)?
                            }
                        {
                            return Ok(DispatchOutcome::Tier(entry));
                        }
                        continue 'dispatch;
                    }
                    Op::JumpIfTrue => {
                        let offset = instr.imm(0);
                        let cond = instr.reg(1);
                        // SAFETY: live top frame (see the `is_at_floor` guard); it is
                        // the executing frame this tick.
                        let frame = top_frame!();
                        // `cond` is a schema register; SAFETY: see `read_register_unchecked`.
                        let taken = unsafe { read_register_unchecked(frame, cond) }
                            .to_boolean(&self.gc_heap);
                        if jit_installed && let Some(cell) = feedback {
                            cell.record_branch(taken);
                        }
                        if taken {
                            apply_branch(frame, offset, &self.interrupt)?;
                            if jit_installed
                                && offset < 0
                                && let Some(entry) = {
                                    source_work.publish();
                                    self.note_backedge_and_maybe_osr(stack, context, top_idx)?
                                }
                            {
                                return Ok(DispatchOutcome::Tier(entry));
                            }
                        } else {
                            frame.advance_pc_fast();
                        }
                        continue 'dispatch;
                    }
                    Op::JumpIfFalse => {
                        let offset = instr.imm(0);
                        let cond = instr.reg(1);
                        // SAFETY: live top frame (see the `is_at_floor` guard); it is
                        // the executing frame this tick.
                        let frame = top_frame!();
                        // `cond` is a schema register; SAFETY: see `read_register_unchecked`.
                        let taken = !unsafe { read_register_unchecked(frame, cond) }
                            .to_boolean(&self.gc_heap);
                        if jit_installed && let Some(cell) = feedback {
                            cell.record_branch(taken);
                        }
                        if taken {
                            apply_branch(frame, offset, &self.interrupt)?;
                            if jit_installed
                                && offset < 0
                                && let Some(entry) = {
                                    source_work.publish();
                                    self.note_backedge_and_maybe_osr(stack, context, top_idx)?
                                }
                            {
                                return Ok(DispatchOutcome::Tier(entry));
                            }
                        } else {
                            frame.advance_pc_fast();
                        }
                        continue 'dispatch;
                    }
                    Op::JumpIfNullish => {
                        let offset = instr.imm(0);
                        let cond = instr.reg(1);
                        // SAFETY: live top frame (see the `is_at_floor` guard); it is
                        // the executing frame this tick.
                        let frame = top_frame!();
                        // SAFETY: see `read_register_unchecked`.
                        if unsafe { read_register_unchecked(frame, cond) }.is_nullish() {
                            apply_branch(frame, offset, &self.interrupt)?;
                        } else {
                            frame.advance_pc_fast();
                        }
                        continue 'dispatch;
                    }
                    Op::LoadLocal => {
                        let dst = instr.reg(0);
                        let idx = instr.imm(1) as u16;
                        // SAFETY: live top frame (see the `is_at_floor` guard); it is
                        // the executing frame this tick.
                        let frame = top_frame!();
                        // Both operands are schema registers (the `Imm32` is a local
                        // index), bounded by the build-time verifier.
                        // SAFETY: see `read_register_unchecked`.
                        unsafe {
                            let value = read_register_unchecked(frame, idx);
                            write_register_unchecked(frame, dst, value);
                        }
                        frame.advance_pc_fast();
                        continue 'dispatch;
                    }
                    Op::StoreLocal => {
                        let src = instr.reg(0);
                        let idx = instr.imm(1) as u16;
                        // SAFETY: live top frame (see the `is_at_floor` guard); it is
                        // the executing frame this tick.
                        let frame = top_frame!();
                        // SAFETY: see `Op::LoadLocal`.
                        unsafe {
                            let value = read_register_unchecked(frame, src);
                            write_register_unchecked(frame, idx, value);
                        }
                        frame.advance_pc_fast();
                        continue 'dispatch;
                    }
                    Op::TdzError => {
                        let local_index = instr.imm(0) as u32;
                        return Err(VmError::TemporalDeadZone { local_index });
                    }
                    Op::Add => {
                        let (dst, lhs, rhs) = instr.reg3();
                        commit_number_fold!('dispatch, feedback, crate::arithmetic_dispatch::fold_numbers, op, dst, register!(lhs), register!(rhs), true);
                        (match self.run_add_regs(stack, context, top_ptr, dst, lhs, rhs, feedback) {
                            Ok(value) => value,
                            Err(CommittedValueError::JavaScript(error)) => return Err(error),
                            Err(CommittedValueError::Fatal(error)) => {
                                return Ok(DispatchOutcome::Fatal(error));
                            }
                        });
                        continue 'activation;
                    }
                    Op::AddImm => {
                        let (dst, lhs) = (instr.reg(0), instr.reg(1));
                        let imm = instr.imm(2);
                        commit_number_fold!('dispatch, feedback, crate::arithmetic_dispatch::fold_numbers, Op::Add, dst, register!(lhs), Value::number_i32(imm), false);
                        (match self.run_add_imm(stack, context, top_ptr, dst, lhs, imm, feedback) {
                            Ok(value) => value,
                            Err(CommittedValueError::JavaScript(error)) => return Err(error),
                            Err(CommittedValueError::Fatal(error)) => {
                                return Ok(DispatchOutcome::Fatal(error));
                            }
                        });
                        continue 'activation;
                    }
                    Op::SubImm => {
                        let (dst, lhs) = (instr.reg(0), instr.reg(1));
                        let imm = instr.imm(2);
                        commit_number_fold!('dispatch, feedback, crate::arithmetic_dispatch::fold_numbers, Op::Sub, dst, register!(lhs), Value::number_i32(imm), false);
                        (match self.run_numeric_imm(
                            stack,
                            context,
                            top_ptr,
                            dst,
                            lhs,
                            imm,
                            number::sub,
                            bigint::ops::sub,
                            feedback,
                        ) {
                            Ok(value) => value,
                            Err(CommittedValueError::JavaScript(error)) => return Err(error),
                            Err(CommittedValueError::Fatal(error)) => {
                                return Ok(DispatchOutcome::Fatal(error));
                            }
                        });
                        continue 'activation;
                    }
                    Op::BitwiseAndImm => {
                        let (dst, lhs) = (instr.reg(0), instr.reg(1));
                        let imm = instr.imm(2);
                        commit_number_fold!('dispatch, feedback, crate::arithmetic_dispatch::fold_numbers, Op::BitwiseAnd, dst, register!(lhs), Value::number_i32(imm), false);
                        (match self.run_numeric_imm(
                            stack,
                            context,
                            top_ptr,
                            dst,
                            lhs,
                            imm,
                            number::bitwise_and,
                            bigint::ops::bitwise_and,
                            feedback,
                        ) {
                            Ok(value) => value,
                            Err(CommittedValueError::JavaScript(error)) => return Err(error),
                            Err(CommittedValueError::Fatal(error)) => {
                                return Ok(DispatchOutcome::Fatal(error));
                            }
                        });
                        continue 'activation;
                    }
                    Op::LessThanImm => {
                        let (dst, lhs) = (instr.reg(0), instr.reg(1));
                        let imm = instr.imm(2);
                        commit_number_fold!('dispatch, feedback, crate::arithmetic_dispatch::compare_numbers, Op::LessThan, dst, register!(lhs), Value::number_i32(imm), false);
                        (match self
                            .run_less_than_imm(stack, context, top_ptr, dst, lhs, imm, feedback)
                        {
                            Ok(value) => value,
                            Err(CommittedValueError::JavaScript(error)) => return Err(error),
                            Err(CommittedValueError::Fatal(error)) => {
                                return Ok(DispatchOutcome::Fatal(error));
                            }
                        });
                        continue 'activation;
                    }
                    Op::EqualImm => {
                        let (dst, lhs) = (instr.reg(0), instr.reg(1));
                        let imm = instr.imm(2);
                        // SAFETY: live top frame (see the `is_at_floor` guard).
                        self.run_equal_imm(top_frame!(), dst, lhs, imm, false, feedback)?;
                        continue 'activation;
                    }
                    Op::NotEqualImm => {
                        let (dst, lhs) = (instr.reg(0), instr.reg(1));
                        let imm = instr.imm(2);
                        // SAFETY: live top frame (see the `is_at_floor` guard).
                        self.run_equal_imm(top_frame!(), dst, lhs, imm, true, feedback)?;
                        continue 'activation;
                    }
                    Op::Sub | Op::Mul | Op::Div | Op::Rem | Op::Pow => {
                        let (dst, lhs, rhs) = instr.reg3();
                        commit_number_fold!('dispatch, feedback, crate::arithmetic_dispatch::fold_numbers, op, dst, register!(lhs), register!(rhs), true);
                        let operation =
                            otter_bytecode::scalar_semantics::numeric_binary_semantics(op)
                                .expect("numeric dispatch group has shared semantics")
                                .operation;
                        (match self.run_numeric_binary_regs(
                            stack,
                            context,
                            top_ptr,
                            [dst, lhs, rhs],
                            operation,
                            feedback,
                        ) {
                            Ok(value) => value,
                            Err(CommittedValueError::JavaScript(error)) => return Err(error),
                            Err(CommittedValueError::Fatal(error)) => {
                                return Ok(DispatchOutcome::Fatal(error));
                            }
                        });
                        continue 'activation;
                    }
                    Op::BitwiseAnd => {
                        let (dst, lhs, rhs) = instr.reg3();
                        commit_number_fold!('dispatch, feedback, crate::arithmetic_dispatch::fold_numbers, op, dst, register!(lhs), register!(rhs), true);
                        (match self.run_numeric_regs(
                            stack,
                            context,
                            top_ptr,
                            dst,
                            lhs,
                            rhs,
                            number::bitwise_and,
                            bigint::ops::bitwise_and,
                            feedback,
                        ) {
                            Ok(value) => value,
                            Err(CommittedValueError::JavaScript(error)) => return Err(error),
                            Err(CommittedValueError::Fatal(error)) => {
                                return Ok(DispatchOutcome::Fatal(error));
                            }
                        });
                        continue 'activation;
                    }
                    Op::BitwiseOr => {
                        let (dst, lhs, rhs) = instr.reg3();
                        commit_number_fold!('dispatch, feedback, crate::arithmetic_dispatch::fold_numbers, op, dst, register!(lhs), register!(rhs), true);
                        (match self.run_numeric_regs(
                            stack,
                            context,
                            top_ptr,
                            dst,
                            lhs,
                            rhs,
                            number::bitwise_or,
                            bigint::ops::bitwise_or,
                            feedback,
                        ) {
                            Ok(value) => value,
                            Err(CommittedValueError::JavaScript(error)) => return Err(error),
                            Err(CommittedValueError::Fatal(error)) => {
                                return Ok(DispatchOutcome::Fatal(error));
                            }
                        });
                        continue 'activation;
                    }
                    Op::BitwiseXor => {
                        let (dst, lhs, rhs) = instr.reg3();
                        commit_number_fold!('dispatch, feedback, crate::arithmetic_dispatch::fold_numbers, op, dst, register!(lhs), register!(rhs), true);
                        (match self.run_numeric_regs(
                            stack,
                            context,
                            top_ptr,
                            dst,
                            lhs,
                            rhs,
                            number::bitwise_xor,
                            bigint::ops::bitwise_xor,
                            feedback,
                        ) {
                            Ok(value) => value,
                            Err(CommittedValueError::JavaScript(error)) => return Err(error),
                            Err(CommittedValueError::Fatal(error)) => {
                                return Ok(DispatchOutcome::Fatal(error));
                            }
                        });
                        continue 'activation;
                    }
                    Op::Shl => {
                        let (dst, lhs, rhs) = instr.reg3();
                        commit_number_fold!('dispatch, feedback, crate::arithmetic_dispatch::fold_numbers, op, dst, register!(lhs), register!(rhs), true);
                        (match self.run_numeric_regs(
                            stack,
                            context,
                            top_ptr,
                            dst,
                            lhs,
                            rhs,
                            number::shl,
                            bigint::ops::shl,
                            feedback,
                        ) {
                            Ok(value) => value,
                            Err(CommittedValueError::JavaScript(error)) => return Err(error),
                            Err(CommittedValueError::Fatal(error)) => {
                                return Ok(DispatchOutcome::Fatal(error));
                            }
                        });
                        continue 'activation;
                    }
                    Op::Shr => {
                        let (dst, lhs, rhs) = instr.reg3();
                        commit_number_fold!('dispatch, feedback, crate::arithmetic_dispatch::fold_numbers, op, dst, register!(lhs), register!(rhs), true);
                        (match self.run_numeric_regs(
                            stack,
                            context,
                            top_ptr,
                            dst,
                            lhs,
                            rhs,
                            number::shr_arith,
                            bigint::ops::shr,
                            feedback,
                        ) {
                            Ok(value) => value,
                            Err(CommittedValueError::JavaScript(error)) => return Err(error),
                            Err(CommittedValueError::Fatal(error)) => {
                                return Ok(DispatchOutcome::Fatal(error));
                            }
                        });
                        continue 'activation;
                    }
                    Op::LessThan | Op::LessEq | Op::GreaterThan | Op::GreaterEq => {
                        let (dst, lhs, rhs) = instr.reg3();
                        commit_number_fold!('dispatch, feedback, crate::arithmetic_dispatch::compare_numbers, op, dst, register!(lhs), register!(rhs), false);
                        (match self
                            .run_compare_regs(stack, context, top_ptr, dst, lhs, rhs, op, feedback)
                        {
                            Ok(value) => value,
                            Err(CommittedValueError::JavaScript(error)) => return Err(error),
                            Err(CommittedValueError::Fatal(error)) => {
                                return Ok(DispatchOutcome::Fatal(error));
                            }
                        });
                        continue 'activation;
                    }
                    Op::Ushr => {
                        let (dst, lhs, rhs) = instr.reg3();
                        (match self.run_ushr_regs(stack, context, top_ptr, dst, lhs, rhs, feedback)
                        {
                            Ok(value) => value,
                            Err(CommittedValueError::JavaScript(error)) => return Err(error),
                            Err(CommittedValueError::Fatal(error)) => {
                                return Ok(DispatchOutcome::Fatal(error));
                            }
                        });
                        continue 'activation;
                    }
                    Op::Neg => {
                        let dst = instr.reg(0);
                        let src = instr.reg(1);
                        (match self.run_neg_regs(stack, context, top_ptr, dst, src, feedback) {
                            Ok(value) => value,
                            Err(CommittedValueError::JavaScript(error)) => return Err(error),
                            Err(CommittedValueError::Fatal(error)) => {
                                return Ok(DispatchOutcome::Fatal(error));
                            }
                        });
                        continue 'activation;
                    }
                    Op::BitwiseNot => {
                        let dst = instr.reg(0);
                        let src = instr.reg(1);
                        (match self.run_bitwise_not_regs(stack, context, top_ptr, dst, src) {
                            Ok(value) => value,
                            Err(CommittedValueError::JavaScript(error)) => return Err(error),
                            Err(CommittedValueError::Fatal(error)) => {
                                return Ok(DispatchOutcome::Fatal(error));
                            }
                        });
                        continue 'activation;
                    }
                    Op::Equal
                    | Op::NotEqual
                    | Op::LooseEqual
                    | Op::LooseNotEqual
                    | Op::SameValue => {
                        let (dst, lhs, rhs) = instr.reg3();
                        match op {
                            // SAFETY (frame-based arms): live top frame (see the
                            // `is_at_floor` guard); it is the executing frame.
                            Op::Equal => {
                                self.run_equal_regs(top_frame!(), dst, lhs, rhs, false, feedback)?
                            }
                            Op::NotEqual => {
                                self.run_equal_regs(top_frame!(), dst, lhs, rhs, true, feedback)?
                            }
                            Op::LooseEqual => {
                                (match self.run_loose_equal_regs(
                                    stack, context, top_ptr, dst, lhs, rhs, false, feedback,
                                ) {
                                    Ok(value) => value,
                                    Err(CommittedValueError::JavaScript(error)) => {
                                        return Err(error);
                                    }
                                    Err(CommittedValueError::Fatal(error)) => {
                                        return Ok(DispatchOutcome::Fatal(error));
                                    }
                                });
                            }
                            Op::LooseNotEqual => {
                                (match self.run_loose_equal_regs(
                                    stack, context, top_ptr, dst, lhs, rhs, true, feedback,
                                ) {
                                    Ok(value) => value,
                                    Err(CommittedValueError::JavaScript(error)) => {
                                        return Err(error);
                                    }
                                    Err(CommittedValueError::Fatal(error)) => {
                                        return Ok(DispatchOutcome::Fatal(error));
                                    }
                                });
                            }
                            Op::SameValue => {
                                let lhs = *read_register(&(*top_frame!()), lhs)?;
                                let rhs = *read_register(&(*top_frame!()), rhs)?;
                                let result = match self.scalar_value(
                                    stack,
                                    context,
                                    crate::ScalarValueOp::SameValue,
                                    lhs,
                                    rhs,
                                    Value::undefined(),
                                ) {
                                    Ok(value) => value,
                                    Err(CommittedValueError::JavaScript(error)) => {
                                        return Err(error);
                                    }
                                    Err(CommittedValueError::Fatal(error)) => {
                                        return Ok(DispatchOutcome::Fatal(error));
                                    }
                                };
                                let frame = &mut (*top_frame!());
                                write_register(frame, dst, result)?;
                                frame.advance_pc_fast();
                            }
                            _ => unreachable!("equality opcode group"),
                        }
                        continue 'activation;
                    }
                    Op::ArrayLength => {
                        let dst = instr.reg(0);
                        let src = instr.reg(1);
                        let source = *read_register(&(*top_frame!()), src)?;
                        let result = match self.scalar_value(
                            stack,
                            context,
                            crate::ScalarValueOp::ArrayLength,
                            source,
                            Value::undefined(),
                            Value::undefined(),
                        ) {
                            Ok(value) => value,
                            Err(CommittedValueError::JavaScript(error)) => return Err(error),
                            Err(CommittedValueError::Fatal(error)) => {
                                return Ok(DispatchOutcome::Fatal(error));
                            }
                        };
                        let frame = &mut (*top_frame!());
                        write_register(frame, dst, result)?;
                        frame.advance_pc()?;
                        continue 'activation;
                    }
                    Op::IsEvalIntrinsic => {
                        let dst = instr.reg(0);
                        let src = instr.reg(1);
                        self.run_is_eval_intrinsic_reg(stack, top_idx, dst, src)?;
                        continue 'activation;
                    }
                    Op::MakeClosure => {
                        let dst = instr.reg(0);
                        let function_index = instr.const_word(1);
                        let ctx = instr.reg(2);
                        let frame = &mut (*top_frame!());
                        self.run_make_closure_regs(context, frame, dst, function_index, ctx)?;
                        continue 'activation;
                    }
                    Op::ArrayBufferCall => {
                        let operands = function.operand_view(instr);
                        self.run_array_buffer_static_call_operands(stack, operands)?;
                        continue 'activation;
                    }
                    Op::SharedArrayBufferCall => {
                        let operands = function.operand_view(instr);
                        self.run_shared_array_buffer_static_call_operands(stack, operands)?;
                        continue 'activation;
                    }
                    Op::BigIntCall | Op::DataViewCall => {
                        let operands = function.operand_view(instr);
                        let frame = &mut (*top_frame!());
                        self.run_static_call_operands(op, context, frame, operands)?;
                        continue 'activation;
                    }
                    Op::ForInKeys => {
                        let operands = function.operand_view(instr);
                        (match self.run_for_in_keys_operands(context, stack, operands) {
                            Ok(value) => value,
                            Err(CommittedValueError::JavaScript(error)) => return Err(error),
                            Err(CommittedValueError::Fatal(error)) => {
                                return Ok(DispatchOutcome::Fatal(error));
                            }
                        });
                        continue 'activation;
                    }
                    Op::CopyDataProperties => {
                        let operands = function.operand_view(instr);
                        (match self.run_copy_data_properties_operands(context, stack, operands) {
                            Ok(value) => value,
                            Err(CommittedValueError::JavaScript(error)) => return Err(error),
                            Err(CommittedValueError::Fatal(error)) => {
                                return Ok(DispatchOutcome::Fatal(error));
                            }
                        });
                        continue 'activation;
                    }
                    Op::StarReexport => {
                        let operands = function.operand_view(instr);
                        (match self.run_star_reexport_operands(context, stack, operands) {
                            Ok(value) => value,
                            Err(CommittedValueError::JavaScript(error)) => return Err(error),
                            Err(CommittedValueError::Fatal(error)) => {
                                return Ok(DispatchOutcome::Fatal(error));
                            }
                        });
                        continue 'activation;
                    }
                    Op::DefineOwnProperty => {
                        let operands = function.operand_view(instr);
                        (match self.run_define_own_property_operands(context, stack, operands) {
                            Ok(value) => value,
                            Err(CommittedValueError::JavaScript(error)) => return Err(error),
                            Err(CommittedValueError::Fatal(error)) => {
                                return Ok(DispatchOutcome::Fatal(error));
                            }
                        });
                        continue 'activation;
                    }
                    Op::DefineMember => {
                        let definition = function
                            .imm32(instr, 3)
                            .and_then(otter_bytecode::MemberDefinition::from_imm32)
                            .ok_or(VmError::InvalidOperand)?;
                        (match self.run_define_member_regs(
                            context,
                            stack,
                            top_idx,
                            instr.reg(0),
                            instr.reg(1),
                            instr.reg(2),
                            definition,
                        ) {
                            Ok(value) => value,
                            Err(CommittedValueError::JavaScript(error)) => return Err(error),
                            Err(CommittedValueError::Fatal(error)) => {
                                return Ok(DispatchOutcome::Fatal(error));
                            }
                        });
                        continue 'activation;
                    }
                    Op::PromiseNew => {
                        let operands = function.operand_view(instr);
                        self.run_promise_new_operands(context, stack, operands)?;
                        continue 'activation;
                    }
                    Op::PromiseCall => {
                        let operands = function.operand_view(instr);
                        match self.run_promise_call_operands(context, stack, operands) {
                            Ok(()) => {}
                            Err(CommittedValueError::JavaScript(error)) => return Err(error),
                            Err(CommittedValueError::Fatal(error)) => {
                                return Ok(DispatchOutcome::Fatal(error));
                            }
                        }
                        continue 'activation;
                    }
                    Op::ImportNamespaceDynamic => {
                        let operands = function.operand_view(instr);
                        (match self.run_import_namespace_dynamic_operands(
                            context, stack, top_idx, operands,
                        ) {
                            Ok(value) => value,
                            Err(CommittedValueError::JavaScript(error)) => return Err(error),
                            Err(CommittedValueError::Fatal(error)) => {
                                return Ok(DispatchOutcome::Fatal(error));
                            }
                        });
                        continue 'activation;
                    }
                    Op::BindFunction => {
                        let operands = function.operand_view(instr);
                        (match self.drive_bind_function(stack, context, operands) {
                            Ok(value) => value,
                            Err(CommittedValueError::JavaScript(error)) => return Err(error),
                            Err(CommittedValueError::Fatal(error)) => {
                                return Ok(DispatchOutcome::Fatal(error));
                            }
                        });
                        continue 'activation;
                    }
                }
            }
        }
    }
}
