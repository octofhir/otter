//! RegExp methods generated code calls through their runtime entries.
//!
//! # Contents
//! - [`crate::native_abi::JitCtx::regexp_method`] runs the realm's original
//!   `%RegExp.prototype.exec%` or `test` on boxed operands.
//!
//! # Invariants
//! - Generated code proved the callee is the original method of the active
//!   source realm, so running its body is the call.
//! - The body is the method's complete semantics: receiver and argument
//!   coercions and the `lastIndex` protocol, which may run JavaScript. An
//!   int32 `lastIndex` and a string subject take the direct match instead,
//!   which runs no user code and allocates only the array of an `exec` hit,
//!   folded into one reservation when the collector allows it.
//! - The direct match needs only the interpreter: it runs before any
//!   runtime call is bound, and binds one only to build a result the folded
//!   path declined, to project an error, or to run the full body. A match it
//!   found is handed over, never searched again, so `lastIndex` and the
//!   legacy statics move once.
//! - No native call frame is published; a failure projects through the
//!   calling function's source like any committed operation.
//!
//! # See also
//! - [`crate::bootstrap_regexp`] owns the method bodies.
//! - [`crate::call_ops::invoke_frame_native_call`] roots the call.

use crate::{CommittedValueError, Value};

use super::RuntimeCall;

impl crate::native_abi::JitCtx {
    /// `receiver.exec(argument)`, or `receiver.test(argument)` when `test`.
    pub fn regexp_method(
        &mut self,
        mut receiver: Value,
        mut argument: Value,
        test: bool,
    ) -> Result<Value, CommittedValueError> {
        let direct = {
            // SAFETY: the trampoline publishes this activation for the
            // dynamic extent of the shared context; `&mut self` keeps any
            // runtime call from being bound while the borrow lives, and the
            // borrow ends before one is.
            let vm = unsafe {
                self.thread
                    .as_ref()
                    .map(|thread| thread.runtime_context as *const crate::VmRuntimeActivation)
                    .filter(|activation| !activation.is_null())
                    .and_then(|activation| (*activation).vm.as_mut())
            };
            let Some(vm) = vm else {
                return Err(CommittedValueError::Fatal(crate::VmError::InvalidOperand));
            };
            match crate::regexp_fast::exec_direct(vm, receiver, argument, test) {
                Some(Ok(crate::regexp_fast::DirectExec::Done(value))) => return Ok(value),
                Some(Ok(crate::regexp_fast::DirectExec::Matched(offsets))) => {
                    let has_indices = receiver
                        .as_regexp()
                        .is_some_and(|regexp| regexp.flags(&vm.gc_heap).has_indices);
                    if !has_indices
                        && let Ok(Some(result)) = crate::regexp_result::build_match_result_folded(
                            vm,
                            &mut receiver,
                            &mut argument,
                            &offsets,
                        )
                    {
                        vm.recycle_match_offsets(offsets);
                        return Ok(result);
                    }
                    let regexp = receiver.as_regexp().expect("exec receiver is a RegExp");
                    let matched = regexp.match_record(&vm.gc_heap, &offsets);
                    vm.recycle_match_offsets(offsets);
                    Some(Ok((matched, has_indices)))
                }
                Some(Err(error)) => Some(Err(error)),
                None => None,
            }
        };
        let mut runtime = self.runtime_call().map_err(CommittedValueError::Fatal)?;
        match direct {
            Some(Ok((matched, has_indices))) => {
                runtime.regexp_match_result(receiver, argument, &matched, has_indices)
            }
            Some(Err(error)) => Err(runtime.regexp_error(error)),
            None => runtime.regexp_body(receiver, argument, test),
        }
    }
}

impl RuntimeCall<'_> {
    /// Project a failure of the direct match through the calling source.
    fn regexp_error(&mut self, error: crate::VmError) -> CommittedValueError {
        // SAFETY: the bound runtime call owns exclusive access to its VM and
        // activation stack for this operation.
        let vm = unsafe { &mut *self.vm.as_ptr() };
        let stack = unsafe { &mut *self.stack.as_ptr() };
        let error = crate::native_function::vm_to_native_error(vm, error, "RegExp");
        crate::error_ops::native_error_to_committed_with_stack(
            vm,
            stack,
            Some(&self.context),
            error,
        )
    }

    /// The array of an `exec` hit the folded path declined.
    fn regexp_match_result(
        &mut self,
        receiver: Value,
        argument: Value,
        matched: &crate::regexp::engine::MatchRecord,
        has_indices: bool,
    ) -> Result<Value, CommittedValueError> {
        // SAFETY: the bound runtime call owns exclusive access to its VM and
        // activation stack for this operation.
        let vm = unsafe { &mut *self.vm.as_ptr() };
        let stack = unsafe { &mut *self.stack.as_ptr() };
        let context: &crate::ExecutionContext = &self.context;
        let result = crate::call_ops::with_frame_native_ctx(
            vm,
            stack,
            crate::runtime_cx::NativeContext::Exact(Some(context)),
            receiver,
            |ctx| {
                ctx.scope(|mut scope| {
                    let input = scope.value(argument);
                    let result = crate::regexp_prototype::build_match_result_native(
                        &matched.as_match(),
                        input,
                        has_indices,
                        &mut scope,
                    )?;
                    Ok(scope.finish(result))
                })
            },
        );
        result.map_err(|error| {
            crate::error_ops::native_error_to_committed_with_stack(vm, stack, Some(context), error)
        })
    }

    /// The method body: coercions, the `lastIndex` protocol and the match.
    fn regexp_body(
        &mut self,
        receiver: Value,
        argument: Value,
        test: bool,
    ) -> Result<Value, CommittedValueError> {
        // SAFETY: the bound runtime call owns exclusive access to its VM and
        // activation stack for this operation.
        let vm = unsafe { &mut *self.vm.as_ptr() };
        let stack = unsafe { &mut *self.stack.as_ptr() };
        let context: &crate::ExecutionContext = &self.context;
        let body: crate::native_function::NativeFastFn = if test {
            crate::bootstrap_regexp::proto_test
        } else {
            crate::bootstrap_regexp::proto_exec
        };
        let result = crate::call_ops::invoke_frame_native_call(
            vm,
            stack,
            crate::runtime_cx::NativeContext::Exact(Some(context)),
            crate::native_function::NativeCallTarget::Static(body),
            receiver,
            &[argument],
        );
        result.map_err(|error| {
            crate::error_ops::native_error_to_committed_with_stack(vm, stack, Some(context), error)
        })
    }
}
