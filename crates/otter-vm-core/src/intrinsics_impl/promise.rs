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

use std::sync::Arc;

use crate::error::VmError;
use crate::gc::GcRef;
use crate::memory::MemoryManager;
use crate::object::{JsObject, PropertyDescriptor, PropertyKey};
use crate::promise::JsPromise;
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

/// Extract array items from a Value (for Promise.all/race/allSettled/any).
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
    // Single value as array of one
    Ok(vec![value.clone()])
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
            |this_val, _args, _mm| {
                let source = get_promise_from_this(this_val)?;
                let result = JsPromise::new();
                let r_fulfill = result.clone();
                let r_reject = result.clone();
                source.then(move |value| {
                    r_fulfill.resolve(value);
                });
                source.catch(move |error| {
                    r_reject.reject(error);
                });
                Ok(Value::promise(result))
            },
            mm.clone(),
            fn_proto,
        )),
    );

    // Promise.prototype.catch(onRejected) — §27.2.5.1
    proto.define_property(
        PropertyKey::string("catch"),
        PropertyDescriptor::builtin_method(Value::native_function_with_proto(
            |this_val, _args, _mm| {
                let source = get_promise_from_this(this_val)?;
                let result = JsPromise::new();
                let r_fulfill = result.clone();
                let r_reject = result.clone();
                source.then(move |value| {
                    r_fulfill.resolve(value);
                });
                source.catch(move |error| {
                    r_reject.reject(error);
                });
                Ok(Value::promise(result))
            },
            mm.clone(),
            fn_proto,
        )),
    );

    // Promise.prototype.finally(onFinally) — §27.2.5.3
    proto.define_property(
        PropertyKey::string("finally"),
        PropertyDescriptor::builtin_method(Value::native_function_with_proto(
            |this_val, _args, _mm| {
                let source = get_promise_from_this(this_val)?;
                let result = JsPromise::new();
                let r_fulfill = result.clone();
                let r_reject = result.clone();
                source.then(move |value| {
                    r_fulfill.resolve(value);
                });
                source.catch(move |error| {
                    r_reject.reject(error);
                });
                Ok(Value::promise(result))
            },
            mm.clone(),
            fn_proto,
        )),
    );
}

// ============================================================================
// Promise constructor statics
// ============================================================================

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
            |_this, args, _mm| {
                let value = args.first().cloned().unwrap_or(Value::undefined());
                if value.is_promise() {
                    return Ok(value);
                }
                Ok(Value::promise(JsPromise::resolved(value)))
            },
            mm.clone(),
            fn_proto,
        )),
    );

    // Promise.reject(reason) — §27.2.4.6
    ctor.define_property(
        PropertyKey::string("reject"),
        PropertyDescriptor::builtin_method(Value::native_function_with_proto(
            |_this, args, _mm| {
                let reason = args.first().cloned().unwrap_or(Value::undefined());
                Ok(Value::promise(JsPromise::rejected(reason)))
            },
            mm.clone(),
            fn_proto,
        )),
    );

    // Promise.all(iterable) — §27.2.4.1
    {
        let mm_all = mm.clone();
        ctor.define_property(
            PropertyKey::string("all"),
            PropertyDescriptor::builtin_method(Value::native_function_with_proto(
                move |_this, args, _mm| {
                    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
                    use std::sync::Mutex;
                    let items = extract_array_items(args.first())?;
                    if items.is_empty() {
                        let arr = GcRef::new(JsObject::array(0, mm_all.clone()));
                        return Ok(Value::promise(JsPromise::resolved(Value::array(arr))));
                    }
                    let result_promise = JsPromise::new();
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
                        let mm_inner = mm_all.clone();
                        if let Some(promise) = item.as_promise() {
                            let result_p_reject = result_p.clone();
                            let rejected_check = rejected.clone();
                            promise.then(move |value| {
                                if rejected.load(Ordering::Acquire) {
                                    return;
                                }
                                if let Ok(mut locked) = results.lock() {
                                    locked[index] = Some(value);
                                }
                                if remaining.fetch_sub(1, Ordering::AcqRel) == 1 {
                                    let arr =
                                        GcRef::new(JsObject::array(count, mm_inner.clone()));
                                    if let Ok(locked) = results.lock() {
                                        for (i, v) in locked.iter().enumerate() {
                                            if let Some(val) = v {
                                                arr.set(
                                                    PropertyKey::Index(i as u32),
                                                    val.clone(),
                                                );
                                            }
                                        }
                                    }
                                    result_p.resolve(Value::array(arr));
                                }
                            });
                            promise.catch(move |error| {
                                if !rejected_check.swap(true, Ordering::AcqRel) {
                                    result_p_reject.reject(error);
                                }
                            });
                        } else {
                            if let Ok(mut locked) = results.lock() {
                                locked[index] = Some(item);
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
                                result_p.resolve(Value::array(arr));
                            }
                        }
                    }
                    Ok(Value::promise(result_promise))
                },
                mm.clone(),
                fn_proto,
            )),
        );
    }

    // Promise.race(iterable) — §27.2.4.5
    ctor.define_property(
        PropertyKey::string("race"),
        PropertyDescriptor::builtin_method(Value::native_function_with_proto(
            |_this, args, _mm| {
                use std::sync::atomic::{AtomicBool, Ordering};
                let items = extract_array_items(args.first())?;
                let result_promise = JsPromise::new();
                let settled = Arc::new(AtomicBool::new(false));
                for item in items {
                    let result_p = result_promise.clone();
                    let result_p_reject = result_promise.clone();
                    let settled1 = settled.clone();
                    let settled2 = settled.clone();
                    if let Some(promise) = item.as_promise() {
                        promise.then(move |value| {
                            if !settled1.swap(true, Ordering::AcqRel) {
                                result_p.resolve(value);
                            }
                        });
                        promise.catch(move |error| {
                            if !settled2.swap(true, Ordering::AcqRel) {
                                result_p_reject.reject(error);
                            }
                        });
                    } else {
                        if !settled.swap(true, Ordering::AcqRel) {
                            result_p.resolve(item);
                        }
                        break;
                    }
                }
                Ok(Value::promise(result_promise))
            },
            mm.clone(),
            fn_proto,
        )),
    );

    // Promise.allSettled(iterable) — §27.2.4.2
    {
        let mm_as = mm.clone();
        ctor.define_property(
            PropertyKey::string("allSettled"),
            PropertyDescriptor::builtin_method(Value::native_function_with_proto(
                move |_this, args, _mm| {
                    use std::sync::atomic::{AtomicUsize, Ordering};
                    use std::sync::Mutex;
                    let items = extract_array_items(args.first())?;
                    if items.is_empty() {
                        let arr = GcRef::new(JsObject::array(0, mm_as.clone()));
                        return Ok(Value::promise(JsPromise::resolved(Value::array(arr))));
                    }
                    let result_promise = JsPromise::new();
                    let count = items.len();
                    let remaining = Arc::new(AtomicUsize::new(count));
                    let results: Arc<Mutex<Vec<Option<Value>>>> =
                        Arc::new(Mutex::new(vec![None; count]));
                    for (index, item) in items.into_iter().enumerate() {
                        let result_p = result_promise.clone();
                        let remaining = remaining.clone();
                        let results = results.clone();
                        let result_p2 = result_promise.clone();
                        let remaining2 = remaining.clone();
                        let results2 = results.clone();
                        let mm_t = mm_as.clone();
                        let mm_c = mm_as.clone();
                        if let Some(promise) = item.as_promise() {
                            promise.then(move |value| {
                                let obj = GcRef::new(JsObject::new(None, mm_t.clone()));
                                obj.set(
                                    "status".into(),
                                    Value::string(JsString::intern("fulfilled")),
                                );
                                obj.set("value".into(), value);
                                if let Ok(mut locked) = results.lock() {
                                    locked[index] = Some(Value::object(obj));
                                }
                                if remaining.fetch_sub(1, Ordering::AcqRel) == 1 {
                                    let arr =
                                        GcRef::new(JsObject::array(count, mm_t.clone()));
                                    if let Ok(locked) = results.lock() {
                                        for (i, v) in locked.iter().enumerate() {
                                            if let Some(val) = v {
                                                arr.set(
                                                    PropertyKey::Index(i as u32),
                                                    val.clone(),
                                                );
                                            }
                                        }
                                    }
                                    result_p.resolve(Value::array(arr));
                                }
                            });
                            promise.catch(move |error| {
                                let obj = GcRef::new(JsObject::new(None, mm_c.clone()));
                                obj.set(
                                    "status".into(),
                                    Value::string(JsString::intern("rejected")),
                                );
                                obj.set("reason".into(), error);
                                if let Ok(mut locked) = results2.lock() {
                                    locked[index] = Some(Value::object(obj));
                                }
                                if remaining2.fetch_sub(1, Ordering::AcqRel) == 1 {
                                    let arr =
                                        GcRef::new(JsObject::array(count, mm_c.clone()));
                                    if let Ok(locked) = results2.lock() {
                                        for (i, v) in locked.iter().enumerate() {
                                            if let Some(val) = v {
                                                arr.set(
                                                    PropertyKey::Index(i as u32),
                                                    val.clone(),
                                                );
                                            }
                                        }
                                    }
                                    result_p2.resolve(Value::array(arr));
                                }
                            });
                        } else {
                            let obj = GcRef::new(JsObject::new(None, mm_t.clone()));
                            obj.set(
                                "status".into(),
                                Value::string(JsString::intern("fulfilled")),
                            );
                            obj.set("value".into(), item);
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
                                result_p.resolve(Value::array(arr));
                            }
                        }
                    }
                    Ok(Value::promise(result_promise))
                },
                mm.clone(),
                fn_proto,
            )),
        );
    }

    // Promise.any(iterable) — §27.2.4.3
    {
        let mm_any = mm.clone();
        ctor.define_property(
            PropertyKey::string("any"),
            PropertyDescriptor::builtin_method(Value::native_function_with_proto(
                move |_this, args, _mm| {
                    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
                    use std::sync::Mutex;
                    let items = extract_array_items(args.first())?;
                    if items.is_empty() {
                        let err =
                            Value::string(JsString::intern("All promises were rejected"));
                        return Ok(Value::promise(JsPromise::rejected(err)));
                    }
                    let result_promise = JsPromise::new();
                    let count = items.len();
                    let fulfilled = Arc::new(AtomicBool::new(false));
                    let remaining = Arc::new(AtomicUsize::new(count));
                    let errors: Arc<Mutex<Vec<Option<Value>>>> =
                        Arc::new(Mutex::new(vec![None; count]));
                    for (index, item) in items.into_iter().enumerate() {
                        let result_p = result_promise.clone();
                        let fulfilled1 = fulfilled.clone();
                        let remaining = remaining.clone();
                        let errors = errors.clone();
                        let result_p2 = result_promise.clone();
                        let fulfilled2 = fulfilled.clone();
                        let mm_err = mm_any.clone();
                        if let Some(promise) = item.as_promise() {
                            promise.then(move |value| {
                                if !fulfilled1.swap(true, Ordering::AcqRel) {
                                    result_p.resolve(value);
                                }
                            });
                            promise.catch(move |error| {
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
                                    let arr = GcRef::new(JsObject::array(
                                        errs.len(),
                                        mm_err.clone(),
                                    ));
                                    for (i, e) in errs.iter().enumerate() {
                                        arr.set(PropertyKey::Index(i as u32), e.clone());
                                    }
                                    let agg =
                                        GcRef::new(JsObject::new(None, mm_err.clone()));
                                    agg.set(
                                        "message".into(),
                                        Value::string(JsString::intern(
                                            "All promises were rejected",
                                        )),
                                    );
                                    agg.set("errors".into(), Value::array(arr));
                                    result_p2.reject(Value::object(agg));
                                }
                            });
                        } else {
                            if !fulfilled.swap(true, Ordering::AcqRel) {
                                result_p.resolve(item);
                            }
                            break;
                        }
                    }
                    Ok(Value::promise(result_promise))
                },
                mm.clone(),
                fn_proto,
            )),
        );
    }

    // Promise.withResolvers() — ES2024
    {
        let mm_wr = mm.clone();
        ctor.define_property(
            PropertyKey::string("withResolvers"),
            PropertyDescriptor::builtin_method(Value::native_function_with_proto(
                move |_this, _args, _mm| {
                    let resolvers = JsPromise::with_resolvers(mm_wr.clone());
                    let result = GcRef::new(JsObject::new(None, mm_wr.clone()));
                    result.set("promise".into(), Value::promise(resolvers.promise));
                    let resolve_fn = resolvers.resolve;
                    result.set(
                        "resolve".into(),
                        Value::native_function(
                            move |_this: &Value,
                                  args: &[Value],
                                  _mm: Arc<MemoryManager>| {
                                let value =
                                    args.first().cloned().unwrap_or(Value::undefined());
                                resolve_fn(value);
                                Ok(Value::undefined())
                            },
                            mm_wr.clone(),
                        ),
                    );
                    let reject_fn = resolvers.reject;
                    result.set(
                        "reject".into(),
                        Value::native_function(
                            move |_this: &Value,
                                  args: &[Value],
                                  _mm: Arc<MemoryManager>| {
                                let reason =
                                    args.first().cloned().unwrap_or(Value::undefined());
                                reject_fn(reason);
                                Ok(Value::undefined())
                            },
                            mm_wr.clone(),
                        ),
                    );
                    Ok(Value::object(result))
                },
                mm.clone(),
                fn_proto,
            )),
        );
    }
}
