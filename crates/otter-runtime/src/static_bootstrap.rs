//! Admission of build-produced classic-script extension bundles.
//!
//! # Contents
//! - `ExtensionJs` owns one source/code/defined-name bundle.
//! - `prepare` shares exact hook, verifier, source and link admission between
//!   default construction and additional-realm installation.
//!
//! # Invariants
//! Each extension is one script; separate extensions keep declaration order.
//! A configured compile hook receives the original complete source and wins
//! over embedded default bytecode. Every bytecode path verifies before linking,
//! and every linked body owns the actual source registry and active realm. The
//! source registers as program-image text, never copied.
//!
//! # See also
//! - `crate::script_source` for immutable source admission.
//! - `otter_bytecode::binary` for the current bytecode codec.

use crate::{
    DiagnosticCode, OtterError, RuntimeCompileRequest, RuntimeHooks, module_loader, script_source,
};
use otter_compiler::CompiledModuleMetadata;
use otter_vm::{ExecutionContext, Interpreter};

/// One build-produced classic script attached to an extension.
///
/// The product build compiles `source` as one script and emits `bytecode` using
/// the current bounded codec. Both representations and the declared names
/// belong to this one static descriptor. Runtime hooks still compile `source`.
#[derive(Debug, Clone, Copy)]
pub struct ExtensionJs {
    /// Complete original source, including separators between constituent files.
    pub source: &'static str,
    /// Current-codec bytecode the product build emitted from that exact source
    /// and verified; it is decoded without repeating the verifier.
    pub bytecode: &'static [u8],
    /// Global names defined by the bundle, in declaration order.
    pub defines: &'static [&'static str],
}

pub(crate) fn prepare(
    interp: &mut Interpreter,
    hooks: &RuntimeHooks,
    script: &ExtensionJs,
    specifier: &str,
) -> Result<(ExecutionContext, CompiledModuleMetadata), OtterError> {
    let account = interp.resource_account();
    let text = otter_resource::SharedSource::from_static(script.source);
    if let Some(hook) = hooks.compile_hook() {
        let source = module_loader::ResolvedSource {
            url: specifier.to_owned(),
            kind: otter_syntax::SourceKind::JavaScript,
            jsx: None,
            text,
        };
        let mut compiled = hook.compile(RuntimeCompileRequest { source: &source })?;
        compiled.bytecode.mark_primordial_iteration();
        let sources =
            script_source::script_sources(&compiled.bytecode, source.text, specifier, &account)?;
        let context = interp.link_module(compiled.bytecode, sources)?;
        return Ok((context, compiled.metadata));
    }
    // The build verified these exact bytes; they are part of the binary.
    let mut verified = otter_bytecode::binary::decode_build_artifact(script.bytecode).map_err(|error| {
        OtterError::Internal {
            code: DiagnosticCode::GlobalClassBootstrap.as_str().to_owned(),
            message: format!("invalid static extension bytecode: {error}"),
        }
    })?;
    // Additional-realm installers retain their existing diagnostic URL. URL
    // metadata is outside the structural verifier proof.
    if verified.module().module != specifier {
        verified = verified.with_module_url(specifier);
    }
    let metadata = CompiledModuleMetadata::span_only_from_bytecode_with_budget(
        verified.module(),
        otter_compiler::MAX_COMPILED_METADATA_BYTES,
    )
    .map_err(|error| crate::map_compile_error(error, specifier))?;
    let sources = script_source::script_sources(verified.module(), text, specifier, &account)?;
    let context = interp.link_verified_module(verified, sources)?;
    Ok((context, metadata))
}

#[cfg(test)]
mod tests;
