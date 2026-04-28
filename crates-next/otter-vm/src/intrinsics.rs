//! Declarative scaffolding for primitive-receiver intrinsics.
//!
//! Slice tasks `10`+ wire JS `String.prototype` / `Number.prototype`
//! methods through this module. The harness slice (task 09)
//! introduces the **types and the macro** so future slices can
//! register intrinsics without ad-hoc dispatch sprawl. No JS-visible
//! intrinsic is registered yet — the type tag and method tables
//! exist to lock the shape.
//!
//! # Why a table, not free functions?
//!
//! Primitive-receiver dispatch (`"abc".slice(0, 1)`,
//! `(1.5).toFixed(2)`) needs three things tied together:
//!
//! 1. a **stable name** that the compiler can refer to;
//! 2. a **canonical Rust function** that implements the method;
//! 3. metadata for diagnostics (arity, expected receiver type) so
//!    bad calls produce structured `Diagnostic`s instead of panics.
//!
//! [`IntrinsicTable`] holds these in one place. The
//! [`intrinsics!`] macro is the user-facing way to populate it
//! without writing boilerplate.
//!
//! # Contents
//! - [`IntrinsicReceiver`] — receiver type tag.
//! - [`IntrinsicArgs`] — borrowed argument slice + receiver handle.
//! - [`IntrinsicFn`] — the function pointer signature.
//! - [`IntrinsicEntry`] — name + arity + impl + receiver kind.
//! - [`IntrinsicTable`] — registry, lookup by `(receiver, name)`.
//! - [`intrinsics!`] — declarative builder macro.
//! - [`IntrinsicError`] — failure modes surfaced through the runtime
//!   error model.
//!
//! # Invariants
//! - The macro produces **`const`** entries; all metadata is
//!   compile-time visible.
//! - Method names are interned at table-build time as `&'static str`
//!   so lookup is a simple string compare without allocation.
//!
//! # See also
//! - [`docs/new-engine/tasks/09-string-core-slice.md`](
//!     ../../../docs/new-engine/tasks/09-string-core-slice.md
//!   )
//! - [`docs/new-engine/tasks/10-string-methods-slice.md`](
//!     ../../../docs/new-engine/tasks/10-string-methods-slice.md
//!   )

use crate::Value;
use crate::string::StringHeap;

/// Receiver type a [`IntrinsicEntry`] is registered against.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum IntrinsicReceiver {
    /// `String.prototype.<name>`. Receiver is a `Value::String`.
    String,
    /// `Number.prototype.<name>`. Receiver is a `Value::Number`
    /// (added in slice 11).
    Number,
    /// `Boolean.prototype.<name>`. Receiver is a `Value::Boolean`
    /// (added in slice 12).
    Boolean,
    /// `Array.prototype.<name>`. Receiver is a `Value::Array`
    /// (added in slice 21).
    Array,
    /// `RegExp.prototype.<name>`. Receiver is a `Value::RegExp`
    /// (added in slice 31).
    RegExp,
    /// `Promise.prototype.<name>`. Receiver is a
    /// `Value::Promise` (added in slice 34).
    Promise,
    /// `Symbol.prototype.<name>`. Receiver is a [`crate::Value::Symbol`]
    /// (added in slice 37).
    ///
    /// # See also
    /// - <https://tc39.es/ecma262/#sec-symbol-prototype-object>
    Symbol,
}

/// Borrowed call frame for an intrinsic.
///
/// Lifetime is bound to the dispatch site. The receiver is a
/// reference into the caller's register window — intrinsic
/// implementations may not stash it.
pub struct IntrinsicArgs<'a> {
    /// The receiver value (`this`).
    pub receiver: &'a Value,
    /// Argument values in declaration order.
    pub args: &'a [Value],
    /// String heap for any allocation an intrinsic performs.
    pub string_heap: &'a StringHeap,
}

/// The intrinsic implementation function pointer.
pub type IntrinsicFn = fn(&IntrinsicArgs<'_>) -> Result<Value, IntrinsicError>;

/// One row in the intrinsic table.
#[derive(Clone, Copy)]
pub struct IntrinsicEntry {
    /// Receiver kind this entry binds to.
    pub receiver: IntrinsicReceiver,
    /// JS-visible method name (interned).
    pub name: &'static str,
    /// Declared arity (used for diagnostics; intrinsics may
    /// inspect [`IntrinsicArgs::args`] directly to handle variadic
    /// shapes).
    pub arity: u16,
    /// Implementation.
    pub impl_fn: IntrinsicFn,
}

/// In-memory registry. Static slices are usable directly via
/// [`IntrinsicTable::from_static`].
#[derive(Debug)]
pub struct IntrinsicTable {
    entries: &'static [IntrinsicEntry],
}

impl IntrinsicTable {
    /// Empty registry.
    #[must_use]
    pub const fn empty() -> Self {
        Self { entries: &[] }
    }

    /// Wrap a `'static` slice produced by [`intrinsics!`].
    #[must_use]
    pub const fn from_static(entries: &'static [IntrinsicEntry]) -> Self {
        Self { entries }
    }

    /// Look up by receiver kind and method name.
    #[must_use]
    pub fn lookup(
        &self,
        receiver: IntrinsicReceiver,
        name: &str,
    ) -> Option<&'static IntrinsicEntry> {
        self.entries
            .iter()
            .find(|e| e.receiver == receiver && e.name == name)
    }
}

impl std::fmt::Debug for IntrinsicEntry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("IntrinsicEntry")
            .field("receiver", &self.receiver)
            .field("name", &self.name)
            .field("arity", &self.arity)
            .finish()
    }
}

/// Errors an intrinsic implementation can raise.
///
/// Idiomatic `thiserror::Error` enum: each variant carries a static
/// `#[error]` template so log lines and CLI diagnostics share the
/// same wording.
#[derive(Debug, Clone, thiserror::Error)]
#[non_exhaustive]
pub enum IntrinsicError {
    /// Receiver is the wrong type for this method.
    #[error("intrinsic called on a non-{expected} receiver")]
    BadReceiver {
        /// Expected receiver kind name (e.g., `"string"`).
        expected: &'static str,
    },
    /// Argument is the wrong type or out of range.
    #[error("argument {index} {reason}")]
    BadArgument {
        /// Argument index (0-based).
        index: u16,
        /// Short reason.
        reason: &'static str,
    },
    /// Method name is not registered for this receiver.
    #[error("unknown intrinsic method `{name}` for receiver")]
    UnknownMethod {
        /// JS-visible method name the call site asked for.
        name: String,
    },
    /// Allocation failed.
    #[error("out of memory: requested {requested_bytes} bytes, heap limit {heap_limit_bytes}")]
    OutOfMemory {
        /// Bytes requested.
        requested_bytes: u64,
        /// Heap cap (`0` = unlimited).
        heap_limit_bytes: u64,
    },
}

impl From<crate::string::StringError> for IntrinsicError {
    fn from(err: crate::string::StringError) -> Self {
        match err {
            crate::string::StringError::OutOfMemory {
                requested_bytes,
                heap_limit_bytes,
            } => Self::OutOfMemory {
                requested_bytes,
                heap_limit_bytes,
            },
        }
    }
}

/// Declarative intrinsic table builder.
///
/// Usage:
///
/// ```ignore
/// use otter_vm::intrinsics;
///
/// fn impl_starts_with(args: &intrinsics::IntrinsicArgs<'_>)
///     -> Result<otter_vm::Value, intrinsics::IntrinsicError> { ... }
///
/// pub static TABLE: otter_vm::intrinsics::IntrinsicTable =
///     otter_vm::intrinsics!(
///         String,
///         "startsWith" / 1 => impl_starts_with,
///         "endsWith"   / 1 => impl_ends_with,
///         "slice"      / 2 => impl_slice,
///     );
/// ```
///
/// The macro builds a `&'static [IntrinsicEntry]` and wraps it in
/// an [`IntrinsicTable`]. Multiple receiver groups are concatenated
/// with `;`.
#[macro_export]
macro_rules! intrinsics {
    ( $( $receiver:ident , $( $name:literal / $arity:literal => $impl_fn:path ),* $(,)? );+ $(;)? ) => {{
        const ENTRIES: &[$crate::intrinsics::IntrinsicEntry] = &[
            $($(
                $crate::intrinsics::IntrinsicEntry {
                    receiver: $crate::intrinsics::IntrinsicReceiver::$receiver,
                    name: $name,
                    arity: $arity,
                    impl_fn: $impl_fn,
                },
            )*)+
        ];
        $crate::intrinsics::IntrinsicTable::from_static(ENTRIES)
    }};
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::string::{JsString, StringHeap};

    fn impl_length(args: &IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
        let recv = match args.receiver {
            Value::String(s) => s,
            _ => return Err(IntrinsicError::BadReceiver { expected: "string" }),
        };
        let n = recv.len();
        Ok(Value::String(JsString::from_str(
            &n.to_string(),
            args.string_heap,
        )?))
    }

    fn impl_concat_with(args: &IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
        let recv = match args.receiver {
            Value::String(s) => s,
            _ => return Err(IntrinsicError::BadReceiver { expected: "string" }),
        };
        let arg0 = match args.args.first() {
            Some(Value::String(s)) => s,
            _ => {
                return Err(IntrinsicError::BadArgument {
                    index: 0,
                    reason: "must be a string",
                });
            }
        };
        let out = JsString::concat(recv, arg0, args.string_heap)?;
        Ok(Value::String(out))
    }

    static STRING_TABLE: IntrinsicTable = intrinsics!(
        String,
        "length"      / 0 => impl_length,
        "concatWith"  / 1 => impl_concat_with,
    );

    #[test]
    fn macro_registers_entries() {
        let entry = STRING_TABLE
            .lookup(IntrinsicReceiver::String, "concatWith")
            .unwrap();
        assert_eq!(entry.arity, 1);
    }

    #[test]
    fn intrinsic_runs_with_string_receiver() {
        let heap = StringHeap::default();
        let recv = Value::String(JsString::from_str("ab", &heap).unwrap());
        let arg = Value::String(JsString::from_str("cd", &heap).unwrap());
        let entry = STRING_TABLE
            .lookup(IntrinsicReceiver::String, "concatWith")
            .unwrap();
        let result = (entry.impl_fn)(&IntrinsicArgs {
            receiver: &recv,
            args: &[arg],
            string_heap: &heap,
        })
        .unwrap();
        assert_eq!(result.display_string(), "abcd");
    }

    #[test]
    fn intrinsic_rejects_bad_receiver() {
        let heap = StringHeap::default();
        let entry = STRING_TABLE
            .lookup(IntrinsicReceiver::String, "length")
            .unwrap();
        let err = (entry.impl_fn)(&IntrinsicArgs {
            receiver: &Value::Undefined,
            args: &[],
            string_heap: &heap,
        })
        .unwrap_err();
        assert!(matches!(err, IntrinsicError::BadReceiver { .. }));
    }
}
