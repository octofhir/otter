//! `JsRuntimeDiagnostic` — `miette::Diagnostic` for uncaught JS throws.
//!
//! When a script terminates with an uncaught throw, the CLI used to print a
//! plain `RuntimeError: TypeError: foo` line and exit. This module promotes
//! that error into a structured diagnostic the CLI can render with miette's
//! fancy reporter — V8/Node-style stack header, source snippet with `^`
//! pointing at the offending span, and (eventually) help/footer rendered from
//! the captured frame metadata.
//!
//! The diagnostic is **source-language agnostic**. By Phase 1 the VM
//! `SourceMap` already records original-source `(line, column)` (TS or JS),
//! and `Module::source_text` already holds the original source byte-for-byte.
//! This module simply composes the two to produce a snippet.
//!
//! Spec references:
//!   - V8 stack trace API:  <https://v8.dev/docs/stack-trace-api>
//!   - §6.2.5 Execution contexts: <https://tc39.es/ecma262/#sec-execution-contexts>

use std::fmt;
use std::sync::Arc;

use miette::{Diagnostic, LabeledSpan, NamedSource, Severity, SourceCode};

use otter_vm::interpreter::{InterpreterError, RuntimeState};
use otter_vm::module::Function;
use otter_vm::object::ObjectHandle;
use otter_vm::source_map::SourceLocation;
use otter_vm::stack_frame::{StackFrameInfo, format_v8_stack};

/// One captured frame, projected from a `StackFrameInfo` plus its module's
/// source text. Stored owned so the diagnostic can outlive the runtime.
#[derive(Debug, Clone)]
pub struct DiagnosticFrame {
    /// Display name of the function (`<anonymous>` when missing).
    pub function_name: String,
    /// Module URL/path used in the stack header.
    pub module_url: String,
    /// 1-based source location, when the function had a source-map entry.
    pub location: Option<SourceLocation>,
    /// Original source text for this frame's module, if any. Lets us render
    /// snippets even when the failing frame is in a different file than the
    /// top frame.
    pub source_text: Option<Arc<str>>,
    /// `at async fn` flag — surfaces in the stack header.
    pub is_async: bool,
    /// `[[Construct]]` flag — surfaces as `at new` in the stack header.
    pub is_construct: bool,
    /// Native (Rust) frame: no source location, no snippet.
    pub is_native: bool,
}

impl DiagnosticFrame {
    fn from_stack_frame(frame: &StackFrameInfo) -> Self {
        let location = frame
            .module
            .function(frame.function_index)
            .and_then(|function: &Function| function.source_map().lookup(frame.pc));
        Self {
            function_name: frame.display_name().to_string(),
            module_url: frame.module_url().to_string(),
            location,
            source_text: frame.module.source_text().cloned(),
            is_async: frame.is_async,
            is_construct: frame.is_construct,
            is_native: frame.is_native,
        }
    }
}

/// Structured runtime diagnostic for an uncaught JS throw.
///
/// Implements `miette::Diagnostic` so the CLI can render a snippet with `^`
/// at the throw site, plus an `Error: name: message` header followed by the
/// V8-style call stack.
#[derive(Debug)]
pub struct JsRuntimeDiagnostic {
    /// Pre-rendered V8/Node-style stack string. Used as the `Display` impl
    /// so callers that don't go through miette still see a stack.
    rendered_stack: String,
    /// Error name (e.g. `TypeError`) for the header.
    name: String,
    /// Error message for the header.
    message: String,
    /// Top-frame source projection used by `source_code()` and `labels()`.
    /// `None` when the top frame has no module source text or no resolvable
    /// location (e.g., a thrown non-Error string from native code).
    top_snippet: Option<TopFrameSnippet>,
    /// All captured frames, top-down (most recent caller first).
    frames: Vec<DiagnosticFrame>,
}

#[derive(Debug)]
struct TopFrameSnippet {
    named_source: NamedSource<String>,
    /// Byte offset into the source text for the `^` underline.
    label_offset: usize,
    /// Byte length of the `^` underline. Always at least 1.
    label_len: usize,
}

impl JsRuntimeDiagnostic {
    /// Returns the rendered V8-style stack as a `&str` for tests / verbose
    /// outputs that need the textual form without re-rendering through miette.
    #[must_use]
    pub fn rendered_stack(&self) -> &str {
        &self.rendered_stack
    }

    /// Returns the captured frames in top-down order (most recent caller
    /// first). Useful for the test262 runner which wants to inspect frames
    /// without going through the miette report.
    #[must_use]
    pub fn frames(&self) -> &[DiagnosticFrame] {
        &self.frames
    }

    /// Returns the error name (`TypeError`, `RangeError`, etc.).
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Returns the error message string.
    #[must_use]
    pub fn message(&self) -> &str {
        &self.message
    }
}

impl fmt::Display for JsRuntimeDiagnostic {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.rendered_stack)
    }
}

impl std::error::Error for JsRuntimeDiagnostic {}

impl Diagnostic for JsRuntimeDiagnostic {
    fn severity(&self) -> Option<Severity> {
        Some(Severity::Error)
    }

    fn source_code(&self) -> Option<&dyn SourceCode> {
        self.top_snippet
            .as_ref()
            .map(|s| &s.named_source as &dyn SourceCode)
    }

    fn labels(&self) -> Option<Box<dyn Iterator<Item = LabeledSpan> + '_>> {
        let snippet = self.top_snippet.as_ref()?;
        let span = LabeledSpan::new_primary_with_span(
            Some("thrown here".to_string()),
            (snippet.label_offset, snippet.label_len),
        );
        Some(Box::new(std::iter::once(span)))
    }
}

// ============================================================
// D6: CompileDiagnostic — miette-rendered compile-error snippets
// ============================================================

/// Structured compile-time diagnostic for a `SourceLoweringError`.
/// Carries the original source text + the offending span so the
/// CLI can render a code frame with a caret under the exact
/// construct that failed, the same way rustc / the JS runtime
/// itself reports uncaught throws.
#[derive(Debug)]
pub struct CompileDiagnostic {
    /// Short human-readable message (`"SyntaxError: …"`,
    /// `"unsupported construct: …"`).
    message: String,
    /// Source text + URL, packaged so miette can print the frame.
    source: NamedSource<Arc<str>>,
    /// `(byte offset, length)` of the offending AST node, when
    /// the underlying error carries one.
    span: Option<(usize, usize)>,
}

impl CompileDiagnostic {
    /// Builds a compile diagnostic from the compiler's
    /// [`otter_vm::source_compiler::SourceLoweringError`] plus the
    /// original source text and URL. The span comes from the
    /// compiler error itself; for `Internal` errors (no span)
    /// the frame renders without a caret.
    pub fn from_source_lowering_error(
        err: &otter_vm::source_compiler::SourceLoweringError,
        source_text: Arc<str>,
        source_url: &str,
    ) -> Self {
        let message = err.to_string();
        let span = err.span().map(|s| {
            let start = s.start as usize;
            let end = s.end as usize;
            (start, end.saturating_sub(start))
        });
        Self {
            message,
            source: NamedSource::new(source_url, source_text),
            span,
        }
    }

    /// Returns the rendered human message.
    #[must_use]
    pub fn message(&self) -> &str {
        &self.message
    }
}

impl fmt::Display for CompileDiagnostic {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for CompileDiagnostic {}

impl Diagnostic for CompileDiagnostic {
    fn severity(&self) -> Option<Severity> {
        Some(Severity::Error)
    }

    fn source_code(&self) -> Option<&dyn SourceCode> {
        Some(&self.source as &dyn SourceCode)
    }

    fn labels(&self) -> Option<Box<dyn Iterator<Item = LabeledSpan> + '_>> {
        let (offset, len) = self.span?;
        // Zero-length spans still render — miette treats them as
        // a caret between two characters. For the compile error
        // surface that's the parser's exact insertion point.
        let effective_len = len.max(1);
        let span =
            LabeledSpan::new_primary_with_span(Some("here".to_string()), (offset, effective_len));
        Some(Box::new(std::iter::once(span)))
    }
}

/// Build a `JsRuntimeDiagnostic` from an `InterpreterError`. Returns `None`
/// for non-throw variants and for thrown values that aren't Error objects
/// (so the caller can fall back to a plain string error).
pub(crate) fn build_js_diagnostic(
    error: &InterpreterError,
    state: &mut RuntimeState,
) -> Option<JsRuntimeDiagnostic> {
    let value = match error {
        InterpreterError::UncaughtThrow(value) => *value,
        _ => return None,
    };
    let handle = value.as_object_handle().map(ObjectHandle)?;

    // Pull frames + (name, message) off the error instance. Falls back to
    // capturing the *current* shadow stack when the error has no captured
    // frames slot — happens when the throw bypasses `Error.captureStackTrace`
    // (e.g. native ThrowTypeError without explicit capture).
    let captured = state
        .read_error_stack_frames(handle)
        .unwrap_or_else(|| state.capture_stack_snapshot(0));
    let (name, message) = state.read_error_name_and_message(handle);

    let frames: Vec<DiagnosticFrame> = captured
        .iter()
        .map(DiagnosticFrame::from_stack_frame)
        .collect();
    let rendered_stack = format_v8_stack(&name, &message, &captured);

    let top_snippet = build_top_frame_snippet(&captured);

    Some(JsRuntimeDiagnostic {
        rendered_stack,
        name,
        message,
        top_snippet,
        frames,
    })
}

/// Builds the top-frame snippet (NamedSource + byte offset) used by
/// `Diagnostic::source_code` and `Diagnostic::labels`.
///
/// Walks the captured frames top-down looking for the first frame with a
/// resolved `(line, column)` and a non-empty source text. Skips native and
/// host frames so the snippet always points at user-visible JS/TS.
fn build_top_frame_snippet(frames: &[StackFrameInfo]) -> Option<TopFrameSnippet> {
    for frame in frames {
        if frame.is_native {
            continue;
        }
        let function = frame.module.function(frame.function_index)?;
        let location = function.source_map().lookup(frame.pc)?;
        let source_text = frame.module.source_text().cloned()?;
        let label_offset = byte_offset_for_location(&source_text, location)?;
        // Underline a single column unit by default. Spans get tightened
        // later when the compiler emits per-expression spans (Phase 1 stops
        // at statement granularity for the first cut, per the plan).
        let label_len = 1;
        let named_source = NamedSource::new(frame.module_url(), source_text.to_string());
        return Some(TopFrameSnippet {
            named_source,
            label_offset,
            label_len,
        });
    }
    None
}

/// Resolves a 1-based `(line, column)` (UTF-16 code units) into a UTF-8 byte
/// offset within `source_text`.
///
/// Used by miette's snippet renderer; miette only knows byte offsets, but
/// our `SourceLocation` records UTF-16 column units (matching V3 source-map
/// conventions). The conversion walks the target line one code point at a
/// time, advancing the byte offset and the UTF-16 unit counter in lockstep.
fn byte_offset_for_location(source_text: &str, location: SourceLocation) -> Option<usize> {
    let target_line = location.line().checked_sub(1)? as usize;
    let target_col = location.column().saturating_sub(1) as usize;

    let mut line_start = 0usize;
    for (idx, _) in source_text.match_indices('\n').take(target_line) {
        line_start = idx + 1;
        let _ = idx;
    }
    if target_line > 0 && line_start == 0 {
        // The source has fewer lines than the target line — clamp to end.
        return Some(source_text.len());
    }

    // Walk the line counting UTF-16 code units until we hit `target_col`.
    let line_slice = &source_text[line_start..];
    let mut utf16_units = 0usize;
    let mut byte_in_line = 0usize;
    for ch in line_slice.chars() {
        if utf16_units == target_col {
            return Some(line_start + byte_in_line);
        }
        if ch == '\n' {
            return Some(line_start + byte_in_line);
        }
        utf16_units = utf16_units.saturating_add(ch.len_utf16());
        byte_in_line = byte_in_line.saturating_add(ch.len_utf8());
    }
    Some(line_start + byte_in_line.min(line_slice.len()))
}
