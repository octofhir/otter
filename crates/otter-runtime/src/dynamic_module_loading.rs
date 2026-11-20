//! Dynamic module loading and settlement in the requesting realm.
//!
//! # Contents
//! Resolution, linking and settlement in the default and additional realms.
//!
//! # Invariants
//! Completion is delivered to the realm that initiated the import.
//!
//! # See also
//! - [`crate::Runtime`] owns the isolate session.

use crate::*;

impl Runtime {
    /// Cancel one pending dynamic import when its admitted host completion can
    /// no longer be delivered. This removes the registry's GC root without
    /// running JavaScript in an already-finalized process.
    pub(crate) fn cancel_dynamic_import(&mut self, token: u64) -> bool {
        self.interp.cancel_dynamic_import(token)
    }

    /// Install the host-side dynamic-import scheduler. Wired by
    /// the isolate runner so `Op::ImportNamespaceDynamic` can
    /// reach the loader through the runtime inbox.
    pub fn install_dynamic_import_loader(&mut self, loader: otter_vm::DynamicImportLoaderHandle) {
        self.interp.set_dynamic_import_loader(loader);
    }

    /// Begin loading a dynamic import on the isolate thread.
    ///
    /// File-backed modules are loaded, compiled, evaluated, and
    /// settled before this returns. HTTPS modules are only resolved
    /// and capability-checked here; network I/O is deferred to the
    /// host service and returns later as owned source text.
    pub(crate) fn begin_dynamic_import(
        &mut self,
        token: u64,
        specifier: &str,
        referrer: &str,
        attr_type: Option<&str>,
    ) -> Result<DynamicImportBegin, OtterError> {
        let Some(realm_id) = self.interp.dynamic_import_realm_id(token) else {
            return Ok(DynamicImportBegin::Settled);
        };
        if realm_id != 0 {
            return self.begin_dynamic_import_in_extra_realm(token, specifier, referrer);
        }
        match self.load_dynamic_module(specifier, referrer, attr_type) {
            Ok(DynamicModuleLoad::Loaded(namespace)) => self
                .settle_dynamic_import_result(token, Ok(namespace))
                .map(|_| DynamicImportBegin::Settled),
            Ok(DynamicModuleLoad::GateOnEvaluation {
                promise,
                target_url,
            }) => {
                let Some(context) = self.interp.dynamic_import_context(token) else {
                    return Ok(DynamicImportBegin::Settled);
                };
                self.interp
                    .settle_dynamic_import_on_async_inits(
                        &context,
                        token,
                        vec![promise],
                        std::sync::Arc::from(target_url.as_str()),
                    )
                    .map_err(|err| {
                        map_vm_error(otter_vm::RunError {
                            error: err,
                            frames: Vec::new(),
                            detail: None,
                        })
                    })?;
                if let Err(err) = self.interp.drain_microtasks(|_, _| Ok(false))
                    && !self.absorb_termination(&err)
                {
                    return Err(enrich_runtime_diagnostic_with_cause(
                        &mut self.interp,
                        map_vm_error(err),
                    ));
                }
                Ok(DynamicImportBegin::Settled)
            }
            Ok(DynamicModuleLoad::PendingAsyncEvaluation {
                promise,
                target_url,
                context,
            }) => {
                self.interp
                    .settle_dynamic_import_on_async_inits(
                        &context,
                        token,
                        vec![promise],
                        std::sync::Arc::from(target_url.as_str()),
                    )
                    .map_err(|err| {
                        map_vm_error(otter_vm::RunError {
                            error: err,
                            frames: Vec::new(),
                            detail: None,
                        })
                    })?;
                // Drive the parked top-level-await frames to
                // completion; their settlement reactions settle the
                // import token, and the same drain delivers the
                // import promise's own reactions.
                if let Err(err) = self.interp.drain_microtasks(|_, _| Ok(false))
                    && !self.absorb_termination(&err)
                {
                    return Err(enrich_runtime_diagnostic_with_cause(
                        &mut self.interp,
                        map_vm_error(err),
                    ));
                }
                Ok(DynamicImportBegin::Settled)
            }
            Ok(DynamicModuleLoad::FetchHttps { target_url }) => {
                Ok(DynamicImportBegin::FetchHttps { target_url })
            }
            Err(DynLoadError::Fatal(error)) => Err(error),
            Err(DynLoadError::Diagnostic {
                kind,
                code,
                message,
            }) => self
                .alloc_dynamic_import_error(kind, code, message)
                .and_then(|value| self.settle_dynamic_import_result(token, Err(value)))
                .map(|_| DynamicImportBegin::Settled),
            Err(DynLoadError::Thrown(value)) => self
                .settle_dynamic_import_result(token, Err(value))
                .map(|_| DynamicImportBegin::Settled),
        }
    }

    /// Finish a remote dynamic import after its complete graph was prepared
    /// away from the isolate thread.
    pub(crate) fn complete_dynamic_import_prepared(
        &mut self,
        token: u64,
        target_url: &str,
        linked: Result<module_graph::LinkedProgram, module_graph::GraphError>,
    ) -> Result<bool, OtterError> {
        let Some(realm_id) = self.interp.dynamic_import_realm_id(token) else {
            return Ok(false);
        };
        if realm_id != 0 {
            return self.complete_dynamic_import_prepared_in_extra_realm(token, target_url, linked);
        }
        match linked
            .map_err(|error| DynLoadError::from_graph_error(&error, error.to_string()))
            .and_then(|linked| self.evaluate_dynamic_linked_module(target_url, linked))
        {
            Ok(DynamicModuleLoad::Loaded(namespace)) => {
                self.settle_dynamic_import_result(token, Ok(namespace))
            }
            Ok(DynamicModuleLoad::GateOnEvaluation {
                promise,
                target_url,
            }) => {
                let Some(context) = self.interp.dynamic_import_context(token) else {
                    return Ok(true);
                };
                self.interp
                    .settle_dynamic_import_on_async_inits(
                        &context,
                        token,
                        vec![promise],
                        std::sync::Arc::from(target_url.as_str()),
                    )
                    .map_err(|error| {
                        map_vm_error(otter_vm::RunError {
                            error,
                            frames: Vec::new(),
                            detail: None,
                        })
                    })?;
                self.interp
                    .drain_microtasks(|_, _| Ok(false))
                    .map_err(|error| {
                        enrich_runtime_diagnostic_with_cause(&mut self.interp, map_vm_error(error))
                    })?;
                Ok(true)
            }
            Ok(DynamicModuleLoad::PendingAsyncEvaluation {
                promise,
                target_url,
                context,
            }) => {
                self.interp
                    .settle_dynamic_import_on_async_inits(
                        &context,
                        token,
                        vec![promise],
                        std::sync::Arc::from(target_url.as_str()),
                    )
                    .map_err(|error| {
                        map_vm_error(otter_vm::RunError {
                            error,
                            frames: Vec::new(),
                            detail: None,
                        })
                    })?;
                self.interp
                    .drain_microtasks(|_, _| Ok(false))
                    .map_err(|error| {
                        enrich_runtime_diagnostic_with_cause(&mut self.interp, map_vm_error(error))
                    })?;
                Ok(true)
            }
            Ok(DynamicModuleLoad::FetchHttps { .. }) => Err(OtterError::Internal {
                code: "DYNAMIC_IMPORT_PREPARED_FETCH".to_string(),
                message: "prepared dynamic module requested a second entry fetch".to_string(),
            }),
            Err(DynLoadError::Fatal(error)) => Err(error),
            Err(DynLoadError::Diagnostic {
                kind,
                code,
                message,
            }) => {
                let value = self.alloc_dynamic_import_error(kind, code, message)?;
                self.settle_dynamic_import_result(token, Err(value))
            }
            Err(DynLoadError::Thrown(value)) => {
                self.settle_dynamic_import_result(token, Err(value))
            }
        }
    }

    pub(super) fn begin_dynamic_import_in_extra_realm(
        &mut self,
        token: u64,
        specifier: &str,
        referrer: &str,
    ) -> Result<DynamicImportBegin, OtterError> {
        use std::path::PathBuf;

        let (entry_for_loader, referrer_opt) =
            dynamic_import_origin(referrer).map_err(|error| OtterError::Io {
                path: PathBuf::from("."),
                kind: IoErrorKind::from_std(error.kind()),
                message: error.to_string(),
            })?;
        let loader = self.module_loader_for_entry(&entry_for_loader);
        let target_url = match loader.resolve(specifier, referrer_opt) {
            Ok(url) => url,
            Err(error) => {
                let (kind, code, message) = unresolved_import(specifier, &error);
                return self
                    .settle_extra_realm_dynamic_error(token, kind, code, message)
                    .map(|_| DynamicImportBegin::Settled);
            }
        };
        let cached = self
            .interp
            .with_dynamic_import_realm(token, |interp| {
                let cached = if let Some(thrown) = interp.module_evaluation_error(&target_url) {
                    Ok(Some(Err(thrown)))
                } else {
                    interp
                        .get_or_create_module_namespace(&target_url)
                        .map(|namespace| {
                            namespace.map(|namespace| Ok(otter_vm::Value::object(namespace)))
                        })
                        .map_err(|error| {
                            hosted_completion::into_runtime(interp, NativeError::from(error))
                        })
                };
                Ok(cached)
            })
            .map_err(realm::map_realm_vm_error)?
            .transpose()?
            .flatten();
        if let Some(outcome) = cached {
            return self
                .settle_extra_realm_dynamic_import(token, outcome)
                .map(|_| DynamicImportBegin::Settled);
        }
        if loader.is_hosted_url(&target_url) {
            let linked = hosted_completion::prepare_dynamic_graph(&loader, &target_url);
            self.complete_dynamic_import_prepared_in_extra_realm(token, &target_url, linked)?;
            return Ok(DynamicImportBegin::Settled);
        }
        if module_loader::is_http_url(&target_url) {
            return Ok(DynamicImportBegin::FetchHttps { target_url });
        }
        let target_path = url_to_path(&target_url).ok_or_else(|| OtterError::Runtime {
            diagnostic: Box::new(Diagnostic::new(
                DiagnosticKind::Type,
                DiagnosticCode::TypeError,
                format!("dynamic import: target is not a file:// URL: \"{target_url}\""),
            )),
        })?;
        let linked = self
            .module_graph
            .load_imported_module(&loader, &target_path)
            .map_err(map_graph_error)?;
        self.complete_dynamic_import_prepared_in_extra_realm(token, &target_url, Ok(linked))?;
        Ok(DynamicImportBegin::Settled)
    }

    pub(super) fn complete_dynamic_import_prepared_in_extra_realm(
        &mut self,
        token: u64,
        target_url: &str,
        linked: Result<module_graph::LinkedProgram, module_graph::GraphError>,
    ) -> Result<bool, OtterError> {
        let linked = match linked {
            Ok(linked) => linked,
            Err(error) => {
                return match DynLoadError::from_graph_error(&error, error.to_string()) {
                    DynLoadError::Fatal(error) => Err(error),
                    DynLoadError::Diagnostic {
                        kind,
                        code,
                        message,
                    } => self.settle_extra_realm_dynamic_error(token, kind, code, message),
                    DynLoadError::Thrown(value) => {
                        self.settle_extra_realm_dynamic_import(token, Err(value))
                    }
                };
            }
        };
        for metadata in &linked.metadata {
            self.source_maps.record_compiled_metadata(metadata);
        }
        let target_url = target_url.to_string();
        let records = &mut self.module_records;
        let config = &self.config;
        let commonjs = &self.commonjs;
        self.interp
            .with_dynamic_import_realm(token, move |interp| {
                Ok(evaluate_and_settle_dynamic_linked_module_on(
                    interp,
                    records,
                    config,
                    commonjs,
                    token,
                    &target_url,
                    linked,
                ))
            })
            .map_err(realm::map_realm_vm_error)?
            .unwrap_or(Ok(false))
    }

    pub(super) fn settle_extra_realm_dynamic_import(
        &mut self,
        token: u64,
        outcome: Result<otter_vm::Value, otter_vm::Value>,
    ) -> Result<bool, OtterError> {
        self.interp
            .with_dynamic_import_realm(token, move |interp| {
                Ok(settle_dynamic_import_result_on(interp, token, outcome))
            })
            .map_err(realm::map_realm_vm_error)?
            .unwrap_or(Ok(false))
    }

    pub(super) fn settle_extra_realm_dynamic_error(
        &mut self,
        token: u64,
        kind: otter_vm::ErrorKind,
        code: Option<&'static str>,
        message: String,
    ) -> Result<bool, OtterError> {
        self.interp
            .with_dynamic_import_realm(token, move |interp| {
                Ok(alloc_dynamic_import_error_on(interp, kind, code, message)
                    .and_then(|value| settle_dynamic_import_result_on(interp, token, Err(value))))
            })
            .map_err(realm::map_realm_vm_error)?
            .unwrap_or(Ok(false))
    }

    pub(super) fn settle_dynamic_import_result(
        &mut self,
        token: u64,
        reaction_outcome: Result<otter_vm::Value, otter_vm::Value>,
    ) -> Result<bool, OtterError> {
        let settled_context = self.interp.settle_dynamic_import(token, reaction_outcome);
        if let Some(_context) = settled_context {
            if let Err(err) = self.interp.drain_microtasks(|_, _| Ok(false))
                && !self.absorb_termination(&err)
            {
                return Err(enrich_runtime_diagnostic_with_cause(
                    &mut self.interp,
                    map_vm_error(err),
                ));
            }
            return Ok(true);
        }
        Ok(false)
    }

    pub(super) fn alloc_dynamic_import_error(
        &mut self,
        kind: otter_vm::ErrorKind,
        code: Option<&'static str>,
        message: String,
    ) -> Result<otter_vm::Value, OtterError> {
        dynamic_import_error::allocate(&mut self.interp, kind, code, &message)
    }

    pub(super) fn load_dynamic_module(
        &mut self,
        specifier: &str,
        referrer: &str,
        attr_type: Option<&str>,
    ) -> Result<DynamicModuleLoad, DynLoadError> {
        use std::path::PathBuf;
        let (entry_for_loader, referrer_opt) = dynamic_import_origin(referrer).map_err(|e| {
            DynLoadError::type_error(format!("dynamic import: cwd lookup failed: {e}"))
        })?;
        let loader = self.module_loader_for_entry(&entry_for_loader);
        let target_url = loader
            .resolve(specifier, referrer_opt)
            .map_err(|error| DynLoadError::unresolved(specifier, &error))?;
        // A `type` attribute selects the data-module pipeline. Route through
        // a synthetic wrapper whose one static edge carries the attribute, so
        // the graph machinery that serves static attributed imports serves
        // the dynamic form identically; the wrapper record is keyed by
        // (type, target) so the same file imported plainly stays a distinct
        // module record.
        if let Some(kind) = attr_type {
            let synthetic_url = format!("otter-attr-dynamic:{kind}:{target_url}");
            if let Some(namespace) = self
                .interp
                .get_or_create_module_namespace(&synthetic_url)
                .map_err(|error| {
                    hosted_completion::into_dynamic(&mut self.interp, NativeError::from(error))
                })?
            {
                return Ok(DynamicModuleLoad::Loaded(otter_vm::Value::object(
                    namespace,
                )));
            }
            let specifier_literal =
                serde_json::to_string(&target_url).unwrap_or_else(|_| "\"\"".to_string());
            let type_literal = serde_json::to_string(kind).unwrap_or_else(|_| "\"\"".to_string());
            let text = format!(
                "import __otterData from {specifier_literal} with {{ type: {type_literal} }};\n\
                 export default __otterData;\n"
            );
            let text = module_loader::admit_source(loader.resource_account(), &synthetic_url, text)
                .map_err(module_graph::GraphError::Loader)
                .map_err(|e| DynLoadError::from_graph_error(&e, &synthetic_url))?;
            let entry = module_loader::ResolvedSource {
                url: synthetic_url.clone(),
                kind: SourceKind::JavaScript,
                jsx: None,
                text,
            };
            let linked = module_graph::load_program_source(&loader, entry).map_err(|e| {
                DynLoadError::from_graph_error(
                    &e,
                    format!("dynamic import: load failed for \"{target_url}\": {e:?}"),
                )
            })?;
            let loaded = self.evaluate_dynamic_linked_module(&synthetic_url, linked)?;
            let Some(namespace) = self
                .interp
                .get_or_create_module_namespace(&synthetic_url)
                .map_err(|error| {
                    hosted_completion::into_dynamic(&mut self.interp, NativeError::from(error))
                })?
            else {
                return Ok(loaded);
            };
            return Ok(DynamicModuleLoad::Loaded(otter_vm::Value::object(
                namespace,
            )));
        }
        // §16.2.1.5 — a record whose evaluation completed abruptly answers
        // every later import by rethrowing the same [[EvaluationError]]; a
        // cached environment must not shortcut past that.
        if let Some(thrown) = self.interp.module_evaluation_error(&target_url) {
            return Err(DynLoadError::Thrown(thrown));
        }
        // §16.2.1.9 — a record still awaiting its top-level-await subtree
        // gates this import on the same evaluation promise, preserving
        // settlement order across repeat imports.
        if let Some(promise) = self.interp.module_pending_evaluation_gate(&target_url) {
            return Ok(DynamicModuleLoad::GateOnEvaluation {
                promise,
                target_url: target_url.clone(),
            });
        }
        if let Some(namespace) = self
            .interp
            .get_or_create_module_namespace(&target_url)
            .map_err(|error| {
                hosted_completion::into_dynamic(&mut self.interp, NativeError::from(error))
            })?
        {
            return Ok(DynamicModuleLoad::Loaded(otter_vm::Value::object(
                namespace,
            )));
        }
        // A web page's modules come from its embedder, which fetches on its
        // own schedule: park the import until the graph is delivered.
        if loader.web_modules().is_some() && !module_loader::is_data_url(&target_url) {
            return Ok(DynamicModuleLoad::FetchHttps { target_url });
        }
        // HTTPS / HTTP targets take a separate fetch path because
        // `module_graph::load_program` only walks file:// URLs.
        // The capability check has already passed inside
        // `loader.resolve` so the target host is on the allowlist.
        if target_url.starts_with("http://") || target_url.starts_with("https://") {
            return Ok(DynamicModuleLoad::FetchHttps { target_url });
        }
        // A hosted builtin (`node:fs`, `process`, ...) has no file to walk. A
        // module that imports it does, so the graph is rooted at one: linking
        // builds the builtin's own namespace — `default` and every named
        // export — which is exactly what a static `import` of it answers, and
        // that namespace is what the import resolves to.
        if loader.is_hosted_url(&target_url) {
            let linked = hosted_completion::prepare_dynamic_graph(&loader, &target_url)
                .map_err(|error| DynLoadError::from_graph_error(&error, error.to_string()))?;
            let entry_url = linked.entry_url.clone();
            self.evaluate_dynamic_linked_module(&entry_url, linked)?;
            let Some(namespace) = self
                .interp
                .get_or_create_module_namespace(&target_url)
                .map_err(|error| {
                    hosted_completion::into_dynamic(&mut self.interp, NativeError::from(error))
                })?
            else {
                return Err(hosted_completion::into_dynamic(
                    &mut self.interp,
                    NativeError::InvalidOperand,
                ));
            };
            return Ok(DynamicModuleLoad::Loaded(otter_vm::Value::object(
                namespace,
            )));
        }
        // A `data:` module has no file to walk either: its source is the
        // specifier, so it is linked straight from the decoded text.
        if module_loader::is_data_url(&target_url) {
            let entry = loader.load_resolved(target_url.clone()).map_err(|e| {
                DynLoadError::from_graph_error(
                    &module_graph::GraphError::from(e),
                    format!("dynamic import: load failed for \"{target_url}\""),
                )
            })?;
            let linked = module_graph::load_program_source(&loader, entry).map_err(|e| {
                DynLoadError::from_graph_error(
                    &e,
                    format!("dynamic import: load failed for \"{target_url}\": {e:?}"),
                )
            })?;
            return self.evaluate_dynamic_linked_module(&target_url, linked);
        }
        let target_path: PathBuf = url_to_path(&target_url).ok_or_else(|| {
            DynLoadError::type_error(format!(
                "dynamic import: target is not a file:// URL: \"{target_url}\""
            ))
        })?;
        let linked = self
            .module_graph
            .load_imported_module(&loader, &target_path)
            .map_err(|e| {
                DynLoadError::from_graph_error(
                    &e,
                    format!("dynamic import: load failed for \"{target_url}\": {e:?}"),
                )
            })?;
        self.evaluate_dynamic_linked_module(&target_url, linked)
    }

    pub(super) fn evaluate_dynamic_linked_module(
        &mut self,
        target_url: &str,
        linked: module_graph::LinkedProgram,
    ) -> Result<DynamicModuleLoad, DynLoadError> {
        self.adopt_web_module_bodies();
        for metadata in &linked.metadata {
            self.source_maps.record_compiled_metadata(metadata);
        }
        let sources = otter_vm::source_registry::SourceRegistry::new(
            linked.module_sources.clone(),
            &self.config.resource_account,
        );
        let sources =
            sources.map_err(|error| DynLoadError::Fatal(OtterError::Resource { error }))?;
        let context = self
            .interp
            .link_evictable_module(linked.module, sources)
            .map_err(|error| DynLoadError::Fatal(OtterError::from(error)))?;
        // Hosted builtins in this batch get their real namespaces (running
        // their installers when needed); plain modules get fresh
        // environments — the same records pipeline the static loader uses.
        self.module_records
            .allocate_for_module_inits(&mut self.interp, &context, &self.commonjs)
            .map_err(|error| hosted_completion::into_dynamic(&mut self.interp, error))?;
        self.register_resolved_exports(&linked.metadata);
        self.report_watch_imports(&linked.module_sources);
        // §13.3.10 step 7 — Evaluate(target): the records-backed
        // InnerModuleEvaluation walks the target's eager dependency
        // closure, parking on top-level await instead of blocking.
        let evaluation = NativeCtx::with_host_context(
            &mut self.interp,
            NativeCallInfo::default_call(),
            Some(&context),
            |ctx| ctx.evaluate_module(target_url),
        );
        match evaluation {
            Ok(Some(promise)) => {
                return Ok(DynamicModuleLoad::PendingAsyncEvaluation {
                    promise,
                    target_url: target_url.to_string(),
                    context,
                });
            }
            Ok(None) => {}
            Err(failure) => {
                // This module evaluation has completed. A terminal engine or
                // escaping allocation failure leaves through the existing
                // runtime mapper with its exact owned detail/source frames.
                if failure.is_fatal() {
                    return Err(DynLoadError::Fatal(map_vm_error(failure)));
                }
                let err = failure.error;
                // §16.2.1.7 step 7.b.i — an evaluation throw maps
                // to a promise rejection. Prefer the original
                // thrown Value (preserved on
                // `pending_uncaught_throw` whenever the throw
                // walked the empty stack inside the dispatch
                // sub-loop) so `.catch` observes the spec-correct
                // payload, not a stringified `VmError::Uncaught`
                // rendering.
                if matches!(err, otter_vm::VmError::Uncaught)
                    && let Some(thrown) = self.interp.take_pending_uncaught_throw()
                {
                    return Err(DynLoadError::Thrown(thrown));
                }
                return Err(DynLoadError::type_error(format!(
                    "dynamic import: evaluation failed for \"{target_url}\": {err}"
                )));
            }
        }
        let namespace = self
            .interp
            .get_or_create_module_namespace(target_url)
            .map_err(|error| {
                hosted_completion::into_dynamic(&mut self.interp, NativeError::from(error))
            })?
            .ok_or_else(|| {
                hosted_completion::into_dynamic(&mut self.interp, NativeError::InvalidOperand)
            })?;
        Ok(DynamicModuleLoad::Loaded(otter_vm::Value::object(
            namespace,
        )))
    }
}
