//! Error types for otter-engine

use thiserror::Error;

/// Engine error type
#[derive(Debug, Error)]
pub enum EngineError {
    /// Module loading/resolution error
    #[error("Module error: {0}")]
    ModuleError(String),

    /// IO error
    #[error("IO error: {0}")]
    IoError(#[from] std::io::Error),

    /// HTTP error
    #[error("HTTP error: {0}")]
    HttpError(String),

    /// Internal error
    #[error("Internal error: {0}")]
    Internal(String),
}

impl EngineError {
    /// Create a module error
    pub fn module(msg: impl Into<String>) -> Self {
        Self::ModuleError(msg.into())
    }

    /// Create an internal error
    pub fn internal(msg: impl Into<String>) -> Self {
        Self::Internal(msg.into())
    }
}

/// Result type using EngineError
pub type EngineResult<T> = Result<T, EngineError>;
