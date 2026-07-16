//! Frameless compiled-callee resource construction.
//!
//! # Contents
//! - [`ResolvedCallTarget`] — one resolved plain or method bytecode target.
//! - [`Interpreter::prepare_jit_resolved_call`] — the single transaction that
//!   enters synchronous re-entry and publishes a compact native-call owner.
//!
//! # Invariants
//! - Target resolution and guard checks finish before this module is entered;
//!   this module never performs property lookup or executable-code selection.
//! - The exact [`jit::JitFunctionCode`] owner remains live until the prepared
//!   record is returned. Native linkage then leases its stable entry cell for
//!   the complete machine-code dynamic extent.
//! - Any setup error rolls back both the register cursor and synchronous
//!   re-entry before it escapes.
//! - Dynamic closure state (borrowed upvalues, `this`, and exact SELF) belongs
//!   to this call and is never retained in a call-site cache.
//! - Successful preparation constructs no [`Frame`] and touches no
//!   [`HoltStack`]. Generated code publishes the returned owner id in its
//!   stack-local [`NativeFrame`] before the next possible GC boundary.
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
/// Owner publication consumes the same immutable function metadata, dynamic
/// per-call state, entry plan, and code-lifetime anchor for both call kinds.
pub(crate) struct ResolvedCallTarget<'a> {
    /// Authoritative executable metadata for owner construction.
    pub(crate) function: &'a crate::executable::CodeBlock,
    /// Allocation-neutral captured cells derived from the current callable.
    pub(crate) parent_upvalues: crate::upvalue_source::UpvalueSource,
    /// Exact callable instance. It is rooted through every preparation
    /// allocation and published as native SELF, keeping borrowed cells alive.
    pub(crate) self_value: Value,
    /// Effective receiver after non-allocating direct-call binding. Sloppy
    /// primitive receivers are rejected to the generic path before this point.
    pub(crate) this_value: Value,
    /// Scalar compiled-entry and frame-layout metadata.
    pub(crate) plan: jit::JitDirectCallPlan,
    /// Strong lifetime anchor upgraded/selected by the resolver. It remains
    /// live until prepare returns the stable entry-cell address to generated
    /// code; the registry owns the generation and native entry takes a lease.
    pub(crate) code: Arc<dyn jit::JitFunctionCode>,
}

impl Interpreter {
    /// Publish one already-resolved compiled callee through the native owner ABI.
    ///
    /// This is the only owner of the prepare transaction for both plain and
    /// method calls. Target resolution may return `None` before calling it, but
    /// once entered, success always means a resource owner and code pin exist
    /// together. No materialized frame is created on this path.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn prepare_jit_resolved_call(
        &mut self,
        target: ResolvedCallTarget<'_>,
        arg_regs: &[u16],
        caller_regs: *const Value,
    ) -> Result<jit::JitPreparedDirectCall, VmError> {
        let tier = target.code.native_frame_kind();
        self.enter_sync_reentry()?;
        let prepared = self.prepare_jit_direct_call_owner(
            target.function,
            target.parent_upvalues,
            target.self_value,
            target.this_value,
            target.plan,
            tier,
            arg_regs,
            caller_regs,
        );
        match prepared {
            Ok(prepared) => {
                // Keep selected code live through owner publication. Generated
                // linkage immediately acquires its registry-owned entry cell
                // before branching to native code.
                drop(target.code);
                Ok(prepared)
            }
            Err(err) => {
                self.leave_sync_reentry();
                Err(err)
            }
        }
    }

    /// Build and publish the VM-owned resources for one compiled callee.
    ///
    /// The caller window is stable and traced for this transaction. Every
    /// operation that can trigger GC completes before the owner is pushed; the
    /// returned raw pointers remain backed by that owner until finish or bail.
    #[allow(clippy::too_many_arguments)]
    fn prepare_jit_direct_call_owner(
        &mut self,
        function: &crate::executable::CodeBlock,
        parent_upvalues: crate::upvalue_source::UpvalueSource,
        mut self_value: Value,
        mut this_for_callee: Value,
        plan: jit::JitDirectCallPlan,
        tier: crate::native_abi::NativeFrameKind,
        arg_regs: &[u16],
        // Base of the caller's live register window (`JitCtx.regs`). It may be
        // a framed window or a frameless register-stack window; both are stable
        // across the allocations below.
        caller_regs: *const Value,
    ) -> Result<jit::JitPreparedDirectCall, VmError> {
        let bind_count = usize::from(plan.param_count).min(arg_regs.len());
        // SAFETY: register ids are compiler-verified indices into caller_regs.
        let read_caller = |reg: u16| -> Value { unsafe { *caller_regs.add(reg as usize) } };

        // Resolution already applied the complete non-allocating receiver
        // policy. Inherited-only closures publish their stable source directly;
        // fresh own cells create exactly one final owned spine.
        let upvalues = if function.own_upvalue_count == 0 {
            crate::native_call_owners::NativeCallUpvalues::borrowed(parent_upvalues)
        } else {
            // No owner exists yet. Keep the exact closure and effective
            // receiver rewriteable while fresh cells allocate. Rooting SELF
            // also keeps the borrowed closure vector allocation alive.
            let mut build_roots = |visitor: &mut dyn FnMut(*mut otter_gc::raw::RawGc)| {
                self_value.trace_value_slot_mut(visitor);
                this_for_callee.trace_value_slot_mut(visitor);
            };
            let owned = Frame::build_upvalues_for_exec_from_source_with_roots(
                &mut self.gc_heap,
                function,
                parent_upvalues,
                &mut build_roots,
            )?;
            crate::native_call_owners::NativeCallUpvalues::owned(owned)
        };
        let window_rollback = self.register_window_rollback();
        let mut window = self.alloc_reg_window(usize::from(plan.register_count))?;
        for (dst_slot, &src) in window.iter_mut().zip(arg_regs.iter()).take(bind_count) {
            *dst_slot = read_caller(src);
        }
        let self_closure_bits = self_value.to_bits();
        let this_bits = this_for_callee.to_bits();
        let (upvalues_ptr, upvalue_count) = {
            let source = upvalues.source();
            (source.base_ptr_or_null() as usize, source.len_u32())
        };

        // This is the publication point. Nothing above can leave a half-owned
        // window behind; nothing below can trigger GC before generated code
        // copies these scalars into and publishes its NativeFrame.
        let owner_id =
            self.native_call_owners
                .push(crate::native_call_owners::NativeCallOwner {
                    function_id: function.id,
                    tier,
                    registers: window,
                    upvalues,
                })?;
        window_rollback.commit();
        Ok(jit::JitPreparedDirectCall {
            entry_cell: plan.entry_cell,
            regs: window.as_mut_ptr().cast::<u64>(),
            self_closure: self_closure_bits,
            this_value: this_bits,
            upvalues_ptr,
            owner_id,
            upvalue_count,
        })
    }
}
