//! `Promise` constructor + statics + prototype dispatch.
//!
//! Slice 34. Connects three layers:
//!
//! - The bytecode-side opcodes ([`otter_bytecode::Op::PromiseNew`],
//!   [`otter_bytecode::Op::PromiseCall`]) and the universal
//!   [`otter_bytecode::Op::CallMethodValue`] when its receiver is
//!   a [`crate::Value::Promise`].
//! - The value-level state machine implemented by
//!   [`crate::JsPromiseHandle`] / [`crate::PurePromise`].
//! - The microtask queue introduced in task 33: settlement
//!   reactions land on the queue as plain [`crate::Microtask`]s.
//!
//! # Contents
//! - [`construct`] — `new Promise(executor)` body.
//! - [`statics_call`] — dispatcher for `Promise.<name>(args...)`
//!   (`resolve`, `reject`, `all`, `race`).
//! - [`prototype_call`] — dispatcher for
//!   `promise.<name>(args...)` (`then`, `catch`, `finally`).
//! - [`make_capability`] — `NewPromiseCapability` (§27.2.1.5).
//!
//! # Invariants
//! - Native `resolve` / `reject` closures capture the promise via
//!   `JsPromiseHandle::clone()` (Rc-shared body). They are
//!   idempotent — once a promise settles, subsequent resolve /
//!   reject calls are no-ops per spec §27.2.1.4 / §27.2.1.7.
//! - Settlement enqueues all pending reactions onto
//!   `Interpreter::microtasks` so the surrounding drain picks
//!   them up on the next generation.
//!
//! # See also
//! - [`docs/new-engine/tasks/34-promise-value.md`](
//!     ../../../docs/new-engine/tasks/34-promise-value.md
//!   )

use smallvec::smallvec;

use crate::native_function::{NativeError, native_value};
use crate::promise::{
    JsPromise, JsPromiseHandle, PromiseCapability, PromiseSettleJobs, PromiseState,
    PromiseThenOutcome,
};
use crate::{Interpreter, Microtask, Value};

/// Foundation `Promise` constructor body. Builds a pending
/// promise, hands native resolve/reject to the executor, and
/// returns the promise value.
///
/// The executor itself is invoked by the caller (the VM
/// dispatcher) — this function only produces the value plumbing.
#[must_use]
pub fn construct() -> (JsPromiseHandle, Value, Value) {
    let promise = JsPromiseHandle::pending();
    let resolve = make_resolve_native(promise.clone());
    let reject = make_reject_native(promise.clone());
    (promise, resolve, reject)
}

/// `NewPromiseCapability` — produce the `{promise, resolve,
/// reject}` triple over a fresh pending promise.
#[must_use]
pub fn make_capability() -> PromiseCapability {
    let (handle, resolve, reject) = construct();
    PromiseCapability {
        promise: Value::Promise(handle),
        resolve,
        reject,
    }
}

/// Dispatch a `Promise.<name>(args...)` static call. Mirrors
/// [`crate::math::call`] / [`crate::json::call`].
pub fn statics_call(
    interp: &mut Interpreter,
    name: &str,
    args: &[Value],
) -> Result<Value, NativeError> {
    match name {
        "resolve" => Ok(Value::Promise(static_resolve(args))),
        "reject" => Ok(Value::Promise(static_reject(args))),
        "all" => Ok(static_all(interp, args)),
        "race" => Ok(static_race(interp, args)),
        other => Err(NativeError::TypeError {
            name: "Promise",
            reason: format!("static `{other}` is not defined"),
        }),
    }
}

/// Dispatch a `promise.<name>(args...)` instance-method call.
/// Branches on `then` / `catch` / `finally`; everything else
/// surfaces as `UnknownIntrinsic` upstream.
pub fn prototype_call(
    interp: &mut Interpreter,
    promise: &JsPromiseHandle,
    name: &str,
    args: &[Value],
) -> Result<Value, NativeError> {
    match name {
        "then" => Ok(method_then(interp, promise, args)),
        "catch" => Ok(method_catch(interp, promise, args)),
        "finally" => Ok(method_finally(interp, promise, args)),
        other => Err(NativeError::TypeError {
            name: "Promise.prototype",
            reason: format!("method `{other}` is not defined"),
        }),
    }
}

// -- statics --------------------------------------------------------

fn static_resolve(args: &[Value]) -> JsPromiseHandle {
    let value = args.first().cloned().unwrap_or(Value::Undefined);
    // Spec: if `value` is already a Promise we'd return it
    // unchanged. Foundation matches that for our concrete handle.
    if let Value::Promise(p) = &value {
        return p.clone();
    }
    JsPromiseHandle::fulfilled(value)
}

fn static_reject(args: &[Value]) -> JsPromiseHandle {
    let reason = args.first().cloned().unwrap_or(Value::Undefined);
    JsPromiseHandle::rejected(reason)
}

fn static_all(interp: &mut Interpreter, args: &[Value]) -> Value {
    let entries = match args.first() {
        Some(Value::Array(arr)) => arr.borrow_body().iter().cloned().collect::<Vec<_>>(),
        _ => {
            // Foundation: only array iterables. Generic iterables
            // arrive once `Symbol.iterator` is in.
            return Value::Promise(JsPromiseHandle::rejected(Value::Undefined));
        }
    };
    let result = JsPromiseHandle::pending();
    if entries.is_empty() {
        // Spec: empty iterable resolves immediately with [].
        let jobs = result.fulfill(Value::Array(crate::JsArray::new()));
        for j in jobs.jobs {
            interp.microtasks_mut().enqueue(j);
        }
        return Value::Promise(result);
    }
    // Track per-slot fulfillment via a shared `Rc<RefCell<...>>`
    // that the per-element resolver mutates. Foundation `Rc`
    // model — task 57 (GC) revisits.
    let total = entries.len();
    let slots: std::rc::Rc<std::cell::RefCell<Vec<Option<Value>>>> =
        std::rc::Rc::new(std::cell::RefCell::new(vec![None; total]));
    let remaining: std::rc::Rc<std::cell::Cell<usize>> =
        std::rc::Rc::new(std::cell::Cell::new(total));
    for (i, entry) in entries.into_iter().enumerate() {
        let slots = slots.clone();
        let remaining = remaining.clone();
        let result_clone = result.clone();
        let entry_promise = match entry {
            Value::Promise(p) => p,
            other => JsPromiseHandle::fulfilled(other),
        };
        let on_fulfill = native_value("Promise.all element", move |interp, args| {
            let v = args.first().cloned().unwrap_or(Value::Undefined);
            let mut slots_mut = slots.borrow_mut();
            slots_mut[i] = Some(v);
            let count = remaining.get().saturating_sub(1);
            remaining.set(count);
            if count == 0 {
                let collected: Vec<Value> = slots_mut
                    .drain(..)
                    .map(|opt| opt.unwrap_or(Value::Undefined))
                    .collect();
                drop(slots_mut);
                let arr = crate::JsArray::from_elements(collected);
                let jobs = result_clone.fulfill(Value::Array(arr));
                for j in jobs.jobs {
                    interp.microtasks_mut().enqueue(j);
                }
            }
            Ok(Value::Undefined)
        });
        let result_for_reject = result.clone();
        let on_reject = native_value("Promise.all reject", move |interp, args| {
            let reason = args.first().cloned().unwrap_or(Value::Undefined);
            let jobs = result_for_reject.reject(reason);
            for j in jobs.jobs {
                interp.microtasks_mut().enqueue(j);
            }
            Ok(Value::Undefined)
        });
        attach_then(interp, &entry_promise, Some(on_fulfill), Some(on_reject));
    }
    Value::Promise(result)
}

fn static_race(interp: &mut Interpreter, args: &[Value]) -> Value {
    let entries = match args.first() {
        Some(Value::Array(arr)) => arr.borrow_body().iter().cloned().collect::<Vec<_>>(),
        _ => return Value::Promise(JsPromiseHandle::rejected(Value::Undefined)),
    };
    let result = JsPromiseHandle::pending();
    for entry in entries {
        let entry_promise = match entry {
            Value::Promise(p) => p,
            other => JsPromiseHandle::fulfilled(other),
        };
        let result_for_fulfill = result.clone();
        let on_fulfill = native_value("Promise.race fulfill", move |interp, args| {
            let v = args.first().cloned().unwrap_or(Value::Undefined);
            let jobs = result_for_fulfill.fulfill(v);
            for j in jobs.jobs {
                interp.microtasks_mut().enqueue(j);
            }
            Ok(Value::Undefined)
        });
        let result_for_reject = result.clone();
        let on_reject = native_value("Promise.race reject", move |interp, args| {
            let reason = args.first().cloned().unwrap_or(Value::Undefined);
            let jobs = result_for_reject.reject(reason);
            for j in jobs.jobs {
                interp.microtasks_mut().enqueue(j);
            }
            Ok(Value::Undefined)
        });
        attach_then(interp, &entry_promise, Some(on_fulfill), Some(on_reject));
    }
    Value::Promise(result)
}

// -- prototype methods ---------------------------------------------

fn method_then(interp: &mut Interpreter, promise: &JsPromiseHandle, args: &[Value]) -> Value {
    let on_fulfilled = match args.first() {
        Some(v) if crate::is_callable_value(v) => Some(v.clone()),
        _ => None,
    };
    let on_rejected = match args.get(1) {
        Some(v) if crate::is_callable_value(v) => Some(v.clone()),
        _ => None,
    };
    perform_then_with_handlers(interp, promise, on_fulfilled, on_rejected)
}

fn method_catch(interp: &mut Interpreter, promise: &JsPromiseHandle, args: &[Value]) -> Value {
    let on_rejected = match args.first() {
        Some(v) if crate::is_callable_value(v) => Some(v.clone()),
        _ => None,
    };
    perform_then_with_handlers(interp, promise, None, on_rejected)
}

fn method_finally(interp: &mut Interpreter, promise: &JsPromiseHandle, args: &[Value]) -> Value {
    let on_finally = match args.first() {
        Some(v) if crate::is_callable_value(v) => v.clone(),
        _ => return perform_then_with_handlers(interp, promise, None, None),
    };
    // Spec §27.2.5.3: build wrapper handlers that invoke
    // `on_finally` and then propagate the original outcome. We
    // synthesise these as native closures that schedule
    // `on_finally` as a microtask and forward the value/reason.
    let then_handler = {
        let on_finally = on_finally.clone();
        native_value("Promise.prototype.finally then", move |interp, args| {
            let value = args.first().cloned().unwrap_or(Value::Undefined);
            interp.microtasks_mut().enqueue(Microtask {
                callee: on_finally.clone(),
                this_value: Value::Undefined,
                args: smallvec![],
                result_capability: None,
                kind: crate::microtask::MicrotaskKind::Call,
            });
            Ok(value)
        })
    };
    let catch_handler = {
        native_value("Promise.prototype.finally catch", move |interp, args| {
            let reason = args.first().cloned().unwrap_or(Value::Undefined);
            interp.microtasks_mut().enqueue(Microtask {
                callee: on_finally.clone(),
                this_value: Value::Undefined,
                args: smallvec![],
                result_capability: None,
                kind: crate::microtask::MicrotaskKind::Call,
            });
            // Propagate the rejection through the next promise:
            // returning the reason here would convert it to a
            // fulfillment, so we wrap a re-throw via a native
            // closure that synthesises a rejection through its
            // capability. Simpler: reject via the returned
            // outcome. Foundation: make this a fulfilment of an
            // already-rejected promise.
            Ok(Value::Promise(JsPromiseHandle::rejected(reason)))
        })
    };
    perform_then_with_handlers(interp, promise, Some(then_handler), Some(catch_handler))
}

// -- helpers -------------------------------------------------------

fn perform_then_with_handlers(
    interp: &mut Interpreter,
    promise: &JsPromiseHandle,
    on_fulfilled: Option<Value>,
    on_rejected: Option<Value>,
) -> Value {
    let capability = make_capability();
    let outcome: PromiseThenOutcome =
        promise.perform_then(on_fulfilled, on_rejected, capability.clone());
    if let Some(job) = outcome.immediate_job {
        interp.microtasks_mut().enqueue(job);
    }
    capability.promise
}

fn attach_then(
    interp: &mut Interpreter,
    promise: &JsPromiseHandle,
    on_fulfilled: Option<Value>,
    on_rejected: Option<Value>,
) {
    // Reusable "result-of-then" path that the combinators don't
    // expose to user code. We still need a capability so the
    // reaction has somewhere to settle, even if we never read it.
    let capability = make_capability();
    let outcome = promise.perform_then(on_fulfilled, on_rejected, capability);
    if let Some(job) = outcome.immediate_job {
        interp.microtasks_mut().enqueue(job);
    }
}

fn make_resolve_native(promise: JsPromiseHandle) -> Value {
    native_value("Promise resolve", move |interp, args| {
        if matches!(promise.state(), PromiseState::Pending) {
            let value = args.first().cloned().unwrap_or(Value::Undefined);
            // If the resolved value is itself a promise, adopt its
            // state. Spec §27.2.1.4 step 8: schedule a job that
            // forwards. Foundation: fulfill directly with the
            // inner value once that promise settles.
            if let Value::Promise(inner) = &value {
                let resolver = promise.clone();
                let on_fulfill =
                    native_value("Promise resolve adopt fulfill", move |interp, args| {
                        let v = args.first().cloned().unwrap_or(Value::Undefined);
                        drain_jobs(interp, resolver.fulfill(v));
                        Ok(Value::Undefined)
                    });
                let resolver_for_reject = promise.clone();
                let on_reject =
                    native_value("Promise resolve adopt reject", move |interp, args| {
                        let reason = args.first().cloned().unwrap_or(Value::Undefined);
                        drain_jobs(interp, resolver_for_reject.reject(reason));
                        Ok(Value::Undefined)
                    });
                attach_then(interp, inner, Some(on_fulfill), Some(on_reject));
                return Ok(Value::Undefined);
            }
            drain_jobs(interp, promise.fulfill(value));
        }
        Ok(Value::Undefined)
    })
}

fn make_reject_native(promise: JsPromiseHandle) -> Value {
    native_value("Promise reject", move |interp, args| {
        if matches!(promise.state(), PromiseState::Pending) {
            let reason = args.first().cloned().unwrap_or(Value::Undefined);
            drain_jobs(interp, promise.reject(reason));
        }
        Ok(Value::Undefined)
    })
}

fn drain_jobs(interp: &mut Interpreter, jobs: PromiseSettleJobs) {
    for j in jobs.jobs {
        interp.microtasks_mut().enqueue(j);
    }
}
