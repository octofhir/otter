//! Conversion of compiler and VM failures into runtime diagnostics.
//!
//! # Contents
//! Source locations, exception cause chains and host-facing errors.
//!
//! # Invariants
//! Thrown values retain their JS error category and source context.
//!
//! # See also
//! - [`crate::Runtime`] owns the runtime session.

use crate::*;

pub(crate) fn map_graph_error(err: module_graph::GraphError) -> OtterError {
    match err {
        module_graph::GraphError::Interrupted => OtterError::Interrupted,
        module_graph::GraphError::Loader(module_loader::LoaderError::Resource { error }) => OtterError::Resource { error },
        // Surface capability denials with their own error code so embedders
        // and CLI users can distinguish a missing permission from a genuinely
        // unresolvable specifier.
        module_graph::GraphError::Loader(module_loader::LoaderError::CapabilityDenied {
            specifier,
            capability,
            resource,
        }) => OtterError::Compile {
            diagnostics: vec![
                Diagnostic::permission(format!(
                    "import of `{specifier}` requires capability `{capability}` for `{resource}`"
                ))
                .with_code_enum(DiagnosticCode::ModuleCapabilityDenied)
                .with_help(format!(
                    "grant the matching capability (e.g. --allow-{capability}={resource}) or remove the import"
                )),
            ],
        },
        module_graph::GraphError::Loader(err) => OtterError::Compile {
            diagnostics: vec![
                Diagnostic::syntax(err.to_string())
                    .with_code_enum(DiagnosticCode::ModuleResolutionError)
                    .with_help(
                        "check the import specifier and package/module loader configuration",
                    ),
            ],
        },
        module_graph::GraphError::Parse { url, error } => map_syntax_error(error, &url),
        module_graph::GraphError::Compile { url, error } => map_compile_error(error, &url),
        module_graph::GraphError::Cycle { url } => OtterError::Compile {
            diagnostics: vec![
                Diagnostic::syntax(format!(
                    "module graph cycle or depth limit reached at `{url}`"
                ))
                .with_code_enum(DiagnosticCode::ModuleGraphCycle)
                .with_source_url(url)
                .with_help("break the import cycle or reduce module graph depth"),
            ],
        },
        module_graph::GraphError::Resolution { url, message } => OtterError::Compile {
            diagnostics: vec![
                Diagnostic::syntax(message)
                    .with_source_url(url)
                    .with_help("export the imported binding from the target module"),
            ],
        },
    }
}

pub(super) fn map_syntax_error(err: SyntaxError, source_url: &str) -> OtterError {
    let diagnostics = if err.diagnostics.is_empty() {
        vec![
            Diagnostic::syntax(err.messages.join("; "))
                .with_source_url(source_url)
                .with_help("fix the syntax error in the source file"),
        ]
    } else {
        err.diagnostics
            .iter()
            .map(|diagnostic| map_syntax_diagnostic(diagnostic, source_url))
            .collect()
    };
    OtterError::Compile { diagnostics }
}

fn map_syntax_diagnostic(diagnostic: &SyntaxDiagnostic, source_url: &str) -> Diagnostic {
    let mut mapped = Diagnostic::syntax(diagnostic.message.clone())
        .with_code(diagnostic.code.clone())
        .with_source_url(source_url);
    if let Some(range) = diagnostic.range {
        mapped = mapped.with_range(range);
    }
    mapped.with_help(
        diagnostic
            .help
            .clone()
            .unwrap_or_else(|| "fix the syntax error in the source file".to_string()),
    )
}

pub(crate) fn map_compile_error(err: otter_compiler::CompileError, source_url: &str) -> OtterError {
    use otter_compiler::CompileError;
    match err {
        CompileError::Syntax {
            messages,
            diagnostics,
        } => {
            if diagnostics.is_empty() {
                OtterError::Compile {
                    diagnostics: vec![
                        Diagnostic::syntax(messages.join("; "))
                            .with_source_url(source_url)
                            .with_help("fix the syntax error in the source file"),
                    ],
                }
            } else {
                OtterError::Compile {
                    diagnostics: diagnostics
                        .iter()
                        .map(|diagnostic| map_syntax_diagnostic(diagnostic, source_url))
                        .collect(),
                }
            }
        }
        CompileError::Unsupported { node, span } => OtterError::Compile {
            diagnostics: vec![
                Diagnostic::unsupported(format!("unsupported AST node: {node}"), span)
                    .with_source_url(source_url),
            ],
        },
        CompileError::TypeScriptUnsupported { node, span } => OtterError::Compile {
            diagnostics: vec![
                Diagnostic::ts_unsupported(
                    format!("typescript {node} is not supported in foundation"),
                    span,
                )
                .with_source_url(source_url),
            ],
        },
        CompileError::MetadataLimit {
            requested_bytes,
            limit_bytes,
        } => OtterError::OutOfMemory {
            requested_bytes: u64::try_from(requested_bytes).unwrap_or(u64::MAX),
            heap_limit_bytes: u64::try_from(limit_bytes).unwrap_or(u64::MAX),
        },
        CompileError::MetadataAllocation { requested_bytes } => OtterError::OutOfMemory {
            requested_bytes: u64::try_from(requested_bytes).unwrap_or(u64::MAX),
            heap_limit_bytes: u64::try_from(otter_compiler::MAX_COMPILED_METADATA_BYTES)
                .unwrap_or(u64::MAX),
        },
        _ => OtterError::Internal {
            code: DiagnosticCode::CompileUnknown.as_str().to_string(),
            message: "unknown compiler error variant".to_string(),
        },
    }
}

/// Maximum depth the diagnostic cause-chain walker descends into
/// nested `Error.cause` properties before bailing out. Protects
/// against pathological self-referential chains.
const MAX_CAUSE_CHAIN_DEPTH: usize = 32;

/// Walk a thrown JS value into a [`Diagnostic`] tree.
///
/// Reads `name` / `message` / `cause` / `errors` from any Error
/// instance (§20.5 / §20.5.7). Recurses on `cause` up to
/// [`MAX_CAUSE_CHAIN_DEPTH`]. Non-Error throws (strings, numbers,
/// …) become plain `TypeError`-categorised diagnostics with the
/// stringified value as `message`.
pub(super) fn diagnostic_from_thrown_value(
    interp: &otter_vm::Interpreter,
    value: &otter_vm::Value,
    depth: usize,
) -> Diagnostic {
    if depth > MAX_CAUSE_CHAIN_DEPTH {
        return Diagnostic::new(
            DiagnosticKind::Internal,
            DiagnosticCode::VmBytecodeInvariant,
            "diagnostic cause chain exceeded depth limit".to_string(),
        );
    }
    let heap = interp.gc_heap();
    let Some(obj) = value.as_object() else {
        return Diagnostic::new(
            DiagnosticKind::Type,
            DiagnosticCode::Uncaught,
            value.display_string(heap),
        );
    };

    let name = otter_vm::object::get(obj, heap, "name")
        .map(|v| v.display_string(heap))
        .unwrap_or_else(|| "Error".to_string());
    let message = otter_vm::object::get(obj, heap, "message")
        .map(|v| v.display_string(heap))
        .unwrap_or_default();
    let full = if message.is_empty() {
        name.clone()
    } else {
        format!("{name}: {message}")
    };
    let (kind, code) = vm_error_kind_and_code_from_name(&name);
    let mut diag = Diagnostic::new(kind, code, full);

    // `cause`: recurse.
    if let Some(cause) = otter_vm::object::get(obj, heap, "cause") {
        diag = diag.with_cause(diagnostic_from_thrown_value(interp, &cause, depth + 1));
    }

    // `errors`: AggregateError. Each entry becomes an aggregated
    // diagnostic; recurse one level so nested causes inside each
    // error survive.
    if let Some(errors) = otter_vm::object::get(obj, heap, "errors").and_then(|v| v.as_array()) {
        let entries: Vec<Diagnostic> = otter_vm::array::with_elements(errors, heap, |slice| {
            slice
                .iter()
                .map(|v| diagnostic_from_thrown_value(interp, v, depth + 1))
                .collect()
        });
        if !entries.is_empty() {
            diag = diag.with_aggregated_errors(entries);
        }
    }

    diag
}

/// Map a thrown Error's `.name` to the matching
/// [`DiagnosticKind`] / [`DiagnosticCode`] pair.
fn vm_error_kind_and_code_from_name(name: &str) -> (DiagnosticKind, DiagnosticCode) {
    match name {
        "TypeError" => (DiagnosticKind::Type, DiagnosticCode::TypeError),
        "RangeError" => (DiagnosticKind::Range, DiagnosticCode::StackOverflow),
        "SyntaxError" => (DiagnosticKind::Syntax, DiagnosticCode::SyntaxError),
        "ReferenceError" => (DiagnosticKind::Reference, DiagnosticCode::Tdz),
        _ => (DiagnosticKind::Type, DiagnosticCode::Uncaught),
    }
}

/// Enrich a runtime diagnostic with the `cause` chain and
/// `errors` aggregated entries from the original thrown JS value,
/// when available.
pub(super) fn enrich_runtime_diagnostic_with_cause(
    interp: &mut otter_vm::Interpreter,
    err: OtterError,
) -> OtterError {
    let OtterError::Runtime { mut diagnostic } = err else {
        return err;
    };
    let Some(thrown) = interp.take_pending_uncaught_throw() else {
        return OtterError::Runtime { diagnostic };
    };
    // Walk the throw value once and attach both `cause` and
    // `aggregated_errors` directly onto the outer diagnostic.
    let chain = diagnostic_from_thrown_value(interp, &thrown, 0);
    diagnostic.cause = chain.cause;
    diagnostic.aggregated_errors = chain.aggregated_errors;
    OtterError::Runtime { diagnostic }
}

pub(super) fn map_vm_error(run_err: otter_vm::RunError) -> OtterError {
    use otter_vm::{ErrorDetail, VmError};
    // Render the message before destructuring moves `detail` out of `run_err`.
    let detail_message = run_err.message();
    let otter_vm::RunError {
        error,
        frames,
        detail,
    } = run_err;
    let stack_frames: Vec<StackFrame> = frames
        .into_iter()
        .map(|f| StackFrame {
            function: f.function_name,
            module: f.module,
            span: Some(f.span),
            source_position: f.source_position,
        })
        .collect();
    let top_span = stack_frames.first().and_then(|f| f.span);
    let display = error.to_string();
    let runtime_diagnostic =
        |kind: DiagnosticKind, code: DiagnosticCode, message: String| OtterError::Runtime {
            diagnostic: Box::new(Diagnostic {
                kind,
                code: code.as_str().to_string(),
                message,
                source_url: stack_frames.first().map(|frame| frame.module.clone()),
                range: top_span,
                span: top_span,
                help: None,
                frames: stack_frames.clone(),
                cause: None,
                aggregated_errors: Vec::new(),
            }),
        };
    match error {
        VmError::ResourceLimit => match detail {
            Some(ErrorDetail::Resource(error)) => OtterError::Resource { error },
            _ => OtterError::Internal {
                code: DiagnosticCode::VmBytecodeInvariant.as_str().to_owned(),
                message: "ResourceLimit is missing its exact resource detail".to_owned(),
            },
        },
        VmError::Interrupted => OtterError::Interrupted,
        VmError::OutOfMemory {
            requested_bytes,
            heap_limit_bytes,
        } => OtterError::OutOfMemory {
            requested_bytes,
            heap_limit_bytes,
        },
        VmError::BudgetExceeded => runtime_diagnostic(
            DiagnosticKind::Timeout,
            DiagnosticCode::BudgetExceeded,
            detail_message,
        ),
        VmError::TypeMismatch => {
            runtime_diagnostic(DiagnosticKind::Type, DiagnosticCode::TypeMismatch, display)
        }
        VmError::TypeError | VmError::TypeMismatchAt => runtime_diagnostic(
            DiagnosticKind::Type,
            DiagnosticCode::TypeError,
            detail_message,
        ),
        VmError::SyntaxError => runtime_diagnostic(
            DiagnosticKind::Syntax,
            DiagnosticCode::SyntaxError,
            detail_message,
        ),
        VmError::UnknownIntrinsic => runtime_diagnostic(
            DiagnosticKind::Type,
            DiagnosticCode::UnknownMethod,
            detail_message,
        ),
        VmError::TemporalDeadZone { local_index } => runtime_diagnostic(
            DiagnosticKind::Reference,
            DiagnosticCode::Tdz,
            format!("cannot access local {local_index} before initialization"),
        ),
        VmError::ThisUninitialized => runtime_diagnostic(
            DiagnosticKind::Reference,
            DiagnosticCode::Tdz,
            detail_message,
        ),
        VmError::StackOverflow { limit } => runtime_diagnostic(
            DiagnosticKind::Range,
            DiagnosticCode::StackOverflow,
            format!("maximum call stack size exceeded (limit {limit})"),
        ),
        VmError::NotCallable => runtime_diagnostic(
            DiagnosticKind::Type,
            DiagnosticCode::NotCallable,
            "value is not a function".to_string(),
        ),
        VmError::Uncaught => runtime_diagnostic(
            DiagnosticKind::Type,
            DiagnosticCode::Uncaught,
            detail_message,
        ),
        VmError::JsonError => {
            // `code` is `&'static str` from the VM JSON path (every
            // value is one of the `JSON_*` codes in the closed
            // [`DiagnosticCode`] set). Parse it back through
            // `DiagnosticCode::parse` so the diagnostic still
            // carries a typed code in the closed set.
            let (code, message) = match &detail {
                Some(ErrorDetail::Json(p)) => (p.code, p.message.clone()),
                _ => ("JSON_BAD_ARG", detail_message),
            };
            let typed = DiagnosticCode::parse(code).unwrap_or(DiagnosticCode::JsonBadArg);
            runtime_diagnostic(DiagnosticKind::Type, typed, message)
        }
        VmError::InvalidRegExp => runtime_diagnostic(
            DiagnosticKind::Syntax,
            DiagnosticCode::InvalidRegexp,
            detail_message,
        ),
        VmError::MissingReturn | VmError::InvalidOperand => OtterError::Internal {
            code: DiagnosticCode::VmBytecodeInvariant.as_str().to_string(),
            message: display,
        },
        _ => OtterError::Internal {
            code: DiagnosticCode::VmUnknown.as_str().to_string(),
            message: display,
        },
    }
}

pub(super) fn map_native_error(err: otter_vm::NativeError) -> OtterError {
    match err {
        otter_vm::NativeError::Resource { error } => OtterError::Resource { error },
        otter_vm::NativeError::ExecutionFailure(failure) => {
            if failure.is_fatal() {
                map_vm_error(failure)
            } else {
                map_vm_error(otter_vm::RunError::bare(otter_vm::VmError::InvalidOperand))
            }
        }
        otter_vm::NativeError::MissingReturn => {
            map_vm_error(otter_vm::RunError::bare(otter_vm::VmError::MissingReturn))
        }
        otter_vm::NativeError::InvalidOperand => {
            map_vm_error(otter_vm::RunError::bare(otter_vm::VmError::InvalidOperand))
        }
        otter_vm::NativeError::BudgetExceeded { reason } => map_vm_error(otter_vm::RunError {
            error: otter_vm::VmError::BudgetExceeded,
            frames: Vec::new(),
            detail: Some(otter_vm::ErrorDetail::Message(reason.into())),
        }),
        otter_vm::NativeError::OutOfMemory {
            requested_bytes,
            heap_limit_bytes,
            ..
        } => OtterError::OutOfMemory {
            requested_bytes,
            heap_limit_bytes,
        },
        otter_vm::NativeError::Interrupted => OtterError::Interrupted,
        otter_vm::NativeError::Exit { code } => OtterError::Runtime {
            diagnostic: Box::new(Diagnostic {
                kind: DiagnosticKind::Type,
                code: DiagnosticCode::Uncaught.as_str().to_string(),
                message: format!("native function requested process exit with code {code}"),
                source_url: None,
                range: None,
                span: None,
                help: None,
                frames: Vec::new(),
                cause: None,
                aggregated_errors: Vec::new(),
            }),
        },
        other => OtterError::Runtime {
            diagnostic: Box::new(Diagnostic {
                kind: DiagnosticKind::Type,
                code: DiagnosticCode::TypeError.as_str().to_string(),
                message: other.to_string(),
                source_url: None,
                range: None,
                span: None,
                help: None,
                frames: Vec::new(),
                cause: None,
                aggregated_errors: Vec::new(),
            }),
        },
    }
}
