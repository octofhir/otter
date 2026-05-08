//! Public diagnostic DTOs emitted by runtime, compiler, and VM boundaries.
//!
//! # Contents
//! - [`Diagnostic`] — stable serializable diagnostic shape.
//! - [`DiagnosticKind`] — broad diagnostic category.
//! - [`StackFrame`] — runtime stack-frame metadata.
//!
//! # Invariants
//! - DTOs are owned and serializable; parser/VM internals never cross the
//!   public runtime boundary.
//! - `range` and `span` are byte offsets into `source_url`; both are kept while
//!   older callers still consume `span`.
//!
//! # See also
//! - [`crate::OtterError`]

use serde::{Deserialize, Serialize};

/// Stable diagnostic shape (foundation subset).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Diagnostic {
    /// Machine-readable kind.
    pub kind: DiagnosticKind,
    /// Stable code (`TS_UNSUPPORTED`, `OOM_HEAP_LIMIT`, …).
    pub code: String,
    /// Human-readable summary.
    pub message: String,
    /// Optional source URL or path.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_url: Option<String>,
    /// Optional source range.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub range: Option<(u32, u32)>,
    /// Optional source span kept for existing embedders.
    pub span: Option<(u32, u32)>,
    /// Optional help text for human-facing renderers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub help: Option<String>,
    /// Stack frames when relevant.
    #[serde(default)]
    pub frames: Vec<StackFrame>,
    /// Optional cause chain.
    #[serde(default)]
    pub cause: Option<Box<Diagnostic>>,
}

impl Diagnostic {
    /// Construct a syntax-class diagnostic.
    #[must_use]
    pub fn syntax(message: impl Into<String>) -> Self {
        Self {
            kind: DiagnosticKind::Syntax,
            code: "SYNTAX_ERROR".to_string(),
            message: message.into(),
            source_url: None,
            range: None,
            span: None,
            help: None,
            frames: Vec::new(),
            cause: None,
        }
    }

    /// Construct a TS-unsupported diagnostic.
    #[must_use]
    pub fn ts_unsupported(message: impl Into<String>, span: (u32, u32)) -> Self {
        Self {
            kind: DiagnosticKind::Syntax,
            code: "TS_UNSUPPORTED".to_string(),
            message: message.into(),
            source_url: None,
            range: Some(span),
            span: Some(span),
            help: Some("rewrite this TypeScript construct to the supported runtime subset".into()),
            frames: Vec::new(),
            cause: None,
        }
    }

    /// Construct a generic "feature not in this slice" diagnostic.
    #[must_use]
    pub fn unsupported(message: impl Into<String>, span: (u32, u32)) -> Self {
        Self {
            kind: DiagnosticKind::Syntax,
            code: "FEATURE_NOT_IN_SLICE".to_string(),
            message: message.into(),
            source_url: None,
            range: Some(span),
            span: Some(span),
            help: Some(
                "rewrite this construct to the currently supported foundation subset".into(),
            ),
            frames: Vec::new(),
            cause: None,
        }
    }

    /// Attach a source URL or path.
    #[must_use]
    pub fn with_source_url(mut self, source_url: impl Into<String>) -> Self {
        self.source_url = Some(source_url.into());
        self
    }

    /// Attach source byte offsets.
    #[must_use]
    pub fn with_range(mut self, range: (u32, u32)) -> Self {
        self.range = Some(range);
        self.span = Some(range);
        self
    }

    /// Attach help text.
    #[must_use]
    pub fn with_help(mut self, help: impl Into<String>) -> Self {
        self.help = Some(help.into());
        self
    }

    /// Override the stable diagnostic code.
    #[must_use]
    pub fn with_code(mut self, code: impl Into<String>) -> Self {
        self.code = code.into();
        self
    }
}

/// Diagnostic category.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum DiagnosticKind {
    /// Syntax / TypeScript erasure / compile-time.
    Syntax,
    /// `TypeError`.
    Type,
    /// `ReferenceError`.
    Reference,
    /// `RangeError`.
    Range,
    /// Heap cap hit.
    OutOfMemory,
    /// Timeout fired.
    Timeout,
    /// Capability denied.
    Capability,
    /// Internal bug.
    Internal,
}

/// Single stack frame.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct StackFrame {
    /// Function name; `"<main>"` for the script entry.
    pub function: String,
    /// Module specifier.
    pub module: String,
    /// Source span within `module`.
    pub span: Option<(u32, u32)>,
}
