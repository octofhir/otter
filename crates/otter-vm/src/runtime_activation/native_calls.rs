//! Static native bodies called from compiled code without a call frame.
//!
//! # Contents
//! - `JitCtx::static_native_call` — the committed completion of a call whose
//!   callee generated code proved a native function of the static entry
//!   kind: the body runs directly on the published activation's native
//!   view, with no runtime-call binding, frame or operand copy.
//!
//! # Invariants
//! - The operands are the generated caller's own boxed span, live until the
//!   call returns. `this` and the argument window are the body's roots, so a
//!   collection rewrites the span in place.
//! - The body sees the caller's chunk lazily, resolved from the calling
//!   record only when it asks, as a trampoline-entered native does.
//! - A native stamped with another realm, or any call while an extra realm
//!   is active, switches realms around the body as the trampoline does.
//! - A failure is projected through the caller's source like the
//!   trampoline's; a callee of any other entry kind is an internal error,
//!   since generated code tests the kind before calling.
//!
//! # See also
//! - `crate::interp::host_call` — the trampoline's native entry this mirrors.

use crate::Value;
use crate::native_abi::CommittedValueError;

impl crate::native_abi::JitCtx {
    /// `values[0](values[1] as this, ...values[2..])` for a static-entry
    /// native `values[0]`, over the caller's span.
    pub fn static_native_call(
        &mut self,
        values: &mut [Value],
    ) -> Result<Value, CommittedValueError> {
        let fatal = || CommittedValueError::Fatal(crate::VmError::InvalidOperand);
        let activation = self.checked_activation().copied().ok_or_else(fatal)?;
        // SAFETY: the published activation owns its VM and stack for this
        // synchronous entry, and nothing else borrows them meanwhile.
        let (Some(vm), Some(stack)) = (unsafe { activation.vm.as_mut() }, unsafe {
            activation.stack.as_mut()
        }) else {
            return Err(fatal());
        };
        // SAFETY: as above; the admitted context outlives the entry.
        let admitted = unsafe { activation.context.as_ref() };
        let [callee, receiver, arguments @ ..] = values else {
            return Err(fatal());
        };
        let native = callee.as_native_function().ok_or_else(fatal)?;
        let body = native.static_fn(&vm.gc_heap).ok_or_else(fatal)?;
        vm.record_runtime_native_call()
            .map_err(CommittedValueError::Fatal)?;
        let caller =
            crate::interp::host_call::CallerContext::of_caller_frame(admitted, self.native_frame);
        let call_info = crate::runtime_cx::NativeCallInfo::call(*receiver);
        // SAFETY: the span stays in place for the call; the collector is its
        // only other writer, through the roots below.
        let window =
            unsafe { crate::ValueWindow::from_raw_parts(arguments.as_ptr(), arguments.len()) };
        let windows = [window];
        let realm = vm.native_target_realm_global(&native);
        let mut invoke = |vm: &mut crate::Interpreter| {
            let roots = crate::runtime_cx::NativeCallRoots::new(&call_info, &windows);
            let _roots = vm
                .gc_heap
                .register_extra_roots(otter_gc::ExtraRoots::new(&roots));
            let turn = crate::runtime_cx::RuntimeTurn::from_rooted_parts(vm, stack);
            let mut ctx = crate::runtime_cx::NativeCtx::with_native_context(
                turn,
                &call_info,
                crate::runtime_cx::NativeContext::Caller(&caller),
            );
            Ok(body(&mut ctx, window))
        };
        let result = match realm {
            Some(global) => vm.with_host_realm_global(global, invoke),
            None => invoke(vm),
        }
        .map_err(CommittedValueError::Fatal)?;
        result.map_err(|error| {
            let context = caller.get(vm);
            crate::error_ops::native_error_to_committed_with_stack(vm, stack, context, error)
        })
    }
}
