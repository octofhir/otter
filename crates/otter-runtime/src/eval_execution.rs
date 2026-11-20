//! Evaluation, microtask checkpoints and bootstrap script compilation.
//!
//! # Contents
//! Eval, microtask dispatch, code caching and compile hooks.
//!
//! # Invariants
//! Compilation uses the active session hooks and source metadata.
//!
//! # See also
//! - [`crate::Runtime`] owns the isolate session.

use crate::*;

impl Runtime {
    /// Drain the microtask queue manually. Embedders that want to
    /// step the queue between script runs (or in response to
    /// host-side events) call this directly. Foundation slices
    /// run `run_script` / `eval` always drain after the script,
    /// so manual draining is rarely needed today.
    ///
    /// # Errors
    /// Any `VmError` raised by a microtask propagates as
    /// `OtterError::Runtime`.
    pub fn run_microtasks(&mut self) -> Result<(), OtterError> {
        self.with_direct_timeout(Self::run_microtasks_unbounded)
    }

    pub(super) fn run_microtasks_unbounded(&mut self) -> Result<(), OtterError> {
        if self.interp.microtasks().has_any_pending() {
            // The host loop drains between turns, and a task that throws here
            // is as uncaught as one thrown during evaluation: it gets the same
            // offer to `process` and to any active domain.
            self.drain_microtasks_dispatching_uncaught()
                .map_err(map_vm_error)?;
        }
        // An `import()` made by a timer or an event the host dispatched is
        // owed the same pump a script turn gives it.
        if !self
            .layer_a_dynamic_imports
            .lock()
            .expect("layer-a dynamic import queue poisoned")
            .is_empty()
        {
            self.pump_layer_a_dynamic_imports()?;
        }
        Ok(())
    }

    /// Convenience: run an expression for tooling.
    ///
    /// # Errors
    /// See [`OtterError`] variants.
    pub fn eval(&mut self, source: SourceInput) -> Result<ExecutionResult, OtterError> {
        self.run_script(source, "<eval>")
    }

    /// Run a classic script and hand its completion value to `with_value`.
    ///
    /// [`ExecutionResult`] renders the completion to a `String`, which is
    /// enough for a CLI and useless to an embedder that wants the object
    /// back. This runs the same path — same realm, same `globalThis`,
    /// microtask checkpoint, dynamic-import pump — and then opens a native
    /// context so the caller can read the value with the ordinary
    /// [`otter_vm::NativeCtx`] API.
    ///
    /// The value is rooted across the checkpoint and the import pump, so it
    /// survives the young-generation moves those cause. Inside `with_value`
    /// the ordinary rule applies: it is a raw handle, valid only until the
    /// first allocation. Park it with `ctx.scope` before allocating.
    ///
    /// # Errors
    /// See [`OtterError`] variants.
    pub fn eval_value<R>(
        &mut self,
        source: SourceInput,
        specifier: &str,
        with_value: impl FnOnce(&mut otter_vm::NativeCtx<'_>, otter_vm::Value) -> R,
    ) -> Result<R, OtterError> {
        self.with_direct_timeout(move |runtime| {
            runtime.eval_value_unbounded(source, specifier, with_value)
        })
    }

    pub(super) fn eval_value_unbounded<R>(
        &mut self,
        source: SourceInput,
        specifier: &str,
        with_value: impl FnOnce(&mut otter_vm::NativeCtx<'_>, otter_vm::Value) -> R,
    ) -> Result<R, OtterError> {
        let compiled = self.compile_source(&source, specifier)?;
        let (module, sources) = self.prepare_script_source(compiled.bytecode, source, specifier)?;
        let context = self.interp.link_module(module, sources)?;
        let value = match self.interp.run(&context) {
            Ok(value) => value,
            Err(err) => {
                let mapped = map_vm_error(err);
                return Err(enrich_runtime_diagnostic_with_cause(
                    &mut self.interp,
                    mapped,
                ));
            }
        };
        // Both the checkpoint and the import pump allocate, and the young
        // generation is a moving collector, so the completion cannot stay in
        // a plain local across them.
        let root = self.interp.persistent_root_insert(value);
        let settled = match self.drain_microtasks_dispatching_uncaught() {
            Ok(()) => self.pump_layer_a_dynamic_imports(),
            Err(err) => {
                let mapped = map_vm_error(err);
                Err(enrich_runtime_diagnostic_with_cause(
                    &mut self.interp,
                    mapped,
                ))
            }
        };
        if let Err(err) = settled {
            self.interp.persistent_root_remove(root);
            return Err(err);
        }
        let global_value = otter_vm::Value::object(*self.interp.global_this());
        Ok(otter_vm::NativeCtx::with_host_context(
            &mut self.interp,
            otter_vm::NativeCallInfo::call(global_value),
            Some(&context),
            |ctx| {
                let value = ctx
                    .persistent_root_remove(root)
                    .expect("the completion root outlives the checkpoint");
                with_value(ctx, value)
            },
        ))
    }

    /// Evaluate a snippet while retaining partial JIT diagnostics on failure.
    pub fn eval_with_diagnostics(&mut self, source: SourceInput) -> ExecutionAttempt {
        let result = self.eval(source);
        self.finish_jit_debug_attempt(result)
    }

    /// Compile-only: parse + erase + lower without executing.
    ///
    /// # Errors
    /// See [`OtterError`] variants.
    pub fn check(&self, source: SourceInput, specifier: &str) -> Result<(), OtterError> {
        self.compile_source(&source, specifier).map(|_| ())
    }

    /// Compile-and-dump: produce the bytecode module for inspection. Every
    /// function body is compiled, where a run would leave the bodies of
    /// functions not yet called to their first call.
    ///
    /// # Errors
    /// See [`OtterError`] variants.
    pub fn dump(&self, source: SourceInput, specifier: &str) -> Result<CompiledModule, OtterError> {
        if self.config.hooks.compile_hook().is_some() || source.allow_top_level_await {
            return self.compile_source(&source, specifier);
        }
        otter_compiler::compile_script_source_to_module(&source.text, source.kind, specifier)
            .map_err(|err| map_compile_error(err, specifier))
    }

    /// Run one trusted bootstrap source under its own specifier, so a stack
    /// trace through a builtin names the extension it came from.
    pub(super) fn run_bootstrap_script(
        &mut self,
        name: &str,
        source: impl Into<String>,
    ) -> Result<ExecutionResult, OtterError> {
        let cache = if self.config.hooks.compile_hook().is_none() {
            compile_cache::CompileCache::user_default()
        } else {
            None
        };
        self.run_bootstrap_script_with_cache(name, source, cache)
    }

    pub(super) fn run_bootstrap_script_with_cache(
        &mut self,
        name: &str,
        source: impl Into<String>,
        cache: Option<compile_cache::CompileCache>,
    ) -> Result<ExecutionResult, OtterError> {
        self.run_bootstrap_script_with_cache_and_metadata_budget(
            name,
            source.into(),
            cache,
            otter_compiler::MAX_COMPILED_METADATA_BYTES,
        )
    }

    pub(super) fn run_bootstrap_script_with_cache_and_metadata_budget(
        &mut self,
        name: &str,
        source: String,
        cache: Option<compile_cache::CompileCache>,
        metadata_budget: usize,
    ) -> Result<ExecutionResult, OtterError> {
        let specifier = format!("<bootstrap:{name}>");
        let source = SourceInput::from_javascript(source);
        let start = std::time::Instant::now();
        // Bootstrap sources are byte-identical on every launch, which is what
        // makes them worth caching. Ordinary script evaluation is deliberately
        // left alone: its sources are one-offs, and an entry per snippet is a
        // directory that grows without ever being read again.
        // A compile hook defines a different compiler contract whose identity
        // is not the embedded default-compiler fingerprint. It must neither
        // consume a shared default entry nor publish hook-produced bytecode
        // under that key.
        let cache = if self.config.hooks.compile_hook().is_none() {
            cache
        } else {
            None
        };
        let key = cache
            .as_ref()
            .map(|_| compile_cache::cache_key(&source.text, source.kind, &specifier));
        if let (Some(cache), Some(key)) = (&cache, &key)
            && let Some(bytecode) = cache.load(key, &source.text)
            && let Ok(metadata) = CompiledModuleMetadata::span_only_from_bytecode_with_budget(
                bytecode.module(),
                metadata_budget,
            )
        {
            self.source_maps.record_compiled_metadata(&metadata);
            let text =
                module_loader::admit_source(&self.config.resource_account, &specifier, source.text)
                    .map_err(module_loader::LoaderError::into_otter_error)?;
            let sources = script_source::script_sources(
                bytecode.module(),
                text,
                &specifier,
                &self.config.resource_account,
            )?;
            return self.run_bootstrap_module(Ok(bytecode), sources, start);
        }
        let compiled = self.compile_source(&source, &specifier)?;
        if let (Some(cache), Some(key)) = (&cache, &key) {
            cache.store(key, &compiled.bytecode);
        }
        let (module, sources) =
            self.prepare_script_source(compiled.bytecode, source, &specifier)?;
        self.run_bootstrap_module(Err(module), sources, start)
    }

    /// Count a compile made away from this runtime and record its source
    /// maps, as [`Self::compile_source`] does for its own.
    pub(crate) fn note_script_compiled(&self, compiled: &CompiledModule) {
        self.scripts_compiled.set(self.scripts_compiled.get() + 1);
        self.source_maps
            .record_compiled_metadata(&compiled.metadata);
    }

    pub(super) fn compile_source(
        &self,
        source: &SourceInput,
        specifier: &str,
    ) -> Result<CompiledModule, OtterError> {
        self.scripts_compiled.set(self.scripts_compiled.get() + 1);
        let compiled = if let Some(hook) = self.config.hooks.compile_hook() {
            let text = module_loader::admit_source(
                &self.config.resource_account,
                specifier,
                source.text.clone(),
            )
            .map_err(module_loader::LoaderError::into_otter_error)?;
            let resolved = module_loader::ResolvedSource {
                url: specifier.to_string(),
                kind: source.kind,
                jsx: None,
                text,
            };
            hook.compile(RuntimeCompileRequest { source: &resolved })?
        } else {
            compile_classic_script(source, specifier)?
        };
        self.source_maps
            .record_compiled_metadata(&compiled.metadata);
        Ok(compiled)
    }
}
