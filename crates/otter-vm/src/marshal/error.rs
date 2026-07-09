//! Binding error model for the marshalling layer.
//!
//! [`JsError`] is the one error type declarative binding bodies and
//! conversions produce. It is constructible without a context, carries
//! no GC handles, and maps onto [`crate::NativeError`] at the binding
//! boundary — where the operation name ("Blob.prototype.slice") is
//! prefixed once by the glue instead of being hand-threaded through
//! every helper.
//!
//! # Contents
//! - [`JsError`] — error kinds + [`JsError::into_native`] /
//!   [`JsError::from_vm`] conversions.
//! - [`ValueIdent`] — names the value being converted for error
//!   messages ("argument 1", "member 'type'").
//!
//! # Invariants
//! - `JsError` holds only owned Rust data; it is safe to build inside
//!   and carry across any allocation, `.await`, or thread boundary.
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
    /// A user-thrown JS value that crossed the native boundary,
    /// rendered to its display string.
    Thrown(String),
}

impl JsError {
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

    /// Lower into the dispatcher-facing [`NativeError`], stamping the
    /// operation name (`"Blob.prototype.slice"`) the glue owns.
    #[must_use]
    pub fn into_native(self, operation: &'static str) -> NativeError {
        match self {
            Self::Type(reason) => NativeError::TypeError {
                name: operation,
                reason,
            },
            Self::Range(reason) => NativeError::RangeError {
                name: operation,
                reason,
            },
            // Until `DOMException` is a natively declared class the DOM
            // error name travels inside the message; the JS shim layer
            // re-wraps it where exact DOMException identity matters.
            Self::Dom { name, message } => NativeError::TypeError {
                name: operation,
                reason: format!("{name}: {message}"),
            },
            Self::Thrown(message) => NativeError::Thrown {
                name: operation,
                message,
            },
        }
    }

    /// Map a re-entry [`crate::VmError`] (a coercion that threw, a
    /// callback that threw) onto the binding error model, preserving
    /// user-thrown values as [`JsError::Thrown`].
    #[must_use]
    pub fn from_vm(interp: &crate::Interpreter, err: crate::VmError) -> Self {
        match crate::native_function::vm_to_native_error(interp, err, "marshal") {
            NativeError::Thrown { message, .. } => Self::Thrown(message),
            NativeError::RangeError { reason, .. } => Self::Range(reason),
            other => Self::Type(other.to_string()),
        }
    }
}

impl std::fmt::Display for JsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Type(m) => write!(f, "TypeError: {m}"),
            Self::Range(m) => write!(f, "RangeError: {m}"),
            Self::Dom { name, message } => write!(f, "{name}: {message}"),
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
