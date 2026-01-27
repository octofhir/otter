//! Promise built-in
//!
//! Provides Promise constructor and static methods:
//! - `new Promise((resolve, reject) => {})`
//! - `Promise.resolve(value)`, `Promise.reject(reason)`
//! - `.then()`, `.catch()`, `.finally()`
//! - `Promise.all()`, `Promise.race()`, `Promise.allSettled()`, `Promise.any()`
//! - `Promise.withResolvers()` (ES2024)

use otter_vm_core::object::{JsObject, PropertyKey};
use otter_vm_core::promise::JsPromise;
use otter_vm_core::string::JsString;
use otter_vm_core::value::Value as VmValue;
use otter_vm_core::memory;
use otter_vm_runtime::{Op, op_native_with_mm as op_native};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

/// Get Promise ops for extension registration
pub fn ops() -> Vec<Op> {
    vec![
        op_native("__Promise_create", native_promise_create),
        op_native("__Promise_resolve", native_promise_resolve),
        op_native("__Promise_reject", native_promise_reject),
        op_native("__Promise_then", native_promise_then),
        op_native("__Promise_catch", native_promise_catch),
        op_native("__Promise_finally", native_promise_finally),
        op_native("__Promise_state", native_promise_state),
        op_native("__Promise_all", native_promise_all),
        op_native("__Promise_race", native_promise_race),
        op_native("__Promise_allSettled", native_promise_all_settled),
        op_native("__Promise_any", native_promise_any),
        op_native("__Promise_withResolvers", native_promise_with_resolvers),
    ]
}

// ============================================================================
// Native Operations
// ============================================================================

/// Create a new pending promise
/// Returns the promise value
fn native_promise_create(
    _args: &[VmValue],
    _mm: Arc<memory::MemoryManager>,
) -> Result<VmValue, String> {
    let promise = JsPromise::new();
    Ok(VmValue::promise(promise))
}

/// Resolve a promise with a value
/// Args: [promise, value] - resolve existing promise
/// Args: [value] - create new resolved promise (Promise.resolve)
fn native_promise_resolve(args: &[VmValue], _mm: Arc<memory::MemoryManager>) -> Result<VmValue, String> {
    match args.len() {
        0 => {
            // Promise.resolve() with no args resolves to undefined
            Ok(VmValue::promise(JsPromise::resolved(VmValue::undefined())))
        }
        1 => {
            let value = args[0].clone();
            // If value is already a promise, return it as-is (per spec)
            if value.is_promise() {
                return Ok(value);
            }
            // Create new resolved promise
            Ok(VmValue::promise(JsPromise::resolved(value)))
        }
        _ => {
            // [promise, value] - resolve an existing promise
            let promise_val = &args[0];
            let value = args[1].clone();

            if let Some(promise) = promise_val.as_promise() {
                // Check if value is a thenable (promise)
                if let Some(inner_promise) = value.as_promise() {
                    // Chain: when inner resolves, resolve outer
                    let outer = promise.clone();
                    inner_promise.then(move |v| {
                        outer.resolve(v);
                    });
                    let outer = promise.clone();
                    inner_promise.catch(move |e| {
                        outer.reject(e);
                    });
                } else {
                    promise.resolve(value);
                }
                Ok(VmValue::undefined())
            } else {
                Err("First argument must be a promise".to_string())
            }
        }
    }
}

/// Reject a promise with a reason
/// Args: [promise, reason] - reject existing promise
/// Args: [reason] - create new rejected promise (Promise.reject)
fn native_promise_reject(args: &[VmValue], _mm: Arc<memory::MemoryManager>) -> Result<VmValue, String> {
    match args.len() {
        0 => {
            // Promise.reject() with no args rejects with undefined
            Ok(VmValue::promise(JsPromise::rejected(VmValue::undefined())))
        }
        1 => {
            let reason = args[0].clone();
            // Create new rejected promise
            Ok(VmValue::promise(JsPromise::rejected(reason)))
        }
        _ => {
            // [promise, reason] - reject an existing promise
            let promise_val = &args[0];
            let reason = args[1].clone();

            if let Some(promise) = promise_val.as_promise() {
                promise.reject(reason);
                Ok(VmValue::undefined())
            } else {
                Err("First argument must be a promise".to_string())
            }
        }
    }
}

/// Register a then callback
/// Args: [promise, onFulfilled, onRejected?]
/// Returns a new promise that resolves/rejects based on callbacks
fn native_promise_then(args: &[VmValue], _mm: Arc<memory::MemoryManager>) -> Result<VmValue, String> {
    let promise_val = args
        .first()
        .ok_or("Promise.then requires a promise argument")?;
    let _on_fulfilled = args.get(1).cloned();
    let _on_rejected = args.get(2).cloned();

    let source_promise = promise_val
        .as_promise()
        .ok_or("First argument must be a promise")?;

    // Create a new promise for chaining
    let result_promise = JsPromise::new();
    let result_for_fulfill = result_promise.clone();
    let result_for_reject = result_promise.clone();

    // Pass through value/error (JS wrapper handles callback invocation)
    source_promise.then(move |value| {
        result_for_fulfill.resolve(value);
    });

    source_promise.catch(move |error| {
        result_for_reject.reject(error);
    });

    Ok(VmValue::promise(result_promise))
}

/// Register a catch callback
/// Args: [promise, onRejected]
fn native_promise_catch(args: &[VmValue], _mm: Arc<memory::MemoryManager>) -> Result<VmValue, String> {
    let promise_val = args
        .first()
        .ok_or("Promise.catch requires a promise argument")?;
    let _on_rejected = args.get(1).cloned();

    let source_promise = promise_val
        .as_promise()
        .ok_or("First argument must be a promise")?;

    let result_promise = JsPromise::new();
    let result_for_fulfill = result_promise.clone();
    let result_for_reject = result_promise.clone();

    // Pass through fulfillment
    source_promise.then(move |value| {
        result_for_fulfill.resolve(value);
    });

    // Handle rejection (JS wrapper handles actual callback)
    source_promise.catch(move |error| {
        result_for_reject.reject(error);
    });

    Ok(VmValue::promise(result_promise))
}

/// Register a finally callback
/// Args: [promise, onFinally]
fn native_promise_finally(args: &[VmValue], _mm: Arc<memory::MemoryManager>) -> Result<VmValue, String> {
    let promise_val = args
        .first()
        .ok_or("Promise.finally requires a promise argument")?;
    let _on_finally = args.get(1).cloned();

    let source_promise = promise_val
        .as_promise()
        .ok_or("First argument must be a promise")?;

    let result_promise = JsPromise::new();
    let result_for_fulfill = result_promise.clone();
    let result_for_reject = result_promise.clone();

    // finally passes through both value and error after running callback
    source_promise.then(move |value| {
        // JS wrapper will invoke onFinally then resolve with original value
        result_for_fulfill.resolve(value);
    });

    source_promise.catch(move |error| {
        // JS wrapper will invoke onFinally then reject with original error
        result_for_reject.reject(error);
    });

    Ok(VmValue::promise(result_promise))
}

/// Get promise state
/// Args: [promise]
/// Returns: { state: "pending"|"fulfilled"|"rejected", value?: any, reason?: any }
fn native_promise_state(args: &[VmValue], mm: Arc<memory::MemoryManager>) -> Result<VmValue, String> {
    let promise_val = args.first().ok_or("Missing promise argument")?;
    let promise = promise_val
        .as_promise()
        .ok_or("Argument must be a promise")?;

    let result = Arc::new(JsObject::new(None, Arc::clone(&mm)));

    match promise.state() {
        otter_vm_core::promise::PromiseState::Pending => {
            result.set("state".into(), VmValue::string(JsString::intern("pending")));
        }
        otter_vm_core::promise::PromiseState::Fulfilled(v) => {
            result.set(
                "state".into(),
                VmValue::string(JsString::intern("fulfilled")),
            );
            result.set("value".into(), v);
        }
        otter_vm_core::promise::PromiseState::Rejected(e) => {
            result.set(
                "state".into(),
                VmValue::string(JsString::intern("rejected")),
            );
            result.set("reason".into(), e);
        }
    }

    Ok(VmValue::object(result))
}

/// Promise.all - wait for all promises to fulfill
/// Args: [array of promises/values]
fn native_promise_all(args: &[VmValue], mm: Arc<memory::MemoryManager>) -> Result<VmValue, String> {
    let iterable = args.first().ok_or("Promise.all requires an iterable")?;

    // Get array of values (can be promises or regular values)
    let items = get_array_items(iterable)?;

    if items.is_empty() {
        // Empty array resolves immediately with empty array
        let result = Arc::new(JsObject::array(0, Arc::clone(&mm)));
        return Ok(VmValue::promise(JsPromise::resolved(VmValue::array(
            result,
        ))));
    }

    let result_promise = JsPromise::new();
    let count = items.len();
    let remaining = Arc::new(AtomicUsize::new(count));
    let results: Arc<Mutex<Vec<Option<VmValue>>>> = Arc::new(Mutex::new(vec![None; count]));
    let rejected = Arc::new(AtomicBool::new(false));

    for (index, item) in items.into_iter().enumerate() {
        let result_p = result_promise.clone();
        let remaining = remaining.clone();
        let results = results.clone();
        let rejected = rejected.clone();
        let mm_for_then = Arc::clone(&mm);

        if let Some(promise) = item.as_promise() {
            // It's a promise - wait for it
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
                    // All done - create result array
                    let arr = Arc::new(JsObject::array(count, Arc::clone(&mm_for_then)));
                    if let Ok(locked) = results.lock() {
                        for (i, v) in locked.iter().enumerate() {
                            if let Some(val) = v {
                                arr.set(PropertyKey::Index(i as u32), val.clone());
                            }
                        }
                    }
                    result_p.resolve(VmValue::array(arr));
                }
            });

            promise.catch(move |error| {
                if !rejected_check.swap(true, Ordering::AcqRel) {
                    result_p_reject.reject(error);
                }
            });
        } else {
            // Not a promise - treat as immediately resolved
            if let Ok(mut locked) = results.lock() {
                locked[index] = Some(item);
            }
            if remaining.fetch_sub(1, Ordering::AcqRel) == 1 {
                let arr = Arc::new(JsObject::array(count, Arc::clone(&mm)));
                if let Ok(locked) = results.lock() {
                    for (i, v) in locked.iter().enumerate() {
                        if let Some(val) = v {
                            arr.set(PropertyKey::Index(i as u32), val.clone());
                        }
                    }
                }
                result_p.resolve(VmValue::array(arr));
            }
        }
    }

    Ok(VmValue::promise(result_promise))
}

/// Promise.race - first promise to settle wins
/// Args: [array of promises/values]
fn native_promise_race(args: &[VmValue], _mm: Arc<memory::MemoryManager>) -> Result<VmValue, String> {
    let iterable = args.first().ok_or("Promise.race requires an iterable")?;
    let items = get_array_items(iterable)?;

    let result_promise = JsPromise::new();
    let settled = Arc::new(AtomicBool::new(false));

    for item in items {
        let result_p = result_promise.clone();
        let result_p_reject = result_promise.clone();
        let settled_clone = settled.clone();
        let settled_clone2 = settled.clone();

        if let Some(promise) = item.as_promise() {
            promise.then(move |value| {
                if !settled_clone.swap(true, Ordering::AcqRel) {
                    result_p.resolve(value);
                }
            });

            promise.catch(move |error| {
                if !settled_clone2.swap(true, Ordering::AcqRel) {
                    result_p_reject.reject(error);
                }
            });
        } else {
            // Non-promise settles immediately
            if !settled.swap(true, Ordering::AcqRel) {
                result_p.resolve(item);
            }
            break;
        }
    }

    Ok(VmValue::promise(result_promise))
}

/// Promise.allSettled - wait for all to settle (fulfill or reject)
/// Args: [array of promises/values]
fn native_promise_all_settled(args: &[VmValue], mm: Arc<memory::MemoryManager>) -> Result<VmValue, String> {
    let iterable = args
        .first()
        .ok_or("Promise.allSettled requires an iterable")?;
    let items = get_array_items(iterable)?;

    if items.is_empty() {
        let result = Arc::new(JsObject::array(0, Arc::clone(&mm)));
        return Ok(VmValue::promise(JsPromise::resolved(VmValue::array(
            result,
        ))));
    }

    let result_promise = JsPromise::new();
    let count = items.len();
    let remaining = Arc::new(AtomicUsize::new(count));
    let results: Arc<Mutex<Vec<Option<VmValue>>>> = Arc::new(Mutex::new(vec![None; count]));

    for (index, item) in items.into_iter().enumerate() {
        let result_p = result_promise.clone();
        let remaining = remaining.clone();
        let results = results.clone();
        let remaining2 = remaining.clone();
        let results2 = results.clone();
        let result_p2 = result_promise.clone();
        let mm_for_then = Arc::clone(&mm);
        let mm_for_catch = Arc::clone(&mm);

        if let Some(promise) = item.as_promise() {
            promise.then(move |value| {
                let obj = Arc::new(JsObject::new(None, Arc::clone(&mm_for_then)));
                obj.set(
                    "status".into(),
                    VmValue::string(JsString::intern("fulfilled")),
                );
                obj.set("value".into(), value);
                if let Ok(mut locked) = results.lock() {
                    locked[index] = Some(VmValue::object(obj));
                }

                if remaining.fetch_sub(1, Ordering::AcqRel) == 1 {
                    let arr = Arc::new(JsObject::array(count, Arc::clone(&mm_for_then)));
                    if let Ok(locked) = results.lock() {
                        for (i, v) in locked.iter().enumerate() {
                            if let Some(val) = v {
                                arr.set(PropertyKey::Index(i as u32), val.clone());
                            }
                        }
                    }
                    result_p.resolve(VmValue::array(arr));
                }
            });

            promise.catch(move |error| {
                let obj = Arc::new(JsObject::new(None, Arc::clone(&mm_for_catch)));
                obj.set(
                    "status".into(),
                    VmValue::string(JsString::intern("rejected")),
                );
                obj.set("reason".into(), error);
                if let Ok(mut locked) = results2.lock() {
                    locked[index] = Some(VmValue::object(obj));
                }

                if remaining2.fetch_sub(1, Ordering::AcqRel) == 1 {
                    let arr = Arc::new(JsObject::array(count, Arc::clone(&mm_for_catch)));
                    if let Ok(locked) = results2.lock() {
                        for (i, v) in locked.iter().enumerate() {
                            if let Some(val) = v {
                                arr.set(PropertyKey::Index(i as u32), val.clone());
                            }
                        }
                    }
                    result_p2.resolve(VmValue::array(arr));
                }
            });
        } else {
            // Non-promise is treated as fulfilled
            let obj = Arc::new(JsObject::new(None, Arc::clone(&mm)));
            obj.set(
                "status".into(),
                VmValue::string(JsString::intern("fulfilled")),
            );
            obj.set("value".into(), item);
            if let Ok(mut locked) = results.lock() {
                locked[index] = Some(VmValue::object(obj));
            }

            if remaining.fetch_sub(1, Ordering::AcqRel) == 1 {
                let arr = Arc::new(JsObject::array(count, Arc::clone(&mm)));
                if let Ok(locked) = results.lock() {
                    for (i, v) in locked.iter().enumerate() {
                        if let Some(val) = v {
                            arr.set(PropertyKey::Index(i as u32), val.clone());
                        }
                    }
                }
                result_p.resolve(VmValue::array(arr));
            }
        }
    }

    Ok(VmValue::promise(result_promise))
}

/// Promise.any - first fulfilled wins, all rejected = AggregateError
/// Args: [array of promises/values]
fn native_promise_any(args: &[VmValue], mm: Arc<memory::MemoryManager>) -> Result<VmValue, String> {
    let iterable = args.first().ok_or("Promise.any requires an iterable")?;
    let items = get_array_items(iterable)?;

    if items.is_empty() {
        // Empty iterable rejects with AggregateError
        let error = create_aggregate_error(vec![], "All promises were rejected", Arc::clone(&mm));
        return Ok(VmValue::promise(JsPromise::rejected(error)));
    }

    let result_promise = JsPromise::new();
    let count = items.len();
    let fulfilled = Arc::new(AtomicBool::new(false));
    let remaining = Arc::new(AtomicUsize::new(count));
    let errors: Arc<Mutex<Vec<Option<VmValue>>>> = Arc::new(Mutex::new(vec![None; count]));

    for (index, item) in items.into_iter().enumerate() {
        let result_p = result_promise.clone();
        let fulfilled_clone = fulfilled.clone();
        let remaining = remaining.clone();
        let errors = errors.clone();
        let result_p2 = result_promise.clone();
        let fulfilled_clone2 = fulfilled.clone();
        let mm_for_error = Arc::clone(&mm);

        if let Some(promise) = item.as_promise() {
            promise.then(move |value| {
                if !fulfilled_clone.swap(true, Ordering::AcqRel) {
                    result_p.resolve(value);
                }
            });

            promise.catch(move |error| {
                if fulfilled_clone2.load(Ordering::Acquire) {
                    return;
                }
                if let Ok(mut locked) = errors.lock() {
                    locked[index] = Some(error);
                }
                if remaining.fetch_sub(1, Ordering::AcqRel) == 1 {
                    // All rejected
                    let errs: Vec<VmValue> = if let Ok(locked) = errors.lock() {
                        locked.iter().filter_map(|e| e.clone()).collect()
                    } else {
                        vec![]
                    };
                    let agg_error = create_aggregate_error(errs, "All promises were rejected", Arc::clone(&mm_for_error));
                    result_p2.reject(agg_error);
                }
            });
        } else {
            // Non-promise is immediately fulfilled
            if !fulfilled.swap(true, Ordering::AcqRel) {
                result_p.resolve(item);
            }
            break;
        }
    }

    Ok(VmValue::promise(result_promise))
}

/// Promise.withResolvers - ES2024 feature
/// Returns: { promise, resolve, reject }
fn native_promise_with_resolvers(
    _args: &[VmValue],
    mm: Arc<memory::MemoryManager>,
) -> Result<VmValue, String> {
    let resolvers = JsPromise::with_resolvers(Arc::clone(&mm));

    let result = Arc::new(JsObject::new(None, Arc::clone(&mm)));
    result.set("promise".into(), VmValue::promise(resolvers.promise));

    // Create native functions for resolve/reject
    let resolve_fn = resolvers.resolve;
    result.set(
        "resolve".into(),
        VmValue::native_function(
            move |args: &[VmValue], _mm: Arc<memory::MemoryManager>| {
                let value = args.first().cloned().unwrap_or(VmValue::undefined());
                resolve_fn(value);
                Ok(VmValue::undefined())
            },
            Arc::clone(&mm),
        ),
    );

    let reject_fn = resolvers.reject;
    result.set(
        "reject".into(),
        VmValue::native_function(
            move |args: &[VmValue], _mm: Arc<memory::MemoryManager>| {
                let reason = args.first().cloned().unwrap_or(VmValue::undefined());
                reject_fn(reason);
                Ok(VmValue::undefined())
            },
            Arc::clone(&mm),
        ),
    );

    Ok(VmValue::object(result))
}

// ============================================================================
// Helper functions
// ============================================================================

/// Extract array items from a value
fn get_array_items(value: &VmValue) -> Result<Vec<VmValue>, String> {
    // Check if it's an array (object with is_array flag) or array-like
    if let Some(obj) = value.as_object() {
        if obj.is_array() {
            let len = obj.array_length();
            let mut items = Vec::with_capacity(len);
            for i in 0..len {
                items.push(
                    obj.get(&PropertyKey::Index(i as u32))
                        .unwrap_or(VmValue::undefined()),
                );
            }
            return Ok(items);
        }

        // Try to iterate if it has length property
        if let Some(len_val) = obj.get(&"length".into())
            && let Some(len) = len_val.as_number()
        {
            let len = len as usize;
            let mut items = Vec::with_capacity(len);
            for i in 0..len {
                items.push(
                    obj.get(&PropertyKey::Index(i as u32))
                        .unwrap_or(VmValue::undefined()),
                );
            }
            return Ok(items);
        }
    }

    Err("Argument is not iterable".to_string())
}

/// Create an AggregateError-like object
fn create_aggregate_error(
    errors: Vec<VmValue>,
    message: &str,
    mm: Arc<memory::MemoryManager>,
) -> VmValue {
    let obj = Arc::new(JsObject::new(None, Arc::clone(&mm)));
    obj.set(
        "name".into(),
        VmValue::string(JsString::intern("AggregateError")),
    );
    obj.set("message".into(), VmValue::string(JsString::intern(message)));

    let errors_arr = Arc::new(JsObject::array(errors.len(), Arc::clone(&mm)));
    for (i, e) in errors.into_iter().enumerate() {
        errors_arr.set(PropertyKey::Index(i as u32), e);
    }
    obj.set("errors".into(), VmValue::array(errors_arr));

    VmValue::object(obj)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_promise_create() {
        let mm = Arc::new(memory::MemoryManager::test());
        let result = native_promise_create(&[], mm).unwrap();
        assert!(result.is_promise());
        assert!(result.as_promise().unwrap().is_pending());
    }

    #[test]
    fn test_promise_resolve_static() {
        let mm = Arc::new(memory::MemoryManager::test());
        let result = native_promise_resolve(&[VmValue::number(42.0)], mm).unwrap();
        assert!(result.is_promise());
        let promise = result.as_promise().unwrap();
        assert!(promise.is_fulfilled());
        match promise.state() {
            otter_vm_core::promise::PromiseState::Fulfilled(v) => {
                assert_eq!(v.as_number(), Some(42.0));
            }
            other => panic!("expected fulfilled promise, got {other:?}"),
        }
    }

    #[test]
    fn test_promise_reject_static() {
        let mm = Arc::new(memory::MemoryManager::test());
        let result = native_promise_reject(&[VmValue::string(JsString::intern("error"))], mm).unwrap();
        assert!(result.is_promise());
        let promise = result.as_promise().unwrap();
        assert!(promise.is_rejected());
    }

    #[test]
    fn test_promise_resolve_existing() {
        let mm = Arc::new(memory::MemoryManager::test());
        let promise = JsPromise::new();
        let promise_val = VmValue::promise(promise.clone());

        native_promise_resolve(&[promise_val, VmValue::number(100.0)], mm).unwrap();

        assert!(promise.is_fulfilled());
        match promise.state() {
            otter_vm_core::promise::PromiseState::Fulfilled(v) => {
                assert_eq!(v.as_number(), Some(100.0));
            }
            other => panic!("expected fulfilled promise, got {other:?}"),
        }
    }

    #[test]
    fn test_promise_with_resolvers() {
        let mm = Arc::new(memory::MemoryManager::test());
        let result = native_promise_with_resolvers(&[], mm).unwrap();
        assert!(result.is_object());

        let obj = result.as_object().unwrap();
        assert!(obj.get(&"promise".into()).unwrap().is_promise());
        assert!(obj.get(&"resolve".into()).unwrap().is_native_function());
        assert!(obj.get(&"reject".into()).unwrap().is_native_function());
    }

    #[test]
    fn test_promise_state() {
        let mm = Arc::new(memory::MemoryManager::test());
        let promise = JsPromise::new();
        let result = native_promise_state(&[VmValue::promise(promise.clone())], Arc::clone(&mm)).unwrap();

        let obj = result.as_object().unwrap();
        let state = obj.get(&"state".into()).unwrap();
        assert_eq!(state.as_string().map(|s| s.as_str()), Some("pending"));

        promise.resolve(VmValue::number(42.0));
        let result = native_promise_state(&[VmValue::promise(promise)], mm).unwrap();
        let obj = result.as_object().unwrap();
        let state = obj.get(&"state".into()).unwrap();
        assert_eq!(state.as_string().map(|s| s.as_str()), Some("fulfilled"));
    }
}
