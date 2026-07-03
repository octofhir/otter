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

/// Boxed JSON failure payload kept out of the hot [`VmError`] enum body.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct VmJsonError {
    /// Stable identifier (e.g. `"JSON_CYCLIC"`).
    pub code: &'static str,
    /// Human-readable diagnostic. Includes the byte position for `JSON_PARSE`.
    pub message: String,
}

/// Boxed Node-style coded failure payload kept out of [`VmError`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct VmCodedError {
    /// JS error class for the thrown instance.
    pub kind: crate::error_classes::ErrorKind,
    /// Stable Node error code (`"ERR_*"`).
    pub code: &'static str,
    /// Human-readable message.
    pub message: String,
}

/// Boxed type-mismatch payload kept out of [`VmError`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct VmTypeMismatchAt {
    /// Operation that rejected the value.
    pub op: String,
    /// Rejected value-kind name.
    pub kind: String,
}

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

    /// Raw address of the backing `AtomicBool`, stable for this flag's life (the
    /// `Arc` keeps it alive). Compiled code polls this byte inline at each
    /// back-edge instead of re-entering the VM; a plain byte load is sufficient
    /// for a cooperative poll (a set flag missed by a stale read is caught on the
    /// next back-edge).
    #[must_use]
    pub fn as_ptr(&self) -> *const u8 {
        self.0.as_ptr().cast::<u8>()
    }

    /// Reset the flag.
    pub fn reset(&self) {
        self.0.store(false, Ordering::Release);
    }
}

/// Owned, dynamic payload for a raised [`VmError`].
///
/// `VmError` itself is `Copy` so it propagates up the interpreter's hot
/// `Result<_, VmError>` chain with zero drop glue (the previous boxed-string
/// variants forced a non-trivial `drop_in_place::<VmError>` after every
/// fallible op — 5–18% of self-time on interpreter-bound benches). The dynamic
/// detail that used to live inline now lives in one per-isolate slot
/// (`Interpreter::pending_error_detail`): the raising helper stashes it, and
/// the surfacing boundary (`vm_error_to_throwable_with_stack_roots`,
/// `vm_to_native_error`, runtime diagnostics) reads it back paired with the
/// `Copy` discriminant. Only one error is in flight per isolate at a time
/// (`?` propagates eagerly), so a single slot is sound; the next raise
/// overwrites it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub enum ErrorDetail {
    /// A human-readable diagnostic message (TypeError / RangeError /
    /// SyntaxError / URIError / BudgetExceeded / ThisUninitialized /
    /// InvalidRegExp).
    Message(Box<str>),
    /// An identifier or intrinsic-method name (UndefinedIdentifier /
    /// UnknownIntrinsic).
    Name(Box<str>),
    /// Display rendering of an uncaught thrown value.
    Uncaught(Box<str>),
    /// Operation + rejected value-kind for [`VmError::TypeMismatchAt`].
    Mismatch(VmTypeMismatchAt),
    /// `JSON.stringify` / `JSON.parse` failure payload.
    Json(VmJsonError),
    /// Node-style coded failure payload.
    Coded(VmCodedError),
}

/// Runtime errors raised by the interpreter.
///
/// `Copy` by construction: every variant carries only `Copy` scalars. Dynamic
/// payloads live in [`ErrorDetail`] on the isolate — see its docs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
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
    /// User-facing version of [`Self::TypeMismatch`]. Detail
    /// ([`ErrorDetail::Mismatch`]) carries the operation name and the
    /// offending value's type; surfaced as a `TypeError` with the message
    /// `<op>: cannot operate on <kind>`.
    TypeMismatchAt,
    /// User-visible `TypeError`. Message in [`ErrorDetail::Message`].
    TypeError,
    /// User-visible `RangeError`. Message in [`ErrorDetail::Message`].
    RangeError,
    /// SyntaxError raised from dynamic parse/compile paths. Message in
    /// [`ErrorDetail::Message`].
    SyntaxError,
    /// `URIError` — malformed input to `decodeURI*` / `encodeURI*`
    /// (§19.2.6). Message in [`ErrorDetail::Message`].
    URIError,
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
    /// a checkpoint. Message in [`ErrorDetail::Message`].
    BudgetExceeded,
    /// `CALL_STRING_METHOD` referenced a method name not in
    /// [`crate::string::prototype::STRING_PROTOTYPE_METHODS`]. Name in
    /// [`ErrorDetail::Name`].
    UnknownIntrinsic,
    /// A `let`/`const` binding was read before its initializer ran
    /// (Temporal Dead Zone).
    TemporalDeadZone {
        /// Compiler-assigned local index.
        local_index: u32,
    },
    /// The `this` binding of a derived-class constructor was used
    /// before the `super(...)` call that initializes it, or `super(...)` ran
    /// more than once. §13.3.7.3 / §10.2.2 — a `ReferenceError`. Detail in
    /// [`ErrorDetail::Message`].
    ThisUninitialized,
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
    /// Name of the unbound identifier in [`ErrorDetail::Name`].
    UndefinedIdentifier,
    /// A user `throw` (or a re-throw from `finally`) walked the
    /// entire frame stack without finding a matching handler. The display
    /// rendering of the thrown value is in [`ErrorDetail::Uncaught`]; the
    /// runtime surfaces this as `OtterError::Runtime { code = "UNCAUGHT" }`.
    Uncaught,
    /// `Op::LoadRegExp` produced a pattern that the regex backend
    /// could not compile. Backend diagnostic in [`ErrorDetail::Message`].
    InvalidRegExp,
    /// `JSON.stringify` / `JSON.parse` rejected its input. Payload in
    /// [`ErrorDetail::Json`] discriminates the failure family for a precise
    /// diagnostic instead of the generic `TYPE_MISMATCH`.
    JsonError,
    /// A JS error carrying a Node-style `.code` (e.g. `ERR_INVALID_ARG_TYPE`).
    /// Payload in [`ErrorDetail::Coded`].
    Coded,
    /// Host-visible termination requested by a native such as
    /// `process.exit(code)`. This is not a JS exception and is not
    /// routed through catch/finally handlers.
    Exit {
        /// Process-style exit status.
        code: u8,
    },
}

const _: () = assert!(std::mem::size_of::<VmError>() <= 24);
// `VmError` must stay `Copy` so the hot `Result<_, VmError>` chain carries no
// drop glue. This fails to compile if any future variant gains an owned field.
const _: fn() = || {
    fn assert_copy<T: Copy>() {}
    assert_copy::<VmError>();
};

impl std::fmt::Display for VmError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            VmError::MissingReturn => write!(f, "function did not RETURN"),
            VmError::InvalidOperand => write!(f, "invalid operand"),
            VmError::TypeMismatch => write!(
                f,
                "type mismatch: this operation does not accept a value of this type"
            ),
            // Variants whose dynamic detail lives in `ErrorDetail` render only
            // their static class text here: `Display` runs without isolate
            // access, so the user-facing message is assembled at the surfacing
            // boundary (`vm_error_to_throwable_with_stack_roots`,
            // `vm_to_native_error`) from the paired `ErrorDetail`.
            VmError::TypeMismatchAt | VmError::TypeError => write!(f, "TypeError"),
            VmError::RangeError => write!(f, "RangeError"),
            VmError::SyntaxError => write!(f, "SyntaxError"),
            VmError::URIError => write!(f, "URIError"),
            VmError::OutOfMemory {
                requested_bytes,
                heap_limit_bytes,
            } => write!(
                f,
                "out of memory: requested {requested_bytes} bytes, heap limit {heap_limit_bytes}"
            ),
            VmError::Interrupted => write!(f, "interrupted"),
            VmError::BudgetExceeded => write!(f, "budget exceeded"),
            VmError::UnknownIntrinsic => write!(f, "unknown intrinsic method"),
            VmError::TemporalDeadZone { local_index } => {
                write!(f, "cannot access local {local_index} before initialization")
            }
            VmError::ThisUninitialized => {
                write!(f, "cannot access binding before initialization")
            }
            VmError::StackOverflow { limit } => {
                write!(f, "maximum call stack size exceeded (limit {limit})")
            }
            VmError::NotCallable => write!(f, "value is not a function"),
            VmError::UndefinedIdentifier => write!(f, "identifier is not defined"),
            VmError::Uncaught => write!(f, "uncaught exception"),
            VmError::InvalidRegExp => write!(f, "invalid regular expression"),
            VmError::JsonError => write!(f, "JSON error"),
            VmError::Coded => write!(f, "error"),
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
    /// Bytecode function id of the frame. Lets `Error.captureStackTrace`
    /// match its `constructorOpt` argument by function identity rather
    /// than by (ambiguous) name when trimming frames.
    pub function_id: u32,
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
    /// Dynamic message/payload for `error`, captured from the isolate's
    /// pending-error slot at the moment the error surfaced (where the isolate
    /// is in scope). `VmError` is `Copy` and carries no message, so this is how
    /// the host-facing renderer recovers the full diagnostic across the
    /// crate boundary. `None` for host-internal `RunError`s.
    pub detail: Option<ErrorDetail>,
}

impl RunError {
    /// Convenience constructor for the no-frames case (e.g., setup
    /// errors before any frame exists). Carries no captured detail.
    #[must_use]
    pub fn bare(error: VmError) -> Self {
        Self {
            error,
            frames: Vec::new(),
            detail: None,
        }
    }

    /// Render the full user-facing message: the captured dynamic detail when
    /// present, else `error`'s static `Display` text.
    #[must_use]
    pub fn message(&self) -> String {
        match (&self.error, &self.detail) {
            (_, Some(ErrorDetail::Message(m))) => m.to_string(),
            (VmError::UndefinedIdentifier, Some(ErrorDetail::Name(n))) => {
                format!("{n} is not defined")
            }
            (VmError::UnknownIntrinsic, Some(ErrorDetail::Name(n))) => {
                format!("unknown intrinsic method `{n}`")
            }
            (_, Some(ErrorDetail::Name(n))) => n.to_string(),
            (_, Some(ErrorDetail::Uncaught(v))) => format!("uncaught exception: {v}"),
            (_, Some(ErrorDetail::Mismatch(p))) => {
                format!("{}: cannot operate on a value of type {}", p.op, p.kind)
            }
            (_, Some(ErrorDetail::Json(p))) => p.message.clone(),
            (_, Some(ErrorDetail::Coded(p))) => p.message.clone(),
            (error, None) => error.to_string(),
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
