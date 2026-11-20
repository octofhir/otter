//! The asynchronous embedding facade and its builder.
//!
//! # Contents
//! Execution and configuration through a sendable runtime handle.
//!
//! # Invariants
//! The handle dispatches work onto its owning isolate.
//!
//! # See also
//! - [`crate::Runtime`] owns the isolate session.

use crate::*;

/// Layer A entry point: zero-config Otter.
///
/// Wraps a [`RuntimeHandle`] with sensible defaults. The simple case
/// for embedders is async-first and safe to clone into Tokio worker
/// tasks.
#[derive(Clone, Debug)]
pub struct Otter {
    pub(super) handle: RuntimeHandle,
}

impl Otter {
    /// Construct with defaults: deny-all capabilities, a 256 MiB heap
    /// cap, and no wall-clock limit.
    #[must_use]
    pub fn new() -> Self {
        Self::builder()
            .build()
            .expect("default OtterBuilder must build")
    }

    /// Start configuring the public handle facade.
    #[must_use]
    pub fn builder() -> OtterBuilder {
        OtterBuilder::default()
    }

    /// Run a file from disk, detecting kind by extension and
    /// routing module-shaped files through the module-graph
    /// pipeline.
    ///
    /// # Algorithm
    /// 1. Read the file's source text.
    /// 2. Use module-only extensions (`.mjs` / `.mts`) to route directly to
    ///    the module graph and script-only extensions (`.cjs` / `.cts`) to
    ///    route directly to script execution.
    /// 3. For ambiguous `.js` / `.ts`, parse once with OXC, inspect the AST
    ///    for module syntax, and reuse that parsed program for script
    ///    compilation when no module syntax is present.
    /// 4. Module sources go through [`Runtime::run_module`]; plain scripts go
    ///    through the script bytecode path.
    ///
    /// # Errors
    /// See [`OtterError`] variants.
    pub async fn run_file(&self, path: impl AsRef<Path>) -> Result<ExecutionResult, OtterError> {
        self.handle.run_file(path.as_ref().to_path_buf()).await
    }

    /// Run an in-memory ES module graph rooted at an absolute URL.
    ///
    /// This is the async facade over [`Runtime::run_module_source`].
    pub async fn run_module_source(
        &self,
        source: SourceInput,
        url: impl Into<String>,
    ) -> Result<ExecutionResult, OtterError> {
        self.handle.run_module_source(source, url).await
    }

    /// Run an in-memory ES module graph in an additional realm.
    pub async fn run_module_source_in_realm(
        &self,
        realm: RuntimeRealmId,
        source: SourceInput,
        url: impl Into<String>,
    ) -> Result<ExecutionResult, OtterError> {
        self.handle
            .run_module_source_in_realm(realm, source, url)
            .await
    }

    /// Run a file and retain partial JIT diagnostics on abrupt failure.
    pub async fn run_file_with_diagnostics(&self, path: impl AsRef<Path>) -> ExecutionAttempt {
        self.handle
            .run_file_with_diagnostics(path.as_ref().to_path_buf())
            .await
    }

    /// Parse and compile a file without executing it.
    ///
    /// Module-shaped files use the same loader and package-graph resolution as
    /// [`Self::run_file`], so CLI `check` and `run` report resolver failures
    /// from the same path.
    ///
    /// # Errors
    /// See [`OtterError`] variants.
    pub async fn check_file(&self, path: impl AsRef<Path>) -> Result<(), OtterError> {
        self.handle.check_file(path.as_ref().to_path_buf()).await
    }

    /// Run an ES module entry file from disk.
    ///
    /// # Errors
    /// See [`OtterError`] variants.
    pub async fn run_module(&self, path: impl AsRef<Path>) -> Result<ExecutionResult, OtterError> {
        self.handle.run_module(path.as_ref().to_path_buf()).await
    }

    /// Run a module and retain partial JIT diagnostics on abrupt failure.
    pub async fn run_module_with_diagnostics(&self, path: impl AsRef<Path>) -> ExecutionAttempt {
        self.handle
            .run_module_with_diagnostics(path.as_ref().to_path_buf())
            .await
    }

    /// Run a string of JavaScript.
    ///
    /// # Errors
    /// See [`OtterError`] variants.
    pub async fn run_script(&self, source: &str) -> Result<ExecutionResult, OtterError> {
        self.handle
            .run_script(
                SourceInput::from_javascript(source).with_top_level_await(),
                "<script>",
            )
            .await
    }

    /// Run a string of TypeScript.
    ///
    /// # Errors
    /// See [`OtterError`] variants.
    pub async fn run_typescript(&self, source: &str) -> Result<ExecutionResult, OtterError> {
        self.handle
            .run_script(
                SourceInput::from_typescript(source).with_top_level_await(),
                "<script>",
            )
            .await
    }

    /// Run a source bundle with an explicit diagnostic specifier.
    ///
    /// This is the async handle equivalent of [`Runtime::run_script`], for
    /// embedders that need to execute multiple script sources in the same
    /// isolate while preserving the full hosted CLI/runtime wiring.
    ///
    /// # Errors
    /// See [`OtterError`] variants.
    pub async fn run_script_source(
        &self,
        source: SourceInput,
        specifier: impl Into<String>,
    ) -> Result<ExecutionResult, OtterError> {
        self.handle.run_script(source, specifier).await
    }

    /// Create and bootstrap an additional realm in this isolate.
    pub async fn create_realm(&self) -> Result<RuntimeRealmId, OtterError> {
        self.handle.create_realm().await
    }

    /// Dispose an additional realm in this isolate.
    pub async fn dispose_realm(&self, realm: RuntimeRealmId) -> Result<(), OtterError> {
        self.handle.dispose_realm(realm).await
    }

    /// Run a classic source bundle in an additional realm.
    pub async fn run_script_in_realm(
        &self,
        realm: RuntimeRealmId,
        source: SourceInput,
        specifier: impl Into<String>,
    ) -> Result<ExecutionResult, OtterError> {
        self.handle
            .run_script_in_realm(realm, source, specifier)
            .await
    }

    /// Run a source bundle and retain partial JIT diagnostics on failure.
    pub async fn run_script_source_with_diagnostics(
        &self,
        source: SourceInput,
        specifier: impl Into<String>,
    ) -> ExecutionAttempt {
        self.handle
            .run_script_with_diagnostics(source, specifier)
            .await
    }

    /// Evaluate a snippet.
    ///
    /// # Errors
    /// See [`OtterError`] variants.
    pub async fn eval(&self, source: &str) -> Result<ExecutionResult, OtterError> {
        self.handle
            .eval(SourceInput::from_javascript(source).with_top_level_await())
            .await
    }

    /// Evaluate a snippet and retain partial JIT diagnostics on failure.
    pub async fn eval_with_diagnostics(&self, source: &str) -> ExecutionAttempt {
        self.handle
            .eval_with_diagnostics(SourceInput::from_javascript(source).with_top_level_await())
            .await
    }

    /// Evaluate a snippet inside `cwd`'s CommonJS scope, the way `node -e`
    /// runs one: `require`, `module` and the builtin modules are in scope.
    pub async fn eval_in_commonjs_scope_with_diagnostics(
        &self,
        source: &str,
        cwd: std::path::PathBuf,
    ) -> ExecutionAttempt {
        self.handle
            .eval_in_commonjs_scope(
                SourceInput::from_javascript(source).with_top_level_await(),
                Some(cwd),
            )
            .await
    }

    /// Start the builtin `builtin` as the run's main program, the way Node
    /// starts `internal/main/test_runner` for `--test`: the runtime loads it
    /// itself, so it may be an internal builtin user code cannot reach.
    pub async fn run_builtin_main_with_diagnostics(&self, builtin: &str) -> ExecutionAttempt {
        self.handle.run_builtin_main(builtin).await
    }

    /// Blocking file execution wrapper for sync embedders.
    ///
    /// # Errors
    /// See [`OtterError`] variants.
    pub fn blocking_run_file(&self, path: impl AsRef<Path>) -> Result<ExecutionResult, OtterError> {
        let path = path.as_ref().to_path_buf();
        self.handle.block_on(self.run_file(path))
    }

    /// Blocking ES module execution wrapper.
    ///
    /// # Errors
    /// See [`OtterError`] variants.
    pub fn blocking_run_module(
        &self,
        path: impl AsRef<Path>,
    ) -> Result<ExecutionResult, OtterError> {
        let path = path.as_ref().to_path_buf();
        self.handle.block_on(self.run_module(path))
    }

    /// Blocking JavaScript execution wrapper.
    ///
    /// # Errors
    /// See [`OtterError`] variants.
    pub fn blocking_run_script(&self, source: &str) -> Result<ExecutionResult, OtterError> {
        let source = source.to_string();
        let handle = self.handle.clone();
        self.handle.block_on(async move {
            handle
                .run_script(
                    SourceInput::from_javascript(source).with_top_level_await(),
                    "<script>",
                )
                .await
        })
    }

    /// Blocking TypeScript execution wrapper.
    ///
    /// # Errors
    /// See [`OtterError`] variants.
    pub fn blocking_run_typescript(&self, source: &str) -> Result<ExecutionResult, OtterError> {
        let source = source.to_string();
        let handle = self.handle.clone();
        self.handle.block_on(async move {
            handle
                .run_script(
                    SourceInput::from_typescript(source).with_top_level_await(),
                    "<script>",
                )
                .await
        })
    }

    /// Blocking eval wrapper.
    ///
    /// # Errors
    /// See [`OtterError`] variants.
    pub fn blocking_eval(&self, source: &str) -> Result<ExecutionResult, OtterError> {
        let source = source.to_string();
        let handle = self.handle.clone();
        self.handle.block_on(async move {
            handle
                .eval(SourceInput::from_javascript(source).with_top_level_await())
                .await
        })
    }

    /// Cooperative cancellation.
    pub fn interrupt(&self) {
        self.handle.interrupt();
    }

    /// Snapshot activity counters.
    #[must_use]
    pub fn activity_stats(&self) -> RuntimeActivityStats {
        self.handle.activity_stats()
    }

    /// Clone the shared resource account used by this isolate and its children.
    #[must_use]
    pub fn resource_account(&self) -> ResourceAccount {
        self.handle.resource_account()
    }

    /// Capture deterministic current, peak, rejection, and limit counters.
    #[must_use]
    pub fn resource_snapshot(&self) -> ResourceSnapshot {
        self.handle.resource_snapshot()
    }

    /// Read the isolate's CPU policy, its published counters, and the shared
    /// resource ledger as one consistent picture.
    #[must_use]
    pub fn budget_report(&self) -> WorkBudgetReport {
        self.handle.budget_report()
    }

    /// Drop down to Layer B.
    #[must_use]
    pub fn handle(&self) -> &RuntimeHandle {
        &self.handle
    }
}

/// Builder for the public [`Otter`] facade.
#[derive(Debug, Clone, Default)]
pub struct OtterBuilder {
    pub(super) runtime: RuntimeBuilder,
}

impl OtterBuilder {
    /// Replace the shared resource ledger with a fresh account using `limits`.
    #[must_use]
    pub fn resource_limits(mut self, limits: ResourceLimits) -> Self {
        self.runtime = self.runtime.resource_limits(limits);
        self
    }

    /// Use an existing account shared with sibling runtime builders.
    #[must_use]
    pub fn resource_account(mut self, account: ResourceAccount) -> Self {
        self.runtime = self.runtime.resource_account(account);
        self
    }

    /// Set the work-slice budget the managed isolate runs under.
    ///
    /// The policy is installed before the isolate's first turn and is readable
    /// with its counters through [`Otter::budget_report`].
    #[must_use]
    pub fn work_budget(mut self, budget: WorkBudget) -> Self {
        self.runtime = self.runtime.work_budget(budget);
        self
    }

    /// Set finite per-isolate capacities for guaranteed terminal work,
    /// in-flight host operations, and live timers. Zero disables admission for
    /// that class.
    #[must_use]
    pub fn completion_capacities(
        mut self,
        guaranteed: usize,
        host_operations: usize,
        timers: usize,
    ) -> Self {
        self.runtime = self
            .runtime
            .completion_capacities(guaranteed, host_operations, timers);
        self
    }

    /// Replace the capability set.
    #[must_use]
    pub fn capabilities(mut self, caps: CapabilitySet) -> Self {
        self.runtime = self.runtime.capabilities(caps);
        self
    }

    /// Set the warning-channel switches (`--no-warnings` and friends).
    #[must_use]
    pub fn warning_options(mut self, options: WarningOptions) -> Self {
        self.runtime = self.runtime.warning_options(options);
        self
    }

    /// Set the initial `process.title` (the CLI's `--title`).
    #[must_use]
    pub fn process_title(mut self, title: Option<String>) -> Self {
        self.runtime = self.runtime.process_title(title);
        self
    }

    /// Engine flags surfaced as `process.execArgv` (the CLI's node-style
    /// switches), so re-execs reproduce the configuration.
    #[must_use]
    pub fn process_exec_argv(mut self, argv: impl IntoIterator<Item = impl Into<String>>) -> Self {
        self.runtime = self.runtime.process_exec_argv(argv);
        self
    }

    /// Install a global `gc()` that forces a full collection
    /// (the CLI's `--expose-gc`).
    #[must_use]
    pub fn expose_gc(mut self, expose: bool) -> Self {
        self.runtime = self.runtime.expose_gc(expose);
        self
    }

    /// See [`RuntimeBuilder::expose_internals`].
    #[must_use]
    pub fn expose_internals(mut self, expose: bool) -> Self {
        self.runtime = self.runtime.expose_internals(expose);
        self
    }

    /// See [`RuntimeBuilder::startup_image`].
    #[must_use]
    pub fn startup_image(mut self, restore: bool) -> Self {
        self.runtime = self.runtime.startup_image(restore);
        self
    }

    /// Allow the main thread to park in `Atomics.wait`.
    ///
    /// Node permits the block; an embedder without a cancellation path
    /// leaves it off.
    #[must_use]
    pub fn allow_blocking_atomics_wait(mut self, allow: bool) -> Self {
        self.runtime = self.runtime.allow_blocking_atomics_wait(allow);
        self
    }

    /// Override capability decisions while retaining runtime-level mandatory
    /// filters such as the environment secret denylist.
    #[must_use]
    pub fn capability_hook(mut self, hook: impl RuntimeCapabilityHook) -> Self {
        self.runtime = self.runtime.capability_hook(hook);
        self
    }

    /// Hard heap cap. `0` disables the cap.
    #[must_use]
    pub fn max_heap_bytes(mut self, bytes: u64) -> Self {
        self.runtime = self.runtime.max_heap_bytes(bytes);
        self
    }

    /// Per-command timeout. `Duration::ZERO` disables the timeout.
    #[must_use]
    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.runtime = self.runtime.timeout(timeout);
        self
    }

    /// JS call-stack depth cap.
    #[must_use]
    pub fn max_stack_depth(mut self, depth: u32) -> Self {
        self.runtime = self.runtime.max_stack_depth(depth);
        self
    }

    /// Use an embedder-owned Tokio runtime for host I/O. See
    /// [`RuntimeBuilder::tokio_handle`].
    #[must_use]
    pub fn tokio_handle(mut self, handle: tokio::runtime::Handle) -> Self {
        self.runtime = self.runtime.tokio_handle(handle);
        self
    }

    /// Share one [`TokioRuntimeHost`] with sibling isolates. See
    /// [`RuntimeBuilder::runtime_host`].
    #[must_use]
    pub fn runtime_host(mut self, host: TokioRuntimeHost) -> Self {
        self.runtime = self.runtime.runtime_host(host);
        self
    }

    /// Override the module-loader configuration.
    #[must_use]
    pub fn module_loader(mut self, loader: module_loader::LoaderConfig) -> Self {
        self.runtime = self.runtime.module_loader(loader);
        self
    }

    /// Install an embedder-owned asynchronous http(s) module provider.
    #[must_use]
    pub fn remote_module_provider(
        mut self,
        provider: impl module_loader::RemoteModuleProvider,
    ) -> Self {
        self.runtime = self.runtime.remote_module_provider(provider);
        self
    }

    /// Register one class-shaped global described by a static spec.
    #[must_use]
    pub fn global_class(mut self, spec: GlobalClass) -> Self {
        self.runtime = self.runtime.global_class(spec);
        self
    }

    /// Register multiple class-shaped globals.
    #[must_use]
    pub fn global_classes(mut self, specs: impl IntoIterator<Item = GlobalClass>) -> Self {
        self.runtime = self.runtime.global_classes(specs);
        self
    }

    /// Register a declared extension (native classes + eager JS bundle).
    #[must_use]
    pub fn extension(mut self, extension: &'static Extension) -> Self {
        self.runtime = self.runtime.extension(extension);
        self
    }

    /// Register one runtime-hosted module such as `node:fs` or `otter:kv`.
    #[must_use]
    pub fn hosted_module(mut self, module: HostedModule) -> Self {
        self.runtime = self.runtime.hosted_module(module);
        self
    }

    /// Register multiple runtime-hosted modules.
    #[must_use]
    pub fn hosted_modules(mut self, modules: impl IntoIterator<Item = HostedModule>) -> Self {
        self.runtime = self.runtime.hosted_modules(modules);
        self
    }

    /// Enable Node-style CommonJS module execution. See
    /// [`RuntimeBuilder::with_nodejs_modules`].
    #[must_use]
    pub fn with_nodejs_modules(mut self) -> Self {
        self.runtime = self.runtime.with_nodejs_modules();
        self
    }

    /// Register the CommonJS native-addon loader. See
    /// [`RuntimeBuilder::commonjs_addon_loader`].
    #[must_use]
    pub fn commonjs_addon_loader(mut self, loader: CommonJsAddonLoader) -> Self {
        self.runtime = self.runtime.commonjs_addon_loader(loader);
        self
    }

    /// Register a global installer that runs on every isolate. See
    /// [`RuntimeBuilder::global_installer`].
    #[must_use]
    pub fn global_installer(mut self, installer: RuntimeGlobalInstaller) -> Self {
        self.runtime = self.runtime.global_installer(installer);
        self
    }

    /// Register an advanced native installer. See
    /// [`RuntimeBuilder::extension_installer`].
    #[must_use]
    pub fn extension_installer(mut self, installer: RuntimeExtensionInstaller) -> Self {
        self.runtime = self.runtime.extension_installer(installer);
        self
    }

    /// Override the implementation behind `console.*`.
    #[must_use]
    pub fn console_sink(mut self, sink: ConsoleSinkHandle) -> Self {
        self.runtime = self.runtime.console_sink(sink);
        self
    }

    /// Observe Promise rejection checkpoints. See
    /// [`RuntimeBuilder::promise_rejection_hook`].
    #[must_use]
    pub fn promise_rejection_hook(mut self, hook: impl PromiseRejectionHook) -> Self {
        self.runtime = self.runtime.promise_rejection_hook(hook);
        self
    }

    /// Set the `process.argv` snapshot installed into the runtime.
    #[must_use]
    pub fn process_argv(mut self, argv: impl IntoIterator<Item = impl Into<String>>) -> Self {
        self.runtime = self.runtime.process_argv(argv);
        self
    }

    /// Set the `process.cwd()` snapshot installed into the runtime.
    #[must_use]
    pub fn process_cwd(mut self, cwd: impl Into<PathBuf>) -> Self {
        self.runtime = self.runtime.process_cwd(cwd);
        self
    }

    /// Add host-supplied environment variables. See
    /// [`RuntimeBuilder::process_env`].
    #[must_use]
    pub fn process_env(
        mut self,
        variables: impl IntoIterator<Item = (impl Into<String>, impl Into<String>)>,
    ) -> Self {
        self.runtime = self.runtime.process_env(variables);
        self
    }

    /// Install a per-instruction step-trace factory. See
    /// [`RuntimeBuilder::tracer_factory`].
    #[must_use]
    pub fn tracer_factory(mut self, factory: Option<TracerFactory>) -> Self {
        self.runtime = self.runtime.tracer_factory(factory);
        self
    }

    /// Sample the VM stack for the whole run. See
    /// [`RuntimeBuilder::cpu_profile_interval`].
    #[must_use]
    pub fn cpu_profile_interval(mut self, interval: Option<u64>) -> Self {
        self.runtime = self.runtime.cpu_profile_interval(interval);
        self
    }

    /// [`RuntimeBuilder::jit_selection`].
    #[must_use]
    pub fn jit_selection(mut self, selection: JitSelection) -> Self {
        self.runtime = self.runtime.jit_selection(selection);
        self
    }

    /// [`RuntimeBuilder::jit_compile_threads`].
    #[must_use]
    pub fn jit_compile_threads(mut self, threads: usize) -> Self {
        self.runtime = self.runtime.jit_compile_threads(threads);
        self
    }

    /// [`RuntimeBuilder::gc_trace`].
    #[must_use]
    pub fn gc_trace(mut self, trace: bool) -> Self {
        self.runtime = self.runtime.gc_trace(trace);
        self
    }

    /// [`RuntimeBuilder::jit_debug`].
    #[must_use]
    pub fn jit_debug(mut self, request: JitDebugRequest) -> Self {
        self.runtime = self.runtime.jit_debug(request);
        self
    }

    /// Bootstrap the runtime this builder declares, encode it as the
    /// executable's startup image and write it into the executable file at
    /// `executable` ([`otter_vm::startup_image::write_into_executable`]);
    /// answers the image's length.
    ///
    /// # Errors
    /// Returns [`OtterError`] if the bootstrap, the capture or the write
    /// fails.
    pub fn write_startup_image(self, executable: &std::path::Path) -> Result<usize, OtterError> {
        let image = self.encode_startup_image()?;
        otter_vm::startup_image::write_into_executable(executable, &image).map_err(|error| {
            OtterError::Internal {
                code: DiagnosticCode::IsolateStart.as_str().to_string(),
                message: format!("startup image write failed: {error}"),
            }
        })?;
        Ok(image.len())
    }

    /// Bootstrap the runtime this builder declares and encode it as the
    /// executable's startup image ([`Runtime::encode_startup_image`]).
    ///
    /// # Errors
    /// Returns [`OtterError`] if the bootstrap or the capture fails.
    pub fn encode_startup_image(self) -> Result<Vec<u8>, OtterError> {
        let mut runtime = self.runtime.startup_image(false).build()?;
        runtime.prepare_startup_image()?;
        runtime
            .encode_startup_image()
            .map_err(|error| OtterError::Internal {
                code: DiagnosticCode::IsolateStart.as_str().to_string(),
                message: format!("startup image capture failed: {error}"),
            })
    }

    /// Construct the public async facade.
    ///
    /// # Errors
    /// Returns [`OtterError`] if configuration validation or isolate
    /// startup fails.
    pub fn build(self) -> Result<Otter, OtterError> {
        Ok(Otter {
            handle: self.runtime.build_handle()?,
        })
    }

    /// Construct the async facade without blocking the async caller during
    /// isolate bootstrap. Browser page/worker creation should prefer this path.
    ///
    /// # Errors
    /// See [`RuntimeBuilder::build_handle_async`].
    pub async fn build_async(self) -> Result<Otter, OtterError> {
        Ok(Otter {
            handle: self.runtime.build_handle_async().await?,
        })
    }
}

impl Default for Otter {
    fn default() -> Self {
        Self::new()
    }
}
