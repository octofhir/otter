//! JIT entry, OSR, and direct-call frame plumbing.
//!
//! # Contents
//! Tier-up dispatch (`maybe_dispatch_jit`, backedge/OSR accounting),
//! compiled-frame entry (`run_compiled_frame`, `jit_runtime_call`),
//! direct-call lifecycle dispatch through focused `jit_calls` modules,
//! in-place coercive unary operations, and
//! raw frame-pointer accessors the emitted code reads (`jit_frame_regs_ptr`
//! and friends). Call and back-edge accounting also feeds the additive
//! optimizing-tier policy without consulting its decision.
//!
//! # Invariants
//! Every publish of a callee frame is paired with a finish/abort helper
//! that releases pinned code and the sync-reentry guard; bail paths must
//! leave the frame stack exactly as the interpreter expects to resume.
//! Reentrant unary coercion owns moving values through the handle arena and
//! commits its destination only after the abstract operation succeeds.
//! Every VM-side compiled entry selection applies both native-layout metadata
//! compatibility and exact isolate-epoch dependency consistency. Safepoint
//! resolution for already-active Invalid code remains independent.
//! Optimized entries run only over fresh ordinary frames; every bail resumes
//! the interpreter on the generated exit's fully reconstructed register
//! window. They use the same fully wired runtime activation, published native
//! frame, and call-scoped VM thread as baseline entries.
#![allow(unused_imports)]
use crate::*;

#[path = "jit_calls/frame.rs"]
mod frame;
#[path = "jit_calls/finish.rs"]
mod finish;
#[path = "jit_calls/cache.rs"]
pub(crate) mod cache;
#[path = "jit_calls/resolve.rs"]
mod resolve;

impl Interpreter {
    /// After a call pushed a fresh bytecode callee frame as the new top of
    /// `stack`, try to run it as compiled baseline code instead of interpreting.
    ///
    /// Only invoked when a JIT hook is installed and a frame was actually
    /// pushed (the caller checks `stack` grew). Returns `Ok(None)` to interpret
    /// normally; `Ok(Some(popped))` when the JIT ran and the callee returned,
    /// where `popped` mirrors [`Self::return_running_finally`] (`Some(v)` means
    /// the return unwound the dispatch entry and the loop should yield `v`).
    pub(crate) fn trace_jit_bail(
        context: &ExecutionContext,
        fid: u32,
        kind: &str,
        osr_pc: Option<u32>,
        pc: u32,
    ) {
        if std::env::var_os("OTTER_JIT_TRACE").is_none() {
            return;
        }
        let function_name = context
            .function(fid)
            .map(|function| function.name.as_str())
            .unwrap_or("<unknown>");
        let instruction = context.exec_function(fid).and_then(|function| {
            function
                .instr_at_index(pc as usize)
                .map(|instr| (function, instr))
        });
        let op = instruction.map(|(function, instr)| function.op(instr));
        let operands = instruction
            .map(|(function, instr)| format!("{:?}", function.operand_view(instr)))
            .unwrap_or_else(|| "[]".to_string());
        eprintln!(
            "[otter-jit] {kind} bail fid {fid} {function_name} osr={osr_pc:?} pc {pc} op {op:?} operands {operands}"
        );
    }

    pub(crate) fn maybe_dispatch_jit(
        &mut self,
        stack: &mut HoltStack,
        context: &ExecutionContext,
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
                if !optimized {
                    let fid = stack[top_idx].function_id;
                    Self::trace_jit_bail(context, fid, "entry", None, pc);
                    if !self.reoptimize_arith_overflow_bail(context, fid, pc) {
                        self.note_jit_entry_bail(fid);
                    }
                }
                Ok(None)
            }
            jit::JitExecOutcome::Returned(value) => {
                if !optimized {
                    self.note_jit_entry_success(stack[top_idx].function_id);
                }
                let popped = self.return_running_finally(stack, value)?;
                Ok(Some(popped))
            }
            jit::JitExecOutcome::Threw(err) => {
                if matches!(err, VmError::Uncaught)
                    && let Some(thrown) = self.pending_uncaught_throw.take()
                {
                    if self.pending_uncaught_frames.is_none() {
                        self.pending_uncaught_frames = Some(snapshot_frames(context, stack));
                    }
                    let unwind = self.unwind_throw(context, stack, thrown);
                    if unwind.is_ok() {
                        self.pending_uncaught_frames = None;
                    } else {
                        self.pending_uncaught_throw = Some(thrown);
                    }
                    unwind?;
                    return if stack.is_empty() {
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
                    let unwind = self.unwind_throw_with_uncaught(context, stack, thrown, uncaught);
                    if unwind.is_ok() {
                        self.pending_uncaught_frames = None;
                    }
                    unwind?;
                    return if stack.is_empty() {
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
        stack: &mut HoltStack,
        context: &ExecutionContext,
        top_idx: usize,
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
        self.maybe_osr(stack, context, top_idx)
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
        stack: &mut HoltStack,
        context: &ExecutionContext,
        top_idx: usize,
    ) -> Result<Option<Option<Value>>, VmError> {
        let frame = &stack[top_idx];
        // Only ordinary bytecode frames; async/generator bodies resume through
        // their own machinery and must not be entered mid-loop.
        if frame.async_state.is_some() || frame.generator_owner.is_some() {
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
            .resolve_optimized_osr_code(context, fid)
            .filter(|code| {
                self.jit_code_registry
                    .dependencies_are_current_for_entry(code.as_ref())
            })
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
            if !self
                .jit_code_registry
                .dependencies_are_current_for_entry(code.as_ref())
            {
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
                let tier = if optimized { "optimized-osr" } else { "osr" };
                Self::trace_jit_bail(context, fid, tier, Some(osr_pc), pc);
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
                let popped = self.return_running_finally(stack, value)?;
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

    /// Drop every installed optimizing-tier body for `fid` so the next tier-up
    /// sees the latest compile policy / feedback snapshot.
    pub(crate) fn invalidate_jit_function(&mut self, fid: u32) {
        self.jit_optimized_code.remove(&fid);
        self.jit_optimized_code_cache = None;
        self.jit_code.remove(&fid);
        self.jit_entry_osr_only.remove(&fid);
        self.jit_code_cache = None;
        self.clear_jit_direct_method_cache_for_fid(fid);
        self.jit_code_registry.invalidate_function(fid);
        self.jit_osr_code
            .retain(|(entry_fid, _), _| *entry_fid != fid);
        self.jit_osr_disabled
            .retain(|(entry_fid, _)| *entry_fid != fid);
        self.jit_osr_counts
            .retain(|(entry_fid, _), _| *entry_fid != fid);
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
        stack: &mut HoltStack,
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
                if !optimized {
                    let fid = stack[top_idx].function_id;
                    Self::trace_jit_bail(context, fid, "sync-entry", None, pc);
                    if !self.reoptimize_arith_overflow_bail(context, fid, pc) {
                        self.note_jit_entry_bail(fid);
                    }
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
        stack: &HoltStack,
        context: &ExecutionContext,
        top_idx: usize,
    ) -> Option<std::sync::Arc<dyn jit::JitFunctionCode>> {
        // Only fresh, ordinary bytecode frames: at entry (pc == 0), not async,
        // not a generator body.
        let frame = &stack[top_idx];
        if frame.pc != 0 || frame.async_state.is_some() || frame.generator_owner.is_some() {
            return None;
        }
        self.resolve_jit_code_for_fid(context, frame.function_id)
    }

    /// Resolve and enter installed optimized code over a fresh interpreter
    /// frame through the same runtime activation used by baseline code.
    pub(crate) fn run_optimized_frame(
        &mut self,
        stack: &mut HoltStack,
        context: &ExecutionContext,
        top_idx: usize,
    ) -> Option<jit::JitExecOutcome> {
        let frame = stack.get(top_idx)?;
        if frame.pc != 0 || frame.async_state.is_some() || frame.generator_owner.is_some() {
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

    /// Resolve a separately installed optimizing body, compiling exactly once
    /// after the deterministic promotion policy reaches `Promote`.
    pub(crate) fn resolve_optimized_code_for_fid(
        &mut self,
        context: &ExecutionContext,
        fid: u32,
    ) -> Option<std::sync::Arc<dyn jit::JitFunctionCode>> {
        if let Some((cached_fid, code)) = &self.jit_optimized_code_cache
            && *cached_fid == fid
            && self
                .jit_code_registry
                .is_compatible_for_entry(code.as_ref())
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
            let compiled = self.compile_optimized_jit_function(context, fid);
            self.jit_optimized_code.insert(fid, compiled.clone());
            self.jit_optimized_code_cache = None;
            compiled
        };
        let code = code.filter(|code| {
            self.jit_code_registry
                .is_compatible_for_entry(code.as_ref())
        });
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
        // Single-entry compiled-code cache. A hot synchronous re-entry (Array
        // callbacks, comparators, `@@iterator` drives) resolves the SAME callee
        // every call; this skips the `jit_code` FxHashMap lookup + `Arc` clone
        // churn when the last resolve matched. The cache only ever holds
        // non-`osr_only` code (populated below + by `jit_resolve_compiled_cached`),
        // so it needs no further filtering.
        if let Some((cached_fid, code)) = &self.jit_code_cache
            && *cached_fid == fid
            && self
                .jit_code_registry
                .is_compatible_for_entry(code.as_ref())
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
            self.clear_jit_direct_method_cache_for_fid(fid);
            compiled
        };
        // The function-entry path never runs OSR-only code (compiled with
        // unsupported opcodes emitted as bails); only loop OSR enters it, at a
        // supported loop header. The code stays cached for that OSR path.
        let code = code.filter(|c| {
            self.jit_code_registry.is_compatible_for_entry(c.as_ref()) && !c.osr_only()
        });
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

    /// Advance the shared function-entry hotness counter once.
    #[inline]
    pub(crate) fn note_jit_function_entry(&mut self, fid: u32) -> u32 {
        let counter = self.jit_call_counts.entry(fid).or_insert(0);
        *counter = counter.saturating_add(1);
        *counter
    }

    /// Run compiled `code` over the rooted register window of frame `top_idx`.
    ///
    /// The window stays rooted on `stack` for the call, so closure allocation
    /// and recursive calls inside the body are GC-safe.
    pub(crate) fn run_compiled_frame(
        &mut self,
        stack: &mut HoltStack,
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
    /// points. Returning a pointer into the closure body's `Vec<UpvalueCell>` is
    /// therefore safe for the dynamic extent of the inlined body: the closure is
    /// still rooted in the caller frame and the upvalue vector is immutable
    /// after closure creation.
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
        self.gc_heap.read_payload(closure.handle(), |body| {
            if body.upvalues.is_empty()
                || body.bound_new_target.is_some()
                || body.bound_derived_this.is_some()
                || body.eval_env.is_some()
            {
                return None;
            }
            Some(body.upvalues.as_ptr() as usize)
        })
    }


    /// Complete one full `CallMethodValue` in place for a compiled caller
    /// whose direct-call prepare reported an ineligible resolution
    /// (polymorphic, native, accessor, or cold method).
    ///
    /// Covers exactly the receiver families whose interpreter semantics are
    /// "resolve the method value, then call it": ordinary property-bearing
    /// receivers (objects, arrays, collections, proxies) and primitives.
    /// Covers callable receivers too: the resolver owns function /
    /// class-constructor / native property walks, and the synchronous
    /// callable path dispatches resolved VM intrinsics (`call`, `apply`,
    /// `bind`) itself. Families the interpreter dispatches through bespoke
    /// opcode branches — generators, iterators, and pending `bind`
    /// continuations — report `Ok(false)` before resolution starts. Once
    /// resolution has invoked an accessor or proxy trap, missing and
    /// non-callable results throw here so an exact side exit cannot replay the
    /// observable `[[Get]]`. On `Ok(true)` the destination register holds the
    /// call result and the compiled caller continues at the next instruction.
    ///
    /// The callee runs through [`Self::run_callable_sync_already_rooted`]
    /// under the caller's published activation: it may allocate, re-enter
    /// arbitrary JS, and invalidate the caller's body (the entry anchor keeps
    /// the mapping alive). Register windows live in the pinned register-stack
    /// slab, so `caller_regs` stays valid across the nested dispatch;
    /// receiver and argument handles are re-read from the traced window after
    /// every allocating step.
    ///
    /// # Safety-adjacent contract
    /// Rebuild an inlined callee's interpreter frame at a deopt exit.
    ///
    /// The optimized code has already written the caller's registers back into
    /// its window and is about to hand control to the interpreter, but the
    /// callee body it had spliced in owes the interpreter a frame of its own.
    ///
    /// Rather than reproduce the call's frame setup — the upvalue spine, `this`,
    /// argument binding, the register window — this rewinds the caller to its
    /// call and runs the interpreter's own `Op::Call` path over the restored
    /// window. The frame that comes out is by construction the frame a real call
    /// would have produced, including the advanced caller PC and the return
    /// register the callee's eventual return writes through.
    ///
    /// The frame is then fast-forwarded to `callee_pc`, the instruction the
    /// exit names, and its register-window base is returned so emitted code can
    /// restore the callee's registers into it.
    ///
    /// # Safety
    /// `stack`'s top frame must be the caller, with its registers already
    /// restored, and `call_pc` must name an `Op::Call` in the caller's body.
    pub unsafe fn jit_deopt_reify_inlined_frame(
        &mut self,
        context: &ExecutionContext,
        stack: &mut HoltStack,
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
        if code_block.op(instruction) != otter_bytecode::Op::Call {
            return Err(VmError::InvalidOperand);
        }
        stack[caller_index].pc = call_pc;
        let operands = code_block.operand_view(instruction);
        self.do_call(stack, context, operands)?;
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

    /// `caller_regs` is the caller's live register window (`JitCtx.regs`);
    /// compiled code guarantees the destination/receiver/argument registers
    /// are in bounds for that window.
    #[allow(clippy::not_unsafe_ptr_arg_deref)]
    #[allow(clippy::too_many_arguments)]
    pub fn jit_runtime_call_method_in_place(
        &mut self,
        context: &ExecutionContext,
        stack: &mut HoltStack,
        frame_index: usize,
        dst_reg: u16,
        recv_reg: u16,
        name_idx: u32,
        site: usize,
        arg_regs: &[u16],
        caller_regs: *mut Value,
    ) -> Result<bool, VmError> {
        self.record_jit_runtime_stub_class(native_abi::RuntimeStubClass::Reentrant);
        self.jit_runtime_stats.runtime_method_stubs = self
            .jit_runtime_stats
            .runtime_method_stubs
            .saturating_add(1);
        // A parked partial `Function.prototype.bind` continuation on this
        // frame re-enters through the interpreter's opcode branch only.
        if self
            .frame_cold(&stack[frame_index])
            .is_some_and(|cold| cold.pending_bind_function.is_some())
        {
            return Ok(false);
        }
        // SAFETY: `recv_reg` is a compiler-emitted index into the caller window.
        let recv = unsafe { *caller_regs.add(recv_reg as usize) };
        if recv.is_nullish() || recv.is_generator() || recv.is_iterator() {
            return Ok(false);
        }
        let caller_fid = stack[frame_index].function_id;
        // Resolve through the same layers the interpreter uses: the per-site
        // method IC for ordinary objects, then the full receiver-family walk
        // (prototype chain, primitive intrinsic prototypes, proxy [[Get]]).
        let Some(name) = context.string_constant_str_for_function(caller_fid, name_idx) else {
            return Ok(false);
        };
        let mut method = None;
        if let Some(obj) = recv.as_object()
            && let Some(key) = context.property_atom_for_function(caller_fid, name_idx)
        {
            method = self.resolve_method_ic(obj, key, site);
        }
        if method.is_none() {
            method = self.get_method_value_for_call(context, stack, recv, name)?;
        }
        let Some(method) = method else {
            return Err(self.err_unknown_intrinsic(name.to_string().into()));
        };
        if !self.is_callable_runtime(&method) {
            return Err(VmError::NotCallable);
        }
        // The resolved method is the one live handle no traced storage
        // holds; anchor it so the feedback capture below (which may
        // allocate) and a moving scavenge cannot strand it.
        let method_anchor = self.push_iteration_anchor(method) - 1;
        // Method-inline feedback, mirroring the interpreter's
        // `Op::CallMethodValue` arm: capture the receiver/prototype layout
        // while the pre-call handle is valid, record it only for a bytecode
        // target after the call completes.
        let recv = unsafe { *caller_regs.add(recv_reg as usize) };
        let method_fid = method
            .as_closure(&self.gc_heap)
            .map(|closure| closure.function_id())
            .or_else(|| method.as_function());
        let method_site = match method_fid {
            Some(_) if !self.method_site_feedback_saturated(site) => {
                self.method_site_for_receiver(context, caller_fid, name_idx, recv)
            }
            _ => None,
        };
        // Re-read every handle after the allocating capture step: the
        // receiver and arguments from the traced window, the method from
        // its anchor slot (a moving scavenge rewrites both in place).
        let method = self.iteration_anchor(method_anchor);
        let recv = unsafe { *caller_regs.add(recv_reg as usize) };
        let mut args: SmallVec<[Value; 8]> = SmallVec::with_capacity(arg_regs.len());
        for &arg in arg_regs {
            // SAFETY: compiler-emitted argument indices into the caller window.
            args.push(unsafe { *caller_regs.add(arg as usize) });
        }
        let result = self.run_callable_sync_already_rooted(context, &method, recv, args);
        self.pop_iteration_anchors_to(method_anchor);
        let result = result?;
        if let (Some(method_fid), Some(method_site)) = (method_fid, method_site) {
            self.note_method_target(site, method_fid, method_site);
        }
        // SAFETY: `dst_reg` is a compiler-emitted index into the caller
        // window; the window slab is pinned, so the pointer survived the
        // nested dispatch.
        unsafe {
            *caller_regs.add(dst_reg as usize) = result;
        }
        Ok(true)
    }

    /// Complete one full plain `Call` in place for a compiled caller whose
    /// direct-call prepare reported an ineligible callee (native, bound, or
    /// a bytecode function outside the direct-call plan).
    ///
    /// The interpreter's `Op::Call` has no exotic receiver branches — its
    /// semantics are exactly "call the callee value with `undefined` as
    /// `this`" — so any callable completes here through
    /// [`Self::run_callable_sync_already_rooted`] under the caller's
    /// published activation. A non-callable value reports `Ok(false)` and
    /// side-exits, keeping the interpreter the owner of the thrown error.
    /// On `Ok(true)` the destination register holds the call result.
    ///
    /// # Safety-adjacent contract
    /// `caller_regs` is the caller's live register window (`JitCtx.regs`);
    /// compiled code guarantees the destination/callee/argument registers
    /// are in bounds for that window.
    #[allow(clippy::not_unsafe_ptr_arg_deref)]
    pub fn jit_runtime_call_in_place(
        &mut self,
        context: &ExecutionContext,
        dst_reg: u16,
        callee_reg: u16,
        arg_regs: &[u16],
        caller_regs: *mut Value,
    ) -> Result<bool, VmError> {
        self.record_jit_runtime_stub_class(native_abi::RuntimeStubClass::Reentrant);
        self.jit_runtime_stats.runtime_calls =
            self.jit_runtime_stats.runtime_calls.saturating_add(1);
        // SAFETY: `callee_reg` is a compiler-emitted index into the caller window.
        let callee = unsafe { *caller_regs.add(callee_reg as usize) };
        if !self.is_callable_runtime(&callee) {
            return Ok(false);
        }
        let mut args: SmallVec<[Value; 8]> = SmallVec::with_capacity(arg_regs.len());
        for &arg in arg_regs {
            // SAFETY: compiler-emitted argument indices into the caller window.
            args.push(unsafe { *caller_regs.add(arg as usize) });
        }
        let result =
            self.run_callable_sync_already_rooted(context, &callee, Value::undefined(), args)?;
        // SAFETY: `dst_reg` is a compiler-emitted index into the caller
        // window; the window slab is pinned, so the pointer survived the
        // nested dispatch.
        unsafe {
            *caller_regs.add(dst_reg as usize) = result;
        }
        Ok(true)
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
    /// [`Self::run_construct_sync_already_rooted`] (the caller already holds an
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
        let result = self.run_construct_sync_already_rooted(context, &callee, callee, args)?;
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
        let eq = self.loose_equal_with_context(context, &lhs, &rhs)?;
        // SAFETY: `dst_reg` is a compiler-emitted index into the caller
        // window; the window slab is pinned, so the pointer survived any
        // nested coercion dispatch.
        unsafe {
            *caller_regs.add(dst_reg as usize) = Value::boolean(eq ^ negate);
        }
        Ok(())
    }

    /// Complete a coercive `ToPrimitive` or `ToNumeric` opcode in place for a
    /// compiled caller. `numeric` selects §7.1.3; otherwise `hint_index`
    /// identifies the compiler-emitted `default`/`number`/`string` token.
    ///
    /// The source, intermediate primitive, and result live in the high-level
    /// handle arena across user `@@toPrimitive`/`valueOf`/`toString` reentry.
    /// The published caller window remains authoritative and receives the
    /// result only after the complete abstract operation succeeds.
    ///
    /// # Safety-adjacent contract
    /// `caller_regs` is the caller's live traced register window and both
    /// register indices are compiler-validated for that window.
    #[allow(clippy::not_unsafe_ptr_arg_deref)]
    #[allow(clippy::too_many_arguments)]
    pub fn jit_runtime_coerce_unary_in_place(
        &mut self,
        context: &ExecutionContext,
        dst_reg: u16,
        src_reg: u16,
        numeric: bool,
        hint_index: u32,
        function_id: u32,
        caller_regs: *mut Value,
    ) -> Result<(), VmError> {
        self.record_jit_runtime_stub_class(native_abi::RuntimeStubClass::Reentrant);
        self.with_handle_scope(|interp, scope| {
            // SAFETY: `src_reg` is a compiler-emitted index into the published
            // caller window.
            let input = interp.scoped_value(scope, unsafe { *caller_regs.add(src_reg as usize) });
            let hint = if numeric {
                abstract_ops::ToPrimitiveHint::Number
            } else {
                let token = context
                    .string_constant_str_for_function(function_id, hint_index)
                    .ok_or(VmError::InvalidOperand)?;
                abstract_ops::ToPrimitiveHint::from_token(token).ok_or(VmError::InvalidOperand)?
            };
            let current = interp.escape_scoped(input);
            let primitive = if abstract_ops::is_primitive(&current) {
                current
            } else {
                interp.evaluate_to_primitive(context, &current, hint)?
            };
            let primitive = interp.scoped_value(scope, primitive);
            let primitive_value = interp.escape_scoped(primitive);
            let result = if !numeric || primitive_value.is_number() || primitive_value.is_big_int()
            {
                primitive_value
            } else if primitive_value.is_symbol() {
                return Err(interp
                    .err_type(("Cannot convert a Symbol value to a number".to_string()).into()));
            } else {
                Value::number(crate::number::NumberValue::from_f64(
                    crate::number::parse::to_number_value(&primitive_value, &interp.gc_heap),
                ))
            };
            let result = interp.scoped_value(scope, result);
            // SAFETY: `dst_reg` is a compiler-emitted index into the pinned
            // caller window; resolve the handle after every possible GC.
            unsafe {
                *caller_regs.add(dst_reg as usize) = interp.escape_scoped(result);
            }
            Ok(())
        })
    }


    /// Resume a *frameless* self-recursive JIT callee that bailed mid-execution.
    ///
    /// The inline self-call ([`crate::baseline`]) runs a self-recursive callee in
    /// compiled code with its register window in the flat register stack and no
    /// `HoltStack` frame. On a bail it has no frame to resume, so this rebuilds
    /// one: it attaches the live top window (`regcount` slots) to a materialized
    /// interpreter [`Frame`] (self-recursion ⇒
    /// the function id and upvalue spine come from the caller frame at
    /// `caller_frame_index`), resumes the interpreter at `bail_pc`, and returns
    /// the callee's completion value (the caller's compiled code stores it).
    pub fn jit_self_call_bail(
        &mut self,
        context: &ExecutionContext,
        stack: &mut HoltStack,
        caller_frame_index: usize,
        bail_pc: u32,
        regcount: usize,
    ) -> Result<Value, VmError> {
        let window = self.register_stack.top_window(regcount)?;

        let caller = stack
            .get(caller_frame_index)
            .ok_or(VmError::InvalidOperand)?;
        let fid = caller.function_id;
        let upvalues = caller.upvalues.clone();
        self.note_jit_entry_bail(fid);
        let function = context.exec_function(fid).ok_or(VmError::InvalidOperand)?;
        let mut frame = Frame::with_exec_return_upvalues_and_this(
            function,
            None,
            upvalues,
            Value::undefined(),
            window,
        );
        frame.pc = bail_pc;
        let initial_stack_len = stack.len();
        stack.push(frame);
        let result = self.dispatch_loop(context, stack);
        while stack.len() > initial_stack_len {
            if let Some(mut frame) = stack.pop() {
                self.frame_release_cold(&mut frame);
                self.reclaim_registers(&mut frame);
            }
        }
        result
    }

    /// Resume a *stack* of inlined callee frames after a guard inside a nested
    /// (recursively spliced) callee body fails. The compiled caller stays live;
    /// this rebuilds the whole inline chain in the interpreter — the outermost
    /// inlined method at the bottom, the guard's method on top — and runs it to
    /// completion. The outermost frame's completion bubbles out of the dispatch
    /// loop (its `return_register` is `None`) and is returned to emitted code,
    /// which stores it into the compiled call's destination; each inner frame's
    /// `return_register` names the parent-frame register the interpreter writes
    /// when that frame returns, so the chain unwinds exactly as a real call would.
    ///
    /// `frames` is ordered outermost first. Every frame's `registers` slice is a
    /// full register window (unwritten slots `undefined`) and its `pc` is the
    /// logical PC to resume at — the guard's PC for the top frame, the PC just
    /// past the nested call for each frame below it.
    pub fn jit_resume_inline_callee_stack(
        &mut self,
        context: &ExecutionContext,
        stack: &mut HoltStack,
        frames: &[jit::JitResumeFrame],
    ) -> Result<Value, VmError> {
        let _window_rollback = self.register_window_rollback();
        let initial_stack_len = stack.len();
        // Build every frame before pushing any: an invalid function id then
        // returns cleanly without leaving a half-pushed reentry on the stack.
        let mut built: smallvec::SmallVec<[Frame; 4]> = smallvec::SmallVec::new();
        for (i, f) in frames.iter().enumerate() {
            if std::env::var_os("OTTER_JIT_TRACE").is_some() {
                eprintln!(
                    "[jit-trace] resume inline stack frame {i}/{} fid={} pc={}",
                    frames.len(),
                    f.callee_fid,
                    f.callee_pc
                );
            }
            let function = context
                .exec_function(f.callee_fid)
                .ok_or(VmError::InvalidOperand)?;
            // A body that reads an upvalue resumes with its method closure's
            // captured spine; one that reads none carries `undefined` here and
            // resumes with an empty spine.
            let upvalues: crate::frame_state::UpvalueSpine =
                match f.closure.as_closure(&self.gc_heap) {
                    Some(c) => self
                        .gc_heap
                        .read_payload(c.handle, |body| body.upvalues.clone().into_boxed_slice()),
                    None => Vec::new().into_boxed_slice(),
                };
            let mut window = self.alloc_reg_window(f.registers.len())?;
            window.copy_from_slice(&f.registers);
            // The bottom (outermost inlined) frame bubbles its result out of the
            // dispatch loop; every frame above it returns into its parent.
            let return_register = if i == 0 {
                None
            } else {
                Some(f.return_register)
            };
            let mut frame = Frame::with_exec_return_upvalues_and_this(
                function,
                return_register,
                upvalues,
                f.this,
                window,
            );
            frame.pc = f.callee_pc;
            built.push(frame);
        }
        self.enter_sync_reentry()?;
        for frame in built {
            stack.push(frame);
        }
        let result = self.dispatch_loop(context, stack);
        self.leave_sync_reentry();
        while stack.len() > initial_stack_len {
            if let Some(mut frame) = stack.pop() {
                self.frame_release_cold(&mut frame);
                self.reclaim_registers(&mut frame);
            }
        }
        result
    }

    /// JIT bridge — build the closure for a `MakeFunction` from compiled code,
    /// writing it into register `dst` of frame `frame_index` (self-reference
    /// capture and upvalue binding go through the normal interpreter path).
    ///
    /// # Errors
    /// Propagates closure-construction errors and `InvalidOperand` for an
    /// out-of-range frame index.
    pub fn jit_runtime_make_function(
        &mut self,
        context: &ExecutionContext,
        stack: &mut HoltStack,
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

    /// JIT bridge — boxed `Value` bits of frame `frame_index`'s `this` binding,
    /// read once at compiled-entry setup so a `LoadThis` is a direct `JitCtx`
    /// read. A hole (`this` not yet initialized in a derived constructor)
    /// surfaces verbatim; the emitter guards against it and bails to the
    /// interpreter, which owns the derived-constructor resolution and throw.
    #[must_use]
    pub fn jit_frame_this_bits(&self, stack: &HoltStack, frame_index: usize) -> u64 {
        match stack.get(frame_index) {
            Some(frame) => frame.this_value.to_bits(),
            None => Value::undefined().to_bits(),
        }
    }

    /// JIT bridge — boxed `Value` bits of frame `frame_index`'s SELF closure,
    /// computed once at compiled-entry setup. A `MakeFunction` of the running
    /// function (the named-function self binding) resolves to exactly this
    /// value, so the emitter reads it from `JitCtx` instead of crossing back
    /// into Rust per call. Mirrors the self branch of
    /// [`Self::run_make_function_reg`]: the frame's recorded closure instance
    /// when present, else the bare interned function value.
    #[must_use]
    pub fn jit_frame_self_closure_bits(&self, stack: &HoltStack, frame_index: usize) -> u64 {
        let Some(frame) = stack.get(frame_index) else {
            return Value::undefined().to_bits();
        };
        // A frame that never acquired cold state has no recorded closure
        // instance, so the self binding is the bare interned function value.
        let value = match self.frame_cold(frame).and_then(|cold| cold.callee_closure) {
            Some(closure) => Value::closure(closure),
            None => Value::function(frame.function_id),
        };
        value.to_bits()
    }

    /// JIT bridge — base pointer of frame `frame_index`'s register window, for
    /// the compiled entry to address registers. The window is rooted on
    /// `stack`, so the pointer is stable for the compiled call's duration
    /// (recursive compiled calls append frames to this reservation-stable
    /// HoltStack, whose buffer does not reallocate before the stack-depth guard).
    #[must_use]
    pub fn jit_frame_regs_ptr(stack: &mut HoltStack, frame_index: usize) -> *mut u64 {
        stack[frame_index].registers.as_mut_ptr().cast::<u64>()
    }

    /// Raw base of frame `frame_index`'s upvalue spine (`Box<[UpvalueCell]>`
    /// data, each a 4-byte compressed `Gc<UpvalueCellBody>` handle), or `0`
    /// when the frame captures nothing. Emitted `LoadUpvalue`/`StoreUpvalue`
    /// read the cell handle at `base + idx*4`, decompress it, and access the
    /// cell body's single `Value`. The spine `Box` is owned by the frame and
    /// stays put for the frame's life (the cells themselves are old-space, so
    /// they never move); a `0` base routes the op to the runtime stub.
    #[must_use]
    pub fn jit_frame_upvalues_ptr(stack: &HoltStack, frame_index: usize) -> usize {
        let upvalues = &stack[frame_index].upvalues;
        if upvalues.is_empty() {
            0
        } else {
            upvalues.as_ptr() as usize
        }
    }
}
