//! Native `node:timers` and `node:timers/promises` modules.
//!
//! Uses runtime-provided global timer APIs (`setTimeout`, `setInterval`,
//! `setImmediate`, `queueMicrotask`) and exposes Node-style module namespaces.

use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::Mutex;

use otter_vm_core::context::NativeContext;
use otter_vm_core::error::VmError;
use otter_vm_core::gc::GcRef;
use otter_vm_core::memory::MemoryManager;
use otter_vm_core::object::{JsObject, PropertyKey};
use otter_vm_core::promise::{JsPromise, PromiseWithResolvers};
use otter_vm_core::string::JsString;
use otter_vm_core::value::Value;
use otter_vm_runtime::extension_v2::{OtterExtension, Profile};
use otter_vm_runtime::registration::RegistrationContext;

/// Native extension for `node:timers` and `node:timers/promises`.
pub struct NodeTimersExtension;

impl OtterExtension for NodeTimersExtension {
    fn name(&self) -> &str {
        "node_timers_module"
    }

    fn profiles(&self) -> &[Profile] {
        static PROFILES: [Profile; 2] = [Profile::SafeCore, Profile::Full];
        &PROFILES
    }

    fn deps(&self) -> &[&str] {
        &[]
    }

    fn module_specifiers(&self) -> &[&str] {
        static SPECIFIERS: [&str; 4] = [
            "node:timers",
            "timers",
            "node:timers/promises",
            "timers/promises",
        ];
        &SPECIFIERS
    }

    fn install(&self, _ctx: &mut RegistrationContext) -> Result<(), VmError> {
        Ok(())
    }

    fn load_module(
        &self,
        specifier: &str,
        ctx: &mut RegistrationContext,
    ) -> Option<GcRef<JsObject>> {
        let is_promises = specifier == "node:timers/promises" || specifier == "timers/promises";
        if is_promises {
            Some(build_timers_promises_module(ctx))
        } else {
            Some(build_timers_module(ctx))
        }
    }
}

/// Create a boxed extension instance for registration.
pub fn node_timers_extension() -> Box<dyn OtterExtension> {
    Box::new(NodeTimersExtension)
}

fn build_timers_module(ctx: &mut RegistrationContext) -> GcRef<JsObject> {
    let mut ns = ctx.module_namespace();
    for name in [
        "setTimeout",
        "clearTimeout",
        "setInterval",
        "clearInterval",
        "setImmediate",
        "clearImmediate",
    ] {
        if let Some(value) = ctx.global().get(&PropertyKey::string(name)) {
            ns = ns.property(name, value);
        }
    }
    ns.build()
}

fn build_timers_promises_module(ctx: &mut RegistrationContext) -> GcRef<JsObject> {
    let mut ns = ctx.module_namespace();
    ns = ns.function("setTimeout", Arc::new(timers_promises_set_timeout), 2);
    ns = ns.function("setImmediate", Arc::new(timers_promises_set_immediate), 1);
    ns = ns.function("setInterval", Arc::new(timers_promises_set_interval), 2);

    let scheduler = ctx
        .module_namespace()
        .function("wait", Arc::new(scheduler_wait), 1)
        .function("yield", Arc::new(scheduler_yield), 0)
        .build();
    ns = ns.property("scheduler", Value::object(scheduler));

    ns.build()
}

fn timers_promises_set_timeout(
    _this: &Value,
    args: &[Value],
    ncx: &mut NativeContext,
) -> Result<Value, VmError> {
    let delay_ms = args
        .first()
        .and_then(|v| v.as_number())
        .unwrap_or(0.0)
        .max(0.0);
    let value = args.get(1).cloned().unwrap_or(Value::undefined());
    let options = args.get(2);

    if signal_is_aborted(options, ncx)? {
        let reason = make_abort_error(ncx);
        return rejected_promise(ncx, reason);
    }

    let set_timeout = get_global_callable(ncx, "setTimeout")?;

    let mm = ncx.memory_manager().clone();
    let js_queue = ncx
        .js_job_queue()
        .ok_or_else(|| VmError::type_error("No JS job queue available for Promise operation"))?;
    let js_queue_for_resolvers = Arc::clone(&js_queue);
    let resolvers = JsPromise::with_resolvers(mm.clone(), move |job, job_args| {
        js_queue_for_resolvers.enqueue(job, job_args);
    });

    let resolve = resolvers.resolve.clone();
    let resolved_value = value.clone();
    let fn_proto = ncx
        .ctx
        .function_prototype()
        .ok_or_else(|| VmError::internal("Function.prototype not found"))?;
    let callback = Value::native_function_with_proto(
        move |_this, _cb_args, _cb_ncx| {
            (resolve)(resolved_value.clone());
            Ok(Value::undefined())
        },
        mm,
        fn_proto,
    );

    let _ = ncx.call_function(
        &set_timeout,
        Value::undefined(),
        &[callback, Value::number(delay_ms)],
    )?;

    Ok(wrap_internal_promise(ncx, resolvers.promise))
}

fn timers_promises_set_immediate(
    _this: &Value,
    args: &[Value],
    ncx: &mut NativeContext,
) -> Result<Value, VmError> {
    let value = args.first().cloned().unwrap_or(Value::undefined());
    let options = args.get(1);

    if signal_is_aborted(options, ncx)? {
        let reason = make_abort_error(ncx);
        return rejected_promise(ncx, reason);
    }

    let set_immediate = get_global_callable(ncx, "setImmediate")?;

    let mm = ncx.memory_manager().clone();
    let js_queue = ncx
        .js_job_queue()
        .ok_or_else(|| VmError::type_error("No JS job queue available for Promise operation"))?;
    let js_queue_for_resolvers = Arc::clone(&js_queue);
    let resolvers = JsPromise::with_resolvers(mm.clone(), move |job, job_args| {
        js_queue_for_resolvers.enqueue(job, job_args);
    });

    let resolve = resolvers.resolve.clone();
    let resolved_value = value.clone();
    let fn_proto = ncx
        .ctx
        .function_prototype()
        .ok_or_else(|| VmError::internal("Function.prototype not found"))?;
    let callback = Value::native_function_with_proto(
        move |_this, _cb_args, _cb_ncx| {
            (resolve)(resolved_value.clone());
            Ok(Value::undefined())
        },
        mm,
        fn_proto,
    );

    let _ = ncx.call_function(&set_immediate, Value::undefined(), &[callback])?;

    Ok(wrap_internal_promise(ncx, resolvers.promise))
}

struct IntervalPendingNext {
    resolve: Arc<dyn Fn(Value) + Send + Sync>,
    reject: Arc<dyn Fn(Value) + Send + Sync>,
}

struct IntervalAsyncIteratorState {
    timer_handle: Option<Value>,
    queued_ticks: usize,
    pending_next: VecDeque<IntervalPendingNext>,
    closed: bool,
    terminal_error: Option<Value>,
    signal: Option<Value>,
}

fn timers_promises_set_interval(
    _this: &Value,
    args: &[Value],
    ncx: &mut NativeContext,
) -> Result<Value, VmError> {
    let delay_ms = args
        .first()
        .and_then(|v| v.as_number())
        .unwrap_or(0.0)
        .max(0.0);
    let tick_value = args.get(1).cloned().unwrap_or(Value::undefined());
    let options = args.get(2);
    let signal = options
        .and_then(|v| v.as_object())
        .and_then(|obj| obj.get(&PropertyKey::string("signal")));

    let state = Arc::new(Mutex::new(IntervalAsyncIteratorState {
        timer_handle: None,
        queued_ticks: 0,
        pending_next: VecDeque::new(),
        closed: false,
        terminal_error: None,
        signal,
    }));

    let fn_proto = ncx
        .ctx
        .function_prototype()
        .ok_or_else(|| VmError::internal("Function.prototype not found"))?;

    // Interval callback: either wakes one pending next(), or increments queued tick count.
    let state_for_tick = Arc::clone(&state);
    let tick_value_for_tick = tick_value.clone();
    let interval_callback = Value::native_function_with_proto(
        move |_cb_this, _cb_args, cb_ncx| {
            let mut state = state_for_tick.lock().expect("interval state poisoned");
            if state.closed {
                return Ok(Value::undefined());
            }

            if let Some(signal) = &state.signal
                && signal_is_aborted_value(signal)
            {
                let abort_err = make_abort_error(cb_ncx);
                let pending = std::mem::take(&mut state.pending_next);
                let timer_handle = state.timer_handle.clone();
                state.closed = true;
                state.terminal_error = Some(abort_err.clone());
                drop(state);

                clear_interval_handle(cb_ncx, timer_handle);
                for waiter in pending {
                    (waiter.reject)(abort_err.clone());
                }
                return Ok(Value::undefined());
            }

            if let Some(waiter) = state.pending_next.pop_front() {
                let result =
                    iteration_result(cb_ncx.memory_manager(), tick_value_for_tick.clone(), false);
                drop(state);
                (waiter.resolve)(result);
                return Ok(Value::undefined());
            }

            state.queued_ticks += 1;
            Ok(Value::undefined())
        },
        ncx.memory_manager().clone(),
        fn_proto,
    );

    let set_interval = get_global_callable(ncx, "setInterval")?;
    let timer_handle = ncx.call_function(
        &set_interval,
        Value::undefined(),
        &[interval_callback, Value::number(delay_ms)],
    )?;

    {
        let mut state_guard = state.lock().expect("interval state poisoned");
        state_guard.timer_handle = Some(timer_handle);
        if let Some(signal) = &state_guard.signal
            && signal_is_aborted_value(signal)
        {
            let abort_err = make_abort_error(ncx);
            let timer = state_guard.timer_handle.clone();
            state_guard.closed = true;
            state_guard.terminal_error = Some(abort_err);
            drop(state_guard);
            clear_interval_handle(ncx, timer);
        }
    }

    let iterator = GcRef::new(JsObject::new(Value::null(), ncx.memory_manager().clone()));

    let state_for_next = Arc::clone(&state);
    let tick_value_for_next = tick_value.clone();
    let next_fn = Value::native_function_with_proto(
        move |_this, _next_args, next_ncx| {
            let resolvers = create_resolvers(next_ncx)?;
            let mut state = state_for_next.lock().expect("interval state poisoned");

            if let Some(error) = &state.terminal_error {
                let err = error.clone();
                drop(state);
                (resolvers.reject)(err);
                return Ok(wrap_internal_promise(next_ncx, resolvers.promise));
            }

            if state.closed {
                let done = iteration_result(next_ncx.memory_manager(), Value::undefined(), true);
                drop(state);
                (resolvers.resolve)(done);
                return Ok(wrap_internal_promise(next_ncx, resolvers.promise));
            }

            if let Some(signal) = &state.signal
                && signal_is_aborted_value(signal)
            {
                let abort_err = make_abort_error(next_ncx);
                let pending = std::mem::take(&mut state.pending_next);
                let timer_handle = state.timer_handle.clone();
                state.closed = true;
                state.terminal_error = Some(abort_err.clone());
                drop(state);

                clear_interval_handle(next_ncx, timer_handle);
                for waiter in pending {
                    (waiter.reject)(abort_err.clone());
                }
                (resolvers.reject)(abort_err);
                return Ok(wrap_internal_promise(next_ncx, resolvers.promise));
            }

            if state.queued_ticks > 0 {
                state.queued_ticks -= 1;
                let result = iteration_result(
                    next_ncx.memory_manager(),
                    tick_value_for_next.clone(),
                    false,
                );
                drop(state);
                (resolvers.resolve)(result);
                return Ok(wrap_internal_promise(next_ncx, resolvers.promise));
            }

            state.pending_next.push_back(IntervalPendingNext {
                resolve: resolvers.resolve.clone(),
                reject: resolvers.reject.clone(),
            });
            drop(state);
            Ok(wrap_internal_promise(next_ncx, resolvers.promise))
        },
        ncx.memory_manager().clone(),
        fn_proto,
    );

    let state_for_return = Arc::clone(&state);
    let return_fn = Value::native_function_with_proto(
        move |_this, return_args, return_ncx| {
            let return_value = return_args.first().cloned().unwrap_or(Value::undefined());
            let resolvers = create_resolvers(return_ncx)?;

            let mut state = state_for_return.lock().expect("interval state poisoned");
            let timer_handle = state.timer_handle.clone();
            let pending = std::mem::take(&mut state.pending_next);
            state.closed = true;
            state.terminal_error = None;
            state.queued_ticks = 0;
            drop(state);

            clear_interval_handle(return_ncx, timer_handle);
            let done_waiter =
                iteration_result(return_ncx.memory_manager(), Value::undefined(), true);
            for waiter in pending {
                (waiter.resolve)(done_waiter.clone());
            }

            let done_current = iteration_result(return_ncx.memory_manager(), return_value, true);
            (resolvers.resolve)(done_current);
            Ok(wrap_internal_promise(return_ncx, resolvers.promise))
        },
        ncx.memory_manager().clone(),
        fn_proto,
    );

    let state_for_throw = Arc::clone(&state);
    let throw_fn = Value::native_function_with_proto(
        move |_this, throw_args, throw_ncx| {
            let throw_value = throw_args.first().cloned().unwrap_or(Value::undefined());
            let resolvers = create_resolvers(throw_ncx)?;

            let mut state = state_for_throw.lock().expect("interval state poisoned");
            let timer_handle = state.timer_handle.clone();
            let pending = std::mem::take(&mut state.pending_next);
            state.closed = true;
            state.terminal_error = Some(throw_value.clone());
            state.queued_ticks = 0;
            drop(state);

            clear_interval_handle(throw_ncx, timer_handle);
            for waiter in pending {
                (waiter.reject)(throw_value.clone());
            }

            (resolvers.reject)(throw_value);
            Ok(wrap_internal_promise(throw_ncx, resolvers.promise))
        },
        ncx.memory_manager().clone(),
        fn_proto,
    );

    let async_iterator_fn = Value::native_function_with_proto(
        move |this, _args, _iter_ncx| Ok(this.clone()),
        ncx.memory_manager().clone(),
        fn_proto,
    );

    let _ = iterator.set(PropertyKey::string("next"), next_fn);
    let _ = iterator.set(PropertyKey::string("return"), return_fn);
    let _ = iterator.set(PropertyKey::string("throw"), throw_fn);
    let _ = iterator.set(
        PropertyKey::Symbol(otter_vm_core::intrinsics::well_known::async_iterator_symbol()),
        async_iterator_fn,
    );

    Ok(Value::object(iterator))
}

fn create_resolvers(ncx: &NativeContext) -> Result<PromiseWithResolvers, VmError> {
    let mm = ncx.memory_manager().clone();
    let js_queue = ncx
        .js_job_queue()
        .ok_or_else(|| VmError::type_error("No JS job queue available for Promise operation"))?;
    let js_queue_for_resolvers = Arc::clone(&js_queue);
    Ok(JsPromise::with_resolvers(mm, move |job, job_args| {
        js_queue_for_resolvers.enqueue(job, job_args);
    }))
}

fn clear_interval_handle(ncx: &mut NativeContext, timer_handle: Option<Value>) {
    if let Some(handle) = timer_handle
        && let Ok(clear_interval) = get_global_callable(ncx, "clearInterval")
    {
        let _ = ncx.call_function(&clear_interval, Value::undefined(), &[handle]);
    }
}

fn signal_is_aborted_value(signal: &Value) -> bool {
    signal
        .as_object()
        .and_then(|o| o.get(&PropertyKey::string("aborted")))
        .is_some_and(|v| v.to_boolean())
}

fn iteration_result(mm: &Arc<MemoryManager>, value: Value, done: bool) -> Value {
    let result = GcRef::new(JsObject::new(Value::null(), mm.clone()));
    let _ = result.set(PropertyKey::string("value"), value);
    let _ = result.set(PropertyKey::string("done"), Value::boolean(done));
    Value::object(result)
}

fn scheduler_wait(
    _this: &Value,
    args: &[Value],
    ncx: &mut NativeContext,
) -> Result<Value, VmError> {
    let delay = args.first().cloned().unwrap_or(Value::number(0.0));
    let options = args.get(1).cloned().unwrap_or(Value::undefined());
    timers_promises_set_timeout(_this, &[delay, Value::undefined(), options], ncx)
}

fn scheduler_yield(
    _this: &Value,
    args: &[Value],
    ncx: &mut NativeContext,
) -> Result<Value, VmError> {
    let options = args.first().cloned().unwrap_or(Value::undefined());
    timers_promises_set_immediate(_this, &[Value::undefined(), options], ncx)
}

fn get_global_callable(ncx: &NativeContext, name: &str) -> Result<Value, VmError> {
    let value = ncx
        .global()
        .get(&PropertyKey::string(name))
        .ok_or_else(|| VmError::type_error(&format!("{} is not available", name)))?;
    if !value.is_callable() {
        return Err(VmError::type_error(&format!("{} is not callable", name)));
    }
    Ok(value)
}

fn signal_is_aborted(options: Option<&Value>, _ncx: &NativeContext) -> Result<bool, VmError> {
    let Some(options) = options else {
        return Ok(false);
    };
    let Some(opts_obj) = options.as_object() else {
        return Ok(false);
    };
    let Some(signal) = opts_obj.get(&PropertyKey::string("signal")) else {
        return Ok(false);
    };
    let Some(signal_obj) = signal.as_object() else {
        return Ok(false);
    };
    Ok(signal_obj
        .get(&PropertyKey::string("aborted"))
        .is_some_and(|v| v.to_boolean()))
}

fn make_abort_error(ncx: &mut NativeContext) -> Value {
    let message = Value::string(JsString::intern("The operation was aborted"));
    if let Some(error_ctor) = ncx.global().get(&PropertyKey::string("Error"))
        && error_ctor.is_callable()
        && let Ok(err) = ncx.call_function_construct(&error_ctor, Value::undefined(), &[message])
    {
        if let Some(obj) = err.as_object() {
            let _ = obj.set(
                PropertyKey::string("name"),
                Value::string(JsString::intern("AbortError")),
            );
            let _ = obj.set(
                PropertyKey::string("code"),
                Value::string(JsString::intern("ABORT_ERR")),
            );
        }
        return err;
    }
    Value::string(JsString::intern("AbortError: The operation was aborted"))
}

fn rejected_promise(ncx: &mut NativeContext, reason: Value) -> Result<Value, VmError> {
    let mm = ncx.memory_manager().clone();
    let js_queue = ncx
        .js_job_queue()
        .ok_or_else(|| VmError::type_error("No JS job queue available for Promise operation"))?;
    let js_queue_for_resolvers = Arc::clone(&js_queue);
    let resolvers = JsPromise::with_resolvers(mm, move |job, job_args| {
        js_queue_for_resolvers.enqueue(job, job_args);
    });
    (resolvers.reject)(reason);
    Ok(wrap_internal_promise(ncx, resolvers.promise))
}

fn wrap_internal_promise(ncx: &NativeContext, internal: GcRef<JsPromise>) -> Value {
    let obj = GcRef::new(JsObject::new(Value::null(), ncx.memory_manager().clone()));
    let _ = obj.set(PropertyKey::string("_internal"), Value::promise(internal));

    if let Some(promise_ctor) = ncx
        .global()
        .get(&PropertyKey::string("Promise"))
        .and_then(|v| v.as_object())
        && let Some(proto) = promise_ctor
            .get(&PropertyKey::string("prototype"))
            .and_then(|v| v.as_object())
    {
        if let Some(then_fn) = proto.get(&PropertyKey::string("then")) {
            let _ = obj.set(PropertyKey::string("then"), then_fn);
        }
        if let Some(catch_fn) = proto.get(&PropertyKey::string("catch")) {
            let _ = obj.set(PropertyKey::string("catch"), catch_fn);
        }
        if let Some(finally_fn) = proto.get(&PropertyKey::string("finally")) {
            let _ = obj.set(PropertyKey::string("finally"), finally_fn);
        }
        obj.set_prototype(Value::object(proto));
    }

    Value::object(obj)
}
