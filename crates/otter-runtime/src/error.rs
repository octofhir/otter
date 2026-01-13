//! Error types for otter-runtime
//!
//! Re-exports core errors from jsc-core and adds runtime-specific errors.

use thiserror::Error;

// Re-export core error types
pub use jsc_core::{JscError as CoreError, JscResult as CoreResult};

/// Errors that can occur during runtime operations
#[derive(Error, Debug)]
pub enum JscError {
    /// Core JSC error
    #[error(transparent)]
    Core(#[from] CoreError),

    /// Script execution timed out
    #[error("Script execution timed out after {0}ms")]
    Timeout(u64),

    /// Memory limit exceeded
    #[error("Script exceeded memory limit")]
    MemoryLimit,

    /// HTTP request error (for fetch implementation)
    #[error("HTTP error: {0}")]
    HttpError(String),

    /// Runtime pool exhausted
    #[error("All runtime instances are busy")]
    PoolExhausted,
}

impl JscError {
    /// Create a script error from error type and message
    pub fn script_error(error_type: impl Into<String>, message: impl Into<String>) -> Self {
        Self::Core(CoreError::script_error(error_type, message))
    }

    /// Create a script error with location
    pub fn script_error_at(message: impl Into<String>, line: u32, column: u32) -> Self {
        Self::Core(CoreError::script_error_at(message, line, column))
    }

    /// Create a type error
    pub fn type_error(expected: impl Into<String>, actual: impl Into<String>) -> Self {
        Self::Core(CoreError::type_error(expected, actual))
    }

    /// Create a context creation error
    pub fn context_creation(message: impl Into<String>) -> Self {
        Self::Core(CoreError::ContextCreation {
            message: message.into(),
        })
    }

    /// Create an internal error
    pub fn internal(message: impl Into<String>) -> Self {
        Self::Core(CoreError::Internal(message.into()))
    }
}

impl From<serde_json::Error> for JscError {
    fn from(e: serde_json::Error) -> Self {
        Self::Core(CoreError::JsonError(e))
    }
}

/// Result type alias for runtime operations
pub type JscResult<T> = Result<T, JscError>;
