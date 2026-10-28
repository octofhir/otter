//! RegExp and regexp-facing String methods generated code calls through
//! their runtime entries.
//!
//! # Contents
//! - [`crate::native_abi::JitCtx::regexp_method`] runs the realm's original
//!   `%RegExp.prototype.exec%` or `test` on boxed operands.
//! - [`crate::native_abi::JitCtx::string_method`] runs the realm's original
//!   `String.prototype` `replace`, `split`, `match` or `search`.
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
//! - A String method runs its own native body with the call's receiver and
//!   arguments, missing ones `undefined`: that body is the call.
//! - No native call frame is published; a failure projects through the
//!   calling function's source like any committed operation.
//!
//! # See also
//! - [`crate::bootstrap_regexp`] owns the method bodies.
//! - [`crate::call_ops::invoke_frame_native_call`] roots the call.

use crate::ValueWindow;
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

impl crate::native_abi::JitCtx {
    /// The RegExp of the regex literal at constant-pool index `constant` of
    /// function `function`'s chunk: the committed miss of a generated birth,
    /// which names the literal's own function (an inlined body's is not the
    /// published frame's).
    pub fn regexp_literal(
        &mut self,
        constant: u64,
        function: u64,
    ) -> Result<Value, CommittedValueError> {
        let (Ok(constant), Ok(function)) = (u32::try_from(constant), u32::try_from(function))
        else {
            return Err(CommittedValueError::Fatal(crate::VmError::InvalidOperand));
        };
        let mut runtime = self.runtime_call().map_err(CommittedValueError::Fatal)?;
        runtime
            .regexp_literal_value(function, constant)
            .map_err(|error| runtime.regexp_error(error))
    }

    /// `receiver.<method>(argument0, argument1)` for the String method the
    /// intrinsic kind `payload` names.
    pub fn string_method(
        &mut self,
        payload: u64,
        receiver: Value,
        arguments: [Value; 2],
    ) -> Result<Value, CommittedValueError> {
        let kind = u32::try_from(payload)
            .ok()
            .and_then(crate::IntrinsicCallKind::from_feedback_payload);
        let body: crate::native_function::NativeFastFn = match kind {
            Some(crate::IntrinsicCallKind::StringReplace) => {
                crate::string::prototype::bridge_replace
            }
            Some(crate::IntrinsicCallKind::StringSplit) => crate::string::prototype::bridge_split,
            Some(crate::IntrinsicCallKind::StringMatch) => crate::string::prototype::bridge_match,
            Some(crate::IntrinsicCallKind::StringSearch) => crate::string::prototype::bridge_search,
            _ => return Err(CommittedValueError::Fatal(crate::VmError::InvalidOperand)),
        };
        // A string subject, a pristine RegExp and a string template replace
        // without the method's frame: nothing the method would observe runs.
        if kind == Some(crate::IntrinsicCallKind::StringReplace)
            && receiver.is_string()
            && arguments[1].is_string()
        {
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
            if vm.is_pristine_regexp_proved(arguments[0])
                && let Some(result) =
                    crate::regexp_fast::replace_direct(vm, arguments[0], receiver, arguments[1])
            {
                return match result {
                    Ok(value) => Ok(value),
                    Err(error) => {
                        let mut runtime =
                            self.runtime_call().map_err(CommittedValueError::Fatal)?;
                        Err(runtime.regexp_error(error))
                    }
                };
            }
        }
        let mut runtime = self.runtime_call().map_err(CommittedValueError::Fatal)?;
        runtime.native_body(body, receiver, &arguments)
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

    /// Run the static native `body` as the call `receiver.body(...arguments)`.
    fn native_body(
        &mut self,
        body: crate::native_function::NativeFastFn,
        receiver: Value,
        arguments: &[Value],
    ) -> Result<Value, CommittedValueError> {
        // SAFETY: the bound runtime call owns exclusive access to its VM and
        // activation stack for this operation.
        let vm = unsafe { &mut *self.vm.as_ptr() };
        let stack = unsafe { &mut *self.stack.as_ptr() };
        let context: &crate::ExecutionContext = &self.context;
        let result = crate::call_ops::invoke_frame_native_call(
            vm,
            stack,
            crate::runtime_cx::NativeContext::Exact(Some(context)),
            crate::native_function::NativeCallTarget::Static(body),
            receiver,
            ValueWindow::from(arguments),
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
        let body: crate::native_function::NativeFastFn = if test {
            crate::bootstrap_regexp::proto_test
        } else {
            crate::bootstrap_regexp::proto_exec
        };
        self.native_body(body, receiver, &[argument])
    }
}
