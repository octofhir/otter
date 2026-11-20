//! Loader, module graph, source maps and diagnostic state owned by a runtime session.
//!
//! # Contents
//! - Entry-aware resolver configuration, module linking and source-map publication.
//!
//! # Invariants
//! - A runtime session owns the loader and source mappings used by its executions.
//!
//! # See also
//! - [`crate::module_graph`] builds the linked modules.

use crate::*;

#[derive(Debug, Clone)]
pub(super) struct RuntimeModuleLoaderState {
    pub(super) configured: Option<module_loader::LoaderConfig>,
    /// `--expose-internals`, which decides the builtins an `import` reaches.
    expose_internals: bool,
}

impl RuntimeModuleLoaderState {
    pub(super) fn new(
        configured: Option<module_loader::LoaderConfig>,
        expose_internals: bool,
    ) -> Self {
        Self {
            configured,
            expose_internals,
        }
    }

    pub(super) fn for_entry(
        &self,
        entry_path: &Path,
        hosted_modules: &[HostedModule],
        capabilities: &CapabilitySet,
        hooks: &RuntimeHooks,
        resource_account: &ResourceAccount,
    ) -> module_loader::ModuleLoader {
        let base_dir = entry_path
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
        self.for_base_dir(
            base_dir,
            hosted_modules,
            capabilities,
            hooks,
            resource_account,
        )
    }

    /// A loader whose referrer-less requests resolve from `base_dir`, unless
    /// the embedder configured its own.
    pub(super) fn for_base_dir(
        &self,
        base_dir: PathBuf,
        hosted_modules: &[HostedModule],
        capabilities: &CapabilitySet,
        hooks: &RuntimeHooks,
        resource_account: &ResourceAccount,
    ) -> module_loader::ModuleLoader {
        let mut cfg = match &self.configured {
            Some(cfg) => cfg.clone(),
            None => module_loader::LoaderConfig::new(base_dir),
        };
        // An `import` is always user code: the builtins' internal ids name no
        // module for it, unless `--expose-internals` opened them.
        cfg.hosted_specifiers.extend(
            hosted_modules
                .iter()
                .copied()
                .filter(|module| {
                    module.supports_esm()
                        && commonjs::user_may_load(module.specifier(), self.expose_internals)
                })
                .map(|module| module.specifier().to_string()),
        );
        // Keep module resolution on the same capability boundary as every
        // other host API. Hooks receive complete URL + initiator data while
        // the default policy remains the configured host allowlist.
        cfg.capabilities = capabilities.clone();
        cfg.capability_hooks = hooks.clone();
        cfg.resource_account = resource_account.clone();
        module_loader::ModuleLoader::with_config(cfg)
    }
}

/// What decides the heap a bootstrap builds: this executable's build and
/// the API surface, not the run. A startup image restores only for a
/// configuration with the same fingerprint as the one that wrote it; an
/// executable with no linker build identity has none.
pub(super) fn surface_fingerprint(config: &RuntimeConfig) -> Option<u64> {
    let build = otter_vm::startup_image::build_identity()?;
    let mut surface = String::new();
    for byte in build {
        surface.push_str(&format!("{byte:02x}"));
    }
    surface.push('|');
    for extension in &config.extensions {
        surface.push_str(extension.name);
        surface.push(',');
    }
    surface.push_str(&format!(
        "|{} classes|{} realm installers|{} hosted|commonjs {}|process {}|worker {}|gc {}|{}",
        config.global_classes.len(),
        config.realm_installers.len(),
        config.hosted_modules.len(),
        config.commonjs_enabled,
        config.install_process_global,
        config.install_worker_global,
        config.expose_gc,
        env!("CARGO_PKG_VERSION"),
    ));
    Some(
        surface
            .bytes()
            .fold(0xcbf2_9ce4_8422_2325_u64, |hash, byte| {
                (hash ^ u64::from(byte)).wrapping_mul(0x0100_0000_01b3)
            }),
    )
}

/// The one CommonJS configuration every `require` of a runtime shares, so
/// they resolve through one loader and its file-system cache.
pub(super) fn commonjs_config(
    config: &RuntimeConfig,
    module_loader: &RuntimeModuleLoaderState,
    runtime_task_spawner: Option<RuntimeTaskSpawner>,
) -> Arc<commonjs::CjsConfig> {
    Arc::new(commonjs::CjsConfig {
        capabilities: config.capabilities.clone(),
        hosted: config.hosted_modules.clone(),
        runtime_task_spawner,
        addon_loader: config.commonjs_addon_loader,
        report_watch_dependencies: commonjs::watch_reporting_requested(),
        expose_internals: config.expose_internals,
        resolver: module_loader.for_base_dir(
            config.process_cwd.clone(),
            &config.hosted_modules,
            &config.capabilities,
            &config.hooks,
            &config.resource_account,
        ),
    })
}

#[derive(Debug, Default)]
pub(super) struct RuntimeModuleGraphState {
    pub(super) last_entry_url: Option<String>,
    pub(super) last_module_count: usize,
}

impl RuntimeModuleGraphState {
    pub(super) fn record_linked(&mut self, linked: &module_graph::LinkedProgram) {
        self.last_entry_url = Some(linked.entry_url.clone());
        self.last_module_count = linked.module.module_inits.len();
    }

    pub(super) fn load_program(
        &mut self,
        loader: &module_loader::ModuleLoader,
        entry_path: &Path,
    ) -> Result<module_graph::LinkedProgram, module_graph::GraphError> {
        let linked = module_graph::load_program(loader, entry_path)?;
        self.record_linked(&linked);
        Ok(linked)
    }

    pub(super) fn load_imported_module(
        &mut self,
        loader: &module_loader::ModuleLoader,
        entry_path: &Path,
    ) -> Result<module_graph::LinkedProgram, module_graph::GraphError> {
        let linked = module_graph::load_imported_module(loader, entry_path)?;
        self.record_linked(&linked);
        Ok(linked)
    }

    pub(super) fn load_program_profiled(
        &mut self,
        loader: &module_loader::ModuleLoader,
        entry_path: &Path,
    ) -> Result<
        (
            module_graph::LinkedProgram,
            module_graph::ModulePhaseTimings,
        ),
        module_graph::GraphError,
    > {
        let (linked, timings) = module_graph::load_program_profiled(loader, entry_path)?;
        self.record_linked(&linked);
        Ok((linked, timings))
    }

    pub(super) fn load_program_interruptible(
        &mut self,
        loader: &module_loader::ModuleLoader,
        entry_path: &Path,
        interrupt: InterruptFlag,
    ) -> Result<module_graph::LinkedProgram, module_graph::GraphError> {
        let linked = module_graph::load_program_interruptible(loader, entry_path, interrupt)?;
        self.record_linked(&linked);
        Ok(linked)
    }

    pub(super) fn load_program_source_interruptible(
        &mut self,
        loader: &module_loader::ModuleLoader,
        entry: module_loader::ResolvedSource,
        interrupt: InterruptFlag,
    ) -> Result<module_graph::LinkedProgram, module_graph::GraphError> {
        let linked = module_graph::load_program_source_interruptible(loader, entry, interrupt)?;
        self.record_linked(&linked);
        Ok(linked)
    }
}

/// Per-runtime source-map table.
///
/// Replaced per source by [`Self::record_compiled_metadata`] every time the
/// runtime compiles a module / script. Each [`CompiledFunctionSpans`] group is
/// keyed by `(module_url, function_id)`
/// because PC namespaces are per-function (two functions in the
/// same source file both start at `pc = 0`).
///
/// The PC vectors are kept sorted by ascending `pc` so
/// [`Self::resolve_frame_span`] can binary-search for the
/// predecessor entry matching a live frame's PC. Recompilation replaces the
/// complete record for each affected URL; appending a second generation would
/// both retain stale functions and break the sorted-PC invariant.
///
/// # See also
/// - [`otter_vm::snapshot_frames`] — the in-VM counterpart that
///   reads [`otter_bytecode::Function::spans`] directly.
#[derive(Debug, Default)]
pub(super) struct RuntimeSourceMapTable {
    pub(super) by_module_url: RefCell<BTreeMap<String, BTreeMap<u32, Vec<SpanEntry>>>>,
}

impl RuntimeSourceMapTable {
    pub(super) fn record_compiled_metadata(&self, metadata: &CompiledModuleMetadata) {
        let mut replacements: BTreeMap<String, BTreeMap<u32, Vec<SpanEntry>>> = BTreeMap::new();
        if !metadata.source_url.is_empty() {
            // Even metadata with no spans is authoritative: recompiling an
            // empty source must clear functions retained from its predecessor.
            replacements.entry(metadata.source_url.clone()).or_default();
        }
        for function in &metadata.function_spans {
            replacements
                .entry(function.module_url_in(&metadata.source_url).to_owned())
                .or_default()
                .entry(function.function_id)
                .or_default()
                .extend_from_slice(&function.spans);
        }
        for functions in replacements.values_mut() {
            for spans in functions.values_mut() {
                spans.sort_unstable_by_key(|entry| entry.pc);
                // One PC has one current source range. Defensive replacement
                // keeps malformed/duplicated metadata deterministic.
                spans.dedup_by_key(|entry| entry.pc);
            }
        }

        let mut by_module_url = self.by_module_url.borrow_mut();
        for (module_url, functions) in replacements {
            by_module_url.insert(module_url, functions);
        }
    }

    /// Map a `(module_url, function_id, pc)` triple back to the
    /// original source byte range.
    ///
    /// Returns the span for the **predecessor entry** — the
    /// largest `entry.pc <= pc` — to mirror
    /// [`otter_vm::snapshot_frames`]'s lookup when the table has
    /// no exact PC match. Returns `None` when the module URL
    /// has no compiled metadata, when the function id is not in
    /// the module, or when the function's span table is empty.
    pub(super) fn resolve_frame_span(
        &self,
        module_url: &str,
        function_id: u32,
        pc: u32,
    ) -> Option<(u32, u32)> {
        let by_module_url = self.by_module_url.borrow();
        let by_function = by_module_url.get(module_url)?;
        let spans = by_function.get(&function_id)?;
        if spans.is_empty() {
            return None;
        }
        let idx = spans.partition_point(|s| s.pc <= pc);
        if idx == 0 {
            spans.first().map(|s| s.span)
        } else {
            Some(spans[idx - 1].span)
        }
    }

    #[cfg(test)]
    pub(super) fn contains_module(&self, module_url: &str) -> bool {
        self.by_module_url.borrow().contains_key(module_url)
    }
}

#[derive(Debug, Default)]
pub(super) struct RuntimeDiagnosticsSink {
    emitted: RefCell<Vec<Diagnostic>>,
}

impl RuntimeDiagnosticsSink {
    pub(super) fn emit(&self, hooks: &RuntimeHooks, diagnostic: &Diagnostic) {
        self.emitted.borrow_mut().push(diagnostic.clone());
        if let Some(hook) = hooks.diagnostic_hook() {
            hook.emit_diagnostic(diagnostic);
        }
    }
}
