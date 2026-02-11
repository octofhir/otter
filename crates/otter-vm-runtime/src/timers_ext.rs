use std::sync::Arc;
use std::time::Duration;

use otter_vm_core::VmError;
use otter_vm_core::value::Value;

use crate::extension::{Extension, Op, OpHandler};
use crate::otter_runtime::Otter;

pub fn create_timers_extension(otter: &Otter) -> Extension {
    let event_loop = otter.event_loop().clone();

    let create_job = |callback: Value| otter_vm_core::promise::JsPromiseJob {
        kind: otter_vm_core::promise::JsPromiseJobKind::Fulfill,
        callback,
        this_arg: Value::undefined(),
        result_promise: None,
    };

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
                        let job = create_job(callback);
                        event_loop_inner.js_job_queue().enqueue(job, extra_args);
                    },
                    true,
                );

                Ok(Value::number(immediate_id.0 as f64))
            }
        })),
    };

    let clear_immediate_op = Op {
        name: "clearImmediate".to_string(),
        handler: OpHandler::Native(Arc::new({
            let event_loop = event_loop.clone();
            move |args, _mm| {
                if let Some(arg) = args.first()
                    && let Some(n) = arg.as_number()
                {
                    event_loop.clear_immediate(crate::timer::ImmediateId(n as u64));
                }
                Ok(Value::undefined())
            }
        })),
    };

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
                        let job = create_job(callback);
                        event_loop_inner.js_job_queue().enqueue(job, extra_args);
                    },
                    Duration::from_millis(delay),
                );

                Ok(Value::number(timer_id.0 as f64))
            }
        })),
    };

    let clear_timeout_op = Op {
        name: "clearTimeout".to_string(),
        handler: OpHandler::Native(Arc::new({
            let event_loop = event_loop.clone();
            move |args, _mm| {
                if let Some(arg) = args.first()
                    && let Some(n) = arg.as_number()
                {
                    event_loop.clear_timeout(crate::timer::TimerId(n as u64));
                }
                Ok(Value::undefined())
            }
        })),
    };

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
                        let job = create_job(callback_clone.clone());
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

    let clear_interval_op = Op {
        name: "clearInterval".to_string(),
        handler: OpHandler::Native(Arc::new({
            let event_loop = event_loop.clone();
            move |args, _mm| {
                if let Some(arg) = args.first()
                    && let Some(n) = arg.as_number()
                {
                    event_loop.clear_timer(crate::timer::TimerId(n as u64));
                }
                Ok(Value::undefined())
            }
        })),
    };

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

                let job = create_job(callback);
                event_loop.js_job_queue().enqueue(job, Vec::new());

                Ok(Value::undefined())
            }
        })),
    };

    Extension::new("runtime_timers").with_ops(vec![
        set_immediate_op,
        clear_immediate_op,
        set_timeout_op,
        clear_timeout_op,
        set_interval_op,
        clear_interval_op,
        queue_microtask_op,
    ])
}
