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

use super::{RuntimeCall, RuntimeFrameIdentity};
use crate::{
    ActiveFrameMut, ActiveFrameRef, feedback::OrdinaryCallTarget, native_abi::NativeFrame,
};

impl RuntimeCall<'_> {
    /// Count elided live actuals for an already-resolved intrinsic apply.
    /// A materialized arguments object or non-intrinsic method returns `None`.
    pub fn forward_argument_count(&self, method: crate::Value) -> Option<u32> {
        // SAFETY: the bound activation owns these live services and windows;
        // this leaf neither allocates nor reenters.
        let vm = unsafe { self.vm.as_ref() };
        let stack = unsafe { self.stack.as_ref() };
        let frame = unsafe { ActiveFrameRef::from_native_ptr(self.frame.as_ptr()) }.ok()?;
        if !crate::method_ops::is_function_prototype_intrinsic_value(
            method,
            &vm.gc_heap,
            crate::native_function::VmIntrinsicFunction::FunctionPrototypeApply,
        ) {
            return None;
        }
        let materialized = match self.identity {
            RuntimeFrameIdentity::Materialized(index) => Some(index),
            RuntimeFrameIdentity::StackOwned => None,
        };
        vm.elided_forward_argument_count(stack, &frame, materialized)
    }

    /// Whether the committed value boundary can complete this source without
    /// first materializing a stack-owned caller. This probe has no JS effects.
    pub fn forward_call_can_complete(&self, method: crate::Value) -> bool {
        matches!(self.identity, RuntimeFrameIdentity::Materialized(_))
            || self.forward_argument_count(method).is_some()
    }

    /// Complete one forwarded call from `[method, callee, receiver, bindings…]`.
    /// Register bindings follow the immutable CodeBlock mapping order; captured
    /// aliases remain live cells. No interpreter destination crosses this API.
    pub fn call_forward_values(
        &mut self,
        values: &[crate::Value],
    ) -> Result<crate::Value, crate::VmError> {
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
        if function.op(instruction) != otter_bytecode::Op::CallForwardArguments
            || values.len() != bindings + 3
            || !self.forward_call_can_complete(values[0])
        {
            return Err(crate::VmError::InvalidOperand);
        }
        let materialized = match self.identity {
            RuntimeFrameIdentity::Materialized(index) => Some(index),
            RuntimeFrameIdentity::StackOwned => None,
        };
        let source = unsafe { ActiveFrameRef::from_native_ptr(self.frame.as_ptr()) }
            .map_err(|_| crate::VmError::InvalidOperand)?;
        let vm = unsafe { &mut *self.vm.as_ptr() };
        let stack = unsafe { &mut *self.stack.as_ptr() };
        vm.jit_runtime_forward_values(context, stack, &source, materialized, values)
    }

    /// Resolve a runtime-selected ordinary bytecode target.
    ///
    /// Function admission is shared with compile-time call baking and may
    /// compile a fresh baseline generation while the published caller owns all
    /// moving roots. The result contains stable engine metadata only; receiver
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
        let source = unsafe { ActiveFrameRef::from_native_ptr(self.frame.as_ptr()) }.ok()?;
        let context = &self.context;
        let caller = context.exec_function(source.function_id())?;
        let call_pc = unsafe { self.frame.as_ref() }.header.pc;
        vm.record_call_attempt_feedback(caller, call_pc, source.function_id());
        let (function_id, captures, flags) = if let Some(function_id) = callee.as_function() {
            (function_id, 0, 0)
        } else {
            let closure = callee.as_closure(&vm.gc_heap)?;
            let header = closure.call_header(&vm.gc_heap);
            if header.requires_runtime_setup() {
                return None;
            }
            (header.function_id, header.upvalue_count, header.flags)
        };
        let callee_context = context.for_function(function_id).ok()?;
        let function = callee_context.exec_function(function_id)?;
        if !function.admits_generated_call(crate::jit::JitDirectCallKind::Plain)
            || captures != u32::from(function.inherited_upvalue_count)
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
        vm.ensure_runtime_forward_callee_plan(&callee_context, function)
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
        destination: *mut NativeFrame,
        parameter_count: u16,
    ) -> Option<u32> {
        // SAFETY: the bound caller and the caller-owned private destination are
        // live and disjoint; checked views retain no Rust slice across VM work.
        let vm = unsafe { self.vm.as_ref() };
        let stack = unsafe { self.stack.as_ref() };
        let Ok(source) = (unsafe { ActiveFrameRef::from_native_ptr(self.frame.as_ptr()) }) else {
            return None;
        };
        let context = &self.context;
        let Ok(mut destination) = (unsafe { ActiveFrameMut::from_native_ptr(destination) }) else {
            return None;
        };
        let function = context.exec_function(source.function_id())?;
        let materialized = match self.identity {
            RuntimeFrameIdentity::Materialized(index) => Some(index),
            RuntimeFrameIdentity::StackOwned => None,
        };
        vm.copy_forwarded_argument_window(
            function,
            stack,
            &source,
            materialized,
            &mut destination,
            parameter_count,
        )
        .ok()
        .flatten()
    }
}
