//! Prototype resolution and primitive receiver boxing.
//!
//! # Contents
//! `get_prototype_for_op`, non-GC exotic prototype overrides, primitive
//! wrapper prototypes, sloppy-mode `this` boxing, well-known symbols,
//! and `install_global_class`.
#![allow(unused_imports)]
use crate::*;

impl Interpreter {
    /// Return the realm's shared `%ThrowTypeError%` function.
    ///
    /// Bootstrap installs it as the getter/setter for
    /// `Function.prototype.caller`; unmapped arguments objects reuse
    /// that exact function object for `callee` so Test262's
    /// well-known-intrinsic identity checks observe one realm-local
    /// intrinsic.
    pub(crate) fn restricted_throw_type_error(&self) -> Result<Value, VmError> {
        let prototype = self.function_prototype_object()?;
        match object::get_own_descriptor(prototype, &self.gc_heap, "caller") {
            Some(object::PropertyDescriptor {
                kind:
                    object::DescriptorKind::Accessor {
                        getter: Some(getter),
                        ..
                    },
                ..
            }) => Ok(getter),
            _ => Err(VmError::TypeMismatch),
        }
    }

    pub(crate) fn non_gc_exotic_prototype_override_key(
        value: &Value,
        heap: &otter_gc::GcHeap,
    ) -> Option<usize> {
        if let Some(buffer) = value.as_array_buffer() {
            return Some(buffer.identity_addr() as usize);
        }
        if let Some(view) = value.as_data_view() {
            return Some(view.identity_addr() as usize);
        }
        if let Some(intl) = value.as_intl(heap) {
            return Some(intl.identity_addr() as usize);
        }
        if let Some(iter) = value.as_iterator() {
            return Some(iter.as_header_ptr() as usize);
        }
        value
            .as_typed_array(heap)
            .map(|array| array.identity_addr() as usize)
    }

    /// Store the allocation-time `[[Prototype]]` selected by
    /// ECMA-262 `GetPrototypeFromConstructor` for exotics whose
    /// bodies are not GC-managed yet.
    ///
    /// # See also
    /// - <https://tc39.es/ecma262/#sec-getprototypefromconstructor>
    pub(crate) fn set_non_gc_exotic_prototype_override(
        &mut self,
        value: &Value,
        proto: Option<Value>,
    ) {
        let Some(key) = Self::non_gc_exotic_prototype_override_key(value, &self.gc_heap) else {
            return;
        };
        match proto {
            Some(proto) => {
                self.non_gc_exotic_prototype_overrides.insert(key, proto);
            }
            None => {
                self.non_gc_exotic_prototype_overrides.remove(&key);
            }
        }
    }

    pub(crate) fn non_gc_exotic_prototype_override(&self, value: &Value) -> Option<Value> {
        let key = Self::non_gc_exotic_prototype_override_key(value, &self.gc_heap)?;
        self.non_gc_exotic_prototype_overrides.get(&key).cloned()
    }

    pub(crate) fn non_gc_exotic_user_props(&self, value: &Value) -> Option<JsObject> {
        let key = Self::non_gc_exotic_prototype_override_key(value, &self.gc_heap)?;
        self.non_gc_exotic_user_props.get(&key).copied()
    }

    pub(crate) fn ensure_non_gc_exotic_user_props(
        &mut self,
        value: &Value,
    ) -> Result<Option<JsObject>, VmError> {
        let Some(key) = Self::non_gc_exotic_prototype_override_key(value, &self.gc_heap) else {
            return Ok(None);
        };
        if let Some(existing) = self.non_gc_exotic_user_props.get(&key) {
            return Ok(Some(*existing));
        }
        let receiver = *value;
        let mut external_visit = |visitor: &mut dyn FnMut(*mut otter_gc::raw::RawGc)| {
            receiver.trace_value_slots(visitor);
        };
        let bag = crate::object::alloc_object_with_roots(&mut self.gc_heap, &mut external_visit)?;
        self.non_gc_exotic_user_props.insert(key, bag);
        Ok(Some(bag))
    }

    /// `[[GetPrototypeOf]]` for non-Proxy heap values. Centralises
    /// the foundation rule that constructor-shaped Objects whose
    /// stored `[[Prototype]]` is missing — or is the realm's
    /// `%Object.prototype%` (the default link from many bootstrap
    /// installers) — surface as `%Function.prototype%`. Explicit
    /// proto links to anything else (e.g. `Error.[[Prototype]]` =
    /// `%Function.prototype%`, `TypeError.[[Prototype]]` = `Error`)
    /// are honoured verbatim.
    ///
    /// # See also
    /// - <https://tc39.es/ecma262/#sec-ordinarygetprototypeof>
    pub(crate) fn get_prototype_for_op(&mut self, value: &Value) -> Result<Value, VmError> {
        // §15.7.14 step 6.b — a class constructor's [[Prototype]] is
        // the parent class value (identity preserved in the
        // ctor_proto slot), %Function.prototype% for a base class,
        // or null for `extends null` / a later setPrototypeOf.
        if let Some(c) = value.as_class_constructor() {
            let stored = c.ctor_proto(&self.gc_heap);
            if !stored.is_undefined() {
                return Ok(stored);
            }
            return Ok(Value::object(self.function_prototype_object()?));
        }
        let intrinsic_or_null =
            |this: &mut Self, v: &Value| match this.intrinsic_prototype_object_for(v) {
                Some(o) => Value::object(o),
                None => Value::null(),
            };
        if let Some(obj) = value.as_object() {
            let stored = object::prototype_value(obj, &self.gc_heap);
            let has_construct = object_has_construct_slot(&Value::object(obj), &self.gc_heap);
            if has_construct {
                let function_proto = self.function_prototype_object().ok();
                let object_proto = self.object_prototype_object_opt();
                match &stored {
                    None => {
                        if let Some(fp) = function_proto {
                            return Ok(Value::object(fp));
                        }
                    }
                    Some(p_val) if p_val.as_object().is_some_and(|p| object_proto == Some(p)) => {
                        if let Some(fp) = function_proto {
                            return Ok(Value::object(fp));
                        }
                    }
                    _ => {}
                }
            }
            return Ok(stored.unwrap_or(Value::null()));
        }
        if let Some(t) = value.as_typed_array(&self.gc_heap) {
            if let Some(over) = t.custom_proto(&self.gc_heap) {
                return Ok(over);
            }
            return Ok(intrinsic_or_null(self, value));
        }
        if let Some(nf) = value.as_native_function() {
            if let Some(over) = nf.prototype_override(&self.gc_heap) {
                return Ok(over);
            }
            return Ok(Value::object(self.function_prototype_object()?));
        }
        if let Some(arr) = value.as_array() {
            if let Some(over) = array::prototype_override(arr, &self.gc_heap) {
                return Ok(over);
            }
            return Ok(intrinsic_or_null(self, value));
        }
        if let Some(map) = value.as_map() {
            if let Some(over) = crate::collections::map_prototype_override(map, &self.gc_heap) {
                return Ok(over);
            }
            return Ok(intrinsic_or_null(self, value));
        }
        if let Some(set) = value.as_set() {
            if let Some(over) = crate::collections::set_prototype_override(set, &self.gc_heap) {
                return Ok(over);
            }
            return Ok(intrinsic_or_null(self, value));
        }
        if let Some(map) = value.as_weak_map() {
            if let Some(over) = crate::collections::weak_map_prototype_override(map, &self.gc_heap)
            {
                return Ok(over);
            }
            return Ok(intrinsic_or_null(self, value));
        }
        if let Some(set) = value.as_weak_set() {
            if let Some(over) = crate::collections::weak_set_prototype_override(set, &self.gc_heap)
            {
                return Ok(over);
            }
            return Ok(intrinsic_or_null(self, value));
        }
        if let Some(promise) = value.as_promise() {
            if let Some(over) = promise.prototype_override(&self.gc_heap) {
                return Ok(over);
            }
            return Ok(intrinsic_or_null(self, value));
        }
        if let Some(regexp) = value.as_regexp() {
            if let Some(over) = regexp.prototype_override(&self.gc_heap) {
                return Ok(over);
            }
            return Ok(intrinsic_or_null(self, value));
        }
        if let Some(weak_ref) = value.as_weak_ref() {
            if let Some(over) =
                crate::weak_refs::weak_ref_prototype_override(weak_ref, &self.gc_heap)
            {
                return Ok(over);
            }
            return Ok(intrinsic_or_null(self, value));
        }
        if let Some(registry) = value.as_finalization_registry() {
            if let Some(over) =
                crate::weak_refs::finalization_registry_prototype_override(registry, &self.gc_heap)
            {
                return Ok(over);
            }
            return Ok(intrinsic_or_null(self, value));
        }
        if value.as_iterator().is_some() {
            if let Some(over) = self.non_gc_exotic_prototype_override(value) {
                return Ok(over);
            }
            return Ok(intrinsic_or_null(self, value));
        }
        if value.is_function()
            || value.is_closure()
            || value.is_bound_function()
            || value.is_class_constructor()
        {
            // §10.2 ordinary bytecode functions: the kind prototype
            // (%GeneratorFunction.prototype% et al.) for generator /
            // async flavours — resolved context-free through the
            // shared code space so proto-chain walks (`instanceof`,
            // `Reflect.getPrototypeOf`) see the same graph as
            // property reads — else `%Function.prototype%`.
            if let Some(function_id) = value.as_function().or_else(|| {
                value
                    .as_closure(&self.gc_heap)
                    .map(|c| c.cached_function_id)
            }) {
                if let Some(over) = self.function_prototype_overrides.get(&function_id).copied() {
                    return Ok(over);
                }
                if let Some(chunk) = self.code_space.chunk_for(function_id)
                    && let Some(local) = function_id.checked_sub(chunk.function_base)
                    && let Some(function) = chunk.module.functions.get(local as usize)
                    && let Some(proto) = self.function_kind_prototypes.kind_prototype_for_flags(
                        function.is_generator,
                        function.is_async || function.is_async_generator,
                    )
                {
                    return Ok(Value::object(proto));
                }
            }
            return Ok(Value::object(self.function_prototype_object()?));
        }
        // §10.4 exotic objects (ArrayBuffer / SharedArrayBuffer /
        // DataView / TypedArray) — per-class realm prototype.
        // <https://tc39.es/ecma262/#sec-ordinarygetprototypeof>
        if value.is_array_buffer() || value.is_data_view() || value.is_typed_array() {
            if let Some(over) = self.non_gc_exotic_prototype_override(value) {
                return Ok(over);
            }
            return Ok(intrinsic_or_null(self, value));
        }
        if let Some(t) = value.as_temporal(&self.gc_heap) {
            return Ok(self
                .temporal_prototype_object(t.kind())
                .map(Value::object)
                .unwrap_or(Value::null()));
        }
        if let Some(intl) = value.as_intl(&self.gc_heap) {
            if let Some(over) = self.non_gc_exotic_prototype_override(value) {
                return Ok(over);
            }
            return Ok(self.intl_kind_prototype_value(intl.kind().class_name()));
        }
        if let Some(generator) = value.as_generator() {
            if let Some(proto) = generator.prototype_override(&self.gc_heap) {
                return Ok(proto);
            }
            return Ok(intrinsic_or_null(self, value));
        }
        if value.is_iterator() {
            if let Some(over) = self.non_gc_exotic_prototype_override(value) {
                return Ok(over);
            }
            return Ok(intrinsic_or_null(self, value));
        }
        // §20.1.2.10 / §7.1.18 — primitives ToObject then walk
        // wrapper's [[Prototype]].
        if value.is_symbol()
            || value.is_string()
            || value.is_number()
            || value.is_boolean()
            || value.is_big_int()
        {
            return Ok(intrinsic_or_null(self, value));
        }
        Err(self.err_type_mismatch_at("Object.getPrototypeOf", value_kind_name(value)))
    }

    pub(crate) fn object_prototype_object_opt(&self) -> Option<JsObject> {
        // Fast path: typed slot populated by RealmIntrinsics::populate.
        if let Some(proto) = self.realm_intrinsics.object_prototype {
            return Some(proto);
        }
        // Fallback for embedders that build a non-default global
        // (e.g. feature-gated bootstrap that omits Object).
        let ctor =
            object::get(self.global_this, &self.gc_heap, "Object").and_then(|v| v.as_object())?;
        object::get(ctor, &self.gc_heap, "prototype").and_then(|v| v.as_object())
    }

    pub(crate) fn function_prototype_object(&self) -> Result<JsObject, VmError> {
        // Fast path: typed slot.
        if let Some(proto) = self.realm_intrinsics.function_prototype {
            return Ok(proto);
        }
        let function_ctor = object::get(self.global_this, &self.gc_heap, "Function")
            .and_then(|v| v.as_object())
            .ok_or(VmError::TypeMismatch)?;
        object::get(function_ctor, &self.gc_heap, "prototype")
            .and_then(|v| v.as_object())
            .ok_or(VmError::TypeMismatch)
    }

    pub(crate) fn is_callable_runtime(&self, value: &Value) -> bool {
        // §10.5.15 — a Proxy is callable only when its target was
        // callable at creation (the heap-blind `is_callable` assumes
        // every proxy is callable). Resolve the real [[Call]] slot here.
        if let Some(proxy) = value.as_proxy() {
            return proxy.is_callable(&self.gc_heap);
        }
        is_callable(value) || object_has_call_slot(value, &self.gc_heap)
    }

    /// Resolve property read on function / closure. Honours user
    /// props via `function_user_props`, lazily allocates
    /// `function_user_props` side table, lazily allocates
    /// `Function.prototype` on first access (§9.2.10
    /// MakeConstructor), and falls back to `name` / `length`
    /// intrinsics. Unknown names return `undefined` per §10.1.8
    /// OrdinaryGet step 4.
    /// Borrow the per-interpreter table of well-known symbol
    /// singletons. The table is constant across the interpreter's
    /// lifetime.
    #[must_use]
    pub fn well_known_symbols(&self) -> &WellKnownSymbols {
        &self.well_known_symbols
    }

    /// Run an intrinsic's well-knowns install hook (the second phase of
    /// the bootstrap registry walk) against this interpreter's heap and
    /// well-known symbol table. Exists because host-side installers
    /// (runtime-builder global classes) hold only `&mut Interpreter`
    /// and the hook needs the heap mutably alongside the symbol table —
    /// a split borrow only this impl can perform.
    pub fn run_install_well_knowns(
        &mut self,
        install: fn(
            &mut otter_gc::GcHeap,
            crate::object::JsObject,
            &WellKnownSymbols,
        ) -> Result<(), crate::js_surface::JsSurfaceError>,
        global: crate::object::JsObject,
    ) -> Result<(), crate::js_surface::JsSurfaceError> {
        install(&mut self.gc_heap, global, &self.well_known_symbols)
    }

    /// Borrow the global symbol registry backing `Symbol.for` /
    /// `Symbol.keyFor`. Returns the same instance across calls.
    #[must_use]
    pub fn symbol_registry(&self) -> &SymbolRegistry {
        &self.symbol_registry
    }

    /// Look up or register a symbol for `key`. Splits borrows over the
    /// registry, the GC heap, and the string heap so callers do not
    /// need to juggle them manually.
    ///
    /// # Errors
    /// Surfaces [`crate::symbol::SymbolRegistryError`] (string or GC
    /// out-of-memory).
    pub fn symbol_for_key(
        &mut self,
        key: &str,
    ) -> Result<JsSymbol, crate::symbol::SymbolRegistryError> {
        self.symbol_registry.for_key(&mut self.gc_heap, key)
    }
}

impl Interpreter {
    pub(crate) fn primitive_wrapper_prototype(
        &mut self,
        constructor_name: &str,
    ) -> Result<JsObject, VmError> {
        let constructor = object::get(self.global_this, &self.gc_heap, constructor_name)
            .ok_or_else(|| VmError::InvalidOperand)?;
        let prototype = if let Some(ctor) = constructor.as_object() {
            object::get(ctor, &self.gc_heap, "prototype")
        } else if let Some(native) = constructor.as_native_function() {
            let desc = native
                .own_property_descriptor(&mut self.gc_heap, "prototype")
                .map_err(|_| VmError::InvalidOperand)?;
            desc.and_then(|d| match d.kind {
                object::DescriptorKind::Data { value } => Some(value),
                _ => None,
            })
        } else {
            None
        };
        prototype
            .and_then(|v| v.as_object())
            .ok_or_else(|| VmError::InvalidOperand)
    }

    pub(crate) fn box_sloppy_this_primitive_runtime_rooted(
        &mut self,
        this_value: Value,
        slice_roots: &[&[Value]],
    ) -> Result<Value, VmError> {
        let object = if let Some(value) = this_value.as_boolean() {
            let proto = self.primitive_wrapper_prototype("Boolean")?;
            let obj =
                self.alloc_runtime_rooted_object_with_proto(proto, &[&this_value], slice_roots)?;
            object::set_boolean_data(obj, &mut self.gc_heap, value);
            obj
        } else if let Some(value) = this_value.as_number() {
            let proto = self.primitive_wrapper_prototype("Number")?;
            let obj =
                self.alloc_runtime_rooted_object_with_proto(proto, &[&this_value], slice_roots)?;
            object::set_number_data(obj, &mut self.gc_heap, value);
            obj
        } else if let Some(value) = this_value.as_string(&self.gc_heap) {
            let proto = self.primitive_wrapper_prototype("String")?;
            let obj =
                self.alloc_runtime_rooted_object_with_proto(proto, &[&this_value], slice_roots)?;
            object::set_string_data(obj, &mut self.gc_heap, value);
            obj
        } else if let Some(sym) = this_value.as_symbol(&self.gc_heap) {
            let proto = self.primitive_wrapper_prototype("Symbol")?;
            let obj =
                self.alloc_runtime_rooted_object_with_proto(proto, &[&this_value], slice_roots)?;
            object::set_symbol_data(obj, &mut self.gc_heap, sym);
            obj
        } else if let Some(value) = this_value.as_big_int() {
            let proto = self.primitive_wrapper_prototype("BigInt")?;
            let obj =
                self.alloc_runtime_rooted_object_with_proto(proto, &[&this_value], slice_roots)?;
            object::set_bigint_data(obj, &mut self.gc_heap, value);
            obj
        } else {
            return Ok(this_value);
        };
        Ok(Value::object(object))
    }

    pub(crate) fn box_sloppy_this_primitive_stack_rooted(
        &mut self,
        stack: &ActivationStack,
        this_value: Value,
        slice_roots: &[&[Value]],
    ) -> Result<Value, VmError> {
        let object = if let Some(value) = this_value.as_boolean() {
            let proto = self.primitive_wrapper_prototype("Boolean")?;
            let obj = self.alloc_stack_rooted_object_with_proto(
                stack,
                proto,
                &[&this_value],
                slice_roots,
            )?;
            object::set_boolean_data(obj, &mut self.gc_heap, value);
            obj
        } else if let Some(value) = this_value.as_number() {
            let proto = self.primitive_wrapper_prototype("Number")?;
            let obj = self.alloc_stack_rooted_object_with_proto(
                stack,
                proto,
                &[&this_value],
                slice_roots,
            )?;
            object::set_number_data(obj, &mut self.gc_heap, value);
            obj
        } else if let Some(value) = this_value.as_string(&self.gc_heap) {
            let proto = self.primitive_wrapper_prototype("String")?;
            let obj = self.alloc_stack_rooted_object_with_proto(
                stack,
                proto,
                &[&this_value],
                slice_roots,
            )?;
            object::set_string_data(obj, &mut self.gc_heap, value);
            obj
        } else if let Some(sym) = this_value.as_symbol(&self.gc_heap) {
            let proto = self.primitive_wrapper_prototype("Symbol")?;
            let obj = self.alloc_stack_rooted_object_with_proto(
                stack,
                proto,
                &[&this_value],
                slice_roots,
            )?;
            object::set_symbol_data(obj, &mut self.gc_heap, sym);
            obj
        } else if let Some(value) = this_value.as_big_int() {
            let proto = self.primitive_wrapper_prototype("BigInt")?;
            let obj = self.alloc_stack_rooted_object_with_proto(
                stack,
                proto,
                &[&this_value],
                slice_roots,
            )?;
            object::set_bigint_data(obj, &mut self.gc_heap, value);
            obj
        } else {
            return Ok(this_value);
        };
        Ok(Value::object(object))
    }

    pub(crate) fn object_for_primitive_property_base_stack_rooted(
        &mut self,
        stack: &ActivationStack,
        value: &Value,
    ) -> Result<Option<JsObject>, VmError> {
        let object = if let Some(v) = value.as_boolean() {
            let proto = self.primitive_wrapper_prototype("Boolean")?;
            let obj = self.alloc_stack_rooted_object_with_proto(stack, proto, &[value], &[])?;
            object::set_boolean_data(obj, &mut self.gc_heap, v);
            obj
        } else if let Some(v) = value.as_number() {
            let proto = self.primitive_wrapper_prototype("Number")?;
            let obj = self.alloc_stack_rooted_object_with_proto(stack, proto, &[value], &[])?;
            object::set_number_data(obj, &mut self.gc_heap, v);
            obj
        } else if let Some(v) = value.as_string(&self.gc_heap) {
            let proto = self.primitive_wrapper_prototype("String")?;
            let obj = self.alloc_stack_rooted_object_with_proto(stack, proto, &[value], &[])?;
            object::set_string_data(obj, &mut self.gc_heap, v);
            obj
        } else if value.is_symbol() {
            let proto = self.primitive_wrapper_prototype("Symbol")?;
            self.alloc_stack_rooted_object_with_proto(stack, proto, &[value], &[])?
        } else if value.is_big_int() {
            let proto = self.primitive_wrapper_prototype("BigInt")?;
            self.alloc_stack_rooted_object_with_proto(stack, proto, &[value], &[])?
        } else {
            return Ok(None);
        };
        Ok(Some(object))
    }

    pub(crate) fn this_for_bytecode_call_runtime_rooted(
        &mut self,
        function: &CodeBlock,
        this_value: Value,
        slice_roots: &[&[Value]],
    ) -> Result<Value, VmError> {
        if function.is_strict || function.is_arrow {
            return Ok(this_value);
        }
        // The dominant sloppy-method case is an object receiver (`recv.m()`),
        // which is its own `this` — return it before the primitive-wrapper ladder.
        if this_value.as_object().is_some() {
            return Ok(this_value);
        }
        match this_value {
            v if v.is_undefined() || v.is_null() => Ok(Value::object(self.global_this)),
            other => self.box_sloppy_this_primitive_runtime_rooted(other, slice_roots),
        }
    }

    pub(crate) fn this_for_bytecode_call_stack_rooted(
        &mut self,
        function: &CodeBlock,
        stack: &ActivationStack,
        this_value: Value,
        slice_roots: &[&[Value]],
    ) -> Result<Value, VmError> {
        if function.is_strict || function.is_arrow {
            return Ok(this_value);
        }
        if this_value.as_object().is_some() {
            return Ok(this_value);
        }
        if this_value.is_undefined() || this_value.is_null() {
            Ok(Value::object(self.global_this))
        } else {
            self.box_sloppy_this_primitive_stack_rooted(stack, this_value, slice_roots)
        }
    }

    /// Install a class-shaped global from a static JS surface spec.
    ///
    /// Product crates use this for centralized bootstrap wiring:
    /// specs stay static, while the actual object allocation and
    /// global mutation happen during one mutator turn.
    pub fn install_global_class(&mut self, spec: &'static ClassSpec) -> Result<(), JsSurfaceError> {
        let _runtime_roots_guard = self.scope_runtime_roots_guard();
        let global_root = Value::object(self.global_this);
        let value = ClassBuilder::from_spec_with_raw_and_value_roots(
            &mut self.gc_heap,
            spec,
            Vec::new(),
            vec![global_root],
        )
        .build()?;
        let descriptor = crate::object::PropertyDescriptor::data(
            value,
            spec.constructor.attrs.writable,
            spec.constructor.attrs.enumerable,
            spec.constructor.attrs.configurable,
        );
        if crate::object::define_own_property(
            self.global_this,
            &mut self.gc_heap,
            spec.constructor.name,
            descriptor,
        ) {
            Ok(())
        } else {
            Err(JsSurfaceError::DefinePropertyFailed(spec.constructor.name))
        }
    }
}
