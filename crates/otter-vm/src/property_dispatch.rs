//! Property-related opcode helpers.
//!
//! The VM dispatch loop handles proxy or call-frame cases before entering the
//! dense register path. This module owns the remaining synchronous property
//! operations that can run directly against a frame.
//!
//! # Contents
//! - Legacy `instanceof` prototype-chain fallback.
//! - Synchronous `in` / `HasProperty` checks through the shared object resolver.
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

use crate::holt_stack::HoltStack;
use smallvec::SmallVec;

use otter_bytecode::{Op, Operand};
use otter_gc::raw::RawGc;

use crate::{
    ExecutionContext, Frame, Interpreter, JsObject, JsString, NumberValue, SuperReadKey, Value,
    VmError, VmGetOutcome, VmPropertyKey, abstract_ops,
    array::JsArray,
    binary, cache_ir, collections_prototype, descriptor_value, function_metadata,
    is_restricted_function_property, object,
    operand_decode::{const_operand, register_operand},
    property_atom::AtomizedPropertyKey,
    property_ic::PropertyIcKind,
    read_register, regexp_prototype, symbol, symbol_prototype, temporal, value_kind_name,
    write_register,
};

/// Resolution of an OrdinarySet for a callable's deleted `name` /
/// `length` along its `[[Prototype]]` (see
/// [`Interpreter::callable_metadata_proto_set`]).
enum MetadataProtoSet {
    /// An inherited non-writable data slot (or setter-less accessor) —
    /// the write is rejected.
    Reject,
    /// An inherited accessor with a setter — invoke it with the callable
    /// as receiver.
    InvokeSetter(Value),
    /// No inherited slot blocks the write — create the own property.
    Create,
}

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
                args.push(*value);
                self.run_callable_sync(context, &setter, Value::array(arr), args)?;
            }
            _ => {
                self.failed_set_result(
                    strict,
                    format!("Cannot assign to accessor property '{key}' without a setter"),
                )?;
            }
        }
        Ok(true)
    }

    fn capture_store_property_transition_with_stack_roots(
        &mut self,
        stack: &HoltStack,
        mut obj: JsObject,
        key: AtomizedPropertyKey<'_>,
        value: &Value,
    ) -> Result<Option<object::StorePropertyTransition>, VmError> {
        let parent = object::shape(obj, &self.gc_heap);
        if parent.is_null() || self.shape_offset_of(parent, key.name()).is_some() {
            return Ok(None);
        }
        // Normalize to dictionary storage past the fast-property cap:
        // returning `None` routes the caller to `ordinary_set_data_property`
        // (which sets `shape = null`), after which every further add sees
        // a null parent shape above and stays dictionary. This keeps bulk
        // property addition O(1) instead of growing an O(n) transition
        // chain that makes lookups — and therefore bulk addition — O(n²).
        if object::shape_property_count(parent, &self.gc_heap) >= object::MAX_FAST_PROPERTIES {
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
            .child_with_roots(
                &mut self.gc_heap,
                parent,
                key.name(),
                object::PropertyFlags::data_default(),
                false,
                &mut external_visit,
            )
            .map_err(VmError::from)?;
        Ok(object::capture_store_property_transition_with_shape(
            obj,
            &mut self.gc_heap,
            key,
            value,
            next_shape,
        ))
    }

    /// §7.1.19 `ToPropertyKey(value)` — primitives round through
    /// unchanged; non-primitives surface as a string (the ToString
    /// result) or a symbol (when `[Symbol.toPrimitive]` returns one).
    ///
    /// # See also
    /// - <https://tc39.es/ecma262/#sec-topropertykey>
    /// - <https://tc39.es/ecma262/#sec-toprimitive>
    pub(crate) fn coerce_property_key_value(
        &mut self,
        context: &ExecutionContext,
        value: Value,
    ) -> Result<Value, VmError> {
        // §7.1.19 ToPropertyKey — `String` / `Number` / `Symbol`
        // operands pass through to their existing per-receiver
        // arms unchanged; `Boolean` / `Null` / `Undefined` /
        // `BigInt` flatten to their display-string form so the
        // downstream match treats them as string keys.
        if value.is_string() || value.is_symbol() || value.is_number() {
            return Ok(value);
        }
        if let Some(b) = value.as_boolean() {
            let s = if b { "true" } else { "false" };
            let js = JsString::from_str(s, self.gc_heap_mut())?;
            return Ok(Value::string(js));
        }
        if value.is_null() {
            let js = JsString::null_str(self.gc_heap_mut())?;
            return Ok(Value::string(js));
        }
        if value.is_undefined() || value.is_hole() {
            let js = JsString::undefined_str(self.gc_heap_mut())?;
            return Ok(Value::string(js));
        }
        if let Some(b) = value.as_big_int() {
            let js = JsString::from_str(&b.to_decimal_string(&self.gc_heap), self.gc_heap_mut())?;
            return Ok(Value::string(js));
        }
        let key = self.to_property_key_sync(context, value)?;
        match key {
            VmPropertyKey::Symbol(sym) => Ok(Value::symbol(sym)),
            VmPropertyKey::Atom(atom) => {
                let s = JsString::from_str(atom.name(), self.gc_heap_mut())?;
                Ok(Value::string(s))
            }
            VmPropertyKey::String(s) => {
                let s = JsString::from_str(s, self.gc_heap_mut())?;
                Ok(Value::string(s))
            }
            VmPropertyKey::OwnedString(s) => {
                let s = JsString::from_str(&s, self.gc_heap_mut())?;
                Ok(Value::string(s))
            }
        }
    }

    fn load_string_primitive_property(
        &mut self,
        context: &ExecutionContext,
        receiver: &Value,
        string: JsString,
        name: &str,
    ) -> Result<Value, VmError> {
        match string_index_property_name(name) {
            Some(index) => match string.char_code_at(index, &self.gc_heap) {
                Some(unit) => Ok(Value::string(JsString::from_utf16_units(
                    &[unit],
                    &mut self.gc_heap,
                )?)),
                None => Ok(Value::undefined()),
            },
            None if name == "length" => Ok(Value::number_i32(string.len() as i32)),
            None => self.load_from_constructor_prototype(context, "String", receiver, name),
        }
    }

    fn function_user_bag_with_stack_roots(
        &mut self,
        stack: &HoltStack,
        owner: Option<crate::closure::JsClosure>,
        function_id: u32,
        value_roots: &[&Value],
    ) -> Result<JsObject, VmError> {
        if let Some(c) = owner {
            if let Some(bag) = c.own_props(&self.gc_heap) {
                return Ok(bag);
            }
            let bag = self.alloc_stack_rooted_object_with_extra_roots(stack, value_roots)?;
            c.set_own_props(&mut self.gc_heap, bag);
            return Ok(bag);
        }
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
        &mut self,
        frame: &mut Frame,
        dst: u16,
        lhs: u16,
        rhs: u16,
    ) -> Result<(), VmError> {
        let lhs = *read_register(frame, lhs)?;
        let rhs = *read_register(frame, rhs)?;
        let result = if let (Some(a), Some(target)) = (lhs.as_object(), rhs.as_object()) {
            match crate::object::get(target, &self.gc_heap, "prototype").and_then(|v| v.as_object())
            {
                Some(proto) => crate::object::has_in_proto_chain(a, &self.gc_heap, proto),
                None => crate::object::has_in_proto_chain(a, &self.gc_heap, target),
            }
        } else if let (Some(a), Some(c)) = (lhs.as_object(), rhs.as_class_constructor()) {
            crate::object::has_in_proto_chain(a, &self.gc_heap, c.prototype(&self.gc_heap))
        } else {
            false
        };
        write_register(frame, dst, Value::boolean(result))?;
        frame.advance_pc(self.current_byte_len)?;
        Ok(())
    }

    pub(crate) fn run_has_property_regs(
        &mut self,
        frame: &mut Frame,
        context: &crate::execution_context::ExecutionContext,
        dst: u16,
        lhs: u16,
        rhs: u16,
    ) -> Result<(), VmError> {
        let lhs = *read_register(frame, lhs)?;
        let rhs = *read_register(frame, rhs)?;
        let key_name = if let Some(s) = lhs.as_string(&self.gc_heap) {
            Some(s.to_lossy_string(&self.gc_heap))
        } else if let Some(n) = lhs.as_number() {
            Some(n.to_display_string())
        } else if let Some(b) = lhs.as_boolean() {
            Some(if b { "true" } else { "false" }.to_string())
        } else if lhs.is_null() {
            Some("null".to_string())
        } else if lhs.is_undefined() {
            Some("undefined".to_string())
        } else if let Some(b) = lhs.as_big_int() {
            Some(b.to_decimal_string(&self.gc_heap))
        } else {
            None
        };
        let present = if rhs.as_object().is_some() {
            // §13.10.1 `in` → HasProperty: one spec funnel for ordinary
            // objects, String exotics, and the prototype chain.
            let vm_key = if let Some(sym) = lhs.as_symbol(&self.gc_heap) {
                VmPropertyKey::Symbol(sym)
            } else if let Some(name) = key_name.as_deref() {
                VmPropertyKey::String(name)
            } else {
                return Err(VmError::TypeMismatch);
            };
            self.ordinary_has_property_value(context, rhs, &vm_key, 0)?
        } else if let Some(arr) = rhs.as_array() {
            // §13.10.1 `in` → HasProperty → OrdinaryHasProperty: own
            // elements / named props, then the Array.prototype chain
            // (inherited indices, `@@iterator`, …). The own-only
            // `has_array_property` is kept as the symbol-less fallback.
            if let Some(sym) = lhs.as_symbol(&self.gc_heap) {
                self.ordinary_has_property_value(context, rhs, &VmPropertyKey::Symbol(sym), 0)?
            } else if let Some(name) = key_name.as_deref() {
                self.ordinary_has_property_value(context, rhs, &VmPropertyKey::String(name), 0)?
            } else {
                has_array_property(self, arr, &lhs)
            }
        } else if rhs.is_object_type() {
            let key = if let Some(sym) = lhs.as_symbol(&self.gc_heap) {
                VmPropertyKey::Symbol(sym)
            } else if let Some(name) = key_name.as_deref() {
                VmPropertyKey::String(name)
            } else {
                return Err(VmError::TypeMismatch);
            };
            self.ordinary_has_property_value(context, rhs, &key, 0)?
        } else {
            return Err(VmError::TypeMismatch);
        };
        write_register(frame, dst, Value::boolean(present))?;
        frame.advance_pc(self.current_byte_len)?;
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
        let receiver = *read_register(frame, obj_reg)?;
        let removed = if let Some(o) = receiver.as_object() {
            // §10.4.6.10 [[Delete]] — a Module Namespace Exotic Object
            // refuses to delete an exported string name (strict module
            // code then throws TypeError below).
            if let Some(env) = crate::object::module_namespace_env(o, &self.gc_heap) {
                crate::object::get(env, &self.gc_heap, name).is_none()
            } else if let Some(key) =
                self.string_object_exotic_descriptor(o, &VmPropertyKey::String(name))?
            {
                // §10.4.3.6 — a String exotic object's own index /
                // length slots are non-configurable, so [[Delete]]
                // returns false (strict code then throws below); the
                // ordinary table is consulted for any other key.
                let _ = key;
                false
            } else {
                crate::object::delete(o, &mut self.gc_heap, name)
            }
        } else if let Some(arr) = receiver.as_array() {
            crate::array::delete_named_property(arr, &mut self.gc_heap, name)
        } else if let Some(class) = receiver.as_class_constructor() {
            let statics = class.statics(&self.gc_heap);
            if crate::object::get_own_descriptor(statics, &self.gc_heap, name).is_some() {
                crate::object::delete(statics, &mut self.gc_heap, name)
            } else if name == "prototype" {
                false
            } else if let Some(function_id) =
                class.ctor(&self.gc_heap).as_function().or_else(|| {
                    class
                        .ctor(&self.gc_heap)
                        .as_closure(&self.gc_heap)
                        .map(|c| c.cached_function_id)
                })
            {
                let owner = class.ctor(&self.gc_heap).as_closure(&self.gc_heap);
                self.ordinary_function_delete_own_property(owner, function_id, name)
            } else if let Some(native) = class.ctor(&self.gc_heap).as_native_function() {
                native.delete_own_property(&mut self.gc_heap, name)
            } else if let Some(bound) = class.ctor(&self.gc_heap).as_bound_function() {
                function_metadata::bound_delete_own_property(&bound, &mut self.gc_heap, name)
            } else {
                true
            }
        } else if let Some(function_id) = receiver.as_function().or_else(|| {
            receiver
                .as_closure(&self.gc_heap)
                .map(|c| c.cached_function_id)
        }) {
            let owner = receiver.as_closure(&self.gc_heap);
            self.ordinary_function_delete_own_property(owner, function_id, name)
        } else if let Some(native) = receiver.as_native_function() {
            native.delete_own_property(&mut self.gc_heap, name)
        } else if let Some(bound) = receiver.as_bound_function() {
            function_metadata::bound_delete_own_property(&bound, &mut self.gc_heap, name)
        } else if let Some(t) = receiver.as_typed_array(&self.gc_heap) {
            if let Some(n) = canonical_numeric_index_string(name) {
                // §10.4.5.10 [[Delete]] — a valid integer index is a
                // non-configurable element (false); anything else
                // deletes vacuously (true).
                typed_array_valid_index(&t, &self.gc_heap, n).is_none()
            } else if let Some(bag) = t.expando(&self.gc_heap) {
                crate::object::delete(bag, &mut self.gc_heap, name)
            } else {
                true
            }
        } else if let Some(promise) = receiver.as_promise() {
            if let Some(bag) = promise.expando(&self.gc_heap) {
                crate::object::delete(bag, &mut self.gc_heap, name)
            } else {
                true
            }
        } else if let Some(dv) = receiver.as_data_view() {
            if let Some(bag) = dv.expando(&self.gc_heap) {
                crate::object::delete(bag, &mut self.gc_heap, name)
            } else {
                true
            }
        } else if let Some(r) = receiver.as_regexp() {
            // §10.1.10 [[Delete]] — `lastIndex` is non-configurable; every
            // other own name lives in the lazy expando bag, and a missing
            // name deletes vacuously (returns `true`).
            if name == "lastIndex" {
                false
            } else if let Some(bag) = r.expando(&self.gc_heap) {
                crate::object::delete(bag, &mut self.gc_heap, name)
            } else {
                true
            }
        } else if let Some(t) = receiver.as_temporal(&self.gc_heap) {
            // Ordinary own properties live in the lazy expando; a missing
            // name deletes vacuously.
            if let Some(bag) = t.expando(&self.gc_heap) {
                crate::object::delete(bag, &mut self.gc_heap, name)
            } else {
                true
            }
        } else if receiver.is_map() || receiver.is_set() {
            // Ordinary own properties on a Map/Set live in the lazy
            // expando; a missing name deletes vacuously.
            if let Some(bag) = self.collection_expando(&receiver) {
                crate::object::delete(bag, &mut self.gc_heap, name)
            } else {
                true
            }
        } else {
            return Err(self.err_type(
                (format!(
                    "Cannot delete property '{name}' of {}",
                    value_kind_name(&receiver)
                ))
                .into(),
            ));
        };
        // §13.5.1.2 step 5.c — when the result of `[[Delete]]` is
        // `false` in strict mode, throw a TypeError.
        if !removed && strict {
            return Err(self.err_type((format!("Cannot delete property '{name}'")).into()));
        }
        write_register(frame, dst, Value::boolean(removed))?;
        frame.advance_pc(self.current_byte_len)?;
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
        let receiver = *read_register(frame, obj_reg)?;
        let idx = *read_register(frame, idx_reg)?;
        let removed = if let Some(obj) = receiver.as_object() {
            // §10.4.6.10 [[Delete]] — a Module Namespace Exotic Object
            // refuses to delete an exported string key (incl. integer
            // index names from arbitrary-module-namespace-names);
            // strict module code then throws TypeError below. Symbol
            // keys fall through to OrdinaryDelete.
            let namespace_env = crate::object::module_namespace_env(obj, &self.gc_heap);
            if let Some(sym) = idx.as_symbol(&self.gc_heap) {
                crate::object::delete_symbol(obj, &mut self.gc_heap, sym)
            } else if let Some(s) = idx.as_string(&self.gc_heap) {
                let name = s.to_lossy_string(&self.gc_heap);
                match namespace_env {
                    Some(env) => crate::object::get(env, &self.gc_heap, &name).is_none(),
                    None => crate::object::delete(obj, &mut self.gc_heap, &name),
                }
            } else if let Some(n) = idx.as_number() {
                let name = match n.as_smi() {
                    Some(v) if v >= 0 => v.to_string(),
                    _ => n.to_display_string(),
                };
                match namespace_env {
                    Some(env) => crate::object::get(env, &self.gc_heap, &name).is_none(),
                    None => crate::object::delete(obj, &mut self.gc_heap, &name),
                }
            } else {
                return Err(VmError::TypeMismatch);
            }
        } else if let Some(arr) = receiver.as_array() {
            if let Some(n) = idx.as_number() {
                match n.as_smi() {
                    Some(v) if v >= 0 => {
                        crate::array::delete_named_property(arr, &mut self.gc_heap, &v.to_string())
                    }
                    _ => crate::array::delete_named_property(
                        arr,
                        &mut self.gc_heap,
                        &n.to_display_string(),
                    ),
                }
            } else if let Some(s) = idx.as_string(&self.gc_heap) {
                let name = s.to_lossy_string(&self.gc_heap);
                crate::array::delete_named_property(arr, &mut self.gc_heap, &name)
            } else if let Some(sym) = idx.as_symbol(&self.gc_heap) {
                crate::array::delete_symbol_property(arr, &mut self.gc_heap, sym)
            } else {
                return Err(VmError::TypeMismatch);
            }
        } else if let Some(class) = receiver.as_class_constructor() {
            let statics = class.statics(&self.gc_heap);
            if let Some(sym) = idx.as_symbol(&self.gc_heap) {
                crate::object::delete_symbol(statics, &mut self.gc_heap, sym)
            } else if let Some(name) = idx
                .as_string(&self.gc_heap)
                .map(|s| s.to_lossy_string(&self.gc_heap))
                .or_else(|| idx.as_number().map(|n| n.to_display_string()))
            {
                if crate::object::get_own_descriptor(statics, &self.gc_heap, &name).is_some() {
                    crate::object::delete(statics, &mut self.gc_heap, &name)
                } else if name == "prototype" {
                    false
                } else if let Some(function_id) =
                    class.ctor(&self.gc_heap).as_function().or_else(|| {
                        class
                            .ctor(&self.gc_heap)
                            .as_closure(&self.gc_heap)
                            .map(|c| c.cached_function_id)
                    })
                {
                    let owner = class.ctor(&self.gc_heap).as_closure(&self.gc_heap);
                    self.ordinary_function_delete_own_property(owner, function_id, &name)
                } else if let Some(native) = class.ctor(&self.gc_heap).as_native_function() {
                    native.delete_own_property(&mut self.gc_heap, &name)
                } else if let Some(bound) = class.ctor(&self.gc_heap).as_bound_function() {
                    function_metadata::bound_delete_own_property(&bound, &mut self.gc_heap, &name)
                } else {
                    true
                }
            } else {
                return Err(VmError::TypeMismatch);
            }
        } else if let Some(s) = receiver.as_string(&self.gc_heap) {
            if let Some(n) = idx.as_number() {
                !matches!(n.as_smi(), Some(v) if v >= 0 && (v as u32) < s.len())
            } else {
                true
            }
        } else if let Some(function_id) = receiver.as_function().or_else(|| {
            receiver
                .as_closure(&self.gc_heap)
                .map(|c| c.cached_function_id)
        }) {
            if let Some(s) = idx.as_string(&self.gc_heap) {
                let name = s.to_lossy_string(&self.gc_heap);
                let owner = receiver.as_closure(&self.gc_heap);
                self.ordinary_function_delete_own_property(owner, function_id, &name)
            } else {
                return Err(VmError::TypeMismatch);
            }
        } else if let Some(native) = receiver.as_native_function() {
            if let Some(sym) = idx.as_symbol(&self.gc_heap) {
                native.delete_own_symbol_property(&mut self.gc_heap, sym)
            } else if let Some(s) = idx.as_string(&self.gc_heap) {
                let name = s.to_lossy_string(&self.gc_heap);
                native.delete_own_property(&mut self.gc_heap, &name)
            } else {
                return Err(VmError::TypeMismatch);
            }
        } else if let Some(bound) = receiver.as_bound_function() {
            if let Some(s) = idx.as_string(&self.gc_heap) {
                let name = s.to_lossy_string(&self.gc_heap);
                function_metadata::bound_delete_own_property(&bound, &mut self.gc_heap, &name)
            } else {
                return Err(VmError::TypeMismatch);
            }
        } else if let Some(t) = receiver.as_typed_array(&self.gc_heap) {
            if let Some(s) = idx.as_string(&self.gc_heap) {
                let name = s.to_lossy_string(&self.gc_heap);
                match canonical_numeric_index_string(&name) {
                    Some(n) => typed_array_valid_index(&t, &self.gc_heap, n).is_none(),
                    None => {
                        if let Some(bag) = t.expando(&self.gc_heap) {
                            crate::object::delete(bag, &mut self.gc_heap, &name)
                        } else {
                            true
                        }
                    }
                }
            } else if let Some(n) = idx.as_number() {
                // §7.1.19 ToPropertyKey ran on the VALUE -0 before
                // [[Delete]], so the key is the string "0" — a valid
                // index answers false (non-configurable element).
                let mut f = n.as_f64();
                if f == 0.0 {
                    f = 0.0;
                }
                t.buffer(&self.gc_heap).is_detached(&self.gc_heap)
                    || typed_array_valid_index(&t, &self.gc_heap, f).is_none()
            } else if let Some(sym) = idx.as_symbol(&self.gc_heap) {
                if let Some(bag) = t.expando(&self.gc_heap) {
                    crate::object::delete_symbol(bag, &mut self.gc_heap, sym)
                } else {
                    true
                }
            } else {
                return Err(VmError::TypeMismatch);
            }
        } else if let Some(r) = receiver.as_regexp() {
            // §10.1.10 [[Delete]] — only `lastIndex` (non-configurable) and
            // the lazy expando carry own names; any other key deletes
            // vacuously.
            if let Some(sym) = idx.as_symbol(&self.gc_heap) {
                if let Some(bag) = r.expando(&self.gc_heap) {
                    crate::object::delete_symbol(bag, &mut self.gc_heap, sym)
                } else {
                    true
                }
            } else if let Some(s) = idx.as_string(&self.gc_heap) {
                let name = s.to_lossy_string(&self.gc_heap);
                if name == "lastIndex" {
                    false
                } else if let Some(bag) = r.expando(&self.gc_heap) {
                    crate::object::delete(bag, &mut self.gc_heap, &name)
                } else {
                    true
                }
            } else if idx.as_number().is_some() {
                true
            } else {
                return Err(VmError::TypeMismatch);
            }
        } else if let Some(dv) = receiver.as_data_view() {
            // §25.3 — ordinary own properties live in the lazy expando;
            // a missing name deletes vacuously.
            if let Some(sym) = idx.as_symbol(&self.gc_heap) {
                match dv.expando(&self.gc_heap) {
                    Some(bag) => crate::object::delete_symbol(bag, &mut self.gc_heap, sym),
                    None => true,
                }
            } else if let Some(s) = idx.as_string(&self.gc_heap) {
                let name = s.to_lossy_string(&self.gc_heap);
                match dv.expando(&self.gc_heap) {
                    Some(bag) => crate::object::delete(bag, &mut self.gc_heap, &name),
                    None => true,
                }
            } else if idx.as_number().is_some() {
                true
            } else {
                return Err(VmError::TypeMismatch);
            }
        } else {
            return Err(VmError::TypeMismatch);
        };
        if !removed && strict {
            return Err(self.err_type(("Cannot delete property".to_string()).into()));
        }
        write_register(frame, dst, Value::boolean(removed))?;
        frame.advance_pc(self.current_byte_len)?;
        Ok(())
    }

    pub(crate) fn run_get_prototype_regs(
        &mut self,
        frame: &mut Frame,
        dst: u16,
        src: u16,
    ) -> Result<(), VmError> {
        let value = *read_register(frame, src)?;
        let result = self.get_prototype_for_op(&value)?;
        write_register(frame, dst, result)?;
        frame.advance_pc(self.current_byte_len)?;
        Ok(())
    }

    /// §13.3.5 MakeSuperPropertyReference + §13.3.4 GetValue for a
    /// `super.name` / `super[key]` read. The lookup base is the home
    /// object's prototype, but accessor getters run with the active
    /// frame's `this` as the receiver. The `this` binding must be
    /// initialized (else ReferenceError) and the super-base must be
    /// object-coercible (else TypeError).
    pub(crate) fn run_load_super_property(
        &mut self,
        context: &ExecutionContext,
        stack: &mut HoltStack,
        top_idx: usize,
        dst: u16,
        home: Value,
        key: SuperReadKey<'_>,
    ) -> Result<(), VmError> {
        // §13.3.7.1 — `GetThisBinding` then `GetSuperBase`, both
        // before any `ToPropertyKey` coercion on a computed key (a
        // key's `toString` must observe the pre-coercion super base).
        let actual_this = stack[top_idx].this_value;
        if actual_this.is_hole() {
            return Err(self.err_this_uninit(( "must call super constructor in derived class before accessing 'this' or returning from derived constructor".to_string()).into()));
        }
        let base = self.get_prototype_for_op(&home)?;
        if base.is_null() || base.is_undefined() {
            return Err(self.err_type(
                ("cannot read property of null or undefined super reference".to_string()).into(),
            ));
        }
        let key = match key {
            SuperReadKey::Resolved(k) => k,
            SuperReadKey::Computed(raw) => {
                let coerced = self.coerce_property_key_value(context, raw)?;
                if let Some(sym) = coerced.as_symbol(&self.gc_heap) {
                    VmPropertyKey::Symbol(sym)
                } else if let Some(s) = coerced.as_string(&self.gc_heap) {
                    VmPropertyKey::OwnedString(s.to_lossy_string(&self.gc_heap))
                } else if let Some(n) = coerced.as_number() {
                    VmPropertyKey::OwnedString(n.to_display_string())
                } else {
                    return Err(VmError::TypeMismatch);
                }
            }
        };
        let value = match self.ordinary_get_value(context, base, actual_this, &key, 0)? {
            VmGetOutcome::Value(v) => v,
            VmGetOutcome::InvokeGetter { getter } => {
                self.run_callable_sync(context, &getter, actual_this, SmallVec::new())?
            }
        };
        write_register(&mut stack[top_idx], dst, value)?;
        stack[top_idx].advance_pc(self.current_byte_len)?;
        Ok(())
    }

    /// §13.3.5 MakeSuperPropertyReference + §6.2.5.5 PutValue
    /// step 6.b for a `super.name = v` / `super[key] = v` write. The
    /// lookup base for a setter is the home object's prototype, but
    /// the setter (or own-data write) targets the active frame's
    /// `this`. `GetSuperBase` happens before any `ToPropertyKey`
    /// coercion of a computed key.
    pub(crate) fn run_store_super_property(
        &mut self,
        context: &ExecutionContext,
        stack: &mut HoltStack,
        top_idx: usize,
        home: Value,
        key: SuperReadKey<'_>,
        value: Value,
        strict: bool,
    ) -> Result<(), VmError> {
        let actual_this = stack[top_idx].this_value;
        if actual_this.is_hole() {
            return Err(self.err_this_uninit(( "must call super constructor in derived class before accessing 'this' or returning from derived constructor".to_string()).into()));
        }
        let base = self.get_prototype_for_op(&home)?;
        if base.is_null() || base.is_undefined() {
            return Err(self.err_type(
                ("cannot write property of null or undefined super reference".to_string()).into(),
            ));
        }
        let key = match key {
            SuperReadKey::Resolved(VmPropertyKey::String(s)) => s.to_string(),
            SuperReadKey::Resolved(k) => match k.string_name() {
                Some(s) => s.to_string(),
                None => return Err(VmError::TypeMismatch),
            },
            SuperReadKey::Computed(raw) => {
                let coerced = self.coerce_property_key_value(context, raw)?;
                if coerced.as_symbol(&self.gc_heap).is_some() {
                    // Symbol-keyed super writes are not yet exercised
                    // by the conformance subset; reject explicitly.
                    return Err(VmError::TypeMismatch);
                } else if let Some(s) = coerced.as_string(&self.gc_heap) {
                    s.to_lossy_string(&self.gc_heap)
                } else if let Some(n) = coerced.as_number() {
                    n.to_display_string()
                } else {
                    return Err(VmError::TypeMismatch);
                }
            }
        };
        let base_obj = base.as_object();
        self.ensure_deferred_namespace_ready(
            context,
            &base,
            !Self::deferred_key_is_symbol_like(&VmPropertyKey::String(&key)),
        )?;
        let outcome = match base_obj {
            Some(obj) => crate::object::resolve_set(obj, &self.gc_heap, &key),
            None => object::SetOutcome::AssignData,
        };
        match outcome {
            object::SetOutcome::InvokeSetter { setter } => {
                let mut args: SmallVec<[Value; 8]> = SmallVec::new();
                args.push(value);
                self.run_callable_sync(context, &setter, actual_this, args)?;
            }
            object::SetOutcome::Reject { .. } => {
                self.failed_set_result(
                    strict,
                    format!("Cannot assign to read-only property '{key}'"),
                )?;
            }
            object::SetOutcome::ExoticParent { parent } => {
                if !self.ordinary_set_data_value(
                    context,
                    parent,
                    &VmPropertyKey::String(&key),
                    value,
                    actual_this,
                    1,
                )? {
                    self.failed_set_result(strict, format!("Cannot assign to property '{key}'"))?;
                }
            }
            object::SetOutcome::AssignData => {
                // No setter on the super base — write an own data
                // property on the receiver (`this`).
                self.ensure_deferred_namespace_ready(
                    context,
                    &actual_this,
                    !Self::deferred_key_is_symbol_like(&VmPropertyKey::String(&key)),
                )?;
                if let Some(this_obj) = actual_this.as_object() {
                    // §10.1.9.2 OrdinarySetWithOwnDescriptor step 2.c —
                    // the data write consults `Receiver.[[GetOwnProperty]]`.
                    // For a module namespace receiver that lookup throws a
                    // TDZ ReferenceError when the target binding is still
                    // uninitialized (§10.4.6.5 step 7), which must surface
                    // before the namespace's non-writable rejection.
                    if object::module_namespace_env(this_obj, &self.gc_heap).is_some() {
                        self.ordinary_get_own_property_descriptor_value_runtime_rooted(
                            context,
                            actual_this,
                            &VmPropertyKey::String(&key),
                            0,
                            &[&actual_this],
                            &[],
                        )?;
                    }
                    if !self.ordinary_set_data_property(this_obj, &key, value)? {
                        self.failed_set_result(
                            strict,
                            format!("Cannot assign to read-only property '{key}'"),
                        )?;
                    }
                } else if let Some(c) = actual_this.as_class_constructor() {
                    // Static elements run with `this` = the class
                    // constructor; its own properties live on the
                    // statics object.
                    let statics = c.statics(&self.gc_heap);
                    if !self.ordinary_set_data_property(statics, &key, value)? {
                        self.failed_set_result(
                            strict,
                            format!("Cannot assign to read-only property '{key}'"),
                        )?;
                    }
                } else {
                    return Err(VmError::TypeMismatch);
                }
            }
        }
        stack[top_idx].advance_pc(self.current_byte_len)?;
        Ok(())
    }

    pub(crate) fn run_set_prototype_regs(
        &mut self,
        context: &ExecutionContext,
        frame: &mut Frame,
        obj_reg: u16,
        proto_reg: u16,
    ) -> Result<(), VmError> {
        let raw_proto = *read_register(frame, proto_reg)?;
        let proto = if raw_proto.is_object()
            || raw_proto.is_proxy()
            || raw_proto.is_iterator()
            || raw_proto.is_null()
        {
            raw_proto
        } else if let Some(c) = raw_proto.as_class_constructor() {
            Value::object(c.statics(&self.gc_heap))
        } else if raw_proto.is_native_function()
            || raw_proto.is_function()
            || raw_proto.is_closure()
            || raw_proto.is_bound_function()
        {
            // §15.7.14 ClassDefinitionEvaluation step 6.b — `class D
            // extends C` sets D.[[Prototype]] (the static side) to
            // the parent constructor C verbatim, so static methods on
            // the parent resolve through the ordinary [[Get]] ladder.
            // This holds whether C is a native constructor
            // (`Promise.reject`, `Map[@@species]`, …) or a plain
            // ECMAScript function used as a base class. Carry the
            // callable through — the prototype walker in
            // `ordinary_get_value` knows how to walk a callable
            // receiver.
            raw_proto
        } else {
            return Err(VmError::TypeMismatch);
        };
        let receiver = *read_register(frame, obj_reg)?;
        if receiver.is_object() {
            let ok = self.set_prototype_value_proxy_aware(context, &receiver, &proto)?;
            if !ok {
                return Err(self.err_type(("Object.setPrototypeOf failed".to_string()).into()));
            }
        } else if receiver.is_function()
            || receiver.is_closure()
            || receiver.is_bound_function()
            || receiver.is_native_function()
        {
            // no-op
        } else if receiver.is_boolean()
            || receiver.is_number()
            || receiver.is_string()
            || receiver.is_symbol()
            || receiver.is_big_int()
        {
            // §20.1.2.21 step 4 — `Object.setPrototypeOf(primitive,
            // proto)` returns the primitive unchanged after the
            // RequireObjectCoercible / proto-typecheck steps (which
            // already succeeded for `Boolean / Number / String /
            // Symbol / BigInt` because they are coercible). Mirror
            // V8 / JSC and skip the prototype write — the wrapper
            // would be unreachable.
        } else {
            return Err(VmError::TypeMismatch);
        }
        frame.advance_pc(self.current_byte_len)?;
        Ok(())
    }

    pub(crate) fn run_load_property_reg(
        &mut self,
        context: &ExecutionContext,
        stack: &mut HoltStack,
        top_idx: usize,
        dst: u16,
        obj_reg: u16,
        key: AtomizedPropertyKey<'_>,
    ) -> Result<(), VmError> {
        let name = key.name();
        let receiver = *read_register(&stack[top_idx], obj_reg)?;
        let value = if receiver.as_object().is_some() {
            let key = VmPropertyKey::String(name);
            match self.ordinary_get_value(context, receiver, receiver, &key, 0)? {
                VmGetOutcome::Value(value) => value,
                VmGetOutcome::InvokeGetter { getter } => {
                    self.run_callable_sync(context, &getter, receiver, SmallVec::new())?
                }
            }
        } else if let Some(c) = receiver.as_class_constructor() {
            if name == "prototype" {
                Value::object(c.prototype(&self.gc_heap))
            } else {
                let statics = c.statics(&self.gc_heap);
                // §10.2.* — `name` / `length` are own properties of the
                // class constructor (the class name and the constructor
                // parameter count), supplied by the backing ctor
                // function unless a static member shadows them. Resolve
                // them from the ctor BEFORE the inherited
                // %Function.prototype% walk, whose own `name`=""/
                // `length`=0 would otherwise shadow the real values.
                if (name == "name" || name == "length")
                    && object::get_own_descriptor(statics, &self.gc_heap, name).is_none()
                {
                    let ctor = c.ctor(&self.gc_heap);
                    if ctor.is_function()
                        || ctor.is_closure()
                        || ctor.is_native_function()
                        || ctor.is_bound_function()
                    {
                        let owner_bag = self.callable_bag_for_value(&ctor);
                        let mut ctx = function_metadata::FunctionMetadataContext::new(
                            context,
                            &mut self.gc_heap,
                            owner_bag,
                            &self.function_deleted_metadata,
                        );
                        function_metadata::callable_intrinsic_property(&mut ctx, &ctor, name)?
                    } else {
                        Value::undefined()
                    }
                } else if let Some(v) = crate::object::get(statics, &self.gc_heap, name) {
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
                        Some(p) if !(p.is_object() || p.is_null() || p.is_undefined()) => {
                            match self.ordinary_get_value(
                                context,
                                p,
                                receiver,
                                &VmPropertyKey::String(name),
                                0,
                            )? {
                                VmGetOutcome::Value(v) => Some(v),
                                VmGetOutcome::InvokeGetter { getter } => {
                                    Some(self.run_callable_sync(
                                        context,
                                        &getter,
                                        receiver,
                                        SmallVec::new(),
                                    )?)
                                }
                            }
                        }
                        _ => None,
                    };
                    walked
                        .filter(|v| !v.is_undefined())
                        .unwrap_or_else(Value::undefined)
                }
            }
        } else if let Some(s) = receiver.as_string(&self.gc_heap) {
            self.load_string_primitive_property(context, &receiver, s, name)?
        } else if receiver.is_array() {
            let v = &receiver;
            let a = v.as_array().unwrap();
            let direct = if let Some((getter, _setter)) =
                crate::array::get_accessor(a, &self.gc_heap, name)
            {
                match getter {
                    Some(getter) if abstract_ops::is_callable(&getter) => {
                        let args: SmallVec<[Value; 8]> = SmallVec::new();
                        Some(self.run_callable_sync(context, &getter, *v, args)?)
                    }
                    _ => Some(Value::undefined()),
                }
            } else {
                crate::array::get_named_property(a, &self.gc_heap, name)
            };
            match direct {
                Some(value) => value,
                // §10.4.2.4 — walk the array's *actual* [[Prototype]]: a
                // `class X extends Array` instance carries a per-instance
                // override (`X.prototype`), so inherited subclass
                // accessors / data properties resolve, not only
                // %Array.prototype%.
                None => match crate::array::prototype_override(a, &self.gc_heap) {
                    Some(proto) if proto.is_object_type() => {
                        match self.ordinary_get_value(
                            context,
                            proto,
                            *v,
                            &crate::VmPropertyKey::String(name),
                            0,
                        )? {
                            VmGetOutcome::Value(val) => val,
                            VmGetOutcome::InvokeGetter { getter } => {
                                self.run_callable_sync(context, &getter, *v, SmallVec::new())?
                            }
                        }
                    }
                    Some(_) => Value::undefined(),
                    None => self.load_from_constructor_prototype(context, "Array", v, name)?,
                },
            }
        } else if let Some(fid) = receiver.as_function().or_else(|| {
            receiver
                .as_closure(&self.gc_heap)
                .map(|c| c.cached_function_id)
        }) {
            let owner = receiver.as_closure(&self.gc_heap);
            self.function_property_get_stack_rooted_with_receiver(
                context,
                stack,
                owner,
                fid,
                Some(receiver),
                name,
            )?
        } else if let Some(native) = receiver.as_native_function() {
            match native.own_property_descriptor(&mut self.gc_heap, name)? {
                Some(desc) => match &desc.kind {
                    object::DescriptorKind::Data { value } => *value,
                    object::DescriptorKind::Accessor { getter, .. } => match getter {
                        Some(g) => {
                            let args: SmallVec<[Value; 8]> = SmallVec::new();
                            self.run_callable_sync(context, g, receiver, args)?
                        }
                        None => Value::undefined(),
                    },
                },
                // §10.1.8 — a native constructor with an explicit
                // [[Prototype]] (Int8Array → %TypedArray%) walks that
                // chain (inherited statics like `from` / `of`) before
                // the %Function.prototype% fallback.
                None => match native.prototype_override(&self.gc_heap) {
                    Some(parent) => {
                        let key = VmPropertyKey::String(name);
                        match self.ordinary_get_value(context, parent, receiver, &key, 0)? {
                            VmGetOutcome::Value(value) => value,
                            VmGetOutcome::InvokeGetter { getter } => {
                                self.run_callable_sync(context, &getter, receiver, SmallVec::new())?
                            }
                        }
                    }
                    None => {
                        if let Ok(proto) = self.function_prototype_object() {
                            let key = VmPropertyKey::String(name);
                            match self.ordinary_get_value(
                                context,
                                Value::object(proto),
                                receiver,
                                &key,
                                0,
                            )? {
                                VmGetOutcome::Value(value) => value,
                                VmGetOutcome::InvokeGetter { getter } => self.run_callable_sync(
                                    context,
                                    &getter,
                                    receiver,
                                    SmallVec::new(),
                                )?,
                            }
                        } else {
                            Value::undefined()
                        }
                    }
                },
            }
        } else if let Some(bound) = receiver.as_bound_function() {
            let bound = &bound;
            match function_metadata::bound_own_property_descriptor(bound, &mut self.gc_heap, name)?
            {
                Some(desc) => match &desc.kind {
                    object::DescriptorKind::Data { value } => *value,
                    object::DescriptorKind::Accessor { getter, .. } => match getter {
                        Some(g) if abstract_ops::is_callable(g) => {
                            self.run_callable_sync(context, g, receiver, SmallVec::new())?
                        }
                        _ => Value::undefined(),
                    },
                },
                None => self
                    .load_function_prototype_method(name)
                    .or_else(|| self.load_object_prototype_method(name))
                    .unwrap_or(Value::undefined()),
            }
        } else if receiver.as_regexp().is_some() {
            // §10.1.8 [[Get]] on a RegExp: route through the shared
            // ladder so an own expando member installed with an
            // accessor (`Object.defineProperty(re, "global", {get})`)
            // fires its getter rather than reading as `undefined`. The
            // ladder checks the expando, the struct flag fast path, and
            // the prototype chain in spec order.
            let key = VmPropertyKey::String(name);
            match self.ordinary_get_value(context, receiver, receiver, &key, 0)? {
                VmGetOutcome::Value(value) => value,
                VmGetOutcome::InvokeGetter { getter } => {
                    self.run_callable_sync(context, &getter, receiver, SmallVec::new())?
                }
            }
        } else if let Some(s) = receiver.as_symbol(&self.gc_heap) {
            symbol_prototype::load_property(s, name)
        } else if receiver.is_iterator() {
            // §27.1.5 — read string-keyed properties through
            // `Iterator.prototype` so the new spec-mandated
            // `next` / `return` / `throw` natives (and the helper
            // terminals like `map` / `forEach` / `toArray`) all
            // resolve uniformly via the realm prototype.
            self.load_from_constructor_prototype(context, "Iterator", &receiver, name)?
        } else if receiver.is_weak_ref() || receiver.is_finalization_registry() {
            let proto_name = if receiver.is_weak_ref() {
                "WeakRef"
            } else {
                "FinalizationRegistry"
            };
            self.load_from_constructor_prototype(context, proto_name, &receiver, name)?
        } else if let Some(p) = receiver.as_promise() {
            // §27.2.5 — user-installed own properties
            // (`promise.then = fn`) live in a lazy expando bag;
            // honour them before the prototype walk.
            if let Some(bag) = p.expando(&self.gc_heap)
                && let Some(value) = crate::object::get(bag, &self.gc_heap, name)
            {
                value
            } else {
                // §27.2.4.7.1 OrdinaryCreateFromConstructor —
                // when `new SubPromise(executor)` set
                // `prototype_override` to `SubPromise.prototype`,
                // walk *that* chain.
                let proto = match p.prototype_override(&self.gc_heap) {
                    Some(proto) => proto,
                    None => self.constructor_prototype_value("Promise")?,
                };
                if proto.is_nullish() {
                    Value::undefined()
                } else {
                    let key = VmPropertyKey::String(name);
                    match self.ordinary_get_value(context, proto, receiver, &key, 0)? {
                        VmGetOutcome::Value(value) => value,
                        VmGetOutcome::InvokeGetter { getter } => self.run_callable_sync(
                            context,
                            &getter,
                            receiver,
                            smallvec::SmallVec::new(),
                        )?,
                    }
                }
            }
        } else if receiver.is_map()
            || receiver.is_set()
            || receiver.is_weak_map()
            || receiver.is_weak_set()
        {
            // A user-assigned own property (`m.x = 5`,
            // `Object.defineProperty(m, …)`) lives in the lazy expando
            // and shadows the prototype methods.
            if let Some(bag) = self.collection_expando(&receiver)
                && let Some(outcome) = Self::expando_own_get_outcome(bag, &self.gc_heap, name)
            {
                match outcome {
                    VmGetOutcome::Value(v) => v,
                    VmGetOutcome::InvokeGetter { getter } => {
                        let args: SmallVec<[Value; 8]> = SmallVec::new();
                        self.run_callable_sync(context, &getter, receiver, args)?
                    }
                }
            } else {
                let direct =
                    collections_prototype::load_property_with_heap(&receiver, name, &self.gc_heap);
                if direct.is_undefined() {
                    let proto_name = if receiver.is_map() {
                        "Map"
                    } else if receiver.is_set() {
                        "Set"
                    } else if receiver.is_weak_map() {
                        "WeakMap"
                    } else {
                        "WeakSet"
                    };
                    self.load_from_constructor_prototype(context, proto_name, &receiver, name)?
                } else {
                    direct
                }
            }
        } else if let Some(t) = receiver.as_temporal(&self.gc_heap) {
            // An ordinary own property (installed via defineProperty /
            // assignment) lives in the expando and shadows the prototype
            // accessor that `load_property` resolves from internal slots.
            if let Some(bag) = t.expando(&self.gc_heap)
                && let Some(outcome) = Self::expando_own_get_outcome(bag, &self.gc_heap, name)
            {
                match outcome {
                    VmGetOutcome::Value(v) => v,
                    VmGetOutcome::InvokeGetter { getter } => {
                        let args: SmallVec<[Value; 8]> = SmallVec::new();
                        self.run_callable_sync(context, &getter, receiver, args)?
                    }
                }
            } else {
                temporal::load_property(t, &mut self.gc_heap, name)
            }
        } else if let Some(b) = receiver.as_array_buffer() {
            // Own expando bag (species `constructor` override, or a
            // cross-brand accessor installed via defineProperty) wins
            // over the data shortcuts and the prototype walk. An own
            // accessor fires with the buffer as receiver.
            if let Some(bag) = b.expando(&self.gc_heap)
                && let Some(outcome) = Self::expando_own_get_outcome(bag, &self.gc_heap, name)
            {
                match outcome {
                    VmGetOutcome::Value(v) => v,
                    VmGetOutcome::InvokeGetter { getter } => {
                        let args: SmallVec<[Value; 8]> = SmallVec::new();
                        self.run_callable_sync(context, &getter, receiver, args)?
                    }
                }
            } else {
                let direct = binary::array_buffer_prototype::load_property(b, &self.gc_heap, name);
                if direct.is_undefined() {
                    let proto_name = if b.is_shared() {
                        "SharedArrayBuffer"
                    } else {
                        "ArrayBuffer"
                    };
                    self.load_from_constructor_prototype(context, proto_name, &receiver, name)?
                } else {
                    direct
                }
            }
        } else if let Some(dv) = receiver.as_data_view() {
            // §25.3 — a `DataView` is an ordinary object; user-installed
            // own properties (`dv.x = 1`, or an own accessor) live in the
            // lazy expando bag and win over the prototype walk.
            if let Some(bag) = dv.expando(&self.gc_heap)
                && let Some(outcome) = Self::expando_own_get_outcome(bag, &self.gc_heap, name)
            {
                match outcome {
                    VmGetOutcome::Value(v) => v,
                    VmGetOutcome::InvokeGetter { getter } => {
                        let args: SmallVec<[Value; 8]> = SmallVec::new();
                        self.run_callable_sync(context, &getter, receiver, args)?
                    }
                }
            } else {
                let direct = binary::data_view_prototype::load_property(&dv, &self.gc_heap, name);
                if direct.is_undefined() {
                    self.load_from_constructor_prototype(context, "DataView", &receiver, name)?
                } else {
                    direct
                }
            }
        } else if let Some(t) = receiver.as_typed_array(&self.gc_heap) {
            // §10.4.5.4 [[Get]] — a canonical numeric index never
            // consults the expando bag or the prototype chain: it is
            // the element value, or `undefined` when invalid
            // (out-of-bounds, fractional, `-0`, detached buffer).
            if let Some(n) = canonical_numeric_index_string(name) {
                match typed_array_valid_index(&t, &self.gc_heap, n) {
                    Some(idx) => t.get(&mut self.gc_heap, idx).map_err(crate::oom_to_vm)?,
                    None => Value::undefined(),
                }
            } else if let Some(bag) = t.expando(&self.gc_heap)
                && let Some(outcome) = Self::expando_own_get_outcome(bag, &self.gc_heap, name)
            {
                match outcome {
                    VmGetOutcome::Value(v) => v,
                    VmGetOutcome::InvokeGetter { getter } => {
                        let args: SmallVec<[Value; 8]> = SmallVec::new();
                        self.run_callable_sync(context, &getter, receiver, args)?
                    }
                }
            } else {
                let direct = binary::typed_array_prototype::load_property(&t, &self.gc_heap, name);
                if direct.is_undefined() {
                    // §10.4.5.4 walks the instance's actual [[Prototype]]
                    // (a subclass `X.prototype`), not the kind default,
                    // so `O.constructor` / user prototype props resolve
                    // against the real chain.
                    let proto = self.get_prototype_for_op(&receiver)?;
                    match proto.as_object() {
                        Some(proto_obj) => {
                            let key = VmPropertyKey::String(name);
                            match self.ordinary_get_value(
                                context,
                                Value::object(proto_obj),
                                receiver,
                                &key,
                                0,
                            )? {
                                VmGetOutcome::Value(v) => v,
                                VmGetOutcome::InvokeGetter { getter } => self.run_callable_sync(
                                    context,
                                    &getter,
                                    receiver,
                                    smallvec::SmallVec::new(),
                                )?,
                            }
                        }
                        None => Value::undefined(),
                    }
                } else {
                    direct
                }
            }
        } else if receiver.is_big_int() {
            self.load_from_constructor_prototype(context, "BigInt", &receiver, name)?
        } else if receiver.is_intl() {
            // ECMA-402: methods resolve through `Intl.<Kind>.prototype`;
            // `ordinary_get_value` walks the kind prototype.
            let key = VmPropertyKey::String(name);
            match self.ordinary_get_value(context, receiver, receiver, &key, 0)? {
                VmGetOutcome::Value(v) => v,
                VmGetOutcome::InvokeGetter { getter } => {
                    self.run_callable_sync(context, &getter, receiver, smallvec::SmallVec::new())?
                }
            }
        } else {
            // Proxy and any other receiver not special-cased above resolve
            // through the generic, proxy-aware value-level `[[Get]]` funnel.
            // The interpreter opcode reaches this via `drive_load_property`'s
            // proxy pre-handling; the JIT bridge calls here directly, so this
            // fallback must cover proxies too.
            let key = VmPropertyKey::String(name);
            match self.ordinary_get_value(context, receiver, receiver, &key, 0)? {
                VmGetOutcome::Value(v) => v,
                VmGetOutcome::InvokeGetter { getter } => {
                    self.run_callable_sync(context, &getter, receiver, SmallVec::new())?
                }
            }
        };
        let frame = &mut stack[top_idx];
        write_register(frame, dst, value)?;
        frame.advance_pc(self.current_byte_len)?;
        Ok(())
    }

    /// §10.1.9.2 — continue OrdinarySet for a callable whose virtual
    /// `name` / `length` own slot is absent (it was deleted), resolving
    /// the write along the callable's `[[Prototype]]`. The default
    /// `%Function.prototype%` carries both as non-writable data
    /// properties (held as native-function metadata, not a plain object
    /// slot), so the descriptor walk uses the value-aware getter rather
    /// than `resolve_set`. Without this, a deleted `name` / `length`
    /// would be silently re-created as an own data property, masking the
    /// inherited non-writable slot.
    fn callable_metadata_proto_set(
        &mut self,
        context: &ExecutionContext,
        receiver: Value,
        name: &str,
    ) -> Result<MetadataProtoSet, VmError> {
        let key = VmPropertyKey::String(name);
        let mut current = self.get_prototype_for_op(&receiver)?;
        let mut hops = 0usize;
        while hops < object::PROTO_CHAIN_HARD_CAP {
            if current.is_null() || current.is_undefined() {
                return Ok(MetadataProtoSet::Create);
            }
            let desc = self.ordinary_get_own_property_descriptor_value_runtime_rooted(
                context,
                current,
                &key,
                0,
                &[&receiver, &current],
                &[],
            )?;
            match desc {
                Some(d) => {
                    return Ok(match &d.kind {
                        object::DescriptorKind::Accessor { setter, .. } => match setter {
                            Some(s) => MetadataProtoSet::InvokeSetter(*s),
                            None => MetadataProtoSet::Reject,
                        },
                        object::DescriptorKind::Data { .. } => {
                            if d.writable() {
                                MetadataProtoSet::Create
                            } else {
                                MetadataProtoSet::Reject
                            }
                        }
                    });
                }
                None => {
                    current = self.get_prototype_for_op(&current)?;
                    hops += 1;
                }
            }
        }
        Ok(MetadataProtoSet::Create)
    }

    pub(crate) fn run_store_property_reg(
        &mut self,
        context: &ExecutionContext,
        stack: &mut HoltStack,
        top_idx: usize,
        obj_reg: u16,
        key: AtomizedPropertyKey<'_>,
        src: u16,
    ) -> Result<(), VmError> {
        let name = key.name();
        let frame = &stack[top_idx];
        let value = *read_register(frame, src)?;
        let strict = context.function_is_strict(frame.function_id);
        let receiver = *read_register(frame, obj_reg)?;
        if let Some(o) = receiver.as_object()
            && object::deferred_namespace_target(o, &self.gc_heap).is_some()
        {
            self.ensure_deferred_namespace_ready(context, &receiver, true)?;
            if !self.ordinary_set_data_property(o, name, value)? {
                self.failed_set_result(
                    strict,
                    format!("Cannot assign to read-only property '{name}'"),
                )?;
            }
            stack[top_idx].advance_pc(self.current_byte_len)?;
            return Ok(());
        }
        let target = if let Some(o) = receiver.as_object() {
            Some(o)
        } else if let Some(c) = receiver.as_class_constructor() {
            if self.class_store_hits_readonly_intrinsic(context, c, name)? {
                self.failed_set_result(
                    strict,
                    format!("Cannot assign to read-only property '{name}' of class"),
                )?;
                stack[top_idx].advance_pc(self.current_byte_len)?;
                return Ok(());
            }
            Some(c.statics(&self.gc_heap))
        } else if let Some(r) = receiver.as_regexp() {
            // `lastIndex` lives in the body slot; every other
            // named write lands in the lazy expando bag so
            // `re.global = false` / `re.exec = fn` survive
            // observability checks.
            if name == "lastIndex" {
                regexp_prototype::store_property(&r, &mut self.gc_heap, name, value);
                None
            } else {
                let absent = r.expando(&self.gc_heap).is_none_or(|bag| {
                    matches!(
                        object::lookup_own(bag, &self.gc_heap, name),
                        object::PropertyLookup::Absent
                    )
                });
                if absent {
                    // §10.1.9.2 OrdinarySet — `lastIndex` is the regexp's
                    // only own data slot, so any other write that has no own
                    // shadow must consult the prototype chain first: an
                    // inherited getter-only accessor (`global`, `source`, …)
                    // rejects the write rather than installing an own slot.
                    let proto = self.get_prototype_for_op(&receiver)?;
                    if let Some(proto_obj) = proto.as_object() {
                        match object::resolve_set(proto_obj, &self.gc_heap, name) {
                            object::SetOutcome::InvokeSetter { setter } => {
                                let mut args: SmallVec<[Value; 8]> = SmallVec::new();
                                args.push(value);
                                self.run_callable_sync(context, &setter, receiver, args)?;
                                stack[top_idx].advance_pc(self.current_byte_len)?;
                                return Ok(());
                            }
                            object::SetOutcome::Reject { .. } => {
                                self.failed_set_result(
                                    strict,
                                    format!("Cannot assign to read-only property '{name}'"),
                                )?;
                                stack[top_idx].advance_pc(self.current_byte_len)?;
                                return Ok(());
                            }
                            object::SetOutcome::AssignData => {}
                            // %RegExp.prototype% chains stay ordinary;
                            // an exotic link falls back to the own-slot
                            // install below (pre-existing behaviour).
                            object::SetOutcome::ExoticParent { .. } => {}
                        }
                    }
                    if !r.is_extensible(&self.gc_heap) {
                        self.failed_set_result(
                            strict,
                            format!("Cannot add property '{name}' to non-extensible RegExp"),
                        )?;
                        None
                    } else {
                        let bag = regexp_ensure_expando(self, &r, &receiver)?;
                        self.ordinary_set_data_property(bag, name, value)?;
                        None
                    }
                } else {
                    let bag = regexp_ensure_expando(self, &r, &receiver)?;
                    if !self.ordinary_set_data_property(bag, name, value)? {
                        self.failed_set_result(
                            strict,
                            format!("Cannot assign to property '{name}'"),
                        )?;
                    }
                    None
                }
            }
        } else if let Some(a) = receiver.as_array() {
            // §10.4.2.4 ArraySetLength — `arr.length = v` is a
            // [[DefineOwnProperty]] of "length", so ToUint32(v) must
            // equal ToNumber(v) (else RangeError) and the value's
            // `valueOf` runs. Route it through the shared define path
            // rather than the lenient named-property store.
            if name == "length" {
                let descriptor = object::PartialPropertyDescriptor {
                    value: Some(value),
                    ..Default::default()
                };
                let ok = self.define_own_property_value(
                    context,
                    &receiver,
                    &crate::VmPropertyKey::String("length"),
                    descriptor,
                )?;
                if !ok {
                    self.failed_set_result(
                        strict,
                        "Cannot assign to read only property 'length' of array".to_string(),
                    )?;
                }
                stack[top_idx].advance_pc(self.current_byte_len)?;
                return Ok(());
            }
            if !self.store_array_accessor_property(context, a, name, &value, strict)? {
                let has_own_named =
                    crate::array::get_named_property(a, &self.gc_heap, name).is_some();
                if !has_own_named {
                    let proto = self.constructor_prototype_value("Array")?;
                    if let Some(proto) = proto.as_object() {
                        match crate::object::resolve_set(proto, &self.gc_heap, name) {
                            object::SetOutcome::InvokeSetter { setter } => {
                                let mut args: SmallVec<[Value; 8]> = SmallVec::new();
                                args.push(value);
                                self.run_callable_sync(context, &setter, Value::array(a), args)?;
                                stack[top_idx].advance_pc(self.current_byte_len)?;
                                return Ok(());
                            }
                            object::SetOutcome::Reject { .. } => {
                                self.failed_set_result(
                                    strict,
                                    format!("Cannot assign to property '{name}'"),
                                )?;
                                stack[top_idx].advance_pc(self.current_byte_len)?;
                                return Ok(());
                            }
                            object::SetOutcome::AssignData => {}
                            // %Array.prototype% chains stay ordinary.
                            object::SetOutcome::ExoticParent { .. } => {}
                        }
                    }
                }
                crate::array::set_named_property(a, &mut self.gc_heap, name, value)?;
            }
            None
        } else if let Some(t) = receiver.as_typed_array(&self.gc_heap) {
            if let Some(n) = canonical_numeric_index_string(name) {
                // §10.4.5.16 step 2 — convert the value (firing its
                // coercion, throwing for a Symbol / cross-type) before
                // the index check, so side effects run even for an
                // out-of-bounds write, which then discards the result.
                let converted = self.typed_array_coerce_element(context, t.kind(), value)?;
                if !t.buffer(&self.gc_heap).is_detached(&self.gc_heap)
                    && n.is_finite()
                    && n.fract() == 0.0
                    && n >= 0.0
                    && (n as usize) < t.length(&self.gc_heap)
                {
                    t.set(&mut self.gc_heap, n as usize, &converted);
                }
            } else {
                // §10.1.9 — non-numeric keys run the full [[Set]]
                // funnel: own expando, then the prototype chain
                // (accessors on %TypedArray.prototype% must fire),
                // receiver-phase define on a fully-absent chain.
                let vm_key = VmPropertyKey::OwnedString(name.to_string());
                self.ordinary_set_data_value(context, receiver, &vm_key, value, receiver, 0)?;
            }
            None
        } else if let Some(fid) = receiver.as_function().or_else(|| {
            receiver
                .as_closure(&self.gc_heap)
                .map(|c| c.cached_function_id)
        }) {
            let owner = receiver.as_closure(&self.gc_heap);
            let has_own = self.ordinary_function_has_own_string_property_for_extensibility(
                context, owner, fid, name,
            )?;
            if matches!(name, "name" | "length") {
                let own = self.ordinary_function_own_property_descriptor(
                    Some(context),
                    owner,
                    fid,
                    name,
                )?;
                match own {
                    Some(desc) if !desc.writable() => {
                        self.failed_set_result(
                            strict,
                            format!("Cannot assign to read-only property '{name}' of function"),
                        )?;
                        None
                    }
                    Some(_) => {
                        // Own writable metadata (made writable via
                        // defineProperty): overwrite in place.
                        let bag = self.function_user_bag_with_stack_roots(
                            stack,
                            owner,
                            fid,
                            &[&receiver, &value],
                        )?;
                        Some(bag)
                    }
                    None => match self.callable_metadata_proto_set(context, receiver, name)? {
                        MetadataProtoSet::Reject => {
                            self.failed_set_result(
                                strict,
                                format!("Cannot assign to read-only property '{name}' of function"),
                            )?;
                            None
                        }
                        MetadataProtoSet::InvokeSetter(setter) => {
                            self.run_callable_sync(
                                context,
                                &setter,
                                receiver,
                                smallvec::smallvec![value],
                            )?;
                            None
                        }
                        MetadataProtoSet::Create => {
                            let bag = self.function_user_bag_with_stack_roots(
                                stack,
                                owner,
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
                    },
                }
            } else if !has_own && !self.ordinary_function_is_extensible(fid) {
                self.failed_set_result(
                    strict,
                    format!("Cannot add property '{name}' to non-extensible function"),
                )?;
                None
            } else {
                let bag = self.function_user_bag_with_stack_roots(
                    stack,
                    owner,
                    fid,
                    &[&receiver, &value],
                )?;
                Some(bag)
            }
        } else if let Some(native) = receiver.as_native_function() {
            match native.own_property_descriptor(&mut self.gc_heap, name)? {
                Some(desc) if !desc.writable() => {
                    self.failed_set_result(
                        strict,
                        format!(
                            "Cannot assign to read-only property '{name}' of function {}",
                            native.name(&self.gc_heap)
                        ),
                    )?;
                    None
                }
                // No own slot for `name`/`length` means it was deleted; the
                // inherited %Function.prototype% slot is non-writable, so
                // resolve the write along the [[Prototype]] rather than
                // silently re-creating an own data property.
                None if matches!(name, "name" | "length") => {
                    match self.callable_metadata_proto_set(context, receiver, name)? {
                        MetadataProtoSet::Reject => {
                            self.failed_set_result(
                                strict,
                                format!(
                                    "Cannot assign to read-only property '{name}' of function {}",
                                    native.name(&self.gc_heap)
                                ),
                            )?;
                        }
                        MetadataProtoSet::InvokeSetter(setter) => {
                            self.run_callable_sync(
                                context,
                                &setter,
                                receiver,
                                smallvec::smallvec![value],
                            )?;
                        }
                        MetadataProtoSet::Create => {
                            let desc = object::PropertyDescriptor::data(value, true, false, true);
                            if !native.define_own_property(&mut self.gc_heap, name, desc) {
                                self.failed_set_result(
                                    strict,
                                    format!(
                                        "Cannot define property '{name}' on function {}",
                                        native.name(&self.gc_heap)
                                    ),
                                )?;
                            }
                        }
                    }
                    None
                }
                _ => {
                    let enumerable =
                        function_metadata::ordinary_function_metadata_key(name).is_none();
                    let desc = object::PropertyDescriptor::data(value, true, enumerable, true);
                    if !native.define_own_property(&mut self.gc_heap, name, desc) {
                        self.failed_set_result(
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
        } else if let Some(bound) = receiver.as_bound_function() {
            let bound = &bound;
            match function_metadata::bound_own_property_descriptor(bound, &mut self.gc_heap, name)?
            {
                Some(desc) if !desc.writable() => {
                    self.failed_set_result(
                        strict,
                        format!("Cannot assign to read-only property '{name}' of bound function"),
                    )?;
                    None
                }
                // Deleted `name`/`length`: resolve along [[Prototype]]
                // (%Function.prototype% rejects the non-writable slot)
                // instead of re-creating an own data property.
                None if matches!(name, "name" | "length") => {
                    match self.callable_metadata_proto_set(context, receiver, name)? {
                        MetadataProtoSet::Reject => {
                            self.failed_set_result(
                                strict,
                                format!(
                                    "Cannot assign to read-only property '{name}' of bound function"
                                ),
                            )?;
                        }
                        MetadataProtoSet::InvokeSetter(setter) => {
                            self.run_callable_sync(
                                context,
                                &setter,
                                receiver,
                                smallvec::smallvec![value],
                            )?;
                        }
                        MetadataProtoSet::Create => {
                            let desc = object::PropertyDescriptor::data(value, true, true, true);
                            if !function_metadata::bound_define_own_property(
                                bound,
                                &mut self.gc_heap,
                                name,
                                desc,
                            ) {
                                self.failed_set_result(
                                    strict,
                                    format!("Cannot define property '{name}' on bound function"),
                                )?;
                            }
                        }
                    }
                    None
                }
                _ => {
                    let desc = object::PropertyDescriptor::data(value, true, true, true);
                    if !function_metadata::bound_define_own_property(
                        bound,
                        &mut self.gc_heap,
                        name,
                        desc,
                    ) {
                        self.failed_set_result(
                            strict,
                            format!("Cannot define property '{name}' on bound function"),
                        )?;
                    }
                    None
                }
            }
        } else if let Some(p) = receiver.as_promise() {
            let bag = if let Some(bag) = p.expando(&self.gc_heap) {
                bag
            } else {
                let p_value = receiver;
                let mut external_visit = |visitor: &mut dyn FnMut(*mut RawGc)| {
                    p_value.trace_value_slots(visitor);
                };
                let bag =
                    crate::object::alloc_object_with_roots(&mut self.gc_heap, &mut external_visit)?;
                p.set_expando(&mut self.gc_heap, bag);
                bag
            };
            Some(bag)
        } else if let Some(b) = receiver.as_array_buffer() {
            // §25.1 / §25.2 — ArrayBuffer and SharedArrayBuffer are
            // ordinary objects; own properties (e.g. a species
            // `constructor` override) land in the lazy expando bag.
            Some(array_buffer_ensure_expando_pub(&mut self.gc_heap, &b)?)
        } else if let Some(dv) = receiver.as_data_view() {
            // §25.3 — ordinary own properties land in the lazy expando.
            Some(data_view_ensure_expando_pub(&mut self.gc_heap, &dv)?)
        } else if receiver.is_temporal() {
            // §10.1.9 OrdinarySet — own expando first, then the
            // prototype chain (a getter-only accessor like `year`
            // rejects the write), receiver-phase define otherwise.
            let vm_key = VmPropertyKey::OwnedString(name.to_string());
            if !self.ordinary_set_data_value(context, receiver, &vm_key, value, receiver, 0)? {
                self.failed_set_result(
                    strict,
                    format!("Cannot assign to read-only property '{name}'"),
                )?;
            }
            None
        } else if receiver.is_map() || receiver.is_set() {
            // §10.1.9 OrdinarySet — a user-assigned own property
            // (`m.x = 5`) lands in the lazy expando; the prototype
            // walk first lets a getter-only accessor (`size`) reject
            // the write.
            let vm_key = VmPropertyKey::OwnedString(name.to_string());
            if !self.ordinary_set_data_value(context, receiver, &vm_key, value, receiver, 0)? {
                self.failed_set_result(
                    strict,
                    format!("Cannot assign to read-only property '{name}'"),
                )?;
            }
            None
        } else if receiver.is_undefined() || receiver.is_null() || receiver.is_hole() {
            return Err(self.err_type(
                (format!(
                    "Cannot set property '{name}' on {}",
                    value_kind_name(&receiver)
                ))
                .into(),
            ));
        } else if receiver.is_boolean()
            || receiver.is_number()
            || receiver.is_string()
            || receiver.is_symbol()
            || receiver.is_big_int()
        {
            self.failed_set_result(
                strict,
                format!(
                    "Cannot set property '{name}' on {}",
                    value_kind_name(&receiver)
                ),
            )?;
            None
        } else {
            // §10.1.9.2 OrdinarySetWithOwnDescriptor — for
            // exotic receivers without their own [[Set]] (Map,
            // Set, WeakMap, WeakSet, WeakRef,
            // FinalizationRegistry, ArrayBuffer,
            // SharedArrayBuffer, DataView, Iterator, Generator,
            // Proxy already handled higher up).
            self.failed_set_result(
                strict,
                format!(
                    "Cannot set property '{name}' on {}",
                    value_kind_name(&receiver)
                ),
            )?;
            None
        };
        if let Some(target) = target {
            self.set_property(target, name, value)?;
        }
        stack[top_idx].advance_pc(self.current_byte_len)?;
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
        let recv = *read_register(frame, recv_reg)?;
        let idx_value_raw = *read_register(frame, idx_reg)?;
        if recv.is_nullish() {
            return Err(
                self.err_type(("Cannot read property of null or undefined".to_string()).into())
            );
        }
        let idx_value = self.coerce_property_key_value(context, idx_value_raw)?;
        write_register(frame, idx_reg, idx_value)?;
        let value = if let Some(obj) = recv.as_object() {
            if let Some(sym) = idx_value.as_symbol(&self.gc_heap) {
                let key = VmPropertyKey::Symbol(sym);
                match self.ordinary_get_value(context, Value::object(obj), recv, &key, 0)? {
                    VmGetOutcome::Value(v) => v,
                    VmGetOutcome::InvokeGetter { getter } => {
                        let args: SmallVec<[Value; 8]> = SmallVec::new();
                        self.run_callable_sync(context, &getter, recv, args)?
                    }
                }
            } else if let Some(key) = idx_value.as_string(&self.gc_heap) {
                crate::object::get(obj, &self.gc_heap, &key.to_lossy_string(&self.gc_heap))
                    .unwrap_or(Value::undefined())
            } else if let Some(n) = idx_value.as_number() {
                let key = n.to_display_string();
                crate::object::get(obj, &self.gc_heap, &key).unwrap_or(Value::undefined())
            } else {
                return Err(VmError::TypeMismatch);
            }
        } else if let Some(arr) = recv.as_array() {
            if let Some(sym) = idx_value.as_symbol(&self.gc_heap) {
                if sym
                    .well_known_tag()
                    .is_some_and(|t| t == symbol::WellKnown::Iterator)
                {
                    if let Some(v) = crate::array::get_symbol_property(arr, &self.gc_heap, sym) {
                        v
                    } else {
                        let key = VmPropertyKey::Symbol(sym);
                        match self.ordinary_get_value(context, recv, recv, &key, 0)? {
                            crate::VmGetOutcome::Value(v) => v,
                            crate::VmGetOutcome::InvokeGetter { getter } => {
                                let args: smallvec::SmallVec<[Value; 8]> =
                                    smallvec::SmallVec::new();
                                self.run_callable_sync(context, &getter, recv, args)?
                            }
                        }
                    }
                } else {
                    // §22.1 Array exotic — symbol-keyed access reads
                    // array's own symbol table first; on miss walks
                    // `Array.prototype`.
                    match crate::array::get_symbol_property(arr, &self.gc_heap, sym) {
                        Some(v) => v,
                        None => {
                            let proto = self.constructor_prototype_value("Array")?;
                            if let Some(p) = proto.as_object() {
                                crate::object::get_symbol(p, &self.gc_heap, sym)
                                    .unwrap_or(Value::undefined())
                            } else {
                                Value::undefined()
                            }
                        }
                    }
                }
            } else if let Some(key) = idx_value.as_string(&self.gc_heap) {
                // Computed string-key access on Array exotic.
                let name = key.to_lossy_string(&self.gc_heap);
                if name == "length" {
                    Value::number(NumberValue::from_f64(
                        crate::array::len(arr, &self.gc_heap) as f64
                    ))
                } else if let Some((getter, _setter)) =
                    crate::array::get_accessor(arr, &self.gc_heap, &name)
                {
                    match getter {
                        Some(getter) if abstract_ops::is_callable(&getter) => {
                            let args: SmallVec<[Value; 8]> = SmallVec::new();
                            self.run_callable_sync(context, &getter, recv, args)?
                        }
                        _ => Value::undefined(),
                    }
                } else if let Some(idx) = crate::object::array_index_property_name(&name) {
                    // §10.4.2.4 [[Get]] — an absent integer index is not a
                    // dead end; OrdinaryGet walks the Array.prototype chain
                    // (e.g. `Array.prototype[0]` inherited by a hole slot).
                    if crate::array::has_own_element(arr, &self.gc_heap, idx as usize) {
                        crate::array::get(arr, &self.gc_heap, idx as usize)
                    } else {
                        self.load_from_constructor_prototype(context, "Array", &recv, &name)?
                    }
                } else {
                    match crate::array::get_named_property(arr, &self.gc_heap, &name) {
                        Some(v) => v,
                        None => {
                            self.load_from_constructor_prototype(context, "Array", &recv, &name)?
                        }
                    }
                }
            } else if let Some(n) = idx_value.as_number() {
                match crate::array::index_from_number(n) {
                    Some(idx)
                        if !crate::array::has_accessors(arr, &self.gc_heap)
                            && crate::array::has_own_element(arr, &self.gc_heap, idx) =>
                    {
                        // Dense own element, no index accessors: read directly,
                        // skipping the per-element `idx.to_string()` + accessor
                        // lookup (the hot array element-load path).
                        crate::array::get(arr, &self.gc_heap, idx)
                    }
                    Some(idx) => {
                        let key = idx.to_string();
                        if let Some((getter, _setter)) =
                            crate::array::get_accessor(arr, &self.gc_heap, &key)
                        {
                            match getter {
                                Some(getter) if abstract_ops::is_callable(&getter) => {
                                    let args: smallvec::SmallVec<[Value; 8]> =
                                        smallvec::SmallVec::new();
                                    self.run_callable_sync(
                                        context,
                                        &getter,
                                        Value::array(arr),
                                        args,
                                    )?
                                }
                                _ => Value::undefined(),
                            }
                        } else if crate::array::has_own_element(arr, &self.gc_heap, idx) {
                            crate::array::get(arr, &self.gc_heap, idx)
                        } else {
                            // §10.4.2.4 — an absent index falls to Array.prototype.
                            self.load_from_constructor_prototype(
                                context,
                                "Array",
                                &recv,
                                &idx.to_string(),
                            )?
                        }
                    }
                    None => {
                        crate::array::get_named_property(arr, &self.gc_heap, &n.to_display_string())
                            .unwrap_or(Value::undefined())
                    }
                }
            } else {
                return Err(VmError::TypeMismatch);
            }
        } else if let Some(fid) = recv
            .as_function()
            .or_else(|| recv.as_closure(&self.gc_heap).map(|c| c.cached_function_id))
        {
            let owner = recv.as_closure(&self.gc_heap);
            if let Some(key) = idx_value.as_string(&self.gc_heap) {
                match self.ordinary_function_own_property_descriptor(
                    Some(context),
                    owner,
                    fid,
                    &key.to_lossy_string(&self.gc_heap),
                )? {
                    Some(desc) => descriptor_value(&desc),
                    None => Value::undefined(),
                }
            } else if let Some(sym) = idx_value.as_symbol(&self.gc_heap) {
                let key = VmPropertyKey::Symbol(sym);
                match self.ordinary_get_value(context, recv, recv, &key, 0)? {
                    crate::VmGetOutcome::Value(v) => v,
                    crate::VmGetOutcome::InvokeGetter { getter } => {
                        let args: smallvec::SmallVec<[Value; 8]> = smallvec::SmallVec::new();
                        self.run_callable_sync(context, &getter, recv, args)?
                    }
                }
            } else {
                return Err(VmError::TypeMismatch);
            }
        } else if recv.as_native_function().is_some() {
            // Computed string keys walk the same ordinary [[Get]]
            // ladder as dotted access (own props, then the
            // prototype_override chain down to %Function.prototype%) —
            // the old own-descriptor read made Object['toString']
            // undefined while Object.toString resolved.
            if let Some(key) = idx_value.as_string(&self.gc_heap) {
                let key = key.to_lossy_string(&self.gc_heap);
                let key = VmPropertyKey::OwnedString(key);
                match self.ordinary_get_value(context, recv, recv, &key, 0)? {
                    crate::VmGetOutcome::Value(v) => v,
                    crate::VmGetOutcome::InvokeGetter { getter } => {
                        let args: smallvec::SmallVec<[Value; 8]> = smallvec::SmallVec::new();
                        self.run_callable_sync(context, &getter, recv, args)?
                    }
                }
            } else if let Some(sym) = idx_value.as_symbol(&self.gc_heap) {
                let key = VmPropertyKey::Symbol(sym);
                match self.ordinary_get_value(context, recv, recv, &key, 0)? {
                    crate::VmGetOutcome::Value(v) => v,
                    crate::VmGetOutcome::InvokeGetter { getter } => {
                        let args: smallvec::SmallVec<[Value; 8]> = smallvec::SmallVec::new();
                        self.run_callable_sync(context, &getter, recv, args)?
                    }
                }
            } else {
                return Err(VmError::TypeMismatch);
            }
        } else if recv.as_bound_function().is_some() {
            // Same ladder for bound functions — own descriptor first,
            // then %Function.prototype% via the shared resolver.
            if let Some(key) = idx_value.as_string(&self.gc_heap) {
                let key = key.to_lossy_string(&self.gc_heap);
                let key = VmPropertyKey::OwnedString(key);
                match self.ordinary_get_value(context, recv, recv, &key, 0)? {
                    crate::VmGetOutcome::Value(v) => v,
                    crate::VmGetOutcome::InvokeGetter { getter } => {
                        let args: smallvec::SmallVec<[Value; 8]> = smallvec::SmallVec::new();
                        self.run_callable_sync(context, &getter, recv, args)?
                    }
                }
            } else if let Some(sym) = idx_value.as_symbol(&self.gc_heap) {
                let key = VmPropertyKey::Symbol(sym);
                match self.ordinary_get_value(context, recv, recv, &key, 0)? {
                    crate::VmGetOutcome::Value(v) => v,
                    crate::VmGetOutcome::InvokeGetter { getter } => {
                        let args: smallvec::SmallVec<[Value; 8]> = smallvec::SmallVec::new();
                        self.run_callable_sync(context, &getter, recv, args)?
                    }
                }
            } else {
                return Err(VmError::TypeMismatch);
            }
        } else if let Some(t) = recv.as_typed_array(&self.gc_heap) {
            if let Some(key) = idx_value.as_string(&self.gc_heap) {
                let name = key.to_lossy_string(&self.gc_heap);
                if let Some(n) = canonical_numeric_index_string(&name) {
                    match typed_array_valid_index(&t, &self.gc_heap, n) {
                        Some(idx) => t.get(&mut self.gc_heap, idx).map_err(crate::oom_to_vm)?,
                        None => Value::undefined(),
                    }
                } else {
                    let mut value = Value::undefined();
                    let mut found = false;
                    if let Some(bag) = t.expando(&self.gc_heap)
                        && let Some(outcome) =
                            Self::expando_own_get_outcome(bag, &self.gc_heap, &name)
                    {
                        value = match outcome {
                            VmGetOutcome::Value(v) => v,
                            VmGetOutcome::InvokeGetter { getter } => {
                                let args: SmallVec<[Value; 8]> = SmallVec::new();
                                self.run_callable_sync(context, &getter, recv, args)?
                            }
                        };
                        found = true;
                    }
                    if !found {
                        let direct =
                            binary::typed_array_prototype::load_property(&t, &self.gc_heap, &name);
                        value = if direct.is_undefined() {
                            let kind_name = t.kind().name();
                            self.load_from_constructor_prototype(context, kind_name, &recv, &name)?
                        } else {
                            direct
                        };
                    }
                    value
                }
            } else if let Some(n) = idx_value.as_number() {
                match crate::array::index_from_number(n) {
                    Some(idx) => match t.get_uint8_value(&self.gc_heap, idx) {
                        Some(value) => value,
                        None => t.get(&mut self.gc_heap, idx).map_err(crate::oom_to_vm)?,
                    },
                    None => Value::undefined(),
                }
            } else if let Some(sym) = idx_value.as_symbol(&self.gc_heap) {
                let key = VmPropertyKey::Symbol(sym);
                match self.ordinary_get_value(context, recv, recv, &key, 0)? {
                    crate::VmGetOutcome::Value(v) => v,
                    crate::VmGetOutcome::InvokeGetter { getter } => {
                        let args: smallvec::SmallVec<[Value; 8]> = smallvec::SmallVec::new();
                        self.run_callable_sync(context, &getter, recv, args)?
                    }
                }
            } else {
                return Err(VmError::TypeMismatch);
            }
        } else if let Some(s) = recv.as_string(&self.gc_heap) {
            // §10.4.3 String exotic [[GetOwnProperty]] — UTF-16 code
            // unit indexed access then String.prototype fallback.
            if let Some(key) = idx_value.as_string(&self.gc_heap) {
                let name = key.to_lossy_string(&self.gc_heap);
                self.load_string_primitive_property(context, &recv, s, &name)?
            } else if let Some(n) = idx_value.as_number() {
                // §10.4.3.5 — every numeric index, in- or out-of-bounds,
                // goes through the one String [[Get]] funnel so OOB
                // reads return `undefined` (not the empty string) and
                // the prototype fallback stays consistent.
                let name = n.to_display_string();
                self.load_string_primitive_property(context, &recv, s, &name)?
            } else if let Some(sym) = idx_value.as_symbol(&self.gc_heap) {
                let key = VmPropertyKey::Symbol(sym);
                let proto = self.constructor_prototype_value("String")?;
                if proto.is_nullish() {
                    Value::undefined()
                } else {
                    match self.ordinary_get_value(context, proto, recv, &key, 0)? {
                        crate::VmGetOutcome::Value(v) => v,
                        crate::VmGetOutcome::InvokeGetter { getter } => {
                            let args: smallvec::SmallVec<[Value; 8]> = smallvec::SmallVec::new();
                            self.run_callable_sync(context, &getter, recv, args)?
                        }
                    }
                }
            } else {
                return Err(VmError::TypeMismatch);
            }
        } else if let Some(r) = recv.as_regexp() {
            if let Some(key) = idx_value.as_string(&self.gc_heap) {
                // Computed string-key on RegExp.
                let name = key.to_lossy_string(&self.gc_heap);
                if let Some(bag) = r.expando(&self.gc_heap)
                    && let Some(value) = crate::object::get(bag, &self.gc_heap, &name)
                {
                    value
                } else {
                    let direct = regexp_prototype::load_property(&r, &mut self.gc_heap, &name);
                    if direct.is_undefined() {
                        self.load_from_constructor_prototype(context, "RegExp", &recv, &name)?
                    } else {
                        direct
                    }
                }
            } else if let Some(sym) = idx_value.as_symbol(&self.gc_heap) {
                let key = VmPropertyKey::Symbol(sym);
                match self.ordinary_get_value(context, recv, recv, &key, 0)? {
                    crate::VmGetOutcome::Value(v) => v,
                    crate::VmGetOutcome::InvokeGetter { getter } => {
                        let args: smallvec::SmallVec<[Value; 8]> = smallvec::SmallVec::new();
                        self.run_callable_sync(context, &getter, recv, args)?
                    }
                }
            } else {
                return Err(VmError::TypeMismatch);
            }
        } else if let Some(m) = recv.as_map() {
            if let Some(sym) = idx_value.as_symbol(&self.gc_heap) {
                if sym
                    .well_known_tag()
                    .is_some_and(|t| t == symbol::WellKnown::Iterator)
                {
                    collections_prototype::make_map_iterator_factory(m, &mut self.gc_heap)?
                } else {
                    let key = VmPropertyKey::Symbol(sym);
                    match self.ordinary_get_value(context, recv, recv, &key, 0)? {
                        crate::VmGetOutcome::Value(v) => v,
                        crate::VmGetOutcome::InvokeGetter { getter } => {
                            let args: smallvec::SmallVec<[Value; 8]> = smallvec::SmallVec::new();
                            self.run_callable_sync(context, &getter, recv, args)?
                        }
                    }
                }
            } else {
                return Err(VmError::TypeMismatch);
            }
        } else if let Some(set) = recv.as_set() {
            if let Some(sym) = idx_value.as_symbol(&self.gc_heap) {
                if sym
                    .well_known_tag()
                    .is_some_and(|t| t == symbol::WellKnown::Iterator)
                {
                    collections_prototype::make_set_iterator_factory(set, &mut self.gc_heap)?
                } else {
                    let key = VmPropertyKey::Symbol(sym);
                    match self.ordinary_get_value(context, recv, recv, &key, 0)? {
                        crate::VmGetOutcome::Value(v) => v,
                        crate::VmGetOutcome::InvokeGetter { getter } => {
                            let args: smallvec::SmallVec<[Value; 8]> = smallvec::SmallVec::new();
                            self.run_callable_sync(context, &getter, recv, args)?
                        }
                    }
                }
            } else {
                return Err(VmError::TypeMismatch);
            }
        } else if recv.is_class_constructor()
            || recv.is_weak_map()
            || recv.is_weak_set()
            || recv.is_weak_ref()
            || recv.is_finalization_registry()
            || recv.is_promise()
            || recv.is_array_buffer()
            || recv.is_data_view()
        {
            // §10.2 — symbol-keyed access on callable / class /
            // collection exotics walks via ordinary [[Get]].
            if let Some(sym) = idx_value.as_symbol(&self.gc_heap) {
                let key = VmPropertyKey::Symbol(sym);
                match self.ordinary_get_value(context, recv, recv, &key, 0)? {
                    crate::VmGetOutcome::Value(v) => v,
                    crate::VmGetOutcome::InvokeGetter { getter } => {
                        let args: smallvec::SmallVec<[Value; 8]> = smallvec::SmallVec::new();
                        self.run_callable_sync(context, &getter, recv, args)?
                    }
                }
            } else {
                return Err(VmError::TypeMismatch);
            }
        } else if recv.is_symbol() || recv.is_boolean() || recv.is_number() || recv.is_big_int() {
            // §7.1.18 ToObject — primitive receivers walk wrapper
            // prototype for string/symbol/number key access.
            let ctor_name = if recv.is_symbol() {
                "Symbol"
            } else if recv.is_boolean() {
                "Boolean"
            } else if recv.is_number() {
                "Number"
            } else {
                "BigInt"
            };
            let key = if let Some(sym) = idx_value.as_symbol(&self.gc_heap) {
                VmPropertyKey::Symbol(sym)
            } else if let Some(s) = idx_value.as_string(&self.gc_heap) {
                VmPropertyKey::OwnedString(s.to_lossy_string(&self.gc_heap))
            } else if let Some(n) = idx_value.as_number() {
                VmPropertyKey::OwnedString(n.to_display_string())
            } else {
                return Err(VmError::TypeMismatch);
            };
            let proto = self.constructor_prototype_value(ctor_name)?;
            if proto.is_nullish() {
                Value::undefined()
            } else {
                match self.ordinary_get_value(context, proto, recv, &key, 0)? {
                    crate::VmGetOutcome::Value(v) => v,
                    crate::VmGetOutcome::InvokeGetter { getter } => {
                        let args: smallvec::SmallVec<[Value; 8]> = smallvec::SmallVec::new();
                        self.run_callable_sync(context, &getter, recv, args)?
                    }
                }
            }
        } else {
            // Remaining object-typed receivers (class constructors,
            // module namespaces, ...) resolve through the generic
            // value-level [[Get]] funnel.
            let key = if let Some(sym) = idx_value.as_symbol(&self.gc_heap) {
                VmPropertyKey::Symbol(sym)
            } else if let Some(k) = idx_value.as_string(&self.gc_heap) {
                VmPropertyKey::OwnedString(k.to_lossy_string(&self.gc_heap))
            } else if let Some(n) = idx_value.as_number() {
                VmPropertyKey::OwnedString(n.to_display_string())
            } else {
                return Err(VmError::TypeMismatch);
            };
            match self.ordinary_get_value(context, recv, recv, &key, 0)? {
                crate::VmGetOutcome::Value(v) => v,
                crate::VmGetOutcome::InvokeGetter { getter } => {
                    let args: smallvec::SmallVec<[Value; 8]> = smallvec::SmallVec::new();
                    self.run_callable_sync(context, &getter, recv, args)?
                }
            }
        };
        write_register(frame, dst, value)?;
        frame.advance_pc(self.current_byte_len)?;
        Ok(())
    }

    /// §10.4.5.16 step 2 — convert a value being stored into a typed
    /// array with `ToBigInt` for BigInt element kinds and `ToNumber`
    /// otherwise (firing the operand's coercion and throwing for a
    /// Symbol / cross-numeric type), then narrow it to the element
    /// representation. The conversion runs before the index check.
    pub(crate) fn typed_array_coerce_element(
        &mut self,
        context: &ExecutionContext,
        kind: crate::binary::TypedArrayKind,
        value: Value,
    ) -> Result<Value, VmError> {
        let converted = if kind.is_bigint() {
            Value::big_int(crate::coerce::to_big_int_or_throw(self, context, &value)?)
        } else {
            Value::number(crate::coerce::to_number_or_throw(self, context, &value)?)
        };
        binary::dispatch::coerce_element_for_store(&mut self.gc_heap, kind, &converted)
    }

    /// §10.4.2 — strict-mode writes to a non-writable array slot
    /// (frozen / per-key flags) throw TypeError instead of silently
    /// dropping.
    fn array_strict_write_guard(
        &self,
        arr: crate::array::JsArray,
        key: &str,
        strict: bool,
    ) -> Result<(), VmError> {
        if strict && !crate::array::can_write_array_property(arr, &self.gc_heap, key) {
            return Err(self.err_type(
                (format!("Cannot assign to read only property '{key}' of array")).into(),
            ));
        }
        Ok(())
    }

    /// OrdinarySet slow path for an array index write when the
    /// element-store protector is tripped: an absent own element must
    /// consult the prototype chain — a setter consumes the write, a
    /// getter-only accessor or non-writable data property rejects it
    /// (TypeError in strict code). Returns `true` when the write was
    /// consumed either way; `false` falls back to the own-element
    /// fast path.
    fn array_index_store_via_proto(
        &mut self,
        context: &ExecutionContext,
        arr: crate::array::JsArray,
        idx: usize,
        value: Value,
        strict: bool,
    ) -> Result<bool, VmError> {
        let custom_proto = crate::array::prototype_override(arr, &self.gc_heap);
        if (!self.array_index_accessor_protector && custom_proto.is_none())
            || crate::array::has_own_element(arr, &self.gc_heap, idx)
        {
            return Ok(false);
        }
        let key = idx.to_string();
        // A custom prototype (e.g. a TypedArray installed via
        // setPrototypeOf) takes the full [[Set]] funnel: an invalid
        // canonical index on a chained typed array consumes the
        // write as a no-op (§10.4.5.5), a setter fires, a data
        // outcome falls back to the own-element fast path.
        if let Some(proto) = custom_proto {
            if proto.is_null() {
                return Ok(false);
            }
            if proto.as_object().is_none() {
                let vm_key = VmPropertyKey::OwnedString(key);
                let receiver = Value::array(arr);
                let handled =
                    self.ordinary_set_data_value(context, proto, &vm_key, value, receiver, 0)?;
                if !handled {
                    self.failed_set_result(strict, "Cannot assign to property")?;
                }
                return Ok(true);
            }
        }
        let proto = match custom_proto {
            Some(p) => p,
            None => self.constructor_prototype_value("Array")?,
        };
        let Some(proto_obj) = proto.as_object() else {
            return Ok(false);
        };
        match object::resolve_set(proto_obj, &self.gc_heap, &key) {
            object::SetOutcome::InvokeSetter { setter } => {
                let mut args: SmallVec<[Value; 8]> = SmallVec::new();
                args.push(value);
                self.run_callable_sync(context, &setter, Value::array(arr), args)?;
                Ok(true)
            }
            object::SetOutcome::Reject { .. } => {
                self.failed_set_result(strict, format!("Cannot assign to property '{key}'"))?;
                Ok(true)
            }
            object::SetOutcome::ExoticParent { parent } => {
                let vm_key = VmPropertyKey::OwnedString(key);
                let receiver = Value::array(arr);
                let handled =
                    self.ordinary_set_data_value(context, parent, &vm_key, value, receiver, 1)?;
                if !handled {
                    self.failed_set_result(strict, "Cannot assign to property")?;
                }
                Ok(true)
            }
            object::SetOutcome::AssignData => Ok(false),
        }
    }

    pub(crate) fn run_store_element_regs(
        &mut self,
        context: &ExecutionContext,
        stack: &mut HoltStack,
        top_idx: usize,
        recv_reg: u16,
        idx_reg: u16,
        src_reg: u16,
    ) -> Result<(), VmError> {
        let frame = &stack[top_idx];
        let recv = *read_register(frame, recv_reg)?;
        let idx_value_raw = *read_register(frame, idx_reg)?;
        let value = *read_register(frame, src_reg)?;
        let strict = context.function_is_strict(frame.function_id);
        let idx_value = self.coerce_property_key_value(context, idx_value_raw)?;
        if let Some(obj) = recv.as_object() {
            if let Some(sym) = idx_value.as_symbol(&self.gc_heap) {
                // §7.3.28 — adding a private element (brand install)
                // to a non-extensible object is a TypeError.
                if sym.is_private_name()
                    && object::get_own_symbol_descriptor(obj, &self.gc_heap, sym).is_none()
                    && !object::is_extensible(obj, &self.gc_heap)
                {
                    return Err(self.err_type(
                        ("Cannot define private member on a non-extensible object".to_string())
                            .into(),
                    ));
                }
                if object::deferred_namespace_target(obj, &self.gc_heap).is_some() {
                    self.ensure_deferred_namespace_ready(context, &recv, false)?;
                    if !object::deferred_namespace_is_populated(obj, &self.gc_heap)
                        && object::get_own_symbol_descriptor(obj, &self.gc_heap, sym).is_none()
                    {
                        return Err(self.err_type(
                            ("Cannot add symbol property to non-extensible module namespace"
                                .to_string())
                            .into(),
                        ));
                    } else {
                        self.ordinary_set_symbol_with_callable_setter(
                            context, obj, sym, value, strict,
                        )?;
                    }
                } else {
                    self.ordinary_set_symbol_with_callable_setter(
                        context, obj, sym, value, strict,
                    )?;
                }
            } else if let Some(key) = idx_value.as_string(&self.gc_heap) {
                let key = key.to_lossy_string(&self.gc_heap);
                self.ensure_deferred_namespace_ready(
                    context,
                    &recv,
                    !Self::deferred_key_is_symbol_like(&VmPropertyKey::String(&key)),
                )?;
                self.store_computed_ordinary_property(context, recv, obj, &key, value, strict)?;
            } else if let Some(n) = idx_value.as_number() {
                let key = n.to_display_string();
                self.ensure_deferred_namespace_ready(
                    context,
                    &recv,
                    !Self::deferred_key_is_symbol_like(&VmPropertyKey::String(&key)),
                )?;
                self.store_computed_ordinary_property(context, recv, obj, &key, value, strict)?;
            } else {
                return Err(VmError::TypeMismatch);
            }
        } else if let Some(fid) = recv
            .as_function()
            .or_else(|| recv.as_closure(&self.gc_heap).map(|c| c.cached_function_id))
        {
            let owner = recv.as_closure(&self.gc_heap);
            if let Some(key) = idx_value.as_string(&self.gc_heap) {
                let key = key.to_lossy_string(&self.gc_heap);
                let has_own = self.ordinary_function_has_own_string_property_for_extensibility(
                    context, owner, fid, &key,
                )?;
                match self.ordinary_function_own_property_descriptor(
                    Some(context),
                    owner,
                    fid,
                    &key,
                )? {
                    Some(desc) if !desc.writable() => {
                        self.failed_set_result(
                            strict,
                            format!("Cannot assign to read-only property '{key}' of function"),
                        )?;
                    }
                    _ => {
                        if !has_own && !self.ordinary_function_is_extensible(fid) {
                            self.failed_set_result(
                                strict,
                                format!("Cannot add property '{key}' to non-extensible function"),
                            )?;
                        } else {
                            let bag = self.function_user_bag_stack_rooted(
                                stack,
                                owner,
                                fid,
                                &[&recv, &idx_value, &value],
                            )?;
                            self.set_property(bag, &key, value)?;
                            if let Some(metadata_key) =
                                function_metadata::ordinary_function_metadata_key(&key)
                            {
                                self.function_deleted_metadata.remove(&(fid, metadata_key));
                            }
                        }
                    }
                }
            } else if let Some(sym) = idx_value.as_symbol(&self.gc_heap) {
                if !self
                    .ordinary_function_has_own_symbol_property_for_extensibility(owner, fid, sym)
                    && !self.ordinary_function_is_extensible(fid)
                {
                    self.failed_set_result(
                        strict,
                        "Cannot add symbol property to non-extensible function",
                    )?;
                    stack[top_idx].advance_pc(self.current_byte_len)?;
                    return Ok(());
                }
                let bag = self.function_user_bag_stack_rooted(
                    stack,
                    owner,
                    fid,
                    &[&recv, &idx_value, &value],
                )?;
                if !crate::object::set_symbol(bag, &mut self.gc_heap, sym, value) {
                    return Err(self.err_type(
                        ("Cannot store symbol property on function".to_string()).into(),
                    ));
                }
            } else {
                return Err(VmError::TypeMismatch);
            }
        } else if let Some(native) = recv.as_native_function() {
            if let Some(key) = idx_value.as_string(&self.gc_heap) {
                let key = key.to_lossy_string(&self.gc_heap);
                match native.own_property_descriptor(&mut self.gc_heap, &key)? {
                    Some(desc) if !desc.writable() => {
                        self.failed_set_result(
                            strict,
                            format!(
                                "Cannot assign to read-only property '{key}' of function {}",
                                native.name(&self.gc_heap)
                            ),
                        )?;
                    }
                    _ => {
                        let desc =
                            crate::object::PropertyDescriptor::data(value, true, false, true);
                        if !native.define_own_property(&mut self.gc_heap, &key, desc) {
                            self.failed_set_result(
                                strict,
                                format!(
                                    "Cannot define property '{key}' on function {}",
                                    native.name(&self.gc_heap)
                                ),
                            )?;
                        }
                    }
                }
            } else if let Some(sym) = idx_value.as_symbol(&self.gc_heap) {
                let desc = object::PartialPropertyDescriptor {
                    value: Some(value),
                    writable: Some(true),
                    enumerable: Some(false),
                    configurable: Some(true),
                    ..Default::default()
                };
                native.define_own_symbol_property(&mut self.gc_heap, sym, desc);
            } else {
                return Err(VmError::TypeMismatch);
            }
        } else if let Some(bound) = recv.as_bound_function() {
            let bound = &bound;
            if let Some(key) = idx_value.as_string(&self.gc_heap) {
                let key = key.to_lossy_string(&self.gc_heap);
                match function_metadata::bound_own_property_descriptor(
                    bound,
                    &mut self.gc_heap,
                    &key,
                )? {
                    Some(desc) if !desc.writable() => {
                        self.failed_set_result(
                            strict,
                            format!(
                                "Cannot assign to read-only property '{key}' of bound function"
                            ),
                        )?;
                    }
                    _ => {
                        let desc =
                            crate::object::PropertyDescriptor::data(value, true, false, true);
                        if !function_metadata::bound_define_own_property(
                            bound,
                            &mut self.gc_heap,
                            &key,
                            desc,
                        ) {
                            self.failed_set_result(
                                strict,
                                format!("Cannot define property '{key}' on bound function"),
                            )?;
                        }
                    }
                }
            } else {
                return Err(VmError::TypeMismatch);
            }
        } else if let Some(arr) = recv.as_array() {
            if let Some(sym) = idx_value.as_symbol(&self.gc_heap) {
                // §22.1 Array exotic — symbol-keyed writes land in
                // per-array symbol-property table.
                crate::array::set_symbol_property(arr, &mut self.gc_heap, sym, value);
            } else if let Some(key) = idx_value.as_string(&self.gc_heap) {
                let name = key.to_lossy_string(&self.gc_heap);
                if self.store_array_accessor_property(context, arr, &name, &value, strict)? {
                    // Accessor setter handled assignment.
                } else if let Some(idx) = crate::object::array_index_property_name(&name) {
                    if self.array_index_store_via_proto(
                        context,
                        arr,
                        idx as usize,
                        value,
                        strict,
                    )? {
                        // Prototype-chain setter (or rejection) consumed the write.
                    } else {
                        self.array_strict_write_guard(arr, &name, strict)?;
                        let roots = self.collect_allocation_roots(stack);
                        let mut external_visit = |visitor: &mut dyn FnMut(*mut RawGc)| {
                            for &slot in &roots {
                                visitor(slot);
                            }
                        };
                        crate::array::set_with_roots(
                            arr,
                            &mut self.gc_heap,
                            idx as usize,
                            value,
                            &mut external_visit,
                        )?;
                    }
                } else {
                    let has_own_named =
                        crate::array::get_named_property(arr, &self.gc_heap, &name).is_some();
                    if !has_own_named {
                        let proto = self.constructor_prototype_value("Array")?;
                        if let Some(proto) = proto.as_object() {
                            match crate::object::resolve_set(proto, &self.gc_heap, &name) {
                                object::SetOutcome::InvokeSetter { setter } => {
                                    let mut args: SmallVec<[Value; 8]> = SmallVec::new();
                                    args.push(value);
                                    self.run_callable_sync(
                                        context,
                                        &setter,
                                        Value::array(arr),
                                        args,
                                    )?;
                                    stack[top_idx].advance_pc(self.current_byte_len)?;
                                    return Ok(());
                                }
                                object::SetOutcome::Reject { .. } => {
                                    self.failed_set_result(
                                        strict,
                                        format!("Cannot assign to property '{name}'"),
                                    )?;
                                    stack[top_idx].advance_pc(self.current_byte_len)?;
                                    return Ok(());
                                }
                                object::SetOutcome::AssignData => {}
                                // %Array.prototype% chains stay ordinary.
                                object::SetOutcome::ExoticParent { .. } => {}
                            }
                        }
                    }
                    crate::array::set_named_property(arr, &mut self.gc_heap, &name, value)
                        .map_err(|_| VmError::TypeMismatch)?;
                }
            } else if let Some(n) = idx_value.as_number() {
                let key = n.to_display_string();
                if self.store_array_accessor_property(context, arr, &key, &value, strict)? {
                    // Accessor setter handled.
                } else if let Some(idx) = crate::array::index_from_number(n) {
                    if self.array_index_store_via_proto(context, arr, idx, value, strict)? {
                        // Prototype-chain setter (or rejection) consumed the write.
                    } else {
                        self.array_strict_write_guard(arr, &key, strict)?;
                        let roots = self.collect_allocation_roots(stack);
                        let mut external_visit = |visitor: &mut dyn FnMut(*mut RawGc)| {
                            for &slot in &roots {
                                visitor(slot);
                            }
                        };
                        crate::array::set_with_roots(
                            arr,
                            &mut self.gc_heap,
                            idx,
                            value,
                            &mut external_visit,
                        )?;
                    }
                } else {
                    self.array_strict_write_guard(arr, &key, strict)?;
                    crate::array::set_named_property(arr, &mut self.gc_heap, &key, value)
                        .map_err(|_| VmError::TypeMismatch)?;
                }
            } else {
                return Err(VmError::TypeMismatch);
            }
        } else if let Some(t) = recv.as_typed_array(&self.gc_heap) {
            // §10.4.5.16 TypedArraySetElement / §10.4.5.5 [[Set]] —
            // determine the canonical numeric index, then convert the
            // value with ToNumber / ToBigInt (firing `valueOf` and
            // throwing for a Symbol / cross-type) **before** the index
            // validity check, so the conversion side effects run even
            // when the index is out of bounds; an invalid index then
            // discards the converted value (§10.4.5.9 step 2).
            let numeric_index: Option<f64> = if let Some(n) = idx_value.as_number() {
                Some(n.as_f64())
            } else if let Some(key) = idx_value.as_string(&self.gc_heap) {
                let name = key.to_lossy_string(&self.gc_heap);
                match canonical_numeric_index_string(&name) {
                    Some(nf) => Some(nf),
                    None => {
                        // Same funnel as the named-store arm: chain
                        // walk + receiver-phase semantics.
                        let vm_key = VmPropertyKey::OwnedString(name);
                        self.ordinary_set_data_value(context, recv, &vm_key, value, recv, 0)?;
                        stack[top_idx].advance_pc(self.current_byte_len)?;
                        return Ok(());
                    }
                }
            } else {
                None
            };
            if let Some(nf) = numeric_index {
                if t.kind() == crate::binary::TypedArrayKind::Uint8
                    && let Some(number) = value.as_number()
                {
                    if let Some(idx) = typed_array_valid_index(&t, &self.gc_heap, nf) {
                        t.set_uint8_number(&mut self.gc_heap, idx, number);
                    }
                    stack[top_idx].advance_pc(self.current_byte_len)?;
                    return Ok(());
                }
                let converted = self.typed_array_coerce_element(context, t.kind(), value)?;
                if let Some(idx) = typed_array_valid_index(&t, &self.gc_heap, nf)
                    && !t.set_uint8_value(&mut self.gc_heap, idx, &converted)
                {
                    t.set(&mut self.gc_heap, idx, &converted);
                }
            } else if let Some(sym) = idx_value.as_symbol(&self.gc_heap) {
                let bag = typed_array_ensure_expando(self, &t)?;
                if !crate::object::set_symbol(bag, &mut self.gc_heap, sym, value) {
                    return Err(self.err_type(
                        ("Cannot store symbol property on TypedArray".to_string()).into(),
                    ));
                }
            } else {
                return Err(VmError::TypeMismatch);
            }
        } else if let Some(r) = recv.as_regexp() {
            if let Some(sym) = idx_value.as_symbol(&self.gc_heap) {
                // §22.2.6 — symbol-keyed writes land in expando bag.
                let absent = r.expando(&self.gc_heap).is_none_or(|bag| {
                    object::get_own_symbol_descriptor(bag, &self.gc_heap, sym).is_none()
                });
                if absent && !r.is_extensible(&self.gc_heap) {
                    self.failed_set_result(
                        strict,
                        "Cannot add symbol property to non-extensible RegExp",
                    )?;
                    stack[top_idx].advance_pc(self.current_byte_len)?;
                    return Ok(());
                }
                let bag = regexp_ensure_expando(self, &r, &recv)?;
                if !crate::object::set_symbol(bag, &mut self.gc_heap, sym, value) {
                    return Err(self
                        .err_type(("Cannot store symbol property on RegExp".to_string()).into()));
                }
            } else {
                return Err(VmError::TypeMismatch);
            }
        } else if let Some(p) = recv.as_promise() {
            if let Some(sym) = idx_value.as_symbol(&self.gc_heap) {
                let bag = promise_ensure_expando_pub(&mut self.gc_heap, &p)?;
                if !crate::object::set_symbol(bag, &mut self.gc_heap, sym, value) {
                    return Err(self
                        .err_type(("Cannot store symbol property on Promise".to_string()).into()));
                }
            } else {
                return Err(VmError::TypeMismatch);
            }
        } else if let Some(dv) = recv.as_data_view() {
            // §25.3 — ordinary object: route both symbol- and
            // string-keyed writes into the lazy expando bag.
            let bag = data_view_ensure_expando_pub(&mut self.gc_heap, &dv)?;
            if let Some(sym) = idx_value.as_symbol(&self.gc_heap) {
                if !crate::object::set_symbol(bag, &mut self.gc_heap, sym, value) {
                    return Err(self.err_type(
                        ("Cannot store symbol property on DataView".to_string()).into(),
                    ));
                }
            } else if let Some(s) = idx_value.as_string(&self.gc_heap) {
                let name = s.to_lossy_string(&self.gc_heap);
                self.set_property(bag, &name, value)?;
            } else {
                return Err(VmError::TypeMismatch);
            }
        } else if let Some(c) = recv.as_class_constructor() {
            if let Some(sym) = idx_value.as_symbol(&self.gc_heap) {
                let statics = c.statics(&self.gc_heap);
                if !crate::object::set_symbol(statics, &mut self.gc_heap, sym, value) {
                    return Err(self.err_type(
                        ("Cannot store symbol property on class constructor".to_string()).into(),
                    ));
                }
            } else {
                return Err(VmError::TypeMismatch);
            }
        } else if recv.is_map() || recv.is_set() {
            // §10.1.9 OrdinarySet — computed string/symbol writes land
            // in the lazy expando via the shared [[Set]] funnel.
            let vm_key = if let Some(sym) = idx_value.as_symbol(&self.gc_heap) {
                VmPropertyKey::Symbol(sym)
            } else if let Some(s) = idx_value.as_string(&self.gc_heap) {
                VmPropertyKey::OwnedString(s.to_lossy_string(&self.gc_heap))
            } else if let Some(n) = idx_value.as_number() {
                VmPropertyKey::OwnedString(n.to_display_string())
            } else {
                return Err(VmError::TypeMismatch);
            };
            if !self.ordinary_set_data_value(context, recv, &vm_key, value, recv, 0)? {
                self.failed_set_result(strict, "Cannot assign to read-only property")?;
            }
        } else if recv.is_undefined() || recv.is_null() || recv.is_hole() {
            return Err(self
                .err_type((format!("Cannot set property on {}", value_kind_name(&recv))).into()));
        } else if recv.is_boolean()
            || recv.is_number()
            || recv.is_string()
            || recv.is_symbol()
            || recv.is_big_int()
        {
            self.failed_set_result(
                strict,
                format!("Cannot set property on {}", value_kind_name(&recv)),
            )?;
        } else {
            return Err(VmError::TypeMismatch);
        }
        let frame = &mut stack[top_idx];
        frame.advance_pc(self.current_byte_len)?;
        Ok(())
    }

    /// Apply descriptor-aware data assignment for computed ordinary-object
    /// writes (`obj[key] = value`).
    pub(crate) fn store_computed_ordinary_property(
        &mut self,
        context: &ExecutionContext,
        receiver: Value,
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
                    self.failed_set_result(
                        strict,
                        format!("Cannot assign to read-only property '{key}'"),
                    )
                }
            }
            object::SetOutcome::InvokeSetter { .. } => self.failed_set_result(
                strict,
                format!("Cannot assign to accessor property '{key}' without a setter"),
            ),
            object::SetOutcome::Reject { .. } => {
                self.failed_set_result(strict, format!("Cannot assign to property '{key}'"))
            }
            // §10.1.9.2 step 2 — the walk hit an exotic prototype
            // (e.g. a TypedArray): continue through parent.[[Set]]
            // so its override (§10.4.5.5) is observable.
            object::SetOutcome::ExoticParent { parent } => {
                if !self.ordinary_set_data_value(
                    context,
                    parent,
                    &VmPropertyKey::String(key),
                    value,
                    receiver,
                    1,
                )? {
                    self.failed_set_result(strict, format!("Cannot assign to property '{key}'"))?;
                }
                Ok(())
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
    /// Pick the right TypeError text for a rejected `[[Set]]`: adding a
    /// brand-new property to a non-extensible (sealed / frozen) object is
    /// V8's "Cannot add property X, object is not extensible", distinct
    /// from the `fallback` used when an existing property is unwritable
    /// or accessor-only.
    fn extensibility_aware_set_message(
        &self,
        obj: JsObject,
        key: &str,
        fallback: String,
    ) -> String {
        let is_new = crate::object::get_own(obj, &self.gc_heap, key).is_none();
        if is_new && !crate::object::is_extensible(obj, &self.gc_heap) {
            format!("Cannot add property {key}, object is not extensible")
        } else {
            fallback
        }
    }

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
                    let message = self.extensibility_aware_set_message(
                        obj,
                        key,
                        format!("Cannot assign to read-only property '{key}'"),
                    );
                    self.failed_set_result(strict, message)
                }
            }
            object::SetOutcome::InvokeSetter { setter } => {
                let mut args: SmallVec<[Value; 8]> = SmallVec::new();
                args.push(value);
                self.run_callable_sync(context, &setter, Value::object(obj), args)?;
                Ok(())
            }
            object::SetOutcome::Reject { .. } => {
                let message = self.extensibility_aware_set_message(
                    obj,
                    key,
                    format!("Cannot assign to property '{key}'"),
                );
                self.failed_set_result(strict, message)
            }
            object::SetOutcome::ExoticParent { parent } => {
                if !self.ordinary_set_data_value(
                    context,
                    parent,
                    &VmPropertyKey::String(key),
                    value,
                    Value::object(obj),
                    1,
                )? {
                    self.failed_set_result(strict, format!("Cannot assign to property '{key}'"))?;
                }
                Ok(())
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
        sym: crate::symbol::JsSymbol,
        value: Value,
        strict: bool,
    ) -> Result<(), VmError> {
        match crate::object::resolve_symbol_set(obj, &self.gc_heap, sym) {
            object::SetOutcome::AssignData => {
                if !crate::object::set_symbol(obj, &mut self.gc_heap, sym, value) {
                    self.failed_set_result(strict, "Cannot assign to symbol property")?;
                }
                Ok(())
            }
            object::SetOutcome::InvokeSetter { setter } => {
                let mut args: SmallVec<[Value; 8]> = SmallVec::new();
                args.push(value);
                self.run_callable_sync(context, &setter, Value::object(obj), args)?;
                Ok(())
            }
            object::SetOutcome::Reject { .. } => {
                self.failed_set_result(strict, "Cannot assign to symbol property")
            }
            object::SetOutcome::ExoticParent { parent } => {
                if !self.ordinary_set_data_value(
                    context,
                    parent,
                    &VmPropertyKey::Symbol(sym),
                    value,
                    Value::object(obj),
                    1,
                )? {
                    self.failed_set_result(strict, "Cannot assign to symbol property")?;
                }
                Ok(())
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
        let Some(proto_obj) = proto.as_object() else {
            return Ok(Value::undefined());
        };
        let key = VmPropertyKey::String(name);
        match self.ordinary_get_value(context, Value::object(proto_obj), *receiver, &key, 0)? {
            VmGetOutcome::Value(value) => Ok(value),
            VmGetOutcome::InvokeGetter { getter } => {
                self.run_callable_sync(context, &getter, *receiver, smallvec::SmallVec::new())
            }
        }
    }
    /// JIT bridge for a named `LoadProperty` from compiled code.
    ///
    /// Compiled frames run at PC 0, so the interpreter's `function_id`/`pc`
    /// IC-site lookup cannot be used. The emitter instead passes the dense
    /// `site` straight from the snapshot field `property_ic_site`, which is the
    /// same index `drive_load_property` resolves. The hot path is the IC hit,
    /// a shape guard plus slot read identical to the interpreter fast path. A
    /// cold own-data or direct-prototype data observation installs the IC here
    /// too, so compiled top-level/OSR code does not depend on interpreter
    /// pre-warming. Accessors, uncached prototype walks, polymorphic overflow,
    /// and non-object receivers fall back to the full read. The frame PC is
    /// saved and restored so a later guard bail still re-runs from PC 0.
    ///
    /// # Errors
    /// Propagates read errors (throwing getter) and `InvalidOperand` for an
    /// unknown property-name index.
    pub fn jit_runtime_load_property(
        &mut self,
        context: &ExecutionContext,
        stack: &mut HoltStack,
        frame_index: usize,
        function_id: u32,
        dst: u16,
        obj_reg: u16,
        name_idx: u32,
        site: usize,
    ) -> Result<u64, VmError> {
        self.record_jit_runtime_property_stub();
        let atomized_key = context
            .property_atom_for_function(function_id, name_idx)
            .ok_or(VmError::InvalidOperand)?;
        let mut receiver = *read_register(&stack[frame_index], obj_reg)?;
        if let Some(obj) = receiver.as_object() {
            // Compiled code normally keeps heap values in the rooted frame
            // window, but a runtime stub can observe a value that survived a
            // recent scavenge before the compiled side has reloaded it. Repair
            // forwarded handles at the bridge boundary before any typed payload
            // read interprets the forwarding word as object fields.
            unsafe {
                let header = obj.as_header_ptr();
                if !header.is_null() && (*header).is_forwarded() {
                    let forwarded = otter_gc::Gc::from_offset(
                        otter_gc::GcHeader::read_forwarding_offset(header),
                    );
                    receiver = Value::object(forwarded);
                    write_register(&mut stack[frame_index], obj_reg, receiver)?;
                }
            }
        }
        if let Some(obj) = receiver.as_object()
            && site < self.load_property_ics.len()
            && !self.load_property_ics[site].is_megamorphic()
        {
            let mut hit_value: Option<Value> = None;
            for ic in self.load_property_ics[site].entries() {
                if let Some(value) = ic.run_load(obj, &self.gc_heap, atomized_key) {
                    hit_value = Some(value);
                    break;
                }
            }
            if let Some(value) = hit_value {
                self.property_ic_stats.record_hit(PropertyIcKind::Load);
                write_register(&mut stack[frame_index], dst, value)?;
                // Report a monomorphic own-data inline-slot fill so the
                // emitted site can self-patch its WhiskerIC cell and inline
                // subsequent loads without re-entering this stub. Mirrors the
                // (former) compile-time bake, but resolved at runtime so a site
                // that was cold when its function tiered up (OSR off an earlier
                // loop) still inlines once warm.
                return Ok(self.whisker_load_cell_fill(site, obj, atomized_key));
            }
            if self.load_property_ics[site].entry_count() > 0 {
                self.load_property_ics[site].record_guard_miss_with_stats(
                    &mut self.property_ic_stats,
                    PropertyIcKind::Load,
                );
            } else {
                self.load_property_ics[site].record_uncached_miss_with_stats(
                    &mut self.property_ic_stats,
                    PropertyIcKind::Load,
                );
            }
            if !self.load_property_ics[site].is_megamorphic()
                && let Some((ic, value)) =
                    cache_ir::CacheStub::install_load(obj, &self.gc_heap, atomized_key)
            {
                self.load_property_ics[site].install_with_stats(
                    &mut self.property_ic_stats,
                    PropertyIcKind::Load,
                    ic,
                );
                write_register(&mut stack[frame_index], dst, value)?;
                return Ok(self.whisker_load_cell_fill(site, obj, atomized_key));
            }
        }
        // Slow path: full `[[Get]]` (proto walk / accessor / non-object). It
        // advances the interpreter PC, which compiled code must not observe.
        let saved_pc = stack[frame_index].pc;
        let result =
            self.run_load_property_reg(context, stack, frame_index, dst, obj_reg, atomized_key);
        stack[frame_index].pc = saved_pc;
        result.map(|()| 0)
    }

    /// Frameless `LoadProperty`: resolve an own-data monomorphic load directly
    /// against the register window `regs`, with no `HoltStack` frame. Used by a
    /// self-recursive callee that runs frameless on the flat JIT register stack.
    ///
    /// Returns `Ok(Some(fill))` when handled — the value is written into
    /// `regs[dst]` and `fill` is the WhiskerIC cell-fill (`0` = no inline) — or
    /// `Ok(None)` when the load needs the full `[[Get]]` ladder (non-object,
    /// accessor, prototype hop, or non-cacheable), in which case the caller
    /// bails to the interpreter and resumes there. The own-data IC warms from
    /// the framed top-level execution before any frameless child runs, so the
    /// steady state is the inline hit (this stub is the cold miss).
    ///
    /// # Safety
    /// `regs` must point at a live, GC-traced register window with at least
    /// `max(dst, obj_reg) + 1` slots (the flat JIT register stack is scanned for
    /// the call's duration, so the receiver and result stay rooted).
    pub unsafe fn jit_runtime_load_property_window(
        &mut self,
        context: &ExecutionContext,
        regs: *mut u64,
        function_id: u32,
        dst: u16,
        obj_reg: u16,
        name_idx: u32,
        site: usize,
    ) -> Result<Option<u64>, VmError> {
        self.record_jit_runtime_property_stub();
        let atomized_key = context
            .property_atom_for_function(function_id, name_idx)
            .ok_or(VmError::InvalidOperand)?;
        let receiver = Value::from_bits(unsafe { *regs.add(obj_reg as usize) });
        let Some(obj) = receiver.as_object() else {
            return Ok(None);
        };
        if site >= self.load_property_ics.len() || self.load_property_ics[site].is_megamorphic() {
            return Ok(None);
        }
        let mut hit_value: Option<Value> = None;
        for ic in self.load_property_ics[site].entries() {
            if let Some(value) = ic.run_load(obj, &self.gc_heap, atomized_key) {
                hit_value = Some(value);
                break;
            }
        }
        if let Some(value) = hit_value {
            self.property_ic_stats.record_hit(PropertyIcKind::Load);
            unsafe {
                *regs.add(dst as usize) = value.to_bits();
            }
            return Ok(Some(self.whisker_load_cell_fill(site, obj, atomized_key)));
        }
        if self.load_property_ics[site].entry_count() > 0 {
            self.load_property_ics[site]
                .record_guard_miss_with_stats(&mut self.property_ic_stats, PropertyIcKind::Load);
        } else {
            self.load_property_ics[site]
                .record_uncached_miss_with_stats(&mut self.property_ic_stats, PropertyIcKind::Load);
        }
        if !self.load_property_ics[site].is_megamorphic()
            && let Some((ic, value)) =
                cache_ir::CacheStub::install_load(obj, &self.gc_heap, atomized_key)
        {
            self.load_property_ics[site].install_with_stats(
                &mut self.property_ic_stats,
                PropertyIcKind::Load,
                ic,
            );
            unsafe {
                *regs.add(dst as usize) = value.to_bits();
            }
            return Ok(Some(self.whisker_load_cell_fill(site, obj, atomized_key)));
        }
        Ok(None)
    }

    /// Frameless `StoreProperty` — the [`Self::jit_runtime_load_property_window`]
    /// counterpart. Resolves an existing-own-data monomorphic store (including
    /// the generational write barrier, which the IC's `store` applies) directly
    /// against the register window. Returns `Ok(Some(fill))` when handled, or
    /// `Ok(None)` for a transition / accessor / reject that needs the full
    /// `[[Set]]` ladder (caller bails).
    ///
    /// # Safety
    /// As [`Self::jit_runtime_load_property_window`].
    pub unsafe fn jit_runtime_store_property_window(
        &mut self,
        context: &ExecutionContext,
        regs: *mut u64,
        function_id: u32,
        obj_reg: u16,
        name_idx: u32,
        src: u16,
        site: usize,
    ) -> Result<Option<u64>, VmError> {
        self.record_jit_runtime_property_stub();
        let atomized_key = context
            .property_atom_for_function(function_id, name_idx)
            .ok_or(VmError::InvalidOperand)?;
        let receiver = Value::from_bits(unsafe { *regs.add(obj_reg as usize) });
        let value = Value::from_bits(unsafe { *regs.add(src as usize) });
        let Some(obj) = receiver.as_object() else {
            return Ok(None);
        };
        if site >= self.store_property_ics.len()
            || !object::supports_fast_property_ic(obj, &self.gc_heap)
        {
            return Ok(None);
        }
        let entries_len = self.store_property_ics[site].entry_count();
        for idx in 0..entries_len {
            let ic = self.store_property_ics[site].entries()[idx].clone();
            if ic
                .run_store(obj, &mut self.gc_heap, atomized_key, &value)
                .is_some()
            {
                self.property_ic_stats.record_hit(PropertyIcKind::Store);
                return Ok(Some(self.whisker_store_cell_fill(
                    site,
                    object::shape(obj, &self.gc_heap).offset(),
                )));
            }
        }
        if entries_len > 0 {
            self.store_property_ics[site]
                .record_guard_miss_with_stats(&mut self.property_ic_stats, PropertyIcKind::Store);
        } else {
            self.store_property_ics[site].record_uncached_miss_with_stats(
                &mut self.property_ic_stats,
                PropertyIcKind::Store,
            );
        }
        if !self.store_property_ics[site].is_megamorphic()
            && let Some(ic) =
                cache_ir::CacheStub::install_store_existing(obj, &self.gc_heap, atomized_key)
            && ic
                .run_store(obj, &mut self.gc_heap, atomized_key, &value)
                .is_some()
        {
            self.store_property_ics[site].install_with_stats(
                &mut self.property_ic_stats,
                PropertyIcKind::Store,
                ic,
            );
            return Ok(Some(self.whisker_store_cell_fill(
                site,
                object::shape(obj, &self.gc_heap).offset(),
            )));
        }
        Ok(None)
    }

    /// Packed WhiskerIC inline-load cell fill for `site`, or `0` for "no inline".
    ///
    /// Low 32 bits = the cached shape-handle compressed offset (a stable guard
    /// token — shapes are immortal and pinned in old space, and a fast-mode
    /// shape is never the null offset `0`). High 32 bits = the byte offset
    /// inside the object's contiguous value slab. Only a warm, single-entry
    /// `OwnData` IC qualifies; everything else returns `0`, leaving the emitted
    /// site on the stub.
    fn whisker_load_cell_fill(
        &self,
        site: usize,
        obj: JsObject,
        atomized_key: AtomizedPropertyKey<'_>,
    ) -> u64 {
        let recv_shape = object::shape(obj, &self.gc_heap).offset();
        if recv_shape == 0 {
            return 0;
        }
        for ic in self.load_property_ics[site].entries() {
            if let Some(hit) = ic.own_data_hit()
                && hit.shape.offset() == recv_shape
                && hit.atom_id == atomized_key.atom().id()
                && object::load_own_data_slot_atom(obj, &self.gc_heap, atomized_key, hit).is_some()
            {
                let value_byte = u32::from(hit.slot)
                    * std::mem::size_of::<crate::value::compressed::CompressedValue>() as u32;
                return (u64::from(value_byte) << 32) | u64::from(hit.shape.offset());
            }
        }
        0
    }

    /// JIT bridge for a computed `LoadElement` (`recv[idx]`) from compiled code.
    /// Delegates to the full interpreter element read
    /// ([`Self::run_load_element_regs`]), which covers dense / sparse array
    /// elements, typed arrays, string indices, and the ordinary `[[Get]]`
    /// fallback for object receivers. The frame PC is saved and restored so a
    /// later guard bail still re-runs the compiled frame from PC 0.
    ///
    /// # Errors
    /// Propagates a throwing getter, a `null`/`undefined` receiver `TypeError`,
    /// or `InvalidOperand`.
    pub fn jit_runtime_load_element(
        &mut self,
        context: &ExecutionContext,
        stack: &mut HoltStack,
        frame_index: usize,
        dst: u16,
        recv_reg: u16,
        idx_reg: u16,
    ) -> Result<(), VmError> {
        self.record_jit_runtime_property_stub();
        let saved_pc = stack[frame_index].pc;
        let frame = &mut stack[frame_index];
        let result = self.run_load_element_regs(context, frame, dst, recv_reg, idx_reg);
        stack[frame_index].pc = saved_pc;
        result
    }

    /// JIT bridge for `LoadGlobalOrThrow` from compiled code, delegating to the
    /// full interpreter global read ([`Self::run_load_global_or_throw_reg`]):
    /// resolves a free identifier through the global object, throwing a
    /// `ReferenceError` when unbound. Frame PC is saved/restored so a later
    /// guard bail re-runs the compiled frame from PC 0.
    ///
    /// # Errors
    /// Propagates the `ReferenceError` for an unbound identifier (and any
    /// throwing global accessor) plus `InvalidOperand`.
    pub fn jit_runtime_load_global(
        &mut self,
        context: &ExecutionContext,
        stack: &mut HoltStack,
        frame_index: usize,
        function_id: u32,
        dst: u16,
        name_idx: u32,
    ) -> Result<(), VmError> {
        self.record_jit_runtime_property_stub();
        let saved_pc = stack[frame_index].pc;
        let frame = &mut stack[frame_index];
        let result = self.run_load_global_or_throw_reg_for_function(
            context,
            frame,
            function_id,
            dst,
            name_idx,
        );
        stack[frame_index].pc = saved_pc;
        result
    }

    /// `Op::DefineDataProperty obj, key, value` — construction-time data-property
    /// definition for object literals. Shared by the dispatch loop and the JIT
    /// delegate bridge; does **not** advance the PC (the caller does).
    ///
    /// # Errors
    /// Propagates a `TypeError` when the target rejects the definition, plus any
    /// error from key coercion (`ToPropertyKey`).
    pub(crate) fn run_define_data_property_regs(
        &mut self,
        context: &ExecutionContext,
        stack: &mut HoltStack,
        top_idx: usize,
        obj_reg: u16,
        key_reg: u16,
        value_reg: u16,
    ) -> Result<(), VmError> {
        let target = *read_register(&stack[top_idx], obj_reg)?;
        let key_value = *read_register(&stack[top_idx], key_reg)?;
        let value = *read_register(&stack[top_idx], value_reg)?;
        let key = self.to_property_key_sync(context, key_value)?;
        // Fast path: a plain object receiver takes the shape-friendly
        // construction-time store (no prototype consult — define semantics).
        if let Some(obj) = target.as_object() {
            match &key {
                VmPropertyKey::Symbol(sym) => {
                    object::set_symbol(obj, &mut self.gc_heap, *sym, value);
                }
                _ => {
                    let name = key
                        .string_name()
                        .expect("non-symbol key has string spelling")
                        .to_string();
                    self.set_property(obj, &name, value)?;
                }
            }
        } else {
            let descriptor = object::PartialPropertyDescriptor {
                value: Some(value),
                writable: Some(true),
                enumerable: Some(true),
                configurable: Some(true),
                ..Default::default()
            };
            if !self.define_own_property_value(context, &target, &key, descriptor)? {
                return Err(
                    self.err_type(("Cannot define property on object literal".to_string()).into())
                );
            }
        }
        Ok(())
    }

    /// Generic JIT bridge: re-run one **synchronous, non-control-flow** opcode
    /// (the instruction at `byte_pc`) through the interpreter's own handler.
    ///
    /// Used by the baseline emitter for opcodes whose operands are variable or
    /// awkward to marshal through the C ABI (closure/array/object construction,
    /// string constants, the checked upvalue store, remainder, unsigned shift).
    /// The bridge fetches the `ExecInstr` at `byte_pc`, reads its operands from
    /// `context`, and dispatches to the exact interpreter helper — so semantics
    /// are identical to the interpreter by construction.
    ///
    /// Only ops that run to completion without pushing a frame or invoking user
    /// JS are delegated here: a stub cannot drive the dispatch loop, so frame-
    /// pushing ops (`New` of a user constructor, `ArrayFrom` with a user
    /// iterator) are deliberately excluded and fall back to the interpreter.
    /// Frame PC is saved/restored around the helper's `advance_pc`.
    ///
    /// # Errors
    /// Propagates whatever the delegated handler raises, and `InvalidOperand`
    /// for an unknown PC, operand shape, or a non-delegable opcode.
    pub fn jit_runtime_delegate_op(
        &mut self,
        context: &ExecutionContext,
        stack: &mut HoltStack,
        frame_index: usize,
        function_id: u32,
        byte_pc: u32,
    ) -> Result<(), VmError> {
        let func = context
            .exec_function(function_id)
            .ok_or(VmError::InvalidOperand)?;
        let instr = func
            .instr_at_byte_pc(byte_pc)
            .ok_or(VmError::InvalidOperand)?;
        let op = instr.op();
        let saved_pc = stack[frame_index].pc;
        stack[frame_index].pc = byte_pc;
        let result = match op {
            Op::MakeClosure => {
                let operands = context.exec_operands(instr);
                let frame = &mut stack[frame_index];
                self.run_make_closure_operands(context, frame, operands)
            }
            Op::NewObject => {
                let dst = context
                    .exec_register(instr, 0)
                    .ok_or(VmError::InvalidOperand)?;
                self.run_new_object_reg(stack, frame_index, dst)
            }
            Op::NewArray => {
                let operands = context.exec_operands(instr);
                self.run_new_array_operands(stack, frame_index, operands)
            }
            Op::StoreUpvalueChecked => {
                let src = context
                    .exec_register(instr, 0)
                    .ok_or(VmError::InvalidOperand)?;
                let idx = context
                    .exec_imm32(instr, 1)
                    .ok_or(VmError::InvalidOperand)?;
                let frame = &mut stack[frame_index];
                self.run_store_upvalue_checked_reg(frame, src, idx)
            }
            Op::Rem => {
                let (dst, lhs, rhs) = context
                    .exec_register3(instr)
                    .ok_or(VmError::InvalidOperand)?;
                let frame = &mut stack[frame_index];
                self.run_numeric_regs(
                    frame,
                    dst,
                    lhs,
                    rhs,
                    crate::number::rem,
                    crate::bigint::ops::rem,
                )
            }
            Op::Add => {
                let (dst, lhs, rhs) = context
                    .exec_register3(instr)
                    .ok_or(VmError::InvalidOperand)?;
                let frame = &mut stack[frame_index];
                self.run_add_regs(frame, dst, lhs, rhs)
            }
            Op::Ushr => {
                let (dst, lhs, rhs) = context
                    .exec_register3(instr)
                    .ok_or(VmError::InvalidOperand)?;
                let frame = &mut stack[frame_index];
                self.run_ushr_regs(frame, dst, lhs, rhs)
            }
            Op::LoadString => {
                let dst = context
                    .exec_register(instr, 0)
                    .ok_or(VmError::InvalidOperand)?;
                let idx = context
                    .exec_const_index(instr, 1)
                    .ok_or(VmError::InvalidOperand)?;
                let value = self.load_string_constant_value(context, idx)?;
                let frame = &mut stack[frame_index];
                write_register(frame, dst, value)
            }
            Op::LoadBigInt => {
                let dst = context
                    .exec_register(instr, 0)
                    .ok_or(VmError::InvalidOperand)?;
                let idx = context
                    .exec_const_index(instr, 1)
                    .ok_or(VmError::InvalidOperand)?;
                let value = self.load_bigint_constant_value(context, idx)?;
                let frame = &mut stack[frame_index];
                write_register(frame, dst, value)
            }
            Op::LoadNumber => {
                let dst = context
                    .exec_register(instr, 0)
                    .ok_or(VmError::InvalidOperand)?;
                let idx = context
                    .exec_const_index(instr, 1)
                    .ok_or(VmError::InvalidOperand)?;
                let bits = context
                    .number_constant_bits(idx)
                    .ok_or(VmError::InvalidOperand)?;
                let value = crate::NumberValue::from_f64(f64::from_bits(bits));
                let frame = &mut stack[frame_index];
                write_register(frame, dst, Value::number(value))
            }
            Op::MathCall => {
                let operands = context.exec_operands(instr);
                self.do_math_call(stack, context, operands)
            }
            Op::DefineDataProperty => {
                let (obj_reg, key_reg, value_reg) = context
                    .exec_register3(instr)
                    .ok_or(VmError::InvalidOperand)?;
                self.run_define_data_property_regs(
                    context,
                    stack,
                    frame_index,
                    obj_reg,
                    key_reg,
                    value_reg,
                )
            }
            Op::FreshUpvalue => {
                let idx = context
                    .exec_imm32(instr, 0)
                    .ok_or(VmError::InvalidOperand)?;
                let frame = &mut stack[frame_index];
                self.run_fresh_upvalue_reg(frame, idx)
            }
            Op::LoadBuiltinError => {
                let dst = context
                    .exec_register(instr, 0)
                    .ok_or(VmError::InvalidOperand)?;
                let kind_idx = context
                    .exec_const_index(instr, 1)
                    .ok_or(VmError::InvalidOperand)?;
                let frame = &mut stack[frame_index];
                self.run_load_builtin_error_reg(context, frame, dst, kind_idx)
            }
            Op::Neg => {
                let dst = context
                    .exec_register(instr, 0)
                    .ok_or(VmError::InvalidOperand)?;
                let src = context
                    .exec_register(instr, 1)
                    .ok_or(VmError::InvalidOperand)?;
                let frame = &mut stack[frame_index];
                self.run_neg_regs(frame, dst, src)
            }
            Op::DefineOwnProperty => {
                let operands = context.exec_operands(instr);
                self.run_define_own_property_operands(context, stack, operands)
            }
            Op::LooseEqual | Op::LooseNotEqual => {
                let (dst, lhs, rhs) = context
                    .exec_register3(instr)
                    .ok_or(VmError::InvalidOperand)?;
                let negate = matches!(op, Op::LooseNotEqual);
                let frame = &mut stack[frame_index];
                self.run_loose_equal_regs(context, frame, dst, lhs, rhs, negate)
            }
            _ => Err(VmError::InvalidOperand),
        };
        stack[frame_index].pc = saved_pc;
        result
    }

    /// JIT bridge for `NewObject` from compiled code. This bypasses the generic
    /// opcode delegate but still uses the shared stack-rooted allocator, so a
    /// young-generation scavenge can rewrite live frame registers before the
    /// object handle is published back into `dst`.
    ///
    /// # Errors
    /// Propagates allocation failures.
    pub fn jit_runtime_new_object(
        &mut self,
        stack: &mut HoltStack,
        frame_index: usize,
        dst: u16,
    ) -> Result<(), VmError> {
        let saved_pc = stack[frame_index].pc;
        let result = self.run_new_object_reg(stack, frame_index, dst);
        stack[frame_index].pc = saved_pc;
        result
    }

    /// JIT bridge for `NewArray` from compiled code. This keeps array literal
    /// allocation on the shared stack-rooted path while bypassing the generic
    /// opcode delegate envelope; the bytecode operands still define the exact
    /// source-register list.
    ///
    /// # Errors
    /// Propagates invalid operands and allocation failures.
    pub fn jit_runtime_new_array(
        &mut self,
        context: &ExecutionContext,
        stack: &mut HoltStack,
        frame_index: usize,
        byte_pc: u32,
    ) -> Result<(), VmError> {
        let fid = stack[frame_index].function_id;
        let func = context.exec_function(fid).ok_or(VmError::InvalidOperand)?;
        let instr = func
            .instr_at_byte_pc(byte_pc)
            .ok_or(VmError::InvalidOperand)?;
        if instr.op() != Op::NewArray {
            return Err(VmError::InvalidOperand);
        }
        let operands = context.exec_operands(instr);
        let saved_pc = stack[frame_index].pc;
        let result = self.run_new_array_operands(stack, frame_index, operands);
        stack[frame_index].pc = saved_pc;
        result
    }

    /// JIT bridge for `LoadString` from compiled code. The constant cache is
    /// VM-owned and traced, so compiled code asks the VM to materialize the
    /// literal instead of embedding a GC pointer that a moving collection could
    /// make stale.
    ///
    /// # Errors
    /// Propagates invalid operands and allocation failures.
    pub fn jit_runtime_load_string(
        &mut self,
        context: &ExecutionContext,
        stack: &mut HoltStack,
        frame_index: usize,
        byte_pc: u32,
    ) -> Result<(), VmError> {
        let fid = stack[frame_index].function_id;
        let func = context.exec_function(fid).ok_or(VmError::InvalidOperand)?;
        let instr = func
            .instr_at_byte_pc(byte_pc)
            .ok_or(VmError::InvalidOperand)?;
        if instr.op() != Op::LoadString {
            return Err(VmError::InvalidOperand);
        }
        let dst = context
            .exec_register(instr, 0)
            .ok_or(VmError::InvalidOperand)?;
        let idx = context
            .exec_const_index(instr, 1)
            .ok_or(VmError::InvalidOperand)?;
        let saved_pc = stack[frame_index].pc;
        let value = self.load_string_constant_value(context, idx)?;
        let result = write_register(&mut stack[frame_index], dst, value);
        stack[frame_index].pc = saved_pc;
        result
    }

    /// JIT bridge for a computed `StoreElement` (`recv[idx] = src`) from compiled
    /// code. Mirrors the interpreter dispatch: the typed-array / dense-array fast
    /// path ([`Self::drive_store_element`]) first, then the general
    /// [`Self::run_store_element_regs`] (`[[Set]]` over arrays, objects, string
    /// exotics). `scratch_reg` is the bytecode's scratch operand. Frame PC is
    /// saved/restored so a later guard bail re-runs from PC 0.
    ///
    /// # Errors
    /// Propagates a throwing setter, a read-only-target `TypeError` in strict
    /// mode, and `InvalidOperand`.
    pub fn jit_runtime_store_element(
        &mut self,
        context: &ExecutionContext,
        stack: &mut HoltStack,
        frame_index: usize,
        recv_reg: u16,
        idx_reg: u16,
        src_reg: u16,
        scratch_reg: u16,
    ) -> Result<(), VmError> {
        self.record_jit_runtime_property_stub();
        let receiver = *read_register(&stack[frame_index], recv_reg)?;
        let key_value = *read_register(&stack[frame_index], idx_reg)?;
        let value = *read_register(&stack[frame_index], src_reg)?;
        if let Some(arr) = receiver.as_array()
            && let Some(n) = key_value.as_number()
            && let Some(idx) = crate::array::index_from_number(n)
            && crate::array::can_fast_fill_dense_range(arr, &self.gc_heap, idx, idx + 1)
        {
            let strict = context.function_is_strict(stack[frame_index].function_id);
            if !self.array_index_store_via_proto(context, arr, idx, value, strict)? {
                if strict {
                    let key = idx.to_string();
                    self.array_strict_write_guard(arr, &key, true)?;
                }
                let roots = self.collect_allocation_roots(stack);
                let mut external_visit = |visitor: &mut dyn FnMut(*mut RawGc)| {
                    for &slot in &roots {
                        visitor(slot);
                    }
                };
                crate::array::fill_dense_range_with_roots(
                    arr,
                    &mut self.gc_heap,
                    idx,
                    idx + 1,
                    value,
                    &mut external_visit,
                )?;
            }
            return Ok(());
        }
        let saved_pc = stack[frame_index].pc;
        let operands = [
            Operand::Register(recv_reg),
            Operand::Register(idx_reg),
            Operand::Register(src_reg),
            Operand::Register(scratch_reg),
        ];
        let result = match self.drive_store_element(stack, context, &operands) {
            Ok(true) => Ok(()),
            Ok(false) => {
                self.run_store_element_regs(context, stack, frame_index, recv_reg, idx_reg, src_reg)
            }
            Err(err) => Err(err),
        };
        stack[frame_index].pc = saved_pc;
        result
    }

    /// JIT bridge for a named `StoreProperty` from compiled code. The store
    /// analogue of [`Self::jit_runtime_load_property`]: the hot path is an IC
    /// hit on an existing own data slot (the dense `site` comes from the
    /// snapshot, since the compiled frame runs at PC 0). A miss — cold or
    /// polymorphic shape, a property that must be added (shape transition), an
    /// accessor setter, or a non-extensible / non-object receiver — falls back
    /// to the full write, with the frame PC saved and restored so a later guard
    /// bail still re-runs from PC 0.
    ///
    /// # Errors
    /// Propagates write failures (read-only target in strict mode, throwing
    /// setter) and `InvalidOperand` for an unknown property-name index.
    pub fn jit_runtime_store_property(
        &mut self,
        context: &ExecutionContext,
        stack: &mut HoltStack,
        frame_index: usize,
        obj_reg: u16,
        name_idx: u32,
        src: u16,
        site: usize,
    ) -> Result<u64, VmError> {
        self.record_jit_runtime_property_stub();
        let atomized_key = context
            .property_atom_for_function(stack[frame_index].function_id, name_idx)
            .ok_or(VmError::InvalidOperand)?;
        let receiver = *read_register(&stack[frame_index], obj_reg)?;
        let value = *read_register(&stack[frame_index], src)?;
        if let Some(obj) = receiver.as_object()
            && site < self.store_property_ics.len()
            && object::supports_fast_property_ic(obj, &self.gc_heap)
        {
            let entries_len = self.store_property_ics[site].entry_count();
            let mut store_hit = false;
            for idx in 0..entries_len {
                // Reborrow each iteration: `store` needs `&mut self.gc_heap`,
                // which conflicts with a long-lived borrow of the entries slice.
                let ic = self.store_property_ics[site].entries()[idx].clone();
                if ic
                    .run_store(obj, &mut self.gc_heap, atomized_key, &value)
                    .is_some()
                {
                    store_hit = true;
                    break;
                }
            }
            if store_hit {
                self.property_ic_stats.record_hit(PropertyIcKind::Store);
                // Report a monomorphic existing-own-data inline-slot fill so the
                // emitted site can self-patch its WhiskerIC cell and inline
                // subsequent stores (shape guard + slot write + value-gated
                // barrier) without re-entering this stub.
                return Ok(
                    self.whisker_store_cell_fill(site, object::shape(obj, &self.gc_heap).offset())
                );
            }
            if entries_len > 0 {
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
            if !self.store_property_ics[site].is_megamorphic()
                && let Some(ic) =
                    cache_ir::CacheStub::install_store_existing(obj, &self.gc_heap, atomized_key)
                && ic
                    .run_store(obj, &mut self.gc_heap, atomized_key, &value)
                    .is_some()
            {
                self.store_property_ics[site].install_with_stats(
                    &mut self.property_ic_stats,
                    PropertyIcKind::Store,
                    ic,
                );
                return Ok(
                    self.whisker_store_cell_fill(site, object::shape(obj, &self.gc_heap).offset())
                );
            }
        }
        // Slow path: full `[[Set]]` (transition / accessor / reject). It
        // advances the interpreter PC, which compiled code must not observe.
        let saved_pc = stack[frame_index].pc;
        let result =
            self.run_store_property_reg(context, stack, frame_index, obj_reg, atomized_key, src);
        stack[frame_index].pc = saved_pc;
        result.map(|()| 0)
    }

    /// Packed WhiskerIC inline-store cell fill for `site`, or `0` for "no
    /// inline". Same encoding as [`Self::whisker_load_cell_fill`]: low 32 =
    /// cached shape-handle offset (non-zero validity token), high 32 = value
    /// slab byte offset. Only a warm, single-entry `ExistingOwnDataStore` IC
    /// qualifies — an add-transition store mutates the shape and grows the
    /// value slab, so it stays on the stub. The shape guard the emitted site
    /// keeps also guarantees the slot is the writable data slot the IC captured
    /// (a shape encodes per-slot flags and key), so the inline write is sound.
    fn whisker_store_cell_fill(&self, site: usize, recv_shape: u32) -> u64 {
        if recv_shape == 0 {
            return 0;
        }
        for ic in self.store_property_ics[site].entries() {
            if let Some(hit) = ic.store_own_data_hit()
                && hit.shape.offset() == recv_shape
            {
                let value_byte = u32::from(hit.slot)
                    * std::mem::size_of::<crate::value::compressed::CompressedValue>() as u32;
                return (u64::from(value_byte) << 32) | u64::from(hit.shape.offset());
            }
        }
        0
    }

    /// JIT bridge: run the GC write barrier after an inline `StoreProperty`
    /// wrote a heap-pointer value into `obj_reg`'s object slab or dense array
    /// storage. The emitted
    /// fast path already performed the slot store and only calls here when the
    /// stored value is a pointer (primitive stores need no barrier), so this
    /// just marks the parent container's card for the old→young edge.
    pub fn jit_runtime_write_barrier(
        &mut self,
        stack: &HoltStack,
        frame_index: usize,
        obj_reg: u16,
        src: u16,
    ) {
        let Ok(receiver) = read_register(&stack[frame_index], obj_reg) else {
            return;
        };
        let Ok(value) = read_register(&stack[frame_index], src) else {
            return;
        };
        if let Some(obj) = receiver.as_object() {
            self.gc_heap.record_write(obj, value);
        } else if let Some(arr) = receiver.as_array() {
            self.gc_heap.record_write(arr, value);
        }
    }

    /// Frameless generational write barrier — the
    /// [`Self::jit_runtime_write_barrier`] counterpart that reads the parent and
    /// child from the register window instead of a `HoltStack` frame.
    ///
    /// # Safety
    /// `regs` must point at a live, GC-traced register window covering
    /// `max(obj_reg, src) + 1` slots.
    pub unsafe fn jit_runtime_write_barrier_window(
        &mut self,
        regs: *mut u64,
        obj_reg: u16,
        src: u16,
    ) {
        let receiver = Value::from_bits(unsafe { *regs.add(obj_reg as usize) });
        let value = Value::from_bits(unsafe { *regs.add(src as usize) });
        if let Some(obj) = receiver.as_object() {
            self.gc_heap.record_write(obj, &value);
        } else if let Some(arr) = receiver.as_array() {
            self.gc_heap.record_write(arr, &value);
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
        stack: &mut HoltStack,
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
        let receiver = *read_register(&stack[top_idx], obj_reg)?;
        if let Some(obj) = receiver.as_object() {
            let site = context
                .property_ic_site(stack[top_idx].function_id, stack[top_idx].pc)
                .ok_or(VmError::InvalidOperand)?;
            let mut site_disabled = self.load_property_ics[site].is_megamorphic();
            let entries = self.load_property_ics[site].entries();
            let mut hit_value: Option<Value> = None;
            for ic in entries {
                if let Some(value) = ic.run_load(obj, &self.gc_heap, atomized_key) {
                    hit_value = Some(value);
                    break;
                }
            }
            if let Some(value) = hit_value {
                self.property_ic_stats.record_hit(PropertyIcKind::Load);
                Self::finish_property_fast_path_value(
                    &mut stack[top_idx],
                    dst,
                    value,
                    self.current_byte_len,
                )?;
                return Ok(true);
            }
            if self.load_property_ics[site].entry_count() > 0 {
                self.load_property_ics[site].record_guard_miss_with_stats(
                    &mut self.property_ic_stats,
                    PropertyIcKind::Load,
                );
                site_disabled = self.load_property_ics[site].is_megamorphic();
            } else {
                self.load_property_ics[site].record_uncached_miss_with_stats(
                    &mut self.property_ic_stats,
                    PropertyIcKind::Load,
                );
            }
            if !site_disabled
                && let Some((ic, value)) =
                    cache_ir::CacheStub::install_load(obj, &self.gc_heap, atomized_key)
            {
                self.load_property_ics[site].install_with_stats(
                    &mut self.property_ic_stats,
                    PropertyIcKind::Load,
                    ic,
                );
                Self::finish_property_fast_path_value(
                    &mut stack[top_idx],
                    dst,
                    value,
                    self.current_byte_len,
                )?;
                return Ok(true);
            }
            let key = VmPropertyKey::atom(atomized_key);
            stack[top_idx].advance_pc(self.current_byte_len)?;
            match self.ordinary_get_value(
                context,
                Value::object(obj),
                Value::object(obj),
                &key,
                0,
            )? {
                VmGetOutcome::Value(value) => write_register(&mut stack[top_idx], dst, value)?,
                VmGetOutcome::InvokeGetter { getter } => {
                    if abstract_ops::is_callable(&getter) {
                        let args: SmallVec<[Value; 8]> = SmallVec::new();
                        self.invoke(stack, context, &getter, Value::object(obj), args, dst)?;
                    } else {
                        write_register(&mut stack[top_idx], dst, Value::undefined())?;
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
        if receiver.is_proxy()
            || receiver.is_generator()
            || receiver.is_iterator()
            || receiver.is_map()
            || receiver.is_set()
            || receiver.is_weak_map()
            || receiver.is_weak_set()
            || receiver.is_weak_ref()
            || receiver.is_finalization_registry()
            || receiver.is_promise()
            || receiver.is_array_buffer()
            || receiver.is_data_view()
        {
            let key = VmPropertyKey::atom(atomized_key);
            stack[top_idx].advance_pc(self.current_byte_len)?;
            match self.ordinary_get_value(context, receiver, receiver, &key, 0)? {
                VmGetOutcome::Value(value) => write_register(&mut stack[top_idx], dst, value)?,
                VmGetOutcome::InvokeGetter { getter } => {
                    if abstract_ops::is_callable(&getter) {
                        let args: SmallVec<[Value; 8]> = SmallVec::new();
                        self.invoke(stack, context, &getter, receiver, args, dst)?;
                    } else {
                        write_register(&mut stack[top_idx], dst, Value::undefined())?;
                    }
                }
            }
            return Ok(true);
        }
        if receiver.is_boolean()
            || receiver.is_number()
            || receiver.is_string()
            || receiver.is_symbol()
            || receiver.is_big_int()
        {
            let boxed = self.box_sloppy_this_primitive_stack_rooted(stack, receiver, &[])?;
            let key = VmPropertyKey::atom(atomized_key);
            stack[top_idx].advance_pc(self.current_byte_len)?;
            match self.ordinary_get_value(context, boxed, receiver, &key, 0)? {
                VmGetOutcome::Value(value) => write_register(&mut stack[top_idx], dst, value)?,
                VmGetOutcome::InvokeGetter { getter } => {
                    if abstract_ops::is_callable(&getter) {
                        let args: SmallVec<[Value; 8]> = SmallVec::new();
                        self.invoke(stack, context, &getter, receiver, args, dst)?;
                    } else {
                        write_register(&mut stack[top_idx], dst, Value::undefined())?;
                    }
                }
            }
            return Ok(true);
        }
        if let Some(bound) = receiver.as_bound_function() {
            let bound = &bound;
            match function_metadata::bound_own_property_descriptor(bound, &mut self.gc_heap, name)?
            {
                Some(object::PropertyDescriptor {
                    kind: object::DescriptorKind::Accessor { getter, .. },
                    ..
                }) => {
                    stack[top_idx].advance_pc(self.current_byte_len)?;
                    match getter {
                        Some(callee) if abstract_ops::is_callable(&callee) => {
                            let args: SmallVec<[Value; 8]> = SmallVec::new();
                            self.invoke(stack, context, &callee, receiver, args, dst)?;
                        }
                        _ => write_register(&mut stack[top_idx], dst, Value::undefined())?,
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
                        name,
                    ) {
                        stack[top_idx].advance_pc(self.current_byte_len)?;
                        match getter {
                            Some(callee) if abstract_ops::is_callable(&callee) => {
                                let args: SmallVec<[Value; 8]> = SmallVec::new();
                                self.invoke(stack, context, &callee, receiver, args, dst)?;
                            }
                            _ => write_register(&mut stack[top_idx], dst, Value::undefined())?,
                        }
                        return Ok(true);
                    }
                    if is_restricted_function_property(name) {
                        stack[top_idx].advance_pc(self.current_byte_len)?;
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
        if receiver.is_function()
            || receiver.is_closure()
            || receiver.is_native_function()
            || receiver.is_class_constructor()
        {
            let own_present = if let Some(fid) = receiver.as_function().or_else(|| {
                receiver
                    .as_closure(&self.gc_heap)
                    .map(|c| c.cached_function_id)
            }) {
                let owner = receiver.as_closure(&self.gc_heap);
                let bag_has = self.callable_bag_read(owner, fid).is_some_and(|bag| {
                    !matches!(
                        object::lookup_own(bag, &self.gc_heap, name),
                        object::PropertyLookup::Absent
                    )
                });
                // Virtual own properties (metadata-backed `name` /
                // `length`, lazily-materialized `prototype`) shadow
                // %Function.prototype% — §13.2 step 18: an accessor
                // named `prototype` installed there must never fire
                // for an ordinary function.
                let metadata_has = self
                    .ordinary_function_own_property_descriptor(None, owner, fid, name)
                    .ok()
                    .flatten()
                    .is_some();
                let prototype_implicit = name == "prototype"
                    && context.function_has_prototype_property(fid)
                    && !self.function_deleted_metadata.contains(&(fid, "prototype"));
                bag_has || metadata_has || prototype_implicit
            } else if let Some(c) = receiver.as_class_constructor() {
                // Class constructors expose `prototype` / `name` /
                // `length` virtually when no static shadows them.
                matches!(name, "prototype" | "name" | "length")
                    || !matches!(
                        object::lookup_own(c.statics(&self.gc_heap), &self.gc_heap, name),
                        object::PropertyLookup::Absent
                    )
            } else if let Some(native) = receiver.as_native_function() {
                native
                    .own_property_descriptor(&mut self.gc_heap, name)?
                    .is_some()
            } else {
                false
            };
            if !own_present {
                let proto = self.function_prototype_object()?;
                if let object::PropertyLookup::Accessor { getter, .. } =
                    object::lookup(proto, &self.gc_heap, name)
                {
                    stack[top_idx].advance_pc(self.current_byte_len)?;
                    match getter {
                        Some(callee) if abstract_ops::is_callable(&callee) => {
                            let args: SmallVec<[Value; 8]> = SmallVec::new();
                            self.invoke(stack, context, &callee, receiver, args, dst)?;
                        }
                        _ => write_register(&mut stack[top_idx], dst, Value::undefined())?,
                    }
                    return Ok(true);
                }
            }
        }
        let obj = if let Some(o) = receiver.as_object() {
            o
        } else if let Some(c) = receiver.as_class_constructor() {
            c.statics(&self.gc_heap)
        } else if let Some(fid) = receiver.as_function().or_else(|| {
            receiver
                .as_closure(&self.gc_heap)
                .map(|c| c.cached_function_id)
        }) {
            let owner = receiver.as_closure(&self.gc_heap);
            match self.callable_bag_read(owner, fid) {
                Some(bag) => bag,
                None => self.function_user_bag_with_stack_roots(stack, owner, fid, &[&receiver])?,
            }
        } else {
            return Ok(false);
        };
        match crate::object::lookup(obj, &self.gc_heap, name) {
            object::PropertyLookup::Accessor { getter, .. } => {
                stack[top_idx].advance_pc(self.current_byte_len)?;
                match getter {
                    Some(callee) if abstract_ops::is_callable(&callee) => {
                        let args: SmallVec<[Value; 8]> = SmallVec::new();
                        self.invoke(stack, context, &callee, receiver, args, dst)?;
                    }
                    _ => {
                        // §10.1.8.1 step 4.b — undefined.
                        write_register(&mut stack[top_idx], dst, Value::undefined())?;
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
        stack: &mut HoltStack,
        context: &ExecutionContext,
        operands: &[Operand],
    ) -> Result<bool, VmError> {
        let dst = register_operand(operands.first())?;
        let lhs_reg = register_operand(operands.get(1))?;
        let rhs_reg = register_operand(operands.get(2))?;
        let top_idx = stack.len() - 1;
        let lhs = *read_register(&stack[top_idx], lhs_reg)?;
        let rhs = *read_register(&stack[top_idx], rhs_reg)?;
        let result = self.instanceof_operator_stack_rooted(context, stack, &lhs, &rhs)?;
        stack[top_idx].advance_pc(self.current_byte_len)?;
        write_register(&mut stack[top_idx], dst, Value::boolean(result))?;
        Ok(true)
    }

    /// Drive one tick of [`Op::LoadElement`] for computed ordinary
    /// object/proxy reads whose resolved descriptor is an accessor.
    pub(crate) fn drive_load_element(
        &mut self,
        stack: &mut HoltStack,
        context: &ExecutionContext,
        operands: &[Operand],
    ) -> Result<bool, VmError> {
        let dst = register_operand(operands.first())?;
        let obj_reg = register_operand(operands.get(1))?;
        let key_reg = register_operand(operands.get(2))?;
        let top_idx = stack.len() - 1;
        let receiver = *read_register(&stack[top_idx], obj_reg)?;
        let key_value_raw = *read_register(&stack[top_idx], key_reg)?;
        if receiver.is_nullish() {
            return Err(
                self.err_type(("Cannot read property of null or undefined".to_string()).into())
            );
        }
        let key_value = self.coerce_property_key_value(context, key_value_raw)?;
        write_register(&mut stack[top_idx], key_reg, key_value)?;
        let key = if let Some(s) = key_value.as_string(&self.gc_heap) {
            VmPropertyKey::OwnedString(s.to_lossy_string(&self.gc_heap))
        } else if let Some(n) = key_value.as_number() {
            VmPropertyKey::OwnedString(n.to_display_string())
        } else if let Some(sym) = key_value.as_symbol(&self.gc_heap) {
            VmPropertyKey::Symbol(sym)
        } else {
            return Ok(false);
        };

        // Heap values that walk a prototype chain via `ordinary_get_value`.
        let prototype_routed = receiver.is_object()
            || receiver.is_proxy()
            || receiver.is_generator()
            || receiver.is_iterator()
            || receiver.is_map()
            || receiver.is_set()
            || receiver.is_weak_map()
            || receiver.is_weak_set()
            || receiver.is_weak_ref()
            || receiver.is_finalization_registry()
            || receiver.is_promise()
            || receiver.is_array_buffer()
            || receiver.is_typed_array()
            || receiver.is_class_constructor()
            || receiver.as_native_function().is_some()
            || receiver.is_data_view();
        if prototype_routed {
            stack[top_idx].advance_pc(self.current_byte_len)?;
            match self.ordinary_get_value(context, receiver, receiver, &key, 0)? {
                VmGetOutcome::Value(value) => write_register(&mut stack[top_idx], dst, value)?,
                VmGetOutcome::InvokeGetter { getter } => {
                    if abstract_ops::is_callable(&getter) {
                        let args: SmallVec<[Value; 8]> = SmallVec::new();
                        self.invoke(stack, context, &getter, receiver, args, dst)?;
                    } else {
                        write_register(&mut stack[top_idx], dst, Value::undefined())?;
                    }
                }
            }
            return Ok(true);
        }

        if let (Some(bound), Some(key)) = (receiver.as_bound_function(), key.string_name()) {
            let bound = &bound;
            match function_metadata::bound_own_property_descriptor(bound, &mut self.gc_heap, key)? {
                Some(object::PropertyDescriptor {
                    kind: object::DescriptorKind::Accessor { getter, .. },
                    ..
                }) => {
                    stack[top_idx].advance_pc(self.current_byte_len)?;
                    match getter {
                        Some(callee) if abstract_ops::is_callable(&callee) => {
                            let args: SmallVec<[Value; 8]> = SmallVec::new();
                            self.invoke(stack, context, &callee, receiver, args, dst)?;
                        }
                        _ => write_register(&mut stack[top_idx], dst, Value::undefined())?,
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
                        stack[top_idx].advance_pc(self.current_byte_len)?;
                        match getter {
                            Some(callee) if abstract_ops::is_callable(&callee) => {
                                let args: SmallVec<[Value; 8]> = SmallVec::new();
                                self.invoke(stack, context, &callee, receiver, args, dst)?;
                            }
                            _ => write_register(&mut stack[top_idx], dst, Value::undefined())?,
                        }
                        return Ok(true);
                    }
                    if is_restricted_function_property(key) {
                        stack[top_idx].advance_pc(self.current_byte_len)?;
                        let callee = self.restricted_throw_type_error()?;
                        let args: SmallVec<[Value; 8]> = SmallVec::new();
                        self.invoke(stack, context, &callee, receiver, args, dst)?;
                        return Ok(true);
                    }
                }
            }
        }

        let obj = if let Some(o) = receiver.as_object() {
            o
        } else if let Some(class) = receiver.as_class_constructor() {
            if key.string_name().is_some_and(|key| key == "prototype") {
                stack[top_idx].advance_pc(self.current_byte_len)?;
                write_register(
                    &mut stack[top_idx],
                    dst,
                    Value::object(class.prototype(&self.gc_heap)),
                )?;
                return Ok(true);
            }
            class.statics(&self.gc_heap)
        } else if let Some(fid) = receiver.as_function().or_else(|| {
            receiver
                .as_closure(&self.gc_heap)
                .map(|c| c.cached_function_id)
        }) {
            let owner = receiver.as_closure(&self.gc_heap);
            let Some(bag) = self.callable_bag_read(owner, fid) else {
                return Ok(false);
            };
            bag
        } else {
            return Ok(false);
        };
        let lookup = match &key {
            VmPropertyKey::Symbol(sym) => crate::object::lookup_symbol(obj, &self.gc_heap, *sym),
            _ => crate::object::lookup(
                obj,
                &self.gc_heap,
                key.string_name()
                    .expect("non-symbol key has string spelling"),
            ),
        };
        match lookup {
            object::PropertyLookup::Data { value, .. } => {
                stack[top_idx].advance_pc(self.current_byte_len)?;
                write_register(&mut stack[top_idx], dst, value)?;
                Ok(true)
            }
            object::PropertyLookup::Accessor { getter, .. } => {
                stack[top_idx].advance_pc(self.current_byte_len)?;
                match getter {
                    Some(callee) if abstract_ops::is_callable(&callee) => {
                        let args: SmallVec<[Value; 8]> = SmallVec::new();
                        self.invoke(stack, context, &callee, receiver, args, dst)?;
                    }
                    _ => {
                        write_register(&mut stack[top_idx], dst, Value::undefined())?;
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

    fn current_frame_is_strict(stack: &HoltStack, context: &ExecutionContext) -> bool {
        stack
            .last()
            .is_some_and(|frame| Self::function_is_strict(context, frame.function_id))
    }

    /// §15.7.14 — `C.prototype`, `C.name`, and `C.length` are
    /// non-writable own properties of a class constructor. The class
    /// value keeps them virtually (delegated to the inner callable's
    /// metadata), so a plain write into the statics object would mint
    /// a shadowing own slot instead of rejecting. Returns `true` when
    /// the named store must fail; an own statics property (a static
    /// method shadow or a post-delete re-creation) falls back to the
    /// ordinary descriptor-aware path.
    fn class_store_hits_readonly_intrinsic(
        &mut self,
        context: &ExecutionContext,
        class: crate::class_constructor::ClassConstructor,
        name: &str,
    ) -> Result<bool, VmError> {
        if name == "prototype" {
            return Ok(true);
        }
        if function_metadata::ordinary_function_metadata_key(name).is_none() {
            return Ok(false);
        }
        let statics = class.statics(&self.gc_heap);
        if crate::object::get_own_descriptor(statics, &self.gc_heap, name).is_some() {
            return Ok(false);
        }
        let ctor = class.ctor(&self.gc_heap);
        if let Some(fid) = ctor
            .as_function()
            .or_else(|| ctor.as_closure(&self.gc_heap).map(|c| c.cached_function_id))
        {
            let owner = ctor.as_closure(&self.gc_heap);
            return Ok(self
                .ordinary_function_own_property_descriptor(Some(context), owner, fid, name)?
                .is_some_and(|desc| !desc.writable()));
        }
        Ok(true)
    }

    fn finish_failed_set(
        &self,
        stack: &mut HoltStack,
        context: &ExecutionContext,
        message: impl Into<Box<str>>,
        byte_len: u32,
    ) -> Result<bool, VmError> {
        if Self::current_frame_is_strict(stack, context) {
            return Err(self.err_type(message.into()));
        }
        let top_idx = stack.len() - 1;
        stack[top_idx].advance_pc(byte_len)?;
        Ok(true)
    }

    fn failed_set_result(&self, strict: bool, message: impl Into<Box<str>>) -> Result<(), VmError> {
        if strict {
            Err(self.err_type(message.into()))
        } else {
            Ok(())
        }
    }

    fn advance_property_fast_path(frame: &mut Frame, byte_len: u32) -> Result<(), VmError> {
        frame.advance_pc(byte_len)
    }

    fn finish_property_fast_path_value(
        frame: &mut Frame,
        dst: u16,
        value: Value,
        byte_len: u32,
    ) -> Result<(), VmError> {
        Self::advance_property_fast_path(frame, byte_len)?;
        write_register(frame, dst, value)
    }

    fn store_to_primitive_base(
        &mut self,
        stack: &mut HoltStack,
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
            if let Some(obj) = proto.as_object() {
                {
                    let lookup = match &key {
                        VmPropertyKey::Symbol(sym) => {
                            object::lookup_own_symbol(obj, &self.gc_heap, *sym)
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
                                self.failed_set_result(
                                    strict,
                                    format!("Cannot assign to read-only property '{name}'"),
                                )?;
                            } else {
                                let name = key.string_name().unwrap_or("symbol");
                                self.failed_set_result(
                                    strict,
                                    format!("Cannot assign to property '{name}' on primitive"),
                                )?;
                            }
                            let top_idx = stack.len() - 1;
                            stack[top_idx].advance_pc(self.current_byte_len)?;
                            return Ok(true);
                        }
                        object::PropertyLookup::Accessor { setter, .. } => {
                            let Some(setter) = setter else {
                                self.failed_set_result(
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
                            stack[top_idx].advance_pc(self.current_byte_len)?;
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
            } else if let Some(proxy) = proto.as_proxy() {
                {
                    let key_value = self.vm_property_key_to_value(&key)?;
                    let trap_args: SmallVec<[Value; 8]> = smallvec::smallvec![
                        proxy.target(&self.gc_heap),
                        key_value,
                        value,
                        receiver
                    ];
                    let top_idx = stack.len() - 1;
                    stack[top_idx].advance_pc(self.current_byte_len)?;
                    match self.invoke_proxy_trap(context, &proxy, "set", trap_args)? {
                        Some(_) => {}
                        None => {
                            let Some(target) = proxy.target(&self.gc_heap).as_object() else {
                                return Err(VmError::TypeMismatch);
                            };
                            match &key {
                                VmPropertyKey::Symbol(sym) => {
                                    match object::resolve_symbol_set(target, &self.gc_heap, *sym) {
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
                                            self.failed_set_result(
                                                strict,
                                                "Cannot assign to symbol property",
                                            )?;
                                        }
                                        object::SetOutcome::ExoticParent { parent } => {
                                            if !self.ordinary_set_data_value(
                                                context,
                                                parent,
                                                &VmPropertyKey::Symbol(*sym),
                                                value,
                                                receiver,
                                                1,
                                            )? {
                                                self.failed_set_result(
                                                    strict,
                                                    "Cannot assign to symbol property",
                                                )?;
                                            }
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
                                            self.failed_set_result(
                                                strict,
                                                format!("Cannot assign to property '{key}'"),
                                            )?;
                                        }
                                        object::SetOutcome::ExoticParent { parent } => {
                                            if !self.ordinary_set_data_value(
                                                context,
                                                parent,
                                                &VmPropertyKey::String(key),
                                                value,
                                                receiver,
                                                1,
                                            )? {
                                                self.failed_set_result(
                                                    strict,
                                                    format!("Cannot assign to property '{key}'"),
                                                )?;
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                    return Ok(true);
                }
            } else {
                break;
            }
        }

        let top_idx = stack.len() - 1;
        let name = key.string_name().unwrap_or("symbol");
        self.failed_set_result(
            strict,
            format!("Cannot assign to property '{name}' on primitive"),
        )?;
        stack[top_idx].advance_pc(self.current_byte_len)?;
        Ok(true)
    }

    /// Drive one tick of [`Op::StoreElement`] when a computed
    /// string, numeric, or symbol property write on an ordinary
    /// object/proxy must obey §10.1.9 OrdinarySet.
    pub(crate) fn drive_store_element(
        &mut self,
        stack: &mut HoltStack,
        context: &ExecutionContext,
        operands: &[Operand],
    ) -> Result<bool, VmError> {
        let obj_reg = register_operand(operands.first())?;
        let key_reg = register_operand(operands.get(1))?;
        let src_reg = register_operand(operands.get(2))?;
        let scratch_reg = register_operand(operands.get(3))?;
        let top_idx = stack.len() - 1;
        let receiver = *read_register(&stack[top_idx], obj_reg)?;
        let key_value_raw = *read_register(&stack[top_idx], key_reg)?;
        let key_value = self.coerce_property_key_value(context, key_value_raw)?;
        let value = *read_register(&stack[top_idx], src_reg)?;

        // Fast path: an integer-keyed write of an already-Number value into a
        // non-BigInt typed array is an IntegerIndexedElementSet (§10.4.5.5)
        // with no observable coercion — `ToNumber(number)` is identity, never
        // throws, and has no side effects, so the value conversion folds into
        // the in-place element write with no index stringification and no
        // expando (a canonical numeric index never creates one).
        // `JsTypedArray::set` truncates per element kind and silently no-ops on
        // a detached buffer or out-of-bounds index; a negative / fractional
        // index has no valid integer index and is likewise a no-op. BigInt
        // arrays, non-Number values, and observable coercions (which can throw
        // or resize the buffer) fall through to the spec `[[Set]]` ladder.
        if let Some(t) = receiver.as_typed_array(&self.gc_heap)
            && value.is_number()
            && !t.kind().is_bigint()
            && let Some(n) = key_value.as_number()
        {
            if let Some(idx) = crate::array::index_from_number(n) {
                t.set(&mut self.gc_heap, idx, &value);
            }
            stack[top_idx].advance_pc(self.current_byte_len)?;
            return Ok(true);
        }

        let strict = Self::current_frame_is_strict(stack, context);
        enum ComputedPropertyKey {
            String(String),
            Symbol(crate::symbol::JsSymbol),
        }
        let key = if let Some(s) = key_value.as_string(&self.gc_heap) {
            ComputedPropertyKey::String(s.to_lossy_string(&self.gc_heap))
        } else if let Some(n) = key_value.as_number() {
            ComputedPropertyKey::String(n.to_display_string())
        } else if let Some(sym) = key_value.as_symbol(&self.gc_heap) {
            ComputedPropertyKey::Symbol(sym)
        } else {
            return Ok(false);
        };
        if let Some(proxy) = receiver.as_proxy() {
            // §6.2.12 — private-name writes land in the proxy's own
            // [[PrivateElements]]; the set trap never fires.
            if let ComputedPropertyKey::Symbol(sym) = &key
                && sym.is_private_name()
            {
                self.proxy_private_upsert(&proxy, *sym, value);
                stack[top_idx].advance_pc(self.current_byte_len)?;
                return Ok(true);
            }
            let key_arg = match &key {
                ComputedPropertyKey::String(key) => {
                    Value::string(JsString::from_str(key, &mut self.gc_heap)?)
                }
                ComputedPropertyKey::Symbol(sym) => Value::symbol(*sym),
            };
            let trap_args: SmallVec<[Value; 8]> = smallvec::smallvec![
                proxy.target(&self.gc_heap),
                key_arg,
                value,
                Value::proxy(proxy),
            ];
            stack[top_idx].advance_pc(self.current_byte_len)?;
            match self.invoke_proxy_trap(context, &proxy, "set", trap_args)? {
                Some(result) => {
                    // §10.5.9 step 13–14 invariants — a trap that reports
                    // success must not contradict a non-configurable,
                    // non-writable data property or a setter-less accessor
                    // on the target.
                    if result.to_boolean(&self.gc_heap) {
                        let target_value = proxy.target(&self.gc_heap);
                        let vm_key = match &key {
                            ComputedPropertyKey::String(k) => VmPropertyKey::OwnedString(k.clone()),
                            ComputedPropertyKey::Symbol(sym) => VmPropertyKey::Symbol(*sym),
                        };
                        let target_desc = self
                            .ordinary_get_own_property_descriptor_value_stack_rooted(
                                context,
                                stack,
                                target_value,
                                &vm_key,
                                0,
                            )?;
                        if let Some(desc) = target_desc.as_ref()
                            && !desc.configurable()
                        {
                            match &desc.kind {
                                object::DescriptorKind::Data { value: target_v }
                                    if !desc.writable()
                                        && !abstract_ops::same_value(
                                            target_v,
                                            &value,
                                            &self.gc_heap,
                                        ) =>
                                {
                                    return Err(self.err_type((
                                            "Proxy set trap reported success but target is non-configurable non-writable with a different value"
                                                .to_string()).into()));
                                }
                                object::DescriptorKind::Accessor { setter: None, .. } => {
                                    return Err(self.err_type((
                                            "Proxy set trap reported success but target is a non-configurable accessor without a setter"
                                                .to_string()).into()));
                                }
                                _ => {}
                            }
                        }
                    }
                }
                None => {
                    // §10.5.9 step 9 — a missing `set` trap forwards to
                    // `target.[[Set]](P, V, Receiver)` with the proxy as
                    // Receiver, so OrdinarySet's own-property steps fire
                    // the proxy's getOwnPropertyDescriptor / defineProperty
                    // traps (and an inherited setter sees `this === proxy`).
                    let target_value = proxy.target(&self.gc_heap);
                    let vm_key = match &key {
                        ComputedPropertyKey::String(key) => VmPropertyKey::OwnedString(key.clone()),
                        ComputedPropertyKey::Symbol(sym) => VmPropertyKey::Symbol(*sym),
                    };
                    if !self.ordinary_set_data_value(
                        context,
                        target_value,
                        &vm_key,
                        value,
                        Value::proxy(proxy),
                        0,
                    )? {
                        self.failed_set_result(strict, "Cannot assign to property")?;
                    }
                }
            }
            return Ok(true);
        }
        if let (Some(bound), ComputedPropertyKey::String(key)) =
            (receiver.as_bound_function(), &key)
        {
            let bound = &bound;
            match function_metadata::bound_own_property_descriptor(bound, &mut self.gc_heap, key)? {
                Some(object::PropertyDescriptor {
                    kind: object::DescriptorKind::Accessor { setter, .. },
                    ..
                }) => {
                    let setter = setter.ok_or(VmError::TypeMismatch)?;
                    if !abstract_ops::is_callable(&setter) {
                        return Err(VmError::TypeMismatch);
                    }
                    stack[top_idx].advance_pc(self.current_byte_len)?;
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
                        stack[top_idx].advance_pc(self.current_byte_len)?;
                        let mut args: SmallVec<[Value; 8]> = SmallVec::new();
                        args.push(value);
                        self.invoke(stack, context, &setter, receiver, args, scratch_reg)?;
                        return Ok(true);
                    }
                    if is_restricted_function_property(key) {
                        stack[top_idx].advance_pc(self.current_byte_len)?;
                        let callee = self.restricted_throw_type_error()?;
                        let mut args: SmallVec<[Value; 8]> = SmallVec::new();
                        args.push(value);
                        self.invoke(stack, context, &callee, receiver, args, scratch_reg)?;
                        return Ok(true);
                    }
                }
            }
        }
        if let ComputedPropertyKey::String(key) = &key
            && (receiver.is_function()
                || receiver.is_closure()
                || receiver.is_native_function()
                || receiver.is_class_constructor())
        {
            let own_present = if let Some(fid) = receiver.as_function().or_else(|| {
                receiver
                    .as_closure(&self.gc_heap)
                    .map(|c| c.cached_function_id)
            }) {
                let owner = receiver.as_closure(&self.gc_heap);
                self.callable_bag_read(owner, fid).is_some_and(|bag| {
                    !matches!(
                        object::lookup_own(bag, &self.gc_heap, key),
                        object::PropertyLookup::Absent
                    )
                })
            } else if let Some(c) = receiver.as_class_constructor() {
                !matches!(
                    object::lookup_own(c.statics(&self.gc_heap), &self.gc_heap, key),
                    object::PropertyLookup::Absent
                )
            } else if let Some(native) = receiver.as_native_function() {
                native
                    .own_property_descriptor(&mut self.gc_heap, key)?
                    .is_some()
            } else {
                false
            };
            if !own_present && is_restricted_function_property(key) {
                stack[top_idx].advance_pc(self.current_byte_len)?;
                let callee = self.restricted_throw_type_error()?;
                let mut args: SmallVec<[Value; 8]> = SmallVec::new();
                args.push(value);
                self.invoke(stack, context, &callee, receiver, args, scratch_reg)?;
                return Ok(true);
            }
        }
        if let (Some(native), ComputedPropertyKey::Symbol(sym)) =
            (receiver.as_native_function(), &key)
        {
            let obj = native.own_properties_object(&self.gc_heap);
            match object::resolve_symbol_set(obj, &self.gc_heap, *sym) {
                object::SetOutcome::AssignData => {
                    if !object::set_symbol(obj, &mut self.gc_heap, *sym, value) {
                        return self.finish_failed_set(
                            stack,
                            context,
                            "Cannot assign to symbol property",
                            self.current_byte_len,
                        );
                    }
                }
                object::SetOutcome::InvokeSetter { setter } => {
                    if !abstract_ops::is_callable(&setter) {
                        return self.finish_failed_set(
                            stack,
                            context,
                            "Cannot assign to accessor property without a setter",
                            self.current_byte_len,
                        );
                    }
                    stack[top_idx].advance_pc(self.current_byte_len)?;
                    let mut args: SmallVec<[Value; 8]> = SmallVec::new();
                    args.push(value);
                    self.invoke(stack, context, &setter, receiver, args, scratch_reg)?;
                    return Ok(true);
                }
                object::SetOutcome::Reject { .. } => {
                    return self.finish_failed_set(
                        stack,
                        context,
                        "Cannot assign to symbol property",
                        self.current_byte_len,
                    );
                }
                // The own-properties bag has no exotic prototype.
                object::SetOutcome::ExoticParent { .. } => {
                    let _ = object::set_symbol(obj, &mut self.gc_heap, *sym, value);
                }
            }
            stack[top_idx].advance_pc(self.current_byte_len)?;
            return Ok(true);
        }
        if receiver.is_boolean()
            || receiver.is_number()
            || receiver.is_string()
            || receiver.is_symbol()
            || receiver.is_big_int()
        {
            let key = match key {
                ComputedPropertyKey::String(key) => VmPropertyKey::OwnedString(key),
                ComputedPropertyKey::Symbol(sym) => VmPropertyKey::Symbol(sym),
            };
            return self.store_to_primitive_base(stack, context, receiver, key, value, scratch_reg);
        }
        if let Some(r) = receiver.as_regexp() {
            let r = &r;
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
                    if absent {
                        // §10.1.9.2 OrdinarySet — with no own shadow, the
                        // prototype chain decides: an inherited getter-only
                        // accessor (`global`, `source`, …) rejects the write
                        // instead of installing an own slot on the regexp.
                        let proto = self.get_prototype_for_op(&receiver)?;
                        if let Some(proto_obj) = proto.as_object() {
                            match object::resolve_set(proto_obj, &self.gc_heap, key) {
                                object::SetOutcome::InvokeSetter { setter } => {
                                    if !abstract_ops::is_callable(&setter) {
                                        return self.finish_failed_set(
                                            stack,
                                            context,
                                            "Cannot assign to accessor property without a setter",
                                            self.current_byte_len,
                                        );
                                    }
                                    stack[top_idx].advance_pc(self.current_byte_len)?;
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
                                    return Ok(true);
                                }
                                object::SetOutcome::Reject { .. } => {
                                    return self.finish_failed_set(
                                        stack,
                                        context,
                                        format!("Cannot assign to read-only property '{key}'"),
                                        self.current_byte_len,
                                    );
                                }
                                object::SetOutcome::AssignData => {}
                                // %RegExp.prototype% chains stay ordinary.
                                object::SetOutcome::ExoticParent { .. } => {}
                            }
                        }
                        if !r.is_extensible(&self.gc_heap) {
                            return self.finish_failed_set(
                                stack,
                                context,
                                format!("Cannot add property '{key}' to non-extensible RegExp"),
                                self.current_byte_len,
                            );
                        }
                    }
                    let bag = regexp_ensure_expando(self, r, &receiver)?;
                    if !self.ordinary_set_data_property(bag, key, value)? {
                        return self.finish_failed_set(
                            stack,
                            context,
                            format!("Cannot assign to property '{key}'"),
                            self.current_byte_len,
                        );
                    }
                }
                ComputedPropertyKey::Symbol(sym) => {
                    let absent = r.expando(&self.gc_heap).is_none_or(|bag| {
                        object::get_own_symbol_descriptor(bag, &self.gc_heap, *sym).is_none()
                    });
                    if absent && !r.is_extensible(&self.gc_heap) {
                        return self.finish_failed_set(
                            stack,
                            context,
                            "Cannot add symbol property to non-extensible RegExp",
                            self.current_byte_len,
                        );
                    }
                    let bag = regexp_ensure_expando(self, r, &receiver)?;
                    if !object::set_symbol(bag, &mut self.gc_heap, *sym, value) {
                        return self.finish_failed_set(
                            stack,
                            context,
                            "Cannot assign to symbol property",
                            self.current_byte_len,
                        );
                    }
                }
            }
            stack[top_idx].advance_pc(self.current_byte_len)?;
            return Ok(true);
        }
        let obj = if let Some(obj) = receiver.as_object() {
            obj
        } else if let Some(class) = receiver.as_class_constructor() {
            if let ComputedPropertyKey::String(name) = &key
                && self.class_store_hits_readonly_intrinsic(context, class, name)?
            {
                return self.finish_failed_set(
                    stack,
                    context,
                    format!("Cannot assign to read-only property '{name}' of class"),
                    self.current_byte_len,
                );
            }
            class.statics(&self.gc_heap)
        } else if let Some(fid) = receiver.as_function().or_else(|| {
            receiver
                .as_closure(&self.gc_heap)
                .map(|c| c.cached_function_id)
        }) {
            let owner = receiver.as_closure(&self.gc_heap);
            match &key {
                ComputedPropertyKey::String(key) => {
                    if function_metadata::ordinary_function_metadata_key(key).is_some()
                        && let Some(desc) = self.ordinary_function_own_property_descriptor(
                            Some(context),
                            owner,
                            fid,
                            key,
                        )?
                        && !desc.writable()
                    {
                        return self.finish_failed_set(
                            stack,
                            context,
                            format!("Cannot assign to read-only property '{key}' of function"),
                            self.current_byte_len,
                        );
                    }
                    let has_own = self
                        .ordinary_function_has_own_string_property_for_extensibility(
                            context, owner, fid, key,
                        )?;
                    if !has_own && !self.ordinary_function_is_extensible(fid) {
                        return self.finish_failed_set(
                            stack,
                            context,
                            format!("Cannot add property '{key}' to non-extensible function"),
                            self.current_byte_len,
                        );
                    }
                }
                ComputedPropertyKey::Symbol(sym) => {
                    if !self.ordinary_function_has_own_symbol_property_for_extensibility(
                        owner, fid, *sym,
                    ) && !self.ordinary_function_is_extensible(fid)
                    {
                        return self.finish_failed_set(
                            stack,
                            context,
                            "Cannot add symbol property to non-extensible function",
                            self.current_byte_len,
                        );
                    }
                }
            }
            self.function_user_bag_stack_rooted(stack, owner, fid, &[&receiver, &value])?
        } else {
            return Ok(false);
        };
        let outcome = match &key {
            ComputedPropertyKey::String(key) => crate::object::resolve_set(obj, &self.gc_heap, key),
            ComputedPropertyKey::Symbol(sym) => {
                crate::object::resolve_symbol_set(obj, &self.gc_heap, *sym)
            }
        };
        match outcome {
            // §10.1.9.2 step 2 — an exotic prototype (TypedArray,
            // Proxy value) owns [[Set]]: continue through the
            // value-level funnel with the ORIGINAL receiver.
            object::SetOutcome::ExoticParent { parent } => {
                let pkey = match &key {
                    ComputedPropertyKey::String(key) => VmPropertyKey::OwnedString(key.clone()),
                    ComputedPropertyKey::Symbol(sym) => VmPropertyKey::Symbol(*sym),
                };
                if !self.ordinary_set_data_value(context, parent, &pkey, value, receiver, 1)? {
                    return self.finish_failed_set(
                        stack,
                        context,
                        "Cannot assign to read-only property",
                        self.current_byte_len,
                    );
                }
                stack[top_idx].advance_pc(self.current_byte_len)?;
                Ok(true)
            }
            object::SetOutcome::AssignData => {
                let ok = match &key {
                    ComputedPropertyKey::String(key) => {
                        self.ordinary_set_data_property(obj, key, value)?
                    }
                    ComputedPropertyKey::Symbol(sym) => {
                        object::set_symbol(obj, &mut self.gc_heap, *sym, value)
                    }
                };
                if !ok {
                    return self.finish_failed_set(
                        stack,
                        context,
                        "Cannot assign to read-only property",
                        self.current_byte_len,
                    );
                }
                stack[top_idx].advance_pc(self.current_byte_len)?;
                Ok(true)
            }
            object::SetOutcome::InvokeSetter { setter } => {
                if !abstract_ops::is_callable(&setter) {
                    return self.finish_failed_set(
                        stack,
                        context,
                        "Cannot assign to accessor property without a setter",
                        self.current_byte_len,
                    );
                }
                stack[top_idx].advance_pc(self.current_byte_len)?;
                let mut args: SmallVec<[Value; 8]> = SmallVec::new();
                args.push(value);
                self.invoke(stack, context, &setter, receiver, args, scratch_reg)?;
                Ok(true)
            }
            object::SetOutcome::Reject { .. } => self.finish_failed_set(
                stack,
                context,
                "Cannot assign to property",
                self.current_byte_len,
            ),
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
        stack: &mut HoltStack,
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
        let receiver = *read_register(&stack[top_idx], obj_reg)?;
        let value = *read_register(&stack[top_idx], src_reg)?;
        let strict = Self::current_frame_is_strict(stack, context);
        if let Some(obj) = receiver.as_object()
            && object::supports_fast_property_ic(obj, &self.gc_heap)
        {
            let site = context
                .property_ic_site(stack[top_idx].function_id, stack[top_idx].pc)
                .ok_or(VmError::InvalidOperand)?;
            let entries_len = self.store_property_ics[site].entry_count();
            // The stub program is `&self`; only `gc_heap` is mutated by a store.
            // `store_property_ics` and `gc_heap` are disjoint fields, so the
            // entries slice and the `&mut gc_heap` a store writes through can be
            // held at once — no per-store clone of the whole stub is needed.
            let mut store_hit = false;
            for ic in self.store_property_ics[site].entries() {
                if ic
                    .run_store(obj, &mut self.gc_heap, atomized_key, &value)
                    .is_some()
                {
                    store_hit = true;
                    break;
                }
            }
            if store_hit {
                self.property_ic_stats.record_hit(PropertyIcKind::Store);
                Self::advance_property_fast_path(&mut stack[top_idx], self.current_byte_len)?;
                return Ok(true);
            }
            if entries_len > 0 {
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
        if let Some(proxy) = receiver.as_proxy() {
            if proxy.is_revoked(&self.gc_heap) {
                return Err(self.err_type(
                    ("Cannot perform 'set' on a proxy that has been revoked".to_string()).into(),
                ));
            }
            let key_str = JsString::from_str(name, self.gc_heap_mut())?;
            let key_vm = VmPropertyKey::atom(atomized_key);
            let trap_args: SmallVec<[Value; 8]> = smallvec::smallvec![
                proxy.target(&self.gc_heap),
                Value::string(key_str),
                value,
                Value::proxy(proxy),
            ];
            stack[top_idx].advance_pc(self.current_byte_len)?;
            match self.invoke_proxy_trap(context, &proxy, "set", trap_args)? {
                Some(result) => {
                    let ok = result.to_boolean(&self.gc_heap);
                    if !ok {
                        self.failed_set_result(
                            strict,
                            format!("Cannot assign to property '{name}'"),
                        )?;
                        return Ok(true);
                    }
                    // §10.5.9 step 13–14 invariants — when trap reports
                    // success, ensure target descriptor admits the
                    // value.
                    let target_value = proxy.target(&self.gc_heap);
                    let target_desc = self
                        .ordinary_get_own_property_descriptor_value_stack_rooted(
                            context,
                            stack,
                            target_value,
                            &key_vm,
                            0,
                        )?;
                    if let Some(desc) = target_desc.as_ref()
                        && !desc.configurable()
                    {
                        match &desc.kind {
                            object::DescriptorKind::Data { value: target_v }
                                if !desc.writable()
                                    && !abstract_ops::same_value(
                                        target_v,
                                        &value,
                                        &self.gc_heap,
                                    ) =>
                            {
                                return Err(self.err_type((
                                            "Proxy set trap reported success but target is non-configurable non-writable with a different value"
                                                .to_string()).into()));
                            }
                            object::DescriptorKind::Accessor { setter: None, .. } => {
                                return Err(self.err_type((
                                        "Proxy set trap reported success but target is a non-configurable accessor without a setter"
                                            .to_string()).into()));
                            }
                            _ => {}
                        }
                    }
                }
                None => {
                    // §10.5.9 step 9 — a missing `set` trap forwards to
                    // `target.[[Set]](P, V, Receiver)` with the PROXY as
                    // Receiver. OrdinarySet's own-property steps then run
                    // against the receiver, so the proxy's
                    // getOwnPropertyDescriptor / defineProperty traps fire
                    // and an inherited setter sees `this === proxy`. Route
                    // every target (ordinary, exotic, or nested proxy)
                    // through the value-level funnel rather than a
                    // receiver-blind fast path.
                    let target_value = proxy.target(&self.gc_heap);
                    if !self.ordinary_set_data_value(
                        context,
                        target_value,
                        &key_vm,
                        value,
                        Value::proxy(proxy),
                        0,
                    )? {
                        self.failed_set_result(
                            strict,
                            format!("Cannot assign to property '{name}'"),
                        )?;
                    }
                }
            }
            return Ok(true);
        }
        if let Some(bound) = receiver.as_bound_function() {
            let bound = &bound;
            match function_metadata::bound_own_property_descriptor(bound, &mut self.gc_heap, name)?
            {
                Some(object::PropertyDescriptor {
                    kind: object::DescriptorKind::Accessor { setter, .. },
                    ..
                }) => {
                    let setter = setter.ok_or(VmError::TypeMismatch)?;
                    if !abstract_ops::is_callable(&setter) {
                        return Err(VmError::TypeMismatch);
                    }
                    stack[top_idx].advance_pc(self.current_byte_len)?;
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
                        name,
                    ) {
                        let setter = setter.ok_or(VmError::TypeMismatch)?;
                        if !abstract_ops::is_callable(&setter) {
                            return Err(VmError::TypeMismatch);
                        }
                        stack[top_idx].advance_pc(self.current_byte_len)?;
                        let mut args: SmallVec<[Value; 8]> = SmallVec::new();
                        args.push(value);
                        self.invoke(stack, context, &setter, receiver, args, scratch_reg)?;
                        return Ok(true);
                    }
                    if is_restricted_function_property(name) {
                        stack[top_idx].advance_pc(self.current_byte_len)?;
                        let callee = self.restricted_throw_type_error()?;
                        let mut args: SmallVec<[Value; 8]> = SmallVec::new();
                        args.push(value);
                        self.invoke(stack, context, &callee, receiver, args, scratch_reg)?;
                        return Ok(true);
                    }
                }
            }
        }
        if receiver.is_function()
            || receiver.is_closure()
            || receiver.is_native_function()
            || receiver.is_class_constructor()
        {
            let own_present = if let Some(fid) = receiver.as_function().or_else(|| {
                receiver
                    .as_closure(&self.gc_heap)
                    .map(|c| c.cached_function_id)
            }) {
                let owner = receiver.as_closure(&self.gc_heap);
                let bag_has = self.callable_bag_read(owner, fid).is_some_and(|bag| {
                    !matches!(
                        object::lookup_own(bag, &self.gc_heap, name),
                        object::PropertyLookup::Absent
                    )
                });
                // Virtual own properties (metadata-backed `name` /
                // `length`, lazily-materialized `prototype`) shadow
                // %Function.prototype% — §13.2 step 18: an accessor
                // named `prototype` installed there must never fire
                // for an ordinary function.
                let metadata_has = self
                    .ordinary_function_own_property_descriptor(None, owner, fid, name)
                    .ok()
                    .flatten()
                    .is_some();
                let prototype_implicit = name == "prototype"
                    && context.function_has_prototype_property(fid)
                    && !self.function_deleted_metadata.contains(&(fid, "prototype"));
                bag_has || metadata_has || prototype_implicit
            } else if let Some(c) = receiver.as_class_constructor() {
                // Class constructors expose `prototype` / `name` /
                // `length` virtually when no static shadows them.
                matches!(name, "prototype" | "name" | "length")
                    || !matches!(
                        object::lookup_own(c.statics(&self.gc_heap), &self.gc_heap, name),
                        object::PropertyLookup::Absent
                    )
            } else if let Some(native) = receiver.as_native_function() {
                native
                    .own_property_descriptor(&mut self.gc_heap, name)?
                    .is_some()
            } else {
                false
            };
            if !own_present && is_restricted_function_property(name) {
                stack[top_idx].advance_pc(self.current_byte_len)?;
                let callee = self.restricted_throw_type_error()?;
                let mut args: SmallVec<[Value; 8]> = SmallVec::new();
                args.push(value);
                self.invoke(stack, context, &callee, receiver, args, scratch_reg)?;
                return Ok(true);
            }
        }
        if receiver.is_boolean()
            || receiver.is_number()
            || receiver.is_string()
            || receiver.is_symbol()
            || receiver.is_big_int()
        {
            return self.store_to_primitive_base(
                stack,
                context,
                receiver,
                VmPropertyKey::atom(atomized_key),
                value,
                scratch_reg,
            );
        }
        let obj = if let Some(o) = receiver.as_object() {
            o
        } else if let Some(c) = receiver.as_class_constructor() {
            if self.class_store_hits_readonly_intrinsic(context, c, name)? {
                return self.finish_failed_set(
                    stack,
                    context,
                    format!("Cannot assign to read-only property '{name}' of class"),
                    self.current_byte_len,
                );
            }
            c.statics(&self.gc_heap)
        } else if let Some(fid) = receiver.as_function().or_else(|| {
            receiver
                .as_closure(&self.gc_heap)
                .map(|c| c.cached_function_id)
        }) {
            let owner = receiver.as_closure(&self.gc_heap);
            if function_metadata::ordinary_function_metadata_key(name).is_some() {
                match self.ordinary_function_own_property_descriptor(
                    Some(context),
                    owner,
                    fid,
                    name,
                )? {
                    Some(desc) if !desc.writable() => {
                        return self.finish_failed_set(
                            stack,
                            context,
                            format!("Cannot assign to read-only property '{name}' of function"),
                            self.current_byte_len,
                        );
                    }
                    // The virtual `name`/`length` was deleted: defer to the
                    // slow store funnel, which resolves the write along the
                    // [[Prototype]] (the inherited %Function.prototype% slot
                    // is non-writable) instead of re-creating an own bag
                    // entry that would mask it.
                    None => return Ok(false),
                    Some(_) => {}
                }
            }
            match self.callable_bag_read(owner, fid) {
                Some(bag) => bag,
                None => self.function_user_bag_with_stack_roots(
                    stack,
                    owner,
                    fid,
                    &[&receiver, &value],
                )?,
            }
        } else {
            return Ok(false);
        };
        let outcome = crate::object::resolve_set(obj, &self.gc_heap, name);
        match outcome {
            // §10.1.9.2 step 2 — an exotic prototype owns [[Set]]:
            // continue through the value-level funnel (TypedArray
            // §10.4.5.5 etc. are observable from plain receivers).
            object::SetOutcome::ExoticParent { parent } => {
                if !self.ordinary_set_data_value(
                    context,
                    parent,
                    &VmPropertyKey::String(name),
                    value,
                    receiver,
                    1,
                )? {
                    return self.finish_failed_set(
                        stack,
                        context,
                        format!("Cannot assign to property '{name}'"),
                        self.current_byte_len,
                    );
                }
                stack[top_idx].advance_pc(self.current_byte_len)?;
                Ok(true)
            }
            object::SetOutcome::AssignData => {
                let transition = if receiver.is_object()
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
                if transition.is_none() && !self.ordinary_set_data_property(obj, name, value)? {
                    return self.finish_failed_set(
                        stack,
                        context,
                        format!("Cannot assign to property '{name}'"),
                        self.current_byte_len,
                    );
                }
                if receiver.is_object() {
                    let site = context
                        .property_ic_site(stack[top_idx].function_id, stack[top_idx].pc)
                        .ok_or(VmError::InvalidOperand)?;
                    if !self.store_property_ics[site].is_megamorphic()
                        && object::supports_fast_property_ic(obj, &self.gc_heap)
                    {
                        if let Some(transition) = transition {
                            self.store_property_ics[site].install_with_stats(
                                &mut self.property_ic_stats,
                                PropertyIcKind::Store,
                                cache_ir::CacheStub::store_transition(transition),
                            );
                        } else if let Some(ic) = cache_ir::CacheStub::install_store_existing(
                            obj,
                            &self.gc_heap,
                            atomized_key,
                        ) {
                            self.store_property_ics[site].install_with_stats(
                                &mut self.property_ic_stats,
                                PropertyIcKind::Store,
                                ic,
                            );
                        }
                    }
                }
                stack[top_idx].advance_pc(self.current_byte_len)?;
                Ok(true)
            }
            object::SetOutcome::InvokeSetter { setter } => {
                if !abstract_ops::is_callable(&setter) {
                    // Spec §10.1.9 step 5.b — accessor with non-
                    // callable setter rejects.
                    return self.finish_failed_set(
                        stack,
                        context,
                        format!("Cannot assign to accessor property '{name}' without a setter"),
                        self.current_byte_len,
                    );
                }
                stack[top_idx].advance_pc(self.current_byte_len)?;
                let mut args: SmallVec<[Value; 8]> = SmallVec::new();
                args.push(value);
                self.invoke(stack, context, &setter, receiver, args, scratch_reg)?;
                Ok(true)
            }
            object::SetOutcome::Reject { .. } => self.finish_failed_set(
                stack,
                context,
                format!("Cannot assign to property '{name}'"),
                self.current_byte_len,
            ),
        }
    }

    /// §7.3.10 HasProperty — ordinary objects may have Proxy
    /// objects in their prototype chain, so the interpreter owns
    /// the trap-aware walk instead of delegating to `object::lookup`.
    pub(crate) fn drive_has_property_proxy(
        &mut self,
        stack: &mut HoltStack,
        context: &ExecutionContext,
        operands: &[Operand],
    ) -> Result<bool, VmError> {
        let dst = register_operand(operands.first())?;
        let lhs_reg = register_operand(operands.get(1))?;
        let rhs_reg = register_operand(operands.get(2))?;
        let top_idx = stack.len() - 1;
        let lhs = *read_register(&stack[top_idx], lhs_reg)?;
        let rhs = *read_register(&stack[top_idx], rhs_reg)?;
        if !(rhs.is_object() || rhs.is_proxy()) {
            return Ok(false);
        };
        // §6.2.12 — private-name presence checks on a Proxy answer
        // from its own [[PrivateElements]]; the has trap never fires.
        if let (Some(p), Some(sym)) = (rhs.as_proxy(), lhs.as_symbol(&self.gc_heap))
            && sym.is_private_name()
        {
            let present = self.proxy_private_find(&p, sym).is_some();
            Self::finish_property_fast_path_value(
                &mut stack[top_idx],
                dst,
                Value::boolean(present),
                self.current_byte_len,
            )?;
            return Ok(true);
        }
        if let (Some(obj), Some(key_string)) = (rhs.as_object(), lhs.as_string(&self.gc_heap)) {
            let site = context
                .property_ic_site(stack[top_idx].function_id, stack[top_idx].pc)
                .ok_or(VmError::InvalidOperand)?;
            let mut site_disabled = self.has_property_ics[site].is_megamorphic();
            let entries_len = self.has_property_ics[site].entry_count();
            let mut probe_hit = false;
            for idx in 0..entries_len {
                let ic = self.has_property_ics[site].entries()[idx].clone();
                if ic.run_has(obj, &self.gc_heap, key_string).is_some() {
                    probe_hit = true;
                    break;
                }
            }
            if probe_hit {
                self.property_ic_stats.record_hit(PropertyIcKind::Has);
                Self::finish_property_fast_path_value(
                    &mut stack[top_idx],
                    dst,
                    Value::boolean(true),
                    self.current_byte_len,
                )?;
                return Ok(true);
            }
            if entries_len > 0 {
                self.has_property_ics[site]
                    .record_guard_miss_with_stats(&mut self.property_ic_stats, PropertyIcKind::Has);
                site_disabled = self.has_property_ics[site].is_megamorphic();
            } else {
                self.has_property_ics[site].record_uncached_miss_with_stats(
                    &mut self.property_ic_stats,
                    PropertyIcKind::Has,
                );
            }
            if !site_disabled
                && let Some(ic) = cache_ir::CacheStub::install_has(obj, &self.gc_heap, key_string)
            {
                self.has_property_ics[site].install_with_stats(
                    &mut self.property_ic_stats,
                    PropertyIcKind::Has,
                    ic,
                );
                Self::finish_property_fast_path_value(
                    &mut stack[top_idx],
                    dst,
                    Value::boolean(true),
                    self.current_byte_len,
                )?;
                return Ok(true);
            }
            self.has_property_ics[site]
                .disable_with_stats(&mut self.property_ic_stats, PropertyIcKind::Has);
        }
        let key = if let Some(sym) = lhs.as_symbol(&self.gc_heap) {
            VmPropertyKey::Symbol(sym)
        } else if let Some(s) = lhs.as_string(&self.gc_heap) {
            VmPropertyKey::OwnedString(s.to_lossy_string(&self.gc_heap))
        } else {
            VmPropertyKey::OwnedString(lhs.display_string(&self.gc_heap))
        };
        stack[top_idx].advance_pc(self.current_byte_len)?;
        let present = self.ordinary_has_property_value(context, rhs, &key, 0)?;
        write_register(&mut stack[top_idx], dst, Value::boolean(present))?;
        Ok(true)
    }

    /// §28.2.4.10 Proxy.[[Delete]] — invoke the `deleteProperty`
    /// trap when the receiver of `delete obj.x` is a Proxy.
    pub(crate) fn drive_delete_property_proxy(
        &mut self,
        stack: &mut HoltStack,
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
        let receiver = *read_register(&stack[top_idx], obj_reg)?;
        let Some(proxy) = receiver.as_proxy() else {
            return Ok(false);
        };
        stack[top_idx].advance_pc(self.current_byte_len)?;
        let removed = self.ordinary_delete_value(
            context,
            Value::proxy(proxy),
            &VmPropertyKey::atom(atomized_key),
            0,
        )?;
        // §13.5.1.2 — strict-mode `delete` whose [[Delete]] returns false
        // throws a TypeError (the computed-key path already does this).
        let strict = context.function_is_strict(stack[top_idx].function_id);
        if !removed && strict {
            return Err(self.err_type(("Cannot delete property".to_string()).into()));
        }
        write_register(&mut stack[top_idx], dst, Value::boolean(removed))?;
        Ok(true)
    }

    /// §28.2.4.10 Proxy.[[Delete]] — computed delete uses the
    /// same trap-aware path as `delete obj.x`.
    pub(crate) fn drive_delete_element_proxy(
        &mut self,
        stack: &mut HoltStack,
        context: &ExecutionContext,
        operands: &[Operand],
    ) -> Result<bool, VmError> {
        let dst = register_operand(operands.first())?;
        let obj_reg = register_operand(operands.get(1))?;
        let idx_reg = register_operand(operands.get(2))?;
        let top_idx = stack.len() - 1;
        let receiver = *read_register(&stack[top_idx], obj_reg)?;
        if !receiver.is_proxy() {
            return Ok(false);
        }
        let idx = *read_register(&stack[top_idx], idx_reg)?;
        let key = Self::coerce_vm_property_key(Some(&idx), &self.gc_heap)?;
        stack[top_idx].advance_pc(self.current_byte_len)?;
        let removed = self.ordinary_delete_value(context, receiver, &key, 0)?;
        let strict = context.function_is_strict(stack[top_idx].function_id);
        if !removed && strict {
            return Err(self.err_type(("Cannot delete property".to_string()).into()));
        }
        write_register(&mut stack[top_idx], dst, Value::boolean(removed))?;
        Ok(true)
    }

    /// §28.2.4.1 Proxy.[[GetPrototypeOf]] — invoke the
    /// `getPrototypeOf` trap when the source is a Proxy.
    pub(crate) fn drive_get_prototype_proxy(
        &mut self,
        stack: &mut HoltStack,
        context: &ExecutionContext,
        operands: &[Operand],
    ) -> Result<bool, VmError> {
        let dst = register_operand(operands.first())?;
        let src = register_operand(operands.get(1))?;
        let top_idx = stack.len() - 1;
        let value = *read_register(&stack[top_idx], src)?;
        if !value.is_proxy() {
            return Ok(false);
        };
        stack[top_idx].advance_pc(self.current_byte_len)?;
        let result = self.ordinary_get_prototype_value(context, value, 0)?;
        write_register(&mut stack[top_idx], dst, result)?;
        Ok(true)
    }

    /// §28.2.4.2 Proxy.[[SetPrototypeOf]] — invoke the
    /// `setPrototypeOf` trap when the receiver is a Proxy.
    pub(crate) fn drive_set_prototype_proxy(
        &mut self,
        stack: &mut HoltStack,
        context: &ExecutionContext,
        operands: &[Operand],
    ) -> Result<bool, VmError> {
        let obj_reg = register_operand(operands.first())?;
        let proto_reg = register_operand(operands.get(1))?;
        let top_idx = stack.len() - 1;
        let recv = *read_register(&stack[top_idx], obj_reg)?;
        if !recv.is_proxy() {
            return Ok(false);
        };
        let proto_val = *read_register(&stack[top_idx], proto_reg)?;
        let proto_obj = if proto_val.is_object() || proto_val.is_proxy() || proto_val.is_null() {
            proto_val
        } else if let Some(c) = proto_val.as_class_constructor() {
            Value::object(c.statics(&self.gc_heap))
        } else {
            return Err(VmError::TypeMismatch);
        };
        stack[top_idx].advance_pc(self.current_byte_len)?;
        // §10.5.7 — dispatch through the value-level helper so
        // nested proxies fall through correctly and §10.5.7 invariants
        // apply on the trap result.
        let ok = self.set_prototype_value_proxy_aware(context, &recv, &proto_obj)?;
        if !ok {
            // Object.setPrototypeOf throws when [[SetPrototypeOf]]
            // returns false (§20.1.2.21 step 4 DefinePropertyOrThrow).
            return Err(self.err_type(("Object.setPrototypeOf failed".to_string()).into()));
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

fn has_array_property(interpreter: &Interpreter, arr: JsArray, key: &Value) -> bool {
    if let Some(n) = key.as_number() {
        match n.as_smi() {
            Some(i) if i >= 0 => {
                crate::array::has_own_element(arr, &interpreter.gc_heap, i as usize)
            }
            _ => {
                crate::array::get_named_property(arr, &interpreter.gc_heap, &n.to_display_string())
                    .is_some()
            }
        }
    } else if let Some(s) = key.as_string(&interpreter.gc_heap) {
        let k = s.to_lossy_string(&interpreter.gc_heap);
        if k == "length" {
            return true;
        }
        if let Some(i) = crate::object::array_index_property_name(&k)
            && crate::array::has_own_element(arr, &interpreter.gc_heap, i as usize)
        {
            return true;
        }
        // §22.1.4 — surface named-property side table for `in`.
        crate::array::get_named_property(arr, &interpreter.gc_heap, &k).is_some()
    } else if let Some(sym) = key.as_symbol(&interpreter.gc_heap) {
        // §22.1 Array exotic — symbol-keyed own table.
        crate::array::get_symbol_property(arr, &interpreter.gc_heap, sym).is_some()
    } else {
        false
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

/// §10.4.5.14 IsValidIntegerIndex — `Some(index)` when `n` addresses
/// a live element of `t`: the buffer is attached, `n` is an integer,
/// not `-0`, non-negative, and below the view length. Every
/// TypedArray exotic internal method funnels its canonical-numeric
/// validity check through here so `-0` / fractional / out-of-bounds
/// keys behave identically across [[Get]] / [[Set]] /
/// [[GetOwnProperty]] / [[DefineOwnProperty]] / [[HasProperty]] /
/// [[Delete]].
pub(crate) fn typed_array_valid_index(
    t: &crate::binary::typed_array::JsTypedArray,
    heap: &otter_gc::GcHeap,
    n: f64,
) -> Option<usize> {
    if t.buffer(heap).is_detached(heap) {
        return None;
    }
    if !n.is_finite() || n.fract() != 0.0 || n.is_sign_negative() {
        return None;
    }
    let idx = n as usize;
    if idx >= t.length(heap) {
        return None;
    }
    Some(idx)
}

impl Interpreter {
    /// Own lookup on a TypedArray expando bag preserving accessor
    /// identity (§10.4.5.4 step 2 — non-canonical keys take
    /// OrdinaryGet, which must invoke own getters with the typed
    /// array as receiver).
    fn expando_own_get_outcome(
        bag: JsObject,
        heap: &otter_gc::GcHeap,
        name: &str,
    ) -> Option<crate::VmGetOutcome> {
        match crate::object::lookup_own(bag, heap, name) {
            crate::object::PropertyLookup::Data { value, .. } => {
                Some(crate::VmGetOutcome::Value(value))
            }
            crate::object::PropertyLookup::Accessor { getter, .. } => Some(match getter {
                Some(g) => crate::VmGetOutcome::InvokeGetter { getter: g },
                None => crate::VmGetOutcome::Value(Value::undefined()),
            }),
            crate::object::PropertyLookup::Absent => None,
        }
    }
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
    if let Some(existing) = t.expando(heap) {
        return Ok(existing);
    }
    let ta_root = Value::typed_array(*t);
    let mut external_visit = |visitor: &mut dyn FnMut(*mut RawGc)| {
        ta_root.trace_value_slots(visitor);
    };
    let bag = crate::object::alloc_object_with_roots(heap, &mut external_visit)?;
    t.set_expando(heap, bag);
    Ok(bag)
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
    let recv = Value::regexp(*r);
    let mut external_visit = |visitor: &mut dyn FnMut(*mut RawGc)| {
        recv.trace_value_slots(visitor);
    };
    let bag = crate::object::alloc_object_with_roots(heap, &mut external_visit)?;
    r.set_expando(heap, bag);
    Ok(bag)
}

/// Lazy-allocate (and cache) the ordinary own-property bag for a Map. Maps are
/// ordinary extensible objects whose `[[MapData]]` entries are not own
/// properties; `m.x = 1` / `Object.defineProperty(m, …)` install onto this bag.
pub(crate) fn map_ensure_expando_pub(
    heap: &mut otter_gc::GcHeap,
    m: crate::collections::JsMap,
) -> Result<JsObject, VmError> {
    if let Some(existing) = crate::collections::map_expando(m, heap) {
        return Ok(existing);
    }
    let recv = Value::map(m);
    let mut external_visit = |visitor: &mut dyn FnMut(*mut RawGc)| {
        recv.trace_value_slots(visitor);
    };
    let bag = crate::object::alloc_object_with_roots(heap, &mut external_visit)?;
    crate::collections::map_set_expando(m, heap, bag);
    Ok(bag)
}

/// As [`map_ensure_expando_pub`] for a Set.
pub(crate) fn set_ensure_expando_pub(
    heap: &mut otter_gc::GcHeap,
    s: crate::collections::JsSet,
) -> Result<JsObject, VmError> {
    if let Some(existing) = crate::collections::set_expando(s, heap) {
        return Ok(existing);
    }
    let recv = Value::set(s);
    let mut external_visit = |visitor: &mut dyn FnMut(*mut RawGc)| {
        recv.trace_value_slots(visitor);
    };
    let bag = crate::object::alloc_object_with_roots(heap, &mut external_visit)?;
    crate::collections::set_set_expando(s, heap, bag);
    Ok(bag)
}

/// Lazy-allocate (and cache) the Temporal expando `JsObject` backing
/// ordinary own properties. Temporal instances are ordinary extensible
/// objects, so `Object.defineProperty(dt, …)` / `dt.x = 1` install onto
/// this bag, shadowing the prototype accessors.
pub(crate) fn temporal_ensure_expando_pub(
    heap: &mut otter_gc::GcHeap,
    t: &crate::temporal::JsTemporal,
) -> Result<JsObject, VmError> {
    if let Some(existing) = t.expando(heap) {
        return Ok(existing);
    }
    let recv = Value::temporal(*t);
    let mut external_visit = |visitor: &mut dyn FnMut(*mut RawGc)| {
        recv.trace_value_slots(visitor);
    };
    let bag = crate::object::alloc_object_with_roots(heap, &mut external_visit)?;
    t.set_expando(heap, bag);
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
    let recv = Value::promise(*p);
    let mut external_visit = |visitor: &mut dyn FnMut(*mut RawGc)| {
        recv.trace_value_slots(visitor);
    };
    let bag = crate::object::alloc_object_with_roots(heap, &mut external_visit)?;
    p.set_expando(heap, bag);
    Ok(bag)
}

/// Lazy-allocate (and cache) the `DataView` expando `JsObject` backing
/// ordinary own properties (`dv.x = 1`). A `DataView` is an ordinary
/// extensible object per §25.3, so it must hold arbitrary own props.
/// Lazy-allocate (and cache) the ArrayBuffer expando bag backing
/// ordinary own properties (`ab.constructor = C` for the species
/// protocol). Local buffers only.
pub(crate) fn array_buffer_ensure_expando_pub(
    heap: &mut otter_gc::GcHeap,
    b: &crate::binary::array_buffer::JsArrayBuffer,
) -> Result<JsObject, VmError> {
    if let Some(existing) = b.expando(heap) {
        return Ok(existing);
    }
    let recv = Value::array_buffer(*b);
    let mut external_visit = |visitor: &mut dyn FnMut(*mut RawGc)| {
        recv.trace_value_slots(visitor);
    };
    let bag = crate::object::alloc_object_with_roots(heap, &mut external_visit)?;
    b.set_expando(heap, bag);
    Ok(bag)
}

pub(crate) fn data_view_ensure_expando_pub(
    heap: &mut otter_gc::GcHeap,
    dv: &crate::binary::JsDataView,
) -> Result<JsObject, VmError> {
    if let Some(existing) = dv.expando(heap) {
        return Ok(existing);
    }
    let recv = Value::data_view(*dv);
    let mut external_visit = |visitor: &mut dyn FnMut(*mut RawGc)| {
        recv.trace_value_slots(visitor);
    };
    let bag = crate::object::alloc_object_with_roots(heap, &mut external_visit)?;
    dv.set_expando(heap, bag);
    Ok(bag)
}
