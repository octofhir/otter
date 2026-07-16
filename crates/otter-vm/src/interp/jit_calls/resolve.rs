//! Direct compiled-call target resolution.
//!
//! # Contents
//! - Shared callable decoding and eligibility checks for plain and method calls.
//! - Installed-code selection and exact code/entry-plan pairing.
//! - Public VM bridges that prepare plain and method direct calls.
//!
//! # Invariants
//! - The callable value is decoded on every invocation. Closure upvalues,
//!   effective `this`, and exact SELF are never taken from a call-site cache.
//! - Inherited closure upvalues remain an allocation-neutral stable source;
//!   owner publication roots the exact callable instead of cloning its spine.
//! - A [`frame::ResolvedCallTarget`] owns the exact code `Arc` whose scalar
//!   entry plan it carries; owner publication pins that same `Arc`.
//! - Resolution returns `None` before entering synchronous re-entry; only
//!   [`frame::prepare_jit_resolved_call`] owns the prepare transaction.
//!
//! # See also
//! - [`super::cache`] for shape/slot guards and cached code-plan ownership.
//! - [`super::frame`] for the shared owner-publication transaction.

use std::sync::Arc;

use crate::*;

use super::frame;

/// Dynamic callable state decoded from the current invocation.
struct DecodedCallTarget<'a> {
    function: &'a crate::executable::CodeBlock,
    parent_upvalues: crate::upvalue_source::UpvalueSource,
    self_value: Value,
    this_value: Value,
}

impl Interpreter {
    /// Decode one current callable value and apply the eligibility policy shared
    /// by plain and method compiled calls.
    fn decode_jit_call_target<'a>(
        &self,
        context: &'a ExecutionContext,
        callable: Value,
        effective_this: Value,
        expected_fid: Option<u32>,
    ) -> Option<DecodedCallTarget<'a>> {
        let (function_id, parent_upvalues, this_value, new_target, derived_this, eval_env) =
            if let Some(function_id) = callable.as_function() {
                (
                    function_id,
                    crate::upvalue_source::UpvalueSource::empty(),
                    effective_this,
                    None,
                    None,
                    None,
                )
            } else {
                let closure = callable.as_closure(&self.gc_heap)?;
                let state = closure.call_state(&self.gc_heap);
                (
                    closure.function_id(),
                    state.upvalues,
                    state.bound_this.unwrap_or(effective_this),
                    state.bound_new_target,
                    state.bound_derived_this,
                    state.eval_env,
                )
            };
        if expected_fid.is_some_and(|expected| expected != function_id)
            || new_target.is_some()
            || derived_this.is_some()
            || eval_env.is_some()
        {
            return None;
        }

        let function = context.exec_function(function_id)?;
        if function.is_generator
            || function.is_async
            || function.is_async_generator
            || function.needs_arguments
            || function.has_rest
            || function.contains_direct_eval
            || function.is_derived_constructor
        {
            return None;
        }

        // Owner construction must not carry a cloned upvalue spine across an
        // untracked allocation. Bind every non-allocating receiver here and
        // reject the one coercing case (sloppy ordinary function + primitive
        // receiver) to the generic call path.
        let this_value = if function.is_strict || function.is_arrow || this_value.is_object_type() {
            this_value
        } else if this_value.is_undefined() || this_value.is_null() {
            Value::object(self.global_this)
        } else {
            return None;
        };

        Some(DecodedCallTarget {
            function,
            parent_upvalues,
            self_value: callable,
            this_value,
        })
    }

    /// Resolve dynamic callable state together with the best currently
    /// installed entry body and its plan. Direct calls keep advancing the
    /// shared entry counter, so a hot baseline-to-baseline edge can promote to
    /// optimizing code without first materializing an interpreter frame.
    pub(super) fn resolve_jit_call_target<'a>(
        &mut self,
        context: &'a ExecutionContext,
        callable: Value,
        effective_this: Value,
    ) -> Option<frame::ResolvedCallTarget<'a>> {
        let decoded = self.decode_jit_call_target(context, callable, effective_this, None)?;
        let (plan, code) =
            if let Some(optimized) = self.installed_optimized_direct_call_code(decoded.function) {
                optimized
            } else {
                let baseline = self.jit_resolve_compiled_cached(decoded.function.id)?;
                self.promote_direct_call_code(context, decoded.function, baseline)?
            };
        Some(frame::ResolvedCallTarget {
            function: decoded.function,
            parent_upvalues: decoded.parent_upvalues,
            self_value: decoded.self_value,
            this_value: decoded.this_value,
            plan,
            code,
        })
    }

    /// Pair freshly decoded callable state with one guarded cache entry.
    ///
    /// The cache stores only an expected function id, immutable scalar plan,
    /// and strong code owner. The callable itself is supplied by a fresh guarded
    /// slot load, so a replacement closure with the same function id contributes
    /// its own upvalues and `this` state without invalidating code selection.
    pub(super) fn resolve_cached_jit_call_target<'a>(
        &mut self,
        context: &'a ExecutionContext,
        callable: Value,
        effective_this: Value,
        expected_fid: u32,
        plan: jit::JitDirectCallPlan,
        code: Arc<dyn jit::JitFunctionCode>,
    ) -> Option<frame::ResolvedCallTarget<'a>> {
        let decoded =
            self.decode_jit_call_target(context, callable, effective_this, Some(expected_fid))?;

        // The cache installs and clones plan+owner as one record. These checks
        // document that pairing in debug builds without restoring virtual calls
        // or registry probes to the release hit path.
        debug_assert_eq!(plan.function_id, expected_fid);
        debug_assert_eq!(plan.function_id, decoded.function.id);
        debug_assert_eq!(
            Some(plan),
            self.jit_code_registry
                .direct_call_plan(decoded.function, code.as_ref())
        );
        debug_assert_eq!(plan.param_count, decoded.function.param_count);
        debug_assert_eq!(plan.register_count, decoded.function.register_count);

        let (plan, code) = if code.native_frame_kind() == native_abi::NativeFrameKind::Optimizing {
            // `cache.rs` already checked the installation epoch, and the
            // plan/code pair was installed atomically into this cache way.
            (plan, code)
        } else if let Some(optimized) = self.installed_optimized_direct_call_code(decoded.function)
        {
            optimized
        } else {
            self.promote_direct_call_code(context, decoded.function, code)?
        };

        Some(frame::ResolvedCallTarget {
            function: decoded.function,
            parent_upvalues: decoded.parent_upvalues,
            self_value: decoded.self_value,
            this_value: decoded.this_value,
            plan,
            code,
        })
    }

    /// Resolve an already installed optimizing body without sampling tier
    /// policy or touching baseline state. Once promoted, direct edges stay on
    /// this constant-time path.
    fn installed_optimized_direct_call_code(
        &mut self,
        function: &crate::executable::CodeBlock,
    ) -> Option<(jit::JitDirectCallPlan, Arc<dyn jit::JitFunctionCode>)> {
        let fid = function.id;
        let code = if let Some((cached_fid, code)) = &self.jit_optimized_code_cache
            && *cached_fid == fid
            && self.jit_code_registry.is_current_for_entry(code.as_ref())
        {
            code.clone()
        } else {
            let code = self.jit_optimized_code.get(&fid)?.as_ref()?.clone();
            if !self.jit_code_registry.is_current_for_entry(code.as_ref()) {
                return None;
            }
            self.jit_optimized_code_cache = Some((fid, code.clone()));
            code
        };
        let plan = self
            .jit_code_registry
            .direct_call_plan(function, code.as_ref())?;
        Some((plan, code))
    }

    /// Count one baseline entry, then select an optimizing body if that exact
    /// sample triggers promotion; otherwise retain `baseline`.
    fn promote_direct_call_code(
        &mut self,
        context: &ExecutionContext,
        function: &crate::executable::CodeBlock,
        baseline: Arc<dyn jit::JitFunctionCode>,
    ) -> Option<(jit::JitDirectCallPlan, Arc<dyn jit::JitFunctionCode>)> {
        self.note_jit_function_entry(function.id);
        let code = self
            .resolve_optimized_code_for_fid(context, function.id)
            .unwrap_or(baseline);
        let plan = self
            .jit_code_registry
            .direct_call_plan(function, code.as_ref())?;
        Some((plan, code))
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
    ) -> Option<Arc<dyn jit::JitFunctionCode>> {
        if let Some((cached_fid, code)) = &self.jit_code_cache
            && *cached_fid == fid
            && self.jit_code_registry.is_current_for_entry(code.as_ref())
        {
            return Some(code.clone());
        }
        let code = self.jit_code.get(&fid)?.clone()?;
        if code.osr_only() || !self.jit_code_registry.is_current_for_entry(code.as_ref()) {
            return None;
        }
        self.jit_code_cache = Some((fid, code.clone()));
        Some(code)
    }

    /// Prepare an eligible compiled **method** callee (`recv.name(args…)`) for
    /// direct machine-code entry, the `CallMethodValue` analogue of
    /// [`Self::jit_prepare_direct_call`].
    ///
    /// Resolves the method through the call site's monomorphic load IC (only the
    /// IC-cacheable own/direct-prototype data-slot case; anything else returns
    /// `Ok(None)`), then publishes callee resources bound with `this = recv`.
    /// Returns `Ok(None)` for any cold / ineligible / non-object-receiver case so
    /// the emitted site falls back to the in-place full method-call stub (not a
    /// bail — a native/polymorphic method in a hot loop must keep running
    /// compiled). On `Ok(Some(_))` the callee owner is published and the
    /// sync-reentry guard is held until a direct-call finish/abort helper runs.
    ///
    /// # Errors
    /// Propagates a sync-reentry stack-depth overflow or owner-setup failure.
    ///
    /// # Safety contract
    /// `caller_regs` must point at the caller's live register window
    /// (`JitCtx.regs`); compiled code guarantees `recv_reg`/argument registers
    /// are in bounds for that window.
    #[allow(clippy::not_unsafe_ptr_arg_deref)]
    pub fn jit_prepare_direct_method_call(
        &mut self,
        context: &ExecutionContext,
        caller_function_id: u32,
        recv_reg: u16,
        name_idx: u32,
        site: usize,
        arg_regs: &[u16],
        // Caller's live register window (`JitCtx.regs`). Receiver and args are
        // read directly from this window, so frameless callers need no parent
        // stack entry.
        caller_regs: *const Value,
    ) -> Result<Option<jit::JitPreparedDirectCall>, VmError> {
        self.record_jit_runtime_stub_class(native_abi::RuntimeStubClass::Alloc);
        self.jit_runtime_stats.runtime_calls =
            self.jit_runtime_stats.runtime_calls.saturating_add(1);
        // A site with a live native prototype-method IC resolved to a builtin
        // last time, which can never be a compiled direct-call target — skip
        // the IC walk and let the in-place method stub take its cached fast
        // path. The stub self-heals the IC when the receiver family changes.
        if self.feedback_directory.has_method_ic(site) {
            return Ok(None);
        }
        // SAFETY: `recv_reg` is a compiler-emitted index into the caller window.
        let recv = unsafe { *caller_regs.add(recv_reg as usize) };
        let Some(obj) = recv.as_object() else {
            return Ok(None);
        };
        if let Some(prepared) = self.try_prepare_cached_direct_method_call(
            context,
            recv,
            obj,
            site,
            arg_regs,
            caller_regs,
        )? {
            return Ok(Some(prepared));
        }
        let Some(key) = context.property_atom_for_function(caller_function_id, name_idx) else {
            return Ok(None);
        };
        // Monomorphic IC-resolved data-slot method only; misses (accessor, deep
        // proto, polymorphic, absent) return None → in-place fallback.
        let Some(method) = self.resolve_method_ic(obj, key, site) else {
            return Ok(None);
        };
        let Some(target) = self.resolve_jit_call_target(context, method, recv) else {
            return Ok(None);
        };
        self.install_jit_direct_method_cache(
            site,
            obj,
            key,
            method,
            target.function.id,
            target.code.clone(),
            target.plan,
        );

        self.prepare_jit_resolved_call(target, arg_regs, caller_regs)
            .map(Some)
    }

    /// Prepare a direct compiled **plain** call (`callee(args…)`) from the
    /// callee value in a caller register. Returns `Ok(None)` when the callee
    /// is not an eligible installed bytecode function — the emitted site bails
    /// to the interpreter, which runs the call with full semantics.
    ///
    /// # Safety-adjacent contract
    /// `caller_regs` is the caller's live register window
    /// (`JitCtx.regs`); compiled code guarantees `callee_reg`/argument
    /// registers are in bounds for that window.
    #[allow(clippy::not_unsafe_ptr_arg_deref)]
    pub fn jit_prepare_direct_call(
        &mut self,
        context: &ExecutionContext,
        callee_reg: u16,
        arg_regs: &[u16],
        caller_regs: *const Value,
    ) -> Result<Option<jit::JitPreparedDirectCall>, VmError> {
        self.record_jit_runtime_stub_class(native_abi::RuntimeStubClass::Alloc);
        self.jit_runtime_stats.runtime_calls =
            self.jit_runtime_stats.runtime_calls.saturating_add(1);
        // SAFETY: `callee_reg` is a compiler-emitted index into the caller window.
        let callee = unsafe { *caller_regs.add(callee_reg as usize) };
        let Some(target) = self.resolve_jit_call_target(context, callee, Value::undefined()) else {
            return Ok(None);
        };
        self.prepare_jit_resolved_call(target, arg_regs, caller_regs)
            .map(Some)
    }
}
