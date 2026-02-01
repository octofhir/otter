//! VM error types

use crate::value::Value;
use thiserror::Error;

/// Interception signals for internal VM operations
///
/// These are used to signal the interpreter that a special operation needs to be performed
/// with full VM context (call stack, upvalues, etc.). Instead of using magic strings,
/// we use a strongly-typed enum for type safety and performance.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InterceptionSignal {
    /// Function.prototype.call with a closure (requires VM context to execute)
    FunctionCall,
    /// Function.prototype.apply with a closure (requires VM context to execute)
    FunctionApply,
    /// Reflect.apply with a closure (requires VM context to execute)
    ReflectApply,
    /// Reflect.construct with a closure (requires VM context to execute)
    ReflectConstruct,
    /// eval() called indirectly — requires VM context to compile and execute code
    EvalCall,
    // ---- Array callback methods (require VM context to call closure callbacks) ----
    /// Array.prototype.forEach
    ArrayForEach,
    /// Array.prototype.map
    ArrayMap,
    /// Array.prototype.filter
    ArrayFilter,
    /// Array.prototype.find
    ArrayFind,
    /// Array.prototype.findIndex
    ArrayFindIndex,
    /// Array.prototype.findLast
    ArrayFindLast,
    /// Array.prototype.findLastIndex
    ArrayFindLastIndex,
    /// Array.prototype.every
    ArrayEvery,
    /// Array.prototype.some
    ArraySome,
    /// Array.prototype.reduce
    ArrayReduce,
    /// Array.prototype.reduceRight
    ArrayReduceRight,
    /// Array.prototype.flatMap
    ArrayFlatMap,
    /// Array.prototype.sort with comparator
    ArraySort,
    // ---- Reflect methods on proxies (require VM context to invoke traps) ----
    /// Reflect.get on a proxy
    ReflectGetProxy,
    /// Reflect.set on a proxy
    ReflectSetProxy,
    /// Reflect.has on a proxy
    ReflectHasProxy,
    /// Reflect.deleteProperty on a proxy
    ReflectDeletePropertyProxy,
    /// Reflect.ownKeys on a proxy
    ReflectOwnKeysProxy,
    /// Reflect.getOwnPropertyDescriptor on a proxy
    ReflectGetOwnPropertyDescriptorProxy,
    /// Reflect.defineProperty on a proxy
    ReflectDefinePropertyProxy,
    /// Reflect.getPrototypeOf on a proxy
    ReflectGetPrototypeOfProxy,
    /// Reflect.setPrototypeOf on a proxy
    ReflectSetPrototypeOfProxy,
    /// Reflect.isExtensible on a proxy
    ReflectIsExtensibleProxy,
    /// Reflect.preventExtensions on a proxy
    ReflectPreventExtensionsProxy,
    /// Reflect.apply on a proxy
    ReflectApplyProxy,
    /// Reflect.construct on a proxy
    ReflectConstructProxy,
}

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
    #[error("Uncaught exception: {0}")]
    Exception(Box<ThrownValue>),

    /// Bytecode error
    #[error("Bytecode error: {0}")]
    Bytecode(#[from] otter_vm_bytecode::BytecodeError),

    /// Execution was interrupted (timeout/cancellation)
    #[error("Execution interrupted")]
    Interrupted,

    /// Internal interception signal (not a real error)
    ///
    /// This is used to signal the interpreter that a special operation needs VM context.
    /// For example, calling Function.prototype.call with a closure requires access to
    /// the call stack and upvalues, which native functions don't have.
    ///
    /// This is NOT displayed to users - it's caught and handled by the interpreter.
    #[error("Internal interception: {0:?}")]
    Interception(InterceptionSignal),
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

    /// Create an interception signal for internal VM operations
    pub fn interception(signal: InterceptionSignal) -> Self {
        Self::Interception(signal)
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
pub type VmResult<T> = Result<T, VmError>;
