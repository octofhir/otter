//! OXC-only frontend for the new Otter engine.
//!
//! All JavaScript and TypeScript parsing in the active engine goes through
//! OXC. This crate is the only place in `crates/*` that
//! depends on `oxc_parser` directly: every other crate consumes the
//! parsed AST through this surface.
//!
//! # Contents
//! - [`SourceKind`] — JavaScript / TypeScript / JSX flavor selector.
//! - [`detect_source_kind`] — decide kind from file extension.
//! - [`with_program`] — parse once and consume the AST inside a callback.
//! - [`SyntaxError`] — concrete error returned when OXC reports
//!   parser diagnostics.
//!
//! # Invariants
//! - We never re-emit JS source and re-parse. The OXC AST is
//!   walked in place by `otter-compiler`.
//! - All `Span` values returned through this crate point into the
//!   original source string supplied by the caller.
//!
//! # See also
//! - [Frontend and compilation](../../../docs/book/src/engine/frontend.md)

mod diagnostic;

use std::path::Path;

use oxc_allocator::Allocator;
use oxc_ast::ast::Program;
use oxc_parser::{ParseOptions, Parser};
use oxc_span::SourceType;
use serde::{Deserialize, Serialize};

pub use diagnostic::{SyntaxDiagnostic, SyntaxError};

/// Source-language flavor.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SourceKind {
    /// JavaScript: `.js`, `.mjs`, `.cjs`.
    #[serde(rename = "javascript")]
    JavaScript,
    /// JavaScript with JSX syntax enabled: `.jsx`.
    #[serde(rename = "jsx")]
    JavaScriptJsx,
    /// TypeScript: `.ts`, `.mts`, `.cts`.
    #[serde(rename = "typescript")]
    TypeScript,
    /// TypeScript with JSX syntax enabled: `.tsx`.
    #[serde(rename = "tsx")]
    TypeScriptJsx,
}

impl SourceKind {
    /// Translate to OXC's `SourceType`.
    #[must_use]
    pub fn to_oxc(self) -> SourceType {
        match self {
            SourceKind::JavaScript => SourceType::default(),
            SourceKind::JavaScriptJsx => SourceType::default().with_jsx(true),
            SourceKind::TypeScript => SourceType::default().with_typescript(true),
            SourceKind::TypeScriptJsx => SourceType::default().with_typescript(true).with_jsx(true),
        }
    }

    /// `true` when this source kind enables TypeScript syntax.
    #[must_use]
    pub fn is_typescript(self) -> bool {
        matches!(self, SourceKind::TypeScript | SourceKind::TypeScriptJsx)
    }
}

/// Decide source kind from a file path's extension.
///
/// Returns `None` if the extension is not one of the supported
/// foundation extensions.
#[must_use]
pub fn detect_source_kind(path: &Path) -> Option<SourceKind> {
    let ext = path.extension()?.to_str()?;
    Some(match ext {
        "js" | "mjs" | "cjs" => SourceKind::JavaScript,
        "jsx" => SourceKind::JavaScriptJsx,
        "ts" | "mts" | "cts" => SourceKind::TypeScript,
        "tsx" => SourceKind::TypeScriptJsx,
        _ => return None,
    })
}

/// Parse `source` once and pass the AST to `f`.
///
/// Use this on compile and analysis paths that need to inspect AST state. The
/// callback form keeps the OXC allocator and source text alive for the exact
/// lifetime of the borrowed [`Program`] without exposing a reparse-capable
/// wrapper.
///
/// # Errors
/// Returns a [`SyntaxError`] when OXC reports parse diagnostics.
pub fn with_program<R>(
    source: impl Into<String>,
    kind: SourceKind,
    f: impl for<'a> FnOnce(&'a Program<'a>) -> R,
) -> Result<R, SyntaxError> {
    let allocator = Allocator::default();
    let source = source.into();
    let parser = Parser::new(&allocator, &source, kind.to_oxc()).with_options(ParseOptions {
        parse_regular_expression: true,
        ..Default::default()
    });
    let ret = parser.parse();
    if !ret.errors.is_empty() {
        return Err(SyntaxError::from_oxc(&ret.errors));
    }
    Ok(f(&ret.program))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_typescript_extension() {
        assert_eq!(
            detect_source_kind(Path::new("x.ts")),
            Some(SourceKind::TypeScript)
        );
        assert_eq!(
            detect_source_kind(Path::new("x.tsx")),
            Some(SourceKind::TypeScriptJsx)
        );
        assert_eq!(
            detect_source_kind(Path::new("x.js")),
            Some(SourceKind::JavaScript)
        );
        assert_eq!(
            detect_source_kind(Path::new("x.jsx")),
            Some(SourceKind::JavaScriptJsx)
        );
        assert_eq!(detect_source_kind(Path::new("x.foo")), None);
    }

    #[test]
    fn with_program_parses_empty_typescript() {
        let is_empty = with_program("", SourceKind::TypeScript, |program| {
            program.body.is_empty()
        })
        .unwrap();
        assert!(is_empty);
    }

    #[test]
    fn with_program_parses_undefined_literal_typescript() {
        let len = with_program("undefined;", SourceKind::TypeScript, |program| {
            program.body.len()
        })
        .unwrap();
        assert_eq!(len, 1);
    }

    #[test]
    fn with_program_parses_jsx_and_tsx_sources() {
        assert!(with_program("const x = <div />;", SourceKind::JavaScriptJsx, |_| ()).is_ok());
        assert!(
            with_program(
                "const x: JSX.Element = <div />;",
                SourceKind::TypeScriptJsx,
                |_| ()
            )
            .is_ok()
        );
    }

    #[test]
    fn with_program_parses_once_for_callback_consumers() {
        let len = with_program("undefined;", SourceKind::TypeScript, |program| {
            program.body.len()
        })
        .unwrap();
        assert_eq!(len, 1);
    }

    #[test]
    fn with_program_rejects_garbage() {
        let err = with_program("@@@@", SourceKind::TypeScript, |_| ()).unwrap_err();
        assert!(!err.messages.is_empty());
    }
}
