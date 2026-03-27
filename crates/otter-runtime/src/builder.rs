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

use std::time::Duration;

use otter_vm::console::ConsoleBackend;
use otter_vm::interpreter::RuntimeState;

use crate::runtime::OtterRuntime;

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

        OtterRuntime::from_state(state, self.timeout)
    }
}
