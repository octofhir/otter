//! Binding error model for the marshalling layer.
//!
//! [`JsError`] is the one error type declarative binding bodies and
//! conversions produce. It is constructible without a context, carries
//! no GC handles, and maps onto [`crate::NativeError`] at the binding
//! boundary. Authored exception classes preserve their message text. The
//! operation name labels thrown-text and OOM transport; it is diagnostic
//! metadata, not an invented prefix on an authored exception's message.
//!
//! # Contents
//! - [`JsError`] — error kinds + [`JsError::into_native`] /
//!   [`JsError::from_vm`] conversions.
//! - [`ValueIdent`] — names the value being converted for error
//!   messages ("argument 1", "member 'type'").
//!
//! # Invariants
//! - `JsError` holds only owned Rust data; it is safe to build inside
//!   and carry across any allocation, `.await`, or thread boundary. A thrown
//!   value's object identity is not part of that owned data: it is retained
//!   only by the current synchronous VM propagation's pending root.
//! - `Dom` renders through the native `TypeError` channel until
//!   `DOMException` is a natively declared class; the variant exists so
//!   declaration sites already state the spec error name.
//!
//! # See also
//! - [`crate::NativeError`] — the dispatcher-facing error model this
//!   lowers into.

use crate::NativeError;

/// Error raised by a declarative binding body or a marshalling
/// conversion.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JsError {
    /// A re-entry error already classified by the one native error owner.
    /// This carries classes, structural failures and control without flattening
    /// them to a TypeError or duplicating their owned payloads here.
    Native(NativeError),
    /// A JS `TypeError`.
    Type(String),
    /// A JS `RangeError`.
    Range(String),
    /// A `DOMException` with the given spec error name
    /// (e.g. `"NotSupportedError"`).
    Dom {
        /// WebIDL/DOM error name.
        name: &'static str,
        /// Human-readable message.
        message: String,
    },
    /// Heap exhaustion preserves its canonical typed cause across the binding.
    OutOfMemory {
        /// Bytes requested by the failed allocation.
        requested_bytes: u64,
        /// Configured heap cap in bytes.
        heap_limit_bytes: u64,
    },
    /// Owned diagnostic text for a throw. This contains no JavaScript value
    /// identity and cannot transport one across a deferred host completion.
    Thrown(String),
}

impl JsError {
    /// Deferred conversion owners preserve heap exhaustion as the original
    /// turn failure instead of attempting to allocate a rejection reason.
    #[must_use]
    pub const fn is_out_of_memory(&self) -> bool {
        match self {
            Self::OutOfMemory { .. } | Self::Native(NativeError::OutOfMemory { .. }) => true,
            Self::Native(NativeError::ExecutionFailure(failure)) => {
                matches!(failure.error, crate::VmError::OutOfMemory { .. })
            }
            _ => false,
        }
    }

    /// Lift a dispatcher-facing native error into the owned marshalling model.
    #[must_use]
    pub(crate) fn from_native(error: NativeError) -> Self {
        Self::Native(error)
    }

    /// Shorthand for a `TypeError` with a formatted message.
    #[must_use]
    pub fn type_error(message: impl Into<String>) -> Self {
        Self::Type(message.into())
    }

    /// Shorthand for a `RangeError` with a formatted message.
    #[must_use]
    pub fn range_error(message: impl Into<String>) -> Self {
        Self::Range(message.into())
    }

    /// Lower into the sole dispatcher-facing [`NativeError`]. Authored
    /// Type/Range/DOM text uses the existing SpecError class payload unchanged.
    /// Imported native errors keep every original field. `operation` labels
    /// only owned thrown-text and OOM transport; it does not prefix authored
    /// JavaScript messages.
    #[must_use]
    pub fn into_native(self, operation: &'static str) -> NativeError {
        match self {
            Self::Native(error) => error,
            Self::Type(message) => NativeError::SpecError {
                kind: crate::ErrorKind::TypeError,
                message,
            },
            Self::Range(message) => NativeError::SpecError {
                kind: crate::ErrorKind::RangeError,
                message,
            },
            // Until `DOMException` is a natively declared class the DOM
            // error name travels inside the message; the JS shim layer
            // re-wraps it where exact DOMException identity matters.
            Self::Dom { name, message } => NativeError::SpecError {
                kind: crate::ErrorKind::TypeError,
                message: format!("{name}: {message}"),
            },
            Self::OutOfMemory {
                requested_bytes,
                heap_limit_bytes,
            } => NativeError::OutOfMemory {
                name: operation,
                requested_bytes,
                heap_limit_bytes,
            },
            Self::Thrown(message) => NativeError::Thrown {
                name: operation,
                message,
            },
        }
    }

    /// Map a re-entry [`crate::VmError`] (a coercion that threw, a
    /// callback that threw) onto the binding error model. The existing
    /// NativeError is preserved unchanged. During immediate synchronous
    /// propagation, the interpreter's pending root retains a user throw's GC
    /// identity until the caller consumes it. The returned Send-owned error
    /// itself carries diagnostic text only; storing it across await does not
    /// reserve or preserve that pending JavaScript value.
    #[must_use]
    pub fn from_vm(interp: &crate::Interpreter, err: crate::VmError) -> Self {
        Self::from_native(crate::native_function::vm_to_native_error(
            interp, err, "marshal",
        ))
    }
}

impl std::fmt::Display for JsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Native(error) => error.fmt(f),
            Self::Type(m) => write!(f, "TypeError: {m}"),
            Self::Range(m) => write!(f, "RangeError: {m}"),
            Self::Dom { name, message } => write!(f, "{name}: {message}"),
            Self::OutOfMemory {
                requested_bytes,
                heap_limit_bytes,
            } => write!(
                f,
                "out of memory: requested {requested_bytes} bytes, heap limit {heap_limit_bytes}"
            ),
            Self::Thrown(m) => write!(f, "{m}"),
        }
    }
}

impl std::error::Error for JsError {}

/// Names the value a conversion is extracting, for error messages.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ValueIdent<'a> {
    /// A positional call argument (zero-based; rendered one-based).
    Argument(usize),
    /// A named dictionary member.
    Member(&'a str),
    /// A sequence element (zero-based).
    Element(usize),
    /// A union variant probe.
    Variant(&'a str),
    /// The call receiver.
    This,
    /// Free-form description.
    Other(&'a str),
}

impl std::fmt::Display for ValueIdent<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Argument(i) => write!(f, "argument {}", i + 1),
            Self::Member(name) => write!(f, "member '{name}'"),
            Self::Element(i) => write!(f, "element {i}"),
            Self::Variant(name) => write!(f, "variant '{name}'"),
            Self::This => write!(f, "this"),
            Self::Other(what) => write!(f, "{what}"),
        }
    }
}
