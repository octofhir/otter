//! JIT entry, OSR, and generated-call frame plumbing.
//!
//! # Contents
//! Tier-entry selection (`prepare_compiled_entry`, backedge/OSR accounting),
//! compiled completions (`complete_compiled_entry`, `jit_runtime_call`),
//! generated-call feedback through focused `jit_calls` modules, and cold
//! inlined/stack-call side-exit materialization in `jit_calls/deopt`.
//! Exact dispatched and Template source-opcode work funds native compilation;
//! generated linkage counters remain diagnostics. Generated-call entry feedback is
//! reconciled once after the outer native activation returns. That outer
//! boundary owns one post-entry transaction which roots and collector-rewrites
//! a validated compiled Return/Throw payload across the cold reconciliation.
//!
//! # Invariants
//! Bail diagnostics resolve the defining function owner across script boundaries.
//! Every generated callee frame remains published until native return, throw,
//! or cold deoptimization releases its entry lease and depth accounting.
//! Every VM-side compiled entry selection requires the exact installed code
//! generation and isolate-epoch dependency state. Safepoint resolution for
//! already-active Invalid code remains independent.
//! Optimized entries run only over fresh ordinary frames; every bail resumes
//! the interpreter on the generated exit's fully reconstructed register
//! window. Spliced exits re-enter each call through the interpreter's canonical
//! frame builder, restore every callee window, and publish the outer caller's
//! exact continuation PC to the native entry boundary. They use the same fully
//! wired runtime activation, published native frame, and call-scoped VM thread
//! as baseline entries.
//! Canonical tier transitions retain one [`Frame`] and register window;
//! The native trampoline owns physical call frames; inline deopt prepares
//! only descendants that had no physical activation.
//! Template entry and loop OSR retain one canonical whole-function body per
//! function id and select a header-specific trampoline from that shared object.
//! A nested compiled return never allocates during post-entry bookkeeping: it
//! only leaves feedback pending for the outermost activation. No result root
//! index or token crosses the VM/JIT boundary.
#![allow(unused_imports)]
use super::jit_compile::TemplateCompileOutcome;
use crate::native_abi::CommittedValueError;
use crate::*;
use crate::{
    native_abi::{Frame, NativeResultDomain, NativeResultPair, NativeResultStatus, SideExit},
    rooting::RootScopeExt,
};

#[derive(Debug, Default)]
struct GeneratedFunctionFeedback {
    entries: u64,
    baseline_entries: u64,
}

#[path = "jit_calls/deopt.rs"]
mod deopt;
#[path = "jit_calls/generated.rs"]
mod generated;

impl Interpreter {
    pub(super) fn jit_tier_work_decision(
        &mut self,
        context: &ExecutionContext,
        fid: u32,
        tier: crate::tier_policy::CostedTier,
        available_code_bytes: u64,
    ) -> Option<crate::tier_policy::TierWorkDecision> {
        let function = context.exec_function(fid)?;
        Some(
            self.optimizing_tier_policy
                .decide(function, tier, available_code_bytes),
        )
    }

    /// Route a pure exception returned by generated code through the canonical
    /// rooted interpreter unwind. Nested-call provenance survives propagation
    /// and is cleared only when this unwind actually lands in a local handler
    /// (or an async frame absorbs the throw).
    pub(crate) fn unwind_compiled_throw_above(
        &mut self,
        context: &ExecutionContext,
        stack: &mut ActivationStack,
        floor: ActivationFloor,
        thrown: Value,
    ) -> Result<(), VmError> {
        if self.pending_uncaught_frames.is_none() {
            self.pending_uncaught_frames = Some(self.snapshot_active_frames(context, usize::MAX));
        }
        // A compiled frame publishes the PC of the instruction that raised.
        let unwind = self.unwind_throw_above(
            context,
            stack,
            floor,
            thrown,
            crate::activation_stack::ThrowSite::Instruction,
        );
        if unwind.is_ok() {
            self.pending_uncaught_frames = None;
        }
        unwind
    }

    /// Count a generated receiver-allocation miss that left compiled code
    /// through an exact `AllocationMiss` exit instead of the cold allocation
    /// sibling, so every counted probe miss has exactly one completion.
    pub(crate) fn note_receiver_allocation_exit(
        &mut self,
        context: &ExecutionContext,
        fid: u32,
        pc: u32,
        exit: SideExit,
    ) {
        if exit.reason() != native_abi::ExitReason::AllocationMiss {
            return;
        }
        let construct = context
            .exec_function(fid)
            .and_then(|function| {
                function
                    .instr_at_index(pc as usize)
                    .map(|instruction| function.op(instruction))
            })
            .is_some_and(|op| {
                matches!(
                    op,
                    Op::New | Op::NewSpread | Op::SuperConstruct | Op::SuperConstructSpread
                )
            });
        if construct {
            self.jit_runtime_stats.receiver_alloc_deopts = self
                .jit_runtime_stats
                .receiver_alloc_deopts
                .saturating_add(1);
        }
    }

    pub(crate) fn record_jit_bail(
        &mut self,
        context: &ExecutionContext,
        fid: u32,
        tier: jit_debug::JitDebugTier,
        target: jit_debug::JitDebugTarget,
        exit: SideExit,
    ) {
        let pc = exit.logical_pc();
        // Optimizing exits are counted by their site-aware owner.
        if tier == jit_debug::JitDebugTier::Template {
            self.note_receiver_allocation_exit(context, fid, pc, exit);
        }
        self.record_jit_debug_event(|| {
            let owner = context.for_function(fid).ok();
            let context = owner.as_deref().unwrap_or(context);
            let function_name = context
                .function(fid)
                .map(|function| function.name.clone())
                .unwrap_or_else(|| "<unknown>".to_string());
            let instruction = context.exec_function(fid).and_then(|function| {
                function
                    .instr_at_index(pc as usize)
                    .map(|instr| (function, instr))
            });
            let op_debug = instruction.map(|(function, instr)| format!("{:?}", function.op(instr)));
            let operands_debug =
                instruction.map(|(function, instr)| format!("{:?}", function.operand_view(instr)));
            jit_debug::JitDebugEvent::Bail {
                function_id: fid,
                function_name,
                tier,
                target,
                resume_pc: pc,
                exit_reason: exit.reason(),
                exit_action: exit.action(),
                op_debug,
                operands_debug,
            }
        });
    }

    /// Select a generation for assembly to enter after this Rust entry returns.
    pub(crate) fn prepare_compiled_entry(
        &mut self,
        stack: &mut ActivationStack,
        context: &ExecutionContext,
    ) -> Result<Option<usize>, VmError> {
        let top = stack.len().checked_sub(1).ok_or(VmError::InvalidOperand)?;
        let frame = &stack[top];
        if frame.pc != 0 || self.frame_has_suspension_owner(frame) {
            return Ok(None);
        }
        let fid = frame.function_id;
        let owner = context
            .for_function(fid)
            .map_err(|_| VmError::InvalidOperand)?;
        let context = &*owner;
        let code = self
            .resolve_optimized_code_for_fid(context, fid)
            .or_else(|| self.resolve_jit_code(stack, context, top));
        let Some(code) = code else {
            return Ok(None);
        };
        let Some(entry) = code.entry_addr().filter(|entry| *entry != 0) else {
            return Ok(None);
        };
        if !self.jit_code_registry.is_current_for_entry(code.as_ref()) {
            return Ok(None);
        }
        let kind = code.native_frame_kind();
        let frame = &mut stack[top];
        if !frame.enter_compiled(kind) {
            return Err(VmError::InvalidOperand);
        }
        frame.code_object_id =
            u32::try_from(code.metadata().id).map_err(|_| VmError::InvalidOperand)?;
        if code.safepoint_count() != 0 {
            frame.header.flags = native_abi::NativeFrameFlags::from_bits(
                frame.header.flags.bits() | native_abi::NativeFrameFlags::HAS_SAFEPOINTS,
            );
        }
        if kind == native_abi::NativeFrameKind::Optimizing {
            self.jit_runtime_stats.optimized_entries =
                self.jit_runtime_stats.optimized_entries.saturating_add(1);
        }
        Ok(Some(entry))
    }

    /// Consume a generated result on the same physical frame at the Rust entry.
    pub(crate) fn complete_compiled_entry(
        &mut self,
        stack: &mut ActivationStack,
        context: &ExecutionContext,
        floor: ActivationFloor,
        result: NativeResultPair,
    ) -> Result<Option<Value>, VmError> {
        let top = stack.len().checked_sub(1).ok_or(VmError::InvalidOperand)?;
        let fid = stack[top].function_id;
        let owner = context
            .for_function(fid)
            .map_err(|_| VmError::InvalidOperand)?;
        let context = &*owner;
        let optimized = stack[top].header.kind == native_abi::NativeFrameKind::Optimizing;
        let exited_code_object_id = u64::from(stack[top].code_object_id);
        let pc = stack[top].pc;
        let osr_origin = self
            .frame_cold_mut(&mut stack[top])
            .and_then(|cold| cold.osr_origin.take());
        let status = result
            .validate(NativeResultDomain::Compiled)
            .ok_or(VmError::InvalidOperand)?;
        if !stack[top].enter_interpreter() {
            return Err(VmError::InvalidOperand);
        }
        match status {
            NativeResultStatus::Success => {
                if !optimized {
                    self.note_jit_entry_success(fid);
                }
                self.pop_frame_above(stack, floor, result.payload_value(), None)
            }
            NativeResultStatus::SideExit => {
                let exit = result.side_exit_payload().ok_or(VmError::InvalidOperand)?;
                if exit.logical_pc() != pc {
                    return Err(VmError::InvalidOperand);
                }
                if optimized {
                    let count = context
                        .exec_function(fid)
                        .ok_or(VmError::InvalidOperand)?
                        .param_count;
                    let parameters = stack[top]
                        .registers
                        .get(..usize::from(count))
                        .ok_or(VmError::InvalidOperand)?
                        .to_vec();
                    let parameters_widened = self.widen_exited_parameters(fid, exit, &parameters);
                    self.note_jit_optimized_bail(
                        context,
                        fid,
                        exited_code_object_id,
                        exit,
                        parameters_widened,
                    );
                }
                self.record_jit_bail(
                    context,
                    fid,
                    if optimized {
                        jit_debug::JitDebugTier::Optimizing
                    } else {
                        jit_debug::JitDebugTier::Template
                    },
                    osr_origin.map_or(jit_debug::JitDebugTarget::Entry, |pc| {
                        jit_debug::JitDebugTarget::Osr { pc }
                    }),
                    exit,
                );
                let repaired =
                    !optimized && self.reoptimize_arith_overflow_bail(context, fid, exit);
                // Reentrant JS can replace this body before its old extent
                // exits. Learning above still belongs to the source, while
                // disable/bail policy belongs only to the exiting owner.
                let current = self
                    .jit_code_registry
                    .is_current_generation(exited_code_object_id);
                if let Some(origin) = osr_origin {
                    if current
                        && !repaired
                        && !Self::is_poll_handoff(exit)
                        && Self::osr_bail_inside_target_loop(context, fid, origin, pc)
                    {
                        self.jit_osr_disabled.insert((fid, origin));
                    }
                } else if current && !optimized && !repaired && !Self::is_poll_handoff(exit) {
                    self.note_jit_entry_bail(context, fid);
                }
                Ok(None)
            }
            NativeResultStatus::Throw => {
                self.unwind_compiled_throw_above(context, stack, floor, result.payload_value())?;
                Ok(stack.is_at_floor(floor).then_some(Value::UNDEFINED))
            }
            NativeResultStatus::Fatal => {
                let ctx = stack.execution_context();
                let error = unsafe { (*ctx).error.as_mut() }.and_then(Option::take);
                Err(error.unwrap_or(VmError::InvalidOperand))
            }
            _ => Err(VmError::InvalidOperand),
        }
    }

    /// At an executed backedge, select OSR using the function's canonical
    /// source-work budget. The dispatched jump already charged its attempt;
    /// this boundary contributes no additional work or static span estimate.
    #[inline]
    pub(crate) fn note_backedge_and_maybe_osr(
        &mut self,
        stack: &mut ActivationStack,
        context: &ExecutionContext,
        top_idx: usize,
    ) -> Result<Option<usize>, VmError> {
        // Interpreter-only (no JIT installed): pay nothing beyond this branch.
        if self.jit_hook.is_none() {
            return Ok(None);
        }
        self.note_interpreted_retraining_backedge(&mut stack[top_idx]);
        if self.jit_retraining_blocks(stack[top_idx].function_id) {
            return Ok(None);
        }
        let frame = &stack[top_idx];
        let key = (frame.function_id, frame.pc);
        // A header that already proved un-tierable, or a whole uncompilable
        // function, never counts again.
        if self.jit_osr_disabled.contains(&key)
            || self.jit_osr_disabled.contains(&(key.0, u32::MAX))
        {
            return Ok(None);
        }
        // This boundary only wakes policy. The dispatched jump already
        // charged its actual attempt in the source CodeBlock.
        let optimizing_enabled = self
            .jit_hook
            .as_ref()
            .is_some_and(|hook| hook.optimizing_tier_enabled());
        let selected = if optimizing_enabled {
            crate::tier_policy::CostedTier::Optimizing
        } else {
            crate::tier_policy::CostedTier::Template
        };
        let available_code_bytes = self.jit_code_registry.available_code_bytes();
        if !self
            .jit_tier_work_decision(context, key.0, selected, available_code_bytes)
            .is_some_and(crate::tier_policy::TierWorkDecision::should_compile)
        {
            return Ok(None);
        }
        // Threshold reached: drop this header's counter (it tiers up now or is
        // marked disabled by `prepare_osr`, so it should not keep counting) and
        // attempt OSR.
        self.jit_runtime_stats.osr_attempts = self.jit_runtime_stats.osr_attempts.saturating_add(1);
        let outcome = self.prepare_osr(stack, context, top_idx, optimizing_enabled);
        outcome
    }

    /// Whether a baseline exit is a back-edge poll handing its loop to the
    /// interpreter (a relink or an OSR hand-off), not a failed operation.
    fn is_poll_handoff(exit: SideExit) -> bool {
        exit.reason() == native_abi::ExitReason::Interrupt
            && exit.action() == native_abi::ExitAction::Resume
    }

    /// Credit `batch` back-edges run by a baseline (Template) body to the loop
    /// header `header_pc` and report whether that loop now warrants optimizing
    /// OSR. Baseline code cannot enter an optimized body mid-loop itself, so a
    /// hot header leaves to the interpreter, whose next back-edge at that
    /// header crosses the same threshold and enters the optimized OSR entry.
    /// This is the baseline tier's OSR urgency check at `JumpLoop`.
    pub(crate) fn baseline_backedges_reach_osr(
        &mut self,
        context: &ExecutionContext,
        fid: u32,
        header_pc: u32,
        _batch: u64,
    ) -> bool {
        let key = (fid, header_pc);
        if self.jit_retraining_blocks(fid)
            || self.jit_osr_disabled.contains(&key)
            || self.jit_osr_disabled.contains(&(fid, u32::MAX))
            || !self
                .jit_hook
                .as_ref()
                .is_some_and(|hook| hook.optimizing_tier_enabled())
            || matches!(self.jit_optimized_code.get(&fid), Some(None))
        {
            return false;
        }
        // Native source segments were charged before the poll. Fuel and
        // backedge batches describe poll cadence, never executed opcode work.
        let available_code_bytes = self.jit_code_registry.available_code_bytes();
        self.jit_tier_work_decision(
            context,
            fid,
            crate::tier_policy::CostedTier::Optimizing,
            available_code_bytes,
        )
        .is_some_and(crate::tier_policy::TierWorkDecision::should_compile)
    }

    /// Select a loop entry and yield it to the common native trampoline.
    pub(crate) fn prepare_osr(
        &mut self,
        stack: &mut ActivationStack,
        context: &ExecutionContext,
        top_idx: usize,
        prefer_optimizing: bool,
    ) -> Result<Option<usize>, VmError> {
        let frame = &stack[top_idx];
        if self.frame_has_suspension_owner(frame) {
            return Ok(None);
        }
        let fid = frame.function_id;
        let osr_pc = frame.pc;
        if self.jit_retraining_blocks(fid)
            || self.jit_osr_disabled.contains(&(fid, u32::MAX))
            || self.jit_osr_disabled.contains(&(fid, osr_pc))
        {
            return Ok(None);
        }
        let optimized = prefer_optimizing
            .then(|| self.resolve_optimized_osr_code(context, fid, osr_pc))
            .flatten()
            .filter(|code| self.jit_code_registry.is_current_for_entry(code.as_ref()))
            .and_then(|code| code.osr_entry_addr(osr_pc).map(|entry| (code, entry)));
        let (code, entry) = if let Some(selected) = optimized {
            selected
        } else {
            let code = match self.resolve_template_osr_code(context, fid, osr_pc) {
                TemplateCompileOutcome::Installed(code) => code,
                TemplateCompileOutcome::Unsupported | TemplateCompileOutcome::Deferred => {
                    return Ok(None);
                }
            };
            if !self.jit_code_registry.is_current_for_entry(code.as_ref()) {
                return Ok(None);
            }
            let Some(entry) = code.osr_entry_addr(osr_pc) else {
                self.jit_osr_disabled.insert((fid, osr_pc));
                return Ok(None);
            };
            (code, entry)
        };
        if entry == 0 {
            return Err(VmError::InvalidOperand);
        }
        let kind = code.native_frame_kind();
        let frame = &mut stack[top_idx];
        if !frame.enter_compiled(kind) {
            return Err(VmError::InvalidOperand);
        }
        frame.code_object_id =
            u32::try_from(code.metadata().id).map_err(|_| VmError::InvalidOperand)?;
        let mut flags = frame.header.flags.bits() & !native_abi::NativeFrameFlags::HAS_SAFEPOINTS;
        if code.safepoint_count() != 0 {
            flags |= native_abi::NativeFrameFlags::HAS_SAFEPOINTS;
        }
        if kind == native_abi::NativeFrameKind::Optimizing {
            flags |= native_abi::NativeFrameFlags::OSR_ENTRY;
            self.jit_runtime_stats.optimized_entries =
                self.jit_runtime_stats.optimized_entries.saturating_add(1);
            self.jit_runtime_stats.optimized_osr_entries = self
                .jit_runtime_stats
                .optimized_osr_entries
                .saturating_add(1);
        }
        frame.header.flags = native_abi::NativeFrameFlags::from_bits(flags);
        self.frame_ensure_cold(frame).osr_origin = Some(osr_pc);
        Ok(Some(entry))
    }

    /// Resolve one whole-function Template body for loop OSR.
    ///
    /// `trigger_pc` identifies the header whose threshold caused the cold
    /// compile and remains useful in diagnostics. It does not specialize the
    /// emitted body: the returned object owns an OSR trampoline for every
    /// eligible loop header in `fid` and is therefore cached by function only.
    fn resolve_template_osr_code(
        &mut self,
        context: &ExecutionContext,
        fid: u32,
        trigger_pc: u32,
    ) -> TemplateCompileOutcome {
        if self.jit_retraining_blocks(fid) {
            return TemplateCompileOutcome::Deferred;
        }
        let cached = self.jit_code.get(&fid).map(|slot| match slot {
            Some(code) => TemplateCompileOutcome::Installed(code.clone()),
            None => TemplateCompileOutcome::Unsupported,
        });
        let outcome = match cached {
            Some(outcome) => outcome,
            None => {
                let outcome = self.compile_jit_function(context, fid, Some(trigger_pc));
                self.retain_template_compile_outcome(fid, outcome)
            }
        };
        match outcome {
            TemplateCompileOutcome::Installed(code)
                if self.jit_code_registry.is_current_for_entry(code.as_ref()) =>
            {
                self.jit_template_osr_fids.insert(fid);
                TemplateCompileOutcome::Installed(code)
            }
            TemplateCompileOutcome::Installed(_) => {
                self.jit_code.remove(&fid);
                self.jit_code_cache = None;
                self.jit_entry_osr_only.remove(&fid);
                self.jit_template_osr_fids.remove(&fid);
                TemplateCompileOutcome::Deferred
            }
            TemplateCompileOutcome::Unsupported => {
                self.jit_osr_disabled.insert((fid, u32::MAX));
                TemplateCompileOutcome::Unsupported
            }
            TemplateCompileOutcome::Deferred => TemplateCompileOutcome::Deferred,
        }
    }

    pub(crate) fn osr_bail_inside_target_loop(
        context: &ExecutionContext,
        fid: u32,
        osr_pc: u32,
        bail_pc: u32,
    ) -> bool {
        let Some(view) = context.jit_compile_snapshot(fid) else {
            return true;
        };
        Self::osr_bail_inside_target_loop_instructions(&view.code_block, osr_pc, bail_pc)
    }

    pub(crate) fn osr_bail_inside_target_loop_instructions(
        code_block: &crate::executable::CodeBlock,
        osr_pc: u32,
        bail_pc: u32,
    ) -> bool {
        let Some(loop_latch) = code_block.loop_latch(osr_pc) else {
            return true;
        };
        osr_pc <= bail_pc && bail_pc <= loop_latch
    }

    /// Recompile after the first typed integer-overflow or negative-zero exit
    /// from an arithmetic site, widening that site's feedback to float
    /// arithmetic. Other exit reasons at the same PC cannot trigger this
    /// policy. Widening once avoids permanently disabling an otherwise valid
    /// hot loop; a repeated exit follows the normal deopt/disable path.
    pub(crate) fn reoptimize_arith_overflow_bail(
        &mut self,
        context: &ExecutionContext,
        fid: u32,
        exit: SideExit,
    ) -> bool {
        if !self.widen_arith_exit_site(context, fid, exit.logical_pc(), exit.reason()) {
            return false;
        }
        self.begin_jit_retraining(context, fid);
        true
    }

    /// Widen the arithmetic feedback of the site `site_fid@site_pc` after an
    /// `Int32Overflow` or `NegativeZero` exit. Every op whose Int32 lowering
    /// can take such an exit reads the same arithmetic feedback: `Add`, `Sub`,
    /// `Mul`, `Neg`, `Increment`, `AddImm` and `SubImm`. Returns `true` only
    /// the first time the site widens.
    fn widen_arith_exit_site(
        &self,
        context: &ExecutionContext,
        site_fid: u32,
        site_pc: u32,
        reason: crate::native_abi::ExitReason,
    ) -> bool {
        if !matches!(
            reason,
            crate::native_abi::ExitReason::Int32Overflow
                | crate::native_abi::ExitReason::NegativeZero
        ) {
            return false;
        }
        let Some(function) = context.exec_function(site_fid) else {
            return false;
        };
        let Some(instr) = function.instr_at_index(site_pc as usize) else {
            return false;
        };
        if !matches!(
            function.op(instr),
            Op::Add | Op::Sub | Op::Mul | Op::Neg | Op::Increment | Op::AddImm | Op::SubImm
        ) {
            return false;
        }
        function
            .feedback_recorder_at(instr.instruction_pc as usize)
            .is_some_and(|feedback| feedback.widen_arith_to_float())
    }

    /// Charge one baseline entry exit and replace the generation only after
    /// the measured round-trip loss has repaid compilation and code memory.
    pub(crate) fn note_jit_entry_bail(&mut self, context: &ExecutionContext, fid: u32) {
        let bails = self.jit_entry_bail_counts.entry(fid).or_insert(0);
        *bails = bails.saturating_add(1);
        let available_code_bytes = self.jit_code_registry.available_code_bytes();
        if !self
            .jit_tier_work_decision(
                context,
                fid,
                crate::tier_policy::CostedTier::Template,
                available_code_bytes,
            )
            .is_some_and(crate::tier_policy::TierWorkDecision::should_compile)
        {
            return;
        }
        self.jit_entry_bail_counts.remove(&fid);
        self.invalidate_jit_function(fid);
    }

    /// Clear `fid`'s consecutive-entry-bail count after a successful compiled
    /// completion. The empty-map probe keeps this free on the hot path: the
    /// map only holds functions that bailed since their last success, which is
    /// almost always none.
    #[inline]
    pub(crate) fn note_jit_entry_success(&mut self, fid: u32) {
        if !self.jit_entry_bail_counts.is_empty() {
            self.jit_entry_bail_counts.remove(&fid);
        }
    }

    /// Widen the parameter profile of `fid` from the actual parameter values
    /// of an optimized entry whose type guards failed before its first
    /// instruction. The next generation's entry representations cannot
    /// repeat the failed parameter speculation, while parameters that held
    /// their representation keep it.
    pub(crate) fn widen_exited_parameters(
        &mut self,
        fid: u32,
        exit: native_abi::SideExit,
        parameters: &[Value],
    ) -> bool {
        if exit.logical_pc() != 0 || exit.reason() != native_abi::ExitReason::TypeMismatch {
            return false;
        }
        let widening = self
            .jit_parameter_widening
            .entry(fid)
            .or_insert_with(|| vec![jit::JitParameterWidening::Int32; parameters.len()].into());
        let mut changed = false;
        for (slot, &value) in widening.iter_mut().zip(parameters) {
            let next = (*slot).max(jit::JitParameterWidening::of(value));
            changed |= next != *slot;
            *slot = next;
        }
        changed
    }

    /// Record one optimizing-tier deoptimization and self-correct the
    /// speculation that caused it.
    ///
    /// Record the exact speculation site before resuming it in the interpreter.
    /// A recompile action retires both native tiers and requires bounded
    /// interpreted work plus a complete path before replacement. Stable
    /// generated callers observe that interpreter destination through the
    /// permanent function entry cell; only spliced dependencies retire with it.
    pub(crate) fn note_jit_optimized_bail(
        &mut self,
        context: &ExecutionContext,
        fid: u32,
        exited_code_object_id: u64,
        exit: native_abi::SideExit,
        parameters_widened: bool,
    ) {
        self.note_jit_optimized_bail_at(
            context,
            fid,
            exited_code_object_id,
            exit,
            (fid, exit.logical_pc()),
            parameters_widened,
        );
    }

    /// [`Self::note_jit_optimized_bail`] for an exit whose speculation lives at
    /// `site.0@site.1`, the innermost spliced body of an inlined deopt.
    ///
    /// An `Int32Overflow` / `NegativeZero` exit first widens that arithmetic
    /// site and retires the generation, so the next compile uses float
    /// arithmetic there. Every entry kind reaches this one owner; without it
    /// an entry exit would keep the same speculation and exit on every call.
    /// Every exit records source history and learning. Only a still-current
    /// exact generation charges exit policy; a leased invalid old body cannot
    /// retire its replacement. New source widening instead retires that source
    /// and its actual splices, independently of the old outer generation.
    pub(crate) fn note_jit_optimized_bail_at(
        &mut self,
        context: &ExecutionContext,
        fid: u32,
        exited_code_object_id: u64,
        exit: native_abi::SideExit,
        site: (u32, u32),
        parameters_widened: bool,
    ) {
        let (site_fid, site_pc) = site;
        let current = self
            .jit_code_registry
            .is_current_generation(exited_code_object_id);
        self.jit_runtime_stats.optimized_deopts =
            self.jit_runtime_stats.optimized_deopts.saturating_add(1);
        self.note_receiver_allocation_exit(context, site_fid, site_pc, exit);
        let population = (exit.reason() == native_abi::ExitReason::ShapeGuard)
            .then(|| {
                context
                    .for_function(site_fid)
                    .ok()?
                    .exec_function(site_fid)?
                    .property_site_population(site_pc as usize)
            })
            .flatten();
        let profile = self
            .jit_optimized_exit_profiles
            .entry((site_fid, site_pc, exit.reason()))
            .or_insert(jit::JitExitProfile {
                action: exit.action(),
                count: 0,
                feedback_population: None,
            });
        profile.action = profile.action.max(exit.action());
        profile.count = profile.count.saturating_add(1);
        if population.is_some() {
            profile.feedback_population = population;
        }
        let action = profile.action;
        let widened = self.widen_arith_exit_site(context, site_fid, site_pc, exit.reason());
        if !current {
            // A genuinely new source representation can stale current narrow
            // code even when this physical activation has already retired.
            // Retire the changed source and canonical splices of that source;
            // newer unrelated outer bodies retain their entry destinations.
            if parameters_widened {
                self.begin_jit_retraining(context, fid);
            }
            if widened && (!parameters_widened || site_fid != fid) {
                self.begin_jit_retraining(context, site_fid);
            }
            return;
        }
        if action == native_abi::ExitAction::Resume && !widened && !parameters_widened {
            return;
        }
        if action == native_abi::ExitAction::Invalidate && !widened && !parameters_widened {
            self.abandon_unsupported_optimized_generation(fid);
            return;
        }
        self.begin_jit_retraining(context, fid);
        if site_fid != fid {
            self.begin_jit_retraining(context, site_fid);
        }
    }

    /// Drop a structurally invalid optimizing generation and cache the decline
    /// until material feedback changes.
    fn abandon_unsupported_optimized_generation(&mut self, fid: u32) {
        let dependents = match self.jit_optimized_code.get(&fid) {
            Some(Some(code)) => self
                .jit_code_registry
                .invalidate_code_object(code.metadata().id),
            _ => return,
        };
        self.jit_optimized_code.insert(fid, None);
        self.jit_optimized_declined_epoch
            .insert(fid, self.code_space.feedback_epoch(fid));
        self.jit_optimized_code_cache = None;
        let dependents: Vec<u32> = dependents
            .into_iter()
            .filter(|&dependent| dependent != fid)
            .collect();
        self.discard_invalidated_jit_state(&dependents);
    }

    /// Remove only invalidated generation ownership from affected functions.
    ///
    /// Hotness and lifetime exit evidence survive; a deopt replacement still
    /// waits for the tier policy's independent interpreted retraining boundary.
    /// Another tier can remain installed for the same function; the registry
    /// alone decides whether that exact map owner stays current.
    pub(crate) fn discard_invalidated_jit_state(&mut self, affected: &[u32]) {
        if affected.is_empty() {
            return;
        }
        let mut retired_template_fids = rustc_hash::FxHashSet::default();
        for &fid in affected {
            let template_current = self
                .jit_code
                .get(&fid)
                .and_then(Option::as_ref)
                .is_some_and(|code| self.jit_code_registry.is_current_for_entry(code.as_ref()));
            if !template_current {
                self.jit_code.remove(&fid);
                self.jit_entry_osr_only.remove(&fid);
                self.jit_entry_bail_counts.remove(&fid);
                retired_template_fids.insert(fid);
            }
            let optimizing_current = self
                .jit_optimized_code
                .get(&fid)
                .and_then(Option::as_ref)
                .is_some_and(|code| self.jit_code_registry.is_current_for_entry(code.as_ref()));
            if !optimizing_current {
                self.jit_optimized_code.remove(&fid);
                self.jit_optimized_declined_epoch.remove(&fid);
            }
        }
        self.jit_template_osr_fids
            .retain(|fid| !retired_template_fids.contains(fid));
        self.jit_osr_disabled
            .retain(|(fid, _)| !retired_template_fids.contains(fid));
        self.jit_code_cache = None;
        self.jit_optimized_code_cache = None;
    }

    /// Unlink every current native generation for `fid`.
    ///
    /// Stable generated callers are not invalidated: they observe a later
    /// replacement through `fid`'s function entry cell. Map/cache ownership is
    /// removed while hotness survives. Deopt uses [`Self::begin_jit_retraining`]
    /// to gate replacement on fresh interpreter execution.
    pub(crate) fn invalidate_jit_function(&mut self, fid: u32) {
        let mut affected = self.jit_code_registry.invalidate_function(fid);
        self.jit_runtime_stats.caller_invalidations =
            self.jit_runtime_stats.caller_invalidations.saturating_add(
                affected
                    .iter()
                    .filter(|&&affected_fid| affected_fid != fid)
                    .count() as u64,
            );
        if affected.binary_search(&fid).is_err() {
            affected.push(fid);
            affected.sort_unstable();
        }
        self.discard_invalidated_jit_state(&affected);
    }

    /// Resolve installed compiled code for the bytecode frame at `top_idx`,
    /// compiling once actual source work funds its cost. Returns `None` when the frame is
    /// ineligible (not a fresh ordinary bytecode entry), still cold, or known to
    /// be outside the compilable subset.
    pub(crate) fn resolve_jit_code(
        &mut self,
        stack: &ActivationStack,
        context: &ExecutionContext,
        top_idx: usize,
    ) -> Option<std::sync::Arc<dyn jit::JitFunctionCode>> {
        // Only fresh, ordinary bytecode frames: at entry (pc == 0), not async,
        // not a generator body.
        let frame = &stack[top_idx];
        if frame.pc != 0 || self.frame_has_suspension_owner(frame) {
            return None;
        }
        self.resolve_jit_code_for_fid(context, frame.function_id)
    }

    /// Run the promotion policy for a function whose generation the call
    /// trampoline found past its absolute source-work target.
    ///
    /// Generated callers enter bytecode callees without the interpreter, so
    /// the trampoline checks that generation's canonical source-work cell and
    /// calls this on the published frame. A promoted body publishes through the
    /// function's permanent entry cell, so every caller switches without
    /// recompiling; a cached outcome suppresses further requests from this
    /// generation, and any other decision names an absolute source-work target.
    pub(crate) fn promote_entered_function(&mut self, context: &ExecutionContext, fid: u32) {
        let Ok(owner) = context.for_function(fid) else {
            return;
        };
        let _ = self.resolve_optimized_code_for_fid(&owner, fid);
        if self.jit_optimized_code.contains_key(&fid)
            || !self
                .jit_hook
                .as_ref()
                .is_some_and(|hook| hook.optimizing_tier_enabled())
            || self.jit_optimized_declined_epoch.get(&fid)
                == Some(&self.code_space.feedback_epoch(fid))
        {
            self.jit_code_registry.suppress_generated_tiering(fid);
        } else if let Some(function) = owner.exec_function(fid) {
            let decision = self.optimizing_tier_policy.decide(
                function,
                crate::tier_policy::CostedTier::Optimizing,
                self.jit_code_registry.available_code_bytes(),
            );
            self.jit_code_registry
                .defer_generated_tiering(fid, decision.work_target);
        }
    }

    /// Resolve the current optimizing body, replacing the baseline generation
    /// exactly once after the deterministic promotion policy reaches
    /// `Promote`.
    pub(crate) fn resolve_optimized_code_for_fid(
        &mut self,
        context: &ExecutionContext,
        fid: u32,
    ) -> Option<std::sync::Arc<dyn jit::JitFunctionCode>> {
        if self.jit_retraining_blocks(fid)
            || !self
                .jit_hook
                .as_ref()
                .is_some_and(|hook| hook.optimizing_tier_enabled())
        {
            return None;
        }
        if let Some((cached_fid, code)) = &self.jit_optimized_code_cache
            && *cached_fid == fid
            && self.jit_code_registry.is_current_for_entry(code.as_ref())
        {
            return Some(code.clone());
        }
        let code = if let Some(slot) = self.jit_optimized_code.get(&fid) {
            slot.clone()
        } else {
            let function = context.exec_function(fid)?;
            let available_code_bytes = self.jit_code_registry.available_code_bytes();
            if self.optimizing_tier_decision_for(function, available_code_bytes)
                != crate::tier_policy::OptimizingDecision::Promote
            {
                return None;
            }
            let prior_attempts = self
                .optimizing_tier_policy
                .compile_attempts(fid, crate::tier_policy::CostedTier::Optimizing);
            let compiled = self.compile_optimized_jit_function(context, fid, None);
            let attempted = self
                .optimizing_tier_policy
                .compile_attempts(fid, crate::tier_policy::CostedTier::Optimizing)
                > prior_attempts;
            let resource_blocked = self
                .optimizing_tier_policy
                .decide(
                    function,
                    crate::tier_policy::CostedTier::Optimizing,
                    self.jit_code_registry.available_code_bytes(),
                )
                .resource_blocked();
            if compiled.is_some() || (attempted && !resource_blocked) {
                self.jit_optimized_code.insert(fid, compiled.clone());
            }
            self.jit_optimized_code_cache = None;
            compiled
        };
        let code = code.filter(|code| self.jit_code_registry.is_current_for_entry(code.as_ref()));
        if let Some(code) = &code {
            self.jit_optimized_code_cache = Some((fid, code.clone()));
        }
        code
    }

    /// Select the installed entry-capable Template body for an interpreter
    /// activation, compiling when the function-entry policy admits it.
    pub(crate) fn resolve_jit_code_for_fid(
        &mut self,
        context: &ExecutionContext,
        fid: u32,
    ) -> Option<std::sync::Arc<dyn jit::JitFunctionCode>> {
        if self.jit_retraining_blocks(fid) {
            return None;
        }
        // Single-entry compiled-code cache. A hot synchronous re-entry (Array
        // callbacks, comparators, `@@iterator` drives) resolves the SAME callee
        // every call; this skips the `jit_code` FxHashMap lookup + `Arc` clone
        // churn when the last resolve matched. The cache only ever holds
        // non-`osr_only` code, so it needs no further filtering.
        if let Some((cached_fid, code)) = &self.jit_code_cache
            && *cached_fid == fid
            && self.jit_code_registry.is_current_for_entry(code.as_ref())
        {
            return Some(code.clone());
        }
        // A body already known to be `osr_only` can never run at function entry;
        // short-circuit before the map probe + `Arc` clone below.
        if self.jit_entry_osr_only.contains(&fid) {
            return None;
        }
        let code = match self.jit_code.get(&fid) {
            Some(Some(code)) => code.clone(),
            Some(None) => return None,
            None => {
                let available_code_bytes = self.jit_code_registry.available_code_bytes();
                if !self
                    .jit_tier_work_decision(
                        context,
                        fid,
                        crate::tier_policy::CostedTier::Template,
                        available_code_bytes,
                    )?
                    .should_compile()
                {
                    return None;
                }
                let outcome = self.compile_jit_function(context, fid, None);
                match self.retain_template_compile_outcome(fid, outcome) {
                    TemplateCompileOutcome::Installed(code) => code,
                    TemplateCompileOutcome::Unsupported | TemplateCompileOutcome::Deferred => {
                        return None;
                    }
                }
            }
        };
        if !self.jit_code_registry.is_current_for_entry(code.as_ref()) {
            self.jit_code.remove(&fid);
            self.jit_code_cache = None;
            self.jit_entry_osr_only.remove(&fid);
            self.jit_template_osr_fids.remove(&fid);
            return None;
        }
        // The function-entry path never runs OSR-only code (compiled with
        // unsupported opcodes emitted as bails); only loop OSR enters it at a
        // supported loop header. The canonical body remains available there.
        if code.osr_only() {
            self.jit_entry_osr_only.insert(fid);
            return None;
        }
        self.jit_code_cache = Some((fid, code.clone()));
        Some(code)
    }

    /// Finish one compiled entry as a single VM-owned result transaction.
    ///
    /// Generated code has already unpublished its own native activation. A
    /// surviving parent activation makes this a nested return: mark feedback
    /// pending and return immediately, without retirement, allocation, or a
    /// temporary root. The outermost return retires unreachable code and, when
    /// feedback is pending, roots only a domain-validated boxed Return/Throw
    /// payload while the cold reconciliation may compile and collect.
    pub(crate) fn finish_compiled_entry_transaction(
        &mut self,
        context: Option<&ExecutionContext>,
        result: NativeResultPair,
        feedback_dirty: bool,
    ) -> Result<NativeResultPair, VmError> {
        self.jit_generated_feedback_pending |= feedback_dirty;
        if self.jit_has_native_frames() {
            return Ok(result);
        }

        // This is the generated-code retirement epoch boundary. No native
        // frame can still hold an unleased entry address, so invalid mappings
        // with no ordinary Arc owner may now be released.
        self.retire_unreferenced_jit_code();
        if !self.jit_generated_feedback_pending {
            return Ok(result);
        }

        Ok(self.with_rooted_compiled_result(result, |vm| {
            vm.reconcile_generated_feedback(context);
        }))
    }

    /// Run outer-boundary work with a compiled boxed result rooted in place.
    ///
    /// The descriptor-owned compiled domain is validated before payload bits
    /// are interpreted. Side exits, fatal results, and malformed carriers pass
    /// through unchanged and never enter a GC root slot.
    fn with_rooted_compiled_result(
        &mut self,
        result: NativeResultPair,
        operation: impl FnOnce(&mut Self),
    ) -> NativeResultPair {
        let Some(status @ (NativeResultStatus::Success | NativeResultStatus::Throw)) =
            result.validate(NativeResultDomain::Compiled)
        else {
            operation(self);
            return result;
        };

        let mut payload = result.payload_value();
        let mut roots = otter_gc::RootScope::new(&mut self.gc_heap);
        // SAFETY: `payload` precedes `roots`, remains stationary until the
        // explicit drop, and is the only moving value held across `operation`.
        unsafe { roots.add_value(&mut payload) };
        operation(self);
        drop(roots);

        match status {
            NativeResultStatus::Success => NativeResultPair::success(payload),
            NativeResultStatus::Throw => NativeResultPair::throw_value(payload),
            NativeResultStatus::SideExit
            | NativeResultStatus::Continue
            | NativeResultStatus::OutOfMemory
            | NativeResultStatus::Yield
            | NativeResultStatus::Fatal => unreachable!("status narrowed above"),
        }
    }

    /// Reconcile generated entry-cell feedback after the outermost native
    /// activation has been unpublished and its boxed result has been rooted.
    ///
    /// Native entries stay allocation- and transition-free. This cold pass
    /// groups exact-generation diagnostic deltas by function, advances the
    /// call-execution budget, then lets the existing optimizing resolver inspect
    /// source work already charged by baseline callees. The Graph
    /// backend reports cold deopts only, with no entry/return accounting;
    /// optimizing generations never become promotion candidates.
    fn reconcile_generated_feedback(&mut self, context: Option<&ExecutionContext>) {
        debug_assert!(!self.jit_has_native_frames());
        debug_assert!(self.jit_generated_feedback_pending);
        self.jit_generated_feedback_pending = false;

        let feedback = self.jit_code_registry.take_generated_feedback();
        let mut functions = rustc_hash::FxHashMap::<u32, GeneratedFunctionFeedback>::default();
        for entry in feedback {
            self.jit_runtime_stats.generated_calls = self
                .jit_runtime_stats
                .generated_calls
                .saturating_add(entry.entries);
            self.jit_runtime_stats.generated_call_deopts = self
                .jit_runtime_stats
                .generated_call_deopts
                .saturating_add(entry.deopts);
            match entry.tier {
                native_abi::NativeFrameKind::Baseline => {
                    self.jit_runtime_stats.generated_template_entries = self
                        .jit_runtime_stats
                        .generated_template_entries
                        .saturating_add(entry.entries);
                    self.jit_runtime_stats.generated_template_returns = self
                        .jit_runtime_stats
                        .generated_template_returns
                        .saturating_add(entry.returns);
                    self.jit_runtime_stats.generated_template_deopts = self
                        .jit_runtime_stats
                        .generated_template_deopts
                        .saturating_add(entry.deopts);
                }
                native_abi::NativeFrameKind::Optimizing => {
                    self.jit_runtime_stats.generated_optimizing_entries = self
                        .jit_runtime_stats
                        .generated_optimizing_entries
                        .saturating_add(entry.entries);
                    self.jit_runtime_stats.generated_optimizing_returns = self
                        .jit_runtime_stats
                        .generated_optimizing_returns
                        .saturating_add(entry.returns);
                    self.jit_runtime_stats.generated_optimizing_deopts = self
                        .jit_runtime_stats
                        .generated_optimizing_deopts
                        .saturating_add(entry.deopts);
                }
                native_abi::NativeFrameKind::Interpreter | native_abi::NativeFrameKind::Host => {
                    // The VM entry already charges its call-execution budget.
                    continue;
                }
            }
            if entry.entries == 0 {
                continue;
            }
            let entries = entry.entries;
            let batch = functions.entry(entry.function_id).or_default();
            batch.entries = batch.entries.saturating_add(entries);
            if entry.tier == native_abi::NativeFrameKind::Baseline {
                batch.baseline_entries = batch.baseline_entries.saturating_add(entries);
            }
        }

        let mut baseline_candidates = Vec::new();
        for (fid, batch) in functions {
            self.record_runtime_bytecode_calls(batch.entries);
            if batch.baseline_entries != 0 {
                baseline_candidates.push(fid);
            }
        }
        // Code object identities and captured artifacts follow this order.
        baseline_candidates.sort_unstable();
        for fid in baseline_candidates {
            if let Ok(owner) = self.function_context(context, fid) {
                let _ = self.resolve_optimized_code_for_fid(&owner, fid);
            }
        }
    }

    /// Complete one full fixed-arity construct in place for a compiled caller
    /// whose fixed-arity construction site fell outside the compiled subset.
    /// Reads the callee and
    /// argument registers from the caller's live window and runs the
    /// interpreter's own `Construct(callee, args, new_target)` synchronously
    /// under the caller's published activation, writing the constructed value
    /// into `dst`. `inherited_new_target` is present only for `super()`.
    ///
    /// A non-constructor callee reports `Ok(false)` and side-exits, keeping the
    /// interpreter the sole owner of the thrown `TypeError`. On `Ok(true)` the
    /// destination register holds the constructed object and the compiled
    /// caller continues at the next instruction.
    ///
    /// The constructor body runs through
    /// [`Self::run_construct_sync_rooted`] (the caller already holds an
    /// `ExtraRoots` registration): it may allocate, re-enter arbitrary JS, and
    /// invalidate the caller's body — the entry anchor keeps the mapping alive.
    /// Register windows live in the native activation extent, so `caller_regs`
    /// stays valid across the nested dispatch; the callee and argument handles
    /// are read from the traced window and handed straight to the synchronous
    /// construct, which roots them at every allocation.
    ///
    /// # Safety-adjacent contract
    /// `caller_regs` is the caller's live register window (`JitCtx.regs`);
    /// compiled code guarantees the destination/callee/argument registers are
    /// in bounds for that window.
    #[allow(clippy::not_unsafe_ptr_arg_deref)]
    pub fn jit_runtime_construct_in_place(
        &mut self,
        context: &ExecutionContext,
        stack: &mut ActivationStack,
        dst_reg: u16,
        callee_reg: u16,
        arg_regs: &[u16],
        caller_regs: *mut Value,
        inherited_new_target: Option<Value>,
        caller_function_id: u32,
        call_pc: u32,
    ) -> Result<bool, VmError> {
        self.record_jit_runtime_stub_class(native_abi::RuntimeStubClass::Reentrant);
        self.jit_runtime_stats.runtime_constructs =
            self.jit_runtime_stats.runtime_constructs.saturating_add(1);
        // SAFETY: `callee_reg` is a compiler-emitted index into the caller window.
        let callee = unsafe { *caller_regs.add(callee_reg as usize) };
        // The interpreter's `Op::New` throws `NotCallable` for a non-constructor
        // callee; leave that error to the exact side exit.
        if !crate::interp::helpers::is_constructor_runtime(&callee, context, &self.gc_heap) {
            return Ok(false);
        }
        let callable = callee
            .as_class_constructor()
            .map(|class| class.ctor(&self.gc_heap))
            .unwrap_or(callee);
        let target_function_id = callable.as_function().or_else(|| {
            callable
                .as_closure(&self.gc_heap)
                .map(|closure| closure.function_id())
        });
        if let (Some(caller), Some(target_function_id)) = (
            context.exec_function(caller_function_id),
            target_function_id,
        ) {
            let transition = self.record_ordinary_call_feedback(
                caller,
                call_pc,
                crate::feedback::OrdinaryCallTarget::Bytecode(target_function_id),
            );
            if transition.evict_for_reopt() {
                self.evict_compiled_for_reopt(caller_function_id);
            }
        }
        let mut args: SmallVec<[Value; 8]> = SmallVec::with_capacity(arg_regs.len());
        for &arg in arg_regs {
            // SAFETY: compiler-emitted argument indices into the caller window.
            args.push(unsafe { *caller_regs.add(arg as usize) });
        }
        let mut callee = callee;
        let mut new_target = inherited_new_target.unwrap_or(callee);
        self.observe_class_constructor_field_transitions_rooted(
            context,
            &mut callee,
            &mut new_target,
            &mut args,
        )?;
        let origin = if inherited_new_target.is_some() {
            self.jit_innermost_native_frame() as u64
        } else {
            0
        };
        let result =
            self.run_construct_sync_rooted(stack, context, &callee, new_target, args, origin)?;
        // SAFETY: `dst_reg` is a compiler-emitted index into the caller
        // window; the window slab is pinned, so the pointer survived the
        // nested dispatch.
        unsafe {
            *caller_regs.add(dst_reg as usize) = result;
        }
        Ok(true)
    }

    /// Complete one full loose-equality opcode in place for a compiled
    /// caller whose inline paths (numeric, nullish) did not decide the
    /// comparison. Runs the interpreter's own §7.2.13 IsLooselyEqual —
    /// object-to-primitive coercion may re-enter arbitrary JS under the
    /// caller's published activation — and writes the (optionally negated)
    /// boolean into the destination register.
    ///
    /// # Safety-adjacent contract
    /// `caller_regs` is the caller's live register window (`JitCtx.regs`);
    /// compiled code guarantees the destination/operand registers are in
    /// bounds for that window.
    #[allow(clippy::not_unsafe_ptr_arg_deref)]
    pub fn jit_runtime_loose_equal_in_place(
        &mut self,
        stack: &mut ActivationStack,
        context: &ExecutionContext,
        dst_reg: u16,
        lhs_reg: u16,
        rhs_reg: u16,
        negate: bool,
        caller_regs: *mut Value,
    ) -> Result<(), CommittedValueError> {
        self.record_jit_runtime_stub_class(native_abi::RuntimeStubClass::Reentrant);
        // SAFETY: compiler-emitted operand indices into the caller window.
        let lhs = unsafe { *caller_regs.add(lhs_reg as usize) };
        let rhs = unsafe { *caller_regs.add(rhs_reg as usize) };
        let eq = self.loose_equal_with_context(stack, context, &lhs, &rhs)?;
        // SAFETY: `dst_reg` is a compiler-emitted index into the caller
        // window; the window slab is pinned, so the pointer survived any
        // nested coercion dispatch.
        unsafe {
            *caller_regs.add(dst_reg as usize) = Value::boolean(eq ^ negate);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    use otter_bytecode::{Function, FunctionCodeBuilder, Op, Operand};

    const FAKE_TEMPLATE_MAPPING_BYTES: usize = 4096;

    fn fund_compile_work(
        vm: &mut Interpreter,
        context: &ExecutionContext,
        fid: u32,
        tier: crate::tier_policy::CostedTier,
    ) {
        let decision = vm
            .jit_tier_work_decision(
                context,
                fid,
                tier,
                vm.jit_code_registry.available_code_bytes(),
            )
            .unwrap();
        let source = context.exec_function(fid).unwrap().source_work();
        source.charge(decision.work_target.unwrap().saturating_sub(source.total()));
    }

    fn template_work_target(context: &ExecutionContext, fid: u32) -> u32 {
        template_work_target_after(context, fid, 0)
    }

    /// Source work a Template body needs after a deterministic number of earlier
    /// compiler invocations.
    fn template_work_target_after(
        context: &ExecutionContext,
        fid: u32,
        previous_compile_attempts: u64,
    ) -> u32 {
        let function = context.exec_function(fid).expect("test function");
        u32::try_from(
            crate::tier_policy::TierWorkModel::calibrated().minimum_required_work(
                crate::tier_policy::TierWorkInput {
                    tier: crate::tier_policy::CostedTier::Template,
                    observed_work: 0,
                    bytecode_instructions: u64::try_from(function.code.len()).unwrap_or(u64::MAX),
                    register_count: u64::from(function.register_count),
                    parameter_count: u64::from(function.param_count),
                    available_code_bytes: crate::tier_policy::JIT_CODE_RESOURCE_LIMIT_BYTES,
                    previous_compile_attempts,
                },
            ),
        )
        .expect("fixture source-work target fits u32")
    }

    #[derive(Debug)]
    struct MultiOsrTemplateCode {
        code_object_id: u64,
        function_id: u32,
        osr_entries: Arc<[u32]>,
    }

    impl jit::JitFunctionCode for MultiOsrTemplateCode {
        fn metadata(&self) -> native_abi::CodeObjectMetadata {
            native_abi::CodeObjectMetadata {
                id: self.code_object_id,
                code_block_id: self.function_id,
                entry_offset: 0,
                code_size: FAKE_TEMPLATE_MAPPING_BYTES as u32,
                safepoint_count: 0,
                frame_map_count: 0,
                spill_map_count: 0,
                dependency_count: 0,
            }
        }

        fn code_len(&self) -> usize {
            FAKE_TEMPLATE_MAPPING_BYTES
        }

        fn entry_addr(&self) -> Option<usize> {
            Some(0x10_0000 + self.code_object_id as usize * 16)
        }

        fn osr_entry_addr(&self, logical_pc: u32) -> Option<usize> {
            self.osr_entries
                .binary_search(&logical_pc)
                .ok()
                .map(|_| 0x20_0000 + self.code_object_id as usize * 16 + logical_pc as usize)
        }
    }

    #[derive(Debug)]
    struct CountingTemplateHook {
        requests: Arc<Mutex<Vec<(u64, Option<u32>)>>>,
    }

    fn compiled_template_status(request: jit::JitCompileRequest) -> jit::JitCompileStatus {
        jit::JitCompileStatus::Compiled {
            code: Arc::new(MultiOsrTemplateCode {
                code_object_id: request.code_object_id,
                function_id: request.snapshot.code_block.id,
                osr_entries: Arc::from(request.snapshot.code_block.loop_headers()),
            }),
            artifact: None,
            diagnostics: Box::default(),
            ir_node_count: u64::try_from(request.snapshot.instructions.len()).unwrap_or(u64::MAX),
        }
    }

    impl jit::JitCompilerHook for CountingTemplateHook {
        fn compile_function(
            &self,
            request: jit::JitCompileRequest,
        ) -> Result<jit::JitCompileStatus, jit::JitCompileError> {
            self.requests
                .lock()
                .expect("compile requests")
                .push((request.code_object_id, request.osr_pc));
            Ok(compiled_template_status(request))
        }
    }

    #[derive(Debug)]
    struct DeferredOnceTemplateHook {
        requests: AtomicUsize,
    }

    impl jit::JitCompilerHook for DeferredOnceTemplateHook {
        fn compile_function(
            &self,
            request: jit::JitCompileRequest,
        ) -> Result<jit::JitCompileStatus, jit::JitCompileError> {
            if self.requests.fetch_add(1, Ordering::Relaxed) == 0 {
                Ok(jit::JitCompileStatus::Unavailable)
            } else {
                Ok(compiled_template_status(request))
            }
        }
    }

    #[derive(Debug)]
    struct UnsupportedTemplateHook {
        requests: AtomicUsize,
    }

    impl jit::JitCompilerHook for UnsupportedTemplateHook {
        fn compile_function(
            &self,
            _request: jit::JitCompileRequest,
        ) -> Result<jit::JitCompileStatus, jit::JitCompileError> {
            self.requests.fetch_add(1, Ordering::Relaxed);
            Ok(jit::JitCompileStatus::Unsupported {
                reason: "fixture is structurally unsupported".to_string(),
            })
        }
    }

    fn installed_template(outcome: TemplateCompileOutcome) -> Arc<dyn jit::JitFunctionCode> {
        match outcome {
            TemplateCompileOutcome::Installed(code) => code,
            TemplateCompileOutcome::Unsupported => panic!("Template fixture was unsupported"),
            TemplateCompileOutcome::Deferred => panic!("Template fixture was deferred"),
        }
    }

    fn multi_loop_context(loop_count: usize) -> (ExecutionContext, Vec<u32>) {
        let mut code = FunctionCodeBuilder::new();
        for _ in 0..loop_count {
            code.push(Op::JumpIfFalse, &[Operand::Imm32(2), Operand::Register(0)]);
            code.push(Op::Nop, &[]);
            code.push(Op::Jump, &[Operand::Imm32(-3)]);
        }
        // Exercise a nonempty body as well as header-specific entry ownership.
        for _ in 0..8 {
            code.push(Op::Nop, &[]);
        }
        code.push(Op::ReturnUndefined, &[]);
        let context = ExecutionContext::from_module(
            otter_bytecode::BytecodeModule {
                module: "template-osr-owner-test.js".to_string(),
                template_sites: Vec::new(),
                source_kind: otter_bytecode::SourceKind::JavaScript,
                functions: vec![Function {
                    id: 0,
                    name: "manyLoops".to_string(),
                    locals: 1,
                    code: code.finish(),
                    ..Function::default()
                }],
                constants: Vec::new(),
                module_resolutions: Vec::new(),
                module_inits: Vec::new(),
                function_source: None,
            },
            crate::source_registry::SourceRegistry::default(),
        )
        .expect("valid multi-loop bytecode fixture");
        let osr_entries = context
            .exec_function(0)
            .expect("main code block")
            .loop_headers()
            .to_vec();
        assert_eq!(osr_entries.len(), loop_count);
        (context, osr_entries)
    }

    fn empty_context() -> ExecutionContext {
        ExecutionContext::from_module(
            crate::test_support::minimal_bytecode_module("compiled-entry-transaction-test.js"),
            crate::source_registry::SourceRegistry::default(),
        )
        .expect("valid bytecode fixture")
    }

    #[derive(Debug)]
    struct UnexpectedRetrainingCompile;

    impl jit::JitCompilerHook for UnexpectedRetrainingCompile {
        fn compile_function(
            &self,
            _request: jit::JitCompileRequest,
        ) -> Result<jit::JitCompileStatus, jit::JitCompileError> {
            panic!("retraining must reject compilation before invoking the hook")
        }

        fn optimizing_tier_enabled(&self) -> bool {
            true
        }

        fn compile_optimized_function(
            &self,
            _request: jit::JitCompileRequest,
        ) -> Result<jit::JitCompileStatus, jit::JitCompileError> {
            panic!("retraining must reject optimizing compilation before invoking the hook")
        }
    }

    #[derive(Debug)]
    struct SelectiveInvalidationCode {
        source: MultiOsrTemplateCode,
        tier: native_abi::NativeFrameKind,
        spliced: Box<[u32]>,
        dependencies: Box<[native_abi::CodeDependency]>,
    }

    impl jit::JitFunctionCode for SelectiveInvalidationCode {
        fn metadata(&self) -> native_abi::CodeObjectMetadata {
            let mut metadata = self.source.metadata();
            metadata.dependency_count = self.dependencies.len() as u32;
            metadata
        }
        fn native_frame_kind(&self) -> native_abi::NativeFrameKind {
            self.tier
        }
        fn spliced_functions(&self) -> &[u32] {
            &self.spliced
        }
        fn dependencies(&self) -> &[native_abi::CodeDependency] {
            &self.dependencies
        }
        fn code_len(&self) -> usize {
            self.source.code_len()
        }
        fn entry_addr(&self) -> Option<usize> {
            self.source.entry_addr()
        }
        fn osr_entry_addr(&self, pc: u32) -> Option<usize> {
            self.source.osr_entry_addr(pc)
        }
    }

    fn selective_code(
        id: u64,
        fid: u32,
        tier: native_abi::NativeFrameKind,
        spliced: &[u32],
        dependencies: &[native_abi::CodeDependency],
    ) -> Arc<dyn jit::JitFunctionCode> {
        Arc::new(SelectiveInvalidationCode {
            source: MultiOsrTemplateCode {
                code_object_id: id,
                function_id: fid,
                osr_entries: Arc::from([0]),
            },
            tier,
            spliced: spliced.into(),
            dependencies: dependencies.into(),
        })
    }

    #[derive(Debug)]
    struct SingleHeaderOptimizingHook {
        requests: AtomicUsize,
        decline_replacement: bool,
    }

    impl jit::JitCompilerHook for SingleHeaderOptimizingHook {
        fn optimizing_tier_enabled(&self) -> bool {
            true
        }

        fn compile_function(
            &self,
            _request: jit::JitCompileRequest,
        ) -> Result<jit::JitCompileStatus, jit::JitCompileError> {
            panic!("the single-header fixture requests only optimizing bodies")
        }

        fn compile_optimized_function(
            &self,
            request: jit::JitCompileRequest,
        ) -> Result<jit::JitCompileStatus, jit::JitCompileError> {
            if self.requests.fetch_add(1, Ordering::Relaxed) != 0 && self.decline_replacement {
                return Ok(jit::JitCompileStatus::Unavailable);
            }
            let code = SelectiveInvalidationCode {
                source: MultiOsrTemplateCode {
                    code_object_id: request.code_object_id,
                    function_id: request.snapshot.code_block.id,
                    osr_entries: Arc::from([request.osr_pc.unwrap()]),
                },
                tier: native_abi::NativeFrameKind::Optimizing,
                spliced: Box::default(),
                dependencies: Box::default(),
            };
            Ok(jit::JitCompileStatus::Compiled {
                code: Arc::new(code),
                artifact: None,
                diagnostics: Box::default(),
                ir_node_count: 1,
            })
        }
    }

    #[test]
    fn source_work_pre_hook_osr_deferral_does_not_cache_a_compiler_decline() {
        let (context, headers) = multi_loop_context(1);
        let mut vm = Interpreter::new().expect("fixture interpreter bootstrap");
        let hook = Arc::new(SingleHeaderOptimizingHook {
            requests: AtomicUsize::new(0),
            decline_replacement: false,
        });
        vm.jit_hook = Some(hook.clone());
        assert!(
            vm.resolve_optimized_osr_code(&context, 0, headers[0])
                .is_none()
        );
        assert!(!vm.jit_optimized_declined_epoch.contains_key(&0));
        assert!(!vm.jit_optimized_code.contains_key(&0));
        assert_eq!(hook.requests.load(Ordering::Relaxed), 0);
        assert_eq!(
            vm.optimizing_tier_policy
                .compile_attempts(0, crate::tier_policy::CostedTier::Optimizing,),
            0
        );

        fund_compile_work(
            &mut vm,
            &context,
            0,
            crate::tier_policy::CostedTier::Optimizing,
        );
        assert!(
            vm.resolve_optimized_osr_code(&context, 0, headers[0])
                .is_some()
        );
        assert_eq!(hook.requests.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn optimizing_osr_replacement_owns_only_the_latest_admitted_header_body() {
        let (context, headers) = multi_loop_context(2);
        for decline_replacement in [false, true] {
            let mut vm = Interpreter::new().expect("fixture interpreter bootstrap");
            let hook = Arc::new(SingleHeaderOptimizingHook {
                requests: AtomicUsize::new(0),
                decline_replacement,
            });
            vm.jit_hook = Some(hook.clone());
            fund_compile_work(
                &mut vm,
                &context,
                0,
                crate::tier_policy::CostedTier::Optimizing,
            );
            let old = vm
                .resolve_optimized_osr_code(&context, 0, headers[0])
                .unwrap();
            fund_compile_work(
                &mut vm,
                &context,
                0,
                crate::tier_policy::CostedTier::Optimizing,
            );
            let replacement = vm.resolve_optimized_osr_code(&context, 0, headers[1]);
            assert_eq!(hook.requests.load(Ordering::Relaxed), 2);
            if decline_replacement {
                assert!(replacement.is_none());
                assert!(vm.jit_code_registry.is_current_for_entry(old.as_ref()));
                assert!(Arc::ptr_eq(
                    vm.jit_optimized_code[&0].as_ref().unwrap(),
                    &old
                ));
                let reused = vm
                    .resolve_optimized_osr_code(&context, 0, headers[0])
                    .unwrap();
                assert!(Arc::ptr_eq(&reused, &old));
            } else {
                let replacement = replacement.unwrap();
                assert!(!vm.jit_code_registry.is_current_for_entry(old.as_ref()));
                assert!(
                    vm.jit_code_registry
                        .is_current_for_entry(replacement.as_ref())
                );
                assert!(Arc::ptr_eq(
                    vm.jit_optimized_code[&0].as_ref().unwrap(),
                    &replacement
                ));
                let installed: Vec<_> = vm
                    .jit_code_generation_snapshot()
                    .into_iter()
                    .filter(|code| code.lifecycle == native_abi::CodeLifetimeState::Installed)
                    .map(|code| code.code_object_id)
                    .collect();
                assert_eq!(installed, [replacement.metadata().id]);
                drop(old);
                assert_eq!(vm.jit_code_registry.retire_unreferenced(), 1);
                assert_eq!(vm.jit_code_residency().unique_code_objects, 1);
            }
        }
    }

    #[test]
    fn source_work_exact_resource_refusal_waits_for_real_headroom_without_new_work() {
        let (context, headers) = multi_loop_context(1);
        let mut vm = Interpreter::new().expect("fixture interpreter bootstrap");
        vm.set_jit_debug_request(jit_debug::JitDebugRequest::events());
        let requests = Arc::new(Mutex::new(Vec::new()));
        vm.jit_hook = Some(Arc::new(CountingTemplateHook {
            requests: requests.clone(),
        }));
        let account = otter_resource::ResourceAccount::new(
            otter_resource::ResourceLimits::builder()
                .limit(otter_resource::ResourceClass::GeneratedCodeBytes, 8192)
                .build(),
        );
        vm.set_resource_account(account.clone()).unwrap();
        let occupied = account
            .reserve_exact(otter_resource::ResourceClass::GeneratedCodeBytes, 6144)
            .unwrap();
        let source = context.exec_function(0).unwrap();
        source.source_work().charge(1_000_000_000);
        let estimated = vm
            .jit_tier_work_decision(
                &context,
                0,
                crate::tier_policy::CostedTier::Template,
                vm.jit_code_registry.available_code_bytes(),
            )
            .unwrap();
        assert!(estimated.should_compile());
        assert!(estimated.estimated_code_bytes < vm.jit_code_registry.available_code_bytes());
        assert!(vm.resolve_jit_code_for_fid(&context, 0).is_none());
        assert_eq!(requests.lock().unwrap().len(), 1);
        assert!(!vm.jit_code.contains_key(&0));
        let blocked = vm
            .jit_tier_work_decision(
                &context,
                0,
                crate::tier_policy::CostedTier::Template,
                vm.jit_code_registry.available_code_bytes(),
            )
            .unwrap();
        assert!(blocked.resource_blocked());
        assert_eq!(blocked.work_target, None);
        for _ in 0..16 {
            assert!(vm.resolve_jit_code_for_fid(&context, 0).is_none());
            assert!(matches!(
                vm.resolve_template_osr_code(&context, 0, headers[0]),
                TemplateCompileOutcome::Deferred
            ));
        }
        assert_eq!(requests.lock().unwrap().len(), 1);
        assert_eq!(
            vm.optimizing_tier_policy
                .compile_attempts(0, crate::tier_policy::CostedTier::Template),
            1
        );
        let same_work = source.source_work().total();
        drop(occupied);
        let admitted = vm.resolve_jit_code_for_fid(&context, 0).unwrap();
        assert!(vm.jit_code_registry.is_current_for_entry(admitted.as_ref()));
        assert_eq!(requests.lock().unwrap().len(), 2);
        assert_eq!(source.source_work().total(), same_work);
        let report = vm.take_jit_debug_report().unwrap();
        assert!(!report.truncated());
        let declines: Vec<_> = report
            .events()
            .iter()
            .filter_map(|event| match event {
                jit_debug::JitDebugEvent::InstallDeclined {
                    function_id,
                    code_object_id,
                    tier,
                    reason,
                } => Some((*function_id, *code_object_id, *tier, *reason)),
                _ => None,
            })
            .collect();
        assert_eq!(declines.len(), 1, "post-hook refusal is recorded once");
        assert_eq!(declines[0].0, 0);
        assert_eq!(declines[0].1, requests.lock().unwrap()[0].0);
        assert_eq!(declines[0].2, jit_debug::JitDebugTier::Template);
        assert_eq!(
            declines[0].3,
            jit_debug::JitInstallDeclineReason::ResourceBudget {
                required_bytes: FAKE_TEMPLATE_MAPPING_BYTES as u64
                    + std::mem::size_of::<native_abi::CodeEntryCell>() as u64,
                available_bytes: 2048,
            }
        );
    }

    #[test]
    fn spliced_tier_invalidation_preserves_the_other_current_owner_without_recompiling() {
        let (context, _) = multi_loop_context(1);
        let mut vm = Interpreter::new().expect("fixture interpreter bootstrap");
        let source_block = crate::executable::CodeBlock::jit_test_stub(7, 0, 1, &[], &[]);
        let source = selective_code(41, 7, native_abi::NativeFrameKind::Baseline, &[], &[]);
        let template = selective_code(42, 0, native_abi::NativeFrameKind::Baseline, &[7], &[]);
        let optimizing = selective_code(43, 0, native_abi::NativeFrameKind::Optimizing, &[], &[]);
        assert!(
            vm.jit_code_registry
                .install_compiled(41, source.clone(), &source_block, None, Box::new([]),)
                .is_ok()
        );
        for code in [&template, &optimizing] {
            assert!(
                vm.jit_code_registry
                    .install_compiled(
                        code.metadata().id,
                        code.clone(),
                        context.exec_function(0).unwrap(),
                        None,
                        Box::new([]),
                    )
                    .is_ok()
            );
        }
        vm.jit_code.insert(7, Some(source));
        vm.jit_code.insert(0, Some(template));
        vm.jit_optimized_code.insert(0, Some(optimizing.clone()));
        vm.jit_entry_osr_only.insert(0);
        vm.jit_template_osr_fids.insert(0);
        vm.jit_entry_bail_counts.insert(0, 3);
        vm.invalidate_jit_function(7);
        assert!(!vm.jit_code.contains_key(&7));
        assert!(!vm.jit_code.contains_key(&0));
        assert!(Arc::ptr_eq(
            vm.jit_optimized_code[&0].as_ref().unwrap(),
            &optimizing
        ));
        assert!(!vm.jit_entry_osr_only.contains(&0));
        assert!(!vm.jit_template_osr_fids.contains(&0));
        assert!(!vm.jit_entry_bail_counts.contains_key(&0));
        assert_eq!(vm.jit_code_residency().unique_code_objects, 1);
        vm.jit_hook = Some(Arc::new(UnexpectedRetrainingCompile));
        let reused = vm.resolve_optimized_code_for_fid(&context, 0).unwrap();
        assert!(
            Arc::ptr_eq(&reused, &optimizing),
            "the registry's valid generation remains the VM owner"
        );
        let installed: Vec<_> = vm
            .jit_code_generation_snapshot()
            .into_iter()
            .filter(|code| code.lifecycle == native_abi::CodeLifetimeState::Installed)
            .map(|code| code.code_object_id)
            .collect();
        assert_eq!(
            installed,
            [43],
            "no orphaned current generation or duplicate install"
        );
    }

    #[test]
    fn protector_tier_invalidation_preserves_baseline_and_its_loop_metadata() {
        let (context, _) = multi_loop_context(1);
        let mut vm = Interpreter::new().expect("fixture interpreter bootstrap");
        let identity = native_abi::ARRAY_INDEX_ACCESSOR_PROTECTOR_IDENTITY;
        let dependency = native_abi::CodeDependency::epoch(
            native_abi::CodeDependencyKind::Protector,
            identity,
            0,
        );
        let template = selective_code(51, 0, native_abi::NativeFrameKind::Baseline, &[], &[]);
        let optimizing = selective_code(
            52,
            0,
            native_abi::NativeFrameKind::Optimizing,
            &[],
            &[dependency],
        );
        for code in [&template, &optimizing] {
            assert!(
                vm.jit_code_registry
                    .install_compiled(
                        code.metadata().id,
                        code.clone(),
                        context.exec_function(0).unwrap(),
                        None,
                        Box::new([]),
                    )
                    .is_ok()
            );
        }
        vm.jit_code.insert(0, Some(template.clone()));
        vm.jit_optimized_code.insert(0, Some(optimizing));
        vm.jit_template_osr_fids.insert(0);
        vm.jit_entry_bail_counts.insert(0, 3);
        let affected = vm.jit_code_registry.invalidate_dependents(
            native_abi::CodeDependencyKind::Protector,
            identity,
            1,
        );
        vm.discard_invalidated_jit_state(&affected);
        assert_eq!(affected, [0]);
        assert!(!vm.jit_optimized_code.contains_key(&0));
        assert!(Arc::ptr_eq(vm.jit_code[&0].as_ref().unwrap(), &template));
        assert!(vm.jit_template_osr_fids.contains(&0));
        assert_eq!(vm.jit_entry_bail_counts[&0], 3);
        vm.jit_hook = Some(Arc::new(UnexpectedRetrainingCompile));
        let reused = vm.resolve_jit_code_for_fid(&context, 0).unwrap();
        assert!(Arc::ptr_eq(&reused, &template));
        let installed: Vec<_> = vm
            .jit_code_generation_snapshot()
            .into_iter()
            .filter(|code| code.lifecycle == native_abi::CodeLifetimeState::Installed)
            .map(|code| code.code_object_id)
            .collect();
        assert_eq!(
            installed,
            [51],
            "the unaffected baseline retains its current publication"
        );
        assert_eq!(vm.jit_code_residency().unique_code_objects, 1);
    }

    #[test]
    fn retraining_blocks_cached_entry_osr_generated_promotion_and_compile_primitives() {
        let (context, headers) = multi_loop_context(2);
        let mut vm = Interpreter::new().expect("fixture interpreter bootstrap");
        vm.jit_hook = Some(Arc::new(CountingTemplateHook {
            requests: Arc::new(Mutex::new(Vec::new())),
        }));
        fund_compile_work(
            &mut vm,
            &context,
            0,
            crate::tier_policy::CostedTier::Template,
        );
        let cached = installed_template(vm.resolve_template_osr_code(&context, 0, headers[0]));
        vm.jit_code_cache = Some((0, cached.clone()));
        vm.jit_optimized_code_cache = Some((0, cached));
        context
            .exec_function(0)
            .unwrap()
            .source_work()
            .charge(1_000_000);
        // Deliberately leave caches installed: every selector must consult the
        // same owner even independently of normal invalidation cache cleanup.
        vm.optimizing_tier_policy.begin_retraining(0, 2);
        vm.jit_hook = Some(Arc::new(UnexpectedRetrainingCompile));
        assert!(vm.resolve_jit_code_for_fid(&context, 0).is_none());
        assert!(vm.resolve_optimized_code_for_fid(&context, 0).is_none());
        assert!(matches!(
            vm.resolve_template_osr_code(&context, 0, headers[0]),
            TemplateCompileOutcome::Deferred
        ));
        assert!(
            vm.resolve_optimized_osr_code(&context, 0, headers[0])
                .is_none()
        );
        assert!(matches!(
            vm.compile_jit_function(&context, 0, None),
            TemplateCompileOutcome::Deferred
        ));
        assert!(
            vm.compile_optimized_jit_function(&context, 0, Some(headers[0]))
                .is_none()
        );
        assert!(!vm.baseline_backedges_reach_osr(&context, 0, headers[0], u64::MAX));
        let mut stack = crate::test_support::FrameChainFixture::new();
        let mut frame = vm.test_frame_for_function(&Function::default()).unwrap();
        frame.pc = headers[0];
        stack.push(frame);
        assert!(
            vm.prepare_osr(&mut stack, &context, 0, true)
                .unwrap()
                .is_none()
        );
        assert!(
            vm.note_backedge_and_maybe_osr(&mut stack, &context, 0)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn inline_exit_history_counts_only_the_owning_source_site() {
        let context = empty_context();
        let mut vm = Interpreter::new().expect("fixture interpreter bootstrap");
        let exit = SideExit::new(
            37,
            native_abi::ExitReason::ShapeGuard,
            native_abi::ExitAction::Recompile,
        );
        for fid in [99, 100] {
            vm.jit_code_registry
                .register(
                    u64::from(fid),
                    selective_code(
                        u64::from(fid),
                        fid,
                        native_abi::NativeFrameKind::Optimizing,
                        &[0],
                        &[],
                    ),
                )
                .unwrap();
        }
        vm.note_jit_optimized_bail_at(&context, 99, 99, exit, (0, 0), false);
        vm.note_jit_optimized_bail_at(&context, 100, 100, exit, (0, 0), false);
        assert_eq!(vm.jit_optimized_exit_profiles.len(), 1);
        assert_eq!(
            vm.jit_optimized_exit_profiles[&(0, 0, exit.reason())].count,
            2,
            "two different inlining callers share the source site's lifetime history"
        );
        assert!(vm.jit_retraining_blocks(0));
        assert!(vm.jit_retraining_blocks(99));
        assert!(
            !vm.jit_retraining_blocks(100),
            "already-invalid sibling does not charge a second outer retraining"
        );
    }

    #[test]
    fn stale_generation_shape_exit_preserves_the_installed_replacement() {
        let context = empty_context();
        let mut vm = Interpreter::new().expect("fixture interpreter bootstrap");
        let code = |id| selective_code(id, 0, native_abi::NativeFrameKind::Optimizing, &[], &[]);
        vm.jit_code_registry.register(11, code(11)).unwrap();
        vm.jit_code_registry.invalidate_function(0);
        vm.jit_code_registry.register(12, code(12)).unwrap();
        let exit = SideExit::new(
            0,
            native_abi::ExitReason::ShapeGuard,
            native_abi::ExitAction::Recompile,
        );
        vm.note_jit_optimized_bail(&context, 0, 11, exit, false);
        assert!(vm.jit_code_registry.is_current_generation(12));
        assert!(!vm.jit_retraining_blocks(0));
        assert_eq!(
            vm.jit_optimized_exit_profiles[&(0, 0, exit.reason())].count,
            1
        );
        assert_eq!(vm.jit_runtime_stats.optimized_deopts, 1);
    }

    #[test]
    fn stale_generation_completion_does_not_disable_replacement_osr_or_charge_template_bail() {
        let (context, headers) = multi_loop_context(1);
        assert!(Interpreter::osr_bail_inside_target_loop(
            &context, 0, headers[0], 1,
        ));
        for tier in [
            native_abi::NativeFrameKind::Baseline,
            native_abi::NativeFrameKind::Optimizing,
        ] {
            for osr_origin in [None, Some(headers[0])] {
                for superseded in [false, true] {
                    let mut vm = Interpreter::new().expect("fixture interpreter bootstrap");
                    vm.set_jit_debug_request(jit_debug::JitDebugRequest::events());
                    vm.jit_code_registry
                        .register(11, selective_code(11, 0, tier, &[], &[]))
                        .unwrap();
                    let current_id = if superseded { 12 } else { 11 };
                    let current = selective_code(current_id, 0, tier, &[], &[]);
                    if superseded {
                        vm.jit_code_registry.invalidate_function(0);
                        vm.jit_code_registry
                            .register(current_id, current.clone())
                            .unwrap();
                    }
                    match tier {
                        native_abi::NativeFrameKind::Baseline => {
                            vm.jit_code.insert(0, Some(current));
                        }
                        native_abi::NativeFrameKind::Optimizing => {
                            vm.jit_optimized_code.insert(0, Some(current));
                        }
                        _ => unreachable!(),
                    }
                    let mut stack = crate::test_support::FrameChainFixture::new();
                    let mut frame = vm
                        .test_frame_for_function(&Function {
                            locals: 1,
                            ..Function::default()
                        })
                        .unwrap();
                    frame.pc = 1;
                    assert!(frame.enter_compiled(tier));
                    frame.code_object_id = 11;
                    vm.frame_ensure_cold(&mut frame).osr_origin = osr_origin;
                    stack.push(frame);
                    let exit = SideExit::new(
                        1,
                        native_abi::ExitReason::ShapeGuard,
                        if superseded {
                            native_abi::ExitAction::Recompile
                        } else {
                            native_abi::ExitAction::Resume
                        },
                    );
                    assert_eq!(
                        vm.complete_compiled_entry(
                            &mut stack,
                            &context,
                            ActivationFloor::ROOT,
                            NativeResultPair::side_exit(exit),
                        )
                        .unwrap(),
                        None,
                    );
                    assert!(vm.jit_code_registry.is_current_generation(current_id));
                    assert_eq!(
                        vm.jit_osr_disabled.contains(&(0, headers[0])),
                        !superseded && osr_origin.is_some()
                    );
                    assert_eq!(
                        vm.jit_entry_bail_counts.get(&0).copied(),
                        (!superseded
                            && osr_origin.is_none()
                            && tier == native_abi::NativeFrameKind::Baseline)
                            .then_some(1)
                    );
                    assert!(!vm.jit_retraining_blocks(0));
                    assert_eq!(
                        stack[0].header.kind,
                        native_abi::NativeFrameKind::Interpreter
                    );
                    assert_eq!(stack[0].code_object_id, 0);
                    let report = vm.take_jit_debug_report().unwrap();
                    assert!(
                        report.events().iter().any(|event| matches!(
                            event,
                            jit_debug::JitDebugEvent::Bail {
                                function_id: 0,
                                resume_pc: 1,
                                ..
                            }
                        )),
                        "stale exits still report the actual completion"
                    );
                    if tier == native_abi::NativeFrameKind::Optimizing {
                        assert_eq!(
                            vm.jit_optimized_exit_profiles[&(0, 1, exit.reason())].count,
                            1
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn stale_generation_new_parameter_learning_retires_current_narrow_code() {
        let mut module = crate::test_support::minimal_bytecode_module("stale-parameter-guard.js");
        module.functions[0].param_count = 1;
        let context = ExecutionContext::from_module(
            module,
            crate::source_registry::SourceRegistry::default(),
        )
        .unwrap();
        let mut vm = Interpreter::new().expect("fixture interpreter bootstrap");
        vm.jit_code_registry
            .register(
                11,
                selective_code(11, 0, native_abi::NativeFrameKind::Optimizing, &[], &[]),
            )
            .unwrap();
        vm.jit_code_registry.invalidate_function(0);
        vm.jit_code_registry
            .register(
                12,
                selective_code(12, 0, native_abi::NativeFrameKind::Optimizing, &[], &[]),
            )
            .unwrap();
        let exit = SideExit::new(
            0,
            native_abi::ExitReason::TypeMismatch,
            native_abi::ExitAction::Recompile,
        );
        let widened = vm.widen_exited_parameters(0, exit, &[Value::number_f64(0.5)]);
        assert!(widened);
        vm.note_jit_optimized_bail(&context, 0, 11, exit, widened);
        assert!(!vm.jit_code_registry.is_current_generation(12));
        assert!(vm.jit_retraining_blocks(0));
        assert!(!vm.widen_exited_parameters(0, exit, &[Value::number_f64(0.5)]));
    }

    #[test]
    fn stale_inline_generation_new_arithmetic_learning_retires_only_source_splices() {
        let mut module = crate::test_support::minimal_bytecode_module("stale-inline-arithmetic.js");
        module.functions[0].param_count = 1;
        module.functions[0].locals = 2;
        let mut code = FunctionCodeBuilder::new();
        code.push(
            Op::Add,
            &[
                Operand::Register(1),
                Operand::Register(0),
                Operand::Register(0),
            ],
        );
        code.push(Op::ReturnValue, &[Operand::Register(1)]);
        module.functions[0].code = code.finish();
        let context = ExecutionContext::from_module(
            module,
            crate::source_registry::SourceRegistry::default(),
        )
        .unwrap();
        let mut vm = Interpreter::new().expect("fixture interpreter bootstrap");
        for (id, fid, splices) in [(11, 99, &[0][..]), (12, 0, &[][..])] {
            vm.jit_code_registry
                .register(
                    id,
                    selective_code(
                        id,
                        fid,
                        native_abi::NativeFrameKind::Optimizing,
                        splices,
                        &[],
                    ),
                )
                .unwrap();
        }
        vm.jit_code_registry.invalidate_function(99);
        // The newer outer calls source 0 through its stable cell; another
        // installed caller really contains source 0's old narrow body.
        vm.jit_code_registry
            .register(
                13,
                selective_code(13, 99, native_abi::NativeFrameKind::Optimizing, &[], &[]),
            )
            .unwrap();
        vm.jit_code_registry
            .register(
                14,
                selective_code(14, 100, native_abi::NativeFrameKind::Optimizing, &[0], &[]),
            )
            .unwrap();
        let exit = SideExit::new(
            37,
            native_abi::ExitReason::Int32Overflow,
            native_abi::ExitAction::Recompile,
        );
        vm.note_jit_optimized_bail_at(&context, 99, 11, exit, (0, 0), false);
        assert!(vm.jit_code_registry.is_current_generation(13));
        assert!(!vm.jit_code_registry.is_current_generation(12));
        assert!(!vm.jit_code_registry.is_current_generation(14));
        assert!(vm.jit_retraining_blocks(0));
        assert!(!vm.jit_retraining_blocks(99));
        assert_eq!(
            vm.jit_optimized_exit_profiles[&(0, 0, exit.reason())].count,
            1
        );
    }

    #[test]
    fn template_osr_reuses_one_function_body_for_every_loop_header() {
        let (context, osr_entries) = multi_loop_context(8);
        let requests = Arc::new(Mutex::new(Vec::new()));
        let hook = Arc::new(CountingTemplateHook {
            requests: Arc::clone(&requests),
        });
        let mut vm = Interpreter::new().expect("fixture interpreter bootstrap");
        vm.jit_hook = Some(hook);

        fund_compile_work(
            &mut vm,
            &context,
            0,
            crate::tier_policy::CostedTier::Template,
        );
        let first = installed_template(vm.resolve_template_osr_code(&context, 0, osr_entries[0]));
        for &osr_pc in &osr_entries[1..] {
            let reused = installed_template(vm.resolve_template_osr_code(&context, 0, osr_pc));
            assert!(Arc::ptr_eq(&first, &reused));
        }

        let expected_request = [(1, Some(osr_entries[0]))];
        assert_eq!(
            requests.lock().expect("compile requests").as_slice(),
            &expected_request
        );
        assert_eq!(vm.jit_next_code_object_id, 2);
        assert_eq!(vm.jit_code.len(), 1);
        assert_eq!(vm.jit_template_osr_fids.len(), 1);

        let residency = vm.jit_code_residency();
        assert_eq!(residency.installed_entry_bodies, 1);
        assert_eq!(residency.installed_osr_bodies, 1);
        assert_eq!(residency.unique_code_objects, 1);
        assert_eq!(residency.code_bytes, FAKE_TEMPLATE_MAPPING_BYTES as u64);

        let generations = vm.jit_code_generation_snapshot();
        assert_eq!(generations.len(), 1);
        assert_eq!(generations[0].code_object_id, 1);
        assert_eq!(generations[0].function_id, 0);
        assert_eq!(
            generations[0].lifecycle,
            native_abi::CodeLifetimeState::Installed
        );

        for &osr_pc in &osr_entries {
            assert_eq!(
                first.osr_entry_addr(osr_pc),
                Some(0x20_0000 + 16 + osr_pc as usize)
            );
        }
    }

    #[test]
    fn template_entry_and_osr_share_the_same_canonical_body() {
        let (context, osr_entries) = multi_loop_context(2);

        let entry_first_requests = Arc::new(Mutex::new(Vec::new()));
        let mut entry_first = Interpreter::new().expect("fixture interpreter bootstrap");
        entry_first.jit_hook = Some(Arc::new(CountingTemplateHook {
            requests: Arc::clone(&entry_first_requests),
        }));
        context
            .exec_function(0)
            .unwrap()
            .source_work()
            .charge(u64::from(template_work_target(&context, 0)));
        let entry_body = entry_first
            .resolve_jit_code_for_fid(&context, 0)
            .expect("entry threshold compiles Template body");
        let osr_body =
            installed_template(entry_first.resolve_template_osr_code(&context, 0, osr_entries[0]));
        assert!(Arc::ptr_eq(&entry_body, &osr_body));
        assert_eq!(
            entry_first_requests
                .lock()
                .expect("entry-first requests")
                .as_slice(),
            &[(1, None)]
        );

        let osr_first_requests = Arc::new(Mutex::new(Vec::new()));
        let mut osr_first = Interpreter::new().expect("fixture interpreter bootstrap");
        osr_first.jit_hook = Some(Arc::new(CountingTemplateHook {
            requests: Arc::clone(&osr_first_requests),
        }));
        let osr_body =
            installed_template(osr_first.resolve_template_osr_code(&context, 0, osr_entries[0]));
        let entry_body = osr_first
            .resolve_jit_code_for_fid(&context, 0)
            .expect("entry reuses entry-capable OSR body");
        assert!(Arc::ptr_eq(&entry_body, &osr_body));
        assert_eq!(
            osr_first_requests
                .lock()
                .expect("OSR-first requests")
                .as_slice(),
            &[(1, Some(osr_entries[0]))]
        );
        let residency = osr_first.jit_code_residency();
        assert_eq!(residency.installed_entry_bodies, 1);
        assert_eq!(residency.installed_osr_bodies, 1);
        assert_eq!(residency.unique_code_objects, 1);
    }

    #[test]
    fn template_invalidation_replaces_the_single_shared_generation() {
        let (context, osr_entries) = multi_loop_context(2);
        let requests = Arc::new(Mutex::new(Vec::new()));
        let mut vm = Interpreter::new().expect("fixture interpreter bootstrap");
        vm.jit_hook = Some(Arc::new(CountingTemplateHook {
            requests: Arc::clone(&requests),
        }));

        fund_compile_work(
            &mut vm,
            &context,
            0,
            crate::tier_policy::CostedTier::Template,
        );
        let first = installed_template(vm.resolve_template_osr_code(&context, 0, osr_entries[0]));
        vm.invalidate_jit_function(0);
        assert!(vm.jit_code.is_empty());
        assert!(vm.jit_template_osr_fids.is_empty());
        assert_eq!(vm.jit_code_residency().unique_code_objects, 0);
        let invalidated = vm.jit_code_generation_snapshot();
        assert_eq!(invalidated.len(), 1);
        assert_eq!(
            invalidated[0].lifecycle,
            native_abi::CodeLifetimeState::Invalid
        );

        fund_compile_work(
            &mut vm,
            &context,
            0,
            crate::tier_policy::CostedTier::Template,
        );
        let replacement =
            installed_template(vm.resolve_template_osr_code(&context, 0, osr_entries[1]));
        assert!(!Arc::ptr_eq(&first, &replacement));
        assert_eq!(vm.jit_code_residency().unique_code_objects, 1);
        assert_eq!(
            requests.lock().expect("compile requests").as_slice(),
            &[(1, Some(osr_entries[0])), (2, Some(osr_entries[1]))]
        );
        let generations = vm.jit_code_generation_snapshot();
        assert_eq!(generations.len(), 2);
        assert_eq!(
            generations[0].lifecycle,
            native_abi::CodeLifetimeState::Invalid
        );
        assert_eq!(
            generations[1].lifecycle,
            native_abi::CodeLifetimeState::Installed
        );
    }

    #[test]
    fn transient_template_failure_retries_after_deterministic_execution_budget() {
        let (context, _) = multi_loop_context(1);
        let hook = Arc::new(DeferredOnceTemplateHook {
            requests: AtomicUsize::new(0),
        });
        let mut vm = Interpreter::new().expect("fixture interpreter bootstrap");
        vm.jit_hook = Some(hook.clone());
        context
            .exec_function(0)
            .unwrap()
            .source_work()
            .charge(u64::from(template_work_target(&context, 0)));

        assert!(vm.resolve_jit_code_for_fid(&context, 0).is_none());
        assert!(!vm.jit_code.contains_key(&0));
        assert_eq!(hook.requests.load(Ordering::Relaxed), 1);
        assert!(
            vm.resolve_jit_code_for_fid(&context, 0).is_none(),
            "a failed compiler invocation must require new execution evidence"
        );
        let function = context.exec_function(0).unwrap();
        function.source_work().charge(
            u64::from(template_work_target_after(&context, 0, 1))
                .saturating_sub(function.source_work().total()),
        );
        assert!(
            vm.resolve_jit_code_for_fid(&context, 0).is_some(),
            "new execution evidence repays the failure"
        );
        assert_eq!(hook.requests.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn permanent_template_failure_is_cached_for_the_whole_function() {
        let (context, osr_entries) = multi_loop_context(2);
        let hook = Arc::new(UnsupportedTemplateHook {
            requests: AtomicUsize::new(0),
        });
        let mut vm = Interpreter::new().expect("fixture interpreter bootstrap");
        vm.jit_hook = Some(hook.clone());

        fund_compile_work(
            &mut vm,
            &context,
            0,
            crate::tier_policy::CostedTier::Template,
        );
        assert!(matches!(
            vm.resolve_template_osr_code(&context, 0, osr_entries[0]),
            TemplateCompileOutcome::Unsupported
        ));
        assert!(matches!(vm.jit_code.get(&0), Some(None)));
        assert!(vm.jit_osr_disabled.contains(&(0, u32::MAX)));
        assert!(matches!(
            vm.resolve_template_osr_code(&context, 0, osr_entries[1]),
            TemplateCompileOutcome::Unsupported
        ));
        assert_eq!(hook.requests.load(Ordering::Relaxed), 1);
    }

    fn assert_moving_payload_is_rewritten(status: NativeResultStatus) {
        let mut vm = Interpreter::new().expect("fixture interpreter bootstrap");
        let object = crate::object::alloc_fixture_object_with_roots(&mut vm.gc_heap, &mut |_| {})
            .expect("young result object");
        let value = Value::object(object);
        let original_bits = value.to_abi_bits();
        let result = match status {
            NativeResultStatus::Success => NativeResultPair::success(value),
            NativeResultStatus::Throw => NativeResultPair::throw_value(value),
            NativeResultStatus::SideExit
            | NativeResultStatus::Continue
            | NativeResultStatus::OutOfMemory
            | NativeResultStatus::Yield
            | NativeResultStatus::Fatal => panic!("fixture requires a boxed compiled result"),
        };

        let rewritten = vm.with_rooted_compiled_result(result, |vm| {
            vm.collect_minor_tracing_runtime_roots();
        });

        assert_eq!(
            rewritten.validate(NativeResultDomain::Compiled),
            Some(status)
        );
        assert_ne!(
            rewritten.payload_bits(),
            original_bits,
            "minor collection must relocate the young result"
        );
        assert!(rewritten.payload_value().as_object().is_some());
    }

    #[test]
    fn compiled_success_payload_is_rewritten_across_moving_collection() {
        assert_moving_payload_is_rewritten(NativeResultStatus::Success);
    }

    #[test]
    fn compiled_throw_payload_is_rewritten_across_moving_collection() {
        assert_moving_payload_is_rewritten(NativeResultStatus::Throw);
    }

    #[test]
    fn compiled_side_exit_is_unchanged_across_collection() {
        let mut vm = Interpreter::new().expect("fixture interpreter bootstrap");
        let result = NativeResultPair::side_exit(crate::native_abi::SideExit::new(
            37,
            crate::native_abi::ExitReason::TypeMismatch,
            crate::native_abi::ExitAction::Recompile,
        ));
        let returned = vm.with_rooted_compiled_result(result, |vm| {
            vm.collect_minor_tracing_runtime_roots();
        });
        assert_eq!(returned, result);
    }

    #[test]
    fn nested_compiled_entry_defers_feedback_without_touching_the_result() {
        let mut vm = Interpreter::new().expect("fixture interpreter bootstrap");
        let context = empty_context();
        let result = NativeResultPair::success(Value::number_i32(41));

        let mut parent = crate::native_abi::Frame::new(
            crate::native_abi::VmFrameHeader::interpreter(0, 0),
            0,
            Value::undefined(),
            Value::undefined(),
        );
        vm.jit_detached_frame = std::ptr::addr_of_mut!(parent) as u64;
        let nested = vm
            .finish_compiled_entry_transaction(Some(&context), result, true)
            .expect("nested completion");
        assert_eq!(nested, result);
        assert!(vm.jit_generated_feedback_pending);

        vm.jit_detached_frame = 0;
        let outer = vm
            .finish_compiled_entry_transaction(Some(&context), result, false)
            .expect("outer completion");
        assert_eq!(outer, result);
        assert!(!vm.jit_generated_feedback_pending);
    }
}
