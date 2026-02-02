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
//! Promise.then/catch/finally return InterceptionSignal to delegate callback
//! registration to the interpreter, which has access to VmContext and JS job queue.

use std::sync::Arc;

use crate::error::{InterceptionSignal, VmError};
use crate::gc::GcRef;
use crate::memory::MemoryManager;
use crate::object::{JsObject, PropertyDescriptor, PropertyKey};
use crate::promise::JsPromise;
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

// NOTE: Promise.all/race/allSettled/any are handled via interpreter interception
// to access the JS job queue for correct microtask semantics.

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
            |_this_val, _args, _mm| {
                // Delegate to interpreter via interception signal
                Err(VmError::interception(InterceptionSignal::PromiseThen))
            },
            mm.clone(),
            fn_proto,
        )),
    );

    // Promise.prototype.catch(onRejected) — §27.2.5.1
    proto.define_property(
        PropertyKey::string("catch"),
        PropertyDescriptor::builtin_method(Value::native_function_with_proto(
            |_this_val, _args, _mm| {
                // Delegate to interpreter via interception signal
                Err(VmError::interception(InterceptionSignal::PromiseCatch))
            },
            mm.clone(),
            fn_proto,
        )),
    );

    // Promise.prototype.finally(onFinally) — §27.2.5.3
    proto.define_property(
        PropertyKey::string("finally"),
        PropertyDescriptor::builtin_method(Value::native_function_with_proto(
            |_this_val, _args, _mm| {
                // Delegate to interpreter via interception signal
                Err(VmError::interception(InterceptionSignal::PromiseFinally))
            },
            mm.clone(),
            fn_proto,
        )),
    );
}

// ============================================================================
// Promise constructor statics
// ============================================================================

/// Create Promise constructor function (intercepts in interpreter).
pub fn create_promise_constructor(
) -> Box<
    dyn Fn(&Value, &[Value], Arc<MemoryManager>) -> Result<Value, VmError> + Send + Sync,
> {
    Box::new(|_this, _args, _mm| Err(VmError::interception(InterceptionSignal::PromiseConstructor)))
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
            |_this, args, _mm| {
                let _ = args;
                Err(VmError::interception(InterceptionSignal::PromiseResolve))
            },
            mm.clone(),
            fn_proto,
        )),
    );

    // Promise.reject(reason) — §27.2.4.6
    ctor.define_property(
        PropertyKey::string("reject"),
        PropertyDescriptor::builtin_method(Value::native_function_with_proto(
            |_this, _args, _mm| Err(VmError::interception(InterceptionSignal::PromiseReject)),
            mm.clone(),
            fn_proto,
        )),
    );

    // Promise.all(iterable) — §27.2.4.1
    ctor.define_property(
        PropertyKey::string("all"),
        PropertyDescriptor::builtin_method(Value::native_function_with_proto(
            |_this, _args, _mm| Err(VmError::interception(InterceptionSignal::PromiseAll)),
            mm.clone(),
            fn_proto,
        )),
    );

    // Promise.race(iterable) — §27.2.4.5
    ctor.define_property(
        PropertyKey::string("race"),
        PropertyDescriptor::builtin_method(Value::native_function_with_proto(
            |_this, _args, _mm| Err(VmError::interception(InterceptionSignal::PromiseRace)),
            mm.clone(),
            fn_proto,
        )),
    );

    // Promise.allSettled(iterable) — §27.2.4.2
    ctor.define_property(
        PropertyKey::string("allSettled"),
        PropertyDescriptor::builtin_method(Value::native_function_with_proto(
            |_this, _args, _mm| Err(VmError::interception(InterceptionSignal::PromiseAllSettled)),
            mm.clone(),
            fn_proto,
        )),
    );

    // Promise.any(iterable) — §27.2.4.3
    ctor.define_property(
        PropertyKey::string("any"),
        PropertyDescriptor::builtin_method(Value::native_function_with_proto(
            |_this, _args, _mm| Err(VmError::interception(InterceptionSignal::PromiseAny)),
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
                move |_this, _args, _mm| {
                    let promise = JsPromise::new();
                    let result = GcRef::new(JsObject::new(None, mm_wr.clone()));
                    result.set("promise".into(), Value::promise(promise.clone()));

                    let resolve_fn = Value::native_function_with_proto(
                        |_this: &Value, _args: &[Value], _mm: Arc<MemoryManager>| {
                            Err(VmError::interception(InterceptionSignal::PromiseResolveFunction))
                        },
                        mm_wr.clone(),
                        fn_proto,
                    );
                    if let Some(obj) = resolve_fn.as_object() {
                        obj.set(
                            PropertyKey::string("__promise__"),
                            Value::promise(promise.clone()),
                        );
                    }
                    result.set("resolve".into(), resolve_fn);

                    let reject_fn = Value::native_function_with_proto(
                        |_this: &Value, _args: &[Value], _mm: Arc<MemoryManager>| {
                            Err(VmError::interception(InterceptionSignal::PromiseRejectFunction))
                        },
                        mm_wr.clone(),
                        fn_proto,
                    );
                    if let Some(obj) = reject_fn.as_object() {
                        obj.set(
                            PropertyKey::string("__promise__"),
                            Value::promise(promise),
                        );
                    }
                    result.set("reject".into(), reject_fn);

                    Ok(Value::object(result))
                },
                mm.clone(),
                fn_proto,
            )),
        );
    }
}
