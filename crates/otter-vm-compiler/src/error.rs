//! Compilation errors

use thiserror::Error;

/// Compilation errors
///
/// Per ES2026 spec, all parse-time errors are SyntaxError (§13.1.1).
/// The Display format prefixes all variants with "SyntaxError:" so that
/// error messages are spec-compliant and can be matched in test262.
#[derive(Debug, Error)]
pub enum CompileError {
    /// Parse error (from oxc parser)
    #[error("SyntaxError: {0}")]
    Parse(String),

    /// Syntax error (from compiler static semantics)
    #[error("SyntaxError: {message} (at {location})")]
    Syntax {
        /// Error message
        message: String,
        /// Source location
        location: String,
    },

    /// Unsupported feature
    #[error("SyntaxError: Unsupported: {0}")]
    Unsupported(String),

    /// Internal compiler error
    #[error("SyntaxError: Internal error: {0}")]
    Internal(String),

    /// Too many locals
    #[error("SyntaxError: Too many local variables (max 65535)")]
    TooManyLocals,

    /// Too many constants
    #[error("SyntaxError: Too many constants (max 4294967295)")]
    TooManyConstants,

    /// Too many functions
    #[error("SyntaxError: Too many functions")]
    TooManyFunctions,

    /// Invalid assignment target
    #[error("SyntaxError: Invalid assignment target")]
    InvalidAssignmentTarget,

    /// Early error detected during parsing/validation
    #[error("SyntaxError: {message} (at {location})")]
    EarlyError {
        /// Error message
        message: String,
        /// Source location
        location: String,
    },

    /// Legacy syntax error in strict mode
    #[error("SyntaxError: Legacy syntax not allowed in strict mode: {message} (at {location})")]
    LegacySyntax {
        /// Error message
        message: String,
        /// Source location
        location: String,
    },

    /// Invalid literal syntax
    #[error("SyntaxError: Invalid literal: {message} (at {location})")]
    InvalidLiteral {
        /// Error message
        message: String,
        /// Source location
        location: String,
    },
}

impl CompileError {
    /// Create a syntax error
    pub fn syntax(message: impl Into<String>, line: u32, column: u32) -> Self {
        Self::Syntax {
            message: message.into(),
            location: format!("{}:{}", line, column),
        }
    }

    /// Create an unsupported error
    pub fn unsupported(feature: impl Into<String>) -> Self {
        Self::Unsupported(feature.into())
    }

    /// Create an internal error
    pub fn internal(msg: impl Into<String>) -> Self {
        Self::Internal(msg.into())
    }

    /// Create an early error
    pub fn early_error(message: impl Into<String>, line: u32, column: u32) -> Self {
        Self::EarlyError {
            message: message.into(),
            location: format!("{}:{}", line, column),
        }
    }

    /// Create a legacy syntax error
    pub fn legacy_syntax(message: impl Into<String>, line: u32, column: u32) -> Self {
        Self::LegacySyntax {
            message: message.into(),
            location: format!("{}:{}", line, column),
        }
    }

    /// Create an invalid literal error
    pub fn invalid_literal(message: impl Into<String>, line: u32, column: u32) -> Self {
        Self::InvalidLiteral {
            message: message.into(),
            location: format!("{}:{}", line, column),
        }
    }
}

/// Result type for compilation
pub type CompileResult<T> = Result<T, CompileError>;
