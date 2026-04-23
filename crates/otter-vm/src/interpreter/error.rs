//! Typed `InterpreterError` + `From<ValueError>` / `From<ObjectError>` conversions.
//! All user-visible runtime errors cross this boundary.

use core::fmt;

use crate::object::ObjectError;
use crate::value::{RegisterValue, ValueError};

/// Errors produced by the new interpreter.
#[derive(Debug, Clone, PartialEq)]
pub enum InterpreterError {
    /// The bytecode referenced a register outside the current frame layout.
    RegisterOutOfBounds,
    /// The interpreter reached the end of bytecode without an explicit return.
    UnexpectedEndOfBytecode,
    /// A branch jumped outside the valid bytecode range.
    InvalidJumpTarget,
    /// A constant table index was out of bounds.
    InvalidConstant,
    /// Execution was interrupted by an external signal (e.g. timeout watchdog).
    Interrupted,
    /// A TypeError was thrown at runtime.
    TypeError(Box<str>),
    /// Arithmetic or comparison failed because the inputs were invalid.
    InvalidValue(ValueError),
    /// The current register value is not an object handle.
    InvalidObjectValue,
    /// The current object handle does not exist in the heap.
    InvalidObjectHandle,
    /// The bytecode referenced a missing property-name entry.
    UnknownPropertyName,
    /// The bytecode referenced a missing string-literal entry.
    UnknownStringLiteral,
    /// The bytecode referenced a missing direct-call entry.
    UnknownCallSite,
    /// The direct-call entry referenced a missing callee function.
    InvalidCallTarget,
    /// The bytecode referenced a missing closure-creation entry.
    UnknownClosureTemplate,
    /// The activation attempted to access an upvalue without a closure context.
    MissingClosureContext,
    /// The closure/upvalue slot index is outside the valid range.
    InvalidHeapSlot,
    /// The heap value kind does not support the requested operation.
    InvalidHeapValueKind,
    /// The current handler path expected a pending exception value.
    MissingPendingException,
    /// Execution finished with an uncaught thrown value.
    UncaughtThrow(RegisterValue),
    /// A native host function failed before producing a JS-visible completion.
    NativeCall(Box<str>),
    /// The configured heap cap was exceeded. Raised by the GC safepoint
    /// when the shared OOM flag is set and surfaced to the host as a
    /// catchable `RangeError` by the outer runtime layer.
    OutOfMemory,
    /// The JS call depth exceeded
    /// [`super::runtime_state::MAX_JS_STACK_DEPTH`](crate::interpreter::MAX_JS_STACK_DEPTH).
    /// Surfaced to user JS as a catchable
    /// `RangeError("Maximum call stack size exceeded")` by the outer
    /// runtime layer. Protects the native Rust thread stack against
    /// unbounded recursion (`function f(){f()}; f();`).
    StackOverflow,
}

impl fmt::Display for InterpreterError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::RegisterOutOfBounds => {
                f.write_str("bytecode referenced a register outside the current frame layout")
            }
            Self::UnexpectedEndOfBytecode => {
                f.write_str("interpreter reached end of bytecode without an explicit return")
            }
            Self::InvalidJumpTarget => {
                f.write_str("branch target is outside the current function bytecode")
            }
            Self::InvalidConstant => f.write_str("constant table index is out of bounds"),
            Self::Interrupted => f.write_str("execution interrupted"),
            Self::TypeError(msg) => write!(f, "TypeError: {msg}"),
            Self::InvalidValue(error) => error.fmt(f),
            Self::InvalidObjectValue => f.write_str("operation expected an object value"),
            Self::InvalidObjectHandle => f.write_str("object handle is outside the current heap"),
            Self::UnknownPropertyName => {
                f.write_str("bytecode referenced a missing property-name entry")
            }
            Self::UnknownStringLiteral => {
                f.write_str("bytecode referenced a missing string-literal entry")
            }
            Self::UnknownCallSite => f.write_str("bytecode referenced a missing direct-call entry"),
            Self::InvalidCallTarget => {
                f.write_str("direct-call entry referenced a missing callee function")
            }
            Self::UnknownClosureTemplate => {
                f.write_str("bytecode referenced a missing closure-creation entry")
            }
            Self::MissingClosureContext => {
                f.write_str("activation attempted to access an upvalue without a closure context")
            }
            Self::InvalidHeapSlot => {
                f.write_str("closure or upvalue slot is outside the valid range")
            }
            Self::InvalidHeapValueKind => {
                f.write_str("operation is not supported for this heap value kind")
            }
            Self::MissingPendingException => {
                f.write_str("handler expected a pending exception value")
            }
            Self::UncaughtThrow(value) => write!(f, "uncaught throw: {:?}", value),
            Self::NativeCall(message) => write!(f, "native host call failed: {message}"),
            Self::OutOfMemory => f.write_str("out of memory: heap limit exceeded"),
            Self::StackOverflow => f.write_str("Maximum call stack size exceeded"),
        }
    }
}

impl std::error::Error for InterpreterError {}

impl From<ValueError> for InterpreterError {
    fn from(value: ValueError) -> Self {
        Self::InvalidValue(value)
    }
}

impl From<ObjectError> for InterpreterError {
    fn from(value: ObjectError) -> Self {
        match value {
            ObjectError::InvalidHandle => Self::InvalidObjectHandle,
            ObjectError::InvalidIndex => Self::InvalidHeapSlot,
            ObjectError::InvalidKind => Self::InvalidHeapValueKind,
            ObjectError::InvalidArrayLength => Self::NativeCall("invalid array length".into()),
            ObjectError::OutOfMemory => Self::OutOfMemory,
            ObjectError::TypeError(msg) => Self::TypeError(msg),
        }
    }
}
