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
        let instr = context
            .exec_function(fid)
            .and_then(|function| function.instr_at_byte_pc(pc));
        let op = instr.map(|instr| instr.op());
        let operands = instr
            .map(|instr| format!("{:?}", context.exec_operands(instr)))
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
                self.reoptimize_arith_overflow_bail(context, fid, pc);
                Ok(None)
            }
            jit::JitExecOutcome::Returned(value) => {
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
        let ptrs = jit::JitReentryPtrs {
            vm: <*mut Interpreter>::cast(self),
            stack: <*mut jit::JitFrameStack>::cast(stack),
            context: <*const ExecutionContext>::cast(context),
            frame_index: top_idx,
        };
        match code.osr_entry(ptrs, osr_pc) {
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
        let Some(view) = context.jit_function_view(fid) else {
            return true;
        };
        Self::osr_bail_inside_target_loop_instructions(&view.instructions, osr_pc, bail_pc)
    }

    pub(crate) fn osr_bail_inside_target_loop_instructions(
        instructions: &[JitInstrView],
        osr_pc: u32,
        bail_pc: u32,
    ) -> bool {
        let mut loop_end = None;
        for instr in instructions {
            if !matches!(instr.op, Op::Jump | Op::JumpIfTrue | Op::JumpIfFalse) {
                continue;
            }
            let Some(otter_bytecode::Operand::Imm32(rel)) = instr.operands.first() else {
                continue;
            };
            let target = i64::from(instr.byte_pc) + 1 + i64::from(*rel);
            if target == i64::from(osr_pc) && instr.byte_pc >= osr_pc {
                loop_end = Some(loop_end.map_or(instr.byte_pc, |end: u32| end.max(instr.byte_pc)));
            }
        }
        let Some(loop_end) = loop_end else {
            return true;
        };
        osr_pc <= bail_pc && bail_pc <= loop_end
    }

    /// Treat the first compiled `Add` / `Sub` / `Mul` bail at a byte-PC as an
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
        let Some(instr) = function.instr_at_byte_pc(bail_pc) else {
            return false;
        };
        if !matches!(instr.op(), Op::Add | Op::Sub | Op::Mul) {
            return false;
        }
        if !self.jit_arith_widen_float.insert((fid, bail_pc)) {
            return false;
        }
        self.invalidate_jit_function(fid);
        true
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
                self.reoptimize_arith_overflow_bail(context, fid, pc);
                Ok(None)
            }
            jit::JitExecOutcome::Returned(value) => Ok(Some(value)),
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
        let code = code.filter(|c| !c.osr_only());
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
        let ptrs = jit::JitReentryPtrs {
            vm: <*mut Interpreter>::cast(self),
            stack: <*mut jit::JitFrameStack>::cast(stack),
            context: <*const ExecutionContext>::cast(context),
            frame_index: top_idx,
        };
        code.run_entry(ptrs)
    }

    /// JIT bridge — perform a `Call` from compiled code. Reads the callee and
    /// argument Values from frame `frame_index`'s register window, runs the
    /// callee synchronously (which may itself tier up), and writes the
    /// completion into register `dst`. Safe: all raw-pointer handling stays in
    /// the JIT crate; this side sees only ordinary references.
    ///
    /// # Errors
    /// Propagates any error the callee raises, and `InvalidOperand` for an
    /// out-of-range frame or register index.
    pub fn jit_runtime_call(
        &mut self,
        context: &ExecutionContext,
        stack: &mut jit::JitFrameStack,
        frame_index: usize,
        dst: u16,
        callee_reg: u16,
        arg_regs: &[u16],
    ) -> Result<(), VmError> {
        self.record_jit_runtime_stub_descriptor(native_abi::STUB_JIT_RUNTIME_CALL);
        self.jit_runtime_stats.runtime_calls =
            self.jit_runtime_stats.runtime_calls.saturating_add(1);
        let frame = stack
            .get(frame_index)
            .ok_or_else(|| VmError::InvalidOperand)?;
        let callee = *frame
            .registers
            .get(callee_reg as usize)
            .ok_or_else(|| VmError::InvalidOperand)?;

        // Fast monomorphic compiled→compiled path: a plain bytecode closure /
        // function with a simple signature whose baseline body is already
        // installed is entered directly, binding arguments straight from the
        // (rooted) caller window into a freshly drawn callee window — no generic
        // re-entry preamble, no intermediate argument `SmallVec`, no per-call
        // compiled-code map probe. Anything outside that shape returns `None` and
        // takes the full synchronous re-entry below (which also drives tier-up).
        if let Some(result) =
            self.try_jit_fast_call(context, stack, frame_index, callee, callee_reg, arg_regs)?
        {
            self.jit_runtime_stats.direct_calls =
                self.jit_runtime_stats.direct_calls.saturating_add(1);
            let frame = stack
                .get_mut(frame_index)
                .ok_or_else(|| VmError::InvalidOperand)?;
            *frame
                .registers
                .get_mut(dst as usize)
                .ok_or_else(|| VmError::InvalidOperand)? = result;
            return Ok(());
        }

        self.jit_runtime_stats.rust_call_fallbacks =
            self.jit_runtime_stats.rust_call_fallbacks.saturating_add(1);
        let frame = stack
            .get(frame_index)
            .ok_or_else(|| VmError::InvalidOperand)?;
        let mut args: SmallVec<[Value; 8]> = SmallVec::with_capacity(arg_regs.len());
        for &r in arg_regs {
            args.push(
                *frame
                    .registers
                    .get(r as usize)
                    .ok_or_else(|| VmError::InvalidOperand)?,
            );
        }
        let result =
            self.run_callable_sync_already_rooted(context, &callee, Value::undefined(), args)?;
        let frame = stack
            .get_mut(frame_index)
            .ok_or_else(|| VmError::InvalidOperand)?;
        *frame
            .registers
            .get_mut(dst as usize)
            .ok_or_else(|| VmError::InvalidOperand)? = result;
        Ok(())
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
        {
            return Some(code.clone());
        }
        let code = self.jit_code.get(&fid)?.clone()?;
        if code.osr_only() {
            return None;
        }
        self.jit_code_cache = Some((fid, code.clone()));
        Some(code)
    }

    pub(crate) fn jit_direct_call_plan_for(
        function: &crate::executable::ExecutableFunction,
        code: &dyn jit::JitFunctionCode,
    ) -> Option<jit::JitDirectCallPlan> {
        let (safepoint_records, safepoint_count) = code.safepoint_table();
        Some(jit::JitDirectCallPlan {
            function_id: function.id,
            entry_addr: code.entry_addr()?,
            safepoint_records,
            safepoint_count,
            param_count: function.param_count,
            register_count: function.register_count,
        })
    }

    /// Drop only the cached direct-method ways whose callee is `fid`, and re-mirror
    /// the flat table for the sites they occupied. A recompile/invalidation of one
    /// function only invalidates the entries that point at *its* code (stale entry
    /// address); wiping the whole cache instead makes a call site with churning
    /// receiver feedback re-resolve and re-install every call (the cache is cleared
    /// out from under it before it can be hit).
    pub(crate) fn clear_jit_direct_method_cache_for_fid(&mut self, fid: u32) {
        for site_idx in 0..self.jit_direct_method_cache.len() {
            let set = &mut self.jit_direct_method_cache[site_idx];
            let before = set.len();
            set.retain(|c| c.function_id != fid);
            if set.len() == before {
                continue;
            }
            let base = site_idx * MAX_DIRECT_METHOD_WAYS;
            for way in 0..MAX_DIRECT_METHOD_WAYS {
                let value = self.jit_direct_method_cache[site_idx]
                    .get(way)
                    .map(|c| c.inline)
                    .unwrap_or(JitDirectMethodInline::EMPTY);
                if let Some(flat) = self.jit_direct_method_inline_slots.get_mut(base + way) {
                    *flat = value;
                }
            }
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
        stack: &mut jit::JitFrameStack,
        frame_index: usize,
        function: &crate::executable::ExecutableFunction,
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
        let registers = self.draw_registers(usize::from(plan.register_count));
        let mut callee_frame = HoltCallReservation::from_frame(Frame::with_exec_registers(
            function,
            None,
            upvalues,
            this_for_callee,
            registers,
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
        Ok(jit::JitPreparedDirectCall {
            entry_addr: plan.entry_addr,
            regs: frame_desc.value_slots().as_mut_ptr().cast::<u64>(),
            safepoint_records: plan.safepoint_records,
            safepoint_count: plan.safepoint_count,
            self_closure: self_closure_bits,
            this_value: this_bits,
            frame_index: frame_desc.index(),
            upvalues_ptr,
        })
    }

    /// Prepare an eligible compiled callee for direct machine-code entry.
    ///
    /// Returns `Ok(None)` for cold/ineligible callees; the caller can then bail
    /// or take a non-direct fallback. On `Ok(Some(_))`, the callee frame is
    /// published and the sync-reentry guard is held until the caller invokes one
    /// of the direct-call finish/abort helpers.
    pub fn jit_prepare_direct_call(
        &mut self,
        context: &ExecutionContext,
        stack: &mut jit::JitFrameStack,
        frame_index: usize,
        callee_reg: u16,
        arg_regs: &[u16],
    ) -> Result<Option<jit::JitPreparedDirectCall>, VmError> {
        self.record_jit_runtime_stub_descriptor(native_abi::STUB_JIT_PREPARE_DIRECT_CALL);
        self.jit_runtime_stats.runtime_calls =
            self.jit_runtime_stats.runtime_calls.saturating_add(1);
        let frame = stack
            .get(frame_index)
            .ok_or_else(|| VmError::InvalidOperand)?;
        let callee = *frame
            .registers
            .get(callee_reg as usize)
            .ok_or_else(|| VmError::InvalidOperand)?;
        let Ok((function_id, parent_upvalues, this0, new_target, derived_this, eval_env)) =
            Self::bytecode_call_target_parts(callee, Value::undefined(), &self.gc_heap)
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
        {
            return Ok(None);
        }
        let Some(code) = self.jit_resolve_compiled_cached(function_id) else {
            return Ok(None);
        };
        let Some(plan) = Self::jit_direct_call_plan_for(function, code.as_ref()) else {
            return Ok(None);
        };

        self.enter_sync_reentry()?;
        // Op::Call reaches here only from a framed caller (a frameless callee
        // makes no plain call — see ExecutableFunction::is_leaf), so the caller's
        // register window is its HoltStack frame's register array.
        let caller_regs = stack
            .get(frame_index)
            .map_or(std::ptr::null(), |f| f.registers.as_ptr());
        match self.prepare_jit_direct_call_frame(
            context,
            stack,
            frame_index,
            function,
            parent_upvalues,
            this0,
            plan,
            Some(callee_reg),
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
        stack: &mut jit::JitFrameStack,
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
        entry_addr: usize,
        register_count: u32,
        upvalues_ptr: usize,
        // `true` when the callee allocates no OWN upvalue cells
        // (`own_upvalue_count == 0`): the callee's spine is exactly its captured
        // spine, so the emitted call can pass a closure's live spine (or the
        // baked plain-fn value) verbatim without the bridge's
        // `build_upvalues_for_exec`. A callee that builds own cells stays on the
        // bridge.
        frameless_upvalues_ok: bool,
    ) {
        // A closure method is eligible for the bridge-free asm flat-table link
        // (machine-code callee window + branch to the compiled entry, no VM frame
        // build) when it builds no own upvalue cells — the emitted call reads its
        // captured spine LIVE from the resolved closure body. A bare function is
        // always eligible.
        let method_is_plain_function = method == Value::function(function_id);
        let asm_link_eligible = method_is_plain_function || frameless_upvalues_ok;
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
            // Derive the inline-link fields from the already-resolved hit (no
            // second shape walk): the receiver-shape guard, the prototype-shape
            // guard and method-slot byte offset for the identity walk, plus the
            // callee entry / window / SELF / upvalue-spine the emitted call needs.
            let cv = std::mem::size_of::<crate::value::compressed::CompressedValue>() as u32;
            // SELF bits = the resolved method value (closure or bare function);
            // `makes_function` callees are rejected upstream so SELF is never
            // read, but keeping it exact is free. The baked `upvalues_ptr` is used
            // only when the runtime method is a bare function (`fid_immediate`
            // emit path); a closure hit reads its spine LIVE, so it is inert for a
            // closure. A closure that builds OWN upvalue cells is not
            // `asm_link_eligible` and keeps the empty slot (bridge).
            let self_closure_bits = method.to_bits();
            let inline = if !asm_link_eligible {
                JitDirectMethodInline::EMPTY
            } else {
                match &hit {
                    JitDirectMethodHit::Own(h) => JitDirectMethodInline {
                        entry_addr,
                        register_count,
                        recv_shape_offset: h.shape.offset(),
                        proto_shape_offset: 0,
                        method_on_receiver: 1,
                        method_value_byte: u32::from(h.slot) * cv,
                        method_fid: function_id,
                        self_closure_bits,
                        upvalues_ptr,
                    },
                    JitDirectMethodHit::DirectPrototype { prototype_hit, .. } => {
                        JitDirectMethodInline {
                            entry_addr,
                            register_count,
                            recv_shape_offset: object::shape(obj, &self.gc_heap).offset(),
                            proto_shape_offset: prototype_hit.shape.offset(),
                            method_on_receiver: 0,
                            method_value_byte: u32::from(prototype_hit.slot) * cv,
                            method_fid: function_id,
                            self_closure_bits,
                            upvalues_ptr,
                        }
                    }
                }
            };
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
                    inline,
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
                // Mirror every cached way into the flat table the JIT walks.
                let base = site * MAX_DIRECT_METHOD_WAYS;
                for way in 0..MAX_DIRECT_METHOD_WAYS {
                    let value = self
                        .jit_direct_method_cache
                        .get(site)
                        .and_then(|s| s.get(way))
                        .map(|c| c.inline)
                        .unwrap_or(JitDirectMethodInline::EMPTY);
                    if let Some(flat) = self.jit_direct_method_inline_slots.get_mut(base + way) {
                        *flat = value;
                    }
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
        stack: &mut jit::JitFrameStack,
        frame_index: usize,
        recv_reg: u16,
        name_idx: u32,
        call_byte_pc: u32,
        site: usize,
        arg_regs: &[u16],
        // Caller's live register window (`JitCtx.regs`); see
        // [`Self::prepare_jit_direct_call_frame`]. Receiver and args are read
        // from here, not `stack[frame_index]`, so a frameless caller works.
        caller_regs: *const Value,
    ) -> Result<Option<jit::JitPreparedDirectCall>, VmError> {
        self.record_jit_runtime_stub_descriptor(native_abi::STUB_JIT_PREPARE_DIRECT_METHOD_CALL);
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
        if self.jit_hook.is_some() {
            let caller_fid = stack
                .get(frame_index)
                .ok_or_else(|| VmError::InvalidOperand)?
                .function_id;
            if !self.method_site_feedback_saturated(caller_fid, call_byte_pc)
                && let Some(site) =
                    self.method_site_for_receiver(context, caller_fid, name_idx, recv)
            {
                self.note_method_target(caller_fid, call_byte_pc, function_id, site);
            }
        }
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
        // The method closure's upvalue spine (constant for a shared prototype
        // method) for the inline direct-call link the install derives.
        let upvalues_ptr = if parent_upvalues.is_empty() {
            0
        } else {
            parent_upvalues.as_ptr() as usize
        };
        // Frameless closure eligibility: (1) builds no own upvalue cells, so its
        // captured spine passes verbatim (no `build_upvalues_for_exec`), and
        // (2) is a leaf — a non-leaf callee's nested call would read a
        // `JitCtx.frame_index` frame the frameless callee does not own.
        let frameless_upvalues_ok = function.own_upvalue_count == 0 && function.is_leaf;
        self.install_jit_direct_method_cache(
            site,
            obj,
            key,
            method,
            function_id,
            code.clone(),
            plan.entry_addr,
            u32::from(plan.register_count),
            upvalues_ptr,
            frameless_upvalues_ok,
        );

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
        stack: &mut jit::JitFrameStack,
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
    /// one: it copies the live top window (`regcount` slots), pops it off the
    /// register stack, materializes an interpreter [`Frame`] (self-recursion ⇒
    /// the function id and upvalue spine come from the caller frame at
    /// `caller_frame_index`), resumes the interpreter at `bail_pc`, and returns
    /// the callee's completion value (the caller's compiled code stores it).
    pub fn jit_self_call_bail(
        &mut self,
        context: &ExecutionContext,
        stack: &mut jit::JitFrameStack,
        caller_frame_index: usize,
        bail_pc: u32,
        regcount: usize,
    ) -> Result<Value, VmError> {
        let base = self
            .reg_top
            .checked_sub(regcount)
            .ok_or(VmError::InvalidOperand)?;
        let mut registers: smallvec::SmallVec<[Value; 8]> =
            smallvec::SmallVec::with_capacity(regcount);
        registers.extend_from_slice(&self.reg_stack[base..base + regcount]);
        self.reg_top = base;

        let caller = stack
            .get(caller_frame_index)
            .ok_or(VmError::InvalidOperand)?;
        let fid = caller.function_id;
        let upvalues = caller.upvalues.clone();
        let function = context.exec_function(fid).ok_or(VmError::InvalidOperand)?;
        let mut frame =
            Frame::with_exec_registers(function, None, upvalues, Value::undefined(), registers);
        frame.pc = bail_pc;
        stack.push(frame);
        self.dispatch_loop(context, stack)
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
    /// byte-PC to resume at — the guard's PC for the top frame, the byte-PC just
    /// past the nested call for each frame below it.
    pub fn jit_resume_inline_callee_stack(
        &mut self,
        context: &ExecutionContext,
        stack: &mut jit::JitFrameStack,
        frames: &[jit::JitResumeFrame],
    ) -> Result<Value, VmError> {
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
            let registers: smallvec::SmallVec<[Value; 8]> =
                smallvec::SmallVec::from_slice(&f.registers);
            // The bottom (outermost inlined) frame bubbles its result out of the
            // dispatch loop; every frame above it returns into its parent.
            let return_register = if i == 0 {
                None
            } else {
                Some(f.return_register)
            };
            let mut frame =
                Frame::with_exec_registers(function, return_register, upvalues, f.this, registers);
            frame.pc = f.callee_pc;
            built.push(frame);
        }
        self.enter_sync_reentry()?;
        for frame in built {
            stack.push(frame);
        }
        let result = self.dispatch_loop(context, stack);
        self.leave_sync_reentry();
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
        stack: &mut jit::JitFrameStack,
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
    pub fn jit_abort_direct_call(
        &mut self,
        stack: &mut jit::JitFrameStack,
        callee_frame_index: usize,
    ) {
        self.truncate_frame_stack_reclaiming(stack, callee_frame_index);
        self.release_jit_direct_code_from(callee_frame_index);
        self.leave_sync_reentry();
    }

    #[inline]
    pub(crate) fn truncate_frame_stack_reclaiming(
        &mut self,
        stack: &mut jit::JitFrameStack,
        len: usize,
    ) {
        while stack.len() > len {
            if let Some(mut frame) = stack.pop() {
                self.reclaim_registers(&mut frame);
            }
        }
    }

    /// Attempt the fast compiled→compiled call. Returns `Ok(Some(value))` when
    /// the callee was a simple, already-compiled bytecode target and ran to
    /// completion; `Ok(None)` when the callee falls outside the fast shape (the
    /// caller then takes the generic re-entry path); `Err` on a callee throw or
    /// a synchronous-re-entry stack-depth overflow.
    ///
    /// GC discipline mirrors [`Self::run_callable_sync_inner`]'s bytecode
    /// branch exactly — upvalue spine built, sloppy-`this` coercion (which never
    /// allocates here because the JIT call binding is always `undefined`), then
    /// the register window drawn — and re-reads the closure handle from the
    /// rooted caller slot when recording the per-instance SELF binding, so an
    /// allocation in between cannot leave it dangling.
    pub(crate) fn try_jit_fast_call(
        &mut self,
        context: &ExecutionContext,
        stack: &mut jit::JitFrameStack,
        frame_index: usize,
        callee: Value,
        callee_reg: u16,
        arg_regs: &[u16],
    ) -> Result<Option<Value>, VmError> {
        // Plain bytecode target only — bound functions, proxies, class
        // constructors, and natives carry distinct Value tags and miss here.
        let Ok((function_id, parent_upvalues, this0, new_target, derived_this, eval_env)) =
            Self::bytecode_call_target_parts(callee, Value::undefined(), &self.gc_heap)
        else {
            return Ok(None);
        };
        // No captured new.target / derived-this cell / inherited eval env.
        if new_target.is_some() || derived_this.is_some() || eval_env.is_some() {
            return Ok(None);
        }
        let Some(function) = context.exec_function(function_id) else {
            return Ok(None);
        };
        // Simple signature only: generators / async suspend, `arguments` / rest
        // need the cold-frame machinery, direct eval needs an eval env, and a
        // class constructor must reject [[Call]] through the generic path.
        if function.is_generator
            || function.is_async
            || function.is_async_generator
            || function.needs_arguments
            || function.has_rest
            || function.contains_direct_eval
            || function.is_derived_constructor
        {
            return Ok(None);
        }
        let Some(code) = self.jit_resolve_compiled_cached(function_id) else {
            return Ok(None);
        };
        let Some(direct_plan) = Self::jit_direct_call_plan_for(function, code.as_ref()) else {
            return Ok(None);
        };

        // Committed: this consumes native stack like any nested call, so it is
        // bounded by the same synchronous-re-entry guard.
        self.enter_sync_reentry()?;
        let outcome = self.run_jit_fast_call_committed(
            context,
            stack,
            frame_index,
            function,
            parent_upvalues,
            this0,
            direct_plan,
            &code,
            callee_reg,
            arg_regs,
        );
        self.leave_sync_reentry();
        outcome.map(Some)
    }

    /// Build the callee frame for the fast call and run its compiled body,
    /// continuing on the interpreter if it bails. Split out so the
    /// synchronous-re-entry guard in [`Self::try_jit_fast_call`] brackets exactly
    /// this work.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn run_jit_fast_call_committed(
        &mut self,
        context: &ExecutionContext,
        stack: &mut jit::JitFrameStack,
        frame_index: usize,
        function: &crate::executable::ExecutableFunction,
        parent_upvalues: crate::frame_state::UpvalueSpine,
        this0: Value,
        direct_plan: jit::JitDirectCallPlan,
        code: &std::sync::Arc<dyn jit::JitFunctionCode>,
        callee_reg: u16,
        arg_regs: &[u16],
    ) -> Result<Value, VmError> {
        // Run the callee on the caller's own stack, in place. Every `HoltStack`
        // is reservation-stable (reserves `DEFAULT_MAX_STACK_DEPTH`; the
        // stack-overflow guard — already entered above via `enter_sync_reentry` —
        // fires before the reservation is exhausted), so appending the callee
        // frame never reallocates and the compiled caller's in-register frame
        // pointer stays valid across the re-entry. The compiled caller is the top
        // frame, so the callee lands directly above it at `idx`. This is the only
        // path: no private re-entry stack, no fallback. It also keeps the callee's
        // register window a precise GC root throughout its compiled body — the
        // appended frame is covered by the enclosing `dispatch_loop`'s
        // `trace_active_frame_roots` provider, which `run_compiled_frame` alone
        // does not install.
        let caller_regs = stack
            .get(frame_index)
            .map_or(std::ptr::null(), |f| f.registers.as_ptr());
        let prepared = self.prepare_jit_direct_call_frame(
            context,
            stack,
            frame_index,
            function,
            parent_upvalues,
            this0,
            direct_plan,
            Some(callee_reg),
            arg_regs,
            caller_regs,
        )?;
        let idx = prepared.frame_index;
        match self.run_compiled_frame(stack, context, idx, code) {
            jit::JitExecOutcome::Returned(value) => {
                if let Some(mut done) = stack.pop() {
                    self.reclaim_registers(&mut done);
                }
                Ok(value)
            }
            // The compiled body returned with its frame (and any nested callee
            // frames) still appended; drop back to the caller before propagating
            // so the stack is left exactly as found.
            jit::JitExecOutcome::Threw(err) => {
                self.truncate_frame_stack_reclaiming(stack, idx);
                Err(err)
            }
            // `new_frame` carries `return_register = None`, so resuming the
            // interpreter pops it (and returns its completion) when it finishes —
            // bounded to this appended frame, never unwinding the caller frames
            // below it.
            jit::JitExecOutcome::Bailed(pc) => {
                stack[idx].pc = pc;
                self.dispatch_loop(context, stack)
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
        stack: &mut jit::JitFrameStack,
        frame_index: usize,
        dst: u16,
        idx: u32,
    ) -> Result<(), VmError> {
        // `self` and `stack` are disjoint, so the two `&mut` are non-aliasing.
        let frame = stack
            .get_mut(frame_index)
            .ok_or_else(|| VmError::InvalidOperand)?;
        self.run_make_function_reg(context, frame, dst, idx)
    }

    /// JIT bridge — boxed `Value` bits of frame `frame_index`'s `this` binding,
    /// read once at compiled-entry setup so a `LoadThis` is a direct `JitCtx`
    /// read. A hole (`this` not yet initialized in a derived constructor)
    /// surfaces verbatim; the emitter guards against it and bails to the
    /// interpreter, which owns the derived-constructor resolution and throw.
    #[must_use]
    pub fn jit_frame_this_bits(&self, stack: &jit::JitFrameStack, frame_index: usize) -> u64 {
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
    pub fn jit_frame_self_closure_bits(
        &self,
        stack: &jit::JitFrameStack,
        frame_index: usize,
    ) -> u64 {
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
    pub fn jit_frame_regs_ptr(stack: &mut jit::JitFrameStack, frame_index: usize) -> *mut u64 {
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
    pub fn jit_frame_upvalues_ptr(stack: &jit::JitFrameStack, frame_index: usize) -> usize {
        let upvalues = &stack[frame_index].upvalues;
        if upvalues.is_empty() {
            0
        } else {
            upvalues.as_ptr() as usize
        }
    }
}
