//! Async generator function and prototype intrinsics.
//!
//! ES2024 §27.4 AsyncGeneratorFunction Objects
//! Spec: <https://tc39.es/ecma262/#sec-asyncgeneratorfunction-objects>
//!
//! ES2024 §27.6 AsyncGenerator Objects
//! Spec: <https://tc39.es/ecma262/#sec-asyncgenerator-objects>

use crate::descriptors::{NativeFunctionDescriptor, VmNativeCallError};
use crate::object::{
    AsyncGeneratorRequest, AsyncGeneratorRequestKind, GeneratorState, HeapValueKind, ObjectHandle,
    PropertyAttributes, PropertyValue,
};
use crate::value::RegisterValue;

use super::install::{IntrinsicInstallContext, IntrinsicInstaller, install_function_length_name};
use super::{IntrinsicsError, VmIntrinsics, WellKnownSymbol};

pub(super) static ASYNC_GENERATOR_INTRINSIC: AsyncGeneratorIntrinsic = AsyncGeneratorIntrinsic;

pub(super) struct AsyncGeneratorIntrinsic;

impl IntrinsicInstaller for AsyncGeneratorIntrinsic {
    fn init(
        &self,
        intrinsics: &mut VmIntrinsics,
        cx: &mut IntrinsicInstallContext<'_>,
    ) -> Result<(), IntrinsicsError> {
        let async_gen_proto = intrinsics.async_generator_prototype();
        let async_gen_fn_proto = intrinsics.async_generator_function_prototype();

        // ─── §27.4.3 %AsyncGeneratorFunction.prototype% ─────────────
        // %AsyncGeneratorFunction.prototype%.prototype = %AsyncGenerator.prototype%
        // Spec: <https://tc39.es/ecma262/#sec-asyncgeneratorfunction-prototype>
        let prototype_prop = cx.property_names.intern("prototype");
        cx.heap.define_own_property(
            async_gen_fn_proto,
            prototype_prop,
            PropertyValue::data_with_attrs(
                RegisterValue::from_object_handle(async_gen_proto.0),
                PropertyAttributes::from_flags(false, false, true),
            ),
        )?;

        // %AsyncGeneratorFunction.prototype%[@@toStringTag] = "AsyncGeneratorFunction"
        install_to_string_tag(async_gen_fn_proto, "AsyncGeneratorFunction", cx)?;

        // ─── §27.6.1 %AsyncGenerator.prototype% ─────────────────────
        // Spec: <https://tc39.es/ecma262/#sec-properties-of-the-asyncgenerator-prototype>

        // %AsyncGenerator.prototype%.constructor = %AsyncGeneratorFunction.prototype%
        let constructor_prop = cx.property_names.intern("constructor");
        cx.heap.define_own_property(
            async_gen_proto,
            constructor_prop,
            PropertyValue::data_with_attrs(
                RegisterValue::from_object_handle(async_gen_fn_proto.0),
                PropertyAttributes::from_flags(false, false, true),
            ),
        )?;

        // %AsyncGenerator.prototype%.next(value)
        // Spec: <https://tc39.es/ecma262/#sec-asyncgenerator-prototype-next>
        install_method(
            async_gen_proto,
            "next",
            1,
            async_generator_prototype_next,
            intrinsics.function_prototype(),
            cx,
        )?;

        // %AsyncGenerator.prototype%.return(value)
        // Spec: <https://tc39.es/ecma262/#sec-asyncgenerator-prototype-return>
        install_method(
            async_gen_proto,
            "return",
            1,
            async_generator_prototype_return,
            intrinsics.function_prototype(),
            cx,
        )?;

        // %AsyncGenerator.prototype%.throw(exception)
        // Spec: <https://tc39.es/ecma262/#sec-asyncgenerator-prototype-throw>
        install_method(
            async_gen_proto,
            "throw",
            1,
            async_generator_prototype_throw,
            intrinsics.function_prototype(),
            cx,
        )?;

        // %AsyncGenerator.prototype%[@@toStringTag] = "AsyncGenerator"
        install_to_string_tag(async_gen_proto, "AsyncGenerator", cx)?;

        Ok(())
    }

    fn install_on_global(
        &self,
        _intrinsics: &VmIntrinsics,
        _cx: &mut IntrinsicInstallContext<'_>,
    ) -> Result<(), IntrinsicsError> {
        // Async generator prototypes are not directly exposed as globals.
        Ok(())
    }
}

// ═══════════════════════════════════════════════════════════════════════════
//  Native implementations
// ═══════════════════════════════════════════════════════════════════════════

type NativeFn = fn(
    &RegisterValue,
    &[RegisterValue],
    &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError>;

/// ES2024 §27.6.1.2 %AsyncGenerator.prototype%.next(value)
/// Spec: <https://tc39.es/ecma262/#sec-asyncgenerator-prototype-next>
///
/// 1. Let generator be the this value.
/// 2. Let promiseCapability be ! NewPromiseCapability(%Promise%).
/// 3. Let result be Completion(AsyncGeneratorValidate(generator, empty)).
/// 4. IfAbruptRejectPromise(result, promiseCapability).
/// 5. Let state be generator.[[AsyncGeneratorState]].
/// 6. If state is completed, then return promise resolved with {value: undefined, done: true}.
/// 7. Let completion be NormalCompletion(value).
/// 8. Perform AsyncGeneratorEnqueue(generator, completion, promiseCapability).
/// 9. ... AsyncGeneratorResume if not executing ...
/// 10. Return promiseCapability.[[Promise]].
fn async_generator_prototype_next(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let generator = require_async_generator_object(*this, runtime)?;
    let value = args
        .first()
        .copied()
        .unwrap_or_else(RegisterValue::undefined);

    async_generator_enqueue_and_resume(
        generator,
        AsyncGeneratorRequestKind::Next,
        value,
        runtime,
    )
}

/// ES2024 §27.6.1.3 %AsyncGenerator.prototype%.return(value)
/// Spec: <https://tc39.es/ecma262/#sec-asyncgenerator-prototype-return>
fn async_generator_prototype_return(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let generator = require_async_generator_object(*this, runtime)?;
    let value = args
        .first()
        .copied()
        .unwrap_or_else(RegisterValue::undefined);

    async_generator_enqueue_and_resume(
        generator,
        AsyncGeneratorRequestKind::Return,
        value,
        runtime,
    )
}

/// ES2024 §27.6.1.4 %AsyncGenerator.prototype%.throw(exception)
/// Spec: <https://tc39.es/ecma262/#sec-asyncgenerator-prototype-throw>
fn async_generator_prototype_throw(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let generator = require_async_generator_object(*this, runtime)?;
    let exception = args
        .first()
        .copied()
        .unwrap_or_else(RegisterValue::undefined);

    async_generator_enqueue_and_resume(
        generator,
        AsyncGeneratorRequestKind::Throw,
        exception,
        runtime,
    )
}

// ═══════════════════════════════════════════════════════════════════════════
//  Core async generator resume logic
// ═══════════════════════════════════════════════════════════════════════════

/// §27.6.3.2 AsyncGeneratorEnqueue + §27.6.3.3 AsyncGeneratorResume (combined).
///
/// Creates a new promise for the caller, enqueues the request, and — if the
/// generator is currently suspended — immediately resumes it.
///
/// Spec: <https://tc39.es/ecma262/#sec-asyncgeneratorenqueue>
/// Spec: <https://tc39.es/ecma262/#sec-asyncgeneratorresume>
fn async_generator_enqueue_and_resume(
    generator: ObjectHandle,
    kind: AsyncGeneratorRequestKind,
    value: RegisterValue,
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    // Step 2: Let promiseCapability be ! NewPromiseCapability(%Promise%).
    let proto = runtime.intrinsics().promise_prototype();
    let promise = runtime.objects_mut().alloc_promise_with_proto(proto);

    // Step 8: Perform AsyncGeneratorEnqueue(generator, completion, promiseCapability).
    let request = AsyncGeneratorRequest {
        kind,
        value,
        promise,
    };
    runtime
        .objects_mut()
        .async_generator_enqueue(generator, request)
        .map_err(to_internal_error)?;

    let state = runtime
        .objects()
        .async_generator_state(generator)
        .map_err(to_internal_error)?;

    // §27.6.3.3 step 6-8: If the generator is suspended, resume it to process the queue.
    match state {
        GeneratorState::SuspendedStart | GeneratorState::SuspendedYield => {
            // Resume the async generator — this will process the front-of-queue request.
            runtime.resume_async_generator(generator)?;
        }
        GeneratorState::Completed => {
            // §27.6.3.7 AsyncGeneratorCompleteStep — drain completed requests immediately.
            async_generator_drain_completed(generator, runtime)?;
        }
        GeneratorState::Executing | GeneratorState::AwaitingReturn => {
            // Generator is busy — the request stays queued and will be processed
            // when the current step completes.
        }
    }

    Ok(RegisterValue::from_object_handle(promise.0))
}

/// Drain all requests from a completed async generator's queue.
/// Each request gets a resolved or rejected promise depending on the request kind.
///
/// §27.6.3.7 AsyncGeneratorCompleteStep
/// Spec: <https://tc39.es/ecma262/#sec-asyncgeneratorcompletestep>
pub(crate) fn async_generator_drain_completed(
    generator: ObjectHandle,
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<(), VmNativeCallError> {
    loop {
        let request = runtime
            .objects_mut()
            .async_generator_dequeue(generator)
            .map_err(to_internal_error)?;
        let Some(request) = request else {
            break;
        };
        match request.kind {
            AsyncGeneratorRequestKind::Next => {
                // {value: undefined, done: true}
                let iter_result =
                    runtime.create_iter_result(RegisterValue::undefined(), true)?;
                resolve_promise(
                    runtime,
                    request.promise,
                    RegisterValue::from_object_handle(iter_result.0),
                )?;
            }
            AsyncGeneratorRequestKind::Return => {
                // {value: returnValue, done: true}
                let iter_result = runtime.create_iter_result(request.value, true)?;
                resolve_promise(
                    runtime,
                    request.promise,
                    RegisterValue::from_object_handle(iter_result.0),
                )?;
            }
            AsyncGeneratorRequestKind::Throw => {
                // Reject the promise with the thrown value.
                reject_promise(runtime, request.promise, request.value)?;
            }
        }
    }
    Ok(())
}

/// Resolves a single request's promise with a yielded {value, done} result.
///
/// §27.6.3.7 AsyncGeneratorCompleteStep (normal completion path)
pub(crate) fn async_generator_complete_step(
    runtime: &mut crate::interpreter::RuntimeState,
    promise: ObjectHandle,
    value: RegisterValue,
    done: bool,
) -> Result<(), VmNativeCallError> {
    let iter_result = runtime.create_iter_result(value, done)?;
    resolve_promise(
        runtime,
        promise,
        RegisterValue::from_object_handle(iter_result.0),
    )
}

// ═══════════════════════════════════════════════════════════════════════════
//  Helpers
// ═══════════════════════════════════════════════════════════════════════════

fn require_async_generator_object(
    this: RegisterValue,
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<ObjectHandle, VmNativeCallError> {
    let Some(handle) = this.as_object_handle().map(ObjectHandle) else {
        let error = runtime
            .alloc_type_error("AsyncGenerator method called on non-object")
            .map_err(to_internal_error)?;
        return Err(VmNativeCallError::Thrown(
            RegisterValue::from_object_handle(error.0),
        ));
    };

    if !matches!(
        runtime.objects().kind(handle),
        Ok(HeapValueKind::AsyncGenerator)
    ) {
        let error = runtime
            .alloc_type_error("AsyncGenerator method requires an async generator object")
            .map_err(to_internal_error)?;
        return Err(VmNativeCallError::Thrown(
            RegisterValue::from_object_handle(error.0),
        ));
    }

    Ok(handle)
}

fn to_internal_error(error: impl std::fmt::Debug) -> VmNativeCallError {
    VmNativeCallError::Internal(format!("async generator internal error: {error:?}").into())
}

/// Resolve a promise (used for settling async generator request promises).
fn resolve_promise(
    runtime: &mut crate::interpreter::RuntimeState,
    promise: ObjectHandle,
    value: RegisterValue,
) -> Result<(), VmNativeCallError> {
    let p = runtime
        .objects_mut()
        .get_promise_mut(promise)
        .ok_or_else(|| VmNativeCallError::Internal("not a promise".into()))?;
    if p.is_pending() && let Some(jobs) = p.fulfill(value) {
        for job in jobs {
            runtime.microtasks_mut().enqueue_promise_job(job);
        }
    }
    Ok(())
}

/// Reject a promise (used for settling async generator request promises).
fn reject_promise(
    runtime: &mut crate::interpreter::RuntimeState,
    promise: ObjectHandle,
    reason: RegisterValue,
) -> Result<(), VmNativeCallError> {
    let p = runtime
        .objects_mut()
        .get_promise_mut(promise)
        .ok_or_else(|| VmNativeCallError::Internal("not a promise".into()))?;
    if p.is_pending() && let Some(jobs) = p.reject(reason) {
        for job in jobs {
            runtime.microtasks_mut().enqueue_promise_job(job);
        }
    }
    Ok(())
}

fn install_method(
    prototype: ObjectHandle,
    name: &str,
    arity: u16,
    f: NativeFn,
    function_prototype: ObjectHandle,
    cx: &mut IntrinsicInstallContext<'_>,
) -> Result<(), IntrinsicsError> {
    let desc = NativeFunctionDescriptor::method(name, arity, f);
    let host_fn = cx.native_functions.register(desc);
    let handle = cx.alloc_intrinsic_host_function(host_fn, function_prototype)?;
    install_function_length_name(handle, arity, name, cx)?;
    let prop = cx.property_names.intern(name);
    cx.heap.define_own_property(
        prototype,
        prop,
        PropertyValue::data_with_attrs(
            RegisterValue::from_object_handle(handle.0),
            PropertyAttributes::builtin_method(),
        ),
    )?;
    Ok(())
}

fn install_to_string_tag(
    target: ObjectHandle,
    tag: &str,
    cx: &mut IntrinsicInstallContext<'_>,
) -> Result<(), IntrinsicsError> {
    let sym_tag = cx
        .property_names
        .intern_symbol(WellKnownSymbol::ToStringTag.stable_id());
    let tag_str = cx.heap.alloc_string(tag);
    cx.heap.define_own_property(
        target,
        sym_tag,
        PropertyValue::data_with_attrs(
            RegisterValue::from_object_handle(tag_str.0),
            PropertyAttributes::from_flags(false, false, true),
        ),
    )?;
    Ok(())
}
