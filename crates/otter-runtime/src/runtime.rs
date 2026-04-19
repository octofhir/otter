//! Core runtime — owns VM state and drives execution to completion.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::Thread;
use std::time::Duration;

use otter_jit::deopt::execute_module_entry_with_runtime;
use otter_vm::Interpreter;
use otter_vm::interpreter::{ExecutionResult, RuntimeState};
use otter_vm::module::Module;

use crate::builder::RuntimeBuilder;
use crate::host::{
    HostConfig, HostState, ModuleLoader, ResolvedModule, execute_preloaded_entry,
    preload_module_graph,
};

/// Error from script execution.
#[derive(Debug)]
pub enum RunError {
    /// Source failed to compile. `Box<CompileDiagnostic>` carries the
    /// offending span + the original source text + the source URL so
    /// the CLI can render a miette code frame with a caret on the
    /// exact AST construct that failed.
    Compile(Box<crate::diagnostic::CompileDiagnostic>),
    /// Runtime error during execution.
    Runtime(String),
    /// Uncaught JS throw, with structured frames + source text for rich
    /// rendering (V8/Node-style stack header + miette snippet at the throw
    /// site). The diagnostic is `Box`ed to keep `RunError` small.
    JsThrow(Box<crate::diagnostic::JsRuntimeDiagnostic>),
}

impl std::fmt::Display for RunError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Compile(diag) => write!(f, "CompileError: {diag}"),
            Self::Runtime(e) => write!(f, "RuntimeError: {e}"),
            Self::JsThrow(diag) => write!(f, "{diag}"),
        }
    }
}

impl std::error::Error for RunError {}

/// Format an `InterpreterError` into a human-readable string.
///
/// For `UncaughtThrow`, applies ES spec §7.1.17 ToString to the thrown value
/// (matching how host environments like Node.js / browsers / JSC display
/// uncaught exceptions). For Error objects, ToString invokes
/// `Error.prototype.toString` (§20.5.3.4) which returns `"Name: Message"`.
pub(crate) fn format_interpreter_error(
    error: &otter_vm::interpreter::InterpreterError,
    state: &mut RuntimeState,
) -> String {
    use otter_vm::interpreter::InterpreterError;

    if let InterpreterError::UncaughtThrow(value) = error {
        return state.js_to_string_infallible(*value).to_string();
    }
    error.to_string()
}

/// Promote an `InterpreterError` into a `RunError`. For `UncaughtThrow` of
/// an Error-like object we prefer the structured `JsThrow` variant; for
/// thrown primitives or non-Error objects we fall back to the legacy
/// `Runtime(String)` form so older callers keep their existing display.
pub(crate) fn run_error_from_interpreter(
    error: &otter_vm::interpreter::InterpreterError,
    state: &mut RuntimeState,
) -> RunError {
    if let Some(diagnostic) = crate::diagnostic::build_js_diagnostic(error, state) {
        return RunError::JsThrow(Box::new(diagnostic));
    }
    RunError::Runtime(format_interpreter_error(error, state))
}

/// The Otter JavaScript runtime.
///
/// Created via [`OtterRuntime::builder()`]. Owns the VM state and provides
/// methods to execute JavaScript code with full event loop support.
///
/// # Example
///
/// ```rust,no_run
/// use otter_runtime::OtterRuntime;
///
/// let mut rt = OtterRuntime::builder().build();
/// rt.run_script("console.log(2 + 2)", "main.js").unwrap();
/// ```
pub struct OtterRuntime {
    state: RuntimeState,
    timeout: Option<Duration>,
    host: HostConfig,
    host_state: HostState,
    /// Per-call cooperative interrupt flag. Lives only between
    /// `run_module` enter / exit, gives `drain_microtasks` and
    /// `run_event_loop` something to poll so a runaway test that
    /// keeps re-enqueueing microtasks (e.g. async iterator + yield*
    /// loops) can be cut off by the watchdog instead of running
    /// forever at 100% CPU.
    current_interrupt: Option<Arc<RunInterrupt>>,
}

struct RunInterrupt {
    flag: Arc<AtomicBool>,
    vm_thread: Thread,
}

impl RunInterrupt {
    fn new() -> Self {
        Self {
            flag: Arc::new(AtomicBool::new(false)),
            vm_thread: std::thread::current(),
        }
    }

    fn flag(&self) -> Arc<AtomicBool> {
        self.flag.clone()
    }

    fn flag_ptr(&self) -> *const u8 {
        Arc::as_ptr(&self.flag).cast::<u8>()
    }

    fn interrupted(&self) -> bool {
        self.flag.load(Ordering::Acquire)
    }

    fn fire(&self) {
        self.flag.store(true, Ordering::Release);
        self.vm_thread.unpark();
    }
}

impl Drop for OtterRuntime {
    fn drop(&mut self) {
        // Dump JIT telemetry before cleanup if requested.
        if otter_jit::config::jit_config().dump_jit_stats {
            otter_jit::telemetry::snapshot().dump();
        }

        // Release all thread-local JIT state (code cache, telemetry, helper
        // symbols). Without this, every `OtterRuntime` instance leaks compiled
        // Cranelift JITModules and accumulated metrics into the thread-local
        // code cache — catastrophic for workloads that create many short-lived
        // runtimes (e.g. the test262 runner).
        otter_jit::cleanup_thread_locals();
    }
}

impl OtterRuntime {
    /// Returns a new [`RuntimeBuilder`] for configuring the runtime.
    pub fn builder() -> RuntimeBuilder {
        RuntimeBuilder::new()
    }

    /// Creates a runtime from pre-configured state. Called by the builder.
    pub(crate) fn from_state(
        mut state: RuntimeState,
        timeout: Option<Duration>,
        host: HostConfig,
    ) -> Self {
        // Install the default JSC-style tier-up hook. Enables in-process
        // compilation of hot inner functions on `CallClosure` after the
        // per-function hotness budget expires. Without this, only the
        // top-level module entry reaches the JIT — inner hot loops stay
        // fully interpreted.
        //
        // The hook is stateless (backed by the thread-local code cache), so
        // one `Arc` shared across every runtime on this thread is fine.
        state.set_tier_up_hook(otter_jit::tier_up_hook::DefaultTierUpHook::new_arc());
        Self {
            state,
            timeout,
            host,
            host_state: HostState::default(),
            current_interrupt: None,
        }
    }

    /// Compiles and executes a JavaScript source string to completion.
    ///
    /// Includes:
    /// - Source compilation (via oxc parser)
    /// - Top-level execution
    /// - Microtask drain (promise reactions, queueMicrotask, nextTick)
    /// - Event loop (setTimeout/setInterval callbacks)
    pub fn run_script(
        &mut self,
        source: &str,
        source_url: &str,
    ) -> Result<ExecutionResult, RunError> {
        let module = otter_vm::source::compile_script(source, source_url).map_err(|e| {
            RunError::Compile(Box::new(
                crate::diagnostic::CompileDiagnostic::from_source_lowering_error(
                    &e,
                    std::sync::Arc::from(source),
                    source_url,
                ),
            ))
        })?;
        self.run_module(&module)
    }

    /// Reads a file and executes it as a JavaScript module.
    /// Module semantics (strict by default, supports `import` /
    /// `export` / `import.meta`) are the default for any file on
    /// disk — classic-script semantics stay reachable via
    /// `run_script` / `-e`. This matches modern toolchains (Bun,
    /// Node with `--input-type=module`) where every file is
    /// assumed to be a module.
    pub fn run_file(&mut self, path: &str) -> Result<ExecutionResult, RunError> {
        let source = std::fs::read_to_string(path)
            .map_err(|e| RunError::Runtime(format!("failed to read {path}: {e}")))?;
        let url = std::path::Path::new(path)
            .canonicalize()
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_else(|_| path.to_string());
        self.run_module_source(&source, &url)
    }

    /// Evaluates JavaScript source and returns the completion value of the
    /// last expression statement. Uses eval-mode compilation.
    /// Spec: <https://tc39.es/ecma262/#sec-eval-x>
    pub fn eval(&mut self, code: &str) -> Result<ExecutionResult, RunError> {
        let module = otter_vm::source::compile_eval(code, "<eval>").map_err(|e| {
            RunError::Compile(Box::new(
                crate::diagnostic::CompileDiagnostic::from_source_lowering_error(
                    &e,
                    std::sync::Arc::from(code),
                    "<eval>",
                ),
            ))
        })?;
        self.run_module(&module)
    }

    /// Compiles and executes a JavaScript module (ESM mode).
    /// Supports top-level `await`.
    /// Spec: <https://tc39.es/ecma262/#sec-modules>
    pub fn run_module_source(
        &mut self,
        source: &str,
        source_url: &str,
    ) -> Result<ExecutionResult, RunError> {
        let module = otter_vm::source::compile_module(source, source_url).map_err(|e| {
            RunError::Compile(Box::new(
                crate::diagnostic::CompileDiagnostic::from_source_lowering_error(
                    &e,
                    std::sync::Arc::from(source),
                    source_url,
                ),
            ))
        })?;
        self.run_module(&module)
    }

    /// Executes a pre-compiled module to completion with the runtime's state.
    pub fn run_module(&mut self, module: &Module) -> Result<ExecutionResult, RunError> {
        // Clear any stale OOM flag from a previous script — a once-hit heap
        // cap should not abort every subsequent execution on the same runtime.
        self.state.clear_oom_flag();

        // Set up timeout interrupt if configured, and attach the shared OOM
        // signal so the interpreter raises a catchable RangeError when the
        // configured heap cap is exceeded.
        let interrupt = Arc::new(RunInterrupt::new());
        let interrupt_flag = interrupt.flag();
        let oom_flag = self.state.oom_flag();
        let interpreter = Interpreter::new()
            .with_interrupt_flag(interrupt_flag.clone())
            .with_oom_flag(oom_flag);
        let _interrupt_guard = self
            .timeout
            .map(|timeout| TimeoutGuard::arm(interrupt.clone(), timeout));
        let interrupt_ptr = interrupt.flag_ptr();
        // Publish the flag so `drain_microtasks` and `run_event_loop`
        // can poll it; cleared at the end of this call so the next
        // `run_module` invocation gets a fresh one.
        self.current_interrupt = Some(interrupt.clone());

        let run_result =
            (|| -> Result<ExecutionResult, otter_vm::interpreter::InterpreterError> {
                self.state
                    .set_active_interrupt_flag(Some(interrupt_flag.clone()));

                // 1. Execute top-level code.
                let result = match execute_module_entry_with_runtime(
                    module,
                    &mut self.state,
                    interrupt_ptr,
                    Some(interrupt_flag.clone()),
                ) {
                    Ok(result) => result,
                    Err(_) => interpreter.execute_module(module, &mut self.state)?,
                };

                // 2. Drain microtasks after top-level execution (ES spec).
                self.drain_microtasks(module)?;

                // 3. Event loop: process pending timers + microtasks until quiescent.
                self.run_event_loop(module)?;

                Ok(result)
            })();

        // Drop the published interrupt flag so subsequent runs get a
        // fresh one (and `drain_microtasks` from outside `run_module`
        // — there is none today, but be defensive — sees None).
        self.current_interrupt = None;
        self.state.set_active_interrupt_flag(None);

        match run_result {
            Ok(result) => Ok(result),
            Err(error) => Err(run_error_from_interpreter(&error, &mut self.state)),
        }
    }

    /// Returns a reference to the underlying VM runtime state.
    /// Used by embedders that need direct access to intrinsics or the object heap.
    pub fn state(&self) -> &RuntimeState {
        &self.state
    }

    /// Returns a mutable reference to the underlying VM runtime state.
    pub fn state_mut(&mut self) -> &mut RuntimeState {
        &mut self.state
    }

    /// Returns the runtime host configuration.
    pub fn host(&self) -> &HostConfig {
        &self.host
    }

    /// Resolve and load one hosted entry specifier through the runtime's
    /// module loader configuration.
    pub fn load_entry_specifier(
        &self,
        specifier: &str,
        referrer: Option<&str>,
    ) -> Result<ResolvedModule, RunError> {
        let loader = ModuleLoader::new(self.host.loader().clone());
        loader
            .load(specifier, referrer)
            .map_err(|error| RunError::Runtime(error.to_string()))
    }

    /// Execute one hosted entry specifier through the runtime-owned host state.
    ///
    /// The current hosted path supports multi-module ESM/CommonJS graphs and
    /// hosted JSON modules through runtime-local module session state. Native
    /// hosted modules still need dedicated follow-up work.
    pub fn run_entry_specifier(
        &mut self,
        specifier: &str,
        referrer: Option<&str>,
    ) -> Result<ExecutionResult, RunError> {
        let loader = ModuleLoader::new(self.host.loader().clone());
        let graph = loader
            .load_graph(specifier, referrer)
            .map_err(|error| RunError::Runtime(error.to_string()))?;
        let entry = graph.entry().ok_or_else(|| {
            RunError::Runtime(format!(
                "hosted module graph for '{specifier}' did not produce an entry node"
            ))
        })?;
        let module = &entry.module;

        if module.source.is_empty() {
            let has_native_module = self.host.native_modules().contains(&module.url);
            if !has_native_module {
                return Err(RunError::Runtime(format!(
                    "hosted native module '{}' is not registered on this runtime",
                    module.url
                )));
            }
        }

        let session = self
            .host_state
            .ensure_module_runtime(&mut self.state, &self.host);
        if let Err(error) =
            preload_module_graph(&mut self.state, session, self.host.loader().clone(), &graph)
        {
            return Err(self.lift_pending_throw_or(error));
        }
        execute_preloaded_entry(&mut self.state, session, &module.url)
            .map_err(|error| self.lift_pending_throw_or(error))
    }

    /// Promotes a stashed pending uncaught throw (set by the host module
    /// runtime when JS code threw inside a hosted module load) into a
    /// structured `RunError::JsThrow`. Falls back to the legacy
    /// `RunError::Runtime` form when no throw is pending.
    fn lift_pending_throw_or(&mut self, fallback_message: String) -> RunError {
        if let Some(value) = self.state.take_pending_uncaught_throw() {
            let error = otter_vm::interpreter::InterpreterError::UncaughtThrow(value);
            if let Some(diagnostic) =
                crate::diagnostic::build_js_diagnostic(&error, &mut self.state)
            {
                return RunError::JsThrow(Box::new(diagnostic));
            }
        }
        RunError::Runtime(fallback_message)
    }

    // -----------------------------------------------------------------------
    // Internal: microtask drain
    // -----------------------------------------------------------------------

    /// Returns `true` when the watchdog has tripped this run's interrupt
    /// flag. Used by `drain_microtasks` and `run_event_loop` to bail out
    /// of host-side reactor loops that would otherwise spin at 100% CPU
    /// (async iterator + yield* infinite reaction queues, etc.).
    fn interrupted(&self) -> bool {
        self.current_interrupt
            .as_ref()
            .map(|interrupt| interrupt.interrupted())
            .unwrap_or(false)
    }

    fn check_interrupt(&self) -> Result<(), otter_vm::interpreter::InterpreterError> {
        if self.interrupted() {
            Err(otter_vm::interpreter::InterpreterError::Interrupted)
        } else {
            Ok(())
        }
    }

    fn drain_microtasks(
        &mut self,
        module: &Module,
    ) -> Result<(), otter_vm::interpreter::InterpreterError> {
        loop {
            // Cooperative bail: if the watchdog has set the interrupt
            // flag, stop draining instead of looping forever on a test
            // that keeps re-enqueueing reactions. The interpreter's
            // own bytecode loop checks the same flag at back-edges.
            self.check_interrupt()?;
            let mut did_work = false;

            while let Some(job) = self.state.microtasks_mut().pop_next_tick() {
                self.check_interrupt()?;
                let _ = Interpreter::call_function(
                    &mut self.state,
                    module,
                    job.callback,
                    job.this_value,
                    &job.args,
                );
                did_work = true;
            }

            while let Some(job) = self.state.microtasks_mut().pop_promise_job() {
                self.check_interrupt()?;
                // ES2024 §27.2.2.1 NewPromiseReactionJob
                let callback_kind = self.state.objects().kind(job.callback);
                // Self-settling callables handle their own promise settlement.
                // PromiseFinallyFunction/PromiseValueThunk return values that
                // MUST be used to settle the downstream promise.
                let callback_is_self_settling = matches!(
                    callback_kind,
                    Ok(otter_vm::object::HeapValueKind::PromiseCapabilityFunction
                        | otter_vm::object::HeapValueKind::PromiseCombinatorElement)
                );

                // Call the handler with the settled value.
                let call_result = Interpreter::call_function(
                    &mut self.state,
                    module,
                    job.callback,
                    job.this_value,
                    &[job.argument],
                );

                // If there's a result_promise AND the callback is a user handler
                // (not a capability function), settle it based on the handler's result.
                // §27.2.2.1 step 1.e-h: If handler returned normally, resolve;
                // if handler threw, reject.
                if let Some(result_promise) = job.result_promise
                    && !callback_is_self_settling
                {
                    match call_result {
                        Ok(handler_result) => {
                            // Resolve result_promise with the handler's return value.
                            let resolve =
                                self.state.objects_mut().alloc_promise_capability_function(
                                    result_promise,
                                    otter_vm::promise::ReactionKind::Fulfill,
                                );
                            let _ = Interpreter::call_function(
                                &mut self.state,
                                module,
                                resolve,
                                otter_vm::value::RegisterValue::undefined(),
                                &[handler_result],
                            );
                        }
                        Err(err) => {
                            // Handler threw — reject result_promise with the error.
                            // §27.2.2.1 step 1.g
                            let reason = match err {
                                otter_vm::interpreter::InterpreterError::UncaughtThrow(v) => v,
                                _ => otter_vm::value::RegisterValue::undefined(),
                            };
                            if let Some(promise) =
                                self.state.objects_mut().get_promise_mut(result_promise)
                                && let Some(jobs) = promise.reject(reason)
                            {
                                for j in jobs {
                                    self.state.microtasks_mut().enqueue_promise_job(j);
                                }
                            }
                        }
                    }
                }
                did_work = true;
            }

            while let Some(job) = self.state.microtasks_mut().pop_microtask() {
                self.check_interrupt()?;
                let _ = Interpreter::call_function(
                    &mut self.state,
                    module,
                    job.callback,
                    job.this_value,
                    &job.args,
                );
                did_work = true;
            }

            if !did_work {
                break;
            }
        }
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Internal: event loop
    // -----------------------------------------------------------------------

    fn sleep_until_interruptible(
        &self,
        deadline: std::time::Instant,
    ) -> Result<(), otter_vm::interpreter::InterpreterError> {
        loop {
            self.check_interrupt()?;
            let now = std::time::Instant::now();
            if now >= deadline {
                return Ok(());
            }
            std::thread::park_timeout(deadline - now);
        }
    }

    fn run_event_loop(
        &mut self,
        module: &Module,
    ) -> Result<(), otter_vm::interpreter::InterpreterError> {
        loop {
            // Watchdog bail: matches the same poll inside drain_microtasks.
            self.check_interrupt()?;
            self.state.drain_host_callbacks();
            self.drain_microtasks(module)?;

            let has_timers = self.state.timers().has_pending();
            let has_microtasks = !self.state.microtasks().is_empty();
            let has_host_callbacks = self.state.has_pending_host_callbacks();

            if !has_timers && !has_microtasks && !has_host_callbacks {
                break;
            }

            let fired = self
                .state
                .timers_mut()
                .collect_fired(std::time::Instant::now());

            if fired.is_empty() && !has_microtasks {
                if has_host_callbacks {
                    let timeout = self.state.timers().next_deadline().map(|deadline| {
                        deadline.saturating_duration_since(std::time::Instant::now())
                    });
                    let interrupt = self.current_interrupt.clone();
                    if self
                        .state
                        .wait_for_host_callbacks_interruptible(timeout, || {
                            interrupt
                                .as_ref()
                                .map(|interrupt| interrupt.interrupted())
                                .unwrap_or(false)
                        })
                    {
                        self.drain_microtasks(module)?;
                        continue;
                    }
                    self.check_interrupt()?;
                }

                if let Some(deadline) = self.state.timers().next_deadline() {
                    self.sleep_until_interruptible(deadline)?;
                    continue;
                }
                break;
            }

            for timer in &fired {
                let _ = Interpreter::call_function(
                    &mut self.state,
                    module,
                    timer.callback,
                    timer.this_value,
                    &[],
                );
                self.drain_microtasks(module)?;
            }

            self.drain_microtasks(module)?;
        }
        Ok(())
    }
}

/// RAII guard that signals an interrupt flag after a timeout.
///
/// A shared watchdog thread (one per process) owns a min-heap of pending
/// deadlines. Arming the guard pushes an entry; dropping the guard marks
/// that entry cancelled so the watchdog skips it. This replaces the old
/// "spawn one thread per `run_script`" design which leaked threads under
/// bulk harnesses like test262 — every guard would sleep for the full
/// timeout even after the run had long finished, eventually tripping the
/// OS thread limit (macOS: ~4096 per task) and panicking the runner.
struct TimeoutGuard {
    entry: Option<Arc<TimeoutEntry>>,
}

impl TimeoutGuard {
    fn arm(interrupt: Arc<RunInterrupt>, timeout: Duration) -> Self {
        let deadline = std::time::Instant::now() + timeout;
        let entry = Arc::new(TimeoutEntry {
            deadline,
            cancelled: AtomicBool::new(false),
            interrupt,
        });
        timeout_watchdog().enqueue(entry.clone());
        Self { entry: Some(entry) }
    }
}

impl Drop for TimeoutGuard {
    fn drop(&mut self) {
        if let Some(entry) = self.entry.take() {
            // Mark cancelled so the shared watchdog skips firing the flag.
            // The entry stays in the heap until its deadline expires, which
            // is fine — only the cancel flag matters at fire time.
            entry.cancelled.store(true, Ordering::Release);
        }
    }
}

struct TimeoutEntry {
    deadline: std::time::Instant,
    cancelled: AtomicBool,
    interrupt: Arc<RunInterrupt>,
}

/// Min-heap ordering: earliest deadline first.
impl PartialEq for TimeoutEntry {
    fn eq(&self, other: &Self) -> bool {
        self.deadline == other.deadline
    }
}
impl Eq for TimeoutEntry {}
impl PartialOrd for TimeoutEntry {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for TimeoutEntry {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        // BinaryHeap is a max-heap; reverse so the earliest deadline pops first.
        other.deadline.cmp(&self.deadline)
    }
}

struct TimeoutWatchdog {
    inner: std::sync::Mutex<std::collections::BinaryHeap<Arc<TimeoutEntry>>>,
    condvar: std::sync::Condvar,
}

impl TimeoutWatchdog {
    fn enqueue(&self, entry: Arc<TimeoutEntry>) {
        let mut heap = self.inner.lock().expect("timeout watchdog mutex poisoned");
        let need_wake = heap
            .peek()
            .is_none_or(|head| entry.deadline < head.deadline);
        heap.push(entry);
        if need_wake {
            self.condvar.notify_one();
        }
    }
}

/// Returns the shared watchdog instance, lazily starting its worker thread.
fn timeout_watchdog() -> &'static TimeoutWatchdog {
    static WATCHDOG: std::sync::OnceLock<&'static TimeoutWatchdog> = std::sync::OnceLock::new();
    WATCHDOG.get_or_init(|| {
        let watchdog: &'static TimeoutWatchdog = Box::leak(Box::new(TimeoutWatchdog {
            inner: std::sync::Mutex::new(std::collections::BinaryHeap::new()),
            condvar: std::sync::Condvar::new(),
        }));
        // Daemon thread: detached, never joined, runs for the lifetime of
        // the process. This is the *only* watchdog thread for the whole
        // runtime, no matter how many `run_script` calls we make.
        std::thread::Builder::new()
            .name("otter-timeout-watchdog".into())
            .spawn(move || timeout_watchdog_loop(watchdog))
            .expect("failed to spawn timeout watchdog");
        watchdog
    })
}

fn timeout_watchdog_loop(watchdog: &'static TimeoutWatchdog) {
    use std::time::Instant;
    let mut heap = watchdog
        .inner
        .lock()
        .expect("timeout watchdog mutex poisoned");
    loop {
        // Discard any cancelled entries at the head before waiting.
        while let Some(top) = heap.peek() {
            if top.cancelled.load(Ordering::Acquire) {
                heap.pop();
            } else {
                break;
            }
        }

        if let Some(top) = heap.peek().cloned() {
            let now = Instant::now();
            if now >= top.deadline {
                heap.pop();
                if !top.cancelled.load(Ordering::Acquire) {
                    top.interrupt.fire();
                }
                continue;
            }
            let wait = top.deadline - now;
            let (new_heap, _) = watchdog
                .condvar
                .wait_timeout(heap, wait)
                .expect("timeout watchdog mutex poisoned");
            heap = new_heap;
        } else {
            // Heap empty — sleep indefinitely until something is enqueued.
            heap = watchdog
                .condvar
                .wait(heap)
                .expect("timeout watchdog mutex poisoned");
        }
    }
}
