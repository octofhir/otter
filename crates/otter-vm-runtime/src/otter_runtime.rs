//! Otter - High-level runtime that integrates VM, event loop, and extensions
//!
//! This module provides a unified runtime that combines:
//! - The bytecode VM (VmRuntime)
//! - Event loop for async operations
//! - Extension system for native functions
//! - Capabilities for permission checking
//! - Environment store for secure env var access

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::sync::mpsc;

use crate::capabilities::Capabilities;
use crate::env_store::IsolatedEnvStore;
use otter_vm_compiler::Compiler;
use otter_vm_core::async_context::VmExecutionResult;
use otter_vm_core::context::{VmContext, VmContextSnapshot};
use otter_vm_core::error::VmError;
use otter_vm_core::gc::GcRef;
use otter_vm_core::interpreter::Interpreter;
use otter_vm_core::object::{JsObject, PropertyKey};
use otter_vm_core::promise::JsPromise;
use otter_vm_core::runtime::VmRuntime;
use otter_vm_core::string::JsString;
use otter_vm_core::value::Value;

use crate::event_loop::{ActiveServerCount, EventLoop, HttpEvent, WsEvent};
use crate::extension::{AsyncOpFn, ExtensionRegistry, OpHandler};

/// Signal for async resume - stores resolved/rejected value
struct ResumeSignal {
    /// Whether we have a resolved value ready
    ready: AtomicBool,
    /// The resolved value (or error)
    value: parking_lot::Mutex<Option<Result<Value, Value>>>,
}

impl ResumeSignal {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            ready: AtomicBool::new(false),
            value: parking_lot::Mutex::new(None),
        })
    }

    fn set_resolved(&self, value: Value) {
        *self.value.lock() = Some(Ok(value));
        self.ready.store(true, Ordering::Release);
    }

    fn set_rejected(&self, error: Value) {
        *self.value.lock() = Some(Err(error));
        self.ready.store(true, Ordering::Release);
    }

    fn is_ready(&self) -> bool {
        self.ready.load(Ordering::Acquire)
    }

    fn take_value(&self) -> Option<Result<Value, Value>> {
        self.value.lock().take()
    }
}

/// High-level runtime that integrates VM, event loop, and extensions
pub struct Otter {
    /// Bytecode VM
    vm: VmRuntime,
    /// Event loop (shared)
    event_loop: Arc<EventLoop>,
    /// Extension registry
    extensions: ExtensionRegistry,
    /// HTTP events sender (for HTTP extension to use)
    http_tx: mpsc::UnboundedSender<HttpEvent>,
    /// HTTP events receiver (moves to event loop on first eval)
    http_rx: Option<mpsc::UnboundedReceiver<HttpEvent>>,
    /// WebSocket events sender (for WS extension to use)
    ws_tx: mpsc::UnboundedSender<WsEvent>,
    /// WebSocket events receiver (moves to event loop on first eval)
    ws_rx: Option<mpsc::UnboundedReceiver<WsEvent>>,
    /// Active HTTP server count (shared with event loop)
    active_servers: ActiveServerCount,
    /// Isolated environment store
    env_store: Arc<IsolatedEnvStore>,
    /// Capabilities (permissions)
    capabilities: Capabilities,
    /// Interrupt flag for timeout/cancellation
    interrupt_flag: Arc<AtomicBool>,
    /// Debug snapshot for watchdogs
    debug_snapshot: Arc<parking_lot::Mutex<VmContextSnapshot>>,
}

impl Otter {
    /// Create new runtime with default configuration
    pub fn new() -> Self {
        let (http_tx, http_rx) = mpsc::unbounded_channel();
        let (ws_tx, ws_rx) = mpsc::unbounded_channel();
        let event_loop = EventLoop::new(); // Already returns Arc<EventLoop>
        let active_servers = event_loop.get_active_server_count();

        Self {
            vm: VmRuntime::new(),
            event_loop,
            extensions: ExtensionRegistry::new(),
            http_tx,
            http_rx: Some(http_rx),
            ws_tx,
            ws_rx: Some(ws_rx),
            active_servers,
            env_store: Arc::new(IsolatedEnvStore::default()),
            capabilities: Capabilities::none(),
            interrupt_flag: Arc::new(AtomicBool::new(false)),
            debug_snapshot: Arc::new(parking_lot::Mutex::new(VmContextSnapshot::default())),
        }
    }

    /// Get the interrupt flag for timeout/cancellation support
    ///
    /// Use this to interrupt long-running scripts from another thread:
    /// ```ignore
    /// let flag = engine.interrupt_flag();
    /// tokio::spawn(async move {
    ///     tokio::time::sleep(Duration::from_secs(5)).await;
    ///     flag.store(true, Ordering::Relaxed);
    /// });
    /// ```
    pub fn interrupt_flag(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.interrupt_flag)
    }

    /// Get the latest debug snapshot (updated periodically during execution).
    pub fn debug_snapshot(&self) -> VmContextSnapshot {
        self.debug_snapshot.lock().clone()
    }

    /// Get the debug snapshot handle for watchdogs.
    pub fn debug_snapshot_handle(&self) -> Arc<parking_lot::Mutex<VmContextSnapshot>> {
        Arc::clone(&self.debug_snapshot)
    }

    /// Request interruption of execution
    pub fn interrupt(&self) {
        self.interrupt_flag.store(true, Ordering::Relaxed);
    }

    /// Clear the interrupt flag (call before re-using the engine)
    pub fn clear_interrupt(&self) {
        self.interrupt_flag.store(false, Ordering::Relaxed);
    }

    /// Register an extension
    pub fn register_extension(&mut self, ext: crate::extension::Extension) -> Result<(), String> {
        self.extensions.register(ext)
    }

    /// Pre-compile all registered extensions to speed up initialization
    pub fn compile_extensions(&mut self) -> Result<(), String> {
        self.extensions.pre_compile_all()
    }

    /// Get HTTP event sender (for creating HTTP extension)
    pub fn http_event_sender(&self) -> mpsc::UnboundedSender<HttpEvent> {
        self.http_tx.clone()
    }

    /// Get WebSocket event sender (for WS extension)
    pub fn ws_event_sender(&self) -> mpsc::UnboundedSender<WsEvent> {
        self.ws_tx.clone()
    }

    /// Get active server count (for HTTP extension)
    pub fn active_server_count(&self) -> ActiveServerCount {
        Arc::clone(&self.active_servers)
    }

    /// Set environment store
    pub fn set_env_store(&mut self, store: Arc<IsolatedEnvStore>) {
        self.env_store = store;
    }

    /// Get environment store
    pub fn env_store(&self) -> &Arc<IsolatedEnvStore> {
        &self.env_store
    }

    /// Set capabilities
    pub fn set_capabilities(&mut self, caps: Capabilities) {
        self.capabilities = caps;
    }

    /// Get capabilities
    pub fn capabilities(&self) -> &Capabilities {
        &self.capabilities
    }

    /// Get the event loop
    pub fn event_loop(&self) -> &Arc<EventLoop> {
        &self.event_loop
    }

    /// Compile and execute JavaScript code
    pub async fn eval(&mut self, code: &str) -> Result<Value, OtterError> {
        // 0. Clear interrupt flag before starting (in case of re-use)
        self.clear_interrupt();

        // 1. Setup capabilities for security checks in ops
        let _caps_guard =
            crate::capabilities_context::CapabilitiesGuard::new(self.capabilities.clone());

        // 2. Setup HTTP event receiver if not already done
        if let Some(rx) = self.http_rx.take() {
            self.event_loop.set_http_receiver(rx);
        }
        if let Some(rx) = self.ws_rx.take() {
            self.event_loop.set_ws_receiver(rx);
        }

        // 3. Create execution context with globals and interrupt flag
        let mut ctx = self.vm.create_context();
        ctx.set_interrupt_flag(Arc::clone(&self.interrupt_flag));
        ctx.set_debug_snapshot_target(Some(Arc::clone(&self.debug_snapshot)));

        // 4. Register extension ops as global native functions
        self.register_ops_in_context(&mut ctx);

        // Expose interrupt flag to JS for the wrapper to see
        // We use a getter so it's always up to date
        let flag = Arc::clone(&self.interrupt_flag);
        let fn_proto = self.vm.function_prototype();
        ctx.global().define_property(
            PropertyKey::string("__otter_interrupted"),
            otter_vm_core::object::PropertyDescriptor::getter(
                Value::native_function_with_proto(
                    move |_, _| Ok(Value::boolean(flag.load(Ordering::Relaxed))),
                    self.vm.memory_manager().clone(),
                    fn_proto,
                ),
            ),
        );

        // 5. Execute setup JS from extensions (using pre-compiled modules if available)
        let compiled_modules = self.extensions.all_compiled_js();
        if !compiled_modules.is_empty() {
            for module in compiled_modules {
                if let Err(e) = self.vm.execute_module_with_context(&module, &mut ctx) {
                    eprintln!("Extension setup failed: {}", e);
                    return Err(OtterError::Runtime(e.to_string()));
                }
            }
        } else {
            // Fallback to source compilation if no pre-compiled modules
            for js in self.extensions.all_js() {
                self.execute_js(&mut ctx, js, "setup.js")?;
            }
        }

        // 6. Wrap code for top-level await support
        let wrapped = Self::wrap_for_top_level_await(code);

        // 7. Set top-level `this` to the global object per ES2023 §19.2.1.
        // Arrow functions in the wrapper inherit this lexical `this`.
        ctx.set_pending_this(Value::object(ctx.global().clone()));

        // 9. Compile and execute main code with suspension support
        let result_promise = JsPromise::new();
        let mut exec_result = self.execute_with_suspension(
            &mut ctx,
            &wrapped,
            "main.js",
            Arc::clone(&result_promise),
        )?;

        // 10. Handle execution result with proper async resume loop
        let final_value = loop {
            match exec_result {
                VmExecutionResult::Complete(value) => {
                    // Execution completed, resolve result promise and break
                    result_promise.resolve(value.clone());
                    break value;
                }
                VmExecutionResult::Suspended(async_ctx) => {
                    // Execution suspended waiting for a Promise
                    // Set up signal for when the promise resolves
                    let signal = ResumeSignal::new();
                    self.register_promise_signal(&async_ctx.awaited_promise, Arc::clone(&signal));

                    // Wait for the promise to resolve
                    let resolved_value = loop {
                        if self.interrupt_flag.load(Ordering::Relaxed) {
                            return Err(OtterError::Runtime("Test timed out".to_string()));
                        }

                        // Drain microtasks first (this may resolve promises)
                        while let Some(task) = self.event_loop.microtask_queue().dequeue() {
                            task();
                        }

                        // Check if our promise resolved
                        if signal.is_ready() {
                            match signal.take_value() {
                                Some(Ok(value)) => break value,
                                Some(Err(error)) => {
                                    return Err(OtterError::Runtime(format!(
                                        "Promise rejected: {:?}",
                                        error
                                    )));
                                }
                                None => {
                                    return Err(OtterError::Runtime(
                                        "Signal ready but no value".to_string(),
                                    ));
                                }
                            }
                        }

                        // Yield to tokio to let spawned async tasks progress
                        tokio::task::yield_now().await;
                    };

                    // Resume VM execution with resolved value
                    let mut interpreter = Interpreter::new();
                    exec_result = interpreter.resume_async(&mut ctx, async_ctx, resolved_value);

                    // Loop will handle the new exec_result
                }
                VmExecutionResult::Error(msg) => {
                    if self.interrupt_flag.load(Ordering::Relaxed) {
                        return Err(OtterError::Runtime(
                            "Execution interrupted (timeout)".to_string(),
                        ));
                    }
                    return Err(OtterError::Runtime(msg));
                }
            }
        };

        if self.interrupt_flag.load(Ordering::Relaxed) {
            return Err(OtterError::Runtime(
                "Execution interrupted (timeout)".to_string(),
            ));
        }

        // 10. Check for script errors captured by the wrapper
        let global = ctx.global();
        if let Some(error) = global.get(&PropertyKey::string("__otter_script_error")) {
            if !error.is_undefined() {
                let msg = if let Some(obj) = error.as_object() {
                    let name = obj
                        .get(&PropertyKey::string("name"))
                        .and_then(|v| v.as_string())
                        .map(|s| s.as_str().to_string());

                    let message = obj
                        .get(&PropertyKey::string("message"))
                        .and_then(|v| v.as_string())
                        .map(|s| s.as_str().to_string());

                    match (name, message) {
                        (Some(n), Some(m)) => format!("{}: {}", n, m),
                        (Some(n), None) => n,
                        (None, Some(m)) => m,
                        _ => format!("{:?}", error),
                    }
                } else if let Some(s) = error.as_string() {
                    let s_str = s.as_str();
                    if s_str == "Execution interrupted (timeout)" {
                        return Err(OtterError::Runtime(s_str.to_string()));
                    }
                    s_str.to_string()
                } else {
                    format!("{:?}", error)
                };
                return Err(OtterError::Runtime(msg));
            }
        }

        Ok(final_value)
    }

    /// Run the event loop with HTTP dispatch support
    ///
    /// This integrates HTTP event handling with the event loop, calling the JS
    /// dispatcher function `__otter_http_dispatch(serverId, requestId)` for each request.
    async fn run_event_loop_with_http(&self, ctx: &mut VmContext) {
        use std::time::Duration;

        loop {
            // 1. Poll and dispatch HTTP events
            self.dispatch_http_events(ctx);
            // 1b. Poll and dispatch WebSocket events
            self.dispatch_ws_events(ctx);

            // 2. Drain microtasks
            while let Some(task) = self.event_loop.microtask_queue().dequeue() {
                task();
            }

            // 2b. Check for interrupt
            if self.interrupt_flag.load(Ordering::Relaxed) {
                break;
            }

            // 3. Check if we should exit
            let has_tasks = self.event_loop.has_pending_tasks();
            let has_servers = self.event_loop.has_active_http_servers();
            let has_async_ops = self.event_loop.has_pending_async_ops();

            if !has_tasks && !has_servers && !has_async_ops {
                break;
            }

            // 4. Yield to tokio for async I/O
            tokio::task::yield_now().await;

            // 5. Small sleep to prevent busy-loop when waiting for HTTP
            if !has_tasks && has_servers {
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        }
    }

    /// Dispatch pending HTTP events by calling JS handler
    fn dispatch_http_events(&self, ctx: &mut VmContext) {
        // Get pending HTTP events
        let events = self.event_loop.take_http_events();

        for event in events {
            // Call __otter_http_dispatch(serverId, requestId) in JS
            let global = ctx.global();
            if let Some(dispatch_fn) = global.get(&PropertyKey::string("__otter_http_dispatch")) {
                // Call the dispatch function using the interpreter
                let args = vec![
                    Value::number(event.server_id as f64),
                    Value::number(event.request_id as f64),
                ];
                let mut interpreter = Interpreter::new();
                let _ = interpreter.call_function(ctx, &dispatch_fn, Value::undefined(), &args);

                // Drain microtasks after each dispatch
                while let Some(task) = self.event_loop.microtask_queue().dequeue() {
                    task();
                }
            } else {
                break;
            }
        }
    }

    /// Dispatch pending WebSocket events by calling JS handler
    fn dispatch_ws_events(&self, ctx: &mut VmContext) {
        let events = self.event_loop.take_ws_events();
        if events.is_empty() {
            return;
        }

        let global = ctx.global();
        let Some(dispatch_fn) = global.get(&PropertyKey::string("__otter_ws_dispatch")) else {
            return;
        };

        let mm = self.vm.memory_manager().clone();
        for event in events {
            let payload = ws_event_to_json(&event);
            let args = vec![json_to_value(&payload, mm.clone())];
            let mut interpreter = Interpreter::new();
            let _ = interpreter.call_function(ctx, &dispatch_fn, Value::undefined(), &args);

            while let Some(task) = self.event_loop.microtask_queue().dequeue() {
                task();
            }
        }
    }

    /// Execute JS code with suspension support
    fn execute_with_suspension(
        &self,
        ctx: &mut VmContext,
        code: &str,
        source_url: &str,
        result_promise: Arc<JsPromise>,
    ) -> Result<VmExecutionResult, OtterError> {
        let compiler = Compiler::new();
        let module = compiler
            .compile(code, source_url)
            .map_err(|e| OtterError::Compile(e.to_string()))?;

        let module_arc = Arc::new(module);
        let mut interpreter = Interpreter::new();

        Ok(interpreter.execute_with_suspension(module_arc, ctx, result_promise))
    }

    /// Register a signal on a promise to be notified when it resolves/rejects
    fn register_promise_signal(&self, promise: &Arc<JsPromise>, signal: Arc<ResumeSignal>) {
        let event_loop = Arc::clone(&self.event_loop);

        // Handle fulfillment
        let signal_clone = Arc::clone(&signal);
        let event_loop_clone = Arc::clone(&event_loop);
        promise.then_with_enqueue(
            move |value| {
                signal_clone.set_resolved(value);
            },
            move |task| {
                event_loop_clone.microtask_queue().enqueue(task);
            },
        );

        // Handle rejection
        let signal_clone2 = Arc::clone(&signal);
        let event_loop_clone2 = Arc::clone(&event_loop);
        promise.catch_with_enqueue(
            move |error| {
                signal_clone2.set_rejected(error);
            },
            move |task| {
                event_loop_clone2.microtask_queue().enqueue(task);
            },
        );
    }

    /// Execute JavaScript code without async event loop
    pub fn eval_sync(&mut self, code: &str) -> Result<Value, OtterError> {
        // Clear interrupt flag before starting
        self.clear_interrupt();

        // Set up capabilities for security checks in ops
        let _caps_guard =
            crate::capabilities_context::CapabilitiesGuard::new(self.capabilities.clone());

        // Create execution context with interrupt flag
        let mut ctx = self.vm.create_context();
        ctx.set_interrupt_flag(Arc::clone(&self.interrupt_flag));
        ctx.set_debug_snapshot_target(Some(Arc::clone(&self.debug_snapshot)));

        // Register extension ops as global native functions
        self.register_ops_in_context(&mut ctx);

        // Execute setup JS from extensions
        for js in self.extensions.all_js() {
            self.execute_js(&mut ctx, js, "setup.js")?;
        }

        // Compile and execute with eval semantics (return last expression value)
        self.execute_js_eval(&mut ctx, code, "eval.js")
    }

    /// Wrap code for top-level await support
    fn wrap_for_top_level_await(code: &str) -> String {
        let trimmed = code.trim_start();
        let has_use_strict =
            trimmed.starts_with("\"use strict\"") || trimmed.starts_with("'use strict'");
        let (strict_prefix, code_body) = if has_use_strict {
            let first_line_end = code.find('\n').unwrap_or(code.len());
            let (prefix, rest) = code.split_at(first_line_end);
            (format!("{};\n", prefix.trim()), rest)
        } else {
            ("".to_string(), code)
        };

        format!(
            r#"{strict_prefix}
            try {{
                globalThis.__otter_main_promise = (async function() {{
                    {code_body}
                }}).call(this);
                globalThis.__otter_main_promise.catch(function(err) {{
                    if (globalThis.__otter_interrupted) {{
                        globalThis.__otter_script_error = "Execution interrupted (timeout)";
                    }} else {{
                        globalThis.__otter_script_error = err;
                    }}
                }});
            }} catch (err) {{
                if (globalThis.__otter_interrupted) {{
                    globalThis.__otter_script_error = "Execution interrupted (timeout)";
                }} else {{
                    globalThis.__otter_script_error = err;
                }}
            }}"#
        )
    }

    /// Register extension ops as global native functions in context
    fn register_ops_in_context(&self, ctx: &mut VmContext) {
        let global = ctx.global().clone();
        let pending_ops = self.event_loop.get_pending_async_ops_count();
        let fn_proto = self.vm.function_prototype();

        for op_name in self.extensions.op_names() {
            if let Some(handler) = self.extensions.get_op(op_name) {
                let native_fn = self.create_native_wrapper(
                    op_name,
                    handler.clone(),
                    Arc::clone(&pending_ops),
                    self.vm.memory_manager().clone(),
                    fn_proto,
                );
                global.set(PropertyKey::string(op_name), native_fn);
            }
        }

        // Also register environment access if capabilities allow
        self.register_env_access(global, fn_proto);

        let ctx_ptr = ctx as *mut VmContext as usize;
        let vm_ptr = &self.vm as *const VmRuntime as usize;
        let mm_eval = self.vm.memory_manager().clone();
        let mm_eval_closure = mm_eval.clone();
        global.set(
            PropertyKey::string("__otter_eval"),
            Value::native_function_with_proto(
                move |args: &[Value], _mm| {
                    let mm_result = mm_eval_closure.clone();
                    let result_ok = |value: Value| {
                        let obj = JsObject::new(None, mm_result.clone());
                        obj.set(PropertyKey::string("ok"), Value::boolean(true));
                        obj.set(PropertyKey::string("value"), value);
                        Value::object(GcRef::new(obj))
                    };

                    let result_err = |error_type: &str, message: &str| {
                        let obj = JsObject::new(None, mm_result.clone());
                        obj.set(PropertyKey::string("ok"), Value::boolean(false));
                        obj.set(
                            PropertyKey::string("errorType"),
                            Value::string(JsString::intern(error_type)),
                        );
                        obj.set(
                            PropertyKey::string("message"),
                            Value::string(JsString::intern(message)),
                        );
                        Value::object(GcRef::new(obj))
                    };

                    let code_value = match args.first() {
                        Some(value) => value.clone(),
                        None => return Ok(result_ok(Value::undefined())),
                    };

                    if !code_value.is_string() {
                        return Ok(result_ok(code_value));
                    }

                    let code = code_value
                        .as_string()
                        .map(|s| s.as_str().to_string())
                        .unwrap_or_default();

                    unsafe {
                        let ctx = &mut *(ctx_ptr as *mut VmContext);
                        let vm = &*(vm_ptr as *const VmRuntime);
                        let compiler = Compiler::new();
                        let module = match compiler.compile(&code, "eval.js") {
                            Ok(module) => module,
                            Err(err) => {
                                return Ok(result_err("SyntaxError", &err.to_string()));
                            }
                        };

                        match vm.execute_module_with_context(&module, ctx) {
                            Ok(value) => Ok(result_ok(value)),
                            Err(err) => {
                                let (error_type, message) = match err {
                                    VmError::TypeError(msg) => ("TypeError", msg),
                                    VmError::ReferenceError(msg) => ("ReferenceError", msg),
                                    VmError::RangeError(msg) => ("RangeError", msg),
                                    VmError::SyntaxError(msg) => ("SyntaxError", msg),
                                    VmError::Exception(ex) => ("Error", ex.message),
                                    other => ("Error", other.to_string()),
                                };
                                Ok(result_err(error_type, &message))
                            }
                        }
                    }
                },
                mm_eval,
                fn_proto,
            ),
        );
    }

    /// Create a native function wrapper for an op handler
    fn create_native_wrapper(
        &self,
        name: &str,
        handler: OpHandler,
        pending_ops: Arc<std::sync::atomic::AtomicU64>,
        mm: Arc<otter_vm_core::MemoryManager>,
        fn_proto: GcRef<JsObject>,
    ) -> Value {
        let _name = name.to_string();

        match handler {
            OpHandler::Native(native_fn) => {
                // Native ops work directly with Value
                Value::native_function_with_proto(
                    move |args, mm_inner| native_fn(args, mm_inner),
                    mm,
                    fn_proto,
                )
            }
            OpHandler::Sync(sync_fn) => {
                // Sync JSON ops need Value -> JSON -> Value conversion
                let mm_inner = mm.clone();
                Value::native_function_with_proto(
                    move |args, _mm_ignored| {
                        let json_args: Vec<serde_json::Value> =
                            args.iter().map(value_to_json).collect();
                        let result = sync_fn(&json_args)?;
                        Ok(json_to_value(&result, mm_inner.clone()))
                    },
                    mm,
                    fn_proto,
                )
            }
            OpHandler::Async(async_fn) => {
                // Async ops return a Promise and spawn a tokio task
                let async_fn: AsyncOpFn = async_fn.clone();
                let pending_ops = Arc::clone(&pending_ops);
                let mm_outer = mm.clone();
                let mm_outer_closure = mm_outer.clone();
                Value::native_function_with_proto(
                    move |args, _mm_ignored| {
                        let mm_promise = mm_outer_closure.clone();
                        let resolvers = JsPromise::with_resolvers(mm_promise.clone());
                        let promise = resolvers.promise.clone();
                        let resolve = resolvers.resolve.clone();
                        let reject = resolvers.reject.clone();

                        let json_args: Vec<serde_json::Value> =
                            args.iter().map(value_to_json).collect();

                        let future = async_fn(&json_args);

                        let pending_ops_clone = Arc::clone(&pending_ops);
                        pending_ops.fetch_add(1, std::sync::atomic::Ordering::Relaxed);

                        let mm_spawn = mm_outer_closure.clone();
                        tokio::spawn(async move {
                            match future.await {
                                Ok(json_result) => {
                                    let value = json_to_value(&json_result, mm_spawn);
                                    resolve(value);
                                }
                                Err(err) => {
                                    let error = Value::string(JsString::intern(&err));
                                    reject(error);
                                }
                            }
                            pending_ops_clone.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
                        });

                        Ok(Value::promise(promise))
                    },
                    mm_outer,
                    fn_proto,
                )
            }
        }
    }

    /// Register environment variable access functions
    fn register_env_access(&self, global: GcRef<JsObject>, fn_proto: GcRef<JsObject>) {
        let env_store = Arc::clone(&self.env_store);
        let caps = self.capabilities.clone();

        // __env_get(key) -> string | undefined
        let env_store_get = Arc::clone(&env_store);
        let caps_get = caps.clone();
        let mm_env = self.vm.memory_manager().clone();
        global.set(
            PropertyKey::string("__env_get"),
            Value::native_function_with_proto(
                move |args: &[Value], _mm| {
                    let key = args
                        .first()
                        .and_then(|v| v.as_string())
                        .map(|s| s.as_str().to_string())
                        .ok_or_else(|| "env_get requires a string key".to_string())?;

                    if !caps_get.can_env(&key) {
                        return Err(format!("Permission denied: env access to '{}'", key));
                    }

                    match env_store_get.get(&key) {
                        Some(val) => {
                            Ok(Value::string(otter_vm_core::string::JsString::intern(&val)))
                        }
                        None => Ok(Value::undefined()),
                    }
                },
                mm_env.clone(),
                fn_proto,
            ),
        );

        let env_store_keys = Arc::clone(&env_store);
        let mm_keys = mm_env.clone();
        let mm_keys_closure = mm_keys.clone();
        global.set(
            PropertyKey::string("__env_keys"),
            Value::native_function_with_proto(
                move |_args: &[Value], _mm| {
                    let keys = env_store_keys.keys();
                    let arr = JsObject::array(keys.len(), mm_keys_closure.clone());
                    for (i, key) in keys.into_iter().enumerate() {
                        arr.set(
                            PropertyKey::Index(i as u32),
                            Value::string(otter_vm_core::string::JsString::intern(&key)),
                        );
                    }
                    Ok(Value::object(GcRef::new(arr)))
                },
                mm_keys,
                fn_proto,
            ),
        );

        // __env_has(key) -> boolean
        let env_store_has = Arc::clone(&env_store);
        let caps_has = caps.clone();
        let mm_has = self.vm.memory_manager().clone();
        global.set(
            PropertyKey::string("__env_has"),
            Value::native_function_with_proto(
                move |args: &[Value], _mm| {
                    let key = args
                        .first()
                        .and_then(|v| v.as_string())
                        .map(|s| s.as_str().to_string())
                        .ok_or_else(|| "env_has requires a string key".to_string())?;

                    if !caps_has.can_env(&key) {
                        return Ok(Value::boolean(false));
                    }

                    Ok(Value::boolean(env_store_has.contains(&key)))
                },
                mm_has,
                fn_proto,
            ),
        );
    }

    /// Compile and execute JS code
    fn execute_js(
        &self,
        ctx: &mut VmContext,
        code: &str,
        source_url: &str,
    ) -> Result<Value, OtterError> {
        let compiler = Compiler::new();
        let module = compiler
            .compile(code, source_url)
            .map_err(|e| OtterError::Compile(e.to_string()))?;

        self.vm
            .execute_module_with_context(&module, ctx)
            .map_err(|e| OtterError::Runtime(e.to_string()))
    }

    /// Execute JS code with eval semantics (returns last expression value)
    fn execute_js_eval(
        &self,
        ctx: &mut VmContext,
        code: &str,
        source_url: &str,
    ) -> Result<Value, OtterError> {
        let compiler = Compiler::new();
        let module = compiler
            .compile_eval(code, source_url)
            .map_err(|e| OtterError::Compile(e.to_string()))?;

        self.vm
            .execute_module_with_context(&module, ctx)
            .map_err(|e| OtterError::Runtime(e.to_string()))
    }

    // ==================== Profiling API ====================

    /// Create a new RuntimeStats instance for profiling
    ///
    /// Use this to enable profiling on a VmContext:
    /// ```ignore
    /// let stats = otter_profiler::RuntimeStats::new();
    /// let stats = Arc::new(stats);
    /// ctx.enable_profiling(Arc::clone(&stats));
    /// // ... run code ...
    /// let snapshot = stats.snapshot();
    /// ```
    #[cfg(feature = "profiling")]
    pub fn create_profiling_stats() -> std::sync::Arc<otter_profiler::RuntimeStats> {
        std::sync::Arc::new(otter_profiler::RuntimeStats::new())
    }

    /// Create a CpuProfiler for sampling-based CPU profiling
    #[cfg(feature = "profiling")]
    pub fn create_cpu_profiler() -> otter_profiler::CpuProfiler {
        otter_profiler::CpuProfiler::new()
    }

    /// Create a MemoryProfiler for heap snapshots
    #[cfg(feature = "profiling")]
    pub fn create_memory_profiler() -> otter_profiler::MemoryProfiler {
        otter_profiler::MemoryProfiler::new()
    }

    /// Create a MemoryProfiler connected to a GcHeap
    #[cfg(feature = "profiling")]
    pub fn create_memory_profiler_with_heap(
        heap: std::sync::Arc<otter_vm_gc::GcHeap>,
    ) -> otter_profiler::MemoryProfiler {
        use otter_profiler::{HeapInfo, MemoryProfiler};

        let provider = std::sync::Arc::new(move || HeapInfo {
            total_allocated: heap.allocated(),
            objects_by_type: std::collections::HashMap::new(),
            object_count: 0,
        });

        MemoryProfiler::with_heap_provider(provider)
    }
}

impl Default for Otter {
    fn default() -> Self {
        Self::new()
    }
}

// SAFETY: Otter uses thread-safe types
unsafe impl Send for Otter {}

/// Convert VM Value to JSON
fn value_to_json(value: &Value) -> serde_json::Value {
    fn value_to_json_limited(value: &Value, depth: usize) -> serde_json::Value {
        if depth > 512 {
            return serde_json::Value::Null;
        }

        if value.is_undefined() || value.is_null() {
            return serde_json::Value::Null;
        }

        if let Some(b) = value.as_boolean() {
            return serde_json::Value::Bool(b);
        }

        if let Some(n) = value.as_number() {
            if n.is_nan() || n.is_infinite() {
                return serde_json::Value::Null;
            }
            if n.fract() == 0.0 && n.abs() < (i64::MAX as f64) {
                return serde_json::Value::Number(serde_json::Number::from(n as i64));
            }
            return serde_json::Number::from_f64(n)
                .map(serde_json::Value::Number)
                .unwrap_or(serde_json::Value::Null);
        }

        if let Some(s) = value.as_string() {
            return serde_json::Value::String(s.as_str().to_string());
        }

        if let Some(obj) = value.as_object() {
            if obj.is_array() {
                let len = obj.array_length();
                let mut arr = Vec::with_capacity(len.min(1000));
                for i in 0..len {
                    if i > 5000 {
                        break;
                    }
                    let elem = obj
                        .get(&PropertyKey::Index(i as u32))
                        .unwrap_or_else(Value::undefined);
                    arr.push(value_to_json_limited(&elem, depth + 1));
                }
                return serde_json::Value::Array(arr);
            }

            // Regular object
            let mut map = serde_json::Map::new();
            for key in obj.own_keys() {
                if let PropertyKey::String(s) = &key
                    && let Some(val) = obj.get(&key)
                {
                    map.insert(
                        s.as_str().to_string(),
                        value_to_json_limited(&val, depth + 1),
                    );
                }
                if map.len() > 1000 {
                    break;
                }
            }
            return serde_json::Value::Object(map);
        }

        serde_json::Value::Null
    }

    value_to_json_limited(value, 0)
}

/// Convert JSON to VM Value
fn json_to_value(json: &serde_json::Value, mm: Arc<otter_vm_core::MemoryManager>) -> Value {
    match json {
        serde_json::Value::Null => Value::null(),
        serde_json::Value::Bool(b) => Value::boolean(*b),
        serde_json::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                if i >= i32::MIN as i64 && i <= i32::MAX as i64 {
                    Value::int32(i as i32)
                } else {
                    Value::number(i as f64)
                }
            } else if let Some(f) = n.as_f64() {
                Value::number(f)
            } else {
                Value::number(0.0)
            }
        }
        serde_json::Value::String(s) => Value::string(otter_vm_core::string::JsString::intern(s)),
        serde_json::Value::Array(arr) => {
            let js_arr = JsObject::array(arr.len(), mm.clone());
            for (i, elem) in arr.iter().enumerate() {
                js_arr.set(
                    PropertyKey::Index(i as u32),
                    json_to_value(elem, mm.clone()),
                );
            }
            Value::object(GcRef::new(js_arr))
        }
        serde_json::Value::Object(obj) => {
            let js_obj = JsObject::new(None, mm.clone());
            for (key, val) in obj {
                js_obj.set(PropertyKey::string(key), json_to_value(val, mm.clone()));
            }
            Value::object(GcRef::new(js_obj))
        }
    }
}

fn ws_event_to_json(event: &WsEvent) -> serde_json::Value {
    match event {
        WsEvent::Open {
            server_id,
            socket_id,
            data,
            remote_addr,
        } => serde_json::json!({
            "type": "open",
            "serverId": server_id,
            "socketId": socket_id,
            "data": data,
            "remoteAddress": remote_addr,
        }),
        WsEvent::Message {
            server_id,
            socket_id,
            data,
            is_text,
        } => {
            if *is_text {
                let text = String::from_utf8_lossy(data).to_string();
                serde_json::json!({
                    "type": "message",
                    "serverId": server_id,
                    "socketId": socket_id,
                    "data": text,
                    "binary": false,
                })
            } else {
                let bytes: Vec<u8> = data.clone();
                serde_json::json!({
                    "type": "message",
                    "serverId": server_id,
                    "socketId": socket_id,
                    "data": bytes,
                    "binary": true,
                })
            }
        }
        WsEvent::Close {
            server_id,
            socket_id,
            code,
            reason,
        } => serde_json::json!({
            "type": "close",
            "serverId": server_id,
            "socketId": socket_id,
            "code": code,
            "reason": reason,
        }),
        WsEvent::Drain {
            server_id,
            socket_id,
        } => serde_json::json!({
            "type": "drain",
            "serverId": server_id,
            "socketId": socket_id,
        }),
        WsEvent::Ping {
            server_id,
            socket_id,
            data,
        } => serde_json::json!({
            "type": "ping",
            "serverId": server_id,
            "socketId": socket_id,
            "data": data,
        }),
        WsEvent::Pong {
            server_id,
            socket_id,
            data,
        } => serde_json::json!({
            "type": "pong",
            "serverId": server_id,
            "socketId": socket_id,
            "data": data,
        }),
        WsEvent::Error {
            server_id,
            socket_id,
            message,
        } => serde_json::json!({
            "type": "error",
            "serverId": server_id,
            "socketId": socket_id,
            "message": message,
        }),
    }
}

/// Error type for Otter
#[derive(Debug, Clone)]
pub enum OtterError {
    /// Compilation error
    Compile(String),
    /// Runtime error
    Runtime(String),
    /// Permission denied
    PermissionDenied(String),
}

impl std::fmt::Display for OtterError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Compile(msg) => write!(f, "CompileError: {}", msg),
            Self::Runtime(msg) => write!(f, "RuntimeError: {}", msg),
            Self::PermissionDenied(msg) => write!(f, "PermissionDenied: {}", msg),
        }
    }
}

impl std::error::Error for OtterError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_runtime_creation() {
        let runtime = Otter::new();
        assert_eq!(runtime.extensions.extension_count(), 0);
    }

    #[test]
    fn test_value_json_conversion() {
        let mm = Arc::new(otter_vm_core::MemoryManager::test());
        // Test primitives
        assert_eq!(value_to_json(&Value::null()), serde_json::Value::Null);
        assert_eq!(
            value_to_json(&Value::boolean(true)),
            serde_json::Value::Bool(true)
        );
        assert_eq!(value_to_json(&Value::int32(42)), serde_json::json!(42));
        assert_eq!(value_to_json(&Value::number(3.14)), serde_json::json!(3.14));

        // Test string
        let s = Value::string(otter_vm_core::string::JsString::intern("hello"));
        assert_eq!(value_to_json(&s), serde_json::json!("hello"));
    }

    #[test]
    fn test_json_value_conversion() {
        let mm = Arc::new(otter_vm_core::MemoryManager::test());
        // Test primitives
        assert!(json_to_value(&serde_json::Value::Null, mm.clone()).is_null());
        assert_eq!(
            json_to_value(&serde_json::json!(true), mm.clone()).as_boolean(),
            Some(true)
        );
        assert_eq!(
            json_to_value(&serde_json::json!(42), mm.clone()).as_int32(),
            Some(42)
        );
        assert_eq!(
            json_to_value(&serde_json::json!(3.14), mm.clone()).as_number(),
            Some(3.14)
        );

        // Test string
        let val = json_to_value(&serde_json::json!("hello"), mm.clone());
        let val_str = val.as_string().unwrap();
        assert_eq!(val_str.as_str(), "hello");
    }

    #[test]
    fn test_eval_sync_simple() {
        let mut runtime = Otter::new();
        // Just verify basic code execution works without errors
        // (Module execution returns undefined, not the last expression)
        let result = runtime.eval_sync("let x = 1 + 1");
        assert!(result.is_ok());
    }

    #[test]
    fn test_eval_sync_global() {
        let mut runtime = Otter::new();
        // Verify setting a global works without error
        let result = runtime.eval_sync("globalThis.x = 42");
        assert!(result.is_ok());
        // Note: Each eval_sync creates a new context, so globals don't persist
        // This tests that the runtime can execute code that modifies globals
    }
}
