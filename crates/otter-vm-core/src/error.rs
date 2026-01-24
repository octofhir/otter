//! VM error types

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
    #[error("Uncaught exception")]
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
    /// The thrown value (as a string representation for now)
    pub message: String,
    /// Stack trace
    pub stack: Vec<StackFrame>,
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

    /// Create an internal error
    pub fn internal(msg: impl Into<String>) -> Self {
        Self::InternalError(msg.into())
    }

    /// Create an interrupted error (for timeout/cancellation)
    pub fn interrupted() -> Self {
        Self::Interrupted
    }
}

/// Result type for VM operations
pub type VmResult<T> = Result<T, VmError>;
