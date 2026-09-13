//! Non-reentrant argument forwarding from a published compiled caller.
//!
//! # Contents
//! - Eligibility/count probe for an already-resolved intrinsic apply.
//! - Complete live-argument copy into an unpublished generated callee.
//!
//! # Invariants
//! - Neither operation allocates in the GC heap or invokes JavaScript.
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
