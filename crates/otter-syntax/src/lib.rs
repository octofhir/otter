//! OXC-only frontend for the new Otter engine.
//!
//! All JavaScript and TypeScript parsing in the active engine goes through
//! OXC. This crate is the only place in `crates/*` that
//! depends on `oxc_parser` directly: every other crate consumes the
//! parsed AST through this surface.
//!
//! # Contents
//! - [`SourceKind`] — JavaScript / TypeScript flavor selector.
//! - [`detect_source_kind`] — decide kind from file extension.
//! - [`Parsed`] — owns an [`oxc_allocator::Allocator`] plus the
//!   resulting [`oxc_ast::ast::Program`] (lifetime-bound to the
//!   allocator).
//! - [`parse`] — parse a string with an explicit [`SourceKind`].
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

use std::path::Path;

use oxc_allocator::Allocator;
use oxc_ast::ast::Program;
use oxc_parser::{ParseOptions, Parser};
use oxc_span::SourceType;
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Source-language flavor.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SourceKind {
    /// JavaScript: `.js`, `.mjs`, `.cjs`.
    JavaScript,
    /// TypeScript: `.ts`, `.mts`, `.cts`.
    TypeScript,
}

impl SourceKind {
    /// Translate to OXC's `SourceType`.
    #[must_use]
    pub fn to_oxc(self) -> SourceType {
        match self {
            SourceKind::JavaScript => SourceType::default(),
            SourceKind::TypeScript => SourceType::default().with_typescript(true),
        }
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
        "ts" | "mts" | "cts" => SourceKind::TypeScript,
        _ => return None,
    })
}

/// Result of [`parse`]: owns its allocator so the AST stays alive.
pub struct Parsed {
    /// Bump-allocated arena that owns every AST node.
    pub allocator: Allocator,
    /// Source text the AST refers into.
    pub source: String,
    /// Source kind used for parsing.
    pub kind: SourceKind,
}

impl std::fmt::Debug for Parsed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Parsed")
            .field("kind", &self.kind)
            .field("source_len", &self.source.len())
            .finish()
    }
}

impl Parsed {
    /// Parse the program back out of the allocator.
    ///
    /// Re-parses on each call: cheap because OXC is fast, and lets
    /// the caller hold a `&Program` with the allocator's lifetime.
    /// In practice the compiler calls this once.
    ///
    /// # Errors
    /// Returns a [`SyntaxError`] when OXC reports parse diagnostics.
    pub fn program<'a>(&'a self) -> Result<Program<'a>, SyntaxError> {
        let parser = Parser::new(&self.allocator, &self.source, self.kind.to_oxc()).with_options(
            ParseOptions {
                parse_regular_expression: true,
                ..Default::default()
            },
        );
        let ret = parser.parse();
        if !ret.errors.is_empty() {
            return Err(SyntaxError {
                messages: ret.errors.iter().map(|e| e.to_string()).collect(),
            });
        }
        Ok(ret.program)
    }
}

/// Parse `source` with the given [`SourceKind`].
///
/// # Errors
/// Returns a [`SyntaxError`] when OXC reports parse diagnostics.
pub fn parse(source: impl Into<String>, kind: SourceKind) -> Result<Parsed, SyntaxError> {
    let parsed = Parsed {
        allocator: Allocator::default(),
        source: source.into(),
        kind,
    };
    // Validate by parsing once.
    {
        let parser = Parser::new(&parsed.allocator, &parsed.source, kind.to_oxc()).with_options(
            ParseOptions {
                parse_regular_expression: true,
                ..Default::default()
            },
        );
        let ret = parser.parse();
        if !ret.errors.is_empty() {
            return Err(SyntaxError {
                messages: ret.errors.iter().map(|e| e.to_string()).collect(),
            });
        }
    }
    Ok(parsed)
}

/// Parse `source` once and pass the AST to `f`.
///
/// Use this on hot compile paths that only need to consume the AST once. It
/// avoids the validation parse performed by [`parse`] plus the later
/// [`Parsed::program`] parse.
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
        return Err(SyntaxError {
            messages: ret.errors.iter().map(|e| e.to_string()).collect(),
        });
    }
    Ok(f(&ret.program))
}

/// Errors produced by the OXC frontend.
#[derive(Debug, Clone, Error, Serialize, Deserialize)]
#[error("syntax error: {}", .messages.join("; "))]
pub struct SyntaxError {
    /// One message per parser diagnostic.
    pub messages: Vec<String>,
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
            detect_source_kind(Path::new("x.js")),
            Some(SourceKind::JavaScript)
        );
        assert_eq!(detect_source_kind(Path::new("x.foo")), None);
    }

    #[test]
    fn parses_empty_typescript() {
        let parsed = parse("", SourceKind::TypeScript).unwrap();
        let program = parsed.program().unwrap();
        assert!(program.body.is_empty());
    }

    #[test]
    fn parses_undefined_literal_typescript() {
        let parsed = parse("undefined;", SourceKind::TypeScript).unwrap();
        assert_eq!(parsed.program().unwrap().body.len(), 1);
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
    fn rejects_garbage() {
        let err = parse("@@@@", SourceKind::TypeScript).unwrap_err();
        assert!(!err.messages.is_empty());
    }

    #[test]
    fn with_program_rejects_garbage() {
        let err = with_program("@@@@", SourceKind::TypeScript, |_| ()).unwrap_err();
        assert!(!err.messages.is_empty());
    }
}
