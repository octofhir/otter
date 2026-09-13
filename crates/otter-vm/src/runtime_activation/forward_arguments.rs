//! Non-reentrant argument forwarding from a published compiled caller.
//!
//! # Contents
//! - Eligibility/count probe for an already-resolved intrinsic apply.
//! - Runtime-selected ordinary target admission and current generation lookup.
//! - Complete live-argument copy into an unpublished generated callee.
//!
//! # Invariants
//! - These operations neither allocate in the GC heap nor invoke JavaScript.
//! - Frame and window pointers remain engine-private and are validated before use.
//! - A rejected probe or copy leaves all JavaScript effects to canonical completion.
//!
//! # See also
//! - [`crate::forward_arguments`] — source semantics and mapped bindings.

use super::{RuntimeCall, RuntimeFrameIdentity};
use crate::{ActiveFrameMut, ActiveFrameRef, native_abi::NativeFrame};

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

    /// Resolve a runtime-selected ordinary bytecode target without allocation.
    /// Function admission is shared with compile-time call baking. The result
    /// contains stable engine metadata only; receiver binding and native-stack
    /// reservation must still be proved before the callee is published.
    pub fn forwarded_call_plan(
        &self,
        callee: crate::Value,
    ) -> Option<crate::jit::JitDirectCallPlan> {
        // SAFETY: these services belong to the live bound activation. No
        // allocation, compilation or JavaScript reentry occurs during lookup.
        let vm = unsafe { &mut *self.vm.as_ptr() };
        let context = unsafe { self.context.as_ref() };
        let source = unsafe { ActiveFrameRef::from_native_ptr(self.frame.as_ptr()) }.ok()?;
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
        let function = context.exec_function(function_id)?;
        if !function.admits_generated_call(crate::jit::JitDirectCallKind::Plain)
            || captures != u32::from(function.inherited_upvalue_count)
            || (!(function.is_strict || function.is_arrow)
                && flags & crate::closure::CLOSURE_CALL_FLAG_BOUND_THIS != 0)
        {
            return None;
        }
        vm.record_resolved_bytecode_call_feedback(caller, call_pc, source.function_id(), callee);
        vm.current_direct_callee_plan(function)
    }

    /// Copy the caller's live argument values into a private generated callee.
    ///
    /// # Safety
    /// `destination` and its complete initialized register/argument windows must
    /// be exclusively owned by generated linkage, disjoint from the caller and
    /// live for this non-allocating call. The callee must not yet be published.
    pub unsafe fn copy_forwarded_arguments(
        &self,
        destination: *mut NativeFrame,
        parameter_count: u16,
    ) -> bool {
        // SAFETY: the bound caller and the caller-owned private destination are
        // live and disjoint; checked views retain no Rust slice across VM work.
        let vm = unsafe { self.vm.as_ref() };
        let stack = unsafe { self.stack.as_ref() };
        let context = unsafe { self.context.as_ref() };
        let Ok(source) = (unsafe { ActiveFrameRef::from_native_ptr(self.frame.as_ptr()) }) else {
            return false;
        };
        let Ok(mut destination) = (unsafe { ActiveFrameMut::from_native_ptr(destination) }) else {
            return false;
        };
        let Some(function) = context.exec_function(source.function_id()) else {
            return false;
        };
        let materialized = match self.identity {
            RuntimeFrameIdentity::Materialized(index) => Some(index),
            RuntimeFrameIdentity::StackOwned => None,
        };
        vm.copy_live_forwarded_arguments(
            function,
            stack,
            &source,
            materialized,
            &mut destination,
            parameter_count,
        )
        .unwrap_or(false)
    }
}
