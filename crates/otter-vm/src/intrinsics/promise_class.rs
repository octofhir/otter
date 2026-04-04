//! Promise constructor and prototype methods — ES2024 §27.2.
//!
//! Implements the Promise class using the descriptor/builder/installer pipeline.
//! Methods delegate to [`JsPromise`] for state management and to
//! [`MicrotaskQueue`] for job scheduling.
//!
//! Spec: <https://tc39.es/ecma262/#sec-promise-objects>

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
        // Prototype methods — §27.2.5
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
        // Static methods — §27.2.4
        .with_binding(NativeBindingDescriptor::new(
            NativeBindingTarget::Constructor,
            NativeFunctionDescriptor::method("resolve", 1, promise_static_resolve),
        ))
        .with_binding(NativeBindingDescriptor::new(
            NativeBindingTarget::Constructor,
            NativeFunctionDescriptor::method("reject", 1, promise_static_reject),
        ))
        .with_binding(NativeBindingDescriptor::new(
            NativeBindingTarget::Constructor,
            NativeFunctionDescriptor::method("all", 1, promise_static_all),
        ))
        .with_binding(NativeBindingDescriptor::new(
            NativeBindingTarget::Constructor,
            NativeFunctionDescriptor::method("race", 1, promise_static_race),
        ))
        .with_binding(NativeBindingDescriptor::new(
            NativeBindingTarget::Constructor,
            NativeFunctionDescriptor::method("allSettled", 1, promise_static_all_settled),
        ))
        .with_binding(NativeBindingDescriptor::new(
            NativeBindingTarget::Constructor,
            NativeFunctionDescriptor::method("any", 1, promise_static_any),
        ))
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Allocates a promise capability (promise + resolve + reject) with the given prototype.
fn alloc_promise_capability_with_proto(
    runtime: &mut RuntimeState,
    proto: ObjectHandle,
) -> PromiseCapability {
    let promise = runtime.objects_mut().alloc_promise_with_proto(proto);
    let resolve = runtime
        .objects_mut()
        .alloc_promise_capability_function(promise, crate::promise::ReactionKind::Fulfill);
    let reject = runtime
        .objects_mut()
        .alloc_promise_capability_function(promise, crate::promise::ReactionKind::Reject);
    PromiseCapability {
        promise,
        resolve,
        reject,
    }
}

/// Allocates a promise capability using the runtime's %Promise.prototype%.
fn alloc_promise_capability(runtime: &mut RuntimeState) -> PromiseCapability {
    let proto = runtime.intrinsics().promise_prototype();
    alloc_promise_capability_with_proto(runtime, proto)
}

// ---------------------------------------------------------------------------
// Constructor — §27.2.3
// ---------------------------------------------------------------------------

/// `new Promise(executor)` — ES2024 §27.2.3
/// Spec: <https://tc39.es/ecma262/#sec-promise-executor>
///
/// 1. Allocates a new pending promise.
/// 2. Creates resolve/reject PromiseCapabilityFunction objects.
/// 3. Calls `executor(resolve, reject)` synchronously.
/// 4. If executor throws, reject the promise with the thrown value.
/// 5. Returns the promise.
fn promise_constructor(
    _this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let executor = args.first().copied().unwrap_or(RegisterValue::undefined());

    // §27.2.3 step 2: If IsCallable(executor) is false, throw a TypeError.
    let executor_handle = executor
        .as_object_handle()
        .map(ObjectHandle)
        .ok_or_else(|| {
            VmNativeCallError::Internal("Promise resolver undefined is not a function".into())
        })?;

    // §27.2.3 step 3-6: Create promise and capability.
    let capability = alloc_promise_capability(runtime);
    let resolve_rv = RegisterValue::from_object_handle(capability.resolve.0);
    let reject_rv = RegisterValue::from_object_handle(capability.reject.0);

    // §27.2.3 step 7: Call executor(resolve, reject) synchronously.
    let call_result = runtime.call_host_function(
        Some(executor_handle),
        RegisterValue::undefined(),
        &[resolve_rv, reject_rv],
    );

    // §27.2.3 step 8: If executor threw, reject the promise with the error.
    if let Err(VmNativeCallError::Thrown(thrown)) = call_result {
        // Call reject(thrown) to settle the promise.
        let _ = runtime.call_host_function(
            Some(capability.reject),
            RegisterValue::undefined(),
            &[thrown],
        );
    }

    Ok(RegisterValue::from_object_handle(capability.promise.0))
}

// ---------------------------------------------------------------------------
// Prototype methods — §27.2.5
// ---------------------------------------------------------------------------

/// `Promise.prototype.then(onFulfilled, onRejected)` — ES2024 §27.2.5.4
/// Spec: <https://tc39.es/ecma262/#sec-promise.prototype.then>
fn promise_then(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let promise_handle = this
        .as_object_handle()
        .map(ObjectHandle)
        .ok_or_else(|| VmNativeCallError::Internal("then called on non-object".into()))?;

    // Validate this is a promise.
    if runtime.objects().get_promise(promise_handle).is_none() {
        return Err(VmNativeCallError::Internal(
            "then called on non-promise".into(),
        ));
    }

    let on_fulfill = args
        .first()
        .copied()
        .and_then(|v| v.as_object_handle().map(ObjectHandle));
    let on_reject = args
        .get(1)
        .copied()
        .and_then(|v| v.as_object_handle().map(ObjectHandle));

    // §27.2.5.4 step 5: Create a new promise capability for the result.
    let capability = alloc_promise_capability(runtime);

    // §27.2.5.4 step 6: PerformPromiseThen.
    let promise = runtime
        .objects_mut()
        .get_promise_mut(promise_handle)
        .expect("validated above");

    if let Some(immediate_job) = promise.then(on_fulfill, on_reject, capability) {
        runtime.microtasks_mut().enqueue_promise_job(immediate_job);
    }

    Ok(RegisterValue::from_object_handle(capability.promise.0))
}

/// `Promise.prototype.catch(onRejected)` — ES2024 §27.2.5.1
/// Spec: <https://tc39.es/ecma262/#sec-promise.prototype.catch>
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
/// Spec: <https://tc39.es/ecma262/#sec-promise.prototype.finally>
///
/// `finally` preserves the settlement value: the returned promise resolves/rejects
/// with the original value unless onFinally itself throws.
fn promise_finally(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    use crate::promise::PromiseFinallyKind;

    let on_finally = args.first().copied().unwrap_or(RegisterValue::undefined());

    // §27.2.5.3 step 5: If onFinally is not callable, pass through directly.
    let on_finally_handle = match on_finally.as_object_handle().map(ObjectHandle) {
        Some(h) if runtime.objects().is_callable(h) => h,
        _ => return promise_then(this, &[on_finally, on_finally], runtime),
    };

    // §27.2.5.3 step 6-7: Create ThenFinally and CatchFinally wrapper functions.
    let constructor = runtime.intrinsics().promise_constructor();
    let then_finally = runtime.objects_mut().alloc_promise_finally_function(
        on_finally_handle,
        constructor,
        PromiseFinallyKind::ThenFinally,
    );
    let catch_finally = runtime.objects_mut().alloc_promise_finally_function(
        on_finally_handle,
        constructor,
        PromiseFinallyKind::CatchFinally,
    );

    let then_rv = RegisterValue::from_object_handle(then_finally.0);
    let catch_rv = RegisterValue::from_object_handle(catch_finally.0);
    promise_then(this, &[then_rv, catch_rv], runtime)
}

// ---------------------------------------------------------------------------
// Static methods — §27.2.4
// ---------------------------------------------------------------------------

/// `Promise.resolve(value)` — ES2024 §27.2.4.7
/// Spec: <https://tc39.es/ecma262/#sec-promise.resolve>
fn promise_static_resolve(
    _this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let value = args.first().copied().unwrap_or(RegisterValue::undefined());

    // §27.2.4.7 step 2: If value is a Promise, return it as-is.
    if let Some(handle) = value.as_object_handle().map(ObjectHandle)
        && runtime.objects().get_promise(handle).is_some()
    {
        return Ok(value);
    }

    // Create a new promise, immediately fulfilled.
    let proto = runtime.intrinsics().promise_prototype();
    let handle = runtime.objects_mut().alloc_promise_with_proto(proto);
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
/// Spec: <https://tc39.es/ecma262/#sec-promise.reject>
fn promise_static_reject(
    _this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let reason = args.first().copied().unwrap_or(RegisterValue::undefined());

    let proto = runtime.intrinsics().promise_prototype();
    let handle = runtime.objects_mut().alloc_promise_with_proto(proto);
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

// ---------------------------------------------------------------------------
// Combinators — §27.2.4
// ---------------------------------------------------------------------------

/// Helper: iterate an iterable and collect promises, wrapping non-promises via
/// Promise.resolve(). Returns a Vec of (index, promise_handle) pairs.
fn collect_promises_from_iterable(
    args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<Vec<ObjectHandle>, VmNativeCallError> {
    let iterable = args.first().copied().unwrap_or(RegisterValue::undefined());

    // If iterable is an array, iterate its elements.
    if let Some(handle) = iterable.as_object_handle().map(ObjectHandle)
        && let Ok(elements) = runtime.objects().array_elements(handle)
    {
        let elements = elements.to_vec();
        let mut promises = Vec::with_capacity(elements.len());
        for elem in &elements {
            let resolved = promise_static_resolve(&RegisterValue::undefined(), &[*elem], runtime)?;
            let h = resolved.as_object_handle().map(ObjectHandle).unwrap();
            promises.push(h);
        }
        return Ok(promises);
    }

    // Fallback: treat as single-element if not iterable.
    if iterable == RegisterValue::undefined() || iterable == RegisterValue::null() {
        return Ok(Vec::new());
    }

    // Single non-iterable value — wrap it.
    let resolved = promise_static_resolve(&RegisterValue::undefined(), &[iterable], runtime)?;
    let h = resolved.as_object_handle().map(ObjectHandle).unwrap();
    Ok(vec![h])
}

/// `Promise.all(iterable)` — ES2024 §27.2.4.1
/// Spec: <https://tc39.es/ecma262/#sec-promise.all>
///
/// Returns a promise that fulfills with an array of results when all input
/// promises fulfill, or rejects with the first rejection reason.
fn promise_static_all(
    _this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    use crate::promise::PromiseCombinatorKind;

    let promises = collect_promises_from_iterable(args, runtime)?;
    let result_cap = alloc_promise_capability(runtime);

    if promises.is_empty() {
        let arr = runtime.objects_mut().alloc_array();
        let promise = runtime
            .objects_mut()
            .get_promise_mut(result_cap.promise)
            .unwrap();
        promise.fulfill(RegisterValue::from_object_handle(arr.0));
        return Ok(RegisterValue::from_object_handle(result_cap.promise.0));
    }

    let count = promises.len();

    // Allocate result array pre-filled with undefined at each index.
    let result_array = runtime.objects_mut().alloc_array();
    for _ in 0..count {
        runtime
            .objects_mut()
            .push_element(result_array, RegisterValue::undefined())
            .ok();
    }

    // Shared mutable counter: 1-element array with initial value = count.
    let counter = runtime.objects_mut().alloc_array();
    runtime
        .objects_mut()
        .push_element(counter, RegisterValue::from_i32(count as i32))
        .ok();

    for (index, input_promise) in promises.into_iter().enumerate() {
        // §27.2.4.1.1: Per-element resolve function.
        let resolve_element = runtime.objects_mut().alloc_promise_combinator_element(
            PromiseCombinatorKind::AllResolve,
            index as u32,
            result_array,
            counter,
            result_cap,
        );

        // Capability for this element's .then() chain.
        let cap = PromiseCapability {
            promise: result_cap.promise,
            resolve: result_cap.resolve,
            reject: result_cap.reject,
        };

        let input = runtime
            .objects_mut()
            .get_promise_mut(input_promise)
            .ok_or_else(|| {
                VmNativeCallError::Internal("Promise.all element is not a promise".into())
            })?;

        // on fulfill → per-element resolve, on reject → reject the whole result.
        if let Some(job) = input.then(Some(resolve_element), Some(result_cap.reject), cap) {
            runtime.microtasks_mut().enqueue_promise_job(job);
        }
    }

    Ok(RegisterValue::from_object_handle(result_cap.promise.0))
}

/// `Promise.race(iterable)` — ES2024 §27.2.4.5
/// Spec: <https://tc39.es/ecma262/#sec-promise.race>
///
/// Returns a promise that settles with the first input promise to settle.
fn promise_static_race(
    _this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let promises = collect_promises_from_iterable(args, runtime)?;
    let result_cap = alloc_promise_capability(runtime);

    for input_promise in promises {
        // Register reactions that settle the result promise.
        let cap = PromiseCapability {
            promise: result_cap.promise,
            resolve: result_cap.resolve,
            reject: result_cap.reject,
        };

        let input = runtime
            .objects_mut()
            .get_promise_mut(input_promise)
            .ok_or_else(|| {
                VmNativeCallError::Internal("Promise.race element is not a promise".into())
            })?;

        if let Some(job) = input.then(Some(result_cap.resolve), Some(result_cap.reject), cap) {
            runtime.microtasks_mut().enqueue_promise_job(job);
        }
    }

    Ok(RegisterValue::from_object_handle(result_cap.promise.0))
}

/// `Promise.allSettled(iterable)` — ES2024 §27.2.4.2
/// Spec: <https://tc39.es/ecma262/#sec-promise.allsettled>
///
/// Returns a promise that fulfills with an array of { status, value/reason }
/// objects once all input promises have settled (fulfilled or rejected).
fn promise_static_all_settled(
    _this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    use crate::promise::PromiseCombinatorKind;

    let promises = collect_promises_from_iterable(args, runtime)?;
    let result_cap = alloc_promise_capability(runtime);

    if promises.is_empty() {
        let arr = runtime.objects_mut().alloc_array();
        let promise = runtime
            .objects_mut()
            .get_promise_mut(result_cap.promise)
            .unwrap();
        promise.fulfill(RegisterValue::from_object_handle(arr.0));
        return Ok(RegisterValue::from_object_handle(result_cap.promise.0));
    }

    let count = promises.len();

    let result_array = runtime.objects_mut().alloc_array();
    for _ in 0..count {
        runtime
            .objects_mut()
            .push_element(result_array, RegisterValue::undefined())
            .ok();
    }

    let counter = runtime.objects_mut().alloc_array();
    runtime
        .objects_mut()
        .push_element(counter, RegisterValue::from_i32(count as i32))
        .ok();

    for (index, input_promise) in promises.into_iter().enumerate() {
        // §27.2.4.2.1: Per-element resolve function creates { status: "fulfilled", value }.
        let resolve_element = runtime.objects_mut().alloc_promise_combinator_element(
            PromiseCombinatorKind::AllSettledResolve,
            index as u32,
            result_array,
            counter,
            result_cap,
        );
        // §27.2.4.2.2: Per-element reject function creates { status: "rejected", reason }.
        let reject_element = runtime.objects_mut().alloc_promise_combinator_element(
            PromiseCombinatorKind::AllSettledReject,
            index as u32,
            result_array,
            counter,
            result_cap,
        );

        let cap = PromiseCapability {
            promise: result_cap.promise,
            resolve: result_cap.resolve,
            reject: result_cap.reject,
        };

        let input = runtime
            .objects_mut()
            .get_promise_mut(input_promise)
            .ok_or_else(|| {
                VmNativeCallError::Internal("Promise.allSettled element is not a promise".into())
            })?;

        if let Some(job) = input.then(Some(resolve_element), Some(reject_element), cap) {
            runtime.microtasks_mut().enqueue_promise_job(job);
        }
    }

    Ok(RegisterValue::from_object_handle(result_cap.promise.0))
}

/// `Promise.any(iterable)` — ES2024 §27.2.4.3
/// Spec: <https://tc39.es/ecma262/#sec-promise.any>
///
/// Returns a promise that fulfills with the first fulfilled value,
/// or rejects with an AggregateError if all reject.
fn promise_static_any(
    _this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    use crate::promise::PromiseCombinatorKind;

    let promises = collect_promises_from_iterable(args, runtime)?;
    let result_cap = alloc_promise_capability(runtime);

    if promises.is_empty() {
        let err = runtime
            .alloc_type_error("All promises were rejected")
            .map_err(|e| {
                VmNativeCallError::Internal(format!("AggregateError alloc failed: {e:?}").into())
            })?;
        let promise = runtime
            .objects_mut()
            .get_promise_mut(result_cap.promise)
            .unwrap();
        promise.reject(RegisterValue::from_object_handle(err.0));
        return Ok(RegisterValue::from_object_handle(result_cap.promise.0));
    }

    let count = promises.len();

    // Errors array — collects rejection reasons.
    let errors_array = runtime.objects_mut().alloc_array();
    for _ in 0..count {
        runtime
            .objects_mut()
            .push_element(errors_array, RegisterValue::undefined())
            .ok();
    }

    let counter = runtime.objects_mut().alloc_array();
    runtime
        .objects_mut()
        .push_element(counter, RegisterValue::from_i32(count as i32))
        .ok();

    for (index, input_promise) in promises.into_iter().enumerate() {
        // §27.2.4.3.1: Per-element reject function.
        let reject_element = runtime.objects_mut().alloc_promise_combinator_element(
            PromiseCombinatorKind::AnyReject,
            index as u32,
            errors_array,
            counter,
            result_cap,
        );

        let cap = PromiseCapability {
            promise: result_cap.promise,
            resolve: result_cap.resolve,
            reject: result_cap.reject,
        };

        let input = runtime
            .objects_mut()
            .get_promise_mut(input_promise)
            .ok_or_else(|| {
                VmNativeCallError::Internal("Promise.any element is not a promise".into())
            })?;

        // on fulfill → resolve result (first wins), on reject → per-element reject.
        if let Some(job) = input.then(Some(result_cap.resolve), Some(reject_element), cap) {
            runtime.microtasks_mut().enqueue_promise_job(job);
        }
    }

    Ok(RegisterValue::from_object_handle(result_cap.promise.0))
}
