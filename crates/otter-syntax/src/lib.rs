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
//! - [`with_program_timing`] — the opt-in Phase 0 parse measurement surface.
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
use std::time::{Duration, Instant};

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

/// Parse goal — ECMA-262 §16.1 Script vs §16.2 Module.
///
/// The goal changes early-error rules at parse time: `await` is a
/// plain identifier in script code, `import` / `export` declarations
/// are script syntax errors, and modules are implicitly strict.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceGoal {
    /// §16.1 Script grammar.
    Script,
    /// §16.2 Module grammar.
    Module,
}

impl SourceKind {
    /// Translate to OXC's `SourceType` with the Module goal.
    #[must_use]
    pub fn to_oxc(self) -> SourceType {
        self.to_oxc_with_goal(SourceGoal::Module)
    }

    /// Translate to OXC's `SourceType` under an explicit parse goal.
    #[must_use]
    pub fn to_oxc_with_goal(self, goal: SourceGoal) -> SourceType {
        let base = match goal {
            SourceGoal::Script => SourceType::default().with_script(true),
            SourceGoal::Module => SourceType::default().with_module(true),
        };
        match self {
            SourceKind::JavaScript => base,
            SourceKind::JavaScriptJsx => base.with_jsx(true),
            SourceKind::TypeScript => base.with_typescript(true),
            SourceKind::TypeScriptJsx => base.with_typescript(true).with_jsx(true),
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

/// Decide source kind from an HTTP `Content-Type` header value.
///
/// Remote modules (http/https) carry no meaningful path extension, so the
/// server-declared media type is authoritative — mirroring Deno's
/// `MediaType::from_content_type`. Parameters (`; charset=…`) are stripped and
/// the bare MIME is matched case-insensitively. Returns `None` for a media type
/// this engine does not classify (the caller falls back to the URL extension,
/// then to a default).
///
/// JSON is intentionally not mapped here: JSON modules take a separate
/// `export default (<text>)` wrapping path, not one of the four script
/// [`SourceKind`]s.
#[must_use]
pub fn source_kind_from_content_type(content_type: &str) -> Option<SourceKind> {
    let mime = content_type
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    Some(match mime.as_str() {
        "application/typescript"
        | "text/typescript"
        | "application/x-typescript"
        | "video/vnd.dlna.mpeg-tts"
        | "video/mp2t" => SourceKind::TypeScript,
        "application/javascript"
        | "text/javascript"
        | "application/ecmascript"
        | "text/ecmascript"
        | "application/x-javascript"
        | "application/node" => SourceKind::JavaScript,
        "text/jsx" | "text/jscript" => SourceKind::JavaScriptJsx,
        "text/tsx" => SourceKind::TypeScriptJsx,
        _ => return None,
    })
}

/// Decide the source kind of a remote module from its response
/// `Content-Type` and its (possibly post-redirect) URL.
///
/// Precedence follows Deno: the declared media type wins; a generic or absent
/// type falls back to the URL's path extension; anything still unknown defaults
/// to JavaScript, since a bare CDN specifier such as `https://esm.sh/hono` is
/// overwhelmingly ECMAScript.
#[must_use]
pub fn remote_source_kind(content_type: Option<&str>, url: &str) -> SourceKind {
    if let Some(ct) = content_type
        && let Some(kind) = source_kind_from_content_type(ct)
    {
        return kind;
    }
    // Fall back to the extension of the URL path (ignoring any query/fragment).
    let path = url.split(['?', '#']).next().unwrap_or(url);
    detect_source_kind(Path::new(path)).unwrap_or(SourceKind::JavaScript)
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
    with_program_goal(source, kind, SourceGoal::Module, f)
}

/// Parse `source` once and return both the callback result and parser time.
///
/// This opt-in surface exists for phase-level benchmark evidence. The duration
/// covers allocator/source setup and OXC parsing, ending before the callback
/// performs AST analysis or bytecode lowering. Ordinary compilation continues
/// to use [`with_program`] and does not read the clock.
///
/// # Errors
/// Returns a [`SyntaxError`] when OXC reports parse diagnostics.
pub fn with_program_timing<R>(
    source: impl Into<String>,
    kind: SourceKind,
    f: impl for<'a> FnOnce(&'a Program<'a>) -> R,
) -> Result<(R, Duration), SyntaxError> {
    let started = Instant::now();
    with_program_goal_after_parse(source, kind, SourceGoal::Module, || started.elapsed(), f)
}

/// Parse `source` once under an explicit [`SourceGoal`] and pass the
/// AST to `f`.
///
/// Script-goal compilation entry points (classic scripts, `eval` /
/// `new Function` bodies per §19.2.1.1) use this so script-only
/// grammar — `await` as an identifier, `import` / `export` as syntax
/// errors — is enforced at parse time. Module pipelines keep
/// [`with_program`].
///
/// # Errors
/// Returns a [`SyntaxError`] when OXC reports parse diagnostics.
pub fn with_program_goal<R>(
    source: impl Into<String>,
    kind: SourceKind,
    goal: SourceGoal,
    f: impl for<'a> FnOnce(&'a Program<'a>) -> R,
) -> Result<R, SyntaxError> {
    with_program_goal_after_parse(source, kind, goal, || (), f).map(|(result, ())| result)
}

fn with_program_goal_after_parse<R, T>(
    source: impl Into<String>,
    kind: SourceKind,
    goal: SourceGoal,
    after_parse: impl FnOnce() -> T,
    f: impl for<'a> FnOnce(&'a Program<'a>) -> R,
) -> Result<(R, T), SyntaxError> {
    let allocator = Allocator::default();
    let source = source.into();
    let parser =
        Parser::new(&allocator, &source, kind.to_oxc_with_goal(goal)).with_options(ParseOptions {
            parse_regular_expression: true,
            ..Default::default()
        });
    let ret = parser.parse();
    let parse_metadata = after_parse();
    if !ret.diagnostics.is_empty() {
        return Err(SyntaxError::from_oxc(&ret.diagnostics));
    }
    Ok((f(&ret.program), parse_metadata))
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
    fn content_type_maps_media_types() {
        assert_eq!(
            source_kind_from_content_type("application/javascript"),
            Some(SourceKind::JavaScript)
        );
        // Parameters are stripped and matching is case-insensitive.
        assert_eq!(
            source_kind_from_content_type("text/JavaScript; charset=utf-8"),
            Some(SourceKind::JavaScript)
        );
        assert_eq!(
            source_kind_from_content_type("application/typescript"),
            Some(SourceKind::TypeScript)
        );
        assert_eq!(
            source_kind_from_content_type("text/tsx"),
            Some(SourceKind::TypeScriptJsx)
        );
        assert_eq!(
            source_kind_from_content_type("text/jsx"),
            Some(SourceKind::JavaScriptJsx)
        );
        assert_eq!(source_kind_from_content_type("text/html"), None);
    }

    #[test]
    fn remote_kind_prefers_content_type_then_extension_then_default() {
        // Content-Type wins over an extensionless URL.
        assert_eq!(
            remote_source_kind(Some("application/typescript"), "https://esm.sh/hono@4"),
            SourceKind::TypeScript
        );
        // Unknown/absent Content-Type falls back to the URL extension.
        assert_eq!(
            remote_source_kind(Some("text/plain"), "https://x/a.tsx?v=1"),
            SourceKind::TypeScriptJsx
        );
        assert_eq!(
            remote_source_kind(None, "https://x/a.mjs"),
            SourceKind::JavaScript
        );
        // Bare extensionless CDN specifier defaults to JavaScript.
        assert_eq!(
            remote_source_kind(None, "https://esm.sh/hono@4"),
            SourceKind::JavaScript
        );
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
    fn timed_parse_keeps_callback_outside_parse_duration() {
        let (statements, _duration) =
            with_program_timing("const value = 1;", SourceKind::JavaScript, |program| {
                program.body.len()
            })
            .expect("parse");
        assert_eq!(statements, 1);
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
