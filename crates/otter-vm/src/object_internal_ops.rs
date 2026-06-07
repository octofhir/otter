//! Object internal-method support helpers.
//!
//! These helpers back the VM's spec-shaped object and Proxy internal methods.
//! They are shared by the main `ordinary_*` algorithms, property opcode
//! dispatch, and conversion paths, so they live outside `lib.rs` without being
//! tied to a specific bytecode.
//!
//! # Contents
//! - Proxy trap invocation.
//! - VM property-key conversion and own-property lookup helpers.
//! - String exotic property reads/descriptors.
//! - Proxy invariant validation helpers.
//! - Realm constructor prototype lookup.
//!
//! # Invariants
//! - Proxy traps are invoked through the normal callable path.
//! - String exotic keys only synthesize `length` and index descriptors.
//! - Constructor prototype lookup preserves existing global-object semantics.
//!
//! # See also
//! - [`crate::property_dispatch`]
//! - [`crate::object`]

use std::collections::BTreeSet;

use smallvec::SmallVec;

use crate::{
    ExecutionContext, Frame, Interpreter, JsObject, JsString, Value, VmError, VmGetOutcome,
    VmPropertyKey, abstract_ops, array, descriptor_value, function_metadata, object, proxy,
    regexp_prototype, string, symbol, to_length,
};

#[derive(Clone, Copy)]
pub(crate) enum ObjectIntegrityLevel {
    Sealed,
    Frozen,
}

/// Convert an already-primitive value to a [`VmPropertyKey`] per
/// §7.1.19 step 2-3: Symbol values pass through unchanged; every
/// other primitive coerces to a UTF-16 string spelling.
fn primitive_to_property_key(
    value: Value,
    heap: &otter_gc::GcHeap,
) -> Result<VmPropertyKey<'static>, VmError> {
    if let Some(sym) = value.as_symbol(heap) {
        return Ok(VmPropertyKey::Symbol(sym));
    }
    if let Some(s) = value.as_string(heap) {
        return Ok(VmPropertyKey::OwnedString(s.to_lossy_string(heap)));
    }
    if let Some(n) = value.as_number() {
        return Ok(VmPropertyKey::OwnedString(n.to_display_string()));
    }
    if let Some(b) = value.as_boolean() {
        return Ok(VmPropertyKey::String(if b { "true" } else { "false" }));
    }
    if value.is_null() {
        return Ok(VmPropertyKey::String("null"));
    }
    if value.is_undefined() {
        return Ok(VmPropertyKey::String("undefined"));
    }
    if let Some(b) = value.as_big_int() {
        return Ok(VmPropertyKey::OwnedString(b.to_decimal_string(heap)));
    }
    Err(VmError::TypeMismatch)
}

fn property_key_value_to_vm_key(
    value: &Value,
    heap: &otter_gc::GcHeap,
) -> Result<VmPropertyKey<'static>, VmError> {
    if let Some(s) = value.as_string(heap) {
        return Ok(VmPropertyKey::OwnedString(s.to_lossy_string(heap)));
    }
    if let Some(sym) = value.as_symbol(heap) {
        return Ok(VmPropertyKey::Symbol(sym));
    }
    Err(VmError::TypeError {
        message: "property key must be a string or symbol".to_string(),
    })
}

fn complete_partial_descriptor_against_current(
    current: &object::PropertyDescriptor,
    partial: &object::PartialPropertyDescriptor,
) -> object::PropertyDescriptor {
    match &current.kind {
        object::DescriptorKind::Data { value } if !partial.is_accessor() => {
            object::PropertyDescriptor::data(
                partial.value.unwrap_or(*value),
                partial.writable.unwrap_or(current.writable()),
                partial.enumerable.unwrap_or(current.enumerable()),
                partial.configurable.unwrap_or(current.configurable()),
            )
        }
        object::DescriptorKind::Accessor { getter, setter } if !partial.is_data() => {
            object::PropertyDescriptor::accessor(
                if partial.get.is_some() {
                    normalize_accessor_slot(partial.get)
                } else {
                    *getter
                },
                if partial.set.is_some() {
                    normalize_accessor_slot(partial.set)
                } else {
                    *setter
                },
                partial.enumerable.unwrap_or(current.enumerable()),
                partial.configurable.unwrap_or(current.configurable()),
            )
        }
        _ if partial.is_accessor() => object::PropertyDescriptor::accessor(
            normalize_accessor_slot(partial.get),
            normalize_accessor_slot(partial.set),
            partial.enumerable.unwrap_or(false),
            partial.configurable.unwrap_or(false),
        ),
        _ => object::PropertyDescriptor::data(
            partial.value.unwrap_or(Value::undefined()),
            partial.writable.unwrap_or(false),
            partial.enumerable.unwrap_or(false),
            partial.configurable.unwrap_or(false),
        ),
    }
}

fn normalize_accessor_slot(value: Option<Value>) -> Option<Value> {
    value.filter(|value| !value.is_undefined())
}

fn same_optional_value(
    left: &Option<Value>,
    right: &Option<Value>,
    heap: &otter_gc::GcHeap,
) -> bool {
    match (left, right) {
        (None, None) => true,
        (Some(left), Some(right)) => abstract_ops::same_value(left, right, heap),
        _ => false,
    }
}

fn descriptor_to_lookup(desc: object::PropertyDescriptor) -> object::PropertyLookup {
    match desc.kind {
        object::DescriptorKind::Data { value } => object::PropertyLookup::Data {
            value,
            flags: desc.flags,
        },
        object::DescriptorKind::Accessor { getter, setter } => object::PropertyLookup::Accessor {
            getter,
            setter,
            flags: desc.flags,
        },
    }
}

#[derive(Clone, Copy)]
enum DescriptorAllocationRoots<'a> {
    Runtime {
        value_roots: &'a [&'a Value],
        slice_roots: &'a [&'a [Value]],
    },
    Stack(&'a SmallVec<[Frame; 8]>),
}

fn partial_descriptor_value_roots(descriptor: &object::PartialPropertyDescriptor) -> Vec<Value> {
    let mut roots = Vec::with_capacity(3);
    if let Some(value) = &descriptor.value {
        roots.push(*value);
    }
    if let Some(get) = &descriptor.get {
        roots.push(*get);
    }
    if let Some(set) = &descriptor.set {
        roots.push(*set);
    }
    roots
}

impl Interpreter {
    /// §28.2 — call a Proxy handler trap. When the trap is missing,
    /// returns `Ok(None)` so the caller can fall through to the
    /// target's behaviour. When the trap exists, invokes it with
    /// `(target, ...trap_args)` (per spec each trap takes the
    /// target as its first explicit argument; subsequent ones come
    /// from `args`) and returns the result.
    pub fn invoke_proxy_trap(
        &mut self,
        context: &ExecutionContext,
        proxy: &crate::proxy::JsProxy,
        trap: &str,
        args: SmallVec<[Value; 8]>,
    ) -> Result<Option<Value>, VmError> {
        if proxy.is_revoked(&self.gc_heap) {
            return Err(VmError::TypeMismatch);
        }
        let handler = proxy.handler(&self.gc_heap);
        let trap_key = VmPropertyKey::String(trap);
        let trap_value = match self.ordinary_get_value(context, handler, handler, &trap_key, 0)? {
            VmGetOutcome::Value(value) => value,
            VmGetOutcome::InvokeGetter { getter } => {
                self.run_callable_sync(context, &getter, handler, SmallVec::new())?
            }
        };
        let trap_fn = if self.is_callable_runtime(&trap_value) {
            trap_value
        } else if trap_value.is_nullish() {
            return Ok(None);
        } else {
            return Err(VmError::TypeMismatch);
        };
        let result = self.run_callable_sync(context, &trap_fn, handler, args)?;
        Ok(Some(result))
    }

    pub(crate) fn vm_property_key_to_value(
        &mut self,
        key: &VmPropertyKey,
    ) -> Result<Value, VmError> {
        if let Some(key) = key.string_name() {
            Ok(Value::string(JsString::from_str(key, &mut self.gc_heap)?))
        } else if let VmPropertyKey::Symbol(sym) = key {
            Ok(Value::symbol(*sym))
        } else {
            unreachable!("every non-string property key is a symbol")
        }
    }

    pub(crate) fn lookup_own_vm_property_key(
        &self,
        obj: JsObject,
        key: &VmPropertyKey,
    ) -> object::PropertyLookup {
        match key {
            VmPropertyKey::Atom(key) => object::lookup_own_atom(obj, &self.gc_heap, *key).lookup,
            VmPropertyKey::Symbol(sym) => object::lookup_own_symbol(obj, &self.gc_heap, *sym),
            _ => object::lookup_own(
                obj,
                &self.gc_heap,
                key.string_name()
                    .expect("non-symbol key has string spelling"),
            ),
        }
    }

    pub(crate) fn string_object_exotic_get(
        &mut self,
        obj: JsObject,
        key: &VmPropertyKey,
    ) -> Result<Option<Value>, VmError> {
        let Some(value) = object::string_data(obj, &self.gc_heap) else {
            return Ok(None);
        };
        let Some(key) = key.string_name() else {
            return Ok(None);
        };
        if key == "length" {
            return Ok(Some(Value::number_i32(value.len() as i32)));
        }
        let Ok(index) = key.parse::<u32>() else {
            return Ok(None);
        };
        let Some(unit) = value.char_code_at(index, &self.gc_heap) else {
            return Ok(None);
        };
        Ok(Some(Value::string(JsString::from_utf16_units(
            &[unit],
            &mut self.gc_heap,
        )?)))
    }

    pub(crate) fn string_object_exotic_descriptor(
        &mut self,
        obj: JsObject,
        key: &VmPropertyKey,
    ) -> Result<Option<object::PropertyDescriptor>, VmError> {
        let Some(value) = object::string_data(obj, &self.gc_heap) else {
            return Ok(None);
        };
        string::exotic::descriptor_for_key(value, key, &mut self.gc_heap)
    }

    fn target_is_non_extensible_object(&self, target: &Value) -> bool {
        target
            .as_object()
            .is_some_and(|obj| !object::is_extensible(obj, &self.gc_heap))
    }

    pub(crate) fn validate_proxy_get_own_property_descriptor(
        &self,
        target: &Value,
        target_desc: Option<&object::PropertyDescriptor>,
        trap_desc: Option<&object::PropertyDescriptor>,
    ) -> Result<(), VmError> {
        match (target_desc, trap_desc) {
            (Some(target_desc), None) => {
                if !target_desc.configurable() || self.target_is_non_extensible_object(target) {
                    return Err(VmError::TypeError {
                        message: "Proxy getOwnPropertyDescriptor trap cannot hide target property"
                            .to_string(),
                    });
                }
            }
            (None, Some(trap_desc)) => {
                if self.target_is_non_extensible_object(target) || !trap_desc.configurable() {
                    return Err(VmError::TypeError {
                        message:
                            "Proxy getOwnPropertyDescriptor trap reported incompatible property"
                                .to_string(),
                    });
                }
            }
            (Some(target_desc), Some(trap_desc)) => {
                if !target_desc.configurable() && trap_desc.configurable() {
                    return Err(VmError::TypeError {
                        message: "Proxy getOwnPropertyDescriptor trap reported configurable descriptor for non-configurable target property".to_string(),
                    });
                }
                if !trap_desc.configurable() && target_desc.configurable() {
                    return Err(VmError::TypeError {
                        message: "Proxy getOwnPropertyDescriptor trap reported non-configurable descriptor for configurable target property".to_string(),
                    });
                }
                if !trap_desc.configurable()
                    && matches!(
                        (&target_desc.kind, &trap_desc.kind),
                        (
                            object::DescriptorKind::Data { .. },
                            object::DescriptorKind::Data { .. }
                        )
                    )
                    && target_desc.writable()
                    && !trap_desc.writable()
                {
                    return Err(VmError::TypeError {
                        message: "Proxy getOwnPropertyDescriptor trap reported non-writable descriptor for writable target property".to_string(),
                    });
                }
            }
            (None, None) => {}
        }
        Ok(())
    }

    fn proxy_get_own_target_descriptor(
        &self,
        target: &Value,
        key: &VmPropertyKey,
    ) -> Option<object::PropertyDescriptor> {
        let obj = target.as_object()?;
        if let Some(key) = key.string_name() {
            object::get_own_descriptor(obj, &self.gc_heap, key)
        } else if let VmPropertyKey::Symbol(sym) = key {
            object::get_own_symbol_descriptor(obj, &self.gc_heap, *sym)
        } else {
            None
        }
    }

    pub(crate) fn validate_proxy_get_invariants(
        &self,
        target: &Value,
        key: &VmPropertyKey,
        trap_result: &Value,
    ) -> Result<(), VmError> {
        let Some(desc) = self.proxy_get_own_target_descriptor(target, key) else {
            return Ok(());
        };
        match desc.kind {
            object::DescriptorKind::Data { value }
                if !desc.configurable()
                    && !desc.writable()
                    && !abstract_ops::same_value(trap_result, &value, &self.gc_heap) =>
            {
                return Err(VmError::TypeError {
                        message: "Proxy get trap returned incompatible value for non-writable non-configurable property".to_string(),
                    });
            }
            object::DescriptorKind::Accessor { getter: None, .. }
                if !desc.configurable() && !trap_result.is_undefined() =>
            {
                return Err(VmError::TypeError {
                    message:
                        "Proxy get trap returned value for non-configurable accessor without getter"
                            .to_string(),
                });
            }
            _ => {}
        }
        Ok(())
    }

    /// `Temporal.<ClassName>.prototype` lookup. Resolves the named
    /// Temporal class on `globalThis.Temporal` and returns its
    /// `prototype` object, or `None` if either step fails (the
    /// class isn't installed, or its constructor lacks the
    /// data-property prototype slot).
    pub(crate) fn temporal_prototype_object(
        &mut self,
        kind: crate::temporal::TemporalKind,
    ) -> Option<JsObject> {
        let temporal_ns =
            object::get(self.global_this, &self.gc_heap, "Temporal").and_then(|v| v.as_object())?;
        let class_value = object::get(temporal_ns, &self.gc_heap, kind.class_name())?;
        if let Some(ctor_obj) = class_value.as_object() {
            return object::get(ctor_obj, &self.gc_heap, "prototype").and_then(|v| v.as_object());
        }
        if let Some(ctor) = class_value.as_native_function() {
            let descriptor = ctor
                .own_property_descriptor(&mut self.gc_heap, "prototype")
                .ok()
                .flatten()?;
            return descriptor_value(&descriptor).as_object();
        }
        None
    }

    pub(crate) fn constructor_prototype_value(
        &mut self,
        constructor_name: &str,
    ) -> Result<Value, VmError> {
        // Fast path: typed slot for well-known intrinsics. Avoids the
        // global → ctor → prototype double-lookup that fires on every
        // `OrdinaryCreateFromConstructor` style allocation.
        let cached = match constructor_name {
            "Object" => self.realm_intrinsics.object_prototype,
            "Function" => self.realm_intrinsics.function_prototype,
            "Array" => self.realm_intrinsics.array_prototype,
            "Promise" => self.realm_intrinsics.promise_prototype,
            _ => None,
        };
        if let Some(proto) = cached {
            return Ok(Value::object(proto));
        }
        let Some(v) = object::get(self.global_this, &self.gc_heap, constructor_name) else {
            return Err(VmError::InvalidOperand);
        };
        if let Some(constructor) = v.as_object() {
            return Ok(
                object::get(constructor, &self.gc_heap, "prototype").unwrap_or(Value::null())
            );
        }
        if let Some(ctor) = v.as_native_function() {
            return match ctor.own_property_descriptor(&mut self.gc_heap, "prototype") {
                Ok(Some(descriptor)) => Ok(descriptor_value(&descriptor)),
                _ => Ok(Value::null()),
            };
        }
        if let Some(class) = v.as_class_constructor() {
            return Ok(Value::object(class.prototype(&self.gc_heap)));
        }
        Err(VmError::InvalidOperand)
    }

    pub(crate) fn ordinary_get_own_property_descriptor_value_stack_rooted(
        &mut self,
        context: &ExecutionContext,
        stack: &SmallVec<[Frame; 8]>,
        target: Value,
        key: &VmPropertyKey,
        hops: usize,
    ) -> Result<Option<object::PropertyDescriptor>, VmError> {
        self.ordinary_get_own_property_descriptor_value_with_roots(
            context,
            target,
            key,
            hops,
            DescriptorAllocationRoots::Stack(stack),
        )
    }

    pub(crate) fn ordinary_get_own_property_descriptor_value_runtime_rooted(
        &mut self,
        context: &ExecutionContext,
        target: Value,
        key: &VmPropertyKey,
        hops: usize,
        value_roots: &[&Value],
        slice_roots: &[&[Value]],
    ) -> Result<Option<object::PropertyDescriptor>, VmError> {
        self.ordinary_get_own_property_descriptor_value_with_roots(
            context,
            target,
            key,
            hops,
            DescriptorAllocationRoots::Runtime {
                value_roots,
                slice_roots,
            },
        )
    }

    fn ordinary_get_own_property_descriptor_value_with_roots(
        &mut self,
        context: &ExecutionContext,
        target: Value,
        key: &VmPropertyKey,
        hops: usize,
        allocation_roots: DescriptorAllocationRoots<'_>,
    ) -> Result<Option<object::PropertyDescriptor>, VmError> {
        if hops >= object::PROTO_CHAIN_HARD_CAP {
            return Ok(None);
        }
        self.ensure_deferred_namespace_ready(
            context,
            &target,
            !Self::deferred_key_is_symbol_like(key),
        )?;
        // §10.4.6.5 [[GetOwnProperty]] — namespace string keys report
        // `{writable:true, enumerable:true, configurable:false}` data
        // descriptors resolved through the environment; symbol keys
        // fall through to the namespace's own properties.
        if let Some(obj) = target.as_object()
            && object::module_namespace_env(obj, &self.gc_heap).is_some()
            && let Some(name) = key.string_name()
        {
            return match self.module_namespace_get_binding(obj, name) {
                // §10.4.6.5 step 7 — an uninitialized binding's
                // descriptor query is a ReferenceError (TDZ).
                Some(v) if v.is_hole() => Err(VmError::ThisUninitialized {
                    message: format!("Cannot access '{name}' before initialization"),
                }),
                Some(v) => Ok(Some(object::PropertyDescriptor::data(v, true, true, false))),
                None => Ok(None),
            };
        }
        if let Some(proxy) = target.as_proxy() {
            let key_value = self.vm_property_key_to_value(key)?;
            let trap_args: SmallVec<[Value; 8]> =
                smallvec::smallvec![proxy.target(&self.gc_heap), key_value];
            return match self.invoke_proxy_trap(
                context,
                &proxy,
                "getOwnPropertyDescriptor",
                trap_args,
            )? {
                Some(v) if v.is_nullish() => {
                    let target_desc = self.ordinary_get_own_property_descriptor_value_with_roots(
                        context,
                        proxy.target(&self.gc_heap),
                        key,
                        hops + 1,
                        allocation_roots,
                    )?;
                    self.validate_proxy_get_own_property_descriptor(
                        &proxy.target(&self.gc_heap),
                        target_desc.as_ref(),
                        None,
                    )?;
                    Ok(None)
                }
                Some(v) if v.is_object() || v.is_proxy() => {
                    // §10.5.5 step 9-ish ToPropertyDescriptor through
                    // ordinary [[Get]]s so a Proxy descriptor object
                    // dispatches its own traps.
                    let partial = self.evaluate_to_property_descriptor(context, &v)?;
                    let desc = partial.complete_for_new_property();
                    let target_desc = self.ordinary_get_own_property_descriptor_value_with_roots(
                        context,
                        proxy.target(&self.gc_heap),
                        key,
                        hops + 1,
                        allocation_roots,
                    )?;
                    self.validate_proxy_get_own_property_descriptor(
                        &proxy.target(&self.gc_heap),
                        target_desc.as_ref(),
                        Some(&desc),
                    )?;
                    Ok(Some(desc))
                }
                Some(_) => Err(VmError::TypeError {
                    message: "Proxy getOwnPropertyDescriptor trap returned non-object descriptor"
                        .to_string(),
                }),
                None => self.ordinary_get_own_property_descriptor_value_with_roots(
                    context,
                    proxy.target(&self.gc_heap),
                    key,
                    hops + 1,
                    allocation_roots,
                ),
            };
        }
        if let Some(obj) = target.as_object() {
            if let Some(desc) = self.string_object_exotic_descriptor(obj, key)? {
                return Ok(Some(desc));
            }
            return Ok(if let Some(key) = key.string_name() {
                object::get_own_descriptor(obj, &self.gc_heap, key)
            } else if let VmPropertyKey::Symbol(sym) = key {
                object::get_own_symbol_descriptor(obj, &self.gc_heap, *sym)
            } else {
                None
            });
        }
        if let Some(value) = target.as_string(&self.gc_heap) {
            return string::exotic::descriptor_for_key(value, key, &mut self.gc_heap);
        }
        if let Some(arr) = target.as_array() {
            // §10.4.2 — own symbol-keyed properties live in a
            // dedicated side table; surface their data
            // descriptor before the string-keyed paths so
            // `Object.getOwnPropertyDescriptor(arr, sym)` and
            // `hasOwnProperty(sym)` observe the spec shape.
            if let VmPropertyKey::Symbol(sym) = key {
                if let Some((getter, setter)) = array::get_symbol_accessor(arr, &self.gc_heap, *sym)
                {
                    return Ok(Some(object::PropertyDescriptor::accessor(
                        getter, setter, true, true,
                    )));
                }
                if let Some(value) = array::get_symbol_property(arr, &self.gc_heap, *sym) {
                    return Ok(Some(object::PropertyDescriptor::data(
                        value, true, true, true,
                    )));
                }
                return Ok(None);
            }
            let Some(key) = key.string_name() else {
                return Ok(None);
            };
            if key == "length" {
                let flags = array::length_flags(arr, &self.gc_heap);
                return Ok(Some(object::PropertyDescriptor::data(
                    Value::number_f64(array::len(arr, &self.gc_heap) as f64),
                    flags.writable(),
                    flags.enumerable(),
                    flags.configurable(),
                )));
            }
            // §10.4.2 — own accessor installed via
            // `Object.defineProperty` lives in the per-array
            // accessor side-table. Consult it before the
            // dense / named slots so reflective probes
            // (`Object.getOwnPropertyDescriptor(arr, "p")`) see
            // the user-installed getter / setter.
            if let Some((getter, setter)) = array::get_accessor(arr, &self.gc_heap, key) {
                let flags = array::get_property_flags(arr, &self.gc_heap, key)
                    .unwrap_or_else(|| object::PropertyFlags::new(false, true, true));
                return Ok(Some(object::PropertyDescriptor {
                    kind: object::DescriptorKind::Accessor { getter, setter },
                    flags,
                }));
            }
            if let Some(idx) = object::array_index_property_name(key) {
                let idx = idx as usize;
                if array::has_own_element(arr, &self.gc_heap, idx) {
                    let flags = array::get_property_flags(arr, &self.gc_heap, key)
                        .unwrap_or_else(object::PropertyFlags::data_default);
                    return Ok(Some(object::PropertyDescriptor {
                        kind: object::DescriptorKind::Data {
                            value: array::get(arr, &self.gc_heap, idx),
                        },
                        flags,
                    }));
                }
                return Ok(None);
            }
            // §10.4.2 — named own properties (`arr.foo = 1`)
            // live in the per-array `named_properties` side
            // table.
            if let Some(value) = self.gc_heap.read_payload(arr, |body| {
                body.named_properties
                    .as_ref()
                    .and_then(|m| m.get(key).cloned())
            }) {
                let flags = array::get_property_flags(arr, &self.gc_heap, key)
                    .unwrap_or_else(object::PropertyFlags::data_default);
                return Ok(Some(object::PropertyDescriptor {
                    kind: object::DescriptorKind::Data { value },
                    flags,
                }));
            }
            return Ok(None);
        }
        // §10.4.5.1 TypedArray [[GetOwnProperty]] — canonical numeric
        // keys resolve to an element data descriptor (or None when
        // invalid: out-of-bounds / fractional / -0 / detached);
        // everything else reads the expando bag.
        if let Some(t) = target.as_typed_array(&self.gc_heap) {
            match key {
                VmPropertyKey::Symbol(sym) => {
                    return Ok(t.expando(&self.gc_heap).and_then(|bag| {
                        object::get_own_symbol_descriptor(bag, &self.gc_heap, *sym)
                    }));
                }
                _ => {
                    let name = key.string_name().expect("non-symbol key");
                    if let Some(n) = crate::property_dispatch::canonical_numeric_index_string(name)
                    {
                        return Ok(
                            match crate::property_dispatch::typed_array_valid_index(
                                &t,
                                &self.gc_heap,
                                n,
                            ) {
                                Some(idx) => Some(object::PropertyDescriptor::data(
                                    t.get(&mut self.gc_heap, idx).map_err(crate::oom_to_vm)?,
                                    true,
                                    true,
                                    true,
                                )),
                                None => None,
                            },
                        );
                    }
                    return Ok(t
                        .expando(&self.gc_heap)
                        .and_then(|bag| object::get_own_descriptor(bag, &self.gc_heap, name)));
                }
            }
        }
        if let Some(re) = target.as_regexp() {
            if key.string_name().is_some_and(|key| key == "lastIndex") {
                return Ok(Some(object::PropertyDescriptor::data(
                    re.last_index_value(&self.gc_heap),
                    re.last_index_writable(&self.gc_heap),
                    false,
                    false,
                )));
            }
            if let Some(bag) = re.expando(&self.gc_heap) {
                if let Some(key) = key.string_name() {
                    if let Some(desc) = object::get_own_descriptor(bag, &self.gc_heap, key) {
                        return Ok(Some(desc));
                    }
                } else if let VmPropertyKey::Symbol(sym) = key
                    && let Some(desc) = object::get_own_symbol_descriptor(bag, &self.gc_heap, *sym)
                {
                    return Ok(Some(desc));
                }
            }
            return Ok(None);
        }
        if let Some(dv) = target.as_data_view() {
            // §25.3 — ordinary own properties live in the lazy expando.
            if let Some(bag) = dv.expando(&self.gc_heap) {
                if let Some(key) = key.string_name() {
                    if let Some(desc) = object::get_own_descriptor(bag, &self.gc_heap, key) {
                        return Ok(Some(desc));
                    }
                } else if let VmPropertyKey::Symbol(sym) = key
                    && let Some(desc) = object::get_own_symbol_descriptor(bag, &self.gc_heap, *sym)
                {
                    return Ok(Some(desc));
                }
            }
            return Ok(None);
        }
        let function_id = target.as_function().or_else(|| {
            target
                .as_closure(&self.gc_heap)
                .map(|c| c.cached_function_id)
        });
        if let Some(function_id) = function_id {
            if let VmPropertyKey::Symbol(sym) = key {
                let Some(bag) = self.function_user_props.get(&function_id).copied() else {
                    return Ok(None);
                };
                return Ok(object::get_own_symbol_descriptor(bag, &self.gc_heap, *sym));
            }
            let key = key
                .string_name()
                .expect("non-symbol key has string spelling");
            if key == "prototype" {
                let _ = match allocation_roots {
                    DescriptorAllocationRoots::Runtime {
                        value_roots,
                        slice_roots,
                    } => self.function_property_get_runtime_rooted_with_receiver(
                        context,
                        function_id,
                        Some(target),
                        "prototype",
                        value_roots,
                        slice_roots,
                    )?,
                    DescriptorAllocationRoots::Stack(stack) => self
                        .function_property_get_stack_rooted_with_receiver(
                            context,
                            stack,
                            function_id,
                            Some(target),
                            "prototype",
                        )?,
                };
                let Some(bag) = self.function_user_props.get(&function_id).copied() else {
                    return Ok(None);
                };
                return Ok(object::get_own_descriptor(bag, &self.gc_heap, key));
            }
            return self.ordinary_function_own_property_descriptor(Some(context), function_id, key);
        }
        if let Some(bound) = target.as_bound_function() {
            let Some(key) = key.string_name() else {
                return Ok(None);
            };
            return function_metadata::bound_own_property_descriptor(
                &bound,
                &mut self.gc_heap,
                key,
            );
        }
        if let Some(native) = target.as_native_function() {
            return Ok(if let VmPropertyKey::Symbol(sym) = key {
                native.own_symbol_property_descriptor(&self.gc_heap, *sym)
            } else {
                let key = key
                    .string_name()
                    .expect("non-symbol key has string spelling");
                native.own_property_descriptor(&mut self.gc_heap, key)?
            });
        }
        if let Some(class) = target.as_class_constructor() {
            if let VmPropertyKey::Symbol(sym) = key {
                return Ok(object::get_own_symbol_descriptor(
                    class.statics(&self.gc_heap),
                    &self.gc_heap,
                    *sym,
                ));
            }
            let key = key
                .string_name()
                .expect("non-symbol key has string spelling");
            if let Some(desc) =
                object::get_own_descriptor(class.statics(&self.gc_heap), &self.gc_heap, key)
            {
                return Ok(Some(desc));
            }
            if key == "prototype" {
                return Ok(Some(object::PropertyDescriptor::data(
                    Value::object(class.prototype(&self.gc_heap)),
                    false,
                    false,
                    false,
                )));
            }
            let ctor = class.ctor(&self.gc_heap);
            if let Some(function_id) = ctor
                .as_function()
                .or_else(|| ctor.as_closure(&self.gc_heap).map(|c| c.cached_function_id))
            {
                return self.ordinary_function_own_property_descriptor(
                    Some(context),
                    function_id,
                    key,
                );
            }
            if let Some(native) = ctor.as_native_function() {
                return Ok(native.own_property_descriptor(&mut self.gc_heap, key)?);
            }
            if let Some(bound) = ctor.as_bound_function() {
                return function_metadata::bound_own_property_descriptor(
                    &bound,
                    &mut self.gc_heap,
                    key,
                );
            }
        }
        Ok(None)
    }

    fn proxy_get_prototype_invariant_target_proto(
        &mut self,
        context: &ExecutionContext,
        target: &Value,
    ) -> Result<Option<Value>, VmError> {
        let Some(obj) = target.as_object() else {
            return Ok(None);
        };
        if object::is_extensible(obj, &self.gc_heap) {
            return Ok(None);
        }
        Ok(Some(
            self.ordinary_get_prototype_value(context, *target, 0)?,
        ))
    }

    pub(crate) fn ordinary_get_prototype_value(
        &mut self,
        context: &ExecutionContext,
        value: Value,
        hops: usize,
    ) -> Result<Value, VmError> {
        if hops >= object::PROTO_CHAIN_HARD_CAP {
            return Ok(Value::null());
        }
        if let Some(proxy) = value.as_proxy() {
            let trap_args: SmallVec<[Value; 8]> = smallvec::smallvec![proxy.target(&self.gc_heap)];
            return match self.invoke_proxy_trap(context, &proxy, "getPrototypeOf", trap_args)? {
                Some(result) => {
                    if !Self::proxy_get_prototype_result_is_object_or_null(&result) {
                        return Err(VmError::TypeError {
                            message: "Proxy getPrototypeOf trap returned non-object".to_string(),
                        });
                    }
                    if let Some(target_proto) = self.proxy_get_prototype_invariant_target_proto(
                        context,
                        &proxy.target(&self.gc_heap),
                    )? && !abstract_ops::same_value(&result, &target_proto, &self.gc_heap)
                    {
                        return Err(VmError::TypeError {
                            message: "Proxy getPrototypeOf trap returned incompatible prototype"
                                .to_string(),
                        });
                    }
                    Ok(result)
                }
                None => self.ordinary_get_prototype_value(
                    context,
                    proxy.target(&self.gc_heap),
                    hops + 1,
                ),
            };
        }
        if let Some(intl) = value.as_intl(&self.gc_heap) {
            return Ok(self.intl_kind_prototype_value(intl.kind().class_name()));
        }
        if value.is_object_type() {
            return self.get_prototype_for_op(&value);
        }
        Err(VmError::TypeMismatch)
    }

    fn proxy_get_prototype_result_is_object_or_null(value: &Value) -> bool {
        // §10.5.1 step 6: `If handlerProto is not Object and not Null,
        // throw TypeError`. Spec `Object` includes callable / exotic
        // targets, so `is_object_type` is the correct predicate
        // (`is_object_like` only matches `TAG_PTR_OBJECT` and rejects
        // a Function returned by the `getPrototypeOf` trap).
        value.is_null() || value.is_object_type()
    }

    /// §10.5.3 / §10.1.3 — value-level `[[IsExtensible]]`.
    /// Proxies dispatch through the `isExtensible` trap and enforce
    /// the §10.5.3 invariant that the trap result must match the
    /// target's actual extensibility.
    pub(crate) fn is_extensible_value(
        &mut self,
        context: &ExecutionContext,
        value: &Value,
    ) -> Result<bool, VmError> {
        // Deferred namespaces report non-extensible (§28.3 [[IsExtensible]]
        // → false) even before population, when the backing object is
        // still internally extensible so export properties can be added.
        if let Some(obj) = value.as_object()
            && object::deferred_namespace_target(obj, &self.gc_heap).is_some()
        {
            return Ok(false);
        }
        if let Some(proxy) = value.as_proxy() {
            if proxy.is_revoked(&self.gc_heap) {
                return Err(VmError::TypeError {
                    message: "Cannot perform 'isExtensible' on a proxy that has been revoked"
                        .to_string(),
                });
            }
            let trap_args: SmallVec<[Value; 8]> = smallvec::smallvec![proxy.target(&self.gc_heap)];
            return match self.invoke_proxy_trap(context, &proxy, "isExtensible", trap_args)? {
                Some(result) => {
                    let trap = result.to_boolean(&self.gc_heap);
                    let target_ext =
                        self.is_extensible_value(context, &proxy.target(&self.gc_heap))?;
                    if trap != target_ext {
                        return Err(VmError::TypeError {
                            message:
                                "Proxy isExtensible trap returned value inconsistent with target"
                                    .to_string(),
                        });
                    }
                    Ok(trap)
                }
                None => self.is_extensible_value(context, &proxy.target(&self.gc_heap)),
            };
        }
        if let Some(obj) = value.as_object() {
            return Ok(object::is_extensible(obj, &self.gc_heap));
        }
        if let Some(t) = value.as_typed_array(&self.gc_heap) {
            return Ok(t
                .expando(&self.gc_heap)
                .is_none_or(|bag| object::is_extensible(bag, &self.gc_heap)));
        }
        if let Some(arr) = value.as_array() {
            return Ok(array::is_extensible(arr, &self.gc_heap));
        }
        if let Some(native) = value.as_native_function() {
            return Ok(native.is_extensible(&self.gc_heap));
        }
        let fid = value.as_function().or_else(|| {
            value
                .as_closure(&self.gc_heap)
                .map(|c| c.cached_function_id)
        });
        if let Some(function_id) = fid {
            return Ok(self.ordinary_function_is_extensible(function_id));
        }
        if let Some(regexp) = value.as_regexp() {
            return Ok(regexp.is_extensible(&self.gc_heap));
        }
        // Per §10.1.3 every other ordinary heap value is extensible
        // by default.
        Ok(true)
    }

    /// §10.5.6 / §10.1.6 — value-level `[[DefineOwnProperty]]`.
    /// Proxies dispatch through the `defineProperty` trap and enforce
    /// the §10.5.6 step 14–18 invariants using the field-presence
    /// information carried by [`object::PartialPropertyDescriptor`].
    /// §10.1.9.2 OrdinarySetWithOwnDescriptor steps 2-3 — the
    /// receiver phase: re-resolve the property on the RECEIVER via
    /// [[GetOwnProperty]] / [[DefineOwnProperty]] (never its
    /// [[Set]]), so exotic receivers (TypedArrays, Proxies,
    /// non-extensible objects) apply their own define semantics.
    pub(crate) fn ordinary_set_on_receiver(
        &mut self,
        context: &ExecutionContext,
        key: &VmPropertyKey,
        value: Value,
        receiver: &Value,
    ) -> Result<bool, VmError> {
        if !crate::reflect::is_type_object_value(receiver) {
            return Ok(false);
        }
        let existing = self.ordinary_get_own_property_descriptor_value_runtime_rooted(
            context,
            *receiver,
            key,
            0,
            &[receiver, &value],
            &[],
        )?;
        match existing {
            Some(desc) => match desc.kind {
                object::DescriptorKind::Accessor { .. } => Ok(false),
                object::DescriptorKind::Data { .. } => {
                    if !desc.flags.writable() {
                        return Ok(false);
                    }
                    let partial = object::PartialPropertyDescriptor {
                        value: Some(value),
                        ..Default::default()
                    };
                    self.define_own_property_value(context, receiver, key, partial)
                }
            },
            None => {
                let descriptor = object::PartialPropertyDescriptor {
                    value: Some(value),
                    writable: Some(true),
                    enumerable: Some(true),
                    configurable: Some(true),
                    ..Default::default()
                };
                self.define_own_property_value(context, receiver, key, descriptor)
            }
        }
    }

    /// §7.3.31 / §7.3.32 private-element resolution. Walks the
    /// receiver's own properties first (instance fields live there),
    /// then the prototype chain (methods and accessors are installed
    /// on the class prototype / statics object), looking for the
    /// class-evaluation private-name symbol. Returns the holder and
    /// its descriptor, or `None` when the brand check fails.
    /// Scan a proxy's [[PrivateElements]] bag for `sym`.
    pub(crate) fn proxy_private_find(
        &self,
        proxy: &crate::proxy::JsProxy,
        sym: crate::symbol::JsSymbol,
    ) -> Option<Value> {
        self.gc_heap.read_payload(proxy.handle(), |body| {
            body.private_elements.as_ref().and_then(|entries| {
                entries
                    .iter()
                    .find(|(s, _)| s.handle() == sym.handle())
                    .map(|(_, v)| *v)
            })
        })
    }

    /// Insert or overwrite `sym` in a proxy's [[PrivateElements]].
    pub(crate) fn proxy_private_upsert(
        &mut self,
        proxy: &crate::proxy::JsProxy,
        sym: crate::symbol::JsSymbol,
        value: Value,
    ) {
        self.gc_heap.with_payload(proxy.handle(), |body| {
            let entries = body.private_elements.get_or_insert_with(Vec::new);
            match entries.iter_mut().find(|(s, _)| s.handle() == sym.handle()) {
                Some(slot) => slot.1 = value,
                None => entries.push((sym, value)),
            }
        });
    }

    pub(crate) fn private_element_lookup(
        &mut self,
        context: &ExecutionContext,
        receiver: &Value,
        sym: crate::symbol::JsSymbol,
    ) -> Result<Option<(Value, object::PropertyDescriptor)>, VmError> {
        // §6.2.12 — a Proxy carries its own [[PrivateElements]];
        // private names never consult traps or the target/prototype
        // chain.
        if sym.is_private_name()
            && let Some(p) = receiver.as_proxy()
        {
            if let Some(value) = self.proxy_private_find(&p, sym) {
                return Ok(Some((
                    *receiver,
                    object::PropertyDescriptor {
                        kind: object::DescriptorKind::Data { value },
                        flags: object::PropertyFlags::new(true, false, false),
                    },
                )));
            }
            // Miss in the bag: brand entries carry the class
            // prototype object as their value, so private METHODS /
            // accessors of a branded receiver resolve through it
            // even though the proxy's chain never reaches the
            // holder. The holder reported is the prototype, which
            // keeps the PrivateSet not-writable check intact.
            let brand_protos: Vec<Value> = self.gc_heap.read_payload(p.handle(), |body| {
                body.private_elements
                    .as_ref()
                    .map(|entries| {
                        entries
                            .iter()
                            .filter(|(_, v)| v.is_object())
                            .map(|(_, v)| *v)
                            .collect()
                    })
                    .unwrap_or_default()
            });
            let key = VmPropertyKey::Symbol(sym);
            for proto in brand_protos {
                if let Some(desc) = self.ordinary_get_own_property_descriptor_value_runtime_rooted(
                    context,
                    proto,
                    &key,
                    0,
                    &[&proto, receiver],
                    &[],
                )? {
                    return Ok(Some((proto, desc)));
                }
            }
            return Ok(None);
        }
        let key = VmPropertyKey::Symbol(sym);
        let mut current = *receiver;
        let mut hops = 0;
        loop {
            if let Some(desc) = self.ordinary_get_own_property_descriptor_value_runtime_rooted(
                context,
                current,
                &key,
                0,
                &[&current, receiver],
                &[],
            )? {
                return Ok(Some((current, desc)));
            }
            if hops >= object::PROTO_CHAIN_HARD_CAP {
                break;
            }
            // §7.3.30 — private elements never inherit across a class
            // boundary: a subclass constructor does not see the parent
            // constructor's static privates (the ctor_proto identity
            // chain would otherwise leak them).
            if current.is_class_constructor() {
                break;
            }
            // Proxies participate via their getPrototypeOf trap —
            // the context-aware walker handles them (the plain
            // get_prototype_for_op rejects proxy values).
            let proto = self.ordinary_get_prototype_value(context, current, hops)?;
            if !proto.is_object() && !proto.is_object_type() {
                break;
            }
            current = proto;
            hops += 1;
        }
        // Constructor-return override: a branded plain object whose
        // [[Prototype]] chain misses the method holder still resolves
        // private methods through its brand entries, whose stored
        // value is the class prototype object (see `__privproto_*`).
        if sym.is_private_name()
            && let Some(obj) = receiver.as_object()
        {
            let brand_protos: Vec<Value> =
                crate::object::with_properties(obj, &self.gc_heap, |props| {
                    props
                        .symbol_keys()
                        .filter(|k| k.is_private_name())
                        .filter_map(|k| crate::object::get_symbol(obj, &self.gc_heap, k))
                        .filter(|v| v.is_object())
                        .collect()
                });
            for proto in brand_protos {
                if proto == *receiver {
                    continue;
                }
                if let Some(desc) = self.ordinary_get_own_property_descriptor_value_runtime_rooted(
                    context,
                    proto,
                    &key,
                    0,
                    &[&proto, receiver],
                    &[],
                )? {
                    return Ok(Some((proto, desc)));
                }
            }
        }
        Ok(None)
    }

    pub(crate) fn define_own_property_value(
        &mut self,
        context: &ExecutionContext,
        target: &Value,
        key: &VmPropertyKey,
        descriptor: object::PartialPropertyDescriptor,
    ) -> Result<bool, VmError> {
        // §6.2.12 / §7.3.28 — private names: a Proxy receiver keeps
        // them in its own [[PrivateElements]] bag (no traps), and an
        // ordinary add to a non-extensible object is a TypeError.
        if let VmPropertyKey::Symbol(sym) = key
            && sym.is_private_name()
        {
            if let Some(p) = target.as_proxy() {
                let value = descriptor.value.unwrap_or(Value::undefined());
                self.proxy_private_upsert(&p, *sym, value);
                return Ok(true);
            }
            let own_bag = target.as_object().or_else(|| {
                target
                    .as_class_constructor()
                    .map(|c| c.statics(&self.gc_heap))
            });
            if let Some(obj) = own_bag
                && !crate::object::is_extensible(obj, &self.gc_heap)
                && crate::object::get_symbol(obj, &self.gc_heap, *sym).is_none()
            {
                return Err(VmError::TypeError {
                    message: "Cannot define private field on a non-extensible object".to_string(),
                });
            }
        }
        // Array index-store protector: an accessor landing on an
        // array-index key anywhere (most relevantly
        // %Array.prototype% / %Object.prototype%) forces array
        // element writes onto the OrdinarySet prototype-walk slow
        // path. Conservative — never reset.
        if (descriptor.get.is_some() || descriptor.set.is_some())
            && key
                .string_name()
                .is_some_and(|name| crate::object::array_index_property_name(name).is_some())
        {
            self.array_index_accessor_protector = true;
        }
        self.ensure_deferred_namespace_ready(
            context,
            target,
            !Self::deferred_key_is_symbol_like(key),
        )?;
        // §10.4.6.6 [[DefineOwnProperty]] — a namespace export is a fixed
        // `{writable:true, enumerable:true, configurable:false}` data
        // property; a define on a string export succeeds only if it
        // requests no change to those attributes or the value. Adding a
        // new name fails. Symbol keys fall through to the ordinary
        // (non-extensible) define on the namespace's own properties.
        if let Some(obj) = target.as_object()
            && let Some(env) = object::module_namespace_env(obj, &self.gc_heap)
            && let Some(name) = key.string_name()
        {
            let Some(current) = object::get(env, &self.gc_heap, name) else {
                return Ok(false);
            };
            let value_ok = match descriptor.value {
                Some(v) => abstract_ops::same_value(&v, &current, &self.gc_heap),
                None => true,
            };
            let ok = descriptor.get.is_none()
                && descriptor.set.is_none()
                && descriptor.configurable != Some(true)
                && descriptor.enumerable != Some(false)
                && descriptor.writable != Some(false)
                && value_ok;
            return Ok(ok);
        }
        if let Some(proxy) = target.as_proxy() {
            if proxy.is_revoked(&self.gc_heap) {
                return Err(VmError::TypeError {
                    message: "Cannot perform 'defineProperty' on a proxy that has been revoked"
                        .to_string(),
                });
            }
            let key_value = self.vm_property_key_to_value(key)?;
            let target_value = proxy.target(&self.gc_heap);
            let descriptor_object =
                self.partial_descriptor_to_object(&descriptor, &[&key_value, &target_value])?;
            let trap_args: SmallVec<[Value; 8]> =
                smallvec::smallvec![target_value, key_value, Value::object(descriptor_object),];
            return match self.invoke_proxy_trap(context, &proxy, "defineProperty", trap_args)? {
                Some(result) => {
                    let ok = result.to_boolean(&self.gc_heap);
                    if !ok {
                        return Ok(false);
                    }
                    let descriptor_roots = partial_descriptor_value_roots(&descriptor);
                    let mut value_roots = Vec::with_capacity(descriptor_roots.len() + 1);
                    value_roots.push(&target_value);
                    value_roots.extend(descriptor_roots.iter());
                    let target_desc = self
                        .ordinary_get_own_property_descriptor_value_runtime_rooted(
                            context,
                            target_value,
                            key,
                            0,
                            value_roots.as_slice(),
                            &[],
                        )?;
                    let extensible = self.is_extensible_value(context, &target_value)?;
                    let setting_config_false = matches!(descriptor.configurable, Some(false))
                        || (descriptor.configurable.is_none() && !descriptor.is_generic() && {
                            // Defaults when adding (current undefined):
                            // configurable=false. The non-generic clause
                            // only matters when target_desc is None.
                            target_desc.is_none()
                        });
                    match target_desc.as_ref() {
                        None => {
                            if !extensible {
                                return Err(VmError::TypeError {
                                        message:
                                            "Proxy defineProperty trap added a property on a non-extensible target"
                                                .to_string(),
                                    });
                            }
                            if setting_config_false {
                                return Err(VmError::TypeError {
                                        message:
                                            "Proxy defineProperty trap added a non-configurable property absent on the target"
                                                .to_string(),
                                    });
                            }
                        }
                        Some(target_desc) => {
                            let target_configurable = target_desc.configurable();
                            if !target_configurable && matches!(descriptor.configurable, Some(true))
                            {
                                return Err(VmError::TypeError {
                                        message:
                                            "Proxy defineProperty trap relaxed a non-configurable target descriptor"
                                                .to_string(),
                                    });
                            }
                            if target_configurable && matches!(descriptor.configurable, Some(false))
                            {
                                return Err(VmError::TypeError {
                                        message:
                                            "Proxy defineProperty trap demoted a configurable target descriptor"
                                                .to_string(),
                                    });
                            }
                            if !target_configurable
                                && target_desc.is_data()
                                && target_desc.writable()
                                && matches!(descriptor.writable, Some(false))
                            {
                                return Err(VmError::TypeError {
                                        message:
                                            "Proxy defineProperty trap narrowed writable on a non-configurable data target"
                                                .to_string(),
                                    });
                            }
                            if !is_compatible_partial_descriptor(
                                target_desc,
                                &descriptor,
                                &self.gc_heap,
                            ) {
                                return Err(VmError::TypeError {
                                    message:
                                        "Proxy defineProperty trap returned incompatible descriptor"
                                            .to_string(),
                                });
                            }
                        }
                    }
                    Ok(true)
                }
                None => {
                    // Trap missing — fall through to target.
                    self.define_own_property_value(
                        context,
                        &proxy.target(&self.gc_heap),
                        key,
                        descriptor,
                    )
                }
            };
        }
        if let Some(obj) = target.as_object() {
            if object::deferred_namespace_target(obj, &self.gc_heap).is_some()
                && !object::deferred_namespace_is_populated(obj, &self.gc_heap)
                && Self::deferred_key_is_symbol_like(key)
                && matches!(
                    self.lookup_own_vm_property_key(obj, key),
                    object::PropertyLookup::Absent
                )
            {
                return Ok(false);
            }
            return Ok(if let VmPropertyKey::Symbol(sym) = key {
                object::define_own_symbol_property_partial(obj, &mut self.gc_heap, *sym, descriptor)
            } else {
                if let Some(current) = self.string_object_exotic_descriptor(obj, key)? {
                    return Ok(is_compatible_partial_descriptor(
                        &current,
                        &descriptor,
                        &self.gc_heap,
                    ));
                }
                let k = key
                    .string_name()
                    .expect("non-symbol key has string spelling");
                self.define_own_property_partial(obj, k, descriptor)?
            });
        }
        if let Some(native) = target.as_native_function() {
            return Ok(if let VmPropertyKey::Symbol(sym) = key {
                native.define_own_symbol_property(&mut self.gc_heap, *sym, descriptor)
            } else {
                let k = key
                    .string_name()
                    .expect("non-symbol key has string spelling");
                native.define_own_property(
                    &mut self.gc_heap,
                    k,
                    descriptor.complete_for_new_property(),
                )
            });
        }
        if let Some(class) = target.as_class_constructor() {
            let statics = class.statics(&self.gc_heap);
            return Ok(if let VmPropertyKey::Symbol(sym) = key {
                object::define_own_symbol_property_partial(
                    statics,
                    &mut self.gc_heap,
                    *sym,
                    descriptor,
                )
            } else {
                let k = key
                    .string_name()
                    .expect("non-symbol key has string spelling");
                self.define_own_property_partial(statics, k, descriptor)?
            });
        }
        let fid = target.as_function().or_else(|| {
            target
                .as_closure(&self.gc_heap)
                .map(|c| c.cached_function_id)
        });
        if let Some(function_id) = fid {
            if let VmPropertyKey::Symbol(sym) = key {
                let bag = self.function_user_bag_runtime_rooted(function_id, &[], &[])?;
                return Ok(object::define_own_symbol_property_partial(
                    bag,
                    &mut self.gc_heap,
                    *sym,
                    descriptor,
                ));
            }
            let Some(k) = key.string_name() else {
                return Ok(false);
            };
            let completed = match self.ordinary_function_own_property_descriptor(
                Some(context),
                function_id,
                k,
            )? {
                Some(current) => complete_partial_descriptor_against_current(&current, &descriptor),
                None => descriptor.complete_for_new_property(),
            };
            return self.ordinary_function_define_own_property(
                Some(context),
                function_id,
                k,
                None,
                completed,
            );
        }
        if let Some(regexp) = target.as_regexp() {
            if key.string_name().is_some_and(|key| key == "lastIndex") {
                let current = object::PropertyDescriptor::data(
                    regexp.last_index_value(&self.gc_heap),
                    regexp.last_index_writable(&self.gc_heap),
                    false,
                    false,
                );
                let completed = complete_partial_descriptor_against_current(&current, &descriptor);
                let Some(updated) =
                    object::validate_descriptor_update(&current, &completed, &self.gc_heap)
                else {
                    return Ok(false);
                };
                let object::DescriptorKind::Data { value } = &updated.kind else {
                    return Ok(false);
                };
                regexp.set_last_index_value(&mut self.gc_heap, *value);
                regexp.set_last_index_writable(&mut self.gc_heap, updated.writable());
                return Ok(true);
            }
            let bag =
                crate::property_dispatch::regexp_ensure_expando_pub(&mut self.gc_heap, &regexp)?;
            return Ok(if let VmPropertyKey::Symbol(sym) = key {
                object::define_own_symbol_property_partial(bag, &mut self.gc_heap, *sym, descriptor)
            } else {
                let k = key
                    .string_name()
                    .expect("non-symbol key has string spelling");
                self.define_own_property_partial(bag, k, descriptor)?
            });
        }
        if let Some(promise) = target.as_promise() {
            // Promise instances are ordinary objects whose user-defined
            // properties (e.g. a shadowing `then` accessor the combinator
            // resolve path observes) live on a lazily-allocated expando.
            let bag =
                crate::property_dispatch::promise_ensure_expando_pub(&mut self.gc_heap, &promise)?;
            return Ok(if let VmPropertyKey::Symbol(sym) = key {
                object::define_own_symbol_property_partial(bag, &mut self.gc_heap, *sym, descriptor)
            } else {
                let k = key
                    .string_name()
                    .expect("non-symbol key has string spelling");
                self.define_own_property_partial(bag, k, descriptor)?
            });
        }
        if let Some(dv) = target.as_data_view() {
            // §25.3 — a `DataView` is an ordinary extensible object;
            // `Object.defineProperty(dv, …)` installs onto the expando.
            let bag =
                crate::property_dispatch::data_view_ensure_expando_pub(&mut self.gc_heap, &dv)?;
            return Ok(if let VmPropertyKey::Symbol(sym) = key {
                object::define_own_symbol_property_partial(bag, &mut self.gc_heap, *sym, descriptor)
            } else {
                let k = key
                    .string_name()
                    .expect("non-symbol key has string spelling");
                self.define_own_property_partial(bag, k, descriptor)?
            });
        }
        if let Some(arr) = target.as_array() {
            if let VmPropertyKey::Symbol(sym) = key {
                // §10.4.2.1 — a symbol accessor descriptor installs a
                // getter/setter pair; a data descriptor stores the value.
                if descriptor.is_accessor() {
                    array::set_symbol_accessor(
                        arr,
                        &mut self.gc_heap,
                        *sym,
                        descriptor.get,
                        descriptor.set,
                    );
                } else {
                    let value = descriptor.value.unwrap_or(Value::undefined());
                    array::set_symbol_property(arr, &mut self.gc_heap, *sym, value);
                }
                return Ok(true);
            }
            let Some(k) = key.string_name() else {
                return Ok(false);
            };
            if k == "length" {
                // §10.4.2.4 ArraySetLength. Steps 3-5 coerce the candidate
                // length BEFORE any property validation: `newLen` runs
                // `ToUint32` (whose inner `ToNumber` is the first observable
                // coercion) and `numberLen` runs `ToNumber` again (the
                // second), so an object value's `valueOf` / `@@toPrimitive`
                // fires exactly twice and a non-integer / negative / overflow
                // length raises `RangeError` ahead of the configurable /
                // enumerable / writable checks.
                let new_len = if let Some(v) = descriptor.value {
                    let number_for_uint = crate::coerce::to_number_or_throw(self, context, &v)?;
                    let new_len = crate::number::bitwise::to_uint32(number_for_uint);
                    let number_len = crate::coerce::to_number_or_throw(self, context, &v)?;
                    if (new_len as f64) != number_len.as_f64() {
                        return Err(VmError::RangeError {
                            message: "Invalid array length".to_string(),
                        });
                    }
                    Some(new_len as usize)
                } else {
                    None
                };
                // OrdinaryDefineOwnProperty validation against length's fixed
                // shape — a non-configurable, non-enumerable data property.
                if descriptor.is_accessor()
                    || matches!(descriptor.configurable, Some(true))
                    || matches!(descriptor.enumerable, Some(true))
                {
                    return Ok(false);
                }
                let old_len = array::len(arr, &self.gc_heap);
                let length_writable = array::length_writable(arr, &self.gc_heap);
                let want_writable_false = matches!(descriptor.writable, Some(false));
                let want_writable_true = matches!(descriptor.writable, Some(true));
                let Some(new_len) = new_len else {
                    // No [[Value]]: only a writable transition is possible.
                    if !length_writable {
                        return Ok(!want_writable_true);
                    }
                    if want_writable_false {
                        array::set_length_writable(arr, &mut self.gc_heap, false);
                    }
                    return Ok(true);
                };
                if new_len >= old_len {
                    // §10.4.2.4 step 9 — grow / no-op. A non-writable length
                    // forbids a value change or a writable→true promotion
                    // (the property is non-configurable).
                    if !length_writable {
                        return Ok(new_len == old_len && !want_writable_true);
                    }
                    array::set_length_checked(arr, &mut self.gc_heap, new_len)
                        .map_err(|_| VmError::TypeMismatch)?;
                    if want_writable_false {
                        array::set_length_writable(arr, &mut self.gc_heap, false);
                    }
                    return Ok(true);
                }
                // §10.4.2.4 step 10 — shrink requires a writable length.
                if !length_writable {
                    return Ok(false);
                }
                let delete_ok = array::set_length_checked(arr, &mut self.gc_heap, new_len)
                    .map_err(|_| VmError::TypeMismatch)?;
                if want_writable_false {
                    array::set_length_writable(arr, &mut self.gc_heap, false);
                }
                return Ok(delete_ok);
            }
            if let Some(idx) = object::array_index_property_name(k) {
                return self.define_array_index_property(arr, k, idx as usize, descriptor);
            }
            return self.define_array_named_property(arr, k, descriptor);
        }
        if let Some(t) = target.as_typed_array(&self.gc_heap) {
            // §10.4.5.3 Integer-Indexed exotic [[DefineOwnProperty]].
            // A canonical numeric index must be an in-bounds, writable,
            // enumerable, configurable data property; any other key is
            // an ordinary define on the typed array's expando bag.
            if let VmPropertyKey::Symbol(sym) = key {
                let bag = crate::property_dispatch::typed_array_ensure_expando_pub(
                    &mut self.gc_heap,
                    &t,
                )?;
                return Ok(object::define_own_symbol_property_partial(
                    bag,
                    &mut self.gc_heap,
                    *sym,
                    descriptor,
                ));
            }
            let Some(name) = key.string_name() else {
                return Ok(false);
            };
            if let Some(n) = crate::property_dispatch::canonical_numeric_index_string(name) {
                let Some(idx) =
                    crate::property_dispatch::typed_array_valid_index(&t, &self.gc_heap, n)
                else {
                    return Ok(false);
                };
                if descriptor.configurable == Some(false)
                    || descriptor.enumerable == Some(false)
                    || descriptor.writable == Some(false)
                    || descriptor.is_accessor()
                {
                    return Ok(false);
                }
                if let Some(value) = descriptor.value {
                    // §10.4.5.3 step f — SetTypedArrayElement converts the
                    // descriptor value with ToNumber / ToBigInt (firing
                    // its coercion and throwing for a Symbol / cross-type)
                    // before storing it.
                    let coerced = self.typed_array_coerce_element(context, t.kind(), value)?;
                    t.set(&mut self.gc_heap, idx, &coerced);
                }
                return Ok(true);
            }
            let bag =
                crate::property_dispatch::typed_array_ensure_expando_pub(&mut self.gc_heap, &t)?;
            return self.define_own_property_partial(bag, name, descriptor);
        }
        Ok(false)
    }

    /// §7.3.15 `SetIntegrityLevel(O, level)`.
    ///
    /// Runs through value-level internal methods so Proxy traps see
    /// `ownKeys`, `preventExtensions`, `getOwnPropertyDescriptor`, and
    /// `defineProperty` in the spec order.
    pub(crate) fn set_integrity_level_value(
        &mut self,
        context: &ExecutionContext,
        target: &Value,
        level: ObjectIntegrityLevel,
    ) -> Result<bool, VmError> {
        let keys = self.own_property_keys_value(context, target)?;
        if !self.prevent_extensions_value(context, target)? {
            return Ok(false);
        }
        for key_value in &keys {
            let key = property_key_value_to_vm_key(key_value, &self.gc_heap)?;
            let descriptor = match level {
                ObjectIntegrityLevel::Sealed => object::PartialPropertyDescriptor {
                    configurable: Some(false),
                    ..Default::default()
                },
                ObjectIntegrityLevel::Frozen => {
                    let current = self.ordinary_get_own_property_descriptor_value_runtime_rooted(
                        context,
                        *target,
                        &key,
                        0,
                        &[target],
                        &[],
                    )?;
                    let Some(current) = current else {
                        continue;
                    };
                    let mut desc = object::PartialPropertyDescriptor {
                        configurable: Some(false),
                        ..Default::default()
                    };
                    if current.is_data() {
                        desc.writable = Some(false);
                    }
                    desc
                }
            };
            if !self.define_own_property_value(context, target, &key, descriptor)? {
                return Ok(false);
            }
        }
        Ok(true)
    }

    /// §7.3.16 `TestIntegrityLevel(O, level)`.
    ///
    /// Uses internal methods for Proxy targets, preserving observable
    /// trap order and symbol keys from `[[OwnPropertyKeys]]`.
    pub(crate) fn test_integrity_level_value(
        &mut self,
        context: &ExecutionContext,
        target: &Value,
        level: ObjectIntegrityLevel,
    ) -> Result<bool, VmError> {
        if self.is_extensible_value(context, target)? {
            return Ok(false);
        }
        let keys = self.own_property_keys_value(context, target)?;
        for key_value in &keys {
            let key = property_key_value_to_vm_key(key_value, &self.gc_heap)?;
            let desc = self.ordinary_get_own_property_descriptor_value_runtime_rooted(
                context,
                *target,
                &key,
                0,
                &[target],
                &[],
            )?;
            let Some(desc) = desc else {
                continue;
            };
            if desc.configurable() {
                return Ok(false);
            }
            if matches!(level, ObjectIntegrityLevel::Frozen) && desc.is_data() && desc.writable() {
                return Ok(false);
            }
        }
        Ok(true)
    }

    fn define_array_named_property(
        &mut self,
        arr: array::JsArray,
        key: &str,
        descriptor: object::PartialPropertyDescriptor,
    ) -> Result<bool, VmError> {
        let current = if let Some((getter, setter)) = array::get_accessor(arr, &self.gc_heap, key) {
            let flags = array::get_property_flags(arr, &self.gc_heap, key)
                .unwrap_or_else(|| object::PropertyFlags::new(false, true, true));
            Some(object::PropertyDescriptor {
                kind: object::DescriptorKind::Accessor { getter, setter },
                flags,
            })
        } else if let Some(value) = array::get_named_property(arr, &self.gc_heap, key) {
            let flags = array::get_property_flags(arr, &self.gc_heap, key)
                .unwrap_or_else(object::PropertyFlags::data_default);
            Some(object::PropertyDescriptor {
                kind: object::DescriptorKind::Data { value },
                flags,
            })
        } else {
            None
        };

        if current.is_none() {
            if !array::is_extensible(arr, &self.gc_heap) {
                return Ok(false);
            }
            self.store_array_named_descriptor(arr, key, descriptor.complete_for_new_property())?;
            return Ok(true);
        }

        let current = current.expect("current descriptor is present");
        if !current.configurable() {
            if matches!(descriptor.configurable, Some(true)) {
                return Ok(false);
            }
            if let Some(enumerable) = descriptor.enumerable
                && enumerable != current.enumerable()
            {
                return Ok(false);
            }
        }

        if descriptor.is_generic() {
            let updated = match current.kind.clone() {
                object::DescriptorKind::Data { value } => object::PropertyDescriptor::data(
                    value,
                    current.writable(),
                    descriptor.enumerable.unwrap_or(current.enumerable()),
                    descriptor.configurable.unwrap_or(current.configurable()),
                ),
                object::DescriptorKind::Accessor { getter, setter } => {
                    object::PropertyDescriptor::accessor(
                        getter,
                        setter,
                        descriptor.enumerable.unwrap_or(current.enumerable()),
                        descriptor.configurable.unwrap_or(current.configurable()),
                    )
                }
            };
            self.store_array_named_descriptor(arr, key, updated)?;
            return Ok(true);
        }

        if current.is_data() != descriptor.is_data() {
            if !current.configurable() {
                return Ok(false);
            }
            let updated = if descriptor.is_data() {
                object::PropertyDescriptor::data(
                    descriptor.value.unwrap_or(Value::undefined()),
                    descriptor.writable.unwrap_or(false),
                    descriptor.enumerable.unwrap_or(current.enumerable()),
                    descriptor.configurable.unwrap_or(current.configurable()),
                )
            } else {
                object::PropertyDescriptor::accessor(
                    if descriptor.get.is_some() {
                        normalize_accessor_slot(descriptor.get)
                    } else {
                        None
                    },
                    if descriptor.set.is_some() {
                        normalize_accessor_slot(descriptor.set)
                    } else {
                        None
                    },
                    descriptor.enumerable.unwrap_or(current.enumerable()),
                    descriptor.configurable.unwrap_or(current.configurable()),
                )
            };
            self.store_array_named_descriptor(arr, key, updated)?;
            return Ok(true);
        }

        match current.kind.clone() {
            object::DescriptorKind::Data {
                value: current_value,
            } => {
                if !current.configurable() && !current.writable() {
                    if matches!(descriptor.writable, Some(true)) {
                        return Ok(false);
                    }
                    if let Some(value) = &descriptor.value
                        && !abstract_ops::same_value(value, &current_value, &self.gc_heap)
                    {
                        return Ok(false);
                    }
                }
                let updated = object::PropertyDescriptor::data(
                    descriptor.value.unwrap_or(current_value),
                    descriptor.writable.unwrap_or(current.writable()),
                    descriptor.enumerable.unwrap_or(current.enumerable()),
                    descriptor.configurable.unwrap_or(current.configurable()),
                );
                self.store_array_named_descriptor(arr, key, updated)?;
                Ok(true)
            }
            object::DescriptorKind::Accessor {
                getter: current_getter,
                setter: current_setter,
            } => {
                let getter = normalize_accessor_slot(descriptor.get);
                let setter = normalize_accessor_slot(descriptor.set);
                if !current.configurable()
                    && ((descriptor.get.is_some()
                        && !same_optional_value(&getter, &current_getter, &self.gc_heap))
                        || (descriptor.set.is_some()
                            && !same_optional_value(&setter, &current_setter, &self.gc_heap)))
                {
                    return Ok(false);
                }
                let updated = object::PropertyDescriptor::accessor(
                    if descriptor.get.is_some() {
                        getter
                    } else {
                        current_getter
                    },
                    if descriptor.set.is_some() {
                        setter
                    } else {
                        current_setter
                    },
                    descriptor.enumerable.unwrap_or(current.enumerable()),
                    descriptor.configurable.unwrap_or(current.configurable()),
                );
                self.store_array_named_descriptor(arr, key, updated)?;
                Ok(true)
            }
        }
    }

    fn define_array_index_property(
        &mut self,
        arr: array::JsArray,
        key: &str,
        idx: usize,
        descriptor: object::PartialPropertyDescriptor,
    ) -> Result<bool, VmError> {
        let current = if let Some((getter, setter)) = array::get_accessor(arr, &self.gc_heap, key) {
            let flags = array::get_property_flags(arr, &self.gc_heap, key)
                .unwrap_or_else(|| object::PropertyFlags::new(false, true, true));
            Some(object::PropertyDescriptor {
                kind: object::DescriptorKind::Accessor { getter, setter },
                flags,
            })
        } else if array::has_own_element(arr, &self.gc_heap, idx) {
            let flags = array::get_property_flags(arr, &self.gc_heap, key)
                .unwrap_or_else(object::PropertyFlags::data_default);
            Some(object::PropertyDescriptor {
                kind: object::DescriptorKind::Data {
                    value: array::get(arr, &self.gc_heap, idx),
                },
                flags,
            })
        } else {
            None
        };

        let old_len = array::len(arr, &self.gc_heap);
        if current.is_none() {
            if !array::is_extensible(arr, &self.gc_heap)
                || (idx >= old_len && !array::length_writable(arr, &self.gc_heap))
            {
                return Ok(false);
            }
            let completed = descriptor.complete_for_new_property();
            if idx >= old_len {
                array::set_length(arr, &mut self.gc_heap, idx + 1)
                    .map_err(|_| VmError::TypeMismatch)?;
            }
            self.store_array_index_descriptor(arr, key, idx, completed)?;
            return Ok(true);
        }

        let current = current.expect("current descriptor is present");
        if !current.configurable() {
            if matches!(descriptor.configurable, Some(true)) {
                return Ok(false);
            }
            if let Some(enumerable) = descriptor.enumerable
                && enumerable != current.enumerable()
            {
                return Ok(false);
            }
        }

        if descriptor.is_generic() {
            let updated = match current.kind.clone() {
                object::DescriptorKind::Data { value } => object::PropertyDescriptor::data(
                    value,
                    current.writable(),
                    descriptor.enumerable.unwrap_or(current.enumerable()),
                    descriptor.configurable.unwrap_or(current.configurable()),
                ),
                object::DescriptorKind::Accessor { getter, setter } => {
                    object::PropertyDescriptor::accessor(
                        getter,
                        setter,
                        descriptor.enumerable.unwrap_or(current.enumerable()),
                        descriptor.configurable.unwrap_or(current.configurable()),
                    )
                }
            };
            self.store_array_index_descriptor(arr, key, idx, updated)?;
            return Ok(true);
        }

        if current.is_data() != descriptor.is_data() {
            if !current.configurable() {
                return Ok(false);
            }
            let updated = if descriptor.is_data() {
                object::PropertyDescriptor::data(
                    descriptor.value.unwrap_or(Value::undefined()),
                    descriptor.writable.unwrap_or(false),
                    descriptor.enumerable.unwrap_or(current.enumerable()),
                    descriptor.configurable.unwrap_or(current.configurable()),
                )
            } else {
                object::PropertyDescriptor::accessor(
                    if descriptor.get.is_some() {
                        normalize_accessor_slot(descriptor.get)
                    } else {
                        None
                    },
                    if descriptor.set.is_some() {
                        normalize_accessor_slot(descriptor.set)
                    } else {
                        None
                    },
                    descriptor.enumerable.unwrap_or(current.enumerable()),
                    descriptor.configurable.unwrap_or(current.configurable()),
                )
            };
            self.store_array_index_descriptor(arr, key, idx, updated)?;
            return Ok(true);
        }

        match current.kind.clone() {
            object::DescriptorKind::Data {
                value: current_value,
            } => {
                if !current.configurable() && !current.writable() {
                    if matches!(descriptor.writable, Some(true)) {
                        return Ok(false);
                    }
                    if let Some(value) = &descriptor.value
                        && !abstract_ops::same_value(value, &current_value, &self.gc_heap)
                    {
                        return Ok(false);
                    }
                }
                let updated = object::PropertyDescriptor::data(
                    descriptor.value.unwrap_or(current_value),
                    descriptor.writable.unwrap_or(current.writable()),
                    descriptor.enumerable.unwrap_or(current.enumerable()),
                    descriptor.configurable.unwrap_or(current.configurable()),
                );
                self.store_array_index_descriptor(arr, key, idx, updated)?;
                Ok(true)
            }
            object::DescriptorKind::Accessor {
                getter: current_getter,
                setter: current_setter,
            } => {
                let getter = normalize_accessor_slot(descriptor.get);
                let setter = normalize_accessor_slot(descriptor.set);
                if !current.configurable()
                    && ((descriptor.get.is_some()
                        && !same_optional_value(&getter, &current_getter, &self.gc_heap))
                        || (descriptor.set.is_some()
                            && !same_optional_value(&setter, &current_setter, &self.gc_heap)))
                {
                    return Ok(false);
                }
                let updated = object::PropertyDescriptor::accessor(
                    if descriptor.get.is_some() {
                        getter
                    } else {
                        current_getter
                    },
                    if descriptor.set.is_some() {
                        setter
                    } else {
                        current_setter
                    },
                    descriptor.enumerable.unwrap_or(current.enumerable()),
                    descriptor.configurable.unwrap_or(current.configurable()),
                );
                self.store_array_index_descriptor(arr, key, idx, updated)?;
                Ok(true)
            }
        }
    }

    fn store_array_index_descriptor(
        &mut self,
        arr: array::JsArray,
        key: &str,
        idx: usize,
        descriptor: object::PropertyDescriptor,
    ) -> Result<(), VmError> {
        match descriptor.kind.clone() {
            object::DescriptorKind::Data { value } => {
                array::delete_accessor(arr, &mut self.gc_heap, key);
                array::define_index_value(arr, &mut self.gc_heap, idx, value)
                    .map_err(|_| VmError::TypeMismatch)?;
            }
            object::DescriptorKind::Accessor { getter, setter } => {
                array::set_accessor(arr, &mut self.gc_heap, key, getter, setter);
            }
        }
        array::set_property_flags(arr, &mut self.gc_heap, key, descriptor.flags);
        Ok(())
    }

    fn store_array_named_descriptor(
        &mut self,
        arr: array::JsArray,
        key: &str,
        descriptor: object::PropertyDescriptor,
    ) -> Result<(), VmError> {
        match descriptor.kind.clone() {
            object::DescriptorKind::Data { value } => {
                array::delete_accessor(arr, &mut self.gc_heap, key);
                array::define_named_data_property(arr, &mut self.gc_heap, key, value);
            }
            object::DescriptorKind::Accessor { getter, setter } => {
                array::set_accessor(arr, &mut self.gc_heap, key, getter, setter);
            }
        }
        array::set_property_flags(arr, &mut self.gc_heap, key, descriptor.flags);
        Ok(())
    }

    /// §6.2.5.4 FromPropertyDescriptor for a
    /// [`object::PartialPropertyDescriptor`] — emit only the fields
    /// the descriptor actually carries so trap observers see the
    /// same shape the caller passed.
    fn partial_descriptor_to_object(
        &mut self,
        descriptor: &object::PartialPropertyDescriptor,
        value_roots: &[&Value],
    ) -> Result<object::JsObject, VmError> {
        let mut roots = Vec::with_capacity(value_roots.len() + 3);
        roots.extend_from_slice(value_roots);
        if let Some(v) = &descriptor.value {
            roots.push(v);
        }
        if let Some(v) = &descriptor.get {
            roots.push(v);
        }
        if let Some(v) = &descriptor.set {
            roots.push(v);
        }
        let obj = self.alloc_runtime_rooted_object_with_roots(roots.as_slice(), &[])?;
        if let Some(v) = &descriptor.value {
            self.set_property(obj, "value", *v)?;
        }
        if let Some(w) = descriptor.writable {
            self.set_property(obj, "writable", Value::boolean(w))?;
        }
        if let Some(g) = &descriptor.get {
            self.set_property(obj, "get", *g)?;
        }
        if let Some(s) = &descriptor.set {
            self.set_property(obj, "set", *s)?;
        }
        if let Some(e) = descriptor.enumerable {
            self.set_property(obj, "enumerable", Value::boolean(e))?;
        }
        if let Some(c) = descriptor.configurable {
            self.set_property(obj, "configurable", Value::boolean(c))?;
        }
        Ok(obj)
    }
    /// §7.1.1 ToPrimitive synchronous helper. Used by sync callers
    /// (Reflect dispatcher, set / has / define paths) that need
    /// observable coercion outside the bytecode dispatch ladder.
    ///
    /// # See also
    /// - <https://tc39.es/ecma262/#sec-toprimitive>
    /// - <https://tc39.es/ecma262/#sec-ordinarytoprimitive>
    pub(crate) fn evaluate_to_primitive(
        &mut self,
        context: &ExecutionContext,
        input: &Value,
        hint: abstract_ops::ToPrimitiveHint,
    ) -> Result<Value, VmError> {
        if abstract_ops::is_primitive(input) {
            return Ok(*input);
        }
        // Step 1.a — try `@@toPrimitive` via OrdinaryGet on the
        // object's prototype chain. Falls back to ordinary toString /
        // valueOf when the exotic hook is absent.
        let to_prim_sym = self.well_known_symbols.get(symbol::WellKnown::ToPrimitive);
        let exotic = match self.ordinary_get_value(
            context,
            *input,
            *input,
            &VmPropertyKey::Symbol(to_prim_sym),
            0,
        )? {
            VmGetOutcome::Value(v) => v,
            VmGetOutcome::InvokeGetter { getter } => {
                let args: SmallVec<[Value; 8]> = SmallVec::new();
                self.run_callable_sync(context, &getter, *input, args)?
            }
        };
        if !exotic.is_nullish() {
            if !self.is_callable_runtime(&exotic) {
                return Err(VmError::TypeError {
                    message: "Symbol.toPrimitive method is not callable".to_string(),
                });
            }
            let hint_str = JsString::from_str(hint.as_token(), &mut self.gc_heap)?;
            let mut args: SmallVec<[Value; 8]> = SmallVec::new();
            args.push(Value::string(hint_str));
            let result = self.run_callable_sync(context, &exotic, *input, args)?;
            if abstract_ops::is_primitive(&result) {
                return Ok(result);
            }
            return Err(VmError::TypeError {
                message: "Symbol.toPrimitive returned a non-primitive".to_string(),
            });
        }
        self.evaluate_ordinary_to_primitive(context, input, hint)
    }

    /// §7.1.1.1 `OrdinaryToPrimitive` synchronous helper. Walks the
    /// hint-dependent `valueOf` / `toString` ladder via `ordinary_get_value`
    /// and `run_callable_sync` without first probing `@@toPrimitive` — this
    /// is the entry point used by `Date.prototype[@@toPrimitive]`
    /// (§21.4.4.45 step 6) to avoid the infinite recursion that would
    /// otherwise occur when `[Symbol.toPrimitive]` resolves to itself.
    ///
    /// # See also
    /// - <https://tc39.es/ecma262/#sec-ordinarytoprimitive>
    pub(crate) fn evaluate_ordinary_to_primitive(
        &mut self,
        context: &ExecutionContext,
        input: &Value,
        hint: abstract_ops::ToPrimitiveHint,
    ) -> Result<Value, VmError> {
        let names: [&str; 2] = match hint {
            abstract_ops::ToPrimitiveHint::String => ["toString", "valueOf"],
            _ => ["valueOf", "toString"],
        };
        for name in names {
            let method = match self.ordinary_get_value(
                context,
                *input,
                *input,
                &VmPropertyKey::String(name),
                0,
            )? {
                VmGetOutcome::Value(v) => v,
                VmGetOutcome::InvokeGetter { getter } => {
                    let args: SmallVec<[Value; 8]> = SmallVec::new();
                    self.run_callable_sync(context, &getter, *input, args)?
                }
            };
            if !self.is_callable_runtime(&method) {
                continue;
            }
            let args: SmallVec<[Value; 8]> = SmallVec::new();
            let result = self.run_callable_sync(context, &method, *input, args)?;
            if abstract_ops::is_primitive(&result) {
                return Ok(result);
            }
        }
        Err(VmError::TypeError {
            message: "OrdinaryToPrimitive could not convert object to primitive".to_string(),
        })
    }

    /// §6.2.5.5 ToPropertyDescriptor synchronous helper.
    ///
    /// Reads every spec-named field (`enumerable`, `configurable`,
    /// `value`, `writable`, `get`, `set`) via the full `[[Get]]`
    /// ladder so accessor getters on the source object are invoked
    /// observably and `HasProperty` walks the prototype chain.
    ///
    /// # See also
    /// - <https://tc39.es/ecma262/#sec-topropertydescriptor>
    pub(crate) fn evaluate_to_property_descriptor(
        &mut self,
        context: &ExecutionContext,
        attributes: &Value,
    ) -> Result<object::PartialPropertyDescriptor, VmError> {
        // Step 1 — `Type(Obj) is not Object → throw TypeError`. We
        // gate via the broader "type Object" check that includes
        // proxies / exotic value kinds.
        if !attributes.is_object_type() {
            return Err(VmError::TypeError {
                message: "ToPropertyDescriptor argument must be an Object".to_string(),
            });
        }

        let read_field = |this: &mut Self, name: &str| -> Result<Option<Value>, VmError> {
            let key = VmPropertyKey::String(name);
            if !this.ordinary_has_property_value(context, *attributes, &key, 0)? {
                return Ok(None);
            }
            let value = match this.ordinary_get_value(context, *attributes, *attributes, &key, 0)? {
                VmGetOutcome::Value(v) => v,
                VmGetOutcome::InvokeGetter { getter } => {
                    let args: SmallVec<[Value; 8]> = SmallVec::new();
                    this.run_callable_sync(context, &getter, *attributes, args)?
                }
            };
            Ok(Some(value))
        };

        let mut descriptor = object::PartialPropertyDescriptor::default();
        // §6.2.5.5 step 3 — enumerable.
        if let Some(v) = read_field(self, "enumerable")? {
            descriptor.enumerable = Some(v.to_boolean(&self.gc_heap));
        }
        // step 4 — configurable.
        if let Some(v) = read_field(self, "configurable")? {
            descriptor.configurable = Some(v.to_boolean(&self.gc_heap));
        }
        // step 5 — value.
        if let Some(v) = read_field(self, "value")? {
            descriptor.value = Some(v);
        }
        // step 6 — writable.
        if let Some(v) = read_field(self, "writable")? {
            descriptor.writable = Some(v.to_boolean(&self.gc_heap));
        }
        // step 7 — get.
        if let Some(v) = read_field(self, "get")? {
            if !v.is_undefined() && !self.is_callable_runtime(&v) {
                return Err(VmError::TypeError {
                    message: "Property descriptor `get` is not callable".to_string(),
                });
            }
            descriptor.get = Some(v);
        }
        // step 8 — set.
        if let Some(v) = read_field(self, "set")? {
            if !v.is_undefined() && !self.is_callable_runtime(&v) {
                return Err(VmError::TypeError {
                    message: "Property descriptor `set` is not callable".to_string(),
                });
            }
            descriptor.set = Some(v);
        }
        // step 9 — cannot mix accessor + data fields.
        if descriptor.is_accessor() && descriptor.is_data() {
            return Err(VmError::TypeError {
                message: "Property descriptor mixes accessor + data fields".to_string(),
            });
        }
        Ok(descriptor)
    }

    /// §7.1.19 ToPropertyKey synchronous helper. Used by Reflect /
    /// Object.defineProperty / Reflect.set / etc. for descriptor key
    /// coercion outside the dispatch ladder.
    ///
    /// # See also
    /// - <https://tc39.es/ecma262/#sec-topropertykey>
    pub(crate) fn evaluate_to_property_key(
        &mut self,
        context: &ExecutionContext,
        input: &Value,
    ) -> Result<VmPropertyKey<'static>, VmError> {
        let primitive =
            self.evaluate_to_primitive(context, input, abstract_ops::ToPrimitiveHint::String)?;
        if let Some(sym) = primitive.as_symbol(&self.gc_heap) {
            return Ok(VmPropertyKey::Symbol(sym));
        }
        Ok(VmPropertyKey::OwnedString(
            primitive.display_string(&self.gc_heap),
        ))
    }

    /// §10.5.11 / §10.1.11 — value-level `[[OwnPropertyKeys]]`.
    ///
    /// Returns every own property key (string + symbol, enumerable +
    /// non-enumerable) for `target`. For proxies the `ownKeys` trap
    /// is invoked and the result is validated against the §10.5.11
    /// invariants: trap entries must be Strings/Symbols, no duplicates,
    /// must include every non-configurable own key of the target, and
    /// when the target is non-extensible the result set must equal
    /// the target's own key set exactly.
    pub(crate) fn own_property_keys_value(
        &mut self,
        context: &ExecutionContext,
        target: &Value,
    ) -> Result<Vec<Value>, VmError> {
        self.ensure_deferred_namespace_ready(context, target, true)?;
        // §10.4.6.13 [[OwnPropertyKeys]] — exported string names in
        // ascending code-unit order, followed by the namespace's own
        // symbol keys (`@@toStringTag`).
        if let Some(obj) = target.as_object()
            && object::module_namespace_env(obj, &self.gc_heap).is_some()
        {
            let mut keys: Vec<Value> = Vec::new();
            for name in self.module_namespace_export_names(obj) {
                keys.push(Value::string(
                    string::JsString::from_str(&name, &mut self.gc_heap).map_err(VmError::from)?,
                ));
            }
            let symbols: Vec<Value> = object::with_properties(obj, &self.gc_heap, |p| {
                p.symbol_keys().map(Value::symbol).collect()
            });
            keys.extend(symbols);
            return Ok(keys);
        }
        if let Some(proxy) = target.as_proxy() {
            if proxy.is_revoked(&self.gc_heap) {
                return Err(VmError::TypeError {
                    message: "Cannot perform 'ownKeys' on a proxy that has been revoked"
                        .to_string(),
                });
            }
            let trap_args: SmallVec<[Value; 8]> = smallvec::smallvec![proxy.target(&self.gc_heap)];
            return match self.invoke_proxy_trap(context, &proxy, "ownKeys", trap_args)? {
                Some(trap_result) => {
                    let trap_keys =
                        self.create_list_from_array_like_property_keys(context, trap_result)?;
                    self.validate_proxy_own_keys(context, &proxy, trap_keys)
                }
                None => self.own_property_keys_value(context, &proxy.target(&self.gc_heap)),
            };
        }
        // §10.4.5.11 TypedArray [[OwnPropertyKeys]] — integer indices
        // in ascending order, then expando string keys in insertion
        // order, then expando symbol keys.
        if let Some(t) = target.as_typed_array(&self.gc_heap) {
            let mut keys: Vec<Value> = Vec::new();
            if !t.buffer(&self.gc_heap).is_detached(&self.gc_heap) {
                let len = t.length(&self.gc_heap);
                keys.reserve(len);
                for idx in 0..len {
                    keys.push(Value::string(
                        string::JsString::from_str(&idx.to_string(), &mut self.gc_heap)
                            .map_err(VmError::from)?,
                    ));
                }
            }
            if let Some(bag) = t.expando(&self.gc_heap) {
                let (strings, symbols): (Vec<String>, Vec<Value>) =
                    object::with_properties(bag, &self.gc_heap, |p| {
                        (
                            p.keys().map(str::to_string).collect(),
                            p.symbol_keys().map(Value::symbol).collect(),
                        )
                    });
                for name in strings {
                    keys.push(Value::string(
                        string::JsString::from_str(&name, &mut self.gc_heap)
                            .map_err(VmError::from)?,
                    ));
                }
                keys.extend(symbols);
            }
            return Ok(keys);
        }
        if let Some(obj) = target.as_object() {
            let mut keys: Vec<Value> = Vec::new();
            let string_data = object::string_data(obj, &self.gc_heap);
            if let Some(value) = &string_data {
                keys.reserve(value.len() as usize + 1);
                for idx in 0..value.len() {
                    let key = idx.to_string();
                    keys.push(Value::string(
                        string::JsString::from_str(&key, &mut self.gc_heap)
                            .map_err(VmError::from)?,
                    ));
                }
            }
            let is_string_exotic = string_data.is_some();
            let (ordinary_strings, symbols): (Vec<String>, Vec<Value>) =
                object::with_properties(obj, &self.gc_heap, |p| {
                    (
                        p.keys().map(str::to_string).collect(),
                        p.symbol_keys().map(Value::symbol).collect(),
                    )
                });
            if is_string_exotic {
                let string_len = string_data.as_ref().map_or(0, |value| value.len());
                let mut indexed = BTreeSet::new();
                let mut non_index_strings = Vec::new();
                for key in ordinary_strings {
                    if key == "length" {
                        continue;
                    }
                    match object::array_index_property_name(&key) {
                        Some(index) if index >= string_len => {
                            indexed.insert(index);
                        }
                        Some(_) => {}
                        None => non_index_strings.push(key),
                    }
                }
                for index in indexed {
                    let key = index.to_string();
                    keys.push(Value::string(
                        string::JsString::from_str(&key, &mut self.gc_heap)
                            .map_err(VmError::from)?,
                    ));
                }
                keys.push(Value::string(
                    string::JsString::from_str("length", &mut self.gc_heap)
                        .map_err(VmError::from)?,
                ));
                for key in non_index_strings {
                    keys.push(Value::string(
                        string::JsString::from_str(&key, &mut self.gc_heap)
                            .map_err(VmError::from)?,
                    ));
                }
            } else {
                for key in ordinary_strings {
                    keys.push(Value::string(
                        string::JsString::from_str(&key, &mut self.gc_heap)
                            .map_err(VmError::from)?,
                    ));
                }
            }
            keys.extend(symbols);
            return Ok(keys);
        }
        if let Some(arr) = target.as_array() {
            let (indices, string_keys) = self.gc_heap.read_payload(arr, |body| {
                let mut indices = BTreeSet::new();
                for (idx, value) in body.elements.iter().enumerate() {
                    if !value.is_hole() {
                        indices.insert(idx);
                    }
                }
                if let Some(sparse) = &body.sparse_elements {
                    indices.extend(sparse.keys().copied());
                }
                let mut string_keys = Vec::new();
                if let Some(named) = &body.named_properties {
                    string_keys.extend(named.keys().cloned());
                }
                if let Some(accessors) = &body.accessors {
                    for key in accessors.keys() {
                        if let Some(index) = object::array_index_property_name(key) {
                            indices.insert(index as usize);
                        } else if !string_keys.iter().any(|existing| existing == key) {
                            string_keys.push(key.clone());
                        }
                    }
                }
                (indices, string_keys)
            });
            let mut keys: Vec<Value> = Vec::with_capacity(indices.len() + string_keys.len() + 2);
            for idx in indices {
                let key = idx.to_string();
                let s =
                    string::JsString::from_str(&key, &mut self.gc_heap).map_err(VmError::from)?;
                keys.push(Value::string(s));
            }
            // §10.4.2 Array exotic objects always expose `length`.
            keys.push(Value::string(
                string::JsString::from_str("length", &mut self.gc_heap).map_err(VmError::from)?,
            ));
            for key in string_keys {
                let s =
                    string::JsString::from_str(&key, &mut self.gc_heap).map_err(VmError::from)?;
                keys.push(Value::string(s));
            }
            // §10.4.2 — own symbol-keyed properties follow the
            // string keys per §7.3.22 OrdinaryOwnPropertyKeys
            // ordering.
            for sym in array::own_symbol_keys(arr, &self.gc_heap) {
                keys.push(Value::symbol(sym));
            }
            return Ok(keys);
        }
        let fid = target.as_function().or_else(|| {
            target
                .as_closure(&self.gc_heap)
                .map(|c| c.cached_function_id)
        });
        if let Some(function_id) = fid {
            let names = self.ordinary_function_own_property_keys(context, function_id);
            let mut keys: Vec<Value> = Vec::with_capacity(names.len());
            for n in names {
                let s = string::JsString::from_str(&n, &mut self.gc_heap).map_err(VmError::from)?;
                keys.push(Value::string(s));
            }
            return Ok(keys);
        }
        if let Some(native) = target.as_native_function() {
            let names = native.own_property_keys(&self.gc_heap);
            let mut keys: Vec<Value> = Vec::with_capacity(names.len());
            for n in names {
                let s = string::JsString::from_str(&n, &mut self.gc_heap).map_err(VmError::from)?;
                keys.push(Value::string(s));
            }
            return Ok(keys);
        }
        if let Some(bound) = target.as_bound_function() {
            let names = function_metadata::bound_own_property_keys(&bound, &self.gc_heap);
            let mut keys: Vec<Value> = Vec::with_capacity(names.len());
            for n in names {
                let s = string::JsString::from_str(&n, &mut self.gc_heap).map_err(VmError::from)?;
                keys.push(Value::string(s));
            }
            return Ok(keys);
        }
        if let Some(class) = target.as_class_constructor() {
            let names = self.class_constructor_own_property_keys(Some(context), class)?;
            let mut keys: Vec<Value> = Vec::with_capacity(names.len());
            for n in names {
                let s = string::JsString::from_str(&n, &mut self.gc_heap).map_err(VmError::from)?;
                keys.push(Value::string(s));
            }
            return Ok(keys);
        }
        if let Some(regexp) = target.as_regexp() {
            let mut keys = Vec::new();
            keys.push(Value::string(
                string::JsString::from_str("lastIndex", &mut self.gc_heap)
                    .map_err(VmError::from)?,
            ));
            if let Some(expando) = regexp.expando(&self.gc_heap) {
                let (strings, symbols): (Vec<String>, Vec<Value>) =
                    object::with_properties(expando, &self.gc_heap, |p| {
                        (
                            p.keys().map(str::to_string).collect(),
                            p.symbol_keys().map(Value::symbol).collect(),
                        )
                    });
                for key in strings {
                    keys.push(Value::string(
                        string::JsString::from_str(&key, &mut self.gc_heap)
                            .map_err(VmError::from)?,
                    ));
                }
                keys.extend(symbols);
            }
            return Ok(keys);
        }
        if let Some(dv) = target.as_data_view() {
            // §25.3 — own keys are exactly the ordinary expando entries
            // (byteLength / byteOffset / buffer are prototype getters).
            let mut keys = Vec::new();
            if let Some(expando) = dv.expando(&self.gc_heap) {
                let (strings, symbols): (Vec<String>, Vec<Value>) =
                    object::with_properties(expando, &self.gc_heap, |p| {
                        (
                            p.keys().map(str::to_string).collect(),
                            p.symbol_keys().map(Value::symbol).collect(),
                        )
                    });
                for key in strings {
                    keys.push(Value::string(
                        string::JsString::from_str(&key, &mut self.gc_heap)
                            .map_err(VmError::from)?,
                    ));
                }
                keys.extend(symbols);
            }
            return Ok(keys);
        }
        Ok(Vec::new())
    }

    /// §7.3.18 CreateListFromArrayLike with elementTypes set to
    /// «String, Symbol» — used by Proxy `ownKeys` trap result
    /// validation per §10.5.11 step 8.
    fn create_list_from_array_like_property_keys(
        &mut self,
        context: &ExecutionContext,
        list_value: Value,
    ) -> Result<Vec<Value>, VmError> {
        if !(list_value.is_object() || list_value.is_array() || list_value.is_proxy()) {
            return Err(VmError::TypeError {
                message: "Proxy ownKeys trap result is not an Object".to_string(),
            });
        }
        let len_value = match self.ordinary_get_value(
            context,
            list_value,
            list_value,
            &VmPropertyKey::String("length"),
            0,
        )? {
            VmGetOutcome::Value(v) => v,
            VmGetOutcome::InvokeGetter { getter } => {
                let args: SmallVec<[Value; 8]> = SmallVec::new();
                self.run_callable_sync(context, &getter, list_value, args)?
            }
        };
        let len = to_length(&len_value, &self.gc_heap)?;
        let mut out: Vec<Value> = Vec::with_capacity(len);
        for i in 0..len {
            let key = VmPropertyKey::OwnedString(i.to_string());
            let element = match self.ordinary_get_value(context, list_value, list_value, &key, 0)? {
                VmGetOutcome::Value(v) => v,
                VmGetOutcome::InvokeGetter { getter } => {
                    let args: SmallVec<[Value; 8]> = SmallVec::new();
                    self.run_callable_sync(context, &getter, list_value, args)?
                }
            };
            if !(element.is_string() || element.is_symbol()) {
                return Err(VmError::TypeError {
                    message: "Proxy ownKeys trap result contains a non-property-key entry"
                        .to_string(),
                });
            }
            out.push(element);
        }
        Ok(out)
    }

    /// §10.5.11 steps 9–17 — validate a Proxy `ownKeys` trap result
    /// against the target's own keys.
    fn validate_proxy_own_keys(
        &mut self,
        context: &ExecutionContext,
        proxy: &proxy::JsProxy,
        trap_result: Vec<Value>,
    ) -> Result<Vec<Value>, VmError> {
        // Step 9 — reject duplicates. String keys hash into a set
        // (the spec requires linear behaviour here — see V8's
        // ownKeys-linear regression); symbol keys are compared
        // pairwise, which stays cheap because real handler results
        // carry at most a handful of symbols.
        let trap_strs: Vec<Option<String>> = trap_result
            .iter()
            .map(|v| {
                v.as_string(&self.gc_heap)
                    .map(|s| s.to_lossy_string(&self.gc_heap))
            })
            .collect();
        {
            let mut seen: std::collections::HashSet<&str> =
                std::collections::HashSet::with_capacity(trap_result.len());
            let mut symbol_indices: Vec<usize> = Vec::new();
            for (i, snap) in trap_strs.iter().enumerate() {
                match snap {
                    Some(name) => {
                        if !seen.insert(name.as_str()) {
                            return Err(VmError::TypeError {
                                message: "Proxy ownKeys trap result contains duplicate entries"
                                    .to_string(),
                            });
                        }
                    }
                    None => symbol_indices.push(i),
                }
            }
            for a in 0..symbol_indices.len() {
                for b in (a + 1)..symbol_indices.len() {
                    if same_property_key(
                        &trap_result[symbol_indices[a]],
                        &trap_result[symbol_indices[b]],
                        &self.gc_heap,
                    ) {
                        return Err(VmError::TypeError {
                            message: "Proxy ownKeys trap result contains duplicate entries"
                                .to_string(),
                        });
                    }
                }
            }
        }
        let target_value = proxy.target(&self.gc_heap);
        let extensible_target = self.is_extensible_value(context, &target_value)?;
        let target_keys = self.own_property_keys_value(context, &target_value)?;
        let mut target_configurable: Vec<Value> = Vec::new();
        let mut target_nonconfigurable: Vec<Value> = Vec::new();
        for key in &target_keys {
            let vm_key = property_key_from_value(key, &self.gc_heap)?;
            let slice_roots: [&[Value]; 4] = [
                target_keys.as_slice(),
                trap_result.as_slice(),
                target_configurable.as_slice(),
                target_nonconfigurable.as_slice(),
            ];
            let desc = self.ordinary_get_own_property_descriptor_value_runtime_rooted(
                context,
                target_value,
                &vm_key,
                0,
                &[&target_value],
                &slice_roots,
            )?;
            match desc {
                Some(d) if !d.configurable() => target_nonconfigurable.push(*key),
                _ => target_configurable.push(*key),
            }
        }
        if extensible_target && target_nonconfigurable.is_empty() {
            return Ok(trap_result);
        }
        // Steps 17–21 — consume trap keys against the target key
        // sets through a hash index (strings) plus a short linear
        // walk (symbols), keeping the whole validation linear.
        let mut str_index: std::collections::HashMap<&str, usize> =
            std::collections::HashMap::with_capacity(trap_result.len());
        for (i, snap) in trap_strs.iter().enumerate() {
            if let Some(name) = snap {
                str_index.insert(name.as_str(), i);
            }
        }
        let mut consumed: Vec<bool> = vec![false; trap_result.len()];
        let mut remaining = trap_result.len();
        let consume = |key: &Value,
                       consumed: &mut Vec<bool>,
                       remaining: &mut usize,
                       heap: &otter_gc::GcHeap|
         -> bool {
            if let Some(name) = key.as_string(heap).map(|s| s.to_lossy_string(heap)) {
                if let Some(&i) = str_index.get(name.as_str())
                    && !consumed[i]
                {
                    consumed[i] = true;
                    *remaining -= 1;
                    return true;
                }
                return false;
            }
            for (i, v) in trap_result.iter().enumerate() {
                if !consumed[i] && same_property_key(v, key, heap) {
                    consumed[i] = true;
                    *remaining -= 1;
                    return true;
                }
            }
            false
        };
        for key in &target_nonconfigurable {
            if !consume(key, &mut consumed, &mut remaining, &self.gc_heap) {
                return Err(VmError::TypeError {
                    message: "Proxy ownKeys trap result omits a non-configurable target own key"
                        .to_string(),
                });
            }
        }
        if extensible_target {
            return Ok(trap_result);
        }
        for key in &target_configurable {
            if !consume(key, &mut consumed, &mut remaining, &self.gc_heap) {
                return Err(VmError::TypeError {
                    message:
                        "Proxy ownKeys trap result omits a target own key while target is non-extensible"
                            .to_string(),
                });
            }
        }
        if remaining != 0 {
            return Err(VmError::TypeError {
                message:
                    "Proxy ownKeys trap result includes extra keys while target is non-extensible"
                        .to_string(),
            });
        }
        Ok(trap_result)
    }

    /// §10.5.2 / §10.1.2 — value-level `[[SetPrototypeOf]]`.
    /// Proxies dispatch through `setPrototypeOf` trap and enforce the
    /// §10.5.7 invariant for non-extensible targets.
    pub(crate) fn set_prototype_value_proxy_aware(
        &mut self,
        context: &ExecutionContext,
        target: &Value,
        proto: &Value,
    ) -> Result<bool, VmError> {
        // Deferred namespaces have an immutable null [[Prototype]]
        // (§28.3 [[SetPrototypeOf]] = SetImmutablePrototype): succeed
        // only when the requested prototype is also null.
        if let Some(obj) = target.as_object()
            && (object::deferred_namespace_target(obj, &self.gc_heap).is_some()
                || object::module_namespace_env(obj, &self.gc_heap).is_some())
        {
            return Ok(proto.is_null());
        }
        if let Some(proxy) = target.as_proxy() {
            if proxy.is_revoked(&self.gc_heap) {
                return Err(VmError::TypeError {
                    message: "Cannot perform 'setPrototypeOf' on a proxy that has been revoked"
                        .to_string(),
                });
            }
            let trap_args: SmallVec<[Value; 8]> =
                smallvec::smallvec![proxy.target(&self.gc_heap), *proto];
            return match self.invoke_proxy_trap(context, &proxy, "setPrototypeOf", trap_args)? {
                Some(result) => {
                    let ok = result.to_boolean(&self.gc_heap);
                    if !ok {
                        return Ok(false);
                    }
                    let target_value = proxy.target(&self.gc_heap);
                    let target_extensible = self.is_extensible_value(context, &target_value)?;
                    if !target_extensible {
                        let target_proto =
                            self.ordinary_get_prototype_value(context, target_value, 0)?;
                        if !abstract_ops::same_value(proto, &target_proto, &self.gc_heap) {
                            return Err(VmError::TypeError {
                                message:
                                    "Proxy setPrototypeOf invariant violated: target is non-extensible and prototypes differ"
                                        .to_string(),
                            });
                        }
                    }
                    Ok(true)
                }
                None => self.set_prototype_value_proxy_aware(
                    context,
                    &proxy.target(&self.gc_heap),
                    proto,
                ),
            };
        }
        // Class constructor [[SetPrototypeOf]] — record the identity
        // in the ctor_proto slot and mirror the walk-able chain on
        // the statics object (a class parent maps to its statics so
        // inherited statics keep resolving).
        if let Some(c) = target.as_class_constructor() {
            c.set_ctor_proto(&mut self.gc_heap, *proto);
            let statics_chain = if let Some(pc) = proto.as_class_constructor() {
                Value::object(pc.statics(&self.gc_heap))
            } else {
                *proto
            };
            let statics = Value::object(c.statics(&self.gc_heap));
            return self.set_prototype_value_proxy_aware(context, &statics, &statics_chain);
        }
        if let Some(obj) = target.as_object() {
            // §10.1.2 OrdinarySetPrototypeOf full algorithm.
            let current_proto =
                object::prototype_value(obj, &self.gc_heap).unwrap_or(Value::null());
            // §20.1.3 — %Object.prototype% is an
            // immutable-prototype exotic object. It reports
            // success only when the requested prototype is
            // SameValue with its current [[Prototype]].
            if self.object_prototype_object_opt() == Some(obj) {
                return Ok(abstract_ops::same_value(
                    proto,
                    &current_proto,
                    &self.gc_heap,
                ));
            }
            if abstract_ops::same_value(proto, &current_proto, &self.gc_heap) {
                return Ok(true);
            }
            if !object::is_extensible(obj, &self.gc_heap) {
                return Ok(false);
            }
            // Step 8 cycle check — walk the candidate chain looking
            // for O itself. Only ordinary-object hops; the spec
            // stops when an exotic [[GetPrototypeOf]] is hit.
            let mut p = *proto;
            let hard_cap = object::PROTO_CHAIN_HARD_CAP;
            let mut hops = 0;
            loop {
                if p.is_null() {
                    break;
                }
                if let Some(candidate) = p.as_object() {
                    if abstract_ops::same_value(
                        &Value::object(candidate),
                        &Value::object(obj),
                        &self.gc_heap,
                    ) {
                        return Ok(false);
                    }
                    if hops >= hard_cap {
                        break;
                    }
                    hops += 1;
                    p = object::prototype_value(candidate, &self.gc_heap).unwrap_or(Value::null());
                } else {
                    // Non-ordinary prototype links short-circuit per
                    // §10.1.2 step 8.c.i.
                    break;
                }
            }
            let proto_opt = if proto.is_null() { None } else { Some(*proto) };
            return Ok(object::set_prototype_value(
                obj,
                &mut self.gc_heap,
                proto_opt,
            ));
        }
        // §10.1.2 — TypedArrays accept ordinary [[SetPrototypeOf]];
        // the override rides a dedicated body slot consulted by every
        // prototype-chain walk.
        if let Some(t) = target.as_typed_array(&self.gc_heap) {
            t.set_custom_proto(&mut self.gc_heap, *proto);
            return Ok(true);
        }
        if let Some(arr) = target.as_array() {
            let current_proto = self.get_prototype_for_op(target)?;
            if abstract_ops::same_value(proto, &current_proto, &self.gc_heap) {
                return Ok(true);
            }
            if !array::is_extensible(arr, &self.gc_heap) {
                return Ok(false);
            }
            if abstract_ops::same_value(proto, target, &self.gc_heap) {
                return Ok(false);
            }
            let proto_opt = if proto.is_null() {
                None
            } else if proto.is_object_type() || proto.is_proxy() {
                Some(*proto)
            } else {
                return Ok(false);
            };
            array::set_prototype_override(arr, &mut self.gc_heap, proto_opt);
            return Ok(true);
        }
        Ok(true)
    }

    /// §10.5.4 / §10.1.4 — value-level `[[PreventExtensions]]`.
    pub(crate) fn prevent_extensions_value(
        &mut self,
        context: &ExecutionContext,
        value: &Value,
    ) -> Result<bool, VmError> {
        // A deferred namespace already reports non-extensible; succeed
        // without freezing the backing object so pending export
        // properties can still be installed on first access.
        if let Some(obj) = value.as_object()
            && object::deferred_namespace_target(obj, &self.gc_heap).is_some()
            && !object::deferred_namespace_is_populated(obj, &self.gc_heap)
        {
            return Ok(true);
        }
        if let Some(proxy) = value.as_proxy() {
            if proxy.is_revoked(&self.gc_heap) {
                return Err(VmError::TypeError {
                    message: "Cannot perform 'preventExtensions' on a proxy that has been revoked"
                        .to_string(),
                });
            }
            let trap_args: SmallVec<[Value; 8]> = smallvec::smallvec![proxy.target(&self.gc_heap)];
            return match self.invoke_proxy_trap(context, &proxy, "preventExtensions", trap_args)? {
                Some(result) => {
                    let ok = result.to_boolean(&self.gc_heap);
                    if ok && self.is_extensible_value(context, &proxy.target(&self.gc_heap))? {
                        return Err(VmError::TypeError {
                            message:
                                "Proxy preventExtensions trap succeeded but target is still extensible"
                                    .to_string(),
                        });
                    }
                    Ok(ok)
                }
                None => self.prevent_extensions_value(context, &proxy.target(&self.gc_heap)),
            };
        }
        if let Some(obj) = value.as_object() {
            object::prevent_extensions(obj, &mut self.gc_heap);
            return Ok(true);
        }
        // §10.4.5 — a TypedArray's extensibility lives on its lazy
        // expando bag (elements are exempt from [[Extensible]]).
        if let Some(t) = value.as_typed_array(&self.gc_heap) {
            let bag =
                crate::property_dispatch::typed_array_ensure_expando_pub(&mut self.gc_heap, &t)?;
            object::prevent_extensions(bag, &mut self.gc_heap);
            return Ok(true);
        }
        if let Some(arr) = value.as_array() {
            array::prevent_extensions(arr, &mut self.gc_heap);
            return Ok(true);
        }
        if let Some(native) = value.as_native_function() {
            native.prevent_extensions(&mut self.gc_heap);
            return Ok(true);
        }
        let fid = value.as_function().or_else(|| {
            value
                .as_closure(&self.gc_heap)
                .map(|c| c.cached_function_id)
        });
        if let Some(function_id) = fid {
            self.ordinary_function_prevent_extensions(function_id);
            return Ok(true);
        }
        if let Some(regexp) = value.as_regexp() {
            regexp.prevent_extensions(&mut self.gc_heap);
            return Ok(true);
        }
        Ok(true)
    }

    pub(crate) fn instanceof_target_prototype(
        &mut self,
        context: &ExecutionContext,
        rhs: &Value,
    ) -> Result<Option<Value>, VmError> {
        if rhs.is_object() || rhs.is_proxy() {
            let key = VmPropertyKey::String("prototype");
            return match self.ordinary_get_value(context, *rhs, *rhs, &key, 0)? {
                VmGetOutcome::Value(v) if v.is_undefined() => Ok(Some(*rhs)),
                VmGetOutcome::Value(value) if value.is_object() || value.is_proxy() => {
                    Ok(Some(value))
                }
                VmGetOutcome::Value(_) => Err(VmError::TypeError {
                    message: "instanceof prototype is not an object".to_string(),
                }),
                VmGetOutcome::InvokeGetter { getter } => {
                    let args: SmallVec<[Value; 8]> = SmallVec::new();
                    let value = self.run_callable_sync(context, &getter, *rhs, args)?;
                    if value.is_object() || value.is_proxy() {
                        Ok(Some(value))
                    } else {
                        Err(VmError::TypeError {
                            message: "instanceof prototype is not an object".to_string(),
                        })
                    }
                }
            };
        }
        let fid = rhs
            .as_function()
            .or_else(|| rhs.as_closure(&self.gc_heap).map(|c| c.cached_function_id));
        if let Some(function_id) = fid {
            let value = self.function_property_get(context, function_id, "prototype")?;
            return if value.is_object() || value.is_proxy() {
                Ok(Some(value))
            } else {
                Err(VmError::TypeError {
                    message: "instanceof prototype is not an object".to_string(),
                })
            };
        }
        if let Some(class) = rhs.as_class_constructor() {
            return Ok(Some(Value::object(class.prototype(&self.gc_heap))));
        }
        if let Some(native) = rhs.as_native_function() {
            let desc = native
                .own_property_descriptor(&mut self.gc_heap, "prototype")
                .map_err(VmError::from)?;
            let value = match desc {
                Some(object::PropertyDescriptor {
                    kind: object::DescriptorKind::Data { value },
                    ..
                }) => value,
                Some(object::PropertyDescriptor {
                    kind: object::DescriptorKind::Accessor { getter, .. },
                    ..
                }) => match getter {
                    Some(getter) if abstract_ops::is_callable(&getter) => {
                        let args: SmallVec<[Value; 8]> = SmallVec::new();
                        self.run_callable_sync(context, &getter, *rhs, args)?
                    }
                    _ => Value::undefined(),
                },
                None => Value::undefined(),
            };
            return if value.is_object() || value.is_proxy() {
                Ok(Some(value))
            } else {
                Err(VmError::TypeError {
                    message: "instanceof prototype is not an object".to_string(),
                })
            };
        }
        Ok(None)
    }

    pub(crate) fn instanceof_target_prototype_stack_rooted(
        &mut self,
        context: &ExecutionContext,
        stack: &SmallVec<[Frame; 8]>,
        rhs: &Value,
    ) -> Result<Option<Value>, VmError> {
        if rhs.is_object() || rhs.is_proxy() {
            let key = VmPropertyKey::String("prototype");
            return match self.ordinary_get_value(context, *rhs, *rhs, &key, 0)? {
                VmGetOutcome::Value(v) if v.is_undefined() => Ok(Some(*rhs)),
                VmGetOutcome::Value(value) if value.is_object() || value.is_proxy() => {
                    Ok(Some(value))
                }
                VmGetOutcome::Value(_) => Err(VmError::TypeError {
                    message: "instanceof prototype is not an object".to_string(),
                }),
                VmGetOutcome::InvokeGetter { getter } => {
                    let args: SmallVec<[Value; 8]> = SmallVec::new();
                    let value = self.run_callable_sync(context, &getter, *rhs, args)?;
                    if value.is_object() || value.is_proxy() {
                        Ok(Some(value))
                    } else {
                        Err(VmError::TypeError {
                            message: "instanceof prototype is not an object".to_string(),
                        })
                    }
                }
            };
        }
        let fid = rhs
            .as_function()
            .or_else(|| rhs.as_closure(&self.gc_heap).map(|c| c.cached_function_id));
        if let Some(function_id) = fid {
            let value =
                self.function_property_get_stack_rooted(context, stack, function_id, "prototype")?;
            return if value.is_object() || value.is_proxy() {
                Ok(Some(value))
            } else {
                Err(VmError::TypeError {
                    message: "instanceof prototype is not an object".to_string(),
                })
            };
        }
        if let Some(class) = rhs.as_class_constructor() {
            return Ok(Some(Value::object(class.prototype(&self.gc_heap))));
        }
        if let Some(native) = rhs.as_native_function() {
            let desc = native
                .own_property_descriptor(&mut self.gc_heap, "prototype")
                .map_err(VmError::from)?;
            let value = match desc {
                Some(object::PropertyDescriptor {
                    kind: object::DescriptorKind::Data { value },
                    ..
                }) => value,
                Some(object::PropertyDescriptor {
                    kind: object::DescriptorKind::Accessor { getter, .. },
                    ..
                }) => match getter {
                    Some(getter) if abstract_ops::is_callable(&getter) => {
                        let args: SmallVec<[Value; 8]> = SmallVec::new();
                        self.run_callable_sync(context, &getter, *rhs, args)?
                    }
                    _ => Value::undefined(),
                },
                None => Value::undefined(),
            };
            return if value.is_object() || value.is_proxy() {
                Ok(Some(value))
            } else {
                Err(VmError::TypeError {
                    message: "instanceof prototype is not an object".to_string(),
                })
            };
        }
        Ok(None)
    }

    pub(crate) fn value_has_proxy_aware_prototype(
        &mut self,
        context: &ExecutionContext,
        lhs: Value,
        target_proto: &Value,
    ) -> Result<bool, VmError> {
        let mut current = lhs;
        for hops in 0..object::PROTO_CHAIN_HARD_CAP {
            current = self.ordinary_get_prototype_value(context, current, hops)?;
            if current.is_null() {
                return Ok(false);
            }
            if abstract_ops::same_value(&current, target_proto, &self.gc_heap) {
                return Ok(true);
            }
        }
        Ok(false)
    }

    pub(crate) fn ordinary_get_value(
        &mut self,
        context: &ExecutionContext,
        base: Value,
        receiver: Value,
        key: &VmPropertyKey,
        hops: usize,
    ) -> Result<VmGetOutcome, VmError> {
        if hops >= object::PROTO_CHAIN_HARD_CAP {
            return Ok(VmGetOutcome::Value(Value::undefined()));
        }
        // TC39 import defer — accessing a deferred namespace evaluates
        // its module, then reads delegate to the module environment.
        self.ensure_deferred_namespace_ready(
            context,
            &base,
            !Self::deferred_key_is_symbol_like(key),
        )?;
        if let Some(obj) = base.as_object() {
            // §10.4.6.8 [[Get]] — a Module Namespace Exotic Object
            // resolves string keys through the wrapped environment;
            // symbol keys (e.g. @@toStringTag) fall through to its own
            // properties.
            if object::module_namespace_env(obj, &self.gc_heap).is_some()
                && let Some(name) = key.string_name()
            {
                // §10.4.6.8 [[Get]] — resolve the export through the
                // module's ResolveExport table to the defining module's
                // live binding. A re-exported / star-exported name reads
                // the source env, not a snapshot.
                return match self.module_namespace_get_binding(obj, name) {
                    // step 9 — reading an export still in its TDZ
                    // (uninitialized binding slot) is a ReferenceError.
                    Some(value) if value.is_hole() => Err(VmError::ThisUninitialized {
                        message: format!("Cannot access '{name}' before initialization"),
                    }),
                    Some(value) => Ok(VmGetOutcome::Value(value)),
                    None => Ok(VmGetOutcome::Value(Value::undefined())),
                };
            }
            if let Some(value) = self.string_object_exotic_get(obj, key)? {
                return Ok(VmGetOutcome::Value(value));
            }
            return match self.lookup_own_vm_property_key(obj, key) {
                object::PropertyLookup::Data { value, .. } => Ok(VmGetOutcome::Value(value)),
                object::PropertyLookup::Accessor { getter, .. } => match getter {
                    Some(getter) if abstract_ops::is_callable(&getter) => {
                        Ok(VmGetOutcome::InvokeGetter { getter })
                    }
                    _ => Ok(VmGetOutcome::Value(Value::undefined())),
                },
                object::PropertyLookup::Absent => match object::prototype_value(obj, &self.gc_heap)
                {
                    Some(proto) => self.ordinary_get_value(context, proto, receiver, key, hops + 1),
                    None => Ok(VmGetOutcome::Value(Value::undefined())),
                },
            };
        }
        if let Some(proxy) = base.as_proxy() {
            let key_value = self.vm_property_key_to_value(key)?;
            let trap_args: SmallVec<[Value; 8]> =
                smallvec::smallvec![proxy.target(&self.gc_heap), key_value, receiver];
            return match self.invoke_proxy_trap(context, &proxy, "get", trap_args)? {
                Some(value) => {
                    self.validate_proxy_get_invariants(&proxy.target(&self.gc_heap), key, &value)?;
                    Ok(VmGetOutcome::Value(value))
                }
                None => self.ordinary_get_value(
                    context,
                    proxy.target(&self.gc_heap),
                    receiver,
                    key,
                    hops + 1,
                ),
            };
        }
        if let Some(arr) = base.as_array() {
            let value = match key {
                VmPropertyKey::Symbol(sym) => {
                    if let Some((getter, _)) =
                        crate::array::get_symbol_accessor(arr, &self.gc_heap, *sym)
                    {
                        match getter {
                            Some(callable) if abstract_ops::is_callable(&callable) => {
                                return Ok(VmGetOutcome::InvokeGetter { getter: callable });
                            }
                            _ => return Ok(VmGetOutcome::Value(Value::undefined())),
                        }
                    }
                    if let Some(v) = crate::array::get_symbol_property(arr, &self.gc_heap, *sym) {
                        v
                    } else if let Some(p) = self
                        .constructor_prototype_value("Array")
                        .ok()
                        .and_then(|v| v.as_object())
                    {
                        return self.ordinary_get_value(
                            context,
                            Value::object(p),
                            receiver,
                            key,
                            hops + 1,
                        );
                    } else {
                        Value::undefined()
                    }
                }
                _ => {
                    let key_str = key
                        .string_name()
                        .expect("non-symbol key has string spelling");
                    if let Some((getter, _)) =
                        crate::array::get_accessor(arr, &self.gc_heap, key_str)
                    {
                        match getter {
                            Some(callable) if abstract_ops::is_callable(&callable) => {
                                return Ok(VmGetOutcome::InvokeGetter { getter: callable });
                            }
                            _ => return Ok(VmGetOutcome::Value(Value::undefined())),
                        }
                    }
                    match crate::array::get_named_property(arr, &self.gc_heap, key_str) {
                        Some(v) => v,
                        None => {
                            let proto = self.constructor_prototype_value("Array")?;
                            if let Some(proto_obj) = proto.as_object() {
                                return self.ordinary_get_value(
                                    context,
                                    Value::object(proto_obj),
                                    receiver,
                                    key,
                                    hops + 1,
                                );
                            }
                            Value::undefined()
                        }
                    }
                }
            };
            return Ok(VmGetOutcome::Value(value));
        }
        let fid = base
            .as_function()
            .or_else(|| base.as_closure(&self.gc_heap).map(|c| c.cached_function_id));
        if let Some(function_id) = fid {
            let lookup = match key {
                VmPropertyKey::Symbol(sym) => {
                    match self
                        .function_user_props
                        .get(&function_id)
                        .copied()
                        .and_then(|bag| object::get_own_symbol_descriptor(bag, &self.gc_heap, *sym))
                    {
                        Some(desc) => descriptor_to_lookup(desc),
                        None => self
                            .function_kind_prototype_for(context, function_id)
                            .and_then(|proto| {
                                match object::lookup_symbol(proto, &self.gc_heap, *sym) {
                                    object::PropertyLookup::Absent => None,
                                    lookup => Some(lookup),
                                }
                            })
                            .or_else(|| {
                                self.function_prototype_object()
                                    .ok()
                                    .map(|proto| object::lookup_symbol(proto, &self.gc_heap, *sym))
                            })
                            .unwrap_or(object::PropertyLookup::Absent),
                    }
                }
                _ => {
                    let key_name = key
                        .string_name()
                        .expect("non-symbol key has string spelling");
                    let own = self.ordinary_function_own_property_descriptor(
                        Some(context),
                        function_id,
                        key_name,
                    )?;
                    match own {
                        Some(desc) => descriptor_to_lookup(desc),
                        None => self
                            .function_kind_prototype_for(context, function_id)
                            .and_then(|proto| {
                                match object::lookup(proto, &self.gc_heap, key_name) {
                                    object::PropertyLookup::Absent => None,
                                    lookup => Some(lookup),
                                }
                            })
                            .or_else(|| {
                                self.function_prototype_object()
                                    .ok()
                                    .map(|proto| object::lookup(proto, &self.gc_heap, key_name))
                            })
                            .unwrap_or(object::PropertyLookup::Absent),
                    }
                }
            };
            let value = match lookup {
                object::PropertyLookup::Data { value, .. } => value,
                object::PropertyLookup::Accessor { getter, .. } => {
                    return Ok(match getter {
                        Some(getter) if abstract_ops::is_callable(&getter) => {
                            VmGetOutcome::InvokeGetter { getter }
                        }
                        _ => VmGetOutcome::Value(Value::undefined()),
                    });
                }
                object::PropertyLookup::Absent => Value::undefined(),
            };
            if let Some(outcome) = self.callable_realm_prototype_accessor_outcome(&value, key)? {
                return Ok(outcome);
            }
            return Ok(VmGetOutcome::Value(value));
        }
        if let Some(native) = base.as_native_function() {
            let value = match key {
                VmPropertyKey::Symbol(sym) => {
                    match native.own_symbol_property_descriptor(&self.gc_heap, *sym) {
                        Some(object::PropertyDescriptor {
                            kind: object::DescriptorKind::Data { value },
                            ..
                        }) => value,
                        Some(object::PropertyDescriptor {
                            kind: object::DescriptorKind::Accessor { getter, .. },
                            ..
                        }) => {
                            return Ok(match getter {
                                Some(getter) if abstract_ops::is_callable(&getter) => {
                                    VmGetOutcome::InvokeGetter { getter }
                                }
                                _ => VmGetOutcome::Value(Value::undefined()),
                            });
                        }
                        None => {
                            // §10.1.8 — a native constructor with a
                            // non-default [[Prototype]] (e.g. `Uint8Array`
                            // → abstract `%TypedArray%`) must walk that
                            // chain for inherited accessors like
                            // `@@species`, not jump straight to
                            // `Function.prototype`.
                            if let Some(proto) = native.prototype_override(&self.gc_heap) {
                                return self.ordinary_get_value(
                                    context,
                                    proto,
                                    receiver,
                                    key,
                                    hops + 1,
                                );
                            }
                            self.function_prototype_object()
                                .ok()
                                .and_then(|p| object::get_symbol(p, &self.gc_heap, *sym))
                                .unwrap_or(Value::undefined())
                        }
                    }
                }
                _ => {
                    let key_name = key
                        .string_name()
                        .expect("non-symbol key has string spelling");
                    match native.own_property_descriptor(&mut self.gc_heap, key_name)? {
                        Some(object::PropertyDescriptor {
                            kind: object::DescriptorKind::Data { value },
                            ..
                        }) => value,
                        Some(object::PropertyDescriptor {
                            kind: object::DescriptorKind::Accessor { getter, .. },
                            ..
                        }) => {
                            return Ok(match getter {
                                Some(getter) if abstract_ops::is_callable(&getter) => {
                                    VmGetOutcome::InvokeGetter { getter }
                                }
                                _ => VmGetOutcome::Value(Value::undefined()),
                            });
                        }
                        None => {
                            if let Some(proto) = native.prototype_override(&self.gc_heap) {
                                return self.ordinary_get_value(
                                    context,
                                    proto,
                                    receiver,
                                    key,
                                    hops + 1,
                                );
                            }
                            self.load_function_prototype_method(key_name)
                                .or_else(|| self.load_object_prototype_method(key_name))
                                .unwrap_or(Value::undefined())
                        }
                    }
                }
            };
            if let Some(outcome) = self.callable_realm_prototype_accessor_outcome(&value, key)? {
                return Ok(outcome);
            }
            return Ok(VmGetOutcome::Value(value));
        }
        if let Some(bound) = base.as_bound_function() {
            let value = match key {
                VmPropertyKey::Symbol(sym) => self
                    .function_prototype_object()
                    .ok()
                    .and_then(|p| object::get_symbol(p, &self.gc_heap, *sym))
                    .unwrap_or(Value::undefined()),
                _ => {
                    let key = key
                        .string_name()
                        .expect("non-symbol key has string spelling");
                    match function_metadata::bound_own_property_descriptor(
                        &bound,
                        &mut self.gc_heap,
                        key,
                    )? {
                        Some(desc) => match &desc.kind {
                            object::DescriptorKind::Data { value } => *value,
                            object::DescriptorKind::Accessor { getter, .. } => {
                                return Ok(match getter {
                                    Some(getter) if abstract_ops::is_callable(getter) => {
                                        VmGetOutcome::InvokeGetter { getter: *getter }
                                    }
                                    _ => VmGetOutcome::Value(Value::undefined()),
                                });
                            }
                        },
                        None => self
                            .load_function_prototype_method(key)
                            .or_else(|| self.load_object_prototype_method(key))
                            .unwrap_or(Value::undefined()),
                    }
                }
            };
            if let Some(outcome) = self.callable_realm_prototype_accessor_outcome(&value, key)? {
                return Ok(outcome);
            }
            return Ok(VmGetOutcome::Value(value));
        }
        if let Some(class) = base.as_class_constructor() {
            if key.string_name().is_some_and(|k| k == "prototype") {
                return Ok(VmGetOutcome::Value(Value::object(
                    class.prototype(&self.gc_heap),
                )));
            }
            let statics = class.statics(&self.gc_heap);
            // `name` / `length` live on the backing constructor
            // function (user-property overrides and deletions
            // included) unless a static member shadows them.
            if let Some(k) = key.string_name()
                && (k == "name" || k == "length")
                && object::get_own_descriptor(statics, &self.gc_heap, k).is_none()
            {
                let ctor = class.ctor(&self.gc_heap);
                return self.ordinary_get_value(context, ctor, receiver, key, hops + 1);
            }
            let outcome =
                self.ordinary_get_value(context, Value::object(statics), receiver, key, hops + 1)?;
            let value = match &outcome {
                VmGetOutcome::Value(v) => *v,
                VmGetOutcome::InvokeGetter { .. } => return Ok(outcome),
            };
            if let Some(outcome) = self.callable_realm_prototype_accessor_outcome(&value, key)? {
                return Ok(outcome);
            }
            return Ok(VmGetOutcome::Value(value));
        }
        if let Some(re) = base.as_regexp() {
            if let Some(bag) = re.expando(&self.gc_heap) {
                let lookup = match key {
                    VmPropertyKey::Symbol(sym) => {
                        object::lookup_own_symbol(bag, &self.gc_heap, *sym)
                    }
                    _ => {
                        let key = key
                            .string_name()
                            .expect("non-symbol key has string spelling");
                        object::lookup_own(bag, &self.gc_heap, key)
                    }
                };
                match lookup {
                    object::PropertyLookup::Data { value, .. } => {
                        return Ok(VmGetOutcome::Value(value));
                    }
                    object::PropertyLookup::Accessor { getter, .. } => {
                        return Ok(match getter {
                            Some(getter) if abstract_ops::is_callable(&getter) => {
                                VmGetOutcome::InvokeGetter { getter }
                            }
                            _ => VmGetOutcome::Value(Value::undefined()),
                        });
                    }
                    object::PropertyLookup::Absent => {}
                }
            }
            // `lastIndex` is the RegExp's only own data property;
            // `source` / `flags` / `global` / … are accessors on
            // `%RegExp.prototype%`. Resolving the latter here from the
            // internal slots would skip the prototype getters, so an
            // overridden / poisoned flag accessor (and the observable
            // component reads `get flags` performs) would never run.
            // Only `lastIndex` short-circuits; the rest fall to the
            // prototype walk below.
            // Match on the resolved name, not a single `String` literal:
            // a key forwarded through a Proxy (or any atomized read)
            // arrives as `Atom` / `OwnedString`, which must still resolve
            // the RegExp's only own data property.
            let direct = if key.string_name() == Some("lastIndex") {
                regexp_prototype::load_property(&re, &mut self.gc_heap, "lastIndex")
            } else {
                Value::undefined()
            };
            return if direct.is_undefined() {
                // Walk the instance's actual `[[Prototype]]` so a
                // `class X extends RegExp` override (e.g. `exec`,
                // `@@replace`) on `X.prototype` shadows the base
                // `%RegExp.prototype%` method, instead of jumping
                // straight to the intrinsic.
                let proto = match re.prototype_override(&self.gc_heap) {
                    Some(p) => p,
                    None => self.constructor_prototype_value("RegExp")?,
                };
                if proto.is_nullish() {
                    return Ok(VmGetOutcome::Value(Value::undefined()));
                }
                self.ordinary_get_value(context, proto, receiver, key, hops + 1)
            } else {
                Ok(VmGetOutcome::Value(direct))
            };
        }
        if let Some(t) = base.as_typed_array(&self.gc_heap) {
            // §10.4.5.4 — a CanonicalNumericIndexString key reads the
            // integer-indexed element via IntegerIndexedElementGet
            // (the element value, or `undefined` when the index is
            // out of bounds / fractional / the buffer is detached). It
            // does NOT consult the expando bag or walk the prototype.
            // The element-opcode path resolves these, but a string-key
            // `[[Get]]` (`Reflect.get`, generic `Array.prototype.*`,
            // HasProperty) reached `load_property`, which only knew the
            // named accessors — so `ta["0"]` came back `undefined`.
            if !matches!(key, VmPropertyKey::Symbol(_)) {
                let name = key
                    .string_name()
                    .expect("non-symbol key has string spelling");
                if let Some(n) = crate::property_dispatch::canonical_numeric_index_string(name) {
                    let value = match crate::property_dispatch::typed_array_valid_index(
                        &t,
                        &self.gc_heap,
                        n,
                    ) {
                        Some(idx) => t.get(&mut self.gc_heap, idx)?,
                        None => Value::undefined(),
                    };
                    return Ok(VmGetOutcome::Value(value));
                }
            }
            // TypedArray [[Get]] for non-index keys — expando own
            // properties first (so user-assigned `constructor` /
            // accessors win), then the per-kind builtin prototype
            // methods, then the kind's constructor prototype chain.
            // Mirrors the opcode `run_load_property_reg` path so
            // synchronous gets (`SpeciesConstructor`, `Reflect.get`)
            // resolve identically.
            if let Some(bag) = t.expando(&self.gc_heap) {
                let lookup = match key {
                    VmPropertyKey::Symbol(sym) => {
                        object::lookup_own_symbol(bag, &self.gc_heap, *sym)
                    }
                    _ => {
                        let key = key
                            .string_name()
                            .expect("non-symbol key has string spelling");
                        object::lookup_own(bag, &self.gc_heap, key)
                    }
                };
                match lookup {
                    object::PropertyLookup::Data { value, .. } => {
                        return Ok(VmGetOutcome::Value(value));
                    }
                    object::PropertyLookup::Accessor { getter, .. } => {
                        return Ok(match getter {
                            Some(getter) if abstract_ops::is_callable(&getter) => {
                                VmGetOutcome::InvokeGetter { getter }
                            }
                            _ => VmGetOutcome::Value(Value::undefined()),
                        });
                    }
                    object::PropertyLookup::Absent => {}
                }
            }
            let direct = match key {
                VmPropertyKey::Symbol(_) => Value::undefined(),
                _ => {
                    let key = key
                        .string_name()
                        .expect("non-symbol key has string spelling");
                    crate::binary::typed_array_prototype::load_property(&t, &self.gc_heap, key)
                }
            };
            return if direct.is_undefined() {
                // §10.4.5.4 walks the instance's actual [[Prototype]]
                // (a subclass `X.prototype` when `class X extends
                // Uint8Array`), not the kind's default prototype — so
                // `O.constructor` / user-added prototype props resolve
                // against the real chain. `get_prototype_for_op`
                // returns the per-instance override or the intrinsic.
                let proto = self.get_prototype_for_op(&base)?;
                if proto.is_nullish() {
                    return Ok(VmGetOutcome::Value(Value::undefined()));
                }
                self.ordinary_get_value(context, proto, receiver, key, hops + 1)
            } else {
                Ok(VmGetOutcome::Value(direct))
            };
        }
        if base.is_map() || base.is_set() || base.is_weak_map() || base.is_weak_set() {
            let proto_name = if base.is_map() {
                "Map"
            } else if base.is_set() {
                "Set"
            } else if base.is_weak_map() {
                "WeakMap"
            } else {
                "WeakSet"
            };
            let proto = self.constructor_prototype_value(proto_name)?;
            if proto.is_nullish() {
                return Ok(VmGetOutcome::Value(Value::undefined()));
            }
            return self.ordinary_get_value(context, proto, receiver, key, hops + 1);
        }
        if let Some(promise) = base.as_promise() {
            if let Some(bag) = promise.expando(&self.gc_heap) {
                let lookup = match key {
                    VmPropertyKey::Symbol(sym) => {
                        object::lookup_own_symbol(bag, &self.gc_heap, *sym)
                    }
                    _ => {
                        let name = key
                            .string_name()
                            .expect("non-symbol key has string spelling");
                        object::lookup_own(bag, &self.gc_heap, name)
                    }
                };
                match lookup {
                    object::PropertyLookup::Data { value, .. } => {
                        return Ok(VmGetOutcome::Value(value));
                    }
                    object::PropertyLookup::Accessor { getter, .. } => {
                        return Ok(match getter {
                            Some(g) if abstract_ops::is_callable(&g) => {
                                VmGetOutcome::InvokeGetter { getter: g }
                            }
                            _ => VmGetOutcome::Value(Value::undefined()),
                        });
                    }
                    object::PropertyLookup::Absent => {}
                }
            }
            let proto = match promise.prototype_override(&self.gc_heap) {
                Some(over) => over,
                None => self.constructor_prototype_value("Promise")?,
            };
            if proto.is_nullish() {
                return Ok(VmGetOutcome::Value(Value::undefined()));
            }
            return self.ordinary_get_value(context, proto, receiver, key, hops + 1);
        }
        if base.is_big_int() {
            let proto = self.constructor_prototype_value("BigInt")?;
            if proto.is_nullish() {
                return Ok(VmGetOutcome::Value(Value::undefined()));
            }
            return self.ordinary_get_value(context, proto, receiver, key, hops + 1);
        }
        if base.is_boolean() || base.is_number() || base.is_symbol() {
            let proto_name = if base.is_boolean() {
                "Boolean"
            } else if base.is_number() {
                "Number"
            } else {
                "Symbol"
            };
            let proto = self.constructor_prototype_value(proto_name)?;
            if proto.is_nullish() {
                return Ok(VmGetOutcome::Value(Value::undefined()));
            }
            return self.ordinary_get_value(context, proto, receiver, key, hops + 1);
        }
        if let Some(s) = base.as_string(&self.gc_heap) {
            if let Some(name) = key.string_name() {
                if let Some(n) = crate::property_dispatch::canonical_numeric_index_string(name)
                    && n.is_finite()
                    && n.fract() == 0.0
                    && n >= 0.0
                    && (n as usize) < s.len() as usize
                {
                    let unit = s.char_code_at(n as u32, &self.gc_heap).unwrap_or(0);
                    let unit_str = crate::JsString::from_utf16_units(&[unit], &mut self.gc_heap)?;
                    return Ok(VmGetOutcome::Value(Value::string(unit_str)));
                }
                if name == "length" {
                    return Ok(VmGetOutcome::Value(Value::number_i32(s.len() as i32)));
                }
            }
            let proto = self.constructor_prototype_value("String")?;
            if proto.is_nullish() {
                return Ok(VmGetOutcome::Value(Value::undefined()));
            }
            return self.ordinary_get_value(context, proto, receiver, key, hops + 1);
        }
        if base.is_weak_ref() || base.is_finalization_registry() {
            let proto_name = if base.is_weak_ref() {
                "WeakRef"
            } else {
                "FinalizationRegistry"
            };
            let proto = self.constructor_prototype_value(proto_name)?;
            if proto.is_nullish() {
                return Ok(VmGetOutcome::Value(Value::undefined()));
            }
            return self.ordinary_get_value(context, proto, receiver, key, hops + 1);
        }
        if let Some(dv) = base.as_data_view() {
            // §25.3 — ordinary own properties in the lazy expando win
            // over the prototype walk.
            if let Some(bag) = dv.expando(&self.gc_heap) {
                let lookup = match key {
                    VmPropertyKey::Symbol(sym) => {
                        object::lookup_own_symbol(bag, &self.gc_heap, *sym)
                    }
                    _ => {
                        let name = key
                            .string_name()
                            .expect("non-symbol key has string spelling");
                        object::lookup_own(bag, &self.gc_heap, name)
                    }
                };
                match lookup {
                    object::PropertyLookup::Data { value, .. } => {
                        return Ok(VmGetOutcome::Value(value));
                    }
                    object::PropertyLookup::Accessor { getter, .. } => {
                        return Ok(match getter {
                            Some(g) if abstract_ops::is_callable(&g) => {
                                VmGetOutcome::InvokeGetter { getter: g }
                            }
                            _ => VmGetOutcome::Value(Value::undefined()),
                        });
                    }
                    object::PropertyLookup::Absent => {}
                }
            }
            let proto = self.get_prototype_for_op(&base)?;
            if proto.is_nullish() {
                return Ok(VmGetOutcome::Value(Value::undefined()));
            }
            return self.ordinary_get_value(context, proto, receiver, key, hops + 1);
        }
        if base.is_array_buffer() {
            let proto = self.get_prototype_for_op(&base)?;
            if proto.is_nullish() {
                return Ok(VmGetOutcome::Value(Value::undefined()));
            }
            return self.ordinary_get_value(context, proto, receiver, key, hops + 1);
        }
        if base.is_generator() || base.is_iterator() {
            let proto = self.get_prototype_for_op(&base)?;
            if proto.is_nullish() {
                return Ok(VmGetOutcome::Value(Value::undefined()));
            }
            return self.ordinary_get_value(context, proto, receiver, key, hops + 1);
        }
        if let Some(t) = base.as_typed_array(&self.gc_heap) {
            if let Some(name) = key.string_name() {
                if let Some(n) = crate::property_dispatch::canonical_numeric_index_string(name) {
                    let Some(idx) =
                        crate::property_dispatch::typed_array_valid_index(&t, &self.gc_heap, n)
                    else {
                        return Ok(VmGetOutcome::Value(Value::undefined()));
                    };
                    return Ok(VmGetOutcome::Value(
                        t.get(&mut self.gc_heap, idx).map_err(crate::oom_to_vm)?,
                    ));
                }
                if let Some(bag) = t.expando(&self.gc_heap)
                    && let Some(v) = crate::object::get(bag, &self.gc_heap, name)
                {
                    return Ok(VmGetOutcome::Value(v));
                }
            }
            if let VmPropertyKey::Symbol(sym) = key
                && let Some(bag) = t.expando(&self.gc_heap)
                && let Some(v) = crate::object::get_symbol(bag, &self.gc_heap, *sym)
            {
                return Ok(VmGetOutcome::Value(v));
            }
            let this_value = Value::typed_array(t);
            let proto = self.get_prototype_for_op(&this_value)?;
            if proto.is_nullish() {
                return Ok(VmGetOutcome::Value(Value::undefined()));
            }
            return self.ordinary_get_value(context, proto, receiver, key, hops + 1);
        }
        if base.as_temporal(&self.gc_heap).is_some() {
            // Temporal value: route property lookup through the
            // per-class prototype installed on `Temporal.<X>.prototype`.
            let proto = self.get_prototype_for_op(&base)?;
            if proto.is_nullish() {
                return Ok(VmGetOutcome::Value(Value::undefined()));
            }
            return self.ordinary_get_value(context, proto, receiver, key, hops + 1);
        }
        if let Some(intl) = base.as_intl(&self.gc_heap) {
            // ECMA-402: an `Intl.<Kind>` instance inherits its methods
            // from `Intl.<Kind>.prototype`. The instance carries no
            // own-property storage, so resolution walks straight to the
            // kind prototype installed by `crate::intl::bootstrap`.
            let proto = self.intl_kind_prototype_value(intl.kind().class_name());
            if proto.is_nullish() {
                return Ok(VmGetOutcome::Value(Value::undefined()));
            }
            return self.ordinary_get_value(context, proto, receiver, key, hops + 1);
        }
        Err(VmError::TypeMismatch)
    }

    /// Resolve `Intl.<class_name>.prototype` by walking
    /// `globalThis.Intl.<class_name>.prototype`. Returns `null` when
    /// the namespace or constructor is missing.
    fn intl_kind_prototype_value(&mut self, class_name: &str) -> Value {
        let Some(intl_ns) =
            object::get(self.global_this, &self.gc_heap, "Intl").and_then(|v| v.as_object())
        else {
            return Value::null();
        };
        let Some(ctor) = object::get(intl_ns, &self.gc_heap, class_name) else {
            return Value::null();
        };
        if let Some(native) = ctor.as_native_function() {
            return match native.own_property_descriptor(&mut self.gc_heap, "prototype") {
                Ok(Some(descriptor)) => descriptor_value(&descriptor),
                _ => Value::null(),
            };
        }
        if let Some(obj) = ctor.as_object() {
            return object::get(obj, &self.gc_heap, "prototype").unwrap_or_else(Value::null);
        }
        Value::null()
    }

    pub(crate) fn ordinary_has_property_value(
        &mut self,
        context: &ExecutionContext,
        base: Value,
        key: &VmPropertyKey,
        hops: usize,
    ) -> Result<bool, VmError> {
        if hops >= object::PROTO_CHAIN_HARD_CAP {
            return Ok(false);
        }
        self.ensure_deferred_namespace_ready(
            context,
            &base,
            !Self::deferred_key_is_symbol_like(key),
        )?;
        if let Some(obj) = base.as_object() {
            // §10.4.6.7 [[HasProperty]] — namespace string keys exist iff
            // the environment exports them; symbol keys check own props.
            if object::module_namespace_env(obj, &self.gc_heap).is_some()
                && let Some(name) = key.string_name()
            {
                // §10.4.6.7 — a string key exists iff it is one of the
                // module's resolved exported names (TDZ-independent).
                return Ok(self
                    .module_namespace_export_names(obj)
                    .iter()
                    .any(|exported| exported == name));
            }
            if !matches!(
                self.lookup_own_vm_property_key(obj, key),
                object::PropertyLookup::Absent
            ) {
                return Ok(true);
            }
            return match object::prototype_value(obj, &self.gc_heap) {
                Some(proto) => self.ordinary_has_property_value(context, proto, key, hops + 1),
                None => Ok(false),
            };
        }
        if let Some(proxy) = base.as_proxy() {
            let key_value = self.vm_property_key_to_value(key)?;
            let trap_args: SmallVec<[Value; 8]> =
                smallvec::smallvec![proxy.target(&self.gc_heap), key_value];
            return match self.invoke_proxy_trap(context, &proxy, "has", trap_args)? {
                Some(value) => {
                    let result = value.to_boolean(&self.gc_heap);
                    if !result {
                        let target_value = proxy.target(&self.gc_heap);
                        let target_desc = self
                            .ordinary_get_own_property_descriptor_value_runtime_rooted(
                                context,
                                target_value,
                                key,
                                hops + 1,
                                &[&target_value],
                                &[],
                            )?;
                        if let Some(desc) = target_desc {
                            if !desc.configurable() {
                                return Err(VmError::TypeError {
                                    message:
                                        "Proxy has trap returned false but target has the property as non-configurable"
                                            .to_string(),
                                });
                            }
                            let target_extensible =
                                self.is_extensible_value(context, &target_value)?;
                            if !target_extensible {
                                return Err(VmError::TypeError {
                                    message:
                                        "Proxy has trap returned false but target has the property and is non-extensible"
                                            .to_string(),
                                });
                            }
                        }
                    }
                    Ok(result)
                }
                None => self.ordinary_has_property_value(
                    context,
                    proxy.target(&self.gc_heap),
                    key,
                    hops + 1,
                ),
            };
        }
        if let Some(arr) = base.as_array() {
            return match key {
                VmPropertyKey::Symbol(sym)
                    if sym.well_known_tag() == Some(symbol::WellKnown::Iterator) =>
                {
                    Ok(true)
                }
                VmPropertyKey::Symbol(sym) => {
                    if array::get_symbol_property(arr, &self.gc_heap, *sym).is_some()
                        || array::get_symbol_accessor(arr, &self.gc_heap, *sym).is_some()
                    {
                        return Ok(true);
                    }
                    let proto = self.constructor_prototype_value("Array")?;
                    if proto.is_null() {
                        return Ok(false);
                    }
                    self.ordinary_has_property_value(context, proto, key, hops + 1)
                }
                _ if key.string_name().is_some_and(|k| k == "length") => Ok(true),
                _ => {
                    let k = key
                        .string_name()
                        .expect("non-symbol key has string spelling");
                    if let Some(idx) = object::array_index_property_name(k).map(|idx| idx as usize)
                        && array::has_own_element(arr, &self.gc_heap, idx)
                    {
                        return Ok(true);
                    }
                    // §10.4.2.1 [[HasProperty]] over own properties — an
                    // indexed or named accessor installed via
                    // `Object.defineProperty` holes its dense / named data
                    // slot, so it is only visible through the accessor table.
                    if array::get_accessor(arr, &self.gc_heap, k).is_some() {
                        return Ok(true);
                    }
                    if array::get_named_property(arr, &self.gc_heap, k).is_some() {
                        return Ok(true);
                    }
                    let proto = self.constructor_prototype_value("Array")?;
                    if proto.is_null() {
                        return Ok(false);
                    }
                    self.ordinary_has_property_value(context, proto, key, hops + 1)
                }
            };
        }
        if base.is_function()
            || base.is_closure()
            || base.is_bound_function()
            || base.is_native_function()
            || base.is_class_constructor()
            || base.is_regexp()
            || base.is_map()
            || base.is_set()
            || base.is_weak_map()
            || base.is_weak_set()
            || base.is_promise()
            || base.is_array_buffer()
            || base.is_data_view()
            || base.is_weak_ref()
            || base.is_finalization_registry()
        {
            return match self.ordinary_get_value(context, base, base, key, hops + 1)? {
                VmGetOutcome::Value(v) if v.is_undefined() => Ok(false),
                _ => Ok(true),
            };
        }
        // §10.4.5.2 TypedArray [[HasProperty]] — a canonical numeric
        // key answers IsValidIntegerIndex with NO prototype walk;
        // anything else takes OrdinaryHasProperty (own expando, then
        // the real prototype chain, dispatching Proxy `has` traps).
        if let Some(t) = base.as_typed_array(&self.gc_heap) {
            if let Some(name) = key.string_name()
                && let Some(n) = crate::property_dispatch::canonical_numeric_index_string(name)
            {
                return Ok(
                    crate::property_dispatch::typed_array_valid_index(&t, &self.gc_heap, n)
                        .is_some(),
                );
            }
            if let Some(bag) = t.expando(&self.gc_heap) {
                let own = match key {
                    VmPropertyKey::Symbol(sym) => !matches!(
                        object::lookup_own_symbol(bag, &self.gc_heap, *sym),
                        object::PropertyLookup::Absent
                    ),
                    _ => !matches!(
                        object::lookup_own(
                            bag,
                            &self.gc_heap,
                            key.string_name().expect("non-symbol key"),
                        ),
                        object::PropertyLookup::Absent
                    ),
                };
                if own {
                    return Ok(true);
                }
            }
            let proto = self.get_prototype_for_op(&base)?;
            if crate::reflect::is_type_object_value(&proto) {
                return self.ordinary_has_property_value(context, proto, key, hops + 1);
            }
            return Ok(false);
        }
        Err(VmError::TypeMismatch)
    }
    pub(crate) fn try_proxy_object_static_call(
        &mut self,
        context: &ExecutionContext,
        stack_roots: Option<&SmallVec<[Frame; 8]>>,
        method: otter_bytecode::method_id::ObjectMethod,
        args: &[Value],
    ) -> Result<Option<Value>, VmError> {
        use otter_bytecode::method_id::ObjectMethod as M;
        let Some(target) = args.first() else {
            return Ok(None);
        };
        // DefineProperty needs observable ToPropertyDescriptor for
        // every Object target, not only Proxy targets. The rest of the
        // proxy preflight is Proxy-specific.
        if matches!(method, M::DefineProperty) && target.is_object_type() {
            let key =
                self.evaluate_to_property_key(context, args.get(1).unwrap_or(&Value::undefined()))?;
            let attributes = args.get(2).cloned().unwrap_or(Value::undefined());
            let descriptor = self.evaluate_to_property_descriptor(context, &attributes)?;
            let ok = self.define_own_property_value(context, target, &key, descriptor)?;
            if !ok {
                return Err(VmError::TypeError {
                    message: "Cannot define property".to_string(),
                });
            }
            return Ok(Some(*target));
        }
        // Module Namespace Exotic Objects (§10.4.6) define their own
        // [[DefineOwnProperty]] / [[OwnPropertyKeys]], so integrity
        // operations must run the generic §7.3.15 SetIntegrityLevel /
        // §7.3.16 TestIntegrityLevel over those internal methods —
        // Object.freeze on a namespace throws (exports stay writable),
        // while Object.seal succeeds without mutating anything.
        let namespace_integrity = matches!(
            method,
            M::Freeze | M::Seal | M::IsFrozen | M::IsSealed | M::IsExtensible
        ) && target
            .as_object()
            .is_some_and(|obj| crate::object::module_namespace_env(obj, &self.gc_heap).is_some());
        if !target.is_proxy() && !namespace_integrity {
            return Ok(None);
        }
        match method {
            M::Freeze => {
                if !self.set_integrity_level_value(context, target, ObjectIntegrityLevel::Frozen)? {
                    return Err(VmError::TypeError {
                        message: "Object.freeze failed".to_string(),
                    });
                }
                Ok(Some(*target))
            }
            M::Seal => {
                if !self.set_integrity_level_value(context, target, ObjectIntegrityLevel::Sealed)? {
                    return Err(VmError::TypeError {
                        message: "Object.seal failed".to_string(),
                    });
                }
                Ok(Some(*target))
            }
            M::IsFrozen => {
                let frozen =
                    self.test_integrity_level_value(context, target, ObjectIntegrityLevel::Frozen)?;
                Ok(Some(Value::boolean(frozen)))
            }
            M::IsSealed => {
                let sealed =
                    self.test_integrity_level_value(context, target, ObjectIntegrityLevel::Sealed)?;
                Ok(Some(Value::boolean(sealed)))
            }
            M::IsExtensible => {
                let ext = self.is_extensible_value(context, target)?;
                Ok(Some(Value::boolean(ext)))
            }
            M::PreventExtensions => {
                let ok = self.prevent_extensions_value(context, target)?;
                // §20.1.2.10 — Object.preventExtensions throws when the
                // underlying `[[PreventExtensions]]` returns false.
                if !ok {
                    return Err(VmError::TypeError {
                        message: "Object.preventExtensions failed".to_string(),
                    });
                }
                Ok(Some(*target))
            }
            // §20.1.2.4 Object.defineProperty(O, P, Attributes) —
            // handled in the pre-Proxy block above.
            M::DefineProperty => {
                let key = self.evaluate_to_property_key(
                    context,
                    args.get(1).unwrap_or(&Value::undefined()),
                )?;
                let attributes = args.get(2).cloned().unwrap_or(Value::undefined());
                let descriptor = self.evaluate_to_property_descriptor(context, &attributes)?;
                let ok = self.define_own_property_value(context, target, &key, descriptor)?;
                if !ok {
                    return Err(VmError::TypeError {
                        message: "Object.defineProperty failed".to_string(),
                    });
                }
                Ok(Some(*target))
            }
            // §20.1.2.10 Object.getOwnPropertyNames(O) — full string
            // key set (enumerable + non-enumerable) for Proxy targets,
            // validated against §10.5.11 invariants.
            M::GetOwnPropertyNames => {
                let target_clone = *target;
                let trap_keys = self.own_property_keys_value(context, &target_clone)?;
                let values: Vec<Value> = trap_keys.into_iter().filter(|v| v.is_string()).collect();
                let array = match stack_roots {
                    Some(stack) => self.alloc_stack_rooted_array_from_values_with_root_slices(
                        stack,
                        values,
                        &[&target_clone],
                        &[args],
                    )?,
                    None => self.alloc_runtime_rooted_array_from_values(
                        values,
                        &[&target_clone],
                        &[args],
                    )?,
                };
                Ok(Some(Value::array(array)))
            }
            M::GetOwnPropertySymbols => {
                let target_clone = *target;
                let trap_keys = self.own_property_keys_value(context, &target_clone)?;
                let values: Vec<Value> = trap_keys.into_iter().filter(|v| v.is_symbol()).collect();
                let array = match stack_roots {
                    Some(stack) => self.alloc_stack_rooted_array_from_values_with_root_slices(
                        stack,
                        values,
                        &[&target_clone],
                        &[args],
                    )?,
                    None => self.alloc_runtime_rooted_array_from_values(
                        values,
                        &[&target_clone],
                        &[args],
                    )?,
                };
                Ok(Some(Value::array(array)))
            }
            _ => Ok(None),
        }
    }

    pub(crate) fn get_own_property_descriptor_for_value(
        &mut self,
        context: &ExecutionContext,
        target: Value,
        key: Option<&Value>,
    ) -> Result<Option<object::PropertyDescriptor>, VmError> {
        let key = self.to_property_key_sync(context, key.cloned().unwrap_or(Value::undefined()))?;
        self.ordinary_get_own_property_descriptor_value_runtime_rooted(
            context,
            target,
            &key,
            0,
            &[&target],
            &[],
        )
    }

    /// §7.1.19 `ToPropertyKey(value)` — synchronous variant for native
    /// dispatch paths (`hasOwnProperty`, `propertyIsEnumerable`,
    /// `getOwnPropertyDescriptor`, …) that need to coerce a non-
    /// primitive `V` to a property key without the call-frame ladder.
    ///
    /// 1. `key = ? ToPrimitive(V, hint = string)`.
    /// 2. If `key` is a Symbol, return `key`.
    /// 3. Else return `ToString(key)`.
    ///
    /// For objects without `[Symbol.toPrimitive]`, falls back to the
    /// §7.1.1.1 `OrdinaryToPrimitive` `toString`/`valueOf` ladder. The
    /// `@@toPrimitive` trap is invoked synchronously via
    /// [`Self::run_callable_sync`] when present.
    ///
    /// # See also
    /// - <https://tc39.es/ecma262/#sec-topropertykey>
    /// - <https://tc39.es/ecma262/#sec-toprimitive>
    #[allow(clippy::wrong_self_convention)]
    pub(crate) fn to_property_key_sync(
        &mut self,
        context: &ExecutionContext,
        value: Value,
    ) -> Result<VmPropertyKey<'static>, VmError> {
        if abstract_ops::is_primitive(&value) {
            return primitive_to_property_key(value, &self.gc_heap);
        }
        let primitive =
            self.to_primitive_sync(context, value, abstract_ops::ToPrimitiveHint::String)?;
        primitive_to_property_key(primitive, &self.gc_heap)
    }

    /// §7.1.1 `ToPrimitive(value, hint)` — synchronous variant. See
    /// [`Self::to_property_key_sync`] for the rationale.
    #[allow(clippy::wrong_self_convention)]
    pub(crate) fn to_primitive_sync(
        &mut self,
        context: &ExecutionContext,
        value: Value,
        hint: abstract_ops::ToPrimitiveHint,
    ) -> Result<Value, VmError> {
        if abstract_ops::is_primitive(&value) {
            return Ok(value);
        }
        let to_prim_sym = self.well_known_symbols.get(symbol::WellKnown::ToPrimitive);
        let to_prim = value
            .as_object()
            .and_then(|o| crate::object::get_symbol(o, &self.gc_heap, to_prim_sym));
        if let Some(callee) = to_prim
            && self.is_callable_runtime(&callee)
        {
            let hint_str = JsString::from_str(hint.as_token(), &mut self.gc_heap)?;
            let mut args: SmallVec<[Value; 8]> = SmallVec::new();
            args.push(Value::string(hint_str));
            let result = self.run_callable_sync(context, &callee, value, args)?;
            if abstract_ops::is_primitive(&result) {
                return Ok(result);
            }
            return Err(VmError::TypeError {
                message: "Cannot convert object to primitive value".to_string(),
            });
        }
        let order: [&str; 2] = match hint {
            abstract_ops::ToPrimitiveHint::String => ["toString", "valueOf"],
            abstract_ops::ToPrimitiveHint::Number | abstract_ops::ToPrimitiveHint::Default => {
                ["valueOf", "toString"]
            }
        };
        for method in order {
            let callee = self.get_property_value_for_call(context, value, method)?;
            if !self.is_callable_runtime(&callee) {
                continue;
            }
            let result = self.run_callable_sync(context, &callee, value, SmallVec::new())?;
            if abstract_ops::is_primitive(&result) {
                return Ok(result);
            }
        }
        Err(VmError::TypeError {
            message: "Cannot convert object to primitive value".to_string(),
        })
    }

    pub(crate) fn enumerable_own_string_keys_for_value(
        &mut self,
        context: &ExecutionContext,
        target: Value,
        hops: usize,
    ) -> Result<Vec<String>, VmError> {
        if hops >= object::PROTO_CHAIN_HARD_CAP {
            return Ok(Vec::new());
        }
        self.ensure_deferred_namespace_ready(context, &target, true)?;
        if let Some(proxy) = target.as_proxy() {
            let trap_args: SmallVec<[Value; 8]> = smallvec::smallvec![proxy.target(&self.gc_heap)];
            let trap_result = self.invoke_proxy_trap(context, &proxy, "ownKeys", trap_args)?;
            let keys = if let Some(arr) = trap_result.and_then(|v| v.as_array()) {
                crate::array::with_elements(arr, &self.gc_heap, |elements| elements.to_vec())
            } else if let Some(v) = trap_result {
                if v.is_nullish() {
                    return self.enumerable_own_string_keys_for_value(
                        context,
                        proxy.target(&self.gc_heap),
                        hops + 1,
                    );
                }
                return Err(VmError::TypeError {
                    message: "Proxy ownKeys trap returned non-array".to_string(),
                });
            } else {
                return self.enumerable_own_string_keys_for_value(
                    context,
                    proxy.target(&self.gc_heap),
                    hops + 1,
                );
            };
            let mut enumerable = Vec::new();
            for key in &keys {
                let Some(name) = key.as_string(&self.gc_heap) else {
                    continue;
                };
                let name = name.to_lossy_string(&self.gc_heap);
                let proxy_root = Value::proxy(proxy);
                let slice_roots: [&[Value]; 1] = [keys.as_slice()];
                let desc = self.ordinary_get_own_property_descriptor_value_runtime_rooted(
                    context,
                    proxy_root,
                    &VmPropertyKey::OwnedString(name.clone()),
                    hops + 1,
                    &[&proxy_root],
                    &slice_roots,
                )?;
                if desc
                    .as_ref()
                    .is_some_and(object::PropertyDescriptor::enumerable)
                {
                    enumerable.push(name);
                }
            }
            return Ok(enumerable);
        }
        if let Some(obj) = target.as_object() {
            // §10.4.6 namespace enumerable string keys are its resolved
            // exported names (all enumerable). EnumerableOwnProperties
            // (§7.3.23) calls [[GetOwnProperty]] per key, so a name whose
            // binding is still uninitialized surfaces a TDZ ReferenceError
            // here (§10.4.6.5 step 7) rather than being silently listed.
            if object::module_namespace_env(obj, &self.gc_heap).is_some() {
                let names = self.module_namespace_export_names(obj);
                let mut out = Vec::with_capacity(names.len());
                for name in names {
                    let desc = self.ordinary_get_own_property_descriptor_value_runtime_rooted(
                        context,
                        target,
                        &VmPropertyKey::OwnedString(name.clone()),
                        hops + 1,
                        &[&target],
                        &[],
                    )?;
                    if desc.is_some_and(|d| d.enumerable()) {
                        out.push(name);
                    }
                }
                return Ok(out);
            }
            let mut keys = Vec::new();
            if let Some(value) = object::string_data(obj, &self.gc_heap) {
                keys.extend((0..value.len()).map(|idx| idx.to_string()));
            }
            keys.extend(crate::object::with_properties(obj, &self.gc_heap, |p| {
                p.enumerable_keys().map(str::to_string).collect::<Vec<_>>()
            }));
            return Ok(keys);
        }
        if let Some(arr) = target.as_array() {
            let target = Value::array(arr);
            let own_keys = self.own_property_keys_value(context, &target)?;
            let mut out = Vec::new();
            for key_value in own_keys {
                let Some(name) = key_value.as_string(&self.gc_heap) else {
                    continue;
                };
                let key = name.to_lossy_string(&self.gc_heap);
                if let Some(desc) = self.ordinary_get_own_property_descriptor_value_runtime_rooted(
                    context,
                    target,
                    &VmPropertyKey::OwnedString(key.clone()),
                    hops + 1,
                    &[&target],
                    &[],
                )? && desc.enumerable()
                {
                    out.push(key);
                }
            }
            return Ok(out);
        }
        let fid = target.as_function().or_else(|| {
            target
                .as_closure(&self.gc_heap)
                .map(|c| c.cached_function_id)
        });
        if let Some(function_id) = fid {
            let keys = self.ordinary_function_own_property_keys(context, function_id);
            let mut out = Vec::with_capacity(keys.len());
            for key in keys {
                if let Some(desc) = self.ordinary_function_own_property_descriptor(
                    Some(context),
                    function_id,
                    &key,
                )? && desc.enumerable()
                {
                    out.push(key);
                }
            }
            return Ok(out);
        }
        if let Some(native) = target.as_native_function() {
            return Ok(native
                .enumerable_own_property_keys(&self.gc_heap)
                .into_iter()
                .collect());
        }
        if let Some(bound) = target.as_bound_function() {
            return Ok(function_metadata::bound_enumerable_own_property_keys(
                &bound,
                &self.gc_heap,
            )
            .into_iter()
            .collect());
        }
        Ok(Vec::new())
    }

    pub(crate) fn enumerable_for_in_string_keys_for_value(
        &mut self,
        context: &ExecutionContext,
        target: Value,
    ) -> Result<Vec<String>, VmError> {
        if target.is_nullish() {
            return Ok(Vec::new());
        }
        self.ensure_deferred_namespace_ready(context, &target, true)?;

        let mut current = target;
        let mut visited = BTreeSet::new();
        let mut out = Vec::new();

        for hops in 0..object::PROTO_CHAIN_HARD_CAP {
            if current.is_null() {
                break;
            }

            let keys = self.own_property_keys_value(context, &current)?;
            for key in &keys {
                let Some(name) = key.as_string(&self.gc_heap) else {
                    continue;
                };
                let name = name.to_lossy_string(&self.gc_heap);
                if !visited.insert(name.clone()) {
                    continue;
                }

                let key = VmPropertyKey::OwnedString(name.clone());
                let desc = self.ordinary_get_own_property_descriptor_value_runtime_rooted(
                    context,
                    current,
                    &key,
                    hops + 1,
                    &[&current],
                    &[keys.as_slice()],
                )?;
                if desc
                    .as_ref()
                    .is_some_and(object::PropertyDescriptor::enumerable)
                {
                    out.push(name);
                }
            }

            current = self.ordinary_get_prototype_value(context, current, hops + 1)?;
        }

        Ok(out)
    }

    pub(crate) fn ordinary_delete_value(
        &mut self,
        context: &ExecutionContext,
        target: Value,
        key: &VmPropertyKey,
        hops: usize,
    ) -> Result<bool, VmError> {
        if hops >= object::PROTO_CHAIN_HARD_CAP {
            return Ok(true);
        }
        self.ensure_deferred_namespace_ready(
            context,
            &target,
            !Self::deferred_key_is_symbol_like(key),
        )?;
        // §10.4.6.11 [[Delete]] — an exported string name cannot be
        // deleted (returns false); a non-export string succeeds. Symbol
        // keys fall through to the ordinary delete on own properties.
        if let Some(obj) = target.as_object()
            && let Some(env) = object::module_namespace_env(obj, &self.gc_heap)
            && let Some(name) = key.string_name()
        {
            return Ok(object::get(env, &self.gc_heap, name).is_none());
        }
        if let Some(proxy) = target.as_proxy() {
            let key_value = self.vm_property_key_to_value(key)?;
            let trap_args: SmallVec<[Value; 8]> =
                smallvec::smallvec![proxy.target(&self.gc_heap), key_value];
            return match self.invoke_proxy_trap(context, &proxy, "deleteProperty", trap_args)? {
                Some(value) => {
                    let result = value.to_boolean(&self.gc_heap);
                    if !result {
                        return Ok(false);
                    }
                    let target_value = proxy.target(&self.gc_heap);
                    let target_desc = self
                        .ordinary_get_own_property_descriptor_value_runtime_rooted(
                            context,
                            target_value,
                            key,
                            hops + 1,
                            &[&target_value],
                            &[],
                        )?;
                    if let Some(desc) = target_desc {
                        if !desc.configurable() {
                            return Err(VmError::TypeError {
                                message:
                                    "Proxy deleteProperty trap returned true but target has the property as non-configurable"
                                        .to_string(),
                            });
                        }
                        let target_extensible = self.is_extensible_value(context, &target_value)?;
                        if !target_extensible {
                            return Err(VmError::TypeError {
                                message:
                                    "Proxy deleteProperty trap returned true but target is non-extensible"
                                        .to_string(),
                            });
                        }
                    }
                    Ok(true)
                }
                None => {
                    self.ordinary_delete_value(context, proxy.target(&self.gc_heap), key, hops + 1)
                }
            };
        }
        if let Some(obj) = target.as_object() {
            if let Some(desc) = self.string_object_exotic_descriptor(obj, key)?
                && !desc.configurable()
            {
                return Ok(false);
            }
            return Ok(if let Some(key) = key.string_name() {
                object::delete(obj, &mut self.gc_heap, key)
            } else if let VmPropertyKey::Symbol(sym) = key {
                object::delete_symbol(obj, &mut self.gc_heap, *sym)
            } else {
                true
            });
        }
        if let Some(arr) = target.as_array() {
            return Ok(if let VmPropertyKey::Symbol(sym) = key {
                array::delete_symbol_property(arr, &mut self.gc_heap, *sym)
            } else if let Some(k) = key.string_name() {
                array::delete_named_property(arr, &mut self.gc_heap, k)
            } else {
                true
            });
        }
        let fid = target.as_function().or_else(|| {
            target
                .as_closure(&self.gc_heap)
                .map(|c| c.cached_function_id)
        });
        if let Some(function_id) = fid {
            return Ok(if let Some(key) = key.string_name() {
                self.ordinary_function_delete_own_property(function_id, key)
            } else if let VmPropertyKey::Symbol(sym) = key {
                self.function_user_props
                    .get(&function_id)
                    .copied()
                    .map(|bag| object::delete_symbol(bag, &mut self.gc_heap, *sym))
                    .unwrap_or(true)
            } else {
                true
            });
        }
        if let Some(native) = target.as_native_function() {
            return Ok(match key.string_name() {
                Some(key) => native.delete_own_property(&mut self.gc_heap, key),
                None if let VmPropertyKey::Symbol(sym) = key => {
                    native.delete_own_symbol_property(&mut self.gc_heap, *sym)
                }
                None => true,
            });
        }
        if let Some(bound) = target.as_bound_function() {
            return Ok(match key.string_name() {
                Some(key) => {
                    function_metadata::bound_delete_own_property(&bound, &mut self.gc_heap, key)
                }
                None => true,
            });
        }
        if target.is_regexp() {
            return Ok(key.string_name().is_none_or(|key| key != "lastIndex"));
        }
        Ok(true)
    }

    pub(crate) fn ordinary_set_data_value(
        &mut self,
        context: &ExecutionContext,
        target: Value,
        key: &VmPropertyKey,
        value: Value,
        receiver: Value,
        hops: usize,
    ) -> Result<bool, VmError> {
        if hops >= object::PROTO_CHAIN_HARD_CAP {
            return Ok(false);
        }
        self.ensure_deferred_namespace_ready(
            context,
            &target,
            !Self::deferred_key_is_symbol_like(key),
        )?;
        // §10.4.6.9 [[Set]] — a Module Namespace Exotic Object never
        // accepts assignment.
        if let Some(obj) = target.as_object()
            && object::module_namespace_env(obj, &self.gc_heap).is_some()
        {
            return Ok(false);
        }
        // §10.4.5.5 TypedArray exotic [[Set]]. A canonical numeric
        // key with the typed array itself as receiver runs
        // TypedArraySetElement (§10.4.5.16): the value conversion
        // fires even when the index is invalid, and the result is
        // `true` regardless. With a foreign receiver, an invalid
        // index returns `true` without any write; a valid one falls
        // through to ordinary receiver semantics.
        if let Some(t) = target.as_typed_array(&self.gc_heap) {
            match key {
                VmPropertyKey::Symbol(sym) => {
                    let bag = crate::property_dispatch::typed_array_ensure_expando_pub(
                        &mut self.gc_heap,
                        &t,
                    )?;
                    return Ok(object::set_symbol(bag, &mut self.gc_heap, *sym, value));
                }
                _ => {
                    let name = key
                        .string_name()
                        .expect("non-symbol key has string spelling")
                        .to_string();
                    if let Some(n) = crate::property_dispatch::canonical_numeric_index_string(&name)
                    {
                        let same_receiver = receiver
                            .as_typed_array(&self.gc_heap)
                            .is_some_and(|r| r == t);
                        if same_receiver {
                            let coerced =
                                self.typed_array_coerce_element(context, t.kind(), value)?;
                            if let Some(idx) = crate::property_dispatch::typed_array_valid_index(
                                &t,
                                &self.gc_heap,
                                n,
                            ) {
                                t.set(&mut self.gc_heap, idx, &coerced);
                            }
                            return Ok(true);
                        }
                        if crate::property_dispatch::typed_array_valid_index(&t, &self.gc_heap, n)
                            .is_none()
                        {
                            return Ok(true);
                        }
                        // Valid target index + foreign receiver —
                        // §10.1.9.2 receiver phase (GetOwnProperty +
                        // DefineOwnProperty on the receiver, never its
                        // [[Set]]).
                        return self.ordinary_set_on_receiver(context, key, value, &receiver);
                    }
                    let bag = crate::property_dispatch::typed_array_ensure_expando_pub(
                        &mut self.gc_heap,
                        &t,
                    )?;
                    // OrdinarySet on the expando: an own non-writable
                    // data property rejects, an own accessor invokes
                    // its setter (receiver = the typed array), and a
                    // fresh key requires the bag to be extensible.
                    match object::lookup_own(bag, &self.gc_heap, &name) {
                        object::PropertyLookup::Data { flags, .. } => {
                            if !flags.writable() {
                                return Ok(false);
                            }
                            object::set(bag, &mut self.gc_heap, &name, value);
                            return Ok(true);
                        }
                        object::PropertyLookup::Accessor { setter, .. } => {
                            let Some(setter) = setter else {
                                return Ok(false);
                            };
                            let argv: SmallVec<[Value; 8]> = smallvec::smallvec![value];
                            self.run_callable_sync(context, &setter, receiver, argv)?;
                            return Ok(true);
                        }
                        object::PropertyLookup::Absent => {
                            if !object::is_extensible(bag, &self.gc_heap) {
                                return Ok(false);
                            }
                            object::set(bag, &mut self.gc_heap, &name, value);
                            return Ok(true);
                        }
                    }
                }
            }
        }
        if let Some(proxy) = target.as_proxy() {
            if proxy.is_revoked(&self.gc_heap) {
                return Err(VmError::TypeError {
                    message: "Cannot perform 'set' on a proxy that has been revoked".to_string(),
                });
            }
            let key_value = self.vm_property_key_to_value(key)?;
            let trap_args: SmallVec<[Value; 8]> =
                smallvec::smallvec![proxy.target(&self.gc_heap), key_value, value, receiver,];
            return match self.invoke_proxy_trap(context, &proxy, "set", trap_args)? {
                Some(result) => {
                    let ok = result.to_boolean(&self.gc_heap);
                    if !ok {
                        return Ok(false);
                    }
                    let target_value = proxy.target(&self.gc_heap);
                    let target_desc = self
                        .ordinary_get_own_property_descriptor_value_runtime_rooted(
                            context,
                            target_value,
                            key,
                            hops + 1,
                            &[&target_value, &value, &receiver],
                            &[],
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
                                return Err(VmError::TypeError {
                                    message:
                                        "Proxy set trap reported success but target is non-configurable non-writable with a different value"
                                            .to_string(),
                                });
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
                    Ok(true)
                }
                None => self.ordinary_set_data_value(
                    context,
                    proxy.target(&self.gc_heap),
                    key,
                    value,
                    receiver,
                    hops + 1,
                ),
            };
        }
        if let Some(arr) = target.as_array() {
            if let Some(name) = key.string_name() {
                // §10.4.2 arrays inherit OrdinarySet but override
                // [[DefineOwnProperty]]; assigning "length" is therefore an
                // ArraySetLength that coerces the value (twice) and can
                // reject — `set_named_property` would store the raw object
                // and falsely report success.
                if name == "length" {
                    let descriptor = object::PartialPropertyDescriptor {
                        value: Some(value),
                        ..Default::default()
                    };
                    return self.define_own_property_value(context, &target, key, descriptor);
                }
                array::set_named_property(arr, &mut self.gc_heap, name, value)
                    .map_err(|_| VmError::TypeMismatch)?;
            }
            return Ok(true);
        }
        if let Some(obj) = target.as_object() {
            if let Some(desc) = self.string_object_exotic_descriptor(obj, key)?
                && !desc.writable()
            {
                return Ok(false);
            }
            return Ok(if let VmPropertyKey::Symbol(sym) = key {
                object::set_symbol(obj, &mut self.gc_heap, *sym, value)
            } else {
                self.ordinary_set_data_property(
                    obj,
                    key.string_name()
                        .expect("non-symbol key has string spelling"),
                    value,
                )?
            });
        }
        if let Some(re) = target.as_regexp() {
            return match key {
                VmPropertyKey::String(key) if *key == "lastIndex" => {
                    // A non-writable `lastIndex` rejects the write
                    // (returns false → `Set` with Throw=true raises a
                    // TypeError); otherwise store the new value.
                    if !re.last_index_writable(&self.gc_heap) {
                        return Ok(false);
                    }
                    regexp_prototype::store_property(&re, &mut self.gc_heap, key, value);
                    Ok(true)
                }
                _ => Ok(false),
            };
        }
        let fid = target.as_function().or_else(|| {
            target
                .as_closure(&self.gc_heap)
                .map(|c| c.cached_function_id)
        });
        if let Some(function_id) = fid {
            return match key {
                VmPropertyKey::Symbol(sym) => {
                    if !self.ordinary_function_has_own_symbol_property_for_extensibility(
                        function_id,
                        *sym,
                    ) && !self.ordinary_function_is_extensible(function_id)
                    {
                        return Ok(false);
                    }
                    let bag = self.function_user_bag_runtime_rooted(function_id, &[&value], &[])?;
                    Ok(object::set_symbol(bag, &mut self.gc_heap, *sym, value))
                }
                _ => {
                    let key = key
                        .string_name()
                        .expect("non-symbol key has string spelling");
                    let has_own = self
                        .ordinary_function_has_own_string_property_for_extensibility(
                            context,
                            function_id,
                            key,
                        )?;
                    if !has_own && !self.ordinary_function_is_extensible(function_id) {
                        return Ok(false);
                    }
                    let descriptor = match self.ordinary_function_own_property_descriptor(
                        Some(context),
                        function_id,
                        key,
                    )? {
                        Some(existing) if !existing.writable() => return Ok(false),
                        Some(existing) => object::PropertyDescriptor::data(
                            value,
                            true,
                            existing.enumerable(),
                            existing.configurable(),
                        ),
                        None => object::PropertyDescriptor::data(value, true, true, true),
                    };
                    self.ordinary_function_define_own_property(
                        Some(context),
                        function_id,
                        key,
                        None,
                        descriptor,
                    )
                }
            };
        }
        Ok(false)
    }

    /// Walk `base`'s prototype chain and return the first `Proxy`
    /// reached through ordinary objects, or `None` if the chain holds
    /// no proxy. Used by `[[Set]]` to honour §10.1.9.2 step 2.b — when
    /// no ordinary node carries the property, a proxy in the chain
    /// still owns `[[Set]]` (its `set` trap must run). Pure ordinary
    /// `[[GetPrototypeOf]]` links are followed; a proxy node stops the
    /// walk (its own prototype is the proxy's concern).
    pub(crate) fn first_proxy_in_prototype_chain(
        &mut self,
        base: Value,
    ) -> Result<Option<Value>, VmError> {
        let mut current = match base.as_object() {
            Some(obj) => object::prototype_value(obj, &self.gc_heap).unwrap_or(Value::null()),
            None => return Ok(None),
        };
        for _ in 0..object::PROTO_CHAIN_HARD_CAP {
            if current.is_nullish() {
                return Ok(None);
            }
            if current.is_proxy() {
                return Ok(Some(current));
            }
            let Some(obj) = current.as_object() else {
                return Ok(None);
            };
            current = object::prototype_value(obj, &self.gc_heap).unwrap_or(Value::null());
        }
        Ok(None)
    }
}

/// §6.2.5.7 IsCompatiblePropertyDescriptor specialised to a target
/// descriptor and a partial incoming descriptor — without mutation.
/// Returns `true` when applying `incoming` against `target_desc` on
/// an extensible object would succeed under §10.1.6.3.
fn is_compatible_partial_descriptor(
    target_desc: &object::PropertyDescriptor,
    incoming: &object::PartialPropertyDescriptor,
    heap: &otter_gc::GcHeap,
) -> bool {
    let target_is_data = target_desc.is_data();
    if !target_desc.configurable() {
        if matches!(incoming.configurable, Some(true)) {
            return false;
        }
        if let Some(en) = incoming.enumerable
            && en != target_desc.enumerable()
        {
            return false;
        }
        if incoming.is_data() && !target_is_data {
            return false;
        }
        if incoming.is_accessor() && target_is_data {
            return false;
        }
        if target_is_data && incoming.is_data() && !target_desc.writable() {
            if matches!(incoming.writable, Some(true)) {
                return false;
            }
            if let (Some(in_v), object::DescriptorKind::Data { value: ex_v }) =
                (&incoming.value, &target_desc.kind)
                && !abstract_ops::same_value(ex_v, in_v, heap)
            {
                return false;
            }
        }
        if !target_is_data
            && incoming.is_accessor()
            && let object::DescriptorKind::Accessor {
                getter: ex_get,
                setter: ex_set,
            } = &target_desc.kind
        {
            if let Some(g) = &incoming.get {
                let normalised = if g.is_undefined() { None } else { Some(*g) };
                if !optional_value_eq_pair(ex_get, &normalised, heap) {
                    return false;
                }
            }
            if let Some(s) = &incoming.set {
                let normalised = if s.is_undefined() { None } else { Some(*s) };
                if !optional_value_eq_pair(ex_set, &normalised, heap) {
                    return false;
                }
            }
        }
    }
    true
}

fn optional_value_eq_pair(a: &Option<Value>, b: &Option<Value>, heap: &otter_gc::GcHeap) -> bool {
    match (a, b) {
        (None, None) => true,
        (Some(x), Some(y)) => abstract_ops::same_value(x, y, heap),
        _ => false,
    }
}

/// SameValue restricted to PropertyKey-typed values (Strings and
/// Symbols). Used by §10.5.11 Proxy `ownKeys` invariant validation.
fn same_property_key(a: &Value, b: &Value, heap: &otter_gc::GcHeap) -> bool {
    if let (Some(x), Some(y)) = (a.as_string(heap), b.as_string(heap)) {
        return x.to_lossy_string(heap) == y.to_lossy_string(heap);
    }
    if let (Some(x), Some(y)) = (a.as_symbol(heap), b.as_symbol(heap)) {
        return x.ptr_eq(y);
    }
    false
}

/// Convert a PropertyKey-typed [`Value`] (String or Symbol) into a
/// [`VmPropertyKey`]. Caller is responsible for ensuring the value
/// actually holds a PropertyKey-typed entry; anything else is a
/// `TypeMismatch`.
fn property_key_from_value(
    value: &Value,
    heap: &otter_gc::GcHeap,
) -> Result<VmPropertyKey<'static>, VmError> {
    if let Some(s) = value.as_string(heap) {
        return Ok(VmPropertyKey::OwnedString(s.to_lossy_string(heap)));
    }
    if let Some(sym) = value.as_symbol(heap) {
        return Ok(VmPropertyKey::Symbol(sym));
    }
    Err(VmError::TypeMismatch)
}
