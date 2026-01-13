//! Error types for otter-runtime

use thiserror::Error;

/// Errors that can occur during JSC operations
#[derive(Error, Debug)]
pub enum JscError {
    /// Failed to create JSC context
    #[error("Failed to create JSC context: {0}")]
    ContextCreation(String),

    /// JavaScript evaluation error
    #[error("JavaScript error: {message}")]
    ScriptError {
        message: String,
        line: Option<u32>,
        column: Option<u32>,
    },

    /// Script execution timed out
    #[error("Script execution timed out after {0}ms")]
    Timeout(u64),

    /// Memory limit exceeded
    #[error("Script exceeded memory limit")]
    MemoryLimit,

    /// Type conversion error
    #[error("Type conversion error: expected {expected}, got {actual}")]
    TypeError { expected: String, actual: String },

    /// JSON serialization/deserialization error
    #[error("JSON error: {0}")]
    JsonError(#[from] serde_json::Error),

    /// HTTP request error (for fetch implementation)
    #[error("HTTP error: {0}")]
    HttpError(String),

    /// Internal error
    #[error("Internal error: {0}")]
    Internal(String),

    /// Runtime pool exhausted
    #[error("All runtime instances are busy")]
    PoolExhausted,
}

impl JscError {
    /// Create a script error from exception message
    pub fn script_error(message: impl Into<String>) -> Self {
        Self::ScriptError {
            message: message.into(),
            line: None,
            column: None,
        }
    }

    /// Create a script error with location
    pub fn script_error_at(message: impl Into<String>, line: u32, column: u32) -> Self {
        Self::ScriptError {
            message: message.into(),
            line: Some(line),
            column: Some(column),
        }
    }

    /// Create a type error
    pub fn type_error(expected: impl Into<String>, actual: impl Into<String>) -> Self {
        Self::TypeError {
            expected: expected.into(),
            actual: actual.into(),
        }
    }
}

/// Result type alias for JSC operations
pub type JscResult<T> = Result<T, JscError>;
