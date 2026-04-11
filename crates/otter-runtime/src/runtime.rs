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
    /// Source failed to compile.
    Compile(String),
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
            Self::Compile(e) => write!(f, "CompileError: {e}"),
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
        state: RuntimeState,
        timeout: Option<Duration>,
        host: HostConfig,
    ) -> Self {
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
        let module = otter_vm::source::compile_script(source, source_url)
            .map_err(|e| RunError::Compile(e.to_string()))?;
        self.run_module(&module)
    }

    /// Reads a file and executes it as a JavaScript script.
    pub fn run_file(&mut self, path: &str) -> Result<ExecutionResult, RunError> {
        let source = std::fs::read_to_string(path)
            .map_err(|e| RunError::Runtime(format!("failed to read {path}: {e}")))?;
        let url = std::path::Path::new(path)
            .canonicalize()
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_else(|_| path.to_string());
        self.run_script(&source, &url)
    }

    /// Evaluates JavaScript source and returns the completion value of the
    /// last expression statement. Uses eval-mode compilation.
    /// Spec: <https://tc39.es/ecma262/#sec-eval-x>
    pub fn eval(&mut self, code: &str) -> Result<ExecutionResult, RunError> {
        let module = otter_vm::source::compile_eval(code, "<eval>")
            .map_err(|e| RunError::Compile(e.to_string()))?;
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
        let module = otter_vm::source::compile_module(source, source_url)
            .map_err(|e| RunError::Compile(e.to_string()))?;
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
            .map_or(true, |head| entry.deadline < head.deadline);
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

#[cfg(test)]
mod tests {
    use super::*;
    use otter_vm::console::CaptureConsoleBackend;
    use otter_vm::descriptors::{NativeFunctionDescriptor, VmNativeCallError};
    use otter_vm::interpreter::{InterpreterError, RuntimeState};
    use otter_vm::value::RegisterValue;
    use std::sync::Arc;
    use std::time::Duration;

    fn rt_with_capture() -> (OtterRuntime, Arc<CaptureConsoleBackend>) {
        let capture = Arc::new(CaptureConsoleBackend::new());
        let rt = OtterRuntime::builder()
            .console(CaptureForTest(capture.clone()))
            .build();
        (rt, capture)
    }

    fn assert_runtime_interrupted(error: RunError) {
        match error {
            RunError::Runtime(message) => {
                assert!(
                    message.contains("execution interrupted"),
                    "expected execution interrupted, got {message}"
                );
            }
            other => panic!("expected Runtime execution interrupted, got {other:?}"),
        }
    }

    struct CaptureForTest(Arc<CaptureConsoleBackend>);
    impl otter_vm::console::ConsoleBackend for CaptureForTest {
        fn log(&self, msg: &str) {
            self.0.log(msg);
        }
        fn warn(&self, msg: &str) {
            self.0.warn(msg);
        }
        fn error(&self, msg: &str) {
            self.0.error(msg);
        }
    }

    fn cooperative_native_spin(
        _this: &RegisterValue,
        _args: &[RegisterValue],
        runtime: &mut RuntimeState,
    ) -> Result<RegisterValue, VmNativeCallError> {
        let mut iterations = 0u64;
        loop {
            if iterations % 4096 == 0 {
                runtime.check_interrupt()?;
            }
            iterations = iterations.wrapping_add(1);
        }
    }

    #[test]
    fn run_simple_arithmetic() {
        let (mut rt, capture) = rt_with_capture();
        rt.run_script("console.log(1 + 2)", "test.js")
            .expect("should run");
        assert_eq!(capture.text(), "3");
    }

    #[test]
    fn run_console_log() {
        let (mut rt, capture) = rt_with_capture();
        rt.run_script("console.log(42)", "test.js")
            .expect("should run");
        assert_eq!(capture.text(), "42");
    }

    #[test]
    fn run_console_multiple_args() {
        let (mut rt, capture) = rt_with_capture();
        rt.run_script("console.log('hello', true, 3.14)", "test.js")
            .expect("should run");
        assert_eq!(capture.text(), "hello true 3.14");
    }

    #[test]
    fn run_function_declaration() {
        let (mut rt, capture) = rt_with_capture();
        rt.run_script(
            "function double(n) { return n * 2; } console.log(double(21))",
            "test.js",
        )
        .expect("should run");
        assert_eq!(capture.text(), "42");
    }

    #[test]
    fn uncaught_error_is_rendered_without_vm_prefix() {
        let mut state = RuntimeState::new();
        let error = state.alloc_type_error("boom").expect("type error alloc");
        let formatted = format_interpreter_error(
            &InterpreterError::UncaughtThrow(RegisterValue::from_object_handle(error.0)),
            &mut state,
        );
        assert_eq!(formatted, "TypeError: boom");
    }

    #[test]
    fn run_script_returns_js_throw_for_uncaught_error_with_stack() {
        // The structured `RunError::JsThrow` variant should fire whenever a
        // script throws an Error-like object. This is the path the CLI uses
        // to render miette snippets, so it's a load-bearing assertion.
        let (mut rt, _capture) = rt_with_capture();
        let err = rt
            .run_script(
                concat!(
                    "function doStuff() {\n",
                    "  throw new TypeError(\"boom\");\n",
                    "}\n",
                    "function main() {\n",
                    "  doStuff();\n",
                    "}\n",
                    "main();\n",
                ),
                "uncaught.js",
            )
            .expect_err("script should throw");
        match err {
            RunError::JsThrow(diag) => {
                assert_eq!(diag.name(), "TypeError");
                assert_eq!(diag.message(), "boom");
                assert!(
                    diag.rendered_stack().contains("TypeError: boom"),
                    "stack header missing TypeError: boom: {}",
                    diag.rendered_stack(),
                );
                // The stack should mention at least one of the user
                // functions; tail-call elision aside, both should appear
                // since neither is a tail call.
                assert!(
                    diag.rendered_stack().contains("doStuff")
                        || diag.rendered_stack().contains("main"),
                    "stack missing user frames: {}",
                    diag.rendered_stack(),
                );
                // At least one frame should resolve to a real (non-zero)
                // line number now that the source map is populated.
                let has_real_location = diag
                    .frames()
                    .iter()
                    .any(|f| f.location.map(|l| l.line() > 0).unwrap_or(false));
                assert!(
                    has_real_location,
                    "expected at least one frame with a non-zero line number",
                );
            }
            other => panic!("expected JsThrow, got {other:?}"),
        }
    }

    #[test]
    fn uncaught_primitive_throw_is_rendered_via_js_tostring() {
        let mut state = RuntimeState::new();
        let string = state.alloc_string("boom");
        let formatted = format_interpreter_error(
            &InterpreterError::UncaughtThrow(RegisterValue::from_object_handle(string.0)),
            &mut state,
        );
        assert_eq!(formatted, "boom");
    }

    #[test]
    fn run_set_timeout() {
        let (mut rt, capture) = rt_with_capture();
        rt.run_script(
            concat!(
                "console.log('before');\n",
                "setTimeout(function() { console.log('timer') }, 0);\n",
                "console.log('after');\n",
            ),
            "test.js",
        )
        .expect("should run");
        assert_eq!(capture.text(), "before\nafter\ntimer");
    }

    #[test]
    fn run_clear_timeout() {
        let (mut rt, capture) = rt_with_capture();
        rt.run_script(
            concat!(
                "var id = setTimeout(function() { console.log('cancelled') }, 50);\n",
                "clearTimeout(id);\n",
                "setTimeout(function() { console.log('ok') }, 0);\n",
            ),
            "test.js",
        )
        .expect("should run");
        assert_eq!(capture.text(), "ok");
    }

    #[test]
    fn run_timer_ordering() {
        let (mut rt, capture) = rt_with_capture();
        rt.run_script(
            concat!(
                "setTimeout(function() { console.log('b') }, 20);\n",
                "setTimeout(function() { console.log('a') }, 0);\n",
            ),
            "test.js",
        )
        .expect("should run");
        assert_eq!(capture.text(), "a\nb");
    }

    #[test]
    fn run_compile_error() {
        let mut rt = OtterRuntime::builder().build();
        let result = rt.run_script("{{{{", "bad.js");
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("CompileError"));
    }

    #[test]
    fn run_try_catch() {
        let (mut rt, capture) = rt_with_capture();
        rt.run_script(
            "try { throw 'oops'; } catch(e) { console.log('caught:', e); }",
            "test.js",
        )
        .expect("should run");
        assert_eq!(capture.text(), "caught: oops");
    }

    #[test]
    fn run_math_abs() {
        let (mut rt, capture) = rt_with_capture();
        rt.run_script("console.log(Math.abs(-42))", "test.js")
            .expect("should run");
        assert_eq!(capture.text(), "42");
    }

    #[test]
    fn function_prototype_call_exists() {
        let (mut rt, capture) = rt_with_capture();
        rt.run_script("console.log(typeof Function.prototype.call)", "test.js")
            .expect("should run");
        assert_eq!(capture.text(), "function");
    }

    #[test]
    fn function_prototype_bind_exists() {
        let (mut rt, capture) = rt_with_capture();
        rt.run_script(
            "console.log(typeof Function.prototype.call.bind)",
            "test.js",
        )
        .expect("should run");
        assert_eq!(capture.text(), "function");
    }

    #[test]
    fn bound_function_is_callable() {
        let (mut rt, capture) = rt_with_capture();
        rt.run_script(
            "var bound = Function.prototype.call.bind(Array.prototype.join); console.log(typeof bound)",
            "test.js",
        )
        .expect("should run");
        assert_eq!(capture.text(), "function");
    }

    #[test]
    fn bound_function_invocation() {
        let (mut rt, capture) = rt_with_capture();
        // First test: direct join works.
        rt.run_script("var arr = [1,2,3]; console.log(arr.join('-'))", "test.js")
            .expect("direct join should work");
        assert_eq!(capture.text(), "1-2-3");
    }

    #[test]
    fn bound_function_call_bind() {
        let (mut rt, capture) = rt_with_capture();
        rt.run_script(
            "var __join = Function.prototype.call.bind(Array.prototype.join); var arr = [1,2,3]; console.log(__join(arr, '-'))",
            "test.js",
        )
        .expect("bound call.bind(join) should work");
        assert_eq!(capture.text(), "1-2-3");
    }

    #[test]
    fn object_get_own_property_descriptor_basic() {
        let (mut rt, capture) = rt_with_capture();
        rt.run_script(
            "var d = Object.getOwnPropertyDescriptor(Math, 'abs'); console.log(typeof d, d.writable, d.enumerable, d.configurable)",
            "test.js",
        )
        .expect("should run");
        assert_eq!(capture.text(), "object true false true");
    }

    #[test]
    fn full_verify_property_test() {
        let base =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/test262/harness");
        let sta = std::fs::read_to_string(base.join("sta.js")).unwrap();
        let assert_js = std::fs::read_to_string(base.join("assert.js")).unwrap();
        let prop_helper = std::fs::read_to_string(base.join("propertyHelper.js")).unwrap();

        // Step 1: sta.js alone
        {
            let mut rt = OtterRuntime::builder().build();
            rt.run_script(&sta, "sta.js").expect("sta.js should load");
        }
        // Step 2: sta + assert
        {
            let mut rt = OtterRuntime::builder().build();
            let code = format!("{sta}\n{assert_js}");
            rt.run_script(&code, "test.js")
                .expect("sta+assert should load");
        }
        // Step 3: sta + assert + propertyHelper
        {
            let mut rt = OtterRuntime::builder().build();
            let code = format!("{sta}\n{assert_js}\n{prop_helper}");
            rt.run_script(&code, "test.js")
                .expect("sta+assert+propHelper should load");
        }
        // Step 3.5: sta + assert + propertyHelper + minimal call
        {
            let mut rt = OtterRuntime::builder().build();
            let code = format!(
                "{sta}\n{assert_js}\n{prop_helper}\nvar d = Object.getOwnPropertyDescriptor(Math, 'abs'); console.log(typeof d);"
            );
            match rt.run_script(&code, "test.js") {
                Ok(_) => {}
                Err(e) => panic!("step 3.5 (GOPD after harness): {e}"),
            }
        }
        // Step 4: full with verifyProperty
        {
            let (mut rt, capture) = rt_with_capture();
            let test_code = "verifyProperty(Math, 'abs', { writable: true, enumerable: false, configurable: true }); console.log('PASS');";
            let full = format!("{sta}\n{assert_js}\n{prop_helper}\n{test_code}");
            match rt.run_script(&full, "test.js") {
                Ok(_) => assert_eq!(capture.text(), "PASS"),
                Err(e) => panic!("verifyProperty failed: {e}"),
            }
        }
    }

    #[test]
    fn run_script_handles_direct_symbol_for_non_constructor_checks() {
        let mut rt = OtterRuntime::builder().build();
        rt.run_script(
            concat!(
                "try {\n",
                "  new Symbol.for();\n",
                "} catch (error) {\n",
                "  if (error.name !== 'TypeError') throw error;\n",
                "}\n",
            ),
            "symbol-for-direct.js",
        )
        .expect("direct new Symbol.for should throw TypeError under run_script");
    }

    #[test]
    fn run_script_handles_direct_symbol_key_for_non_constructor_checks() {
        let mut rt = OtterRuntime::builder().build();
        rt.run_script(
            concat!(
                "try {\n",
                "  new Symbol.keyFor(Symbol());\n",
                "} catch (error) {\n",
                "  if (error.name !== 'TypeError') throw error;\n",
                "}\n",
            ),
            "symbol-key-for-direct.js",
        )
        .expect("direct new Symbol.keyFor should throw TypeError under run_script");
    }

    #[test]
    fn run_script_handles_assert_throws_for_symbol_non_constructors() {
        let base =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/test262/harness");
        let sta = std::fs::read_to_string(base.join("sta.js")).unwrap();
        let assert_js = std::fs::read_to_string(base.join("assert.js")).unwrap();
        let body = concat!(
            "assert.throws(TypeError, () => {\n",
            "  new Symbol.for();\n",
            "});\n",
            "assert.throws(TypeError, () => {\n",
            "  new Symbol.keyFor(Symbol());\n",
            "});\n",
        );

        let mut rt = OtterRuntime::builder().build();
        let code = format!("{sta}\n{assert_js}\n{body}");
        rt.run_script(&code, "symbol-assert-throws.js")
            .expect("assert.throws should handle Symbol non-constructors");
    }

    #[test]
    fn run_script_handles_minimal_is_constructor_shape() {
        let mut rt = OtterRuntime::builder().build();
        rt.run_script(
            concat!(
                "function isConstructor(f) {\n",
                "  try {\n",
                "    Reflect.construct(function(){}, [], f);\n",
                "  } catch (error) {\n",
                "    return false;\n",
                "  }\n",
                "  return true;\n",
                "}\n",
                "if (isConstructor(Symbol.for) !== false) throw new Error('Symbol.for should not be constructible');\n",
                "if (isConstructor(Symbol.keyFor) !== false) throw new Error('Symbol.keyFor should not be constructible');\n",
            ),
            "symbol-minimal-is-constructor.js",
        )
        .expect("minimal isConstructor shape should pass");
    }

    #[test]
    fn run_script_exposes_test262_helpers_across_scripts() {
        let base =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/test262/harness");
        let sta = std::fs::read_to_string(base.join("sta.js")).unwrap();
        let assert_js = std::fs::read_to_string(base.join("assert.js")).unwrap();
        let is_constructor = std::fs::read_to_string(base.join("isConstructor.js")).unwrap();

        let mut rt = OtterRuntime::builder().build();
        rt.run_script(&sta, "sta.js").expect("sta.js should load");
        rt.run_script(&assert_js, "assert.js")
            .expect("assert.js should load");
        rt.run_script(&is_constructor, "isConstructor.js")
            .expect("isConstructor.js should load");
        rt.run_script(
            concat!(
                "if (typeof assert !== 'function') throw new Error('assert should survive');\n",
                "if (typeof isConstructor !== 'function') throw new Error('isConstructor should survive');\n",
            ),
            "helpers-visible.js",
        )
        .expect("test262 helpers should be visible in later scripts");
    }

    #[test]
    fn run_script_persists_top_level_function_declarations_across_scripts() {
        let mut rt = OtterRuntime::builder().build();
        rt.run_script("function persist() { return 7; }", "persist-a.js")
            .expect("function declaration should load");
        rt.run_script(
            "if (persist() !== 7) throw new Error('persist should survive across scripts');",
            "persist-b.js",
        )
        .expect("top-level function declaration should be callable in later scripts");
    }

    #[test]
    fn run_script_handles_is_constructor_after_separate_test262_includes() {
        let base =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/test262/harness");
        let sta = std::fs::read_to_string(base.join("sta.js")).unwrap();
        let assert_js = std::fs::read_to_string(base.join("assert.js")).unwrap();
        let is_constructor = std::fs::read_to_string(base.join("isConstructor.js")).unwrap();

        let mut rt = OtterRuntime::builder().build();
        rt.run_script(&sta, "sta.js").expect("sta.js should load");
        rt.run_script(&assert_js, "assert.js")
            .expect("assert.js should load");
        rt.run_script(&is_constructor, "isConstructor.js")
            .expect("isConstructor.js should load");
        rt.run_script(
            "assert.sameValue(isConstructor(Symbol.for), false, 'Symbol.for is not constructible');",
            "is-constructor-check.js",
        )
        .expect("isConstructor(Symbol.for) should work after separate includes");
    }

    #[test]
    fn run_script_evaluates_is_constructor_after_separate_include_without_assert() {
        let base =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/test262/harness");
        let sta = std::fs::read_to_string(base.join("sta.js")).unwrap();
        let is_constructor = std::fs::read_to_string(base.join("isConstructor.js")).unwrap();

        let mut rt = OtterRuntime::builder().build();
        rt.run_script(&sta, "sta.js").expect("sta.js should load");
        rt.run_script(&is_constructor, "isConstructor.js")
            .expect("isConstructor.js should load");
        rt.run_script(
            concat!(
                "var result = isConstructor(Symbol.for);\n",
                "if (result !== false) throw new Error('expected false');\n",
            ),
            "is-constructor-raw-check.js",
        )
        .expect("isConstructor(Symbol.for) should evaluate to false after separate include");
    }

    #[test]
    fn run_script_handles_assert_same_value_after_separate_include() {
        let base =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/test262/harness");
        let sta = std::fs::read_to_string(base.join("sta.js")).unwrap();
        let assert_js = std::fs::read_to_string(base.join("assert.js")).unwrap();

        let mut rt = OtterRuntime::builder().build();
        rt.run_script(&sta, "sta.js").expect("sta.js should load");
        rt.run_script(&assert_js, "assert.js")
            .expect("assert.js should load");
        rt.run_script(
            "assert.sameValue(1, 1, 'assert.sameValue should survive across scripts');",
            "assert-same-value.js",
        )
        .expect("assert.sameValue should work after separate include");
    }

    #[test]
    fn run_script_handles_symbol_for_length_descriptor_after_separate_include() {
        let base =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/test262/harness");
        let sta = std::fs::read_to_string(base.join("sta.js")).unwrap();
        let assert_js = std::fs::read_to_string(base.join("assert.js")).unwrap();

        let mut rt = OtterRuntime::builder().build();
        rt.run_script(&sta, "sta.js").expect("sta.js should load");
        rt.run_script(&assert_js, "assert.js")
            .expect("assert.js should load");
        rt.run_script(
            concat!(
                "var desc = Object.getOwnPropertyDescriptor(Symbol.for, 'length');\n",
                "if (typeof desc !== 'object') throw new Error('desc should be object');\n",
                "if (desc.value !== 1) throw new Error('length should be 1');\n",
                "if (desc.writable !== false) throw new Error('length writable should be false');\n",
                "if (desc.enumerable !== false) throw new Error('length enumerable should be false');\n",
                "if (desc.configurable !== true) throw new Error('length configurable should be true');\n",
            ),
            "symbol-for-length-desc.js",
        )
        .expect("direct Symbol.for length descriptor lookup should work");
    }

    #[test]
    fn run_script_handles_symbol_wrapper_redefined_nullish_to_primitive_with_assert_throws() {
        let base =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/test262/harness");
        let sta = std::fs::read_to_string(base.join("sta.js")).unwrap();
        let assert_js = std::fs::read_to_string(base.join("assert.js")).unwrap();
        let body = concat!(
            "Object.defineProperty(Symbol.prototype, Symbol.toPrimitive, { value: null });\n",
            "assert.sameValue(Object(Symbol()) == 'Symbol()', false, 'hint: default');\n",
            "assert.throws(TypeError, () => { +Object(Symbol()); }, 'hint: number');\n",
            "assert.sameValue(`${Object(Symbol())}`, 'Symbol()', 'hint: string');\n",
            "Object.defineProperty(Symbol.prototype, Symbol.toPrimitive, { value: undefined });\n",
            "assert(Object(Symbol.iterator) == Symbol.iterator, 'hint: default');\n",
            "assert.throws(TypeError, () => { Object(Symbol()) <= ''; }, 'hint: number');\n",
            "assert.sameValue({ 'Symbol()': 1 }[Object(Symbol())], 1, 'hint: string');\n",
        );

        let mut rt = OtterRuntime::builder().build();
        let code = format!("{sta}\n{assert_js}\n{body}");
        rt.run_script(&code, "symbol-wrapper-redefined-nullish.js")
            .expect("runtime should satisfy the exact test262 symbol wrapper nullish @@toPrimitive shape");
    }

    #[test]
    fn run_script_reports_zero_failures_for_symbol_wrapper_redefined_nullish_probe() {
        let (mut rt, capture) = rt_with_capture();
        rt.run_script(
            concat!(
                "var failures = 0;\n",
                "Object.defineProperty(Symbol.prototype, Symbol.toPrimitive, { value: null });\n",
                "if (Object(Symbol()) == 'Symbol()') failures |= 1;\n",
                "try { +Object(Symbol()); failures |= 2; } catch (thrown) {\n",
                "  if (typeof thrown !== 'object' || thrown === null) failures |= 4;\n",
                "  else if (thrown.constructor !== TypeError) failures |= 8;\n",
                "}\n",
                "if (`${Object(Symbol())}` !== 'Symbol()') failures |= 16;\n",
                "Object.defineProperty(Symbol.prototype, Symbol.toPrimitive, { value: undefined });\n",
                "if (!(Object(Symbol.iterator) == Symbol.iterator)) failures |= 32;\n",
                "try { Object(Symbol()) <= ''; failures |= 64; } catch (thrown) {\n",
                "  if (typeof thrown !== 'object' || thrown === null) failures |= 128;\n",
                "  else if (thrown.constructor !== TypeError) failures |= 256;\n",
                "}\n",
                "if ({ 'Symbol()': 1 }[Object(Symbol())] !== 1) failures |= 512;\n",
                "console.log(failures);\n",
            ),
            "symbol-wrapper-redefined-nullish-probe.js",
        )
        .expect("runtime probe should execute");
        assert_eq!(capture.text(), "0");
    }

    #[test]
    fn run_script_handles_date_constructor_after_symbol_wrapper_ordinary_to_primitive() {
        let (mut rt, capture) = rt_with_capture();
        rt.run_script(
            concat!(
                "if (!delete Symbol.prototype[Symbol.toPrimitive]) throw new Error('delete failed');\n",
                "let valueOfFunction = null;\n",
                "Object.defineProperty(Symbol.prototype, 'valueOf', {\n",
                "  get: () => valueOfFunction,\n",
                "});\n",
                "let toStringFunction = () => 'foo';\n",
                "Object.defineProperty(Symbol.prototype, 'toString', {\n",
                "  get: () => toStringFunction,\n",
                "});\n",
                "console.log(String(new Date(Object(Symbol())).getTime()));\n",
            ),
            "symbol-wrapper-date-probe.js",
        )
        .expect("Date constructor probe should execute");
        assert_eq!(capture.text(), "NaN");
    }

    #[test]
    fn run_script_exposes_string_prototype_concat_on_string_literals() {
        let (mut rt, capture) = rt_with_capture();
        rt.run_script(
            concat!(
                "console.log(typeof ''.concat);\n",
                "console.log(''.concat('a'));\n",
            ),
            "string-concat-smoke.js",
        )
        .expect("String.prototype.concat should work on string literals");
        assert_eq!(capture.text(), "function\na");
    }

    #[test]
    fn run_script_exposes_string_concat_surface_on_string_literals() {
        let (mut rt, capture) = rt_with_capture();
        rt.run_script(
            concat!(
                "console.log(typeof String.prototype.concat);\n",
                "console.log(typeof ''.concat);\n",
                "console.log(Object.getPrototypeOf('') === String.prototype);\n",
            ),
            "string-concat-surface.js",
        )
        .expect("String concat surface lookup should execute");
        // ''.concat is found via String.prototype — typeof returns "function".
        assert_eq!(capture.text(), "function\nfunction\ntrue");
    }

    #[test]
    fn run_script_exposes_string_concat_on_string_variables() {
        let (mut rt, capture) = rt_with_capture();
        rt.run_script(
            concat!(
                "var text = '';\n",
                "console.log(typeof text.concat);\n",
                "console.log(text.concat('a'));\n",
            ),
            "string-concat-variable-smoke.js",
        )
        .expect("String.prototype.concat should work on string variables");
        assert_eq!(capture.text(), "function\na");
    }

    #[test]
    fn runtime_state_finds_concat_on_allocated_string_values() {
        let (mut rt, _capture) = rt_with_capture();
        let property = rt.state_mut().intern_property_name("concat");
        let string = rt.state_mut().alloc_string("");
        let prototype = rt
            .state()
            .objects()
            .get_prototype(string)
            .expect("string prototype lookup should succeed")
            .expect("allocated strings should have a prototype");
        assert_eq!(prototype, rt.state().intrinsics().string_prototype());
        let prototype_lookup = rt
            .state_mut()
            .property_lookup(prototype, property)
            .expect("prototype property lookup should succeed")
            .expect("concat should exist directly on String.prototype");
        match prototype_lookup.value() {
            otter_vm::object::PropertyValue::Data { value, .. } => {
                assert!(value.as_object_handle().is_some());
            }
            other => panic!("expected data property for String.prototype.concat, got {other:?}"),
        }
        let lookup = rt
            .state_mut()
            .property_lookup(string, property)
            .expect("string property lookup should succeed")
            .expect("concat should exist on string prototype");
        match lookup.value() {
            otter_vm::object::PropertyValue::Data { value, .. } => {
                let handle = value
                    .as_object_handle()
                    .map(otter_vm::object::ObjectHandle)
                    .expect("concat should resolve to a callable object");
                let kind = rt
                    .state()
                    .objects()
                    .kind(handle)
                    .expect("callable kind lookup should succeed");
                assert_eq!(kind, otter_vm::object::HeapValueKind::HostFunction);
            }
            other => panic!("expected data property for concat lookup, got {other:?}"),
        }
    }

    #[test]
    fn run_script_reports_progress_through_removed_symbol_wrapper_ordinary_to_primitive() {
        let (mut rt, capture) = rt_with_capture();
        let result = rt.run_script(
            concat!(
                "function ProbeError() {}\n",
                "if (!delete Symbol.prototype[Symbol.toPrimitive]) throw new Error('delete failed');\n",
                "console.log('d');\n",
                "let valueOfGets = 0;\n",
                "let valueOfCalls = 0;\n",
                "let valueOfFunction = () => { ++valueOfCalls; return 123; };\n",
                "Object.defineProperty(Symbol.prototype, 'valueOf', { get: () => { ++valueOfGets; return valueOfFunction; } });\n",
                "console.log('v');\n",
                "if (!(Object(Symbol()) == 123)) throw new Error('stage1-a');\n",
                "console.log('1a');\n",
                "if (Object(Symbol()) - 0 !== 123) throw new Error('stage1-b');\n",
                "console.log('1b');\n",
                "if (''.concat(Object(Symbol())) !== 'Symbol()') throw new Error('stage1-c');\n",
                "console.log('1');\n",
                "let toStringGets = 0;\n",
                "let toStringCalls = 0;\n",
                "let toStringFunction = () => { ++toStringCalls; return 'foo'; };\n",
                "Object.defineProperty(Symbol.prototype, 'toString', { get: () => { ++toStringGets; return toStringFunction; } });\n",
                "if ('' + Object(Symbol()) !== '123') throw new Error('stage2-a');\n",
                "if (Object(Symbol()) * 1 !== 123) throw new Error('stage2-b');\n",
                "if ({ '123': 1, 'Symbol()': 2, 'foo': 3 }[Object(Symbol())] !== 3) throw new Error('stage2-c');\n",
                "console.log('2');\n",
                "valueOfFunction = null;\n",
                "if (String(new Date(Object(Symbol())).getTime()) !== 'NaN') throw new Error('stage3-a');\n",
                "if (String(+Object(Symbol())) !== 'NaN') throw new Error('stage3-b');\n",
                "if (`${Object(Symbol())}` !== 'foo') throw new Error('stage3-c');\n",
                "console.log('3');\n",
                "toStringFunction = function() { throw new ProbeError(); };\n",
                "try { Object(Symbol()) != 123; throw new Error('stage4-a'); } catch (error) { if (error.constructor !== ProbeError) throw error; }\n",
                "console.log('4a');\n",
                "try { Object(Symbol()) / 0; throw new Error('stage4-b'); } catch (error) { if (error.constructor !== ProbeError) throw error; }\n",
                "console.log('4b');\n",
                "try { ''.concat(Object(Symbol())); throw new Error('stage4-c'); } catch (error) { if (error.constructor !== ProbeError) throw error; }\n",
                "console.log('4');\n",
                "toStringFunction = undefined;\n",
                "try { 1 + Object(Symbol()); throw new Error('stage5-a'); } catch (error) { if (error.name !== 'TypeError') throw error; }\n",
                "console.log('5a');\n",
                "try { Number(Object(Symbol())); throw new Error('stage5-b'); } catch (error) { if (error.name !== 'TypeError') throw error; }\n",
                "console.log('5b');\n",
                "try { String(Object(Symbol())); throw new Error('stage5-c'); } catch (error) { if (error.name !== 'TypeError') throw error; }\n",
                "console.log('5');\n",
            ),
            "symbol-wrapper-removed-ordinary-progress.js",
        );
        assert!(
            result.is_ok(),
            "result = {result:?}, progress = {}",
            capture.text()
        );
    }

    #[test]
    fn run_script_reports_strict_symbol_primitive_assignment_errors() {
        let (mut rt, capture) = rt_with_capture();
        rt.run_script(
            concat!(
                "\"use strict\";\n",
                "var sym = Symbol('66');\n",
                "try {\n",
                "  sym.toString = 0;\n",
                "  console.log('no-throw-1');\n",
                "} catch (error) {\n",
                "  console.log(error.name);\n",
                "  console.log(error.constructor === TypeError);\n",
                "}\n",
                "try {\n",
                "  sym.valueOf = 0;\n",
                "  console.log('no-throw-2');\n",
                "} catch (error) {\n",
                "  console.log(error.name);\n",
                "  console.log(error.constructor === TypeError);\n",
                "}\n",
            ),
            "strict-symbol-primitive-assignment-diagnostics.js",
        )
        .expect("strict symbol assignment diagnostics should execute");
        assert_eq!(capture.text(), "TypeError\ntrue\nTypeError\ntrue");
    }

    #[test]
    fn run_script_handles_property_helper_on_math_after_separate_include() {
        let base =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/test262/harness");
        let sta = std::fs::read_to_string(base.join("sta.js")).unwrap();
        let assert_js = std::fs::read_to_string(base.join("assert.js")).unwrap();
        let property_helper = std::fs::read_to_string(base.join("propertyHelper.js")).unwrap();

        let mut rt = OtterRuntime::builder().build();
        rt.run_script(&sta, "sta.js").expect("sta.js should load");
        rt.run_script(&assert_js, "assert.js")
            .expect("assert.js should load");
        rt.run_script(&property_helper, "propertyHelper.js")
            .expect("propertyHelper.js should load");
        rt.run_script(
            "verifyProperty(Math, 'abs', { writable: true, enumerable: false, configurable: true });",
            "verify-math-abs.js",
        )
        .expect("propertyHelper should work on Math.abs after separate include");
    }

    #[test]
    fn run_script_handles_property_helper_on_symbol_for_after_separate_include() {
        let base =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/test262/harness");
        let sta = std::fs::read_to_string(base.join("sta.js")).unwrap();
        let assert_js = std::fs::read_to_string(base.join("assert.js")).unwrap();
        let property_helper = std::fs::read_to_string(base.join("propertyHelper.js")).unwrap();

        let mut rt = OtterRuntime::builder().build();
        rt.run_script(&sta, "sta.js").expect("sta.js should load");
        rt.run_script(&assert_js, "assert.js")
            .expect("assert.js should load");
        rt.run_script(&property_helper, "propertyHelper.js")
            .expect("propertyHelper.js should load");
        rt.run_script(
            "verifyProperty(Symbol.for, 'length', { value: 1, writable: false, enumerable: false, configurable: true });",
            "verify-symbol-for-length.js",
        )
        .expect("propertyHelper should work on Symbol.for after separate include");
    }

    #[test]
    fn run_script_handles_split_cross_script_calls_in_sequence() {
        let base =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/test262/harness");
        let sta = std::fs::read_to_string(base.join("sta.js")).unwrap();
        let assert_js = std::fs::read_to_string(base.join("assert.js")).unwrap();
        let is_constructor = std::fs::read_to_string(base.join("isConstructor.js")).unwrap();

        let mut rt = OtterRuntime::builder().build();
        rt.run_script(&sta, "sta.js").expect("sta.js should load");
        rt.run_script(&assert_js, "assert.js")
            .expect("assert.js should load");
        rt.run_script(&is_constructor, "isConstructor.js")
            .expect("isConstructor.js should load");
        rt.run_script(
            concat!(
                "var result = isConstructor(Symbol.for);\n",
                "assert.sameValue(result, false, 'split cross-script calls should work');\n",
            ),
            "split-cross-script-calls.js",
        )
        .expect("split cross-script calls should work");
    }

    #[test]
    fn arguments_object_basic() {
        let (mut rt, capture) = rt_with_capture();
        rt.run_script(
            "function f(a, b) { return arguments.length; } console.log(f(1, 2, 3))",
            "test.js",
        )
        .expect("arguments.length should work");
        assert_eq!(capture.text(), "3");
    }

    #[test]
    fn arguments_indexed_access() {
        let (mut rt, capture) = rt_with_capture();
        rt.run_script(
            "function f(a) { return arguments[1]; } console.log(f(10, 20, 30))",
            "test.js",
        )
        .expect("arguments[1] should work");
        assert_eq!(capture.text(), "20");
    }

    #[test]
    fn minimal_verify_property() {
        let (mut rt, capture) = rt_with_capture();
        rt.run_script(
            r#"
            var desc = Object.getOwnPropertyDescriptor(Math, "abs");
            console.log(typeof desc);
            console.log(desc.writable);
            console.log(desc.enumerable);
            console.log(desc.configurable);
        "#,
            "test.js",
        )
        .expect("minimal verifyProperty");
        assert_eq!(capture.text(), "object\ntrue\nfalse\ntrue");
    }

    #[test]
    fn for_in_on_descriptor() {
        let (mut rt, capture) = rt_with_capture();
        rt.run_script(
            r#"
            var desc = { value: 1, writable: true, enumerable: false, configurable: true };
            var names = Object.getOwnPropertyNames(desc);
            console.log(names.join(","));
        "#,
            "test.js",
        )
        .expect("for-in on descriptor");
        assert_eq!(capture.text(), "value,writable,enumerable,configurable");
    }

    #[test]
    fn property_helper_harness_loads() {
        let mut rt = OtterRuntime::builder().build();
        // Minimal propertyHelper.js preamble.
        let result = rt.run_script(
            concat!(
                "var __isArray = Array.isArray;\n",
                "var __defineProperty = Object.defineProperty;\n",
                "var __getOwnPropertyDescriptor = Object.getOwnPropertyDescriptor;\n",
                "var __getOwnPropertyNames = Object.getOwnPropertyNames;\n",
                "var __join = Function.prototype.call.bind(Array.prototype.join);\n",
                "var __push = Function.prototype.call.bind(Array.prototype.push);\n",
                "var __hasOwnProperty = Function.prototype.call.bind(Object.prototype.hasOwnProperty);\n",
                "var __propertyIsEnumerable = Function.prototype.call.bind(Object.prototype.propertyIsEnumerable);\n",
                "console.log('ok');\n",
            ),
            "test.js",
        );
        match result {
            Ok(_) => {}
            Err(e) => panic!("propertyHelper preamble failed: {e}"),
        }
    }

    #[test]
    fn property_helper_bound_globals_survive_separate_script_loads() {
        let base =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/test262/harness");
        let sta = std::fs::read_to_string(base.join("sta.js")).unwrap();
        let assert_js = std::fs::read_to_string(base.join("assert.js")).unwrap();
        let property_helper = std::fs::read_to_string(base.join("propertyHelper.js")).unwrap();

        let (mut rt, capture) = rt_with_capture();
        rt.run_script(&sta, "sta.js").expect("sta.js should load");
        rt.run_script(&assert_js, "assert.js")
            .expect("assert.js should load");
        rt.run_script(&property_helper, "propertyHelper.js")
            .expect("propertyHelper.js should load");
        rt.run_script(
            concat!(
                "console.log(typeof __getOwnPropertyNames);\n",
                "console.log(typeof __join);\n",
                "console.log(typeof __push);\n",
                "console.log(typeof __hasOwnProperty);\n",
                "console.log(typeof __propertyIsEnumerable);\n",
            ),
            "property-helper-types.js",
        )
        .expect("propertyHelper globals should survive separate script loads");
        assert_eq!(
            capture.text(),
            "function\nfunction\nfunction\nfunction\nfunction"
        );
    }

    #[test]
    fn property_helper_bound_has_own_property_works_across_scripts() {
        let base =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/test262/harness");
        let sta = std::fs::read_to_string(base.join("sta.js")).unwrap();
        let assert_js = std::fs::read_to_string(base.join("assert.js")).unwrap();
        let property_helper = std::fs::read_to_string(base.join("propertyHelper.js")).unwrap();

        let (mut rt, capture) = rt_with_capture();
        rt.run_script(&sta, "sta.js").expect("sta.js should load");
        rt.run_script(&assert_js, "assert.js")
            .expect("assert.js should load");
        rt.run_script(&property_helper, "propertyHelper.js")
            .expect("propertyHelper.js should load");
        rt.run_script(
            "console.log(__hasOwnProperty({ a: 1 }, 'a'));",
            "property-helper-has-own.js",
        )
        .expect("__hasOwnProperty should work across scripts");
        assert_eq!(capture.text(), "true");
    }

    #[test]
    fn property_helper_bound_push_works_across_scripts() {
        let base =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/test262/harness");
        let sta = std::fs::read_to_string(base.join("sta.js")).unwrap();
        let assert_js = std::fs::read_to_string(base.join("assert.js")).unwrap();
        let property_helper = std::fs::read_to_string(base.join("propertyHelper.js")).unwrap();

        let (mut rt, capture) = rt_with_capture();
        rt.run_script(&sta, "sta.js").expect("sta.js should load");
        rt.run_script(&assert_js, "assert.js")
            .expect("assert.js should load");
        rt.run_script(&property_helper, "propertyHelper.js")
            .expect("propertyHelper.js should load");
        rt.run_script(
            concat!(
                "var failures = [];\n",
                "console.log(__push(failures, 'x'));\n",
                "console.log(failures.length);\n",
            ),
            "property-helper-push.js",
        )
        .expect("__push should work across scripts");
        assert_eq!(capture.text(), "1\n1");
    }

    #[test]
    fn property_helper_get_own_property_names_works_across_scripts() {
        let base =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/test262/harness");
        let sta = std::fs::read_to_string(base.join("sta.js")).unwrap();
        let assert_js = std::fs::read_to_string(base.join("assert.js")).unwrap();
        let property_helper = std::fs::read_to_string(base.join("propertyHelper.js")).unwrap();

        let (mut rt, capture) = rt_with_capture();
        rt.run_script(&sta, "sta.js").expect("sta.js should load");
        rt.run_script(&assert_js, "assert.js")
            .expect("assert.js should load");
        rt.run_script(&property_helper, "propertyHelper.js")
            .expect("propertyHelper.js should load");
        rt.run_script(
            "console.log(__getOwnPropertyNames({ a: 1, b: 2 }).join(','));",
            "property-helper-own-property-names.js",
        )
        .expect("__getOwnPropertyNames should work across scripts");
        assert_eq!(capture.text(), "a,b");
    }

    #[test]
    fn property_helper_is_enumerable_works_across_scripts() {
        let base =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/test262/harness");
        let sta = std::fs::read_to_string(base.join("sta.js")).unwrap();
        let assert_js = std::fs::read_to_string(base.join("assert.js")).unwrap();
        let property_helper = std::fs::read_to_string(base.join("propertyHelper.js")).unwrap();

        let mut rt = OtterRuntime::builder().build();
        rt.run_script(&sta, "sta.js").expect("sta.js should load");
        rt.run_script(&assert_js, "assert.js")
            .expect("assert.js should load");
        rt.run_script(&property_helper, "propertyHelper.js")
            .expect("propertyHelper.js should load");
        rt.run_script(
            "assert.sameValue(isEnumerable(Math, 'abs'), false);",
            "property-helper-is-enumerable.js",
        )
        .expect("isEnumerable should work across scripts");
    }

    #[test]
    fn property_helper_is_writable_works_across_scripts() {
        let base =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/test262/harness");
        let sta = std::fs::read_to_string(base.join("sta.js")).unwrap();
        let assert_js = std::fs::read_to_string(base.join("assert.js")).unwrap();
        let property_helper = std::fs::read_to_string(base.join("propertyHelper.js")).unwrap();

        let mut rt = OtterRuntime::builder().build();
        rt.run_script(&sta, "sta.js").expect("sta.js should load");
        rt.run_script(&assert_js, "assert.js")
            .expect("assert.js should load");
        rt.run_script(&property_helper, "propertyHelper.js")
            .expect("propertyHelper.js should load");
        rt.run_script(
            "assert.sameValue(isWritable(Math, 'abs'), true);",
            "property-helper-is-writable.js",
        )
        .expect("isWritable should work across scripts");
    }

    #[test]
    fn property_helper_is_configurable_works_across_scripts() {
        let base =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/test262/harness");
        let sta = std::fs::read_to_string(base.join("sta.js")).unwrap();
        let assert_js = std::fs::read_to_string(base.join("assert.js")).unwrap();
        let property_helper = std::fs::read_to_string(base.join("propertyHelper.js")).unwrap();

        let mut rt = OtterRuntime::builder().build();
        rt.run_script(&sta, "sta.js").expect("sta.js should load");
        rt.run_script(&assert_js, "assert.js")
            .expect("assert.js should load");
        rt.run_script(&property_helper, "propertyHelper.js")
            .expect("propertyHelper.js should load");
        rt.run_script(
            "assert.sameValue(isConfigurable(Math, 'abs'), true);",
            "property-helper-is-configurable.js",
        )
        .expect("isConfigurable should work across scripts");
    }

    #[test]
    fn cross_script_functions_receive_three_arguments() {
        let mut rt = OtterRuntime::builder().build();
        rt.run_script(
            concat!(
                "function probe(a, b, c) {\n",
                "  if (a !== 1) throw new Error('a');\n",
                "  if (b !== 2) throw new Error('b');\n",
                "  if (typeof c !== 'object') throw new Error('c:' + typeof c);\n",
                "}\n",
            ),
            "cross-script-probe-setup.js",
        )
        .expect("probe should load");
        rt.run_script("probe(1, 2, { ok: true });", "cross-script-probe-call.js")
            .expect("cross-script function should receive three arguments");
    }

    #[test]
    fn cross_script_functions_preserve_arguments_length() {
        let mut rt = OtterRuntime::builder().build();
        rt.run_script(
            concat!(
                "function probe() {\n",
                "  if (arguments.length !== 3) throw new Error('argc:' + arguments.length);\n",
                "}\n",
            ),
            "cross-script-arguments-setup.js",
        )
        .expect("probe should load");
        rt.run_script("probe(1, 2, 3);", "cross-script-arguments-call.js")
            .expect("cross-script function should preserve arguments.length");
    }

    #[test]
    fn nested_cross_script_function_calls_work_across_multiple_prior_scripts() {
        let mut rt = OtterRuntime::builder().build();
        rt.run_script(
            "function helper(value) { if (value !== 7) throw new Error('helper'); }",
            "nested-cross-script-helper.js",
        )
        .expect("helper should load");
        rt.run_script(
            "function probe() { helper(7); }",
            "nested-cross-script-probe.js",
        )
        .expect("probe should load");
        rt.run_script("probe();", "nested-cross-script-call.js")
            .expect("nested cross-script function calls should work");
    }

    // -----------------------------------------------------------------------
    // Promise tests — ES2024 §27.2
    // -----------------------------------------------------------------------

    #[test]
    fn promise_constructor_exists() {
        let (mut rt, capture) = rt_with_capture();
        rt.run_script("console.log(typeof Promise)", "test.js")
            .expect("should run");
        assert_eq!(capture.text(), "function");
    }

    #[test]
    fn promise_resolve_basic() {
        let (mut rt, capture) = rt_with_capture();
        rt.run_script(
            "Promise.resolve(42).then(function(v) { console.log(v); });",
            "test.js",
        )
        .expect("should run");
        assert_eq!(capture.text(), "42");
    }

    #[test]
    fn promise_reject_catch() {
        let (mut rt, capture) = rt_with_capture();
        rt.run_script(
            "Promise.reject('err').catch(function(e) { console.log(e); });",
            "test.js",
        )
        .expect("should run");
        assert_eq!(capture.text(), "err");
    }

    #[test]
    fn promise_constructor_with_executor() {
        let (mut rt, capture) = rt_with_capture();
        rt.run_script(
            concat!(
                "var p = new Promise(function(resolve, reject) {\n",
                "  resolve(99);\n",
                "});\n",
                "p.then(function(v) { console.log(v); });\n",
            ),
            "test.js",
        )
        .expect("should run");
        assert_eq!(capture.text(), "99");
    }

    #[test]
    fn promise_constructor_executor_reject() {
        let (mut rt, capture) = rt_with_capture();
        rt.run_script(
            concat!(
                "var p = new Promise(function(resolve, reject) {\n",
                "  reject('bad');\n",
                "});\n",
                "p.catch(function(e) { console.log(e); });\n",
            ),
            "test.js",
        )
        .expect("should run");
        assert_eq!(capture.text(), "bad");
    }

    #[test]
    fn promise_constructor_executor_throws() {
        let (mut rt, capture) = rt_with_capture();
        rt.run_script(
            concat!(
                "var p = new Promise(function(resolve, reject) {\n",
                "  throw 'oops';\n",
                "});\n",
                "p.catch(function(e) { console.log(e); });\n",
            ),
            "test.js",
        )
        .expect("should run");
        assert_eq!(capture.text(), "oops");
    }

    #[test]
    fn promise_then_chaining() {
        let (mut rt, capture) = rt_with_capture();
        rt.run_script(
            concat!(
                "Promise.resolve(1)\n",
                "  .then(function(v) { return v + 1; })\n",
                "  .then(function(v) { return v * 3; })\n",
                "  .then(function(v) { console.log(v); });\n",
            ),
            "test.js",
        )
        .expect("should run");
        assert_eq!(capture.text(), "6");
    }

    #[test]
    fn promise_then_returns_promise() {
        let (mut rt, capture) = rt_with_capture();
        rt.run_script(
            concat!(
                "var p = Promise.resolve(10).then(function(v) { return v; });\n",
                "console.log(typeof p.then);\n",
            ),
            "test.js",
        )
        .expect("should run");
        assert_eq!(capture.text(), "function");
    }

    #[test]
    fn promise_resolve_with_promise() {
        let (mut rt, capture) = rt_with_capture();
        rt.run_script(
            concat!(
                "var inner = Promise.resolve(7);\n",
                "var outer = Promise.resolve(inner);\n",
                "console.log(inner === outer);\n",
            ),
            "test.js",
        )
        .expect("should run");
        // Promise.resolve returns the same promise if argument is already a promise.
        assert_eq!(capture.text(), "true");
    }

    #[test]
    fn promise_microtask_ordering() {
        let (mut rt, capture) = rt_with_capture();
        rt.run_script(
            concat!(
                "console.log('sync');\n",
                "Promise.resolve().then(function() { console.log('micro'); });\n",
                "console.log('sync2');\n",
            ),
            "test.js",
        )
        .expect("should run");
        // Microtasks run after synchronous code completes.
        assert_eq!(capture.text(), "sync\nsync2\nmicro");
    }

    #[test]
    fn promise_resolve_value_is_correct() {
        let (mut rt, capture) = rt_with_capture();
        rt.run_script(
            concat!(
                "var arr = [Promise.resolve('x')];\n",
                "arr[0].then(function(v) { console.log(typeof v, v); });\n",
            ),
            "test.js",
        )
        .expect("should run");
        assert_eq!(capture.text(), "string x");
    }

    #[test]
    fn promise_race_first_wins() {
        // Manual race: first settled promise wins.
        let (mut rt, capture) = rt_with_capture();
        rt.run_script(
            concat!(
                "var p = Promise.resolve('first');\n",
                "var result = new Promise(function(resolve, reject) {\n",
                "  p.then(resolve, reject);\n",
                "});\n",
                "result.then(function(v) { console.log(v); });\n",
            ),
            "test.js",
        )
        .expect("should run");
        assert_eq!(capture.text(), "first");
    }

    #[test]
    fn promise_all_basic() {
        let (mut rt, capture) = rt_with_capture();
        rt.run_script(
            concat!(
                "var p1 = Promise.resolve(1);\n",
                "var p2 = Promise.resolve(2);\n",
                "var p3 = Promise.resolve(3);\n",
                "Promise.all([p1, p2, p3]).then(function(arr) {\n",
                "  console.log(arr.length, arr[0], arr[1], arr[2]);\n",
                "});\n",
            ),
            "test.js",
        )
        .expect("should run");
        assert_eq!(capture.text(), "3 1 2 3");
    }

    #[test]
    fn promise_deferred_reject_triggers_catch() {
        let (mut rt, capture) = rt_with_capture();
        rt.run_script(
            concat!(
                "var reject;\n",
                "var p = new Promise(function(res, rej) { reject = rej; });\n",
                "p.catch(function(e) { console.log('caught:', e); });\n",
                "reject('deferred');\n",
            ),
            "test.js",
        )
        .expect("should run");
        assert_eq!(capture.text(), "caught: deferred");
    }

    #[test]
    fn promise_reject_via_microtask_chain() {
        // Tests that rejection cascading works during microtask drain.
        let (mut rt, capture) = rt_with_capture();
        rt.run_script(
            concat!(
                "var p1 = Promise.reject('oops');\n",
                "var p2 = new Promise(function(resolve, reject) {\n",
                "  p1.then(resolve, reject);\n",
                "});\n",
                "p2.catch(function(e) { console.log(e); });\n",
            ),
            "test.js",
        )
        .expect("should run");
        assert_eq!(capture.text(), "oops");
    }

    #[test]
    fn promise_all_rejects_on_first_rejection() {
        let (mut rt, capture) = rt_with_capture();
        rt.run_script(
            concat!(
                "var p1 = Promise.resolve(1);\n",
                "var p2 = Promise.reject('fail');\n",
                "var p3 = Promise.resolve(3);\n",
                "Promise.all([p1, p2, p3]).catch(function(e) { console.log(e); });\n",
            ),
            "test.js",
        )
        .expect("should run");
        assert_eq!(capture.text(), "fail");
    }

    #[test]
    fn promise_all_empty_resolves_with_empty_array() {
        let (mut rt, capture) = rt_with_capture();
        rt.run_script(
            concat!(
                "Promise.all([]).then(function(arr) {\n",
                "  console.log(Array.isArray(arr), arr.length);\n",
                "});\n",
            ),
            "test.js",
        )
        .expect("should run");
        assert_eq!(capture.text(), "true 0");
    }

    #[test]
    fn promise_all_settled_basic() {
        let (mut rt, capture) = rt_with_capture();
        rt.run_script(
            concat!(
                "var p1 = Promise.resolve('ok');\n",
                "var p2 = Promise.reject('err');\n",
                "Promise.allSettled([p1, p2]).then(function(results) {\n",
                "  console.log(results.length);\n",
                "  console.log(results[0].status, results[0].value);\n",
                "  console.log(results[1].status, results[1].reason);\n",
                "});\n",
            ),
            "test.js",
        )
        .expect("should run");
        assert_eq!(capture.text(), "2\nfulfilled ok\nrejected err");
    }

    #[test]
    fn promise_any_first_fulfill_wins() {
        let (mut rt, capture) = rt_with_capture();
        rt.run_script(
            concat!(
                "var p1 = Promise.reject('a');\n",
                "var p2 = Promise.resolve('b');\n",
                "var p3 = Promise.resolve('c');\n",
                "Promise.any([p1, p2, p3]).then(function(v) { console.log(v); });\n",
            ),
            "test.js",
        )
        .expect("should run");
        assert_eq!(capture.text(), "b");
    }

    #[test]
    fn promise_any_all_reject_gives_aggregate_error() {
        let (mut rt, capture) = rt_with_capture();
        rt.run_script(
            concat!(
                "var p1 = Promise.reject('x');\n",
                "var p2 = Promise.reject('y');\n",
                "Promise.any([p1, p2]).catch(function(e) { console.log(e.message); });\n",
            ),
            "test.js",
        )
        .expect("should run");
        assert_eq!(capture.text(), "All promises were rejected");
    }

    #[test]
    fn promise_finally_preserves_value() {
        let (mut rt, capture) = rt_with_capture();
        rt.run_script(
            concat!(
                "Promise.resolve(42)\n",
                "  .finally(function() { console.log('finally'); })\n",
                "  .then(function(v) { console.log(v); });\n",
            ),
            "test.js",
        )
        .expect("should run");
        assert_eq!(capture.text(), "finally\n42");
    }

    #[test]
    fn promise_finally_preserves_rejection() {
        let (mut rt, capture) = rt_with_capture();
        rt.run_script(
            concat!(
                "Promise.reject('bad')\n",
                "  .finally(function() { console.log('finally'); })\n",
                "  .catch(function(e) { console.log(e); });\n",
            ),
            "test.js",
        )
        .expect("should run");
        assert_eq!(capture.text(), "finally\nbad");
    }

    #[test]
    fn promise_resolve_thenable_chain() {
        let (mut rt, capture) = rt_with_capture();
        rt.run_script(
            concat!(
                "var p1 = new Promise(function(resolve) { resolve(5); });\n",
                "var p2 = new Promise(function(resolve) { resolve(p1); });\n",
                "p2.then(function(v) { console.log(v); });\n",
            ),
            "test.js",
        )
        .expect("should run");
        // p2 resolves with p1, so p2 should unwrap to 5.
        assert_eq!(capture.text(), "5");
    }

    #[test]
    fn promise_self_resolve_throws_type_error() {
        let (mut rt, capture) = rt_with_capture();
        rt.run_script(
            concat!(
                "var resolve;\n",
                "var p = new Promise(function(r) { resolve = r; });\n",
                "resolve(p);\n",
                "p.catch(function(e) { console.log(e.message); });\n",
            ),
            "test.js",
        )
        .expect("should run");
        assert_eq!(capture.text(), "A promise cannot be resolved with itself");
    }

    // -----------------------------------------------------------------------
    // Top-level await tests — ES2022 §16.2
    // -----------------------------------------------------------------------

    #[test]
    fn top_level_await_resolved_value() {
        let (mut rt, capture) = rt_with_capture();
        rt.run_module_source(
            "var x = await Promise.resolve(42); console.log(x);",
            "tla.mjs",
        )
        .expect("should run");
        assert_eq!(capture.text(), "42");
    }

    #[test]
    fn top_level_await_non_promise() {
        let (mut rt, capture) = rt_with_capture();
        rt.run_module_source("var x = await 99; console.log(x);", "tla.mjs")
            .expect("should run");
        assert_eq!(capture.text(), "99");
    }

    #[test]
    fn top_level_await_chained_promises() {
        let (mut rt, capture) = rt_with_capture();
        rt.run_module_source(
            concat!(
                "var a = await Promise.resolve(10);\n",
                "var b = await Promise.resolve(20);\n",
                "console.log(a + b);\n",
            ),
            "tla.mjs",
        )
        .expect("should run");
        assert_eq!(capture.text(), "30");
    }

    #[test]
    fn top_level_await_async_function() {
        let (mut rt, capture) = rt_with_capture();
        rt.run_module_source(
            concat!(
                "async function fetchData() { return 'data'; }\n",
                "var result = await fetchData();\n",
                "console.log(result);\n",
            ),
            "tla.mjs",
        )
        .expect("should run");
        assert_eq!(capture.text(), "data");
    }

    // -----------------------------------------------------------------------
    // Heap limit (`max_heap_bytes`) end-to-end tests — analogue of Node.js's
    // `--max-old-space-size`. The runtime should surface `OutOfMemory` as a
    // catchable, non-fatal `RangeError`-style error.
    // -----------------------------------------------------------------------

    #[test]
    fn max_heap_bytes_unlimited_by_default() {
        let mut rt = OtterRuntime::builder().build();
        // Plenty of small allocations under the uncapped default: should
        // complete without triggering any OOM plumbing.
        rt.run_script(
            "var arr = []; for (var i = 0; i < 10; i++) arr.push(i);",
            "no-cap.js",
        )
        .expect("uncapped runtime should accept normal workloads");
    }

    #[test]
    fn max_heap_bytes_zero_disables_cap() {
        // `.max_heap_bytes(0)` is the explicit opt-out from the Node-style
        // default limit. Must behave identically to the uncapped default.
        let mut rt = OtterRuntime::builder().max_heap_bytes(0).build();
        rt.run_script("var x = 1 + 2;", "zero.js")
            .expect("zero cap should be unlimited");
    }

    #[test]
    fn pathological_array_length_is_caught_as_range_error() {
        // `new Array(0xFFFFFFFF + 1)` with a spec-valid constructor length
        // uses the ES §22.1.1 path; for a direct `length` set on an array
        // we rely on the VM `set_array_length` MAX_ARRAY_LENGTH guard.
        let mut rt = OtterRuntime::builder().build();
        let code = concat!(
            "let threw = false;\n",
            "try { let a = []; a.length = 4294967296; }\n",
            "catch (e) { if (e instanceof RangeError) threw = true; }\n",
            "if (!threw) throw new Error('expected RangeError');\n",
        );
        rt.run_script(code, "cap.js")
            .expect("RangeError should be caught by JS, runtime must stay alive");
    }

    #[test]
    fn concat_above_uint32_cap_throws_range_error() {
        // §22.1.3.1 — `{[Symbol.isConcatSpreadable]: true, length: 2^32}`
        // must not trigger a 32 GB `Vec::resize`. With the Phase 3 defenses
        // concat pre-computes the total and throws RangeError instead.
        let mut rt = OtterRuntime::builder().build();
        let code = concat!(
            "let threw = false;\n",
            "try {\n",
            "  let huge = {length: 4294967296};\n",
            "  huge[Symbol.isConcatSpreadable] = true;\n",
            "  [].concat(huge);\n",
            "} catch (e) { if (e instanceof RangeError) threw = true; }\n",
            "if (!threw) throw new Error('expected RangeError from concat');\n",
        );
        rt.run_script(code, "concat-cap.js")
            .expect("spec-cap violation should surface as RangeError");
    }

    #[test]
    fn set_length_beyond_cap_throws_range_error() {
        // Pathological `length = 2^32` on an existing array must trip the
        // `set_array_length` MAX_ARRAY_LENGTH guard.
        let mut rt = OtterRuntime::builder().build();
        let code = concat!(
            "let threw = false;\n",
            "try { [].length = 4294967296; }\n",
            "catch (e) { if (e instanceof RangeError) threw = true; }\n",
            "if (!threw) throw new Error('expected RangeError');\n",
        );
        rt.run_script(code, "setlen-cap.js")
            .expect("huge array length set should surface as RangeError");
    }

    #[test]
    fn timeout_interrupts_sync_infinite_loop() {
        let mut rt = OtterRuntime::builder()
            .timeout(Duration::from_millis(20))
            .build();

        let started = std::time::Instant::now();
        let error = rt
            .run_script("while (true) {}", "sync-infinite-loop.js")
            .expect_err("sync infinite loop should time out");

        assert_runtime_interrupted(error);
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "timeout should interrupt promptly"
        );
    }

    #[test]
    fn timeout_interrupts_infinite_microtask_chain() {
        let mut rt = OtterRuntime::builder()
            .timeout(Duration::from_millis(20))
            .build();

        let error = rt
            .run_script(
                concat!(
                    "function spin() { Promise.resolve().then(spin); }\n",
                    "Promise.resolve().then(spin);\n",
                ),
                "microtask-spin.js",
            )
            .expect_err("microtask spin should time out");

        assert_runtime_interrupted(error);
    }

    #[test]
    fn timeout_interrupts_js_loop_inside_promise_handler() {
        let mut rt = OtterRuntime::builder()
            .timeout(Duration::from_millis(20))
            .build();

        let error = rt
            .run_script(
                "Promise.resolve().then(function() { while (true) {} });",
                "promise-handler-infinite-loop.js",
            )
            .expect_err("JS loop in promise handler should time out");

        assert_runtime_interrupted(error);
    }

    #[test]
    fn timeout_interrupts_await_local_microtask_spin() {
        let mut rt = OtterRuntime::builder()
            .timeout(Duration::from_millis(20))
            .build();

        let error = rt
            .run_script(
                concat!(
                    "async function f() {\n",
                    "  Promise.resolve().then(function spin() { Promise.resolve().then(spin); });\n",
                    "  await Promise.resolve(1);\n",
                    "}\n",
                    "f();\n",
                ),
                "await-microtask-spin.js",
            )
            .expect_err("await-local microtask spin should time out");

        assert_runtime_interrupted(error);
    }

    #[test]
    fn timeout_interrupts_pending_host_callback_wait() {
        let mut rt = OtterRuntime::builder()
            .timeout(Duration::from_millis(20))
            .build();
        let _reservation = rt.state().host_callback_sender().reserve();

        let started = std::time::Instant::now();
        let error = rt
            .run_script("1 + 1;", "pending-host-callback.js")
            .expect_err("pending host callback wait should time out");

        assert_runtime_interrupted(error);
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "host callback wait should wake promptly on timeout"
        );
    }

    #[test]
    fn timeout_interrupts_sleeping_timer_wait() {
        let mut rt = OtterRuntime::builder()
            .timeout(Duration::from_millis(20))
            .build();

        let started = std::time::Instant::now();
        let error = rt
            .run_script(
                "setTimeout(function() {}, 60_000);",
                "sleeping-timer-wait.js",
            )
            .expect_err("sleeping timer wait should time out");

        assert_runtime_interrupted(error);
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "timer sleep should wake promptly on timeout"
        );
    }

    #[test]
    fn timeout_interrupts_js_loop_inside_timer_callback() {
        let mut rt = OtterRuntime::builder()
            .timeout(Duration::from_millis(20))
            .build();

        let error = rt
            .run_script(
                "setTimeout(function() { while (true) {} }, 0);",
                "timer-callback-infinite-loop.js",
            )
            .expect_err("JS loop in timer callback should time out");

        assert_runtime_interrupted(error);
    }

    #[test]
    fn timeout_interrupts_cooperative_native_loop() {
        let mut rt = OtterRuntime::builder()
            .timeout(Duration::from_millis(20))
            .build();
        rt.state_mut()
            .install_native_global(NativeFunctionDescriptor::method(
                "nativeSpin",
                0,
                cooperative_native_spin,
            ));

        let started = std::time::Instant::now();
        let error = rt
            .run_script("nativeSpin();", "cooperative-native-spin.js")
            .expect_err("cooperative native loop should time out");

        assert_runtime_interrupted(error);
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "native cooperative loop should observe timeout promptly"
        );
    }

    #[test]
    fn timeout_interrupts_array_like_native_scan() {
        let mut rt = OtterRuntime::builder()
            .timeout(Duration::from_millis(20))
            .build();

        let started = std::time::Instant::now();
        let error = rt
            .run_script(
                concat!(
                    "const a = { length: 16777216 };\n",
                    "Array.prototype.includes.call(a, 1);\n",
                ),
                "array-like-native-scan.js",
            )
            .expect_err("array-like native scan should time out");

        assert_runtime_interrupted(error);
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "native array scan should observe timeout promptly"
        );
    }

    #[test]
    fn runtime_can_run_again_after_timeout() {
        let (mut rt, capture) = {
            let capture = Arc::new(CaptureConsoleBackend::new());
            let rt = OtterRuntime::builder()
                .timeout(Duration::from_millis(200))
                .console(CaptureForTest(capture.clone()))
                .build();
            (rt, capture)
        };

        let error = rt
            .run_script("while (true) {}", "first-times-out.js")
            .expect_err("first run should time out");
        assert_runtime_interrupted(error);

        rt.run_script("console.log('second run ok');", "second-run.js")
            .expect("fresh run must not observe stale interrupt");
        assert_eq!(capture.text(), "second run ok");
    }

    #[test]
    fn host_callback_wakes_event_loop_before_timeout() {
        let (mut rt, capture) = {
            let capture = Arc::new(CaptureConsoleBackend::new());
            let rt = OtterRuntime::builder()
                .timeout(Duration::from_secs(2))
                .console(CaptureForTest(capture.clone()))
                .build();
            (rt, capture)
        };
        let reservation = rt.state().host_callback_sender().reserve();

        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(20));
            let _ = reservation.enqueue(|runtime| {
                runtime.console().log("host callback fired");
            });
        });

        let started = std::time::Instant::now();
        rt.run_script("1 + 1;", "host-callback-wake.js")
            .expect("host callback should wake event loop before timeout");

        assert_eq!(capture.text(), "host callback fired");
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "host callback should wake promptly"
        );
    }
}
