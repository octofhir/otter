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
//! - [`with_program_timing`] — the opt-in parse timing surface.
//! - [`SyntaxError`] — concrete error returned when OXC reports
//!   parser diagnostics.
//!
//! # Invariants
//! - We never re-emit JS source and re-parse, with one exception: a
//!   sloppy Script-goal CallExpression assignment target (web-compat
//!   §13.15.1 host exemption) is a fatal OXC parse error, so the
//!   source is retried once per site with the target rewritten to a
//!   member store on [`INVALID_ASSIGNMENT_TARGET_PROPERTY`], which
//!   `otter-compiler` lowers to the runtime ReferenceError.
//! - All `Span` values returned through this crate point into the
//!   source string the callback's `Program` was parsed from.
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
/// callback form borrows the caller's source directly and keeps the OXC
/// allocator alive for the exact lifetime of the borrowed [`Program`] without
/// exposing a reparse-capable wrapper or cloning the source text.
///
/// # Errors
/// Returns a [`SyntaxError`] when OXC reports parse diagnostics.
pub fn with_program<R>(
    source: &str,
    kind: SourceKind,
    f: impl for<'a> FnOnce(&'a Program<'a>) -> R,
) -> Result<R, SyntaxError> {
    with_program_goal(source, kind, SourceGoal::Module, f)
}

/// Parse `source` once and return both the callback result and parser time.
///
/// This opt-in surface exists for phase-level benchmark evidence. The duration
/// covers allocator setup and OXC parsing, ending before the callback performs
/// AST analysis or bytecode lowering. Ordinary compilation continues to use
/// [`with_program`] and does not read the clock.
///
/// # Errors
/// Returns a [`SyntaxError`] when OXC reports parse diagnostics.
pub fn with_program_timing<R>(
    source: &str,
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
    source: &str,
    kind: SourceKind,
    goal: SourceGoal,
    f: impl for<'a> FnOnce(&'a Program<'a>) -> R,
) -> Result<R, SyntaxError> {
    with_program_goal_after_parse(source, kind, goal, || (), f).map(|(result, ())| result)
}

fn with_program_goal_after_parse<R, T>(
    source: &str,
    kind: SourceKind,
    goal: SourceGoal,
    after_parse: impl FnOnce() -> T,
    f: impl for<'a> FnOnce(&'a Program<'a>) -> R,
) -> Result<(R, T), SyntaxError> {
    let mut patched: Option<String> = None;
    let mut attempts = 0usize;
    loop {
        let src = patched.as_deref().unwrap_or(source);
        let allocator = Allocator::default();
        let parser =
            Parser::new(&allocator, src, kind.to_oxc_with_goal(goal)).with_options(ParseOptions {
                parse_regular_expression: true,
                ..Default::default()
            });
        let ret = parser.parse();
        if ret.diagnostics.is_empty() {
            let parse_metadata = after_parse();
            return Ok((f(&ret.program), parse_metadata));
        }
        // §13.15.1 web-compat host exemption — a sloppy Script-goal
        // CallExpression assignment target is a RUNTIME ReferenceError,
        // but OXC reports it as a fatal parse error with no AST. Retry
        // with the call target rewritten to a member store on the
        // synthetic property; the compiler lowers that member to
        // "evaluate the call, then throw ReferenceError" (and back to
        // the early SyntaxError in strict code). One site per retry —
        // the parser stops at the first fatal error.
        if goal == SourceGoal::Script
            && attempts < MAX_CALL_TARGET_PATCHES
            && let Some(next) = patch_call_assignment_target(src, kind, &ret.diagnostics)
        {
            patched = Some(next);
            attempts += 1;
            continue;
        }
        let _ = after_parse();
        return Err(SyntaxError::from_oxc(&ret.diagnostics));
    }
}

/// Synthetic member name the web-compat retry writes over a sloppy
/// CallExpression assignment target. `otter-compiler` recognizes it in
/// every assignment-target position and lowers "evaluate the call, then
/// throw ReferenceError" (strict code keeps the early SyntaxError).
pub const INVALID_ASSIGNMENT_TARGET_PROPERTY: &str = "__otter_invalid_assignment_target__";

/// Retry bound for [`patch_call_assignment_target`] — each fatal parse
/// reveals at most one further call-target site.
const MAX_CALL_TARGET_PATCHES: usize = 32;

/// When the first fatal diagnostic is OXC's invalid-assignment error and
/// its span is a plain CallExpression (§13.15.1 web-compat shape — not an
/// optional chain, not a primary expression), return the source with that
/// span rewritten to `(<call>).__otter_invalid_assignment_target__`.
fn patch_call_assignment_target(
    source: &str,
    kind: SourceKind,
    diagnostics: &[oxc_diagnostics::OxcDiagnostic],
) -> Option<String> {
    let diagnostic = diagnostics
        .iter()
        .find(|diagnostic| diagnostic.to_string() == "Cannot assign to this expression")?;
    let label = diagnostic.labels.as_slice().first()?;
    let start = label.offset() as usize;
    let end = start.checked_add(label.len() as usize)?;
    let target = source.get(start..end)?;
    if !snippet_is_plain_call(target, kind) {
        return None;
    }
    // §13.15.1 — the web-compat exemption covers plain and compound
    // assignment, updates, and for-in/of heads, but NOT logical
    // assignment: `f() &&= v` stays an early SyntaxError.
    if next_operator_is_logical_assignment(&source[end..]) {
        return None;
    }
    let mut next = String::with_capacity(source.len() + target.len() + 48);
    next.push_str(&source[..start]);
    next.push('(');
    next.push_str(target);
    next.push_str(").");
    next.push_str(INVALID_ASSIGNMENT_TARGET_PROPERTY);
    next.push_str(&source[end..]);
    Some(next)
}

/// `true` when the source following an assignment target begins —
/// after whitespace and comments — with a logical-assignment operator
/// (`&&=`, `||=`, `??=`), which the §13.15.1 web-compat exemption does
/// not cover. A token-level peek, not a parse: only trivia is skipped.
fn next_operator_is_logical_assignment(rest: &str) -> bool {
    let bytes = rest.as_bytes();
    let mut i = 0usize;
    while i < bytes.len() {
        match bytes[i] {
            b' ' | b'\t' | b'\r' | b'\n' | 0x0B | 0x0C => i += 1,
            b'/' if bytes.get(i + 1) == Some(&b'/') => {
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                }
            }
            b'/' if bytes.get(i + 1) == Some(&b'*') => match rest[i + 2..].find("*/") {
                Some(close) => i += 2 + close + 2,
                None => return false,
            },
            // The diagnostic span excludes wrapping parentheses:
            // `(f()) &&= 1` reports only `f()`. Closing parens are
            // part of the same target, so step over them.
            b')' => i += 1,
            _ => break,
        }
    }
    let tail = &rest[i.min(rest.len())..];
    tail.starts_with("&&=") || tail.starts_with("||=") || tail.starts_with("??=")
}

/// `true` when `snippet` parses on its own as exactly one expression
/// statement whose expression — modulo parentheses — is a plain (non-
/// optional-chain) CallExpression.
fn snippet_is_plain_call(snippet: &str, kind: SourceKind) -> bool {
    let allocator = Allocator::default();
    let parser = Parser::new(
        &allocator,
        snippet,
        kind.to_oxc_with_goal(SourceGoal::Script),
    )
    .with_options(ParseOptions {
        parse_regular_expression: true,
        ..Default::default()
    });
    let ret = parser.parse();
    if !ret.diagnostics.is_empty() || ret.program.body.len() != 1 {
        return false;
    }
    let oxc_ast::ast::Statement::ExpressionStatement(stmt) = &ret.program.body[0] else {
        return false;
    };
    let mut expr = &stmt.expression;
    while let oxc_ast::ast::Expression::ParenthesizedExpression(paren) = expr {
        expr = &paren.expression;
    }
    matches!(expr, oxc_ast::ast::Expression::CallExpression(call) if !call.optional)
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
        let source = String::from("undefined;");
        let len = with_program(source.as_str(), SourceKind::TypeScript, |program| {
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
