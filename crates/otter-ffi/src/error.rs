//! FFI error types.

/// Errors that can occur during FFI operations.
#[derive(Debug, thiserror::Error)]
pub enum FfiError {
    #[error("Failed to load library '{path}': {reason}")]
    LibraryLoad { path: String, reason: String },

    #[error("Symbol '{name}' not found: {reason}")]
    SymbolNotFound { name: String, reason: String },

    #[error("Type mismatch: expected {expected}, got {got}")]
    TypeMismatch {
        expected: &'static str,
        got: &'static str,
    },

    #[error("Invalid FFI type: '{name}'")]
    InvalidType { name: String },

    #[error("Argument count mismatch: expected {expected}, got {got}")]
    ArgCountMismatch { expected: usize, got: usize },

    #[error("Null pointer dereference")]
    NullPointer,

    #[error("FFI call failed: {0}")]
    CallFailed(String),

    #[error("Library already closed")]
    LibraryClosed,
}
