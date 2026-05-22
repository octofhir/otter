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

use crate::array::{self, JsArray};
use crate::binary::JsArrayBuffer;
use crate::collections::{self, CollectionError, JsMap, JsSet, JsWeakMap, JsWeakSet};
use crate::object::{self, JsObject};
use crate::{IteratorHandle, IteratorState, Value};
use otter_gc::raw::RawGc;

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
    /// GC heap for object and string allocations the intrinsic must
    /// perform. The intrinsic entry receives `&mut IntrinsicArgs`,
    /// so helper implementations borrow this directly without
    /// interior mutability.
    pub gc_heap: &'a mut otter_gc::GcHeap,
    /// External roots supplied by the caller when the intrinsic is
    /// invoked from a native/runtime path that has no visible VM
    /// frame stack. Intrinsics must combine these with their
    /// receiver, argument slice, and local buffers before any
    /// allocation that may trigger GC.
    pub allocation_roots: &'a [*mut RawGc],
}

impl IntrinsicArgs<'_> {
    /// Allocate an ordinary object while exposing the intrinsic
    /// receiver, call arguments, caller-provided roots, and local
    /// values that have not yet been published into the object.
    pub fn alloc_object_rooted(
        &mut self,
        value_roots: &[&Value],
        slice_roots: &[&[Value]],
    ) -> Result<JsObject, otter_gc::OutOfMemory> {
        let allocation_roots = self.allocation_roots;
        let receiver = self.receiver;
        let args = self.args;
        let mut external_visit = |visitor: &mut dyn FnMut(*mut RawGc)| {
            visit_intrinsic_allocation_roots(
                visitor,
                allocation_roots,
                receiver,
                args,
                value_roots,
                slice_roots,
            );
        };
        object::alloc_object_with_roots(self.gc_heap, &mut external_visit)
    }

    /// Insert into a `Map` while exposing the intrinsic receiver,
    /// call arguments, caller-provided roots, and pending key/value.
    pub fn map_set_rooted(
        &mut self,
        map: &mut JsMap,
        key: Value,
        value: Value,
    ) -> Result<(), otter_gc::OutOfMemory> {
        let allocation_roots = self.allocation_roots;
        let receiver = self.receiver;
        let args = self.args;
        let mut external_visit = |visitor: &mut dyn FnMut(*mut RawGc)| {
            visit_intrinsic_allocation_roots(visitor, allocation_roots, receiver, args, &[], &[]);
        };
        collections::map_set_with_roots(map, self.gc_heap, key, value, &mut external_visit)
    }

    /// Insert into a `Set` while exposing the intrinsic receiver,
    /// call arguments, caller-provided roots, and pending value.
    pub fn set_add_rooted(
        &mut self,
        set: &mut JsSet,
        value: Value,
    ) -> Result<(), otter_gc::OutOfMemory> {
        let allocation_roots = self.allocation_roots;
        let receiver = self.receiver;
        let args = self.args;
        let mut external_visit = |visitor: &mut dyn FnMut(*mut RawGc)| {
            visit_intrinsic_allocation_roots(visitor, allocation_roots, receiver, args, &[], &[]);
        };
        collections::set_add_with_roots(set, self.gc_heap, value, &mut external_visit)
    }

    /// Insert into a `WeakMap` while exposing intrinsic roots and pending
    /// key/value across storage reservation.
    pub fn weak_map_set_rooted(
        &mut self,
        map: &mut JsWeakMap,
        key: Value,
        value: Value,
    ) -> Result<(), CollectionError> {
        let allocation_roots = self.allocation_roots;
        let receiver = self.receiver;
        let args = self.args;
        let mut external_visit = |visitor: &mut dyn FnMut(*mut RawGc)| {
            visit_intrinsic_allocation_roots(visitor, allocation_roots, receiver, args, &[], &[]);
        };
        collections::weak_map_set_with_roots(map, self.gc_heap, key, value, &mut external_visit)
    }

    /// Insert into a `WeakSet` while exposing intrinsic roots and pending
    /// value across storage reservation.
    pub fn weak_set_add_rooted(
        &mut self,
        set: &mut JsWeakSet,
        value: Value,
    ) -> Result<(), CollectionError> {
        let allocation_roots = self.allocation_roots;
        let receiver = self.receiver;
        let args = self.args;
        let mut external_visit = |visitor: &mut dyn FnMut(*mut RawGc)| {
            visit_intrinsic_allocation_roots(visitor, allocation_roots, receiver, args, &[], &[]);
        };
        collections::weak_set_add_with_roots(set, self.gc_heap, value, &mut external_visit)
    }

    /// Allocate an array while exposing the intrinsic receiver, call
    /// arguments, caller-provided roots, and local pre-publish buffers.
    pub fn array_from_elements_rooted<I>(
        &mut self,
        elements: I,
        value_roots: &[&Value],
        slice_roots: &[&[Value]],
    ) -> Result<JsArray, otter_gc::OutOfMemory>
    where
        I: IntoIterator<Item = Value>,
    {
        let elements: Vec<Value> = elements.into_iter().collect();
        let allocation_roots = self.allocation_roots;
        let receiver = self.receiver;
        let args = self.args;
        let mut external_visit = |visitor: &mut dyn FnMut(*mut RawGc)| {
            visit_intrinsic_allocation_roots(
                visitor,
                allocation_roots,
                receiver,
                args,
                value_roots,
                slice_roots,
            );
        };
        array::from_elements_with_roots(self.gc_heap, elements, &mut external_visit)
    }

    /// Push into a receiver array while exposing the intrinsic root
    /// contract to any backing-storage growth.
    pub fn array_push_rooted(
        &mut self,
        array: JsArray,
        value: Value,
    ) -> Result<usize, otter_gc::OutOfMemory> {
        let value_root = value.clone();
        let allocation_roots = self.allocation_roots;
        let receiver = self.receiver;
        let args = self.args;
        let mut external_visit = |visitor: &mut dyn FnMut(*mut RawGc)| {
            visit_intrinsic_allocation_roots(
                visitor,
                allocation_roots,
                receiver,
                args,
                &[&value_root],
                &[],
            );
        };
        array::push_with_roots(array, self.gc_heap, value, &mut external_visit)
    }

    /// Allocate iterator state through the same root contract as
    /// intrinsic array/object allocation.
    pub fn alloc_iterator_state_rooted(
        &mut self,
        state: IteratorState,
        value_roots: &[&Value],
        slice_roots: &[&[Value]],
    ) -> Result<IteratorHandle, otter_gc::OutOfMemory> {
        let allocation_roots = self.allocation_roots;
        let receiver = self.receiver;
        let args = self.args;
        let mut external_visit = |visitor: &mut dyn FnMut(*mut RawGc)| {
            visit_intrinsic_allocation_roots(
                visitor,
                allocation_roots,
                receiver,
                args,
                value_roots,
                slice_roots,
            );
        };
        self.gc_heap.alloc_with_roots(state, &mut external_visit)
    }

    /// Wrap a freshly copied `ArrayBuffer` backing store while exposing the
    /// intrinsic receiver, arguments, caller roots, and local values if the
    /// external-memory reservation triggers GC.
    pub fn array_buffer_from_bytes_rooted(
        &mut self,
        bytes: Vec<u8>,
        value_roots: &[&Value],
        slice_roots: &[&[Value]],
    ) -> Result<JsArrayBuffer, otter_gc::OutOfMemory> {
        let allocation_roots = self.allocation_roots;
        let receiver = self.receiver;
        let args = self.args;
        let mut external_visit = |visitor: &mut dyn FnMut(*mut RawGc)| {
            visit_intrinsic_allocation_roots(
                visitor,
                allocation_roots,
                receiver,
                args,
                value_roots,
                slice_roots,
            );
        };
        JsArrayBuffer::from_bytes_with_roots(bytes, self.gc_heap, &mut external_visit)
    }

    /// Allocate a resizable `ArrayBuffer` backing store under the intrinsic
    /// root contract.
    pub fn array_buffer_resizable_rooted(
        &mut self,
        len: usize,
        max_byte_length: usize,
        value_roots: &[&Value],
        slice_roots: &[&[Value]],
    ) -> Result<Option<JsArrayBuffer>, otter_gc::OutOfMemory> {
        let allocation_roots = self.allocation_roots;
        let receiver = self.receiver;
        let args = self.args;
        let mut external_visit = |visitor: &mut dyn FnMut(*mut RawGc)| {
            visit_intrinsic_allocation_roots(
                visitor,
                allocation_roots,
                receiver,
                args,
                value_roots,
                slice_roots,
            );
        };
        JsArrayBuffer::new_resizable_with_roots(
            len,
            max_byte_length,
            self.gc_heap,
            &mut external_visit,
        )
    }

    /// Allocate a zero-filled fixed-length `ArrayBuffer` backing store under
    /// the intrinsic root contract.
    pub fn array_buffer_zeroed_rooted(
        &mut self,
        len: usize,
        value_roots: &[&Value],
        slice_roots: &[&[Value]],
    ) -> Result<Option<JsArrayBuffer>, otter_gc::OutOfMemory> {
        let allocation_roots = self.allocation_roots;
        let receiver = self.receiver;
        let args = self.args;
        let mut external_visit = |visitor: &mut dyn FnMut(*mut RawGc)| {
            visit_intrinsic_allocation_roots(
                visitor,
                allocation_roots,
                receiver,
                args,
                value_roots,
                slice_roots,
            );
        };
        JsArrayBuffer::try_new_with_roots(len, self.gc_heap, &mut external_visit)
    }
}

/// The intrinsic implementation function pointer.
pub type IntrinsicFn = fn(&mut IntrinsicArgs<'_>) -> Result<Value, IntrinsicError>;

fn visit_intrinsic_allocation_roots(
    visitor: &mut dyn FnMut(*mut RawGc),
    allocation_roots: &[*mut RawGc],
    receiver: &Value,
    args: &[Value],
    value_roots: &[&Value],
    slice_roots: &[&[Value]],
) {
    for &slot in allocation_roots {
        visitor(slot);
    }
    receiver.trace_value_slots(visitor);
    for value in args {
        value.trace_value_slots(visitor);
    }
    for value in value_roots {
        value.trace_value_slots(visitor);
    }
    for slice in slice_roots {
        for value in *slice {
            value.trace_value_slots(visitor);
        }
    }
}

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
    /// Argument falls outside the spec-mandated range. Distinct
    /// from [`Self::BadArgument`] so the runtime surface throws
    /// `RangeError` rather than `TypeError` (per ECMA-262 for
    /// e.g. `toFixed` `fractionDigits` outside `0..=100`).
    #[error("argument {index} out of range: {reason}")]
    OutOfRange {
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
/// fn impl_starts_with(args: &mut intrinsics::IntrinsicArgs<'_>)
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
    use crate::string::JsString;

    fn impl_length(args: &mut IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
        let recv = match args.receiver {
            Value::String(s) => s,
            _ => return Err(IntrinsicError::BadReceiver { expected: "string" }),
        };
        let n = recv.len();
        Ok(Value::String(JsString::from_str(
            &n.to_string(),
            args.gc_heap,
        )?))
    }

    fn impl_concat_with(args: &mut IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
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
        let out = JsString::concat(recv, arg0, args.gc_heap)?;
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
        let mut gc_heap = otter_gc::GcHeap::new().expect("gc heap");
        let recv = Value::String(JsString::from_str("ab", &mut gc_heap).unwrap());
        let arg = Value::String(JsString::from_str("cd", &mut gc_heap).unwrap());
        let entry = STRING_TABLE
            .lookup(IntrinsicReceiver::String, "concatWith")
            .unwrap();
        let result = (entry.impl_fn)(&mut IntrinsicArgs {
            receiver: &recv,
            args: &[arg],
            gc_heap: &mut gc_heap,
            allocation_roots: &[],
        })
        .unwrap();
        assert_eq!(result.display_string(&gc_heap), "abcd");
    }

    #[test]
    fn intrinsic_rejects_bad_receiver() {
        let mut gc_heap = otter_gc::GcHeap::new().expect("gc heap");
        let entry = STRING_TABLE
            .lookup(IntrinsicReceiver::String, "length")
            .unwrap();
        let err = (entry.impl_fn)(&mut IntrinsicArgs {
            receiver: &Value::Undefined,
            args: &[],
            gc_heap: &mut gc_heap,
            allocation_roots: &[],
        })
        .unwrap_err();
        assert!(matches!(err, IntrinsicError::BadReceiver { .. }));
    }
}
