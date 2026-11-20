//! Construction of direct and hosted runtime sessions.
//!
//! # Contents
//! Builder configuration, bootstrap image and isolate admission.
//!
//! # Invariants
//! Both construction paths use the configured resource account.
//!
//! # See also
//! - [`crate::Runtime`] owns the isolate session.

use crate::*;

impl RuntimeBuilder {
    /// Replace the shared resource ledger with a fresh account using `limits`.
    ///
    /// Cloning this builder after the call shares the newly created account.
    /// Runtime construction charges its complete role tuple against this
    /// account before creating an interpreter, channel, or native thread.
    #[must_use]
    pub fn resource_limits(mut self, limits: ResourceLimits) -> Self {
        self.config.resource_account = ResourceAccount::new(limits);
        self
    }

    /// Use an existing account shared with sibling runtime builders.
    #[must_use]
    pub fn resource_account(mut self, account: ResourceAccount) -> Self {
        self.config.resource_account = account;
        self
    }

    /// Set the per-slice work budget the isolate runs under.
    ///
    /// This is the CPU half of the same budget model the resource ledger owns
    /// for memory and queue depth. Bytecode, generated backedges, native calls,
    /// GC, RegExp, and microtasks debit one counter; allocation, host-op, time,
    /// and external-memory limits share its checkpoints. Configuring it
    /// here rather than mutating a built runtime means the policy is in force
    /// before the first turn, on handle threads and workers alike; workers
    /// inherit it and cannot widen it.
    #[must_use]
    pub fn work_budget(mut self, budget: WorkBudget) -> Self {
        self.config.work_budget = budget;
        self
    }

    /// Override the hard-limit family charged by every JavaScript `Worker`
    /// spawned from this runtime and its nested workers. Test-only: the
    /// production family is process-fixed.
    #[cfg(test)]
    #[must_use]
    pub(crate) fn worker_family(mut self, family: Arc<worker::WorkerFamily>) -> Self {
        self.config.worker_family = Some(family);
        self
    }

    /// Set the finite per-isolate capacities for guaranteed terminal work,
    /// in-flight host operations, and live timers. A zero value intentionally
    /// disables admission for that class (fail closed).
    #[must_use]
    pub fn completion_capacities(
        mut self,
        guaranteed: usize,
        host_operations: usize,
        timers: usize,
    ) -> Self {
        self.config.completion_capacities = completion_admission::CompletionCapacities {
            guaranteed,
            host_operations,
            timers,
        };
        self
    }

    /// Replace the capability set.
    #[must_use]
    pub fn capabilities(mut self, caps: CapabilitySet) -> Self {
        self.config.capabilities = caps;
        self
    }

    /// Hard heap cap. `0` disables the cap.
    #[must_use]
    pub fn max_heap_bytes(mut self, bytes: u64) -> Self {
        self.config.max_heap_bytes = bytes;
        self
    }

    /// Per-`run_*` timeout. `Duration::ZERO` disables the timeout.
    #[must_use]
    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.config.timeout = timeout;
        self
    }

    /// JS call-stack depth cap.
    #[must_use]
    pub fn max_stack_depth(mut self, depth: u32) -> Self {
        self.config.max_stack_depth = depth;
        self
    }

    /// Share one Tokio-backed host across this isolate and sibling isolates.
    ///
    /// A browser should construct one [`TokioRuntimeHost`] for the process and
    /// clone it into every page or worker builder. Each runtime still owns an
    /// independent isolate; only executor-backed host services are shared.
    #[must_use]
    pub fn runtime_host(mut self, host: TokioRuntimeHost) -> Self {
        self.config.runtime_host = Some(host);
        self
    }

    /// Use an embedder-owned Tokio runtime for Layer B host I/O.
    ///
    /// [`Self::build_handle`] uses this handle for timers, remote module
    /// fetches, and async native futures instead of discovering an ambient
    /// runtime or creating another executor. The handle is cloned and the
    /// embedder must keep its Tokio runtime alive for at least as long as the
    /// returned [`RuntimeHandle`].
    ///
    /// A direct [`Runtime`] remains executor-agnostic: Layer A embedders install
    /// their own [`HostCompletionSink`] and deliver the resulting jobs through
    /// [`Runtime::run_host_completion`].
    #[must_use]
    pub fn tokio_handle(mut self, handle: tokio::runtime::Handle) -> Self {
        self.config.runtime_host = Some(TokioRuntimeHost::from_handle(handle));
        self
    }

    /// Override the module-loader configuration. The default
    /// (used when this is left untouched) infers `base_dir` from
    /// the entry file's parent directory at `run_module` time
    /// and uses the foundation extension list / ESM+CJS
    /// condition names with `node_modules` walk-up enabled.
    ///
    /// Spec mapping: <https://nodejs.org/api/esm.html#resolution-and-loading-algorithm>
    #[must_use]
    pub fn module_loader(mut self, loader: module_loader::LoaderConfig) -> Self {
        self.config.loader = Some(loader);
        self
    }

    /// Serve ES modules the way a browser does, from `map`.
    ///
    /// Specifiers resolve by the HTML rules through the map's import map,
    /// module sources come only from what the embedder delivered into the
    /// map, and dynamic `import()` parks until its graph is delivered (see
    /// [`Runtime::web_module_fetches`] and
    /// [`Runtime::resume_web_dynamic_imports`]). The rest of an existing
    /// loader configuration is kept.
    #[must_use]
    pub fn web_modules(mut self, map: web_modules::WebModuleMap) -> Self {
        let mut loader = self
            .config
            .loader
            .take()
            .unwrap_or_else(|| module_loader::LoaderConfig::new(PathBuf::from("/")));
        loader.web_modules = Some(map);
        self.config.loader = Some(loader);
        self
    }

    /// Install an embedder-owned asynchronous http(s) module provider.
    ///
    /// The provider receives owned requests and runs outside the isolate.
    /// Otter performs capability checks, bounded graph scheduling, caching,
    /// cancellation, parsing, and compilation around this transport boundary.
    #[must_use]
    pub fn remote_module_provider(
        mut self,
        provider: impl module_loader::RemoteModuleProvider,
    ) -> Self {
        self.config.remote_module_provider = Some(Arc::new(provider));
        self
    }

    /// Register one runtime-hosted module such as `otter:kv`.
    #[must_use]
    pub fn hosted_module(mut self, module: HostedModule) -> Self {
        self.config.hosted_modules.push(module);
        self
    }

    /// Register multiple runtime-hosted modules.
    #[must_use]
    pub fn hosted_modules(mut self, modules: impl IntoIterator<Item = HostedModule>) -> Self {
        self.config.hosted_modules.extend(modules);
        self
    }

    /// Enable Node-style CommonJS module execution (`require`,
    /// `module.exports`, `exports`, `__dirname`, `__filename`) for
    /// script-shaped sources (`.cjs`, CommonJS-typed `.js`, and ambiguous
    /// non-module `.js`). Builtin `require('node:fs')` / `require('fs')` resolve
    /// through the registered hosted modules.
    #[must_use]
    pub fn with_nodejs_modules(mut self) -> Self {
        self.config.commonjs_enabled = true;
        self
    }

    /// Register the native-addon loader used for CommonJS `.node` files.
    ///
    /// This does not grant filesystem or FFI access. The runtime checks `read`
    /// during module resolution and the loader must independently check `ffi`.
    #[must_use]
    pub fn commonjs_addon_loader(mut self, loader: CommonJsAddonLoader) -> Self {
        self.config.commonjs_addon_loader = Some(loader);
        self
    }

    /// Register one class-shaped global described by a static spec.
    #[must_use]
    pub fn global_class(mut self, spec: GlobalClass) -> Self {
        self.config.global_classes.push(spec);
        self
    }

    /// Register multiple class-shaped globals.
    #[must_use]
    pub fn global_classes(mut self, specs: impl IntoIterator<Item = GlobalClass>) -> Self {
        self.config.global_classes.extend(specs);
        self
    }

    /// Register a declared extension. Native classes install eagerly; its
    /// complete JS bundle runs after native/global installers, before class glue.
    #[must_use]
    pub fn extension(mut self, extension: &'static Extension) -> Self {
        self.config.extensions.push(extension);
        self
    }

    /// Register a callback that installs embedder globals on this runtime and
    /// on worker runtimes spawned from it.
    #[must_use]
    pub fn global_installer(mut self, installer: RuntimeGlobalInstaller) -> Self {
        self.config
            .realm_installers
            .push(ConfiguredRealmInstaller::Global(installer));
        self
    }

    /// Register an advanced native installer for an engine extension crate.
    ///
    /// Browser/application orchestration should use [`Self::global_installer`].
    #[must_use]
    pub fn extension_installer(mut self, installer: RuntimeExtensionInstaller) -> Self {
        self.config
            .realm_installers
            .push(ConfiguredRealmInstaller::Extension(installer));
        self
    }

    /// Allow blocking `Atomics.wait` on the main runtime.
    ///
    /// The default is `false`, so direct embedders cannot accidentally park
    /// their host thread forever. Enable this only when the embedder has a
    /// watchdog or another reliable cancellation path.
    #[must_use]
    pub fn allow_blocking_atomics_wait(mut self, allow: bool) -> Self {
        self.config.allow_blocking_atomics_wait = allow;
        self
    }

    /// Install the product `process` global. Enabled by default for CLI and
    /// embedding compatibility; spec harnesses can disable it to keep the
    /// global object closer to an engine shell.
    #[must_use]
    pub fn process_global(mut self, install: bool) -> Self {
        self.config.install_process_global = install;
        self
    }

    /// Install Worker-related globals. Enabled by default for product runtimes;
    /// spec harnesses may disable it once their `$262.agent` host is decoupled
    /// from the runtime Worker backend.
    #[must_use]
    pub fn worker_global(mut self, install: bool) -> Self {
        self.config.install_worker_global = install;
        self
    }

    /// Override the implementation behind `console.*`.
    ///
    /// The default sink writes `log` / `info` / `debug` to standard
    /// output and `warn` / `error` / `trace` / failed `assert` to
    /// standard error, dropping a line the stream refuses. Embedders
    /// can replace it with a sink backed by `tracing`, structured
    /// logs, or test capture.
    #[must_use]
    pub fn console_sink(mut self, sink: ConsoleSinkHandle) -> Self {
        self.config.console_sink = sink;
        self
    }

    /// Observe unhandled and later-handled Promise rejections from Rust.
    ///
    /// The hook is installed independently in every runtime built from this
    /// builder and takes precedence over the legacy
    /// `__otterFirePromiseRejection` JavaScript reporter.
    #[must_use]
    pub fn promise_rejection_hook(mut self, hook: impl PromiseRejectionHook) -> Self {
        self.config.promise_rejection_hook = Some(PromiseRejectionHookHandle::new(hook));
        self
    }

    /// Replace the runtime hook set.
    #[must_use]
    pub fn hooks(mut self, hooks: RuntimeHooks) -> Self {
        self.config.hooks = hooks;
        self
    }

    /// Set the module-resolution hook.
    #[must_use]
    pub fn resolve_hook(mut self, hook: impl RuntimeResolveHook) -> Self {
        self.config.hooks = self.config.hooks.with_resolve_hook(hook);
        self
    }

    /// Set the source-loading hook.
    #[must_use]
    pub fn load_hook(mut self, hook: impl RuntimeLoadHook) -> Self {
        self.config.hooks = self.config.hooks.with_load_hook(hook);
        self
    }

    /// Set the compile hook.
    #[must_use]
    pub fn compile_hook(mut self, hook: impl RuntimeCompileHook) -> Self {
        self.config.hooks = self.config.hooks.with_compile_hook(hook);
        self
    }

    /// Set the runtime job enqueue hook.
    #[must_use]
    pub fn job_hook(mut self, hook: impl RuntimeJobHook) -> Self {
        self.config.hooks = self.config.hooks.with_job_hook(hook);
        self
    }

    /// Set the diagnostic sink hook.
    #[must_use]
    pub fn diagnostic_hook(mut self, hook: impl RuntimeDiagnosticHook) -> Self {
        self.config.hooks = self.config.hooks.with_diagnostic_hook(hook);
        self
    }

    /// Set the capability-check hook.
    #[must_use]
    pub fn capability_hook(mut self, hook: impl RuntimeCapabilityHook) -> Self {
        self.config.hooks = self.config.hooks.with_capability_hook(hook);
        self
    }

    /// Set the `process.argv` snapshot installed into the runtime.
    #[must_use]
    pub fn process_argv(mut self, argv: impl IntoIterator<Item = impl Into<String>>) -> Self {
        self.config.process_argv = Some(argv.into_iter().map(Into::into).collect());
        self
    }

    /// Engine-level flags the process was started with, surfaced as
    /// `process.execArgv` so a re-exec (`spawn(process.execPath,
    /// [...process.execArgv, ...])` and the test harness's flag check)
    /// reproduces the current configuration.
    #[must_use]
    pub fn process_exec_argv(mut self, argv: impl IntoIterator<Item = impl Into<String>>) -> Self {
        self.config.process_exec_argv = argv.into_iter().map(Into::into).collect();
        self
    }

    /// Set the warning-channel switches (`--no-warnings` and friends).
    #[must_use]
    pub fn warning_options(mut self, options: WarningOptions) -> Self {
        self.config.warning_options = options;
        self
    }

    /// Set the initial `process.title` (the CLI's `--title`).
    #[must_use]
    pub fn process_title(mut self, title: Option<String>) -> Self {
        self.config.process_title = title;
        self
    }

    /// Install a global `gc()` that forces a full collection
    /// (the CLI's `--expose-gc`).
    #[must_use]
    pub fn expose_gc(mut self, expose: bool) -> Self {
        self.config.expose_gc = expose;
        self
    }

    /// Let user code load the `internal/*` builtins — all but the realm
    /// module that supplies the others' primordials — through `require`,
    /// `import` and every other route (the CLI's `--expose-internals`).
    #[must_use]
    pub fn expose_internals(mut self, expose: bool) -> Self {
        self.config.expose_internals = expose;
        self
    }

    /// Restore from the executable's startup image instead of running the
    /// bootstrap, when it carries one written for the same API surface this
    /// builder declares ([`Runtime::encode_startup_image`],
    /// [`otter_vm::startup_image::write_into_executable`]); otherwise build as
    /// usual.
    #[must_use]
    pub fn startup_image(mut self, restore: bool) -> Self {
        self.config.startup_image = restore;
        self
    }

    /// Set the `process.cwd()` snapshot installed into the runtime.
    #[must_use]
    pub fn process_cwd(mut self, cwd: impl Into<PathBuf>) -> Self {
        self.config.process_cwd = cwd.into();
        self
    }

    /// Add environment variables the host supplies rather than the operating
    /// system, such as a project's `.env` file.
    ///
    /// A name the process environment already carries wins: the surrounding
    /// environment is the more specific answer, and a file must not silently
    /// override what a shell or a deployment set. Every name still passes the
    /// same capability check as an inherited one.
    #[must_use]
    pub fn process_env(
        mut self,
        variables: impl IntoIterator<Item = (impl Into<String>, impl Into<String>)>,
    ) -> Self {
        self.config.process_env_overlay.extend(
            variables
                .into_iter()
                .map(|(name, value)| (name.into(), value.into())),
        );
        self
    }

    /// Install a per-instruction step-trace factory. The factory
    /// runs once on the isolate runner thread immediately after the
    /// interpreter is constructed; its produced tracer routes every
    /// dispatched instruction through
    /// [`inspect::StepTracer::on_step`]. Pass `None` (the default)
    /// to skip tracing — the dispatch loop then pays only a single
    /// `Option` check per instruction.
    #[must_use]
    pub fn tracer_factory(mut self, factory: Option<TracerFactory>) -> Self {
        self.config.tracer_factory = factory;
        self
    }

    /// Retain at most one VM stack sample per `interval` wall-clock
    /// microseconds for the whole run, so a program that only finishes when
    /// its event loop drains — a server, say — still produces a profile.
    /// Sampling fires from bytecode dispatch and compiled back-edge polls,
    /// covering interpreter and native JIT execution alike. The samples reach
    /// the caller on [`ExecutionResult::cpu_profile`]. `None` (the default)
    /// leaves the profiler uninstalled. Capture uses fixed internal sample,
    /// frame, and retained-byte bounds; discarded work is reported on
    /// [`CpuProfile::dropped_samples`] without another configuration surface.
    #[must_use]
    pub fn cpu_profile_interval(mut self, interval: Option<u64>) -> Self {
        self.config.cpu_profile_interval = interval;
        self
    }

    /// Select which execution tiers the runtime installs. Differential
    /// harnesses build one runtime per [`JitSelection`] and compare
    /// observable behavior; production embedders keep the default.
    #[must_use]
    pub fn jit_selection(mut self, selection: JitSelection) -> Self {
        self.config.jit_selection = selection;
        self
    }

    /// Whether each run's result carries the runtime's execution statistics
    /// ([`ExecutionResult::stats`]). On by default. Gathering them walks every
    /// linked function's inline caches, so an embedder that runs many small
    /// scripts into a large heap — a browser dispatching events into a page —
    /// and never reads them turns this off; [`Runtime::execution_stats`] still
    /// answers when asked.
    #[must_use]
    pub fn execution_stats(mut self, collect: bool) -> Self {
        self.config.execution_stats = collect;
        self
    }

    /// Observe every collection pause of the runtime's runs, with the heap
    /// occupancy before and after it; each result carries its run's pauses
    /// ([`ExecutionResult::gc_pauses`]). Off by default.
    #[must_use]
    pub fn gc_trace(mut self, trace: bool) -> Self {
        self.config.gc_trace = trace;
        self
    }

    /// Threads the isolate compiles optimized code on, started at its first
    /// optimizing compilation; [`DEFAULT_JIT_COMPILE_THREADS`] by default.
    /// A function keeps running its current tier while it compiles. Zero
    /// compiles on the isolate thread, which installs the result before it
    /// continues: tier transitions then happen at fixed points of execution,
    /// which harnesses asserting them rely on.
    #[must_use]
    pub fn jit_compile_threads(mut self, threads: usize) -> Self {
        self.config.jit_compile_threads = threads;
        self
    }

    /// Configure default-off structured JIT diagnostics.
    #[must_use]
    pub fn jit_debug(mut self, request: JitDebugRequest) -> Self {
        self.config.jit_debug = request;
        self
    }

    /// Construct the runtime.
    ///
    /// # Errors
    /// Returns [`OtterError::Config`] when the configuration is
    /// inconsistent.
    pub fn build(self) -> Result<Runtime, OtterError> {
        Runtime::from_config(self.config)
    }

    /// Construct a sendable runtime handle.
    ///
    /// # Errors
    /// Returns [`OtterError`] when the configuration is invalid or the
    /// isolate runner cannot be started.
    pub fn build_handle(self) -> Result<RuntimeHandle, OtterError> {
        RuntimeHandle::spawn(self.config)
    }

    /// Construct a sendable runtime whose admission includes a worker slot.
    ///
    /// Kept private to the worker surface so ordinary handles cannot
    /// accidentally consume worker capacity and workers cannot bypass it.
    pub(crate) fn build_worker_handle(self) -> Result<RuntimeHandle, OtterError> {
        RuntimeHandle::spawn_worker(self.config)
    }

    /// Construct a sendable runtime handle without blocking the async caller
    /// during isolate bootstrap.
    ///
    /// Browser page creation should prefer this method. Validation and isolate
    /// admission happen before any blocking task is scheduled; thread startup
    /// and the interrupt-handle handshake then run on the configured shared
    /// Tokio host's blocking pool. UI/core async workers remain free to drive
    /// input, rendering and I/O.
    ///
    /// # Errors
    /// Returns [`OtterError`] when no Tokio host/current runtime is available,
    /// configuration is invalid, or isolate startup fails.
    pub async fn build_handle_async(self) -> Result<RuntimeHandle, OtterError> {
        let admitted = Runtime::admit_config(self.config, RuntimeAdmissionKind::HandleThread)?;
        let host_handle = match admitted.config.runtime_host() {
            Some(host) => host.handle(),
            None => {
                tokio::runtime::Handle::try_current().map_err(|error| OtterError::Internal {
                    code: DiagnosticCode::TokioRuntimeCreate.as_str().to_string(),
                    message: format!("async isolate startup requires a Tokio runtime: {error}"),
                })?
            }
        };
        host_handle
            .spawn_blocking(move || RuntimeHandle::spawn_admitted(admitted))
            .await
            .map_err(|error| OtterError::Internal {
                code: DiagnosticCode::IsolateStart.as_str().to_string(),
                message: format!("async isolate startup task failed: {error}"),
            })?
    }
}
