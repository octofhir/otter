//! Direct compiled-call target resolution.
//!
//! # Contents
//! - Shared callable decoding and eligibility checks for plain and method calls.
//! - Installed-code selection and exact code/entry-plan pairing.
//! - Public VM bridges that prepare plain and method direct calls.
//!
//! # Invariants
//! - The callable value is decoded on every invocation. Closure upvalues,
//!   effective `this`, and the caller-held SELF value are never taken from a
//!   call-site cache.
//! - A [`frame::ResolvedCallTarget`] owns the exact code `Arc` whose scalar
//!   entry plan it carries; frame publication pins that same `Arc`.
//! - Method calls reject named-SELF callees because their callable does not
//!   live in a caller register that frame construction can re-read after GC.
//! - Resolution returns `None` before entering synchronous re-entry; only
//!   [`frame::prepare_jit_resolved_call`] owns the prepare transaction.
//!
//! # See also
//! - [`super::cache`] for shape/slot guards and cached code-plan ownership.
//! - [`super::frame`] for the shared frame-publication transaction.

use std::sync::Arc;

use crate::*;

use super::frame;

/// Where a callable came from, including the rooting capability available to
/// frame construction after it performs allocations.
#[derive(Clone, Copy)]
pub(super) enum CallTargetOrigin {
    /// `Op::Call`: the caller register can be re-read for named SELF binding.
    Plain { callee_reg: u16 },
    /// `Op::CallMethodValue`: the callable was loaded from a receiver slot.
    Method,
}

impl CallTargetOrigin {
    #[inline]
    fn callee_reg(self) -> Option<u16> {
        match self {
            Self::Plain { callee_reg } => Some(callee_reg),
            Self::Method => None,
        }
    }

    #[inline]
    fn supports_named_self(self) -> bool {
        matches!(self, Self::Plain { .. })
    }
}

/// Dynamic callable state decoded from the current invocation.
struct DecodedCallTarget<'a> {
    function: &'a crate::executable::CodeBlock,
    parent_upvalues: crate::frame_state::UpvalueSpine,
    this_value: Value,
    callee_reg: Option<u16>,
}

impl Interpreter {
    /// Decode one current callable value and apply the eligibility policy shared
    /// by plain and method compiled calls.
    fn decode_jit_call_target<'a>(
        &self,
        context: &'a ExecutionContext,
        callable: Value,
        effective_this: Value,
        origin: CallTargetOrigin,
        expected_fid: Option<u32>,
    ) -> Option<DecodedCallTarget<'a>> {
        let (function_id, parent_upvalues, this_value, new_target, derived_this, eval_env) =
            Self::bytecode_call_target_parts(callable, effective_this, &self.gc_heap).ok()?;
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
            || (function.makes_function && !origin.supports_named_self())
        {
            return None;
        }

        Some(DecodedCallTarget {
            function,
            parent_upvalues,
            this_value,
            callee_reg: origin.callee_reg(),
        })
    }

    /// Resolve dynamic callable state together with the currently installed
    /// compatible baseline body and its entry plan.
    pub(super) fn resolve_jit_call_target<'a>(
        &mut self,
        context: &'a ExecutionContext,
        callable: Value,
        effective_this: Value,
        origin: CallTargetOrigin,
    ) -> Option<frame::ResolvedCallTarget<'a>> {
        let decoded =
            self.decode_jit_call_target(context, callable, effective_this, origin, None)?;
        let code = self.jit_resolve_compiled_cached(decoded.function.id)?;
        let plan = self
            .jit_code_registry
            .direct_call_plan(decoded.function, code.as_ref())?;
        Some(frame::ResolvedCallTarget {
            function: decoded.function,
            parent_upvalues: decoded.parent_upvalues,
            this_value: decoded.this_value,
            plan,
            callee_reg: decoded.callee_reg,
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
        &self,
        context: &'a ExecutionContext,
        callable: Value,
        effective_this: Value,
        origin: CallTargetOrigin,
        expected_fid: u32,
        plan: jit::JitDirectCallPlan,
        code: Arc<dyn jit::JitFunctionCode>,
    ) -> Option<frame::ResolvedCallTarget<'a>> {
        let decoded = self.decode_jit_call_target(
            context,
            callable,
            effective_this,
            origin,
            Some(expected_fid),
        )?;

        // The cache installs and clones plan+owner as one record. These checks
        // document that pairing in debug builds without restoring virtual calls
        // or registry probes to the release hit path.
        debug_assert_eq!(plan.function_id, expected_fid);
        debug_assert_eq!(plan.function_id, decoded.function.id);
        debug_assert_eq!(plan.code_object_id, code.metadata().id);
        debug_assert_eq!(
            Some(plan),
            self.jit_code_registry
                .direct_call_plan(decoded.function, code.as_ref())
        );
        debug_assert_eq!(plan.param_count, decoded.function.param_count);
        debug_assert_eq!(plan.register_count, decoded.function.register_count);

        Some(frame::ResolvedCallTarget {
            function: decoded.function,
            parent_upvalues: decoded.parent_upvalues,
            this_value: decoded.this_value,
            plan,
            callee_reg: decoded.callee_reg,
            code,
        })
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
            && self
                .jit_code_registry
                .is_compatible_for_entry(code.as_ref())
        {
            return Some(code.clone());
        }
        let code = self.jit_code.get(&fid)?.clone()?;
        if code.osr_only()
            || !self
                .jit_code_registry
                .is_compatible_for_entry(code.as_ref())
        {
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
        // Caller's live register window (`JitCtx.regs`). Receiver and args are
        // read from here, not `stack[frame_index]`, so frameless callers work.
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
        let Some(target) =
            self.resolve_jit_call_target(context, method, recv, CallTargetOrigin::Method)
        else {
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

        self.prepare_jit_resolved_call(stack, frame_index, target, arg_regs, caller_regs)
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
        stack: &mut HoltStack,
        frame_index: usize,
        callee_reg: u16,
        arg_regs: &[u16],
        caller_regs: *const Value,
    ) -> Result<Option<jit::JitPreparedDirectCall>, VmError> {
        self.record_jit_runtime_stub_class(native_abi::RuntimeStubClass::Alloc);
        self.jit_runtime_stats.runtime_calls =
            self.jit_runtime_stats.runtime_calls.saturating_add(1);
        // SAFETY: `callee_reg` is a compiler-emitted index into the caller window.
        let callee = unsafe { *caller_regs.add(callee_reg as usize) };
        let Some(target) = self.resolve_jit_call_target(
            context,
            callee,
            Value::undefined(),
            CallTargetOrigin::Plain { callee_reg },
        ) else {
            return Ok(None);
        };
        self.prepare_jit_resolved_call(stack, frame_index, target, arg_regs, caller_regs)
            .map(Some)
    }
}
