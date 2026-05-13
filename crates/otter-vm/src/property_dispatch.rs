//! Property-related opcode helpers.
//!
//! The VM dispatch loop handles proxy or call-frame cases before entering the
//! dense register path. This module owns the remaining synchronous property
//! predicates that can run directly against a frame.
//!
//! # Contents
//! - Legacy `instanceof` prototype-chain fallback.
//! - Synchronous `in` / `HasProperty` checks for arrays and class static sides.
//!
//! # Invariants
//! - Stack-modifying proxy and `@@hasInstance` cases are handled before these
//!   helpers are called.
//! - Inputs are already decoded from the executable instruction format.
//!
//! # See also
//! - [`crate::executable`]
//! - [`crate::object`]

use crate::{
    ClassConstructor, ExecutionContext, Frame, Interpreter, JsObject, NumberValue, Value, VmError,
    VmGetOutcome, VmPropertyKey, array::JsArray, binary, collections_prototype, descriptor_value,
    function_metadata, make_array_iterator_factory, object, read_register, regexp_prototype,
    symbol, symbol_prototype, temporal, value_kind_name, write_register,
};

impl Interpreter {
    pub(crate) fn run_instanceof_legacy_regs(
        &self,
        frame: &mut Frame,
        dst: u16,
        lhs: u16,
        rhs: u16,
    ) -> Result<(), VmError> {
        let lhs = read_register(frame, lhs)?.clone();
        let rhs = read_register(frame, rhs)?.clone();
        let result = match (&lhs, &rhs) {
            (Value::Object(a), Value::Object(target)) => {
                match crate::object::get(*target, &self.gc_heap, "prototype") {
                    Some(Value::Object(proto)) => {
                        crate::object::has_in_proto_chain(*a, &self.gc_heap, proto)
                    }
                    _ => crate::object::has_in_proto_chain(*a, &self.gc_heap, *target),
                }
            }
            (Value::Object(a), Value::ClassConstructor(c)) => {
                crate::object::has_in_proto_chain(*a, &self.gc_heap, c.prototype(&self.gc_heap))
            }
            _ => false,
        };
        write_register(frame, dst, Value::Boolean(result))?;
        frame.pc += 1;
        Ok(())
    }

    pub(crate) fn run_has_property_regs(
        &self,
        frame: &mut Frame,
        dst: u16,
        lhs: u16,
        rhs: u16,
    ) -> Result<(), VmError> {
        let lhs = read_register(frame, lhs)?.clone();
        let rhs = read_register(frame, rhs)?.clone();
        let present = match &rhs {
            Value::Object(obj) => has_object_property(self, *obj, &lhs),
            Value::Array(arr) => has_array_property(self, *arr, &lhs),
            Value::ClassConstructor(c) => has_class_static_property(self, c, &lhs),
            _ => return Err(VmError::TypeMismatch),
        };
        write_register(frame, dst, Value::Boolean(present))?;
        frame.pc += 1;
        Ok(())
    }

    pub(crate) fn run_delete_property_reg(
        &mut self,
        frame: &mut Frame,
        dst: u16,
        obj_reg: u16,
        name: &str,
    ) -> Result<(), VmError> {
        let receiver = read_register(frame, obj_reg)?.clone();
        let removed = match &receiver {
            Value::Object(o) => crate::object::delete(*o, &mut self.gc_heap, name),
            Value::Function { function_id } | Value::Closure { function_id, .. } => {
                self.ordinary_function_delete_own_property(*function_id, name)
            }
            Value::NativeFunction(native) => native.delete_own_property(&mut self.gc_heap, name),
            Value::BoundFunction(bound) => {
                function_metadata::bound_delete_own_property(bound, &mut self.gc_heap, name)
            }
            other => {
                return Err(VmError::TypeError {
                    message: format!(
                        "Cannot delete property '{name}' of {}",
                        value_kind_name(other)
                    ),
                });
            }
        };
        write_register(frame, dst, Value::Boolean(removed))?;
        frame.pc += 1;
        Ok(())
    }

    pub(crate) fn run_delete_element_regs(
        &mut self,
        frame: &mut Frame,
        dst: u16,
        obj_reg: u16,
        idx_reg: u16,
    ) -> Result<(), VmError> {
        let receiver = read_register(frame, obj_reg)?.clone();
        let idx = read_register(frame, idx_reg)?.clone();
        let removed = match (&receiver, idx) {
            (Value::Object(obj), Value::Symbol(sym)) => {
                crate::object::delete_symbol(*obj, &mut self.gc_heap, &sym)
            }
            (Value::Object(obj), Value::String(s)) => {
                crate::object::delete(*obj, &mut self.gc_heap, &s.to_lossy_string())
            }
            (Value::Object(obj), Value::Number(n)) => match n.as_smi() {
                Some(v) if v >= 0 => crate::object::delete(*obj, &mut self.gc_heap, &v.to_string()),
                _ => crate::object::delete(*obj, &mut self.gc_heap, &n.to_display_string()),
            },
            (
                Value::Function { function_id } | Value::Closure { function_id, .. },
                Value::String(s),
            ) => self.ordinary_function_delete_own_property(*function_id, &s.to_lossy_string()),
            (Value::NativeFunction(native), Value::String(s)) => {
                native.delete_own_property(&mut self.gc_heap, &s.to_lossy_string())
            }
            (Value::BoundFunction(bound), Value::String(s)) => {
                function_metadata::bound_delete_own_property(
                    bound,
                    &mut self.gc_heap,
                    &s.to_lossy_string(),
                )
            }
            _ => return Err(VmError::TypeMismatch),
        };
        write_register(frame, dst, Value::Boolean(removed))?;
        frame.pc += 1;
        Ok(())
    }

    pub(crate) fn run_get_prototype_regs(
        &self,
        frame: &mut Frame,
        dst: u16,
        src: u16,
    ) -> Result<(), VmError> {
        let value = read_register(frame, src)?.clone();
        let result = self.get_prototype_for_op(&value)?;
        write_register(frame, dst, result)?;
        frame.pc += 1;
        Ok(())
    }

    pub(crate) fn run_set_prototype_regs(
        &mut self,
        context: &ExecutionContext,
        frame: &mut Frame,
        obj_reg: u16,
        proto_reg: u16,
    ) -> Result<(), VmError> {
        let proto = match read_register(frame, proto_reg)? {
            Value::Object(_) | Value::Proxy(_) | Value::Null => {
                read_register(frame, proto_reg)?.clone()
            }
            Value::ClassConstructor(c) => Value::Object(c.statics(&self.gc_heap)),
            _ => return Err(VmError::TypeMismatch),
        };
        let receiver = read_register(frame, obj_reg)?.clone();
        match &receiver {
            Value::Object(_) => {
                let ok = self.set_prototype_value_proxy_aware(context, &receiver, &proto)?;
                if !ok {
                    return Err(VmError::TypeError {
                        message: "Object.setPrototypeOf failed".to_string(),
                    });
                }
            }
            Value::Function { .. }
            | Value::Closure { .. }
            | Value::BoundFunction(_)
            | Value::NativeFunction(_) => {}
            _ => return Err(VmError::TypeMismatch),
        }
        frame.pc += 1;
        Ok(())
    }

    pub(crate) fn run_load_property_reg(
        &mut self,
        context: &ExecutionContext,
        frame: &mut Frame,
        dst: u16,
        obj_reg: u16,
        name: &str,
    ) -> Result<(), VmError> {
        let value = match read_register(frame, obj_reg)? {
            Value::Object(o) => {
                crate::object::get(*o, &self.gc_heap, name).unwrap_or(Value::Undefined)
            }
            Value::ClassConstructor(c) => {
                if name == "prototype" {
                    Value::Object(c.prototype(&self.gc_heap))
                } else {
                    match crate::object::get(c.statics(&self.gc_heap), &self.gc_heap, name) {
                        Some(v) => v,
                        None if name == "name" || name == "length" => {
                            let ctor = c.ctor(&self.gc_heap);
                            match &ctor {
                                Value::Function { .. }
                                | Value::Closure { .. }
                                | Value::NativeFunction(_)
                                | Value::BoundFunction(_) => {
                                    let ctx = function_metadata::FunctionMetadataContext::new(
                                        context,
                                        &self.gc_heap,
                                        &self.string_heap,
                                        &self.function_user_props,
                                        &self.function_deleted_metadata,
                                    );
                                    function_metadata::callable_intrinsic_property(
                                        &ctx, &ctor, name,
                                    )?
                                }
                                _ => Value::Undefined,
                            }
                        }
                        None => Value::Undefined,
                    }
                }
            }
            Value::String(s) if name == "length" => {
                Value::Number(NumberValue::from_i32(s.len() as i32))
            }
            v @ Value::Array(_) => {
                let direct = if let Value::Array(a) = v {
                    crate::array::get_named_property(*a, &self.gc_heap, name)
                } else {
                    None
                };
                match direct {
                    Some(value) => value,
                    None => self.load_from_constructor_prototype(context, "Array", v, name)?,
                }
            }
            Value::Function { function_id } => {
                let fid = *function_id;
                self.function_property_get(context, fid, name)?
            }
            Value::Closure { function_id, .. } => {
                let fid = *function_id;
                self.function_property_get(context, fid, name)?
            }
            Value::NativeFunction(native) => {
                match native.own_property_descriptor(&self.gc_heap, &self.string_heap, name)? {
                    Some(desc) => descriptor_value(&desc),
                    None => self
                        .load_function_prototype_method(name)
                        .or_else(|| self.load_object_prototype_method(name))
                        .unwrap_or(Value::Undefined),
                }
            }
            Value::BoundFunction(bound) => match function_metadata::bound_own_property_descriptor(
                bound,
                &self.gc_heap,
                &self.string_heap,
                name,
            )? {
                Some(desc) => descriptor_value(&desc),
                None => self
                    .load_function_prototype_method(name)
                    .or_else(|| self.load_object_prototype_method(name))
                    .unwrap_or(Value::Undefined),
            },
            v @ Value::RegExp(_) => {
                let direct = if let Value::RegExp(r) = v {
                    regexp_prototype::load_property(r, &self.gc_heap, name, &self.string_heap)
                } else {
                    Value::Undefined
                };
                match direct {
                    Value::Undefined => {
                        self.load_from_constructor_prototype(context, "RegExp", v, name)?
                    }
                    value => value,
                }
            }
            Value::Symbol(s) => symbol_prototype::load_property(s, name),
            Value::Iterator(_) => match name {
                "next" | "return" | "throw" => {
                    let receiver_value = read_register(frame, obj_reg)?.clone();
                    self.synthesize_iterator_method(name, receiver_value)?
                }
                _ => Value::Undefined,
            },
            v @ (Value::WeakRef(_) | Value::FinalizationRegistry(_)) => {
                let proto_name = match v {
                    Value::WeakRef(_) => "WeakRef",
                    Value::FinalizationRegistry(_) => "FinalizationRegistry",
                    _ => unreachable!(),
                };
                self.load_from_constructor_prototype(context, proto_name, v, name)?
            }
            v @ Value::Promise(_) => {
                self.load_from_constructor_prototype(context, "Promise", v, name)?
            }
            v @ (Value::Map(_) | Value::Set(_) | Value::WeakMap(_) | Value::WeakSet(_)) => {
                match collections_prototype::load_property_with_heap(v, name, &self.gc_heap) {
                    Value::Undefined => {
                        let proto_name = match v {
                            Value::Map(_) => "Map",
                            Value::Set(_) => "Set",
                            Value::WeakMap(_) => "WeakMap",
                            Value::WeakSet(_) => "WeakSet",
                            _ => unreachable!(),
                        };
                        self.load_from_constructor_prototype(context, proto_name, v, name)?
                    }
                    value => value,
                }
            }
            Value::Temporal(t) => temporal::load_property(t, name),
            v @ Value::ArrayBuffer(_) => {
                let (direct, is_shared) = if let Value::ArrayBuffer(b) = v {
                    (
                        binary::array_buffer_prototype::load_property(b, name),
                        b.is_shared(),
                    )
                } else {
                    (Value::Undefined, false)
                };
                match direct {
                    Value::Undefined => {
                        let proto_name = if is_shared {
                            "SharedArrayBuffer"
                        } else {
                            "ArrayBuffer"
                        };
                        self.load_from_constructor_prototype(context, proto_name, v, name)?
                    }
                    value => value,
                }
            }
            v @ Value::DataView(_) => {
                let direct = if let Value::DataView(dv) = v {
                    binary::data_view_prototype::load_property(dv, name)
                } else {
                    Value::Undefined
                };
                match direct {
                    Value::Undefined => {
                        self.load_from_constructor_prototype(context, "DataView", v, name)?
                    }
                    value => value,
                }
            }
            v @ Value::TypedArray(_) => {
                let direct = if let Value::TypedArray(t) = v {
                    binary::typed_array_prototype::load_property(t, name)
                } else {
                    Value::Undefined
                };
                match direct {
                    Value::Undefined => {
                        let kind_name = if let Value::TypedArray(t) = v {
                            t.kind().name()
                        } else {
                            unreachable!()
                        };
                        self.load_from_constructor_prototype(context, kind_name, v, name)?
                    }
                    value => value,
                }
            }
            v @ Value::BigInt(_) => {
                self.load_from_constructor_prototype(context, "BigInt", v, name)?
            }
            other => {
                return Err(VmError::TypeMismatchAt {
                    op: "property read",
                    kind: value_kind_name(other),
                });
            }
        };
        write_register(frame, dst, value)?;
        frame.pc += 1;
        Ok(())
    }

    pub(crate) fn run_store_property_reg(
        &mut self,
        context: &ExecutionContext,
        frame: &mut Frame,
        obj_reg: u16,
        name: &str,
        src: u16,
    ) -> Result<(), VmError> {
        let value = read_register(frame, src)?.clone();
        let strict = context.function_is_strict(frame.function_id);
        let receiver = read_register(frame, obj_reg)?.clone();
        let target = match &receiver {
            Value::Object(o) => Some(*o),
            Value::ClassConstructor(c) => Some(c.statics(&self.gc_heap)),
            Value::RegExp(r) => {
                regexp_prototype::store_property(r, &mut self.gc_heap, name, value.clone());
                None
            }
            Value::Array(a) => {
                crate::array::set_named_property(*a, &mut self.gc_heap, name, value.clone())?;
                None
            }
            Value::Function { function_id } | Value::Closure { function_id, .. } => {
                let fid = *function_id;
                if matches!(name, "name" | "length") {
                    if let Some(desc) =
                        self.ordinary_function_own_property_descriptor(Some(context), fid, name)?
                        && !desc.writable()
                    {
                        Self::failed_set_result(
                            strict,
                            format!("Cannot assign to read-only property '{name}' of function"),
                        )?;
                        None
                    } else {
                        let bag = match self.function_user_props.get(&fid).copied() {
                            Some(b) => b,
                            None => {
                                let new_bag = crate::object::alloc_object(&mut self.gc_heap)?;
                                self.function_user_props.insert(fid, new_bag);
                                new_bag
                            }
                        };
                        if let Some(metadata_key) =
                            function_metadata::ordinary_function_metadata_key(name)
                        {
                            self.function_deleted_metadata.remove(&(fid, metadata_key));
                        }
                        Some(bag)
                    }
                } else {
                    let bag = match self.function_user_props.get(&fid).copied() {
                        Some(b) => b,
                        None => {
                            let new_bag = crate::object::alloc_object(&mut self.gc_heap)?;
                            self.function_user_props.insert(fid, new_bag);
                            new_bag
                        }
                    };
                    Some(bag)
                }
            }
            Value::NativeFunction(native) => {
                match native.own_property_descriptor(&self.gc_heap, &self.string_heap, name)? {
                    Some(desc) if !desc.writable() => {
                        Self::failed_set_result(
                            strict,
                            format!(
                                "Cannot assign to read-only property '{name}' of function {}",
                                native.name(&self.gc_heap)
                            ),
                        )?;
                        None
                    }
                    _ => {
                        let enumerable =
                            function_metadata::ordinary_function_metadata_key(name).is_none();
                        let desc =
                            object::PropertyDescriptor::data(value.clone(), true, enumerable, true);
                        if !native.define_own_property(
                            &mut self.gc_heap,
                            &self.string_heap,
                            name,
                            desc,
                        ) {
                            Self::failed_set_result(
                                strict,
                                format!(
                                    "Cannot define property '{name}' on function {}",
                                    native.name(&self.gc_heap)
                                ),
                            )?;
                        }
                        None
                    }
                }
            }
            Value::BoundFunction(bound) => match function_metadata::bound_own_property_descriptor(
                bound,
                &self.gc_heap,
                &self.string_heap,
                name,
            )? {
                Some(desc) if !desc.writable() => {
                    Self::failed_set_result(
                        strict,
                        format!("Cannot assign to read-only property '{name}' of bound function"),
                    )?;
                    None
                }
                _ => {
                    let desc = object::PropertyDescriptor::data(value.clone(), true, true, true);
                    if !function_metadata::bound_define_own_property(
                        bound,
                        &mut self.gc_heap,
                        &self.string_heap,
                        name,
                        desc,
                    ) {
                        Self::failed_set_result(
                            strict,
                            format!("Cannot define property '{name}' on bound function"),
                        )?;
                    }
                    None
                }
            },
            Value::Undefined | Value::Null | Value::Hole => {
                return Err(VmError::TypeError {
                    message: format!(
                        "Cannot set property '{name}' on {}",
                        value_kind_name(&receiver)
                    ),
                });
            }
            Value::Boolean(_)
            | Value::Number(_)
            | Value::String(_)
            | Value::Symbol(_)
            | Value::BigInt(_) => {
                Self::failed_set_result(
                    strict,
                    format!(
                        "Cannot set property '{name}' on {}",
                        value_kind_name(&receiver)
                    ),
                )?;
                None
            }
            other => {
                return Err(VmError::TypeError {
                    message: format!("Cannot set property '{name}' on {}", value_kind_name(other)),
                });
            }
        };
        if let Some(target) = target {
            crate::object::set(target, &mut self.gc_heap, name, value);
        }
        frame.pc += 1;
        Ok(())
    }

    pub(crate) fn run_load_element_regs(
        &mut self,
        context: &ExecutionContext,
        frame: &mut Frame,
        dst: u16,
        recv_reg: u16,
        idx_reg: u16,
    ) -> Result<(), VmError> {
        let recv = read_register(frame, recv_reg)?.clone();
        let idx_value = read_register(frame, idx_reg)?.clone();
        let value = match (&recv, &idx_value) {
            (Value::Object(obj), Value::Symbol(sym)) => {
                crate::object::get_symbol(*obj, &self.gc_heap, sym).unwrap_or(Value::Undefined)
            }
            (Value::Object(obj), Value::String(key)) => {
                crate::object::get(*obj, &self.gc_heap, &key.to_lossy_string())
                    .unwrap_or(Value::Undefined)
            }
            (Value::Object(obj), Value::Number(n)) => {
                let key = n.to_display_string();
                crate::object::get(*obj, &self.gc_heap, &key).unwrap_or(Value::Undefined)
            }
            (
                Value::Function { function_id } | Value::Closure { function_id, .. },
                Value::String(key),
            ) => {
                match self.ordinary_function_own_property_descriptor(
                    Some(context),
                    *function_id,
                    &key.to_lossy_string(),
                )? {
                    Some(desc) => descriptor_value(&desc),
                    None => Value::Undefined,
                }
            }
            (Value::NativeFunction(native), Value::String(key)) => {
                match native.own_property_descriptor(
                    &self.gc_heap,
                    &self.string_heap,
                    &key.to_lossy_string(),
                )? {
                    Some(desc) => descriptor_value(&desc),
                    None => Value::Undefined,
                }
            }
            (Value::BoundFunction(bound), Value::String(key)) => {
                match function_metadata::bound_own_property_descriptor(
                    bound,
                    &self.gc_heap,
                    &self.string_heap,
                    &key.to_lossy_string(),
                )? {
                    Some(desc) => descriptor_value(&desc),
                    None => Value::Undefined,
                }
            }
            (Value::Array(arr), Value::Symbol(sym))
                if sym
                    .well_known_tag()
                    .is_some_and(|t| t == symbol::WellKnown::Iterator) =>
            {
                make_array_iterator_factory(*arr, &mut self.gc_heap)?
            }
            (Value::Map(m), Value::Symbol(sym))
                if sym
                    .well_known_tag()
                    .is_some_and(|t| t == symbol::WellKnown::Iterator) =>
            {
                collections_prototype::make_map_iterator_factory(*m, &mut self.gc_heap)?
            }
            (Value::Set(s), Value::Symbol(sym))
                if sym
                    .well_known_tag()
                    .is_some_and(|t| t == symbol::WellKnown::Iterator) =>
            {
                collections_prototype::make_set_iterator_factory(*s, &mut self.gc_heap)?
            }
            _ => {
                let idx = match &idx_value {
                    Value::Number(n) => {
                        crate::array::index_from_number(*n).ok_or(VmError::TypeMismatch)?
                    }
                    _ => return Err(VmError::TypeMismatch),
                };
                match recv {
                    Value::Array(a) => crate::array::get(a, &self.gc_heap, idx),
                    Value::String(s) => match s.char_code_at(idx as u32) {
                        Some(unit) => Value::String(crate::JsString::from_utf16_units(
                            &[unit],
                            &self.string_heap,
                        )?),
                        None => Value::String(crate::JsString::empty(&self.string_heap)?),
                    },
                    Value::TypedArray(t) => t.get(idx),
                    _ => return Err(VmError::TypeMismatch),
                }
            }
        };
        write_register(frame, dst, value)?;
        frame.pc += 1;
        Ok(())
    }

    fn load_from_constructor_prototype(
        &mut self,
        context: &ExecutionContext,
        proto_name: &str,
        receiver: &Value,
        name: &str,
    ) -> Result<Value, VmError> {
        let proto = self.constructor_prototype_value(proto_name)?;
        let Value::Object(proto_obj) = proto else {
            return Ok(Value::Undefined);
        };
        let key = VmPropertyKey::String(name.to_string());
        match self.ordinary_get_value(
            context,
            Value::Object(proto_obj),
            receiver.clone(),
            &key,
            0,
        )? {
            VmGetOutcome::Value(value) => Ok(value),
            VmGetOutcome::InvokeGetter { getter } => self.run_callable_sync(
                context,
                &getter,
                receiver.clone(),
                smallvec::SmallVec::new(),
            ),
        }
    }
}

fn has_object_property(interpreter: &Interpreter, obj: JsObject, key: &Value) -> bool {
    match key {
        Value::Symbol(s) => crate::object::get_symbol(obj, &interpreter.gc_heap, s).is_some(),
        Value::String(s) => {
            let key = s.to_lossy_string();
            !matches!(
                crate::object::lookup(obj, &interpreter.gc_heap, &key),
                object::PropertyLookup::Absent
            )
        }
        Value::Number(n) => {
            let key = n.to_display_string();
            !matches!(
                crate::object::lookup(obj, &interpreter.gc_heap, &key),
                object::PropertyLookup::Absent
            )
        }
        other => {
            let key = other.display_string();
            !matches!(
                crate::object::lookup(obj, &interpreter.gc_heap, &key),
                object::PropertyLookup::Absent
            )
        }
    }
}

fn has_array_property(interpreter: &Interpreter, arr: JsArray, key: &Value) -> bool {
    match key {
        Value::Number(n) => match n.as_smi() {
            Some(i) if i >= 0 => {
                crate::array::has_own_element(arr, &interpreter.gc_heap, i as usize)
            }
            _ => false,
        },
        Value::String(s) => {
            let key = s.to_lossy_string();
            if key == "length" {
                true
            } else if let Ok(i) = key.parse::<usize>() {
                crate::array::has_own_element(arr, &interpreter.gc_heap, i)
            } else {
                false
            }
        }
        _ => false,
    }
}

fn has_class_static_property(
    interpreter: &Interpreter,
    class: &ClassConstructor,
    key: &Value,
) -> bool {
    match key {
        Value::String(s) if s.to_lossy_string() == "prototype" => true,
        Value::String(s) => !matches!(
            crate::object::lookup(
                class.statics(&interpreter.gc_heap),
                &interpreter.gc_heap,
                &s.to_lossy_string()
            ),
            object::PropertyLookup::Absent
        ),
        _ => false,
    }
}
