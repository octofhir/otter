//! Property-related opcode helpers.
//!
//! The VM dispatch loop handles proxy or call-frame cases before entering the
//! dense register path. This module owns the remaining synchronous property
//! operations that can run directly against a frame.
//!
//! # Contents
//! - Legacy `instanceof` prototype-chain fallback.
//! - Synchronous `in` / `HasProperty` checks for arrays and class static sides.
//! - Synchronous property and element load/store tails.
//!
//! # Invariants
//! - Stack-modifying proxy and `@@hasInstance` cases are handled before these
//!   helpers are called.
//! - Inputs are already decoded from the executable instruction format.
//!
//! # See also
//! - [`crate::executable`]
//! - [`crate::object`]

use smallvec::SmallVec;

use otter_bytecode::Operand;
use otter_gc::raw::RawGc;

use crate::{
    ClassConstructor, ExecutionContext, Frame, Interpreter, JsObject, JsString, NumberValue, Value,
    VmError, VmGetOutcome, VmPropertyKey, abstract_ops,
    array::JsArray,
    binary, collections_prototype, descriptor_value, function_metadata,
    is_restricted_function_property, make_array_iterator_factory, object,
    operand_decode::{const_operand, register_operand},
    property_atom::AtomizedPropertyKey,
    property_ic::{HasPropertyIc, LoadPropertyIc, PropertyIcKind, StorePropertyIc},
    read_register, regexp_prototype, symbol, symbol_prototype, temporal, value_kind_name,
    write_register,
};

impl Interpreter {
    fn store_array_accessor_property(
        &mut self,
        context: &ExecutionContext,
        arr: JsArray,
        key: &str,
        value: &Value,
        strict: bool,
    ) -> Result<bool, VmError> {
        let Some((_getter, setter)) = crate::array::get_accessor(arr, &self.gc_heap, key) else {
            return Ok(false);
        };
        match setter {
            Some(setter) if abstract_ops::is_callable(&setter) => {
                let mut args: SmallVec<[Value; 8]> = SmallVec::new();
                args.push(value.clone());
                self.run_callable_sync(context, &setter, Value::Array(arr), args)?;
            }
            _ => {
                Self::failed_set_result(
                    strict,
                    format!("Cannot assign to accessor property '{key}' without a setter"),
                )?;
            }
        }
        Ok(true)
    }

    fn capture_store_property_transition_with_stack_roots(
        &mut self,
        stack: &SmallVec<[Frame; 8]>,
        mut obj: JsObject,
        key: AtomizedPropertyKey<'_>,
        value: &Value,
    ) -> Result<Option<object::StorePropertyTransition>, VmError> {
        let parent = object::shape(obj, &self.gc_heap);
        if parent.is_null() || self.shape_offset_of(parent, key.name()).is_some() {
            return Ok(None);
        }
        let roots = self.collect_allocation_roots(stack);
        let mut external_visit = |visitor: &mut dyn FnMut(*mut RawGc)| {
            for &slot in &roots {
                visitor(slot);
            }
            let p = &mut obj as *mut JsObject as *mut RawGc;
            visitor(p);
            value.trace_value_slots(visitor);
        };
        let next_shape = self
            .shape_runtime
            .child_with_roots(&mut self.gc_heap, parent, key.name(), &mut external_visit)
            .map_err(VmError::from)?;
        Ok(object::capture_store_property_transition_with_shape(
            obj,
            &mut self.gc_heap,
            key,
            value,
            next_shape,
        ))
    }

    /// §7.1.19 `ToPropertyKey(value)` — projection used by the
    /// computed-key `LoadElement` / `StoreElement` opcode dispatch
    /// before the per-receiver match runs. Primitive operands round
    /// through their existing arms unchanged; objects, functions,
    /// closures, arrays, and other non-primitives surface as a
    /// `Value::String` (the `ToString` result) or `Value::Symbol`
    /// (when `[Symbol.toPrimitive]` returns a Symbol). Bypassing
    /// this step caused `obj[() => {}]` / `class { [() => {}](){} }`
    /// to raise `TypeMismatch` even though the spec mandates a
    /// successful ToString coercion of the key.
    ///
    /// # See also
    /// - <https://tc39.es/ecma262/#sec-topropertykey>
    /// - <https://tc39.es/ecma262/#sec-toprimitive>
    fn coerce_property_key_value(
        &mut self,
        context: &ExecutionContext,
        value: Value,
    ) -> Result<Value, VmError> {
        // §7.1.19 ToPropertyKey — `String` / `Number` / `Symbol`
        // operands pass through to their existing per-receiver
        // arms unchanged; `Boolean` / `Null` / `Undefined` /
        // `BigInt` flatten to their display-string form so the
        // downstream match treats them as string keys.
        match &value {
            Value::String(_) | Value::Symbol(_) | Value::Number(_) => return Ok(value),
            Value::Boolean(b) => {
                let s = if *b { "true" } else { "false" };
                let js = JsString::from_str(s, &self.string_heap)?;
                return Ok(Value::String(js));
            }
            Value::Null => {
                let js = JsString::from_str("null", &self.string_heap)?;
                return Ok(Value::String(js));
            }
            Value::Undefined | Value::Hole => {
                let js = JsString::from_str("undefined", &self.string_heap)?;
                return Ok(Value::String(js));
            }
            Value::BigInt(b) => {
                let js = JsString::from_str(&b.to_decimal_string(), &self.string_heap)?;
                return Ok(Value::String(js));
            }
            _ => {}
        }
        let key = self.to_property_key_sync(context, value)?;
        match key {
            VmPropertyKey::Symbol(sym) => Ok(Value::Symbol(sym)),
            VmPropertyKey::Atom(atom) => {
                let s = JsString::from_str(atom.name(), &self.string_heap)?;
                Ok(Value::String(s))
            }
            VmPropertyKey::String(s) => {
                let s = JsString::from_str(s, &self.string_heap)?;
                Ok(Value::String(s))
            }
            VmPropertyKey::OwnedString(s) => {
                let s = JsString::from_str(&s, &self.string_heap)?;
                Ok(Value::String(s))
            }
        }
    }

    fn load_string_primitive_property(
        &mut self,
        context: &ExecutionContext,
        receiver: &Value,
        string: &JsString,
        name: &str,
    ) -> Result<Value, VmError> {
        match string_index_property_name(name) {
            Some(index) => match string.char_code_at(index) {
                Some(unit) => Ok(Value::String(JsString::from_utf16_units(
                    &[unit],
                    &self.string_heap,
                )?)),
                None => Ok(Value::Undefined),
            },
            None if name == "length" => {
                Ok(Value::Number(NumberValue::from_i32(string.len() as i32)))
            }
            None => self.load_from_constructor_prototype(context, "String", receiver, name),
        }
    }

    fn function_user_bag_with_stack_roots(
        &mut self,
        stack: &SmallVec<[Frame; 8]>,
        function_id: u32,
        value_roots: &[&Value],
    ) -> Result<JsObject, VmError> {
        match self.function_user_props.get(&function_id).copied() {
            Some(bag) => Ok(bag),
            None => {
                let bag = self.alloc_stack_rooted_object_with_extra_roots(stack, value_roots)?;
                self.function_user_props.insert(function_id, bag);
                Ok(bag)
            }
        }
    }

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
        context: &crate::execution_context::ExecutionContext,
        dst: u16,
        lhs: u16,
        rhs: u16,
    ) -> Result<(), VmError> {
        let lhs = read_register(frame, lhs)?.clone();
        let rhs = read_register(frame, rhs)?.clone();
        let key_name = match &lhs {
            Value::String(s) => Some(s.to_lossy_string()),
            Value::Number(n) => Some(n.to_display_string()),
            Value::Boolean(b) => Some(if *b { "true" } else { "false" }.to_string()),
            Value::Null => Some("null".to_string()),
            Value::Undefined => Some("undefined".to_string()),
            Value::BigInt(b) => Some(b.to_decimal_string()),
            Value::Symbol(_) => None,
            _ => None,
        };
        let present = match &rhs {
            Value::Object(obj) => has_object_property(self, *obj, &lhs),
            Value::Array(arr) => has_array_property(self, *arr, &lhs),
            Value::ClassConstructor(c) => has_class_static_property(self, c, &lhs),
            // §10.2.1 Function exotic — own descriptor table covers
            // `length` / `name` / `prototype` (the latter is absent on
            // methods / arrows); Function.prototype hosts the
            // canonical `call` / `apply` / `bind` / `toString` shims.
            // Lookups for symbol keys fall through to `false` because
            // the foundation Function layout has no symbol slot.
            Value::Function { function_id } | Value::Closure { function_id, .. } => {
                if let Some(name) = key_name.as_deref() {
                    let bag_has = self
                        .function_user_props
                        .get(function_id)
                        .copied()
                        .is_some_and(|bag| {
                            crate::object::with_properties(bag, &self.gc_heap, |p| {
                                p.keys().any(|k| k == name)
                            })
                        });
                    let metadata_has = self
                        .ordinary_function_own_property_descriptor(None, *function_id, name)
                        .ok()
                        .flatten()
                        .is_some();
                    // §10.2.4 ordinary functions expose `prototype` as
                    // an own data property unless arrow / method /
                    // explicit deletion.
                    let prototype_implicit = name == "prototype"
                        && !context.function_is_arrow(*function_id)
                        && !self
                            .function_deleted_metadata
                            .contains(&(*function_id, "prototype"));
                    bag_has
                        || metadata_has
                        || prototype_implicit
                        || matches!(name, "call" | "apply" | "bind" | "toString")
                } else {
                    false
                }
            }
            Value::NativeFunction(native) => {
                if let Some(name) = key_name.as_deref() {
                    native
                        .own_property_descriptor(&self.gc_heap, &self.string_heap, name)
                        .ok()
                        .flatten()
                        .is_some()
                        || matches!(name, "call" | "apply" | "bind" | "toString")
                } else if let Value::Symbol(sym) = &lhs {
                    native
                        .own_symbol_property_descriptor(&self.gc_heap, sym)
                        .is_some()
                } else {
                    false
                }
            }
            Value::BoundFunction(bound) => {
                if let Some(name) = key_name.as_deref() {
                    function_metadata::bound_own_property_descriptor(
                        bound,
                        &self.gc_heap,
                        &self.string_heap,
                        name,
                    )
                    .ok()
                    .flatten()
                    .is_some()
                        || matches!(name, "call" | "apply" | "bind" | "toString")
                } else {
                    false
                }
            }
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
        key: AtomizedPropertyKey<'_>,
        strict: bool,
    ) -> Result<(), VmError> {
        let name = key.name();
        let receiver = read_register(frame, obj_reg)?.clone();
        let removed = match &receiver {
            Value::Object(o) => crate::object::delete(*o, &mut self.gc_heap, name),
            // §10.4.2 [[Delete]] for Array exotic objects: integer
            // index keys delete the dense/sparse slot (and the spec
            // protects `length` from deletion); named keys route
            // through the array exotic's named-property store.
            Value::Array(arr) => crate::array::delete_named_property(*arr, &mut self.gc_heap, name),
            Value::Function { function_id } | Value::Closure { function_id, .. } => {
                self.ordinary_function_delete_own_property(*function_id, name)
            }
            Value::NativeFunction(native) => native.delete_own_property(&mut self.gc_heap, name),
            Value::BoundFunction(bound) => {
                function_metadata::bound_delete_own_property(bound, &mut self.gc_heap, name)
            }
            // §10.4.5.5 [[Delete]] — canonical-numeric-index keys
            // reject in-range elements; other keys probe the lazy
            // expando bag.
            Value::TypedArray(t) => {
                if let Some(n) = canonical_numeric_index_string(name) {
                    if t.buffer().is_detached() {
                        true
                    } else if n.is_finite()
                        && n.fract() == 0.0
                        && n >= 0.0
                        && (n as usize) < t.length()
                    {
                        false
                    } else {
                        true
                    }
                } else if let Some(bag) = t.expando() {
                    crate::object::delete(bag, &mut self.gc_heap, name)
                } else {
                    true
                }
            }
            Value::Promise(promise) => {
                if let Some(bag) = promise.expando(&self.gc_heap) {
                    crate::object::delete(bag, &mut self.gc_heap, name)
                } else {
                    true
                }
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
        // §13.5.1.2 step 5.c — when the result of `[[Delete]]` is
        // `false` in strict mode, throw a TypeError.
        if !removed && strict {
            return Err(VmError::TypeError {
                message: format!("Cannot delete property '{name}'"),
            });
        }
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
        strict: bool,
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
            // §10.4.2 Array exotic [[Delete]]: number / string index
            // keys both flow through `delete_named_property`, which
            // handles dense / sparse / named storage and protects
            // `length`.
            (Value::Array(arr), Value::Number(n)) => match n.as_smi() {
                Some(v) if v >= 0 => {
                    crate::array::delete_named_property(*arr, &mut self.gc_heap, &v.to_string())
                }
                _ => crate::array::delete_named_property(
                    *arr,
                    &mut self.gc_heap,
                    &n.to_display_string(),
                ),
            },
            (Value::Array(arr), Value::String(s)) => {
                crate::array::delete_named_property(*arr, &mut self.gc_heap, &s.to_lossy_string())
            }
            (Value::Array(arr), Value::Symbol(sym)) => {
                crate::array::delete_symbol_property(*arr, &mut self.gc_heap, &sym)
            }
            // §10.4.3 String exotic objects expose a non-configurable
            // own property at every valid character index; integer
            // indices in `[0, length)` therefore reject deletion,
            // while out-of-range integer indices and non-integer
            // keys succeed (the wrapper has no such own property and
            // ordinary [[Delete]] succeeds vacuously).
            // <https://tc39.es/ecma262/#sec-string-exotic-objects-delete-p>
            (Value::String(s), Value::Number(n)) => match n.as_smi() {
                Some(v) if v >= 0 && (v as u32) < s.len() => false,
                _ => true,
            },
            (Value::String(_), _) => true,
            (
                Value::Function { function_id } | Value::Closure { function_id, .. },
                Value::String(s),
            ) => self.ordinary_function_delete_own_property(*function_id, &s.to_lossy_string()),
            (Value::NativeFunction(native), Value::Symbol(sym)) => {
                native.delete_own_symbol_property(&mut self.gc_heap, &sym)
            }
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
            // §10.4.5.5 IntegerIndexedExoticObject [[Delete]]:
            // canonical-numeric-index strings reject deletion only
            // when the index resolves to a valid in-range element
            // (and the buffer is attached). Out-of-range indices,
            // non-integer canonical numerics, and arbitrary
            // non-canonical keys all succeed vacuously — the
            // TypedArray exotic has no expando own properties.
            // <https://tc39.es/ecma262/#sec-integer-indexed-exotic-objects-delete-p>
            (Value::TypedArray(t), Value::String(s)) => {
                let name = s.to_lossy_string();
                match canonical_numeric_index_string(&name) {
                    Some(n) => {
                        if t.buffer().is_detached() {
                            true
                        } else if n.is_finite()
                            && n.fract() == 0.0
                            && n >= 0.0
                            && (n as usize) < t.length()
                        {
                            false
                        } else {
                            true
                        }
                    }
                    None => {
                        if let Some(bag) = t.expando() {
                            crate::object::delete(bag, &mut self.gc_heap, &name)
                        } else {
                            true
                        }
                    }
                }
            }
            (Value::TypedArray(t), Value::Number(n)) => {
                if t.buffer().is_detached() {
                    true
                } else {
                    match n.as_smi() {
                        Some(v) if v >= 0 && (v as usize) < t.length() => false,
                        _ => true,
                    }
                }
            }
            (Value::TypedArray(t), Value::Symbol(sym)) => {
                if let Some(bag) = t.expando() {
                    crate::object::delete_symbol(bag, &mut self.gc_heap, &sym)
                } else {
                    true
                }
            }
            _ => return Err(VmError::TypeMismatch),
        };
        if !removed && strict {
            return Err(VmError::TypeError {
                message: "Cannot delete property".to_string(),
            });
        }
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
            // §15.7.14 ClassDefinitionEvaluation step 6.b — `class D
            // extends C` sets D.[[Prototype]] (the static side) to
            // the parent constructor C verbatim, so static methods on
            // a native parent (`Promise.reject`, `Map[@@species]`, …)
            // resolve through the ordinary [[Get]] ladder. Carry the
            // native callable through as an `ObjectPrototype::Value`
            // — the prototype walker in `ordinary_get_value` knows
            // how to walk into a NativeFunction receiver.
            Value::NativeFunction(_) => read_register(frame, proto_reg)?.clone(),
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
            // §20.1.2.21 step 4 — `Object.setPrototypeOf(primitive,
            // proto)` returns the primitive unchanged after the
            // RequireObjectCoercible / proto-typecheck steps (which
            // already succeeded for `Boolean / Number / String /
            // Symbol / BigInt` because they are coercible). Mirror
            // V8 / JSC and skip the prototype write — the wrapper
            // would be unreachable.
            Value::Boolean(_)
            | Value::Number(_)
            | Value::String(_)
            | Value::Symbol(_)
            | Value::BigInt(_) => {}
            _ => return Err(VmError::TypeMismatch),
        }
        frame.pc += 1;
        Ok(())
    }

    pub(crate) fn run_load_property_reg(
        &mut self,
        context: &ExecutionContext,
        stack: &mut SmallVec<[Frame; 8]>,
        top_idx: usize,
        dst: u16,
        obj_reg: u16,
        key: AtomizedPropertyKey<'_>,
    ) -> Result<(), VmError> {
        let name = key.name();
        let receiver = read_register(&stack[top_idx], obj_reg)?.clone();
        let value = match &receiver {
            Value::Object(o) => {
                crate::object::get(*o, &self.gc_heap, name).unwrap_or(Value::Undefined)
            }
            Value::ClassConstructor(c) => {
                if name == "prototype" {
                    Value::Object(c.prototype(&self.gc_heap))
                } else {
                    let statics = c.statics(&self.gc_heap);
                    let direct = crate::object::get(statics, &self.gc_heap, name);
                    if let Some(v) = direct {
                        v
                    } else {
                        // §15.7.10 step 6.b — `class D extends C` sets
                        // D.[[Prototype]] = C. When the parent is a
                        // non-Object callable (NativeFunction such as
                        // `Promise`, ClassConstructor for a user
                        // class), the proto chain walked by
                        // `object::get` stops at the first non-Object
                        // hop. Fall back to `ordinary_get_value` on
                        // the statics's stored prototype so static
                        // inheritance (`Foo.reject`,
                        // `MySet[Symbol.species]`, ...) resolves.
                        let parent = crate::object::prototype_value(statics, &self.gc_heap);
                        let walked = match parent {
                            Some(p)
                                if !matches!(
                                    p,
                                    Value::Object(_) | Value::Null | Value::Undefined
                                ) =>
                            {
                                match self.ordinary_get_value(
                                    context,
                                    p,
                                    receiver.clone(),
                                    &VmPropertyKey::String(name),
                                    0,
                                )? {
                                    VmGetOutcome::Value(v) => Some(v),
                                    VmGetOutcome::InvokeGetter { getter } => {
                                        Some(self.run_callable_sync(
                                            context,
                                            &getter,
                                            receiver.clone(),
                                            SmallVec::new(),
                                        )?)
                                    }
                                }
                            }
                            _ => None,
                        };
                        match walked {
                            Some(v) if !matches!(v, Value::Undefined) => v,
                            _ if name == "name" || name == "length" => {
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
                            _ => Value::Undefined,
                        }
                    }
                }
            }
            Value::String(s) => self.load_string_primitive_property(context, &receiver, s, name)?,
            v @ Value::Array(_) => {
                let direct = if let Value::Array(a) = v {
                    if let Some((getter, _setter)) =
                        crate::array::get_accessor(*a, &self.gc_heap, name)
                    {
                        match getter {
                            Some(getter) if abstract_ops::is_callable(&getter) => {
                                let args: SmallVec<[Value; 8]> = SmallVec::new();
                                Some(self.run_callable_sync(context, &getter, v.clone(), args)?)
                            }
                            _ => Some(Value::Undefined),
                        }
                    } else {
                        crate::array::get_named_property(*a, &self.gc_heap, name)
                    }
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
                self.function_property_get_stack_rooted(context, stack, fid, name)?
            }
            Value::Closure { function_id, .. } => {
                let fid = *function_id;
                self.function_property_get_stack_rooted(context, stack, fid, name)?
            }
            Value::NativeFunction(native) => {
                match native.own_property_descriptor(&self.gc_heap, &self.string_heap, name)? {
                    Some(desc) => match &desc.kind {
                        object::DescriptorKind::Data { value } => value.clone(),
                        // §10.1.8.1 OrdinaryGet step 7 — accessor
                        // descriptors invoke `[[Get]]` with the
                        // receiver. RegExp's `%RegExp%.input` /
                        // `lastMatch` / etc. legacy accessors land
                        // here.
                        object::DescriptorKind::Accessor { getter, .. } => match getter {
                            Some(g) => {
                                let args: SmallVec<[Value; 8]> = SmallVec::new();
                                self.run_callable_sync(context, g, receiver.clone(), args)?
                            }
                            None => Value::Undefined,
                        },
                    },
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
                Some(desc) => match &desc.kind {
                    object::DescriptorKind::Data { value } => value.clone(),
                    object::DescriptorKind::Accessor { getter, .. } => match getter {
                        Some(g) if abstract_ops::is_callable(g) => {
                            self.run_callable_sync(context, g, receiver.clone(), SmallVec::new())?
                        }
                        _ => Value::Undefined,
                    },
                },
                None => self
                    .load_function_prototype_method(name)
                    .or_else(|| self.load_object_prototype_method(name))
                    .unwrap_or(Value::Undefined),
            },
            v @ Value::RegExp(_) => {
                let r = if let Value::RegExp(r) = v {
                    *r
                } else {
                    unreachable!()
                };
                // Expando bag wins over the spec-mandated direct
                // load so user-installed members
                // (`re.exec = fn`, `re.global = false`) shadow the
                // built-in accessors during test262 observability
                // checks.
                if let Some(bag) = r.expando(&self.gc_heap)
                    && let Some(value) = crate::object::get(bag, &self.gc_heap, name)
                {
                    value
                } else {
                    let direct =
                        regexp_prototype::load_property(&r, &self.gc_heap, name, &self.string_heap);
                    match direct {
                        Value::Undefined => {
                            self.load_from_constructor_prototype(context, "RegExp", v, name)?
                        }
                        value => value,
                    }
                }
            }
            Value::Symbol(s) => symbol_prototype::load_property(s, name),
            // §27.1.5 — read string-keyed properties through
            // `Iterator.prototype` so the new spec-mandated
            // `next` / `return` / `throw` natives (and the helper
            // terminals like `map` / `forEach` / `toArray`) all
            // resolve uniformly via the realm prototype.
            // `this` rebinding through `Function.prototype.call`
            // surfaces correctly because the native methods take
            // their receiver from `ctx.this_value()` rather than
            // synthesized captures.
            v @ Value::Iterator(_) => {
                self.load_from_constructor_prototype(context, "Iterator", v, name)?
            }
            v @ (Value::WeakRef(_) | Value::FinalizationRegistry(_)) => {
                let proto_name = match v {
                    Value::WeakRef(_) => "WeakRef",
                    Value::FinalizationRegistry(_) => "FinalizationRegistry",
                    _ => unreachable!(),
                };
                self.load_from_constructor_prototype(context, proto_name, v, name)?
            }
            v @ Value::Promise(_) => {
                // §27.2.5 — user-installed own properties
                // (`promise.then = fn`) live in a lazy expando bag;
                // honour them before the prototype walk.
                let p = if let Value::Promise(p) = v {
                    *p
                } else {
                    unreachable!()
                };
                if let Some(bag) = p.expando(&self.gc_heap)
                    && let Some(value) = crate::object::get(bag, &self.gc_heap, name)
                {
                    value
                } else {
                    // §27.2.4.7.1 OrdinaryCreateFromConstructor —
                    // when `new SubPromise(executor)` set
                    // `prototype_override` to `SubPromise.prototype`,
                    // walk *that* chain, not the realm
                    // `%Promise.prototype%`. Without this the
                    // sub-class instance's `.constructor` resolves
                    // to the parent Promise and SpeciesConstructor
                    // misroutes downstream chained promises.
                    let proto = match p.prototype_override(&self.gc_heap) {
                        Some(proto) => proto,
                        None => self.constructor_prototype_value("Promise")?,
                    };
                    if matches!(proto, Value::Null | Value::Undefined) {
                        Value::Undefined
                    } else {
                        let key = VmPropertyKey::String(name);
                        match self.ordinary_get_value(context, proto, v.clone(), &key, 0)? {
                            VmGetOutcome::Value(value) => value,
                            VmGetOutcome::InvokeGetter { getter } => self.run_callable_sync(
                                context,
                                &getter,
                                v.clone(),
                                smallvec::SmallVec::new(),
                            )?,
                        }
                    }
                }
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
                let t = if let Value::TypedArray(t) = v {
                    t
                } else {
                    unreachable!()
                };
                // §10.4.5.4 [[Get]] — check the expando bag before
                // the per-kind built-ins so user-installed
                // properties (`typedArr.foo = 1`,
                // `typedArr.constructor = X`) win over inherited
                // defaults.
                if let Some(bag) = t.expando()
                    && let Some(value) = crate::object::get(bag, &self.gc_heap, name)
                {
                    value
                } else {
                    let direct = binary::typed_array_prototype::load_property(t, name);
                    match direct {
                        Value::Undefined => {
                            let kind_name = t.kind().name();
                            self.load_from_constructor_prototype(context, kind_name, v, name)?
                        }
                        value => value,
                    }
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
        let frame = &mut stack[top_idx];
        write_register(frame, dst, value)?;
        frame.pc += 1;
        Ok(())
    }

    pub(crate) fn run_store_property_reg(
        &mut self,
        context: &ExecutionContext,
        stack: &mut SmallVec<[Frame; 8]>,
        top_idx: usize,
        obj_reg: u16,
        key: AtomizedPropertyKey<'_>,
        src: u16,
    ) -> Result<(), VmError> {
        let name = key.name();
        let frame = &stack[top_idx];
        let value = read_register(frame, src)?.clone();
        let strict = context.function_is_strict(frame.function_id);
        let receiver = read_register(frame, obj_reg)?.clone();
        let target = match &receiver {
            Value::Object(o) => Some(*o),
            Value::ClassConstructor(c) => Some(c.statics(&self.gc_heap)),
            Value::RegExp(r) => {
                // `lastIndex` lives in the body slot; every other
                // named write lands in the lazy expando bag so
                // `re.global = false` / `re.exec = fn` survive
                // observability checks.
                if name == "lastIndex" {
                    regexp_prototype::store_property(r, &mut self.gc_heap, name, value.clone());
                    None
                } else {
                    let absent = r.expando(&self.gc_heap).is_none_or(|bag| {
                        matches!(
                            object::lookup_own(bag, &self.gc_heap, name),
                            object::PropertyLookup::Absent
                        )
                    });
                    if absent && !r.is_extensible(&self.gc_heap) {
                        Self::failed_set_result(
                            strict,
                            format!("Cannot add property '{name}' to non-extensible RegExp"),
                        )?;
                        None
                    } else {
                        let bag = regexp_ensure_expando(self, r, &receiver)?;
                        if !self.ordinary_set_data_property(bag, name, value.clone())? {
                            Self::failed_set_result(
                                strict,
                                format!("Cannot assign to property '{name}'"),
                            )?;
                        }
                        None
                    }
                }
            }
            Value::Array(a) => {
                if !self.store_array_accessor_property(context, *a, name, &value, strict)? {
                    crate::array::set_named_property(*a, &mut self.gc_heap, name, value.clone())?;
                }
                None
            }
            Value::TypedArray(t) => {
                if let Some(n) = canonical_numeric_index_string(name) {
                    if !t.buffer().is_detached()
                        && n.is_finite()
                        && n.fract() == 0.0
                        && n >= 0.0
                        && (n as usize) < t.length()
                    {
                        let coerced = binary::dispatch::coerce_element_for_store(t.kind(), &value)?;
                        t.set(n as usize, &coerced);
                    }
                } else {
                    let t_clone = t.clone();
                    typed_array_set_expando(self, &t_clone, name, value.clone())?;
                }
                None
            }
            Value::Function { function_id } | Value::Closure { function_id, .. } => {
                let fid = *function_id;
                let has_own = self.ordinary_function_has_own_string_property_for_extensibility(
                    context, fid, name,
                )?;
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
                        let bag = self.function_user_bag_with_stack_roots(
                            stack,
                            fid,
                            &[&receiver, &value],
                        )?;
                        if let Some(metadata_key) =
                            function_metadata::ordinary_function_metadata_key(name)
                        {
                            self.function_deleted_metadata.remove(&(fid, metadata_key));
                        }
                        Some(bag)
                    }
                } else if !has_own && !self.ordinary_function_is_extensible(fid) {
                    Self::failed_set_result(
                        strict,
                        format!("Cannot add property '{name}' to non-extensible function"),
                    )?;
                    None
                } else {
                    let bag =
                        self.function_user_bag_with_stack_roots(stack, fid, &[&receiver, &value])?;
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
            Value::Promise(p) => {
                let bag = if let Some(bag) = p.expando(&self.gc_heap) {
                    bag
                } else {
                    let p_value = receiver.clone();
                    let mut external_visit = |visitor: &mut dyn FnMut(*mut RawGc)| {
                        p_value.trace_value_slots(visitor);
                    };
                    let bag = crate::object::alloc_object_with_roots(
                        &mut self.gc_heap,
                        &mut external_visit,
                    )?;
                    p.set_expando(&mut self.gc_heap, bag);
                    bag
                };
                Some(bag)
            }
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
                // §10.1.9.2 OrdinarySetWithOwnDescriptor — for
                // exotic receivers without their own [[Set]] (Map,
                // Set, WeakMap, WeakSet, WeakRef,
                // FinalizationRegistry, ArrayBuffer,
                // SharedArrayBuffer, DataView, Iterator, Generator,
                // Proxy already handled higher up), the spec
                // delegates to the prototype's [[Set]]. The
                // prototype is the realm's `<Kind>.prototype`
                // (ordinary object) and the write would create an
                // own data property on the receiver — but the
                // receiver carries no expando slot here. Mirror
                // [[Set]]'s observable contract: silently ignore
                // in non-strict mode and surface TypeError in
                // strict mode.
                Self::failed_set_result(
                    strict,
                    format!("Cannot set property '{name}' on {}", value_kind_name(other)),
                )?;
                None
            }
        };
        if let Some(target) = target {
            self.set_property(target, name, value)?;
        }
        stack[top_idx].pc += 1;
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
        let idx_value_raw = read_register(frame, idx_reg)?.clone();
        let idx_value = self.coerce_property_key_value(context, idx_value_raw)?;
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
                // §22.1.5.1 — own Symbol.iterator override on the
                // array exotic body wins over the prototype slot so
                // user-installed `arr[Symbol.iterator] = fn` is
                // observable.
                if let Some(v) = crate::array::get_symbol_property(*arr, &self.gc_heap, sym) {
                    v
                } else {
                    make_array_iterator_factory(*arr, &mut self.gc_heap)?
                }
            }
            // §22.1 Array exotic — symbol-keyed access reads the
            // array's own symbol table first (e.g.
            // `arr[Symbol.toStringTag]`); on miss, walks
            // `Array.prototype` so inherited symbol-keyed members
            // (e.g. `@@toStringTag` accessor) still resolve.
            (Value::Array(arr), Value::Symbol(sym)) => {
                match crate::array::get_symbol_property(*arr, &self.gc_heap, sym) {
                    Some(v) => v,
                    None => {
                        let proto = self.constructor_prototype_value("Array")?;
                        if let Value::Object(p) = proto {
                            crate::object::get_symbol(p, &self.gc_heap, sym)
                                .unwrap_or(Value::Undefined)
                        } else {
                            Value::Undefined
                        }
                    }
                }
            }
            // Computed string-key access on Array exotic objects:
            // `arr["0"]`, `arr["length"]`, `arr[i]` after `for (i in
            // arr)` (where `i` is a string property key) must observe
            // the spec's `Array` [[Get]] internal method — integer-
            // index strings route to dense / sparse element storage,
            // `length` returns the length, and anything else falls
            // through to the named-property table plus the Array
            // prototype chain (so `arr["push"]` resolves to
            // `Array.prototype.push`). Without this arm the default
            // branch demanded a numeric key and surfaced a bogus
            // `TypeMismatch` for `for-in` body access.
            // <https://tc39.es/ecma262/#sec-array-exotic-objects-get-p-receiver>
            (Value::Array(arr), Value::String(key)) => {
                let name = key.to_lossy_string();
                if name == "length" {
                    Value::Number(NumberValue::from_f64(
                        crate::array::len(*arr, &self.gc_heap) as f64,
                    ))
                } else if let Some((getter, _setter)) =
                    crate::array::get_accessor(*arr, &self.gc_heap, &name)
                {
                    match getter {
                        Some(getter) if abstract_ops::is_callable(&getter) => {
                            let args: SmallVec<[Value; 8]> = SmallVec::new();
                            self.run_callable_sync(context, &getter, recv.clone(), args)?
                        }
                        _ => Value::Undefined,
                    }
                } else if let Some(idx) = crate::object::array_index_property_name(&name) {
                    crate::array::get(*arr, &self.gc_heap, idx as usize)
                } else {
                    match crate::array::get_named_property(*arr, &self.gc_heap, &name) {
                        Some(v) => v,
                        None => {
                            self.load_from_constructor_prototype(context, "Array", &recv, &name)?
                        }
                    }
                }
            }
            // §10.4.5.4 IntegerIndexedExoticObject [[Get]]:
            // canonical numeric index strings short-circuit to the
            // element / undefined path; non-numeric keys walk the
            // ordinary prototype chain.
            // <https://tc39.es/ecma262/#sec-integer-indexed-exotic-objects-get-p-receiver>
            (Value::TypedArray(t), Value::String(key)) => {
                let name = key.to_lossy_string();
                if let Some(n) = canonical_numeric_index_string(&name) {
                    if n.is_finite() && n.fract() == 0.0 && n >= 0.0 && (n as usize) < t.length() {
                        t.get(n as usize)
                    } else {
                        Value::Undefined
                    }
                } else {
                    // §10.4.5.4 step 3 — non-canonical-numeric keys
                    // fall through to OrdinaryGet. Honour the lazy
                    // expando bag first, then the prototype chain.
                    let mut value = Value::Undefined;
                    let mut found = false;
                    if let Some(bag) = t.expando()
                        && let Some(v) = crate::object::get(bag, &self.gc_heap, &name)
                    {
                        value = v;
                        found = true;
                    }
                    if !found {
                        let direct = binary::typed_array_prototype::load_property(t, &name);
                        value = match direct {
                            Value::Undefined => {
                                let kind_name = t.kind().name();
                                self.load_from_constructor_prototype(
                                    context, kind_name, &recv, &name,
                                )?
                            }
                            v => v,
                        };
                    }
                    value
                }
            }
            // §10.4.3 String exotic [[GetOwnProperty]] exposes each
            // UTF-16 code unit as an own, read-only indexed property.
            // Computed access reaches this arm after ToPropertyKey,
            // so both `"abc"[0]` and `"abc"["0"]` must resolve here
            // before falling back to String.prototype.
            // <https://tc39.es/ecma262/#sec-string-exotic-objects-getownproperty-p>
            (Value::String(s), Value::String(key)) => {
                let name = key.to_lossy_string();
                self.load_string_primitive_property(context, &recv, s, &name)?
            }
            (Value::String(s), Value::Number(key)) => {
                let name = key.to_display_string();
                self.load_string_primitive_property(context, &recv, s, &name)?
            }
            // Computed string-key access on RegExp must observe the
            // same own/prototype lookup as `re.lastIndex` (member
            // access). Without this arm, `re["lastIndex"]` falls
            // through to the numeric-index default and surfaces a
            // bogus `TypeMismatch` (see ECMA-262 §22.2.5 — RegExp
            // exposes `source`, `flags`, `global`, `lastIndex`, etc.
            // as proper own/prototype properties).
            (Value::RegExp(r), Value::String(key)) => {
                let name = key.to_lossy_string();
                if let Some(bag) = r.expando(&self.gc_heap)
                    && let Some(value) = crate::object::get(bag, &self.gc_heap, &name)
                {
                    value
                } else {
                    let direct =
                        regexp_prototype::load_property(r, &self.gc_heap, &name, &self.string_heap);
                    match direct {
                        Value::Undefined => {
                            self.load_from_constructor_prototype(context, "RegExp", &recv, &name)?
                        }
                        value => value,
                    }
                }
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
            // §10.2 — callable + class shapes inherit @@-keyed
            // properties through `Function.prototype` (or the class
            // statics for `ClassConstructor`). Without a Symbol-key
            // arm here, `f[Symbol.hasInstance]` falls through to the
            // numeric-index default and trips `TypeMismatch`. Route
            // through `ordinary_get_value` so the per-shape lookup
            // (function user-props → Function.prototype → walked
            // accessor outcomes) fires correctly.
            (
                Value::Function { .. }
                | Value::Closure { .. }
                | Value::NativeFunction(_)
                | Value::BoundFunction(_)
                | Value::ClassConstructor(_)
                | Value::RegExp(_)
                | Value::Map(_)
                | Value::Set(_)
                | Value::WeakMap(_)
                | Value::WeakSet(_)
                | Value::WeakRef(_)
                | Value::FinalizationRegistry(_)
                | Value::Promise(_)
                | Value::ArrayBuffer(_)
                | Value::DataView(_)
                | Value::TypedArray(_),
                Value::Symbol(_),
            ) => {
                let key = match &idx_value {
                    Value::Symbol(sym) => VmPropertyKey::Symbol(sym.clone()),
                    _ => unreachable!(),
                };
                match self.ordinary_get_value(context, recv.clone(), recv.clone(), &key, 0)? {
                    crate::VmGetOutcome::Value(v) => v,
                    crate::VmGetOutcome::InvokeGetter { getter } => {
                        let args: smallvec::SmallVec<[Value; 8]> = smallvec::SmallVec::new();
                        self.run_callable_sync(context, &getter, recv.clone(), args)?
                    }
                }
            }
            // §7.1.18 ToObject — primitive receivers walk their
            // wrapper prototype for both string- and symbol-keyed
            // access. Mirrors V8 / JSC where `Symbol()[
            // Symbol.toPrimitive]` resolves to
            // `Symbol.prototype[Symbol.toPrimitive]` rather than
            // throwing. The wrapper itself is not materialized; the
            // primitive flows through as `this` to any subsequent
            // call.
            (
                Value::Symbol(_)
                | Value::Boolean(_)
                | Value::Number(_)
                | Value::BigInt(_)
                | Value::String(_),
                Value::Symbol(_) | Value::String(_) | Value::Number(_),
            ) => {
                let ctor_name = match &recv {
                    Value::Symbol(_) => "Symbol",
                    Value::Boolean(_) => "Boolean",
                    Value::Number(_) => "Number",
                    Value::BigInt(_) => "BigInt",
                    Value::String(_) => "String",
                    _ => unreachable!(),
                };
                let key = match &idx_value {
                    Value::Symbol(sym) => VmPropertyKey::Symbol(sym.clone()),
                    Value::String(s) => VmPropertyKey::OwnedString(s.to_lossy_string()),
                    Value::Number(n) => VmPropertyKey::OwnedString(n.to_display_string()),
                    _ => unreachable!(),
                };
                let proto = self.constructor_prototype_value(ctor_name)?;
                if matches!(proto, Value::Null | Value::Undefined) {
                    Value::Undefined
                } else {
                    match self.ordinary_get_value(context, proto, recv.clone(), &key, 0)? {
                        crate::VmGetOutcome::Value(v) => v,
                        crate::VmGetOutcome::InvokeGetter { getter } => {
                            let args: smallvec::SmallVec<[Value; 8]> = smallvec::SmallVec::new();
                            self.run_callable_sync(context, &getter, recv.clone(), args)?
                        }
                    }
                }
            }
            _ => {
                let (idx, fallback_key) = match &idx_value {
                    Value::Number(n) => match crate::array::index_from_number(*n) {
                        Some(idx) => (Some(idx), None),
                        None => (None, Some(n.to_display_string())),
                    },
                    _ => return Err(VmError::TypeMismatch),
                };
                match recv {
                    Value::Array(a) => match idx {
                        Some(idx) => {
                            let key = idx.to_string();
                            if let Some((getter, _setter)) =
                                crate::array::get_accessor(a, &self.gc_heap, &key)
                            {
                                match getter {
                                    Some(getter) if abstract_ops::is_callable(&getter) => {
                                        let args: smallvec::SmallVec<[Value; 8]> =
                                            smallvec::SmallVec::new();
                                        self.run_callable_sync(
                                            context,
                                            &getter,
                                            Value::Array(a),
                                            args,
                                        )?
                                    }
                                    _ => Value::Undefined,
                                }
                            } else {
                                crate::array::get(a, &self.gc_heap, idx)
                            }
                        }
                        None => crate::array::get_named_property(
                            a,
                            &self.gc_heap,
                            fallback_key.as_deref().expect("fallback key"),
                        )
                        .unwrap_or(Value::Undefined),
                    },
                    Value::String(s) => match idx {
                        Some(idx) => match s.char_code_at(idx as u32) {
                            Some(unit) => Value::String(crate::JsString::from_utf16_units(
                                &[unit],
                                &self.string_heap,
                            )?),
                            None => Value::String(crate::JsString::empty(&self.string_heap)?),
                        },
                        None => Value::Undefined,
                    },
                    Value::TypedArray(t) => match idx {
                        Some(idx) => t.get(idx),
                        None => Value::Undefined,
                    },
                    _ => return Err(VmError::TypeMismatch),
                }
            }
        };
        write_register(frame, dst, value)?;
        frame.pc += 1;
        Ok(())
    }

    pub(crate) fn run_store_element_regs(
        &mut self,
        context: &ExecutionContext,
        stack: &mut SmallVec<[Frame; 8]>,
        top_idx: usize,
        recv_reg: u16,
        idx_reg: u16,
        src_reg: u16,
    ) -> Result<(), VmError> {
        let frame = &stack[top_idx];
        let recv = read_register(frame, recv_reg)?.clone();
        let idx_value_raw = read_register(frame, idx_reg)?.clone();
        let value = read_register(frame, src_reg)?.clone();
        let strict = context.function_is_strict(frame.function_id);
        let idx_value = self.coerce_property_key_value(context, idx_value_raw)?;
        match (&recv, &idx_value) {
            // Symbol-keyed write on an object.
            (Value::Object(obj), Value::Symbol(sym)) => {
                if !crate::object::set_symbol(*obj, &mut self.gc_heap, sym.clone(), value) {
                    Self::failed_set_result(strict, "Cannot assign to symbol property")?;
                }
            }
            // Computed string-key write (`obj["k"] = ...`).
            (Value::Object(obj), Value::String(key)) => {
                let key = key.to_lossy_string();
                self.store_computed_ordinary_property(*obj, &key, value, strict)?;
            }
            // Computed numeric property write on ordinary objects,
            // e.g. `arguments[0] = v`.
            (Value::Object(obj), Value::Number(n)) => {
                let key = n.to_display_string();
                self.store_computed_ordinary_property(*obj, &key, value, strict)?;
            }
            (
                Value::Function { function_id } | Value::Closure { function_id, .. },
                Value::String(key),
            ) => {
                let key = key.to_lossy_string();
                let has_own = self.ordinary_function_has_own_string_property_for_extensibility(
                    context,
                    *function_id,
                    &key,
                )?;
                match self.ordinary_function_own_property_descriptor(
                    Some(context),
                    *function_id,
                    &key,
                )? {
                    Some(desc) if !desc.writable() => {
                        Self::failed_set_result(
                            strict,
                            format!("Cannot assign to read-only property '{key}' of function"),
                        )?;
                    }
                    _ => {
                        if !has_own && !self.ordinary_function_is_extensible(*function_id) {
                            Self::failed_set_result(
                                strict,
                                format!("Cannot add property '{key}' to non-extensible function"),
                            )?;
                        } else {
                            let bag = self.function_user_bag_stack_rooted(
                                stack,
                                *function_id,
                                &[&recv, &idx_value, &value],
                            )?;
                            self.set_property(bag, &key, value)?;
                            if let Some(metadata_key) =
                                function_metadata::ordinary_function_metadata_key(&key)
                            {
                                self.function_deleted_metadata
                                    .remove(&(*function_id, metadata_key));
                            }
                        }
                    }
                }
            }
            // Computed write to built-in function metadata follows
            // the same descriptor path as `f.name = ...`.
            (Value::NativeFunction(native), Value::String(key)) => {
                let key = key.to_lossy_string();
                match native.own_property_descriptor(&self.gc_heap, &self.string_heap, &key)? {
                    Some(desc) if !desc.writable() => {
                        Self::failed_set_result(
                            strict,
                            format!(
                                "Cannot assign to read-only property '{key}' of function {}",
                                native.name(&self.gc_heap)
                            ),
                        )?;
                    }
                    _ => {
                        let desc = crate::object::PropertyDescriptor::data(
                            value.clone(),
                            true,
                            false,
                            true,
                        );
                        if !native.define_own_property(
                            &mut self.gc_heap,
                            &self.string_heap,
                            &key,
                            desc,
                        ) {
                            Self::failed_set_result(
                                strict,
                                format!(
                                    "Cannot define property '{key}' on function {}",
                                    native.name(&self.gc_heap)
                                ),
                            )?;
                        }
                    }
                }
            }
            (Value::BoundFunction(bound), Value::String(key)) => {
                let key = key.to_lossy_string();
                match function_metadata::bound_own_property_descriptor(
                    bound,
                    &self.gc_heap,
                    &self.string_heap,
                    &key,
                )? {
                    Some(desc) if !desc.writable() => {
                        Self::failed_set_result(
                            strict,
                            format!(
                                "Cannot assign to read-only property '{key}' of bound function"
                            ),
                        )?;
                    }
                    _ => {
                        let desc = crate::object::PropertyDescriptor::data(
                            value.clone(),
                            true,
                            false,
                            true,
                        );
                        if !function_metadata::bound_define_own_property(
                            bound,
                            &mut self.gc_heap,
                            &self.string_heap,
                            &key,
                            desc,
                        ) {
                            Self::failed_set_result(
                                strict,
                                format!("Cannot define property '{key}' on bound function"),
                            )?;
                        }
                    }
                }
            }
            // §22.1 Array exotic — symbol-keyed writes land in the
            // per-array symbol-property table so reflective probes
            // (`arr[Symbol.toStringTag] = "X"`,
            // `Object.getOwnPropertySymbols(arr)`,
            // `arr[Symbol.iterator] = fn`) round-trip.
            (Value::Array(arr), Value::Symbol(sym)) => {
                crate::array::set_symbol_property(*arr, &mut self.gc_heap, sym, value);
            }
            // §22.1 Array exotic — string-keyed write of an
            // integer-string lands as a dense element; everything
            // else stores on the named-properties side table so
            // `arr["i"] = 10` round-trips.
            (Value::Array(arr), Value::String(key)) => {
                let name = key.to_lossy_string();
                if self.store_array_accessor_property(context, *arr, &name, &value, strict)? {
                    // Accessor setter handled the assignment.
                } else if let Some(idx) = crate::object::array_index_property_name(&name) {
                    let roots = self.collect_allocation_roots(stack);
                    let mut external_visit = |visitor: &mut dyn FnMut(*mut RawGc)| {
                        for &slot in &roots {
                            visitor(slot);
                        }
                    };
                    crate::array::set_with_roots(
                        *arr,
                        &mut self.gc_heap,
                        idx as usize,
                        value,
                        &mut external_visit,
                    )?;
                } else {
                    crate::array::set_named_property(*arr, &mut self.gc_heap, &name, value)
                        .map_err(|_| VmError::TypeMismatch)?;
                }
            }
            // Numeric-indexed array write.
            (Value::Array(arr), Value::Number(n)) => {
                let key = n.to_display_string();
                if self.store_array_accessor_property(context, *arr, &key, &value, strict)? {
                    // Accessor setter handled the assignment.
                } else if let Some(idx) = crate::array::index_from_number(*n) {
                    let roots = self.collect_allocation_roots(stack);
                    let mut external_visit = |visitor: &mut dyn FnMut(*mut RawGc)| {
                        for &slot in &roots {
                            visitor(slot);
                        }
                    };
                    crate::array::set_with_roots(
                        *arr,
                        &mut self.gc_heap,
                        idx,
                        value,
                        &mut external_visit,
                    )?;
                } else {
                    crate::array::set_named_property(*arr, &mut self.gc_heap, &key, value)
                        .map_err(|_| VmError::TypeMismatch)?;
                }
            }
            // §10.4.5.14 IntegerIndexedElementSet — out-of-range indices
            // silently no-op; element-type / value-type mismatches raise
            // TypeError.
            // <https://tc39.es/ecma262/#sec-integerindexedelementset>
            (Value::TypedArray(t), Value::Number(n)) => match n.as_smi() {
                Some(v) if v >= 0 => {
                    let coerced = binary::dispatch::coerce_element_for_store(t.kind(), &value)?;
                    t.set(v as usize, &coerced);
                }
                _ => return Err(VmError::TypeMismatch),
            },
            // §10.4.5.6 IntegerIndexedExoticObject [[Set]]:
            // canonical-numeric-index strings funnel into element
            // storage (or no-op on out-of-range); non-canonical
            // string / symbol keys store into the lazy expando bag.
            (Value::TypedArray(t), Value::String(key)) => {
                let name = key.to_lossy_string();
                if let Some(n) = canonical_numeric_index_string(&name) {
                    if t.buffer().is_detached()
                        || !n.is_finite()
                        || n.fract() != 0.0
                        || n < 0.0
                        || (n as usize) >= t.length()
                    {
                        // out-of-range / non-integer — silent no-op
                    } else {
                        let coerced = binary::dispatch::coerce_element_for_store(t.kind(), &value)?;
                        t.set(n as usize, &coerced);
                    }
                } else {
                    typed_array_set_expando(self, t, &name, value.clone())?;
                }
            }
            (Value::TypedArray(t), Value::Symbol(sym)) => {
                let bag = typed_array_ensure_expando(self, t)?;
                if !crate::object::set_symbol(bag, &mut self.gc_heap, sym.clone(), value.clone()) {
                    return Err(VmError::TypeError {
                        message: "Cannot store symbol property on TypedArray".to_string(),
                    });
                }
            }
            // §22.2.6 / §27.2.5 — exotic objects that carry a lazy
            // expando bag persist user-installed symbol-keyed
            // properties there. Without this, `re[Symbol.toStringTag]
            // = "tag"` and similar reflective writes would surface a
            // bogus `TypeMismatch`.
            (Value::RegExp(r), Value::Symbol(sym)) => {
                let absent = r.expando(&self.gc_heap).is_none_or(|bag| {
                    object::get_own_symbol_descriptor(bag, &self.gc_heap, sym).is_none()
                });
                if absent && !r.is_extensible(&self.gc_heap) {
                    Self::failed_set_result(
                        strict,
                        "Cannot add symbol property to non-extensible RegExp",
                    )?;
                    stack[top_idx].pc += 1;
                    return Ok(());
                }
                let bag = regexp_ensure_expando(self, r, &recv)?;
                if !crate::object::set_symbol(bag, &mut self.gc_heap, sym.clone(), value.clone()) {
                    return Err(VmError::TypeError {
                        message: "Cannot store symbol property on RegExp".to_string(),
                    });
                }
            }
            (Value::Promise(p), Value::Symbol(sym)) => {
                let bag = promise_ensure_expando_pub(&mut self.gc_heap, p)?;
                if !crate::object::set_symbol(bag, &mut self.gc_heap, sym.clone(), value.clone()) {
                    return Err(VmError::TypeError {
                        message: "Cannot store symbol property on Promise".to_string(),
                    });
                }
            }
            // Heap-allocated callable wrappers expose the
            // function user-property bag for symbol-keyed writes
            // exactly like string-keyed ones.
            (
                Value::Function { function_id } | Value::Closure { function_id, .. },
                Value::Symbol(sym),
            ) => {
                if !self
                    .ordinary_function_has_own_symbol_property_for_extensibility(*function_id, sym)
                    && !self.ordinary_function_is_extensible(*function_id)
                {
                    Self::failed_set_result(
                        strict,
                        "Cannot add symbol property to non-extensible function",
                    )?;
                    stack[top_idx].pc += 1;
                    return Ok(());
                }
                let bag = self.function_user_bag_stack_rooted(
                    stack,
                    *function_id,
                    &[&recv, &idx_value, &value],
                )?;
                if !crate::object::set_symbol(bag, &mut self.gc_heap, sym.clone(), value.clone()) {
                    return Err(VmError::TypeError {
                        message: "Cannot store symbol property on function".to_string(),
                    });
                }
            }
            (Value::NativeFunction(native), Value::Symbol(sym)) => {
                let desc = object::PartialPropertyDescriptor {
                    value: Some(value.clone()),
                    writable: Some(true),
                    enumerable: Some(false),
                    configurable: Some(true),
                    ..Default::default()
                };
                native.define_own_symbol_property(&mut self.gc_heap, sym, desc);
            }
            (Value::ClassConstructor(c), Value::Symbol(sym)) => {
                let statics = c.statics(&self.gc_heap);
                if !crate::object::set_symbol(
                    statics,
                    &mut self.gc_heap,
                    sym.clone(),
                    value.clone(),
                ) {
                    return Err(VmError::TypeError {
                        message: "Cannot store symbol property on class constructor".to_string(),
                    });
                }
            }
            (Value::Undefined | Value::Null | Value::Hole, _) => {
                return Err(VmError::TypeError {
                    message: format!("Cannot set property on {}", value_kind_name(&recv)),
                });
            }
            (
                Value::Boolean(_)
                | Value::Number(_)
                | Value::String(_)
                | Value::Symbol(_)
                | Value::BigInt(_),
                _,
            ) => {
                Self::failed_set_result(
                    strict,
                    format!("Cannot set property on {}", value_kind_name(&recv)),
                )?;
            }
            _ => return Err(VmError::TypeMismatch),
        }
        let frame = &mut stack[top_idx];
        frame.pc += 1;
        Ok(())
    }

    /// Apply descriptor-aware data assignment for computed ordinary-object
    /// writes (`obj[key] = value`).
    pub(crate) fn store_computed_ordinary_property(
        &mut self,
        obj: JsObject,
        key: &str,
        value: Value,
        strict: bool,
    ) -> Result<(), VmError> {
        match crate::object::resolve_set(obj, &self.gc_heap, key) {
            object::SetOutcome::AssignData => {
                if self.ordinary_set_data_property(obj, key, value)? {
                    Ok(())
                } else {
                    Self::failed_set_result(
                        strict,
                        format!("Cannot assign to read-only property '{key}'"),
                    )
                }
            }
            object::SetOutcome::InvokeSetter { .. } => Self::failed_set_result(
                strict,
                format!("Cannot assign to accessor property '{key}' without a setter"),
            ),
            object::SetOutcome::Reject { .. } => {
                Self::failed_set_result(strict, format!("Cannot assign to property '{key}'"))
            }
        }
    }

    /// §10.1.9 `OrdinarySet` — descriptor-aware set that *invokes
    /// accessor setters* via the synchronous interpreter entry. Used
    /// by native helpers (e.g. `Object.assign` per §20.1.2.1
    /// step 4.c.iii.2.b) that need full \[\[Set]] semantics outside
    /// the bytecode dispatch loop. Returns `Ok(())` after the setter
    /// completes; rejects in strict mode with TypeError when the
    /// resolved descriptor is non-writable / accessor-without-setter /
    /// non-extensible.
    pub(crate) fn ordinary_set_with_callable_setter(
        &mut self,
        context: &ExecutionContext,
        obj: JsObject,
        key: &str,
        value: Value,
        strict: bool,
    ) -> Result<(), VmError> {
        match crate::object::resolve_set(obj, &self.gc_heap, key) {
            object::SetOutcome::AssignData => {
                if self.ordinary_set_data_property(obj, key, value)? {
                    Ok(())
                } else {
                    Self::failed_set_result(
                        strict,
                        format!("Cannot assign to read-only property '{key}'"),
                    )
                }
            }
            object::SetOutcome::InvokeSetter { setter } => {
                let mut args: SmallVec<[Value; 8]> = SmallVec::new();
                args.push(value);
                self.run_callable_sync(context, &setter, Value::Object(obj), args)?;
                Ok(())
            }
            object::SetOutcome::Reject { .. } => {
                Self::failed_set_result(strict, format!("Cannot assign to property '{key}'"))
            }
        }
    }

    /// Symbol-keyed counterpart to
    /// [`Self::ordinary_set_with_callable_setter`]. Used by the
    /// `Object.assign` symbol-key copy loop.
    pub(crate) fn ordinary_set_symbol_with_callable_setter(
        &mut self,
        context: &ExecutionContext,
        obj: JsObject,
        sym: &crate::symbol::JsSymbol,
        value: Value,
        strict: bool,
    ) -> Result<(), VmError> {
        match crate::object::resolve_symbol_set(obj, &self.gc_heap, sym) {
            object::SetOutcome::AssignData => {
                if !crate::object::set_symbol(obj, &mut self.gc_heap, sym.clone(), value) {
                    Self::failed_set_result(strict, "Cannot assign to symbol property")?;
                }
                Ok(())
            }
            object::SetOutcome::InvokeSetter { setter } => {
                let mut args: SmallVec<[Value; 8]> = SmallVec::new();
                args.push(value);
                self.run_callable_sync(context, &setter, Value::Object(obj), args)?;
                Ok(())
            }
            object::SetOutcome::Reject { .. } => {
                Self::failed_set_result(strict, "Cannot assign to symbol property")
            }
        }
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
        let key = VmPropertyKey::String(name);
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
    /// Drive one tick of [`Op::LoadProperty`] when the receiver is
    /// an object and the resolved property is an accessor descriptor.
    /// Returns `Ok(true)` when an accessor was dispatched (frame
    /// pushed or undefined written) and the outer loop should
    /// `continue`; `Ok(false)` when the in-frame fast path should
    /// run (data slot, non-object receiver, or absent property).
    ///
    /// # Algorithm — §10.1.8 OrdinaryGet
    /// 1. Decode the operands and read the receiver register.
    /// 2. Probe the receiver's own + prototype chain.
    ///    - Absent / data slot: hand off to the in-frame fast path.
    ///    - Accessor with no getter: write `undefined` to `dst`,
    ///      advance pc, signal handled.
    ///    - Accessor with a getter: advance pc, push a call to the
    ///      getter with `this = receiver` and dst = `dst`.
    /// 3. Class constructors and other special receiver kinds skip
    ///    accessor handling: their property tables are plain data
    ///    today, so the in-frame match is authoritative.
    ///
    /// # See also
    /// - <https://tc39.es/ecma262/#sec-ordinaryget>
    pub(crate) fn drive_load_property(
        &mut self,
        stack: &mut SmallVec<[Frame; 8]>,
        context: &ExecutionContext,
        operands: &[Operand],
    ) -> Result<bool, VmError> {
        let dst = register_operand(operands.first())?;
        let obj_reg = register_operand(operands.get(1))?;
        let name_idx = const_operand(operands.get(2))?;
        let atomized_key = context
            .property_atom(name_idx)
            .ok_or(VmError::InvalidOperand)?;
        let name = atomized_key.name();
        let top_idx = stack.len() - 1;
        let receiver = read_register(&stack[top_idx], obj_reg)?.clone();
        if let Value::Object(obj) = &receiver {
            let obj = *obj;
            let site = context
                .property_ic_site(stack[top_idx].function_id, stack[top_idx].pc)
                .ok_or(VmError::InvalidOperand)?;
            let mut site_disabled = self.load_property_ics[site].is_disabled();
            if let Some(ic) = self.load_property_ics[site].cached() {
                if let Some(value) = ic.load(obj, &self.gc_heap, atomized_key) {
                    self.property_ic_stats.record_hit(PropertyIcKind::Load);
                    Self::finish_property_fast_path_value(&mut stack[top_idx], dst, value)?;
                    return Ok(true);
                }
                self.load_property_ics[site].record_guard_miss_with_stats(
                    &mut self.property_ic_stats,
                    PropertyIcKind::Load,
                );
                site_disabled = self.load_property_ics[site].is_disabled();
            } else {
                self.load_property_ics[site].record_uncached_miss_with_stats(
                    &mut self.property_ic_stats,
                    PropertyIcKind::Load,
                );
            }
            if !site_disabled
                && let Some((ic, value)) =
                    LoadPropertyIc::install_candidate(obj, &self.gc_heap, atomized_key)
            {
                self.load_property_ics[site].install_with_stats(
                    &mut self.property_ic_stats,
                    PropertyIcKind::Load,
                    ic,
                );
                Self::finish_property_fast_path_value(&mut stack[top_idx], dst, value)?;
                return Ok(true);
            }
            let key = VmPropertyKey::atom(atomized_key);
            let pc = stack[top_idx].pc;
            stack[top_idx].pc = pc.checked_add(1).ok_or(VmError::InvalidOperand)?;
            match self.ordinary_get_value(
                context,
                Value::Object(obj),
                Value::Object(obj),
                &key,
                0,
            )? {
                VmGetOutcome::Value(value) => write_register(&mut stack[top_idx], dst, value)?,
                VmGetOutcome::InvokeGetter { getter } => {
                    if abstract_ops::is_callable(&getter) {
                        let args: SmallVec<[Value; 8]> = SmallVec::new();
                        self.invoke(stack, context, &getter, Value::Object(obj), args, dst)?;
                    } else {
                        write_register(&mut stack[top_idx], dst, Value::Undefined)?;
                    }
                }
            }
            return Ok(true);
        }
        // Heap variants that walk a prototype chain in
        // `ordinary_get_value`. Symbol / atomized string keys on
        // Generator / Iterator / Map / Set / WeakRef / Promise /
        // ArrayBuffer / DataView previously fell to the slow
        // `run_load_property_regs` path whose per-type match had no
        // arms for these receivers and surfaced a bogus
        // `TypeMismatch`. Route through the same `[[Get]]` substrate
        // the Object / Proxy fast paths already use so static-key
        // reads (`iter.next`, `map.size`, `prom.then`, …) resolve
        // consistently.
        if matches!(
            receiver,
            Value::Proxy(_)
                | Value::Generator(_)
                | Value::Iterator(_)
                | Value::Map(_)
                | Value::Set(_)
                | Value::WeakMap(_)
                | Value::WeakSet(_)
                | Value::WeakRef(_)
                | Value::FinalizationRegistry(_)
                | Value::Promise(_)
                | Value::ArrayBuffer(_)
                | Value::DataView(_)
        ) {
            let key = VmPropertyKey::atom(atomized_key);
            let pc = stack[top_idx].pc;
            stack[top_idx].pc = pc.checked_add(1).ok_or(VmError::InvalidOperand)?;
            match self.ordinary_get_value(context, receiver.clone(), receiver.clone(), &key, 0)? {
                VmGetOutcome::Value(value) => write_register(&mut stack[top_idx], dst, value)?,
                VmGetOutcome::InvokeGetter { getter } => {
                    if abstract_ops::is_callable(&getter) {
                        let args: SmallVec<[Value; 8]> = SmallVec::new();
                        self.invoke(stack, context, &getter, receiver, args, dst)?;
                    } else {
                        write_register(&mut stack[top_idx], dst, Value::Undefined)?;
                    }
                }
            }
            return Ok(true);
        }
        if matches!(
            receiver,
            Value::Boolean(_)
                | Value::Number(_)
                | Value::String(_)
                | Value::Symbol(_)
                | Value::BigInt(_)
        ) {
            let boxed =
                self.box_sloppy_this_primitive_stack_rooted(stack, receiver.clone(), &[])?;
            let key = VmPropertyKey::atom(atomized_key);
            let pc = stack[top_idx].pc;
            stack[top_idx].pc = pc.checked_add(1).ok_or(VmError::InvalidOperand)?;
            match self.ordinary_get_value(context, boxed, receiver.clone(), &key, 0)? {
                VmGetOutcome::Value(value) => write_register(&mut stack[top_idx], dst, value)?,
                VmGetOutcome::InvokeGetter { getter } => {
                    if abstract_ops::is_callable(&getter) {
                        let args: SmallVec<[Value; 8]> = SmallVec::new();
                        self.invoke(stack, context, &getter, receiver, args, dst)?;
                    } else {
                        write_register(&mut stack[top_idx], dst, Value::Undefined)?;
                    }
                }
            }
            return Ok(true);
        }
        if let Value::BoundFunction(bound) = &receiver {
            match function_metadata::bound_own_property_descriptor(
                bound,
                &self.gc_heap,
                &self.string_heap,
                &name,
            )? {
                Some(object::PropertyDescriptor {
                    kind: object::DescriptorKind::Accessor { getter, .. },
                    ..
                }) => {
                    let pc = stack[top_idx].pc;
                    stack[top_idx].pc = pc.checked_add(1).ok_or(VmError::InvalidOperand)?;
                    match getter {
                        Some(callee) if abstract_ops::is_callable(&callee) => {
                            let args: SmallVec<[Value; 8]> = SmallVec::new();
                            self.invoke(stack, context, &callee, receiver, args, dst)?;
                        }
                        _ => write_register(&mut stack[top_idx], dst, Value::Undefined)?,
                    }
                    return Ok(true);
                }
                Some(_) => return Ok(false),
                None => {
                    if let Some(object::PropertyDescriptor {
                        kind: object::DescriptorKind::Accessor { getter, .. },
                        ..
                    }) = object::get_own_descriptor(
                        self.function_prototype_object()?,
                        &self.gc_heap,
                        &name,
                    ) {
                        let pc = stack[top_idx].pc;
                        stack[top_idx].pc = pc.checked_add(1).ok_or(VmError::InvalidOperand)?;
                        match getter {
                            Some(callee) if abstract_ops::is_callable(&callee) => {
                                let args: SmallVec<[Value; 8]> = SmallVec::new();
                                self.invoke(stack, context, &callee, receiver, args, dst)?;
                            }
                            _ => write_register(&mut stack[top_idx], dst, Value::Undefined)?,
                        }
                        return Ok(true);
                    }
                    if is_restricted_function_property(&name) {
                        let pc = stack[top_idx].pc;
                        stack[top_idx].pc = pc.checked_add(1).ok_or(VmError::InvalidOperand)?;
                        let callee = self.restricted_throw_type_error()?;
                        let args: SmallVec<[Value; 8]> = SmallVec::new();
                        self.invoke(stack, context, &callee, receiver, args, dst)?;
                        return Ok(true);
                    }
                }
            }
        }
        // Function / Closure / NativeFunction / ClassConstructor —
        // probe `%Function.prototype%` for accessor descriptors so
        // §10.2.4 `AddRestrictedFunctionProperties` poison pills
        // (`caller`, `arguments`) and any user-installed accessor on
        // `Function.prototype` invoke their getter rather than
        // collapsing to `undefined` through the in-frame data path.
        if matches!(
            receiver,
            Value::Function { .. }
                | Value::Closure { .. }
                | Value::NativeFunction(_)
                | Value::ClassConstructor(_)
        ) {
            let own_present = match &receiver {
                Value::Function { function_id } | Value::Closure { function_id, .. } => self
                    .function_user_props
                    .get(function_id)
                    .copied()
                    .is_some_and(|bag| {
                        !matches!(
                            object::lookup_own(bag, &self.gc_heap, &name),
                            object::PropertyLookup::Absent
                        )
                    }),
                Value::ClassConstructor(c) => !matches!(
                    object::lookup_own(c.statics(&self.gc_heap), &self.gc_heap, &name),
                    object::PropertyLookup::Absent
                ),
                Value::NativeFunction(native) => native
                    .own_property_descriptor(&self.gc_heap, &self.string_heap, &name)?
                    .is_some(),
                _ => false,
            };
            if !own_present {
                let proto = self.function_prototype_object()?;
                if let object::PropertyLookup::Accessor { getter, .. } =
                    object::lookup(proto, &self.gc_heap, &name)
                {
                    let pc = stack[top_idx].pc;
                    stack[top_idx].pc = pc.checked_add(1).ok_or(VmError::InvalidOperand)?;
                    match getter {
                        Some(callee) if abstract_ops::is_callable(&callee) => {
                            let args: SmallVec<[Value; 8]> = SmallVec::new();
                            self.invoke(stack, context, &callee, receiver, args, dst)?;
                        }
                        _ => write_register(&mut stack[top_idx], dst, Value::Undefined)?,
                    }
                    return Ok(true);
                }
            }
        }
        let obj = match &receiver {
            Value::Object(o) => *o,
            Value::ClassConstructor(c) => c.statics(&self.gc_heap),
            Value::Function { function_id } | Value::Closure { function_id, .. } => {
                let fid = *function_id;
                match self.function_user_props.get(&fid).copied() {
                    Some(bag) => bag,
                    None => self.function_user_bag_with_stack_roots(stack, fid, &[&receiver])?,
                }
            }
            _ => return Ok(false),
        };
        match crate::object::lookup(obj, &self.gc_heap, &name) {
            object::PropertyLookup::Accessor { getter, .. } => {
                let pc = stack[top_idx].pc;
                stack[top_idx].pc = pc.checked_add(1).ok_or(VmError::InvalidOperand)?;
                match getter {
                    Some(callee) if abstract_ops::is_callable(&callee) => {
                        let args: SmallVec<[Value; 8]> = SmallVec::new();
                        self.invoke(stack, context, &callee, receiver, args, dst)?;
                    }
                    _ => {
                        // No getter (or non-callable) — §10.1.8.1
                        // step 4.b returns undefined.
                        write_register(&mut stack[top_idx], dst, Value::Undefined)?;
                    }
                }
                Ok(true)
            }
            // Data or absent — fall through to the in-frame fast path.
            _ => Ok(false),
        }
    }

    /// Drive one tick of [`Op::Instanceof`] through ECMA-262 §13.10.2
    /// `InstanceofOperator(V, target)`. The previous foundation path
    /// only walked `OrdinaryHasInstance`; this version honours
    /// `target[@@hasInstance]` per spec.
    ///
    /// Returns `Ok(false)` only when the right-hand operand is one
    /// of the legacy "raw prototype object as rhs" shapes the older
    /// fixtures pass — those still fall through to the in-frame
    /// fast path's prototype-walk fallback.
    pub(crate) fn drive_instanceof(
        &mut self,
        stack: &mut SmallVec<[Frame; 8]>,
        context: &ExecutionContext,
        operands: &[Operand],
    ) -> Result<bool, VmError> {
        let dst = register_operand(operands.first())?;
        let lhs_reg = register_operand(operands.get(1))?;
        let rhs_reg = register_operand(operands.get(2))?;
        let top_idx = stack.len() - 1;
        let lhs = read_register(&stack[top_idx], lhs_reg)?.clone();
        let rhs = read_register(&stack[top_idx], rhs_reg)?.clone();
        let result = self.instanceof_operator_stack_rooted(context, stack, &lhs, &rhs)?;
        let pc = stack[top_idx].pc;
        stack[top_idx].pc = pc.checked_add(1).ok_or(VmError::InvalidOperand)?;
        write_register(&mut stack[top_idx], dst, Value::Boolean(result))?;
        Ok(true)
    }

    /// Drive one tick of [`Op::LoadElement`] for computed ordinary
    /// object/proxy reads whose resolved descriptor is an accessor.
    pub(crate) fn drive_load_element(
        &mut self,
        stack: &mut SmallVec<[Frame; 8]>,
        context: &ExecutionContext,
        operands: &[Operand],
    ) -> Result<bool, VmError> {
        let dst = register_operand(operands.first())?;
        let obj_reg = register_operand(operands.get(1))?;
        let key_reg = register_operand(operands.get(2))?;
        let top_idx = stack.len() - 1;
        let receiver = read_register(&stack[top_idx], obj_reg)?.clone();
        let key_value_raw = read_register(&stack[top_idx], key_reg)?.clone();
        let key_value = self.coerce_property_key_value(context, key_value_raw)?;
        let key = match &key_value {
            Value::String(s) => VmPropertyKey::OwnedString(s.to_lossy_string()),
            Value::Number(n) => VmPropertyKey::OwnedString(n.to_display_string()),
            Value::Symbol(sym) => VmPropertyKey::Symbol(sym.clone()),
            _ => return Ok(false),
        };

        // Heap values that walk a prototype chain in `ordinary_get_value`.
        // `Array` / `TypedArray` / primitive wrappers / `BoundFunction` /
        // function callables keep their own legacy fast paths below; the
        // arms listed here previously fell through to a TypeMismatch on
        // symbol / numeric keys because the slow `run_load_element_regs`
        // path had no matching arm. Routing them through the common
        // `[[Get]]` substrate gives Generator / Iterator / Map / Set /
        // WeakRef / Promise / ArrayBuffer / DataView consistent symbol
        // and numeric-key behaviour (notably `@@toStringTag`).
        let prototype_routed = matches!(
            receiver,
            Value::Object(_)
                | Value::Proxy(_)
                | Value::Generator(_)
                | Value::Iterator(_)
                | Value::Map(_)
                | Value::Set(_)
                | Value::WeakMap(_)
                | Value::WeakSet(_)
                | Value::WeakRef(_)
                | Value::FinalizationRegistry(_)
                | Value::Promise(_)
                | Value::ArrayBuffer(_)
                | Value::DataView(_)
        );
        if prototype_routed {
            let pc = stack[top_idx].pc;
            stack[top_idx].pc = pc.checked_add(1).ok_or(VmError::InvalidOperand)?;
            match self.ordinary_get_value(context, receiver.clone(), receiver.clone(), &key, 0)? {
                VmGetOutcome::Value(value) => write_register(&mut stack[top_idx], dst, value)?,
                VmGetOutcome::InvokeGetter { getter } => {
                    if abstract_ops::is_callable(&getter) {
                        let args: SmallVec<[Value; 8]> = SmallVec::new();
                        self.invoke(stack, context, &getter, receiver, args, dst)?;
                    } else {
                        write_register(&mut stack[top_idx], dst, Value::Undefined)?;
                    }
                }
            }
            return Ok(true);
        }

        if let (Value::BoundFunction(bound), Some(key)) = (&receiver, key.string_name()) {
            match function_metadata::bound_own_property_descriptor(
                bound,
                &self.gc_heap,
                &self.string_heap,
                key,
            )? {
                Some(object::PropertyDescriptor {
                    kind: object::DescriptorKind::Accessor { getter, .. },
                    ..
                }) => {
                    let pc = stack[top_idx].pc;
                    stack[top_idx].pc = pc.checked_add(1).ok_or(VmError::InvalidOperand)?;
                    match getter {
                        Some(callee) if abstract_ops::is_callable(&callee) => {
                            let args: SmallVec<[Value; 8]> = SmallVec::new();
                            self.invoke(stack, context, &callee, receiver, args, dst)?;
                        }
                        _ => write_register(&mut stack[top_idx], dst, Value::Undefined)?,
                    }
                    return Ok(true);
                }
                Some(_) => return Ok(false),
                None => {
                    if let Some(object::PropertyDescriptor {
                        kind: object::DescriptorKind::Accessor { getter, .. },
                        ..
                    }) = object::get_own_descriptor(
                        self.function_prototype_object()?,
                        &self.gc_heap,
                        key,
                    ) {
                        let pc = stack[top_idx].pc;
                        stack[top_idx].pc = pc.checked_add(1).ok_or(VmError::InvalidOperand)?;
                        match getter {
                            Some(callee) if abstract_ops::is_callable(&callee) => {
                                let args: SmallVec<[Value; 8]> = SmallVec::new();
                                self.invoke(stack, context, &callee, receiver, args, dst)?;
                            }
                            _ => write_register(&mut stack[top_idx], dst, Value::Undefined)?,
                        }
                        return Ok(true);
                    }
                    if is_restricted_function_property(key) {
                        let pc = stack[top_idx].pc;
                        stack[top_idx].pc = pc.checked_add(1).ok_or(VmError::InvalidOperand)?;
                        let callee = self.restricted_throw_type_error()?;
                        let args: SmallVec<[Value; 8]> = SmallVec::new();
                        self.invoke(stack, context, &callee, receiver, args, dst)?;
                        return Ok(true);
                    }
                }
            }
        }

        let obj = match &receiver {
            Value::Object(obj) => *obj,
            Value::ClassConstructor(class) => {
                if key.string_name().is_some_and(|key| key == "prototype") {
                    let pc = stack[top_idx].pc;
                    stack[top_idx].pc = pc.checked_add(1).ok_or(VmError::InvalidOperand)?;
                    write_register(
                        &mut stack[top_idx],
                        dst,
                        Value::Object(class.prototype(&self.gc_heap)),
                    )?;
                    return Ok(true);
                }
                class.statics(&self.gc_heap)
            }
            Value::Function { function_id } | Value::Closure { function_id, .. } => {
                let Some(bag) = self.function_user_props.get(function_id).copied() else {
                    return Ok(false);
                };
                bag
            }
            _ => return Ok(false),
        };
        let lookup = match &key {
            VmPropertyKey::Symbol(sym) => crate::object::lookup_symbol(obj, &self.gc_heap, sym),
            _ => crate::object::lookup(
                obj,
                &self.gc_heap,
                key.string_name()
                    .expect("non-symbol key has string spelling"),
            ),
        };
        match lookup {
            object::PropertyLookup::Data { value, .. } => {
                let pc = stack[top_idx].pc;
                stack[top_idx].pc = pc.checked_add(1).ok_or(VmError::InvalidOperand)?;
                write_register(&mut stack[top_idx], dst, value)?;
                Ok(true)
            }
            object::PropertyLookup::Accessor { getter, .. } => {
                let pc = stack[top_idx].pc;
                stack[top_idx].pc = pc.checked_add(1).ok_or(VmError::InvalidOperand)?;
                match getter {
                    Some(callee) if abstract_ops::is_callable(&callee) => {
                        let args: SmallVec<[Value; 8]> = SmallVec::new();
                        self.invoke(stack, context, &callee, receiver, args, dst)?;
                    }
                    _ => {
                        write_register(&mut stack[top_idx], dst, Value::Undefined)?;
                    }
                }
                Ok(true)
            }
            _ => Ok(false),
        }
    }

    /// Apply descriptor-aware data assignment for computed ordinary-object
    /// writes (`obj[key] = value`).
    fn function_is_strict(context: &ExecutionContext, function_id: u32) -> bool {
        context.function_is_strict(function_id)
    }

    fn current_frame_is_strict(stack: &SmallVec<[Frame; 8]>, context: &ExecutionContext) -> bool {
        stack
            .last()
            .is_some_and(|frame| Self::function_is_strict(context, frame.function_id))
    }

    fn finish_failed_set(
        stack: &mut SmallVec<[Frame; 8]>,
        context: &ExecutionContext,
        message: impl Into<String>,
    ) -> Result<bool, VmError> {
        if Self::current_frame_is_strict(stack, context) {
            return Err(VmError::TypeError {
                message: message.into(),
            });
        }
        let top_idx = stack.len() - 1;
        let pc = stack[top_idx].pc;
        stack[top_idx].pc = pc.checked_add(1).ok_or(VmError::InvalidOperand)?;
        Ok(true)
    }

    fn failed_set_result(strict: bool, message: impl Into<String>) -> Result<(), VmError> {
        if strict {
            Err(VmError::TypeError {
                message: message.into(),
            })
        } else {
            Ok(())
        }
    }

    fn advance_property_fast_path(frame: &mut Frame) -> Result<(), VmError> {
        let pc = frame.pc;
        frame.pc = pc.checked_add(1).ok_or(VmError::InvalidOperand)?;
        Ok(())
    }

    fn finish_property_fast_path_value(
        frame: &mut Frame,
        dst: u16,
        value: Value,
    ) -> Result<(), VmError> {
        Self::advance_property_fast_path(frame)?;
        write_register(frame, dst, value)
    }

    fn store_to_primitive_base(
        &mut self,
        stack: &mut SmallVec<[Frame; 8]>,
        context: &ExecutionContext,
        receiver: Value,
        key: VmPropertyKey,
        value: Value,
        scratch_reg: u16,
    ) -> Result<bool, VmError> {
        let Some(base_object) =
            self.object_for_primitive_property_base_stack_rooted(stack, &receiver)?
        else {
            return Ok(false);
        };
        let strict = Self::current_frame_is_strict(stack, context);
        let mut current = object::prototype_value(base_object, &self.gc_heap);
        let mut hops = 0;
        while let Some(proto) = current {
            if hops >= object::PROTO_CHAIN_HARD_CAP {
                break;
            }
            hops += 1;
            match proto {
                Value::Object(obj) => {
                    let lookup = match &key {
                        VmPropertyKey::Symbol(sym) => {
                            object::lookup_own_symbol(obj, &self.gc_heap, sym)
                        }
                        _ => object::lookup_own(
                            obj,
                            &self.gc_heap,
                            key.string_name()
                                .expect("non-symbol key has string spelling"),
                        ),
                    };
                    match lookup {
                        object::PropertyLookup::Data { flags, .. } => {
                            if !flags.writable() {
                                let name = key.string_name().unwrap_or("symbol");
                                Self::failed_set_result(
                                    strict,
                                    format!("Cannot assign to read-only property '{name}'"),
                                )?;
                            } else {
                                let name = key.string_name().unwrap_or("symbol");
                                Self::failed_set_result(
                                    strict,
                                    format!("Cannot assign to property '{name}' on primitive"),
                                )?;
                            }
                            let top_idx = stack.len() - 1;
                            let pc = stack[top_idx].pc;
                            stack[top_idx].pc = pc.checked_add(1).ok_or(VmError::InvalidOperand)?;
                            return Ok(true);
                        }
                        object::PropertyLookup::Accessor { setter, .. } => {
                            let Some(setter) = setter else {
                                Self::failed_set_result(
                                    strict,
                                    "Cannot assign to accessor property without a setter",
                                )?;
                                let top_idx = stack.len() - 1;
                                let pc = stack[top_idx].pc;
                                stack[top_idx].pc =
                                    pc.checked_add(1).ok_or(VmError::InvalidOperand)?;
                                return Ok(true);
                            };
                            let top_idx = stack.len() - 1;
                            let pc = stack[top_idx].pc;
                            stack[top_idx].pc = pc.checked_add(1).ok_or(VmError::InvalidOperand)?;
                            let mut args: SmallVec<[Value; 8]> = SmallVec::new();
                            args.push(value);
                            self.invoke(stack, context, &setter, receiver, args, scratch_reg)?;
                            return Ok(true);
                        }
                        object::PropertyLookup::Absent => {
                            current = object::prototype_value(obj, &self.gc_heap);
                        }
                    }
                }
                Value::Proxy(proxy) => {
                    let key_value = self.vm_property_key_to_value(&key)?;
                    let trap_args: SmallVec<[Value; 8]> = smallvec::smallvec![
                        proxy.target(),
                        key_value,
                        value.clone(),
                        receiver.clone()
                    ];
                    let top_idx = stack.len() - 1;
                    let pc = stack[top_idx].pc;
                    stack[top_idx].pc = pc.checked_add(1).ok_or(VmError::InvalidOperand)?;
                    match self.invoke_proxy_trap(context, &proxy, "set", trap_args)? {
                        Some(_) => {}
                        None => {
                            let Value::Object(target) = proxy.target() else {
                                return Err(VmError::TypeMismatch);
                            };
                            match &key {
                                VmPropertyKey::Symbol(sym) => {
                                    match object::resolve_symbol_set(target, &self.gc_heap, sym) {
                                        object::SetOutcome::AssignData => {}
                                        object::SetOutcome::InvokeSetter { setter } => {
                                            let mut args: SmallVec<[Value; 8]> = SmallVec::new();
                                            args.push(value);
                                            self.invoke(
                                                stack,
                                                context,
                                                &setter,
                                                receiver,
                                                args,
                                                scratch_reg,
                                            )?;
                                        }
                                        object::SetOutcome::Reject { .. } => {
                                            Self::failed_set_result(
                                                strict,
                                                "Cannot assign to symbol property",
                                            )?;
                                        }
                                    }
                                }
                                _ => {
                                    let key = key
                                        .string_name()
                                        .expect("non-symbol key has string spelling");
                                    match object::resolve_set(target, &self.gc_heap, key) {
                                        object::SetOutcome::AssignData => {}
                                        object::SetOutcome::InvokeSetter { setter } => {
                                            let mut args: SmallVec<[Value; 8]> = SmallVec::new();
                                            args.push(value);
                                            self.invoke(
                                                stack,
                                                context,
                                                &setter,
                                                receiver,
                                                args,
                                                scratch_reg,
                                            )?;
                                        }
                                        object::SetOutcome::Reject { .. } => {
                                            Self::failed_set_result(
                                                strict,
                                                format!("Cannot assign to property '{key}'"),
                                            )?;
                                        }
                                    }
                                }
                            }
                        }
                    }
                    return Ok(true);
                }
                _ => break,
            }
        }

        let top_idx = stack.len() - 1;
        let name = key.string_name().unwrap_or("symbol");
        Self::failed_set_result(
            strict,
            format!("Cannot assign to property '{name}' on primitive"),
        )?;
        let pc = stack[top_idx].pc;
        stack[top_idx].pc = pc.checked_add(1).ok_or(VmError::InvalidOperand)?;
        Ok(true)
    }

    /// Drive one tick of [`Op::StoreElement`] when a computed
    /// string, numeric, or symbol property write on an ordinary
    /// object/proxy must obey §10.1.9 OrdinarySet.
    pub(crate) fn drive_store_element(
        &mut self,
        stack: &mut SmallVec<[Frame; 8]>,
        context: &ExecutionContext,
        operands: &[Operand],
    ) -> Result<bool, VmError> {
        let obj_reg = register_operand(operands.first())?;
        let key_reg = register_operand(operands.get(1))?;
        let src_reg = register_operand(operands.get(2))?;
        let scratch_reg = register_operand(operands.get(3))?;
        let top_idx = stack.len() - 1;
        let receiver = read_register(&stack[top_idx], obj_reg)?.clone();
        let key_value_raw = read_register(&stack[top_idx], key_reg)?.clone();
        let key_value = self.coerce_property_key_value(context, key_value_raw)?;
        let value = read_register(&stack[top_idx], src_reg)?.clone();
        let strict = Self::current_frame_is_strict(stack, context);
        enum ComputedPropertyKey {
            String(String),
            Symbol(crate::symbol::JsSymbol),
        }
        let key = match &key_value {
            Value::String(s) => ComputedPropertyKey::String(s.to_lossy_string()),
            Value::Number(n) => ComputedPropertyKey::String(n.to_display_string()),
            Value::Symbol(sym) => ComputedPropertyKey::Symbol(sym.clone()),
            _ => return Ok(false),
        };
        if let Value::Proxy(p) = &receiver {
            let proxy = p.clone();
            let key_arg = match &key {
                ComputedPropertyKey::String(key) => {
                    Value::String(JsString::from_str(key, &self.string_heap)?)
                }
                ComputedPropertyKey::Symbol(sym) => Value::Symbol(sym.clone()),
            };
            let trap_args: SmallVec<[Value; 8]> = smallvec::smallvec![
                proxy.target(),
                key_arg,
                value.clone(),
                Value::Proxy(proxy.clone()),
            ];
            let pc = stack[top_idx].pc;
            stack[top_idx].pc = pc.checked_add(1).ok_or(VmError::InvalidOperand)?;
            match self.invoke_proxy_trap(context, &proxy, "set", trap_args)? {
                Some(_) => {}
                None => {
                    let target_value = proxy.target();
                    let Value::Object(target) = target_value else {
                        let vm_key = match &key {
                            ComputedPropertyKey::String(key) => {
                                VmPropertyKey::OwnedString(key.clone())
                            }
                            ComputedPropertyKey::Symbol(sym) => VmPropertyKey::Symbol(sym.clone()),
                        };
                        if !self.ordinary_set_data_value(
                            context,
                            target_value,
                            &vm_key,
                            value,
                            Value::Proxy(proxy.clone()),
                            0,
                        )? {
                            Self::failed_set_result(strict, "Cannot assign to property")?;
                        }
                        return Ok(true);
                    };
                    let outcome = match &key {
                        ComputedPropertyKey::String(key) => {
                            object::resolve_set(target, &self.gc_heap, key)
                        }
                        ComputedPropertyKey::Symbol(sym) => {
                            object::resolve_symbol_set(target, &self.gc_heap, sym)
                        }
                    };
                    match outcome {
                        object::SetOutcome::AssignData => {
                            let ok = match &key {
                                ComputedPropertyKey::String(key) => {
                                    self.ordinary_set_data_property(target, key, value)?
                                }
                                ComputedPropertyKey::Symbol(sym) => object::set_symbol(
                                    target,
                                    &mut self.gc_heap,
                                    sym.clone(),
                                    value,
                                ),
                            };
                            if !ok {
                                Self::failed_set_result(strict, "Cannot assign to property")?;
                            }
                        }
                        object::SetOutcome::InvokeSetter { setter } => {
                            if !abstract_ops::is_callable(&setter) {
                                Self::failed_set_result(
                                    strict,
                                    "Cannot assign to accessor property without a setter",
                                )?;
                            } else {
                                let mut args: SmallVec<[Value; 8]> = SmallVec::new();
                                args.push(value);
                                self.invoke(
                                    stack,
                                    context,
                                    &setter,
                                    Value::Proxy(proxy.clone()),
                                    args,
                                    scratch_reg,
                                )?;
                            }
                        }
                        object::SetOutcome::Reject { .. } => {
                            Self::failed_set_result(strict, "Cannot assign to property")?;
                        }
                    }
                }
            }
            return Ok(true);
        }
        if let (Value::BoundFunction(bound), ComputedPropertyKey::String(key)) = (&receiver, &key) {
            match function_metadata::bound_own_property_descriptor(
                bound,
                &self.gc_heap,
                &self.string_heap,
                key,
            )? {
                Some(object::PropertyDescriptor {
                    kind: object::DescriptorKind::Accessor { setter, .. },
                    ..
                }) => {
                    let setter = setter.ok_or(VmError::TypeMismatch)?;
                    if !abstract_ops::is_callable(&setter) {
                        return Err(VmError::TypeMismatch);
                    }
                    let pc = stack[top_idx].pc;
                    stack[top_idx].pc = pc.checked_add(1).ok_or(VmError::InvalidOperand)?;
                    let mut args: SmallVec<[Value; 8]> = SmallVec::new();
                    args.push(value);
                    self.invoke(stack, context, &setter, receiver, args, scratch_reg)?;
                    return Ok(true);
                }
                Some(_) => return Ok(false),
                None => {
                    if let Some(object::PropertyDescriptor {
                        kind: object::DescriptorKind::Accessor { setter, .. },
                        ..
                    }) = object::get_own_descriptor(
                        self.function_prototype_object()?,
                        &self.gc_heap,
                        key,
                    ) {
                        let setter = setter.ok_or(VmError::TypeMismatch)?;
                        if !abstract_ops::is_callable(&setter) {
                            return Err(VmError::TypeMismatch);
                        }
                        let pc = stack[top_idx].pc;
                        stack[top_idx].pc = pc.checked_add(1).ok_or(VmError::InvalidOperand)?;
                        let mut args: SmallVec<[Value; 8]> = SmallVec::new();
                        args.push(value);
                        self.invoke(stack, context, &setter, receiver, args, scratch_reg)?;
                        return Ok(true);
                    }
                    if is_restricted_function_property(key) {
                        let pc = stack[top_idx].pc;
                        stack[top_idx].pc = pc.checked_add(1).ok_or(VmError::InvalidOperand)?;
                        let callee = self.restricted_throw_type_error()?;
                        let mut args: SmallVec<[Value; 8]> = SmallVec::new();
                        args.push(value);
                        self.invoke(stack, context, &callee, receiver, args, scratch_reg)?;
                        return Ok(true);
                    }
                }
            }
        }
        if let (Value::NativeFunction(native), ComputedPropertyKey::Symbol(sym)) = (&receiver, &key)
        {
            let obj = native.own_properties_object(&self.gc_heap);
            match object::resolve_symbol_set(obj, &self.gc_heap, sym) {
                object::SetOutcome::AssignData => {
                    if !object::set_symbol(obj, &mut self.gc_heap, sym.clone(), value) {
                        return Self::finish_failed_set(
                            stack,
                            context,
                            "Cannot assign to symbol property",
                        );
                    }
                }
                object::SetOutcome::InvokeSetter { setter } => {
                    if !abstract_ops::is_callable(&setter) {
                        return Self::finish_failed_set(
                            stack,
                            context,
                            "Cannot assign to accessor property without a setter",
                        );
                    }
                    let pc = stack[top_idx].pc;
                    stack[top_idx].pc = pc.checked_add(1).ok_or(VmError::InvalidOperand)?;
                    let mut args: SmallVec<[Value; 8]> = SmallVec::new();
                    args.push(value);
                    self.invoke(stack, context, &setter, receiver, args, scratch_reg)?;
                    return Ok(true);
                }
                object::SetOutcome::Reject { .. } => {
                    return Self::finish_failed_set(
                        stack,
                        context,
                        "Cannot assign to symbol property",
                    );
                }
            }
            let pc = stack[top_idx].pc;
            stack[top_idx].pc = pc.checked_add(1).ok_or(VmError::InvalidOperand)?;
            return Ok(true);
        }
        if matches!(
            receiver,
            Value::Boolean(_)
                | Value::Number(_)
                | Value::String(_)
                | Value::Symbol(_)
                | Value::BigInt(_)
        ) {
            let key = match key {
                ComputedPropertyKey::String(key) => VmPropertyKey::OwnedString(key),
                ComputedPropertyKey::Symbol(sym) => VmPropertyKey::Symbol(sym),
            };
            return self.store_to_primitive_base(stack, context, receiver, key, value, scratch_reg);
        }
        if let Value::RegExp(r) = &receiver {
            match &key {
                ComputedPropertyKey::String(key) if key == "lastIndex" => {
                    regexp_prototype::store_property(r, &mut self.gc_heap, key, value);
                }
                ComputedPropertyKey::String(key) => {
                    let absent = r.expando(&self.gc_heap).is_none_or(|bag| {
                        matches!(
                            object::lookup_own(bag, &self.gc_heap, key),
                            object::PropertyLookup::Absent
                        )
                    });
                    if absent && !r.is_extensible(&self.gc_heap) {
                        return Self::finish_failed_set(
                            stack,
                            context,
                            format!("Cannot add property '{key}' to non-extensible RegExp"),
                        );
                    }
                    let bag = regexp_ensure_expando(self, r, &receiver)?;
                    if !self.ordinary_set_data_property(bag, key, value)? {
                        return Self::finish_failed_set(
                            stack,
                            context,
                            format!("Cannot assign to property '{key}'"),
                        );
                    }
                }
                ComputedPropertyKey::Symbol(sym) => {
                    let absent = r.expando(&self.gc_heap).is_none_or(|bag| {
                        object::get_own_symbol_descriptor(bag, &self.gc_heap, sym).is_none()
                    });
                    if absent && !r.is_extensible(&self.gc_heap) {
                        return Self::finish_failed_set(
                            stack,
                            context,
                            "Cannot add symbol property to non-extensible RegExp",
                        );
                    }
                    let bag = regexp_ensure_expando(self, r, &receiver)?;
                    if !object::set_symbol(bag, &mut self.gc_heap, sym.clone(), value) {
                        return Self::finish_failed_set(
                            stack,
                            context,
                            "Cannot assign to symbol property",
                        );
                    }
                }
            }
            let pc = stack[top_idx].pc;
            stack[top_idx].pc = pc.checked_add(1).ok_or(VmError::InvalidOperand)?;
            return Ok(true);
        }
        let obj = match &receiver {
            Value::Object(obj) => *obj,
            Value::ClassConstructor(class) => class.statics(&self.gc_heap),
            Value::Function { function_id } | Value::Closure { function_id, .. } => {
                match &key {
                    ComputedPropertyKey::String(key) => {
                        if function_metadata::ordinary_function_metadata_key(key).is_some()
                            && let Some(desc) = self.ordinary_function_own_property_descriptor(
                                Some(context),
                                *function_id,
                                key,
                            )?
                            && !desc.writable()
                        {
                            return Self::finish_failed_set(
                                stack,
                                context,
                                format!("Cannot assign to read-only property '{key}' of function"),
                            );
                        }
                        let has_own = self
                            .ordinary_function_has_own_string_property_for_extensibility(
                                context,
                                *function_id,
                                key,
                            )?;
                        if !has_own && !self.ordinary_function_is_extensible(*function_id) {
                            return Self::finish_failed_set(
                                stack,
                                context,
                                format!("Cannot add property '{key}' to non-extensible function"),
                            );
                        }
                    }
                    ComputedPropertyKey::Symbol(sym) => {
                        if !self.ordinary_function_has_own_symbol_property_for_extensibility(
                            *function_id,
                            sym,
                        ) && !self.ordinary_function_is_extensible(*function_id)
                        {
                            return Self::finish_failed_set(
                                stack,
                                context,
                                "Cannot add symbol property to non-extensible function",
                            );
                        }
                    }
                }
                self.function_user_bag_stack_rooted(stack, *function_id, &[&receiver, &value])?
            }
            _ => return Ok(false),
        };
        let outcome = match &key {
            ComputedPropertyKey::String(key) => crate::object::resolve_set(obj, &self.gc_heap, key),
            ComputedPropertyKey::Symbol(sym) => {
                crate::object::resolve_symbol_set(obj, &self.gc_heap, sym)
            }
        };
        match outcome {
            object::SetOutcome::AssignData => {
                let ok = match &key {
                    ComputedPropertyKey::String(key) => {
                        self.ordinary_set_data_property(obj, key, value)?
                    }
                    ComputedPropertyKey::Symbol(sym) => {
                        object::set_symbol(obj, &mut self.gc_heap, sym.clone(), value)
                    }
                };
                if !ok {
                    return Self::finish_failed_set(
                        stack,
                        context,
                        "Cannot assign to read-only property",
                    );
                }
                let pc = stack[top_idx].pc;
                stack[top_idx].pc = pc.checked_add(1).ok_or(VmError::InvalidOperand)?;
                Ok(true)
            }
            object::SetOutcome::InvokeSetter { setter } => {
                if !abstract_ops::is_callable(&setter) {
                    return Self::finish_failed_set(
                        stack,
                        context,
                        "Cannot assign to accessor property without a setter",
                    );
                }
                let pc = stack[top_idx].pc;
                stack[top_idx].pc = pc.checked_add(1).ok_or(VmError::InvalidOperand)?;
                let mut args: SmallVec<[Value; 8]> = SmallVec::new();
                args.push(value);
                self.invoke(stack, context, &setter, receiver, args, scratch_reg)?;
                Ok(true)
            }
            object::SetOutcome::Reject { .. } => {
                Self::finish_failed_set(stack, context, "Cannot assign to property")
            }
        }
    }

    /// Drive one tick of [`Op::StoreProperty`] when §10.1.9
    /// OrdinarySet routes through an accessor setter, hits a
    /// non-writable shadow, or hits a non-extensible receiver.
    /// Returns `Ok(true)` when the dispatch path took over,
    /// `Ok(false)` when the in-frame data-write fast path should run.
    ///
    /// Non-writable / accessor-without-setter / non-extensible
    /// rejections follow the caller frame's compiled strict flag:
    /// strict callers throw `TypeError`, sloppy callers silently
    /// ignore the failed write after advancing the program counter.
    ///
    /// # See also
    /// - <https://tc39.es/ecma262/#sec-ordinaryset>
    /// - <https://tc39.es/ecma262/#sec-ordinarysetwithowndescriptor>
    pub(crate) fn drive_store_property(
        &mut self,
        stack: &mut SmallVec<[Frame; 8]>,
        context: &ExecutionContext,
        operands: &[Operand],
    ) -> Result<bool, VmError> {
        let obj_reg = register_operand(operands.first())?;
        let name_idx = const_operand(operands.get(1))?;
        let src_reg = register_operand(operands.get(2))?;
        let scratch_reg = register_operand(operands.get(3))?;
        let atomized_key = context
            .property_atom(name_idx)
            .ok_or(VmError::InvalidOperand)?;
        let name = atomized_key.name();
        let top_idx = stack.len() - 1;
        let receiver = read_register(&stack[top_idx], obj_reg)?.clone();
        let value = read_register(&stack[top_idx], src_reg)?.clone();
        let strict = Self::current_frame_is_strict(stack, context);
        if let Value::Object(obj) = &receiver
            && object::supports_fast_property_ic(*obj, &self.gc_heap)
        {
            let obj = *obj;
            let site = context
                .property_ic_site(stack[top_idx].function_id, stack[top_idx].pc)
                .ok_or(VmError::InvalidOperand)?;
            if let Some(ic) = self.store_property_ics[site].cached_ref() {
                if ic
                    .store(obj, &mut self.gc_heap, atomized_key, &value)
                    .is_some()
                {
                    self.property_ic_stats.record_hit(PropertyIcKind::Store);
                    Self::advance_property_fast_path(&mut stack[top_idx])?;
                    return Ok(true);
                }
                self.store_property_ics[site].record_guard_miss_with_stats(
                    &mut self.property_ic_stats,
                    PropertyIcKind::Store,
                );
            } else {
                self.store_property_ics[site].record_uncached_miss_with_stats(
                    &mut self.property_ic_stats,
                    PropertyIcKind::Store,
                );
            }
        }
        // §28.2.4.5 / §10.5.9 Proxy.[[Set]] — invoke the `set` trap
        // when present; otherwise delegate to the target.
        if let Value::Proxy(p) = &receiver {
            let proxy = p.clone();
            if proxy.is_revoked() {
                return Err(VmError::TypeError {
                    message: "Cannot perform 'set' on a proxy that has been revoked".to_string(),
                });
            }
            let key_str = JsString::from_str(name, &self.string_heap)?;
            let key_vm = VmPropertyKey::atom(atomized_key);
            let trap_args: SmallVec<[Value; 8]> = smallvec::smallvec![
                proxy.target(),
                Value::String(key_str),
                value.clone(),
                Value::Proxy(proxy.clone()),
            ];
            let pc = stack[top_idx].pc;
            stack[top_idx].pc = pc.checked_add(1).ok_or(VmError::InvalidOperand)?;
            match self.invoke_proxy_trap(context, &proxy, "set", trap_args)? {
                Some(result) => {
                    let ok = result.to_boolean();
                    if !ok {
                        Self::failed_set_result(
                            strict,
                            format!("Cannot assign to property '{name}'"),
                        )?;
                        return Ok(true);
                    }
                    // §10.5.9 step 13–14 invariants — when trap reports
                    // success, ensure target descriptor admits the
                    // value.
                    let target_value = proxy.target();
                    let target_desc = self
                        .ordinary_get_own_property_descriptor_value_stack_rooted(
                            context,
                            stack,
                            target_value.clone(),
                            &key_vm,
                            0,
                        )?;
                    if let Some(desc) = target_desc.as_ref()
                        && !desc.configurable()
                    {
                        match &desc.kind {
                            object::DescriptorKind::Data { value: target_v }
                                if !desc.writable() =>
                            {
                                if !abstract_ops::same_value(target_v, &value) {
                                    return Err(VmError::TypeError {
                                        message:
                                            "Proxy set trap reported success but target is non-configurable non-writable with a different value"
                                                .to_string(),
                                    });
                                }
                            }
                            object::DescriptorKind::Accessor { setter: None, .. } => {
                                return Err(VmError::TypeError {
                                    message:
                                        "Proxy set trap reported success but target is a non-configurable accessor without a setter"
                                            .to_string(),
                                });
                            }
                            _ => {}
                        }
                    }
                }
                None => {
                    let target_value = proxy.target();
                    let Value::Object(target) = target_value else {
                        if !self.ordinary_set_data_value(
                            context,
                            target_value,
                            &key_vm,
                            value,
                            Value::Proxy(proxy.clone()),
                            0,
                        )? {
                            Self::failed_set_result(
                                strict,
                                format!("Cannot assign to property '{name}'"),
                            )?;
                        }
                        return Ok(true);
                    };
                    match object::resolve_set(target, &self.gc_heap, &name) {
                        object::SetOutcome::AssignData => {
                            if !self.ordinary_set_data_property(target, &name, value)? {
                                Self::failed_set_result(
                                    strict,
                                    format!("Cannot assign to property '{name}'"),
                                )?;
                            }
                        }
                        object::SetOutcome::InvokeSetter { setter } => {
                            if !abstract_ops::is_callable(&setter) {
                                Self::failed_set_result(
                                    strict,
                                    format!(
                                        "Cannot assign to accessor property '{name}' without a setter"
                                    ),
                                )?;
                            } else {
                                let mut args: SmallVec<[Value; 8]> = SmallVec::new();
                                args.push(value);
                                self.invoke(
                                    stack,
                                    context,
                                    &setter,
                                    Value::Proxy(proxy.clone()),
                                    args,
                                    scratch_reg,
                                )?;
                            }
                        }
                        object::SetOutcome::Reject { .. } => {
                            Self::failed_set_result(
                                strict,
                                format!("Cannot assign to property '{name}'"),
                            )?;
                        }
                    }
                }
            }
            return Ok(true);
        }
        if let Value::BoundFunction(bound) = &receiver {
            match function_metadata::bound_own_property_descriptor(
                bound,
                &self.gc_heap,
                &self.string_heap,
                &name,
            )? {
                Some(object::PropertyDescriptor {
                    kind: object::DescriptorKind::Accessor { setter, .. },
                    ..
                }) => {
                    let setter = setter.ok_or(VmError::TypeMismatch)?;
                    if !abstract_ops::is_callable(&setter) {
                        return Err(VmError::TypeMismatch);
                    }
                    let pc = stack[top_idx].pc;
                    stack[top_idx].pc = pc.checked_add(1).ok_or(VmError::InvalidOperand)?;
                    let mut args: SmallVec<[Value; 8]> = SmallVec::new();
                    args.push(value);
                    self.invoke(stack, context, &setter, receiver, args, scratch_reg)?;
                    return Ok(true);
                }
                Some(_) => return Ok(false),
                None => {
                    if let Some(object::PropertyDescriptor {
                        kind: object::DescriptorKind::Accessor { setter, .. },
                        ..
                    }) = object::get_own_descriptor(
                        self.function_prototype_object()?,
                        &self.gc_heap,
                        &name,
                    ) {
                        let setter = setter.ok_or(VmError::TypeMismatch)?;
                        if !abstract_ops::is_callable(&setter) {
                            return Err(VmError::TypeMismatch);
                        }
                        let pc = stack[top_idx].pc;
                        stack[top_idx].pc = pc.checked_add(1).ok_or(VmError::InvalidOperand)?;
                        let mut args: SmallVec<[Value; 8]> = SmallVec::new();
                        args.push(value);
                        self.invoke(stack, context, &setter, receiver, args, scratch_reg)?;
                        return Ok(true);
                    }
                    if is_restricted_function_property(&name) {
                        let pc = stack[top_idx].pc;
                        stack[top_idx].pc = pc.checked_add(1).ok_or(VmError::InvalidOperand)?;
                        let callee = self.restricted_throw_type_error()?;
                        let mut args: SmallVec<[Value; 8]> = SmallVec::new();
                        args.push(value);
                        self.invoke(stack, context, &callee, receiver, args, scratch_reg)?;
                        return Ok(true);
                    }
                }
            }
        }
        if matches!(
            receiver,
            Value::Boolean(_)
                | Value::Number(_)
                | Value::String(_)
                | Value::Symbol(_)
                | Value::BigInt(_)
        ) {
            return self.store_to_primitive_base(
                stack,
                context,
                receiver,
                VmPropertyKey::atom(atomized_key),
                value,
                scratch_reg,
            );
        }
        let obj = match &receiver {
            Value::Object(o) => *o,
            Value::ClassConstructor(c) => c.statics(&self.gc_heap),
            Value::Function { function_id } | Value::Closure { function_id, .. } => {
                let fid = *function_id;
                if function_metadata::ordinary_function_metadata_key(&name).is_some()
                    && let Some(desc) =
                        self.ordinary_function_own_property_descriptor(Some(context), fid, &name)?
                    && !desc.writable()
                {
                    return Self::finish_failed_set(
                        stack,
                        context,
                        format!("Cannot assign to read-only property '{name}' of function"),
                    );
                }
                match self.function_user_props.get(&fid).copied() {
                    Some(bag) => bag,
                    None => {
                        self.function_user_bag_with_stack_roots(stack, fid, &[&receiver, &value])?
                    }
                }
            }
            _ => return Ok(false),
        };
        let outcome = crate::object::resolve_set(obj, &self.gc_heap, &name);
        match outcome {
            object::SetOutcome::AssignData => {
                let transition = if matches!(receiver, Value::Object(_))
                    && object::supports_fast_property_ic(obj, &self.gc_heap)
                {
                    self.capture_store_property_transition_with_stack_roots(
                        stack,
                        obj,
                        atomized_key,
                        &value,
                    )?
                } else {
                    None
                };
                if transition.is_none() && !self.ordinary_set_data_property(obj, &name, value)? {
                    return Self::finish_failed_set(
                        stack,
                        context,
                        format!("Cannot assign to property '{name}'"),
                    );
                }
                if matches!(receiver, Value::Object(_)) {
                    let site = context
                        .property_ic_site(stack[top_idx].function_id, stack[top_idx].pc)
                        .ok_or(VmError::InvalidOperand)?;
                    if !self.store_property_ics[site].is_disabled()
                        && object::supports_fast_property_ic(obj, &self.gc_heap)
                    {
                        if let Some(transition) = transition {
                            self.store_property_ics[site].install_with_stats(
                                &mut self.property_ic_stats,
                                PropertyIcKind::Store,
                                StorePropertyIc::transition(transition),
                            );
                        } else if let Some(ic) =
                            StorePropertyIc::existing_own_data_install_candidate(
                                obj,
                                &self.gc_heap,
                                atomized_key,
                            )
                        {
                            self.store_property_ics[site].install_with_stats(
                                &mut self.property_ic_stats,
                                PropertyIcKind::Store,
                                ic,
                            );
                        }
                    }
                }
                let pc = stack[top_idx].pc;
                stack[top_idx].pc = pc.checked_add(1).ok_or(VmError::InvalidOperand)?;
                Ok(true)
            }
            object::SetOutcome::InvokeSetter { setter } => {
                if !abstract_ops::is_callable(&setter) {
                    // Spec §10.1.9 step 5.b — accessor with non-
                    // callable setter rejects.
                    return Self::finish_failed_set(
                        stack,
                        context,
                        format!("Cannot assign to accessor property '{name}' without a setter"),
                    );
                }
                let pc = stack[top_idx].pc;
                stack[top_idx].pc = pc.checked_add(1).ok_or(VmError::InvalidOperand)?;
                let mut args: SmallVec<[Value; 8]> = SmallVec::new();
                args.push(value);
                self.invoke(stack, context, &setter, receiver, args, scratch_reg)?;
                Ok(true)
            }
            object::SetOutcome::Reject { .. } => Self::finish_failed_set(
                stack,
                context,
                format!("Cannot assign to property '{name}'"),
            ),
        }
    }

    /// §7.3.10 HasProperty — ordinary objects may have Proxy
    /// objects in their prototype chain, so the interpreter owns
    /// the trap-aware walk instead of delegating to `object::lookup`.
    pub(crate) fn drive_has_property_proxy(
        &mut self,
        stack: &mut SmallVec<[Frame; 8]>,
        context: &ExecutionContext,
        operands: &[Operand],
    ) -> Result<bool, VmError> {
        let dst = register_operand(operands.first())?;
        let lhs_reg = register_operand(operands.get(1))?;
        let rhs_reg = register_operand(operands.get(2))?;
        let top_idx = stack.len() - 1;
        let lhs = read_register(&stack[top_idx], lhs_reg)?.clone();
        let rhs = read_register(&stack[top_idx], rhs_reg)?.clone();
        if !matches!(rhs, Value::Object(_) | Value::Proxy(_)) {
            return Ok(false);
        };
        if let (Value::Object(obj), Value::String(key_string)) = (&rhs, &lhs) {
            let obj = *obj;
            let site = context
                .property_ic_site(stack[top_idx].function_id, stack[top_idx].pc)
                .ok_or(VmError::InvalidOperand)?;
            let mut site_disabled = self.has_property_ics[site].is_disabled();
            if let Some(ic) = self.has_property_ics[site].cached_ref() {
                if ic.probe(obj, &self.gc_heap, key_string).is_some() {
                    self.property_ic_stats.record_hit(PropertyIcKind::Has);
                    Self::finish_property_fast_path_value(
                        &mut stack[top_idx],
                        dst,
                        Value::Boolean(true),
                    )?;
                    return Ok(true);
                }
                self.has_property_ics[site]
                    .record_guard_miss_with_stats(&mut self.property_ic_stats, PropertyIcKind::Has);
                site_disabled = self.has_property_ics[site].is_disabled();
            } else {
                self.has_property_ics[site].record_uncached_miss_with_stats(
                    &mut self.property_ic_stats,
                    PropertyIcKind::Has,
                );
            }
            if !site_disabled
                && let Some(ic) = HasPropertyIc::install_candidate(obj, &self.gc_heap, key_string)
            {
                self.has_property_ics[site].install_with_stats(
                    &mut self.property_ic_stats,
                    PropertyIcKind::Has,
                    ic,
                );
                Self::finish_property_fast_path_value(
                    &mut stack[top_idx],
                    dst,
                    Value::Boolean(true),
                )?;
                return Ok(true);
            }
            self.has_property_ics[site]
                .disable_with_stats(&mut self.property_ic_stats, PropertyIcKind::Has);
        }
        let key = match &lhs {
            Value::Symbol(sym) => VmPropertyKey::Symbol(sym.clone()),
            Value::String(s) => VmPropertyKey::OwnedString(s.to_lossy_string()),
            other => VmPropertyKey::OwnedString(other.display_string()),
        };
        let pc = stack[top_idx].pc;
        stack[top_idx].pc = pc.checked_add(1).ok_or(VmError::InvalidOperand)?;
        let present = self.ordinary_has_property_value(context, rhs, &key, 0)?;
        write_register(&mut stack[top_idx], dst, Value::Boolean(present))?;
        Ok(true)
    }

    /// §28.2.4.10 Proxy.[[Delete]] — invoke the `deleteProperty`
    /// trap when the receiver of `delete obj.x` is a Proxy.
    pub(crate) fn drive_delete_property_proxy(
        &mut self,
        stack: &mut SmallVec<[Frame; 8]>,
        context: &ExecutionContext,
        operands: &[Operand],
    ) -> Result<bool, VmError> {
        let dst = register_operand(operands.first())?;
        let obj_reg = register_operand(operands.get(1))?;
        let name_idx = const_operand(operands.get(2))?;
        let atomized_key = context
            .property_atom(name_idx)
            .ok_or(VmError::InvalidOperand)?;
        let top_idx = stack.len() - 1;
        let receiver = read_register(&stack[top_idx], obj_reg)?.clone();
        let Value::Proxy(proxy) = receiver else {
            return Ok(false);
        };
        let pc = stack[top_idx].pc;
        stack[top_idx].pc = pc.checked_add(1).ok_or(VmError::InvalidOperand)?;
        let removed = self.ordinary_delete_value(
            context,
            Value::Proxy(proxy),
            &VmPropertyKey::atom(atomized_key),
            0,
        )?;
        write_register(&mut stack[top_idx], dst, Value::Boolean(removed))?;
        Ok(true)
    }

    /// §28.2.4.10 Proxy.[[Delete]] — computed delete uses the
    /// same trap-aware path as `delete obj.x`.
    pub(crate) fn drive_delete_element_proxy(
        &mut self,
        stack: &mut SmallVec<[Frame; 8]>,
        context: &ExecutionContext,
        operands: &[Operand],
    ) -> Result<bool, VmError> {
        let dst = register_operand(operands.first())?;
        let obj_reg = register_operand(operands.get(1))?;
        let idx_reg = register_operand(operands.get(2))?;
        let top_idx = stack.len() - 1;
        let receiver = read_register(&stack[top_idx], obj_reg)?.clone();
        if !matches!(receiver, Value::Proxy(_)) {
            return Ok(false);
        }
        let idx = read_register(&stack[top_idx], idx_reg)?.clone();
        let key = Self::coerce_vm_property_key(Some(&idx))?;
        let pc = stack[top_idx].pc;
        stack[top_idx].pc = pc.checked_add(1).ok_or(VmError::InvalidOperand)?;
        let removed = self.ordinary_delete_value(context, receiver, &key, 0)?;
        let strict = context.function_is_strict(stack[top_idx].function_id);
        if !removed && strict {
            return Err(VmError::TypeError {
                message: "Cannot delete property".to_string(),
            });
        }
        write_register(&mut stack[top_idx], dst, Value::Boolean(removed))?;
        Ok(true)
    }

    /// §28.2.4.1 Proxy.[[GetPrototypeOf]] — invoke the
    /// `getPrototypeOf` trap when the source is a Proxy.
    pub(crate) fn drive_get_prototype_proxy(
        &mut self,
        stack: &mut SmallVec<[Frame; 8]>,
        context: &ExecutionContext,
        operands: &[Operand],
    ) -> Result<bool, VmError> {
        let dst = register_operand(operands.first())?;
        let src = register_operand(operands.get(1))?;
        let top_idx = stack.len() - 1;
        let value = read_register(&stack[top_idx], src)?.clone();
        if !matches!(value, Value::Proxy(_)) {
            return Ok(false);
        };
        let pc = stack[top_idx].pc;
        stack[top_idx].pc = pc.checked_add(1).ok_or(VmError::InvalidOperand)?;
        let result = self.ordinary_get_prototype_value(context, value, 0)?;
        write_register(&mut stack[top_idx], dst, result)?;
        Ok(true)
    }

    /// §28.2.4.2 Proxy.[[SetPrototypeOf]] — invoke the
    /// `setPrototypeOf` trap when the receiver is a Proxy.
    pub(crate) fn drive_set_prototype_proxy(
        &mut self,
        stack: &mut SmallVec<[Frame; 8]>,
        context: &ExecutionContext,
        operands: &[Operand],
    ) -> Result<bool, VmError> {
        let obj_reg = register_operand(operands.first())?;
        let proto_reg = register_operand(operands.get(1))?;
        let top_idx = stack.len() - 1;
        let recv = read_register(&stack[top_idx], obj_reg)?.clone();
        let Value::Proxy(_) = &recv else {
            return Ok(false);
        };
        let proto_val = read_register(&stack[top_idx], proto_reg)?.clone();
        let proto_obj = match &proto_val {
            Value::Object(_) | Value::Proxy(_) | Value::Null => proto_val.clone(),
            Value::ClassConstructor(c) => Value::Object(c.statics(&self.gc_heap)),
            _ => return Err(VmError::TypeMismatch),
        };
        let pc = stack[top_idx].pc;
        stack[top_idx].pc = pc.checked_add(1).ok_or(VmError::InvalidOperand)?;
        // §10.5.7 — dispatch through the value-level helper so
        // nested proxies fall through correctly and §10.5.7 invariants
        // apply on the trap result.
        let ok = self.set_prototype_value_proxy_aware(context, &recv, &proto_obj)?;
        if !ok {
            // Object.setPrototypeOf throws when [[SetPrototypeOf]]
            // returns false (§20.1.2.21 step 4 DefinePropertyOrThrow).
            return Err(VmError::TypeError {
                message: "Object.setPrototypeOf failed".to_string(),
            });
        }
        Ok(true)
    }
}

fn string_index_property_name(key: &str) -> Option<u32> {
    if key.is_empty() {
        return None;
    }
    if key.len() > 1 && key.as_bytes().first() == Some(&b'0') {
        return None;
    }
    let value = key.parse::<u32>().ok()?;
    if value == u32::MAX {
        return None;
    }
    Some(value)
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
            _ => {
                crate::array::get_named_property(arr, &interpreter.gc_heap, &n.to_display_string())
                    .is_some()
            }
        },
        Value::String(s) => {
            let key = s.to_lossy_string();
            if key == "length" {
                return true;
            }
            if let Some(i) = crate::object::array_index_property_name(&key)
                && crate::array::has_own_element(arr, &interpreter.gc_heap, i as usize)
            {
                return true;
            }
            // §22.1.4 — Array exotic surface user-installed extra
            // string-keyed properties through the named-properties
            // side table. `in` must consult it before falling through.
            crate::array::get_named_property(arr, &interpreter.gc_heap, &key).is_some()
        }
        // §22.1 Array exotic — symbol-keyed own properties live in a
        // dedicated side table; surface them through the `in`
        // operator so reflective probes
        // (`Symbol.toStringTag in arr`,
        // `Symbol.iterator in arr`) observe the installed values.
        Value::Symbol(sym) => {
            crate::array::get_symbol_property(arr, &interpreter.gc_heap, sym).is_some()
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

/// §7.1.16 CanonicalNumericIndexString — `"-0"` maps to `-0`, any
/// string whose ToNumber round-trips back to the same string maps to
/// that number, otherwise undefined. Used by TypedArray and TypedArray
/// prototype walks to recognise integer-indexed exotic keys.
/// <https://tc39.es/ecma262/#sec-canonicalnumericindexstring>
pub(crate) fn canonical_numeric_index_string(s: &str) -> Option<f64> {
    if s == "-0" {
        return Some(-0.0);
    }
    let n: f64 = s.parse().ok()?;
    let formatted = crate::number::NumberValue::from_f64(n).to_display_string();
    if formatted == s { Some(n) } else { None }
}

/// Lazy-allocate (and cache) the TypedArray expando JsObject used
/// to back non-canonical-numeric own properties such as
/// `typedArr.constructor = X`.
fn typed_array_ensure_expando(
    interp: &mut Interpreter,
    t: &crate::binary::typed_array::JsTypedArray,
) -> Result<JsObject, VmError> {
    typed_array_ensure_expando_pub(&mut interp.gc_heap, t)
}

/// Public-crate variant of `typed_array_ensure_expando` so static
/// callers (e.g. `Object.defineProperty`) can lazily materialise
/// the bag without going through `Interpreter`.
pub(crate) fn typed_array_ensure_expando_pub(
    heap: &mut otter_gc::GcHeap,
    t: &crate::binary::typed_array::JsTypedArray,
) -> Result<JsObject, VmError> {
    if let Some(existing) = t.expando() {
        return Ok(existing);
    }
    let ta_root = Value::TypedArray(t.clone());
    let mut external_visit = |visitor: &mut dyn FnMut(*mut RawGc)| {
        ta_root.trace_value_slots(visitor);
    };
    let bag = crate::object::alloc_object_with_roots(heap, &mut external_visit)?;
    t.set_expando(bag);
    Ok(bag)
}

fn typed_array_set_expando(
    interp: &mut Interpreter,
    t: &crate::binary::typed_array::JsTypedArray,
    name: &str,
    value: Value,
) -> Result<(), VmError> {
    let bag = typed_array_ensure_expando(interp, t)?;
    interp.set_property(bag, name, value)?;
    Ok(())
}

/// Lazy-allocate (and cache) the RegExp expando JsObject used
/// to back non-spec own properties like `re.exec = fn`.
fn regexp_ensure_expando(
    interp: &mut Interpreter,
    r: &crate::regexp::JsRegExp,
    _receiver: &Value,
) -> Result<JsObject, VmError> {
    regexp_ensure_expando_pub(&mut interp.gc_heap, r)
}

/// Public-crate variant for `Object.defineProperty` callers.
pub(crate) fn regexp_ensure_expando_pub(
    heap: &mut otter_gc::GcHeap,
    r: &crate::regexp::JsRegExp,
) -> Result<JsObject, VmError> {
    if let Some(existing) = r.expando(heap) {
        return Ok(existing);
    }
    let recv = Value::RegExp(*r);
    let mut external_visit = |visitor: &mut dyn FnMut(*mut RawGc)| {
        recv.trace_value_slots(visitor);
    };
    let bag = crate::object::alloc_object_with_roots(heap, &mut external_visit)?;
    r.set_expando(heap, bag);
    Ok(bag)
}

/// Public-crate variant of the Promise expando lazy allocator.
pub(crate) fn promise_ensure_expando_pub(
    heap: &mut otter_gc::GcHeap,
    p: &crate::promise::JsPromiseHandle,
) -> Result<JsObject, VmError> {
    if let Some(existing) = p.expando(heap) {
        return Ok(existing);
    }
    let recv = Value::Promise(*p);
    let mut external_visit = |visitor: &mut dyn FnMut(*mut RawGc)| {
        recv.trace_value_slots(visitor);
    };
    let bag = crate::object::alloc_object_with_roots(heap, &mut external_visit)?;
    p.set_expando(heap, bag);
    Ok(bag)
}
