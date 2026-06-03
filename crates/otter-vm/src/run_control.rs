//! Runtime control and failure surface for VM execution.
//!
//! This module owns the small cross-thread interrupt handle and the structured
//! error values returned by interpreter turns. Keeping them outside `lib.rs`
//! keeps the crate root focused on the public map and dispatch glue.
//!
//! # Contents
//! - [`InterruptFlag`] — cheap cooperative cancellation flag.
//! - [`VmError`] — structured interpreter/runtime failure categories.
//! - [`StackFrameSnapshot`] and [`RunError`] — error plus stack context returned
//!   from VM entry points.
//! - [`DEFAULT_MAX_STACK_DEPTH`] and [`NO_HANDLER_OFFSET`] — execution-control
//!   constants shared with embedders and bytecode helpers.
//!
//! # Invariants
//! - Interrupts are cooperative: callers may trip [`InterruptFlag`] from any
//!   thread, but the VM observes it only at explicit checkpoints.
//! - [`VmError::BudgetExceeded`] is a structured runtime rejection, not an
//!   internal crash.
//! - [`RunError::frames`] is top-of-stack first and may be empty for setup
//!   failures raised before a frame exists.
//!
//! # See also
//! - [`crate::Interpreter`]
//! - [`crate::runtime_budget`]
//! - [Runtime principles](../../../docs/book/src/engine/runtime-principles.md)

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use serde::{Deserialize, Serialize};

/// Cooperative cancellation flag.
///
/// Cheap, cloneable, `Send + Sync`. The interpreter polls this flag
/// before each instruction. An interrupt request converts into
/// [`VmError::Interrupted`] at the next checkpoint.
#[derive(Debug, Default, Clone)]
pub struct InterruptFlag(Arc<AtomicBool>);

impl InterruptFlag {
    /// Construct a fresh, un-tripped flag.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Trip the flag from any thread.
    pub fn interrupt(&self) {
        self.0.store(true, Ordering::Release);
    }

    /// Check the flag without resetting it.
    #[must_use]
    pub fn is_set(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }

    /// Reset the flag.
    pub fn reset(&self) {
        self.0.store(false, Ordering::Release);
    }
}

/// Runtime errors raised by the interpreter.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub enum VmError {
    /// The program counter walked off the end of `code` without a
    /// `RETURN`. Indicates a compiler bug.
    MissingReturn,
    /// An operand index was out of range. Indicates a compiler bug
    /// or a malformed bytecode dump.
    InvalidOperand,
    /// An operand had the wrong type for its opcode (e.g.,
    /// `STRING_CONCAT` on a non-string register). Indicates a
    /// compiler bug at this slice.
    TypeMismatch,
    /// User-facing version of [`Self::TypeMismatch`] that carries
    /// the operation name and the offending value's type. Surfaced
    /// to JS as a `TypeError` with the spec-shaped message
    /// `<op>: cannot operate on <kind>` so the user can read which
    /// site rejected which kind without learning the engine
    /// internals.
    TypeMismatchAt {
        /// Operation that rejected the value (e.g.
        /// `"Object.getPrototypeOf"`, `"Op::LoadProperty"`).
        op: &'static str,
        /// Value-kind name from `value_kind_name` (e.g. `"Symbol"`,
        /// `"BigInt"`, `"TypedArray"`).
        kind: &'static str,
    },
    /// User-visible `TypeError` with operation context.
    TypeError {
        /// Human-readable diagnostic.
        message: String,
    },
    /// User-visible `RangeError`. Distinct from
    /// [`Self::TypeError`] so that intrinsics like
    /// `Number.prototype.toFixed` can surface the spec-mandated
    /// `RangeError` for out-of-range arguments instead of the
    /// fallback `TypeError`.
    RangeError {
        /// Human-readable diagnostic.
        message: String,
    },
    /// SyntaxError raised from dynamic parse/compile paths.
    SyntaxError {
        /// Human-readable diagnostic.
        message: String,
    },
    /// `URIError` — malformed input to `decodeURI*` / `encodeURI*`
    /// (§19.2.6). Distinct from [`Self::TypeError`] so the spec-mandated
    /// class survives to the thrown instance.
    URIError {
        /// Human-readable diagnostic.
        message: String,
    },
    /// String allocation failed because the heap cap was hit.
    OutOfMemory {
        /// Bytes the allocation requested.
        requested_bytes: u64,
        /// Heap cap (`0` = unlimited).
        heap_limit_bytes: u64,
    },
    /// `InterruptFlag` was tripped before the next checkpoint.
    Interrupted,
    /// A configured runtime budget rejected the current VM turn at
    /// a checkpoint.
    BudgetExceeded {
        /// Human-readable diagnostic.
        message: String,
    },
    /// `CALL_STRING_METHOD` referenced a method name not in
    /// [`crate::string::prototype::STRING_PROTOTYPE_METHODS`].
    UnknownIntrinsic {
        /// Method name as it appeared in the constant pool.
        name: String,
    },
    /// A `let`/`const` binding was read before its initializer ran
    /// (Temporal Dead Zone).
    TemporalDeadZone {
        /// Compiler-assigned local index.
        local_index: u32,
    },
    /// The `this` binding of a derived-class constructor was used
    /// (read, written, or implicitly returned) before the
    /// `super(...)` call that initializes it, or `super(...)` ran
    /// more than once. §13.3.7.3 / §10.2.2 — a `ReferenceError`.
    ThisUninitialized {
        /// Human-readable detail for the thrown `ReferenceError`.
        message: String,
    },
    /// JS call-stack depth exceeded the configured limit. Catchable
    /// per foundation plan §M7 ("stack-depth limit returns a
    /// catchable JS error").
    StackOverflow {
        /// Maximum depth that was about to be exceeded.
        limit: u32,
    },
    /// Tried to call a value that is not callable.
    NotCallable,
    /// `LoadGlobalOrThrow` (or another lookup site) hit an
    /// unbound free identifier in strict mode. Convertible to a real
    /// `ReferenceError` instance through the dispatch loop's stack-rooted
    /// throwable conversion.
    UndefinedIdentifier {
        /// Name of the unbound identifier.
        name: String,
    },
    /// A user `throw` (or a re-throw from `finally`) walked the
    /// entire frame stack without finding a matching handler. The
    /// payload is the JS value that was thrown, rendered for
    /// diagnostics through `Value::display_string`; the runtime
    /// surfaces this as `OtterError::Runtime { code = "UNCAUGHT" }`.
    Uncaught {
        /// Display rendering of the thrown value.
        value: String,
    },
    /// `Op::LoadRegExp` produced a pattern that the regex backend
    /// could not compile. Catchable as `SyntaxError` once a real
    /// error model lands; for now it surfaces through the standard
    /// runtime-error code.
    InvalidRegExp {
        /// Backend diagnostic — pattern + flags + reason.
        message: String,
    },
    /// `JSON.stringify` / `JSON.parse` rejected its input. The
    /// `code` discriminates the failure family so the runtime can
    /// surface a precise diagnostic (`JSON.stringify cannot
    /// serialize cyclic structures.`, `JSON Parse error: <reason>
    /// at byte N`, …) instead of the generic `TYPE_MISMATCH`.
    JsonError {
        /// Stable identifier (e.g. `"JSON_CYCLIC"`).
        code: &'static str,
        /// Human-readable diagnostic. Includes the byte position
        /// for `JSON_PARSE`.
        message: String,
    },
    /// Host-visible termination requested by a native such as
    /// `process.exit(code)`. This is not a JS exception and is not
    /// routed through catch/finally handlers.
    Exit {
        /// Process-style exit status.
        code: u8,
    },
}

impl std::fmt::Display for VmError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            VmError::MissingReturn => write!(f, "function did not RETURN"),
            VmError::InvalidOperand => write!(f, "invalid operand"),
            VmError::TypeMismatch => write!(
                f,
                "type mismatch: this operation does not accept a value of this type"
            ),
            VmError::TypeMismatchAt { op, kind } => {
                write!(f, "{op}: cannot operate on a value of type {kind}")
            }
            VmError::TypeError { message } => write!(f, "{message}"),
            VmError::RangeError { message } => write!(f, "{message}"),
            VmError::SyntaxError { message } => write!(f, "{message}"),
            VmError::URIError { message } => write!(f, "{message}"),
            VmError::OutOfMemory {
                requested_bytes,
                heap_limit_bytes,
            } => write!(
                f,
                "out of memory: requested {requested_bytes} bytes, heap limit {heap_limit_bytes}"
            ),
            VmError::Interrupted => write!(f, "interrupted"),
            VmError::BudgetExceeded { message } => write!(f, "{message}"),
            VmError::UnknownIntrinsic { name } => write!(f, "unknown intrinsic method `{name}`"),
            VmError::TemporalDeadZone { local_index } => {
                write!(f, "cannot access local {local_index} before initialization")
            }
            VmError::ThisUninitialized { message } => write!(f, "{message}"),
            VmError::StackOverflow { limit } => {
                write!(f, "maximum call stack size exceeded (limit {limit})")
            }
            VmError::NotCallable => write!(f, "value is not a function"),
            VmError::UndefinedIdentifier { name } => write!(f, "{name} is not defined"),
            VmError::Uncaught { value } => write!(f, "uncaught exception: {value}"),
            VmError::InvalidRegExp { message } => write!(f, "{message}"),
            VmError::JsonError { message, .. } => write!(f, "{message}"),
            VmError::Exit { code } => write!(f, "process exited with code {code}"),
        }
    }
}

impl std::error::Error for VmError {}

impl From<otter_gc::OutOfMemory> for VmError {
    fn from(err: otter_gc::OutOfMemory) -> Self {
        VmError::OutOfMemory {
            requested_bytes: err.requested_bytes(),
            heap_limit_bytes: err.heap_limit_bytes(),
        }
    }
}

/// Default JS call-stack depth limit. Catchable via
/// [`VmError::StackOverflow`].
pub const DEFAULT_MAX_STACK_DEPTH: u32 = 1024;

/// Default synchronous re-entry limit for host-driven JS callbacks.
pub const DEFAULT_MAX_SYNC_REENTRY_DEPTH: u32 = 256;

/// Re-export of the bytecode-defined sentinel for "this try block
/// has no catch / finally clause". Kept on the VM surface so
/// embedders that want to hand-build EnterTry operands have one
/// import path for the runtime semantics.
pub use otter_bytecode::NO_HANDLER_OFFSET;

/// One stack-frame snapshot captured at the moment an error is
/// raised. Foundation slice 16 ships this — task 24 (exceptions)
/// reuses it for catchable error frames.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StackFrameSnapshot {
    /// Function name; `<main>` for the script entry,
    /// `<arrow>`/`<anonymous>` for function expressions.
    pub function_name: String,
    /// Module specifier the function was compiled from.
    pub module: String,
    /// Source span of the failing instruction (byte offsets).
    pub span: (u32, u32),
}

/// Result type returned by [`crate::Interpreter::run`] on failure: the
/// underlying [`VmError`] plus a snapshot of the live frame stack
/// at the moment the error was raised. Caller-level translation
/// (e.g., `otter-runtime::map_vm_error`) propagates `frames` into
/// `Diagnostic.frames`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunError {
    /// Underlying error.
    pub error: VmError,
    /// Top-of-stack first; element zero is the failing function.
    pub frames: Vec<StackFrameSnapshot>,
}

impl RunError {
    /// Convenience constructor for the no-frames case (e.g., setup
    /// errors before any frame exists).
    #[must_use]
    pub fn bare(error: VmError) -> Self {
        Self {
            error,
            frames: Vec::new(),
        }
    }
}

impl std::fmt::Display for RunError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.error)
    }
}

impl std::error::Error for RunError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interrupt_flag_is_clone_shared_and_resettable() {
        let flag = InterruptFlag::new();
        let clone = flag.clone();

        assert!(!flag.is_set());
        clone.interrupt();
        assert!(flag.is_set());
        flag.reset();
        assert!(!clone.is_set());
    }

    #[test]
    fn oom_errors_convert_to_vm_error() {
        let err = otter_gc::OutOfMemory::HeapCapExceeded {
            requested_bytes: 64,
            heap_limit_bytes: 32,
        };

        assert_eq!(
            VmError::from(err),
            VmError::OutOfMemory {
                requested_bytes: 64,
                heap_limit_bytes: 32
            }
        );
    }

    #[test]
    fn bare_run_error_keeps_empty_stack() {
        let error = RunError::bare(VmError::Interrupted);

        assert_eq!(error.error, VmError::Interrupted);
        assert!(error.frames.is_empty());
        assert_eq!(error.to_string(), "interrupted");
    }
}
