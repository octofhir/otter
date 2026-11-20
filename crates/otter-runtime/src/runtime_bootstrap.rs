//! Runtime configuration validation, bootstrap and image restoration.
//!
//! # Contents
//! Creation and restoration of the VM and its realm services.
//!
//! # Invariants
//! Configuration is validated before the session is published.
//!
//! # See also
//! - [`crate::Runtime`] owns the isolate session.

use crate::*;

impl Runtime {
    pub(crate) fn validate_config(config: &RuntimeConfig) -> Result<(), OtterError> {
        if config.max_stack_depth == 0 {
            return Err(OtterError::Config {
                reason: ConfigError::InvalidStackDepth {
                    message: "max_stack_depth must be > 0".to_string(),
                },
            });
        }
        for (name, capacity) in [
            (
                "guaranteed completions",
                config.completion_capacities.guaranteed,
            ),
            (
                "host operations",
                config.completion_capacities.host_operations,
            ),
            ("timers", config.completion_capacities.timers),
        ] {
            if capacity > tokio::sync::Semaphore::MAX_PERMITS {
                return Err(OtterError::Config {
                    reason: ConfigError::InvalidCompletionCapacity {
                        message: format!(
                            "{name} capacity {capacity} exceeds the maximum {}",
                            tokio::sync::Semaphore::MAX_PERMITS
                        ),
                    },
                });
            }
        }
        // Hosted-module lookups take the first specifier match, so a
        // duplicate registration would silently shadow the later installer.
        // Surface the conflict at build time instead.
        let mut seen = std::collections::HashSet::new();
        for module in &config.hosted_modules {
            if !seen.insert(module.specifier()) {
                return Err(OtterError::HostedModule {
                    specifier: module.specifier().to_string(),
                    message: "registered more than once".to_string(),
                });
            }
        }
        Ok(())
    }

    pub(crate) fn from_config(config: RuntimeConfig) -> Result<Self, OtterError> {
        let admitted = Self::admit_config(config, RuntimeAdmissionKind::Direct)?;
        Self::from_config_with_task_spawner(admitted, None)
    }

    /// Validate `config`, then atomically reserve the complete role tuple
    /// before interpreter, event-loop, channel, or thread construction.
    pub(crate) fn admit_config(
        config: RuntimeConfig,
        kind: RuntimeAdmissionKind,
    ) -> Result<AdmittedRuntimeConfig, OtterError> {
        AdmittedRuntimeConfig::admit(config, kind)
    }

    /// Assemble a runtime around an isolate restored from `snapshot`,
    /// skipping the bootstrap and every surface installer: the image
    /// carries the fully-built realm, including the extension globals
    /// and the dynamic natives the installers would have created.
    ///
    /// In-process only — see
    /// [`otter_vm::Interpreter::from_isolate_snapshot`].
    ///
    /// # Errors
    /// Propagates config validation and image-restore failures.
    pub fn from_isolate_snapshot(snapshot: &RuntimeSnapshot) -> Result<Self, OtterError> {
        Self::from_isolate_snapshot_with(snapshot, SnapshotRuntimeOptions::default())
    }

    /// [`Self::from_isolate_snapshot`] with the per-isolate knobs a
    /// host actually varies per run. Everything else comes from the snapshot's
    /// donor configuration so captured native closures and runtime policy keep
    /// one authority. The snapshot donor account is mandatory.
    ///
    /// # Errors
    /// Propagates config validation and image-restore failures.
    pub fn from_isolate_snapshot_with(
        snapshot: &RuntimeSnapshot,
        options: SnapshotRuntimeOptions,
    ) -> Result<Self, OtterError> {
        let config = Self::snapshot_runtime_config(options, snapshot.donor_config.clone());
        let AdmittedRuntimeConfig {
            config,
            resource_leases,
            kind,
        } = Self::admit_config(config, RuntimeAdmissionKind::Direct)?;
        let mut runtime = Self::assemble_restored(&snapshot.isolate, config, None)?;
        runtime.attach_run_state(None)?;
        Ok(runtime.with_admission(resource_leases, kind))
    }

    pub(super) fn snapshot_runtime_config(
        options: SnapshotRuntimeOptions,
        mut donor_config: RuntimeConfig,
    ) -> RuntimeConfig {
        donor_config.timeout = options.timeout;
        donor_config.max_heap_bytes = options.max_heap_bytes;
        donor_config.allow_blocking_atomics_wait = options.allow_blocking_atomics_wait;
        donor_config.jit_selection = options.jit_selection;
        donor_config
    }

    /// Build a full runtime around a restored interpreter: the same
    /// host wiring `from_config` applies after bootstrap, minus
    /// everything that already lives in the image.
    pub(super) fn assemble_restored(
        snapshot: &otter_vm::snapshot::IsolateSnapshot,
        config: RuntimeConfig,
        runtime_task_spawner: Option<RuntimeTaskSpawner>,
    ) -> Result<Self, OtterError> {
        let module_loader =
            RuntimeModuleLoaderState::new(config.loader.clone(), config.expose_internals);
        let completion_pool = completion_admission::CompletionAdmissionPool::new(
            config.resource_account.clone(),
            config.completion_capacities,
        );
        let commonjs = commonjs_config(&config, &module_loader, runtime_task_spawner.clone());
        let mut interp = Interpreter::from_isolate_snapshot_capped(
            snapshot,
            config.max_heap_bytes,
            &config.resource_account,
        )
        .map_err(|err| match err {
            otter_gc::ImageError::OutOfMemory(oom) => string_oom_to_error(oom),
            otter_gc::ImageError::Resource(error) => OtterError::Resource { error },
            other => OtterError::Internal {
                code: DiagnosticCode::IsolateStart.as_str().to_string(),
                message: format!("snapshot restore failed: {other}"),
            },
        })?;
        interp.set_max_stack_depth(config.max_stack_depth);
        // Host services are host machinery, not heap state: a restored
        // isolate wires its run's own.
        interp.insert_host_service(commonjs::CommonJsHost(Arc::clone(&commonjs)));
        interp.set_allow_blocking_atomics_wait(config.allow_blocking_atomics_wait);
        interp.set_console_sink(config.console_sink.clone());
        // A restored isolate runs no bootstrap, so its configured per-turn
        // policy is in force from the first instruction it executes.
        interp.set_work_budget(config.work_budget);
        // §19.4.1 / §20.2.1.1 — the eval hook is host machinery, not
        // heap state, so a restored isolate wires it fresh.
        let code_cache_hook = hooks::CodeCacheSlot::default();
        interp.set_eval_hook(Some(standard_eval_hook(
            config.hooks.shared_compile_hook(),
            code_cache_hook.clone(),
        )));
        interp.set_lazy_compile_hook(Some(lazy_compile_hook()));
        if let Some(hook) = config.promise_rejection_hook.clone() {
            interp.set_promise_rejection_hook(hook);
        }
        let layer_a_dynamic_imports = LayerADynamicImportQueue::default();
        // Diagnostics are host machinery too: the image carries heap state,
        // while the donor configuration carries tracing and JIT-diagnostics
        // requests. Reinstall them without re-running any realm installer.
        interp.set_jit_debug_request(config.jit_debug);
        interp.set_jit_compiler_threads(config.jit_compile_threads);
        if config.gc_trace {
            let _ = interp
                .gc_heap_mut()
                .start_gc_pause_capture(GC_TRACE_RECORDS);
        }
        match config.jit_selection {
            JitSelection::ProductionTiered => {
                interp.set_jit_compiler(Some(std::sync::Arc::new(
                    otter_jit::OtterJitCompiler::production_tiered(),
                )));
            }
            JitSelection::Template => {
                interp.set_jit_compiler(Some(std::sync::Arc::new(
                    otter_jit::OtterJitCompiler::template_only(),
                )));
            }
            JitSelection::InterpreterOnly => {}
        }
        if let Some(factory) = &config.tracer_factory {
            interp.set_tracer(Some(factory.build()));
        }
        if let Some(interval) = config.cpu_profile_interval {
            interp.enable_cpu_profiler(interval);
        }
        interp.set_dynamic_import_loader(std::sync::Arc::new(LayerADynamicImportLoader {
            queue: layer_a_dynamic_imports.clone(),
            completion_pool,
        }));
        Ok(Runtime {
            interp,
            code_cache_hook,
            scripts_compiled: std::cell::Cell::new(0),
            restored_from_snapshot: true,
            realm_owner_id: realm::next_realm_owner_id(),
            config,
            enforce_direct_timeout: true,
            module_loader,
            module_graph: RuntimeModuleGraphState::default(),
            module_records: module_records::RuntimeModuleRecords::default(),
            source_maps: RuntimeSourceMapTable::default(),
            diagnostics: RuntimeDiagnosticsSink::default(),
            layer_a_dynamic_imports,
            web_dynamic_imports: Vec::new(),
            runtime_task_spawner,
            commonjs,
            pending_exit_code: None,
            worker_child_ready: false,
            admission_leases: None,
        })
    }

    pub(crate) fn from_config_with_task_spawner(
        admitted: AdmittedRuntimeConfig,
        runtime_task_spawner: Option<RuntimeTaskSpawner>,
    ) -> Result<Self, OtterError> {
        let AdmittedRuntimeConfig {
            config,
            resource_leases,
            kind,
        } = admitted;
        let image = config
            .startup_image
            .then(otter_vm::startup_image::StartupImage::of_this_executable)
            .flatten();
        let restored = match image {
            Some(image) => {
                Self::restore_startup_image(image, &config, runtime_task_spawner.clone())?
            }
            None => None,
        };
        let runtime = match restored {
            Some(runtime) => runtime,
            None => Self::build_from_config(config, runtime_task_spawner)?,
        };
        Ok(runtime.with_admission(resource_leases, kind))
    }

    /// A runtime restored from `image` for `config`, or `None` when the image
    /// was written for another API surface or does not read as one; the
    /// caller then bootstraps. The restored heap holds everything the
    /// bootstrap made; this run's own state — the `process` properties and
    /// every host service a static native reads — is installed afresh.
    pub(super) fn restore_startup_image(
        image: otter_vm::startup_image::StartupImage,
        config: &RuntimeConfig,
        runtime_task_spawner: Option<RuntimeTaskSpawner>,
    ) -> Result<Option<Self>, OtterError> {
        let mut input = image.reader();
        let Some(fingerprint) = surface_fingerprint(config) else {
            return Ok(None);
        };
        if input.u64() != Some(fingerprint) {
            return Ok(None);
        }
        let Some(snapshot) = image.snapshot(&mut input, &config.resource_account) else {
            return Ok(None);
        };
        let mut runtime =
            Self::assemble_restored(&snapshot, config.clone(), runtime_task_spawner.clone())?;
        runtime.enforce_direct_timeout = runtime_task_spawner.is_none();
        runtime.attach_run_state(runtime_task_spawner)?;
        Ok(Some(runtime))
    }

    /// Give a restored runtime this run's own state: the `process`
    /// properties and every host service a static native reads.
    pub(super) fn attach_run_state(
        &mut self,
        runtime_task_spawner: Option<RuntimeTaskSpawner>,
    ) -> Result<(), OtterError> {
        let runtime = self;
        let config = &runtime.config;
        if config.install_process_global {
            process::reattach_after_restore(
                &mut runtime.interp,
                config.process_argv.as_deref(),
                &config.process_exec_argv,
                &config.process_cwd,
                &config.process_env_overlay,
                &config.capabilities,
                &config.hooks,
                &config.warning_options,
                config.process_title.as_deref(),
                runtime_task_spawner.as_ref(),
                &runtime.commonjs,
            )?;
        }
        if runtime.config.install_worker_global {
            worker::insert_main_worker_host(runtime);
        }
        let realm_installers = runtime.config.realm_installers.clone();
        let capabilities = runtime.config.capabilities.clone();
        let hooks = runtime.config.hooks.clone();
        for installer in realm_installers {
            installer.install_run_services(
                &mut runtime.interp,
                &capabilities,
                &hooks,
                runtime_task_spawner.clone(),
            )?;
        }
        Ok(())
    }

    /// Build an isolate while its already-charged admission lease remains in
    /// the caller, so every bootstrap failure releases the slot exactly once.
    pub(super) fn build_from_config(
        config: RuntimeConfig,
        runtime_task_spawner: Option<RuntimeTaskSpawner>,
    ) -> Result<Self, OtterError> {
        let module_loader =
            RuntimeModuleLoaderState::new(config.loader.clone(), config.expose_internals);
        let completion_pool = completion_admission::CompletionAdmissionPool::new(
            config.resource_account.clone(),
            config.completion_capacities,
        );
        let commonjs = commonjs_config(&config, &module_loader, runtime_task_spawner.clone());
        let code_cache_hook = hooks::CodeCacheSlot::default();
        // The interpreter owns both per-isolate heaps; the string and GC
        // allocators honor the configured cap.
        let mut interp = Interpreter::with_string_heap_cap(config.max_heap_bytes)?;
        // Everything this build allocates — globals, prototypes, the objects
        // the bootstrap sources create — lives as long as the runtime does.
        // Allocating it young makes the first scavenges copy the whole set and
        // every later one re-trace it, for objects that were never going to
        // die, so the build runs tenured and the nursery starts empty.
        interp.gc_heap_mut().set_tenure_all(true);
        interp.set_max_stack_depth(config.max_stack_depth);
        interp.insert_host_service(commonjs::CommonJsHost(Arc::clone(&commonjs)));
        interp.set_allow_blocking_atomics_wait(config.allow_blocking_atomics_wait);
        interp.set_resource_account(config.resource_account.clone())?;
        interp.set_console_sink(config.console_sink.clone());
        if let Some(hook) = config.promise_rejection_hook.clone() {
            interp.set_promise_rejection_hook(hook);
        }
        let layer_a_dynamic_imports = LayerADynamicImportQueue::default();
        // Attached class glue and extension JS are deferred until the
        // runtime is fully assembled: the sources reference globals
        // installed later in this build, and evaluation needs the
        // compile pipeline. Both run in top-level activations whose scanned
        // registers retain every constructed value through collecting calls.
        let (pending_class_js, pending_extension_js) = interp.with_runtime_roots(
            |interp| -> Result<
                (
                    Vec<(&'static str, &'static str)>,
                    Vec<(&'static str, ExtensionJs)>,
                ),
                OtterError,
            > {
                let mut pending_class_js: Vec<(&'static str, &'static str)> = Vec::new();
                let mut pending_extension_js: Vec<(&'static str, ExtensionJs)> = Vec::new();
                for spec in &config.global_classes {
                    match spec.inner {
                        GlobalClassInner::Spec(raw) => {
                            interp.install_global_class(raw).map_err(OtterError::from)?;
                        }
                        GlobalClassInner::Intrinsic {
                            install,
                            install_well_knowns,
                            js_glue,
                            name,
                        } => {
                            if let Some(source) = js_glue {
                                pending_class_js.push((name, source));
                            }
                            let global = *interp.global_this();
                            install(interp.gc_heap_mut(), global).map_err(OtterError::from)?;
                            // Second phase, mirroring the bootstrap registry walk:
                            // symbol-keyed members (@@toStringTag) resolve against
                            // the realm's already-materialized well-known table.
                            interp
                                .run_install_well_knowns(install_well_knowns, global)
                                .map_err(OtterError::from)?;
                        }
                    }
                }
                // Declared extensions: native classes ride the identical
                // install path (declaration order, parents before subclasses),
                // then its one classic-script bundle runs after native installers.
                let extensions = config.extensions.clone();
                for extension in extensions {
                    for spec in extension.classes {
                        match spec.inner {
                            GlobalClassInner::Spec(raw) => {
                                interp.install_global_class(raw).map_err(OtterError::from)?;
                            }
                            GlobalClassInner::Intrinsic {
                                install,
                                install_well_knowns,
                                js_glue,
                                name,
                            } => {
                                if let Some(source) = js_glue {
                                    pending_class_js.push((name, source));
                                }
                                let global = *interp.global_this();
                                install(interp.gc_heap_mut(), global).map_err(OtterError::from)?;
                                interp
                                    .run_install_well_knowns(install_well_knowns, global)
                                    .map_err(OtterError::from)?;
                            }
                        }
                    }
                    if let Some(script) = extension.js {
                        pending_extension_js.push((extension.name, script));
                    }
                }
                if config.install_process_global {
                    process::install_global(
                        &mut *interp,
                        config.process_argv.as_deref(),
                        &config.process_exec_argv,
                        &config.process_cwd,
                        &config.process_env_overlay,
                        &config.capabilities,
                        &config.hooks,
                        &config.warning_options,
                        config.process_title.as_deref(),
                        runtime_task_spawner.as_ref(),
                        &commonjs,
                    )?;
                }
                if config.expose_gc {
                    install_expose_gc_global(&mut *interp)?;
                }
                // §19.4.1 / §20.2.1.1 — wire the eval hook so `eval(src)` /
                // `new Function(...)` reach a real parse + compile path.
                interp.set_eval_hook(Some(standard_eval_hook(
                    config.hooks.shared_compile_hook(),
                    code_cache_hook.clone(),
                )));
                interp.set_lazy_compile_hook(Some(lazy_compile_hook()));
                interp.set_jit_debug_request(config.jit_debug);
                if let Some(factory) = &config.tracer_factory {
                    interp.set_tracer(Some(factory.build()));
                }
                if let Some(interval) = config.cpu_profile_interval {
                    interp.enable_cpu_profiler(interval);
                }
                interp.set_dynamic_import_loader(std::sync::Arc::new(LayerADynamicImportLoader {
                    queue: layer_a_dynamic_imports.clone(),
                    completion_pool: completion_pool.clone(),
                }));
                Ok((pending_class_js, pending_extension_js))
            },
        )?;
        let enforce_direct_timeout = runtime_task_spawner.is_none();
        let mut runtime = Runtime {
            interp,
            code_cache_hook,
            scripts_compiled: std::cell::Cell::new(0),
            realm_owner_id: realm::next_realm_owner_id(),
            config,
            enforce_direct_timeout,
            module_loader,
            module_graph: RuntimeModuleGraphState::default(),
            module_records: module_records::RuntimeModuleRecords::default(),
            source_maps: RuntimeSourceMapTable::default(),
            diagnostics: RuntimeDiagnosticsSink::default(),
            layer_a_dynamic_imports,
            web_dynamic_imports: Vec::new(),
            runtime_task_spawner,
            commonjs,
            pending_exit_code: None,
            worker_child_ready: false,
            restored_from_snapshot: false,
            admission_leases: None,
        };
        if runtime.config.install_worker_global {
            worker::install_main_worker_globals(&mut runtime)?;
        }
        let realm_installers = runtime.config.realm_installers.clone();
        let capabilities = runtime.config.capabilities.clone();
        let hooks = runtime.config.hooks.clone();
        let runtime_task_spawner = runtime.runtime_task_spawner.clone();
        for installer in realm_installers {
            installer.install(
                &mut runtime.interp,
                &capabilities,
                &hooks,
                runtime_task_spawner.clone(),
            )?;
        }
        // Extension JS (Web platform shims, etc.) installs its globals
        // eagerly, in declaration order, after the native function
        // installers above so it can reference `self`/`structuredClone`
        // and before the class glue below so that glue sees the real
        // globals. Each source is a self-contained installer IIFE that
        // attaches to `globalThis`; evaluating here — at a top-level
        // frame — keeps every object it allocates rooted through GC.
        for (name, script) in pending_extension_js {
            let start = std::time::Instant::now();
            let specifier = format!("<bootstrap:{name}>");
            let installed = static_bootstrap::prepare(
                &mut runtime.interp,
                &runtime.config.hooks,
                &script,
                &specifier,
            )
            .and_then(|(context, metadata)| {
                runtime.source_maps.record_compiled_metadata(&metadata);
                runtime
                    .run_linked_script_outcome(context, start)
                    .map(|_| ())
            });
            installed.map_err(|err| OtterError::Internal {
                code: DiagnosticCode::GlobalClassBootstrap.as_str().to_string(),
                message: format!("extension `{name}` globals failed: {err}"),
            })?;
        }
        // Co-located class glue: the JS half of a declared class
        // installs in the same build as its native half, in class
        // declaration order, after every installer above — so the glue
        // can reference every installed extension global safely.
        for (name, source) in pending_class_js {
            runtime
                .run_bootstrap_script(name, source)
                .map_err(|err| OtterError::Internal {
                    code: DiagnosticCode::GlobalClassBootstrap.as_str().to_string(),
                    message: format!("class `{name}` attached JS glue failed: {err}"),
                })?;
        }
        // Fresh construction runs trusted installers before the user execution
        // tier policy is installed. Bootstrap therefore avoids compiler tier
        // decisions; calls made after the runtime is returned use the
        // configured compiler.
        runtime
            .interp
            .set_jit_compiler_threads(runtime.config.jit_compile_threads);
        if runtime.config.gc_trace {
            let _ = runtime
                .interp
                .gc_heap_mut()
                .start_gc_pause_capture(GC_TRACE_RECORDS);
        }
        match runtime.config.jit_selection {
            JitSelection::ProductionTiered => {
                runtime.interp.set_jit_compiler(Some(std::sync::Arc::new(
                    otter_jit::OtterJitCompiler::production_tiered(),
                )));
            }
            JitSelection::Template => {
                runtime.interp.set_jit_compiler(Some(std::sync::Arc::new(
                    otter_jit::OtterJitCompiler::template_only(),
                )));
            }
            JitSelection::InterpreterOnly => {}
        }
        // Bootstrap is over; user allocations go back through the nursery,
        // where most of them die.
        runtime.interp.gc_heap_mut().set_tenure_all(false);
        // The work policy governs the isolate's user execution, not its
        // construction: the builtin shims above are host-chosen work of a
        // fixed size, and metering them against a caller's slice limit
        // would reject the isolate instead of the script it was configured
        // for. The counters start from zero with it, so what an embedder
        // reads is its own workload no matter what bootstrap accounts for.
        let budget = runtime.config.work_budget;
        runtime.interp.reset_work_budget_stats();
        runtime.interp.set_work_budget(budget);
        Ok(runtime)
    }

    pub(super) fn with_admission(
        mut self,
        resource_leases: otter_resource::ResourceLeaseSet,
        kind: RuntimeAdmissionKind,
    ) -> Self {
        debug_assert!(self.admission_leases.is_none());
        debug_assert_eq!(resource_leases.amount(ResourceClass::Isolates), 1);
        debug_assert_eq!(
            resource_leases.amount(ResourceClass::Workers),
            if kind == RuntimeAdmissionKind::WorkerThread {
                1
            } else {
                0
            }
        );
        debug_assert_eq!(
            resource_leases.amount(ResourceClass::WorkerStackBytes),
            if kind == RuntimeAdmissionKind::Direct {
                0
            } else {
                admission::RUNTIME_THREAD_STACK_BYTES as u64
            }
        );
        self.admission_leases = Some(resource_leases);
        self
    }
}
