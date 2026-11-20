//! Module and file execution and CommonJS routing.
//!
//! # Contents
//! Module and file entry points, CommonJS routing, linking and evaluation.
//!
//! # Invariants
//! Module linking and evaluation use the session-owned module graph.
//!
//! # See also
//! - [`crate::Runtime`] owns the isolate session.

use crate::*;

impl Runtime {
    /// Load + link + execute the module-graph rooted at `entry_path`.
    ///
    /// # Algorithm
    /// 1. Build a [`module_loader::ModuleLoader`] rooted at the
    ///    entry's parent directory.
    /// 2. Walk the dependency graph
    ///    ([`module_graph::load_program`]), resolving every static
    ///    import + literal-string `import("./x")` into a unified
    ///    linked [`BytecodeModule`].
    /// 3. Pre-allocate one `module_env` JsObject per module URL,
    ///    register each in the interpreter, and append self-loop
    ///    `(referrer = entry_url, specifier = url, target = url)`
    ///    rows to `module_resolutions` so the synthesised
    ///    `<entry>` driver's `Op::ImportNamespace` lookups succeed.
    /// 4. Run `<entry>` through the existing dispatch loop. Drain
    ///    microtasks afterwards.
    ///
    /// # Errors
    /// See [`OtterError`] variants. Loader / link errors surface
    /// as [`OtterError::Compile`].
    ///
    /// # See also
    /// - <https://tc39.es/ecma262/#sec-modules>
    pub fn run_module(
        &mut self,
        entry_path: impl AsRef<Path>,
    ) -> Result<ExecutionResult, OtterError> {
        self.interp.begin_jit_debug_capture();
        self.run_module_with_context(entry_path)
            .map(|(result, _)| self.attach_jit_debug_report(result))
    }

    /// Load, link, and execute an ES module graph from an in-memory entry.
    ///
    /// `url` is the module's canonical absolute URL. Static imports resolve
    /// relative to it and load through the configured module loader / remote
    /// fetch hook. The entry text itself is never read from the filesystem,
    /// which makes this the `<script type="module">` and pre-fetched HTTP
    /// module entry point for browser embedders.
    ///
    /// # Errors
    /// See [`OtterError`] variants. A relative or malformed entry URL is a
    /// resolution diagnostic.
    pub fn run_module_source(
        &mut self,
        source: SourceInput,
        url: impl Into<String>,
    ) -> Result<ExecutionResult, OtterError> {
        let url = url.into();
        self.with_direct_timeout(move |runtime| runtime.run_module_source_unbounded(source, url))
    }

    pub(super) fn run_module_source_unbounded(
        &mut self,
        source: SourceInput,
        url: String,
    ) -> Result<ExecutionResult, OtterError> {
        self.interp.begin_jit_debug_capture();
        let start = std::time::Instant::now();
        let loader = self.module_loader_for_entry(Path::new("."));
        let interrupt = self.interp.interrupt_handle();
        let linked = self
            .module_graph
            .load_program_source_interruptible(
                &loader,
                {
                    let text =
                        module_loader::admit_source(loader.resource_account(), &url, source.text)
                            .map_err(module_graph::GraphError::Loader)
                            .map_err(map_graph_error)?;
                    module_loader::ResolvedSource {
                        url,
                        kind: source.kind,
                        jsx: None,
                        text,
                    }
                },
                interrupt,
            )
            .map_err(map_graph_error)?;
        self.execute_linked_module(linked, start, None)
            .map(|(result, _)| self.attach_jit_debug_report(result))
    }

    /// Execute a module graph prepared away from the isolate thread.
    ///
    /// Layer B builds and links remote graphs on Tokio's blocking pool, then
    /// transfers this owned bytecode/metadata bundle to the isolate for the
    /// GC- and realm-local instantiation/evaluation phase.
    pub(crate) fn run_prepared_module(
        &mut self,
        linked: module_graph::LinkedProgram,
    ) -> Result<ExecutionResult, OtterError> {
        self.interp.begin_jit_debug_capture();
        self.module_graph.record_linked(&linked);
        self.execute_linked_module(linked, std::time::Instant::now(), None)
            .map(|(result, _)| self.attach_jit_debug_report(result))
    }

    /// Run a module while retaining partial JIT diagnostics on abrupt failure.
    pub fn run_module_with_diagnostics(
        &mut self,
        entry_path: impl AsRef<Path>,
    ) -> ExecutionAttempt {
        let result = self.run_module(entry_path);
        self.finish_jit_debug_attempt(result)
    }

    /// Load, link, and execute a module graph with opt-in phase timings.
    ///
    /// Ordinary [`Self::run_module`] calls do not read the clock at phase
    /// boundaries or accumulate telemetry. This surface is intended for
    /// benchmark evidence and explicit diagnostics.
    ///
    /// # Errors
    /// See [`OtterError`] variants.
    pub fn run_module_profiled(
        &mut self,
        entry_path: impl AsRef<Path>,
    ) -> Result<(ExecutionResult, module_graph::ModulePhaseTimings), OtterError> {
        let entry_path = entry_path.as_ref().to_path_buf();
        self.with_direct_timeout(move |runtime| runtime.run_module_profiled_unbounded(&entry_path))
    }

    pub(super) fn run_module_profiled_unbounded(
        &mut self,
        entry_path: &Path,
    ) -> Result<(ExecutionResult, module_graph::ModulePhaseTimings), OtterError> {
        self.interp.begin_jit_debug_capture();
        let mut timings = module_graph::ModulePhaseTimings::default();
        let (result, _) = self.run_module_with_context_inner(entry_path, Some(&mut timings))?;
        Ok((self.attach_jit_debug_report(result), timings))
    }

    /// Register every linked module's §16.2.1.6 ResolveExport table with
    /// the interpreter so the Module Namespace Exotic Object reads and
    /// `Op::LoadImportBinding` resolve re-exported / star-exported names
    /// to the defining module's live binding. Called once per graph load,
    /// before evaluation begins.
    pub(super) fn register_resolved_exports(&mut self, metadata: &[CompiledModuleMetadata]) {
        for module in metadata {
            if module.source_url.is_empty() || module.resolved_exports.is_empty() {
                continue;
            }
            let table = module
                .resolved_exports
                .iter()
                .map(|(name, resolved)| {
                    (
                        name.clone(),
                        (
                            std::sync::Arc::from(resolved.defining_module.as_str()),
                            resolved.binding.clone(),
                        ),
                    )
                })
                .collect();
            self.interp.register_module_resolved_exports(
                std::sync::Arc::from(module.source_url.as_str()),
                table,
            );
        }
    }

    /// Forward every linked module's verbatim source to the interpreter
    /// so `Error.prototype.stack` and `util.getCallSites` can resolve a
    /// frame's byte span to a `(line, column)` position. Called once per
    /// graph load, before evaluation begins.
    pub(super) fn report_watch_imports(
        &mut self,
        sources: &std::collections::BTreeMap<String, otter_resource::SharedSource>,
    ) {
        if !commonjs::watch_reporting_requested() {
            return;
        }
        let files: Vec<&str> = sources
            .keys()
            .filter(|url| url.starts_with("file://"))
            .map(String::as_str)
            .collect();
        if files.is_empty() {
            return;
        }
        let Some(context) = self.realm_execution_context() else {
            return;
        };
        otter_vm::NativeCtx::with_host_context(
            &mut self.interp,
            otter_vm::NativeCallInfo::default_call(),
            Some(&context),
            |ctx| commonjs::report_watch_imports(ctx, files.iter().copied()),
        );
    }

    pub(crate) fn run_module_with_context(
        &mut self,
        entry_path: impl AsRef<Path>,
    ) -> Result<(ExecutionResult, ExecutionContext), OtterError> {
        let entry_path = entry_path.as_ref().to_path_buf();
        self.with_direct_timeout(move |runtime| {
            runtime.run_module_with_context_inner(&entry_path, None)
        })
    }

    pub(super) fn run_module_with_context_inner(
        &mut self,
        entry_path: &Path,
        mut timings: Option<&mut module_graph::ModulePhaseTimings>,
    ) -> Result<(ExecutionResult, ExecutionContext), OtterError> {
        let start = std::time::Instant::now();
        let loader = self.module_loader_for_entry(entry_path);
        let linked = if timings.is_some() {
            let (linked, graph_timings) = self
                .module_graph
                .load_program_profiled(&loader, entry_path)
                .map_err(map_graph_error)?;
            *timings.as_deref_mut().expect("checked timing sink") = graph_timings;
            linked
        } else {
            let interrupt = self.interp.interrupt_handle();
            self.module_graph
                .load_program_interruptible(&loader, entry_path, interrupt)
                .map_err(map_graph_error)?
        };

        self.execute_linked_module(linked, start, timings)
    }

    pub(super) fn execute_linked_module(
        &mut self,
        linked: module_graph::LinkedProgram,
        start: std::time::Instant,
        mut timings: Option<&mut module_graph::ModulePhaseTimings>,
    ) -> Result<(ExecutionResult, ExecutionContext), OtterError> {
        let runtime_link_started = timings.is_some().then(std::time::Instant::now);
        self.adopt_web_module_bodies();
        let mut module = linked.module;
        for metadata in &linked.metadata {
            self.source_maps.record_compiled_metadata(metadata);
        }
        let entry_url = linked.entry_url.clone();
        let realm_id = self.interp.active_host_realm_id();
        hosted_completion::prepare_entry_resolutions(
            &self.module_records,
            realm_id,
            &mut module,
            &entry_url,
        );
        let sources = otter_vm::source_registry::SourceRegistry::new(
            linked.module_sources.clone(),
            &self.config.resource_account,
        );
        if let (Some(timings), Some(started)) = (timings.as_deref_mut(), runtime_link_started) {
            timings.link_time_ns = timings
                .link_time_ns
                .saturating_add(started.elapsed().as_nanos().min(u128::from(u64::MAX)) as u64);
        }
        let sources = sources.map_err(OtterError::from)?;
        let codeblock_started = timings.is_some().then(std::time::Instant::now);
        let context = self.interp.link_module(module, sources)?;
        if let (Some(timings), Some(started)) = (timings.as_deref_mut(), codeblock_started) {
            timings.compile_time_ns = timings
                .compile_time_ns
                .saturating_add(started.elapsed().as_nanos().min(u128::from(u64::MAX)) as u64);
        }
        let instantiate_started = timings.is_some().then(std::time::Instant::now);
        self.module_records
            .allocate_for_module_inits(&mut self.interp, &context, &self.commonjs)
            .map_err(|error| hosted_completion::into_runtime(&mut self.interp, error))?;
        // Only real registered environments precede export-table publication.
        self.register_resolved_exports(&linked.metadata);
        self.report_watch_imports(&linked.module_sources);
        self.module_records.mark_evaluating(realm_id);
        if let (Some(timings), Some(started)) = (timings.as_deref_mut(), instantiate_started) {
            timings.link_time_ns = timings
                .link_time_ns
                .saturating_add(started.elapsed().as_nanos().min(u128::from(u64::MAX)) as u64);
        }
        let execute_started = timings.is_some().then(std::time::Instant::now);
        let script_outcome = self.interp.run(&context);
        let drain_outcome = checkpoint::after_script(&script_outcome, || {
            self.drain_microtasks_dispatching_uncaught()
        });
        let value = match (script_outcome, drain_outcome) {
            (
                Err(otter_vm::RunError {
                    error: otter_vm::VmError::Exit { code },
                    ..
                }),
                _,
            )
            | (
                Ok(_),
                Err(otter_vm::RunError {
                    error: otter_vm::VmError::Exit { code },
                    ..
                }),
            ) => {
                self.module_records.mark_evaluated(realm_id);
                if let (Some(timings), Some(started)) = (timings.as_deref_mut(), execute_started) {
                    timings.execute_time_ns =
                        started.elapsed().as_nanos().min(u128::from(u64::MAX)) as u64;
                }
                let result = ExecutionResult::from_exit_code(code, start.elapsed());
                return Ok((self.attach_execution_stats(result), context));
            }
            (Err(script_err), _) => {
                self.module_records.mark_errored(realm_id);
                return Err(enrich_runtime_diagnostic_with_cause(
                    &mut self.interp,
                    map_vm_error(script_err),
                ));
            }
            (Ok(_), Err(drain_err)) => {
                self.module_records.mark_errored(realm_id);
                return Err(enrich_runtime_diagnostic_with_cause(
                    &mut self.interp,
                    map_vm_error(drain_err),
                ));
            }
            (Ok(v), Ok(())) => v,
        };
        self.pump_layer_a_dynamic_imports()?;
        if let (Some(timings), Some(started)) = (timings, execute_started) {
            timings.execute_time_ns = started.elapsed().as_nanos().min(u128::from(u64::MAX)) as u64;
        }
        self.module_records.mark_evaluated(realm_id);
        let result =
            ExecutionResult::from_vm_value(value, start.elapsed(), self.interp.gc_heap_mut())
                .with_exit_code(process::exit_code(&self.interp));
        let result = self.attach_execution_stats(result);
        Ok((result, context))
    }

    pub(super) fn module_loader_for_entry(&self, entry_path: &Path) -> module_loader::ModuleLoader {
        self.module_loader.for_entry(
            entry_path,
            &self.config.hosted_modules,
            &self.config.capabilities,
            &self.config.hooks,
            &self.config.resource_account,
        )
    }

    /// Parse and compile a file without executing it.
    ///
    /// Module-shaped inputs use the same [`module_loader`] and module graph
    /// pipeline as [`Self::run_file`]. Script-shaped inputs use the same
    /// script compiler path, stopping before interpreter dispatch.
    ///
    /// # Errors
    /// See [`OtterError`] variants.
    pub fn check_file(&mut self, path: impl AsRef<Path>) -> Result<(), OtterError> {
        let path = path.as_ref();
        let source = SourceInput::from_path(path)?;
        if source_path_has_module_extension(path) {
            return self.check_module(path);
        }
        let package_type = {
            let loader = self.module_loader_for_entry(path);
            source_path_package_type(path, &loader)
        };
        if package_type == Some(module_loader::LoaderPackageType::Module) {
            return self.check_module(path);
        }
        let specifier = path.to_string_lossy().to_string();
        if package_type == Some(module_loader::LoaderPackageType::CommonJs) {
            return compile_script_source(&source.text, source.kind, &specifier)
                .map(|_| ())
                .map_err(|err| map_compile_error(err, &specifier));
        }
        if !source_path_has_script_extension(path) {
            let module = with_program(&source.text, source.kind, |program| {
                if program_looks_like_module(program) {
                    return Ok(None);
                }
                compile_script_program(program, source.kind, &specifier, LAZY_FUNCTIONS)
                    .map(Some)
                    .map_err(|err| map_compile_error(err, &specifier))
            })
            .map_err(|err| map_syntax_error(err, &specifier))??;
            if module.is_some() {
                return Ok(());
            }
            return self.check_module(path);
        }
        compile_script_source(&source.text, source.kind, &specifier)
            .map(|_| ())
            .map_err(|err| map_compile_error(err, &specifier))
    }

    pub(super) fn check_module(&mut self, entry_path: &Path) -> Result<(), OtterError> {
        let loader = self.module_loader_for_entry(entry_path);
        let linked = self
            .module_graph
            .load_program(&loader, entry_path)
            .map_err(map_graph_error)?;
        for metadata in &linked.metadata {
            self.source_maps.record_compiled_metadata(metadata);
        }
        self.register_resolved_exports(&linked.metadata);
        Ok(())
    }

    /// Run a file from disk, detecting script vs module shape.
    ///
    /// # Errors
    /// See [`OtterError`] variants.
    pub fn run_file(&mut self, path: impl AsRef<Path>) -> Result<ExecutionResult, OtterError> {
        self.interp.begin_jit_debug_capture();
        self.run_file_inner(path)
            .map(|result| self.attach_jit_debug_report(result))
    }

    /// Run a file while retaining partial JIT diagnostics on abrupt failure.
    pub fn run_file_with_diagnostics(&mut self, path: impl AsRef<Path>) -> ExecutionAttempt {
        let result = self.run_file(path);
        self.finish_jit_debug_attempt(result)
    }

    /// Drain microtasks, offering catchable uncaught throws to `process` and
    /// an active domain. A handled throw permits the remaining tasks to run.
    /// Fatal or escaping OOM stops this checkpoint; queue-owned unexecuted jobs
    /// remain available to a later deliberate direct Runtime turn.
    pub(super) fn drain_microtasks_dispatching_uncaught(
        &mut self,
    ) -> Result<(), otter_vm::RunError> {
        self.interp
            .drain_microtasks(|ctx, _error| uncaught::dispatch(ctx))
    }

    /// Offer the in-flight uncaught throw to `process`, the way Node does: the
    /// capture callback installed by `setUncaughtExceptionCaptureCallback`
    /// first, then `uncaughtException` listeners. Answers whether JavaScript
    /// took ownership of the throw; when it did not, the pending value is put
    /// back so the diagnostic still reports the original error.
    ///
    /// # Errors
    /// Returns the error raised by the handler itself — a throw inside the
    /// handler is not caught again.
    pub(super) fn dispatch_uncaught_exception(
        &mut self,
        context: Option<&ExecutionContext>,
    ) -> Result<bool, OtterError> {
        let handled = otter_vm::NativeCtx::with_host_context(
            &mut self.interp,
            otter_vm::NativeCallInfo::default_call(),
            context,
            uncaught::dispatch,
        );
        match handled {
            Ok(true) => Ok(true),
            Ok(false) => Ok(false),
            // `process.exit` inside the handler is Node's documented way to
            // end the process from `uncaughtException` — a clean exit
            // request, not a handler failure.
            Err(error)
                if error.is_fatal()
                    || matches!(error, otter_vm::NativeError::OutOfMemory { .. }) =>
            {
                Err(map_native_error(error))
            }
            // A replaced, non-callable `process._fatalException` is Node's
            // "internal fatal exception handler failure" (exit code 6).
            Err(otter_vm::NativeError::Coded {
                code: "ERR_FATAL_HANDLER_INVALID",
                message,
                ..
            }) => Err(OtterError::Internal {
                code: "FATAL_HANDLER_INVALID".to_string(),
                message,
            }),
            // A throw escaping the `uncaughtException` handler is Node's
            // "internal fatal exception handler run-time failure": the
            // process dies with code 7 and the handler's own error.
            Err(error) => Err(OtterError::Internal {
                code: "UNCAUGHT_HANDLER_THREW".to_string(),
                message: format!("uncaught exception handler failed: {error}"),
            }),
        }
    }

    /// Install the CommonJS scope a `-e`/`-p` snippet runs in: the `[eval]`
    /// module's own `require`/`module`/`exports` on the global object, and a
    /// lazy global per builtin module.
    ///
    /// # Errors
    /// See [`OtterError`] variants.
    pub(crate) fn install_commonjs_eval_scope(&mut self, cwd: &Path) -> Result<(), OtterError> {
        let source = SourceInput::from_javascript(include_str!("eval_scope.js"));
        self.run_commonjs_file(&cwd.join("[eval]"), source)
            .map(|_| ())
    }

    /// How [`Self::run_file`] runs a source that is not an explicit module.
    ///
    /// The CommonJS wrapper compiles its body as JavaScript (through the
    /// eval/`new Function` path), so only JavaScript-kind sources route
    /// there; TypeScript CommonJS (`.cts`) runs as a script. CommonJS handles
    /// every script-shaped source: only a source that actually parses as a
    /// module takes the ESM path. A source that does not parse under the
    /// module grammar is CommonJS, since its body runs inside the wrapper
    /// function where constructs the module grammar rejects — a top-level
    /// `return`, chiefly — are legal, and compiling the same text there is
    /// also what reports a genuine syntax error.
    pub(crate) fn commonjs_routing(
        &self,
        path: &Path,
        source: &SourceInput,
        package_type: Option<module_loader::LoaderPackageType>,
    ) -> Result<CommonJsRouting, OtterError> {
        let commonjs_kind = matches!(
            source.kind,
            SourceKind::JavaScript | SourceKind::JavaScriptJsx
        );
        if !(self.config.commonjs_enabled && commonjs_kind) {
            return Ok(CommonJsRouting::Script);
        }
        if package_type == Some(module_loader::LoaderPackageType::CommonJs)
            || source_path_has_script_extension(path)
        {
            return Ok(CommonJsRouting::CommonJs);
        }
        let looks_module = match with_program(&source.text, source.kind, |program| {
            Ok::<bool, OtterError>(program_looks_like_module(program))
        }) {
            Ok(looks_module) => looks_module?,
            Err(_) => false,
        };
        Ok(if looks_module {
            CommonJsRouting::Module
        } else {
            CommonJsRouting::CommonJs
        })
    }
}
