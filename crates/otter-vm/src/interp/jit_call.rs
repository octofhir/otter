//! JIT entry, OSR, and generated-call frame plumbing.
//!
//! # Contents
//! Tier-up dispatch (`maybe_dispatch_jit`, backedge/OSR accounting),
//! compiled-frame entry (`run_compiled_frame`, `jit_runtime_call`),
//! generated-call feedback through focused `jit_calls` modules, and cold
//! inlined/stack-call side-exit materialization in `jit_calls/deopt`.
//! Call and back-edge accounting also feeds the additive optimizing-tier
//! policy without consulting its decision. Generated-call entry feedback is
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
//! Canonical tier transitions retain one [`NativeFrame`] and register window;
//! materialized [`Frame`] construction is confined to cold deoptimization and
//! interpreter-owned dispatch.
//! Template entry and loop OSR retain one canonical whole-function body per
//! function id and select a header-specific trampoline from that shared object.
//! A nested compiled return never allocates during post-entry bookkeeping: it
//! only leaves feedback pending for the outermost activation. No result root
//! index or token crosses the VM/JIT boundary.
#![allow(unused_imports)]
use super::jit_compile::TemplateCompileOutcome;
use crate::*;
use crate::{
    native_abi::{NativeFrame, NativeResultDomain, NativeResultPair, NativeResultStatus, SideExit},
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
    pub(super) fn jit_tier_cost_decision(
        &self,
        context: &ExecutionContext,
        fid: u32,
        tier: crate::tier_policy::CostedTier,
        trigger: crate::tier_policy::TierTrigger,
        executions: u64,
        exits: u64,
        resident_code_bytes: u64,
    ) -> Option<crate::tier_policy::TierCostDecision> {
        let function = context.exec_function(fid)?;
        Some(crate::tier_policy::TierCostModel::calibrated().decide(
            crate::tier_policy::TierCostInput {
                tier,
                trigger,
                executions,
                exits,
                bytecode_instructions: u64::try_from(function.code.len()).unwrap_or(u64::MAX),
                register_count: u64::from(function.register_count),
                parameter_count: u64::from(function.param_count),
                resident_code_bytes,
                cumulative_compile_ns: self.optimizing_tier_policy.cumulative_compile_ns(fid, tier),
            },
        ))
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
            self.pending_uncaught_frames = Some(snapshot_frames(context, stack));
        }
        let unwind = self.unwind_throw_above(context, stack, floor, thrown);
        if unwind.is_ok() {
            self.pending_uncaught_frames = None;
        }
        unwind
    }

    /// After a call pushed a fresh bytecode callee frame as the new top of
    /// `stack`, try to run it as compiled baseline code instead of interpreting.
    ///
    /// Only invoked when a JIT hook is installed and a frame was actually
    /// pushed (the caller checks `stack` grew). Returns `Ok(None)` to interpret
    /// normally; `Ok(Some(popped))` when the JIT ran and the callee returned,
    /// where `popped` mirrors [`Self::return_running_finally`] (`Some(v)` means
    /// the return unwound the dispatch entry and the loop should yield `v`).
    pub(crate) fn record_jit_bail(
        &mut self,
        context: &ExecutionContext,
        fid: u32,
        tier: jit_debug::JitDebugTier,
        target: jit_debug::JitDebugTarget,
        exit: SideExit,
    ) {
        let pc = exit.logical_pc();
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

    pub(crate) fn maybe_dispatch_jit(
        &mut self,
        stack: &mut ActivationStack,
        context: &ExecutionContext,
        floor: ActivationFloor,
    ) -> Result<Option<Option<Value>>, VmError> {
        let top_idx = stack.len() - 1;
        let (outcome, optimized) =
            if let Some(outcome) = self.run_optimized_frame(stack, context, top_idx) {
                (outcome, true)
            } else {
                let Some(code) = self.resolve_jit_code(stack, context, top_idx) else {
                    return Ok(None);
                };
                (
                    self.run_compiled_frame(stack, context, top_idx, &code),
                    false,
                )
            };
        match outcome {
            jit::JitExecOutcome::Bailed(exit) => {
                let pc = exit.logical_pc();
                stack[top_idx].pc = pc;
                let fid = stack[top_idx].function_id;
                self.record_jit_bail(
                    context,
                    fid,
                    if optimized {
                        jit_debug::JitDebugTier::Optimizing
                    } else {
                        jit_debug::JitDebugTier::Template
                    },
                    jit_debug::JitDebugTarget::Entry,
                    exit,
                );
                // `run_optimized_frame` owns optimizing-bail accounting because
                // callers such as the iterator fast path consume its outcome
                // directly. Do not count the same exit again at this dispatch
                // wrapper.
                if !optimized && !self.reoptimize_arith_overflow_bail(context, fid, exit) {
                    self.note_jit_entry_bail(context, fid);
                }
                Ok(None)
            }
            jit::JitExecOutcome::Returned(value) => {
                if !optimized {
                    self.note_jit_entry_success(stack[top_idx].function_id);
                }
                let popped = self.return_running_finally_above(stack, floor, value)?;
                Ok(Some(popped))
            }
            jit::JitExecOutcome::Throw(thrown) => {
                self.unwind_compiled_throw_above(context, stack, floor, thrown)?;
                if stack.is_at_floor(floor) {
                    Ok(Some(Some(Value::undefined())))
                } else {
                    Ok(None)
                }
            }
            jit::JitExecOutcome::Fatal(err) => Err(err),
        }
    }

    /// Per-back-edge hook: bump the counter for *this loop header* and, on the
    /// iteration where it reaches the OSR threshold, attempt loop tier-up.
    ///
    /// The counter is keyed by `(function_id, loop_header_pc)` so each hot loop
    /// warms up independently — a frequently-back-edging callee can no longer
    /// monopolize a single shared counter and starve a hot script loop that
    /// calls out. The hot path is one hashmap bump; the lookup runs only while a
    /// JIT hook is installed and only until the header tiers up (after which the
    /// loop runs compiled and stops hitting this interpreter hook).
    #[inline]
    pub(crate) fn note_backedge_and_maybe_osr(
        &mut self,
        stack: &mut ActivationStack,
        context: &ExecutionContext,
        top_idx: usize,
        floor: ActivationFloor,
    ) -> Result<Option<Option<Value>>, VmError> {
        // Interpreter-only (no JIT installed): pay nothing beyond this branch.
        if self.jit_hook.is_none() {
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
        let count = {
            let c = self.jit_osr_counts.entry(key).or_insert(0);
            *c = c.saturating_add(1);
            *c
        };
        let loop_span = context
            .exec_function(key.0)
            .and_then(|function| function.loop_latch(key.1).map(|latch| latch - key.1 + 1))
            .map(u64::from)
            .unwrap_or(1);
        let trigger = crate::tier_policy::TierTrigger::LoopBackedge {
            span_instructions: loop_span,
        };
        let optimizing_enabled = self
            .jit_hook
            .as_ref()
            .is_some_and(|hook| hook.optimizing_tier_enabled());
        let optimizing = optimizing_enabled
            && self
                .jit_tier_cost_decision(
                    context,
                    key.0,
                    crate::tier_policy::CostedTier::Optimizing,
                    trigger,
                    u64::from(count),
                    0,
                    0,
                )
                .is_some_and(crate::tier_policy::TierCostDecision::should_compile);
        let template = !optimizing_enabled
            && self
                .jit_tier_cost_decision(
                    context,
                    key.0,
                    crate::tier_policy::CostedTier::Template,
                    trigger,
                    u64::from(count),
                    0,
                    0,
                )
                .is_some_and(crate::tier_policy::TierCostDecision::should_compile);
        if !optimizing && !template {
            return Ok(None);
        }
        let resident_code_bytes = self.jit_code_residency().code_bytes;
        let selected = if optimizing {
            crate::tier_policy::CostedTier::Optimizing
        } else {
            crate::tier_policy::CostedTier::Template
        };
        if !self
            .jit_tier_cost_decision(
                context,
                key.0,
                selected,
                trigger,
                u64::from(count),
                0,
                resident_code_bytes,
            )
            .is_some_and(crate::tier_policy::TierCostDecision::should_compile)
        {
            return Ok(None);
        }
        // Threshold reached: drop this header's counter (it tiers up now or is
        // marked disabled by `maybe_osr`, so it should not keep counting) and
        // attempt OSR.
        self.jit_runtime_stats.osr_attempts = self.jit_runtime_stats.osr_attempts.saturating_add(1);
        self.jit_osr_counts.remove(&key);
        self.maybe_osr(stack, context, top_idx, floor, optimizing)
    }

    /// Loop-OSR tier-up. Called from [`Self::note_backedge_and_maybe_osr`] at
    /// the threshold crossing (the top frame's `pc` is the loop header just
    /// branched to). It prefers whole-body optimizing OSR, then preserves the
    /// template OSR fallback for functions outside the optimizing subset.
    ///
    /// Returns `Ok(None)` to keep interpreting (ineligible, no OSR entry for
    /// this header, or the compiled body bailed); `Ok(Some(popped))` when
    /// compiled code ran the frame to `Return` and unwound the dispatch entry
    /// (mirrors [`Self::maybe_dispatch_jit`]).
    pub(crate) fn maybe_osr(
        &mut self,
        stack: &mut ActivationStack,
        context: &ExecutionContext,
        top_idx: usize,
        floor: ActivationFloor,
        prefer_optimizing: bool,
    ) -> Result<Option<Option<Value>>, VmError> {
        let frame = &stack[top_idx];
        // Only ordinary bytecode frames; async/generator bodies resume through
        // their own machinery and must not be entered mid-loop.
        if self.frame_has_suspension_owner(frame) {
            return Ok(None);
        }
        let fid = frame.function_id;
        // Whole function uncompilable → never retry, never re-arm.
        if self.jit_osr_disabled.contains(&(fid, u32::MAX)) {
            return Ok(None);
        }
        let osr_pc = frame.pc;
        // This specific loop header already proved un-tierable (bailed / no
        // trampoline). The caller re-arms the counter, so a different hot loop
        // in the same function still gets a tier-up shot.
        if self.jit_osr_disabled.contains(&(fid, osr_pc)) {
            return Ok(None);
        }
        // See `run_compiled_frame`: the activation must name the chunk owning
        // the OSR-entered frame, not the caller tick's chunk.
        let resolved = context
            .for_function(fid)
            .map_err(|_| VmError::InvalidOperand)?;
        let activation = jit::VmRuntimeActivation::new(self, stack, &resolved, top_idx);
        let optimized_outcome = prefer_optimizing
            .then(|| self.resolve_optimized_osr_code(context, fid, osr_pc))
            .flatten()
            .filter(|code| self.jit_code_registry.is_current_for_entry(code.as_ref()))
            .and_then(|code| code.run_optimized_osr_entry(activation, osr_pc));
        let (outcome, optimized) = if let Some(outcome) = optimized_outcome {
            self.jit_runtime_stats.optimized_entries =
                self.jit_runtime_stats.optimized_entries.saturating_add(1);
            self.jit_runtime_stats.optimized_osr_entries = self
                .jit_runtime_stats
                .optimized_osr_entries
                .saturating_add(1);
            if let jit::JitExecOutcome::Bailed(exit) = outcome {
                self.note_jit_optimized_bail(context, fid, exit);
            }
            (outcome, true)
        } else {
            // The whole-body optimizer declined this function/header. A
            // Template body already contains the trampolines for every
            // eligible loop header, so one function-owned object serves every
            // OSR target instead of duplicating the whole executable mapping.
            let code = match self.resolve_template_osr_code(context, fid, osr_pc) {
                TemplateCompileOutcome::Installed(code) => code,
                TemplateCompileOutcome::Unsupported | TemplateCompileOutcome::Deferred => {
                    return Ok(None);
                }
            };
            if !self.jit_code_registry.is_current_for_entry(code.as_ref()) {
                return Ok(None);
            }
            let Some(outcome) = code.osr_entry(activation, osr_pc) else {
                self.jit_osr_disabled.insert((fid, osr_pc));
                return Ok(None);
            };
            (outcome, false)
        };
        match outcome {
            jit::JitExecOutcome::Bailed(exit) => {
                let pc = exit.logical_pc();
                // Compiled body hit a guard or unsupported opcode. Resume the
                // interpreter at the exact bail PC (committed side effects are
                // preserved). Disable this loop header only when the miss was in
                // the target loop itself. A compiled OSR slice may finish the hot
                // loop, continue through cold epilogue/outer-loop code, and bail
                // there; that should not permanently suppress the header on the
                // next hot iteration.
                self.record_jit_bail(
                    context,
                    fid,
                    if optimized {
                        jit_debug::JitDebugTier::Optimizing
                    } else {
                        jit_debug::JitDebugTier::Template
                    },
                    jit_debug::JitDebugTarget::Osr { pc: osr_pc },
                    exit,
                );
                stack[top_idx].pc = pc;
                // An optimizing exit already ran its arithmetic repair in
                // `note_jit_optimized_bail`; a template exit repairs here.
                if !optimized && self.reoptimize_arith_overflow_bail(context, fid, exit) {
                    return Ok(None);
                }
                let tier_still_installed =
                    !optimized || matches!(self.jit_optimized_code.get(&fid), Some(Some(_)));
                if tier_still_installed
                    && Self::osr_bail_inside_target_loop(context, fid, osr_pc, pc)
                {
                    self.jit_osr_disabled.insert((fid, osr_pc));
                }
                Ok(None)
            }
            jit::JitExecOutcome::Returned(value) => {
                let popped = self.return_running_finally_above(stack, floor, value)?;
                Ok(Some(popped))
            }
            jit::JitExecOutcome::Throw(thrown) => {
                self.unwind_compiled_throw_above(context, stack, floor, thrown)?;
                if stack.is_at_floor(floor) {
                    Ok(Some(Some(Value::undefined())))
                } else {
                    Ok(None)
                }
            }
            jit::JitExecOutcome::Fatal(err) => Err(err),
        }
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
        self.invalidate_jit_function(fid);
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
        let exits = u64::from(*bails);
        let profitable = self
            .jit_tier_cost_decision(
                context,
                fid,
                crate::tier_policy::CostedTier::Template,
                crate::tier_policy::TierTrigger::FunctionEntry,
                0,
                exits,
                0,
            )
            .is_some_and(crate::tier_policy::TierCostDecision::should_compile);
        if !profitable {
            return;
        }
        let resident_code_bytes = self.jit_code_residency().code_bytes;
        if !self
            .jit_tier_cost_decision(
                context,
                fid,
                crate::tier_policy::CostedTier::Template,
                crate::tier_policy::TierTrigger::FunctionEntry,
                0,
                exits,
                resident_code_bytes,
            )
            .is_some_and(crate::tier_policy::TierCostDecision::should_compile)
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

    /// Record one optimizing-tier deoptimization and self-correct the
    /// speculation that caused it.
    ///
    /// The resume PC joins the function's speculation-failure record, which
    /// the next compile reads back. One exit site bailing repeatedly is a
    /// wrong speculation in a loop: the interpreter has been refining the
    /// site's feedback on every bail, so the optimizing generation — and only
    /// it — is discarded and the next hot entry recompiles against what
    /// execution actually does. Callers that bound the discarded entry
    /// directly drop with it; the function's template generation survives.
    /// When another compile cannot repay itself, the exiting body is discarded
    /// immediately: retaining it would charge a deopt round trip on every
    /// matching entry. The function drops to its template generation and is
    /// A cost-deferred rebuild remains eligible once later executions make it
    /// profitable; only a structural invalidation is cached by feedback epoch.
    pub(crate) fn note_jit_optimized_bail(
        &mut self,
        context: &ExecutionContext,
        fid: u32,
        exit: native_abi::SideExit,
    ) {
        self.note_jit_optimized_bail_at(context, fid, exit, fid, exit.logical_pc());
    }

    /// [`Self::note_jit_optimized_bail`] for an exit whose speculation lives at
    /// `site_fid@site_pc`, the innermost spliced body of an inlined deopt.
    ///
    /// An `Int32Overflow` / `NegativeZero` exit first widens that arithmetic
    /// site and retires the generation, so the next compile uses float
    /// arithmetic there. Every entry kind reaches this one owner; without it
    /// an entry exit would keep the same speculation and exit on every call.
    /// A repeated exit at an already-widened site is charged like any other.
    pub(crate) fn note_jit_optimized_bail_at(
        &mut self,
        context: &ExecutionContext,
        fid: u32,
        exit: native_abi::SideExit,
        site_fid: u32,
        site_pc: u32,
    ) {
        let resume_pc = exit.logical_pc();
        self.jit_runtime_stats.optimized_deopts =
            self.jit_runtime_stats.optimized_deopts.saturating_add(1);
        let profile = self
            .jit_optimized_exit_profiles
            .entry((fid, resume_pc, exit.reason()))
            .or_insert(jit::JitExitProfile {
                action: exit.action(),
                count: 0,
            });
        profile.action = profile.action.max(exit.action());
        profile.count = profile.count.saturating_add(1);
        let action = profile.action;
        if self.widen_arith_exit_site(context, site_fid, site_pc, exit.reason()) {
            self.invalidate_jit_function(fid);
            return;
        }
        if action == native_abi::ExitAction::Resume {
            return;
        }
        if action == native_abi::ExitAction::Invalidate {
            self.abandon_unsupported_optimized_generation(fid);
            return;
        }
        let exits = self
            .jit_optimized_exit_profiles
            .iter()
            .filter(|((profile_fid, _, _), _)| *profile_fid == fid)
            .map(|(_, profile)| u64::from(profile.count))
            .sum::<u64>();
        let profitable = self
            .jit_tier_cost_decision(
                context,
                fid,
                crate::tier_policy::CostedTier::Optimizing,
                crate::tier_policy::TierTrigger::FunctionEntry,
                0,
                exits,
                0,
            )
            .is_some_and(crate::tier_policy::TierCostDecision::should_compile);
        if !profitable {
            self.retire_unprofitable_optimized_generation(fid);
            return;
        }
        for (&(profile_fid, _, _), profile) in &mut self.jit_optimized_exit_profiles {
            if profile_fid == fid {
                profile.count = 0;
            }
        }
        let dependents = match self.jit_optimized_code.get(&fid) {
            Some(Some(code)) => self
                .jit_code_registry
                .invalidate_code_object(code.metadata().id),
            _ => Vec::new(),
        };
        self.jit_optimized_code.remove(&fid);
        self.jit_optimized_declined_epoch.remove(&fid);
        self.jit_optimized_code_cache = None;
        let dependents: Vec<u32> = dependents
            .into_iter()
            .filter(|&dependent| dependent != fid)
            .collect();
        self.discard_invalidated_jit_state(&dependents);
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

    /// Drop an unprofitable optimizing generation without turning a dynamic
    /// cost decision into unsupported-function state. Accumulated compile cost
    /// raises the next break-even point, while later executions can still make
    /// a replacement profitable.
    fn retire_unprofitable_optimized_generation(&mut self, fid: u32) {
        let dependents = match self.jit_optimized_code.get(&fid) {
            Some(Some(code)) => self
                .jit_code_registry
                .invalidate_code_object(code.metadata().id),
            _ => return,
        };
        self.jit_optimized_code.remove(&fid);
        self.jit_optimized_declined_epoch.remove(&fid);
        self.jit_optimized_code_cache = None;
        let dependents: Vec<u32> = dependents
            .into_iter()
            .filter(|&dependent| dependent != fid)
            .collect();
        self.discard_invalidated_jit_state(&dependents);
    }

    /// Remove cache/map ownership for every function whose installed code was
    /// invalidated.
    ///
    /// Hotness and bounded reoptimization history survive so the next entry
    /// can compile immediately instead of warming from zero.
    pub(crate) fn discard_invalidated_jit_state(&mut self, affected: &[u32]) {
        if affected.is_empty() {
            return;
        }
        let affected = affected
            .iter()
            .copied()
            .collect::<rustc_hash::FxHashSet<_>>();
        for &fid in &affected {
            self.jit_code.remove(&fid);
            self.jit_optimized_code.remove(&fid);
            self.jit_entry_osr_only.remove(&fid);
            self.jit_entry_bail_counts.remove(&fid);
            self.jit_optimized_declined_epoch.remove(&fid);
        }
        for (&(profile_fid, _, _), profile) in &mut self.jit_optimized_exit_profiles {
            if affected.contains(&profile_fid) {
                profile.count = 0;
            }
        }
        self.jit_template_osr_fids
            .retain(|fid| !affected.contains(fid));
        self.jit_osr_disabled
            .retain(|(fid, _)| !affected.contains(fid));
        self.jit_osr_counts
            .retain(|(fid, _), _| !affected.contains(fid));
        self.jit_code_cache = None;
        self.jit_optimized_code_cache = None;
    }

    /// Unlink every current native generation for `fid`.
    ///
    /// Stable generated callers are not invalidated: they observe a later
    /// replacement through `fid`'s function entry cell. Map/cache ownership is
    /// removed while hotness survives, so the next entry recompiles immediately.
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

    /// Cold repair for one stable generated-call function cell.
    ///
    /// Publication normally keeps the cell hot and this is never called.
    /// A zero target first republishes an installed fallback generation. If
    /// invalidation removed every generation, the resolver may compile a fresh
    /// baseline while the caller's native frame remains the authoritative root
    /// owner. Failure leaves the caller on its exact pre-effect side exit.
    pub fn jit_resolve_direct_entry(
        &mut self,
        context: &ExecutionContext,
        function_entry_addr: u64,
    ) -> u64 {
        self.jit_runtime_stats.cold_entry_resolver_misses = self
            .jit_runtime_stats
            .cold_entry_resolver_misses
            .saturating_add(1);
        let resolved = self
            .jit_code_registry
            .resolve_function_entry(function_entry_addr);
        if resolved != 0 {
            return resolved;
        }
        let Some(fid) = self
            .jit_code_registry
            .function_id_for_entry_addr(function_entry_addr)
        else {
            return 0;
        };
        if matches!(self.jit_code.get(&fid), Some(None)) {
            return 0;
        }
        self.jit_runtime_stats.compile_attempts =
            self.jit_runtime_stats.compile_attempts.saturating_add(1);
        let outcome = self.compile_jit_function(context, fid, None);
        self.retain_template_compile_outcome(fid, outcome);
        self.jit_code_registry
            .resolve_function_entry(function_entry_addr)
    }

    /// Tier-up entry point for a synchronously-entered call frame (the
    /// [`Self::run_callable_sync`] path), where the callee frame was just
    /// pushed as the sole frame above `floor`. Mirrors
    /// [`Self::maybe_dispatch_jit`] but, on a successful compiled run, the
    /// completion *is* the call result (there is no caller frame to unwind
    /// into). The stack below `floor` may hold frames owned by live compiled
    /// entries — an unhandled throw must never unwind past `floor`.
    ///
    /// Returns `Ok(Some(v))` when compiled code ran the frame to completion, or
    /// `Ok(None)` to interpret it normally.
    pub(crate) fn dispatch_jit_sync_entry(
        &mut self,
        stack: &mut ActivationStack,
        context: &ExecutionContext,
        floor: ActivationFloor,
    ) -> Result<Option<Value>, VmError> {
        if self.jit_hook.is_none() {
            return Ok(None);
        }
        let top_idx = stack.len() - 1;
        let (outcome, optimized) =
            if let Some(outcome) = self.run_optimized_frame(stack, context, top_idx) {
                (outcome, true)
            } else {
                let Some(code) = self.resolve_jit_code(stack, context, top_idx) else {
                    return Ok(None);
                };
                (
                    self.run_compiled_frame(stack, context, top_idx, &code),
                    false,
                )
            };
        match outcome {
            jit::JitExecOutcome::Bailed(exit) => {
                let pc = exit.logical_pc();
                stack[top_idx].pc = pc;
                let fid = stack[top_idx].function_id;
                self.record_jit_bail(
                    context,
                    fid,
                    if optimized {
                        jit_debug::JitDebugTier::Optimizing
                    } else {
                        jit_debug::JitDebugTier::Template
                    },
                    jit_debug::JitDebugTarget::SyncEntry,
                    exit,
                );
                // The optimizing entry helper already recorded this exit; this
                // synchronous wrapper only owns template-entry accounting.
                if !optimized && !self.reoptimize_arith_overflow_bail(context, fid, exit) {
                    self.note_jit_entry_bail(context, fid);
                }
                Ok(None)
            }
            jit::JitExecOutcome::Returned(value) => {
                if !optimized {
                    self.note_jit_entry_success(stack[top_idx].function_id);
                }
                Ok(Some(value))
            }
            jit::JitExecOutcome::Throw(thrown) => {
                self.unwind_compiled_throw_above(context, stack, floor, thrown)?;
                Ok(None)
            }
            jit::JitExecOutcome::Fatal(err) => Err(err),
        }
    }

    /// Resolve installed compiled code for the bytecode frame at `top_idx`,
    /// compiling once the measured payoff becomes positive. Returns `None` when the frame is
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

    /// Resolve and enter installed optimized code over a fresh interpreter
    /// frame through the same runtime activation used by baseline code.
    pub(crate) fn run_optimized_frame(
        &mut self,
        stack: &mut ActivationStack,
        context: &ExecutionContext,
        top_idx: usize,
    ) -> Option<jit::JitExecOutcome> {
        self.promote_hot_generated_callee(context);
        let frame = stack.get(top_idx)?;
        if frame.pc != 0 || self.frame_has_suspension_owner(frame) {
            return None;
        }
        let fid = frame.function_id;
        let code = self.resolve_optimized_code_for_fid(context, fid)?;
        // Resolve the chunk owning the entered frame before any table read: a
        // caller in another chunk (a later script, an eval body, a
        // synchronous re-entry through an Array callback or comparator)
        // passes its own ambient chunk, whose tables do not describe this
        // function id at all — and runtime stubs decode published pcs and
        // constant indices through the activation, so the activation must be
        // the owner as well.
        let resolved = context.for_function(fid).ok()?;
        let function = resolved.exec_function(fid)?;
        let param_count = usize::from(function.param_count);
        if param_count > stack[top_idx].registers.len() {
            return None;
        }
        self.jit_runtime_stats.optimized_entries =
            self.jit_runtime_stats.optimized_entries.saturating_add(1);
        let activation = VmRuntimeActivation::new(self, stack, &resolved, top_idx);
        let outcome = code.run_optimized_entry(activation)?;
        if let jit::JitExecOutcome::Bailed(exit) = outcome {
            self.note_jit_optimized_bail(context, fid, exit);
        }
        Some(outcome)
    }

    /// Run promotion policy for the first pending generated callee request.
    ///
    /// Generated direct calls never enter through the interpreter, so their
    /// callee's entry counter would stay cold forever; the linkage instead
    /// counts entries into the callee's generation and records the function in
    /// the registry mailbox at its function-specific break-even point. This
    /// drain runs the complete live decision for it: a
    /// promoted body publishes through the function's permanent entry cell,
    /// so every generated caller switches without recompiling.
    pub(crate) fn promote_hot_generated_callee(&mut self, context: &ExecutionContext) {
        let Some(fid) = self.jit_code_registry.take_hot_function() else {
            return;
        };
        let Ok(owner) = context.for_function(fid) else {
            return;
        };
        let _ = self.resolve_optimized_code_for_fid(&owner, fid);
        if self.jit_optimized_code.contains_key(&fid) {
            self.jit_code_registry.suppress_generated_tiering(fid);
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
        if !self
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
            let instructions = u64::try_from(function.code.len()).unwrap_or(u64::MAX);
            let registers = u64::from(function.register_count);
            let parameters = u64::from(function.param_count);
            if self.optimizing_tier_decision_for(fid, instructions, registers, parameters, 0)
                != crate::tier_policy::OptimizingDecision::Promote
            {
                return None;
            }
            let resident_code_bytes = self.jit_code_residency().code_bytes;
            if self.optimizing_tier_decision_for(
                fid,
                instructions,
                registers,
                parameters,
                resident_code_bytes,
            ) != crate::tier_policy::OptimizingDecision::Promote
            {
                return None;
            }
            self.jit_runtime_stats.compile_attempts =
                self.jit_runtime_stats.compile_attempts.saturating_add(1);
            let compiled = self.compile_optimized_jit_function(context, fid, None);
            self.jit_optimized_code.insert(fid, compiled.clone());
            self.jit_optimized_code_cache = None;
            compiled
        };
        let code = code.filter(|code| self.jit_code_registry.is_current_for_entry(code.as_ref()));
        if let Some(code) = &code {
            self.jit_optimized_code_cache = Some((fid, code.clone()));
        }
        code
    }

    /// Resolve (and compile once profitable) the installed non-OSR
    /// baseline body for `fid`, independent of any stack frame. The lean
    /// callback loop uses this to tier up its callee without synthesizing a
    /// frame, then enters the cached body directly; [`Self::resolve_jit_code`]
    /// wraps it for the frame-entry path after its freshness checks.
    pub(crate) fn resolve_jit_code_for_fid(
        &mut self,
        context: &ExecutionContext,
        fid: u32,
    ) -> Option<std::sync::Arc<dyn jit::JitFunctionCode>> {
        self.promote_hot_generated_callee(context);
        let count = self.note_jit_function_entry(fid);
        self.maybe_refresh_successful_baseline(context, fid);
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
                let decision = self.jit_tier_cost_decision(
                    context,
                    fid,
                    crate::tier_policy::CostedTier::Template,
                    crate::tier_policy::TierTrigger::FunctionEntry,
                    u64::from(count),
                    0,
                    0,
                )?;
                if !decision.should_compile() {
                    return None;
                }
                let resident_code_bytes = self.jit_code_residency().code_bytes;
                if !self
                    .jit_tier_cost_decision(
                        context,
                        fid,
                        crate::tier_policy::CostedTier::Template,
                        crate::tier_policy::TierTrigger::FunctionEntry,
                        u64::from(count),
                        0,
                        resident_code_bytes,
                    )?
                    .should_compile()
                {
                    return None;
                }
                self.jit_runtime_stats.compile_attempts =
                    self.jit_runtime_stats.compile_attempts.saturating_add(1);
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
        context: &ExecutionContext,
        result: NativeResultPair,
        feedback_dirty: bool,
    ) -> Result<NativeResultPair, VmError> {
        self.jit_generated_feedback_pending |= feedback_dirty;
        if self.jit_native_activation_top != 0 {
            return Ok(result);
        }

        // This is the generated-code retirement epoch boundary. No native
        // frame can still hold an unleased entry address, so invalid mappings
        // with no ordinary Arc owner may now be released.
        self.jit_code_registry.retire_unreferenced();
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
    /// groups exact-generation deltas by function, advances the same hotness
    /// and call-budget counters as materialized bytecode calls, then lets the
    /// existing optimizing resolver sample hot baseline callees. Optimizing
    /// generations never become promotion candidates.
    fn reconcile_generated_feedback(&mut self, context: &ExecutionContext) {
        debug_assert_eq!(self.jit_native_activation_top, 0);
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
                    self.jit_runtime_stats.generated_template_throws = self
                        .jit_runtime_stats
                        .generated_template_throws
                        .saturating_add(entry.throws);
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
                    self.jit_runtime_stats.generated_optimizing_throws = self
                        .jit_runtime_stats
                        .generated_optimizing_throws
                        .saturating_add(entry.throws);
                }
                native_abi::NativeFrameKind::Interpreter => {
                    debug_assert!(false, "entry cells never describe interpreter frames");
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
            self.note_jit_function_entries(fid, batch.entries);
            self.record_runtime_bytecode_calls(batch.entries);
            if batch.baseline_entries != 0 {
                baseline_candidates.push(fid);
            }
        }
        // Compilation order determines code-object ids and artifact ordering.
        // Keep it stable even though the aggregation map is intentionally fast.
        // Callees are commonly assigned after their callers by source
        // lowering. Refresh higher ids first so their new entry generations
        // are available when a lower-id caller rebuilds in this cold batch.
        baseline_candidates.sort_unstable_by(|left, right| right.cmp(left));
        for fid in baseline_candidates {
            // Generated calls never revisit the ordinary entry resolver while
            // their caller stays native. Drive the same one-shot successful
            // baseline refresh here, after entry-cell feedback has been
            // reconciled and no native activation remains published.
            if self.feedback_refresh_due(context, fid) {
                self.maybe_refresh_successful_baseline(context, fid);
                let _ = self.resolve_jit_code_for_fid(context, fid);
            }
            let _ = self.resolve_optimized_code_for_fid(context, fid);
        }
    }

    /// Advance the shared function-entry hotness counter by one cold batch.
    #[inline]
    pub(crate) fn note_jit_function_entries(&mut self, fid: u32, entries: u64) -> u32 {
        let entries = u32::try_from(entries).unwrap_or(u32::MAX);
        let counter = self.jit_call_counts.entry(fid).or_insert(0);
        *counter = counter.saturating_add(entries);
        *counter
    }

    /// Advance the shared function-entry hotness counter once.
    #[inline]
    pub(crate) fn note_jit_function_entry(&mut self, fid: u32) -> u32 {
        self.note_jit_function_entries(fid, 1)
    }

    /// Whether `fid` has accumulated enough successful entry feedback for its
    /// one-shot baseline rebuild.
    #[inline]
    fn feedback_refresh_due(&self, context: &ExecutionContext, fid: u32) -> bool {
        let Some(pending_targets) = self.jit_pending_direct_targets.get(&fid) else {
            return false;
        };
        let executions = u64::from(self.jit_call_counts.get(&fid).copied().unwrap_or(0));
        !self.jit_feedback_refresh_attempted.contains(&fid)
            && self
                .jit_tier_cost_decision(
                    context,
                    fid,
                    crate::tier_policy::CostedTier::Template,
                    crate::tier_policy::TierTrigger::FunctionEntry,
                    executions,
                    0,
                    0,
                )
                .is_some_and(crate::tier_policy::TierCostDecision::should_compile)
            && matches!(self.jit_code.get(&fid), Some(Some(code))
                if self.jit_code_registry.is_current_for_entry(code.as_ref())
                    && !code.osr_only())
            && pending_targets.iter().all(|target_fid| {
                context
                    .exec_function(*target_fid)
                    .and_then(|target| self.current_direct_callee_plan(target))
                    .is_some()
            })
    }

    /// Rebuild one successful hot baseline generation against mature call
    /// feedback. This policy is independent from bail/deopt recovery: it is
    /// deliberately one-shot and never consumes the unhealthy-generation
    /// budget.
    fn maybe_refresh_successful_baseline(&mut self, context: &ExecutionContext, fid: u32) {
        if !self.feedback_refresh_due(context, fid) {
            return;
        }
        self.jit_feedback_refresh_attempted.insert(fid);
        self.jit_pending_direct_targets.remove(&fid);
        self.jit_runtime_stats.feedback_refreshes =
            self.jit_runtime_stats.feedback_refreshes.saturating_add(1);
        self.invalidate_jit_baseline_generation(fid);
    }

    /// Unlink `fid`'s canonical Template generation.
    ///
    /// Baseline feedback refresh replaces the one body shared by ordinary
    /// entry and Template OSR. Machine entry/OSR objects have independent
    /// feedback and remain installed.
    fn invalidate_jit_baseline_generation(&mut self, fid: u32) {
        let code = self.jit_code.remove(&fid).and_then(|slot| slot);
        if let Some(code) = code {
            let affected = self
                .jit_code_registry
                .invalidate_code_object(code.metadata().id);
            self.jit_runtime_stats.caller_invalidations =
                self.jit_runtime_stats.caller_invalidations.saturating_add(
                    affected
                        .iter()
                        .filter(|&&affected_fid| affected_fid != fid)
                        .count() as u64,
                );
        }
        self.jit_template_osr_fids.remove(&fid);
        self.jit_osr_disabled
            .retain(|(disabled_fid, _)| *disabled_fid != fid);
        self.jit_osr_counts
            .retain(|(counted_fid, _), _| *counted_fid != fid);
        if self
            .jit_code_cache
            .as_ref()
            .is_some_and(|(cached_fid, _)| *cached_fid == fid)
        {
            self.jit_code_cache = None;
        }
        self.jit_entry_osr_only.remove(&fid);
        self.jit_entry_bail_counts.remove(&fid);
    }

    /// Run compiled `code` over the rooted register window of frame `top_idx`.
    ///
    /// The window stays rooted on `stack` for the call, so closure allocation
    /// and recursive calls inside the body are GC-safe.
    pub(crate) fn run_compiled_frame(
        &mut self,
        stack: &mut ActivationStack,
        context: &ExecutionContext,
        top_idx: usize,
        code: &std::sync::Arc<dyn jit::JitFunctionCode>,
    ) -> jit::JitExecOutcome {
        // The activation context must be the chunk owning the entered frame:
        // the caller's dispatch tick may be running a sibling script's chunk,
        // and reentrant transitions resolve constants/atoms through the
        // activation. Entering with the caller's chunk would decode the
        // callee's constant-pool indices against foreign tables.
        let fid = stack
            .get(top_idx)
            .map_or(u32::MAX, |frame| frame.function_id);
        let resolved = match context.for_function(fid) {
            Ok(resolved) => resolved,
            Err(_) => return jit::JitExecOutcome::Fatal(VmError::InvalidOperand),
        };
        // SAFETY: the raw pointers are formed from this method's own live
        // borrows (`self`, `stack`, `resolved`) and are valid for the duration
        // of `run_entry`; the JIT does not retain them, and we do not touch
        // those borrows again until `run_entry` returns.
        let activation = jit::VmRuntimeActivation::new(self, stack, &resolved, top_idx);
        code.run_entry(activation)
    }

    /// Validate a tiny closure-call inline candidate and return its captured
    /// upvalue-spine base without cloning or publishing a callee frame.
    ///
    /// The baseline uses this only for leaf bodies with no allocation/call GC
    /// points. The pointer comes from [`crate::closure::ClosureCallHeader`]'s
    /// fixed-width ABI, never from interpreting Rust `Vec` / `Option` layout. It
    /// is valid only for the dynamic extent of the inlined body: the closure
    /// stays rooted in the caller frame and its upvalue backing allocation is
    /// immutable. A closure with runtime-setup flags declines this frameless
    /// leaf inline; the containing call takes its generated-call guard path or
    /// exact pre-effect side exit.
    pub fn jit_inline_closure_upvalues(
        &mut self,
        callee: Value,
        expected_fid: u32,
    ) -> Option<usize> {
        self.jit_runtime_stats.runtime_calls =
            self.jit_runtime_stats.runtime_calls.saturating_add(1);
        let closure = callee.as_closure(&self.gc_heap)?;
        if closure.function_id() != expected_fid {
            return None;
        }
        let header = closure.call_header(&self.gc_heap);
        if header.upvalue_count == 0
            || header.upvalue_base == 0
            || header.requires_runtime_setup()
            || !header.eval_env.is_null()
        {
            return None;
        }
        usize::try_from(header.upvalue_base).ok()
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
    /// Register windows live in the pinned register-stack slab, so `caller_regs`
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
        let new_target = inherited_new_target.unwrap_or(callee);
        self.observe_class_constructor_field_transitions(context, new_target)?;
        let result = self.run_construct_sync_rooted(stack, context, &callee, new_target, args)?;
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
    ) -> Result<(), VmError> {
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

    fn template_entry_break_even(context: &ExecutionContext, fid: u32) -> u32 {
        let function = context.exec_function(fid).expect("test function");
        u32::try_from(
            crate::tier_policy::TierCostModel::calibrated().minimum_profitable_executions(
                crate::tier_policy::TierCostInput {
                    tier: crate::tier_policy::CostedTier::Template,
                    trigger: crate::tier_policy::TierTrigger::FunctionEntry,
                    executions: 0,
                    exits: 0,
                    bytecode_instructions: u64::try_from(function.code.len()).unwrap_or(u64::MAX),
                    register_count: u64::from(function.register_count),
                    parameter_count: u64::from(function.param_count),
                    resident_code_bytes: 0,
                    cumulative_compile_ns: 0,
                },
            ),
        )
        .expect("fixture break-even fits u32")
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

        fn run_entry(&self, _activation: jit::VmRuntimeActivation) -> jit::JitExecOutcome {
            unreachable!("the ownership fixture never enters at function entry")
        }

        fn osr_entry(
            &self,
            _activation: jit::VmRuntimeActivation,
            logical_pc: u32,
        ) -> Option<jit::JitExecOutcome> {
            self.osr_entries.binary_search(&logical_pc).ok().map(|_| {
                jit::JitExecOutcome::Bailed(native_abi::SideExit::new(
                    logical_pc,
                    native_abi::ExitReason::UnsupportedOperation,
                    native_abi::ExitAction::Recompile,
                ))
            })
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
        // Keep entry payoff positive under the measured size-sensitive model;
        // this fixture tests shared ownership and retry, not tiny-body policy.
        for _ in 0..8 {
            code.push(Op::Nop, &[]);
        }
        code.push(Op::ReturnUndefined, &[]);
        let context = ExecutionContext::from_module(otter_bytecode::BytecodeModule {
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
        })
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
        ExecutionContext::from_module(crate::test_support::minimal_bytecode_module(
            "compiled-entry-transaction-test.js",
        ))
        .expect("valid bytecode fixture")
    }

    #[test]
    fn template_osr_reuses_one_function_body_for_every_loop_header() {
        let (context, osr_entries) = multi_loop_context(8);
        let requests = Arc::new(Mutex::new(Vec::new()));
        let hook = Arc::new(CountingTemplateHook {
            requests: Arc::clone(&requests),
        });
        let mut vm = Interpreter::new();
        vm.jit_hook = Some(hook);

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

        let activation = jit::VmRuntimeActivation::for_test(&mut vm);
        for &osr_pc in &osr_entries {
            assert!(matches!(
                first.osr_entry(activation, osr_pc),
                Some(jit::JitExecOutcome::Bailed(exit)) if exit.logical_pc() == osr_pc
            ));
        }
    }

    #[test]
    fn template_entry_and_osr_share_the_same_canonical_body() {
        let (context, osr_entries) = multi_loop_context(2);

        let entry_first_requests = Arc::new(Mutex::new(Vec::new()));
        let mut entry_first = Interpreter::new();
        entry_first.jit_hook = Some(Arc::new(CountingTemplateHook {
            requests: Arc::clone(&entry_first_requests),
        }));
        entry_first
            .jit_call_counts
            .insert(0, template_entry_break_even(&context, 0) - 1);
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
        let mut osr_first = Interpreter::new();
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
        let mut vm = Interpreter::new();
        vm.jit_hook = Some(Arc::new(CountingTemplateHook {
            requests: Arc::clone(&requests),
        }));

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
    fn transient_template_failure_retries_after_repaying_measured_compile_cost() {
        let (context, _) = multi_loop_context(1);
        let hook = Arc::new(DeferredOnceTemplateHook {
            requests: AtomicUsize::new(0),
        });
        let mut vm = Interpreter::new();
        vm.jit_hook = Some(hook.clone());
        vm.jit_call_counts
            .insert(0, template_entry_break_even(&context, 0) - 1);

        assert!(vm.resolve_jit_code_for_fid(&context, 0).is_none());
        assert!(!vm.jit_code.contains_key(&0));
        assert_eq!(hook.requests.load(Ordering::Relaxed), 1);
        assert!(
            vm.resolve_jit_code_for_fid(&context, 0).is_none(),
            "actual failed-compile duration must prevent an immediate retry"
        );
        let mut installed = false;
        for _ in 0..100_000 {
            if vm.resolve_jit_code_for_fid(&context, 0).is_some() {
                installed = true;
                break;
            }
        }
        assert!(
            installed,
            "new execution evidence eventually repays the failure"
        );
        assert_eq!(hook.requests.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn permanent_template_failure_is_cached_for_the_whole_function() {
        let (context, osr_entries) = multi_loop_context(2);
        let hook = Arc::new(UnsupportedTemplateHook {
            requests: AtomicUsize::new(0),
        });
        let mut vm = Interpreter::new();
        vm.jit_hook = Some(hook.clone());

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
        let mut vm = Interpreter::new();
        let object = crate::object::alloc_object_with_roots(&mut vm.gc_heap, &mut |_| {})
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
        let mut vm = Interpreter::new();
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
        let mut vm = Interpreter::new();
        let context = empty_context();
        let result = NativeResultPair::success(Value::number_i32(41));

        vm.jit_native_activation_top = 1;
        let nested = vm
            .finish_compiled_entry_transaction(&context, result, true)
            .expect("nested completion");
        assert_eq!(nested, result);
        assert!(vm.jit_generated_feedback_pending);

        vm.jit_native_activation_top = 0;
        let outer = vm
            .finish_compiled_entry_transaction(&context, result, false)
            .expect("outer completion");
        assert_eq!(outer, result);
        assert!(!vm.jit_generated_feedback_pending);
    }
}
