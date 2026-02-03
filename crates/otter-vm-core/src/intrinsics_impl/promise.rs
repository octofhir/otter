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
use crate::error::{InterceptionSignal, VmError};
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
fn get_promise_from_this(this_val: &Value) -> Result<Arc<JsPromise>, VmError> {
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
fn extract_internal_promise(value: &Value) -> Option<Arc<JsPromise>> {
    if let Some(promise) = value.as_promise() {
        return Some(promise.clone());
    }
    if let Some(obj) = value.as_object() {
        if let Some(internal) = obj.get(&PropertyKey::string("_internal")) {
            if let Some(promise) = internal.as_promise() {
                return Some(promise.clone());
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
fn create_js_promise_wrapper(ncx: &crate::context::NativeContext<'_>, internal: Arc<JsPromise>) -> Value {
    create_js_promise_wrapper_with_mm(ncx.memory_manager(), ncx.ctx, internal)
}

/// Create a JavaScript Promise wrapper with explicit memory manager and context.
fn create_js_promise_wrapper_with_mm(
    mm: &Arc<MemoryManager>,
    ctx: &crate::context::VmContext,
    internal: Arc<JsPromise>,
) -> Value {
    let obj = GcRef::new(JsObject::new(None, mm.clone()));

    // Set _internal to the raw promise
    obj.set(PropertyKey::string("_internal"), Value::promise(internal));

    // Try to get Promise.prototype and copy its methods
    if let Some(promise_ctor) = ctx.get_global("Promise").and_then(|v| v.as_object()) {
        if let Some(proto) = promise_ctor
            .get(&PropertyKey::string("prototype"))
            .and_then(|v| v.as_object())
        {
            // Copy then, catch, finally from prototype
            if let Some(then_fn) = proto.get(&PropertyKey::string("then")) {
                obj.set(PropertyKey::string("then"), then_fn);
            }
            if let Some(catch_fn) = proto.get(&PropertyKey::string("catch")) {
                obj.set(PropertyKey::string("catch"), catch_fn);
            }
            if let Some(finally_fn) = proto.get(&PropertyKey::string("finally")) {
                obj.set(PropertyKey::string("finally"), finally_fn);
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
                    result_promise: Some(result_promise.clone()),
                };

                let reject_job = JsPromiseJob {
                    kind: if on_rejected.is_callable() {
                        JsPromiseJobKind::Reject
                    } else {
                        JsPromiseJobKind::PassthroughReject
                    },
                    callback: on_rejected,
                    this_arg: Value::undefined(),
                    result_promise: Some(result_promise.clone()),
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
                    result_promise: Some(result_promise.clone()),
                };

                let reject_job = JsPromiseJob {
                    kind: if on_rejected.is_callable() {
                        JsPromiseJobKind::Reject
                    } else {
                        JsPromiseJobKind::PassthroughReject
                    },
                    callback: on_rejected,
                    this_arg: Value::undefined(),
                    result_promise: Some(result_promise.clone()),
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
                    result_promise: Some(result_promise.clone()),
                };

                let reject_job = JsPromiseJob {
                    kind: reject_kind,
                    callback: reject_callback,
                    this_arg: Value::undefined(),
                    result_promise: Some(result_promise.clone()),
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
/// Returns an interception signal so the interpreter can handle:
/// - `new Promise(executor)` → creates promise and calls executor with resolve/reject
/// - `Promise(executor)` → throws TypeError (requires 'new')
///
/// NOTE: The Promise constructor still uses interception because calling the executor
/// via `ncx.call_function()` from within a native constructor causes re-entry issues
/// when the executor throws an error. This needs to be investigated separately.
pub fn create_promise_constructor(
) -> Box<
    dyn Fn(&Value, &[Value], &mut crate::context::NativeContext<'_>) -> Result<Value, VmError> + Send + Sync,
> {
    Box::new(|_this, _args, _ncx| {
        Err(VmError::interception(InterceptionSignal::PromiseConstructor))
    })
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
                    return Ok(create_js_promise_wrapper(ncx, promise.clone()));
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
                                    result_promise: Some(result_promise.clone()),
                                };
                                ncx.enqueue_js_job(job, Vec::new());
                                return Ok(create_js_promise_wrapper(ncx, result_promise));
                            }
                        }
                    }
                }

                // Resolve with the value directly
                result_promise.resolve_with_js_jobs(value, |job, args| {
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
                result_promise.reject_with_js_jobs(reason, |job, args| {
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
                    result_promise.resolve_with_js_jobs(Value::array(arr), |job, args| {
                        ncx.enqueue_js_job(job, args);
                    });
                    return Ok(create_js_promise_wrapper(ncx, result_promise));
                }

                // Get job queue for async callbacks
                let queue = ncx.js_job_queue().ok_or_else(|| {
                    VmError::type_error("Promise.all requires a job queue")
                })?;
                let enqueue = make_enqueue_fn(queue);

                let count = items.len();
                let remaining = Arc::new(AtomicUsize::new(count));
                let results: Arc<Mutex<Vec<Option<Value>>>> =
                    Arc::new(Mutex::new(vec![None; count]));
                let rejected = Arc::new(AtomicBool::new(false));

                for (index, item) in items.into_iter().enumerate() {
                    let result_p = result_promise.clone();
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
                        p.resolve_with_js_jobs(item, enqueue.clone());
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
                                        arr.set(PropertyKey::Index(i as u32), val.clone());
                                    }
                                }
                            }
                            result_p.resolve_with_js_jobs(Value::array(arr), enqueue_fulfill.clone());
                        }
                    });
                    source_promise.catch(move |error| {
                        if !rejected_check.swap(true, Ordering::AcqRel) {
                            result_p_reject.reject_with_js_jobs(error, enqueue_reject.clone());
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
                let queue = ncx.js_job_queue().ok_or_else(|| {
                    VmError::type_error("Promise.race requires a job queue")
                })?;
                let enqueue = make_enqueue_fn(queue);

                for item in items {
                    let result_p = result_promise.clone();
                    let result_p_reject = result_promise.clone();
                    let settled1 = settled.clone();
                    let settled2 = settled.clone();
                    let enqueue_fulfill = enqueue.clone();
                    let enqueue_reject = enqueue.clone();

                    let source_promise = if let Some(promise) = extract_internal_promise(&item) {
                        promise
                    } else {
                        let p = JsPromise::new();
                        p.resolve_with_js_jobs(item, enqueue.clone());
                        p
                    };

                    source_promise.then(move |value| {
                        if !settled1.swap(true, Ordering::AcqRel) {
                            result_p.resolve_with_js_jobs(value, enqueue_fulfill.clone());
                        }
                    });
                    source_promise.catch(move |error| {
                        if !settled2.swap(true, Ordering::AcqRel) {
                            result_p_reject.reject_with_js_jobs(error, enqueue_reject.clone());
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
                    result_promise.resolve_with_js_jobs(Value::array(arr), |job, args| {
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
                    let result_p = result_promise.clone();
                    let result_p2 = result_promise.clone();
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
                        p.resolve_with_js_jobs(item, enqueue.clone());
                        p
                    };

                    source_promise.then(move |value| {
                        let obj = GcRef::new(JsObject::new(None, mm_t.clone()));
                        obj.set("status".into(), Value::string(JsString::intern("fulfilled")));
                        obj.set("value".into(), value);
                        if let Ok(mut locked) = results.lock() {
                            locked[index] = Some(Value::object(obj));
                        }
                        if remaining.fetch_sub(1, Ordering::AcqRel) == 1 {
                            let arr = GcRef::new(JsObject::array(count, mm_t.clone()));
                            if let Ok(locked) = results.lock() {
                                for (i, v) in locked.iter().enumerate() {
                                    if let Some(val) = v {
                                        arr.set(PropertyKey::Index(i as u32), val.clone());
                                    }
                                }
                            }
                            result_p.resolve_with_js_jobs(Value::array(arr), enqueue_fulfill.clone());
                        }
                    });
                    source_promise.catch(move |error| {
                        let obj = GcRef::new(JsObject::new(None, mm_c.clone()));
                        obj.set("status".into(), Value::string(JsString::intern("rejected")));
                        obj.set("reason".into(), error);
                        if let Ok(mut locked) = results2.lock() {
                            locked[index] = Some(Value::object(obj));
                        }
                        if remaining2.fetch_sub(1, Ordering::AcqRel) == 1 {
                            let arr = GcRef::new(JsObject::array(count, mm_c.clone()));
                            if let Ok(locked) = results2.lock() {
                                for (i, v) in locked.iter().enumerate() {
                                    if let Some(val) = v {
                                        arr.set(PropertyKey::Index(i as u32), val.clone());
                                    }
                                }
                            }
                            result_p2.resolve_with_js_jobs(Value::array(arr), enqueue_reject.clone());
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
                    result_promise.reject_with_js_jobs(err, |job, args| {
                        ncx.enqueue_js_job(job, args);
                    });
                    return Ok(create_js_promise_wrapper(ncx, result_promise));
                }

                // Get job queue for async callbacks
                let queue = ncx.js_job_queue().ok_or_else(|| {
                    VmError::type_error("Promise.any requires a job queue")
                })?;
                let enqueue = make_enqueue_fn(queue);

                let count = items.len();
                let fulfilled = Arc::new(AtomicBool::new(false));
                let remaining = Arc::new(AtomicUsize::new(count));
                let errors: Arc<Mutex<Vec<Option<Value>>>> =
                    Arc::new(Mutex::new(vec![None; count]));

                for (index, item) in items.into_iter().enumerate() {
                    let result_p = result_promise.clone();
                    let result_p2 = result_promise.clone();
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
                        p.resolve_with_js_jobs(item, enqueue.clone());
                        p
                    };

                    source_promise.then(move |value| {
                        if !fulfilled1.swap(true, Ordering::AcqRel) {
                            result_p.resolve_with_js_jobs(value, enqueue_fulfill.clone());
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
                                arr.set(PropertyKey::Index(i as u32), e.clone());
                            }
                            let agg = GcRef::new(JsObject::new(None, mm_err.clone()));
                            agg.set(
                                "message".into(),
                                Value::string(JsString::intern("All promises were rejected")),
                            );
                            agg.set("errors".into(), Value::array(arr));
                            result_p2.reject_with_js_jobs(Value::object(agg), enqueue_reject.clone());
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
                    let result = GcRef::new(JsObject::new(None, mm_wr.clone()));

                    // Get job queue for resolver/rejecter functions
                    let queue = ncx.js_job_queue();

                    // Create wrapped promise for the result
                    let wrapped_promise = create_js_promise_wrapper(ncx, promise.clone());
                    result.set("promise".into(), wrapped_promise);

                    // Create resolve function that captures the promise
                    let promise_for_resolve = promise.clone();
                    let queue_for_resolve = queue.clone();
                    let resolve_fn = Value::native_function_with_proto(
                        move |_this: &Value, args: &[Value], _ncx: &mut crate::context::NativeContext<'_>| {
                            let value = args.first().cloned().unwrap_or(Value::undefined());
                            if let Some(q) = &queue_for_resolve {
                                promise_for_resolve.resolve_with_js_jobs(value, |job, args| {
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
                    result.set("resolve".into(), resolve_fn);

                    // Create reject function that captures the promise
                    let promise_for_reject = promise;
                    let queue_for_reject = queue;
                    let reject_fn = Value::native_function_with_proto(
                        move |_this: &Value, args: &[Value], _ncx: &mut crate::context::NativeContext<'_>| {
                            let reason = args.first().cloned().unwrap_or(Value::undefined());
                            if let Some(q) = &queue_for_reject {
                                promise_for_reject.reject_with_js_jobs(reason, |job, args| {
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
                    result.set("reject".into(), reject_fn);

                    Ok(Value::object(result))
                },
                mm.clone(),
                fn_proto,
            )),
        );
    }
}
