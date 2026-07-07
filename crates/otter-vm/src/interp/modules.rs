//! Module-graph runtime state: init, envs, namespaces, dynamic import.
//!
//! # Contents
//! `run_module_init`/`run_module_init_hoist`, dynamic-import settlement,
//! module environment/namespace registries, and cross-module binding
//! resolution.
//!
//! # Invariants
//! Namespace objects are created lazily and cached per URL; module
//! environments registered here are GC roots via the trace surface.
#![allow(unused_imports)]
use crate::*;

impl Interpreter {
    /// Settle a pending dynamic-import promise registered under
    /// `token`. Routes through the standard promise dispatch path
    /// so reactions land on the per-isolate microtask queue;
    /// callers are expected to drain microtasks after calling
    /// this. A missing or already-settled token is a silent no-op.
    pub fn settle_dynamic_import(
        &mut self,
        token: u64,
        outcome: Result<Value, Value>,
    ) -> Option<ExecutionContext> {
        // Host-settlement entry point: runs outside any rooted VM
        // scope, and settling can allocate reaction records — root
        // the runtime state for the duration.
        let extra_roots = otter_gc::ExtraRoots::new(self as &Interpreter);
        let extra_root_depth = self.gc_heap.push_extra_roots(extra_roots);
        let settled = self.settle_dynamic_import_inner(token, outcome);
        self.gc_heap.pop_extra_roots_to(extra_root_depth - 1);
        settled
    }

    pub(crate) fn settle_dynamic_import_inner(
        &mut self,
        token: u64,
        outcome: Result<Value, Value>,
    ) -> Option<ExecutionContext> {
        let entry = self.dynamic_import_registry.take(token)?;
        let jobs = match outcome {
            Ok(value) => crate::JsPromise::fulfill(&entry.promise, &mut self.gc_heap, value),
            Err(reason) => crate::JsPromise::reject(&entry.promise, &mut self.gc_heap, reason),
        };
        for j in jobs.jobs {
            self.microtasks.enqueue(j);
        }
        Some(entry.context)
    }

    /// Run one dynamically loaded module init to completion or first
    /// suspension. Mirrors `run_inner`'s entry wiring: a top-level-await
    /// module body compiles to an async `<main>`, which needs an async
    /// result promise before `Op::Await` can park the frame (§16.2.1.9
    /// ExecuteAsyncModule). Returns that promise for async inits so the
    /// dynamic-import machinery can defer settlement until the module
    /// body actually finishes; sync inits return `None` after running
    /// to completion.
    ///
    /// # Errors
    /// Propagates any `VmError` thrown synchronously by the init body.
    pub fn run_module_init(
        &mut self,
        context: &ExecutionContext,
        function_id: u32,
        env: Value,
        import_meta: Value,
    ) -> Result<Option<crate::promise::JsPromiseHandle>, VmError> {
        self.run_module_init_phase(context, function_id, env, import_meta, false)
    }

    /// Link-phase init invocation — runs only the §16.2.1.7
    /// InitializeEnvironment prologue (export TDZ slots + hoisted
    /// function instantiation) and returns before any body statement.
    pub fn run_module_init_hoist(
        &mut self,
        context: &ExecutionContext,
        function_id: u32,
        env: Value,
        import_meta: Value,
    ) -> Result<(), VmError> {
        self.run_module_init_phase(context, function_id, env, import_meta, true)
            .map(|_| ())
    }

    pub(crate) fn run_module_init_phase(
        &mut self,
        context: &ExecutionContext,
        function_id: u32,
        env: Value,
        import_meta: Value,
        hoist_phase: bool,
    ) -> Result<Option<crate::promise::JsPromiseHandle>, VmError> {
        self.enter_sync_reentry()?;
        let extra_roots = otter_gc::ExtraRoots::new(self as &Interpreter);
        let extra_root_depth = self.gc_heap.push_extra_roots(extra_roots);
        let result =
            self.run_module_init_inner(context, function_id, env, import_meta, hoist_phase);
        self.gc_heap.pop_extra_roots_to(extra_root_depth - 1);
        self.leave_sync_reentry();
        result
    }

    pub(crate) fn run_module_init_inner(
        &mut self,
        context: &ExecutionContext,
        function_id: u32,
        env: Value,
        import_meta: Value,
        hoist_phase: bool,
    ) -> Result<Option<crate::promise::JsPromiseHandle>, VmError> {
        let function = context
            .exec_function(function_id)
            .ok_or_else(|| VmError::InvalidOperand)?;
        // The module environment record: link-phase and
        // evaluation-phase invocations share one persistent set of
        // own-upvalue cells so hoisted closures and the body bind the
        // same module-scope storage.
        let module_url: std::sync::Arc<str> = std::sync::Arc::from(function.module_url.as_ref());
        let upvalues = if let Some(cells) = self.module_init_upvalues.get(&module_url) {
            cells.clone()
        } else {
            let built = Frame::build_upvalues_for_exec(
                &mut self.gc_heap,
                function,
                Frame::empty_upvalues(),
            )?;
            self.module_init_upvalues.insert(module_url, built.clone());
            built
        };
        let mut frame =
            Frame::with_exec_return_upvalues_and_this(function, None, upvalues, Value::undefined());
        let args: SmallVec<[Value; 8]> =
            smallvec::smallvec![env, import_meta, Value::boolean(hoist_phase)];
        self.bind_bytecode_call_arguments(function, &mut frame, args)?;
        let mut stack: HoltStack = HoltStack::new();
        stack.push(frame);
        let init_promise = if function.is_async {
            let result = promise_dispatch::PromiseBuilder::with_context(context.clone())
                .pending_stack_rooted(self, &stack, &[&env, &import_meta], &[])?;
            stack
                .last_mut()
                .expect("init frame was just pushed")
                .async_state = Some(AsyncFrameState {
                result_promise: result,
            });
            Some(result)
        } else {
            None
        };
        self.dispatch_loop(context, &mut stack)?;
        Ok(init_promise)
    }

    /// Defer settlement of dynamic-import `token` until the gating
    /// async module-init promise settles — the target's init promise,
    /// pushed last by the evaluation DFS (spec `[[TopLevelCapability]]`
    /// shape, §16.2.1.9). A rejection rejects the import; fulfilment
    /// resolves it with the namespace registered for `namespace_url`.
    ///
    /// # Errors
    /// Returns `VmError` only for allocation failure while building
    /// the reaction callables.
    pub fn settle_dynamic_import_on_async_inits(
        &mut self,
        context: &ExecutionContext,
        token: u64,
        promises: Vec<crate::promise::JsPromiseHandle>,
        namespace_url: std::sync::Arc<str>,
    ) -> Result<(), VmError> {
        debug_assert!(!promises.is_empty());
        let Some(gate) = promises.last().copied() else {
            return Ok(());
        };
        let url = namespace_url;
        let on_fulfilled = crate::native_function::native_value_with_captures_unchecked_with_roots(
            &mut self.gc_heap,
            "dynamicImportInitFulfilled",
            SmallVec::new(),
            &mut |_visitor| {},
            move |ncx, _args, _captures| {
                let interp = ncx.interp_mut();
                let namespace = interp
                    .module_env(&url)
                    .map(Value::object)
                    .unwrap_or_else(Value::undefined);
                let _ = interp.settle_dynamic_import(token, Ok(namespace));
                Ok(Value::undefined())
            },
        )?;
        let on_rejected = crate::native_function::native_value_with_captures_unchecked_with_roots(
            &mut self.gc_heap,
            "dynamicImportInitRejected",
            SmallVec::new(),
            &mut |visitor| on_fulfilled.trace_value_slots(visitor),
            move |ncx, args, _captures| {
                let reason = args.first().copied().unwrap_or_else(Value::undefined);
                let _ = ncx.interp_mut().settle_dynamic_import(token, Err(reason));
                Ok(Value::undefined())
            },
        )?;
        let capability = promise_dispatch::PromiseBuilder::with_context(context.clone())
            .capability_runtime_rooted(self, &[&on_fulfilled, &on_rejected], &[])?;
        let outcome = crate::JsPromise::perform_then_with_context(
            &gate,
            &mut self.gc_heap,
            Some(on_fulfilled),
            Some(on_rejected),
            capability,
            Some(context.clone()),
        );
        if let Some(job) = outcome.immediate_job {
            self.microtasks.enqueue(job);
        }
        Ok(())
    }
}

impl Interpreter {
    /// Register or overwrite a module's `module_env` object so
    /// later [`Op::ImportNamespace`] dispatches can resolve
    /// references to it.
    ///
    /// Called by the runtime's module-graph driver as it walks
    /// the topological order — once a module's `<module-init>`
    /// has run and populated its env, the driver records it
    /// here keyed by canonical URL.
    pub fn register_module_env(&mut self, url: std::sync::Arc<str>, env: JsObject) {
        self.module_environments.insert(url, env);
    }

    /// Look up the cached namespace object for a host-installed builtin
    /// module specifier (e.g. `otter:kv`). The cache survives
    /// [`Self::reset_module_state`], so every loader (ESM graph, CommonJS
    /// `require`) observes the identical namespace for the isolate's life.
    #[must_use]
    pub fn host_module_env_cached(&self, specifier: &str) -> Option<JsObject> {
        self.host_module_env_cache.get(specifier).copied()
    }

    /// Record a freshly installed builtin-module namespace so later loads of
    /// `specifier` — from any loader, in any later program run — reuse it
    /// instead of re-running the installer.
    pub fn cache_host_module_env(&mut self, specifier: std::sync::Arc<str>, env: JsObject) {
        self.host_module_env_cache.insert(specifier, env);
    }

    /// Register a module's §16.2.1.6 ResolveExport table (exported name
    /// → `(defining_module, binding)`), computed by the linker. Read by
    /// the Module Namespace Exotic Object MOP forks and
    /// [`Op::LoadImportBinding`] so re-exported / star-exported names
    /// resolve to the defining module's live binding. Overwrites any
    /// prior table for `url`; cleared by [`Self::reset_module_state`].
    pub fn register_module_resolved_exports(
        &mut self,
        url: std::sync::Arc<str>,
        table: std::collections::BTreeMap<String, (std::sync::Arc<str>, String)>,
    ) {
        self.module_resolved_exports.insert(url, table);
    }

    /// Borrow a module's `module_env` JsObject by URL. Returns
    /// `None` when the URL is unknown — the runtime surfaces
    /// that as a catchable diagnostic upstream rather than
    /// silently filling with `undefined`.
    #[must_use]
    pub fn module_env(&self, url: &str) -> Option<JsObject> {
        self.module_environments.get(url).cloned()
    }

    /// Drop every recorded module environment + resolution
    /// cache entry. Called between top-level `run` invocations
    /// on the same interpreter so a fresh script never observes
    /// stale modules.
    pub fn reset_module_state(&mut self) {
        self.module_environments.clear();
        self.module_init_upvalues.clear();
        self.module_hoisted.clear();
        self.module_resolution_cache.clear();
        self.module_records.clear();
        self.next_module_async_order = 0;
        self.deferred_namespaces.clear();
        self.module_namespaces.clear();
        self.module_resolved_exports.clear();
    }

    /// Resolve a specifier seen by the running module to the
    /// target module's `module_env`. Returns `None` when the
    /// linker did not register a resolution for the
    /// `(referrer, specifier)` pair, or when the resolution
    /// pointed at a URL that no `module_env` has been recorded
    /// for yet.
    ///
    /// # Algorithm
    /// 1. Look in `module_resolution_cache` keyed by
    ///    `(referrer, specifier)`. Fast path: pre-built entry,
    ///    one hashmap probe.
    /// 2. On miss, scan
    ///    [`otter_bytecode::BytecodeModule::module_resolutions`]
    ///    for the matching triple, populate the cache, return.
    /// 3. With the resolved target URL in hand, look up the
    ///    `module_env` in `module_environments`.
    ///
    /// # Invariants
    /// - `module_resolutions` is small (one entry per actual
    ///   import edge in the graph), so the linear scan on
    ///   miss is cheap. Real engines reach for a hashmap;
    ///   the foundation prefers a flat vector that round-trips
    ///   cleanly through the bytecode dump.
    pub(crate) fn resolve_module_namespace(
        &mut self,
        context: &ExecutionContext,
        referrer: &str,
        specifier: &str,
    ) -> Option<JsObject> {
        let referrer_rc: std::sync::Arc<str> = std::sync::Arc::from(referrer);
        let key = (referrer_rc.clone(), specifier.to_string());
        let target_url = if let Some(hit) = self.module_resolution_cache.get(&key) {
            hit.clone()
        } else {
            let target = context.module_resolution_target(referrer, specifier)?;
            let target_rc: std::sync::Arc<str> = std::sync::Arc::from(target);
            self.module_resolution_cache.insert(key, target_rc.clone());
            target_rc
        };
        self.module_environments.get(target_url.as_ref()).cloned()
    }

    /// Resolve `(referrer, specifier)` to the eager Module Namespace
    /// Exotic Object (§10.4.6) — used by the user-visible `import * as
    /// ns` binding and `export * as ns`, distinct from the raw module
    /// environment used for named-import indirection.
    pub(crate) fn resolve_module_namespace_object(
        &mut self,
        context: &ExecutionContext,
        referrer: &str,
        specifier: &str,
    ) -> Option<JsObject> {
        let referrer_rc: std::sync::Arc<str> = std::sync::Arc::from(referrer);
        let key = (referrer_rc, specifier.to_string());
        let target_url = if let Some(hit) = self.module_resolution_cache.get(&key) {
            hit.clone()
        } else {
            let target = context.module_resolution_target(referrer, specifier)?;
            let target_rc: std::sync::Arc<str> = std::sync::Arc::from(target);
            self.module_resolution_cache.insert(key, target_rc.clone());
            target_rc
        };
        self.get_or_create_module_namespace(target_url.as_ref())
    }

    /// Eager Module Namespace Exotic Object (§10.4.6) wrapping the
    /// environment of `target_url`, created on first use and cached so
    /// every `import * as ns` / re-export of the same module yields the
    /// identical object.
    pub(crate) fn get_or_create_module_namespace(&mut self, target_url: &str) -> Option<JsObject> {
        let target_rc: std::sync::Arc<str> = std::sync::Arc::from(target_url);
        if let Some(ns) = self.module_namespaces.get(&target_rc) {
            return Some(*ns);
        }
        let env = *self.module_environments.get(&target_rc)?;
        let ns = self
            .alloc_module_namespace_object(env, target_rc.clone())
            .ok()?;
        self.module_namespaces.insert(target_rc, ns);
        Some(ns)
    }

    /// §10.4.6 namespace string-key resolution. Resolves `name` through
    /// `ns_obj`'s module §16.2.1.6 ResolveExport table to the live
    /// binding value. Returns `Some(value)` when `name` is an exported
    /// binding — the value may be the TDZ hole, which the caller maps to
    /// a `ReferenceError` (§10.4.6.8 step 9). Returns `None` when `name`
    /// is not exported. A re-exported / star-exported name resolves to
    /// the *defining* module's live environment, not a snapshot. The
    /// `"*namespace*"` binding (`export * as ns`) resolves to the
    /// defining module's namespace object. Unmodeled (host) modules with
    /// no table fall back to reading the wrapped environment directly.
    pub(crate) fn module_namespace_get_binding(
        &mut self,
        ns_obj: JsObject,
        name: &str,
    ) -> Option<Value> {
        let url = crate::object::module_namespace_url(ns_obj, &self.gc_heap)?;
        self.resolve_module_binding(&url, name)
    }

    /// §16.2.1.6 ResolveExport + §9.1.1.5 GetBindingValue for one
    /// `(module_url, exported name)` pair. Returns the defining module's
    /// live binding value (possibly the TDZ hole), the defining module's
    /// namespace object for the `"*namespace*"` sentinel, or `None` when
    /// the name is not exported. Backs both the namespace MOP forks and
    /// [`Op::LoadImportBinding`]. Unmodeled (host) modules with no table
    /// read their environment directly by name.
    pub(crate) fn resolve_module_binding(&mut self, module_url: &str, name: &str) -> Option<Value> {
        if let Some(table) = self.module_resolved_exports.get(module_url) {
            let (defmod, binding) = table.get(name)?.clone();
            if binding == "*namespace*" {
                return self
                    .get_or_create_module_namespace(&defmod)
                    .map(Value::object);
            }
            if binding == "*deferred-namespace*" {
                return self
                    .get_or_create_deferred_namespace(defmod)
                    .ok()
                    .map(Value::object);
            }
            let env = *self.module_environments.get(&defmod)?;
            return Some(
                crate::object::get(env, &self.gc_heap, &binding).unwrap_or_else(Value::hole),
            );
        }
        let env = *self.module_environments.get(module_url)?;
        crate::object::get(env, &self.gc_heap, name)
    }

    /// Exported string names a namespace exposes — its ResolveExport
    /// table keys (already ascending), or the wrapped env keys for
    /// unmodeled (host) modules. Used by the namespace `[[HasProperty]]`
    /// and `[[OwnPropertyKeys]]` MOP forks.
    pub(crate) fn module_namespace_export_names(&self, ns_obj: JsObject) -> Vec<String> {
        let Some(url) = crate::object::module_namespace_url(ns_obj, &self.gc_heap) else {
            return Vec::new();
        };
        if let Some(table) = self.module_resolved_exports.get(&url) {
            return table.keys().cloned().collect();
        }
        match crate::object::module_namespace_env(ns_obj, &self.gc_heap) {
            Some(env) => crate::object::module_namespace_sorted_string_keys(env, &self.gc_heap),
            None => Vec::new(),
        }
    }
}
