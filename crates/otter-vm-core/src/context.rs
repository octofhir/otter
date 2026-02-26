//! VM execution context
//!
//! The context holds per-execution state: registers, call stack, locals.

use parking_lot::Mutex;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use crate::async_context::SavedFrame;
use crate::error::{VmError, VmResult};
use crate::gc::GcRef;
use crate::interpreter::PreferredType;
use crate::object::JsObject;
use crate::realm::{RealmId, RealmRegistry};
use crate::string::JsString;
use crate::symbol_registry::SymbolRegistry;
use crate::value::{UpvalueCell, Value};
use num_bigint::BigInt as NumBigInt;
use otter_vm_bytecode::Module;

#[cfg(feature = "profiling")]
use otter_profiler::RuntimeStats;

/// Default maximum call stack depth (matches RuntimeConfig default)
pub const DEFAULT_MAX_STACK_DEPTH: usize = 10000;

/// Context passed to native functions, enabling VM re-entry.
///
/// Native functions can now call JavaScript functions (closures or other
/// natives) through `call_function`, access the memory manager, global
/// object, and enqueue microtask jobs — all without interception signals.
pub struct NativeContext<'a> {
    /// The VM execution context (registers, call stack, etc.)
    pub ctx: &'a mut VmContext,
    /// Reference to the interpreter for executing closures
    interpreter: &'a crate::interpreter::Interpreter,
    /// Whether this native function is being called as a constructor (via `new`).
    is_construct: bool,
}

impl<'a> NativeContext<'a> {
    /// Create a new `NativeContext` for a regular function call.
    pub fn new(ctx: &'a mut VmContext, interpreter: &'a crate::interpreter::Interpreter) -> Self {
        Self {
            ctx,
            interpreter,
            is_construct: false,
        }
    }

    /// Create a new `NativeContext` for a constructor call (via `new`).
    pub fn new_construct(
        ctx: &'a mut VmContext,
        interpreter: &'a crate::interpreter::Interpreter,
    ) -> Self {
        Self {
            ctx,
            interpreter,
            is_construct: true,
        }
    }

    /// Returns true if this function is being called as a constructor (via `new`).
    pub fn is_construct(&self) -> bool {
        self.is_construct
    }

    /// Check if the VM has been interrupted (e.g. by a timeout watchdog).
    /// Native methods with loops should call this periodically and return
    /// an error if true, so the cooperative timeout actually works.
    pub fn is_interrupted(&self) -> bool {
        self.ctx.is_interrupted()
    }

    /// Call a JavaScript function (closure or native) with full VM context.
    ///
    /// This is the key method that eliminates the need for interception signals.
    /// Native builtins can now call user-provided callbacks directly.
    pub fn call_function(
        &mut self,
        func: &crate::value::Value,
        this_value: crate::value::Value,
        args: &[crate::value::Value],
    ) -> crate::error::VmResult<crate::value::Value> {
        let mut current_func = func.clone();
        let mut current_this = this_value;
        let mut current_args: Vec<crate::value::Value> = args.to_vec();

        // Unwrap bound functions (stored as objects)
        while let Some(obj) = current_func.as_object() {
            if let Some(bound_fn) =
                obj.get(&crate::object::PropertyKey::string("__boundFunction__"))
            {
                let raw_this_arg = obj
                    .get(&crate::object::PropertyKey::string("__boundThis__"))
                    .unwrap_or_else(crate::value::Value::undefined);
                if raw_this_arg.is_null() || raw_this_arg.is_undefined() {
                    current_this = crate::value::Value::object(self.ctx.global());
                } else {
                    current_this = raw_this_arg;
                }

                if let Some(bound_args_val) =
                    obj.get(&crate::object::PropertyKey::string("__boundArgs__"))
                {
                    if let Some(args_obj) = bound_args_val.as_object() {
                        let len = args_obj
                            .get(&crate::object::PropertyKey::string("length"))
                            .and_then(|v| v.as_int32())
                            .unwrap_or(0) as usize;
                        let mut new_args = Vec::with_capacity(len + current_args.len());
                        for i in 0..len {
                            new_args.push(
                                args_obj
                                    .get(&crate::object::PropertyKey::Index(i as u32))
                                    .unwrap_or_else(crate::value::Value::undefined),
                            );
                        }
                        new_args.extend(current_args);
                        current_args = new_args;
                    }
                }
                current_func = bound_fn;
            } else {
                break;
            }
        }

        if let Some(proxy) = current_func.as_proxy() {
            return crate::proxy_operations::proxy_apply(self, proxy, current_this, &current_args);
        }

        self.interpreter
            .call_function(self.ctx, &current_func, current_this, &current_args)
    }

    /// Call a function as a constructor (native or closure).
    pub fn call_function_construct(
        &mut self,
        func: &crate::value::Value,
        this_value: crate::value::Value,
        args: &[crate::value::Value],
    ) -> crate::error::VmResult<crate::value::Value> {
        self.interpreter
            .call_function_construct(self.ctx, func, this_value, args)
    }

    pub(crate) fn to_primitive(&mut self, value: &Value, hint: PreferredType) -> VmResult<Value> {
        self.interpreter.to_primitive(self.ctx, value, hint)
    }

    pub(crate) fn to_string_value(&mut self, value: &Value) -> VmResult<String> {
        self.interpreter.to_string_value(self.ctx, value)
    }

    pub(crate) fn to_number_value(&mut self, value: &Value) -> VmResult<f64> {
        self.interpreter.to_number_value(self.ctx, value)
    }

    pub(crate) fn parse_bigint_str(&self, value: &str) -> VmResult<NumBigInt> {
        self.interpreter.parse_bigint_str(value)
    }

    pub(crate) fn default_object_prototype_for_constructor(
        &self,
        ctor: &Value,
    ) -> Option<GcRef<JsObject>> {
        self.interpreter
            .default_object_prototype_for_constructor(self.ctx, ctor)
    }

    pub(crate) fn get_prototype_from_constructor(&self, ctor: &Value) -> Option<GcRef<JsObject>> {
        ctor.as_object()
            .and_then(|o| o.get(&crate::object::PropertyKey::string("prototype")))
            .and_then(|v| v.as_object())
            .or_else(|| self.default_object_prototype_for_constructor(ctor))
    }

    pub(crate) fn get_prototype_from_constructor_with_default(
        &self,
        ctor: &Value,
        default: Option<GcRef<JsObject>>,
    ) -> Option<GcRef<JsObject>> {
        ctor.as_object()
            .and_then(|o| o.get(&crate::object::PropertyKey::string("prototype")))
            .and_then(|v| v.as_object())
            .or(default)
            .or_else(|| self.default_object_prototype_for_constructor(ctor))
    }

    pub(crate) fn realm_id_for_function(&self, value: &Value) -> RealmId {
        self.interpreter.realm_id_for_function(self.ctx, value)
    }

    /// Access the memory manager.
    pub fn memory_manager(&self) -> &Arc<crate::memory::MemoryManager> {
        self.ctx.memory_manager()
    }

    /// Access the global object.
    pub fn global(&self) -> GcRef<JsObject> {
        self.ctx.global()
    }

    /// Enqueue a JS microtask job (for Promise callbacks).
    pub fn enqueue_js_job(
        &self,
        job: crate::promise::JsPromiseJob,
        args: Vec<crate::value::Value>,
    ) -> bool {
        self.ctx.enqueue_js_job(job, args)
    }

    /// Enqueue a `process.nextTick()` callback.
    /// Returns true if enqueued, false if no nextTick queue is configured.
    pub fn enqueue_next_tick(
        &self,
        callback: crate::value::Value,
        args: Vec<crate::value::Value>,
    ) -> bool {
        self.ctx.enqueue_next_tick(callback, args)
    }

    /// Get the JS job queue, if configured.
    pub fn js_job_queue(&self) -> Option<Arc<dyn JsJobQueueTrait + Send + Sync>> {
        self.ctx.js_job_queue()
    }

    /// Check if a JS job queue is available.
    pub fn has_js_job_queue(&self) -> bool {
        self.ctx.has_js_job_queue()
    }

    /// Get the pending async ops counter, if configured.
    pub fn pending_async_ops(&self) -> Option<Arc<std::sync::atomic::AtomicU64>> {
        self.ctx.pending_async_ops()
    }

    /// Get the interpreter reference (for advanced operations).
    pub fn interpreter(&self) -> &crate::interpreter::Interpreter {
        self.interpreter
    }

    /// Perform a full JS-level property Get on an object.
    /// This handles prototype chain walking, accessor (getter) invocation, and proxies.
    /// Use this instead of `obj.get()` when observable side effects matter.
    pub fn get_property(
        &mut self,
        obj: &GcRef<crate::object::JsObject>,
        key: &crate::object::PropertyKey,
    ) -> crate::error::VmResult<Value> {
        let key_value = crate::proxy_operations::property_key_to_value_pub(key);
        let receiver = Value::object(obj.clone());
        self.interpreter
            .get_with_proxy_chain(self.ctx, obj, key, key_value, &receiver)
    }

    /// Perform a full JS-level property Get on any Value (object, proxy, or primitive wrapper).
    /// This is the Value-level equivalent of `get_property` — handles both objects and proxies.
    pub fn get_property_of_value(
        &mut self,
        val: &Value,
        key: &crate::object::PropertyKey,
    ) -> crate::error::VmResult<Value> {
        if let Some(obj) = val.as_object() {
            return self.get_property(&obj, key);
        }
        if let Some(proxy) = val.as_proxy() {
            let key_value = crate::proxy_operations::property_key_to_value_pub(key);
            let receiver = val.clone();
            return crate::proxy_operations::proxy_get(self, proxy, key, key_value, receiver);
        }
        Ok(Value::undefined())
    }

    /// Execute a generator operation (next/return/throw) via the interpreter.
    ///
    /// This bridges from NativeContext-based generator prototype methods to the
    /// interpreter's generator execution machinery.
    pub fn execute_generator(
        &mut self,
        generator: GcRef<crate::generator::JsGenerator>,
        sent_value: Option<Value>,
    ) -> crate::interpreter::GeneratorResult {
        self.interpreter
            .execute_generator(generator, self.ctx, sent_value)
    }

    /// Execute a compiled module within this context.
    ///
    /// Used by `require()` to synchronously execute CJS modules.
    /// Pushes a new frame, runs until completion, returns without
    /// consuming outer call frames.
    pub fn execute_module(&mut self, module: &Module) -> VmResult<Value> {
        self.interpreter.execute_eval_module(self.ctx, module)
    }

    /// Compile and execute source as a global script (for $262.evalScript semantics).
    /// Top-level `let`/`const` declarations become persistent global bindings.
    pub fn eval_as_global_script(&mut self, code: &str) -> VmResult<Value> {
        let module = self.ctx.compile_global_script(code)?;
        // Per spec GlobalDeclarationInstantiation steps 3-5:
        let global = self.ctx.global();
        for lex_name in &module.global_lex_names {
            // Step 3a: If env.HasLexicalDeclaration(name), throw SyntaxError.
            // (Check existing global lex bindings — tracked via global_lex_names set)
            if self.ctx.has_global_lex_name(lex_name) {
                return Err(VmError::SyntaxError(format!(
                    "Identifier '{}' has already been declared",
                    lex_name
                )));
            }
            // Step 3d: If env.HasRestrictedGlobalProperty(name), throw SyntaxError.
            // A restricted global property is a non-configurable own property of the global object.
            // Configurable properties (e.g. from eval-created var bindings) are NOT restricted.
            if let Some(desc) =
                global.get_own_property_descriptor(&crate::object::PropertyKey::string(lex_name))
            {
                if !desc.is_configurable() {
                    return Err(VmError::SyntaxError(format!(
                        "Identifier '{}' has already been declared",
                        lex_name
                    )));
                }
            }
        }
        // Record lex names so subsequent scripts see them as declared
        let lex_names: Vec<String> = module.global_lex_names.clone();
        let result = self.execute_eval_module(&module)?;
        for name in lex_names {
            self.ctx.add_global_lex_name(name);
        }
        Ok(result)
    }

    /// Execute an eval-compiled module within this context.
    pub fn execute_eval_module(&mut self, module: &Module) -> VmResult<Value> {
        let interpreter = self.interpreter;
        let ctx = &mut *self.ctx;
        interpreter.execute_eval_module(ctx, module)
    }

    /// Execute an eval-compiled module within a specific realm/global.
    pub fn execute_eval_module_in_realm(
        &mut self,
        realm_id: RealmId,
        module: &Module,
    ) -> VmResult<Value> {
        if let Some(global) = self.ctx.realm_global(realm_id) {
            let interpreter = self.interpreter;
            self.ctx.with_realm(realm_id, global, |ctx| {
                interpreter.execute_eval_module(ctx, module)
            })
        } else {
            self.execute_eval_module(module)
        }
    }

    /// Check for interrupt (timeout) during long-running native functions.
    ///
    /// Native functions that run long loops (e.g., Array.prototype.map iterating
    /// over large sparse arrays) should call this periodically so that
    /// timeouts/interrupts are respected (the interpreter loop can't check).
    ///
    /// **Note:** This intentionally does NOT trigger GC. Running GC from inside
    /// a native function is unsafe because `GcRef` values on the Rust call stack
    /// are not visible to the GC root collector and would be freed (use-after-free).
    /// GC runs safely at interpreter safepoints during `ncx.call_function()` calls.
    ///
    /// Returns `Err(VmError::interrupted())` if execution should stop.
    #[inline]
    pub fn check_for_interrupt(&mut self) -> VmResult<()> {
        if self.ctx.is_interrupted() {
            return Err(VmError::interrupted());
        }
        Ok(())
    }
}

/// Default maximum native call depth to prevent Rust stack overflow
pub const DEFAULT_MAX_NATIVE_DEPTH: usize = 100;

/// Maximum number of registers per function
const MAX_REGISTERS: usize = 65536;

/// Interval for interrupt checking in hot loops (every N instructions)
/// Increased from 1000 to reduce GC check overhead (GC was taking 43% CPU)
pub const INTERRUPT_CHECK_INTERVAL: u32 = 10_000;

/// A call stack frame
#[derive(Debug)]
pub struct CallFrame {
    /// Function index in the module
    pub function_index: u32,
    /// The module this function belongs to
    pub module: std::sync::Arc<otter_vm_bytecode::Module>,
    /// Realm id for this call frame
    pub realm_id: RealmId,
    /// Program counter (instruction index)
    pub pc: usize,
    /// Base register index
    pub register_base: usize,
    /// Local variables
    pub locals: Vec<Value>,
    /// Captured upvalues (heap-allocated cells for shared mutable access)
    pub upvalues: Vec<UpvalueCell>,
    /// Return register (where to put the result)
    pub return_register: Option<u16>,
    /// Source location for error reporting
    pub source_location: Option<SourceLocation>,
    /// The `this` value for this call frame
    pub this_value: Value,
    /// Whether this call is a `new`/constructor invocation
    pub is_construct: bool,
    /// Whether this is an async function (return value should be wrapped in Promise)
    pub is_async: bool,
    /// Unique frame ID for tracking open upvalues
    pub frame_id: usize,
    /// Number of arguments passed to this function
    pub argc: usize,
    /// Home object for methods (used for `super` resolution)
    pub home_object: Option<GcRef<JsObject>>,
    /// The prototype to use when creating `this` in super() chain (new.target.prototype).
    /// Propagated from the outermost derived constructor through the chain.
    pub new_target_proto: Option<GcRef<JsObject>>,
    /// Whether `this` has been initialized in a derived constructor
    /// (super() must be called before accessing `this`)
    pub this_initialized: bool,
    /// Whether this is a derived constructor (class extends)
    pub is_derived: bool,
    /// The callee function value (for arguments.callee in sloppy mode)
    pub callee_value: Option<Value>,
}

/// Source location for error reporting
#[derive(Debug, Clone)]
pub struct SourceLocation {
    /// File path
    pub file: String,
    /// Line number
    pub line: u32,
    /// Column number
    pub column: u32,
}

/// VM execution context
///
/// Holds execution state for a single "thread" of execution.
/// Note: This is not thread-safe internally, but the VmRuntime
/// coordinates access across threads.
pub struct VmContext {
    /// Virtual registers
    registers: Vec<Value>,
    /// Call stack
    call_stack: Vec<CallFrame>,
    /// Global object
    global: GcRef<JsObject>,
    /// Exception state
    exception: Option<Value>,
    /// Try/catch handler stack (catch pc + frame depth)
    try_stack: Vec<TryHandler>,
    /// Is context running
    running: bool,
    /// Pending arguments for next call
    pending_args: Vec<Value>,
    /// Pending `this` value for next call
    pending_this: Option<Value>,
    /// Pending upvalues for next call (captured closure cells)
    pending_upvalues: Vec<UpvalueCell>,
    /// Pending home object for next call (for super resolution in methods)
    pending_home_object: Option<GcRef<JsObject>>,
    /// Pending is_derived flag for next call
    pending_is_derived: bool,
    /// Pending new_target_proto for multi-level super() chain
    pending_new_target_proto: Option<GcRef<JsObject>>,
    /// Pending callee value for next call (for arguments.callee)
    pending_callee_value: Option<Value>,
    /// Pending realm id for next call frame
    pending_realm_id: Option<RealmId>,
    /// Open upvalues: maps (frame_id, local_idx) to the cell.
    /// When a closure captures a local, we create/reuse a cell here.
    /// Multiple closures in the same frame share the same cell.
    open_upvalues: HashMap<(usize, u16), UpvalueCell>,
    /// Next frame ID counter (monotonically increasing)
    next_frame_id: usize,
    /// Interrupt flag for timeout/cancellation support
    interrupt_flag: Arc<AtomicBool>,
    /// Maximum call stack depth (configurable)
    max_stack_depth: usize,
    /// Current native call depth (for protecting against Rust stack overflow)
    native_call_depth: AtomicUsize,
    /// Maximum native call depth
    max_native_depth: usize,
    /// Instruction counter for periodic interrupt checking
    instruction_count: u32,
    /// Optional profiling stats (enabled with 'profiling' feature)
    #[cfg(feature = "profiling")]
    profiling_stats: Option<Arc<RuntimeStats>>,
    /// Memory manager for accounting and limits
    memory_manager: Arc<crate::memory::MemoryManager>,
    /// Root slots for Handle<T> references (managed by HandleScope)
    root_slots: Vec<Value>,
    /// Scope boundaries (stack of base indices for nested HandleScopes)
    scope_markers: Vec<usize>,
    /// Optional debug snapshot target for watchdogs
    debug_snapshot: Option<Arc<Mutex<VmContextSnapshot>>>,
    /// Optional debugger statement hook (`debugger;`).
    debugger_hook: Option<Arc<dyn Fn(&VmContext) + Send + Sync>>,
    /// Intrinsic `%Function.prototype%` (ES2023 §10.3.1).
    /// Set during context creation from VmRuntime so that the
    /// interpreter can assign it as [[Prototype]] on closures
    /// and native functions without a global lookup.
    function_prototype_intrinsic: Option<GcRef<JsObject>>,
    /// Intrinsic `%GeneratorPrototype%` object (ES2026 §27.5.1).
    generator_prototype_intrinsic: Option<GcRef<JsObject>>,
    /// Intrinsic `%AsyncGeneratorPrototype%` object (ES2026 §27.6.1).
    async_generator_prototype_intrinsic: Option<GcRef<JsObject>>,
    /// Set of global var-declared names (from DeclareGlobalVar).
    /// Used by GlobalDeclarationInstantiation to check for lex/var collisions
    /// across script evaluations ($262.evalScript).
    global_var_names: HashSet<String>,
    /// Set of global lex-declared names (from top-level let/const in scripts).
    /// Used by GlobalDeclarationInstantiation step 3a (HasLexicalDeclaration).
    global_lex_names: HashSet<String>,
    /// Eval compiler callback: compiles eval source code into a Module.
    /// Set by otter-vm-runtime to bridge the compiler (which otter-vm-core
    /// cannot depend on directly). The interpreter handles execution.
    /// The boolean parameter indicates whether the caller is in strict mode context.
    eval_fn:
        Option<Arc<dyn Fn(&str, bool) -> Result<otter_vm_bytecode::Module, VmError> + Send + Sync>>,
    /// Script compiler callback: compiles source as a global script where `let`/`const`
    /// at top level behave as global var bindings (for $262.evalScript semantics).
    script_eval_fn:
        Option<Arc<dyn Fn(&str) -> Result<otter_vm_bytecode::Module, VmError> + Send + Sync>>,
    /// Microtask enqueue function for Promise callbacks.
    /// Set by otter-vm-runtime to enable proper microtask queuing from intrinsics.
    microtask_enqueue: Option<Arc<dyn Fn(Box<dyn FnOnce() + Send>) + Send + Sync>>,
    /// process.nextTick() enqueue function.
    /// Set by otter-vm-runtime to enable nextTick callbacks from native extensions.
    next_tick_enqueue: Option<Arc<dyn Fn(Value, Vec<Value>) + Send + Sync>>,
    /// JS job queue for JavaScript function callbacks (Promise.then/catch/finally).
    /// Set by otter-vm-runtime to enable Promise callbacks to be executed via interpreter.
    js_job_queue: Option<Arc<dyn JsJobQueueTrait + Send + Sync>>,
    /// Counter for pending async operations (tokio tasks).
    /// Used by the event loop to know when async work is still in flight.
    pending_async_ops: Option<Arc<std::sync::atomic::AtomicU64>>,
    /// External root sets registered by the runtime (e.g., job queues).
    /// These are traced during GC root collection.
    external_root_sets: Vec<Arc<dyn ExternalRootSet + Send + Sync>>,
    /// Global Symbol registry shared across contexts.
    symbol_registry: Arc<SymbolRegistry>,
    /// Realm id for this context.
    realm_id: RealmId,
    /// Registry of all realms (for cross-realm lookups).
    realm_registry: Option<Arc<RealmRegistry>>,
    /// Trace state (if tracing is enabled)
    pub(crate) trace_state: Option<crate::trace::TraceState>,
    /// Captured exports from the last executed module.
    /// Used by ModuleLoader to populate namespaces.
    captured_module_exports: Option<HashMap<String, Value>>,
    /// Pending throw value for async rejection propagation.
    /// When an awaited Promise rejects, the rejection value is stored here
    /// so that `run_loop_with_suspension` can process it through try-catch.
    pending_throw: Option<Value>,
    /// Cached template objects for tagged template sites.
    template_cache: HashMap<TemplateCacheKey, GcRef<JsObject>>,
    /// Cached RegExp objects per (module_ptr, constant_index) so each literal is only compiled once.
    regexp_cache: HashMap<(u64, u32), Value>,
}

/// Trait for JS job queue access (allows runtime to inject the queue)
pub trait JsJobQueueTrait {
    /// Enqueue a JS callback job
    fn enqueue(&self, job: crate::promise::JsPromiseJob, args: Vec<Value>);
}

/// Trait for external root sets (allows runtime to expose GC roots)
pub trait ExternalRootSet {
    /// Trace all GC roots in this external set
    fn trace_roots(&self, tracer: &mut dyn FnMut(*const crate::gc::GcHeader));
}

#[derive(Debug, Clone)]
struct TryHandler {
    catch_pc: usize,
    frame_depth: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct TemplateCacheKey {
    pub realm_id: RealmId,
    pub module_ptr: usize,
    pub site_id: u32,
}

/// Lightweight debug snapshot of VM execution state.
#[derive(Debug, Clone)]
pub struct FrameSnapshot {
    pub function_index: u32,
    pub function_name: Option<String>,
    pub pc: usize,
    pub instruction: Option<String>,
    pub module_url: String,
    pub is_async: bool,
    pub is_generator: bool,
    pub is_construct: bool,
}

/// Lightweight debug snapshot of VM execution state.
#[derive(Debug, Clone, Default)]
pub struct VmContextSnapshot {
    /// Current call stack depth
    pub stack_depth: usize,
    /// Current try stack depth
    pub try_stack_depth: usize,
    /// Instruction counter since last interrupt check
    pub instruction_count: u32,
    /// Current native call depth
    pub native_call_depth: usize,
    /// Program counter of the current frame (if any)
    pub pc: Option<usize>,
    /// Debug string for the current instruction (if available)
    pub instruction: Option<String>,
    /// Function index of the current frame (if any)
    pub function_index: Option<u32>,
    /// Function name of the current frame (if known)
    pub function_name: Option<String>,
    /// Module URL of the current frame (if known)
    pub module_url: Option<String>,
    /// Whether the current frame is async (if any)
    pub is_async: Option<bool>,
    /// Whether the current frame is a generator (if any)
    pub is_generator: Option<bool>,
    /// Whether the current frame is a constructor call (if any)
    pub is_construct: Option<bool>,
    /// Top stack frames (most recent first) for debugging
    pub frames: Vec<FrameSnapshot>,
    /// Recent instructions (if trace is enabled)
    pub recent_instructions: Vec<crate::trace::TraceEntry>,
    /// Current frame snapshot (if available)
    pub current_frame: Option<FrameSnapshot>,
    /// Full call stack for detailed debugging
    pub call_stack: Vec<FrameSnapshot>,
    /// Captured JS stack frames prepared for CPU profiling.
    #[cfg(feature = "profiling")]
    pub profiler_stack: Vec<otter_profiler::StackFrame>,
}

impl VmContext {
    /// Create a new context with a global object
    pub fn new(global: GcRef<JsObject>, memory_manager: Arc<crate::memory::MemoryManager>) -> Self {
        Self::with_config(
            global,
            DEFAULT_MAX_STACK_DEPTH,
            DEFAULT_MAX_NATIVE_DEPTH,
            memory_manager,
        )
    }

    /// Create a new context with custom stack limits
    pub fn with_config(
        global: GcRef<JsObject>,
        max_stack_depth: usize,
        max_native_depth: usize,
        memory_manager: Arc<crate::memory::MemoryManager>,
    ) -> Self {
        // Set thread-local MemoryManager for GcRef::new() tracking
        crate::memory::MemoryManager::set_thread_default(memory_manager.clone());

        Self {
            registers: vec![Value::undefined(); MAX_REGISTERS],
            call_stack: Vec::with_capacity(64),
            global,
            exception: None,
            try_stack: Vec::new(),
            running: false,
            pending_args: Vec::new(),
            pending_this: None,
            pending_upvalues: Vec::new(),
            pending_home_object: None,
            pending_is_derived: false,
            pending_new_target_proto: None,
            pending_callee_value: None,
            pending_realm_id: None,
            open_upvalues: HashMap::new(),
            next_frame_id: 0,
            interrupt_flag: Arc::new(AtomicBool::new(false)),
            max_stack_depth,
            native_call_depth: AtomicUsize::new(0),
            max_native_depth,
            instruction_count: 0,
            #[cfg(feature = "profiling")]
            profiling_stats: None,
            memory_manager,
            root_slots: Vec::new(),
            scope_markers: Vec::new(),
            debug_snapshot: None,
            debugger_hook: None,
            function_prototype_intrinsic: None,
            generator_prototype_intrinsic: None,
            async_generator_prototype_intrinsic: None,
            global_var_names: HashSet::new(),
            global_lex_names: HashSet::new(),
            eval_fn: None,
            script_eval_fn: None,
            microtask_enqueue: None,
            next_tick_enqueue: None,
            js_job_queue: None,
            pending_async_ops: None,
            external_root_sets: Vec::new(),
            symbol_registry: Arc::new(SymbolRegistry::new()),
            realm_id: 0,
            realm_registry: None,
            trace_state: None,
            captured_module_exports: None,
            pending_throw: None,
            template_cache: HashMap::new(),
            regexp_cache: HashMap::new(),
        }
    }

    pub(crate) fn get_cached_template_object(
        &self,
        key: TemplateCacheKey,
    ) -> Option<GcRef<JsObject>> {
        self.template_cache.get(&key).copied()
    }

    pub(crate) fn cache_template_object(&mut self, key: TemplateCacheKey, obj: GcRef<JsObject>) {
        self.template_cache.insert(key, obj);
    }

    pub(crate) fn get_cached_regexp(&self, module_id: u64, const_idx: u32) -> Option<Value> {
        self.regexp_cache.get(&(module_id, const_idx)).cloned()
    }

    pub(crate) fn cache_regexp(&mut self, module_id: u64, const_idx: u32, val: Value) {
        self.regexp_cache.insert((module_id, const_idx), val);
    }

    /// Set captured module exports.
    pub fn set_captured_exports(&mut self, exports: HashMap<String, Value>) {
        self.captured_module_exports = Some(exports);
    }

    /// Get captured module exports.
    pub fn captured_exports(&self) -> Option<&HashMap<String, Value>> {
        self.captured_module_exports.as_ref()
    }

    /// Take captured module exports (clearing them).
    pub fn take_captured_exports(&mut self) -> Option<HashMap<String, Value>> {
        self.captured_module_exports.take()
    }

    /// Set a pending throw value for async rejection propagation.
    pub fn set_pending_throw(&mut self, value: Option<Value>) {
        self.pending_throw = value;
    }

    /// Take the pending throw value (if any), clearing it.
    pub fn take_pending_throw(&mut self) -> Option<Value> {
        self.pending_throw.take()
    }

    /// Get the memory manager
    pub fn memory_manager(&self) -> &Arc<crate::memory::MemoryManager> {
        &self.memory_manager
    }

    pub(crate) fn symbol_registry(&self) -> &Arc<SymbolRegistry> {
        &self.symbol_registry
    }

    /// Set the realm metadata for this context.
    pub fn set_realm(&mut self, realm_id: RealmId, registry: Arc<RealmRegistry>) {
        self.realm_id = realm_id;
        self.realm_registry = Some(registry);
    }

    /// Get the realm id for this context.
    pub fn realm_id(&self) -> RealmId {
        self.realm_id
    }

    /// Lookup intrinsics for a realm id.
    pub fn realm_intrinsics(&self, realm_id: RealmId) -> Option<crate::intrinsics::Intrinsics> {
        self.realm_registry
            .as_ref()
            .and_then(|registry| registry.get(realm_id))
            .map(|rec| rec.intrinsics)
    }

    /// Lookup global object for a realm id.
    pub fn realm_global(&self, realm_id: RealmId) -> Option<GcRef<JsObject>> {
        self.realm_registry
            .as_ref()
            .and_then(|registry| registry.get(realm_id))
            .map(|rec| rec.global)
    }

    /// Lookup the realm's Function.prototype for a realm id.
    pub fn realm_function_prototype(&self, realm_id: RealmId) -> Option<GcRef<JsObject>> {
        self.realm_registry
            .as_ref()
            .and_then(|registry| registry.get(realm_id))
            .map(|rec| rec.function_prototype)
    }

    /// Temporarily run with a different realm/global, restoring after the closure.
    pub fn with_realm<R>(
        &mut self,
        realm_id: RealmId,
        global: GcRef<JsObject>,
        f: impl FnOnce(&mut VmContext) -> R,
    ) -> R {
        let old_realm_id = self.realm_id;
        let old_global = self.global;
        let old_fn_proto = self.function_prototype_intrinsic;
        let old_gen_proto = self.generator_prototype_intrinsic;
        let old_async_gen_proto = self.async_generator_prototype_intrinsic;

        self.realm_id = realm_id;
        self.global = global;
        if let Some(intrinsics) = self.realm_intrinsics(realm_id) {
            self.function_prototype_intrinsic = Some(intrinsics.function_prototype);
            self.generator_prototype_intrinsic = Some(intrinsics.generator_prototype);
            self.async_generator_prototype_intrinsic = Some(intrinsics.async_generator_prototype);
        }

        let result = f(self);

        self.realm_id = old_realm_id;
        self.global = old_global;
        self.function_prototype_intrinsic = old_fn_proto;
        self.generator_prototype_intrinsic = old_gen_proto;
        self.async_generator_prototype_intrinsic = old_async_gen_proto;

        result
    }

    /// Switch this context to a different realm/global.
    pub fn switch_realm(&mut self, realm_id: RealmId) {
        if realm_id == self.realm_id {
            return;
        }
        if let Some(global) = self.realm_global(realm_id) {
            self.global = global;
        }
        if let Some(intrinsics) = self.realm_intrinsics(realm_id) {
            self.function_prototype_intrinsic = Some(intrinsics.function_prototype);
            self.generator_prototype_intrinsic = Some(intrinsics.generator_prototype);
            self.async_generator_prototype_intrinsic = Some(intrinsics.async_generator_prototype);
        }
        self.realm_id = realm_id;
    }

    /// Attach a debug snapshot target to update periodically.
    pub fn set_debug_snapshot_target(&mut self, target: Option<Arc<Mutex<VmContextSnapshot>>>) {
        self.debug_snapshot = target;
        self.update_debug_snapshot();
    }

    /// Get the current debug snapshot (if enabled).
    pub fn debug_snapshot(&self) -> Option<VmContextSnapshot> {
        self.debug_snapshot
            .as_ref()
            .map(|snapshot| snapshot.lock().clone())
    }

    /// Attach a callback for `debugger;` statements.
    pub fn set_debugger_hook(&mut self, hook: Option<Arc<dyn Fn(&VmContext) + Send + Sync>>) {
        self.debugger_hook = hook;
    }

    /// Invoke the debugger hook if present.
    pub fn trigger_debugger_hook(&self) {
        if let Some(hook) = &self.debugger_hook {
            hook(self);
        }
    }

    // ─────────────────────────────────────────────────────────────────────────────
    // Trace management
    // ─────────────────────────────────────────────────────────────────────────────

    /// Set trace configuration and enable tracing.
    pub fn set_trace_config(&mut self, config: crate::trace::TraceConfig) {
        if config.enabled {
            self.trace_state = Some(crate::trace::TraceState::new(config));
        } else {
            self.trace_state = None;
        }
    }

    /// Get the trace buffer (if tracing is enabled).
    pub fn get_trace_buffer(&self) -> Option<&crate::trace::TraceRingBuffer> {
        self.trace_state.as_ref().map(|s| &s.ring_buffer)
    }

    /// Record a trace entry (called at interrupt check points or every instruction in FullTrace mode).
    pub fn record_trace_entry(
        &mut self,
        instruction: &otter_vm_bytecode::Instruction,
        pc: usize,
        function_index: u32,
        module: &std::sync::Arc<otter_vm_bytecode::Module>,
        modified_registers: Vec<(u16, String)>,
        execution_time_ns: Option<u64>,
    ) {
        let Some(trace_state) = &mut self.trace_state else {
            return;
        };

        let func = module.function(function_index);
        let function_name = func.and_then(|f| f.name.clone());

        // Format instruction operands (simplified for MVP)
        let operands = format!("{:?}", instruction);

        let entry = crate::trace::TraceEntry {
            instruction_number: trace_state.instruction_counter,
            pc,
            function_index,
            function_name,
            module_url: module.source_url.clone(),
            opcode: format!("{:?}", instruction)
                .split(' ')
                .next()
                .unwrap_or("Unknown")
                .to_string(),
            operands,
            modified_registers,
            execution_time_ns,
        };

        // Check filter
        if !trace_state.matches_filter(&entry) {
            trace_state.instruction_counter += 1;
            return;
        }

        // Always add to ring buffer (for timeout dumps)
        trace_state.ring_buffer.push(entry.clone());

        // Write to trace file if in FullTrace mode
        if trace_state.config.mode == crate::trace::TraceMode::FullTrace {
            if let Some(ref mut writer) = trace_state.trace_writer {
                let _ = writer.write_entry(&entry);
            }
        }

        trace_state.instruction_counter += 1;
    }

    // ─────────────────────────────────────────────────────────────────────────────
    // Root slot management (for HandleScope)
    // ─────────────────────────────────────────────────────────────────────────────

    /// Get the current number of root slots
    #[inline]
    pub fn root_slots_len(&self) -> usize {
        self.root_slots.len()
    }

    /// Push a value to root slots, returning its index
    #[inline]
    pub fn push_root_slot(&mut self, value: Value) -> usize {
        let index = self.root_slots.len();
        self.root_slots.push(value);
        index
    }

    /// Pop the specified number of root slots
    #[inline]
    pub fn pop_root_slots(&mut self, count: usize) {
        let new_len = self.root_slots.len().saturating_sub(count);
        self.root_slots.truncate(new_len);
    }

    /// Get a reference to a root slot value
    #[inline]
    pub fn get_root_slot(&self, index: usize) -> &Value {
        &self.root_slots[index]
    }

    /// Get the total number of roots (for testing)
    #[inline]
    pub fn root_count(&self) -> usize {
        self.root_slots.len()
    }

    /// Push a scope marker (base index of a new HandleScope)
    #[inline]
    pub fn push_scope_marker(&mut self, index: usize) {
        self.scope_markers.push(index);
    }

    /// Pop the most recent scope marker
    #[inline]
    pub fn pop_scope_marker(&mut self) {
        self.scope_markers.pop();
    }

    /// Get root slots for GC tracing
    #[inline]
    pub fn root_slots_to_trace(&self) -> &[Value] {
        &self.root_slots
    }

    /// Set the maximum stack depth
    pub fn set_max_stack_depth(&mut self, depth: usize) {
        self.max_stack_depth = depth;
    }

    /// Get the maximum stack depth
    pub fn max_stack_depth(&self) -> usize {
        self.max_stack_depth
    }

    /// Set the maximum native call depth
    pub fn set_max_native_depth(&mut self, depth: usize) {
        self.max_native_depth = depth;
    }

    /// Get the maximum native call depth
    pub fn max_native_depth(&self) -> usize {
        self.max_native_depth
    }

    /// Increment native call depth and check for overflow
    ///
    /// Returns an error if the native call depth exceeds the maximum.
    /// Call this before invoking native functions from the interpreter.
    #[inline]
    pub fn enter_native_call(&self) -> VmResult<()> {
        let depth = self.native_call_depth.fetch_add(1, Ordering::Relaxed);
        if depth >= self.max_native_depth {
            self.native_call_depth.fetch_sub(1, Ordering::Relaxed);
            return Err(VmError::StackOverflow);
        }
        Ok(())
    }

    /// Decrement native call depth
    ///
    /// Call this after a native function returns.
    #[inline]
    pub fn exit_native_call(&self) {
        self.native_call_depth.fetch_sub(1, Ordering::Relaxed);
    }

    /// Get current native call depth
    pub fn native_call_depth(&self) -> usize {
        self.native_call_depth.load(Ordering::Relaxed)
    }

    /// Check and increment instruction count for periodic interrupt checking
    ///
    /// Returns true if interrupt check should be performed (every INTERRUPT_CHECK_INTERVAL instructions).
    #[inline]
    pub fn should_check_interrupt(&mut self) -> bool {
        self.instruction_count += 1;
        if self.instruction_count >= INTERRUPT_CHECK_INTERVAL {
            self.instruction_count = 0;
            self.update_debug_snapshot();
            true
        } else {
            false
        }
    }

    /// Get the interrupt flag for external timeout/cancellation
    ///
    /// Call `flag.store(true, Ordering::Relaxed)` to interrupt execution.
    /// The VM will check this flag periodically and return an error if set.
    pub fn interrupt_flag(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.interrupt_flag)
    }

    /// Set a custom interrupt flag (for sharing across contexts)
    pub fn set_interrupt_flag(&mut self, flag: Arc<AtomicBool>) {
        self.interrupt_flag = flag;
    }

    /// Check if execution was interrupted
    #[inline]
    pub fn is_interrupted(&self) -> bool {
        self.interrupt_flag.load(Ordering::Relaxed)
    }

    /// Request interruption of execution
    pub fn interrupt(&self) {
        self.interrupt_flag.store(true, Ordering::Relaxed);
    }

    /// Clear the interrupt flag
    pub fn clear_interrupt(&self) {
        self.interrupt_flag.store(false, Ordering::Relaxed);
    }

    pub(crate) fn update_debug_snapshot(&self) {
        let Some(target) = &self.debug_snapshot else {
            return;
        };

        let mut snapshot = VmContextSnapshot::default();
        snapshot.stack_depth = self.call_stack.len();
        snapshot.try_stack_depth = self.try_stack.len();
        snapshot.instruction_count = self.instruction_count;
        snapshot.native_call_depth = self.native_call_depth();

        if let Some(frame) = self.call_stack.last() {
            snapshot.pc = Some(frame.pc);
            snapshot.function_index = Some(frame.function_index);
            snapshot.module_url = Some(frame.module.source_url.clone());
            snapshot.is_async = Some(frame.is_async);
            snapshot.is_construct = Some(frame.is_construct);
            if let Some(func) = frame.module.function(frame.function_index) {
                snapshot.function_name = func.name.clone();
                snapshot.is_generator = Some(func.flags.is_generator);
                if frame.pc < func.instructions.len() {
                    snapshot.instruction = Some(format!("{:?}", func.instructions[frame.pc]));
                }
            }

            // Add current frame snapshot
            if let Some(func) = frame.module.function(frame.function_index) {
                let instruction = if frame.pc < func.instructions.len() {
                    Some(format!("{:?}", func.instructions[frame.pc]))
                } else {
                    None
                };
                snapshot.current_frame = Some(FrameSnapshot {
                    function_index: frame.function_index,
                    function_name: func.name.clone(),
                    pc: frame.pc,
                    instruction,
                    module_url: frame.module.source_url.clone(),
                    is_async: frame.is_async,
                    is_generator: func.flags.is_generator,
                    is_construct: frame.is_construct,
                });
            }
        }

        // Capture a few top frames for better watchdog debugging.
        for frame in self.call_stack.iter().rev().take(5) {
            if let Some(func) = frame.module.function(frame.function_index) {
                let instruction = if frame.pc < func.instructions.len() {
                    Some(format!("{:?}", func.instructions[frame.pc]))
                } else {
                    None
                };
                snapshot.frames.push(FrameSnapshot {
                    function_index: frame.function_index,
                    function_name: func.name.clone(),
                    pc: frame.pc,
                    instruction,
                    module_url: frame.module.source_url.clone(),
                    is_async: frame.is_async,
                    is_generator: func.flags.is_generator,
                    is_construct: frame.is_construct,
                });
            }
        }

        // Capture ALL frames for full call stack
        for frame in self.call_stack.iter().rev() {
            if let Some(func) = frame.module.function(frame.function_index) {
                let instruction = if frame.pc < func.instructions.len() {
                    Some(format!("{:?}", func.instructions[frame.pc]))
                } else {
                    None
                };
                snapshot.call_stack.push(FrameSnapshot {
                    function_index: frame.function_index,
                    function_name: func.name.clone(),
                    pc: frame.pc,
                    instruction,
                    module_url: frame.module.source_url.clone(),
                    is_async: frame.is_async,
                    is_generator: func.flags.is_generator,
                    is_construct: frame.is_construct,
                });
            }
        }

        // Add trace buffer entries if available
        if let Some(trace_state) = &self.trace_state {
            snapshot.recent_instructions = trace_state.ring_buffer.iter().cloned().collect();
        }

        #[cfg(feature = "profiling")]
        {
            snapshot.profiler_stack = self.capture_profiler_stack();
        }

        *target.lock() = snapshot;
    }

    // ==================== Profiling Methods ====================

    /// Enable profiling with the given stats collector
    #[cfg(feature = "profiling")]
    pub fn enable_profiling(&mut self, stats: Arc<RuntimeStats>) {
        self.profiling_stats = Some(stats);
    }

    /// Disable profiling
    #[cfg(feature = "profiling")]
    pub fn disable_profiling(&mut self) {
        self.profiling_stats = None;
    }

    /// Get profiling stats if enabled
    #[cfg(feature = "profiling")]
    pub fn profiling_stats(&self) -> Option<&Arc<RuntimeStats>> {
        self.profiling_stats.as_ref()
    }

    /// Record an instruction execution (only when profiling is enabled)
    #[cfg(feature = "profiling")]
    #[inline]
    pub fn record_instruction(&self) {
        if let Some(stats) = &self.profiling_stats {
            stats.record_instruction();
        }
    }

    /// No-op when profiling feature is disabled
    #[cfg(not(feature = "profiling"))]
    #[inline]
    pub fn record_instruction(&self) {}

    /// Get a register value
    #[inline]
    pub fn get_register(&self, index: u16) -> &Value {
        static UNDEFINED: Value = Value::undefined();
        let frame = match self.current_frame() {
            Some(f) => f,
            None => return &UNDEFINED,
        };
        &self.registers[frame.register_base + index as usize]
    }

    /// Set a register value
    #[inline]
    pub fn set_register(&mut self, index: u16, value: Value) {
        let base = match self.current_frame() {
            Some(f) => f.register_base,
            None => return,
        };
        self.registers[base + index as usize] = value;
    }

    /// Get a local variable
    #[inline]
    pub fn get_local(&self, index: u16) -> VmResult<Value> {
        let frame = self
            .current_frame()
            .ok_or_else(|| VmError::internal("no call frame"))?;

        // If this local has been captured and is still open, use the cell value.
        // This ensures shared mutable access between the parent function and closures.
        let frame_id = frame.frame_id;
        if let Some(cell) = self.open_upvalues.get(&(frame_id, index)) {
            return Ok(cell.get());
        }

        frame
            .locals
            .get(index as usize)
            .cloned()
            .ok_or_else(|| VmError::internal(format!("local index {} out of bounds", index)))
    }

    /// Set a local variable
    /// If this local has been captured by a closure, also update the shared cell
    #[inline]
    pub fn set_local(&mut self, index: u16, value: Value) -> VmResult<()> {
        let frame = self
            .current_frame_mut()
            .ok_or_else(|| VmError::internal("no call frame"))?;
        if (index as usize) < frame.locals.len() {
            frame.locals[index as usize] = value.clone();
            // If this local has been captured, update the cell too
            let frame_id = frame.frame_id;
            if let Some(cell) = self.open_upvalues.get(&(frame_id, index)) {
                let _ = cell.set(value);
            }
            Ok(())
        } else {
            Err(VmError::internal(format!(
                "local index {} out of bounds",
                index
            )))
        }
    }

    /// Get global object
    pub fn global(&self) -> GcRef<JsObject> {
        self.global
    }

    /// Record a global var-declared name (from DeclareGlobalVar).
    pub fn add_global_var_name(&mut self, name: String) {
        self.global_var_names.insert(name);
    }

    /// Check if a name was declared as a global var.
    pub fn has_global_var_name(&self, name: &str) -> bool {
        self.global_var_names.contains(name)
    }

    /// Record a global lex-declared name (from top-level let/const in scripts).
    pub fn add_global_lex_name(&mut self, name: String) {
        self.global_lex_names.insert(name);
    }

    /// Check if a name was declared as a global lex binding (let/const).
    pub fn has_global_lex_name(&self, name: &str) -> bool {
        self.global_lex_names.contains(name)
    }

    /// Push a try handler for the current frame.
    pub fn push_try(&mut self, catch_pc: usize) {
        self.try_stack.push(TryHandler {
            catch_pc,
            frame_depth: self.call_stack.len(),
        });
    }

    /// Pop the most recently pushed try handler.
    pub fn pop_try(&mut self) {
        self.try_stack.pop();
    }

    /// Pop the most recent try handler if it belongs to the current frame.
    pub fn pop_try_for_current_frame(&mut self) {
        if let Some(top) = self.try_stack.last()
            && top.frame_depth == self.call_stack.len()
        {
            self.try_stack.pop();
        }
    }

    /// Pop and return the nearest try handler.
    pub fn take_nearest_try(&mut self) -> Option<(usize, usize)> {
        let handler = self.try_stack.pop()?;
        Some((handler.frame_depth, handler.catch_pc))
    }

    /// Peek the nearest try handler without popping it.
    pub fn peek_nearest_try(&self) -> Option<(usize, usize)> {
        self.try_stack.last().map(|h| (h.frame_depth, h.catch_pc))
    }

    /// Get try handlers for the current frame (for generator frame serialization).
    ///
    /// Returns a vector of (catch_pc, frame_depth) tuples for all try handlers
    /// that belong to the current frame.
    pub fn get_try_handlers_for_current_frame(&self) -> Vec<(usize, usize)> {
        let current_depth = self.call_stack.len();
        self.try_stack
            .iter()
            .filter(|h| h.frame_depth == current_depth)
            .map(|h| (h.catch_pc, h.frame_depth))
            .collect()
    }

    /// Restore try handlers (for generator frame restoration).
    ///
    /// Takes a vector of (catch_pc, frame_depth) tuples and pushes them
    /// onto the try stack.
    pub fn restore_try_handlers(&mut self, handlers: &[(usize, usize)]) {
        for (catch_pc, frame_depth) in handlers {
            self.try_stack.push(TryHandler {
                catch_pc: *catch_pc,
                frame_depth: *frame_depth,
            });
        }
    }

    /// Get the intrinsic `%Function.prototype%` object (ES2023 §10.3.1).
    ///
    /// Returns the intrinsic if set, otherwise falls back to looking up
    /// `globalThis.Function.prototype` for backwards compatibility.
    pub fn function_prototype(&self) -> Option<GcRef<JsObject>> {
        if let Some(ref fp) = self.function_prototype_intrinsic {
            return Some(*fp);
        }
        // Fallback: dynamic lookup (used when intrinsic hasn't been set yet)
        use crate::object::PropertyKey;
        self.global
            .get(&PropertyKey::string("Function"))
            .and_then(|v| v.as_object())
            .and_then(|func_obj| func_obj.get(&PropertyKey::string("prototype")))
            .and_then(|v| v.as_object())
    }

    /// Set the intrinsic `%Function.prototype%` object.
    /// Called by VmRuntime during context creation.
    pub fn set_function_prototype_intrinsic(&mut self, proto: GcRef<JsObject>) {
        self.function_prototype_intrinsic = Some(proto);
    }

    /// Get the intrinsic `%Function.prototype%` object if explicitly set.
    pub fn function_prototype_intrinsic(&self) -> Option<GcRef<JsObject>> {
        self.function_prototype_intrinsic
    }

    /// Set the intrinsic `%GeneratorPrototype%` object.
    pub fn set_generator_prototype_intrinsic(&mut self, proto: GcRef<JsObject>) {
        self.generator_prototype_intrinsic = Some(proto);
    }

    /// Get the intrinsic `%GeneratorPrototype%` object.
    pub fn generator_prototype_intrinsic(&self) -> Option<GcRef<JsObject>> {
        self.generator_prototype_intrinsic
    }

    /// Set the intrinsic `%AsyncGeneratorPrototype%` object.
    pub fn set_async_generator_prototype_intrinsic(&mut self, proto: GcRef<JsObject>) {
        self.async_generator_prototype_intrinsic = Some(proto);
    }

    /// Get the intrinsic `%AsyncGeneratorPrototype%` object.
    pub fn async_generator_prototype_intrinsic(&self) -> Option<GcRef<JsObject>> {
        self.async_generator_prototype_intrinsic
    }

    /// Get the attached realm registry.
    pub fn realm_registry(&self) -> Option<&Arc<RealmRegistry>> {
        self.realm_registry.as_ref()
    }

    /// Set the eval compiler callback used by the interpreter to compile
    /// eval code at runtime. Called by otter-vm-runtime during context setup.
    /// The callback takes source code and a boolean indicating strict mode context,
    /// and returns a compiled Module.
    pub fn set_eval_fn(
        &mut self,
        f: Arc<dyn Fn(&str, bool) -> Result<otter_vm_bytecode::Module, VmError> + Send + Sync>,
    ) {
        self.eval_fn = Some(f);
    }

    /// Compile eval code into a Module using the registered eval compiler.
    /// The strict_context parameter indicates whether the caller is in strict mode.
    /// Returns `VmError::TypeError` if the eval callback is not configured.
    pub fn compile_eval(
        &self,
        code: &str,
        strict_context: bool,
    ) -> Result<otter_vm_bytecode::Module, VmError> {
        let eval_fn = self
            .eval_fn
            .as_ref()
            .ok_or_else(|| VmError::type_error("eval() is not available in this context"))?;
        eval_fn(code, strict_context)
    }

    /// Set the script compiler callback for $262.evalScript semantics.
    /// The callback compiles source as a global script where top-level `let`/`const`
    /// behave as global property bindings (persisting across script evaluations).
    pub fn set_script_eval_fn(
        &mut self,
        f: Arc<dyn Fn(&str) -> Result<otter_vm_bytecode::Module, VmError> + Send + Sync>,
    ) {
        self.script_eval_fn = Some(f);
    }

    /// Compile source as a global script (for $262.evalScript semantics).
    /// Top-level `let`/`const` are treated as global var bindings.
    /// Falls back to `compile_eval` if no script compiler is configured.
    pub fn compile_global_script(&self, code: &str) -> Result<otter_vm_bytecode::Module, VmError> {
        if let Some(script_fn) = self.script_eval_fn.as_ref() {
            script_fn(code)
        } else {
            // Fallback: use regular eval compilation
            self.compile_eval(code, false)
        }
    }

    /// Set the microtask enqueue callback used by Promise intrinsics.
    /// Called by otter-vm-runtime during context setup to enable proper
    /// microtask queuing for Promise.prototype.then/catch/finally.
    pub fn set_microtask_enqueue(
        &mut self,
        f: Arc<dyn Fn(Box<dyn FnOnce() + Send>) + Send + Sync>,
    ) {
        self.microtask_enqueue = Some(f);
    }

    /// Set the nextTick enqueue callback used by `process.nextTick()`.
    /// Called by otter-vm-runtime during context setup.
    pub fn set_next_tick_enqueue(&mut self, f: Arc<dyn Fn(Value, Vec<Value>) + Send + Sync>) {
        self.next_tick_enqueue = Some(f);
    }

    /// Enqueue a nextTick callback if a nextTick queue is configured.
    /// Returns true if enqueued, false if no queue is available.
    pub fn enqueue_next_tick(&self, callback: Value, args: Vec<Value>) -> bool {
        if let Some(enqueue) = &self.next_tick_enqueue {
            enqueue(callback, args);
            true
        } else {
            false
        }
    }

    /// Set the pending async ops counter.
    /// Called by otter-vm-runtime during context setup.
    pub fn set_pending_async_ops(&mut self, counter: Arc<std::sync::atomic::AtomicU64>) {
        self.pending_async_ops = Some(counter);
    }

    /// Get the pending async ops counter, if configured.
    pub fn pending_async_ops(&self) -> Option<Arc<std::sync::atomic::AtomicU64>> {
        self.pending_async_ops.clone()
    }

    /// Enqueue a microtask if a microtask queue is configured.
    /// Returns true if the task was enqueued, false if no queue is available.
    pub fn enqueue_microtask(&self, task: Box<dyn FnOnce() + Send>) -> bool {
        if let Some(enqueue) = &self.microtask_enqueue {
            enqueue(task);
            true
        } else {
            false
        }
    }

    /// Set the JS job queue for Promise callbacks
    pub fn set_js_job_queue(&mut self, queue: Arc<dyn JsJobQueueTrait + Send + Sync>) {
        self.js_job_queue = Some(queue);
    }

    /// Register an external root set for GC (e.g., job queues)
    pub fn register_external_root_set(&mut self, roots: Arc<dyn ExternalRootSet + Send + Sync>) {
        self.external_root_sets.push(roots);
    }

    /// Enqueue a JS callback job if a queue is configured.
    /// Returns true if the job was enqueued, false if no queue is available.
    pub fn enqueue_js_job(&self, job: crate::promise::JsPromiseJob, args: Vec<Value>) -> bool {
        if let Some(queue) = &self.js_job_queue {
            queue.enqueue(job, args);
            true
        } else {
            false
        }
    }

    /// Get the JS job queue, if configured.
    pub fn js_job_queue(&self) -> Option<Arc<dyn JsJobQueueTrait + Send + Sync>> {
        self.js_job_queue.clone()
    }

    /// Check if a JS job queue is available
    pub fn has_js_job_queue(&self) -> bool {
        self.js_job_queue.is_some()
    }

    /// Get global variable
    pub fn get_global(&self, name: &str) -> Option<Value> {
        use crate::object::PropertyKey;
        self.global.get(&PropertyKey::string(name))
    }

    /// Get global variable by UTF-16 code units
    pub fn get_global_utf16(&self, units: &[u16]) -> Option<Value> {
        use crate::object::PropertyKey;
        let key = PropertyKey::from_js_string(JsString::intern_utf16(units));
        self.global.get(&key)
    }

    /// Set global variable
    pub fn set_global(&self, name: &str, value: Value) {
        use crate::object::PropertyKey;
        let _ = self.global.set(PropertyKey::string(name), value);
    }

    /// Set global variable by UTF-16 code units
    pub fn set_global_utf16(&self, units: &[u16], value: Value) {
        use crate::object::PropertyKey;
        let key = PropertyKey::from_js_string(JsString::intern_utf16(units));
        let _ = self.global.set(key, value);
    }

    /// Push a new call frame
    pub fn push_frame(
        &mut self,
        function_index: u32,
        module: std::sync::Arc<otter_vm_bytecode::Module>,
        local_count: u16,
        return_register: Option<u16>,
        is_construct: bool,
        is_async: bool,
        argc: usize,
    ) -> VmResult<()> {
        if self.call_stack.len() >= self.max_stack_depth {
            return Err(VmError::StackOverflow);
        }

        let register_base = self
            .call_stack
            .last()
            .map(|f| f.register_base + MAX_REGISTERS)
            .unwrap_or(0);

        // Ensure we have enough registers
        let needed = register_base + MAX_REGISTERS;
        if needed > self.registers.len() {
            self.registers.resize(needed, Value::undefined());
        }

        // Take pending arguments and copy to locals
        let args = self.take_pending_args();
        let func = &module.functions[function_index as usize];
        let param_count = func.param_count as usize;
        let has_rest = func.flags.has_rest;

        // If the function has a rest parameter, the interpreter packages the rest args
        // into an array and passes it as the last argument. This rest array should
        // be mapped to the rest parameter's local slot (at index param_count).
        // So effective_param_count is param_count + 1 when has_rest is true.
        let effective_param_count = if has_rest {
            param_count + 1
        } else {
            param_count
        };

        let extra_args_count = if args.len() > effective_param_count {
            args.len() - effective_param_count
        } else {
            0
        };

        let total_locals = local_count as usize + extra_args_count;
        let mut locals = vec![Value::undefined(); total_locals];

        for (i, arg) in args.into_iter().enumerate() {
            if i < effective_param_count {
                // Regular argument (or rest array) -> mapped to parameter local
                if i < locals.len() {
                    locals[i] = arg;
                }
            } else {
                // Extra argument -> stored after all regular locals (including vars)
                // This ensures we don't overwrite local variables with extra arguments
                let target_idx = local_count as usize + (i - effective_param_count);
                if target_idx < locals.len() {
                    locals[target_idx] = arg;
                }
            }
        }

        if std::env::var("OTTER_TRACE_ASSERT_ARGS").is_ok() {
            if let Some(func) = module.functions.get(function_index as usize) {
                if func.name.as_deref() == Some("assert") {
                    let arg_types: Vec<_> = locals
                        .iter()
                        .take(param_count)
                        .map(|v| v.type_of())
                        .collect();
                    eprintln!(
                        "[OTTER_TRACE_ASSERT_ARGS] params={} types={:?}",
                        param_count, arg_types
                    );
                }
            }
        }

        let frame_realm_id = self.pending_realm_id.take().unwrap_or(self.realm_id);
        let frame_global = self.realm_global(frame_realm_id).unwrap_or(self.global);

        // Take pending this value (defaults to undefined)
        // ES2023 §10.2.1.1: In non-strict mode, undefined/null this becomes globalThis
        let mut this_value = self.take_pending_this();
        if this_value.is_undefined() && !func.flags.is_strict && !is_construct {
            this_value = Value::object(frame_global);
        }

        // Take pending upvalues (captured closure cells)
        let upvalues = self.take_pending_upvalues();

        // Take pending home object and derived flag
        let home_object = self.pending_home_object.take();
        let is_derived = std::mem::take(&mut self.pending_is_derived);
        let callee_value = self.pending_callee_value.take();

        // Assign a unique frame ID
        let frame_id = self.next_frame_id;
        self.next_frame_id += 1;

        self.call_stack.push(CallFrame {
            function_index,
            module,
            realm_id: frame_realm_id,
            pc: 0,
            register_base,
            locals,
            upvalues,
            return_register,
            source_location: None,
            this_value,
            is_construct,
            is_async,
            frame_id,
            argc,
            home_object,
            new_target_proto: self.pending_new_target_proto.take(),
            // In derived constructors, this is NOT initialized until super() is called
            this_initialized: !is_derived,
            is_derived,
            callee_value,
        });
        self.switch_realm(frame_realm_id);
        Ok(())
    }

    /// Pop the current call frame
    pub fn pop_frame(&mut self) -> Option<CallFrame> {
        // Remove any try handlers associated with this frame.
        let current_depth = self.call_stack.len();
        while self
            .try_stack
            .last()
            .is_some_and(|handler| handler.frame_depth == current_depth)
        {
            self.try_stack.pop();
        }

        if let Some(frame) = self.call_stack.last() {
            // Clean up open upvalues for this frame
            // (cells are already synced via set_local updates)
            let frame_id = frame.frame_id;
            self.open_upvalues.retain(|(fid, _), _| *fid != frame_id);
        }
        let popped = self.call_stack.pop();
        if let Some(frame) = self.call_stack.last() {
            self.switch_realm(frame.realm_id);
        }
        popped
    }

    /// Get current call frame
    #[inline]
    pub fn current_frame(&self) -> Option<&CallFrame> {
        self.call_stack.last()
    }

    /// Get current call frame mutably
    #[inline]
    pub fn current_frame_mut(&mut self) -> Option<&mut CallFrame> {
        self.call_stack.last_mut()
    }

    /// Get program counter
    #[inline]
    pub fn pc(&self) -> usize {
        self.current_frame().map(|f| f.pc).unwrap_or(0)
    }

    /// Set program counter
    #[inline]
    pub fn set_pc(&mut self, pc: usize) {
        if let Some(frame) = self.current_frame_mut() {
            frame.pc = pc;
        }
    }

    /// Increment program counter
    #[inline]
    pub fn advance_pc(&mut self) {
        if let Some(frame) = self.current_frame_mut() {
            frame.pc += 1;
        }
    }

    /// Jump relative to current PC
    #[inline]
    pub fn jump(&mut self, offset: i32) {
        if let Some(frame) = self.current_frame_mut() {
            frame.pc = (frame.pc as i64 + offset as i64) as usize;
        }
    }

    /// Get call stack depth
    pub fn stack_depth(&self) -> usize {
        self.call_stack.len()
    }

    /// Get exception if any
    pub fn exception(&self) -> Option<&Value> {
        self.exception.as_ref()
    }

    /// Set exception
    pub fn set_exception(&mut self, value: Value) {
        self.exception = Some(value);
    }

    /// Clear exception
    pub fn clear_exception(&mut self) {
        self.exception = None;
    }

    /// Take and clear exception value
    pub fn take_exception(&mut self) -> Option<Value> {
        self.exception.take()
    }

    /// Set pending arguments for next function call
    pub fn set_pending_args(&mut self, args: Vec<Value>) {
        self.pending_args = args;
    }

    /// Take pending arguments (transfers ownership)
    pub fn take_pending_args(&mut self) -> Vec<Value> {
        std::mem::take(&mut self.pending_args)
    }

    /// Set pending `this` value for next function call
    pub fn set_pending_this(&mut self, this_value: Value) {
        self.pending_this = Some(this_value);
    }

    /// Take pending `this` value (defaults to undefined)
    pub fn take_pending_this(&mut self) -> Value {
        self.pending_this.take().unwrap_or_else(Value::undefined)
    }

    /// Set pending upvalues for next function call (captured closure cells)
    pub fn set_pending_upvalues(&mut self, upvalues: Vec<UpvalueCell>) {
        self.pending_upvalues = upvalues;
    }

    /// Take pending upvalues (transfers ownership)
    pub fn take_pending_upvalues(&mut self) -> Vec<UpvalueCell> {
        std::mem::take(&mut self.pending_upvalues)
    }

    /// Set pending home object for next function call (for super resolution)
    pub fn set_pending_home_object(&mut self, home_object: GcRef<JsObject>) {
        self.pending_home_object = Some(home_object);
    }

    /// Set pending is_derived flag for next function call
    pub fn set_pending_is_derived(&mut self, is_derived: bool) {
        self.pending_is_derived = is_derived;
    }

    /// Set pending callee value for next function call (for arguments.callee)
    pub fn set_pending_callee_value(&mut self, callee: Value) {
        self.pending_callee_value = Some(callee);
    }

    pub fn set_pending_new_target_proto(&mut self, proto: GcRef<JsObject>) {
        self.pending_new_target_proto = Some(proto);
    }

    pub fn set_pending_realm_id(&mut self, realm_id: RealmId) {
        self.pending_realm_id = Some(realm_id);
    }

    /// Get an upvalue value from the current call frame
    #[inline]
    pub fn get_upvalue(&self, index: u16) -> VmResult<Value> {
        let frame = self
            .current_frame()
            .ok_or_else(|| VmError::internal("no call frame"))?;
        let cell = frame
            .upvalues
            .get(index as usize)
            .ok_or_else(|| VmError::internal(format!("upvalue index {} out of bounds", index)))?;
        Ok(cell.get())
    }

    /// Get an upvalue cell from the current call frame (for capturing)
    #[inline]
    pub fn get_upvalue_cell(&self, index: u16) -> VmResult<&UpvalueCell> {
        let frame = self
            .current_frame()
            .ok_or_else(|| VmError::internal("no call frame"))?;
        frame
            .upvalues
            .get(index as usize)
            .ok_or_else(|| VmError::internal(format!("upvalue index {} out of bounds", index)))
    }

    /// Set an upvalue in the current call frame
    #[inline]
    pub fn set_upvalue(&mut self, index: u16, value: Value) -> VmResult<()> {
        let frame = self
            .current_frame()
            .ok_or_else(|| VmError::internal("no call frame"))?;
        let cell = frame
            .upvalues
            .get(index as usize)
            .ok_or_else(|| VmError::internal(format!("upvalue index {} out of bounds", index)))?;
        let _ = cell.set(value);
        Ok(())
    }

    /// Get or create an open upvalue cell for a local variable in the current frame.
    /// If the cell already exists, return the existing one (shared between closures).
    pub fn get_or_create_open_upvalue(&mut self, local_idx: u16) -> VmResult<UpvalueCell> {
        let frame = self
            .current_frame()
            .ok_or_else(|| VmError::internal("no call frame"))?;
        let frame_id = frame.frame_id;
        let key = (frame_id, local_idx);

        if let Some(cell) = self.open_upvalues.get(&key) {
            return Ok(cell.clone());
        }

        // Create a new cell with the current local value
        let value = self.get_local(local_idx)?;
        let cell = UpvalueCell::new(value);
        self.open_upvalues.insert(key, cell.clone());
        Ok(cell)
    }

    /// Close an upvalue: sync the local variable's current value to the cell
    /// and remove from open upvalues map. Called when exiting a scope where
    /// the local was captured.
    pub fn close_upvalue(&mut self, local_idx: u16) -> VmResult<()> {
        let frame = self
            .current_frame()
            .ok_or_else(|| VmError::internal("no call frame"))?;
        let frame_id = frame.frame_id;
        let key = (frame_id, local_idx);

        if let Some(cell) = self.open_upvalues.get(&key) {
            // Sync the current local value into the cell
            let value = self.get_local(local_idx)?;
            let _ = cell.set(value);
        }
        // Remove from open upvalues (the closures keep their own clones of the cell)
        self.open_upvalues.remove(&key);
        Ok(())
    }

    /// Clean up all open upvalues for a frame that's being popped
    pub fn close_all_upvalues_for_frame(&mut self, frame_id: usize) {
        self.open_upvalues.retain(|(fid, _), _| *fid != frame_id);
    }

    /// Get the `this` value of the current call frame
    pub fn this_value(&self) -> Value {
        self.current_frame()
            .map(|f| f.this_value.clone())
            .unwrap_or_else(Value::undefined)
    }

    /// Check if context is running
    pub fn is_running(&self) -> bool {
        self.running
    }

    /// Set running state
    pub fn set_running(&mut self, running: bool) {
        self.running = running;
    }

    /// Get stack trace for error reporting
    pub fn stack_trace(&self) -> Vec<SourceLocation> {
        self.call_stack
            .iter()
            .rev()
            .filter_map(|frame| frame.source_location.clone())
            .collect()
    }

    /// Capture current JS stack for CPU profiling
    /// Returns frames with function names, files, and line numbers
    #[cfg(feature = "profiling")]
    pub fn capture_profiler_stack(&self) -> Vec<otter_profiler::StackFrame> {
        self.call_stack
            .iter()
            .rev()
            .map(|frame| {
                let func = frame.module.function(frame.function_index);
                let func_name = func
                    .and_then(|f| f.name.clone())
                    .unwrap_or_else(|| "(anonymous)".to_string());
                let (file, line, column) = frame
                    .source_location
                    .as_ref()
                    .map(|loc| (Some(loc.file.clone()), Some(loc.line), Some(loc.column)))
                    .unwrap_or((None, None, None));

                otter_profiler::StackFrame {
                    function: func_name,
                    file,
                    line,
                    column,
                }
            })
            .collect()
    }

    // ==================== Async Context Save/Restore ====================

    /// Save all call frames as SavedFrames for async suspension
    ///
    /// This captures the complete call stack state so we can restore it later.
    /// Includes both locals and registers for each frame.
    pub fn save_frames(&self) -> Vec<SavedFrame> {
        self.call_stack
            .iter()
            .map(|frame| {
                // Extract registers for this frame (256 registers per frame)
                let reg_start = frame.register_base;
                let reg_end = (reg_start + MAX_REGISTERS).min(self.registers.len());
                let frame_registers = self.registers[reg_start..reg_end].to_vec();

                SavedFrame::new(
                    frame.function_index,
                    Arc::clone(&frame.module),
                    frame.realm_id,
                    frame.pc,
                    frame.locals.clone(),
                    frame_registers,
                    frame.upvalues.clone(),
                    frame.return_register,
                    frame.this_value.clone(),
                    frame.is_construct,
                    frame.is_async,
                    frame.frame_id,
                    frame.argc,
                )
            })
            .collect()
    }

    /// Restore call frames from SavedFrames after async resumption
    ///
    /// This replaces the current call stack with the saved state.
    /// Restores both locals and registers for each frame.
    pub fn restore_frames(&mut self, saved_frames: Vec<SavedFrame>) -> VmResult<()> {
        // Clear current call stack
        self.call_stack.clear();

        // Ensure we have enough registers for all frames
        let max_registers_needed = saved_frames.len() * MAX_REGISTERS;
        if max_registers_needed > self.registers.len() {
            self.registers
                .resize(max_registers_needed, Value::undefined());
        }

        // Restore each frame
        for (i, saved) in saved_frames.into_iter().enumerate() {
            let register_base = i * MAX_REGISTERS;

            // Restore registers for this frame
            for (j, reg) in saved.registers.into_iter().enumerate() {
                if register_base + j < self.registers.len() {
                    self.registers[register_base + j] = reg;
                }
            }

            self.call_stack.push(CallFrame {
                function_index: saved.function_index,
                module: saved.module,
                realm_id: saved.realm_id,
                pc: saved.pc,
                register_base,
                locals: saved.locals,
                upvalues: saved.upvalues,
                return_register: saved.return_register,
                source_location: None,
                this_value: saved.this_value,
                is_construct: saved.is_construct,
                is_async: saved.is_async,
                frame_id: saved.frame_id,
                argc: saved.argc,
                home_object: None,
                new_target_proto: None,
                this_initialized: true,
                is_derived: false,
                callee_value: None,
            });

            // Update next_frame_id to be greater than any restored frame
            if saved.frame_id >= self.next_frame_id {
                self.next_frame_id = saved.frame_id + 1;
            }
        }

        if let Some(frame) = self.call_stack.last() {
            self.switch_realm(frame.realm_id);
        }

        Ok(())
    }

    /// Get mutable access to the call stack (for advanced manipulation)
    pub fn call_stack_mut(&mut self) -> &mut Vec<CallFrame> {
        &mut self.call_stack
    }

    /// Get the call stack (for inspection)
    pub fn call_stack(&self) -> &[CallFrame] {
        &self.call_stack
    }

    pub fn registers_to_trace(&self) -> &[Value] {
        &self.registers
    }

    pub fn pending_args_to_trace(&self) -> &[Value] {
        &self.pending_args
    }

    pub fn pending_this_to_trace(&self) -> Option<&Value> {
        self.pending_this.as_ref()
    }

    pub fn pending_upvalues_to_trace(&self) -> &[UpvalueCell] {
        &self.pending_upvalues
    }

    pub fn open_upvalues_to_trace(&self) -> &HashMap<(usize, u16), UpvalueCell> {
        &self.open_upvalues
    }

    // ─────────────────────────────────────────────────────────────────────────────
    // Garbage Collection
    // ─────────────────────────────────────────────────────────────────────────────

    /// Trigger a garbage collection cycle
    ///
    /// This performs a stop-the-world mark/sweep collection:
    /// 1. Collects all root pointers from VM state
    /// 2. Marks all reachable objects
    /// 3. Sweeps (frees) all unreachable objects
    ///
    /// Returns the number of bytes reclaimed.
    pub fn collect_garbage(&self) -> usize {
        let roots = self.collect_gc_roots();
        let ephemeron_tables = self.collect_ephemeron_tables();

        let reclaimed = if ephemeron_tables.is_empty() {
            otter_vm_gc::global_registry()
                .collect_with_pre_sweep_hook(&roots, crate::weak_gc::combined_pre_sweep_hook)
        } else {
            let table_refs: Vec<_> = ephemeron_tables.iter().map(|t| t.as_ref()).collect();
            otter_vm_gc::global_registry().collect_with_ephemerons_and_pre_sweep_hook(
                &roots,
                &table_refs,
                crate::weak_gc::combined_pre_sweep_hook,
            )
        };

        // Update memory manager with post-GC state
        let live_bytes = otter_vm_gc::global_registry().total_bytes();
        self.memory_manager.on_gc_complete(live_bytes);

        reclaimed
    }

    /// Collect all ephemeron tables from WeakMap/WeakSet objects
    ///
    /// This traverses all root values and collects any ephemeron tables
    /// for proper weak collection semantics during GC.
    fn collect_ephemeron_tables(&self) -> Vec<crate::gc::GcRef<otter_vm_gc::EphemeronTable>> {
        let mut tables = Vec::new();
        let mut visited = std::collections::HashSet::new();

        // Helper to check a value for ephemeron tables
        let mut check_value = |value: &Value| {
            // Direct ephemeron table value
            if let Some(table) = value.as_ephemeron_table() {
                let ptr = table.as_ptr() as usize;
                if visited.insert(ptr) {
                    tables.push(table);
                }
            }

            // WeakMap/WeakSet object containing ephemeron table
            if let Some(obj) = value.as_object() {
                // Check for __weakmap_entries__
                if let Some(entries_value) =
                    obj.get(&crate::object::PropertyKey::string("__weakmap_entries__"))
                {
                    if let Some(table) = entries_value.as_ephemeron_table() {
                        let ptr = table.as_ptr() as usize;
                        if visited.insert(ptr) {
                            tables.push(table);
                        }
                    }
                }
                // Check for __weakset_entries__
                if let Some(entries_value) =
                    obj.get(&crate::object::PropertyKey::string("__weakset_entries__"))
                {
                    if let Some(table) = entries_value.as_ephemeron_table() {
                        let ptr = table.as_ptr() as usize;
                        if visited.insert(ptr) {
                            tables.push(table);
                        }
                    }
                }
            }
        };

        // Check global object
        check_value(&Value::object(self.global));

        // Check registers
        for value in self.registers.iter() {
            check_value(value);
        }

        // Check call stack locals
        for frame in self.call_stack.iter() {
            for value in frame.locals.iter() {
                check_value(value);
            }
            check_value(&frame.this_value);
        }

        // Check root slots
        for value in self.root_slots.iter() {
            check_value(value);
        }

        // Check exception
        if let Some(exc) = &self.exception {
            check_value(exc);
        }

        // Check pending args
        for value in self.pending_args.iter() {
            check_value(value);
        }

        // Check pending this
        if let Some(this) = &self.pending_this {
            check_value(this);
        }

        tables
    }

    /// Trigger GC if allocation threshold is exceeded.
    ///
    /// Uses incremental marking when possible: starts marking, then processes
    /// a budget of gray objects per safepoint. Falls back to full STW collection
    /// when ephemeron tables are present (fixpoint requires complete worklist).
    ///
    /// Returns true if GC work was performed.
    pub fn maybe_collect_garbage(&self) -> bool {
        /// Budget: number of objects to mark per incremental step (~50-100μs)
        const MARKING_BUDGET: usize = 1000;

        let registry = otter_vm_gc::global_registry();

        // If incremental marking is in progress, do a step
        if registry.is_marking() {
            let done = registry.incremental_mark_step(MARKING_BUDGET);
            if done {
                let _reclaimed =
                    registry.finish_gc_with_pre_sweep_hook(crate::weak_gc::combined_pre_sweep_hook);
                let live_bytes = registry.total_bytes();
                self.memory_manager.on_gc_complete(live_bytes);
            }
            return true;
        }

        // Check if we should start a new GC cycle
        if self.memory_manager.should_collect_garbage() {
            let roots = self.collect_gc_roots();
            let ephemeron_tables = self.collect_ephemeron_tables();

            if ephemeron_tables.is_empty() {
                // Start incremental marking
                registry.start_incremental_gc(&roots);
                // Do first step immediately
                let done = registry.incremental_mark_step(MARKING_BUDGET);
                if done {
                    let _reclaimed = registry
                        .finish_gc_with_pre_sweep_hook(crate::weak_gc::combined_pre_sweep_hook);
                    let live_bytes = registry.total_bytes();
                    self.memory_manager.on_gc_complete(live_bytes);
                }
            } else {
                // Ephemerons: full STW (fixpoint needs complete worklist)
                let table_refs: Vec<_> = ephemeron_tables.iter().map(|t| t.as_ref()).collect();
                registry.collect_with_ephemerons_and_pre_sweep_hook(
                    &roots,
                    &table_refs,
                    crate::weak_gc::combined_pre_sweep_hook,
                );
                let live_bytes = registry.total_bytes();
                self.memory_manager.on_gc_complete(live_bytes);
            }
            return true;
        }

        false
    }

    /// Request an explicit GC cycle
    ///
    /// The GC will run at the next safepoint.
    pub fn request_gc(&self) {
        self.memory_manager.request_gc();
    }

    /// Get current heap size (bytes allocated by GC)
    pub fn heap_size(&self) -> usize {
        otter_vm_gc::global_registry().total_bytes()
    }

    /// Get GC statistics
    pub fn gc_stats(&self) -> otter_vm_gc::RegistryStats {
        otter_vm_gc::global_registry().stats()
    }

    /// Set the GC threshold (bytes before auto-collection)
    pub fn set_gc_threshold(&self, threshold: usize) {
        otter_vm_gc::global_registry().set_gc_threshold(threshold);
    }

    /// Collect all GC root pointers from VM state
    ///
    /// This gathers pointers to all GcHeaders that are currently reachable:
    /// - Global object
    /// - Registers
    /// - Call stack locals and upvalues
    /// - Root slots (HandleScope roots)
    /// - Exception value
    /// - Pending call arguments
    fn collect_gc_roots(&self) -> Vec<*const otter_vm_gc::GcHeader> {
        let mut roots: Vec<*const otter_vm_gc::GcHeader> = Vec::new();

        // Add global object
        roots.push(self.global.header() as *const _);

        // Add values from registers
        for value in self.registers.iter() {
            value.trace(&mut |header| roots.push(header));
        }

        // Add values from call stack
        for frame in self.call_stack.iter() {
            // Locals
            for value in frame.locals.iter() {
                value.trace(&mut |header| roots.push(header));
            }
            // Upvalues
            for cell in frame.upvalues.iter() {
                cell.get().trace(&mut |header| roots.push(header));
            }
            // This value
            frame.this_value.trace(&mut |header| roots.push(header));
            // Home object (for super calls)
            if let Some(ho) = &frame.home_object {
                roots.push(ho.header() as *const _);
            }
            // New target proto (for constructor chains)
            if let Some(ntp) = &frame.new_target_proto {
                roots.push(ntp.header() as *const _);
            }
            // Callee value (for arguments.callee)
            if let Some(cv) = &frame.callee_value {
                cv.trace(&mut |header| roots.push(header));
            }
        }

        // Add root slots (HandleScope roots)
        for value in self.root_slots.iter() {
            value.trace(&mut |header| roots.push(header));
        }

        // Add exception if any
        if let Some(exc) = &self.exception {
            exc.trace(&mut |header| roots.push(header));
        }

        // Add pending args
        for value in self.pending_args.iter() {
            value.trace(&mut |header| roots.push(header));
        }

        // Add pending this
        if let Some(this) = &self.pending_this {
            this.trace(&mut |header| roots.push(header));
        }

        // Add pending home object
        if let Some(ho) = &self.pending_home_object {
            roots.push(ho.header() as *const _);
        }

        // Add pending new target proto
        if let Some(ntp) = &self.pending_new_target_proto {
            roots.push(ntp.header() as *const _);
        }

        // Add pending callee value
        if let Some(cv) = &self.pending_callee_value {
            cv.trace(&mut |header| roots.push(header));
        }

        // Add pending upvalues
        for cell in self.pending_upvalues.iter() {
            cell.get().trace(&mut |header| roots.push(header));
        }

        // Add cached template objects
        for template_obj in self.template_cache.values() {
            roots.push(template_obj.header() as *const _);
        }

        // Add open upvalues
        for cell in self.open_upvalues.values() {
            cell.get().trace(&mut |header| roots.push(header));
        }

        // Add external root sets (runtime-managed queues, etc.)
        for root_set in &self.external_root_sets {
            root_set.trace_roots(&mut |header| roots.push(header));
        }

        // Add context-level intrinsic roots.
        if let Some(fp) = self.function_prototype_intrinsic {
            roots.push(fp.header() as *const _);
        }
        if let Some(gp) = self.generator_prototype_intrinsic {
            roots.push(gp.header() as *const _);
        }
        if let Some(agp) = self.async_generator_prototype_intrinsic {
            roots.push(agp.header() as *const _);
        }

        // Add all realm roots (globals, function prototypes, intrinsics, symbols).
        if let Some(registry) = &self.realm_registry {
            registry.trace_roots(&mut |header| roots.push(header));
        }

        // Add global symbol registry
        self.symbol_registry
            .trace_roots(&mut |header| roots.push(header));

        // NOTE: The global string intern table is NOT added as a root here.
        // Instead, `combined_pre_sweep_hook()` is called as a pre-sweep hook
        // (see `collect_garbage` / `maybe_collect_garbage`).  This hook:
        // 1. Prunes dead string table entries (weak interning)
        // 2. Clears dead WeakRef targets from the side table
        // 3. Sweeps FinalizationRegistry entries for dead targets

        roots
    }
}

impl std::fmt::Debug for VmContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VmContext")
            .field("stack_depth", &self.call_stack.len())
            .field("running", &self.running)
            .field("has_exception", &self.exception.is_some())
            .finish()
    }
}

/// A thread-safe wrapper for VmContext
pub struct SharedContext(Mutex<VmContext>);

impl SharedContext {
    /// Create a new shared context
    pub fn new(ctx: VmContext) -> Self {
        Self(Mutex::new(ctx))
    }

    /// Lock and access the context
    pub fn lock(&self) -> parking_lot::MutexGuard<'_, VmContext> {
        self.0.lock()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use otter_vm_bytecode::{Function, Instruction, Module, Register};

    fn dummy_module() -> Arc<Module> {
        let mut builder = Module::builder("test.js");
        let func = Function::builder()
            .name("main")
            .instruction(Instruction::Return { src: Register(0) })
            .build();
        builder.add_function(func);
        Arc::new(builder.build())
    }

    #[test]
    fn test_context_registers() {
        let runtime = crate::runtime::VmRuntime::new();
        let mut ctx = runtime.create_context();

        ctx.push_frame(0, dummy_module(), 0, None, false, false, 0)
            .unwrap();
        ctx.set_register(0, Value::int32(42));

        assert_eq!(ctx.get_register(0).as_int32(), Some(42));
    }

    #[test]
    fn test_context_locals() {
        let runtime = crate::runtime::VmRuntime::new();
        let mut ctx = runtime.create_context();

        ctx.push_frame(0, dummy_module(), 3, None, false, false, 0)
            .unwrap();
        ctx.set_local(0, Value::int32(1)).unwrap();
        ctx.set_local(1, Value::int32(2)).unwrap();
        ctx.set_local(2, Value::int32(3)).unwrap();

        assert_eq!(ctx.get_local(0).unwrap().as_int32(), Some(1));
        assert_eq!(ctx.get_local(1).unwrap().as_int32(), Some(2));
        assert_eq!(ctx.get_local(2).unwrap().as_int32(), Some(3));
    }

    #[test]
    fn test_stack_overflow() {
        let runtime = crate::runtime::VmRuntime::new();
        let memory_manager = runtime.memory_manager().clone();
        let global = GcRef::new(JsObject::new(Value::null(), memory_manager.clone()));
        // Use a small max_stack_depth for testing
        let test_max_depth = 100;
        let mut ctx = VmContext::with_config(
            global,
            test_max_depth,
            DEFAULT_MAX_NATIVE_DEPTH,
            memory_manager,
        );
        let module = dummy_module();

        // Push frames until overflow
        for _ in 0..test_max_depth {
            ctx.push_frame(0, Arc::clone(&module), 0, None, false, false, 0)
                .unwrap();
        }

        // Next push should fail
        let result = ctx.push_frame(0, module, 0, None, false, false, 0);
        assert!(matches!(result, Err(VmError::StackOverflow)));
    }

    #[test]
    fn test_native_call_depth() {
        let runtime = crate::runtime::VmRuntime::new();
        let memory_manager = runtime.memory_manager().clone();
        let global = GcRef::new(JsObject::new(Value::null(), memory_manager.clone()));
        let ctx = VmContext::with_config(global, DEFAULT_MAX_STACK_DEPTH, 3, memory_manager);

        // Should be able to enter 3 native calls
        assert!(ctx.enter_native_call().is_ok());
        assert!(ctx.enter_native_call().is_ok());
        assert!(ctx.enter_native_call().is_ok());

        // Fourth should fail
        assert!(ctx.enter_native_call().is_err());

        // Exit one, then should be able to enter again
        ctx.exit_native_call();
        assert!(ctx.enter_native_call().is_ok());
    }

    #[test]
    fn test_program_counter() {
        let runtime = crate::runtime::VmRuntime::new();
        let mut ctx = runtime.create_context();

        ctx.push_frame(0, dummy_module(), 0, None, false, false, 0)
            .unwrap();
        assert_eq!(ctx.pc(), 0);

        ctx.advance_pc();
        assert_eq!(ctx.pc(), 1);

        ctx.jump(5);
        assert_eq!(ctx.pc(), 6);

        ctx.jump(-3);
        assert_eq!(ctx.pc(), 3);
    }
}
