//! Unified compiled-callee frame construction.
//!
//! # Contents
//! - [`ResolvedCallTarget`] — one resolved plain or method bytecode target.
//! - [`Interpreter::prepare_jit_resolved_call`] — the single transaction that
//!   enters synchronous re-entry, publishes the callee frame, and pins its code.
//!
//! # Invariants
//! - Target resolution and guard checks finish before this module is entered;
//!   this module never performs property lookup or executable-code selection.
//! - The exact [`jit::JitFunctionCode`] owner remains live until the prepared
//!   record is returned. Native linkage then leases its stable entry cell for
//!   the complete machine-code dynamic extent; no frame-indexed `Arc` side
//!   table participates in call lifetime.
//! - Any frame-build error rolls back synchronous re-entry before it escapes.
//! - Dynamic closure state (upvalues, `this`, and SELF) belongs to this call and
//!   is never retained in a call-site cache.
//!
//! # See also
//! - [`super::resolve`] for plain/method target resolution.
//! - [`super::finish`] for paired finish/abort handling.
//! - [`crate::jit::JitPreparedDirectCall`] for the emitted-code staging record.

use std::sync::Arc;

use crate::*;

/// A bytecode call target after semantic resolution and compiled-code selection.
///
/// Plain calls and method calls differ only in how they produce this record.
/// Frame publication consumes the same immutable function metadata, dynamic
/// per-call state, entry plan, and code-lifetime anchor for both call kinds.
pub(crate) struct ResolvedCallTarget<'a> {
    /// Authoritative executable metadata for frame construction.
    pub(crate) function: &'a crate::executable::CodeBlock,
    /// Captured cells derived from the current callable value.
    pub(crate) parent_upvalues: crate::frame_state::UpvalueSpine,
    /// Effective receiver after callable resolution.
    pub(crate) this_value: Value,
    /// Scalar compiled-entry and frame-layout metadata.
    pub(crate) plan: jit::JitDirectCallPlan,
    /// Caller register holding the plain callee, used to re-read named SELF
    /// after allocation. Method resolution carries its callable separately.
    pub(crate) callee_reg: Option<u16>,
    /// Strong lifetime anchor upgraded/selected by the resolver. It remains
    /// live until prepare returns the stable entry-cell address to generated
    /// code; the registry owns the generation and native entry takes a lease.
    pub(crate) code: Arc<dyn jit::JitFunctionCode>,
}

impl Interpreter {
    /// Publish one already-resolved compiled callee through the shared frame ABI.
    ///
    /// This is the only owner of the prepare transaction for both plain and
    /// method calls. Target resolution may return `None` before calling it, but
    /// once entered, success always means a frame and code pin exist together.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn prepare_jit_resolved_call(
        &mut self,
        stack: &mut HoltStack,
        caller_frame_index: usize,
        target: ResolvedCallTarget<'_>,
        arg_regs: &[u16],
        caller_regs: *const Value,
    ) -> Result<jit::JitPreparedDirectCall, VmError> {
        self.enter_sync_reentry()?;
        let prepared = self.prepare_jit_direct_call_frame(
            stack,
            caller_frame_index,
            target.function,
            target.parent_upvalues,
            target.this_value,
            target.plan,
            target.callee_reg,
            arg_regs,
            caller_regs,
        );
        match prepared {
            Ok(prepared) => {
                // Keep the selected owner live through publication. Once this
                // function returns, generated linkage immediately acquires the
                // registry-owned entry cell before branching to native code.
                drop(target.code);
                self.jit_runtime_stats.direct_calls =
                    self.jit_runtime_stats.direct_calls.saturating_add(1);
                Ok(prepared)
            }
            Err(err) => {
                self.leave_sync_reentry();
                Err(err)
            }
        }
    }

    /// Build and publish the VM-owned frame for one resolved compiled callee.
    ///
    /// The caller window is stable and traced for this transaction. No raw
    /// callable or upvalue pointer is retained after the frame is published.
    #[allow(clippy::too_many_arguments)]
    fn prepare_jit_direct_call_frame(
        &mut self,
        stack: &mut HoltStack,
        frame_index: usize,
        function: &crate::executable::CodeBlock,
        parent_upvalues: crate::frame_state::UpvalueSpine,
        this0: Value,
        plan: jit::JitDirectCallPlan,
        // `Some(reg)` for `Op::Call`: the callee closure lives in a caller
        // register and is re-read post-allocation for the named SELF binding.
        // Method resolution passes `None` and rejects `makes_function` targets.
        callee_reg: Option<u16>,
        arg_regs: &[u16],
        // Base of the caller's live register window (`JitCtx.regs`). It may be
        // a framed window or a frameless register-stack window; both are stable
        // across the allocations below.
        caller_regs: *const Value,
    ) -> Result<jit::JitPreparedDirectCall, VmError> {
        let bind_count = usize::from(plan.param_count).min(arg_regs.len());
        let _ = frame_index;
        // SAFETY: register ids are compiler-verified indices into caller_regs.
        let read_caller = |reg: u16| -> Value { unsafe { *caller_regs.add(reg as usize) } };
        let upvalues = if function.own_upvalue_count == 0 {
            parent_upvalues
        } else {
            // `this0` is a copied compressed handle and the callee frame is not
            // published yet. Keep it rewriteable while fresh upvalue cells may
            // trigger a moving collection. The spine builder roots its inherited
            // and newly allocated cells itself.
            let mut build_roots = |visitor: &mut dyn FnMut(*mut otter_gc::raw::RawGc)| {
                this0.trace_value_slots(visitor);
            };
            Frame::build_upvalues_for_exec_with_roots(
                &mut self.gc_heap,
                function,
                parent_upvalues,
                &mut build_roots,
            )?
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
        let frame_flags = if plan.has_safepoints {
            crate::native_abi::NativeFrameFlags::HAS_SAFEPOINTS
        } else {
            0
        };
        Ok(jit::JitPreparedDirectCall {
            entry_cell: plan.entry_cell,
            regs: frame_desc.register_window().as_mut_ptr().cast::<u64>(),
            self_closure: self_closure_bits,
            this_value: this_bits,
            frame_index: frame_desc.index(),
            upvalues_ptr,
            frame_ids: u64::from(plan.function_id) | (u64::from(plan.function_id) << 32),
            frame_meta: (u64::from(plan.register_count) << 32)
                | ((crate::native_abi::NativeFrameKind::Baseline as u64) << 48)
                | (u64::from(frame_flags) << 56),
            code_object_id: plan.code_object_id,
        })
    }
}
