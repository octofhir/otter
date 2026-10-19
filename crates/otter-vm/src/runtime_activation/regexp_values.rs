//! RegExp methods generated code calls through their runtime entries.
//!
//! # Contents
//! - [`RuntimeCall::regexp_method`] runs the realm's original
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
//! - No native call frame is published; a failure projects through the
//!   calling function's source like any committed operation.
//!
//! # See also
//! - [`crate::bootstrap_regexp`] owns the method bodies.
//! - [`crate::call_ops::invoke_frame_native_call`] roots the call.

use crate::{CommittedValueError, Value};

use super::RuntimeCall;

impl RuntimeCall<'_> {
    /// `receiver.exec(argument)`, or `receiver.test(argument)` when `test`.
    pub fn regexp_method(
        &mut self,
        mut receiver: Value,
        mut argument: Value,
        test: bool,
    ) -> Result<Value, CommittedValueError> {
        // SAFETY: the bound runtime call owns exclusive access to its VM and
        // activation stack for this operation.
        let vm = unsafe { &mut *self.vm.as_ptr() };
        let stack = unsafe { &mut *self.stack.as_ptr() };
        let context: &crate::ExecutionContext = &self.context;
        let project = |vm: &mut crate::Interpreter, stack: &crate::ActivationStack, error| {
            crate::error_ops::native_error_to_committed_with_stack(vm, stack, Some(context), error)
        };
        // An int32 `lastIndex` and a string subject: no user code runs, and
        // nothing allocates before an `exec` hit builds its array.
        match crate::regexp_fast::exec_direct(vm, receiver, argument, test) {
            Some(Ok(crate::regexp_fast::DirectExec::Done(value))) => return Ok(value),
            Some(Ok(crate::regexp_fast::DirectExec::Matched(matched))) => {
                let has_indices = receiver
                    .as_regexp()
                    .is_some_and(|regexp| regexp.flags(&vm.gc_heap).has_indices);
                if !has_indices {
                    match crate::regexp_result::build_match_result_folded(
                        vm,
                        &mut receiver,
                        &mut argument,
                        &matched,
                    ) {
                        Ok(Some(result)) => return Ok(result),
                        Ok(None) => {}
                        Err(error) => {
                            let error =
                                crate::native_function::vm_to_native_error(vm, error, "RegExp");
                            return Err(project(vm, stack, error));
                        }
                    }
                }
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
                return result.map_err(|error| project(vm, stack, error));
            }
            Some(Err(error)) => {
                let error = crate::native_function::vm_to_native_error(vm, error, "RegExp");
                return Err(project(vm, stack, error));
            }
            None => {}
        }
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
        result.map_err(|error| project(vm, stack, error))
    }
}
