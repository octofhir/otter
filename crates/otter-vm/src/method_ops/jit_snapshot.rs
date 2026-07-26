//! Typed JIT compile snapshots for guarded builtin method calls.
//!
//! These methods freeze guarded method metadata consumed directly by generated
//! code. Runtime method dispatch remains interpreter-owned.
//!
//! # Contents
//! - Collection reads and writes, dense-array mutations and primitive-string
//!   builtins, each described as one [`crate::jit::JitGuardedMethodCall`].
//!
//! # Invariants
//! - A snapshot names a receiver, a latch, a pinned prototype and a declared
//!   entry; the call protocol follows from the family the entry id resolves in,
//!   never from the builtin's identity.
//! - Every field is re-validated by the emitted guard, so a stale snapshot can
//!   only side-exit, never miscompile.

use super::MethodCallIc;
use crate::Interpreter;
use crate::jit::{JitGuardedMethodCall, JitGuardedReceiver, JitReceiverLatch};

fn compressed_slot_byte(slot: u16) -> u32 {
    u32::from(slot) * std::mem::size_of::<crate::value::compressed::CompressedValue>() as u32
}

/// The 32-bit no-expando/no-override word every `Map` and `Set` body carries.
fn collection_latch() -> JitReceiverLatch {
    JitReceiverLatch::Flags {
        byte: otter_gc::header::HEADER_SIZE as u32
            + crate::collections::MAP_BODY_JIT_GUARD_FLAGS_OFFSET as u32,
    }
}

/// An array body's exotic sidecar. A null sidecar means the realm
/// `%Array.prototype%` is still the receiver's `[[Prototype]]` and nothing on
/// the instance can shadow the method or override an element's attributes,
/// which is what makes the prototype-slot guard sufficient.
fn dense_array_latch() -> JitReceiverLatch {
    JitReceiverLatch::Sidecar {
        byte: otter_gc::header::HEADER_SIZE as u32
            + std::mem::offset_of!(crate::array::ArrayBody, exotic) as u32,
    }
}

impl Interpreter {
    /// Snapshot a monomorphic dense-array mutation site (`push` / `pop` /
    /// `shift` / `unshift`) into JIT-readable guard metadata.
    ///
    /// `pop` / `shift` only rewrite the dense buffer, so they run as mutating
    /// leaves with no safepoint; `push` / `unshift` may grow it and need the
    /// allocating entry with a precise root map. Returns `None` for any other
    /// method, family, or when the prototype slot no longer holds the original
    /// native builtin.
    pub(crate) fn jit_array_method_call(
        &self,
        site: usize,
        alloc_safepoint_id: crate::native_abi::SafepointId,
    ) -> Option<JitGuardedMethodCall> {
        use crate::native_abi::{
            NO_SAFEPOINT, STUB_ARRAY_POP_LEAF, STUB_ARRAY_PUSH_ALLOC, STUB_ARRAY_SHIFT_LEAF,
            STUB_ARRAY_UNSHIFT_ALLOC,
        };

        let ic = match self.feedback_directory.method_ic(site)? {
            MethodCallIc::Array(ic) => ic,
            MethodCallIc::Collection(_) | MethodCallIc::Ordinary(_) => return None,
        };
        use crate::array_prototype::ArrayMethodTag as Tag;
        let (stub_id, safepoint_id, argument_count) = match ic.tag {
            Tag::Pop => (STUB_ARRAY_POP_LEAF.id, NO_SAFEPOINT, 0),
            Tag::Shift => (STUB_ARRAY_SHIFT_LEAF.id, NO_SAFEPOINT, 0),
            Tag::Push => (STUB_ARRAY_PUSH_ALLOC.id, alloc_safepoint_id, 1),
            Tag::Unshift => (STUB_ARRAY_UNSHIFT_ALLOC.id, alloc_safepoint_id, 1),
            _ => return None,
        };
        let proto = self.realm_intrinsics.array_prototype?;
        let method = crate::object::data_slot_value_at(proto, &self.gc_heap, ic.proto_slot)?;
        if !ic.tag.matches_builtin(method, &self.gc_heap) {
            return None;
        }
        let builtin_fn_addr = method
            .as_native_function()
            .and_then(|native| native.jit_static_fn_addr(&self.gc_heap))?;
        Some(JitGuardedMethodCall {
            receiver: JitGuardedReceiver::Exotic {
                type_tag: crate::array::ARRAY_BODY_TYPE_TAG,
                latch: dense_array_latch(),
                proto_offset: proto.offset(),
            },
            holder_shape: crate::object::shape(proto, &self.gc_heap).offset(),
            method_value_byte: compressed_slot_byte(ic.proto_slot),
            builtin_fn_addr,
            entry_stub_id: stub_id,
            safepoint_id,
            argument_count,
        })
    }

    /// One primitive-receiver builtin call (`"…".charCodeAt(i)`) as a guarded
    /// method call.
    ///
    /// The receiver's cell type tag stands in for a collection's latch word: a
    /// primitive body carries no expando or override state, so proving the tag
    /// plus the realm prototype's shape and method-slot identity is the whole
    /// guard.
    pub(crate) fn jit_primitive_method_call(
        &self,
        hint: crate::jit::JitMethodHint,
    ) -> Option<JitGuardedMethodCall> {
        use crate::jit::JitMethodHint;
        use crate::native_abi::{
            NO_SAFEPOINT, STUB_STRING_CHAR_CODE_AT_LEAF, STUB_STRING_CODE_POINT_AT_LEAF,
            STUB_STRING_ENDS_WITH_LEAF, STUB_STRING_INCLUDES_LEAF, STUB_STRING_INDEX_OF_LEAF,
            STUB_STRING_STARTS_WITH_LEAF,
        };
        let (name, stub_id) = match hint {
            JitMethodHint::StringCharCodeAt => ("charCodeAt", STUB_STRING_CHAR_CODE_AT_LEAF.id),
            JitMethodHint::StringCodePointAt => ("codePointAt", STUB_STRING_CODE_POINT_AT_LEAF.id),
            JitMethodHint::StringIndexOf => ("indexOf", STUB_STRING_INDEX_OF_LEAF.id),
            JitMethodHint::StringIncludes => ("includes", STUB_STRING_INCLUDES_LEAF.id),
            JitMethodHint::StringStartsWith => ("startsWith", STUB_STRING_STARTS_WITH_LEAF.id),
            JitMethodHint::StringEndsWith => ("endsWith", STUB_STRING_ENDS_WITH_LEAF.id),
            JitMethodHint::None | JitMethodHint::NumberToString => return None,
        };
        let proto = self.realm_intrinsics.string_prototype?;
        let (hit, lookup) = crate::object::lookup_own_slot(proto, &self.gc_heap, name);
        let hit = hit?;
        let method = match lookup {
            crate::object::PropertyLookup::Data { value, .. } => value,
            crate::object::PropertyLookup::Accessor { .. }
            | crate::object::PropertyLookup::Absent => return None,
        };
        let bridge = crate::string::prototype::prototype_bridge(name)?;
        if !crate::string::prototype::is_prototype_builtin(method, &self.gc_heap, bridge) {
            return None;
        }
        let builtin_fn_addr = method
            .as_native_function()
            .and_then(|native| native.jit_static_fn_addr(&self.gc_heap))?;
        Some(JitGuardedMethodCall {
            receiver: JitGuardedReceiver::Exotic {
                type_tag: crate::string::JS_STRING_BODY_TYPE_TAG,
                latch: JitReceiverLatch::None,
                proto_offset: proto.offset(),
            },
            holder_shape: crate::object::shape(proto, &self.gc_heap).offset(),
            method_value_byte: compressed_slot_byte(hit.slot),
            builtin_fn_addr,
            entry_stub_id: stub_id,
            safepoint_id: NO_SAFEPOINT,
            argument_count: 1,
        })
    }

    /// One collection builtin (`map.get` / `map.set` / `set.add` / …) as a
    /// guarded method call.
    ///
    /// A read resolves in the leaf family and runs with no safepoint; a write
    /// resolves in the allocating family and publishes `alloc_safepoint_id`.
    ///
    /// This is intentionally stricter than the runtime IC guard: explicit
    /// prototype overrides, even if they point back to the canonical prototype,
    /// are left to the normal fallback path because generated code only checks
    /// the collection body's no-override/no-expando guard flags.
    pub(crate) fn jit_collection_method_call(
        &self,
        site: usize,
        alloc_safepoint_id: crate::native_abi::SafepointId,
    ) -> Option<JitGuardedMethodCall> {
        let ic = match self.feedback_directory.method_ic(site)? {
            MethodCallIc::Collection(ic) => ic,
            MethodCallIc::Array(_) | MethodCallIc::Ordinary(_) => return None,
        };
        // A read that has a leaf entry never needs the allocating one: the leaf
        // family cannot collect, so it carries no safepoint and no root map.
        let (stub_id, safepoint_id) = match ic.leaf_stub_id {
            Some(leaf) => (leaf, crate::native_abi::NO_SAFEPOINT),
            None => (ic.alloc_stub_id?, alloc_safepoint_id),
        };
        let (proto, receiver_type_tag) = if ic.op.is_map() {
            (
                self.realm_intrinsics.map_prototype?,
                crate::collections::MAP_BODY_TYPE_TAG,
            )
        } else {
            (
                self.realm_intrinsics.set_prototype?,
                crate::collections::SET_BODY_TYPE_TAG,
            )
        };
        if crate::object::shape_id(proto, &self.gc_heap) != ic.proto_shape {
            return None;
        }
        let method = crate::object::data_slot_value_at(proto, &self.gc_heap, ic.proto_slot)?;
        if !ic.op.matches_builtin(method, &self.gc_heap) {
            return None;
        }
        let builtin_fn_addr = method
            .as_native_function()
            .and_then(|native| native.jit_static_fn_addr(&self.gc_heap))?;
        Some(JitGuardedMethodCall {
            receiver: JitGuardedReceiver::Exotic {
                type_tag: receiver_type_tag,
                latch: collection_latch(),
                proto_offset: proto.offset(),
            },
            holder_shape: crate::object::shape(proto, &self.gc_heap).offset(),
            method_value_byte: compressed_slot_byte(ic.proto_slot),
            builtin_fn_addr,
            entry_stub_id: stub_id,
            safepoint_id,
            argument_count: ic.op.argument_count(),
        })
    }
}
