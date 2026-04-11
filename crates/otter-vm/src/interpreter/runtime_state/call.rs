//! `call_callable` + `construct_callable` — central call/construct entry
//! points used from the interpreter and host bridge code; also houses
//! `call_host_function`, `delete_named_property`, and the VmPromise
//! allocator helpers.

use crate::descriptors::VmNativeCallError;
use crate::frame::{FrameFlags, FrameMetadata, RegisterIndex};
use crate::object::{HeapValueKind, ObjectHandle};
use crate::property::PropertyNameId;
use crate::value::RegisterValue;

use super::{Activation, Completion, Interpreter, InterpreterError, RuntimeState};

impl RuntimeState {
    pub fn call_host_function(
        &mut self,
        callable: Option<ObjectHandle>,
        receiver: RegisterValue,
        arguments: &[RegisterValue],
    ) -> Result<RegisterValue, VmNativeCallError> {
        self.check_interrupt()?;

        let Some(callable) = callable else {
            return Ok(RegisterValue::undefined());
        };

        // ES2024 §10.4.1.1 [[Call]] — resolve bound function chain.
        if let Ok(HeapValueKind::BoundFunction) = self.objects.kind(callable) {
            let (target, bound_this, bound_args) =
                self.objects.bound_function_parts(callable).map_err(|e| {
                    VmNativeCallError::Internal(format!("bound function resolution: {e:?}").into())
                })?;
            // Prepend bound_args to arguments.
            let mut full_args = bound_args;
            full_args.extend_from_slice(arguments);
            return self.call_host_function(Some(target), bound_this, &full_args);
        }

        // ES2024 §27.2.1.3 — Promise capability resolve/reject functions.
        if let Ok(HeapValueKind::PromiseCapabilityFunction) = self.objects.kind(callable) {
            let value = arguments
                .first()
                .copied()
                .unwrap_or(RegisterValue::undefined());
            Interpreter::invoke_promise_capability_function(self, callable, value).map_err(
                |e| match e {
                    InterpreterError::UncaughtThrow(v) => VmNativeCallError::Thrown(v),
                    other => VmNativeCallError::Internal(format!("{other}").into()),
                },
            )?;
            return Ok(RegisterValue::undefined());
        }

        // Promise combinator/finally/thunk dispatch.
        match self.objects.kind(callable) {
            Ok(HeapValueKind::PromiseCombinatorElement) => {
                let value = arguments
                    .first()
                    .copied()
                    .unwrap_or(RegisterValue::undefined());
                return Interpreter::invoke_promise_combinator_element(self, callable, value)
                    .map_err(|e| match e {
                        InterpreterError::UncaughtThrow(v) => VmNativeCallError::Thrown(v),
                        other => VmNativeCallError::Internal(format!("{other}").into()),
                    });
            }
            Ok(HeapValueKind::PromiseFinallyFunction) => {
                let value = arguments
                    .first()
                    .copied()
                    .unwrap_or(RegisterValue::undefined());
                return Interpreter::invoke_promise_finally_function(self, callable, value)
                    .map_err(|e| match e {
                        InterpreterError::UncaughtThrow(v) => VmNativeCallError::Thrown(v),
                        other => VmNativeCallError::Internal(format!("{other}").into()),
                    });
            }
            Ok(HeapValueKind::PromiseValueThunk) => {
                if let Some((v, k)) = self.objects.promise_value_thunk_info(callable) {
                    return match k {
                        crate::promise::PromiseFinallyKind::ThenFinally => Ok(v),
                        crate::promise::PromiseFinallyKind::CatchFinally => {
                            Err(VmNativeCallError::Thrown(v))
                        }
                    };
                }
            }
            _ => {}
        }

        // If it's a Closure (compiled JS function), dispatch through Interpreter::call_function.
        if let Ok(HeapValueKind::Closure) = self.objects.kind(callable) {
            // call_function ignores the module param for closures (gets it from the closure).
            // We need a Module reference, so extract from the closure itself.
            let module = self.objects.closure_module(callable).map_err(|e| {
                VmNativeCallError::Internal(format!("closure module lookup: {e:?}").into())
            })?;
            return Interpreter::call_function(self, &module, callable, receiver, arguments)
                .map_err(|e| match e {
                    InterpreterError::UncaughtThrow(v) => VmNativeCallError::Thrown(v),
                    other => VmNativeCallError::Internal(format!("{other}").into()),
                });
        }

        let host_function = self
            .objects
            .host_function(callable)
            .map_err(|error| {
                VmNativeCallError::Internal(
                    format!("native callable lookup failed: {error:?}").into(),
                )
            })?
            .ok_or_else(|| {
                VmNativeCallError::Internal("native callable is not a host function".into())
            })?;
        let descriptor = self
            .native_functions
            .get(host_function)
            .cloned()
            .ok_or_else(|| {
                VmNativeCallError::Internal("host function descriptor is missing".into())
            })?;

        self.native_callee_stack.push(callable);
        let result = (descriptor.callback())(&receiver, arguments, self);
        self.native_callee_stack.pop();
        self.check_interrupt()?;
        match result {
            Ok(value) => Ok(value),
            Err(VmNativeCallError::Thrown(value)) => Err(VmNativeCallError::Thrown(value)),
            Err(VmNativeCallError::Internal(message)) => Err(VmNativeCallError::Internal(message)),
        }
    }

    /// Allocates a reusable VM promise backed by the runtime's intrinsic Promise prototype.
    pub fn alloc_vm_promise(&mut self) -> crate::promise::VmPromise {
        let promise_prototype = self.intrinsics().promise_prototype();
        let promise = self
            .objects_mut()
            .alloc_promise_with_proto(promise_prototype);
        let resolve = self
            .objects_mut()
            .alloc_promise_capability_function(promise, crate::promise::ReactionKind::Fulfill);
        let reject = self
            .objects_mut()
            .alloc_promise_capability_function(promise, crate::promise::ReactionKind::Reject);
        if let Some(js_promise) = self.objects_mut().get_promise_mut(promise) {
            js_promise.resolve_function = Some(resolve);
            js_promise.reject_function = Some(reject);
        }
        crate::promise::VmPromise::new(crate::promise::PromiseCapability {
            promise,
            resolve,
            reject,
        })
    }

    /// Settles one reusable VM promise through its resolve capability function.
    pub fn fulfill_vm_promise(
        &mut self,
        promise: crate::promise::VmPromise,
        value: RegisterValue,
    ) -> Result<(), VmNativeCallError> {
        self.call_host_function(
            Some(promise.resolve_handle()),
            RegisterValue::undefined(),
            &[value],
        )?;
        Ok(())
    }

    /// Settles one reusable VM promise through its reject capability function.
    pub fn reject_vm_promise(
        &mut self,
        promise: crate::promise::VmPromise,
        reason: RegisterValue,
    ) -> Result<(), VmNativeCallError> {
        self.call_host_function(
            Some(promise.reject_handle()),
            RegisterValue::undefined(),
            &[reason],
        )?;
        Ok(())
    }

    /// Allocates and immediately fulfills one reusable VM promise.
    pub fn alloc_fulfilled_vm_promise(
        &mut self,
        value: RegisterValue,
    ) -> Result<crate::promise::VmPromise, VmNativeCallError> {
        let promise = self.alloc_vm_promise();
        self.fulfill_vm_promise(promise, value)?;
        Ok(promise)
    }

    /// Allocates and immediately rejects one reusable VM promise.
    pub fn alloc_rejected_vm_promise(
        &mut self,
        reason: RegisterValue,
    ) -> Result<crate::promise::VmPromise, VmNativeCallError> {
        let promise = self.alloc_vm_promise();
        self.reject_vm_promise(promise, reason)?;
        Ok(promise)
    }

    /// Allocates a promise already fulfilled with the provided value.
    pub fn alloc_resolved_promise(
        &mut self,
        value: RegisterValue,
    ) -> Result<ObjectHandle, VmNativeCallError> {
        Ok(self.alloc_fulfilled_vm_promise(value)?.promise_handle())
    }

    /// Allocates a promise already rejected with the provided reason.
    pub fn alloc_rejected_promise(
        &mut self,
        reason: RegisterValue,
    ) -> Result<ObjectHandle, VmNativeCallError> {
        Ok(self.alloc_rejected_vm_promise(reason)?.promise_handle())
    }

    /// Allocates one iterator result object `{ value, done }`.
    pub fn alloc_iter_result_object(
        &mut self,
        value: RegisterValue,
        done: bool,
    ) -> Result<RegisterValue, VmNativeCallError> {
        crate::intrinsics::create_iter_result_object(value, done, self)
    }

    pub fn call_callable(
        &mut self,
        callable: ObjectHandle,
        receiver: RegisterValue,
        arguments: &[RegisterValue],
    ) -> Result<RegisterValue, VmNativeCallError> {
        self.call_callable_for_accessor(Some(callable), receiver, arguments)
            .map_err(|error| match error {
                InterpreterError::UncaughtThrow(value) => VmNativeCallError::Thrown(value),
                InterpreterError::TypeError(message) => {
                    // Convert TypeError to a catchable JS TypeError so
                    // `assert.throws(TypeError, ...)` can intercept it.
                    match self.alloc_type_error(&message) {
                        Ok(handle) => {
                            VmNativeCallError::Thrown(RegisterValue::from_object_handle(handle.0))
                        }
                        Err(_) => VmNativeCallError::Internal(message),
                    }
                }
                InterpreterError::NativeCall(message) => VmNativeCallError::Internal(message),
                other => VmNativeCallError::Internal(format!("{other}").into()),
            })
    }

    pub fn construct_callable(
        &mut self,
        target: ObjectHandle,
        arguments: &[RegisterValue],
        new_target: ObjectHandle,
    ) -> Result<RegisterValue, VmNativeCallError> {
        if !self.is_constructible(target) {
            let error = self
                .alloc_type_error("construct target is not constructible")
                .map_err(|error| {
                    VmNativeCallError::Internal(
                        format!("construct TypeError allocation failed: {error}").into(),
                    )
                })?;
            return Err(VmNativeCallError::Thrown(
                RegisterValue::from_object_handle(error.0),
            ));
        }
        if !self.is_constructible(new_target) {
            let error = self
                .alloc_type_error("construct newTarget is not constructible")
                .map_err(|error| {
                    VmNativeCallError::Internal(
                        format!("construct TypeError allocation failed: {error}").into(),
                    )
                })?;
            return Err(VmNativeCallError::Thrown(
                RegisterValue::from_object_handle(error.0),
            ));
        }
        let kind = self.objects.kind(target).map_err(|error| {
            VmNativeCallError::Internal(
                format!("construct target kind lookup failed: {error:?}").into(),
            )
        })?;
        let completion = match kind {
            HeapValueKind::BoundFunction => {
                let (bound_target, _, bound_args) =
                    self.objects.bound_function_parts(target).map_err(|error| {
                        VmNativeCallError::Internal(
                            format!("construct bound function lookup failed: {error:?}").into(),
                        )
                    })?;
                let mut full_args = bound_args;
                full_args.extend_from_slice(arguments);
                let forwarded_new_target = if new_target == target {
                    bound_target
                } else {
                    new_target
                };
                return self.construct_callable(bound_target, &full_args, forwarded_new_target);
            }
            HeapValueKind::HostFunction => {
                let host_function = self
                    .objects
                    .host_function(target)
                    .map_err(|error| {
                        VmNativeCallError::Internal(
                            format!("construct host function lookup failed: {error:?}").into(),
                        )
                    })?
                    .ok_or_else(|| {
                        VmNativeCallError::Internal(
                            "construct target host function is missing".into(),
                        )
                    })?;
                let intrinsic_default =
                    Interpreter::host_function_default_intrinsic(self, host_function);
                let default_receiver = RegisterValue::from_object_handle(
                    Interpreter::allocate_construct_receiver(self, new_target, intrinsic_default)
                        .map_err(|error| match error {
                            InterpreterError::UncaughtThrow(value) => {
                                VmNativeCallError::Thrown(value)
                            }
                            other => VmNativeCallError::Internal(format!("{other}").into()),
                        })?
                        .0,
                );
                let completion = Interpreter::invoke_registered_host_function(
                    self,
                    host_function,
                    target,
                    default_receiver,
                    arguments,
                    true,
                )
                .map_err(|error| match error {
                    InterpreterError::UncaughtThrow(value) => VmNativeCallError::Thrown(value),
                    other => VmNativeCallError::Internal(format!("{other}").into()),
                })?;
                Interpreter::apply_construct_return_override(completion, default_receiver)
            }
            HeapValueKind::Closure => {
                let module = self.objects.closure_module(target).map_err(|error| {
                    VmNativeCallError::Internal(
                        format!("construct closure module lookup failed: {error:?}").into(),
                    )
                })?;
                let callee_index = self.objects.closure_callee(target).map_err(|error| {
                    VmNativeCallError::Internal(
                        format!("construct closure callee lookup failed: {error:?}").into(),
                    )
                })?;
                let callee_function = module.function(callee_index).ok_or_else(|| {
                    VmNativeCallError::Internal("construct closure callee is missing".into())
                })?;
                let register_count = callee_function.frame_layout().register_count();
                let is_derived_constructor = callee_function.is_derived_constructor();
                let default_receiver = if is_derived_constructor {
                    RegisterValue::undefined()
                } else {
                    RegisterValue::from_object_handle(
                        Interpreter::allocate_construct_receiver(
                            self,
                            new_target,
                            crate::intrinsics::IntrinsicKey::ObjectPrototype,
                        )
                        .map_err(|error| match error {
                            InterpreterError::UncaughtThrow(value) => {
                                VmNativeCallError::Thrown(value)
                            }
                            other => VmNativeCallError::Internal(format!("{other}").into()),
                        })?
                        .0,
                    )
                };
                let mut activation = Activation::with_context(
                    callee_index,
                    register_count,
                    FrameMetadata::new(
                        arguments.len() as RegisterIndex,
                        FrameFlags::new(true, true, false),
                    ),
                    Some(target),
                );
                activation.set_construct_new_target(Some(new_target));

                if callee_function.frame_layout().receiver_slot().is_some() {
                    activation
                        .set_receiver(callee_function, default_receiver)
                        .map_err(|error| VmNativeCallError::Internal(format!("{error}").into()))?;
                }

                let param_count = callee_function.frame_layout().parameter_count();
                for (index, &argument) in arguments.iter().take(param_count as usize).enumerate() {
                    let register = callee_function
                        .frame_layout()
                        .resolve_user_visible(index as u16)
                        .ok_or_else(|| {
                            VmNativeCallError::Internal(
                                "construct argument register resolution failed".into(),
                            )
                        })?;
                    activation
                        .set_register(register, argument)
                        .map_err(|error| VmNativeCallError::Internal(format!("{error}").into()))?;
                }
                if arguments.len() > param_count as usize {
                    activation.overflow_args = arguments[param_count as usize..].to_vec();
                }

                let completion = Interpreter::for_runtime(self)
                    .run_completion_with_runtime(&module, &mut activation, self)
                    .map_err(|error| match error {
                        InterpreterError::UncaughtThrow(value) => VmNativeCallError::Thrown(value),
                        InterpreterError::NativeCall(message)
                        | InterpreterError::TypeError(message) => {
                            VmNativeCallError::Internal(message)
                        }
                        other => VmNativeCallError::Internal(format!("{other}").into()),
                    })?;
                if is_derived_constructor {
                    match completion {
                        Completion::Return(value) if self.is_ecma_object(value) => {
                            Completion::Return(value)
                        }
                        Completion::Return(value) if value != RegisterValue::undefined() => {
                            let error = self
                                .alloc_type_error(
                                    "Derived constructors may only return object or undefined values",
                                )
                                .map_err(|error| {
                                    VmNativeCallError::Internal(format!("{error}").into())
                                })?;
                            Completion::Throw(RegisterValue::from_object_handle(error.0))
                        }
                        Completion::Return(_) => {
                            // §10.2.1.3 [[Construct]] step 11: read `this`
                            // from the receiver slot. If `super()` was called
                            // from inside an arrow (which writes to the lexical
                            // "this" upvalue instead), the receiver slot may
                            // still hold `undefined`. Fall back to scanning the
                            // first few local registers for an initialized
                            // object — the compile-time "this" binding is
                            // always the first local allocated by
                            // `declare_this_binding`.
                            let mut this_value = RegisterValue::undefined();
                            if callee_function.frame_layout().receiver_slot().is_some() {
                                let recv =
                                    activation.receiver(callee_function).map_err(|error| {
                                        VmNativeCallError::Internal(format!("{error}").into())
                                    })?;
                                if self.is_ecma_object(recv) {
                                    this_value = recv;
                                } else {
                                    let local_range = callee_function.frame_layout().local_range();
                                    if !local_range.is_empty()
                                        && let Ok(val) = activation.register(
                                            callee_function
                                                .frame_layout()
                                                .resolve_user_visible(
                                                    callee_function
                                                        .frame_layout()
                                                        .parameter_count(),
                                                )
                                                .unwrap_or(0),
                                        )
                                        && self.is_ecma_object(val)
                                    {
                                        this_value = val;
                                    }
                                }
                            }
                            if self.is_ecma_object(this_value) {
                                Completion::Return(this_value)
                            } else {
                                let error = self
                                    .alloc_reference_error(
                                        "Must call super constructor in derived class before returning from derived constructor",
                                    )
                                    .map_err(|error| {
                                        VmNativeCallError::Internal(
                                            format!(
                                                "construct ReferenceError allocation failed: {error}"
                                            )
                                            .into(),
                                        )
                                    })?;
                                Completion::Throw(RegisterValue::from_object_handle(error.0))
                            }
                        }
                        Completion::Throw(value) => Completion::Throw(value),
                    }
                } else {
                    Interpreter::apply_construct_return_override(completion, default_receiver)
                }
            }
            _ => {
                return Err(VmNativeCallError::Internal(
                    "construct target is not callable".into(),
                ));
            }
        };

        match completion {
            Completion::Return(value) => Ok(value),
            Completion::Throw(value) => Err(VmNativeCallError::Thrown(value)),
        }
    }

    pub(crate) fn delete_named_property(
        &mut self,
        target: ObjectHandle,
        property: PropertyNameId,
    ) -> Result<bool, InterpreterError> {
        self.objects
            .delete_property_with_registry(target, property, &self.property_names)
            .map_err(Into::into)
    }
}
