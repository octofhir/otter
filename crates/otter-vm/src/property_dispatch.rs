//! Property-related opcode helpers.
//!
//! The VM dispatch loop handles proxy or call-frame cases before entering the
//! dense register path. This module owns the remaining synchronous property
//! operations that can run directly against a frame.
//!
//! # Contents
//! - Legacy `instanceof` prototype-chain fallback.
//! - Synchronous `in` / `HasProperty` checks through the shared object resolver.
//! - Synchronous property and element load/store tails, including the shared
//!   named-value `[[Set]]` entry used by JIT miss transitions.
//! - Allocation-free completion for plain dense-array slots after a generated
//!   element guard rejects a hole or pointer-valued store.
//!
//! # Invariants
//! - Stack-modifying proxy and `@@hasInstance` cases are handled before these
//!   helpers are called.
//! - Inputs are already decoded from the executable instruction format.
//! - A dense hole bypasses prototype lookup only while the one-shot indexed
//!   accessor protector remains intact; arrays with sidecars stay generic.
//!
//! # See also
//! - [`crate::executable`]
//! - [`crate::object`]

use crate::activation_stack::ActivationStack;
use smallvec::SmallVec;

use otter_gc::raw::RawGc;

use crate::{
    ExecutionContext, Frame, Interpreter, JsObject, JsString, SuperReadKey, Value, VmError,
    VmGetOutcome, VmPropertyKey, abstract_ops, array::JsArray, function_metadata, object,
    property_atom::AtomizedPropertyKey, read_register, value_kind_name, write_register,
};

mod drivers;
mod elements;
mod jit_runtime;
mod properties;

/// Resolution of an OrdinarySet for a callable's deleted `name` /
/// `length` along its `[[Prototype]]` (see
/// [`Interpreter::callable_metadata_proto_set`]).
pub(crate) enum MetadataProtoSet {
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
        stack: &mut ActivationStack,
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
                self.run_callable_sync_rooted(stack, context, &setter, Value::array(arr), args)?;
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

    pub(crate) fn capture_store_property_transition_with_stack_roots(
        &mut self,
        stack: &ActivationStack,
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
        stack: &mut ActivationStack,
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
        let key = self.to_property_key_sync(stack, context, value)?;
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
        stack: &mut ActivationStack,
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
            None if name == "length" => Ok(Value::number_u32(string.len())),
            None => self.load_from_constructor_prototype(stack, context, "String", receiver, name),
        }
    }

    fn function_user_bag_with_stack_roots(
        &mut self,
        stack: &ActivationStack,
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

    pub(crate) fn run_delete_property_reg(
        &mut self,
        context: &crate::ExecutionContext,
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
                self.ordinary_function_delete_own_property(owner, function_id, name, false)
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
            let has_prototype = context.function_has_prototype_property(function_id);
            self.ordinary_function_delete_own_property(owner, function_id, name, has_prototype)
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
        } else if receiver.is_map() || receiver.is_set() || receiver.is_generator() {
            // Ordinary own properties on a Map/Set/Generator live in the
            // lazy expando; a missing name deletes vacuously.
            if let Some(bag) = self.collection_expando(&receiver) {
                crate::object::delete(bag, &mut self.gc_heap, name)
            } else {
                true
            }
        } else if let Some(s) = receiver.as_string(&self.gc_heap) {
            // §13.5.1.2 — ToObject boxes the primitive; the fresh
            // wrapper's own index slots and `length` are
            // non-configurable (`false`, TypeError in strict mode),
            // everything else deletes vacuously off the discarded
            // wrapper.
            let own = name == "length"
                || name
                    .parse::<u32>()
                    .is_ok_and(|index| index < s.len() && index.to_string() == name);
            !own
        } else if receiver.is_number()
            || receiver.is_boolean()
            || receiver.is_big_int()
            || receiver.is_symbol()
        {
            // §13.5.1.2 — a fresh primitive wrapper has no own
            // configurable-relevant slots; the delete is vacuous.
            true
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
        // `false` in strict mode, throw a TypeError. The receiver renders in
        // V8's `#<Tag>` shape (`@@toStringTag` first, constructor name after),
        // which the Node harness matches against.
        if !removed && strict {
            let receiver_tag = receiver
                .as_object()
                .and_then(|object| {
                    let tag = self
                        .well_known_symbols()
                        .get(crate::symbol::WellKnown::ToStringTag);
                    crate::object::get_symbol(object, &self.gc_heap, tag)
                        .and_then(|value| value.as_string(&self.gc_heap))
                        .map(|value| value.to_lossy_string(&self.gc_heap))
                })
                .unwrap_or_else(|| "Object".to_string());
            return Err(self.err_type(
                (format!("Cannot delete property '{name}' of #<{receiver_tag}>")).into(),
            ));
        }
        write_register(frame, dst, Value::boolean(removed))?;
        frame.advance_pc()?;
        Ok(())
    }

    pub(crate) fn run_delete_element_regs(
        &mut self,
        context: &crate::ExecutionContext,
        stack: &mut crate::activation_stack::ActivationStack,
        top_idx: usize,
        dst: u16,
        obj_reg: u16,
        idx_reg: u16,
        strict: bool,
    ) -> Result<(), VmError> {
        let mut idx = *read_register(&stack[top_idx], idx_reg)?;
        if !crate::abstract_ops::is_primitive(&idx) {
            // §13.5.1.2 — ToPropertyKey runs the key's coercion (a
            // user `toString`) before the [[Delete]]; the receiver is
            // re-read from its traced register afterwards.
            let key = self.to_property_key_sync(stack, context, idx)?;
            idx = match key {
                crate::VmPropertyKey::Symbol(sym) => Value::symbol(sym),
                other => {
                    let name = other.string_name().map(str::to_string).unwrap_or_default();
                    Value::string(crate::JsString::from_str(&name, &mut self.gc_heap)?)
                }
            };
        }
        let receiver = *read_register(&stack[top_idx], obj_reg)?;
        let frame = &mut stack[top_idx];
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
                    self.ordinary_function_delete_own_property(owner, function_id, &name, false)
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
                let has_prototype = context.function_has_prototype_property(function_id);
                self.ordinary_function_delete_own_property(owner, function_id, &name, has_prototype)
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
        frame.advance_pc()?;
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
        stack: &mut ActivationStack,
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
                let coerced = self.coerce_property_key_value(stack, context, raw)?;
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
        let value = match self.ordinary_get_value(stack, context, base, actual_this, &key, 0)? {
            VmGetOutcome::Value(v) => v,
            VmGetOutcome::InvokeGetter { getter } => self.run_callable_sync_rooted(
                stack,
                context,
                &getter,
                actual_this,
                SmallVec::new(),
            )?,
        };
        write_register(&mut stack[top_idx], dst, value)?;
        stack[top_idx].advance_pc()?;
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
        stack: &mut ActivationStack,
        top_idx: usize,
        base: Value,
        key: SuperReadKey<'_>,
        value: Value,
        strict: bool,
    ) -> Result<(), VmError> {
        let actual_this = stack[top_idx].this_value;
        if actual_this.is_hole() {
            return Err(self.err_this_uninit(( "must call super constructor in derived class before accessing 'this' or returning from derived constructor".to_string()).into()));
        }
        // §13.3.5.3 MakeSuperPropertyReference — `base` was resolved by
        // the lowering (home's [[GetPrototypeOf]]) BEFORE the RHS
        // evaluated, so a setPrototypeOf side effect inside the RHS is
        // not observable here.
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
                let coerced = self.coerce_property_key_value(stack, context, raw)?;
                if let Some(sym) = coerced.as_symbol(&self.gc_heap) {
                    // §6.2.5.5 step 6.b with a symbol key: a setter on
                    // the super base runs with the frame's `this`;
                    // otherwise the data write lands on the receiver.
                    let outcome = match base.as_object() {
                        Some(obj) => crate::object::resolve_symbol_set(obj, &self.gc_heap, sym),
                        None => object::SetOutcome::AssignData,
                    };
                    match outcome {
                        object::SetOutcome::InvokeSetter { setter } => {
                            let mut args: SmallVec<[Value; 8]> = SmallVec::new();
                            args.push(value);
                            self.run_callable_sync_rooted(
                                stack,
                                context,
                                &setter,
                                actual_this,
                                args,
                            )?;
                        }
                        object::SetOutcome::Reject { .. } => {
                            self.failed_set_result(
                                strict,
                                "Cannot assign to read-only symbol property".to_string(),
                            )?;
                        }
                        object::SetOutcome::AssignData
                        | object::SetOutcome::ExoticParent { .. } => {
                            let target = if let Some(this_obj) = actual_this.as_object() {
                                Some(this_obj)
                            } else {
                                actual_this
                                    .as_class_constructor()
                                    .map(|c| c.statics(&self.gc_heap))
                            };
                            let Some(target) = target else {
                                return Err(VmError::TypeMismatch);
                            };
                            if !crate::object::set_symbol(target, &mut self.gc_heap, sym, value) {
                                self.failed_set_result(
                                    strict,
                                    "Cannot assign to read-only symbol property".to_string(),
                                )?;
                            }
                        }
                    }
                    stack[top_idx].advance_pc()?;
                    return Ok(());
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
            stack,
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
                self.run_callable_sync_rooted(stack, context, &setter, actual_this, args)?;
            }
            object::SetOutcome::Reject { .. } => {
                self.failed_set_result(
                    strict,
                    format!("Cannot assign to read-only property '{key}'"),
                )?;
            }
            object::SetOutcome::ExoticParent { parent } => {
                if !self.ordinary_set_data_value(
                    stack,
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
                    stack,
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
                        self.ordinary_get_own_property_descriptor_value(
                            stack,
                            context,
                            actual_this,
                            &VmPropertyKey::String(&key),
                            0,
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
        stack[top_idx].advance_pc()?;
        Ok(())
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
        stack: &mut ActivationStack,
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
                self.run_callable_sync_rooted(stack, context, &setter, Value::object(obj), args)?;
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
                    stack,
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
        stack: &mut ActivationStack,
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
                self.run_callable_sync_rooted(stack, context, &setter, Value::object(obj), args)?;
                Ok(())
            }
            object::SetOutcome::Reject { .. } => {
                self.failed_set_result(strict, "Cannot assign to symbol property")
            }
            object::SetOutcome::ExoticParent { parent } => {
                if !self.ordinary_set_data_value(
                    stack,
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

    /// A collection receiver's per-instance `[[Prototype]]` override
    /// (`class Q extends Map` stores `Q.prototype` here at construct),
    /// or `None` for every other shape. Property loads must consult
    /// this before the canonical constructor-name walk, or subclass
    /// instance methods become invisible.
    pub(crate) fn collection_prototype_override_value(&self, receiver: &Value) -> Option<Value> {
        if let Some(arr) = receiver.as_array() {
            crate::array::prototype_override(arr, &self.gc_heap)
        } else if let Some(map) = receiver.as_map() {
            crate::collections::map_prototype_override(map, &self.gc_heap)
        } else if let Some(set) = receiver.as_set() {
            crate::collections::set_prototype_override(set, &self.gc_heap)
        } else if let Some(map) = receiver.as_weak_map() {
            crate::collections::weak_map_prototype_override(map, &self.gc_heap)
        } else if let Some(set) = receiver.as_weak_set() {
            crate::collections::weak_set_prototype_override(set, &self.gc_heap)
        } else if let Some(buffer) = receiver.as_array_buffer() {
            buffer.custom_proto(&self.gc_heap)
        } else if let Some(view) = receiver.as_data_view() {
            view.custom_proto(&self.gc_heap)
        } else {
            None
        }
    }

    fn load_from_constructor_prototype(
        &mut self,
        stack: &mut ActivationStack,
        context: &ExecutionContext,
        proto_name: &str,
        receiver: &Value,
        name: &str,
    ) -> Result<Value, VmError> {
        let proto = match self.collection_prototype_override_value(receiver) {
            Some(proto) => proto,
            None => self.constructor_prototype_value(proto_name)?,
        };
        let Some(proto_obj) = proto.as_object() else {
            return Ok(Value::undefined());
        };
        let key = VmPropertyKey::String(name);
        match self.ordinary_get_value(
            stack,
            context,
            Value::object(proto_obj),
            *receiver,
            &key,
            0,
        )? {
            VmGetOutcome::Value(value) => Ok(value),
            VmGetOutcome::InvokeGetter { getter } => self.run_callable_sync_rooted(
                stack,
                context,
                &getter,
                *receiver,
                smallvec::SmallVec::new(),
            ),
        }
    }
}

pub(crate) fn string_index_property_name(key: &str) -> Option<u32> {
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

/// Lazy-allocate and cache the TypedArray expando through the VM handle arena.
///
/// The typed-array body is re-read from its scoped handle after allocating the
/// bag, so a nursery move can never make `set_expando` target a stale body.
/// The returned bag is current at scope exit and callers hand it immediately to
/// descriptor/store code; any later allocating operation owns its own roots.
pub(crate) fn typed_array_ensure_expando(
    interp: &mut Interpreter,
    t: &crate::binary::typed_array::JsTypedArray,
) -> Result<JsObject, VmError> {
    if let Some(existing) = t.expando(&interp.gc_heap) {
        return Ok(existing);
    }
    interp.with_handle_scope(|interp, scope| {
        let typed_array = interp.scoped_value(scope, Value::typed_array(*t));
        let bag = interp.scoped_object_bare(scope)?;
        let current_t = interp
            .escape_scoped(typed_array)
            .as_typed_array(&interp.gc_heap)
            .ok_or(VmError::TypeMismatch)?;
        let current_bag = interp
            .escape_scoped(bag)
            .as_object()
            .ok_or(VmError::TypeMismatch)?;
        current_t.set_expando(&mut interp.gc_heap, current_bag);
        Ok(current_bag)
    })
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
    let mut recv = Value::typed_array(*t);
    let recv_ptr: *mut Value = &mut recv;
    let mut external_visit = |visitor: &mut dyn FnMut(*mut RawGc)| {
        // SAFETY: `recv` outlives the allocation; the collector rewrites
        // the embedded moving offset in place.
        unsafe { (*recv_ptr).trace_value_slot_mut(visitor) };
    };
    let bag = crate::object::alloc_object_with_roots(heap, &mut external_visit)?;
    let t = recv
        .as_typed_array(heap)
        .expect("receiver stays a typed array across the bag allocation");
    t.set_expando(heap, bag);
    Ok(bag)
}

/// Lazy-allocate (and cache) the RegExp expando JsObject used
/// to back non-spec own properties like `re.exec = fn`.
pub(crate) fn regexp_ensure_expando(
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
    let mut recv = Value::regexp(*r);
    let recv_ptr: *mut Value = &mut recv;
    let mut external_visit = |visitor: &mut dyn FnMut(*mut RawGc)| {
        // SAFETY: `recv` outlives the allocation; the collector rewrites
        // the embedded moving offset in place.
        unsafe { (*recv_ptr).trace_value_slot_mut(visitor) };
    };
    let bag = crate::object::alloc_object_with_roots(heap, &mut external_visit)?;
    let r = recv
        .as_regexp()
        .expect("receiver stays a regexp across the bag allocation");
    // A RegExp keeps its own `[[Extensible]]` slot, so a bag created after
    // `Object.preventExtensions` must be born non-extensible — otherwise the
    // first own-property store materialises a fresh extensible bag and slips
    // past the frozen receiver.
    if !r.is_extensible(heap) {
        crate::object::prevent_extensions(bag, heap);
    }
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
    let mut recv = Value::map(m);
    let recv_ptr: *mut Value = &mut recv;
    let mut external_visit = |visitor: &mut dyn FnMut(*mut RawGc)| {
        // SAFETY: `recv` outlives the allocation; the collector rewrites
        // the embedded moving offset in place.
        unsafe { (*recv_ptr).trace_value_slot_mut(visitor) };
    };
    let bag = crate::object::alloc_object_with_roots(heap, &mut external_visit)?;
    let m = recv
        .as_map()
        .expect("receiver stays a map across the bag allocation");
    crate::collections::map_set_expando(m, heap, bag);
    Ok(bag)
}

/// As [`map_ensure_expando_pub`] for a WeakMap.
pub(crate) fn weak_map_ensure_expando_pub(
    heap: &mut otter_gc::GcHeap,
    m: crate::collections::JsWeakMap,
) -> Result<JsObject, VmError> {
    if let Some(existing) = crate::collections::weak_map_expando(m, heap) {
        return Ok(existing);
    }
    let mut recv = Value::weak_map(m);
    let recv_ptr: *mut Value = &mut recv;
    let mut external_visit = |visitor: &mut dyn FnMut(*mut RawGc)| {
        // SAFETY: `recv` outlives the allocation; the collector rewrites
        // the embedded moving offset in place.
        unsafe { (*recv_ptr).trace_value_slot_mut(visitor) };
    };
    let bag = crate::object::alloc_object_with_roots(heap, &mut external_visit)?;
    let m = recv
        .as_weak_map()
        .expect("receiver stays a weak map across the bag allocation");
    crate::collections::weak_map_set_expando(m, heap, bag);
    Ok(bag)
}

/// As [`map_ensure_expando_pub`] for a WeakSet.
pub(crate) fn weak_set_ensure_expando_pub(
    heap: &mut otter_gc::GcHeap,
    s: crate::collections::JsWeakSet,
) -> Result<JsObject, VmError> {
    if let Some(existing) = crate::collections::weak_set_expando(s, heap) {
        return Ok(existing);
    }
    let mut recv = Value::weak_set(s);
    let recv_ptr: *mut Value = &mut recv;
    let mut external_visit = |visitor: &mut dyn FnMut(*mut RawGc)| {
        // SAFETY: `recv` outlives the allocation; the collector rewrites
        // the embedded moving offset in place.
        unsafe { (*recv_ptr).trace_value_slot_mut(visitor) };
    };
    let bag = crate::object::alloc_object_with_roots(heap, &mut external_visit)?;
    let s = recv
        .as_weak_set()
        .expect("receiver stays a weak set across the bag allocation");
    crate::collections::weak_set_set_expando(s, heap, bag);
    Ok(bag)
}

/// As [`map_ensure_expando_pub`] for a Generator object — generators
/// are ordinary extensible objects (§27.5.2) whose user-defined own
/// properties (`gen.return = fn`) live on this lazy bag.
pub(crate) fn generator_ensure_expando_pub(
    heap: &mut otter_gc::GcHeap,
    g: &crate::generator::JsGenerator,
) -> Result<JsObject, VmError> {
    if let Some(existing) = g.expando(heap) {
        return Ok(existing);
    }
    let mut recv = Value::generator(*g);
    let recv_ptr: *mut Value = &mut recv;
    let mut external_visit = |visitor: &mut dyn FnMut(*mut RawGc)| {
        // SAFETY: `recv` outlives the allocation; the collector rewrites
        // the embedded moving offset in place.
        unsafe { (*recv_ptr).trace_value_slot_mut(visitor) };
    };
    let bag = crate::object::alloc_object_with_roots(heap, &mut external_visit)?;
    let g = recv
        .as_generator()
        .expect("receiver stays a generator across the bag allocation");
    g.set_expando(heap, bag);
    Ok(bag)
}

/// As [`map_ensure_expando_pub`] for a Set.
pub(crate) fn set_ensure_expando_pub(
    heap: &mut otter_gc::GcHeap,
    mut s: crate::collections::JsSet,
) -> Result<JsObject, VmError> {
    if let Some(existing) = crate::collections::set_expando(s, heap) {
        return Ok(existing);
    }
    let mut external_visit = |visitor: &mut dyn FnMut(*mut RawGc)| {
        visitor(std::ptr::addr_of_mut!(s) as *mut RawGc);
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
    let mut recv = Value::temporal(*t);
    let recv_ptr: *mut Value = &mut recv;
    let mut external_visit = |visitor: &mut dyn FnMut(*mut RawGc)| {
        // SAFETY: `recv` outlives the allocation; the collector rewrites
        // the embedded moving offset in place.
        unsafe { (*recv_ptr).trace_value_slot_mut(visitor) };
    };
    let bag = crate::object::alloc_object_with_roots(heap, &mut external_visit)?;
    let t = recv
        .as_temporal(heap)
        .expect("receiver stays a temporal instance across the bag allocation");
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
    let mut recv = Value::promise(*p);
    let recv_ptr: *mut Value = &mut recv;
    let mut external_visit = |visitor: &mut dyn FnMut(*mut RawGc)| {
        // SAFETY: `recv` outlives the allocation; the collector rewrites
        // the embedded moving offset in place.
        unsafe { (*recv_ptr).trace_value_slot_mut(visitor) };
    };
    let bag = crate::object::alloc_object_with_roots(heap, &mut external_visit)?;
    let p = recv
        .as_promise()
        .expect("receiver stays a promise across the bag allocation");
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
    let mut recv = Value::array_buffer(*b);
    let recv_ptr: *mut Value = &mut recv;
    let mut external_visit = |visitor: &mut dyn FnMut(*mut RawGc)| {
        // SAFETY: `recv` outlives the allocation; the collector rewrites
        // the embedded moving offset in place.
        unsafe { (*recv_ptr).trace_value_slot_mut(visitor) };
    };
    let bag = crate::object::alloc_object_with_roots(heap, &mut external_visit)?;
    let b = recv
        .as_array_buffer()
        .expect("receiver stays an array buffer across the bag allocation");
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
    let mut recv = Value::data_view(*dv);
    let recv_ptr: *mut Value = &mut recv;
    let mut external_visit = |visitor: &mut dyn FnMut(*mut RawGc)| {
        // SAFETY: `recv` outlives the allocation; the collector rewrites
        // the embedded moving offset in place.
        unsafe { (*recv_ptr).trace_value_slot_mut(visitor) };
    };
    let bag = crate::object::alloc_object_with_roots(heap, &mut external_visit)?;
    let dv = recv
        .as_data_view()
        .expect("receiver stays a data view across the bag allocation");
    dv.set_expando(heap, bag);
    Ok(bag)
}
