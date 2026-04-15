//! AST-to-bytecode lowering for the Ignition-style ISA.
//!
//! [`ModuleCompiler`] is the single entry point the rest of the VM uses
//! to turn a JavaScript/TypeScript source string into a
//! [`crate::module::Module`]. It owns the oxc `Allocator` for the
//! current compilation and drives the staged lowering: parse → AST
//! semantics → bytecode emit → `Module`.
//!
//! # Current state (M0)
//!
//! The compiler is a scaffold. Every input — including the empty
//! string — returns
//! [`SourceLoweringError::Unsupported { construct: "program", .. }`].
//! Real AST lowering lands incrementally starting from M1 (see
//! `V2_MIGRATION.md`); the stub contract exists so the rest of the
//! workspace can build, and `otter run foo.js` can fail fast with a
//! user-visible "not supported yet" error rather than silently producing
//! empty bytecode.

mod error;

#[cfg(test)]
mod tests;

pub use error::SourceLoweringError;

use oxc_allocator::Allocator;
use oxc_parser::Parser;
use oxc_span::{SourceType, Span};

use crate::module::Module;

/// Staged AST-to-bytecode compiler for a single source file.
///
/// Construct one `ModuleCompiler` per source file. The compiler walks
/// the parsed AST and, when a construct is recognised, emits the
/// corresponding Ignition bytecode; unrecognised constructs produce a
/// [`SourceLoweringError::Unsupported`].
#[derive(Debug, Default)]
pub struct ModuleCompiler;

impl ModuleCompiler {
    /// Creates a new, empty compiler.
    #[must_use]
    pub const fn new() -> Self {
        Self
    }

    /// Parse and lower `source` into a [`Module`].
    ///
    /// `source_url` is used for diagnostics only — it is not fetched or
    /// resolved. `source_type` controls whether the parser treats the
    /// input as a script, module, or `.ts`/`.tsx` file; the value is
    /// forwarded verbatim to `oxc_parser`.
    ///
    /// # Current behaviour
    ///
    /// Returns `Err(SourceLoweringError::Unsupported { construct:
    /// "program", .. })` for any non-empty parse, and the same error
    /// with a zero span for empty input. Concrete AST coverage is added
    /// per milestone (see `V2_MIGRATION.md`).
    pub fn compile(
        &self,
        source: &str,
        source_url: &str,
        source_type: SourceType,
    ) -> Result<Module, SourceLoweringError> {
        let _ = source_url;
        let allocator = Allocator::default();
        let parser_return = Parser::new(&allocator, source, source_type).parse();

        if !parser_return.errors.is_empty() {
            let diag = &parser_return.errors[0];
            let label_span = diag
                .labels
                .as_ref()
                .and_then(|labels| labels.first())
                .map(|label| {
                    let start = u32::try_from(label.offset()).unwrap_or(0);
                    let length = u32::try_from(label.len()).unwrap_or(0);
                    Span::new(start, start.saturating_add(length))
                })
                .unwrap_or_else(|| Span::new(0, 0));
            return Err(SourceLoweringError::Parse {
                message: diag.message.to_string(),
                span: label_span,
            });
        }

        Err(SourceLoweringError::Unsupported {
            construct: "program",
            span: parser_return.program.span,
        })
    }
}
