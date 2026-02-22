//! VM error types

use crate::value::Value;
use thiserror::Error;

/// VM execution errors
#[derive(Debug, Error)]
pub enum VmError {
    /// Type error (e.g., calling non-function)
    #[error("TypeError: {0}")]
    TypeError(String),

    /// Reference error (undefined variable)
    #[error("ReferenceError: {0}")]
    ReferenceError(String),

    /// Range error (e.g., invalid array length)
    #[error("RangeError: {0}")]
    RangeError(String),

    /// Syntax error (should be rare at runtime)
    #[error("SyntaxError: {0}")]
    SyntaxError(String),

    /// URI error (malformed URI sequence)
    #[error("URIError: {0}")]
    URIError(String),

    /// Internal error
    #[error("InternalError: {0}")]
    InternalError(String),

    /// Stack overflow
    #[error("RangeError: Maximum call stack size exceeded")]
    StackOverflow,

    /// Out of memory
    #[error("OutOfMemory")]
    OutOfMemory,

    /// Thrown JS exception
    #[error("Uncaught exception: {0}")]
    Exception(Box<ThrownValue>),

    /// Bytecode error
    #[error("Bytecode error: {0}")]
    Bytecode(#[from] otter_vm_bytecode::BytecodeError),

    /// Execution was interrupted (timeout/cancellation)
    #[error("Execution interrupted")]
    Interrupted,
}

/// A thrown JavaScript value
#[derive(Debug)]
pub struct ThrownValue {
    /// The thrown value
    pub value: Value,
    /// The thrown value (as a string representation)
    pub message: String,
    /// Stack trace
    pub stack: Vec<StackFrame>,
}

impl std::fmt::Display for ThrownValue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}

/// A stack frame in error trace
#[derive(Debug, Clone)]
pub struct StackFrame {
    /// Function name
    pub function_name: String,
    /// Source file
    pub file: String,
    /// Line number
    pub line: u32,
    /// Column number
    pub column: u32,
}

impl VmError {
    /// Create a type error
    pub fn type_error(msg: impl Into<String>) -> Self {
        Self::TypeError(msg.into())
    }

    /// Create a reference error
    pub fn reference_error(msg: impl Into<String>) -> Self {
        Self::ReferenceError(msg.into())
    }

    /// Create a range error
    pub fn range_error(msg: impl Into<String>) -> Self {
        Self::RangeError(msg.into())
    }

    /// Create a syntax error
    pub fn syntax_error(msg: impl Into<String>) -> Self {
        Self::SyntaxError(msg.into())
    }

    /// Create a URI error
    pub fn uri_error(msg: impl Into<String>) -> Self {
        Self::URIError(msg.into())
    }

    /// Create an internal error
    pub fn internal(msg: impl Into<String>) -> Self {
        Self::InternalError(msg.into())
    }

    /// Create an interrupted error (for timeout/cancellation)
    pub fn interrupted() -> Self {
        Self::Interrupted
    }

    /// Create an exception from a thrown JS value
    pub fn exception(value: Value) -> Self {
        let message = if let Some(s) = value.as_string() {
            s.as_str().to_string()
        } else {
            format!("{:?}", value)
        };
        Self::Exception(Box::new(ThrownValue {
            message,
            value,
            stack: Vec::new(),
        }))
    }
}

// Automatic conversion from String to VmError for backwards compatibility
// This allows existing code using ? with String errors to compile
impl From<String> for VmError {
    fn from(s: String) -> Self {
        VmError::type_error(s)
    }
}

impl From<&str> for VmError {
    fn from(s: &str) -> Self {
        VmError::type_error(s)
    }
}

/// Result type for VM operations
pub type VmResult<T> = std::result::Result<T, VmError>;
