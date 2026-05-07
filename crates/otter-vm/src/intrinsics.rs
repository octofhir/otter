//! Declarative scaffolding for primitive-receiver intrinsics.
//!
//! JS `String.prototype` / `Number.prototype` methods can route
//! through this module. The type tags, tables, and macro let the VM
//! register intrinsics without ad-hoc dispatch sprawl.
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
//! - [Architecture](../../../docs/book/src/engine/architecture.md)

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
    /// `BigInt.prototype.<name>`. Receiver is a `Value::BigInt`.
    ///
    /// # See also
    /// - <https://tc39.es/ecma262/#sec-properties-of-the-bigint-prototype-object>
    BigInt,
    /// `Date.prototype.<name>`. Receiver is a [`crate::Value::Date`].
    ///
    /// # See also
    /// - <https://tc39.es/ecma262/#sec-properties-of-the-date-prototype-object>
    Date,
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
    /// `Map.prototype.<name>`. Receiver is a [`crate::Value::Map`].
    ///
    /// # See also
    /// - <https://tc39.es/ecma262/#sec-map-prototype-object>
    Map,
    /// `Set.prototype.<name>`. Receiver is a [`crate::Value::Set`].
    ///
    /// # See also
    /// - <https://tc39.es/ecma262/#sec-set-prototype-object>
    Set,
    /// `WeakMap.prototype.<name>`. Receiver is a [`crate::Value::WeakMap`].
    ///
    /// # See also
    /// - <https://tc39.es/ecma262/#sec-weakmap-prototype-object>
    WeakMap,
    /// `WeakSet.prototype.<name>`. Receiver is a [`crate::Value::WeakSet`].
    ///
    /// # See also
    /// - <https://tc39.es/ecma262/#sec-weakset-prototype-object>
    WeakSet,
    /// `WeakRef.prototype.<name>`. Receiver is a [`crate::Value::WeakRef`].
    ///
    /// # See also
    /// - <https://tc39.es/ecma262/#sec-weak-ref-objects>
    WeakRef,
    /// `FinalizationRegistry.prototype.<name>`. Receiver is a
    /// [`crate::Value::FinalizationRegistry`].
    ///
    /// # See also
    /// - <https://tc39.es/ecma262/#sec-finalization-registry-objects>
    FinalizationRegistry,
    /// `Temporal.<Class>.prototype.<name>`. Receiver is a
    /// [`crate::Value::Temporal`]. Per-kind routing happens in
    /// [`crate::temporal::lookup_prototype`].
    ///
    /// # See also
    /// - <https://tc39.es/proposal-temporal/>
    Temporal,
    /// `Intl.<Class>.prototype.<name>`. Receiver is a
    /// [`crate::Value::Intl`]. Per-kind routing happens in
    /// [`crate::intl::lookup_prototype`].
    ///
    /// # See also
    /// - <https://tc39.es/ecma402/>
    Intl,
    /// `ArrayBuffer.prototype.<name>`. Receiver is a
    /// [`crate::Value::ArrayBuffer`].
    ///
    /// # See also
    /// - <https://tc39.es/ecma262/#sec-properties-of-the-arraybuffer-prototype-object>
    ArrayBuffer,
    /// `DataView.prototype.<name>`. Receiver is a
    /// [`crate::Value::DataView`].
    ///
    /// # See also
    /// - <https://tc39.es/ecma262/#sec-properties-of-the-dataview-prototype-object>
    DataView,
    /// `%TypedArray%.prototype.<name>`. Receiver is a
    /// [`crate::Value::TypedArray`]; the implementation reads the
    /// receiver's [`crate::binary::TypedArrayKind`] to specialise.
    ///
    /// # See also
    /// - <https://tc39.es/ecma262/#sec-properties-of-the-%25typedarrayprototype%25-object>
    TypedArray,
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
    /// GC heap for object allocations the intrinsic must perform
    /// (RegExp `groups` / `indices` accessor objects in §22.2.7.7
    /// MakeMatchIndicesIndexPairArray, JSON reviver wrappers,
    /// boxed-primitive method receivers, …). Wrapped in
    /// [`std::cell::RefCell`] so a `fn(&IntrinsicArgs)` impl can
    /// upgrade to a unique borrow for the duration of an
    /// allocation; the underlying `&'a mut GcHeap` itself is the
    /// one and only mutator. The dispatch site is single-threaded
    /// and the runtime borrow is scoped to a single
    /// `[[Call]]`-shaped intrinsic body so re-entrancy can't strike
    /// here. Threaded explicitly through the active mutator context:
    /// no thread-local heap lookup.
    pub gc_heap: std::cell::RefCell<&'a mut otter_gc::GcHeap>,
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

impl From<otter_gc::OutOfMemory> for IntrinsicError {
    fn from(err: otter_gc::OutOfMemory) -> Self {
        Self::OutOfMemory {
            requested_bytes: err.requested_bytes(),
            heap_limit_bytes: err.heap_limit_bytes(),
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
        let mut gc_heap = otter_gc::GcHeap::new().expect("gc heap");
        let recv = Value::String(JsString::from_str("ab", &heap).unwrap());
        let arg = Value::String(JsString::from_str("cd", &heap).unwrap());
        let entry = STRING_TABLE
            .lookup(IntrinsicReceiver::String, "concatWith")
            .unwrap();
        let result = (entry.impl_fn)(&IntrinsicArgs {
            receiver: &recv,
            args: &[arg],
            string_heap: &heap,
            gc_heap: std::cell::RefCell::new(&mut gc_heap),
        })
        .unwrap();
        assert_eq!(result.display_string(), "abcd");
    }

    #[test]
    fn intrinsic_rejects_bad_receiver() {
        let heap = StringHeap::default();
        let mut gc_heap = otter_gc::GcHeap::new().expect("gc heap");
        let entry = STRING_TABLE
            .lookup(IntrinsicReceiver::String, "length")
            .unwrap();
        let err = (entry.impl_fn)(&IntrinsicArgs {
            receiver: &Value::Undefined,
            args: &[],
            string_heap: &heap,
            gc_heap: std::cell::RefCell::new(&mut gc_heap),
        })
        .unwrap_err();
        assert!(matches!(err, IntrinsicError::BadReceiver { .. }));
    }
}
