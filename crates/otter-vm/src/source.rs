//! JS source compilation entry points.
//!
//! Thin façade over [`crate::source_compiler::ModuleCompiler`]. Exists so
//! callers outside of `otter-vm` can write `otter_vm::source::compile_script`
//! without reaching into the compiler module, and so the shim can route to
//! the right [`SourceType`] for each entry kind (script vs module vs eval).
//!
//! All four entry points return the same error type,
//! [`SourceLoweringError`]. While the source compiler is at the M0 stub,
//! every call fails with `SourceLoweringError::Unsupported { construct:
//! "program", .. }` — this is the contract and not a bug.

pub use crate::source_compiler::SourceLoweringError;

use oxc_span::SourceType;

use crate::module::Module;
use crate::source_compiler::ModuleCompiler;

/// Parse, lower, and compile a script into a [`Module`].
///
/// Scripts run in the global scope; top-level `var` declarations bind on
/// the global object, top-level `let`/`const` bind in the global lexical
/// environment. Use [`compile_module`] for ES-module semantics
/// (imports/exports, strict by default).
///
/// # Errors
///
/// Returns `SourceLoweringError::Unsupported` until the compiler grows
/// past M0. See `V2_MIGRATION.md` for the staged rollout.
pub fn compile_script(source: &str, source_url: &str) -> Result<Module, SourceLoweringError> {
    ModuleCompiler::new().compile(source, source_url, SourceType::default())
}

/// Parse, lower, and compile `source` as an ES module (`SourceType::mjs`).
///
/// Imports/exports are required; top-level code is always strict.
///
/// # Errors
///
/// See [`compile_script`] for the current contract.
pub fn compile_module(source: &str, source_url: &str) -> Result<Module, SourceLoweringError> {
    ModuleCompiler::new().compile(source, source_url, SourceType::mjs())
}

/// Parse, lower, and compile `source` in eval mode.
///
/// The compiler returns the completion value of the last expression
/// statement so the CLI `-p` switch and the runtime `eval()` helper can
/// print it.
///
/// # Errors
///
/// See [`compile_script`] for the current contract.
pub fn compile_eval(source: &str, source_url: &str) -> Result<Module, SourceLoweringError> {
    // Eval inherits the surrounding SourceType; we approximate with the
    // classic-script default. A dedicated eval mode will land with the
    // broader eval/$262 work post-M1.
    ModuleCompiler::new().compile_eval(source, source_url, SourceType::default())
}

/// Eval mode variant used by field initializers (§B.3.5.2). Same contract
/// as [`compile_eval`] while the compiler is in M0.
pub fn compile_eval_field_init(
    source: &str,
    source_url: &str,
) -> Result<Module, SourceLoweringError> {
    ModuleCompiler::new().compile_eval(source, source_url, SourceType::default())
}
