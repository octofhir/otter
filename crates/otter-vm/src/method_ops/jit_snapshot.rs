//! Typed JIT compile snapshots for collection and primitive method ICs.
//!
//! These methods freeze guarded method metadata consumed directly by generated
//! code. Runtime method dispatch remains interpreter-owned.

use super::MethodCallIc;
use crate::{Interpreter, Value};

fn compressed_slot_byte(slot: u16) -> u32 {
    u32::from(slot) * std::mem::size_of::<crate::value::compressed::CompressedValue>() as u32
}

impl Interpreter {
    /// Snapshot a collection leaf method IC into JIT-readable guard metadata.
    ///
    /// This is intentionally stricter than the runtime IC guard: explicit
    /// prototype overrides, even if they point back to the canonical prototype,
    /// are left to the normal fallback path because generated code only checks
    /// the collection body's no-override/no-expando guard flags.
    /// Snapshot a monomorphic dense-array `push` / `pop` method-call IC into
    /// JIT-readable guard metadata for an inline fast path. Returns `None` for
    /// any other method, family, or when the prototype slot no longer holds the
    /// original native builtin. The emitted guard re-validates the prototype
    /// shape and builtin identity, so a stale snapshot can only side-exit,
    /// never miscompile.
    pub(crate) fn jit_array_method_feedback(
        &self,
        site: usize,
        push_safepoint_id: crate::native_abi::SafepointId,
    ) -> Option<crate::jit::JitArrayMethod> {
        use crate::native_abi::{
            NO_SAFEPOINT, STUB_ARRAY_POP_LEAF, STUB_ARRAY_PUSH_ALLOC, STUB_ARRAY_SHIFT_LEAF,
            STUB_ARRAY_UNSHIFT_ALLOC,
        };

        let ic = match self.feedback_directory.method_ic(site)? {
            MethodCallIc::Array(ic) => ic,
            MethodCallIc::Collection(_) => return None,
        };
        // `pop` / `shift` only rewrite the dense buffer, so they run as mutating
        // leaves with no safepoint; `push` / `unshift` may grow it and need the
        // allocating entry with a precise root map.
        use crate::array_prototype::ArrayMethodTag as Tag;
        use crate::jit::JitArrayMethodKind as Kind;
        let (kind, stub_id, safepoint_id) = match ic.tag {
            Tag::Pop => (Kind::Pop, STUB_ARRAY_POP_LEAF.id, NO_SAFEPOINT),
            Tag::Shift => (Kind::Shift, STUB_ARRAY_SHIFT_LEAF.id, NO_SAFEPOINT),
            Tag::Push => (Kind::Push, STUB_ARRAY_PUSH_ALLOC.id, push_safepoint_id),
            Tag::Unshift => (
                Kind::Unshift,
                STUB_ARRAY_UNSHIFT_ALLOC.id,
                push_safepoint_id,
            ),
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
        Some(crate::jit::JitArrayMethod {
            proto_offset: proto.offset(),
            proto_shape: crate::object::shape(proto, &self.gc_heap).offset(),
            method_value_byte: compressed_slot_byte(ic.proto_slot),
            builtin_fn_addr,
            kind,
            stub_id,
            safepoint_id,
        })
    }

    pub(crate) fn jit_primitive_method_guard(
        &self,
        hint: crate::jit::JitMethodHint,
    ) -> Option<crate::jit::JitPrimitiveMethodGuard> {
        use crate::jit::JitMethodHint;
        use crate::native_abi::{
            STUB_STRING_CHAR_CODE_AT_LEAF, STUB_STRING_CODE_POINT_AT_LEAF,
            STUB_STRING_ENDS_WITH_LEAF, STUB_STRING_INCLUDES_LEAF, STUB_STRING_INDEX_OF_LEAF,
            STUB_STRING_STARTS_WITH_LEAF,
        };
        let (name, leaf_stub_id) = match hint {
            JitMethodHint::StringCharCodeAt => ("charCodeAt", STUB_STRING_CHAR_CODE_AT_LEAF.id),
            JitMethodHint::StringCodePointAt => ("codePointAt", STUB_STRING_CODE_POINT_AT_LEAF.id),
            JitMethodHint::StringIndexOf => ("indexOf", STUB_STRING_INDEX_OF_LEAF.id),
            JitMethodHint::StringIncludes => ("includes", STUB_STRING_INCLUDES_LEAF.id),
            JitMethodHint::StringStartsWith => ("startsWith", STUB_STRING_STARTS_WITH_LEAF.id),
            JitMethodHint::StringEndsWith => ("endsWith", STUB_STRING_ENDS_WITH_LEAF.id),
            JitMethodHint::None | JitMethodHint::NumberToString => return None,
        };
        let proto = self.realm_intrinsics.string_prototype?;
        let receiver_type_tag = crate::string::JS_STRING_BODY_TYPE_TAG;
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
        Some(crate::jit::JitPrimitiveMethodGuard {
            proto_offset: proto.offset(),
            proto_shape: crate::object::shape(proto, &self.gc_heap).offset(),
            method_value_byte: compressed_slot_byte(hit.slot),
            builtin_fn_addr,
            leaf_stub_id,
            receiver_type_tag,
        })
    }

    pub(crate) fn jit_collection_leaf_method_feedback(
        &self,
        site: usize,
    ) -> Option<crate::jit::JitCollectionLeafMethod> {
        let ic = match self.feedback_directory.method_ic(site)? {
            MethodCallIc::Collection(ic) => ic,
            MethodCallIc::Array(_) => return None,
        };
        let stub_id = ic.leaf_stub_id?;
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
        Some(crate::jit::JitCollectionLeafMethod {
            receiver_type_tag,
            proto_offset: proto.offset(),
            proto_shape: crate::object::shape(proto, &self.gc_heap).offset(),
            method_value_byte: u32::from(ic.proto_slot) * std::mem::size_of::<Value>() as u32,
            builtin_fn_addr,
            leaf_stub_id: stub_id,
        })
    }

    /// Snapshot a collection allocating method IC into JIT-readable guard
    /// metadata.
    ///
    /// This deliberately carries no safepoint id. The backend owns
    /// instruction-level safepoint creation and must only call the descriptor's
    /// allocating ABI entry after publishing a precise root map for receiver,
    /// arguments, live frame slots, and tagged machine values.
    pub(crate) fn jit_collection_alloc_method_feedback(
        &self,
        site: usize,
        safepoint_id: crate::native_abi::SafepointId,
    ) -> Option<crate::jit::JitCollectionAllocMethod> {
        let ic = match self.feedback_directory.method_ic(site)? {
            MethodCallIc::Collection(ic) => ic,
            MethodCallIc::Array(_) => return None,
        };
        let stub_id = ic.alloc_stub_id?;
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
        Some(crate::jit::JitCollectionAllocMethod {
            receiver_type_tag,
            proto_offset: proto.offset(),
            proto_shape: crate::object::shape(proto, &self.gc_heap).offset(),
            method_value_byte: u32::from(ic.proto_slot) * std::mem::size_of::<Value>() as u32,
            builtin_fn_addr,
            alloc_stub_id: stub_id,
            safepoint_id,
            value_arg_count: 3,
        })
    }
}
