//! Linked module evaluation and dynamic import completion.
//!
//! # Contents
//! Module evaluation, import promises and load-error conversion.
//!
//! # Invariants
//! Evaluation settles the promise of the originating request.
//!
//! # See also
//! - [`crate::Runtime`] owns the runtime session.

use crate::*;

/// Where a dynamic `import()` resolves its specifier from: the file the
/// loader is built for, and the referrer a relative specifier is relative to.
///
/// A referrer naming a file — or a document fetched over HTTP — resolves
/// against itself. Anything else (`<eval>`, a hosted builtin, a synthesized
/// wrapper) has no file of its own, so a specifier written there resolves the
/// way Node resolves one written under `--eval`: as though it were written in
/// a file sitting in the working directory.
pub(super) fn dynamic_import_origin(
    referrer: &str,
) -> std::io::Result<(std::path::PathBuf, Option<&str>)> {
    if !referrer.is_empty() {
        if module_loader::is_http_url(referrer) {
            return Ok((std::path::PathBuf::from("."), Some(referrer)));
        }
        if let Some(path) = url_to_path(referrer) {
            return Ok((path, Some(referrer)));
        }
    }
    std::env::current_dir().map(|cwd| (cwd.join("[eval]"), None))
}

/// Convert a `file://` URL back into a filesystem path. Returns
/// `None` for any other scheme (e.g. `https://`) — dynamic
/// imports targeting non-`file://` URLs use a separate fetch
/// path inside [`Runtime::load_dynamic_module`].
pub(crate) fn url_to_path(url: &str) -> Option<std::path::PathBuf> {
    if let Some(trimmed) = url.strip_prefix("file://") {
        return Some(std::path::PathBuf::from(trimmed));
    }
    // A CommonJS module's referrer is its filesystem path; dynamic `import()`
    // is legal there and resolves against that file exactly as a URL one.
    if std::path::Path::new(url).is_absolute() {
        return Some(std::path::PathBuf::from(url));
    }
    None
}

/// Internal error type for [`Runtime::load_dynamic_module`]. Split
/// so the surrounding settle path can distinguish:
///
/// - [`DynLoadError::Diagnostic`] — host-side resolve / load /
///   compile / link failure. The settler synthesises a
///   fresh JS error of the carried kind from the message.
/// - [`DynLoadError::Fatal`] — a completed terminal engine or escaping OOM
///   failure, mapped once with its original owned detail and source frames.
/// - [`DynLoadError::Thrown`] — the dynamically-loaded module's
///   `<module-init>` threw a JS value. The settler uses that
///   value directly as the promise's rejection reason per
///   §16.2.1.7 step 7.b.i + §27.2.1.7.
pub(super) fn evaluate_and_settle_dynamic_linked_module_on(
    interp: &mut Interpreter,
    records: &mut module_records::RuntimeModuleRecords,
    config: &RuntimeConfig,
    commonjs: &Arc<commonjs::CjsConfig>,
    token: u64,
    target_url: &str,
    linked: module_graph::LinkedProgram,
) -> Result<bool, OtterError> {
    let outcome =
        evaluate_dynamic_linked_module_on(interp, records, config, commonjs, target_url, linked);
    match outcome {
        Ok(DynamicModuleLoad::Loaded(namespace)) => {
            settle_dynamic_import_result_on(interp, token, Ok(namespace))
        }
        Ok(DynamicModuleLoad::GateOnEvaluation {
            promise,
            target_url,
        }) => {
            let Some(context) = interp.dynamic_import_context(token) else {
                return Ok(true);
            };
            interp
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
            interp.drain_microtasks(|_, _| Ok(false)).map_err(|error| {
                enrich_runtime_diagnostic_with_cause(interp, map_vm_error(error))
            })?;
            Ok(true)
        }
        Ok(DynamicModuleLoad::PendingAsyncEvaluation {
            promise,
            target_url,
            context,
        }) => {
            interp
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
            interp.drain_microtasks(|_, _| Ok(false)).map_err(|error| {
                enrich_runtime_diagnostic_with_cause(interp, map_vm_error(error))
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
            let value = alloc_dynamic_import_error_on(interp, kind, code, message)?;
            settle_dynamic_import_result_on(interp, token, Err(value))
        }
        Err(DynLoadError::Thrown(value)) => {
            settle_dynamic_import_result_on(interp, token, Err(value))
        }
    }
}

pub(super) fn evaluate_dynamic_linked_module_on(
    interp: &mut Interpreter,
    records: &mut module_records::RuntimeModuleRecords,
    config: &RuntimeConfig,
    commonjs: &Arc<commonjs::CjsConfig>,
    target_url: &str,
    linked: module_graph::LinkedProgram,
) -> Result<DynamicModuleLoad, DynLoadError> {
    // Hosted imports evaluate their real admitted wrapper, whose dependency
    // owns the namespace returned below. Plain modules retain their target URL.
    let evaluation_url = if config
        .hosted_modules
        .iter()
        .any(|hosted| hosted.specifier() == target_url)
    {
        linked.entry_url.clone()
    } else {
        target_url.to_owned()
    };
    let sources = otter_vm::source_registry::SourceRegistry::new(
        linked.module_sources,
        &interp.resource_account(),
    )
    .map_err(|error| DynLoadError::Fatal(OtterError::from(error)))?;
    let context = interp
        .link_evictable_module(linked.module, sources)
        .map_err(|error| DynLoadError::Fatal(OtterError::from(error)))?;
    records
        .allocate_for_module_inits(interp, &context, commonjs)
        .map_err(|error| hosted_completion::into_dynamic(interp, error))?;
    for metadata in &linked.metadata {
        if metadata.source_url.is_empty() || metadata.resolved_exports.is_empty() {
            continue;
        }
        let table = metadata
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
        interp.register_module_resolved_exports(
            std::sync::Arc::from(metadata.source_url.as_str()),
            table,
        );
    }
    let evaluation = NativeCtx::with_host_context(
        interp,
        NativeCallInfo::default_call(),
        Some(&context),
        |ctx| ctx.evaluate_module(&evaluation_url),
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
            // Preserve the same completed failure boundary in the selected
            // additional realm; no rejection materialization runs for it.
            if failure.is_fatal() {
                return Err(DynLoadError::Fatal(map_vm_error(failure)));
            }
            let error = failure.error;
            if matches!(error, otter_vm::VmError::Uncaught)
                && let Some(thrown) = interp.take_pending_uncaught_throw()
            {
                return Err(DynLoadError::Thrown(thrown));
            }
            return Err(DynLoadError::type_error(format!(
                "dynamic import: evaluation failed for \"{target_url}\": {error}"
            )));
        }
    }
    let namespace = interp
        .get_or_create_module_namespace(target_url)
        .map_err(|error| hosted_completion::into_dynamic(interp, NativeError::from(error)))?
        .ok_or_else(|| hosted_completion::into_dynamic(interp, NativeError::InvalidOperand))?;
    Ok(DynamicModuleLoad::Loaded(otter_vm::Value::object(
        namespace,
    )))
}

pub(super) fn settle_dynamic_import_result_on(
    interp: &mut Interpreter,
    token: u64,
    outcome: Result<otter_vm::Value, otter_vm::Value>,
) -> Result<bool, OtterError> {
    let settled_context = interp.settle_dynamic_import(token, outcome);
    if let Some(_context) = settled_context {
        interp
            .drain_microtasks(|_, _| Ok(false))
            .map_err(|error| enrich_runtime_diagnostic_with_cause(interp, map_vm_error(error)))?;
        return Ok(true);
    }
    Ok(false)
}

pub(super) fn alloc_dynamic_import_error_on(
    interp: &mut Interpreter,
    kind: otter_vm::ErrorKind,
    code: Option<&'static str>,
    message: String,
) -> Result<otter_vm::Value, OtterError> {
    dynamic_import_error::allocate(interp, kind, code, &message)
}

/// What `import()` of a specifier the loader could not resolve rejects with:
/// the error Node throws for it when the loader's reason has one — an unknown
/// builtin, a missing package — and the engine's own description otherwise.
pub(super) fn unresolved_import(
    specifier: &str,
    error: &module_loader::LoaderError,
) -> (otter_vm::ErrorKind, Option<&'static str>, String) {
    match error.node_error() {
        Some((code, message)) => (otter_vm::ErrorKind::Error, Some(code), message),
        None => (
            otter_vm::ErrorKind::TypeError,
            None,
            format!("dynamic import: cannot resolve \"{specifier}\": {error}"),
        ),
    }
}

pub(super) enum DynLoadError {
    Diagnostic {
        kind: otter_vm::ErrorKind,
        /// Node's `code` for the failure, when Node gives it one.
        code: Option<&'static str>,
        message: String,
    },
    Thrown(otter_vm::Value),
    Fatal(OtterError),
}

impl DynLoadError {
    pub(super) fn diagnostic(kind: otter_vm::ErrorKind, message: impl Into<String>) -> Self {
        Self::Diagnostic {
            kind,
            code: None,
            message: message.into(),
        }
    }

    pub(super) fn type_error(message: impl Into<String>) -> Self {
        Self::diagnostic(otter_vm::ErrorKind::TypeError, message)
    }

    /// `import()` of a specifier the loader could not resolve.
    pub(super) fn unresolved(specifier: &str, error: &module_loader::LoaderError) -> Self {
        let (kind, code, message) = unresolved_import(specifier, error);
        Self::Diagnostic {
            kind,
            code,
            message,
        }
    }

    pub(super) fn from_graph_error(
        err: &module_graph::GraphError,
        message: impl Into<String>,
    ) -> Self {
        match err {
            module_graph::GraphError::Loader(module_loader::LoaderError::Resource { error }) => {
                return Self::Fatal(OtterError::Resource {
                    error: error.clone(),
                });
            }
            module_graph::GraphError::Interrupted => return Self::Fatal(OtterError::Interrupted),
            module_graph::GraphError::Loader(error) => {
                if let Some((code, message)) = error.node_error() {
                    return Self::Diagnostic {
                        kind: otter_vm::ErrorKind::Error,
                        code: Some(code),
                        message,
                    };
                }
            }
            _ => {}
        }
        let kind = match err {
            module_graph::GraphError::Interrupted => otter_vm::ErrorKind::TypeError,
            module_graph::GraphError::Parse { .. } => otter_vm::ErrorKind::SyntaxError,
            module_graph::GraphError::Compile { error, .. } => match error {
                otter_compiler::CompileError::Syntax { .. } => otter_vm::ErrorKind::SyntaxError,
                _ => otter_vm::ErrorKind::TypeError,
            },
            module_graph::GraphError::Cycle { .. } => otter_vm::ErrorKind::RangeError,
            module_graph::GraphError::Resolution { .. } => otter_vm::ErrorKind::SyntaxError,
            module_graph::GraphError::Loader(_) => otter_vm::ErrorKind::TypeError,
        };
        Self::diagnostic(kind, message)
    }
}
