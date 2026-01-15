//! Core error types for JavaScriptCore operations
//!
//! This module provides a structured error hierarchy that preserves
//! JavaScript exception details including stack traces, line numbers,
//! and error types for better debugging.

use thiserror::Error;

/// Result type alias for JSC operations
pub type JscResult<T> = Result<T, JscError>;

/// Structured error types for JavaScriptCore operations
#[derive(Debug, Error)]
pub enum JscError {
    /// Failed to create a JSC context
    #[error("Context creation failed: {message}")]
    ContextCreation { message: String },

    /// JavaScript syntax error during parsing
    #[error("Syntax error{}: {message}", format_location(file, line, column))]
    SyntaxError {
        message: String,
        file: Option<String>,
        line: Option<u32>,
        column: Option<u32>,
    },

    /// JavaScript runtime error (throw, TypeError, etc.)
    #[error("{error_type}: {message}")]
    ScriptError {
        error_type: String,
        message: String,
        file: Option<String>,
        line: Option<u32>,
        column: Option<u32>,
        stack: Option<String>,
    },

    /// Type conversion error
    #[error("Type error: expected {expected}, got {actual}")]
    TypeError { expected: String, actual: String },

    /// Null pointer returned from JSC API
    #[error("Internal JSC error: {operation} returned null")]
    NullPointer { operation: String },

    /// String encoding error
    #[error("String encoding error: {0}")]
    StringEncoding(String),

    /// Property access error
    #[error("Property error: {0}")]
    PropertyError(String),

    /// Function call error
    #[error("Call error: {0}")]
    CallError(String),

    /// Module loading error
    #[error("Module error: {0}")]
    ModuleError(String),

    /// Resource limit exceeded
    #[error("Resource limit: {0}")]
    ResourceLimit(String),

    /// JSON serialization/deserialization error
    #[error("JSON error: {0}")]
    JsonError(#[from] serde_json::Error),

    /// Internal/unexpected error
    #[error("Internal error: {0}")]
    Internal(String),
}

/// Format location for error display
fn format_location(file: &Option<String>, line: &Option<u32>, column: &Option<u32>) -> String {
    match (file, line, column) {
        (Some(f), Some(l), Some(c)) => format!(" at {}:{}:{}", f, l, c),
        (Some(f), Some(l), None) => format!(" at {}:{}", f, l),
        (None, Some(l), Some(c)) => format!(" at line {}:{}", l, c),
        (None, Some(l), None) => format!(" at line {}", l),
        _ => String::new(),
    }
}

impl JscError {
    /// Create a script error from error type and message
    pub fn script_error(error_type: impl Into<String>, message: impl Into<String>) -> Self {
        Self::ScriptError {
            error_type: error_type.into(),
            message: message.into(),
            file: None,
            line: None,
            column: None,
            stack: None,
        }
    }

    /// Create a script error with location info
    pub fn script_error_with_location(
        error_type: impl Into<String>,
        message: impl Into<String>,
        file: Option<String>,
        line: Option<u32>,
        column: Option<u32>,
        stack: Option<String>,
    ) -> Self {
        Self::ScriptError {
            error_type: error_type.into(),
            message: message.into(),
            file,
            line,
            column,
            stack,
        }
    }

    /// Create a script error at a specific location (legacy helper)
    pub fn script_error_at(message: impl Into<String>, line: u32, column: u32) -> Self {
        Self::ScriptError {
            error_type: "Error".into(),
            message: message.into(),
            file: None,
            line: Some(line),
            column: Some(column),
            stack: None,
        }
    }

    /// Create a syntax error
    pub fn syntax_error(message: impl Into<String>) -> Self {
        Self::SyntaxError {
            message: message.into(),
            file: None,
            line: None,
            column: None,
        }
    }

    /// Create a syntax error with location
    pub fn syntax_error_with_location(
        message: impl Into<String>,
        file: Option<String>,
        line: Option<u32>,
        column: Option<u32>,
    ) -> Self {
        Self::SyntaxError {
            message: message.into(),
            file,
            line,
            column,
        }
    }

    /// Create a type error
    pub fn type_error(expected: impl Into<String>, actual: impl Into<String>) -> Self {
        Self::TypeError {
            expected: expected.into(),
            actual: actual.into(),
        }
    }

    /// Create an internal error
    pub fn internal(message: impl Into<String>) -> Self {
        Self::Internal(message.into())
    }

    /// Create a null pointer error
    pub fn null_pointer(operation: impl Into<String>) -> Self {
        Self::NullPointer {
            operation: operation.into(),
        }
    }

    /// Check if this is a user-facing script error
    pub fn is_script_error(&self) -> bool {
        matches!(self, Self::ScriptError { .. } | Self::SyntaxError { .. })
    }

    /// Get the stack trace if available
    pub fn stack_trace(&self) -> Option<&str> {
        match self {
            Self::ScriptError { stack, .. } => stack.as_deref(),
            _ => None,
        }
    }

    /// Get source location if available
    pub fn location(&self) -> Option<(Option<&str>, Option<u32>, Option<u32>)> {
        match self {
            Self::ScriptError {
                file, line, column, ..
            }
            | Self::SyntaxError {
                file, line, column, ..
            } => Some((file.as_deref(), *line, *column)),
            _ => None,
        }
    }

    /// Get the error type name (e.g., "TypeError", "ReferenceError")
    pub fn error_type(&self) -> &str {
        match self {
            Self::ScriptError { error_type, .. } => error_type,
            Self::SyntaxError { .. } => "SyntaxError",
            Self::TypeError { .. } => "TypeError",
            Self::ContextCreation { .. } => "ContextError",
            Self::NullPointer { .. } => "InternalError",
            Self::StringEncoding(_) => "EncodingError",
            Self::PropertyError(_) => "PropertyError",
            Self::CallError(_) => "CallError",
            Self::ModuleError(_) => "ModuleError",
            Self::ResourceLimit(_) => "ResourceLimitError",
            Self::JsonError(_) => "JsonError",
            Self::Internal(_) => "InternalError",
        }
    }
}

// For backwards compatibility with code using ContextCreation(String)
impl From<String> for JscError {
    fn from(s: String) -> Self {
        Self::Internal(s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_script_error_display() {
        let err = JscError::script_error("TypeError", "undefined is not a function");
        assert_eq!(err.to_string(), "TypeError: undefined is not a function");
    }

    #[test]
    fn test_script_error_with_location() {
        let err = JscError::script_error_with_location(
            "ReferenceError",
            "x is not defined",
            Some("script.js".into()),
            Some(10),
            Some(5),
            Some("at foo (script.js:10:5)".into()),
        );

        assert!(err.is_script_error());
        assert_eq!(err.stack_trace(), Some("at foo (script.js:10:5)"));
        assert_eq!(err.error_type(), "ReferenceError");

        let (file, line, col) = err.location().unwrap();
        assert_eq!(file, Some("script.js"));
        assert_eq!(line, Some(10));
        assert_eq!(col, Some(5));
    }

    #[test]
    fn test_syntax_error() {
        let err = JscError::SyntaxError {
            message: "Unexpected token".into(),
            file: Some("test.js".into()),
            line: Some(1),
            column: Some(10),
        };

        assert!(err.to_string().contains("Syntax error"));
        assert!(err.to_string().contains("test.js:1:10"));
        assert!(err.is_script_error());
    }

    #[test]
    fn test_type_error() {
        let err = JscError::type_error("string", "number");
        assert!(err.to_string().contains("expected string"));
        assert!(err.to_string().contains("got number"));
        assert_eq!(err.error_type(), "TypeError");
    }

    #[test]
    fn test_null_pointer() {
        let err = JscError::null_pointer("JSEvaluateScript");
        assert!(err.to_string().contains("JSEvaluateScript"));
        assert!(err.to_string().contains("returned null"));
    }

    #[test]
    fn test_internal_error() {
        let err = JscError::internal("something went wrong");
        assert_eq!(err.to_string(), "Internal error: something went wrong");
    }

    #[test]
    fn test_location_none() {
        let err = JscError::Internal("test".into());
        assert!(err.location().is_none());
        assert!(err.stack_trace().is_none());
    }
}
