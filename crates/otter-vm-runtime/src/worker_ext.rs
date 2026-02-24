//! Web Worker JS API extension.
//!
//! Provides the `Worker` global constructor (Web Workers API, NOT Node.js worker_threads).
//! Each Worker runs in its own OS thread with its own Isolate and event loop.
//!
//! ## API
//! ```js
//! // Main thread
//! const worker = new Worker("worker.js");
//! worker.onmessage = (event) => console.log(event.data);
//! worker.postMessage("hello");
//! worker.terminate();
//!
//! // Inside worker.js
//! self.onmessage = (event) => {
//!     self.postMessage("echo: " + event.data);
//! };
//! ```

use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::Arc;

use otter_macros::{js_class, js_constructor, js_method};
use otter_vm_core::context::NativeContext;
use otter_vm_core::error::VmError;
use otter_vm_core::gc::GcRef;
use otter_vm_core::object::{JsObject, PropertyKey};
use otter_vm_core::value::Value;

use crate::extension_v2::{OtterExtension, Profile};
use crate::registration::RegistrationContext;
use crate::worker::{Worker, WorkerContext, WorkerMessage};

// ---------------------------------------------------------------------------
// Thread-local worker registry
// ---------------------------------------------------------------------------

// Marker property on Worker JS objects.
const WORKER_ID_KEY: &str = "__worker_id__";

thread_local! {
    // Active workers keyed by worker ID.
    // Thread-local because each Otter isolate runs on a single thread.
    static WORKER_REGISTRY: RefCell<HashMap<u64, Arc<Worker>>> = RefCell::new(HashMap::new());
}

// ---------------------------------------------------------------------------
// OtterExtension
// ---------------------------------------------------------------------------

/// Web Workers API extension.
///
/// Registers the `Worker` global constructor on `globalThis`.
pub struct WorkerExtension;

impl OtterExtension for WorkerExtension {
    fn name(&self) -> &str {
        "worker"
    }

    fn profiles(&self) -> &[Profile] {
        static P: [Profile; 1] = [Profile::Full];
        &P
    }

    fn install(&self, ctx: &mut RegistrationContext) -> Result<(), VmError> {
        // Build Worker constructor + prototype using BuiltInBuilder
        let (_ctor_name, ctor_fn, ctor_len) = JsWorker::constructor_decl();
        let (post_msg_name, post_msg_fn, post_msg_len) = JsWorker::post_message_decl();
        let (terminate_name, terminate_fn, terminate_len) = JsWorker::terminate_decl();

        let ctor_val = ctx
            .builtin_fresh("Worker")
            .constructor_fn(move |this, args, ncx| ctor_fn(this, args, ncx), ctor_len)
            .method_native(post_msg_name, post_msg_fn, post_msg_len)
            .method_native(terminate_name, terminate_fn, terminate_len)
            .build();

        ctx.global_value("Worker", ctor_val);

        Ok(())
    }
}

/// Create a boxed Worker extension instance for registration.
pub fn worker_extension() -> Box<dyn OtterExtension> {
    Box::new(WorkerExtension)
}

// ---------------------------------------------------------------------------
// JS class via #[js_class] macro
// ---------------------------------------------------------------------------

#[allow(missing_docs)]
#[js_class(name = "Worker")]
pub struct JsWorker;

#[js_class]
impl JsWorker {
    /// `new Worker(scriptURL)` — create a new worker thread.
    #[js_constructor(name = "Worker", length = 1)]
    pub fn constructor(
        this: &Value,
        args: &[Value],
        ncx: &mut NativeContext,
    ) -> Result<Value, VmError> {
        if !ncx.is_construct() {
            return Err(VmError::type_error("Constructor Worker requires 'new'"));
        }

        let script_src = match args.first() {
            Some(val) if val.is_string() => val.as_string().unwrap().as_str().to_string(),
            Some(val) if val.is_number() => {
                format!("{}", val.as_number().unwrap())
            }
            Some(_) => {
                return Err(VmError::type_error(
                    "Worker constructor argument must be a string",
                ));
            }
            None => {
                return Err(VmError::type_error(
                    "Worker constructor requires a script URL argument",
                ));
            }
        };

        let main_mm = ncx.memory_manager().clone();
        let worker_mm = Arc::new(otter_vm_core::MemoryManager::new(
            512 * 1024 * 1024, // 512 MB default
        ));

        let script = script_src.clone();

        let worker = Worker::new(
            move |wctx: WorkerContext| {
                worker_thread_main(wctx, &script);
            },
            worker_mm,
            main_mm,
        );

        let worker_id = worker.id();

        // Store in thread-local registry
        WORKER_REGISTRY.with(|reg| {
            reg.borrow_mut().insert(worker_id, worker);
        });

        // Set ID on `this` object
        if let Some(obj) = this.as_object() {
            let _ = obj.set(
                PropertyKey::string(WORKER_ID_KEY),
                Value::number(worker_id as f64),
            );
        }

        Ok(Value::undefined())
    }

    /// `worker.postMessage(data)` — send a message to the worker.
    #[js_method(name = "postMessage", length = 1)]
    pub fn post_message(
        this: &Value,
        args: &[Value],
        _ncx: &mut NativeContext,
    ) -> Result<Value, VmError> {
        let worker_id = get_worker_id(this)?;
        let data = args.first().cloned().unwrap_or(Value::undefined());

        WORKER_REGISTRY.with(|reg| {
            let registry = reg.borrow();
            let worker = registry
                .get(&worker_id)
                .ok_or_else(|| VmError::type_error("Worker has been terminated"))?;

            worker
                .post_message(data)
                .map_err(|e| VmError::type_error(format!("postMessage failed: {}", e)))?;

            Ok(Value::undefined())
        })
    }

    /// `worker.terminate()` — terminate the worker.
    #[js_method(name = "terminate", length = 0)]
    pub fn terminate(
        this: &Value,
        _args: &[Value],
        _ncx: &mut NativeContext,
    ) -> Result<Value, VmError> {
        let worker_id = get_worker_id(this)?;

        WORKER_REGISTRY.with(|reg| {
            if let Some(worker) = reg.borrow_mut().remove(&worker_id) {
                worker.terminate();
            }
        });

        Ok(Value::undefined())
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Extract the worker ID from a Worker JS object.
fn get_worker_id(this: &Value) -> Result<u64, VmError> {
    let obj = this
        .as_object()
        .ok_or_else(|| VmError::type_error("Worker method called on non-object"))?;

    obj.get(&PropertyKey::string(WORKER_ID_KEY))
        .and_then(|v| v.as_number())
        .map(|n| n as u64)
        .ok_or_else(|| VmError::type_error("Worker method called on non-Worker object"))
}

// ---------------------------------------------------------------------------
// Worker thread entry point
// ---------------------------------------------------------------------------

/// Entry point for the worker thread.
///
/// Creates a fresh `Otter` runtime, sets up worker-specific globals
/// (`self.postMessage`, `self.onmessage`), executes the script, then
/// enters a message receive loop.
fn worker_thread_main(wctx: WorkerContext, script_src: &str) {
    use crate::otter_runtime::Otter;

    // Create a fresh Otter runtime on this thread.
    // New Isolate, new GC, new string table — fully independent.
    let mut engine = Otter::new();

    // Shared state for the onmessage handler
    let on_message_handler: Arc<parking_lot::Mutex<Option<Value>>> =
        Arc::new(parking_lot::Mutex::new(None));

    // Install worker-specific globals
    install_worker_globals(&mut engine, &on_message_handler);

    // Execute the worker script
    let result = engine.eval_sync(script_src);
    if let Err(e) = result {
        wctx.post_error(format!("Worker script error: {}", e));
        return;
    }

    // Message receive loop
    while !wctx.is_terminated() {
        match wctx.recv() {
            Some(WorkerMessage::Data(value)) => {
                let handler = on_message_handler.lock().clone();
                if let Some(handler_fn) = handler {
                    dispatch_message(&mut engine, &handler_fn, value);
                }
            }
            Some(WorkerMessage::Terminate) => {
                wctx.mark_terminated();
                break;
            }
            Some(WorkerMessage::Error(err)) => {
                eprintln!("Worker error: {}", err);
            }
            None => break, // Channel closed
        }
    }
}

/// Install worker-specific globals (`self`, `postMessage`, `onmessage`) on globalThis.
fn install_worker_globals(
    engine: &mut crate::otter_runtime::Otter,
    on_message_handler: &Arc<parking_lot::Mutex<Option<Value>>>,
) {
    let runtime = engine.isolate.runtime();
    let mm = runtime.memory_manager().clone();
    let fn_proto = runtime.function_prototype();
    let global = {
        let ctx = runtime.create_context();
        ctx.global()
    };

    // self === globalThis (Web Worker convention)
    let _ = global.set(PropertyKey::string("self"), Value::object(global));

    // self.postMessage(data) — placeholder for now.
    // Full implementation requires exposing WorkerContext's sender channel.
    let post_msg_fn = Value::native_function_with_proto(
        |_this, _args, _ncx| {
            // TODO: Wire to WorkerContext's tx channel
            Ok(Value::undefined())
        },
        mm.clone(),
        fn_proto,
    );
    let _ = global.set(PropertyKey::string("postMessage"), post_msg_fn);

    // onmessage getter/setter
    let handler_for_set = on_message_handler.clone();
    let handler_for_get = on_message_handler.clone();

    let setter = Value::native_function_with_proto(
        move |_this, args, _ncx| {
            let handler = args.first().cloned().unwrap_or(Value::undefined());
            if handler.is_function() || handler.is_native_function() {
                *handler_for_set.lock() = Some(handler);
            } else {
                *handler_for_set.lock() = None;
            }
            Ok(Value::undefined())
        },
        mm.clone(),
        fn_proto,
    );

    let getter = Value::native_function_with_proto(
        move |_this, _args, _ncx| Ok(handler_for_get.lock().clone().unwrap_or(Value::null())),
        mm.clone(),
        fn_proto,
    );

    global.define_property(
        PropertyKey::string("onmessage"),
        otter_vm_core::object::PropertyDescriptor::Accessor {
            get: Some(getter),
            set: Some(setter),
            attributes: otter_vm_core::object::PropertyAttributes::builtin_accessor(),
        },
    );
}

/// Dispatch an incoming message to the worker's onmessage handler.
fn dispatch_message(engine: &mut crate::otter_runtime::Otter, handler: &Value, data: Value) {
    let runtime = engine.isolate.runtime();
    let mm = runtime.memory_manager().clone();

    // Create event-like object: { data: value }
    let event = GcRef::new(JsObject::new(Value::null(), mm));
    let _ = event.set(PropertyKey::string("data"), data);

    // Call handler(event)
    let mut ctx = runtime.create_context();
    let interpreter = otter_vm_core::Interpreter::new();
    let _ = interpreter.call_function(
        &mut ctx,
        handler,
        Value::undefined(),
        &[Value::object(event)],
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn test_worker_extension_registers() {
        let mut engine = crate::otter_runtime::Otter::new();
        engine
            .register_native_extension(Box::new(WorkerExtension))
            .unwrap();

        let result = engine.eval_sync("typeof Worker");
        assert!(result.is_ok());
        let val = result.unwrap();
        assert_eq!(
            val.as_string().map(|s| s.as_str().to_string()),
            Some("function".to_string())
        );
    }

    #[test]
    fn test_worker_constructor_requires_argument() {
        let mut engine = crate::otter_runtime::Otter::new();
        engine
            .register_native_extension(Box::new(WorkerExtension))
            .unwrap();

        // Calling `new Worker()` without args should throw
        let result = engine.eval_sync("new Worker()");
        assert!(
            result.is_err(),
            "Expected error from Worker() without args, got: {:?}",
            result
        );
    }

    #[test]
    fn test_worker_create_and_terminate() {
        let mut engine = crate::otter_runtime::Otter::new();
        engine
            .register_native_extension(Box::new(WorkerExtension))
            .unwrap();

        // Create a worker with inline script
        let result = engine.eval_sync(
            r#"
            var w = new Worker("var x = 1 + 2;");
            w.terminate();
            "ok"
            "#,
        );
        assert!(result.is_ok());
        let val = result.unwrap();
        assert_eq!(
            val.as_string().map(|s| s.as_str().to_string()),
            Some("ok".to_string())
        );

        // Give worker thread time to clean up
        std::thread::sleep(Duration::from_millis(100));
    }
}
