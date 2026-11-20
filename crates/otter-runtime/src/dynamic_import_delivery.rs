//! Delivery of queued direct and web module imports.
//!
//! # Contents
//! Queued import evaluation, web fetch completion and pending root release.
//!
//! # Invariants
//! Queued imports retain their originating runtime and realm.
//!
//! # See also
//! - [`crate::Runtime`] owns the isolate session.

use crate::*;

impl Runtime {
    /// Drive direct-mode dynamic imports to completion: pop every
    /// queued `import()` request, load + evaluate it through
    /// [`Self::begin_dynamic_import`], then drain microtasks — whose
    /// reactions may queue further imports — until the queue is dry.
    /// Each request and reaction owns its source and realm context.
    /// HTTPS targets need the isolate runner's fetcher and reject
    /// with a `TypeError` here.
    pub(super) fn pump_layer_a_dynamic_imports(&mut self) -> Result<(), OtterError> {
        loop {
            let request = {
                let mut queue = self
                    .layer_a_dynamic_imports
                    .lock()
                    .expect("layer-a dynamic import queue poisoned");
                queue.pop_front()
            };
            if let Some(request) = request {
                let LayerADynamicImportRequest {
                    admission,
                    token,
                    specifier,
                    referrer,
                    attr_type,
                } = request;
                let begun =
                    self.begin_dynamic_import(token, &specifier, &referrer, attr_type.as_deref());
                if let Ok(DynamicImportBegin::FetchHttps { target_url }) = &begun
                    && self.web_module_map().is_some()
                {
                    self.web_dynamic_imports.push(ParkedWebImport {
                        admission,
                        token,
                        target_url: target_url.clone(),
                    });
                    continue;
                }
                let active = admission.begin_dispatch();
                let result = (|| -> Result<(), OtterError> {
                    match begun? {
                        DynamicImportBegin::Settled => {}
                        DynamicImportBegin::FetchHttps { target_url } => {
                            let message = format!(
                                "dynamic import: remote module \"{target_url}\" requires the isolate runner"
                            );
                            if self
                                .interp
                                .dynamic_import_realm_id(token)
                                .is_some_and(|realm_id| realm_id != 0)
                            {
                                self.settle_extra_realm_dynamic_error(
                                    token,
                                    otter_vm::ErrorKind::TypeError,
                                    None,
                                    message,
                                )?;
                            } else {
                                let reason = self.alloc_dynamic_import_error(
                                    otter_vm::ErrorKind::TypeError,
                                    None,
                                    message,
                                )?;
                                self.settle_dynamic_import_result(token, Err(reason))?;
                            }
                        }
                    }
                    Ok(())
                })();
                if let Err(error) = result {
                    drop(active);
                    self.cancel_layer_a_dynamic_imports_after_failure(token);
                    return Err(error);
                }
                drop(active);
                continue;
            }

            if let Err(err) = self.interp.drain_microtasks(|_, _| Ok(false))
                && !self.absorb_termination(&err)
            {
                self.cancel_layer_a_dynamic_imports_after_failure(0);
                return Err(enrich_runtime_diagnostic_with_cause(
                    &mut self.interp,
                    map_vm_error(err),
                ));
            }
            if self
                .layer_a_dynamic_imports
                .lock()
                .expect("layer-a dynamic import queue poisoned")
                .is_empty()
                && !self.settle_delivered_web_dynamic_imports()?
            {
                return Ok(());
            }
        }
    }

    /// The embedder's module map, when this runtime serves web modules.
    #[must_use]
    pub fn web_module_map(&self) -> Option<&web_modules::WebModuleMap> {
        self.module_loader
            .configured
            .as_ref()
            .and_then(|config| config.web_modules.as_ref())
    }

    /// Whether a dynamic `import()` is waiting for the embedder.
    #[must_use]
    pub fn has_pending_web_imports(&self) -> bool {
        !self.web_dynamic_imports.is_empty()
    }

    /// Module URLs the parked dynamic imports need that nobody has asked
    /// for yet. Each URL is reported once; the embedder fetches it and
    /// delivers the result into [`Self::web_module_map`].
    #[must_use]
    pub fn web_module_fetches(&self) -> Vec<String> {
        let Some(map) = self.web_module_map() else {
            return Vec::new();
        };
        let mut fetch = Vec::new();
        for parked in &self.web_dynamic_imports {
            if let Ok(web_modules::WebModuleGraph::Pending { fetch: more }) =
                map.graph_status(&parked.target_url)
            {
                fetch.extend(more);
            }
        }
        fetch
    }

    /// Settle every parked dynamic import whose graph is now delivered (or
    /// failed), evaluate what they imported, and run the microtasks and any
    /// further imports that follow.
    ///
    /// # Errors
    /// A fatal runtime failure while evaluating an imported graph. A failed
    /// fetch, parse or evaluation rejects the import's promise instead.
    pub fn resume_web_dynamic_imports(&mut self) -> Result<(), OtterError> {
        self.with_direct_timeout(|runtime| {
            if runtime.settle_delivered_web_dynamic_imports()? {
                runtime.pump_layer_a_dynamic_imports()?;
            }
            Ok(())
        })
    }

    /// One pass over the parked web imports; `true` when any settled.
    pub(super) fn settle_delivered_web_dynamic_imports(&mut self) -> Result<bool, OtterError> {
        let Some(map) = self.web_module_map().cloned() else {
            return Ok(false);
        };
        let mut settled = false;
        let mut index = 0;
        while index < self.web_dynamic_imports.len() {
            let target_url = self.web_dynamic_imports[index].target_url.clone();
            let (entry_url, linked) = match map.peek_graph(&target_url) {
                Ok(web_modules::WebModuleGraph::Pending { .. }) => {
                    index += 1;
                    continue;
                }
                Ok(web_modules::WebModuleGraph::Ready) => {
                    let loader = self.module_loader_for_entry(Path::new("."));
                    match loader.load_resolved(target_url.clone()) {
                        Ok(entry) => (
                            entry.url.clone(),
                            module_graph::load_program_source(&loader, entry),
                        ),
                        Err(error) => (target_url, Err(module_graph::GraphError::Loader(error))),
                    }
                }
                Err(error) => (
                    target_url.clone(),
                    Err(module_graph::GraphError::Loader(
                        module_loader::LoaderError::Load {
                            url: target_url,
                            message: error.to_string(),
                        },
                    )),
                ),
            };
            let parked = self.web_dynamic_imports.remove(index);
            let active = parked.admission.begin_dispatch();
            let completed = self.complete_dynamic_import_prepared(parked.token, &entry_url, linked);
            drop(active);
            completed?;
            settled = true;
        }
        Ok(settled)
    }

    /// Fatal Layer A pumping errors cannot leave a registry root after the
    /// unique admission that protected it is dropped. Cancel the current token
    /// (when non-zero), then consume and cancel every still-admitted FIFO item
    /// one at a time so cleanup itself allocates no batch vector.
    pub(super) fn cancel_layer_a_dynamic_imports_after_failure(&mut self, current_token: u64) {
        if current_token != 0 {
            let _ = self.cancel_dynamic_import(current_token);
        }
        loop {
            let request = self
                .layer_a_dynamic_imports
                .lock()
                .expect("layer-a dynamic import queue poisoned")
                .pop_front();
            let Some(request) = request else { return };
            let _ = self.cancel_dynamic_import(request.token);
            drop(request);
        }
    }
}
