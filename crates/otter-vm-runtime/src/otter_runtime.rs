//! Otter - High-level runtime that integrates VM, event loop, and extensions
//!
//! This module provides a unified runtime that combines:
//! - The bytecode VM (VmRuntime)
//! - Event loop for async operations
//! - Extension system for native functions
//! - Capabilities for permission checking
//! - Environment store for secure env var access

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::sync::mpsc;

use crate::capabilities::Capabilities;
use crate::env_store::IsolatedEnvStore;
use crate::module_provider::ModuleType;
use otter_vm_bytecode::module::{ImportBinding, ImportRecord};
use otter_vm_bytecode::{Instruction, Module, Register, TypeFlags, UpvalueCapture};
use otter_vm_compiler::Compiler;
use otter_vm_core::async_context::VmExecutionResult;
use otter_vm_core::context::{VmContext, VmContextSnapshot};
use otter_vm_core::error::VmError;
use otter_vm_core::gc::GcRef;
use otter_vm_core::interpreter::Interpreter;
use otter_vm_core::object::{JsObject, PropertyDescriptor, PropertyKey};
use otter_vm_core::promise::{JsPromise, JsPromiseJob, JsPromiseJobKind};
use otter_vm_core::realm::RealmId;
use otter_vm_core::runtime::VmRuntime;
use otter_vm_core::string::JsString;
use otter_vm_core::value::Value;

use crate::event_loop::{ActiveServerCount, EventLoop, HttpEvent, WsEvent};
use crate::extension::{AsyncOpFn, ExtensionRegistry, OpHandler};
use crate::microtask::JsJobQueueWrapper;

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
    /// Module loader for resolving and loading ES modules
    loader: Arc<crate::module_loader::ModuleLoader>,
    /// Extension registry (v1 — JSON ops + JS shims)
    extensions: ExtensionRegistry,
    /// Extension registry (v2 — native-first, no JS shims)
    native_extensions: crate::extension_v2::NativeExtensionRegistry,
    /// Per-extension typed state storage
    extension_state: crate::extension_state::ExtensionState,
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
    /// Trace configuration (if tracing is enabled)
    trace_config: Option<otter_vm_core::TraceConfig>,
    /// Stored realm contexts created via createRealm().
    realms: Arc<parking_lot::Mutex<Vec<RealmSlot>>>,
}

#[derive(Debug)]
struct RealmSlot {
    id: RealmId,
    ctx: VmContext,
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
            loader: Arc::new(crate::module_loader::ModuleLoader::new(
                std::env::current_dir().unwrap_or_default(),
            )),
            extensions: ExtensionRegistry::new(),
            native_extensions: crate::extension_v2::NativeExtensionRegistry::new(),
            extension_state: crate::extension_state::ExtensionState::new(),
            http_tx,
            http_rx: Some(http_rx),
            ws_tx,
            ws_rx: Some(ws_rx),
            active_servers,
            env_store: Arc::new(IsolatedEnvStore::default()),
            capabilities: Capabilities::none(),
            interrupt_flag: Arc::new(AtomicBool::new(false)),
            debug_snapshot: Arc::new(parking_lot::Mutex::new(VmContextSnapshot::default())),
            trace_config: None,
            realms: Arc::new(parking_lot::Mutex::new(Vec::new())),
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

    /// Reset the default realm for test isolation.
    ///
    /// Creates a fresh realm (intrinsics, global, Function.prototype) on the
    /// existing GC heap, sets it as the default, and removes the old realm
    /// from the registry so its objects can be GC'd.
    ///
    /// Much lighter than rebuilding the entire engine:
    /// - Reuses the same MemoryManager, EventLoop, ExtensionRegistry
    /// - Extensions are re-applied by eval() automatically
    /// - The GC heap is NOT wiped; old objects are collected normally
    ///
    /// For post-panic recovery, use full engine rebuild instead.
    pub fn reset_realm(&mut self) {
        // Drop stored realm contexts from $262.createRealm()
        self.realms.lock().clear();

        // Create fresh realm, swap default, drop old
        self.vm.reset_default_realm();

        // Reset execution state
        self.clear_interrupt();
        *self.debug_snapshot.lock() = VmContextSnapshot::default();
    }

    /// Set trace configuration for instruction-level tracing
    pub fn set_trace_config(&mut self, config: otter_vm_core::TraceConfig) {
        self.trace_config = Some(config);
    }

    /// Dump formatted snapshot to writer (useful for debugging timeouts)
    pub fn dump_snapshot(&self, writer: &mut dyn std::io::Write) -> std::io::Result<()> {
        let snapshot = self.debug_snapshot();

        // Create a temporary ring buffer from the snapshot entries if present
        let trace_buffer = if !snapshot.recent_instructions.is_empty() {
            let mut buffer =
                otter_vm_core::TraceRingBuffer::new(snapshot.recent_instructions.len().max(1));
            for entry in &snapshot.recent_instructions {
                buffer.push(entry.clone());
            }
            Some(buffer)
        } else {
            None
        };

        // Use format module to create human-readable output
        let formatted = otter_vm_core::format::format_snapshot(&snapshot, trace_buffer.as_ref());

        write!(writer, "{}", formatted)
    }

    /// Register an extension
    pub fn register_extension(&mut self, ext: crate::extension::Extension) -> Result<(), String> {
        self.extensions.register(ext)
    }

    /// Register a v2 native extension.
    ///
    /// V2 extensions use `OtterExtension` trait and register native functions
    /// directly without JS shims or JSON serde.
    pub fn register_native_extension(
        &mut self,
        ext: Box<dyn crate::extension_v2::OtterExtension>,
    ) -> Result<(), String> {
        self.native_extensions.register(ext)
    }

    /// Get the v2 extension registry.
    pub fn native_extensions(&self) -> &crate::extension_v2::NativeExtensionRegistry {
        &self.native_extensions
    }

    /// Get the extension state.
    pub fn extension_state(&self) -> &crate::extension_state::ExtensionState {
        &self.extension_state
    }

    /// Get the extension state mutably.
    pub fn extension_state_mut(&mut self) -> &mut crate::extension_state::ExtensionState {
        &mut self.extension_state
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

    /// Register a module provider to handle custom import protocols or resolution logic.
    pub fn register_module_provider(
        &self,
        provider: Arc<dyn crate::module_provider::ModuleProvider>,
    ) {
        self.loader.register_provider(provider);
    }

    /// Get the module loader instance.
    pub fn loader(&self) -> Arc<crate::module_loader::ModuleLoader> {
        Arc::clone(&self.loader)
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

    /// Compile and execute JavaScript code.
    ///
    /// Code is compiled as an ES module (allowing top-level await) and executed
    /// directly in the global scope — no async IIFE wrapper. This preserves:
    /// - `var` declarations creating global properties
    /// - `function` declarations creating global properties
    /// - Correct `this` binding (global object for scripts)
    /// - Top-level `await` via the interpreter's suspension machinery
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

        // Apply trace configuration if enabled
        if let Some(ref trace_config) = self.trace_config {
            ctx.set_trace_config(trace_config.clone());
        }

        Self::configure_eval(&mut ctx);
        Self::configure_js_job_queue(&mut ctx, &self.event_loop);

        // 4. Register extension ops as global native functions
        self.register_ops_in_context(&mut ctx);

        // 4b. Bootstrap v2 native extensions (no JS compilation needed)
        self.bootstrap_native_extensions(&ctx)?;

        // 5. Execute setup JS from extensions (using pre-compiled modules if available)
        let compiled_modules = self.extensions.all_compiled_js();
        if !compiled_modules.is_empty() {
            for module in compiled_modules {
                if let Err(e) = self.vm.execute_module_with_context(&module, &mut ctx) {
                    eprintln!("Extension setup failed: {}", e);
                    return Err(OtterError::Runtime(e.to_string()));
                }
                // ES spec: Drain microtasks after each module evaluation
                self.drain_microtasks(&mut ctx)?;
            }
        } else {
            // Fallback to source compilation if no pre-compiled modules
            for js in self.extensions.all_js() {
                self.execute_js(&mut ctx, js, "setup.js")?;
                self.drain_microtasks(&mut ctx)?;
            }
        }

        // ES spec: Drain microtasks after extension setup JS execution
        self.drain_microtasks(&mut ctx)?;

        // 6. Set top-level `this` to the global object per ES2023 §19.2.1.
        ctx.set_pending_this(Value::object(ctx.global().clone()));

        // 7. Load, link and execute as module
        let main_url = "main.js";

        let bytecode = match self.loader.compile_source(code, main_url, false) {
            Ok(b) => b,
            Err(e) => return Err(OtterError::Compile(e.to_string())),
        };

        // Link the module (resolves all imports)
        if let Err(e) = self.loader.link(main_url) {
            return Err(OtterError::Runtime(format!("Module linking failed: {}", e)));
        }

        // Build the dependency graph and execute modules in order
        let order = match self.loader.build_graph(main_url) {
            Ok(o) => o,
            Err(e) => return Err(OtterError::Runtime(e.to_string())),
        };

        // Execute all dependencies first (excluding main)
        let normalized_main_url = self.loader.normalize_url(main_url);
        for url in &order {
            if url == main_url || url == &normalized_main_url {
                continue;
            }
            let m = self.loader.get(url).unwrap();
            let (bytecode, already_evaluated) = {
                let guard = m.read().unwrap();
                (
                    Arc::clone(&guard.bytecode),
                    guard.state == crate::module_loader::ModuleState::Evaluated,
                )
            };

            // Skip native modules that are already evaluated (v2 extensions)
            if already_evaluated {
                continue;
            }

            // Execute dependency module
            if let Err(e) = self.vm.execute_module_with_context(&bytecode, &mut ctx) {
                return Err(OtterError::Runtime(e.to_string()));
            }

            // Populate namespace
            self.loader.update_namespace(url, &ctx);
        }

        // Resolve imports for the main module
        let initial_locals = self.resolve_module_imports(&bytecode, main_url)?;
        let locals_opt = if initial_locals.is_empty() {
            None
        } else {
            Some(initial_locals)
        };

        // Now execute the main module with suspension support
        let result_promise = JsPromise::new();
        let mut exec_result = self.execute_with_suspension(&mut ctx, bytecode, result_promise, locals_opt)?;

        // After main execution starts, we might need a way to update its namespace too if it's imported elsewhere,
        // but for high-level eval it's usually the end of the chain.
        self.loader.update_namespace(main_url, &ctx); // 8. Handle execution result with async resume loop
        let final_value = loop {
            match exec_result {
                VmExecutionResult::Complete(value) => {
                    // Execution completed, resolve result promise and break
                    result_promise.resolve(value.clone());
                    // ES spec: Drain microtasks after synchronous execution
                    self.drain_microtasks(&mut ctx)?;
                    break value;
                }
                VmExecutionResult::Suspended(async_ctx) => {
                    // Execution suspended waiting for a Promise
                    // Set up signal for when the promise resolves
                    let signal = ResumeSignal::new();
                    self.register_promise_signal(async_ctx.awaited_promise, Arc::clone(&signal));

                    // Wait for the promise to resolve
                    let resolved_value = loop {
                        if self.interrupt_flag.load(Ordering::Relaxed) {
                            return Err(OtterError::Runtime("Test timed out".to_string()));
                        }

                        // Drain microtasks first (this may resolve promises)
                        self.drain_microtasks(&mut ctx)?;

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
                    let interpreter = Interpreter::new();
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

            // 2. Drain microtasks and JS callback jobs
            if let Err(e) = self.drain_microtasks(ctx) {
                eprintln!("Error draining microtasks in event loop: {}", e);
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
                let interpreter = Interpreter::new();
                let _ = interpreter.call_function(ctx, &dispatch_fn, Value::undefined(), &args);

                // Drain microtasks (including JS jobs) after each dispatch
                if let Err(e) = self.drain_microtasks(ctx) {
                    eprintln!("Error draining microtasks after HTTP dispatch: {}", e);
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
            let interpreter = Interpreter::new();
            let _ = interpreter.call_function(ctx, &dispatch_fn, Value::undefined(), &args);

            if let Err(e) = self.drain_microtasks(ctx) {
                eprintln!("Error draining microtasks after WS dispatch: {}", e);
            }
        }
    }

    /// Execute JS code with suspension support.
    ///
    /// Compiles as a script (NOT a module) to preserve non-strict mode semantics,
    /// while still supporting top-level await through the interpreter's suspension machinery.
    /// ES2023 §16.1.6: Scripts are not automatically strict unless they have "use strict" directive.
    /// Execute pre-compiled bytecode with suspension support.
    fn execute_with_suspension(
        &self,
        ctx: &mut VmContext,
        module: Arc<otter_vm_bytecode::Module>,
        result_promise: GcRef<JsPromise>,
        initial_locals: Option<HashMap<u16, Value>>,
    ) -> Result<VmExecutionResult, OtterError> {
        let interpreter = Interpreter::new();
        Ok(interpreter.execute_with_suspension_and_locals(module, ctx, result_promise, initial_locals))
    }

    /// Register a signal on a promise to be notified when it resolves/rejects
    fn register_promise_signal(&self, promise: GcRef<JsPromise>, signal: Arc<ResumeSignal>) {
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

        // Apply trace configuration if enabled
        if let Some(ref trace_config) = self.trace_config {
            ctx.set_trace_config(trace_config.clone());
        }

        Self::configure_eval(&mut ctx);
        Self::configure_js_job_queue(&mut ctx, &self.event_loop);

        // Register extension ops as global native functions
        self.register_ops_in_context(&mut ctx);

        // Bootstrap v2 native extensions (no JS compilation needed)
        self.bootstrap_native_extensions(&ctx)?;

        // Execute setup JS from extensions
        for js in self.extensions.all_js() {
            self.execute_js(&mut ctx, js, "setup.js")?;
            self.drain_microtasks(&mut ctx)?;
        }

        // Set top-level `this` to global object
        ctx.set_pending_this(Value::object(ctx.global().clone()));

        // Load, link and execute
        let is_module =
            code.contains("import ") || code.contains("export ") || code.contains("await ");
        let main_url = if is_module { "main.mjs" } else { "main.js" };
        let bytecode = match self.loader.compile_source(code, main_url, true) {
            Ok(b) => b,
            Err(e) => return Err(OtterError::Compile(e.to_string())),
        };

        if let Err(e) = self.loader.link(main_url) {
            return Err(OtterError::Runtime(format!("Module linking failed: {}", e)));
        }

        // Execute dependencies
        let order = match self.loader.build_graph(main_url) {
            Ok(o) => o,
            Err(e) => return Err(OtterError::Runtime(e.to_string())),
        };

        let normalized_main_url = self.loader.normalize_url(main_url);
        for url in &order {
            if url == main_url || url == &normalized_main_url {
                continue;
            }
            let m = self.loader.get(url).unwrap();
            let (module_bytecode, already_evaluated) = {
                let guard = m.read().unwrap();
                (
                    Arc::clone(&guard.bytecode),
                    guard.state == crate::module_loader::ModuleState::Evaluated,
                )
            };

            // Skip native modules that are already evaluated (v2 extensions)
            if already_evaluated {
                continue;
            }

            // Resolve imports for this dependency
            let dep_locals = self.resolve_module_imports(&module_bytecode, url)?;

            self.vm
                .execute_module_with_context_and_locals(
                    &module_bytecode,
                    &mut ctx,
                    if dep_locals.is_empty() {
                        None
                    } else {
                        Some(dep_locals)
                    },
                )
                .map_err(|e: otter_vm_core::VmError| OtterError::Runtime(e.to_string()))?;

            self.loader.update_namespace(url, &ctx);
        }

        // Resolve imports for main module
        let initial_locals = self.resolve_module_imports(&bytecode, main_url)?;
        let result = self
            .vm
            .execute_module_with_context_and_locals(
                &bytecode,
                &mut ctx,
                if initial_locals.is_empty() {
                    None
                } else {
                    Some(initial_locals)
                },
            )
            .map_err(|e: otter_vm_core::VmError| OtterError::Runtime(e.to_string()))?;

        self.loader.update_namespace(main_url, &ctx);

        Ok(result)
    }

    /// Resolve import bindings for a module into initial locals map.
    ///
    /// Walks the module's import records, resolves each to its source module,
    /// and maps imported values to local variable indices.
    /// Used by both `eval()` and `eval_sync()`.
    fn resolve_module_imports(
        &self,
        bytecode: &otter_vm_bytecode::Module,
        module_url: &str,
    ) -> Result<HashMap<u16, Value>, OtterError> {
        let mut initial_locals = HashMap::new();
        let entry_func = match bytecode.entry_function() {
            Some(f) => f,
            None => return Ok(initial_locals),
        };

        let mm = self.vm.memory_manager().clone();

        for import in &bytecode.imports {
            let resolved = self
                .loader
                .resolve(&import.specifier, module_url)
                .map_err(|e| OtterError::Runtime(format!("Import resolution failed: {}", e)))?;

            let module_arc = match self.loader.get(&resolved.url) {
                Some(m) => m,
                None => continue,
            };
            let module_lock = module_arc.read().map_err(|e| {
                OtterError::Runtime(format!("Failed to read module: {}", e))
            })?;

            for binding in &import.bindings {
                let (imported_name, local_name, is_namespace) = match binding {
                    ImportBinding::Named { imported, local } => {
                        (imported.as_str(), local.as_str(), false)
                    }
                    ImportBinding::Default { local } => ("default", local.as_str(), false),
                    ImportBinding::Namespace { local } => ("*", local.as_str(), true),
                };

                let value = if is_namespace {
                    // Build namespace object from all exports
                    crate::module_loader::namespace_to_object(
                        &module_lock.namespace,
                        mm.clone(),
                    )
                } else {
                    match module_lock.get_export(imported_name) {
                        Some(val) => val,
                        None => continue,
                    }
                };

                if let Some(idx) =
                    entry_func.local_names.iter().position(|n| n == local_name)
                {
                    initial_locals.insert(idx as u16, value);
                }
            }
        }

        Ok(initial_locals)
    }

    /// Bootstrap v2 native extensions (no JS compilation needed).
    ///
    /// Creates a `RegistrationContext` and runs the full bootstrap sequence
    /// (init_state → allocate → install) for all registered v2 extensions.
    /// Then builds native module namespaces and registers them with the loader.
    fn bootstrap_native_extensions(&mut self, ctx: &VmContext) -> Result<(), OtterError> {
        if self.native_extensions.extension_count() == 0 {
            return Ok(());
        }

        let intrinsics = self.vm.intrinsics();
        let global = ctx.global().clone();
        let mm = self.vm.memory_manager().clone();

        // Determine profile — for now default to Full, will be configurable later
        let profile = crate::extension_v2::Profile::Full;

        let mut reg_ctx = crate::registration::RegistrationContext::new(
            intrinsics,
            global,
            mm.clone(),
            &mut self.extension_state,
        );

        // Phase 1: bootstrap (init_state → allocate → install)
        self.native_extensions
            .bootstrap(&mut reg_ctx, profile)
            .map_err(|e| OtterError::Runtime(e.to_string()))?;

        // Phase 2: build native module namespaces and register with loader
        // Collect specifiers from all extensions, then load each module
        let mut specifier_map: Vec<(String, usize)> = Vec::new();
        for (idx, ext) in self.native_extensions.extensions().iter().enumerate() {
            for &spec in ext.module_specifiers() {
                specifier_map.push((spec.to_string(), idx));
            }
        }

        for (specifier, idx) in &specifier_map {
            let ext = &self.native_extensions.extensions()[*idx];
            let mut load_ctx = crate::registration::RegistrationContext::new(
                intrinsics,
                ctx.global().clone(),
                mm.clone(),
                &mut self.extension_state,
            );
            if let Some(namespace_obj) = ext.load_module(specifier, &mut load_ctx) {
                // Register with the loader using the builtin:// URL format
                // that NodeModuleProvider produces: "builtin://node:path"
                let builtin_url = format!("builtin://{}", specifier);
                self.loader.register_native_module(&builtin_url, namespace_obj);
            }
        }

        Ok(())
    }

    /// Register extension ops as global native functions in context
    fn register_ops_in_context(&self, ctx: &mut VmContext) {
        let global = ctx.global().clone();
        let pending_ops = self.event_loop.get_pending_async_ops_count();
        let fn_proto = ctx
            .function_prototype()
            .unwrap_or_else(|| self.vm.function_prototype());

        for op_name in self.extensions.op_names() {
            if let Some(handler) = self.extensions.get_op(op_name) {
                let native_fn = self.create_native_wrapper(
                    op_name,
                    handler.clone(),
                    Arc::clone(&pending_ops),
                    self.vm.memory_manager().clone(),
                    fn_proto,
                );
                let _ = global.set(PropertyKey::string(op_name), native_fn);
            }
        }

        // Also register environment access if capabilities allow
        self.register_env_access(global, fn_proto);

        let ctx_ptr = ctx as *mut VmContext as usize;
        let vm_ptr = &self.vm as *const VmRuntime as usize;
        let otter_ptr = self as *const Otter as usize;
        let realms_store = Arc::clone(&self.realms);
        let mm_eval = self.vm.memory_manager().clone();
        let mm_eval_closure = mm_eval.clone();
        let _ = global.set(
            PropertyKey::string("__otter_eval"),
            Value::native_function_with_proto(
                move |_this: &Value, args: &[Value], _mm| {
                    let mm_result = mm_eval_closure.clone();
                    let result_ok = |value: Value| {
                        let obj = JsObject::new(Value::null(), mm_result.clone());
                        let _ = obj.set(PropertyKey::string("ok"), Value::boolean(true));
                        let _ = obj.set(PropertyKey::string("value"), value);
                        Value::object(GcRef::new(obj))
                    };

                    let result_err = |error_type: &str, message: &str| {
                        let obj = JsObject::new(Value::null(), mm_result.clone());
                        let _ = obj.set(PropertyKey::string("ok"), Value::boolean(false));
                        let _ = obj.set(
                            PropertyKey::string("errorType"),
                            Value::string(JsString::intern(error_type)),
                        );
                        let _ = obj.set(
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

                        // Determine if we're in strict mode context (for direct eval)
                        // Per ES2023 §19.2.1.1: Direct eval inherits strict mode from calling context
                        let is_strict_context = ctx
                            .current_frame()
                            .and_then(|frame| {
                                frame.module.functions.get(frame.function_index as usize)
                            })
                            .map(|func| func.flags.is_strict)
                            .unwrap_or(false);

                        let compiler = Compiler::new();
                        let module = match compiler.compile(&code, "<eval>", is_strict_context) {
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

        // Create a new realm and return { global, evalScript }
        let mm_realm = self.vm.memory_manager().clone();
        let fn_proto_realm = fn_proto;
        let realms_for_create = Arc::clone(&realms_store);
        let mm_realm_for_create = mm_realm.clone();
        let _ = global.set(
            PropertyKey::string("__otter_create_realm"),
            Value::native_function_with_proto(
                move |_this: &Value, _args: &[Value], _mm| {
                    unsafe {
                        let otter = &*(otter_ptr as *const Otter);
                        let realm_id = otter.vm.create_realm();
                        let mut realm_ctx = otter.vm.create_context_in_realm(realm_id);
                        otter
                            .setup_context(&mut realm_ctx)
                            .map_err(|e| VmError::internal(e.to_string()))?;

                        let realm_global = realm_ctx.global();

                        // Store the realm context for future evalScript calls.
                        {
                            let mut guard = realms_for_create.lock();
                            guard.push(RealmSlot {
                                id: realm_id,
                                ctx: realm_ctx,
                            });
                        }

                        let realms_for_eval = Arc::clone(&realms_for_create);
                        let mm_eval_realm = mm_realm_for_create.clone();
                        let eval_fn = Value::native_function_with_proto(
                            move |_this: &Value, args: &[Value], _mm| {
                                let code_value = match args.first() {
                                    Some(value) => value.clone(),
                                    None => return Ok(Value::undefined()),
                                };

                                if !code_value.is_string() {
                                    return Ok(code_value);
                                }

                                let code = code_value
                                    .as_string()
                                    .map(|s| s.as_str().to_string())
                                    .unwrap_or_default();

                                let otter = &*(otter_ptr as *const Otter);
                                let mut guard = realms_for_eval.lock();
                                let slot = guard
                                    .iter_mut()
                                    .find(|slot| slot.id == realm_id)
                                    .ok_or_else(|| VmError::internal("Realm not found"))?;
                                otter
                                    .eval_in_context(&mut slot.ctx, &code)
                                    .map_err(|e| VmError::type_error(e.to_string()))
                            },
                            mm_eval_realm,
                            fn_proto_realm,
                        );

                        let realm_obj =
                            GcRef::new(JsObject::new(Value::null(), mm_realm_for_create.clone()));
                        let _ = realm_obj
                            .set(PropertyKey::string("global"), Value::object(realm_global));
                        let _ = realm_obj.set(PropertyKey::string("evalScript"), eval_fn);
                        Ok(Value::object(realm_obj))
                    }
                },
                mm_realm.clone(),
                fn_proto_realm,
            ),
        );

        // Create console object from __console_* ops
        let console_obj = GcRef::new(JsObject::new(
            Value::null(),
            self.vm.memory_manager().clone(),
        ));

        // Helper to wire console methods from global __console_* functions
        let wire_console = |method_name: &str, global_name: &str| {
            if let Some(func) = global.get(&PropertyKey::string(global_name)) {
                let _ = console_obj.set(PropertyKey::string(method_name), func);
            }
        };

        wire_console("log", "__console_log");
        wire_console("error", "__console_error");
        wire_console("warn", "__console_warn");
        wire_console("info", "__console_info");
        wire_console("debug", "__console_debug");
        wire_console("trace", "__console_trace");
        wire_console("time", "__console_time");
        wire_console("timeEnd", "__console_timeEnd");
        wire_console("timeLog", "__console_timeLog");
        wire_console("assert", "__console_assert");
        wire_console("clear", "__console_clear");
        wire_console("count", "__console_count");
        wire_console("countReset", "__console_countReset");
        wire_console("table", "__console_table");
        wire_console("dir", "__console_dir");
        wire_console("dirxml", "__console_dirxml");

        // group/groupCollapsed/groupEnd alias to log
        if let Some(log_fn) = global.get(&PropertyKey::string("__console_log")) {
            let _ = console_obj.set(PropertyKey::string("group"), log_fn.clone());
            let _ = console_obj.set(PropertyKey::string("groupCollapsed"), log_fn.clone());
            let _ = console_obj.set(PropertyKey::string("groupEnd"), log_fn);
        }

        // Install console on global
        let _ = global.set(PropertyKey::string("console"), Value::object(console_obj));

        // NOTE: Temporal namespace creation moved to intrinsics.rs install_on_global()
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
                    move |_this, args, ncx| native_fn(args, ncx.memory_manager().clone()),
                    mm,
                    fn_proto,
                )
            }
            OpHandler::Sync(sync_fn) => {
                // Sync JSON ops need Value -> JSON -> Value conversion
                let mm_inner = mm.clone();
                Value::native_function_with_proto(
                    move |_this, args, _mm_ignored| {
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
                let js_queue = Arc::clone(self.event_loop.js_job_queue());
                Value::native_function_with_proto(
                    move |_this, args, _mm_ignored| {
                        let mm_promise = mm_outer_closure.clone();
                        let js_queue = Arc::clone(&js_queue);
                        let resolvers = JsPromise::with_resolvers_with_js_jobs(
                            mm_promise.clone(),
                            move |job, job_args| {
                                js_queue.enqueue(job, job_args);
                            },
                        );
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
        let _ = global.set(
            PropertyKey::string("__env_get"),
            Value::native_function_with_proto(
                move |_this: &Value, args: &[Value], _mm| {
                    let key = args
                        .first()
                        .and_then(|v| v.as_string())
                        .map(|s| s.as_str().to_string())
                        .ok_or_else(|| "env_get requires a string key".to_string())?;

                    if !caps_get.can_env(&key) {
                        return Err(VmError::type_error(format!(
                            "Permission denied: env access to '{}'",
                            key
                        )));
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
        let _ = global.set(
            PropertyKey::string("__env_keys"),
            Value::native_function_with_proto(
                move |_this: &Value, _args: &[Value], _mm| {
                    let keys = env_store_keys.keys();
                    let arr = JsObject::array(keys.len(), mm_keys_closure.clone());
                    for (i, key) in keys.into_iter().enumerate() {
                        let _ = arr.set(
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
        let _ = global.set(
            PropertyKey::string("__env_has"),
            Value::native_function_with_proto(
                move |_this: &Value, args: &[Value], _mm| {
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
            .compile(code, source_url, false) // Non-strict context for top-level code
            .map_err(|e| OtterError::Compile(e.to_string()))?;

        self.vm
            .execute_module_with_context(&module, ctx)
            .map_err(|e| OtterError::Runtime(e.to_string()))
    }

    /// Create a persistent execution context with all extensions registered.
    /// The caller owns the context and can reuse it across multiple `eval_in_context` calls.
    pub fn create_test_context(&self) -> Result<VmContext, OtterError> {
        let mut ctx = self.vm.create_context();
        self.setup_context(&mut ctx)?;
        Ok(ctx)
    }

    /// Configure a context with runtime hooks, ops, and extension setup JS.
    fn setup_context(&self, ctx: &mut VmContext) -> Result<(), OtterError> {
        ctx.set_interrupt_flag(Arc::clone(&self.interrupt_flag));
        ctx.set_debug_snapshot_target(Some(Arc::clone(&self.debug_snapshot)));
        Self::configure_eval(ctx);
        Self::configure_js_job_queue(ctx, &self.event_loop);

        // Register extension ops as global native functions
        self.register_ops_in_context(ctx);

        // Execute setup JS from extensions
        for js in self.extensions.all_js() {
            self.execute_js(ctx, js, "setup.js")?;
        }

        // ES spec: Drain microtasks after extension setup JS execution
        self.drain_microtasks(ctx)?;
        Ok(())
    }

    /// Execute JS code in an existing context (no context creation/teardown).
    pub fn eval_in_context(&self, ctx: &mut VmContext, code: &str) -> Result<Value, OtterError> {
        self.clear_interrupt();
        let compiler = Compiler::new();
        let module = compiler
            .compile_eval(code, "eval.js", false)
            .map_err(|e| OtterError::Compile(e.to_string()))?;

        // Execute in provided context
        let result = self
            .vm
            .execute_module_with_context(&module, ctx)
            .map_err(|e| OtterError::Runtime(e.to_string()))?;

        // ES spec: Drain microtasks after script execution
        self.drain_microtasks(ctx)?;

        Ok(result)
    }

    /// Drains all pending microtasks until the queue is empty.
    /// This is a critical synchronization point for ES spec compliance.
    ///
    /// Drain microtasks and JS callback jobs.
    ///
    /// JS callback jobs (Promise callbacks) are executed FIRST with highest priority,
    /// then Rust microtasks. Both are executed in FIFO order.
    ///
    /// New microtasks/jobs enqueued during execution are also drained in the same call.
    ///
    /// If a task panics or errors, the error is captured and the first error is returned.
    /// Remaining tasks continue to execute.
    fn drain_microtasks(&self, ctx: &mut VmContext) -> Result<(), OtterError> {
        use std::panic::{AssertUnwindSafe, catch_unwind};

        // // eprintln!("DEBUG: drain_microtasks called");

        let mut first_error: Option<String> = None;
        let mut interpreter = Interpreter::new();
        let event_loop = Arc::clone(&self.event_loop);
        let memory_manager = ctx.memory_manager().clone();
        let fn_proto = ctx.function_prototype();

        let make_js_enqueuer = || {
            let js_queue = Arc::clone(event_loop.js_job_queue());
            move |job, args| {
                js_queue.enqueue(job, args);
            }
        };
        let js_queue = Arc::clone(event_loop.js_job_queue());

        let make_microtask_enqueuer = || {
            let event_loop = Arc::clone(&event_loop);
            move |task| {
                event_loop.queue_microtask(task);
            }
        };

        let get_then_property = |interpreter: &mut Interpreter,
                                 ctx: &mut VmContext,
                                 value: &Value|
         -> Result<Value, VmError> {
            let key = PropertyKey::string("then");

            if let Some(proxy) = value.as_proxy() {
                let key_value = Value::string(JsString::intern("then"));
                let mut ncx = otter_vm_core::context::NativeContext::new(ctx, interpreter);
                return otter_vm_core::proxy_operations::proxy_get(
                    &mut ncx,
                    proxy,
                    &key,
                    key_value,
                    value.clone(),
                );
            }

            let Some(obj) = value.as_object() else {
                return Ok(Value::undefined());
            };

            match obj.lookup_property_descriptor(&key) {
                Some(PropertyDescriptor::Accessor { get, .. }) => {
                    let Some(getter) = get else {
                        return Ok(Value::undefined());
                    };
                    interpreter.call_function(ctx, &getter, value.clone(), &[])
                }
                Some(PropertyDescriptor::Data {
                    value: prop_value, ..
                }) => Ok(prop_value),
                _ => Ok(Value::undefined()),
            }
        };

        let resolve_result_promise = |interpreter: &mut Interpreter,
                                      ctx: &mut VmContext,
                                      result_promise: GcRef<JsPromise>,
                                      value: Value| {
            if let Some(promise) = value.as_promise() {
                if std::ptr::eq(promise.as_ptr(), result_promise.as_ptr()) {
                    let error_val =
                        make_error_value(ctx, "TypeError", "Promise cannot resolve itself");
                    JsPromise::reject_with_js_jobs(result_promise, error_val, make_js_enqueuer());
                    return;
                }

                let result_clone = result_promise.clone();
                let enqueue_js = make_js_enqueuer();
                let enqueue_microtask = make_microtask_enqueuer();
                promise.then_with_enqueue(
                    move |v| {
                        let job = JsPromiseJob {
                            kind: JsPromiseJobKind::PassthroughFulfill,
                            callback: Value::undefined(),
                            this_arg: Value::undefined(),
                            result_promise: Some(result_clone.clone()),
                        };
                        enqueue_js(job, vec![v]);
                    },
                    enqueue_microtask,
                );

                let result_clone = result_promise;
                let enqueue_js = make_js_enqueuer();
                let enqueue_microtask = make_microtask_enqueuer();
                promise.catch_with_enqueue(
                    move |e| {
                        let job = JsPromiseJob {
                            kind: JsPromiseJobKind::PassthroughReject,
                            callback: Value::undefined(),
                            this_arg: Value::undefined(),
                            result_promise: Some(result_clone.clone()),
                        };
                        enqueue_js(job, vec![e]);
                    },
                    enqueue_microtask,
                );
                return;
            }

            if value.is_object() {
                match get_then_property(interpreter, ctx, &value) {
                    Ok(then_val) if then_val.is_callable() => {
                        let job = JsPromiseJob {
                            kind: JsPromiseJobKind::ResolveThenable,
                            callback: then_val,
                            this_arg: value,
                            result_promise: Some(result_promise),
                        };
                        make_js_enqueuer()(job, Vec::new());
                        return;
                    }
                    Ok(_) => {}
                    Err(vm_err) => {
                        let error_val = vm_error_to_value(ctx, vm_err);
                        JsPromise::reject_with_js_jobs(
                            result_promise,
                            error_val,
                            make_js_enqueuer(),
                        );
                        return;
                    }
                }
            }

            JsPromise::resolve_with_js_jobs(result_promise, value, make_js_enqueuer());
        };

        let call_thenable = |interpreter: &mut Interpreter,
                             ctx: &mut VmContext,
                             then_fn: Value,
                             then_this: Value,
                             promise: GcRef<JsPromise>,
                             first_error: &mut Option<String>| {
            if !then_fn.is_callable() {
                let js_queue = Arc::clone(&js_queue);
                JsPromise::fulfill_with_js_jobs(promise, then_this, move |job, args| {
                    js_queue.enqueue(job, args);
                });
                return;
            }

            let called = Arc::new(AtomicBool::new(false));

            let resolve_fn = {
                let called = Arc::clone(&called);
                let result_promise = promise.clone();
                let js_queue = Arc::clone(&js_queue);
                if let Some(proto) = fn_proto {
                    Value::native_function_with_proto(
                        move |_this, args, _mm| {
                            if called.swap(true, Ordering::AcqRel) {
                                return Ok(Value::undefined());
                            }
                            let value = args.get(0).cloned().unwrap_or(Value::undefined());
                            let js_queue = Arc::clone(&js_queue);
                            JsPromise::resolve_from_thenable_with_js_jobs(
                                result_promise,
                                value,
                                move |job, args| {
                                    js_queue.enqueue(job, args);
                                },
                            );
                            Ok(Value::undefined())
                        },
                        memory_manager.clone(),
                        proto,
                    )
                } else {
                    Value::native_function(
                        move |_this, args, _mm| {
                            if called.swap(true, Ordering::AcqRel) {
                                return Ok(Value::undefined());
                            }
                            let value = args.get(0).cloned().unwrap_or(Value::undefined());
                            let js_queue = Arc::clone(&js_queue);
                            JsPromise::resolve_from_thenable_with_js_jobs(
                                result_promise,
                                value,
                                move |job, args| {
                                    js_queue.enqueue(job, args);
                                },
                            );
                            Ok(Value::undefined())
                        },
                        memory_manager.clone(),
                    )
                }
            };

            let reject_fn = {
                let called = Arc::clone(&called);
                let result_promise = promise.clone();
                let js_queue = Arc::clone(&js_queue);
                if let Some(proto) = fn_proto {
                    Value::native_function_with_proto(
                        move |_this, args, _mm| {
                            if called.swap(true, Ordering::AcqRel) {
                                return Ok(Value::undefined());
                            }
                            let value = args.get(0).cloned().unwrap_or(Value::undefined());
                            let js_queue = Arc::clone(&js_queue);
                            JsPromise::reject_from_thenable_with_js_jobs(
                                result_promise,
                                value,
                                move |job, args| {
                                    js_queue.enqueue(job, args);
                                },
                            );
                            Ok(Value::undefined())
                        },
                        memory_manager.clone(),
                        proto,
                    )
                } else {
                    Value::native_function(
                        move |_this, args, _mm| {
                            if called.swap(true, Ordering::AcqRel) {
                                return Ok(Value::undefined());
                            }
                            let value = args.get(0).cloned().unwrap_or(Value::undefined());
                            let js_queue = Arc::clone(&js_queue);
                            JsPromise::reject_from_thenable_with_js_jobs(
                                result_promise,
                                value,
                                move |job, args| {
                                    js_queue.enqueue(job, args);
                                },
                            );
                            Ok(Value::undefined())
                        },
                        memory_manager.clone(),
                    )
                }
            };

            let result = catch_unwind(AssertUnwindSafe(|| {
                interpreter.call_function(ctx, &then_fn, then_this, &[resolve_fn, reject_fn])
            }));

            let mut runtime_error: Option<String> = None;

            match result {
                Ok(Ok(_)) => {}
                Ok(Err(vm_err)) => {
                    if !called.load(Ordering::Acquire) {
                        let err_msg = vm_err.to_string();
                        let error_val = vm_error_to_value(ctx, vm_err);
                        JsPromise::reject_from_thenable_with_js_jobs(
                            promise,
                            error_val,
                            make_js_enqueuer(),
                        );
                        runtime_error = Some(format!("Error in thenable resolve: {}", err_msg));
                    }
                }
                Err(panic_err) => {
                    if !called.load(Ordering::Acquire) {
                        let error_msg = if let Some(s) = panic_err.downcast_ref::<&str>() {
                            format!("Panic in thenable resolve: {}", s)
                        } else if let Some(s) = panic_err.downcast_ref::<String>() {
                            format!("Panic in thenable resolve: {}", s)
                        } else {
                            "Unknown panic in thenable resolve".to_string()
                        };
                        let error_val = Value::string(JsString::intern(&error_msg));
                        JsPromise::reject_from_thenable_with_js_jobs(
                            promise,
                            error_val,
                            make_js_enqueuer(),
                        );
                        runtime_error = Some(error_msg);
                    }
                }
            }

            if let Some(err) = runtime_error {
                if first_error.is_none() {
                    *first_error = Some(err);
                }
            }
        };

        loop {
            let next_js = self.event_loop.js_job_queue().peek_seq();
            let next_rust = self.event_loop.microtask_queue().peek_seq();

            let run_js = match (next_js, next_rust) {
                (None, None) => break,
                (Some(_), None) => true,
                (None, Some(_)) => false,
                (Some(js_seq), Some(rust_seq)) => js_seq <= rust_seq,
            };

            if run_js {
                let Some(job) = self.event_loop.js_job_queue().dequeue() else {
                    continue;
                };
                let crate::microtask::JsCallbackJob { args, job } = job;
                let otter_vm_core::promise::JsPromiseJob {
                    kind,
                    callback,
                    this_arg,
                    result_promise,
                } = job;

                let passthrough_value = args.get(0).cloned().unwrap_or(Value::undefined());

                match kind {
                    JsPromiseJobKind::PassthroughFulfill => {
                        if let Some(promise) = result_promise {
                            JsPromise::resolve_from_thenable_with_js_jobs(
                                promise,
                                passthrough_value,
                                make_js_enqueuer(),
                            );
                        }
                        continue;
                    }
                    JsPromiseJobKind::PassthroughReject => {
                        if let Some(promise) = result_promise {
                            JsPromise::reject_from_thenable_with_js_jobs(
                                promise,
                                passthrough_value,
                                make_js_enqueuer(),
                            );
                        }
                        continue;
                    }
                    JsPromiseJobKind::ResolveThenableLookup => {
                        let Some(promise) = result_promise else {
                            continue;
                        };

                        let then_val = get_then_property(&mut interpreter, ctx, &this_arg);
                        match then_val {
                            Ok(then_fn) => {
                                call_thenable(
                                    &mut interpreter,
                                    ctx,
                                    then_fn,
                                    this_arg,
                                    promise,
                                    &mut first_error,
                                );
                            }
                            Err(vm_err) => {
                                let error_val = vm_error_to_value(ctx, vm_err);
                                JsPromise::reject_from_thenable_with_js_jobs(
                                    promise,
                                    error_val,
                                    make_js_enqueuer(),
                                );
                            }
                        }
                        continue;
                    }
                    JsPromiseJobKind::FinallyFulfill => {
                        let Some(promise) = result_promise else {
                            continue;
                        };
                        let original_value = passthrough_value;

                        let result = catch_unwind(AssertUnwindSafe(|| {
                            interpreter.call_function(ctx, &callback, this_arg, &[])
                        }));

                        let mut runtime_error: Option<String> = None;

                        match result {
                            Ok(Ok(value)) => {
                                let gate_promise = JsPromise::new();
                                resolve_result_promise(
                                    &mut interpreter,
                                    ctx,
                                    gate_promise.clone(),
                                    value,
                                );

                                let enqueue_microtask = make_microtask_enqueuer();
                                let enqueue_js = make_js_enqueuer();
                                let result_clone = promise.clone();
                                let original_clone = original_value.clone();
                                gate_promise.then_with_enqueue(
                                    move |_| {
                                        let job = JsPromiseJob {
                                            kind: JsPromiseJobKind::PassthroughFulfill,
                                            callback: Value::undefined(),
                                            this_arg: Value::undefined(),
                                            result_promise: Some(result_clone.clone()),
                                        };
                                        enqueue_js(job, vec![original_clone.clone()]);
                                    },
                                    enqueue_microtask,
                                );

                                let enqueue_microtask = make_microtask_enqueuer();
                                let enqueue_js = make_js_enqueuer();
                                let result_clone = promise.clone();
                                gate_promise.catch_with_enqueue(
                                    move |e| {
                                        let job = JsPromiseJob {
                                            kind: JsPromiseJobKind::PassthroughReject,
                                            callback: Value::undefined(),
                                            this_arg: Value::undefined(),
                                            result_promise: Some(result_clone.clone()),
                                        };
                                        enqueue_js(job, vec![e]);
                                    },
                                    enqueue_microtask,
                                );
                            }
                            Ok(Err(vm_err)) => {
                                runtime_error =
                                    Some(format!("Error in finally callback: {}", vm_err));
                                let error_val = vm_error_to_value(ctx, vm_err);
                                JsPromise::reject_with_js_jobs(
                                    promise,
                                    error_val,
                                    make_js_enqueuer(),
                                );
                            }
                            Err(panic_err) => {
                                let error_msg = if let Some(s) = panic_err.downcast_ref::<&str>() {
                                    format!("Panic in finally callback: {}", s)
                                } else if let Some(s) = panic_err.downcast_ref::<String>() {
                                    format!("Panic in finally callback: {}", s)
                                } else {
                                    "Unknown panic in finally callback".to_string()
                                };
                                let error_val = Value::string(JsString::intern(&error_msg));
                                JsPromise::reject_with_js_jobs(
                                    promise,
                                    error_val,
                                    make_js_enqueuer(),
                                );
                                runtime_error = Some(error_msg);
                            }
                        }

                        if let Some(err) = runtime_error {
                            if first_error.is_none() {
                                first_error = Some(err);
                            }
                        }

                        continue;
                    }
                    JsPromiseJobKind::FinallyReject => {
                        let Some(promise) = result_promise else {
                            continue;
                        };
                        let original_reason = passthrough_value;

                        let result = catch_unwind(AssertUnwindSafe(|| {
                            interpreter.call_function(ctx, &callback, this_arg, &[])
                        }));

                        let mut runtime_error: Option<String> = None;

                        match result {
                            Ok(Ok(value)) => {
                                let gate_promise = JsPromise::new();
                                resolve_result_promise(
                                    &mut interpreter,
                                    ctx,
                                    gate_promise.clone(),
                                    value,
                                );

                                let enqueue_microtask = make_microtask_enqueuer();
                                let enqueue_js = make_js_enqueuer();
                                let result_clone = promise.clone();
                                let original_clone = original_reason.clone();
                                gate_promise.then_with_enqueue(
                                    move |_| {
                                        let job = JsPromiseJob {
                                            kind: JsPromiseJobKind::PassthroughReject,
                                            callback: Value::undefined(),
                                            this_arg: Value::undefined(),
                                            result_promise: Some(result_clone.clone()),
                                        };
                                        enqueue_js(job, vec![original_clone.clone()]);
                                    },
                                    enqueue_microtask,
                                );

                                let enqueue_microtask = make_microtask_enqueuer();
                                let enqueue_js = make_js_enqueuer();
                                let result_clone = promise.clone();
                                gate_promise.catch_with_enqueue(
                                    move |e| {
                                        let job = JsPromiseJob {
                                            kind: JsPromiseJobKind::PassthroughReject,
                                            callback: Value::undefined(),
                                            this_arg: Value::undefined(),
                                            result_promise: Some(result_clone.clone()),
                                        };
                                        enqueue_js(job, vec![e]);
                                    },
                                    enqueue_microtask,
                                );
                            }
                            Ok(Err(vm_err)) => {
                                runtime_error =
                                    Some(format!("Error in finally callback: {}", vm_err));
                                let error_val = vm_error_to_value(ctx, vm_err);
                                JsPromise::reject_with_js_jobs(
                                    promise,
                                    error_val,
                                    make_js_enqueuer(),
                                );
                            }
                            Err(panic_err) => {
                                let error_msg = if let Some(s) = panic_err.downcast_ref::<&str>() {
                                    format!("Panic in finally callback: {}", s)
                                } else if let Some(s) = panic_err.downcast_ref::<String>() {
                                    format!("Panic in finally callback: {}", s)
                                } else {
                                    "Unknown panic in finally callback".to_string()
                                };
                                let error_val = Value::string(JsString::intern(&error_msg));
                                JsPromise::reject_with_js_jobs(
                                    promise,
                                    error_val,
                                    make_js_enqueuer(),
                                );
                                runtime_error = Some(error_msg);
                            }
                        }

                        if let Some(err) = runtime_error {
                            if first_error.is_none() {
                                first_error = Some(err);
                            }
                        }

                        continue;
                    }
                    JsPromiseJobKind::ResolveThenable => {
                        let Some(promise) = result_promise else {
                            continue;
                        };

                        call_thenable(
                            &mut interpreter,
                            ctx,
                            callback,
                            this_arg,
                            promise,
                            &mut first_error,
                        );
                        continue;
                    }
                    _ => {}
                }

                if !callback.is_callable() {
                    if let Some(promise) = result_promise {
                        match kind {
                            JsPromiseJobKind::Reject | JsPromiseJobKind::FinallyReject => {
                                JsPromise::reject_from_thenable_with_js_jobs(
                                    promise,
                                    passthrough_value,
                                    make_js_enqueuer(),
                                );
                            }
                            _ => {
                                JsPromise::resolve_from_thenable_with_js_jobs(
                                    promise,
                                    passthrough_value,
                                    make_js_enqueuer(),
                                );
                            }
                        }
                    }
                    continue;
                }

                let result = catch_unwind(AssertUnwindSafe(|| {
                    interpreter.call_function(ctx, &callback, this_arg, &args)
                }));

                let mut runtime_error: Option<String> = None;

                if let Some(promise) = result_promise {
                    match result {
                        Ok(Ok(value)) => {
                            resolve_result_promise(&mut interpreter, ctx, promise, value);
                        }
                        Ok(Err(vm_err)) => {
                            runtime_error = Some(format!("Error in JS callback: {}", vm_err));
                            let error_val = vm_error_to_value(ctx, vm_err);
                            JsPromise::reject_with_js_jobs(promise, error_val, make_js_enqueuer());
                        }
                        Err(panic_err) => {
                            let error_msg = if let Some(s) = panic_err.downcast_ref::<&str>() {
                                format!("Panic in JS callback: {}", s)
                            } else if let Some(s) = panic_err.downcast_ref::<String>() {
                                format!("Panic in JS callback: {}", s)
                            } else {
                                "Unknown panic in JS callback".to_string()
                            };
                            let error_val = Value::string(JsString::intern(&error_msg));
                            JsPromise::reject_with_js_jobs(promise, error_val, make_js_enqueuer());
                            runtime_error = Some(error_msg);
                        }
                    }
                } else if let Err(panic_err) = result {
                    let error_msg = if let Some(s) = panic_err.downcast_ref::<&str>() {
                        format!("Panic in JS callback: {}", s)
                    } else if let Some(s) = panic_err.downcast_ref::<String>() {
                        format!("Panic in JS callback: {}", s)
                    } else {
                        "Unknown panic in JS callback".to_string()
                    };
                    runtime_error = Some(error_msg);
                } else if let Ok(Err(vm_err)) = result {
                    runtime_error = Some(format!("Error in JS callback: {}", vm_err));
                }

                if let Some(err) = runtime_error {
                    if first_error.is_none() {
                        first_error = Some(err);
                    }
                }
            } else {
                let Some(task) = self.event_loop.microtask_queue().dequeue() else {
                    continue;
                };

                let result = catch_unwind(AssertUnwindSafe(|| {
                    task();
                }));

                if let Err(panic_err) = result {
                    if first_error.is_none() {
                        let error_msg = if let Some(s) = panic_err.downcast_ref::<&str>() {
                            format!("Panic in microtask: {}", s)
                        } else if let Some(s) = panic_err.downcast_ref::<String>() {
                            format!("Panic in microtask: {}", s)
                        } else {
                            "Unknown panic in microtask".to_string()
                        };
                        first_error = Some(error_msg);
                    }
                }
            }
        }

        // Return first error if any occurred
        if let Some(err) = first_error {
            Err(OtterError::Runtime(err))
        } else {
            Ok(())
        }
    }

    /// Configure the eval compiler callback on a VmContext so that `eval()`
    /// and `CallEval` bytecode can compile code at runtime.
    /// The interpreter handles execution with proper stack depth tracking.
    fn configure_eval(ctx: &mut VmContext) {
        ctx.set_eval_fn(Arc::new(|code: &str, strict_context: bool| {
            let compiler = Compiler::new();
            compiler
                .compile_eval(code, "<eval>", strict_context)
                .map_err(|e| VmError::SyntaxError(e.to_string()))
        }));
    }

    /// Configure the JS job queue on a VmContext to enable Promise callbacks
    fn configure_js_job_queue(ctx: &mut VmContext, event_loop: &Arc<EventLoop>) {
        let wrapper = JsJobQueueWrapper::new(Arc::clone(event_loop.js_job_queue()));
        let queue: Arc<dyn otter_vm_core::context::JsJobQueueTrait + Send + Sync> = wrapper.clone();
        ctx.set_js_job_queue(queue);
        ctx.register_external_root_set(wrapper);
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
            .compile_eval(code, source_url, false)
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

    /// Create a MemoryProfiler connected to the global GC registry
    #[cfg(feature = "profiling")]
    pub fn create_memory_profiler_with_gc() -> otter_profiler::MemoryProfiler {
        use otter_profiler::{HeapInfo, MemoryProfiler};

        let provider = std::sync::Arc::new(|| HeapInfo {
            total_allocated: otter_vm_gc::global_registry().total_bytes(),
            objects_by_type: std::collections::HashMap::new(),
            object_count: otter_vm_gc::global_registry().allocation_count(),
        });

        MemoryProfiler::with_heap_provider(provider)
    }

    /// Clear the global string intern table and deallocate all GC objects.
    ///
    /// **WARNING**: This invalidates ALL `GcRef` pointers system-wide,
    /// including those held by this engine. Only call this when the engine
    /// is about to be replaced (e.g. between test runs).
    ///
    /// The correct sequence is:
    /// 1. Drop or replace this `Otter` instance (releases Rust-side refs)
    /// 2. Call `Otter::reset_gc()` (frees all GC memory)
    /// 3. Create a fresh engine
    pub fn reset_gc() {
        otter_vm_core::clear_global_string_table();
        otter_vm_gc::global_registry().dealloc_all();
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
                let _ = js_arr.set(
                    PropertyKey::Index(i as u32),
                    json_to_value(elem, mm.clone()),
                );
            }
            Value::object(GcRef::new(js_arr))
        }
        serde_json::Value::Object(obj) => {
            let js_obj = JsObject::new(Value::null(), mm.clone());
            for (key, val) in obj {
                let _ = js_obj.set(PropertyKey::string(key), json_to_value(val, mm.clone()));
            }
            Value::object(GcRef::new(js_obj))
        }
    }
}

fn make_error_value(ctx: &VmContext, name: &str, message: &str) -> Value {
    let ctor_value = ctx.get_global(name);
    let proto = ctor_value
        .as_ref()
        .and_then(|v| v.as_object())
        .and_then(|obj| obj.get(&PropertyKey::string("prototype")))
        .unwrap_or_else(Value::null);

    let obj = GcRef::new(JsObject::new(proto, ctx.memory_manager().clone()));
    let _ = obj.set(
        PropertyKey::string("name"),
        Value::string(JsString::intern(name)),
    );
    let _ = obj.set(
        PropertyKey::string("message"),
        Value::string(JsString::intern(message)),
    );
    Value::object(obj)
}

fn vm_error_to_value(ctx: &VmContext, err: VmError) -> Value {
    match err {
        VmError::Exception(thrown) => thrown.value,
        VmError::TypeError(message) => make_error_value(ctx, "TypeError", &message),
        VmError::RangeError(message) => make_error_value(ctx, "RangeError", &message),
        VmError::ReferenceError(message) => make_error_value(ctx, "ReferenceError", &message),
        VmError::SyntaxError(message) => make_error_value(ctx, "SyntaxError", &message),
        other => {
            let message = other.to_string();
            Value::string(JsString::intern(&message))
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
