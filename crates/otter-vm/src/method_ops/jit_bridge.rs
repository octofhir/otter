//! JIT runtime bridges for `CallMethodValue` and the collection method IC.
//!
//! These methods are called from compiled leaf stubs (JIT off by default) to
//! resolve and invoke a receiver method, and to build / publish / clear the
//! per-site collection method inline cache the compiled tier reads. Split out
//! of the `CallMethodValue` interpreter dispatch in [`super`] for readability.

use smallvec::SmallVec;

use super::{CollectionMethodCallIc, MethodCallIc};
use crate::holt_stack::HoltStack;
use crate::native_abi::RuntimeStubId;
use crate::{ExecutionContext, Interpreter, Value, VmError, read_register, write_register};

fn compressed_slot_byte(slot: u16) -> u32 {
    u32::from(slot) * std::mem::size_of::<crate::value::compressed::CompressedValue>() as u32
}

impl Interpreter {
    /// JIT bridge for `CallMethodValue` (`recv.name(args…)`) from compiled code.
    ///
    /// Resolves the method through the full `[[Get]]` ladder
    /// ([`Self::get_method_value_for_call`]) and invokes it synchronously with
    /// `this` = `recv` via [`Self::run_callable_sync`] — the same primitive the
    /// `Op::Call` bridge uses, so native and ordinary bytecode methods complete
    /// inline and the result lands in `dst`. The frame PC is saved/restored so a
    /// later guard bail re-runs the compiled frame from PC 0.
    ///
    /// # Errors
    /// `TypeError` for a nullish receiver, `NotCallable` when the resolved
    /// property is not callable, plus any error the method itself throws.
    pub fn jit_runtime_call_method(
        &mut self,
        context: &ExecutionContext,
        stack: &mut HoltStack,
        frame_index: usize,
        dst: u16,
        recv_reg: u16,
        name_idx: u32,
        site: Option<usize>,
        arg_regs: &[u16],
        source: crate::JitRuntimeMethodStubSource,
    ) -> Result<(), VmError> {
        self.record_jit_runtime_method_stub(source);
        let recv = *read_register(&stack[frame_index], recv_reg)?;
        if recv.is_nullish() {
            let label = if recv.is_null() { "null" } else { "undefined" };
            return Err(self.err_type((format!("Cannot read properties of {label}")).into()));
        }
        let mut args: SmallVec<[Value; 8]> = SmallVec::with_capacity(arg_regs.len());
        for &r in arg_regs {
            args.push(*read_register(&stack[frame_index], r)?);
        }
        // Cached dense-array builtin: a guard hit dispatches without resolving
        // the method name, hashing the prototype slot, or string-matching.
        if let Some(site) = site {
            if let Some(result) =
                self.try_array_method_call_ic(context, site, recv, args.as_slice())
            {
                let value = result?;
                self.record_jit_method_array_fast_hit();
                write_register(&mut stack[frame_index], dst, value)?;
                return Ok(());
            }
            if let Some(result) = self.try_collection_method_call_ic(site, recv, args.as_slice()) {
                let value = result?;
                self.record_jit_method_collection_ic_hit();
                write_register(&mut stack[frame_index], dst, value)?;
                return Ok(());
            }
        }
        let name = context
            .string_constant_str_for_function(stack[frame_index].function_id, name_idx)
            .ok_or(VmError::InvalidOperand)?;
        if let Some(site) = site {
            if let Some(result) =
                self.try_fast_array_proto_method(context, site, recv, name, args.as_slice())
            {
                let value = result?;
                self.record_jit_method_array_fast_hit();
                write_register(&mut stack[frame_index], dst, value)?;
                return Ok(());
            }
            if let Some(result) =
                self.try_fast_collection_proto_method(site, recv, name, args.as_slice())
            {
                let value = result?;
                self.record_jit_method_fast_collection_hit();
                write_register(&mut stack[frame_index], dst, value)?;
                return Ok(());
            }
        }
        if name == "charCodeAt"
            && recv.is_string()
            && let Some(result) =
                self.try_fast_primitive_string_char_code_at(recv, args.as_slice())?
        {
            self.record_jit_method_string_fast_hit();
            write_register(&mut stack[frame_index], dst, result)?;
            return Ok(());
        }
        if name == "toString"
            && recv.is_number()
            && let Some(result) = self.try_fast_primitive_number_to_string(recv, args.as_slice())?
        {
            self.record_jit_method_number_fast_hit();
            write_register(&mut stack[frame_index], dst, result)?;
            return Ok(());
        }
        let saved_pc = stack[frame_index].pc;
        let method = self
            .get_method_value_for_call(context, stack, recv, name)?
            .unwrap_or_else(Value::undefined);
        if !self.is_callable_runtime(&method) {
            stack[frame_index].pc = saved_pc;
            return Err(VmError::NotCallable);
        }
        self.record_jit_method_generic_call();
        let result = self.run_callable_sync(context, &method, recv, args)?;
        stack[frame_index].pc = saved_pc;
        write_register(&mut stack[frame_index], dst, result)?;
        Ok(())
    }

    /// Fast non-allocating primitive `String.prototype.charCodeAt` bridge.
    ///
    /// Returns `Ok(true)` only when the receiver is a primitive string, the
    /// prototype still exposes the canonical builtin, and the argument shape is
    /// handled by the primitive fast path. Every miss falls back to the full
    /// method-call bridge so user overrides and coercions keep interpreter
    /// semantics.
    pub fn jit_runtime_try_string_char_code_at(
        &mut self,
        stack: &mut HoltStack,
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
        stack: &mut HoltStack,
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
        stack: &mut HoltStack,
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
        let result = crate::runtime_stubs::leaf_no_alloc_stub2_trampoline(
            &self.gc_heap as *const otter_gc::GcHeap,
            stub_id,
            recv.to_abi_bits(),
            key.to_abi_bits(),
        );
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
        stack: &mut HoltStack,
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
        stack: &HoltStack,
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
        let ic = match (*self.method_call_ics.get(site)?).as_ref().copied()? {
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
        let ic = match (*self.method_call_ics.get(site)?).as_ref().copied()? {
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
        let ic = match (*self.method_call_ics.get(site)?).as_ref().copied()? {
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

    pub(crate) fn clear_jit_collection_method_ic(&mut self, site: usize) {
        if let Some(slot) = self.jit_collection_method_ics.get_mut(site) {
            *slot = crate::jit::JitCollectionMethodIcSlot::EMPTY;
        }
    }

    pub(crate) fn publish_jit_collection_method_ic(
        &mut self,
        site: usize,
        ic: CollectionMethodCallIc,
    ) {
        let slot = self
            .jit_collection_method_ic_slot(ic)
            .unwrap_or(crate::jit::JitCollectionMethodIcSlot::EMPTY);
        if let Some(dst) = self.jit_collection_method_ics.get_mut(site) {
            *dst = slot;
        }
    }

    pub(crate) fn jit_collection_method_ic_slot(
        &self,
        ic: CollectionMethodCallIc,
    ) -> Option<crate::jit::JitCollectionMethodIcSlot> {
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
        Some(crate::jit::JitCollectionMethodIcSlot {
            state: crate::jit::JIT_COLLECTION_METHOD_IC_COLLECTION,
            receiver_type_tag,
            reserved0: 0,
            proto_offset: proto.offset(),
            proto_shape: crate::object::shape(proto, &self.gc_heap).offset(),
            method_value_byte: u32::from(ic.proto_slot) * std::mem::size_of::<Value>() as u32,
            leaf_stub_id: ic
                .leaf_stub_id
                .unwrap_or(crate::jit::JIT_COLLECTION_METHOD_IC_NO_STUB),
            alloc_stub_id: ic
                .alloc_stub_id
                .unwrap_or(crate::jit::JIT_COLLECTION_METHOD_IC_NO_STUB),
            builtin_fn_addr,
        })
    }
}
