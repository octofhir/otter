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

use otter_macros::dive;
use otter_vm::console::ConsoleBackend;
use otter_vm::descriptors::VmNativeCallError;
use otter_vm::interpreter::RuntimeState;
use otter_vm::value::RegisterValue;

use std::sync::Arc;

use crate::host::{
    Capabilities, EnvStoreBuilder, HostConfig, HostedExtension, HostedNativeModuleLoader,
    IsolatedEnvStore, ModuleLoaderConfig, RuntimeProfile, install_runtime_capabilities,
};
use crate::runtime::OtterRuntime;

/// Epoch for performance.now() — set once at process start.
static EPOCH: std::sync::LazyLock<Instant> = std::sync::LazyLock::new(Instant::now);

fn install_performance_global(state: &mut RuntimeState) {
    // Force epoch initialization.
    let _ = *EPOCH;

    let now_id = state.register_native_function(performance_now_descriptor());
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

#[dive(name = "now", length = 0)]
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
    host: HostConfig,
}

impl RuntimeBuilder {
    /// Creates a new builder with default settings.
    pub(crate) fn new() -> Self {
        Self {
            console: None,
            timeout: None,
            host: HostConfig::default(),
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

    /// Sets host capabilities for the runtime instance.
    pub fn capabilities(mut self, caps: Capabilities) -> Self {
        self.host.set_capabilities(caps);
        self
    }

    /// Sets an explicit isolated environment store.
    pub fn env_store(mut self, store: IsolatedEnvStore) -> Self {
        self.host.set_env_store(std::sync::Arc::new(store));
        self
    }

    /// Builds an isolated environment store with the provided closure.
    pub fn env(mut self, f: impl FnOnce(EnvStoreBuilder) -> EnvStoreBuilder) -> Self {
        self.host
            .set_env_store(std::sync::Arc::new(f(EnvStoreBuilder::new()).build()));
        self
    }

    /// Sets the runtime host profile.
    pub fn profile(mut self, profile: RuntimeProfile) -> Self {
        self.host.set_profile(profile);
        self
    }

    /// Sets hosted module loader configuration.
    pub fn module_loader(mut self, loader: ModuleLoaderConfig) -> Self {
        self.host.set_loader(loader);
        self
    }

    /// Registers one native hosted module specifier on the new host layer.
    pub fn native_module(
        mut self,
        specifier: impl Into<String>,
        module: impl HostedNativeModuleLoader + 'static,
    ) -> Self {
        let mut registry = self.host.native_modules().clone();
        registry
            .register(specifier, Arc::new(module))
            .expect("native hosted module registration should be unique");
        self.host.set_native_modules(registry);
        self
    }

    /// Registers one hosted extension on the new host layer.
    pub fn extension(mut self, extension: impl HostedExtension + 'static) -> Self {
        let mut registry = self.host.extensions().clone();
        registry
            .register(Arc::new(extension))
            .expect("hosted extension registration should be valid");
        self.host.set_extensions(registry);
        self
    }

    /// Builds the configured runtime.
    pub fn build(self) -> OtterRuntime {
        let mut state = RuntimeState::new();
        let mut host = self.host;

        // Apply console backend.
        if let Some(console) = self.console {
            state.set_console_backend(console);
        }
        // Default: StdioConsoleBackend (already set in RuntimeState::new())

        // Install performance.now() on the global object.
        install_performance_global(&mut state);
        install_runtime_capabilities(&mut state, host.capabilities().clone());

        host.extensions()
            .bootstrap(&mut state, host.profile())
            .expect("hosted extension bootstrap should succeed");

        let mut native_modules = host.native_modules().clone();
        let extension_modules = host
            .extensions()
            .native_module_registry(host.profile())
            .expect("hosted extension native modules should register");
        for (specifier, loader) in extension_modules.into_entries() {
            native_modules
                .register(specifier, loader)
                .expect("extension/native module specifiers should not conflict");
        }
        host.set_native_modules(native_modules);

        OtterRuntime::from_state(state, self.timeout, host)
    }
}
