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
pub(super) struct PendingSourceWork<'a> {
    cell: &'a crate::native_abi::SourceWork,
    pending: u64,
}

impl<'a> PendingSourceWork<'a> {
    #[inline]
    fn new(cell: &'a crate::native_abi::SourceWork) -> Self {
        Self { cell, pending: 0 }
    }

    #[inline]
    pub(super) fn count(&mut self) {
        self.pending += 1;
    }

    #[inline]
    pub(super) fn publish(&mut self) {
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
        let code = function.code.wordcode().instructions();
        let mut run = super::dispatch_ops::OpRun {
            context,
            stack,
            top_ptr,
            top_idx,
            depth,
            floor,
            function,
            function_id,
            jit_installed,
            source_work: &mut source_work,
        };
        // An opcode that can stage a call or complete the activation
        // continues at `'activation`, which tests both; an opcode that only
        // computes in registers and the PC continues straight at `'dispatch`.
        'activation: loop {
            if run.stack.has_pending_call() {
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
            // The PC lives in a register while opcodes only compute: an
            // opcode's handler runs with it published and the loop reloads
            // it after.
            let mut pc = top_frame!().pc;
            'dispatch: loop {
                debug_assert_eq!(
                    run.stack.top_with_depth(),
                    (top_ptr, depth),
                    "an interpreter run dispatches one activation"
                );
                let idx = pc as usize;
                let Some(instruction) = code.get(idx) else {
                    tick.pc = pc;
                    return Err(VmError::MissingReturn);
                };
                let op = instruction.op;
                // Interpreter-only execution charges no source work and
                // records no feedback.
                if jit_installed {
                    run.source_work.count();
                }
                // Every opcode has a static base work charge; the checkpoint stays
                // gated on `enforce_budget` in the default Observe mode.
                budget_work.units = budget_work
                    .units
                    .wrapping_add(u64::from(instruction.work_units()));
                // Budget enforcement, CPU sampling and step tracing are each
                // installed between turns, never mid-loop, and run out of line.
                if has_hooks {
                    top_frame!().pc = pc;
                    tick.pc = pc;
                    if self.run_step_hooks(&mut run, idx, budget_work.units, enforce_budget)? {
                        budget_work.units = 0;
                    }
                }
                let word = |index: usize| instruction.inline_word(index);
                let registers = top_frame!().registers.as_mut_ptr();
                // SAFETY (both): a schema register operand, bounded by the
                // build-time verifier against this activation's window.
                let read = |register: u32| unsafe { *registers.add(register as usize) };
                let write = |register: u32, value: Value| unsafe {
                    *registers.add(register as usize) = value
                };
                // Take the branch at operand 0: a back edge polls for
                // interruption and, under a JIT, publishes source work and
                // may enter compiled code. Anything apply_branch would reject
                // runs the opcode's handler.
                macro_rules! take_branch {
                    () => {{
                        let offset = word(0) as i32;
                        let target = i64::from(pc) + 1 + i64::from(offset);
                        if (0..=i64::from(u32::MAX)).contains(&target)
                            && (offset >= 0 || !self.interrupt.is_set())
                        {
                            pc = target as u32;
                            if jit_installed && offset < 0 {
                                top_frame!().pc = pc;
                                tick.pc = pc;
                                run.source_work.publish();
                                if let Some(entry) =
                                    self.note_backedge_and_maybe_osr(run.stack, context, top_idx)?
                                {
                                    return Ok(DispatchOutcome::Tier(entry));
                                }
                            }
                            continue 'dispatch;
                        }
                    }};
                }
                // An int32 pair takes the opcode's own int32 form when it has
                // one and the result stays int32; any other Number pair folds
                // as the opcode's handler does; anything else runs it.
                // Feedback mirrors the handler: arithmetic also records the
                // result's representation, an immediate form and a comparison
                // only their operands.
                macro_rules! binary {
                    ($lhs:expr, $rhs:expr, $fold_op:expr, $records_result:expr, |$a:ident, $b:ident| $int:expr) => {{
                        let (lhs, rhs): (Value, Value) = ($lhs, $rhs);
                        let int = match (lhs.as_i32(), rhs.as_i32()) {
                            (Some($a), Some($b)) => ($int).map(Value::number_i32),
                            _ => None,
                        };
                        if let Some(result) = int.or_else(|| {
                            crate::arithmetic_dispatch::fold_numbers($fold_op, lhs, rhs)
                        }) {
                            if jit_installed
                                && let Some(feedback) = function.feedback_recorder_at(idx)
                            {
                                feedback.record_arith(lhs, rhs);
                                if $records_result {
                                    feedback.record_arith(result, result);
                                }
                            }
                            write(word(0), result);
                            pc += 1;
                            continue 'dispatch;
                        }
                    }};
                }
                macro_rules! compare {
                    ($lhs:expr, $rhs:expr, $compare_op:expr, |$a:ident, $b:ident| $int:expr) => {{
                        let (lhs, rhs): (Value, Value) = ($lhs, $rhs);
                        let int = match (lhs.as_i32(), rhs.as_i32()) {
                            (Some($a), Some($b)) => Some(Value::boolean($int)),
                            _ => None,
                        };
                        if let Some(result) = int.or_else(|| {
                            crate::arithmetic_dispatch::compare_numbers($compare_op, lhs, rhs)
                        }) {
                            if jit_installed
                                && let Some(feedback) = function.feedback_recorder_at(idx)
                            {
                                feedback.record_arith(lhs, rhs);
                            }
                            write(word(0), result);
                            pc += 1;
                            continue 'dispatch;
                        }
                    }};
                }
                macro_rules! next {
                    () => {{
                        pc += 1;
                        continue 'dispatch;
                    }};
                }
                macro_rules! commit {
                    ($effect:expr) => {{
                        $effect;
                        next!();
                    }};
                }
                // Opcodes that only compute commit here when their operands
                // allow; every other opcode, and every operand a committed
                // form refuses, runs the opcode's handler.
                match op {
                    Op::Nop => next!(),
                    Op::LoadUndefined => commit!(write(word(0), Value::undefined())),
                    Op::LoadHole => commit!(write(word(0), Value::hole())),
                    Op::LoadTrue => commit!(write(word(0), Value::boolean(true))),
                    Op::LoadFalse => commit!(write(word(0), Value::boolean(false))),
                    Op::LoadNull => commit!(write(word(0), Value::null())),
                    Op::LoadInt32 => {
                        commit!(write(
                            word(0),
                            Value::number(NumberValue::Smi(word(1) as i32))
                        ))
                    }
                    // The `Imm32` is a local index, itself a schema register.
                    Op::LoadLocal => commit!(write(word(0), read(word(1)))),
                    Op::StoreLocal => commit!(write(word(1), read(word(0)))),
                    Op::Jump => take_branch!(),
                    Op::JumpIfTrue | Op::JumpIfFalse => {
                        let taken =
                            read(word(1)).to_boolean(&self.gc_heap) == (op == Op::JumpIfTrue);
                        if jit_installed && let Some(cell) = function.feedback_recorder_at(idx) {
                            cell.record_branch(taken);
                        }
                        if !taken {
                            next!();
                        }
                        take_branch!();
                    }
                    Op::JumpIfNullish => {
                        if !read(word(1)).is_nullish() {
                            next!();
                        }
                        take_branch!();
                    }
                    Op::Increment => {
                        let value = read(word(1));
                        let delta = if instruction.operand_count() > 2 {
                            word(2) as i32
                        } else {
                            1
                        };
                        if let Some(result) = crate::arithmetic_dispatch::step_number(value, delta)
                        {
                            if jit_installed
                                && let Some(feedback) = function.feedback_recorder_at(idx)
                            {
                                feedback.record_arith(value, Value::number_i32(delta));
                                feedback.record_arith(result, result);
                            }
                            commit!(write(word(0), result));
                        }
                    }
                    Op::Add => binary!(read(word(1)), read(word(2)), Op::Add, true, |a, b| a
                        .checked_add(b)),
                    Op::Sub => binary!(read(word(1)), read(word(2)), Op::Sub, true, |a, b| a
                        .checked_sub(b)),
                    // An int32 product of zero with a negative factor is `-0`, a double.
                    Op::Mul => binary!(read(word(1)), read(word(2)), Op::Mul, true, |a, b| a
                        .checked_mul(b)
                        .filter(|&product| product != 0 || (a >= 0 && b >= 0))),
                    Op::Div => binary!(read(word(1)), read(word(2)), Op::Div, true, |_a, _b| None),
                    Op::Rem => binary!(read(word(1)), read(word(2)), Op::Rem, true, |_a, _b| None),
                    Op::Pow => binary!(read(word(1)), read(word(2)), Op::Pow, true, |_a, _b| None),
                    Op::BitwiseAnd => binary!(
                        read(word(1)),
                        read(word(2)),
                        Op::BitwiseAnd,
                        true,
                        |a, b| Some(a & b)
                    ),
                    Op::BitwiseOr => {
                        binary!(read(word(1)), read(word(2)), Op::BitwiseOr, true, |a, b| {
                            Some(a | b)
                        })
                    }
                    Op::BitwiseXor => binary!(
                        read(word(1)),
                        read(word(2)),
                        Op::BitwiseXor,
                        true,
                        |a, b| Some(a ^ b)
                    ),
                    Op::Shl => binary!(read(word(1)), read(word(2)), Op::Shl, true, |a, b| Some(
                        a.wrapping_shl(b as u32 & 31)
                    )),
                    Op::Shr => binary!(read(word(1)), read(word(2)), Op::Shr, true, |a, b| Some(
                        a.wrapping_shr(b as u32 & 31)
                    )),
                    Op::AddImm => binary!(
                        read(word(1)),
                        Value::number_i32(word(2) as i32),
                        Op::Add,
                        false,
                        |a, b| a.checked_add(b)
                    ),
                    Op::SubImm => binary!(
                        read(word(1)),
                        Value::number_i32(word(2) as i32),
                        Op::Sub,
                        false,
                        |a, b| a.checked_sub(b)
                    ),
                    Op::BitwiseAndImm => binary!(
                        read(word(1)),
                        Value::number_i32(word(2) as i32),
                        Op::BitwiseAnd,
                        false,
                        |a, b| Some(a & b)
                    ),
                    Op::LessThan => {
                        compare!(read(word(1)), read(word(2)), Op::LessThan, |a, b| a < b)
                    }
                    Op::LessEq => compare!(read(word(1)), read(word(2)), Op::LessEq, |a, b| a <= b),
                    Op::GreaterThan => {
                        compare!(read(word(1)), read(word(2)), Op::GreaterThan, |a, b| a > b)
                    }
                    Op::GreaterEq => {
                        compare!(read(word(1)), read(word(2)), Op::GreaterEq, |a, b| a >= b)
                    }
                    Op::LessThanImm => compare!(
                        read(word(1)),
                        Value::number_i32(word(2) as i32),
                        Op::LessThan,
                        |a, b| a < b
                    ),
                    _ => {}
                }
                top_frame!().pc = pc;
                tick.pc = pc;
                let step = match op {
                    Op::ReturnValue | Op::Return => self.op_return_value(&mut run, idx),
                    Op::ReturnUndefined => self.op_return_undefined(&mut run, idx),
                    Op::ReturnDerived => self.op_return_derived(&mut run, idx),
                    Op::Call | Op::CallWithThis => self.op_call(&mut run, idx),
                    Op::TailCall => self.op_tail_call(&mut run, idx),
                    Op::CallForwardArguments => self.op_call_forward_arguments(&mut run, idx),
                    Op::CallMethodValue => self.op_call_method_value(&mut run, idx),
                    Op::CallSpread => self.op_call_spread(&mut run, idx),
                    Op::New => self.op_new(&mut run, idx),
                    Op::SuperConstruct => self.op_super_construct(&mut run, idx),
                    Op::NewSpread => self.op_new_spread(&mut run, idx),
                    Op::SuperConstructSpread => self.op_super_construct_spread(&mut run, idx),
                    Op::BindThisValue => self.op_bind_this_value(&mut run, idx),
                    Op::Throw => self.op_throw(&mut run, idx),
                    Op::Await => self.op_await(&mut run, idx),
                    Op::YieldDelegate => self.op_yield_delegate(&mut run, idx),
                    Op::Yield => self.op_yield(&mut run, idx),
                    Op::GeneratorStart => self.op_generator_start(&mut run, idx),
                    Op::ToNumber => self.op_to_number(&mut run, idx),
                    Op::ToPrimitive => self.op_to_primitive(&mut run, idx),
                    Op::GetIterator => self.op_get_iterator(&mut run, idx),
                    Op::GetAsyncIterator => self.op_get_async_iterator(&mut run, idx),
                    Op::IteratorNext => self.op_iterator_next(&mut run, idx),
                    Op::IteratorClose => self.op_iterator_close(&mut run, idx),
                    Op::IteratorCloseThrow => self.op_iterator_close_throw(&mut run, idx),
                    Op::AsyncIteratorReturn => self.op_async_iterator_return(&mut run, idx),
                    Op::CheckIteratorResult => self.op_check_iterator_result(&mut run, idx),
                    Op::HasNamedProperty => self.op_has_named_property(&mut run, idx),
                    Op::LoadProperty => self.op_load_property(&mut run, idx),
                    Op::LoadElement => self.op_load_element(&mut run, idx),
                    Op::LoadSuperProperty => self.op_load_super_property(&mut run, idx),
                    Op::LoadSuperElement => self.op_load_super_element(&mut run, idx),
                    Op::SetSuperProperty => self.op_set_super_property(&mut run, idx),
                    Op::SetSuperElement => self.op_set_super_element(&mut run, idx),
                    Op::StoreProperty => self.op_store_property(&mut run, idx),
                    Op::StorePropertyStrict => self.op_store_property_strict(&mut run, idx),
                    Op::StoreElement | Op::StoreElementStrict => {
                        self.op_store_element(&mut run, idx)
                    }
                    Op::Instanceof => self.op_instanceof(&mut run, idx),
                    Op::HasProperty => self.op_has_property(&mut run, idx),
                    Op::DeleteProperty => self.op_delete_property(&mut run, idx),
                    Op::DeleteElement => self.op_delete_element(&mut run, idx),
                    Op::GetPrototype => self.op_get_prototype(&mut run, idx),
                    Op::SetPrototype => self.op_set_prototype(&mut run, idx),
                    Op::Eval => self.op_eval(&mut run, idx),
                    Op::LoadArgumentsLength | Op::LoadArgumentsElement => {
                        self.op_load_arguments_length(&mut run, idx)
                    }
                    Op::CollectArguments => self.op_collect_arguments(&mut run, idx),
                    Op::Nop => self.op_nop(&mut run, idx),
                    Op::LoadUndefined => self.op_load_undefined(&mut run, idx),
                    Op::LoadHole => self.op_load_hole(&mut run, idx),
                    Op::LoadTrue => self.op_load_true(&mut run, idx),
                    Op::LoadFalse => self.op_load_false(&mut run, idx),
                    Op::LoadNull => self.op_load_null(&mut run, idx),
                    Op::LoadInt32 => self.op_load_int32(&mut run, idx),
                    Op::LoadNumber => self.op_load_number(&mut run, idx),
                    Op::LoadString => self.op_load_string(&mut run, idx),
                    Op::LoadLength => self.op_load_length(&mut run, idx),
                    Op::LogicalNot => self.op_logical_not(&mut run, idx),
                    Op::ToBoolean => self.op_to_boolean(&mut run, idx),
                    Op::GetStringIndex => self.op_get_string_index(&mut run, idx),
                    Op::TypeOf => self.op_type_of(&mut run, idx),
                    Op::TestTypeOf => self.op_test_type_of(&mut run, idx),
                    Op::LoadThis => self.op_load_this(&mut run, idx),
                    Op::LoadNewTarget => self.op_load_new_target(&mut run, idx),
                    Op::NewObject => self.op_new_object(&mut run, idx),
                    Op::NewArray => self.op_new_array(&mut run, idx),
                    Op::NewArrayBoilerplate => self.op_new_array_boilerplate(&mut run, idx),
                    Op::NewObjectLiteral => self.op_new_object_literal(&mut run, idx),
                    Op::LoadRegExp => self.op_load_reg_exp(&mut run, idx),
                    Op::LoadBigInt => self.op_load_big_int(&mut run, idx),
                    Op::LoadClosureContext => self.op_load_closure_context(&mut run, idx),
                    Op::LoadSelf => self.op_load_self(&mut run, idx),
                    Op::CreateContext => self.op_create_context(&mut run, idx),
                    Op::CopyContext => self.op_copy_context(&mut run, idx),
                    Op::LoadContextSlot | Op::LoadContextSlotChecked => {
                        self.op_load_context_slot(&mut run, idx)
                    }
                    Op::StoreContextSlot | Op::StoreContextSlotChecked => {
                        self.op_store_context_slot(&mut run, idx)
                    }
                    Op::BindThisContextSlot => self.op_bind_this_context_slot(&mut run, idx),
                    Op::CollectRest => self.op_collect_rest(&mut run, idx),
                    Op::MakeFunction => self.op_make_function(&mut run, idx),
                    Op::MakeClass => self.op_make_class(&mut run, idx),
                    Op::NewError => self.op_new_error(&mut run, idx),
                    Op::LoadGlobalThis => self.op_load_global_this(&mut run, idx),
                    Op::LoadGlobalOrThrow => self.op_load_global_or_throw(&mut run, idx),
                    Op::LoadGlobalOrUndefined => self.op_load_global_or_undefined(&mut run, idx),
                    Op::DeclareGlobalVar => self.op_declare_global_var(&mut run, idx),
                    Op::GetTemplateObject => self.op_get_template_object(&mut run, idx),
                    Op::LoadLookupSlot => self.op_load_lookup_slot(&mut run, idx),
                    Op::StoreLookupSlot => self.op_store_lookup_slot(&mut run, idx),
                    Op::DeleteLookupSlot => self.op_delete_lookup_slot(&mut run, idx),
                    Op::LoadLookupGlobal | Op::TypeofLookupGlobal => {
                        self.op_load_lookup_global(&mut run, idx)
                    }
                    Op::StoreLookupGlobal => self.op_store_lookup_global(&mut run, idx),
                    Op::DeleteLookupGlobal => self.op_delete_lookup_global(&mut run, idx),
                    Op::ResolveLookupRef => self.op_resolve_lookup_ref(&mut run, idx),
                    Op::StoreRef => self.op_store_ref(&mut run, idx),
                    Op::DeclareEvalVar => self.op_declare_eval_var(&mut run, idx),
                    Op::StoreVarScope => self.op_store_var_scope(&mut run, idx),
                    Op::NewPrivateName => self.op_new_private_name(&mut run, idx),
                    Op::DefineGlobalFunction => self.op_define_global_function(&mut run, idx),
                    Op::DeclareGlobalLex => self.op_declare_global_lex(&mut run, idx),
                    Op::StoreGlobalBinding => self.op_store_global_binding(&mut run, idx),
                    Op::InitGlobalLex => self.op_init_global_lex(&mut run, idx),
                    Op::ClassCheck => self.op_class_check(&mut run, idx),
                    Op::DefineDataProperty => self.op_define_data_property(&mut run, idx),
                    Op::SetFunctionName => self.op_set_function_name(&mut run, idx),
                    Op::PrivateGet => self.op_private_get(&mut run, idx),
                    Op::PrivateSet => self.op_private_set(&mut run, idx),
                    Op::ToNumeric => self.op_to_numeric(&mut run, idx),
                    Op::ToObject => self.op_to_object(&mut run, idx),
                    Op::ToPropertyKey => self.op_to_property_key(&mut run, idx),
                    Op::PrivateBrandCheck => self.op_private_brand_check(&mut run, idx),
                    Op::Increment => self.op_increment(&mut run, idx),
                    Op::ValidateGlobalDecl => self.op_validate_global_decl(&mut run, idx),
                    Op::DefineGlobalVar => self.op_define_global_var(&mut run, idx),
                    Op::ImportNamespace => self.op_import_namespace(&mut run, idx),
                    Op::ImportNamespaceDeferred => self.op_import_namespace_deferred(&mut run, idx),
                    Op::ModuleNamespaceObject => self.op_module_namespace_object(&mut run, idx),
                    Op::LoadImportBinding => self.op_load_import_binding(&mut run, idx),
                    Op::EvaluateModule => self.op_evaluate_module(&mut run, idx),
                    Op::MarkModuleEvaluated => self.op_mark_module_evaluated(&mut run, idx),
                    Op::ImportMetaResolve => self.op_import_meta_resolve(&mut run, idx),
                    Op::PromiseFulfilledOf => self.op_promise_fulfilled_of(&mut run, idx),
                    Op::ArrayPush => self.op_array_push(&mut run, idx),
                    Op::SpreadAppend => self.op_spread_append(&mut run, idx),
                    Op::NewWeakRef => self.op_new_weak_ref(&mut run, idx),
                    Op::NewFinalizationRegistry => self.op_new_finalization_registry(&mut run, idx),
                    Op::NewCollection => self.op_new_collection(&mut run, idx),
                    Op::SymbolLoad => self.op_symbol_load(&mut run, idx),
                    Op::TemporalLoad => self.op_temporal_load(&mut run, idx),
                    Op::GlobalBindingExists => self.op_global_binding_exists(&mut run, idx),
                    Op::StoreGlobalChecked => self.op_store_global_checked(&mut run, idx),
                    Op::Jump => self.op_jump(&mut run, idx),
                    Op::JumpIfTrue => self.op_jump_if_true(&mut run, idx),
                    Op::JumpIfFalse => self.op_jump_if_false(&mut run, idx),
                    Op::JumpIfNullish => self.op_jump_if_nullish(&mut run, idx),
                    Op::LoadLocal => self.op_load_local(&mut run, idx),
                    Op::StoreLocal => self.op_store_local(&mut run, idx),
                    Op::TdzError => self.op_tdz_error(&mut run, idx),
                    Op::Add => self.op_add(&mut run, idx),
                    Op::AddImm => self.op_add_imm(&mut run, idx),
                    Op::SubImm => self.op_sub_imm(&mut run, idx),
                    Op::BitwiseAndImm => self.op_bitwise_and_imm(&mut run, idx),
                    Op::LessThanImm => self.op_less_than_imm(&mut run, idx),
                    Op::EqualImm => self.op_equal_imm(&mut run, idx),
                    Op::NotEqualImm => self.op_not_equal_imm(&mut run, idx),
                    Op::Sub | Op::Mul | Op::Div | Op::Rem | Op::Pow => self.op_sub(&mut run, idx),
                    Op::BitwiseAnd => self.op_bitwise_and(&mut run, idx),
                    Op::BitwiseOr => self.op_bitwise_or(&mut run, idx),
                    Op::BitwiseXor => self.op_bitwise_xor(&mut run, idx),
                    Op::Shl => self.op_shl(&mut run, idx),
                    Op::Shr => self.op_shr(&mut run, idx),
                    Op::LessThan | Op::LessEq | Op::GreaterThan | Op::GreaterEq => {
                        self.op_less_than(&mut run, idx)
                    }
                    Op::Ushr => self.op_ushr(&mut run, idx),
                    Op::Neg => self.op_neg(&mut run, idx),
                    Op::BitwiseNot => self.op_bitwise_not(&mut run, idx),
                    Op::Equal
                    | Op::NotEqual
                    | Op::LooseEqual
                    | Op::LooseNotEqual
                    | Op::SameValue => self.op_equal(&mut run, idx),
                    Op::ArrayLength => self.op_array_length(&mut run, idx),
                    Op::IsEvalIntrinsic => self.op_is_eval_intrinsic(&mut run, idx),
                    Op::MakeClosure => self.op_make_closure(&mut run, idx),
                    Op::ArrayBufferCall => self.op_array_buffer_call(&mut run, idx),
                    Op::SharedArrayBufferCall => self.op_shared_array_buffer_call(&mut run, idx),
                    Op::BigIntCall | Op::DataViewCall => self.op_big_int_call(&mut run, idx),
                    Op::ForInKeys => self.op_for_in_keys(&mut run, idx),
                    Op::CopyDataProperties => self.op_copy_data_properties(&mut run, idx),
                    Op::StarReexport => self.op_star_reexport(&mut run, idx),
                    Op::DefineOwnProperty => self.op_define_own_property(&mut run, idx),
                    Op::DefineMember => self.op_define_member(&mut run, idx),
                    Op::PromiseNew => self.op_promise_new(&mut run, idx),
                    Op::PromiseCall => self.op_promise_call(&mut run, idx),
                    Op::ImportNamespaceDynamic => self.op_import_namespace_dynamic(&mut run, idx),
                    Op::BindFunction => self.op_bind_function(&mut run, idx),
                }?;
                match step {
                    super::dispatch_ops::OpStep::Dispatch => pc = top_frame!().pc,
                    super::dispatch_ops::OpStep::Activation => continue 'activation,
                    super::dispatch_ops::OpStep::Leave(outcome) => return Ok(outcome),
                }
            }
        }
    }

    /// The per-opcode hooks of a hooked run: the budget checkpoint over the
    /// run's `units` so far, CPU sampling and step tracing. Returns whether
    /// the units were recorded.
    #[cold]
    #[inline(never)]
    fn run_step_hooks(
        &mut self,
        run: &mut super::dispatch_ops::OpRun<'_, '_>,
        idx: usize,
        units: u64,
        enforce_budget: bool,
    ) -> Result<bool, VmError> {
        if enforce_budget {
            self.work_budget_stats.record_work(units);
            self.enforce_work_budget_checkpoint()?;
        }
        if let Some(mut profiler) = self.cpu_profiler.take() {
            profiler.maybe_sample(self, run.context);
            self.cpu_profiler = Some(profiler);
        }
        if self.tracer.is_some() {
            let function = run.function;
            let instr = function.instr_at_index(idx).ok_or(VmError::MissingReturn)?;
            self.emit_step_trace(
                run.context,
                run.stack,
                run.top_idx,
                function,
                run.function_id,
                idx,
                instr,
            )?;
        }
        Ok(enforce_budget)
    }
}
