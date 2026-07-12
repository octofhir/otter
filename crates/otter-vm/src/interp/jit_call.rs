//! JIT entry, OSR, and direct-call frame plumbing.
//!
//! # Contents
//! Tier-up dispatch (`maybe_dispatch_jit`, backedge/OSR accounting),
//! compiled-frame entry (`run_compiled_frame`, `jit_runtime_call`),
//! direct-call and direct-method-call preparation/finish/abort, the
//! per-fid direct-method inline cache, and raw frame-pointer accessors
//! the emitted code reads (`jit_frame_regs_ptr` and friends).
//!
//! # Invariants
//! Every publish of a callee frame is paired with a finish/abort helper
//! that releases pinned code and the sync-reentry guard; bail paths must
//! leave the frame stack exactly as the interpreter expects to resume.
#![allow(unused_imports)]
use crate::*;

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
        let Some(code) = self.resolve_jit_code(stack, context, top_idx) else {
            return Ok(None);
        };
        match self.run_compiled_frame(stack, context, top_idx, &code) {
            jit::JitExecOutcome::Bailed(pc) => {
                let fid = stack[top_idx].function_id;
                Self::trace_jit_bail(context, fid, "entry", None, pc);
                stack[top_idx].pc = pc;
                if !self.reoptimize_arith_overflow_bail(context, fid, pc) {
                    self.note_jit_entry_bail(fid);
                }
                Ok(None)
            }
            jit::JitExecOutcome::Returned(value) => {
                self.note_jit_entry_success(stack[top_idx].function_id);
                let popped = self.return_running_finally(stack, value)?;
                Ok(Some(popped))
            }
            jit::JitExecOutcome::Threw(err) => {
                if matches!(err, VmError::Uncaught)
                    && let Some(thrown) = self.pending_uncaught_throw.take()
                {
                    self.pending_uncaught_frames = Some(snapshot_frames(context, stack));
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
                    self.pending_uncaught_frames = Some(snapshot_frames(context, stack));
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
    /// branched to). Compiles the function (if needed) and enters compiled code
    /// at the header so the rest of the loop runs natively.
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
        // Resolve compiled code, compiling once and caching the result (shared
        // with the function-entry path; `None` records an uncompilable body).
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
            // This header's OSR region is uncompilable (its slice holds an
            // unsupported opcode — e.g. an object-valued `StoreElement`). The
            // region is built from `osr_pc`, so a different hot loop in the same
            // function can still compile; disable only this header, not the body.
            self.jit_osr_disabled.insert((fid, osr_pc));
            return Ok(None);
        };
        let activation = jit::VmRuntimeActivation::new(self, stack, context, top_idx);
        match code.osr_entry(activation, osr_pc) {
            // No trampoline for this header — it's not an OSR target. Disable
            // just this header and re-arm so another header can still tier up.
            None => {
                self.jit_osr_disabled.insert((fid, osr_pc));
                Ok(None)
            }
            Some(jit::JitExecOutcome::Bailed(pc)) => {
                // Compiled body hit a guard or unsupported opcode. Resume the
                // interpreter at the exact bail PC (committed side effects are
                // preserved). Disable this loop header only when the miss was in
                // the target loop itself. A compiled OSR slice may finish the hot
                // loop, continue through cold epilogue/outer-loop code, and bail
                // there; that should not permanently suppress the header on the
                // next hot iteration.
                Self::trace_jit_bail(context, fid, "osr", Some(osr_pc), pc);
                stack[top_idx].pc = pc;
                if self.reoptimize_arith_overflow_bail(context, fid, pc) {
                    return Ok(None);
                }
                if Self::osr_bail_inside_target_loop(context, fid, osr_pc, pc) {
                    self.jit_osr_disabled.insert((fid, osr_pc));
                }
                Ok(None)
            }
            Some(jit::JitExecOutcome::Returned(value)) => {
                let popped = self.return_running_finally(stack, value)?;
                Ok(Some(popped))
            }
            Some(jit::JitExecOutcome::Threw(err)) => Err(err),
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
        let Some(feedback) = function.feedback_at(instr.instruction_pc as usize) else {
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
        self.jit_code.remove(&fid);
        self.jit_entry_osr_only.remove(&fid);
        self.jit_code_cache = None;
        self.clear_jit_direct_method_cache_for_fid(fid);
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
        let Some(code) = self.resolve_jit_code(stack, context, top_idx) else {
            return Ok(None);
        };
        match self.run_compiled_frame(stack, context, top_idx, &code) {
            jit::JitExecOutcome::Bailed(pc) => {
                let fid = stack[top_idx].function_id;
                Self::trace_jit_bail(context, fid, "sync-entry", None, pc);
                stack[top_idx].pc = pc;
                if !self.reoptimize_arith_overflow_bail(context, fid, pc) {
                    self.note_jit_entry_bail(fid);
                }
                Ok(None)
            }
            jit::JitExecOutcome::Returned(value) => {
                self.note_jit_entry_success(stack[top_idx].function_id);
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
        // Single-entry compiled-code cache. A hot synchronous re-entry (Array
        // callbacks, comparators, `@@iterator` drives) resolves the SAME callee
        // every call; this skips the `jit_code` FxHashMap lookup + `Arc` clone
        // churn when the last resolve matched. The cache only ever holds
        // non-`osr_only` code (populated below + by `jit_resolve_compiled_cached`),
        // so it needs no further filtering.
        if let Some((cached_fid, code)) = &self.jit_code_cache
            && *cached_fid == fid
            && code.metadata().is_compatible_with_current_vm()
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
            let count = {
                let counter = self.jit_call_counts.entry(fid).or_insert(0);
                *counter = counter.saturating_add(1);
                *counter
            };
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
        let code = code.filter(|c| c.metadata().is_compatible_with_current_vm() && !c.osr_only());
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
        // SAFETY: the raw pointers are formed from this method's own live
        // borrows (`self`, `stack`, `context`) and are valid for the duration
        // of `run_entry`; the JIT does not retain them, and we do not touch
        // those borrows again until `run_entry` returns.
        let activation = jit::VmRuntimeActivation::new(self, stack, context, top_idx);
        code.run_entry(activation)
    }

    /// Resolve `fid`'s installed non-OSR baseline body through the single-entry
    /// monomorphic cache, falling back to the [`Self::jit_code`] map probe and
    /// refreshing the cache on a hit. Returns `None` when no compiled body is
    /// installed yet, the body is OSR-only, or the function was marked
    /// uncompilable — every such case defers to the full re-entry path so the
    /// normal tier-up counter keeps advancing.
    pub(crate) fn jit_resolve_compiled_cached(
        &mut self,
        fid: u32,
    ) -> Option<std::sync::Arc<dyn jit::JitFunctionCode>> {
        if let Some((cached_fid, code)) = &self.jit_code_cache
            && *cached_fid == fid
            && code.metadata().is_compatible_with_current_vm()
        {
            return Some(code.clone());
        }
        let code = self.jit_code.get(&fid)?.clone()?;
        if code.osr_only() || !code.metadata().is_compatible_with_current_vm() {
            return None;
        }
        self.jit_code_cache = Some((fid, code.clone()));
        Some(code)
    }

    pub(crate) fn jit_direct_call_plan_for(
        function: &crate::executable::CodeBlock,
        code: &dyn jit::JitFunctionCode,
    ) -> Option<jit::JitDirectCallPlan> {
        if !code.metadata().is_compatible_with_current_vm() || code.safepoint_count() != 0 {
            return None;
        }
        Some(jit::JitDirectCallPlan {
            function_id: function.id,
            entry_addr: code.entry_addr()?,
            param_count: function.param_count,
            register_count: function.register_count,
        })
    }

    /// Drop only the cached direct-method ways whose callee is `fid`. A
    /// recompile/invalidation of one
    /// function only invalidates the entries that point at *its* code (stale entry
    /// address); wiping the whole cache instead makes a call site with churning
    /// receiver feedback re-resolve and re-install every call (the cache is cleared
    /// out from under it before it can be hit).
    pub(crate) fn clear_jit_direct_method_cache_for_fid(&mut self, fid: u32) {
        for set in &mut self.jit_direct_method_cache {
            set.retain(|c| c.function_id != fid);
        }
    }

    pub(crate) fn pin_jit_direct_code(
        &mut self,
        frame_index: usize,
        code: std::sync::Arc<dyn jit::JitFunctionCode>,
    ) {
        self.jit_direct_code_anchors.push((frame_index, code));
    }

    pub(crate) fn release_jit_direct_code_at(&mut self, frame_index: usize) {
        if let Some(pos) = self
            .jit_direct_code_anchors
            .iter()
            .rposition(|(idx, _)| *idx == frame_index)
        {
            self.jit_direct_code_anchors.swap_remove(pos);
        }
    }

    pub(crate) fn release_jit_direct_code_from(&mut self, frame_index: usize) {
        self.jit_direct_code_anchors
            .retain(|(idx, _)| *idx < frame_index);
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn prepare_jit_direct_call_frame(
        &mut self,
        _context: &ExecutionContext,
        stack: &mut HoltStack,
        frame_index: usize,
        function: &crate::executable::CodeBlock,
        parent_upvalues: crate::frame_state::UpvalueSpine,
        this0: Value,
        plan: jit::JitDirectCallPlan,
        // `Some(reg)` for `Op::Call` (the callee closure lives in a caller
        // register and is re-read post-allocation for the named-function SELF
        // binding). `None` for the method-call path, where the callee is the
        // IC-resolved method value, not a caller register — that path bails
        // eligibility on `makes_function`, so no SELF re-read is needed.
        callee_reg: Option<u16>,
        arg_regs: &[u16],
        // Base of the CALLER's live register window (`JitCtx.regs`). For a
        // framed caller this equals `stack[frame_index].registers`; for a
        // frameless caller (register-CC direct call) it is the reg-stack window
        // the caller runs on, which `frame_index` does NOT point at. Reading the
        // caller's argument registers from this window is correct in both cases
        // and is what emitted code uses (x19). The window lives in the fixed,
        // GC-traced `reg_stack` (never reallocated) or in the caller frame that
        // cannot move during its own compiled execution, so the pointer is stable
        // across the `draw_registers` / `build_upvalues_for_exec` below.
        caller_regs: *const Value,
    ) -> Result<jit::JitPreparedDirectCall, VmError> {
        let bind_count = usize::from(plan.param_count).min(arg_regs.len());
        let _ = frame_index;
        // SAFETY: `caller_regs` is the caller's live register base; `arg_regs` /
        // `callee_reg` are compiler-emitted indices into it (the same indices the
        // caller's compiled body addresses off x19), so each `add` is in bounds.
        let read_caller = |reg: u16| -> Value { unsafe { *caller_regs.add(reg as usize) } };
        let upvalues = if function.own_upvalue_count == 0 {
            parent_upvalues
        } else {
            Frame::build_upvalues_for_exec(&mut self.gc_heap, function, parent_upvalues)?
        };
        let this_for_callee = self.this_for_bytecode_call_runtime_rooted(function, this0, &[])?;
        let window_rollback = self.register_window_rollback();
        let window = self.alloc_reg_window(usize::from(plan.register_count))?;
        let mut callee_frame =
            HoltCallReservation::from_frame(Frame::with_exec_return_upvalues_and_this(
                function,
                None,
                upvalues,
                this_for_callee,
                window,
            ));
        let callee_now = {
            for (dst_slot, &src) in callee_frame
                .frame_mut()
                .registers
                .iter_mut()
                .zip(arg_regs.iter())
                .take(bind_count)
            {
                *dst_slot = read_caller(src);
            }
            callee_reg.map(read_caller)
        };
        let self_closure = if function.makes_function
            && let Some(closure) = callee_now.and_then(|v| v.as_closure(&self.gc_heap))
        {
            self.frame_ensure_cold(callee_frame.frame_mut())
                .callee_closure = Some(closure);
            Some(closure)
        } else {
            None
        };

        // Compute the entry bits from local state rather than re-indexing the
        // segmented frame stack three more times after publishing. The SELF
        // binding mirrors `jit_frame_self_closure_bits` (recorded closure, else
        // the bare interned function value); `this` is the already-resolved
        // receiver; the upvalue base is the spine's stable `Box` data pointer,
        // unchanged by publishing. This is the hot per-call path for compiled
        // direct calls (recursion, property-using callees).
        let self_closure_bits = match self_closure {
            Some(closure) => Value::closure(closure).to_bits(),
            None => Value::function(function.id).to_bits(),
        };
        let this_bits = this_for_callee.to_bits();
        let upvalues_ptr = {
            let spine = &callee_frame.frame_mut().upvalues;
            if spine.is_empty() {
                0
            } else {
                spine.as_ptr() as usize
            }
        };

        let frame_desc = callee_frame.publish(stack);
        window_rollback.commit();
        Ok(jit::JitPreparedDirectCall {
            entry_addr: plan.entry_addr,
            regs: frame_desc.register_window().as_mut_ptr().cast::<u64>(),
            self_closure: self_closure_bits,
            this_value: this_bits,
            frame_index: frame_desc.index(),
            upvalues_ptr,
        })
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

    pub(crate) fn cached_direct_method_value(
        &self,
        recv: crate::object::JsObject,
        hit: &JitDirectMethodHit,
    ) -> Option<Value> {
        match *hit {
            JitDirectMethodHit::Own(slot) => {
                object::load_plain_shaped_own_data_slot_hit(recv, &self.gc_heap, slot)
            }
            JitDirectMethodHit::DirectPrototype {
                receiver_shape_id,
                prototype_hit,
            } => {
                if object::shape_id(recv, &self.gc_heap) != receiver_shape_id {
                    return None;
                }
                let proto = object::prototype(recv, &self.gc_heap)?;
                object::load_plain_shaped_own_data_slot_hit(proto, &self.gc_heap, prototype_hit)
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn try_prepare_cached_direct_method_call(
        &mut self,
        context: &ExecutionContext,
        stack: &mut HoltStack,
        frame_index: usize,
        recv: Value,
        obj: crate::object::JsObject,
        site: usize,
        arg_regs: &[u16],
        caller_regs: *const Value,
    ) -> Result<Option<jit::JitPreparedDirectCall>, VmError> {
        // Polymorphic cache: find the entry whose cached receiver shape still
        // resolves the same method. A miss just falls to the generic path — the
        // other cached shapes stay, so a site that alternates shapes does not
        // thrash and re-resolve every call.
        let Some(set) = self.jit_direct_method_cache.get(site) else {
            return Ok(None);
        };
        let mut resolved = None;
        for cache in set {
            if self.cached_direct_method_value(obj, &cache.hit) == Some(cache.method_value) {
                resolved = Some((cache.method_value, cache.function_id, cache.code.clone()));
                break;
            }
        }
        let Some((method_value, function_id, code)) = resolved else {
            return Ok(None);
        };
        // Derive the callee's upvalue spine and `this` from the resolved method
        // value. A plain-function method carries no upvalues; a closure method
        // (the common shape for a prototype method that captures module state)
        // supplies its captured spine here — the reason this fast path exists is
        // to skip the slow path's IC walk and method-site feedback, not to
        // restrict itself to upvalue-free methods.
        let Ok((target_fid, parent_upvalues, this0, new_target, derived_this, eval_env)) =
            Self::bytecode_call_target_parts(method_value, recv, &self.gc_heap)
        else {
            return Ok(None);
        };
        if target_fid != function_id
            || new_target.is_some()
            || derived_this.is_some()
            || eval_env.is_some()
        {
            return Ok(None);
        }
        let Some(function) = context.exec_function(function_id) else {
            return Ok(None);
        };
        let Some(plan) = Self::jit_direct_call_plan_for(function, code.as_ref()) else {
            return Ok(None);
        };

        self.enter_sync_reentry()?;
        match self.prepare_jit_direct_call_frame(
            context,
            stack,
            frame_index,
            function,
            parent_upvalues,
            this0,
            plan,
            None,
            arg_regs,
            caller_regs,
        ) {
            Ok(prepared) => {
                self.pin_jit_direct_code(prepared.frame_index, code);
                self.jit_runtime_stats.direct_calls =
                    self.jit_runtime_stats.direct_calls.saturating_add(1);
                Ok(Some(prepared))
            }
            Err(err) => {
                self.leave_sync_reentry();
                Err(err)
            }
        }
    }

    pub(crate) fn install_jit_direct_method_cache(
        &mut self,
        site: usize,
        obj: crate::object::JsObject,
        key: crate::property_atom::AtomizedPropertyKey<'_>,
        method: Value,
        function_id: u32,
        code: std::sync::Arc<dyn jit::JitFunctionCode>,
    ) {
        let method_is_plain_function = method == Value::function(function_id);
        // A closure method only caches at a monomorphic site. A polymorphic receiver
        // family (one `arr[i].run()` site over sibling classes) exposes no single
        // stable own/direct-prototype stub to cache, so the shape-walk loop below
        // would find nothing and re-run every call for no benefit — reject it up
        // front (as cheaply as the pre-cache path did) and leave it on the uniform
        // slow path. Plain-function methods keep their existing mono/poly caching.
        if !method_is_plain_function
            && self.load_property_ics.get(site).map(|e| e.entry_count()) != Some(1)
        {
            return;
        }
        let method_fid = method
            .as_function()
            .or_else(|| method.as_closure(&self.gc_heap).map(|c| c.function_id()));
        if method_fid != Some(function_id) {
            return;
        }
        // A saturated site whose receiver shape is not already one of the cached
        // ways is megamorphic: installing cannot help (the way budget is full and a
        // new shape would be dropped), so skip the receiver/prototype shape walk
        // below and leave those calls on the uniform slow path. Without this a
        // megamorphic site pays the full resolve loop on every call.
        let recv_shape_id = object::shape_id(obj, &self.gc_heap);
        if let Some(set) = self.jit_direct_method_cache.get(site)
            && set.len() >= MAX_DIRECT_METHOD_WAYS
            && !set.iter().any(|c| c.cached_shape_id() == recv_shape_id)
        {
            return;
        }
        let Some(entry) = self.load_property_ics.get(site) else {
            return;
        };
        // A megamorphic load IC (the receiver family exceeds the IC's shape
        // budget — e.g. one `arr[i].run()` site driven with both a 4-shape and a
        // 6-shape array) exposes no monomorphic own/direct-prototype stub to cache
        // from, so the shape-walk loop below would find nothing on every call.
        // Bail to the uniform slow path, matching the pre-cache behaviour.
        if entry.is_megamorphic() {
            return;
        }
        let mut cached_hit = None;
        for stub in entry.entries() {
            if let Some(hit) = stub.own_data_hit()
                && object::load_own_data_slot_atom(obj, &self.gc_heap, key, hit) == Some(method)
                && object::load_plain_shaped_own_data_slot_hit(obj, &self.gc_heap, hit)
                    == Some(method)
            {
                cached_hit = Some(JitDirectMethodHit::Own(hit));
                break;
            }
            if let Some((receiver_shape_id, prototype_hit)) = stub.direct_prototype_load()
                && object::shape_id(obj, &self.gc_heap) == receiver_shape_id
                && let Some(proto) = object::prototype(obj, &self.gc_heap)
                && object::load_own_data_slot_atom(proto, &self.gc_heap, key, prototype_hit)
                    == Some(method)
                && object::load_plain_shaped_own_data_slot_hit(proto, &self.gc_heap, prototype_hit)
                    == Some(method)
            {
                cached_hit = Some(JitDirectMethodHit::DirectPrototype {
                    receiver_shape_id,
                    prototype_hit,
                });
                break;
            }
        }
        if let Some(hit) = cached_hit {
            let new_shape = match &hit {
                JitDirectMethodHit::Own(h) => h.shape_id,
                JitDirectMethodHit::DirectPrototype {
                    receiver_shape_id, ..
                } => *receiver_shape_id,
            };
            if let Some(set) = self.jit_direct_method_cache.get_mut(site) {
                let entry = JitDirectMethodCache {
                    hit,
                    function_id,
                    method_value: method,
                    code,
                };
                // Replace a same-receiver-shape entry (method reassigned) in place;
                // otherwise append while the site stays within the way budget.
                let pos = set.iter().position(|c| {
                    let s = match &c.hit {
                        JitDirectMethodHit::Own(h) => h.shape_id,
                        JitDirectMethodHit::DirectPrototype {
                            receiver_shape_id, ..
                        } => *receiver_shape_id,
                    };
                    s == new_shape
                });
                match pos {
                    Some(i) => set[i] = entry,
                    None if set.len() < MAX_DIRECT_METHOD_WAYS => set.push(entry),
                    None => {}
                }
            }
        }
    }

    /// Prepare an eligible compiled **method** callee (`recv.name(args…)`) for
    /// direct machine-code entry, the `CallMethodValue` analogue of
    /// [`Self::jit_prepare_direct_call`].
    ///
    /// Resolves the method through the call site's monomorphic load IC (only the
    /// IC-cacheable own/direct-prototype data-slot case; anything else returns
    /// `Ok(None)`), then publishes a callee frame bound with `this = recv`.
    /// Returns `Ok(None)` for any cold / ineligible / non-object-receiver case so
    /// the emitted site falls back to the in-place full method-call stub (not a
    /// bail — a native/polymorphic method in a hot loop must keep running
    /// compiled). On `Ok(Some(_))` the callee frame is published and the
    /// sync-reentry guard is held until a direct-call finish/abort helper runs.
    ///
    /// # Errors
    /// Propagates a sync-reentry stack-depth overflow or a frame-build failure.
    ///
    /// # Safety contract
    /// `caller_regs` must point at the caller's live register window
    /// (`JitCtx.regs`); compiled code guarantees `recv_reg`/argument registers
    /// are in bounds for that window.
    #[allow(clippy::not_unsafe_ptr_arg_deref)]
    pub fn jit_prepare_direct_method_call(
        &mut self,
        context: &ExecutionContext,
        stack: &mut HoltStack,
        frame_index: usize,
        recv_reg: u16,
        name_idx: u32,
        site: usize,
        arg_regs: &[u16],
        // Caller's live register window (`JitCtx.regs`); see
        // [`Self::prepare_jit_direct_call_frame`]. Receiver and args are read
        // from here, not `stack[frame_index]`, so a frameless caller works.
        caller_regs: *const Value,
    ) -> Result<Option<jit::JitPreparedDirectCall>, VmError> {
        self.record_jit_runtime_stub_class(native_abi::RuntimeStubClass::Alloc);
        self.jit_runtime_stats.runtime_calls =
            self.jit_runtime_stats.runtime_calls.saturating_add(1);
        // A site with a live native prototype-method IC resolved to a builtin
        // last time, which can never be a compiled direct-call target — skip
        // the IC walk and let the in-place method stub take its cached fast
        // path. The stub self-heals the IC (clearing it) when the receiver
        // family changes, so this never permanently strands a now-compiled
        // method.
        if self.method_call_ics.get(site).is_some_and(Option::is_some) {
            return Ok(None);
        }
        // SAFETY: `recv_reg` is a compiler-emitted index into the caller window.
        let recv = unsafe { *caller_regs.add(recv_reg as usize) };
        let Some(obj) = recv.as_object() else {
            return Ok(None);
        };
        if let Some(prepared) = self.try_prepare_cached_direct_method_call(
            context,
            stack,
            frame_index,
            recv,
            obj,
            site,
            arg_regs,
            caller_regs,
        )? {
            return Ok(Some(prepared));
        }
        let Some(key) =
            context.property_atom_for_function(stack[frame_index].function_id, name_idx)
        else {
            return Ok(None);
        };
        // Monomorphic IC-resolved data-slot method only; misses (accessor, deep
        // proto, polymorphic, absent) return None → in-place fallback.
        let Some(method) = self.resolve_method_ic(obj, key, site) else {
            return Ok(None);
        };
        let Ok((function_id, parent_upvalues, this0, new_target, derived_this, eval_env)) =
            Self::bytecode_call_target_parts(method, recv, &self.gc_heap)
        else {
            return Ok(None);
        };
        if new_target.is_some() || derived_this.is_some() || eval_env.is_some() {
            return Ok(None);
        }
        let Some(function) = context.exec_function(function_id) else {
            return Ok(None);
        };
        if function.is_generator
            || function.is_async
            || function.is_async_generator
            || function.needs_arguments
            || function.has_rest
            || function.contains_direct_eval
            || function.is_derived_constructor
            // The method path carries no caller register for the callee, so it
            // cannot re-root the closure post-allocation for a named-function
            // SELF binding; bail those to the in-place fallback.
            || function.makes_function
        {
            return Ok(None);
        }
        let Some(code) = self.jit_resolve_compiled_cached(function_id) else {
            return Ok(None);
        };
        let Some(plan) = Self::jit_direct_call_plan_for(function, code.as_ref()) else {
            return Ok(None);
        };
        self.install_jit_direct_method_cache(site, obj, key, method, function_id, code.clone());

        self.enter_sync_reentry()?;
        match self.prepare_jit_direct_call_frame(
            context,
            stack,
            frame_index,
            function,
            parent_upvalues,
            this0,
            plan,
            None,
            arg_regs,
            caller_regs,
        ) {
            Ok(prepared) => {
                self.pin_jit_direct_code(prepared.frame_index, code.clone());
                self.jit_runtime_stats.direct_calls =
                    self.jit_runtime_stats.direct_calls.saturating_add(1);
                Ok(Some(prepared))
            }
            Err(err) => {
                self.leave_sync_reentry();
                Err(err)
            }
        }
    }

    /// Finish a direct compiled call that returned normally.
    ///
    /// Pops and reclaims the published callee frame, stores `value` into the
    /// caller destination register, and releases the sync-reentry guard held by
    /// [`Self::jit_prepare_direct_call`].
    pub fn jit_finish_direct_call_returned(
        &mut self,
        stack: &mut HoltStack,
        caller_frame_index: usize,
        callee_frame_index: usize,
        dst: u16,
        value: Value,
    ) -> Result<(), VmError> {
        if stack.len() != callee_frame_index + 1 {
            self.release_jit_direct_code_from(callee_frame_index);
            self.leave_sync_reentry();
            return Err(VmError::InvalidOperand);
        }
        if let Some(mut done) = stack.pop() {
            self.note_jit_entry_success(done.function_id);
            self.reclaim_registers(&mut done);
        }
        self.release_jit_direct_code_at(callee_frame_index);
        let caller = stack
            .get_mut(caller_frame_index)
            .ok_or_else(|| VmError::InvalidOperand)?;
        *caller
            .registers
            .get_mut(dst as usize)
            .ok_or_else(|| VmError::InvalidOperand)? = value;
        self.leave_sync_reentry();
        Ok(())
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

    /// Finish a direct compiled call whose callee bailed to the interpreter.
    ///
    /// Resumes the interpreter at `bail_pc` inside the already-published callee
    /// frame, stores the resulting completion into the caller destination, and
    /// releases the sync-reentry guard.
    pub fn jit_finish_direct_call_bailed(
        &mut self,
        context: &ExecutionContext,
        stack: &mut HoltStack,
        caller_frame_index: usize,
        callee_frame_index: usize,
        dst: u16,
        bail_pc: u32,
    ) -> Result<(), VmError> {
        if callee_frame_index >= stack.len() {
            self.release_jit_direct_code_from(callee_frame_index);
            self.leave_sync_reentry();
            return Err(VmError::InvalidOperand);
        }
        self.release_jit_direct_code_at(callee_frame_index);
        self.note_jit_entry_bail(stack[callee_frame_index].function_id);
        stack[callee_frame_index].pc = bail_pc;
        match self.dispatch_loop(context, stack) {
            Ok(value) => {
                let caller = stack
                    .get_mut(caller_frame_index)
                    .ok_or_else(|| VmError::InvalidOperand)?;
                *caller
                    .registers
                    .get_mut(dst as usize)
                    .ok_or_else(|| VmError::InvalidOperand)? = value;
                self.leave_sync_reentry();
                Ok(())
            }
            Err(err) => {
                self.leave_sync_reentry();
                Err(err)
            }
        }
    }

    /// Abort a prepared direct call before normal return completion.
    ///
    /// Used by direct-call throw/bail paths: drops the callee frame and any
    /// nested frames above it, then releases the sync-reentry guard.
    pub fn jit_abort_direct_call(&mut self, stack: &mut HoltStack, callee_frame_index: usize) {
        self.truncate_frame_stack_reclaiming(stack, callee_frame_index);
        self.release_jit_direct_code_from(callee_frame_index);
        self.leave_sync_reentry();
    }

    #[inline]
    pub(crate) fn truncate_frame_stack_reclaiming(&mut self, stack: &mut HoltStack, len: usize) {
        while stack.len() > len {
            if let Some(mut frame) = stack.pop() {
                self.reclaim_registers(&mut frame);
            }
        }
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
