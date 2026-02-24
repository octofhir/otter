use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use otter_vm_core::VmError;
use otter_vm_core::value::Value;

use crate::extension::{Extension, Op, OpHandler};
use crate::otter_runtime::Otter;

/// Build the timer [`Extension`] (setTimeout, setInterval, setImmediate, queueMicrotask, etc.).
pub fn create_timers_extension(otter: &Otter) -> Extension {
    let event_loop = otter.event_loop().clone();
    // Shared GC root registry for all active timer callbacks.
    // Populated here (Arc clone passed into each Op handler closure) and
    // registered with VmContext in `Otter::configure_timer_roots`.
    let timer_roots = event_loop.timer_callback_roots();

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
            let timer_roots = Arc::clone(&timer_roots);
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

                // Clone for GC root registration; the original is moved into the closure.
                let callback_for_roots = callback.clone();
                let extra_args_for_roots = extra_args.clone();

                // Slot to communicate the immediate_id back into the firing closure
                // so it can remove itself from GC roots when it fires.
                let id_slot: Arc<AtomicU64> = Arc::new(AtomicU64::new(0));
                let id_slot_inner = Arc::clone(&id_slot);
                let roots_inner = Arc::clone(&timer_roots);

                let event_loop_inner = event_loop.clone();
                let immediate_id = event_loop.schedule_immediate(
                    move || {
                        // Remove from GC roots — this once-only callback is consumed.
                        roots_inner.remove(id_slot_inner.load(Ordering::Acquire));
                        let job = create_job(callback);
                        event_loop_inner.js_job_queue().enqueue(job, extra_args);
                    },
                    true,
                );

                // Store id so the closure can remove itself on fire.
                // Safe: setImmediate runs at the next event loop tick, which only
                // happens when JS returns control — never during this native handler.
                id_slot.store(immediate_id.0, Ordering::Release);
                timer_roots.register(immediate_id.0, callback_for_roots, extra_args_for_roots);

                Ok(Value::number(immediate_id.0 as f64))
            }
        })),
    };

    let clear_immediate_op = Op {
        name: "clearImmediate".to_string(),
        handler: OpHandler::Native(Arc::new({
            let event_loop = event_loop.clone();
            let timer_roots = Arc::clone(&timer_roots);
            move |args, _mm| {
                if let Some(arg) = args.first()
                    && let Some(n) = arg.as_number()
                {
                    let id = n as u64;
                    timer_roots.remove(id);
                    event_loop.clear_immediate(crate::timer::ImmediateId(id));
                }
                Ok(Value::undefined())
            }
        })),
    };

    let set_timeout_op = Op {
        name: "setTimeout".to_string(),
        handler: OpHandler::Native(Arc::new({
            let event_loop = event_loop.clone();
            let timer_roots = Arc::clone(&timer_roots);
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

                let callback_for_roots = callback.clone();
                let extra_args_for_roots = extra_args.clone();

                let id_slot: Arc<AtomicU64> = Arc::new(AtomicU64::new(0));
                let id_slot_inner = Arc::clone(&id_slot);
                let roots_inner = Arc::clone(&timer_roots);

                let event_loop_inner = event_loop.clone();
                let timer_id = event_loop.set_timeout(
                    move || {
                        roots_inner.remove(id_slot_inner.load(Ordering::Acquire));
                        let job = create_job(callback);
                        event_loop_inner.js_job_queue().enqueue(job, extra_args);
                    },
                    Duration::from_millis(delay),
                );

                id_slot.store(timer_id.0, Ordering::Release);
                timer_roots.register(timer_id.0, callback_for_roots, extra_args_for_roots);

                Ok(Value::number(timer_id.0 as f64))
            }
        })),
    };

    let clear_timeout_op = Op {
        name: "clearTimeout".to_string(),
        handler: OpHandler::Native(Arc::new({
            let event_loop = event_loop.clone();
            let timer_roots = Arc::clone(&timer_roots);
            move |args, _mm| {
                if let Some(arg) = args.first()
                    && let Some(n) = arg.as_number()
                {
                    let id = n as u64;
                    timer_roots.remove(id);
                    event_loop.clear_timeout(crate::timer::TimerId(id));
                }
                Ok(Value::undefined())
            }
        })),
    };

    let set_interval_op = Op {
        name: "setInterval".to_string(),
        handler: OpHandler::Native(Arc::new({
            let event_loop = event_loop.clone();
            let timer_roots = Arc::clone(&timer_roots);
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

                let callback_for_roots = callback.clone();
                let extra_args_for_roots = extra_args.clone();

                let event_loop_inner = event_loop.clone();
                let extra_args_clone = extra_args.clone();
                let callback_clone = callback.clone();
                let timer_id = event_loop.set_interval(
                    move || {
                        // Interval fires repeatedly — do NOT remove from roots here.
                        // Removal happens in clearInterval.
                        let job = create_job(callback_clone.clone());
                        event_loop_inner
                            .js_job_queue()
                            .enqueue(job, extra_args_clone.clone());
                    },
                    Duration::from_millis(delay),
                );

                timer_roots.register(timer_id.0, callback_for_roots, extra_args_for_roots);

                Ok(Value::number(timer_id.0 as f64))
            }
        })),
    };

    let clear_interval_op = Op {
        name: "clearInterval".to_string(),
        handler: OpHandler::Native(Arc::new({
            let event_loop = event_loop.clone();
            let timer_roots = Arc::clone(&timer_roots);
            move |args, _mm| {
                if let Some(arg) = args.first()
                    && let Some(n) = arg.as_number()
                {
                    let id = n as u64;
                    timer_roots.remove(id);
                    event_loop.clear_timer(crate::timer::TimerId(id));
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

                // queueMicrotask fires synchronously before next event loop tick;
                // the callback is enqueued into js_job_queue which is already a GC root.
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
