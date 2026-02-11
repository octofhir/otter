use otter_vm_core::VmError;
use otter_vm_core::value::Value;
use otter_vm_runtime::extension::{Extension, Op, OpHandler};
use otter_vm_runtime::otter_runtime::Otter;
use std::sync::Arc;
use std::time::Duration;

pub fn create_timers_extension(otter: &Otter) -> Extension {
    let event_loop = otter.event_loop().clone();

    // Helper to create job for callback
    let create_job = |callback: Value, _args: Vec<Value>| otter_vm_core::promise::JsPromiseJob {
        kind: otter_vm_core::promise::JsPromiseJobKind::Fulfill,
        callback,
        this_arg: Value::undefined(),
        result_promise: None,
    };

    // setImmediate(callback, ...args)
    let set_immediate_op = Op {
        name: "setImmediate".to_string(),
        handler: OpHandler::Native(Arc::new({
            let event_loop = event_loop.clone();
            move |args, _mm| {
                if args.is_empty() {
                    return Err(VmError::type_error("Callback required"));
                }
                let callback = args[0].clone();
                if !callback.is_function() {
                    return Err(VmError::type_error("Callback must be a function"));
                }
                let extra_args = if args.len() > 1 {
                    args[1..].to_vec()
                } else {
                    Vec::new()
                };

                let event_loop_inner = event_loop.clone();
                let immediate_id = event_loop.schedule_immediate(
                    move || {
                        let job = create_job(callback, extra_args.clone());
                        event_loop_inner.js_job_queue().enqueue(job, extra_args);
                    },
                    true,
                );

                Ok(Value::number(immediate_id.0 as f64))
            }
        })),
    };

    // clearImmediate(immediate)
    let clear_immediate_op = Op {
        name: "clearImmediate".to_string(),
        handler: OpHandler::Native(Arc::new({
            let event_loop = event_loop.clone();
            move |args, _mm| {
                if let Some(arg) = args.first() {
                    if let Some(n) = arg.as_number() {
                        event_loop.clear_immediate(otter_vm_runtime::timer::ImmediateId(n as u64));
                    }
                }
                Ok(Value::undefined())
            }
        })),
    };

    // setTimeout(callback, delay, ...args)
    let set_timeout_op = Op {
        name: "setTimeout".to_string(),
        handler: OpHandler::Native(Arc::new({
            let event_loop = event_loop.clone();
            move |args, _mm| {
                if args.is_empty() {
                    return Err(VmError::type_error("Callback required"));
                }
                let callback = args[0].clone();
                if !callback.is_function() {
                    return Err(VmError::type_error("Callback must be a function"));
                }
                let delay = args
                    .get(1)
                    .and_then(|v| v.as_number())
                    .unwrap_or(0.0)
                    .max(0.0) as u64;
                let extra_args = if args.len() > 2 {
                    args[2..].to_vec()
                } else {
                    Vec::new()
                };

                let event_loop_inner = event_loop.clone();
                let timer_id = event_loop.set_timeout(
                    move || {
                        let job = create_job(callback, extra_args.clone());
                        event_loop_inner.js_job_queue().enqueue(job, extra_args);
                    },
                    Duration::from_millis(delay),
                );

                Ok(Value::number(timer_id.0 as f64))
            }
        })),
    };

    // clearTimeout(timeout)
    let clear_timeout_op = Op {
        name: "clearTimeout".to_string(),
        handler: OpHandler::Native(Arc::new({
            let event_loop = event_loop.clone();
            move |args, _mm| {
                if let Some(arg) = args.first() {
                    if let Some(n) = arg.as_number() {
                        event_loop.clear_timeout(otter_vm_runtime::timer::TimerId(n as u64));
                    }
                }
                Ok(Value::undefined())
            }
        })),
    };

    // setInterval(callback, delay, ...args)
    let set_interval_op = Op {
        name: "setInterval".to_string(),
        handler: OpHandler::Native(Arc::new({
            let event_loop = event_loop.clone();
            move |args, _mm| {
                if args.is_empty() {
                    return Err(VmError::type_error("Callback required"));
                }
                let callback = args[0].clone();
                if !callback.is_function() {
                    return Err(VmError::type_error("Callback must be a function"));
                }
                let delay = args
                    .get(1)
                    .and_then(|v| v.as_number())
                    .unwrap_or(0.0)
                    .max(0.0) as u64;
                let extra_args = if args.len() > 2 {
                    args[2..].to_vec()
                } else {
                    Vec::new()
                };

                let event_loop_inner = event_loop.clone();
                let extra_args_clone = extra_args.clone();
                let callback_clone = callback.clone();

                let timer_id = event_loop.set_interval(
                    move || {
                        let job = create_job(callback_clone.clone(), extra_args_clone.clone());
                        event_loop_inner
                            .js_job_queue()
                            .enqueue(job, extra_args_clone.clone());
                    },
                    Duration::from_millis(delay),
                );

                Ok(Value::number(timer_id.0 as f64))
            }
        })),
    };

    // clearInterval(interval)
    let clear_interval_op = Op {
        name: "clearInterval".to_string(),
        handler: OpHandler::Native(Arc::new({
            let event_loop = event_loop.clone();
            move |args, _mm| {
                if let Some(arg) = args.first() {
                    if let Some(n) = arg.as_number() {
                        event_loop.clear_timer(otter_vm_runtime::timer::TimerId(n as u64));
                    }
                }
                Ok(Value::undefined())
            }
        })),
    };

    // queueMicrotask(callback)
    let queue_microtask_op = Op {
        name: "queueMicrotask".to_string(),
        handler: OpHandler::Native(Arc::new({
            let event_loop = event_loop.clone();
            move |args, _mm| {
                if args.is_empty() {
                    return Err(VmError::type_error("Callback required"));
                }
                let callback = args[0].clone();
                if !callback.is_function() {
                    return Err(VmError::type_error("Callback must be a function"));
                }

                let job = create_job(callback, Vec::new());
                event_loop.js_job_queue().enqueue(job, Vec::new());

                Ok(Value::undefined())
            }
        })),
    };

    Extension::new("node_timers")
        .with_ops(vec![
            set_immediate_op,
            clear_immediate_op,
            set_timeout_op,
            clear_timeout_op,
            set_interval_op,
            clear_interval_op,
            queue_microtask_op,
        ])
        .with_js("
            // structuredClone shim
            globalThis.structuredClone = function(value) {
                if (value === undefined) return undefined;
                return JSON.parse(JSON.stringify(value));
            };
            
            // fetch stub
            globalThis.fetch = function(url, options) {
                return Promise.reject(new Error('fetch is not yet implemented in node-compat mode'));
            };
        ")
}
