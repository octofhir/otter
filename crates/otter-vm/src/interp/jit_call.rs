//! JIT entry, OSR, and generated-call frame plumbing.
//!
//! # Contents
//! Tier-up dispatch (`maybe_dispatch_jit`, backedge/OSR accounting),
//! compiled-frame entry (`run_compiled_frame`, `jit_runtime_call`),
//! generated-call feedback through focused `jit_calls` modules, and cold
//! inlined/stack-call side-exit materialization in `jit_calls/deopt`.
//! Call and back-edge accounting also feeds the additive optimizing-tier
//! policy without consulting its decision. Generated-call entry feedback is
//! reconciled once after the outer native activation returns.
//!
//! # Invariants
//! Every generated callee frame remains published until native return, throw,
//! or cold deoptimization releases its entry lease and depth accounting.
//! Every VM-side compiled entry selection requires the exact installed code
//! generation and isolate-epoch dependency state. Safepoint resolution for
//! already-active Invalid code remains independent.
//! Optimized entries run only over fresh ordinary frames; every bail resumes
//! the interpreter on the generated exit's fully reconstructed register
//! window. They use the same fully wired runtime activation, published native
//! frame, and call-scoped VM thread as baseline entries.
//! Canonical tier transitions retain one [`NativeFrame`] and register window;
//! materialized [`Frame`] construction is confined to cold deoptimization and
//! interpreter-owned dispatch.
#![allow(unused_imports)]
use crate::*;

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
        pc: u32,
    ) {
        self.record_jit_debug_event(|| {
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
            jit::JitExecOutcome::Bailed(pc) => {
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
                    pc,
                );
                if !optimized && !self.reoptimize_arith_overflow_bail(context, fid, pc) {
                    self.note_jit_entry_bail(fid);
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
            jit::JitExecOutcome::Threw(err) => {
                if matches!(err, VmError::Uncaught)
                    && let Some(thrown) = self.pending_uncaught_throw.take()
                {
                    if self.pending_uncaught_frames.is_none() {
                        self.pending_uncaught_frames = Some(snapshot_frames(context, stack));
                    }
                    let unwind = self.unwind_throw_above(context, stack, floor, thrown);
                    if unwind.is_ok() {
                        self.pending_uncaught_frames = None;
                    } else {
                        self.pending_uncaught_throw = Some(thrown);
                    }
                    unwind?;
                    return if stack.is_at_floor(floor) {
                        Ok(Some(Some(Value::undefined())))
                    } else {
                        Ok(None)
                    };
                }
                if let Some(thrown) =
                    self.vm_error_to_throwable_with_stack_roots(Some(context), stack, &err)
                {
                    let uncaught =
                        if matches!(err, VmError::OutOfMemory { .. } | VmError::JsonError) {
                            Some(err)
                        } else {
                            None
                        };
                    if self.pending_uncaught_frames.is_none() {
                        self.pending_uncaught_frames = Some(snapshot_frames(context, stack));
                    }
                    let unwind = self
                        .unwind_throw_with_uncaught_above(context, stack, floor, thrown, uncaught);
                    if unwind.is_ok() {
                        self.pending_uncaught_frames = None;
                    }
                    unwind?;
                    return if stack.is_at_floor(floor) {
                        Ok(Some(Some(Value::undefined())))
                    } else {
                        Ok(None)
                    };
                }
                Err(err)
            }
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
        if count < self.jit_osr_threshold {
            return Ok(None);
        }
        // Threshold reached: drop this header's counter (it tiers up now or is
        // marked disabled by `maybe_osr`, so it should not keep counting) and
        // attempt OSR.
        self.jit_runtime_stats.osr_attempts = self.jit_runtime_stats.osr_attempts.saturating_add(1);
        self.jit_osr_counts.remove(&key);
        self.maybe_osr(stack, context, top_idx, floor)
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
        let resolved = context.for_function(fid).ok_or(VmError::InvalidOperand)?;
        let activation = jit::VmRuntimeActivation::new(self, stack, &resolved, top_idx);
        let optimized_outcome = self
            .resolve_optimized_osr_code(context, fid, osr_pc)
            .filter(|code| self.jit_code_registry.is_current_for_entry(code.as_ref()))
            .and_then(|code| code.run_optimized_osr_entry(activation, osr_pc));
        let (outcome, optimized) = if let Some(outcome) = optimized_outcome {
            self.jit_runtime_stats.optimized_entries =
                self.jit_runtime_stats.optimized_entries.saturating_add(1);
            self.jit_runtime_stats.optimized_osr_entries = self
                .jit_runtime_stats
                .optimized_osr_entries
                .saturating_add(1);
            if matches!(outcome, jit::JitExecOutcome::Bailed(_)) {
                self.jit_runtime_stats.optimized_deopts =
                    self.jit_runtime_stats.optimized_deopts.saturating_add(1);
            }
            (outcome, true)
        } else {
            // The whole-body optimizer declined this function/header. Resolve
            // the existing template OSR object exactly as before.
            let osr_key = (fid, osr_pc);
            let code = match self.jit_osr_code.get(&osr_key) {
                Some(slot) => slot.clone(),
                None => {
                    let compiled = self.compile_jit_function(context, fid, Some(osr_pc));
                    self.jit_osr_code.insert(osr_key, compiled.clone());
                    compiled
                }
            };
            let Some(code) = code else {
                self.jit_osr_disabled.insert((fid, osr_pc));
                return Ok(None);
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
            jit::JitExecOutcome::Bailed(pc) => {
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
                    pc,
                );
                stack[top_idx].pc = pc;
                if self.reoptimize_arith_overflow_bail(context, fid, pc) {
                    return Ok(None);
                }
                if Self::osr_bail_inside_target_loop(context, fid, osr_pc, pc) {
                    self.jit_osr_disabled.insert((fid, osr_pc));
                }
                Ok(None)
            }
            jit::JitExecOutcome::Returned(value) => {
                let popped = self.return_running_finally_above(stack, floor, value)?;
                Ok(Some(popped))
            }
            jit::JitExecOutcome::Threw(err) => Err(err),
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

    /// Treat the first compiled `Add` / `Sub` / `Mul` bail at a logical PC as an
    /// int32-result overflow and recompile that function with the site widened
    /// to float arithmetic. The interpreter feedback only records operand
    /// representations, so an accumulator can keep looking int32-only while its
    /// result has grown past the int32 range. Widening once avoids permanently
    /// disabling an otherwise valid hot loop; a second bail at the same site is
    /// left to the normal deopt/disable path.
    pub(crate) fn reoptimize_arith_overflow_bail(
        &mut self,
        context: &ExecutionContext,
        fid: u32,
        bail_pc: u32,
    ) -> bool {
        let Some(function) = context.exec_function(fid) else {
            return false;
        };
        let Some(instr) = function.instr_at_index(bail_pc as usize) else {
            return false;
        };
        if !matches!(function.op(instr), Op::Add | Op::Sub | Op::Mul) {
            return false;
        }
        let Some(feedback) = function.feedback_recorder_at(instr.instruction_pc as usize) else {
            return false;
        };
        if !feedback.widen_arith_to_float() {
            return false;
        }
        self.invalidate_jit_function(fid);
        true
    }

    /// Record one *entry* bail out of `fid`'s installed compiled body and
    /// evict-for-recompile when it keeps happening.
    ///
    /// A body whose guard fails right after entry on every call (typically
    /// compiled at the tier-up threshold against feedback that later turned
    /// polymorphic) is strictly worse than interpreting: each call pays the
    /// compiled prologue, the failing guard, and the bail hand-off, then
    /// interprets anyway — and nothing evicts it, so it stays that way forever.
    /// Each bailed call *does* complete in the interpreter, enriching the
    /// property/method/arith feedback for exactly the sites that failed, so at
    /// [`Self::JIT_ENTRY_BAIL_REOPT_THRESHOLD`] the body is dropped and the
    /// next resolve recompiles it against that richer snapshot. A function
    /// that has been recompiled [`Self::JIT_MAX_ENTRY_BAIL_REOPTS`] times and
    /// still bail-loops is pinned to the interpreter (`jit_code[fid] = None`,
    /// the "uncompilable" verdict) instead of thrashing the compiler.
    /// The count is of *consecutive* bails: a successful compiled completion
    /// clears it (see [`Self::note_jit_entry_success`]), so a body whose rare
    /// cold branch bails but whose hot path completes fine never accumulates
    /// to the threshold — only a bail-dominated body is evicted.
    pub(crate) fn note_jit_entry_bail(&mut self, fid: u32) {
        let bails = self.jit_entry_bail_counts.entry(fid).or_insert(0);
        *bails = bails.saturating_add(1);
        if *bails < Self::JIT_ENTRY_BAIL_REOPT_THRESHOLD {
            return;
        }
        self.reopt_or_pin_jit_function(fid);
    }

    /// Invalidate one unhealthy generation and consume its bounded recompile
    /// budget, pinning the function to the interpreter when recompilation has
    /// repeatedly failed to produce a stable body.
    pub(crate) fn reopt_or_pin_jit_function(&mut self, fid: u32) {
        self.jit_entry_bail_counts.remove(&fid);
        let reopts = self.jit_entry_reopt_counts.entry(fid).or_insert(0);
        let exhausted = *reopts >= Self::JIT_MAX_ENTRY_BAIL_REOPTS;
        *reopts = reopts.saturating_add(1);
        self.invalidate_jit_function(fid);
        if exhausted {
            self.jit_code.insert(fid, None);
        }
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
        self.jit_osr_code
            .retain(|(fid, _), _| !affected.contains(fid));
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
    /// A zero target enters this single no-allocation resolver, which can
    /// republish an already-installed fallback generation before the caller
    /// takes its exact pre-effect side exit.
    pub fn jit_resolve_direct_entry(&mut self, function_entry_addr: u64) -> u64 {
        self.jit_runtime_stats.cold_entry_resolver_misses = self
            .jit_runtime_stats
            .cold_entry_resolver_misses
            .saturating_add(1);
        self.jit_code_registry
            .resolve_function_entry(function_entry_addr)
    }

    /// Tier-up entry point for a synchronously-entered call frame (the
    /// [`Self::run_callable_sync`] path), where the callee frame is the sole
    /// entry on its own `stack`. Mirrors [`Self::maybe_dispatch_jit`] but, on a
    /// successful compiled run, the completion *is* the call result (there is no
    /// caller frame to unwind into).
    ///
    /// Returns `Ok(Some(v))` when compiled code ran the frame to completion, or
    /// `Ok(None)` to interpret it normally.
    pub(crate) fn dispatch_jit_sync_entry(
        &mut self,
        stack: &mut ActivationStack,
        context: &ExecutionContext,
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
            jit::JitExecOutcome::Bailed(pc) => {
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
                    pc,
                );
                if !optimized && !self.reoptimize_arith_overflow_bail(context, fid, pc) {
                    self.note_jit_entry_bail(fid);
                }
                Ok(None)
            }
            jit::JitExecOutcome::Returned(value) => {
                if !optimized {
                    self.note_jit_entry_success(stack[top_idx].function_id);
                }
                Ok(Some(value))
            }
            jit::JitExecOutcome::Threw(err) => Err(err),
        }
    }

    /// Resolve installed compiled code for the bytecode frame at `top_idx`,
    /// compiling once at the tier-up threshold. Returns `None` when the frame is
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
        let frame = stack.get(top_idx)?;
        if frame.pc != 0 || self.frame_has_suspension_owner(frame) {
            return None;
        }
        let fid = frame.function_id;
        let code = self.resolve_optimized_code_for_fid(context, fid)?;
        let function = context.exec_function(fid)?;
        let param_count = usize::from(function.param_count);
        if param_count > stack[top_idx].registers.len() {
            return None;
        }
        self.jit_runtime_stats.optimized_entries =
            self.jit_runtime_stats.optimized_entries.saturating_add(1);
        let activation = VmRuntimeActivation::new(self, stack, context, top_idx);
        let outcome = code.run_optimized_entry(activation)?;
        if matches!(outcome, jit::JitExecOutcome::Bailed(_)) {
            self.jit_runtime_stats.optimized_deopts =
                self.jit_runtime_stats.optimized_deopts.saturating_add(1);
        }
        Some(outcome)
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
            if self.optimizing_tier_decision(fid) != crate::tier_policy::OptimizingDecision::Promote
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

    /// Resolve (and compile-once at the tier-up threshold) the installed non-OSR
    /// baseline body for `fid`, independent of any stack frame. The lean
    /// callback loop uses this to tier up its callee without synthesizing a
    /// frame, then enters the cached body directly; [`Self::resolve_jit_code`]
    /// wraps it for the frame-entry path after its freshness checks.
    pub(crate) fn resolve_jit_code_for_fid(
        &mut self,
        context: &ExecutionContext,
        fid: u32,
    ) -> Option<std::sync::Arc<dyn jit::JitFunctionCode>> {
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
        let code = if let Some(slot) = self.jit_code.get(&fid) {
            slot.clone()
        } else {
            if count < Self::JIT_TIER_UP_THRESHOLD {
                return None;
            }
            let compiled = self.compile_jit_function(context, fid, None);
            self.jit_runtime_stats.compile_attempts =
                self.jit_runtime_stats.compile_attempts.saturating_add(1);
            self.jit_code.insert(fid, compiled.clone());
            self.jit_code_cache = None;
            compiled
        };
        // The function-entry path never runs OSR-only code (compiled with
        // unsupported opcodes emitted as bails); only loop OSR enters it, at a
        // supported loop header. The code stays cached for that OSR path.
        let code = code
            .filter(|c| self.jit_code_registry.is_current_for_entry(c.as_ref()) && !c.osr_only());
        if let Some(c) = &code {
            self.jit_code_cache = Some((fid, c.clone()));
        } else {
            // Reached only past the tier-up threshold (a below-threshold fid
            // returns early above), so `jit_code[fid]` is installed and its
            // `None`/`osr_only` verdict is final: record it so the entry path
            // stops re-probing it.
            self.jit_entry_osr_only.insert(fid);
        }
        code
    }

    /// Reconcile generated entry-cell feedback after the outermost native
    /// activation has been unpublished.
    ///
    /// Native entries stay allocation- and transition-free. This cold pass
    /// groups exact-generation deltas by function, advances the same hotness
    /// and call-budget counters as materialized bytecode calls, then lets the
    /// existing optimizing resolver sample hot baseline callees. Optimizing
    /// generations never become promotion candidates.
    pub fn jit_reconcile_generated_feedback(&mut self, context: &ExecutionContext) {
        if self.jit_native_activation_top != 0 {
            return;
        }
        // This is the generated-code retirement epoch boundary. No native
        // frame can still hold an unleased entry address, so invalid mappings
        // with no ordinary Arc owner may now be released.
        self.jit_code_registry.retire_unreferenced();
        if !self.jit_generated_feedback_pending {
            return;
        }
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
        !self.jit_feedback_refresh_attempted.contains(&fid)
            && self.jit_call_counts.get(&fid).copied().unwrap_or(0)
                >= Self::JIT_FEEDBACK_REFRESH_THRESHOLD
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

    /// Unlink only `fid`'s current template entry generation.
    ///
    /// Baseline feedback refresh is an entry-code replacement, not a function
    /// invalidation. Optimizing entry/OSR objects have independent feedback and
    /// remain installed so a hot loop does not fall back to template merely
    /// because direct-call targets matured later.
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
            Some(resolved) => resolved,
            None => return jit::JitExecOutcome::Threw(VmError::InvalidOperand),
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
        if header.upvalue_count == 0 || header.upvalue_base == 0 || header.requires_runtime_setup()
        {
            return None;
        }
        usize::try_from(header.upvalue_base).ok()
    }

    /// Rebuild an inlined callee's interpreter frame at a deopt exit.
    ///
    /// The optimized code has already written the caller's registers back into
    /// its window and is about to hand control to the interpreter, but the
    /// callee body it had spliced in owes the interpreter a frame of its own.
    ///
    /// Rather than reproduce the call's frame setup — the upvalue spine, `this`,
    /// argument binding, the register window — this rewinds the caller to its
    /// call and runs the interpreter's own ordinary- or method-call path over
    /// the restored window. The frame that comes out is by construction the
    /// frame a real call would have produced, including the advanced caller PC,
    /// exact method receiver, and return destination.
    ///
    /// The frame is then fast-forwarded to `callee_pc`, the instruction the
    /// exit names, and its register-window base is returned so emitted code can
    /// restore the callee's registers into it.
    ///
    /// # Safety
    /// `stack`'s top frame must be the caller, with its registers already
    /// restored, and `call_pc` must name an `Op::Call` or
    /// `Op::CallMethodValue` in the caller's body.
    pub unsafe fn jit_deopt_reify_inlined_frame(
        &mut self,
        context: &ExecutionContext,
        stack: &mut ActivationStack,
        call_pc: u32,
        callee_pc: u32,
    ) -> Result<*mut crate::Value, VmError> {
        let caller_index = stack.len().checked_sub(1).ok_or(VmError::InvalidOperand)?;
        let function_id = stack[caller_index].function_id;
        let code_block = context
            .exec_function(function_id)
            .ok_or(VmError::InvalidOperand)?;
        let instruction = code_block
            .instr_at_index(call_pc as usize)
            .ok_or(VmError::InvalidOperand)?;
        let op = code_block.op(instruction);
        stack[caller_index].pc = call_pc;
        let operands = code_block.operand_view(instruction);
        match op {
            otter_bytecode::Op::Call => self.do_call(stack, context, operands)?,
            otter_bytecode::Op::CallMethodValue => {
                self.do_call_method_value(stack, context, operands)?;
            }
            _ => return Err(VmError::InvalidOperand),
        }
        // A bytecode callee is now on top; a native or otherwise non-bytecode
        // callee would have completed in place, which the identity guard at the
        // spliced call site rules out.
        if stack.len() != caller_index + 2 {
            return Err(VmError::InvalidOperand);
        }
        let callee_index = caller_index + 1;
        // The interpreter's call path starts the callee at its entry; the
        // optimized code was further in. Fast-forward the frame to the exact
        // instruction the exit names.
        let callee_body = context
            .exec_function(stack[callee_index].function_id)
            .ok_or(VmError::InvalidOperand)?;
        if callee_pc as usize >= callee_body.code.len() {
            return Err(VmError::InvalidOperand);
        }
        stack[callee_index].pc = callee_pc;
        Ok(stack[callee_index].registers.as_mut_ptr())
    }

    /// Complete one full `Op::New` construct in place for a compiled caller
    /// whose New site fell outside the compiled subset. Reads the callee and
    /// argument registers from the caller's live window and runs the
    /// interpreter's own `Construct(callee, args, callee)` synchronously under
    /// the caller's published activation, writing the constructed value into
    /// `dst`.
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
        let mut args: SmallVec<[Value; 8]> = SmallVec::with_capacity(arg_regs.len());
        for &arg in arg_regs {
            // SAFETY: compiler-emitted argument indices into the caller window.
            args.push(unsafe { *caller_regs.add(arg as usize) });
        }
        // A plain `new callee(args…)` uses the callee itself as `new.target`,
        // matching `do_construct`'s `effective_new_target`.
        let result = self.run_construct_sync_rooted(stack, context, &callee, callee, args)?;
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

    /// Build the closure for a compiled `MakeFunction`,
    /// writing it into register `dst` of frame `frame_index` (self-reference
    /// capture and upvalue binding go through the normal interpreter path).
    ///
    /// # Errors
    /// Propagates closure-construction errors and `InvalidOperand` for an
    /// out-of-range frame index.
    pub fn jit_runtime_make_function(
        &mut self,
        context: &ExecutionContext,
        stack: &mut ActivationStack,
        frame_index: usize,
        dst: u16,
        idx: u32,
    ) -> Result<(), VmError> {
        // `self` and `stack` are disjoint, so the two `&mut` are non-aliasing.
        let frame = stack
            .get_mut(frame_index)
            .ok_or_else(|| VmError::InvalidOperand)?;
        // `idx` is a constant-pool index of the COMPILED function's chunk;
        // in a multi-script runtime the ambient context may belong to a
        // different chunk, so resolve the owner before decoding.
        let resolved = context
            .for_function(frame.function_id)
            .ok_or(VmError::InvalidOperand)?;
        self.run_make_function_reg(&resolved, frame, dst, idx)
    }
}
