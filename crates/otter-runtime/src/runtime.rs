//! Core runtime — owns VM state and drives execution to completion.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use otter_vm::interpreter::{ExecutionResult, RuntimeState};
use otter_vm::module::Module;
use otter_vm::Interpreter;

use crate::builder::RuntimeBuilder;

/// Error from script execution.
#[derive(Debug)]
pub enum RunError {
    /// Source failed to compile.
    Compile(String),
    /// Runtime error during execution.
    Runtime(String),
}

impl std::fmt::Display for RunError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Compile(e) => write!(f, "CompileError: {e}"),
            Self::Runtime(e) => write!(f, "RuntimeError: {e}"),
        }
    }
}

impl std::error::Error for RunError {}

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
}

impl OtterRuntime {
    /// Returns a new [`RuntimeBuilder`] for configuring the runtime.
    pub fn builder() -> RuntimeBuilder {
        RuntimeBuilder::new()
    }

    /// Creates a runtime from pre-configured state. Called by the builder.
    pub(crate) fn from_state(state: RuntimeState, timeout: Option<Duration>) -> Self {
        Self { state, timeout }
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

    /// Evaluates a JavaScript expression and returns the result.
    /// Convenience wrapper over [`run_script`].
    pub fn eval(&mut self, code: &str) -> Result<ExecutionResult, RunError> {
        self.run_script(code, "<eval>")
    }

    /// Executes a pre-compiled module to completion with the runtime's state.
    pub fn run_module(&mut self, module: &Module) -> Result<ExecutionResult, RunError> {
        // Set up timeout interrupt if configured.
        let mut interpreter = Interpreter::new();
        let _interrupt_guard = self.timeout.map(|timeout| {
            let flag = interpreter.interrupt_flag();
            TimeoutGuard::arm(flag, timeout)
        });

        // 1. Execute top-level code.
        let result = interpreter
            .execute_module(module, &mut self.state)
            .map_err(|e| RunError::Runtime(e.to_string()))?;

        // 2. Drain microtasks after top-level execution (ES spec).
        self.drain_microtasks(module);

        // 3. Event loop: process pending timers + microtasks until quiescent.
        self.run_event_loop(module);

        Ok(result)
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

    // -----------------------------------------------------------------------
    // Internal: microtask drain
    // -----------------------------------------------------------------------

    fn drain_microtasks(&mut self, module: &Module) {
        loop {
            let mut did_work = false;

            while let Some(job) = self.state.microtasks_mut().pop_next_tick() {
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
                let _ = Interpreter::call_function(
                    &mut self.state,
                    module,
                    job.callback,
                    job.this_value,
                    &[job.argument],
                );
                did_work = true;
            }

            while let Some(job) = self.state.microtasks_mut().pop_microtask() {
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
    }

    // -----------------------------------------------------------------------
    // Internal: event loop
    // -----------------------------------------------------------------------

    fn run_event_loop(&mut self, module: &Module) {
        loop {
            let has_timers = self.state.timers().has_pending();
            let has_microtasks = !self.state.microtasks().is_empty();

            if !has_timers && !has_microtasks {
                break;
            }

            let fired = self
                .state
                .timers_mut()
                .collect_fired(std::time::Instant::now());

            if fired.is_empty() && !has_microtasks {
                if let Some(deadline) = self.state.timers().next_deadline() {
                    let now = std::time::Instant::now();
                    if deadline > now {
                        std::thread::sleep(deadline - now);
                    }
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
                self.drain_microtasks(module);
            }

            self.drain_microtasks(module);
        }
    }
}

/// RAII guard that spawns a thread to set the interrupt flag after a timeout.
struct TimeoutGuard {
    _handle: std::thread::JoinHandle<()>,
}

impl TimeoutGuard {
    fn arm(flag: Arc<AtomicBool>, timeout: Duration) -> Self {
        let handle = std::thread::spawn(move || {
            std::thread::sleep(timeout);
            flag.store(true, Ordering::Relaxed);
        });
        Self { _handle: handle }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use otter_vm::console::CaptureConsoleBackend;
    use std::sync::Arc;

    fn rt_with_capture() -> (OtterRuntime, Arc<CaptureConsoleBackend>) {
        let capture = Arc::new(CaptureConsoleBackend::new());
        let rt = OtterRuntime::builder()
            .console(CaptureForTest(capture.clone()))
            .build();
        (rt, capture)
    }

    struct CaptureForTest(Arc<CaptureConsoleBackend>);
    impl otter_vm::console::ConsoleBackend for CaptureForTest {
        fn log(&self, msg: &str) { self.0.log(msg); }
        fn warn(&self, msg: &str) { self.0.warn(msg); }
        fn error(&self, msg: &str) { self.0.error(msg); }
    }

    #[test]
    fn run_simple_arithmetic() {
        let (mut rt, capture) = rt_with_capture();
        rt.run_script("console.log(1 + 2)", "test.js").expect("should run");
        assert_eq!(capture.text(), "3");
    }

    #[test]
    fn run_console_log() {
        let (mut rt, capture) = rt_with_capture();
        rt.run_script("console.log(42)", "test.js").expect("should run");
        assert_eq!(capture.text(), "42");
    }

    #[test]
    fn run_console_multiple_args() {
        let (mut rt, capture) = rt_with_capture();
        rt.run_script(
            "console.log('hello', true, 3.14)",
            "test.js",
        )
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
}
