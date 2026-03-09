//! Property access slow-path methods for the interpreter.
//!
//! Contains property get/set/has operations that walk prototype chains
//! with proxy trap support, string property access, and IC miss handling.

use super::*;
use crate::context::DispatchAction;

impl Interpreter {
    pub(super) fn get_property_value(
        &self,
        ctx: &mut VmContext,
        obj: &GcRef<JsObject>,
        key: &PropertyKey,
        receiver: &Value,
    ) -> VmResult<Value> {
        match obj.lookup_property_descriptor(key) {
            Some(PropertyDescriptor::Accessor { get, .. }) => {
                let Some(getter) = get else {
                    return Ok(Value::undefined());
                };
                if !getter.is_callable() {
                    return Err(VmError::type_error("getter is not a function"));
                }
                self.call_function(ctx, &getter, receiver.clone(), &[])
            }
            Some(PropertyDescriptor::Data { value, .. }) => Ok(value),
            _ => Ok(Value::undefined()),
        }
    }

    /// Cold slow path for GetPropConst: handles proxy, string, array.length,
    /// full IC miss with descriptor lookup + IC update, and primitive autoboxing.
    /// Separated from the hot IC-hit path to keep the branch predictor clean.
    #[cold]
    #[inline(never)]
    pub(super) fn getprop_const_slow(
        &self,
        ctx: &mut VmContext,
        module: &Arc<Module>,
        dst: Register,
        obj: Register,
        name: ConstantIndex,
        ic_index: u16,
        object: Value,
    ) -> VmResult<()> {
        let name_const = module
            .constants
            .get(name.0)
            .ok_or_else(|| VmError::internal("constant not found"))?;
        let name_str = name_const
            .as_string()
            .ok_or_else(|| VmError::internal("expected string constant"))?;

        // 1. Proxy
        if let Some(proxy) = object.as_proxy() {
            let key = Self::utf16_key(name_str);
            let key_value = Value::string(JsString::intern_utf16(name_str));
            let receiver = object.clone();
            let result = {
                let mut ncx = crate::context::NativeContext::new(ctx, self);
                crate::proxy_operations::proxy_get(&mut ncx, proxy, &key, key_value, receiver)?
            };
            ctx.set_register(dst.0, result);
            return Ok(());
        }

        // 2. String — quicken to GetPropString for future hits
        if object.as_string().is_some() {
            if let Some(frame) = ctx.current_frame() {
                if let Some(func) = module.function(frame.function_index) {
                    func.quicken_instruction(
                        frame.pc,
                        Instruction::GetPropString {
                            dst,
                            obj,
                            name,
                            ic_index,
                        },
                    );
                }
            }
            return self.handle_string_prop_access(ctx, &object, name_str, dst);
        }

        // 3. Object path: array.length, accessor dispatch, IC miss + update
        if let Some(obj_ref) = object.as_object() {
            // Array .length — quicken to GetArrayLength
            if obj_ref.is_array() && Self::utf16_eq_ascii(name_str, "length") {
                if let Some(frame) = ctx.current_frame() {
                    if let Some(func) = module.function(frame.function_index) {
                        func.quicken_instruction(
                            frame.pc,
                            Instruction::GetArrayLength {
                                dst,
                                obj,
                                name,
                                ic_index,
                            },
                        );
                    }
                }
                ctx.set_register(dst.0, Value::int32(obj_ref.array_length() as i32));
                return Ok(());
            }

            let receiver = object.clone();
            let key = Self::utf16_key(name_str);

            match obj_ref.lookup_property_descriptor(&key) {
                Some(crate::object::PropertyDescriptor::Accessor { get, .. }) => {
                    let Some(getter) = get else {
                        ctx.set_register(dst.0, Value::undefined());
                        return Ok(());
                    };

                    if let Some(native_fn) = getter.as_native_function() {
                        let result = self.call_native_fn(ctx, native_fn, &receiver, &[])?;
                        ctx.set_register(dst.0, result);
                        return Ok(());
                    } else if let Some(closure) = getter.as_function() {
                        ctx.set_pending_args_empty();
                        ctx.set_pending_this(receiver);
                        ctx.dispatch_action = Some(DispatchAction::Call {
                            func_index: closure.function_index,
                            module_id: closure.module.module_id,
                            argc: 0,
                            return_reg: dst.0,
                            is_construct: false,
                            is_async: closure.is_async,
                            upvalues: closure.upvalues.clone(),
                        });
                        return Ok(());
                    } else {
                        return Err(VmError::type_error("getter is not a function"));
                    }
                }
                _ => {
                    // IC miss: full proto walk + IC state update
                    if !obj_ref.is_dictionary_mode() {
                        let mut current_obj = Some(obj_ref.clone());
                        let mut depth = 0;
                        let mut found_offset = None;
                        let mut found_shape = 0;

                        while let Some(cur) = current_obj.take() {
                            if cur.is_dictionary_mode() {
                                break;
                            }
                            if let Some(offset) = cur.shape_get_offset(&key) {
                                found_offset = Some(offset);
                                found_shape = cur.shape_id();
                                break;
                            }
                            if let Some(proto) = cur.prototype().as_object() {
                                current_obj = Some(proto.clone());
                                depth += 1;
                            }
                        }

                        if let Some(offset) = found_offset {
                            let frame = ctx
                                .current_frame()
                                .ok_or_else(|| VmError::internal("no frame"))?;
                            let feedback = frame.feedback().write();
                            if let Some(ic) = feedback.get_mut(ic_index as usize) {
                                use otter_vm_bytecode::function::InlineCacheState;
                                let shape_ptr = obj_ref.shape_id();
                                let proto_shape_id = if depth > 0 { found_shape } else { 0 };
                                let current_epoch = ctx.cached_proto_epoch;

                                match &mut ic.ic_state {
                                    InlineCacheState::Uninitialized => {
                                        ic.ic_state = InlineCacheState::Monomorphic {
                                            shape_id: shape_ptr,
                                            proto_shape_id: 0,
                                            depth: 0,
                                            offset: offset as u32,
                                        };
                                        ic.proto_epoch = current_epoch;
                                    }
                                    InlineCacheState::Monomorphic {
                                        shape_id: old_shape,
                                        proto_shape_id: old_proto_shape,
                                        depth: old_depth,
                                        offset: old_offset,
                                    } => {
                                        if *old_shape != shape_ptr {
                                            let mut entries = [(0u64, 0u64, 0u8, 0u32); 4];
                                            entries[0] = (
                                                *old_shape,
                                                *old_proto_shape,
                                                *old_depth,
                                                *old_offset,
                                            );
                                            entries[1] =
                                                (shape_ptr, proto_shape_id, depth, offset as u32);
                                            ic.ic_state =
                                                InlineCacheState::Polymorphic { count: 2, entries };
                                            ic.proto_epoch = current_epoch;
                                        }
                                    }
                                    InlineCacheState::Polymorphic { count, entries } => {
                                        let mut found = false;
                                        for i in 0..(*count as usize) {
                                            if entries[i].0 == shape_ptr {
                                                found = true;
                                                break;
                                            }
                                        }
                                        if !found {
                                            if (*count as usize) < 4 {
                                                entries[*count as usize] = (
                                                    shape_ptr,
                                                    proto_shape_id,
                                                    depth,
                                                    offset as u32,
                                                );
                                                *count += 1;
                                                ic.proto_epoch = current_epoch;
                                            } else {
                                                ic.ic_state = InlineCacheState::Megamorphic;
                                            }
                                        }
                                    }
                                    _ => {}
                                }

                                // Quickening
                                ic.hit_count = ic.hit_count.saturating_add(1);
                                if ic.hit_count >= otter_vm_bytecode::function::QUICKENING_WARMUP {
                                    if let InlineCacheState::Monomorphic {
                                        shape_id,
                                        offset,
                                        depth: ic_depth,
                                        ..
                                    } = ic.ic_state
                                    {
                                        if let Some(func) = module.function(frame.function_index) {
                                            let pc = frame.pc;
                                            Self::try_quicken_property_access(
                                                func,
                                                pc,
                                                &Instruction::GetPropConst {
                                                    dst,
                                                    obj,
                                                    name,
                                                    ic_index,
                                                },
                                                shape_id,
                                                offset,
                                                ic_depth,
                                                ic.proto_epoch,
                                            );
                                        }
                                    }
                                }
                            }
                        }
                    }
                    let key_value = Value::string(JsString::intern_utf16(name_str));
                    let value =
                        self.get_with_proxy_chain(ctx, &obj_ref, &key, key_value, &receiver)?;
                    ctx.set_register(dst.0, value);
                    return Ok(());
                }
            }
        }

        // 4. Primitive autoboxing (number, boolean, symbol)
        let key = Self::utf16_key(name_str);
        if object.is_number() {
            if let Some(number_obj) = ctx.get_global("Number").and_then(|v| v.as_object()) {
                if let Some(proto) = number_obj
                    .get(&PropertyKey::string("prototype"))
                    .and_then(|v| v.as_object())
                {
                    let value = proto.get(&key).unwrap_or_else(Value::undefined);
                    ctx.set_register(dst.0, value);
                    return Ok(());
                }
            }
        } else if object.is_boolean() {
            if let Some(boolean_obj) = ctx.get_global("Boolean").and_then(|v| v.as_object()) {
                if let Some(proto) = boolean_obj
                    .get(&PropertyKey::string("prototype"))
                    .and_then(|v| v.as_object())
                {
                    let value = proto.get(&key).unwrap_or_else(Value::undefined);
                    ctx.set_register(dst.0, value);
                    return Ok(());
                }
            }
        } else if object.is_symbol() {
            if let Some(symbol_obj) = ctx.get_global("Symbol").and_then(|v| v.as_object()) {
                if let Some(proto) = symbol_obj
                    .get(&PropertyKey::string("prototype"))
                    .and_then(|v| v.as_object())
                {
                    let value = self.get_property_value(ctx, &proto, &key, &object)?;
                    ctx.set_register(dst.0, value);
                    return Ok(());
                }
            }
        }

        ctx.set_register(dst.0, Value::undefined());
        Ok(())
    }

    /// Handle string property access (length, index, String.prototype method).
    /// Shared by GetPropConst slow path and GetPropString handler.
    #[inline]
    pub(super) fn handle_string_prop_access(
        &self,
        ctx: &mut VmContext,
        object: &Value,
        name_str: &[u16],
        dst: Register,
    ) -> VmResult<()> {
        let str_ref = object.as_string().unwrap();
        if Self::utf16_eq_ascii(name_str, "length") {
            ctx.set_register(dst.0, Value::int32(str_ref.len_utf16() as i32));
            return Ok(());
        }

        if let Some(index) = Self::utf16_to_index(name_str) {
            let units = str_ref.as_utf16();
            if let Some(unit) = units.get(index as usize) {
                let ch = JsString::intern_utf16(&[*unit]);
                ctx.set_register(dst.0, Value::string(ch));
            } else {
                ctx.set_register(dst.0, Value::undefined());
            }
            return Ok(());
        }

        if let Some(proto) = ctx.string_prototype() {
            let key = Self::utf16_key(name_str);
            let value = proto.get(&key).unwrap_or_else(Value::undefined);
            ctx.set_register(dst.0, value);
            return Ok(());
        }

        ctx.set_register(dst.0, Value::undefined());
        Ok(())
    }

    /// Get a property from an object, walking the prototype chain with proxy trap support.
    /// Unlike `JsObject::get()` which transparently bypasses proxy traps in the prototype chain,
    /// this method properly dispatches to `proxy_get` when a Proxy is encountered.
    pub(crate) fn get_with_proxy_chain(
        &self,
        ctx: &mut VmContext,
        obj: &GcRef<JsObject>,
        key: &PropertyKey,
        key_value: Value,
        receiver: &Value,
    ) -> VmResult<Value> {
        // 1. Check own property (shape/dictionary + elements)
        if let Some(value) = Self::get_own_value(obj, key) {
            return Ok(value);
        }
        // Check for accessor descriptors separately (getters need to be called)
        if let Some(desc) = obj.get_own_property_descriptor(key) {
            if let PropertyDescriptor::Accessor { get, .. } = desc {
                if let Some(getter) = get {
                    return self.call_function(ctx, &getter, receiver.clone(), &[]);
                }
                return Ok(Value::undefined());
            }
        }
        // 2. Walk prototype chain with proxy support
        let mut current = obj.prototype();
        let mut depth = 0;
        loop {
            if current.is_null() || current.is_undefined() {
                return Ok(Value::undefined());
            }
            depth += 1;
            if depth > 256 {
                return Ok(Value::undefined());
            }

            if let Some(proxy) = current.as_proxy() {
                let mut ncx = crate::context::NativeContext::new(ctx, self);
                return crate::proxy_operations::proxy_get(
                    &mut ncx,
                    proxy,
                    key,
                    key_value,
                    receiver.clone(),
                );
            }
            if let Some(proto_obj) = current.as_object() {
                if let Some(value) = Self::get_own_value(&proto_obj, key) {
                    return Ok(value);
                }
                if let Some(desc) = proto_obj.get_own_property_descriptor(key) {
                    if let PropertyDescriptor::Accessor { get, .. } = desc {
                        if let Some(getter) = get {
                            return self.call_function(ctx, &getter, receiver.clone(), &[]);
                        }
                        return Ok(Value::undefined());
                    }
                }
                current = proto_obj.prototype();
            } else {
                break;
            }
        }
        Ok(Value::undefined())
    }

    /// Get own data value from an object, checking both property descriptor and elements array.
    /// Returns None if not found or if it's an accessor (caller must handle accessors).
    fn get_own_value(obj: &GcRef<JsObject>, key: &PropertyKey) -> Option<Value> {
        // Check property descriptor first
        if let Some(desc) = obj.get_own_property_descriptor(key) {
            match desc {
                PropertyDescriptor::Data { value, .. } => return Some(value),
                PropertyDescriptor::Accessor { .. } => return None, // caller handles
                PropertyDescriptor::Deleted => return None,
            }
        }
        // Check indexed elements (JsObject::get does this for all objects, not just arrays)
        if let PropertyKey::Index(i) = key {
            let elements = obj.get_elements_storage().borrow();
            let idx = *i as usize;
            if let Some(v) = elements.get(idx) {
                if !v.is_hole() {
                    return Some(v);
                }
            }
        }
        None
    }

    /// Check if a property exists on an object, walking the prototype chain with proxy trap support.
    pub(super) fn has_with_proxy_chain(
        &self,
        ctx: &mut VmContext,
        obj: &GcRef<JsObject>,
        key: &PropertyKey,
        key_value: Value,
    ) -> VmResult<bool> {
        if Self::has_own_property(obj, key) {
            return Ok(true);
        }
        let mut current = obj.prototype();
        let mut depth = 0;
        loop {
            if current.is_null() || current.is_undefined() {
                return Ok(false);
            }
            depth += 1;
            if depth > 256 {
                return Ok(false);
            }
            if let Some(proxy) = current.as_proxy() {
                let mut ncx = crate::context::NativeContext::new(ctx, self);
                return crate::proxy_operations::proxy_has(&mut ncx, proxy, key, key_value);
            }
            if let Some(proto_obj) = current.as_object() {
                if Self::has_own_property(&proto_obj, key) {
                    return Ok(true);
                }
                current = proto_obj.prototype();
            } else {
                break;
            }
        }
        Ok(false)
    }

    /// Check if an object has an own property, including elements array.
    fn has_own_property(obj: &GcRef<JsObject>, key: &PropertyKey) -> bool {
        if obj.get_own_property_descriptor(key).is_some() {
            return true;
        }
        // Also check elements for Index keys (arguments object stores values in elements)
        if let PropertyKey::Index(i) = key {
            let elements = obj.get_elements_storage().borrow();
            let idx = *i as usize;
            if let Some(v) = elements.get(idx) {
                if !v.is_hole() {
                    return true;
                }
            }
        }
        false
    }

    /// Set a property on an object, walking the prototype chain with proxy trap support.
    ///
    /// Per ES2023 §9.1.9 OrdinarySet:
    /// If the object doesn't have the own property and a proxy is found in the prototype
    /// chain, the proxy's [[Set]] trap should be invoked with the original receiver.
    pub(super) fn set_with_proxy_chain(
        &self,
        ctx: &mut VmContext,
        obj: &GcRef<JsObject>,
        key: &PropertyKey,
        key_value: Value,
        value: Value,
        receiver: &Value,
    ) -> VmResult<bool> {
        // 1. Check for own property descriptor
        if let Some(desc) = obj.get_own_property_descriptor(key) {
            match desc {
                crate::object::PropertyDescriptor::Accessor { set, .. } => {
                    if let Some(setter) = set {
                        self.call_function(ctx, &setter, receiver.clone(), &[value])?;
                        return Ok(true);
                    }
                    return Ok(false);
                }
                _ => {
                    // Data property or deleted - set directly on receiver
                    if let Some(recv_obj) = receiver.as_object() {
                        return Ok(recv_obj.set(*key, value).is_ok());
                    }
                    return Ok(false);
                }
            }
        }
        // Also check elements for own Index properties
        if let PropertyKey::Index(i) = key {
            let elements = obj.get_elements_storage().borrow();
            let idx = *i as usize;
            if let Some(v) = elements.get(idx) {
                if !v.is_hole() {
                    drop(elements);
                    if let Some(recv_obj) = receiver.as_object() {
                        return Ok(recv_obj.set(*key, value).is_ok());
                    }
                    return Ok(false);
                }
            }
        }
        // 2. Walk prototype chain looking for proxy or accessor
        let mut current = obj.prototype();
        let mut depth = 0;
        loop {
            if current.is_null() || current.is_undefined() {
                // Not found in chain - set on receiver
                if let Some(recv_obj) = receiver.as_object() {
                    return Ok(recv_obj.set(*key, value).is_ok());
                }
                return Ok(false);
            }
            depth += 1;
            if depth > 256 {
                return Ok(false);
            }
            if let Some(proxy) = current.as_proxy() {
                let mut ncx = crate::context::NativeContext::new(ctx, self);
                return crate::proxy_operations::proxy_set(
                    &mut ncx,
                    proxy,
                    key,
                    key_value,
                    value,
                    receiver.clone(),
                );
            }
            if let Some(proto_obj) = current.as_object() {
                if let Some(desc) = proto_obj.get_own_property_descriptor(key) {
                    match desc {
                        crate::object::PropertyDescriptor::Accessor { set, .. } => {
                            if let Some(setter) = set {
                                self.call_function(ctx, &setter, receiver.clone(), &[value])?;
                                return Ok(true);
                            }
                            return Ok(false);
                        }
                        _ => {
                            // Data property found in prototype - set on receiver
                            if let Some(recv_obj) = receiver.as_object() {
                                return Ok(recv_obj.set(*key, value).is_ok());
                            }
                            return Ok(false);
                        }
                    }
                }
                current = proto_obj.prototype();
            } else {
                break;
            }
        }
        // Fallback: set directly on receiver
        if let Some(recv_obj) = receiver.as_object() {
            return Ok(recv_obj.set(*key, value).is_ok());
        }
        Ok(false)
    }
}
