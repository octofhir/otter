//! Core runtime — owns VM state and drives execution to completion.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use otter_vm::Interpreter;
use otter_vm::interpreter::{ExecutionResult, RuntimeState};
use otter_vm::module::Module;

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
}
