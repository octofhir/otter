//! Compilation errors

use thiserror::Error;

/// Compilation errors
#[derive(Debug, Error)]
pub enum CompileError {
    /// Parse error
    #[error("Parse error: {0}")]
    Parse(String),

    /// Syntax error
    #[error("Syntax error at {location}: {message}")]
    Syntax {
        /// Error message
        message: String,
        /// Source location
        location: String,
    },

    /// Unsupported feature
    #[error("Unsupported: {0}")]
    Unsupported(String),

    /// Internal compiler error
    #[error("Internal error: {0}")]
    Internal(String),

    /// Too many locals
    #[error("Too many local variables (max 65535)")]
    TooManyLocals,

    /// Too many constants
    #[error("Too many constants (max 4294967295)")]
    TooManyConstants,

    /// Too many functions
    #[error("Too many functions")]
    TooManyFunctions,

    /// Invalid assignment target
    #[error("Invalid assignment target")]
    InvalidAssignmentTarget,

    /// Early error detected during parsing/validation
    #[error("Early error at {location}: {message}")]
    EarlyError {
        /// Error message
        message: String,
        /// Source location
        location: String,
    },

    /// Legacy syntax error in strict mode
    #[error("Legacy syntax not allowed in strict mode at {location}: {message}")]
    LegacySyntax {
        /// Error message
        message: String,
        /// Source location
        location: String,
    },

    /// Invalid literal syntax
    #[error("Invalid literal syntax at {location}: {message}")]
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
