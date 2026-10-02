//! Argument forwarding from a published compiled caller.
//!
//! # Contents
//! - Eligibility/count probe for an already-resolved intrinsic apply.
//! - Runtime-selected ordinary target admission and current generation lookup.
//! - Complete live-argument copy into an unpublished generated callee.
//! - Committed boxed-value completion with pure exception results.
//!
//! # Invariants
//! - Probes and native-window copies never allocate or invoke JavaScript.
//! - Committed completion roots explicit operands before allocation or reentry.
//! - Frame and window pointers remain engine-private and are validated before use.
//! - A rejected probe or copy leaves all JavaScript effects to canonical completion.
//!
//! # See also
//! - [`crate::forward_arguments`] — source semantics and mapped bindings.

use super::RuntimeCall;
use crate::{ActiveFrameMut, ActiveFrameRef, feedback::OrdinaryCallTarget, native_abi::Frame};

impl RuntimeCall<'_> {
    /// Count elided live actuals for an already-resolved intrinsic apply.
    /// A materialized arguments object or non-intrinsic method returns `None`.
    pub fn forward_argument_count(&self, method: crate::Value) -> Option<u32> {
        // SAFETY: the bound activation owns these live services and windows;
        // this leaf neither allocates nor reenters.
        let vm = unsafe { self.vm.as_ref() };
        let frame = unsafe { ActiveFrameRef::from_ptr(self.frame.as_ptr()) }.ok()?;
        if !crate::method_ops::is_function_prototype_intrinsic_value(
            method,
            &vm.gc_heap,
            crate::native_function::VmIntrinsicFunction::FunctionPrototypeApply,
        ) {
            return None;
        }
        vm.elided_forward_argument_count(&frame)
    }

    /// Whether the committed value boundary can complete this source without
    /// first materializing a stack-owned caller. This probe has no JS effects.
    pub fn forward_call_can_complete(&self, _method: crate::Value) -> bool {
        self.frame_index().is_ok()
    }

    /// Resolve one forwarded call from `[method, callee, receiver, register
    /// bindings…, formals context]` into its callee, receiver and actuals.
    /// Register bindings follow the immutable CodeBlock mapping order; the
    /// trailing context word is present exactly when a mapped formal is
    /// context-held. The caller stages the result before any allocation.
    pub fn stage_forward_values(
        &mut self,
        values: &[crate::Value],
    ) -> Result<
        (
            crate::Value,
            crate::Value,
            smallvec::SmallVec<[crate::Value; 8]>,
        ),
        crate::VmError,
    > {
        // SAFETY: the bound activation owns the context and published frame;
        // the checked source borrows no managed slice across reentrant work.
        let context = &self.context;
        let function = context
            .exec_function(self.function_id())
            .ok_or(crate::VmError::InvalidOperand)?;
        let instruction = function
            .instr_at_index(self.pc() as usize)
            .ok_or(crate::VmError::InvalidOperand)?;
        let bindings = function
            .forwarded_argument_bindings()
            .filter(|(_, storage)| {
                matches!(
                    storage,
                    otter_bytecode::ArgumentBindingStorage::Register { .. }
                )
            })
            .count();
        let context_words = usize::from(function.forwarded_formals_context().is_some());
        if function.op(instruction) != otter_bytecode::Op::CallForwardArguments
            || values.len() != bindings + 3 + context_words
            || !self.forward_call_can_complete(values[0])
        {
            return Err(crate::VmError::InvalidOperand);
        }
        let source = unsafe { ActiveFrameRef::from_ptr(self.frame.as_ptr()) }
            .map_err(|_| crate::VmError::InvalidOperand)?;
        let vm = unsafe { &mut *self.vm.as_ptr() };
        let stack = unsafe { &mut *self.stack.as_ptr() };
        vm.jit_runtime_forward_request(context, stack, &source, values)
    }

    /// Resolve a runtime-selected ordinary bytecode target.
    ///
    /// Function admission is shared with compile-time call baking. Every linked
    /// bytecode target already owns an interpreter or compiled destination.
    /// The result contains stable engine metadata only; receiver
    /// binding and native-stack reservation must still be proved before the
    /// callee is published.
    pub fn forwarded_call_plan(
        &self,
        callee: crate::Value,
    ) -> Option<crate::jit::JitDirectCallPlan> {
        // SAFETY: these services belong to the live bound activation. The
        // caller stays published across optional compilation, and no
        // JavaScript reentry occurs during lookup.
        let vm = unsafe { &mut *self.vm.as_ptr() };
        let source = unsafe { ActiveFrameRef::from_ptr(self.frame.as_ptr()) }.ok()?;
        let context = &self.context;
        let caller = context.exec_function(source.function_id())?;
        let call_pc = unsafe { self.frame.as_ref() }.header.pc;
        vm.record_call_attempt_feedback(caller, call_pc, source.function_id());
        let (function_id, flags) = if let Some(function_id) = callee.as_function() {
            (function_id, 0)
        } else {
            let closure = callee.as_closure(&vm.gc_heap)?;
            let header = closure.call_header(&vm.gc_heap);
            if header.requires_runtime_setup() {
                return None;
            }
            (header.function_id, header.flags)
        };
        let callee_context = context.for_function(function_id).ok()?;
        let function = callee_context.exec_function(function_id)?;
        if !function.admits_generated_call(crate::jit::JitDirectCallKind::Plain)
            || (!(function.is_strict || function.is_arrow)
                && flags & crate::closure::CLOSURE_CALL_FLAG_BOUND_THIS != 0)
        {
            return None;
        }
        let transition = vm.record_ordinary_call_feedback(
            caller,
            call_pc,
            OrdinaryCallTarget::Bytecode(function_id),
        );
        if transition.evict_for_reopt() {
            vm.recompile_active_caller_for_feedback(context, source.function_id());
        }
        vm.current_direct_callee_plan(function)
    }

    /// Copy incoming actuals and live captured aliases into a private callee.
    /// Returns the actual count; generated code must still patch register aliases
    /// from its current value homes before publishing the callee.
    ///
    /// # Safety
    /// `destination` and its complete initialized register/argument windows must
    /// be exclusively owned by generated linkage, disjoint from the caller and
    /// live for this non-allocating call. The callee must not yet be published.
    pub unsafe fn copy_forwarded_argument_window(
        &self,
        destination: *mut Frame,
        parameter_count: u16,
    ) -> Option<u32> {
        // SAFETY: the bound caller and the caller-owned private destination are
        // live and disjoint; checked views retain no Rust slice across VM work.
        let vm = unsafe { self.vm.as_ref() };
        let Ok(source) = (unsafe { ActiveFrameRef::from_ptr(self.frame.as_ptr()) }) else {
            return None;
        };
        let context = &self.context;
        let Ok(mut destination) = (unsafe { ActiveFrameMut::from_ptr(destination) }) else {
            return None;
        };
        let function = context.exec_function(source.function_id())?;
        vm.copy_forwarded_argument_window(function, &source, &mut destination, parameter_count)
            .ok()
            .flatten()
    }
}
