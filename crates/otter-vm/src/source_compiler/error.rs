//! Error types produced while lowering JS source into the Ignition ISA.
//!
//! The lowering pipeline is staged: each milestone (`M1..M10`) widens the
//! supported AST surface. Constructs outside the currently supported
//! surface surface as [`SourceLoweringError::Unsupported`] with a
//! `construct: &'static str` tag so diagnostic tools can map an error
//! back to the milestone that will add support.

use oxc_span::Span;

/// Error produced by [`super::ModuleCompiler::compile`].
///
/// Every error carries a [`Span`] pointing at the offending source region
/// so diagnostics can underline the exact AST node that could not be
/// lowered. `Unsupported` additionally tags the AST construct name with
/// a static string; unsupported is the *expected* error path during the
/// staged rollout and is not a bug.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SourceLoweringError {
    /// The oxc parser rejected the input.
    Parse { message: String, span: Span },
    /// The input uses an AST construct the compiler does not yet lower.
    /// Callers distinguish constructs by the static `construct` tag.
    Unsupported {
        construct: &'static str,
        span: Span,
    },
    /// An ECMAScript static-semantics early error the parser did not catch.
    EarlyError { message: String, span: Span },
    /// The input required more locals than the current register layout
    /// supports (exceeds `u16::MAX`).
    TooManyLocals { span: Span },
    /// Internal invariant violation while lowering. Bug, not user input.
    Internal(String),
}

impl std::fmt::Display for SourceLoweringError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Parse { message, .. } => write!(f, "SyntaxError: {message}"),
            Self::Unsupported { construct, .. } => {
                write!(f, "unsupported construct: {construct}")
            }
            Self::EarlyError { message, .. } => write!(f, "SyntaxError: {message}"),
            Self::TooManyLocals { .. } => {
                f.write_str("source exceeds the local-slot register limit")
            }
            Self::Internal(msg) => write!(f, "internal compiler error: {msg}"),
        }
    }
}

impl std::error::Error for SourceLoweringError {}

impl SourceLoweringError {
    /// Convenience constructor for [`Self::Unsupported`].
    #[must_use]
    pub const fn unsupported(construct: &'static str, span: Span) -> Self {
        Self::Unsupported { construct, span }
    }

    /// Returns the span associated with the error, if any.
    #[must_use]
    pub const fn span(&self) -> Option<Span> {
        match self {
            Self::Parse { span, .. }
            | Self::Unsupported { span, .. }
            | Self::EarlyError { span, .. }
            | Self::TooManyLocals { span } => Some(*span),
            Self::Internal(_) => None,
        }
    }
}
