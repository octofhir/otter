//! Typed JIT runtime operations for collection and primitive method ICs.
//!
//! These methods execute already-guarded leaf/allocating operations and build,
//! publish, or clear the per-site method IC mirrors read by compiled code.

use smallvec::SmallVec;

use super::MethodCallIc;
use crate::activation_stack::ActivationStack;
use crate::native_abi::RuntimeStubId;
use crate::{Interpreter, Value, VmError, read_register, write_register};

fn compressed_slot_byte(slot: u16) -> u32 {
    u32::from(slot) * std::mem::size_of::<crate::value::compressed::CompressedValue>() as u32
}

impl Interpreter {
    /// Fast non-allocating primitive `String.prototype.charCodeAt` bridge.
    ///
    /// Returns `Ok(true)` only when the receiver is a primitive string, the
    /// prototype still exposes the canonical builtin, and the argument shape is
    /// handled by the primitive fast path. Every miss falls back to the full
    /// method-call bridge so user overrides and coercions keep interpreter
    /// semantics.
    pub fn jit_runtime_try_string_char_code_at(
        &mut self,
        stack: &mut ActivationStack,
        frame_index: usize,
        dst: u16,
        recv: Value,
        arg: Value,
    ) -> Result<bool, VmError> {
        let Some(result) = self.try_fast_primitive_string_char_code_at(recv, &[arg])? else {
            return Ok(false);
        };
        self.record_jit_method_string_fast_hit();
        write_register(&mut stack[frame_index], dst, result)?;
        Ok(true)
    }

    /// Fast non-allocating primitive `String.prototype.charCodeAt` bridge that
    /// returns the value directly to compiled code.
    pub fn jit_runtime_try_string_char_code_at_value(
        &mut self,
        recv: Value,
        arg: Value,
    ) -> Result<Option<Value>, VmError> {
        let Some(result) = self.try_fast_primitive_string_char_code_at(recv, &[arg])? else {
            return Ok(None);
        };
        self.record_jit_method_string_fast_hit();
        Ok(Some(result))
    }

    /// Primitive `String.prototype.charCodeAt` bridge for code that has already
    /// validated the prototype builtin identity in generated guards.
    pub fn jit_runtime_string_char_code_at_value_guarded(
        &mut self,
        recv: Value,
        arg: Value,
    ) -> Result<Option<Value>, VmError> {
        let result =
            crate::string::prototype::fast_primitive_char_code_at(recv, &[arg], &mut self.gc_heap);
        if result.is_some() {
            self.record_jit_method_string_fast_hit();
        }
        Ok(result)
    }

    /// Fast primitive `Number.prototype.toString` bridge.
    ///
    /// The result may allocate, so callers must have already materialized the
    /// interpreter frame as the GC root source. Misses preserve full method-call
    /// semantics by falling back to the generic bridge.
    pub fn jit_runtime_try_number_to_string(
        &mut self,
        stack: &mut ActivationStack,
        frame_index: usize,
        dst: u16,
        recv: Value,
        arg: Value,
        has_arg: bool,
    ) -> Result<bool, VmError> {
        let result = if has_arg {
            self.try_fast_primitive_number_to_string(recv, &[arg])?
        } else {
            self.try_fast_primitive_number_to_string(recv, &[])?
        };
        let Some(result) = result else {
            return Ok(false);
        };
        self.record_jit_method_number_fast_hit();
        write_register(&mut stack[frame_index], dst, result)?;
        Ok(true)
    }

    /// JIT bridge for the leaf/no-allocation Map/Set method path.
    ///
    /// Returns `Ok(true)` when a live collection method IC validates and the
    /// matching leaf runtime stub produced a value written to `dst`.
    /// `Ok(false)` is a guard/stub miss; compiled code must continue to the
    /// existing direct-call/full-method fallback path. This bridge never
    /// performs method resolution or calls user code.
    pub fn jit_runtime_try_collection_leaf_method(
        &mut self,
        stack: &mut ActivationStack,
        frame_index: usize,
        dst: u16,
        recv_reg: u16,
        site: usize,
        arg_regs: &[u16],
    ) -> Result<bool, VmError> {
        let Some(stub_id) = self.jit_runtime_resolve_collection_leaf_method_stub(
            stack,
            frame_index,
            recv_reg,
            site,
        )?
        else {
            return Ok(false);
        };
        let recv = *read_register(&stack[frame_index], recv_reg)?;
        let key = if let Some(&reg) = arg_regs.first() {
            *read_register(&stack[frame_index], reg)?
        } else {
            Value::undefined()
        };
        let result =
            crate::runtime_stubs::invoke_leaf_no_alloc_stub2(&self.gc_heap, stub_id, recv, key);
        let Some(value) = result.into_value() else {
            return Ok(false);
        };
        write_register(&mut stack[frame_index], dst, value)?;
        Ok(true)
    }

    /// JIT bridge for the guarded collection method IC only.
    ///
    /// This accepts any live Map/Set IC operation, including allocating
    /// mutations and materializing string keys. It deliberately skips method
    /// name lookup, generic callable dispatch, and `NativeCtx`; callers fall
    /// back to [`Self::jit_runtime_call_method`] on `Ok(false)`.
    pub fn jit_runtime_try_collection_method_ic(
        &mut self,
        stack: &mut ActivationStack,
        frame_index: usize,
        dst: u16,
        recv_reg: u16,
        site: usize,
        arg_regs: &[u16],
    ) -> Result<bool, VmError> {
        let recv = *read_register(&stack[frame_index], recv_reg)?;
        let mut args: SmallVec<[Value; 8]> = SmallVec::with_capacity(arg_regs.len());
        for &r in arg_regs {
            args.push(*read_register(&stack[frame_index], r)?);
        }
        let Some(result) = self.try_collection_method_call_ic(site, recv, args.as_slice()) else {
            return Ok(false);
        };
        self.record_jit_runtime_collection_method_ic_stub();
        self.record_jit_method_collection_ic_hit();
        write_register(&mut stack[frame_index], dst, result?)?;
        Ok(true)
    }

    /// JIT bridge for the guarded collection method IC only.
    ///
    /// Returns the leaf stub descriptor id when receiver/prototype/builtin
    /// guards validate. The caller is responsible for invoking the returned
    /// VM-native leaf ABI entry against raw register-window values.
    pub fn jit_runtime_resolve_collection_leaf_method_stub(
        &mut self,
        stack: &ActivationStack,
        frame_index: usize,
        recv_reg: u16,
        site: usize,
    ) -> Result<Option<RuntimeStubId>, VmError> {
        let recv = *read_register(&stack[frame_index], recv_reg)?;
        if recv.is_nullish() {
            return Ok(None);
        }
        Ok(self
            .collection_method_call_ic_target(site, recv)
            .and_then(|target| target.leaf_stub_id))
    }

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
    /// shape and builtin identity, so a stale snapshot can only miss to the
    /// runtime bridge, never miscompile.
    pub(crate) fn jit_array_method_feedback(
        &self,
        site: usize,
    ) -> Option<crate::jit::JitArrayMethod> {
        let ic = match self.feedback_directory.method_ic(site)? {
            MethodCallIc::Array(ic) => ic,
            MethodCallIc::Collection(_) => return None,
        };
        let kind = match ic.tag {
            crate::array_prototype::ArrayMethodTag::Pop => crate::jit::JitArrayMethodKind::Pop,
            crate::array_prototype::ArrayMethodTag::Push => crate::jit::JitArrayMethodKind::Push,
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
        })
    }

    pub(crate) fn jit_primitive_method_guard(
        &self,
        hint: crate::jit::JitMethodHint,
    ) -> Option<crate::jit::JitPrimitiveMethodGuard> {
        let (proto, name) = match hint {
            crate::jit::JitMethodHint::StringCharCodeAt => {
                (self.realm_intrinsics.string_prototype?, "charCodeAt")
            }
            crate::jit::JitMethodHint::None | crate::jit::JitMethodHint::NumberToString => {
                return None;
            }
        };
        let (hit, lookup) = crate::object::lookup_own_slot(proto, &self.gc_heap, name);
        let hit = hit?;
        let method = match lookup {
            crate::object::PropertyLookup::Data { value, .. } => value,
            crate::object::PropertyLookup::Accessor { .. }
            | crate::object::PropertyLookup::Absent => return None,
        };
        match hint {
            crate::jit::JitMethodHint::StringCharCodeAt => {
                if !crate::string::prototype::is_char_code_at_builtin(method, &self.gc_heap) {
                    return None;
                }
            }
            crate::jit::JitMethodHint::None | crate::jit::JitMethodHint::NumberToString => {
                return None;
            }
        }
        let builtin_fn_addr = method
            .as_native_function()
            .and_then(|native| native.jit_static_fn_addr(&self.gc_heap))?;
        Some(crate::jit::JitPrimitiveMethodGuard {
            proto_offset: proto.offset(),
            proto_shape: crate::object::shape(proto, &self.gc_heap).offset(),
            method_value_byte: compressed_slot_byte(hit.slot),
            builtin_fn_addr,
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
