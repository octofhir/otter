//! Runtime builder — fluent API for configuring the Otter JS runtime.
//!
//! # Examples
//!
//! ```rust
//! use otter_runtime::OtterRuntime;
//!
//! // Minimal:
//! let mut rt = OtterRuntime::builder().build();
//! rt.run_script("console.log('hello')", "main.js").unwrap();
//!
//! // With custom console:
//! // let mut rt = OtterRuntime::builder()
//! //     .console(MyLogBackend)
//! //     .build();
//! ```

use std::time::{Duration, Instant};

use otter_vm::console::ConsoleBackend;
use otter_vm::descriptors::{NativeFunctionDescriptor, VmNativeCallError};
use otter_vm::interpreter::RuntimeState;
use otter_vm::value::RegisterValue;

use crate::runtime::OtterRuntime;

/// Epoch for performance.now() — set once at process start.
static EPOCH: std::sync::LazyLock<Instant> = std::sync::LazyLock::new(Instant::now);

fn install_performance_global(state: &mut RuntimeState) {
    // Force epoch initialization.
    let _ = *EPOCH;

    let now_desc = NativeFunctionDescriptor::method("now", 0, performance_now);
    let now_id = state.register_native_function(now_desc);
    let now_fn = state.alloc_host_function(now_id);

    let perf_obj = state.alloc_object();
    let now_prop = state.intern_property_name("now");
    state
        .objects_mut()
        .set_property(
            perf_obj,
            now_prop,
            RegisterValue::from_object_handle(now_fn.0),
        )
        .ok();

    state.install_global_value("performance", RegisterValue::from_object_handle(perf_obj.0));
}

fn performance_now(
    _this: &RegisterValue,
    _args: &[RegisterValue],
    _runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let elapsed = EPOCH.elapsed();
    let ms = elapsed.as_secs_f64() * 1000.0;
    Ok(RegisterValue::from_number(ms))
}

/// Builder for constructing a configured [`OtterRuntime`].
pub struct RuntimeBuilder {
    console: Option<Box<dyn ConsoleBackend>>,
    timeout: Option<Duration>,
}

impl RuntimeBuilder {
    /// Creates a new builder with default settings.
    pub(crate) fn new() -> Self {
        Self {
            console: None,
            timeout: None,
        }
    }

    /// Sets a custom console backend. If not called, uses [`StdioConsoleBackend`]
    /// (stdout for log/info/debug, stderr for warn/error).
    pub fn console(mut self, backend: impl ConsoleBackend + 'static) -> Self {
        self.console = Some(Box::new(backend));
        self
    }

    /// Sets a maximum execution timeout. Scripts that exceed this duration
    /// will be interrupted with an error.
    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.timeout = Some(timeout);
        self
    }

    // Future: .with_node_builtins(), .module(), .permissions()
    // These will be added when the module system is implemented.

    /// Builds the configured runtime.
    pub fn build(self) -> OtterRuntime {
        let mut state = RuntimeState::new();

        // Apply console backend.
        if let Some(console) = self.console {
            state.set_console_backend(console);
        }
        // Default: StdioConsoleBackend (already set in RuntimeState::new())

        // Install performance.now() on the global object.
        install_performance_global(&mut state);

        OtterRuntime::from_state(state, self.timeout)
    }
}
