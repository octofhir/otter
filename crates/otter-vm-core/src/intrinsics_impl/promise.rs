//! Promise constructor and prototype methods (ES2026)
//!
//! ## Constructor statics:
//! - `Promise.resolve(value)` — §27.2.4.7
//! - `Promise.reject(reason)` — §27.2.4.6
//! - `Promise.all(iterable)` — §27.2.4.1
//! - `Promise.race(iterable)` — §27.2.4.5
//! - `Promise.allSettled(iterable)` — §27.2.4.2
//! - `Promise.any(iterable)` — §27.2.4.3
//! - `Promise.withResolvers()` — ES2024
//!
//! ## Prototype methods:
//! - `Promise.prototype.then(onFulfilled, onRejected)` — §27.2.5.4
//! - `Promise.prototype.catch(onRejected)` — §27.2.5.1
//! - `Promise.prototype.finally(onFinally)` — §27.2.5.3
//!
//! ## Implementation Architecture:
//! Promise prototype methods and static methods use `NativeContext::enqueue_js_job()`
//! to register callbacks with the microtask queue. Promise combinators capture the
//! job queue Arc for async callback registration.
//!
//! The Promise constructor still uses an interception signal because `new Promise(executor)`
//! requires calling the executor with resolve/reject functions, which needs interpreter
//! support for proper execution context.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use crate::context::JsJobQueueTrait;
use crate::error::VmError;
use crate::gc::GcRef;
use crate::memory::MemoryManager;
use crate::object::{JsObject, PropertyDescriptor, PropertyKey};
use crate::promise::{JsPromise, JsPromiseJob, JsPromiseJobKind};
use crate::string::JsString;
use crate::value::Value;

// ============================================================================
// Helpers
// ============================================================================

/// Extract the internal promise from `this`.
///
/// Handles both raw `Value::promise` and JS wrapper objects `{ _internal: <promise> }`.
fn get_promise_from_this(this_val: &Value) -> Result<GcRef<JsPromise>, VmError> {
    if let Some(p) = this_val.as_promise() {
        return Ok(p.clone());
    }
    if let Some(obj) = this_val.as_object() {
        if let Some(internal) = obj.get(&PropertyKey::string("_internal")) {
            if let Some(p) = internal.as_promise() {
                return Ok(p.clone());
            }
        }
    }
    Err(VmError::type_error("Promise method called on non-promise"))
}

/// Extract the internal promise from a value (Option variant for combinators).
fn extract_internal_promise(value: &Value) -> Option<GcRef<JsPromise>> {
    if let Some(promise) = value.as_promise() {
        return Some(promise);
    }
    if let Some(obj) = value.as_object() {
        if let Some(internal) = obj.get(&PropertyKey::string("_internal")) {
            if let Some(promise) = internal.as_promise() {
                return Some(promise);
            }
        }
    }
    None
}

/// Check if a value is already a wrapped promise object.
fn is_wrapped_promise(value: &Value) -> bool {
    if let Some(obj) = value.as_object() {
        if let Some(internal) = obj.get(&PropertyKey::string("_internal")) {
            return internal.is_promise();
        }
    }
    false
}

/// Extract array items from an iterable value.
fn extract_array_items(value: Option<&Value>) -> Result<Vec<Value>, VmError> {
    let value = value.ok_or_else(|| VmError::type_error("Expected an iterable"))?;
    if let Some(obj) = value.as_object() {
        if obj.is_array() {
            let len = obj.array_length();
            let mut items = Vec::with_capacity(len);
            for i in 0..len {
                items.push(
                    obj.get(&PropertyKey::Index(i as u32))
                        .unwrap_or(Value::undefined()),
                );
            }
            return Ok(items);
        }
    }
    Ok(vec![value.clone()])
}

/// Create a JavaScript Promise wrapper object from an internal promise.
///
/// Creates an object with `_internal` field and copies methods from Promise.prototype.
fn create_js_promise_wrapper(
    ncx: &crate::context::NativeContext<'_>,
    internal: GcRef<JsPromise>,
) -> Value {
    create_js_promise_wrapper_with_mm(ncx.memory_manager(), ncx.ctx, internal)
}

/// Create a JavaScript Promise wrapper with explicit memory manager and context.
fn create_js_promise_wrapper_with_mm(
    mm: &Arc<MemoryManager>,
    ctx: &crate::context::VmContext,
    internal: GcRef<JsPromise>,
) -> Value {
    let obj = GcRef::new(JsObject::new(Value::null(), mm.clone()));

    // Set _internal to the raw promise
    let _ = obj.set(PropertyKey::string("_internal"), Value::promise(internal));

    // Try to get Promise.prototype and copy its methods
    if let Some(promise_ctor) = ctx.get_global("Promise").and_then(|v| v.as_object()) {
        if let Some(proto) = promise_ctor
            .get(&PropertyKey::string("prototype"))
            .and_then(|v| v.as_object())
        {
            // Copy then, catch, finally from prototype
            if let Some(then_fn) = proto.get(&PropertyKey::string("then")) {
                let _ = obj.set(PropertyKey::string("then"), then_fn);
            }
            if let Some(catch_fn) = proto.get(&PropertyKey::string("catch")) {
                let _ = obj.set(PropertyKey::string("catch"), catch_fn);
            }
            if let Some(finally_fn) = proto.get(&PropertyKey::string("finally")) {
                let _ = obj.set(PropertyKey::string("finally"), finally_fn);
            }
        }
    }

    Value::object(obj)
}

/// Helper to create an enqueue closure from a job queue.
fn make_enqueue_fn(
    queue: Arc<dyn JsJobQueueTrait + Send + Sync>,
) -> impl Fn(JsPromiseJob, Vec<Value>) + Send + Sync + Clone + 'static {
    move |job, args| {
        queue.enqueue(job, args);
    }
}

// ============================================================================
// Promise.prototype methods
// ============================================================================

/// Install `then`, `catch`, `finally` on Promise.prototype.
pub fn init_promise_prototype(
    proto: GcRef<JsObject>,
    fn_proto: GcRef<JsObject>,
    mm: &Arc<MemoryManager>,
) {
    // Promise.prototype.then(onFulfilled, onRejected) — §27.2.5.4
    proto.define_property(
        PropertyKey::string("then"),
        PropertyDescriptor::builtin_method(Value::native_function_with_proto(
            |this_val, args, ncx| {
                let source = get_promise_from_this(this_val)?;

                let on_fulfilled = args.first().cloned().unwrap_or(Value::undefined());
                let on_rejected = args.get(1).cloned().unwrap_or(Value::undefined());

                // Create result promise for chaining
                let result_promise = JsPromise::new();

                let fulfill_job = JsPromiseJob {
                    kind: if on_fulfilled.is_callable() {
                        JsPromiseJobKind::Fulfill
                    } else {
                        JsPromiseJobKind::PassthroughFulfill
                    },
                    callback: on_fulfilled,
                    this_arg: Value::undefined(),
                    result_promise: Some(result_promise),
                };

                let reject_job = JsPromiseJob {
                    kind: if on_rejected.is_callable() {
                        JsPromiseJobKind::Reject
                    } else {
                        JsPromiseJobKind::PassthroughReject
                    },
                    callback: on_rejected,
                    this_arg: Value::undefined(),
                    result_promise: Some(result_promise),
                };

                source.then_js(fulfill_job, |job, job_args| {
                    ncx.enqueue_js_job(job, job_args);
                });
                source.catch_js(reject_job, |job, job_args| {
                    ncx.enqueue_js_job(job, job_args);
                });

                Ok(create_js_promise_wrapper(ncx, result_promise))
            },
            mm.clone(),
            fn_proto,
        )),
    );

    // Promise.prototype.catch(onRejected) — §27.2.5.1
    proto.define_property(
        PropertyKey::string("catch"),
        PropertyDescriptor::builtin_method(Value::native_function_with_proto(
            |this_val, args, ncx| {
                let source = get_promise_from_this(this_val)?;

                let on_rejected = args.first().cloned().unwrap_or(Value::undefined());

                // Create result promise for chaining
                let result_promise = JsPromise::new();

                let fulfill_job = JsPromiseJob {
                    kind: JsPromiseJobKind::PassthroughFulfill,
                    callback: Value::undefined(),
                    this_arg: Value::undefined(),
                    result_promise: Some(result_promise),
                };

                let reject_job = JsPromiseJob {
                    kind: if on_rejected.is_callable() {
                        JsPromiseJobKind::Reject
                    } else {
                        JsPromiseJobKind::PassthroughReject
                    },
                    callback: on_rejected,
                    this_arg: Value::undefined(),
                    result_promise: Some(result_promise),
                };

                source.then_js(fulfill_job, |job, job_args| {
                    ncx.enqueue_js_job(job, job_args);
                });
                source.catch_js(reject_job, |job, job_args| {
                    ncx.enqueue_js_job(job, job_args);
                });

                Ok(create_js_promise_wrapper(ncx, result_promise))
            },
            mm.clone(),
            fn_proto,
        )),
    );

    // Promise.prototype.finally(onFinally) — §27.2.5.3
    proto.define_property(
        PropertyKey::string("finally"),
        PropertyDescriptor::builtin_method(Value::native_function_with_proto(
            |this_val, args, ncx| {
                let source = get_promise_from_this(this_val)?;

                let on_finally = args.first().cloned().unwrap_or(Value::undefined());

                // Create result promise for chaining
                let result_promise = JsPromise::new();

                let (fulfill_kind, reject_kind, fulfill_callback, reject_callback) =
                    if on_finally.is_callable() {
                        (
                            JsPromiseJobKind::FinallyFulfill,
                            JsPromiseJobKind::FinallyReject,
                            on_finally.clone(),
                            on_finally,
                        )
                    } else {
                        (
                            JsPromiseJobKind::PassthroughFulfill,
                            JsPromiseJobKind::PassthroughReject,
                            Value::undefined(),
                            Value::undefined(),
                        )
                    };

                let fulfill_job = JsPromiseJob {
                    kind: fulfill_kind,
                    callback: fulfill_callback,
                    this_arg: Value::undefined(),
                    result_promise: Some(result_promise),
                };

                let reject_job = JsPromiseJob {
                    kind: reject_kind,
                    callback: reject_callback,
                    this_arg: Value::undefined(),
                    result_promise: Some(result_promise),
                };

                source.then_js(fulfill_job, |job, job_args| {
                    ncx.enqueue_js_job(job, job_args);
                });
                source.catch_js(reject_job, |job, job_args| {
                    ncx.enqueue_js_job(job, job_args);
                });

                Ok(create_js_promise_wrapper(ncx, result_promise))
            },
            mm.clone(),
            fn_proto,
        )),
    );
}

// ============================================================================
// Promise constructor statics
// ============================================================================

/// Create Promise constructor function.
///
/// Implements ES2023 §27.2.3.1 Promise(executor):
/// - `new Promise(executor)` → creates promise and calls executor with resolve/reject
/// - `Promise(executor)` → throws TypeError (requires 'new')
///
/// The executor is called synchronously with (resolve, reject) arguments.
/// If the executor throws, the promise is rejected with the thrown value.
pub fn create_promise_constructor() -> Box<
    dyn Fn(&Value, &[Value], &mut crate::context::NativeContext<'_>) -> Result<Value, VmError>
        + Send
        + Sync,
> {
    Box::new(|_this, args, ncx| {
        // Check if called as constructor
        if !ncx.is_construct() {
            return Err(VmError::type_error("Promise constructor requires 'new'"));
        }

        // Get the executor function
        let executor = args.first().cloned().unwrap_or(Value::undefined());
        if !executor.is_callable() {
            return Err(VmError::type_error("Promise resolver is not a function"));
        }

        // Create the internal promise
        let promise = JsPromise::new();

        // Get job queue for callbacks
        let js_queue = ncx.js_job_queue();
        let enqueue_js_job = {
            let js_queue = js_queue.clone();
            move |job: crate::promise::JsPromiseJob, args: Vec<Value>| {
                if let Some(queue) = &js_queue {
                    queue.enqueue(job, args);
                }
            }
        };

        // Get function prototype for creating resolve/reject functions
        let fn_proto = ncx
            .ctx
            .function_prototype()
            .ok_or_else(|| VmError::internal("Function.prototype is not defined"))?;

        // Create resolve function
        let resolve_promise = promise;
        let enqueue_resolve = enqueue_js_job.clone();
        let resolve_fn = Value::native_function_with_proto(
            move |_this, args, _ncx| {
                let value = args.first().cloned().unwrap_or(Value::undefined());
                JsPromise::resolve_with_js_jobs(resolve_promise, value, enqueue_resolve.clone());
                Ok(Value::undefined())
            },
            ncx.memory_manager().clone(),
            fn_proto,
        );

        // Create reject function
        let reject_promise = promise;
        let enqueue_reject = enqueue_js_job.clone();
        let reject_fn = Value::native_function_with_proto(
            move |_this, args, _ncx| {
                let reason = args.first().cloned().unwrap_or(Value::undefined());
                JsPromise::reject_with_js_jobs(reject_promise, reason, enqueue_reject.clone());
                Ok(Value::undefined())
            },
            ncx.memory_manager().clone(),
            fn_proto,
        );

        // Call the executor with (resolve, reject)
        // If it throws, we catch the error and reject the promise
        let call_result =
            ncx.call_function(&executor, Value::undefined(), &[resolve_fn, reject_fn]);

        if let Err(err) = call_result {
            // Convert error to value and reject the promise
            // Convert error to value and reject the promise
            let error_val = match err {
                VmError::Exception(thrown) => thrown.value,
                VmError::TypeError(message) => create_error_value(ncx, "TypeError", &message),
                VmError::RangeError(message) => create_error_value(ncx, "RangeError", &message),
                VmError::ReferenceError(message) => {
                    create_error_value(ncx, "ReferenceError", &message)
                }
                VmError::SyntaxError(message) => create_error_value(ncx, "SyntaxError", &message),
                other => {
                    let message = other.to_string();
                    Value::string(JsString::intern(&message))
                }
            };
            JsPromise::reject_with_js_jobs(promise, error_val, enqueue_js_job);
        }

        // Create and return the JS promise wrapper
        Ok(create_js_promise_wrapper(ncx, promise))
    })
}

/// Helper to create an error value from error name and message.
fn create_error_value(ncx: &crate::context::NativeContext<'_>, name: &str, message: &str) -> Value {
    use crate::object::PropertyKey;

    // Try to get the error constructor prototype
    let proto = ncx
        .ctx
        .get_global(name)
        .and_then(|v| v.as_object())
        .and_then(|obj| obj.get(&PropertyKey::string("prototype")))
        .and_then(|v| v.as_object());

    let obj = GcRef::new(JsObject::new(
        proto.map(Value::object).unwrap_or_else(Value::null),
        ncx.memory_manager().clone(),
    ));

    let _ = obj.set(
        PropertyKey::string("name"),
        Value::string(JsString::intern(name)),
    );
    let _ = obj.set(
        PropertyKey::string("message"),
        Value::string(JsString::intern(message)),
    );

    let stack = if message.is_empty() {
        name.to_string()
    } else {
        format!("{}: {}", name, message)
    };
    let _ = obj.set(
        PropertyKey::string("stack"),
        Value::string(JsString::intern(&stack)),
    );

    Value::object(obj)
}

/// Install static methods on the Promise constructor object.
pub fn install_promise_statics(
    ctor: GcRef<JsObject>,
    fn_proto: GcRef<JsObject>,
    mm: &Arc<MemoryManager>,
) {
    // Promise.resolve(value) — §27.2.4.7
    ctor.define_property(
        PropertyKey::string("resolve"),
        PropertyDescriptor::builtin_method(Value::native_function_with_proto(
            |_this, args, ncx| {
                let value = args.first().cloned().unwrap_or(Value::undefined());

                // If already a wrapped promise, return as-is
                if is_wrapped_promise(&value) {
                    return Ok(value);
                }

                // If raw promise, wrap it
                if let Some(promise) = value.as_promise() {
                    return Ok(create_js_promise_wrapper(ncx, promise));
                }

                let result_promise = JsPromise::new();

                // Check if value is a thenable (has callable .then)
                if value.is_object() {
                    if let Some(obj) = value.as_object() {
                        if let Some(then_val) = obj.get(&PropertyKey::string("then")) {
                            if then_val.is_callable() {
                                // Schedule thenable resolution
                                let job = JsPromiseJob {
                                    kind: JsPromiseJobKind::ResolveThenable,
                                    callback: then_val,
                                    this_arg: value,
                                    result_promise: Some(result_promise),
                                };
                                ncx.enqueue_js_job(job, Vec::new());
                                return Ok(create_js_promise_wrapper(ncx, result_promise));
                            }
                        }
                    }
                }

                // Resolve with the value directly
                JsPromise::resolve_with_js_jobs(result_promise, value, |job, args| {
                    ncx.enqueue_js_job(job, args);
                });
                Ok(create_js_promise_wrapper(ncx, result_promise))
            },
            mm.clone(),
            fn_proto,
        )),
    );

    // Promise.reject(reason) — §27.2.4.6
    ctor.define_property(
        PropertyKey::string("reject"),
        PropertyDescriptor::builtin_method(Value::native_function_with_proto(
            |_this, args, ncx| {
                let reason = args.first().cloned().unwrap_or(Value::undefined());
                let result_promise = JsPromise::new();
                JsPromise::reject_with_js_jobs(result_promise, reason, |job, args| {
                    ncx.enqueue_js_job(job, args);
                });
                Ok(create_js_promise_wrapper(ncx, result_promise))
            },
            mm.clone(),
            fn_proto,
        )),
    );

    // Promise.all(iterable) — §27.2.4.1
    ctor.define_property(
        PropertyKey::string("all"),
        PropertyDescriptor::builtin_method(Value::native_function_with_proto(
            |_this, args, ncx| {
                let items = extract_array_items(args.first())?;
                let result_promise = JsPromise::new();
                let mm = ncx.memory_manager().clone();

                // Empty array resolves immediately with []
                if items.is_empty() {
                    let arr = GcRef::new(JsObject::array(0, mm));
                    JsPromise::resolve_with_js_jobs(result_promise, Value::array(arr), |job, args| {
                        ncx.enqueue_js_job(job, args);
                    });
                    return Ok(create_js_promise_wrapper(ncx, result_promise));
                }

                // Get job queue for async callbacks
                let queue = ncx
                    .js_job_queue()
                    .ok_or_else(|| VmError::type_error("Promise.all requires a job queue"))?;
                let enqueue = make_enqueue_fn(queue);

                let count = items.len();
                let remaining = Arc::new(AtomicUsize::new(count));
                let results: Arc<Mutex<Vec<Option<Value>>>> =
                    Arc::new(Mutex::new(vec![None; count]));
                let rejected = Arc::new(AtomicBool::new(false));

                for (index, item) in items.into_iter().enumerate() {
                    let result_p = result_promise;
                    let remaining = remaining.clone();
                    let results = results.clone();
                    let rejected = rejected.clone();
                    let mm_inner = mm.clone();
                    let enqueue_fulfill = enqueue.clone();
                    let enqueue_reject = enqueue.clone();

                    let source_promise = if let Some(promise) = extract_internal_promise(&item) {
                        promise
                    } else {
                        let p = JsPromise::new();
                        JsPromise::resolve_with_js_jobs(p, item, enqueue.clone());
                        p
                    };

                    let result_p_reject = result_p.clone();
                    let rejected_check = rejected.clone();

                    source_promise.then(move |value| {
                        if rejected.load(Ordering::Acquire) {
                            return;
                        }
                        if let Ok(mut locked) = results.lock() {
                            locked[index] = Some(value);
                        }
                        if remaining.fetch_sub(1, Ordering::AcqRel) == 1 {
                            let arr = GcRef::new(JsObject::array(count, mm_inner.clone()));
                            if let Ok(locked) = results.lock() {
                                for (i, v) in locked.iter().enumerate() {
                                    if let Some(val) = v {
                                        let _ = arr.set(PropertyKey::Index(i as u32), val.clone());
                                    }
                                }
                            }
                            JsPromise::resolve_with_js_jobs(result_p, Value::array(arr), enqueue_fulfill.clone());
                        }
                    });
                    source_promise.catch(move |error| {
                        if !rejected_check.swap(true, Ordering::AcqRel) {
                            JsPromise::reject_with_js_jobs(result_p_reject, error, enqueue_reject.clone());
                        }
                    });
                }

                Ok(create_js_promise_wrapper(ncx, result_promise))
            },
            mm.clone(),
            fn_proto,
        )),
    );

    // Promise.race(iterable) — §27.2.4.5
    ctor.define_property(
        PropertyKey::string("race"),
        PropertyDescriptor::builtin_method(Value::native_function_with_proto(
            |_this, args, ncx| {
                let items = extract_array_items(args.first())?;
                let result_promise = JsPromise::new();
                let settled = Arc::new(AtomicBool::new(false));

                // Get job queue for async callbacks
                let queue = ncx
                    .js_job_queue()
                    .ok_or_else(|| VmError::type_error("Promise.race requires a job queue"))?;
                let enqueue = make_enqueue_fn(queue);

                for item in items {
                    let result_p = result_promise;
                    let result_p_reject = result_promise;
                    let settled1 = settled.clone();
                    let settled2 = settled.clone();
                    let enqueue_fulfill = enqueue.clone();
                    let enqueue_reject = enqueue.clone();

                    let source_promise = if let Some(promise) = extract_internal_promise(&item) {
                        promise
                    } else {
                        let p = JsPromise::new();
                        JsPromise::resolve_with_js_jobs(p, item, enqueue.clone());
                        p
                    };

                    source_promise.then(move |value| {
                        if !settled1.swap(true, Ordering::AcqRel) {
                            JsPromise::resolve_with_js_jobs(result_p, value, enqueue_fulfill.clone());
                        }
                    });
                    source_promise.catch(move |error| {
                        if !settled2.swap(true, Ordering::AcqRel) {
                            JsPromise::reject_with_js_jobs(result_p_reject, error, enqueue_reject.clone());
                        }
                    });
                }

                Ok(create_js_promise_wrapper(ncx, result_promise))
            },
            mm.clone(),
            fn_proto,
        )),
    );

    // Promise.allSettled(iterable) — §27.2.4.2
    ctor.define_property(
        PropertyKey::string("allSettled"),
        PropertyDescriptor::builtin_method(Value::native_function_with_proto(
            |_this, args, ncx| {
                let items = extract_array_items(args.first())?;
                let result_promise = JsPromise::new();
                let mm = ncx.memory_manager().clone();

                // Empty array resolves immediately with []
                if items.is_empty() {
                    let arr = GcRef::new(JsObject::array(0, mm));
                    JsPromise::resolve_with_js_jobs(result_promise, Value::array(arr), |job, args| {
                        ncx.enqueue_js_job(job, args);
                    });
                    return Ok(create_js_promise_wrapper(ncx, result_promise));
                }

                // Get job queue for async callbacks
                let queue = ncx.js_job_queue().ok_or_else(|| {
                    VmError::type_error("Promise.allSettled requires a job queue")
                })?;
                let enqueue = make_enqueue_fn(queue);

                let count = items.len();
                let remaining = Arc::new(AtomicUsize::new(count));
                let results: Arc<Mutex<Vec<Option<Value>>>> =
                    Arc::new(Mutex::new(vec![None; count]));

                for (index, item) in items.into_iter().enumerate() {
                    let result_p = result_promise;
                    let result_p2 = result_promise;
                    let remaining = remaining.clone();
                    let remaining2 = remaining.clone();
                    let results = results.clone();
                    let results2 = results.clone();
                    let mm_t = mm.clone();
                    let mm_c = mm.clone();
                    let enqueue_fulfill = enqueue.clone();
                    let enqueue_reject = enqueue.clone();

                    let source_promise = if let Some(promise) = extract_internal_promise(&item) {
                        promise
                    } else {
                        let p = JsPromise::new();
                        JsPromise::resolve_with_js_jobs(p, item, enqueue.clone());
                        p
                    };

                    source_promise.then(move |value| {
                        let obj = GcRef::new(JsObject::new(Value::null(), mm_t.clone()));
                        let _ = obj.set(
                            "status".into(),
                            Value::string(JsString::intern("fulfilled")),
                        );
                        let _ = obj.set("value".into(), value);
                        if let Ok(mut locked) = results.lock() {
                            locked[index] = Some(Value::object(obj));
                        }
                        if remaining.fetch_sub(1, Ordering::AcqRel) == 1 {
                            let arr = GcRef::new(JsObject::array(count, mm_t.clone()));
                            if let Ok(locked) = results.lock() {
                                for (i, v) in locked.iter().enumerate() {
                                    if let Some(val) = v {
                                        let _ = arr.set(PropertyKey::Index(i as u32), val.clone());
                                    }
                                }
                            }
                            JsPromise::resolve_with_js_jobs(result_p, Value::array(arr), enqueue_fulfill.clone());
                        }
                    });
                    source_promise.catch(move |error| {
                        let obj = GcRef::new(JsObject::new(Value::null(), mm_c.clone()));
                        let _ = obj.set("status".into(), Value::string(JsString::intern("rejected")));
                        let _ = obj.set("reason".into(), error);
                        if let Ok(mut locked) = results2.lock() {
                            locked[index] = Some(Value::object(obj));
                        }
                        if remaining2.fetch_sub(1, Ordering::AcqRel) == 1 {
                            let arr = GcRef::new(JsObject::array(count, mm_c.clone()));
                            if let Ok(locked) = results2.lock() {
                                for (i, v) in locked.iter().enumerate() {
                                    if let Some(val) = v {
                                        let _ = arr.set(PropertyKey::Index(i as u32), val.clone());
                                    }
                                }
                            }
                            JsPromise::resolve_with_js_jobs(result_p2, Value::array(arr), enqueue_reject.clone());
                        }
                    });
                }

                Ok(create_js_promise_wrapper(ncx, result_promise))
            },
            mm.clone(),
            fn_proto,
        )),
    );

    // Promise.any(iterable) — §27.2.4.3
    ctor.define_property(
        PropertyKey::string("any"),
        PropertyDescriptor::builtin_method(Value::native_function_with_proto(
            |_this, args, ncx| {
                let items = extract_array_items(args.first())?;
                let result_promise = JsPromise::new();
                let mm = ncx.memory_manager().clone();

                // Empty array rejects with AggregateError
                if items.is_empty() {
                    let err = Value::string(JsString::intern("All promises were rejected"));
                    JsPromise::reject_with_js_jobs(result_promise, err, |job, args| {
                        ncx.enqueue_js_job(job, args);
                    });
                    return Ok(create_js_promise_wrapper(ncx, result_promise));
                }

                // Get job queue for async callbacks
                let queue = ncx
                    .js_job_queue()
                    .ok_or_else(|| VmError::type_error("Promise.any requires a job queue"))?;
                let enqueue = make_enqueue_fn(queue);

                let count = items.len();
                let fulfilled = Arc::new(AtomicBool::new(false));
                let remaining = Arc::new(AtomicUsize::new(count));
                let errors: Arc<Mutex<Vec<Option<Value>>>> =
                    Arc::new(Mutex::new(vec![None; count]));

                for (index, item) in items.into_iter().enumerate() {
                    let result_p = result_promise;
                    let result_p2 = result_promise;
                    let fulfilled1 = fulfilled.clone();
                    let fulfilled2 = fulfilled.clone();
                    let remaining = remaining.clone();
                    let errors = errors.clone();
                    let mm_err = mm.clone();
                    let enqueue_fulfill = enqueue.clone();
                    let enqueue_reject = enqueue.clone();

                    let source_promise = if let Some(promise) = extract_internal_promise(&item) {
                        promise
                    } else {
                        let p = JsPromise::new();
                        JsPromise::resolve_with_js_jobs(p, item, enqueue.clone());
                        p
                    };

                    source_promise.then(move |value| {
                        if !fulfilled1.swap(true, Ordering::AcqRel) {
                            JsPromise::resolve_with_js_jobs(result_p, value, enqueue_fulfill.clone());
                        }
                    });
                    source_promise.catch(move |error| {
                        if fulfilled2.load(Ordering::Acquire) {
                            return;
                        }
                        if let Ok(mut locked) = errors.lock() {
                            locked[index] = Some(error);
                        }
                        if remaining.fetch_sub(1, Ordering::AcqRel) == 1 {
                            let errs: Vec<Value> = if let Ok(locked) = errors.lock() {
                                locked.iter().filter_map(|e| e.clone()).collect()
                            } else {
                                vec![]
                            };
                            let arr = GcRef::new(JsObject::array(errs.len(), mm_err.clone()));
                            for (i, e) in errs.iter().enumerate() {
                                let _ = arr.set(PropertyKey::Index(i as u32), e.clone());
                            }
                            let agg = GcRef::new(JsObject::new(Value::null(), mm_err.clone()));
                            let _ = agg.set(
                                "message".into(),
                                Value::string(JsString::intern("All promises were rejected")),
                            );
                            let _ = agg.set("errors".into(), Value::array(arr));
                            JsPromise::reject_with_js_jobs(result_p2, Value::object(agg), enqueue_reject.clone());
                        }
                    });
                }

                Ok(create_js_promise_wrapper(ncx, result_promise))
            },
            mm.clone(),
            fn_proto,
        )),
    );

    // Promise.withResolvers() — ES2024
    {
        let mm_wr = mm.clone();
        ctor.define_property(
            PropertyKey::string("withResolvers"),
            PropertyDescriptor::builtin_method(Value::native_function_with_proto(
                move |_this, _args, ncx| {
                    let promise = JsPromise::new();
                    let result = GcRef::new(JsObject::new(Value::null(), mm_wr.clone()));

                    // Get job queue for resolver/rejecter functions
                    let queue = ncx.js_job_queue();

                    // Create wrapped promise for the result
                    let wrapped_promise = create_js_promise_wrapper(ncx, promise);
                    let _ = result.set("promise".into(), wrapped_promise);

                    // Create resolve function that captures the promise
                    let promise_for_resolve = promise;
                    let queue_for_resolve = queue.clone();
                    let resolve_fn = Value::native_function_with_proto(
                        move |_this: &Value, args: &[Value], _ncx: &mut crate::context::NativeContext<'_>| {
                            let value = args.first().cloned().unwrap_or(Value::undefined());
                            if let Some(q) = &queue_for_resolve {
                                JsPromise::resolve_with_js_jobs(promise_for_resolve, value, |job, args| {
                                    q.enqueue(job, args);
                                });
                            } else {
                                // No queue - resolve synchronously (no callbacks will fire)
                                promise_for_resolve.resolve(value);
                            }
                            Ok(Value::undefined())
                        },
                        mm_wr.clone(),
                        fn_proto,
                    );
                    let _ = result.set("resolve".into(), resolve_fn);

                    // Create reject function that captures the promise
                    let promise_for_reject = promise;
                    let queue_for_reject = queue;
                    let reject_fn = Value::native_function_with_proto(
                        move |_this: &Value, args: &[Value], _ncx: &mut crate::context::NativeContext<'_>| {
                            let reason = args.first().cloned().unwrap_or(Value::undefined());
                            if let Some(q) = &queue_for_reject {
                                JsPromise::reject_with_js_jobs(promise_for_reject, reason, |job, args| {
                                    q.enqueue(job, args);
                                });
                            } else {
                                // No queue - reject synchronously (no callbacks will fire)
                                promise_for_reject.reject(reason);
                            }
                            Ok(Value::undefined())
                        },
                        mm_wr.clone(),
                        fn_proto,
                    );
                    let _ = result.set("reject".into(), reject_fn);

                    Ok(Value::object(result))
                },
                mm.clone(),
                fn_proto,
            )),
        );
    }
}
