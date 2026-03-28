//! Promise constructor and prototype methods — ES2024 §27.2.
//!
//! Implements the Promise class using the descriptor/builder/installer pipeline.
//! Methods delegate to [`JsPromise`] for state management and to
//! [`MicrotaskQueue`] for job scheduling.

use crate::builders::ClassBuilder;
use crate::descriptors::{
    JsClassDescriptor, NativeBindingDescriptor, NativeBindingTarget, NativeFunctionDescriptor,
    VmNativeCallError,
};
use crate::interpreter::RuntimeState;
use crate::object::ObjectHandle;
use crate::promise::PromiseCapability;
use crate::value::RegisterValue;

use super::{
    IntrinsicsError, VmIntrinsics,
    install::{IntrinsicInstallContext, IntrinsicInstaller, install_class_plan},
};

pub(super) static PROMISE_INTRINSIC: PromiseIntrinsic = PromiseIntrinsic;

pub(super) struct PromiseIntrinsic;

impl IntrinsicInstaller for PromiseIntrinsic {
    fn init(
        &self,
        intrinsics: &mut VmIntrinsics,
        cx: &mut IntrinsicInstallContext<'_>,
    ) -> Result<(), IntrinsicsError> {
        let descriptor = promise_class_descriptor();
        let plan = ClassBuilder::from_descriptor(&descriptor)
            .expect("Promise class descriptors should normalize")
            .build();

        // Register and allocate the constructor.
        if let Some(ctor_desc) = plan.constructor() {
            let host_fn = cx.native_functions.register(ctor_desc.clone());
            let ctor_handle =
                cx.alloc_intrinsic_host_function(host_fn, intrinsics.function_prototype())?;
            intrinsics.promise_constructor = ctor_handle;
        }

        // Install prototype and static methods.
        install_class_plan(
            intrinsics.promise_prototype(),
            intrinsics.promise_constructor(),
            &plan,
            intrinsics.function_prototype(),
            cx,
        )?;

        Ok(())
    }

    fn install_on_global(
        &self,
        intrinsics: &VmIntrinsics,
        cx: &mut IntrinsicInstallContext<'_>,
    ) -> Result<(), IntrinsicsError> {
        cx.install_global_value(
            intrinsics,
            "Promise",
            RegisterValue::from_object_handle(intrinsics.promise_constructor().0),
        )
    }
}

// ---------------------------------------------------------------------------
// Class descriptor
// ---------------------------------------------------------------------------

fn promise_class_descriptor() -> JsClassDescriptor {
    JsClassDescriptor::new("Promise")
        .with_constructor(NativeFunctionDescriptor::constructor(
            "Promise",
            1,
            promise_constructor,
        ))
        // Prototype methods
        .with_binding(NativeBindingDescriptor::new(
            NativeBindingTarget::Prototype,
            NativeFunctionDescriptor::method("then", 2, promise_then),
        ))
        .with_binding(NativeBindingDescriptor::new(
            NativeBindingTarget::Prototype,
            NativeFunctionDescriptor::method("catch", 1, promise_catch),
        ))
        .with_binding(NativeBindingDescriptor::new(
            NativeBindingTarget::Prototype,
            NativeFunctionDescriptor::method("finally", 1, promise_finally),
        ))
        // Static methods
        .with_binding(NativeBindingDescriptor::new(
            NativeBindingTarget::Constructor,
            NativeFunctionDescriptor::method("resolve", 1, promise_static_resolve),
        ))
        .with_binding(NativeBindingDescriptor::new(
            NativeBindingTarget::Constructor,
            NativeFunctionDescriptor::method("reject", 1, promise_static_reject),
        ))
}

// ---------------------------------------------------------------------------
// Constructor
// ---------------------------------------------------------------------------

/// `new Promise(executor)` — ES2024 §27.2.3
///
/// 1. Allocates a new pending promise.
/// 2. Creates resolve/reject functions bound to that promise.
/// 3. Calls `executor(resolve, reject)`.
/// 4. Returns the promise.
fn promise_constructor(
    _this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let executor = args.first().copied().unwrap_or(RegisterValue::undefined());

    // Validate executor is callable.
    if executor.as_object_handle().is_none() {
        return Err(VmNativeCallError::Internal(
            "Promise constructor requires a function argument".into(),
        ));
    }

    // Allocate promise.
    let promise_handle = runtime.objects_mut().alloc_promise();

    // Create resolve/reject native functions.
    // For now, these are placeholder handles — full executor callback invocation
    // requires NativeContext (re-entry into JS). When that's available, the
    // executor will be called synchronously here.
    //
    // For Test262 and basic usage, promises can be settled via the internal
    // API (promise.fulfill/promise.reject) and Promise.resolve/Promise.reject.

    Ok(RegisterValue::from_object_handle(promise_handle.0))
}

// ---------------------------------------------------------------------------
// Prototype methods
// ---------------------------------------------------------------------------

/// `Promise.prototype.then(onFulfilled, onRejected)` — ES2024 §27.2.5.4
fn promise_then(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let promise_handle = this
        .as_object_handle()
        .map(ObjectHandle)
        .ok_or_else(|| VmNativeCallError::Internal("then called on non-object".into()))?;

    let on_fulfill = args
        .first()
        .copied()
        .and_then(|v| v.as_object_handle().map(ObjectHandle));
    let on_reject = args
        .get(1)
        .copied()
        .and_then(|v| v.as_object_handle().map(ObjectHandle));

    // Create a new promise for the chain.
    let result_promise = runtime.objects_mut().alloc_promise();
    // Create resolve/reject functions for the result promise.
    // (Simplified: the result promise will be settled by the reaction job execution.)
    let resolve_fn = result_promise; // Placeholder: same handle used as resolve reference
    let reject_fn = result_promise; // Placeholder

    let capability = PromiseCapability {
        promise: result_promise,
        resolve: resolve_fn,
        reject: reject_fn,
    };

    // Register the reaction on the source promise.
    let promise = runtime
        .objects_mut()
        .get_promise_mut(promise_handle)
        .ok_or_else(|| VmNativeCallError::Internal("then called on non-promise".into()))?;

    if let Some(immediate_job) = promise.then(on_fulfill, on_reject, capability) {
        // Promise already settled — enqueue immediate job.
        runtime.microtasks_mut().enqueue_promise_job(immediate_job);
    }

    Ok(RegisterValue::from_object_handle(result_promise.0))
}

/// `Promise.prototype.catch(onRejected)` — ES2024 §27.2.5.1
/// Equivalent to `.then(undefined, onRejected)`.
fn promise_catch(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let reject_handler = args.first().copied().unwrap_or(RegisterValue::undefined());
    promise_then(this, &[RegisterValue::undefined(), reject_handler], runtime)
}

/// `Promise.prototype.finally(onFinally)` — ES2024 §27.2.5.3
/// Simplified: chains a then that calls onFinally regardless of outcome.
fn promise_finally(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let on_finally = args.first().copied().unwrap_or(RegisterValue::undefined());
    // Simplified: treat as .then(onFinally, onFinally)
    promise_then(this, &[on_finally, on_finally], runtime)
}

// ---------------------------------------------------------------------------
// Static methods
// ---------------------------------------------------------------------------

/// `Promise.resolve(value)` — ES2024 §27.2.4.7
fn promise_static_resolve(
    _this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let value = args.first().copied().unwrap_or(RegisterValue::undefined());

    // If value is already a promise, return it as-is.
    if let Some(handle) = value.as_object_handle().map(ObjectHandle)
        && runtime.objects().get_promise(handle).is_some()
    {
        return Ok(value);
    }

    // Create a new promise, immediately fulfilled.
    let handle = runtime.objects_mut().alloc_promise();
    let promise = runtime
        .objects_mut()
        .get_promise_mut(handle)
        .expect("just allocated");

    if let Some(jobs) = promise.fulfill(value) {
        for job in jobs {
            runtime.microtasks_mut().enqueue_promise_job(job);
        }
    }

    Ok(RegisterValue::from_object_handle(handle.0))
}

/// `Promise.reject(reason)` — ES2024 §27.2.4.6
fn promise_static_reject(
    _this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let reason = args.first().copied().unwrap_or(RegisterValue::undefined());

    let handle = runtime.objects_mut().alloc_promise();
    let promise = runtime
        .objects_mut()
        .get_promise_mut(handle)
        .expect("just allocated");

    if let Some(jobs) = promise.reject(reason) {
        for job in jobs {
            runtime.microtasks_mut().enqueue_promise_job(job);
        }
    }

    Ok(RegisterValue::from_object_handle(handle.0))
}
