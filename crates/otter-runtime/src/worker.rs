//! Worker isolate handles and isolate-pool routing.
//!
//! The runtime worker model is isolate-per-worker: each worker owns a
//! separate runtime runner and therefore a separate VM, runtime state,
//! and GC heap. This module provides the host-facing handle shape while
//! keeping JS-visible `Worker`, message ports, and transferables for
//! later slices.
//!
//! # Contents
//!
//! - [`Worker`] — sendable handle to one worker isolate.
//! - [`WorkerBuilder`] — configuration for one worker.
//! - [`OtterPool`] — small round-robin isolate pool prototype.
//!
//! # Invariants
//!
//! - A worker is backed by its own [`crate::RuntimeHandle`]; no heap
//!   or VM state is shared between workers.
//! - Worker methods accept only owned public inputs and return
//!   [`crate::ExecutionResult`] / [`crate::OtterError`].
//! - Structured worker messages must use
//!   [`crate::StructuredCloneValue`], not `otter_vm::Value` or GC
//!   handles.
//!
//! # See also
//!
//! - [Event loop](../../../docs/book/src/engine/event-loop.md)
//! - [Runtime architecture](../../../docs/book/src/engine/architecture.md)

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::Duration;

use otter_gc::raw::RawGc;
use otter_vm::bigint::BigIntValue;
use otter_vm::binary::JsArrayBuffer;
use otter_vm::binary::array_buffer::SharedBody;
use otter_vm::number::NumberValue;
use otter_vm::string::JsString;
use otter_vm::{
    Local, NativeCall, NativeCtx, NativeError, NativeFn, Value, array, collections, object,
};
use smallvec::smallvec;

use crate::module_loader;
use crate::{
    CapabilitySet, ExecutionResult, OtterError, Runtime, RuntimeActivityStats, RuntimeBuilder,
    RuntimeConfig, RuntimeHandle, SourceInput, StructuredCloneNumber, StructuredCloneTransferList,
    StructuredCloneValue,
};

static NEXT_WORKER_ID: AtomicU64 = AtomicU64::new(1);

/// Stable host-side worker identifier.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct WorkerId(u64);

impl WorkerId {
    /// Numeric worker id. Monotonic within this process.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }
}

#[derive(Clone)]
enum WorkerPayload {
    Undefined,
    Null,
    Boolean(bool),
    Number(StructuredCloneNumber),
    BigInt(String),
    String(String),
    Array(Vec<WorkerPayload>),
    Object(Vec<(String, WorkerPayload)>),
    Map(Vec<(WorkerPayload, WorkerPayload)>),
    Set(Vec<WorkerPayload>),
    ArrayBuffer(Vec<u8>),
    SharedArrayBuffer(Arc<SharedBody>),
}

#[derive(Default)]
struct WorkerTransferList {
    buffers: Vec<JsArrayBuffer>,
    set: HashSet<JsArrayBuffer>,
}

enum WorkerCommand {
    Message(WorkerPayload),
    Shutdown,
}

enum WorkerEvent {
    Message(WorkerPayload),
    Error(String),
    MessageError(String),
    Closed,
}

const WORKER_COMMAND_POLL_INTERVAL: Duration = Duration::from_millis(10);

struct WorkerRecord {
    id: WorkerId,
    tx: mpsc::Sender<WorkerCommand>,
    events: Mutex<mpsc::Receiver<WorkerEvent>>,
    interrupt: crate::InterruptHandle,
    join: Mutex<Option<thread::JoinHandle<()>>>,
    terminated: std::sync::atomic::AtomicBool,
}

impl WorkerRecord {
    fn terminate(&self) {
        if self
            .terminated
            .swap(true, std::sync::atomic::Ordering::SeqCst)
        {
            return;
        }
        let _ = self.tx.send(WorkerCommand::Shutdown);
        self.interrupt.interrupt();
        otter_vm::atomics_wait::cancel_all_waiters();
    }

    fn join(&self) {
        if let Some(join) = self.join.lock().expect("worker join mutex poisoned").take() {
            let _ = join.join();
        }
    }
}

impl Drop for WorkerRecord {
    fn drop(&mut self) {
        self.terminate();
        if let Some(join) = self
            .join
            .get_mut()
            .expect("worker join mutex poisoned")
            .take()
        {
            let _ = join.join();
        }
    }
}

#[derive(Default)]
struct WorkerHostState {
    config: RuntimeConfig,
    workers: Mutex<HashMap<u64, Arc<WorkerRecord>>>,
}

impl WorkerHostState {
    fn new(config: RuntimeConfig) -> Self {
        Self {
            config,
            workers: Mutex::new(HashMap::new()),
        }
    }

    fn insert(&self, record: Arc<WorkerRecord>) {
        self.workers
            .lock()
            .expect("worker registry poisoned")
            .insert(record.id.get(), record);
    }

    fn get(&self, id: u64) -> Option<Arc<WorkerRecord>> {
        self.workers
            .lock()
            .expect("worker registry poisoned")
            .get(&id)
            .cloned()
    }

    fn remove(&self, id: u64) -> Option<Arc<WorkerRecord>> {
        self.workers
            .lock()
            .expect("worker registry poisoned")
            .remove(&id)
    }
}

impl Drop for WorkerHostState {
    fn drop(&mut self) {
        let workers: Vec<_> = self
            .workers
            .lock()
            .expect("worker registry poisoned")
            .drain()
            .map(|(_, worker)| worker)
            .collect();
        for worker in &workers {
            worker.terminate();
        }
        for worker in workers {
            worker.join();
        }
    }
}

pub(crate) fn install_main_worker_globals(runtime: &mut Runtime) -> Result<(), OtterError> {
    let host = Arc::new(WorkerHostState::new(runtime.config.clone()));
    install_worker_host_natives(runtime, Arc::clone(&host))?;
    runtime.install_native_constructor_global_call("Worker", 2, worker_constructor_call(host))?;
    Ok(())
}

fn install_worker_host_natives(
    runtime: &mut Runtime,
    host: Arc<WorkerHostState>,
) -> Result<(), OtterError> {
    runtime.install_native_global_call(
        "__otter_worker_spawn",
        2,
        worker_spawn_call(host.clone()),
    )?;
    runtime.install_native_global_call(
        "__otter_worker_post_message",
        3,
        worker_post_message_call(host.clone()),
    )?;
    runtime.install_native_global_call(
        "__otter_worker_terminate",
        1,
        worker_terminate_call(host.clone()),
    )?;
    runtime.install_native_global_call("__otter_worker_drain", 1, worker_drain_call(host))?;
    Ok(())
}

fn worker_constructor_call(host: Arc<WorkerHostState>) -> NativeCall {
    let call: Arc<NativeFn> = Arc::new(move |ctx, args, _captures| {
        let specifier = value_to_string(ctx, args.first().unwrap_or(&Value::undefined()))?;
        let id = spawn_worker_record(&host, specifier)?;
        let post_host = host.clone();
        let post: Arc<NativeFn> = Arc::new(move |ctx, args, _captures| {
            let id = worker_id_from_this(ctx, "Worker.postMessage")?;
            let Some(record) = post_host.get(id) else {
                return Err(type_err(
                    "Worker.postMessage",
                    "worker is not running".to_string(),
                ));
            };
            if record.terminated.load(std::sync::atomic::Ordering::SeqCst) {
                return Err(type_err(
                    "Worker.postMessage",
                    "worker has been terminated".to_string(),
                ));
            }
            let transfers = parse_worker_transfer_list(args.get(1), ctx)?;
            let payload = clone_worker_value(
                args.first().unwrap_or(&Value::undefined()),
                ctx.heap(),
                &transfers,
            )?;
            detach_worker_transfers(&transfers, ctx.heap_mut());
            record
                .tx
                .send(WorkerCommand::Message(payload))
                .map_err(|_| {
                    type_err("Worker.postMessage", "worker channel is closed".to_string())
                })?;
            Ok(Value::undefined())
        });

        let terminate_host = host.clone();
        let terminate: Arc<NativeFn> = Arc::new(move |ctx, _args, _captures| {
            let worker = ctx
                .this_value()
                .as_object()
                .ok_or_else(|| type_err("Worker.terminate", "invalid receiver".to_string()))?;
            clear_worker_poll_timer(ctx, worker)?;
            let id = worker_id_from_object(ctx, worker, "Worker.terminate")?;
            if let Some(record) = terminate_host.remove(id) {
                record.terminate();
            }
            Ok(Value::undefined())
        });

        let dispatch: Arc<NativeFn> = Arc::new(move |ctx, args, _captures| {
            let worker = ctx
                .this_value()
                .as_object()
                .ok_or_else(|| type_err("Worker.dispatchEvent", "invalid receiver".to_string()))?;
            let event = args.first().copied().unwrap_or(Value::undefined());
            let event_obj = event.as_object().ok_or_else(|| {
                type_err(
                    "Worker.dispatchEvent",
                    "event must be an object".to_string(),
                )
            })?;
            dispatch_event_object(ctx, worker, event_obj)?;
            Ok(Value::boolean(true))
        });

        let add: Arc<NativeFn> = Arc::new(move |ctx, args, _captures| {
            let worker = ctx.this_value().as_object().ok_or_else(|| {
                type_err("Worker.addEventListener", "invalid receiver".to_string())
            })?;
            let ty = value_to_string(ctx, args.first().unwrap_or(&Value::undefined()))?;
            if let Some(listener) = args.get(1)
                && listener.is_callable()
            {
                add_worker_event_listener(ctx, worker, &ty, *listener)?;
            }
            Ok(Value::undefined())
        });

        let remove: Arc<NativeFn> = Arc::new(move |ctx, args, _captures| {
            let worker = ctx.this_value().as_object().ok_or_else(|| {
                type_err("Worker.removeEventListener", "invalid receiver".to_string())
            })?;
            let ty = value_to_string(ctx, args.first().unwrap_or(&Value::undefined()))?;
            if let Some(listener) = args.get(1) {
                remove_worker_event_listener(ctx, worker, &ty, *listener)?;
            }
            Ok(Value::undefined())
        });

        let worker = ctx.scope(|mut scope| {
            let worker = scope.object()?;
            let worker_id = scope.number(id as f64);
            scope.set(worker, "__otterWorkerId", worker_id)?;
            let null = scope.null();
            scope.set(worker, "onmessage", null)?;
            scope.set(worker, "onerror", null)?;
            scope.set(worker, "onmessageerror", null)?;
            let listeners = scope.object()?;
            scope.set(worker, "__otterListeners", listeners)?;

            for (name, length, call) in [
                ("postMessage", 1, NativeCall::Dynamic(post)),
                ("terminate", 0, NativeCall::Dynamic(terminate)),
                ("dispatchEvent", 1, NativeCall::Dynamic(dispatch)),
                ("addEventListener", 2, NativeCall::Dynamic(add)),
                ("removeEventListener", 2, NativeCall::Dynamic(remove)),
            ] {
                let function = scope.native_call(name, length, call)?;
                scope.set(worker, name, function)?;
            }
            Ok::<Value, NativeError>(scope.finish(worker))
        })?;
        install_worker_poll_timer(ctx, host.clone(), worker)
    });
    NativeCall::Dynamic(call)
}

fn spawn_worker_record(host: &Arc<WorkerHostState>, specifier: String) -> Result<u64, NativeError> {
    let id = WorkerId(NEXT_WORKER_ID.fetch_add(1, Ordering::Relaxed));
    let (cmd_tx, cmd_rx) = mpsc::channel();
    let (event_tx, event_rx) = mpsc::channel();
    let child_config = host.config.clone();
    let (interrupt_tx, interrupt_rx) = mpsc::sync_channel(1);
    let thread_name = format!("otter-worker-{}", id.get());
    let join = thread::Builder::new()
        .name(thread_name)
        .spawn(move || {
            run_js_worker(id, specifier, child_config, cmd_rx, event_tx, interrupt_tx);
        })
        .map_err(|err| type_err("Worker", format!("worker spawn failed: {err}")))?;
    let interrupt = interrupt_rx.recv().map_err(|_| {
        type_err(
            "Worker",
            "worker runtime stopped before exposing interrupt handle".to_string(),
        )
    })?;
    let record = Arc::new(WorkerRecord {
        id,
        tx: cmd_tx,
        events: Mutex::new(event_rx),
        interrupt,
        join: Mutex::new(Some(join)),
        terminated: std::sync::atomic::AtomicBool::new(false),
    });
    host.insert(record);
    Ok(id.get())
}

fn worker_spawn_call(host: Arc<WorkerHostState>) -> NativeCall {
    let call: Arc<NativeFn> = Arc::new(move |ctx, args, _captures| {
        let specifier = value_to_string(ctx, args.first().unwrap_or(&Value::undefined()))?;
        let id = WorkerId(NEXT_WORKER_ID.fetch_add(1, Ordering::Relaxed));
        let (cmd_tx, cmd_rx) = mpsc::channel();
        let (event_tx, event_rx) = mpsc::channel();
        let child_config = host.config.clone();
        let (interrupt_tx, interrupt_rx) = mpsc::sync_channel(1);
        let thread_name = format!("otter-worker-{}", id.get());
        let join = thread::Builder::new()
            .name(thread_name)
            .spawn(move || {
                run_js_worker(id, specifier, child_config, cmd_rx, event_tx, interrupt_tx);
            })
            .map_err(|err| type_err("Worker", format!("worker spawn failed: {err}")))?;
        let interrupt = interrupt_rx.recv().map_err(|_| {
            type_err(
                "Worker",
                "worker runtime stopped before exposing interrupt handle".to_string(),
            )
        })?;
        let record = Arc::new(WorkerRecord {
            id,
            tx: cmd_tx,
            events: Mutex::new(event_rx),
            interrupt,
            join: Mutex::new(Some(join)),
            terminated: std::sync::atomic::AtomicBool::new(false),
        });
        host.insert(record);
        Ok(Value::number_f64(id.get() as f64))
    });
    NativeCall::Dynamic(call)
}

fn worker_post_message_call(host: Arc<WorkerHostState>) -> NativeCall {
    let call: Arc<NativeFn> = Arc::new(move |ctx, args, _captures| {
        let id = numeric_worker_id(args.first().unwrap_or(&Value::undefined()))?;
        let Some(record) = host.get(id) else {
            return Err(type_err(
                "Worker.postMessage",
                "worker is not running".to_string(),
            ));
        };
        if record.terminated.load(std::sync::atomic::Ordering::SeqCst) {
            return Err(type_err(
                "Worker.postMessage",
                "worker has been terminated".to_string(),
            ));
        }
        let undefined = Value::undefined();
        let value = args.get(1).unwrap_or(&undefined);
        let transfers = parse_worker_transfer_list(args.get(2), ctx)?;
        let payload = clone_worker_value(value, ctx.heap(), &transfers)?;
        detach_worker_transfers(&transfers, ctx.heap_mut());
        record
            .tx
            .send(WorkerCommand::Message(payload))
            .map_err(|_| type_err("Worker.postMessage", "worker channel is closed".to_string()))?;
        Ok(Value::undefined())
    });
    NativeCall::Dynamic(call)
}

fn worker_terminate_call(host: Arc<WorkerHostState>) -> NativeCall {
    let call: Arc<NativeFn> = Arc::new(move |_ctx, args, _captures| {
        let id = numeric_worker_id(args.first().unwrap_or(&Value::undefined()))?;
        if let Some(record) = host.remove(id) {
            record.terminate();
        }
        Ok(Value::undefined())
    });
    NativeCall::Dynamic(call)
}

fn worker_drain_call(host: Arc<WorkerHostState>) -> NativeCall {
    let call: Arc<NativeFn> = Arc::new(move |ctx, args, _captures| {
        let id = numeric_worker_id(args.first().unwrap_or(&Value::undefined()))?;
        let Some(record) = host.get(id) else {
            return Ok(Value::undefined());
        };
        let mut drained = Vec::new();
        {
            let events = record
                .events
                .lock()
                .expect("worker event receiver poisoned");
            while let Ok(event) = events.try_recv() {
                match worker_event_to_value(ctx, event) {
                    Ok(value) => drained.push(value),
                    Err(err) => drained.push(worker_event_to_value(
                        ctx,
                        WorkerEvent::MessageError(err.to_string()),
                    )?),
                }
            }
        }
        if drained.is_empty() {
            return Ok(Value::undefined());
        }
        let array = ctx.array_from_elements(drained)?;
        Ok(Value::array(array))
    });
    NativeCall::Dynamic(call)
}

fn install_worker_poll_timer(
    ctx: &mut NativeCtx<'_>,
    host: Arc<WorkerHostState>,
    worker_value: Value,
) -> Result<Value, NativeError> {
    let poll = ctx.native_value(
        "__otter_worker_poll",
        smallvec![worker_value],
        move |ctx, _args, captures| {
            let Some(worker) = captures.first().and_then(|value| value.as_object()) else {
                return Ok(Value::undefined());
            };
            let id = worker_id_from_object(ctx, worker, "Worker")?;
            let Some(record) = host.get(id) else {
                return Ok(Value::undefined());
            };
            let mut events = Vec::new();
            {
                let rx = record
                    .events
                    .lock()
                    .expect("worker event receiver poisoned");
                while let Ok(event) = rx.try_recv() {
                    events.push(event);
                }
            }
            for event in events {
                let event_obj = match worker_event_to_value(ctx, event) {
                    Ok(value) => value.as_object().expect("event materializes to object"),
                    Err(err) => {
                        worker_event_to_value(ctx, WorkerEvent::MessageError(err.to_string()))?
                            .as_object()
                            .expect("messageerror materializes to object")
                    }
                };
                let ty = object::get(event_obj, ctx.heap(), "type")
                    .and_then(|value| value.as_string(ctx.heap()))
                    .map(|s| s.to_lossy_string(ctx.heap()))
                    .unwrap_or_default();
                if ty == "close" {
                    return Ok(Value::undefined());
                }
                dispatch_event_object(ctx, worker, event_obj)?;
            }
            Ok(Value::undefined())
        },
    )?;
    ctx.scope(|mut scope| {
        let worker = scope.value(worker_value);
        let poll = scope.value(poll);
        let set_interval = scope
            .global("setInterval")
            .ok_or_else(|| type_err("Worker", "setInterval is not installed".to_string()))?;
        let interval_ms = scope.number(1.0);
        let undefined = scope.undefined();
        let token = scope.call(set_interval, undefined, &[poll, interval_ms])?;
        scope.set(worker, "__otterPoll", token)?;
        Ok(scope.finish(worker))
    })
}

fn dispatch_event_object(
    ctx: &mut NativeCtx<'_>,
    worker: object::JsObject,
    event: object::JsObject,
) -> Result<(), NativeError> {
    let ty = object::get(event, ctx.heap(), "type")
        .and_then(|value| value.as_string(ctx.heap()))
        .map(|s| s.to_lossy_string(ctx.heap()))
        .unwrap_or_default();
    let handler_key = format!("on{ty}");
    let handler = object::get(worker, ctx.heap(), &handler_key).unwrap_or(Value::undefined());
    let listeners = worker_event_listeners(ctx, worker, &ty);
    ctx.scope(|mut scope| {
        let worker = scope.value(Value::object(worker));
        let event = scope.value(Value::object(event));
        let handler = scope.value(handler);
        let listeners: Vec<Local<'_>> = listeners
            .into_iter()
            .map(|listener| scope.value(listener))
            .collect();
        if scope.is_callable(handler) {
            scope.call(handler, worker, &[event])?;
        }
        for listener in listeners {
            if scope.is_callable(listener) {
                scope.call(listener, worker, &[event])?;
            }
        }
        Ok(())
    })
}

fn worker_listener_store(
    ctx: &mut NativeCtx<'_>,
    mut worker: object::JsObject,
) -> Result<object::JsObject, NativeError> {
    if let Some(store) =
        object::get(worker, ctx.heap(), "__otterListeners").and_then(|value| value.as_object())
    {
        return Ok(store);
    }
    let store = ctx.alloc_object()?;
    object::set(
        &mut worker,
        ctx.heap_mut(),
        "__otterListeners",
        Value::object(store),
    );
    Ok(store)
}

fn add_worker_event_listener(
    ctx: &mut NativeCtx<'_>,
    worker: object::JsObject,
    ty: &str,
    listener: Value,
) -> Result<(), NativeError> {
    let mut store = worker_listener_store(ctx, worker)?;
    let list = match object::get(store, ctx.heap(), ty).and_then(|value| value.as_array()) {
        Some(list) => list,
        None => {
            let list = ctx.array_from_elements(Vec::new())?;
            object::set(&mut store, ctx.heap_mut(), ty, Value::array(list));
            list
        }
    };
    let len = array::len(list, ctx.heap());
    for idx in 0..len {
        if array::get(list, ctx.heap(), idx) == listener {
            return Ok(());
        }
    }
    array::set(list, ctx.heap_mut(), len, listener).map_err(|err| {
        type_err(
            "Worker.addEventListener",
            format!(
                "listener allocation failed: requested {}, limit {}",
                err.requested_bytes(),
                err.heap_limit_bytes()
            ),
        )
    })
}

fn remove_worker_event_listener(
    ctx: &mut NativeCtx<'_>,
    worker: object::JsObject,
    ty: &str,
    listener: Value,
) -> Result<(), NativeError> {
    let mut store = worker_listener_store(ctx, worker)?;
    let Some(list) = object::get(store, ctx.heap(), ty).and_then(|value| value.as_array()) else {
        return Ok(());
    };
    let len = array::len(list, ctx.heap());
    let kept: Vec<Value> = (0..len)
        .map(|idx| array::get(list, ctx.heap(), idx))
        .filter(|value| *value != listener)
        .collect();
    let next = ctx.array_from_elements(kept)?;
    object::set(&mut store, ctx.heap_mut(), ty, Value::array(next));
    Ok(())
}

fn worker_event_listeners(ctx: &NativeCtx<'_>, worker: object::JsObject, ty: &str) -> Vec<Value> {
    let Some(store) =
        object::get(worker, ctx.heap(), "__otterListeners").and_then(|value| value.as_object())
    else {
        return Vec::new();
    };
    let Some(list) = object::get(store, ctx.heap(), ty).and_then(|value| value.as_array()) else {
        return Vec::new();
    };
    let len = array::len(list, ctx.heap());
    (0..len)
        .map(|idx| array::get(list, ctx.heap(), idx))
        .collect()
}

fn clear_worker_poll_timer(
    ctx: &mut NativeCtx<'_>,
    worker: object::JsObject,
) -> Result<(), NativeError> {
    let Some(token) = object::get(worker, ctx.heap(), "__otterPoll") else {
        return Ok(());
    };
    let clear_interval = {
        let (interp, _exec) = ctx.interp_mut_and_context();
        object::get(*interp.global_this(), interp.gc_heap(), "clearInterval")
    }
    .ok_or_else(|| {
        type_err(
            "Worker.terminate",
            "clearInterval is not installed".to_string(),
        )
    })?;
    let exec = ctx
        .interp_mut_and_context()
        .1
        .ok_or_else(|| type_err("Worker.terminate", "missing execution context".to_string()))?;
    let (interp, _) = ctx.interp_mut_and_context();
    interp
        .run_callable_sync(&exec, &clear_interval, Value::undefined(), smallvec![token])
        .map(|_| ())
        .map_err(vm_error_to_native)
}

fn worker_id_from_this(ctx: &NativeCtx<'_>, name: &'static str) -> Result<u64, NativeError> {
    let worker = ctx
        .this_value()
        .as_object()
        .ok_or_else(|| type_err(name, "invalid receiver".to_string()))?;
    worker_id_from_object(ctx, worker, name)
}

fn worker_id_from_object(
    ctx: &NativeCtx<'_>,
    worker: object::JsObject,
    name: &'static str,
) -> Result<u64, NativeError> {
    let value = object::get(worker, ctx.heap(), "__otterWorkerId")
        .ok_or_else(|| type_err(name, "missing worker id".to_string()))?;
    numeric_worker_id(&value)
}

fn vm_error_to_native(err: otter_vm::VmError) -> NativeError {
    match err {
        otter_vm::VmError::Uncaught => NativeError::Thrown {
            name: "Worker",
            message: err.to_string(),
        },
        other => type_err("Worker", other.to_string()),
    }
}

fn run_js_worker(
    _id: WorkerId,
    specifier: String,
    config: RuntimeConfig,
    rx: mpsc::Receiver<WorkerCommand>,
    tx: mpsc::Sender<WorkerEvent>,
    interrupt_tx: mpsc::SyncSender<crate::InterruptHandle>,
) {
    let mut runtime = match Runtime::from_config(config) {
        Ok(runtime) => runtime,
        Err(err) => {
            let _ = tx.send(WorkerEvent::Error(err.to_string()));
            return;
        }
    };
    runtime.set_allow_blocking_atomics_wait(true);
    let interrupt = runtime.interrupt_handle();
    let _ = interrupt_tx.send(interrupt.clone());
    let closed = Arc::new(std::sync::atomic::AtomicBool::new(false));
    if let Err(err) = install_worker_scope_natives(&mut runtime, tx.clone(), closed.clone()) {
        let _ = tx.send(WorkerEvent::Error(err.to_string()));
        return;
    }
    let context = match run_worker_entry(&mut runtime, &specifier) {
        Ok((_result, context)) => context,
        Err(err) => {
            let _ = tx.send(WorkerEvent::Error(err.to_string()));
            let _ = tx.send(WorkerEvent::Closed);
            return;
        }
    };
    while !closed.load(std::sync::atomic::Ordering::SeqCst) {
        match rx.recv_timeout(WORKER_COMMAND_POLL_INTERVAL) {
            Ok(WorkerCommand::Message(payload)) => {
                if let Err(err) = runtime.dispatch_worker_message_event(&context, |ctx| {
                    materialize_worker_payload(ctx, &payload)
                }) {
                    match err {
                        crate::MessageEventDispatchError::Materialize(err) => {
                            let _ = tx.send(WorkerEvent::MessageError(err.to_string()));
                        }
                        crate::MessageEventDispatchError::Handler(err) => {
                            let _ = tx.send(WorkerEvent::Error(err.to_string()));
                        }
                    }
                }
            }
            Ok(WorkerCommand::Shutdown) => break,
            Err(mpsc::RecvTimeoutError::Timeout) if interrupt.is_interrupted() => break,
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
    let _ = tx.send(WorkerEvent::Closed);
}

fn run_worker_entry(
    runtime: &mut Runtime,
    specifier: &str,
) -> Result<(ExecutionResult, otter_vm::ExecutionContext), OtterError> {
    let path = PathBuf::from(specifier);
    if path.exists() {
        runtime.run_file_with_context(path)
    } else {
        runtime.run_module_with_context(path)
    }
}

fn install_worker_scope_natives(
    runtime: &mut Runtime,
    tx: mpsc::Sender<WorkerEvent>,
    closed: Arc<std::sync::atomic::AtomicBool>,
) -> Result<(), OtterError> {
    let post_tx = tx.clone();
    let post: Arc<NativeFn> = Arc::new(move |ctx, args, _captures| {
        let transfers = parse_worker_transfer_list(args.get(1), ctx)?;
        let payload = clone_worker_value(
            args.first().unwrap_or(&Value::undefined()),
            ctx.heap(),
            &transfers,
        )?;
        detach_worker_transfers(&transfers, ctx.heap_mut());
        post_tx
            .send(WorkerEvent::Message(payload))
            .map_err(|_| type_err("postMessage", "parent channel is closed".to_string()))?;
        Ok(Value::undefined())
    });
    runtime.install_native_global_call("postMessage", 2, NativeCall::Dynamic(post))?;

    let close_flag = closed;
    let close: Arc<NativeFn> = Arc::new(move |_ctx, _args, _captures| {
        close_flag.store(true, std::sync::atomic::Ordering::SeqCst);
        Ok(Value::undefined())
    });
    runtime.install_native_global_call("close", 0, NativeCall::Dynamic(close))?;
    runtime.set_global("self", runtime.global_this_value());
    runtime.set_global("onmessage", Value::null());
    runtime.set_global("onerror", Value::null());
    Ok(())
}

fn worker_event_to_value(
    ctx: &mut NativeCtx<'_>,
    event: WorkerEvent,
) -> Result<Value, NativeError> {
    let mut object = ctx.alloc_object()?;
    match event {
        WorkerEvent::Message(payload) => {
            let data = materialize_worker_payload(ctx, &payload)?;
            let ty = string_value(ctx, "message")?;
            object::set(&mut object, ctx.heap_mut(), "type", ty);
            object::set(&mut object, ctx.heap_mut(), "data", data);
        }
        WorkerEvent::Error(message) => {
            let ty = string_value(ctx, "error")?;
            let message = string_value(ctx, &message)?;
            object::set(&mut object, ctx.heap_mut(), "type", ty);
            object::set(&mut object, ctx.heap_mut(), "message", message);
        }
        WorkerEvent::MessageError(message) => {
            let ty = string_value(ctx, "messageerror")?;
            let message = string_value(ctx, &message)?;
            object::set(&mut object, ctx.heap_mut(), "type", ty);
            object::set(&mut object, ctx.heap_mut(), "message", message);
        }
        WorkerEvent::Closed => {
            let ty = string_value(ctx, "close")?;
            object::set(&mut object, ctx.heap_mut(), "type", ty);
        }
    }
    Ok(Value::object(object))
}

fn parse_worker_transfer_list(
    value: Option<&Value>,
    ctx: &mut NativeCtx<'_>,
) -> Result<WorkerTransferList, NativeError> {
    let Some(value) = value else {
        return Ok(WorkerTransferList::default());
    };
    if value.is_undefined() || value.is_null() {
        return Ok(WorkerTransferList::default());
    }
    let array = value.as_array().ok_or_else(|| {
        type_err(
            "Worker.postMessage",
            "transferList must be an Array".to_string(),
        )
    })?;
    let mut out = WorkerTransferList::default();
    let len = array::len(array, ctx.heap());
    for idx in 0..len {
        let item = array::get(array, ctx.heap(), idx);
        let buffer = item.as_array_buffer().ok_or_else(|| {
            type_err(
                "Worker.postMessage",
                format!("transferList[{idx}] is not an ArrayBuffer"),
            )
        })?;
        if buffer.is_shared() {
            return Err(type_err(
                "Worker.postMessage",
                format!("transferList[{idx}] is a SharedArrayBuffer"),
            ));
        }
        if buffer.is_detached(ctx.heap()) {
            return Err(type_err(
                "Worker.postMessage",
                format!("transferList[{idx}] is detached"),
            ));
        }
        if !out.set.insert(buffer) {
            return Err(type_err(
                "Worker.postMessage",
                format!("duplicate transferable ArrayBuffer at transferList[{idx}]"),
            ));
        }
        out.buffers.push(buffer);
    }
    Ok(out)
}

fn detach_worker_transfers(transfers: &WorkerTransferList, heap: &mut otter_gc::GcHeap) {
    for buffer in &transfers.buffers {
        buffer.detach(heap);
    }
}

fn clone_worker_value(
    value: &Value,
    heap: &otter_gc::GcHeap,
    transfers: &WorkerTransferList,
) -> Result<WorkerPayload, NativeError> {
    let mut active = HashSet::new();
    clone_worker_value_inner(value, heap, transfers, "$".to_string(), 0, &mut active)
}

fn clone_worker_value_inner(
    value: &Value,
    heap: &otter_gc::GcHeap,
    transfers: &WorkerTransferList,
    path: String,
    depth: usize,
    active: &mut HashSet<RawGc>,
) -> Result<WorkerPayload, NativeError> {
    if depth > crate::structured_clone::DEFAULT_STRUCTURED_CLONE_MAX_DEPTH {
        return Err(type_err(
            "structuredClone",
            format!("depth limit exceeded at {path}"),
        ));
    }
    if value.is_undefined() {
        return Ok(WorkerPayload::Undefined);
    }
    if value.is_null() {
        return Ok(WorkerPayload::Null);
    }
    if let Some(b) = value.as_boolean() {
        return Ok(WorkerPayload::Boolean(b));
    }
    if let Some(n) = value.as_number() {
        return Ok(WorkerPayload::Number(n.into()));
    }
    if let Some(b) = value.as_big_int() {
        return Ok(WorkerPayload::BigInt(b.to_decimal_string(heap)));
    }
    if let Some(s) = value.as_string(heap) {
        return Ok(WorkerPayload::String(s.to_lossy_string(heap)));
    }
    if let Some(buf) = value.as_array_buffer() {
        if let Some(shared) = buf.as_shared_arc(heap) {
            return Ok(WorkerPayload::SharedArrayBuffer(shared));
        }
        if transfers.set.contains(&buf) && buf.is_detached(heap) {
            return Err(type_err(
                "structuredClone",
                format!("cannot transfer detached ArrayBuffer at {path}"),
            ));
        }
        return Ok(WorkerPayload::ArrayBuffer(
            buf.with_bytes(heap, |bytes| bytes.to_vec()),
        ));
    }
    if let Some(arr) = value.as_array() {
        if !active.insert(arr.raw()) {
            return Err(type_err(
                "structuredClone",
                format!("cycle detected at {path}"),
            ));
        }
        let len = array::len(arr, heap);
        let values: Vec<Value> = (0..len).map(|idx| array::get(arr, heap, idx)).collect();
        let mut cloned = Vec::with_capacity(values.len());
        for (idx, value) in values.iter().enumerate() {
            cloned.push(clone_worker_value_inner(
                value,
                heap,
                transfers,
                format!("{path}[{idx}]"),
                depth + 1,
                active,
            )?);
        }
        active.remove(&arr.raw());
        return Ok(WorkerPayload::Array(cloned));
    }
    if let Some(map) = value.as_map() {
        if !active.insert(map.raw()) {
            return Err(type_err(
                "structuredClone",
                format!("cycle detected at {path}"),
            ));
        }
        let entries = collections::map_entries(map, heap);
        let mut cloned = Vec::with_capacity(entries.len());
        for (idx, (key, value)) in entries.iter().enumerate() {
            cloned.push((
                clone_worker_value_inner(
                    key,
                    heap,
                    transfers,
                    format!("{path}<map-key:{idx}>"),
                    depth + 1,
                    active,
                )?,
                clone_worker_value_inner(
                    value,
                    heap,
                    transfers,
                    format!("{path}<map-value:{idx}>"),
                    depth + 1,
                    active,
                )?,
            ));
        }
        active.remove(&map.raw());
        return Ok(WorkerPayload::Map(cloned));
    }
    if let Some(set) = value.as_set() {
        if !active.insert(set.raw()) {
            return Err(type_err(
                "structuredClone",
                format!("cycle detected at {path}"),
            ));
        }
        let values = collections::set_values(set, heap);
        let mut cloned = Vec::with_capacity(values.len());
        for (idx, value) in values.iter().enumerate() {
            cloned.push(clone_worker_value_inner(
                value,
                heap,
                transfers,
                format!("{path}<set-value:{idx}>"),
                depth + 1,
                active,
            )?);
        }
        active.remove(&set.raw());
        return Ok(WorkerPayload::Set(cloned));
    }
    if let Some(obj) = value.as_object() {
        if !active.insert(obj.raw()) {
            return Err(type_err(
                "structuredClone",
                format!("cycle detected at {path}"),
            ));
        }
        let properties: Vec<(String, Value)> = object::with_properties(obj, heap, |properties| {
            properties
                .enumerable_data_iter()
                .map(|(key, value)| (key.to_string(), value))
                .collect()
        });
        let mut cloned = Vec::with_capacity(properties.len());
        for (key, value) in properties {
            cloned.push((
                key.clone(),
                clone_worker_value_inner(
                    &value,
                    heap,
                    transfers,
                    format!("{path}.{key}"),
                    depth + 1,
                    active,
                )?,
            ));
        }
        active.remove(&obj.raw());
        return Ok(WorkerPayload::Object(cloned));
    }
    Err(type_err(
        "structuredClone",
        format!("unsupported value at {path}: {:?}", value.kind()),
    ))
}

fn materialize_worker_payload(
    ctx: &mut NativeCtx<'_>,
    payload: &WorkerPayload,
) -> Result<Value, NativeError> {
    match payload {
        WorkerPayload::Undefined => Ok(Value::undefined()),
        WorkerPayload::Null => Ok(Value::null()),
        WorkerPayload::Boolean(value) => Ok(Value::boolean(*value)),
        WorkerPayload::Number(value) => Ok(Value::number(NumberValue::from_f64(value.as_f64()))),
        WorkerPayload::BigInt(value) => {
            let bigint = BigIntValue::from_decimal(ctx.heap_mut(), value)
                .ok_or_else(|| type_err("structuredClone", "invalid BigInt payload".to_string()))?
                .map_err(|err| {
                    type_err(
                        "structuredClone",
                        format!(
                            "BigInt allocation failed: requested {}, limit {}",
                            err.requested_bytes(),
                            err.heap_limit_bytes()
                        ),
                    )
                })?;
            Ok(Value::big_int(bigint))
        }
        WorkerPayload::String(value) => string_value(ctx, value),
        WorkerPayload::Array(values) => {
            let mut out = Vec::with_capacity(values.len());
            for value in values {
                out.push(materialize_worker_payload(ctx, value)?);
            }
            let array = ctx.array_from_elements(out)?;
            Ok(Value::array(array))
        }
        WorkerPayload::Object(properties) => {
            let mut object = ctx.alloc_object()?;
            for (key, value) in properties {
                let value = materialize_worker_payload(ctx, value)?;
                object::set(&mut object, ctx.heap_mut(), key, value);
            }
            Ok(Value::object(object))
        }
        WorkerPayload::Map(entries) => {
            let mut map = ctx.alloc_map()?;
            for (key, value) in entries {
                let key = materialize_worker_payload(ctx, key)?;
                let value = materialize_worker_payload(ctx, value)?;
                ctx.map_set(&mut map, key, value)?;
            }
            Ok(Value::map(map))
        }
        WorkerPayload::Set(values) => {
            let mut set = ctx.alloc_set()?;
            for value in values {
                let value = materialize_worker_payload(ctx, value)?;
                ctx.set_add(&mut set, value)?;
            }
            Ok(Value::set(set))
        }
        WorkerPayload::ArrayBuffer(bytes) => {
            let buffer = ctx.array_buffer_from_bytes(bytes.to_vec())?;
            Ok(Value::array_buffer(buffer))
        }
        WorkerPayload::SharedArrayBuffer(body) => {
            let buffer =
                JsArrayBuffer::from_shared_arc(ctx.heap_mut(), body.clone()).map_err(|err| {
                    type_err(
                        "structuredClone",
                        format!(
                            "SharedArrayBuffer allocation failed: requested {}, limit {}",
                            err.requested_bytes(),
                            err.heap_limit_bytes()
                        ),
                    )
                })?;
            Ok(Value::array_buffer(buffer))
        }
    }
}

fn value_to_string(ctx: &mut NativeCtx<'_>, value: &Value) -> Result<String, NativeError> {
    if let Some(s) = value.as_string(ctx.heap()) {
        Ok(s.to_lossy_string(ctx.heap()))
    } else if value.is_undefined() {
        Ok("undefined".to_string())
    } else {
        Ok(value.display_string(ctx.heap()))
    }
}

fn string_value(ctx: &mut NativeCtx<'_>, value: &str) -> Result<Value, NativeError> {
    Ok(Value::string(
        JsString::from_str(value, ctx.heap_mut())
            .map_err(|err| type_err("Worker", err.to_string()))?,
    ))
}

fn numeric_worker_id(value: &Value) -> Result<u64, NativeError> {
    match value.as_number() {
        Some(n) if n.as_f64().is_finite() && n.as_f64() >= 1.0 => Ok(n.as_f64() as u64),
        _ => Err(type_err("Worker", "invalid worker id".to_string())),
    }
}

fn type_err(name: &'static str, reason: String) -> NativeError {
    NativeError::TypeError { name, reason }
}

/// Sendable handle to one worker isolate.
#[derive(Clone, Debug)]
pub struct Worker {
    id: WorkerId,
    handle: RuntimeHandle,
}

/// Worker shutdown / leak diagnostic snapshot.
#[derive(Debug, Clone)]
pub struct WorkerShutdownReport {
    /// Worker id.
    pub worker_id: WorkerId,
    /// Runtime handle references still pointing at this isolate.
    pub live_runtime_handles: usize,
    /// Commands queued from the handle side at report time.
    pub queued_messages: usize,
    /// Runtime activity at the time of the report.
    pub activity: RuntimeActivityStats,
    /// Transferable resources still owned by the worker boundary.
    ///
    /// This is always zero until message ports and ArrayBuffer
    /// transfer ownership land; keeping it in the report now fixes
    /// the public diagnostic shape.
    pub leaked_transferables: usize,
}

impl WorkerShutdownReport {
    /// `true` if any shutdown-relevant work/resource is still live.
    #[must_use]
    pub fn has_leaks(&self) -> bool {
        self.activity.queued_commands > 0
            || self.queued_messages > 0
            || self.live_runtime_handles > 1
            || self.activity.pending_ref_host_ops > 0
            || self.activity.pending_unref_host_ops > 0
            || self.activity.pending_ref_timers > 0
            || self.activity.pending_unref_timers > 0
            || self.activity.pending_dynamic_module_jobs > 0
            || self.leaked_transferables > 0
    }
}

impl Worker {
    /// Start configuring a worker isolate.
    #[must_use]
    pub fn builder() -> WorkerBuilder {
        WorkerBuilder::default()
    }

    /// Build a worker with default runtime configuration.
    ///
    /// # Errors
    /// Returns [`OtterError`] if the runtime isolate cannot start.
    pub fn new() -> Result<Self, OtterError> {
        Self::builder().build()
    }

    /// Host-side worker id.
    #[must_use]
    pub const fn id(&self) -> WorkerId {
        self.id
    }

    /// Run a file from disk on this worker isolate.
    ///
    /// # Errors
    /// See [`OtterError`].
    pub async fn run_file(&self, path: impl AsRef<Path>) -> Result<ExecutionResult, OtterError> {
        self.handle.run_file(path.as_ref().to_path_buf()).await
    }

    /// Run an ES module entry file on this worker isolate.
    ///
    /// # Errors
    /// See [`OtterError`].
    pub async fn run_module(&self, path: impl AsRef<Path>) -> Result<ExecutionResult, OtterError> {
        self.handle.run_module(path.as_ref().to_path_buf()).await
    }

    /// Run JavaScript source on this worker isolate.
    ///
    /// # Errors
    /// See [`OtterError`].
    pub async fn run_script(&self, source: &str) -> Result<ExecutionResult, OtterError> {
        self.handle
            .run_script(
                SourceInput::from_javascript(source).with_top_level_await(),
                worker_specifier(self.id),
            )
            .await
    }

    /// Run TypeScript source on this worker isolate.
    ///
    /// # Errors
    /// See [`OtterError`].
    pub async fn run_typescript(&self, source: &str) -> Result<ExecutionResult, OtterError> {
        self.handle
            .run_script(
                SourceInput::from_typescript(source).with_top_level_await(),
                worker_specifier(self.id),
            )
            .await
    }

    /// Evaluate JavaScript source on this worker isolate.
    ///
    /// # Errors
    /// See [`OtterError`].
    pub async fn eval(&self, source: &str) -> Result<ExecutionResult, OtterError> {
        self.handle
            .eval(SourceInput::from_javascript(source).with_top_level_await())
            .await
    }

    /// Validate that a message already crossed the structured-clone
    /// boundary. Full JS delivery lands with message ports in a later
    /// task-92 slice.
    #[must_use]
    pub fn accepts_message(&self, _message: &StructuredCloneValue) -> bool {
        true
    }

    /// Validate transfer-list metadata for a future message send.
    #[must_use]
    pub fn accepts_transfer_list(&self, transfers: &StructuredCloneTransferList) -> bool {
        transfers.validate().is_ok()
    }

    /// Cooperative cancellation for this worker isolate.
    pub fn interrupt(&self) {
        self.handle.interrupt();
    }

    /// Snapshot worker activity counters.
    #[must_use]
    pub fn activity_stats(&self) -> RuntimeActivityStats {
        self.handle.activity_stats()
    }

    /// Snapshot shutdown diagnostics without tearing down the worker.
    #[must_use]
    pub fn shutdown_report(&self) -> WorkerShutdownReport {
        let activity = self.activity_stats();
        WorkerShutdownReport {
            worker_id: self.id,
            live_runtime_handles: self.handle.live_handle_count(),
            queued_messages: activity.queued_commands,
            activity,
            leaked_transferables: 0,
        }
    }

    /// Drop down to the sendable runtime handle.
    #[must_use]
    pub fn handle(&self) -> &RuntimeHandle {
        &self.handle
    }
}

/// Builder for one worker isolate.
#[derive(Debug, Clone, Default)]
pub struct WorkerBuilder {
    runtime: RuntimeBuilder,
}

impl WorkerBuilder {
    /// Replace the capability set.
    #[must_use]
    pub fn capabilities(mut self, caps: CapabilitySet) -> Self {
        self.runtime = self.runtime.capabilities(caps);
        self
    }

    /// Hard heap cap. `0` disables the cap.
    #[must_use]
    pub fn max_heap_bytes(mut self, bytes: u64) -> Self {
        self.runtime = self.runtime.max_heap_bytes(bytes);
        self
    }

    /// Per-command timeout. `Duration::ZERO` disables the timeout.
    #[must_use]
    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.runtime = self.runtime.timeout(timeout);
        self
    }

    /// JS call-stack depth cap.
    #[must_use]
    pub fn max_stack_depth(mut self, depth: u32) -> Self {
        self.runtime = self.runtime.max_stack_depth(depth);
        self
    }

    /// Override the module-loader configuration.
    #[must_use]
    pub fn module_loader(mut self, loader: module_loader::LoaderConfig) -> Self {
        self.runtime = self.runtime.module_loader(loader);
        self
    }

    /// Construct a worker isolate.
    ///
    /// # Errors
    /// Returns [`OtterError`] when config validation or isolate
    /// startup fails.
    pub fn build(self) -> Result<Worker, OtterError> {
        let id = WorkerId(NEXT_WORKER_ID.fetch_add(1, Ordering::Relaxed));
        Ok(Worker {
            id,
            handle: self.runtime.build_handle()?,
        })
    }
}

/// Round-robin pool of independent worker isolates.
#[derive(Clone, Debug)]
pub struct OtterPool {
    workers: Arc<[Worker]>,
    next: Arc<AtomicUsize>,
}

impl OtterPool {
    /// Start configuring an isolate pool.
    #[must_use]
    pub fn builder() -> OtterPoolBuilder {
        OtterPoolBuilder::default()
    }

    /// Number of workers in the pool.
    #[must_use]
    pub fn len(&self) -> usize {
        self.workers.len()
    }

    /// `true` when the pool has no workers.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.workers.is_empty()
    }

    /// Borrow the worker slice for diagnostics/tests.
    #[must_use]
    pub fn workers(&self) -> &[Worker] {
        &self.workers
    }

    /// Snapshot shutdown diagnostics for every worker.
    #[must_use]
    pub fn shutdown_reports(&self) -> Vec<WorkerShutdownReport> {
        self.workers.iter().map(Worker::shutdown_report).collect()
    }

    /// Pick the next worker using round-robin routing.
    #[must_use]
    pub fn next_worker(&self) -> Worker {
        let idx = self.next.fetch_add(1, Ordering::Relaxed) % self.workers.len();
        self.workers[idx].clone()
    }

    /// Run JavaScript on the next worker.
    ///
    /// # Errors
    /// See [`OtterError`].
    pub async fn run_script(&self, source: &str) -> Result<ExecutionResult, OtterError> {
        self.next_worker().run_script(source).await
    }

    /// Run TypeScript on the next worker.
    ///
    /// # Errors
    /// See [`OtterError`].
    pub async fn run_typescript(&self, source: &str) -> Result<ExecutionResult, OtterError> {
        self.next_worker().run_typescript(source).await
    }

    /// Run a file on the next worker.
    ///
    /// # Errors
    /// See [`OtterError`].
    pub async fn run_file(&self, path: impl AsRef<Path>) -> Result<ExecutionResult, OtterError> {
        self.next_worker().run_file(path.as_ref()).await
    }
}

/// Builder for [`OtterPool`].
#[derive(Debug, Clone)]
pub struct OtterPoolBuilder {
    runtime: RuntimeBuilder,
    workers: usize,
}

impl Default for OtterPoolBuilder {
    fn default() -> Self {
        Self {
            runtime: RuntimeBuilder::default(),
            workers: 1,
        }
    }
}

impl OtterPoolBuilder {
    /// Number of worker isolates to spawn. Values below one are
    /// rejected at [`Self::build`].
    #[must_use]
    pub fn workers(mut self, workers: usize) -> Self {
        self.workers = workers;
        self
    }

    /// Replace the capability set for every worker.
    #[must_use]
    pub fn capabilities(mut self, caps: CapabilitySet) -> Self {
        self.runtime = self.runtime.capabilities(caps);
        self
    }

    /// Hard heap cap per worker. `0` disables each cap.
    #[must_use]
    pub fn max_heap_bytes(mut self, bytes: u64) -> Self {
        self.runtime = self.runtime.max_heap_bytes(bytes);
        self
    }

    /// Per-command timeout for every worker.
    #[must_use]
    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.runtime = self.runtime.timeout(timeout);
        self
    }

    /// JS call-stack depth cap for every worker.
    #[must_use]
    pub fn max_stack_depth(mut self, depth: u32) -> Self {
        self.runtime = self.runtime.max_stack_depth(depth);
        self
    }

    /// Override the module-loader configuration for every worker.
    #[must_use]
    pub fn module_loader(mut self, loader: module_loader::LoaderConfig) -> Self {
        self.runtime = self.runtime.module_loader(loader);
        self
    }

    /// Construct an isolate pool.
    ///
    /// # Errors
    /// Returns [`OtterError`] when config validation or isolate
    /// startup fails. A zero-worker pool is rejected as a config
    /// error because routing could not make progress.
    pub fn build(self) -> Result<OtterPool, OtterError> {
        if self.workers == 0 {
            return Err(OtterError::Config {
                reason: crate::ConfigError::ConflictingCapabilities {
                    message: "worker pool must contain at least one worker".to_string(),
                },
            });
        }

        let mut workers = Vec::with_capacity(self.workers);
        for _ in 0..self.workers {
            workers.push(
                WorkerBuilder {
                    runtime: self.runtime.clone(),
                }
                .build()?,
            );
        }

        Ok(OtterPool {
            workers: workers.into(),
            next: Arc::new(AtomicUsize::new(0)),
        })
    }
}

fn worker_specifier(id: WorkerId) -> String {
    format!("<worker:{}>", id.get())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Otter;
    use std::fs;

    fn assert_send_sync_static<T: Send + Sync + 'static>() {}

    #[test]
    fn worker_handles_are_send_sync_static() {
        assert_send_sync_static::<Worker>();
        assert_send_sync_static::<OtterPool>();
        assert_send_sync_static::<WorkerShutdownReport>();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn two_workers_run_concurrently_with_separate_globals() {
        let left = Worker::new().unwrap();
        let right = Worker::new().unwrap();

        let (left_result, right_result) = tokio::join!(
            left.run_script("globalThis.workerSlot = 7; workerSlot"),
            right.run_script("typeof globalThis.workerSlot"),
        );

        assert_eq!(left_result.unwrap().completion_string(), "7");
        assert_eq!(right_result.unwrap().completion_string(), "undefined");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn pool_routes_round_robin_across_isolates() {
        let pool = OtterPool::builder().workers(2).build().unwrap();

        let first = pool.next_worker();
        let second = pool.next_worker();
        let third = pool.next_worker();

        assert_eq!(pool.len(), 2);
        assert_ne!(first.id(), second.id());
        assert_eq!(first.id(), third.id());

        first.run_script("globalThis.onlyFirst = 1").await.unwrap();
        let second_read = second
            .run_script("typeof globalThis.onlyFirst")
            .await
            .unwrap();

        assert_eq!(second_read.completion_string(), "undefined");
    }

    #[test]
    fn zero_worker_pool_is_rejected() {
        let err = OtterPool::builder().workers(0).build().unwrap_err();
        assert!(matches!(err, OtterError::Config { .. }));
    }

    #[test]
    fn worker_message_boundary_accepts_only_structured_clone_payload() {
        let worker = Worker::new().unwrap();
        let message = StructuredCloneValue::Object(vec![crate::StructuredCloneProperty {
            key: "ok".to_string(),
            value: StructuredCloneValue::Boolean(true),
        }]);
        let transfers = StructuredCloneTransferList::empty();

        assert!(worker.accepts_message(&message));
        assert!(worker.accepts_transfer_list(&transfers));
    }

    #[test]
    fn shutdown_report_marks_pending_timer_as_leak() {
        use crate::event_loop::TimerRequest;

        let worker = Worker::new().unwrap();
        let token = worker.handle().schedule_timer(TimerRequest {
            delay: Duration::from_secs(60),
            repeat: None,
        });

        let report = worker.shutdown_report();

        assert_eq!(report.worker_id, worker.id());
        assert_eq!(report.live_runtime_handles, 1);
        assert_eq!(report.queued_messages, report.activity.queued_commands);
        assert!(report.has_leaks());
        assert_eq!(report.activity.pending_ref_timers, 1);
        assert!(worker.handle().cancel_timer(token));
    }

    #[test]
    fn shutdown_report_tracks_live_handle_refs() {
        let worker = Worker::new().unwrap();
        let cloned = worker.clone();

        let report = worker.shutdown_report();

        assert_eq!(report.live_runtime_handles, 2);
        assert!(report.has_leaks());
        drop(cloned);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn global_worker_receives_worker_message() {
        let dir = tempfile::tempdir().unwrap();
        let worker_path = dir.path().join("worker.js");
        fs::write(&worker_path, "postMessage('ready');").unwrap();
        let entry = dir.path().join("entry.js");
        fs::write(
            &entry,
            format!(
                r#"
                let got = "pending";
                const w = new Worker({:?});
                w.onerror = (event) => {{
                  got = "ERR:" + event.message;
                  w.terminate();
                }};
                w.onmessage = (event) => {{
                  got = event.data;
                  w.terminate();
                }};
                setTimeout(() => {{
                  if (got !== "ready") throw "bad worker message: " + got;
                }}, 20);
                "#,
                worker_path.to_string_lossy()
            ),
        )
        .unwrap();

        let otter = Otter::builder()
            .capabilities(CapabilitySet::allow_all())
            .build()
            .unwrap();
        otter.run_file(&entry).await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn global_worker_event_listener_surface_dispatches_and_removes() {
        let dir = tempfile::tempdir().unwrap();
        let worker_path = dir.path().join("worker.js");
        fs::write(&worker_path, "postMessage('ready');").unwrap();
        let entry = dir.path().join("entry.js");
        fs::write(
            &entry,
            format!(
                r#"
                let count = 0;
                let got = "pending";
                const w = new Worker({:?});
                const removed = () => {{ count += 100; }};
                w.addEventListener("message", removed);
                w.removeEventListener("message", removed);
                w.onmessage = () => {{ count += 10; }};
                w.addEventListener("message", (event) => {{
                  count += 1;
                  got = event.data;
                  w.terminate();
                }});
                setTimeout(() => {{
                  if (got !== "ready") throw "listener did not receive message";
                  if (count !== 11) throw "bad listener count: " + count;
                }}, 20);
                "#,
                worker_path.to_string_lossy()
            ),
        )
        .unwrap();

        let otter = Otter::builder()
            .capabilities(CapabilitySet::allow_all())
            .build()
            .unwrap();
        otter.run_file(&entry).await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn global_worker_parent_to_worker_message() {
        let dir = tempfile::tempdir().unwrap();
        let worker_path = dir.path().join("worker.js");
        fs::write(
            &worker_path,
            "globalThis.onmessage = (event) => postMessage(event.data.answer + 1);",
        )
        .unwrap();
        let entry = dir.path().join("entry.js");
        fs::write(
            &entry,
            format!(
                r#"
                let got = 0;
                const w = new Worker({:?});
                w.onerror = (event) => {{
                  got = -1;
                  w.terminate();
                }};
                w.onmessage = (event) => {{
                  got = event.data;
                  w.terminate();
                }};
                w.postMessage({{ answer: 41 }});
                setTimeout(() => {{
                  if (got !== 42) throw "bad response: " + got;
                }}, 20);
                "#,
                worker_path.to_string_lossy()
            ),
        )
        .unwrap();

        let otter = Otter::builder()
            .capabilities(CapabilitySet::allow_all())
            .build()
            .unwrap();
        otter.run_file(&entry).await.unwrap();
    }

    /// Run with `OTTER_GC_STRESS=full` to force relocation after handler
    /// lookup, payload materialization, and event allocation.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn worker_message_event_rooting_survives_gc_relocation() {
        let dir = tempfile::tempdir().unwrap();
        let worker_path = dir.path().join("worker.js");
        fs::write(
            &worker_path,
            r#"
            globalThis.onmessage = (event) => {
              if (event.type !== "message") throw "bad event type";
              postMessage(event.type + ":" + event.data);
            };
            "#,
        )
        .unwrap();
        let entry = dir.path().join("entry.js");
        fs::write(
            &entry,
            format!(
                r#"
                let got = "pending";
                const w = new Worker({:?});
                w.onerror = (event) => {{
                  got = "ERR:" + event.message;
                  w.terminate();
                }};
                w.onmessage = (event) => {{
                  got = event.data;
                  w.terminate();
                }};
                w.postMessage("rooted-payload");
                setTimeout(() => {{
                  if (got !== "message:rooted-payload") {{
                    throw "bad rooted worker event: " + got;
                  }}
                }}, 20);
                "#,
                worker_path.to_string_lossy()
            ),
        )
        .unwrap();

        let otter = Otter::builder()
            .capabilities(CapabilitySet::allow_all())
            .build()
            .unwrap();
        otter.run_file(&entry).await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn global_worker_terminate_interrupts_infinite_loop() {
        let dir = tempfile::tempdir().unwrap();
        let worker_path = dir.path().join("worker.js");
        fs::write(&worker_path, "while (true) {}").unwrap();
        let entry = dir.path().join("entry.js");
        fs::write(
            &entry,
            format!(
                r#"
                const w = new Worker({:?});
                w.terminate();
                "terminated";
                "#,
                worker_path.to_string_lossy()
            ),
        )
        .unwrap();

        let otter = Otter::builder()
            .capabilities(CapabilitySet::allow_all())
            .build()
            .unwrap();
        otter.run_file(&entry).await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn global_worker_terminate_interrupts_atomics_wait() {
        let dir = tempfile::tempdir().unwrap();
        let worker_path = dir.path().join("worker.js");
        fs::write(
            &worker_path,
            r#"
            globalThis.onmessage = (event) => {
              const view = new Int32Array(event.data);
              Atomics.wait(view, 0, 0);
              postMessage("after-wait");
            };
            "#,
        )
        .unwrap();
        let entry = dir.path().join("entry.js");
        fs::write(
            &entry,
            format!(
                r#"
                const sab = new SharedArrayBuffer(4);
                let got = "pending";
                const w = new Worker({:?});
                w.onmessage = (event) => {{
                  got = event.data;
                }};
                w.onerror = () => {{
                  got = "interrupted";
                }};
                w.postMessage(sab);
                setTimeout(() => w.terminate(), 5);
                setTimeout(() => {{
                  if (got === "after-wait") throw "Atomics.wait was not cancelled";
                }}, 20);
                "#,
                worker_path.to_string_lossy()
            ),
        )
        .unwrap();

        let otter = Otter::builder()
            .capabilities(CapabilitySet::allow_all())
            .build()
            .unwrap();
        otter.run_file(&entry).await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn global_worker_shares_shared_array_buffer() {
        let dir = tempfile::tempdir().unwrap();
        let worker_path = dir.path().join("worker.js");
        fs::write(
            &worker_path,
            r#"
            globalThis.onmessage = (event) => {
              const view = new Int32Array(event.data);
              Atomics.store(view, 0, 7);
              Atomics.notify(view, 0, 1);
              postMessage("stored");
            };
            "#,
        )
        .unwrap();
        let entry = dir.path().join("entry.js");
        fs::write(
            &entry,
            format!(
                r#"
                const sab = new SharedArrayBuffer(4);
                const view = new Int32Array(sab);
                let got = "pending";
                const w = new Worker({:?});
                w.onerror = (event) => {{
                  got = "ERR:" + event.message;
                  w.terminate();
                }};
                w.onmessage = (event) => {{
                  got = event.data;
                  w.terminate();
                }};
                w.postMessage(sab);
                setTimeout(() => {{
                  if (got !== "stored") throw "bad response: " + got;
                  if (Atomics.load(view, 0) !== 7) throw "shared write missing";
                }}, 20);
                "#,
                worker_path.to_string_lossy()
            ),
        )
        .unwrap();

        let otter = Otter::builder()
            .capabilities(CapabilitySet::allow_all())
            .build()
            .unwrap();
        otter.run_file(&entry).await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn global_worker_transfers_array_buffer_and_detaches_sender() {
        let dir = tempfile::tempdir().unwrap();
        let worker_path = dir.path().join("worker.js");
        fs::write(
            &worker_path,
            r#"
            globalThis.onmessage = (event) => {
              const view = new Uint8Array(event.data);
              postMessage([event.data.byteLength, view[0], view[1], view[2]]);
            };
            "#,
        )
        .unwrap();
        let entry = dir.path().join("entry.js");
        fs::write(
            &entry,
            format!(
                r#"
                const buffer = new ArrayBuffer(3);
                const view = new Uint8Array(buffer);
                view[0] = 4;
                view[1] = 5;
                view[2] = 6;
                let got = null;
                const w = new Worker({:?});
                w.onerror = (event) => {{
                  got = "ERR:" + event.message;
                  w.terminate();
                }};
                w.onmessage = (event) => {{
                  got = event.data.join(",");
                  w.terminate();
                }};
                w.postMessage(buffer, [buffer]);
                if (buffer.byteLength !== 0) throw "sender buffer was not detached";
                setTimeout(() => {{
                  if (got !== "3,4,5,6") throw "bad transfer result: " + got;
                }}, 20);
                "#,
                worker_path.to_string_lossy()
            ),
        )
        .unwrap();

        let otter = Otter::builder()
            .capabilities(CapabilitySet::allow_all())
            .build()
            .unwrap();
        otter.run_file(&entry).await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn global_worker_post_message_rejects_unsupported_value() {
        let dir = tempfile::tempdir().unwrap();
        let worker_path = dir.path().join("worker.js");
        fs::write(&worker_path, "globalThis.onmessage = () => {};").unwrap();
        let entry = dir.path().join("entry.js");
        fs::write(
            &entry,
            format!(
                r#"
                const w = new Worker({:?});
                let threw = false;
                try {{
                  w.postMessage(() => 1);
                }} catch (err) {{
                  threw = String(err).includes("structuredClone");
                }}
                w.terminate();
                if (!threw) throw "unsupported value did not throw";
                "#,
                worker_path.to_string_lossy()
            ),
        )
        .unwrap();

        let otter = Otter::builder()
            .capabilities(CapabilitySet::allow_all())
            .build()
            .unwrap();
        otter.run_file(&entry).await.unwrap();
    }
}
