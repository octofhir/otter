//! Public diagnostic DTOs emitted by runtime, compiler, and VM boundaries.
//!
//! # Contents
//! - [`Diagnostic`] — stable serializable diagnostic shape.
//! - [`DiagnosticKind`] — broad ECMAScript-thrown class for runtime
//!   diagnostics (matches the `TypeError` / `RangeError` etc.
//!   surface JS scripts observe).
//! - [`DiagnosticCode`] / [`DiagnosticCategory`] — closed,
//!   stable wire-format code set.
//! - [`StackFrame`] — runtime stack-frame metadata.
//!
//! # Invariants
//! - DTOs are owned and serializable; parser/VM internals never cross the
//!   public runtime boundary.
//! - `range` and `span` are byte offsets into `source_url`; both are kept while
//!   older callers still consume `span`.
//! - The [`Diagnostic::code`] field always carries a
//!   [`DiagnosticCode::as_str`] value — producers stamp the code
//!   through [`Diagnostic::with_code_enum`] /
//!   [`Diagnostic::new`] rather than ad-hoc string literals.
//!
//! # See also
//! - [`crate::OtterError`]

pub mod codes;

pub use codes::{DiagnosticCategory, DiagnosticCode};

use serde::{Deserialize, Serialize};

/// Stable diagnostic shape (foundation subset).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Diagnostic {
    /// ECMAScript-thrown class for runtime diagnostics
    /// (`TypeError` / `RangeError` / …) or one of the host-side
    /// buckets (`Capability`, `Timeout`, `OutOfMemory`,
    /// `Internal`, `Syntax`). Distinct from [`Self::code`], which
    /// carries the wire-format machine code.
    pub kind: DiagnosticKind,
    /// Stable wire-format code from the closed
    /// [`DiagnosticCode`] set (`"TS_UNSUPPORTED"`,
    /// `"MODULE_CAPABILITY_DENIED"`, …). Producers stamp this
    /// through [`Diagnostic::new`] / [`Diagnostic::with_code_enum`]
    /// — never an ad-hoc string literal.
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
    /// Optional cause chain. Walks the JS-side `Error.cause`
    /// option (§20.5.6.1.1 + §20.5.7.1.1 InstallErrorCause)
    /// recursively.
    #[serde(default)]
    pub cause: Option<Box<Diagnostic>>,
    /// AggregateError aggregated errors per §20.5.7. Empty for
    /// every non-AggregateError diagnostic.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub aggregated_errors: Vec<Diagnostic>,
}

impl Diagnostic {
    /// Build a diagnostic from a `(kind, code)` pair plus a
    /// human-readable message. The single canonical constructor;
    /// every helper method delegates to this.
    #[must_use]
    pub fn new(kind: DiagnosticKind, code: DiagnosticCode, message: impl Into<String>) -> Self {
        Self {
            kind,
            code: code.as_str().to_string(),
            message: message.into(),
            source_url: None,
            range: None,
            span: None,
            help: None,
            frames: Vec::new(),
            cause: None,
            aggregated_errors: Vec::new(),
        }
    }

    /// Attach an aggregated-errors list (`AggregateError.errors`).
    #[must_use]
    pub fn with_aggregated_errors(mut self, errors: Vec<Diagnostic>) -> Self {
        self.aggregated_errors = errors;
        self
    }

    /// Construct a syntax-class diagnostic with
    /// [`DiagnosticCode::SyntaxError`].
    #[must_use]
    pub fn syntax(message: impl Into<String>) -> Self {
        Self::new(DiagnosticKind::Syntax, DiagnosticCode::SyntaxError, message)
    }

    /// Construct a TS-unsupported diagnostic with
    /// [`DiagnosticCode::TsUnsupported`].
    #[must_use]
    pub fn ts_unsupported(message: impl Into<String>, span: (u32, u32)) -> Self {
        Self::new(
            DiagnosticKind::Syntax,
            DiagnosticCode::TsUnsupported,
            message,
        )
        .with_range(span)
        .with_help("rewrite this TypeScript construct to the supported runtime subset")
    }

    /// Construct a capability-denied diagnostic with
    /// [`DiagnosticCode::CapabilityDenied`]. Used when a host
    /// resource (network, filesystem, env, …) was requested
    /// without the matching capability granted.
    #[must_use]
    pub fn permission(message: impl Into<String>) -> Self {
        Self::new(
            DiagnosticKind::Capability,
            DiagnosticCode::CapabilityDenied,
            message,
        )
    }

    /// Construct a generic "feature not in this slice" diagnostic
    /// with [`DiagnosticCode::FeatureNotInSlice`].
    #[must_use]
    pub fn unsupported(message: impl Into<String>, span: (u32, u32)) -> Self {
        Self::new(
            DiagnosticKind::Syntax,
            DiagnosticCode::FeatureNotInSlice,
            message,
        )
        .with_range(span)
        .with_help("rewrite this construct to the currently supported foundation subset")
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

    /// Override the stable diagnostic code with a typed
    /// [`DiagnosticCode`] variant.
    #[must_use]
    pub fn with_code_enum(mut self, code: DiagnosticCode) -> Self {
        self.code = code.as_str().to_string();
        self
    }

    /// Override the stable diagnostic code.
    ///
    /// Reserved for cross-crate call sites that already carry a
    /// canonical [`DiagnosticCode::as_str`] string (e.g.
    /// [`crate::compile_program`] mapping
    /// [`otter_syntax::SyntaxDiagnostic::code`] forward). Internal
    /// runtime callers should prefer [`Self::with_code_enum`].
    #[must_use]
    pub fn with_code(mut self, code: impl Into<String>) -> Self {
        self.code = code.into();
        self
    }

    /// Attach a `cause` chain entry.
    #[must_use]
    pub fn with_cause(mut self, cause: Diagnostic) -> Self {
        self.cause = Some(Box::new(cause));
        self
    }

    /// Try to parse [`Self::code`] back into the closed
    /// [`DiagnosticCode`] set. Returns `None` for codes outside
    /// the set (e.g. legacy strings staged for cleanup).
    #[must_use]
    pub fn code_enum(&self) -> Option<DiagnosticCode> {
        DiagnosticCode::parse(&self.code)
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
