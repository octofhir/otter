//! Runtime builder — fluent API for configuring the Otter JS runtime.
//!
//! # Examples
//!
//! ```rust,no_run
//! use otter_runtime::OtterRuntime;
//!
//! // Minimal:
//! let mut rt = OtterRuntime::builder().build();
//! // Script execution requires the source compiler to accept the input;
//! // during the M0 migration every script fails with `SourceLoweringError::Unsupported`,
//! // so the example below is `no_run` until the compiler covers M1+ AST.
//! let _ = rt.run_script("console.log('hello')", "main.js");
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
    Capabilities, EnvStoreBuilder, HostConfig, HostProcessConfig, HostedExtension,
    HostedNativeModuleLoader, IsolatedEnvStore, ModuleLoaderConfig, RuntimeProfile,
    install_runtime_capabilities, install_runtime_process,
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
    /// Hard cap on the total heap size, in bytes. Analogue of Node.js's
    /// `--max-old-space-size`. `None` = unlimited.
    max_heap_bytes: Option<usize>,
    /// JIT debug flag overrides (applied on top of env-var defaults).
    jit_overrides: otter_jit::config::JitConfigOverrides,
}

impl RuntimeBuilder {
    /// Creates a new builder with default settings.
    pub(crate) fn new() -> Self {
        Self {
            console: None,
            timeout: None,
            host: HostConfig::default(),
            max_heap_bytes: None,
            jit_overrides: otter_jit::config::JitConfigOverrides::default(),
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

    /// Sets a hard cap on the total heap size (object shells + explicit
    /// container reservations), analogous to Node.js's
    /// `--max-old-space-size`. When exceeded, the runtime raises a
    /// catchable `RangeError` (`"out of memory: heap limit exceeded"`)
    /// instead of allowing the OS to terminate the process.
    ///
    /// Passing `0` disables the cap.
    pub fn max_heap_bytes(mut self, bytes: usize) -> Self {
        self.max_heap_bytes = if bytes == 0 { None } else { Some(bytes) };
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

    /// Sets the process metadata exposed to hosted Node-like surfaces.
    pub fn process(mut self, process: HostProcessConfig) -> Self {
        self.host.set_process(process);
        self
    }

    /// Overrides `process.argv`.
    pub fn process_argv(mut self, argv: impl IntoIterator<Item = String>) -> Self {
        let mut process = self.host.process().clone();
        process.argv = argv.into_iter().collect();
        self.host.set_process(process);
        self
    }

    /// Overrides `process.execArgv`.
    pub fn process_exec_argv(mut self, exec_argv: impl IntoIterator<Item = String>) -> Self {
        let mut process = self.host.process().clone();
        process.exec_argv = exec_argv.into_iter().collect();
        self.host.set_process(process);
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

    // ---- JIT debug dump flags ----

    /// Enable `--dump-bytecode`: print compiled bytecodes before JIT compilation.
    pub fn dump_bytecode(mut self, enabled: bool) -> Self {
        self.jit_overrides.dump_bytecode = Some(enabled);
        self
    }

    /// Enable `--dump-mir`: print MIR before codegen.
    pub fn dump_mir(mut self, enabled: bool) -> Self {
        self.jit_overrides.dump_mir = Some(enabled);
        self
    }

    /// Enable `--dump-clif`: print Cranelift IR before native compilation.
    pub fn dump_clif(mut self, enabled: bool) -> Self {
        self.jit_overrides.dump_clif = Some(enabled);
        self
    }

    /// Enable `--dump-asm`: print native code hex dump after compilation.
    pub fn dump_asm(mut self, enabled: bool) -> Self {
        self.jit_overrides.dump_asm = Some(enabled);
        self
    }

    /// Enable `--dump-jit-stats`: print JIT telemetry on runtime exit.
    pub fn dump_jit_stats(mut self, enabled: bool) -> Self {
        self.jit_overrides.dump_jit_stats = Some(enabled);
        self
    }

    /// Builds the configured runtime.
    pub fn build(self) -> OtterRuntime {
        // Apply JIT debug flag overrides before any compilation happens.
        otter_jit::config::apply_overrides(&self.jit_overrides);

        let gc_config = otter_vm::otter_gc::heap::GcConfig {
            max_heap_bytes: self.max_heap_bytes,
            ..otter_vm::otter_gc::heap::GcConfig::default()
        };
        let mut state = RuntimeState::with_gc_config(gc_config);
        let mut host = self.host;

        // Apply console backend.
        if let Some(console) = self.console {
            state.set_console_backend(console);
        }
        // Default: StdioConsoleBackend (already set in RuntimeState::new())

        // Install performance.now() on the global object.
        install_performance_global(&mut state);
        install_runtime_capabilities(&mut state, host.capabilities().clone());
        install_runtime_process(&mut state, host.process().clone(), host.env_store().clone());

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
        let mut loader_config = host.loader().clone();
        loader_config.native_specifiers = host.native_modules().specifiers().into_iter().collect();
        host.set_loader(loader_config);

        OtterRuntime::from_state(state, self.timeout, host)
    }
}
