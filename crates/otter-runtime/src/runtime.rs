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
        assert_eq!(capture.text(), "function\nfunction\nfunction\nfunction\nfunction");
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
        rt.run_script(
            "probe(1, 2, { ok: true });",
            "cross-script-probe-call.js",
        )
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
        rt.run_script(
            "probe(1, 2, 3);",
            "cross-script-arguments-call.js",
        )
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
        rt.run_script(
            "probe();",
            "nested-cross-script-call.js",
        )
        .expect("nested cross-script function calls should work");
    }
}
