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

use smallvec::SmallVec;

use crate::{
    ExecutionContext, Frame, Interpreter, JsObject, JsString, NumberValue, Value, VmError,
    VmGetOutcome, VmPropertyKey, abstract_ops, array, descriptor_value, function_metadata,
    make_array_iterator_factory_runtime_rooted, object, object_statics, proxy, regexp_prototype,
    string, symbol, to_length,
};

/// Convert an already-primitive value to a [`VmPropertyKey`] per
/// §7.1.19 step 2-3: Symbol values pass through unchanged; every
/// other primitive coerces to a UTF-16 string spelling.
fn primitive_to_property_key(value: Value) -> Result<VmPropertyKey<'static>, VmError> {
    match value {
        Value::Symbol(sym) => Ok(VmPropertyKey::Symbol(sym)),
        Value::String(s) => Ok(VmPropertyKey::OwnedString(s.to_lossy_string())),
        Value::Number(n) => Ok(VmPropertyKey::OwnedString(n.to_display_string())),
        Value::Boolean(true) => Ok(VmPropertyKey::String("true")),
        Value::Boolean(false) => Ok(VmPropertyKey::String("false")),
        Value::Null => Ok(VmPropertyKey::String("null")),
        Value::Undefined => Ok(VmPropertyKey::String("undefined")),
        Value::BigInt(b) => Ok(VmPropertyKey::OwnedString(b.to_decimal_string())),
        _ => Err(VmError::TypeMismatch),
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
        roots.push(value.clone());
    }
    if let Some(get) = &descriptor.get {
        roots.push(get.clone());
    }
    if let Some(set) = &descriptor.set {
        roots.push(set.clone());
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
        if proxy.is_revoked() {
            return Err(VmError::TypeMismatch);
        }
        let handler = proxy.handler();
        let trap_fn = match crate::object::get(handler, &self.gc_heap, trap) {
            Some(v) if self.is_callable_runtime(&v) => v,
            Some(Value::Undefined) | Some(Value::Null) | None => return Ok(None),
            _ => return Err(VmError::TypeMismatch),
        };
        let result = self.run_callable_sync(context, &trap_fn, Value::Object(handler), args)?;
        Ok(Some(result))
    }

    pub(crate) fn vm_property_key_to_value(&self, key: &VmPropertyKey) -> Result<Value, VmError> {
        if let Some(key) = key.string_name() {
            Ok(Value::String(JsString::from_str(key, &self.string_heap)?))
        } else if let VmPropertyKey::Symbol(sym) = key {
            Ok(Value::Symbol(sym.clone()))
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
            VmPropertyKey::Symbol(sym) => object::lookup_own_symbol(obj, &self.gc_heap, sym),
            _ => object::lookup_own(
                obj,
                &self.gc_heap,
                key.string_name()
                    .expect("non-symbol key has string spelling"),
            ),
        }
    }

    pub(crate) fn string_object_exotic_get(
        &self,
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
            return Ok(Some(Value::Number(NumberValue::from_i32(
                value.len() as i32
            ))));
        }
        let Ok(index) = key.parse::<u32>() else {
            return Ok(None);
        };
        let Some(unit) = value.char_code_at(index) else {
            return Ok(None);
        };
        Ok(Some(Value::String(JsString::from_utf16_units(
            &[unit],
            &self.string_heap,
        )?)))
    }

    pub(crate) fn string_object_exotic_descriptor(
        &self,
        obj: JsObject,
        key: &VmPropertyKey,
    ) -> Result<Option<object::PropertyDescriptor>, VmError> {
        let Some(value) = object::string_data(obj, &self.gc_heap) else {
            return Ok(None);
        };
        let Some(key) = key.string_name() else {
            return Ok(None);
        };
        if key == "length" {
            return Ok(Some(object::PropertyDescriptor::data(
                Value::Number(NumberValue::from_i32(value.len() as i32)),
                false,
                false,
                false,
            )));
        }
        let Ok(index) = key.parse::<u32>() else {
            return Ok(None);
        };
        let Some(unit) = value.char_code_at(index) else {
            return Ok(None);
        };
        Ok(Some(object::PropertyDescriptor::data(
            Value::String(JsString::from_utf16_units(&[unit], &self.string_heap)?),
            false,
            true,
            false,
        )))
    }

    fn target_is_non_extensible_object(&self, target: &Value) -> bool {
        match target {
            Value::Object(obj) => !object::is_extensible(*obj, &self.gc_heap),
            _ => false,
        }
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
        let Value::Object(obj) = target else {
            return None;
        };
        if let Some(key) = key.string_name() {
            object::get_own_descriptor(*obj, &self.gc_heap, key)
        } else if let VmPropertyKey::Symbol(sym) = key {
            object::get_own_symbol_descriptor(*obj, &self.gc_heap, sym)
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
            object::DescriptorKind::Data { value } if !desc.configurable() && !desc.writable() => {
                if !abstract_ops::same_value(trap_result, &value) {
                    return Err(VmError::TypeError {
                        message: "Proxy get trap returned incompatible value for non-writable non-configurable property".to_string(),
                    });
                }
            }
            object::DescriptorKind::Accessor { getter: None, .. } if !desc.configurable() => {
                if !matches!(trap_result, Value::Undefined) {
                    return Err(VmError::TypeError {
                        message: "Proxy get trap returned value for non-configurable accessor without getter".to_string(),
                    });
                }
            }
            _ => {}
        }
        Ok(())
    }

    pub(crate) fn constructor_prototype_value(
        &self,
        constructor_name: &str,
    ) -> Result<Value, VmError> {
        match object::get(self.global_this, &self.gc_heap, constructor_name) {
            Some(Value::Object(constructor)) => {
                Ok(object::get(constructor, &self.gc_heap, "prototype").unwrap_or(Value::Null))
            }
            Some(Value::NativeFunction(ctor)) => {
                match ctor.own_property_descriptor(&self.gc_heap, &self.string_heap, "prototype") {
                    Ok(Some(descriptor)) => Ok(descriptor_value(&descriptor)),
                    _ => Ok(Value::Null),
                }
            }
            Some(Value::ClassConstructor(class)) => {
                Ok(Value::Object(class.prototype(&self.gc_heap)))
            }
            _ => Err(VmError::InvalidOperand),
        }
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
        match target {
            Value::Proxy(proxy) => {
                let key_value = self.vm_property_key_to_value(key)?;
                let trap_args: SmallVec<[Value; 8]> =
                    smallvec::smallvec![proxy.target(), key_value];
                match self.invoke_proxy_trap(
                    context,
                    &proxy,
                    "getOwnPropertyDescriptor",
                    trap_args,
                )? {
                    Some(Value::Undefined) | Some(Value::Null) => {
                        let target_desc = self
                            .ordinary_get_own_property_descriptor_value_with_roots(
                                context,
                                proxy.target(),
                                key,
                                hops + 1,
                                allocation_roots,
                            )?;
                        self.validate_proxy_get_own_property_descriptor(
                            &proxy.target(),
                            target_desc.as_ref(),
                            None,
                        )?;
                        Ok(None)
                    }
                    Some(Value::Object(desc_obj)) => {
                        let partial =
                            object_statics::coerce_to_descriptor(&desc_obj, &self.gc_heap)?;
                        let desc = partial.complete_for_new_property();
                        let target_desc = self
                            .ordinary_get_own_property_descriptor_value_with_roots(
                                context,
                                proxy.target(),
                                key,
                                hops + 1,
                                allocation_roots,
                            )?;
                        self.validate_proxy_get_own_property_descriptor(
                            &proxy.target(),
                            target_desc.as_ref(),
                            Some(&desc),
                        )?;
                        Ok(Some(desc))
                    }
                    Some(_) => Err(VmError::TypeError {
                        message:
                            "Proxy getOwnPropertyDescriptor trap returned non-object descriptor"
                                .to_string(),
                    }),
                    None => self.ordinary_get_own_property_descriptor_value_with_roots(
                        context,
                        proxy.target(),
                        key,
                        hops + 1,
                        allocation_roots,
                    ),
                }
            }
            Value::Object(obj) => {
                if let Some(desc) = self.string_object_exotic_descriptor(obj, key)? {
                    return Ok(Some(desc));
                }
                Ok(if let Some(key) = key.string_name() {
                    object::get_own_descriptor(obj, &self.gc_heap, key)
                } else if let VmPropertyKey::Symbol(sym) = key {
                    object::get_own_symbol_descriptor(obj, &self.gc_heap, sym)
                } else {
                    None
                })
            }
            Value::Array(arr) => {
                // §10.4.2 — own symbol-keyed properties live in a
                // dedicated side table; surface their data
                // descriptor before the string-keyed paths so
                // `Object.getOwnPropertyDescriptor(arr, sym)` and
                // `hasOwnProperty(sym)` observe the spec shape.
                if let VmPropertyKey::Symbol(sym) = key {
                    if let Some(value) = array::get_symbol_property(arr, &self.gc_heap, sym) {
                        return Ok(Some(object::PropertyDescriptor::data(value, true, true, true)));
                    }
                    return Ok(None);
                }
                let Some(key) = key.string_name() else {
                    return Ok(None);
                };
                if key == "length" {
                    return Ok(Some(object::PropertyDescriptor::data(
                        Value::Number(NumberValue::from_i32(array::len(arr, &self.gc_heap) as i32)),
                        true,
                        false,
                        false,
                    )));
                }
                // §10.4.2 — own accessor installed via
                // `Object.defineProperty` lives in the per-array
                // accessor side-table. Consult it before the
                // dense / named slots so reflective probes
                // (`Object.getOwnPropertyDescriptor(arr, "p")`) see
                // the user-installed getter / setter.
                if let Some((getter, setter)) = array::get_accessor(arr, &self.gc_heap, key) {
                    return Ok(Some(object::PropertyDescriptor::accessor(
                        getter, setter, true, true,
                    )));
                }
                if let Ok(idx) = key.parse::<usize>() {
                    if array::has_own_element(arr, &self.gc_heap, idx) {
                        return Ok(Some(object::PropertyDescriptor::data(
                            array::get(arr, &self.gc_heap, idx),
                            true,
                            true,
                            true,
                        )));
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
                    return Ok(Some(object::PropertyDescriptor::data(
                        value, true, true, true,
                    )));
                }
                Ok(None)
            }
            Value::RegExp(re) => {
                if key.string_name().is_some_and(|key| key == "lastIndex") {
                    return Ok(Some(object::PropertyDescriptor::data(
                        re.last_index_value(&self.gc_heap),
                        true,
                        false,
                        false,
                    )));
                }
                // §22.2.6 — user-installed own properties
                // (`re.foo = 1`) live in the lazy expando bag. Surface
                // their full descriptor so reflective probes
                // (`getOwnPropertyDescriptor`, `hasOwnProperty`,
                // `ToPropertyDescriptor`) see the same shape ordinary
                // objects expose.
                if let Some(bag) = re.expando(&self.gc_heap) {
                    if let Some(key) = key.string_name() {
                        if let Some(desc) = object::get_own_descriptor(bag, &self.gc_heap, key) {
                            return Ok(Some(desc));
                        }
                    } else if let VmPropertyKey::Symbol(sym) = key {
                        if let Some(desc) =
                            object::get_own_symbol_descriptor(bag, &self.gc_heap, sym)
                        {
                            return Ok(Some(desc));
                        }
                    }
                }
                Ok(None)
            }
            Value::Function { function_id } | Value::Closure { function_id, .. } => match key {
                VmPropertyKey::Symbol(sym) => {
                    let Some(bag) = self.function_user_props.get(&function_id).copied() else {
                        return Ok(None);
                    };
                    Ok(object::get_own_symbol_descriptor(bag, &self.gc_heap, sym))
                }
                _ => {
                    let key = key
                        .string_name()
                        .expect("non-symbol key has string spelling");
                    if key == "prototype" {
                        let _ = match allocation_roots {
                            DescriptorAllocationRoots::Runtime {
                                value_roots,
                                slice_roots,
                            } => self.function_property_get_runtime_rooted(
                                context,
                                function_id,
                                "prototype",
                                value_roots,
                                slice_roots,
                            )?,
                            DescriptorAllocationRoots::Stack(stack) => self
                                .function_property_get_stack_rooted(
                                    context,
                                    stack,
                                    function_id,
                                    "prototype",
                                )?,
                        };
                        let Some(bag) = self.function_user_props.get(&function_id).copied() else {
                            return Ok(None);
                        };
                        Ok(object::get_own_descriptor(bag, &self.gc_heap, key))
                    } else {
                        self.ordinary_function_own_property_descriptor(
                            Some(context),
                            function_id,
                            key,
                        )
                    }
                }
            },
            Value::BoundFunction(bound) => {
                let Some(key) = key.string_name() else {
                    return Ok(None);
                };
                function_metadata::bound_own_property_descriptor(
                    &bound,
                    &self.gc_heap,
                    &self.string_heap,
                    key,
                )
            }
            Value::NativeFunction(native) => Ok(match key {
                VmPropertyKey::Symbol(sym) => {
                    native.own_symbol_property_descriptor(&self.gc_heap, sym)
                }
                _ => {
                    let key = key
                        .string_name()
                        .expect("non-symbol key has string spelling");
                    native.own_property_descriptor(&self.gc_heap, &self.string_heap, key)?
                }
            }),
            _ => Ok(None),
        }
    }

    fn proxy_get_prototype_invariant_target_proto(
        &mut self,
        context: &ExecutionContext,
        target: &Value,
    ) -> Result<Option<Value>, VmError> {
        let Value::Object(obj) = target else {
            return Ok(None);
        };
        if object::is_extensible(*obj, &self.gc_heap) {
            return Ok(None);
        }
        Ok(Some(self.ordinary_get_prototype_value(
            context,
            target.clone(),
            0,
        )?))
    }

    pub(crate) fn ordinary_get_prototype_value(
        &mut self,
        context: &ExecutionContext,
        value: Value,
        hops: usize,
    ) -> Result<Value, VmError> {
        if hops >= object::PROTO_CHAIN_HARD_CAP {
            return Ok(Value::Null);
        }
        match value {
            Value::Proxy(proxy) => {
                let trap_args: SmallVec<[Value; 8]> = smallvec::smallvec![proxy.target()];
                match self.invoke_proxy_trap(context, &proxy, "getPrototypeOf", trap_args)? {
                    Some(result) => {
                        if !matches!(result, Value::Object(_) | Value::Proxy(_) | Value::Null) {
                            return Err(VmError::TypeError {
                                message: "Proxy getPrototypeOf trap returned non-object"
                                    .to_string(),
                            });
                        }
                        if let Some(target_proto) = self
                            .proxy_get_prototype_invariant_target_proto(context, &proxy.target())?
                            && !abstract_ops::same_value(&result, &target_proto)
                        {
                            return Err(VmError::TypeError {
                                message:
                                    "Proxy getPrototypeOf trap returned incompatible prototype"
                                        .to_string(),
                            });
                        }
                        Ok(result)
                    }
                    None => self.ordinary_get_prototype_value(context, proxy.target(), hops + 1),
                }
            }
            Value::Object(_)
            | Value::Array(_)
            | Value::NativeFunction(_)
            | Value::Function { .. }
            | Value::Closure { .. }
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
            | Value::TypedArray(_)
            | Value::Iterator(_)
            | Value::Generator(_) => self.get_prototype_for_op(&value),
            _ => Err(VmError::TypeMismatch),
        }
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
        match value {
            Value::Proxy(proxy) => {
                if proxy.is_revoked() {
                    return Err(VmError::TypeError {
                        message: "Cannot perform 'isExtensible' on a proxy that has been revoked"
                            .to_string(),
                    });
                }
                let trap_args: SmallVec<[Value; 8]> = smallvec::smallvec![proxy.target()];
                match self.invoke_proxy_trap(context, proxy, "isExtensible", trap_args)? {
                    Some(result) => {
                        let trap = result.to_boolean();
                        let target_ext = self.is_extensible_value(context, &proxy.target())?;
                        if trap != target_ext {
                            return Err(VmError::TypeError {
                                message:
                                    "Proxy isExtensible trap returned value inconsistent with target"
                                        .to_string(),
                            });
                        }
                        Ok(trap)
                    }
                    None => self.is_extensible_value(context, &proxy.target()),
                }
            }
            Value::Object(obj) => Ok(object::is_extensible(*obj, &self.gc_heap)),
            // Per §10.1.3 every other ordinary heap value is extensible
            // by default. Non-object primitives never reach this path
            // (callers gate via `Type(O) is Object`).
            _ => Ok(true),
        }
    }

    /// §10.5.6 / §10.1.6 — value-level `[[DefineOwnProperty]]`.
    /// Proxies dispatch through the `defineProperty` trap and enforce
    /// the §10.5.6 step 14–18 invariants using the field-presence
    /// information carried by [`object::PartialPropertyDescriptor`].
    pub(crate) fn define_own_property_value(
        &mut self,
        context: &ExecutionContext,
        target: &Value,
        key: &VmPropertyKey,
        descriptor: object::PartialPropertyDescriptor,
    ) -> Result<bool, VmError> {
        match target {
            Value::Proxy(proxy) => {
                if proxy.is_revoked() {
                    return Err(VmError::TypeError {
                        message: "Cannot perform 'defineProperty' on a proxy that has been revoked"
                            .to_string(),
                    });
                }
                let key_value = self.vm_property_key_to_value(key)?;
                let target_value = proxy.target();
                let descriptor_object =
                    self.partial_descriptor_to_object(&descriptor, &[&key_value, &target_value])?;
                let trap_args: SmallVec<[Value; 8]> = smallvec::smallvec![
                    target_value.clone(),
                    key_value,
                    Value::Object(descriptor_object),
                ];
                match self.invoke_proxy_trap(context, proxy, "defineProperty", trap_args)? {
                    Some(result) => {
                        let ok = result.to_boolean();
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
                                target_value.clone(),
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
                                if !target_configurable
                                    && matches!(descriptor.configurable, Some(true))
                                {
                                    return Err(VmError::TypeError {
                                        message:
                                            "Proxy defineProperty trap relaxed a non-configurable target descriptor"
                                                .to_string(),
                                    });
                                }
                                if target_configurable
                                    && matches!(descriptor.configurable, Some(false))
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
                                if !is_compatible_partial_descriptor(target_desc, &descriptor) {
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
                        self.define_own_property_value(context, &proxy.target(), key, descriptor)
                    }
                }
            }
            Value::Object(obj) => Ok(match key {
                VmPropertyKey::Symbol(sym) => object::define_own_symbol_property_partial(
                    *obj,
                    &mut self.gc_heap,
                    sym,
                    descriptor,
                ),
                _ => {
                    let k = key
                        .string_name()
                        .expect("non-symbol key has string spelling");
                    self.define_own_property_partial(*obj, k, descriptor)?
                }
            }),
            Value::NativeFunction(native) => Ok(match key {
                VmPropertyKey::Symbol(sym) => {
                    native.define_own_symbol_property(&mut self.gc_heap, sym, descriptor)
                }
                _ => {
                    let k = key
                        .string_name()
                        .expect("non-symbol key has string spelling");
                    native.define_own_property(
                        &mut self.gc_heap,
                        &self.string_heap,
                        k,
                        descriptor.complete_for_new_property(),
                    )
                }
            }),
            // §10.4.2 ArrayExoticObject [[DefineOwnProperty]] —
            // foundation surface handles indexed writes by routing to
            // dense storage; descriptor attributes are not yet
            // tracked on Array slots, so accessor descriptors reject.
            // Non-indexed string keys fall through to the array's
            // named-property side-table so user-defined own props on
            // arrays (e.g. `arr.foo = 1`) still observe define-success
            // — descriptor attribute enforcement on those slots is
            // tracked separately.
            Value::Array(arr) => {
                // §10.4.2 — symbol-keyed defineProperty on an Array
                // stores into the per-array symbol-property table.
                // Accessor descriptors are currently flattened to
                // data values; full descriptor attributes will land
                // alongside the rest of the array attribute story.
                if let VmPropertyKey::Symbol(sym) = key {
                    let value = descriptor.value.clone().unwrap_or(Value::Undefined);
                    array::set_symbol_property(*arr, &mut self.gc_heap, sym, value);
                    return Ok(true);
                }
                let Some(k) = key.string_name() else {
                    return Ok(false);
                };
                if descriptor.is_accessor() {
                    // §10.4.2.1 Array exotic [[DefineOwnProperty]] —
                    // store accessor descriptor in the per-array
                    // accessor side-table. The dense / named slot is
                    // hidden so subsequent reads invoke the getter.
                    let getter = descriptor.get.clone();
                    let setter = descriptor.set.clone();
                    array::set_accessor(*arr, &mut self.gc_heap, k, getter, setter);
                    return Ok(true);
                }
                if let Ok(idx) = k.parse::<usize>() {
                    let value = descriptor
                        .value
                        .clone()
                        .or_else(|| {
                            array::with_elements(*arr, &self.gc_heap, |elements| {
                                elements.get(idx).cloned()
                            })
                        })
                        .unwrap_or(Value::Undefined);
                    // Defining a data descriptor replaces any prior
                    // accessor at this slot.
                    array::delete_accessor(*arr, &mut self.gc_heap, k);
                    array::set(*arr, &mut self.gc_heap, idx, value)
                        .map_err(|_| VmError::TypeMismatch)?;
                    return Ok(true);
                }
                if k == "length" {
                    // §10.4.2.4 ArraySetLength — when [[Value]] is
                    // present, coerce it through ToUint32 / ToNumber
                    // and resize the dense storage; mismatches surface
                    // as RangeError per step 5. Missing [[Value]] is
                    // currently a no-op since Array length is not yet
                    // configurable / non-writable.
                    if let Some(v) = descriptor.value.clone() {
                        let number_len = crate::coerce::to_number_or_throw(self, context, &v)?;
                        let new_len = crate::number::bitwise::to_uint32(number_len);
                        if (new_len as f64) != number_len.as_f64() {
                            return Err(VmError::RangeError {
                                message: "Invalid array length".to_string(),
                            });
                        }
                        array::set_length(*arr, &mut self.gc_heap, new_len as usize)
                            .map_err(|_| VmError::TypeMismatch)?;
                    }
                    return Ok(true);
                }
                array::delete_accessor(*arr, &mut self.gc_heap, k);
                let value = descriptor.value.clone().unwrap_or(Value::Undefined);
                array::set_named_property(*arr, &mut self.gc_heap, k, value)
                    .map_err(|_| VmError::TypeMismatch)?;
                Ok(true)
            }
            _ => Ok(false),
        }
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
            self.set_property(obj, "value", v.clone())?;
        }
        if let Some(w) = descriptor.writable {
            self.set_property(obj, "writable", Value::Boolean(w))?;
        }
        if let Some(g) = &descriptor.get {
            self.set_property(obj, "get", g.clone())?;
        }
        if let Some(s) = &descriptor.set {
            self.set_property(obj, "set", s.clone())?;
        }
        if let Some(e) = descriptor.enumerable {
            self.set_property(obj, "enumerable", Value::Boolean(e))?;
        }
        if let Some(c) = descriptor.configurable {
            self.set_property(obj, "configurable", Value::Boolean(c))?;
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
            return Ok(input.clone());
        }
        // Step 1.a — try `@@toPrimitive` via OrdinaryGet on the
        // object's prototype chain. Falls back to ordinary toString /
        // valueOf when the exotic hook is absent.
        let to_prim_sym = self.well_known_symbols.get(symbol::WellKnown::ToPrimitive);
        let exotic = match self.ordinary_get_value(
            context,
            input.clone(),
            input.clone(),
            &VmPropertyKey::Symbol(to_prim_sym),
            0,
        )? {
            VmGetOutcome::Value(v) => v,
            VmGetOutcome::InvokeGetter { getter } => {
                let args: SmallVec<[Value; 8]> = SmallVec::new();
                self.run_callable_sync(context, &getter, input.clone(), args)?
            }
        };
        if !matches!(exotic, Value::Undefined | Value::Null) {
            if !self.is_callable_runtime(&exotic) {
                return Err(VmError::TypeError {
                    message: "Symbol.toPrimitive method is not callable".to_string(),
                });
            }
            let hint_str = JsString::from_str(hint.as_token(), &self.string_heap)?;
            let mut args: SmallVec<[Value; 8]> = SmallVec::new();
            args.push(Value::String(hint_str));
            let result = self.run_callable_sync(context, &exotic, input.clone(), args)?;
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
                input.clone(),
                input.clone(),
                &VmPropertyKey::String(name),
                0,
            )? {
                VmGetOutcome::Value(v) => v,
                VmGetOutcome::InvokeGetter { getter } => {
                    let args: SmallVec<[Value; 8]> = SmallVec::new();
                    self.run_callable_sync(context, &getter, input.clone(), args)?
                }
            };
            if !self.is_callable_runtime(&method) {
                continue;
            }
            let args: SmallVec<[Value; 8]> = SmallVec::new();
            let result = self.run_callable_sync(context, &method, input.clone(), args)?;
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
        if !matches!(
            attributes,
            Value::Object(_)
                | Value::Proxy(_)
                | Value::Array(_)
                | Value::Function { .. }
                | Value::Closure { .. }
                | Value::BoundFunction(_)
                | Value::NativeFunction(_)
                | Value::ClassConstructor(_)
                | Value::Map(_)
                | Value::Set(_)
                | Value::WeakMap(_)
                | Value::WeakSet(_)
                | Value::WeakRef(_)
                | Value::FinalizationRegistry(_)
                | Value::RegExp(_)
                | Value::Promise(_)
                | Value::ArrayBuffer(_)
                | Value::DataView(_)
                | Value::TypedArray(_)
        ) {
            return Err(VmError::TypeError {
                message: "ToPropertyDescriptor argument must be an Object".to_string(),
            });
        }

        let read_field = |this: &mut Self, name: &str| -> Result<Option<Value>, VmError> {
            let key = VmPropertyKey::String(name);
            if !this.ordinary_has_property_value(context, attributes.clone(), &key, 0)? {
                return Ok(None);
            }
            let value = match this.ordinary_get_value(
                context,
                attributes.clone(),
                attributes.clone(),
                &key,
                0,
            )? {
                VmGetOutcome::Value(v) => v,
                VmGetOutcome::InvokeGetter { getter } => {
                    let args: SmallVec<[Value; 8]> = SmallVec::new();
                    this.run_callable_sync(context, &getter, attributes.clone(), args)?
                }
            };
            Ok(Some(value))
        };

        let mut descriptor = object::PartialPropertyDescriptor::default();
        // §6.2.5.5 step 3 — enumerable.
        if let Some(v) = read_field(self, "enumerable")? {
            descriptor.enumerable = Some(v.to_boolean());
        }
        // step 4 — configurable.
        if let Some(v) = read_field(self, "configurable")? {
            descriptor.configurable = Some(v.to_boolean());
        }
        // step 5 — value.
        if let Some(v) = read_field(self, "value")? {
            descriptor.value = Some(v);
        }
        // step 6 — writable.
        if let Some(v) = read_field(self, "writable")? {
            descriptor.writable = Some(v.to_boolean());
        }
        // step 7 — get.
        if let Some(v) = read_field(self, "get")? {
            if !matches!(v, Value::Undefined) && !self.is_callable_runtime(&v) {
                return Err(VmError::TypeError {
                    message: "Property descriptor `get` is not callable".to_string(),
                });
            }
            descriptor.get = Some(v);
        }
        // step 8 — set.
        if let Some(v) = read_field(self, "set")? {
            if !matches!(v, Value::Undefined) && !self.is_callable_runtime(&v) {
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
        if let Value::Symbol(sym) = primitive {
            return Ok(VmPropertyKey::Symbol(sym));
        }
        Ok(VmPropertyKey::OwnedString(primitive.display_string()))
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
        string_heap: &string::StringHeap,
    ) -> Result<Vec<Value>, VmError> {
        match target {
            Value::Proxy(proxy) => {
                if proxy.is_revoked() {
                    return Err(VmError::TypeError {
                        message: "Cannot perform 'ownKeys' on a proxy that has been revoked"
                            .to_string(),
                    });
                }
                let trap_args: SmallVec<[Value; 8]> = smallvec::smallvec![proxy.target()];
                match self.invoke_proxy_trap(context, proxy, "ownKeys", trap_args)? {
                    Some(trap_result) => {
                        let trap_keys =
                            self.create_list_from_array_like_property_keys(context, trap_result)?;
                        self.validate_proxy_own_keys(context, proxy, trap_keys, string_heap)
                    }
                    None => self.own_property_keys_value(context, &proxy.target(), string_heap),
                }
            }
            Value::Object(obj) => {
                let keys: Vec<Value> = object::with_properties(*obj, &self.gc_heap, |p| {
                    let mut keys: Vec<Value> = p
                        .keys()
                        .map(|k| {
                            string::JsString::from_str(k, string_heap)
                                .map(Value::String)
                                .unwrap_or(Value::Undefined)
                        })
                        .collect();
                    keys.extend(p.symbol_keys().map(Value::Symbol));
                    keys
                });
                Ok(keys)
            }
            Value::Array(arr) => {
                let len = array::len(*arr, &self.gc_heap);
                let mut keys: Vec<Value> = Vec::with_capacity(len + 2);
                for idx in 0..len {
                    if array::has_own_element(*arr, &self.gc_heap, idx) {
                        let key = idx.to_string();
                        let s =
                            string::JsString::from_str(&key, string_heap).map_err(VmError::from)?;
                        keys.push(Value::String(s));
                    }
                }
                // §10.4.2 Array exotic objects always expose `length`.
                keys.push(Value::String(
                    string::JsString::from_str("length", string_heap).map_err(VmError::from)?,
                ));
                // §10.4.2 — own symbol-keyed properties follow the
                // string keys per §7.3.22 OrdinaryOwnPropertyKeys
                // ordering.
                for sym in array::own_symbol_keys(*arr, &self.gc_heap) {
                    keys.push(Value::Symbol(sym));
                }
                Ok(keys)
            }
            Value::NativeFunction(native) => {
                let names = native.own_property_keys(&self.gc_heap);
                let mut keys: Vec<Value> = Vec::with_capacity(names.len());
                for n in names {
                    let s = string::JsString::from_str(&n, string_heap).map_err(VmError::from)?;
                    keys.push(Value::String(s));
                }
                Ok(keys)
            }
            Value::BoundFunction(bound) => {
                let names = function_metadata::bound_own_property_keys(bound, &self.gc_heap);
                let mut keys: Vec<Value> = Vec::with_capacity(names.len());
                for n in names {
                    let s = string::JsString::from_str(&n, string_heap).map_err(VmError::from)?;
                    keys.push(Value::String(s));
                }
                Ok(keys)
            }
            _ => Ok(Vec::new()),
        }
    }

    /// §7.3.18 CreateListFromArrayLike with elementTypes set to
    /// «String, Symbol» — used by Proxy `ownKeys` trap result
    /// validation per §10.5.11 step 8.
    fn create_list_from_array_like_property_keys(
        &mut self,
        context: &ExecutionContext,
        list_value: Value,
    ) -> Result<Vec<Value>, VmError> {
        if !matches!(
            list_value,
            Value::Object(_) | Value::Array(_) | Value::Proxy(_)
        ) {
            return Err(VmError::TypeError {
                message: "Proxy ownKeys trap result is not an Object".to_string(),
            });
        }
        let len_value = match self.ordinary_get_value(
            context,
            list_value.clone(),
            list_value.clone(),
            &VmPropertyKey::String("length"),
            0,
        )? {
            VmGetOutcome::Value(v) => v,
            VmGetOutcome::InvokeGetter { getter } => {
                let args: SmallVec<[Value; 8]> = SmallVec::new();
                self.run_callable_sync(context, &getter, list_value.clone(), args)?
            }
        };
        let len = to_length(&len_value)?;
        let mut out: Vec<Value> = Vec::with_capacity(len);
        for i in 0..len {
            let key = VmPropertyKey::OwnedString(i.to_string());
            let element = match self.ordinary_get_value(
                context,
                list_value.clone(),
                list_value.clone(),
                &key,
                0,
            )? {
                VmGetOutcome::Value(v) => v,
                VmGetOutcome::InvokeGetter { getter } => {
                    let args: SmallVec<[Value; 8]> = SmallVec::new();
                    self.run_callable_sync(context, &getter, list_value.clone(), args)?
                }
            };
            if !matches!(element, Value::String(_) | Value::Symbol(_)) {
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
        string_heap: &string::StringHeap,
    ) -> Result<Vec<Value>, VmError> {
        // Step 9 — reject duplicates.
        for i in 0..trap_result.len() {
            for j in (i + 1)..trap_result.len() {
                if same_property_key(&trap_result[i], &trap_result[j]) {
                    return Err(VmError::TypeError {
                        message: "Proxy ownKeys trap result contains duplicate entries".to_string(),
                    });
                }
            }
        }
        let target_value = proxy.target();
        let extensible_target = self.is_extensible_value(context, &target_value)?;
        let target_keys = self.own_property_keys_value(context, &target_value, string_heap)?;
        let mut target_configurable: Vec<Value> = Vec::new();
        let mut target_nonconfigurable: Vec<Value> = Vec::new();
        for key in &target_keys {
            let vm_key = property_key_from_value(&key)?;
            let slice_roots: [&[Value]; 4] = [
                target_keys.as_slice(),
                trap_result.as_slice(),
                target_configurable.as_slice(),
                target_nonconfigurable.as_slice(),
            ];
            let desc = self.ordinary_get_own_property_descriptor_value_runtime_rooted(
                context,
                target_value.clone(),
                &vm_key,
                0,
                &[&target_value],
                &slice_roots,
            )?;
            match desc {
                Some(d) if !d.configurable() => target_nonconfigurable.push(key.clone()),
                _ => target_configurable.push(key.clone()),
            }
        }
        if extensible_target && target_nonconfigurable.is_empty() {
            return Ok(trap_result);
        }
        let mut unchecked: Vec<Value> = trap_result.clone();
        for key in &target_nonconfigurable {
            match unchecked.iter().position(|v| same_property_key(v, key)) {
                Some(idx) => {
                    unchecked.swap_remove(idx);
                }
                None => {
                    return Err(VmError::TypeError {
                        message:
                            "Proxy ownKeys trap result omits a non-configurable target own key"
                                .to_string(),
                    });
                }
            }
        }
        if extensible_target {
            return Ok(trap_result);
        }
        for key in &target_configurable {
            match unchecked.iter().position(|v| same_property_key(v, key)) {
                Some(idx) => {
                    unchecked.swap_remove(idx);
                }
                None => {
                    return Err(VmError::TypeError {
                        message:
                            "Proxy ownKeys trap result omits a target own key while target is non-extensible"
                                .to_string(),
                    });
                }
            }
        }
        if !unchecked.is_empty() {
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
        match target {
            Value::Proxy(proxy) => {
                if proxy.is_revoked() {
                    return Err(VmError::TypeError {
                        message: "Cannot perform 'setPrototypeOf' on a proxy that has been revoked"
                            .to_string(),
                    });
                }
                let trap_args: SmallVec<[Value; 8]> =
                    smallvec::smallvec![proxy.target(), proto.clone()];
                match self.invoke_proxy_trap(context, proxy, "setPrototypeOf", trap_args)? {
                    Some(result) => {
                        let ok = result.to_boolean();
                        if !ok {
                            return Ok(false);
                        }
                        // §10.5.7 invariant: when the trap reports
                        // success and the target is non-extensible,
                        // the requested prototype must equal the
                        // target's current prototype.
                        let target_value = proxy.target();
                        let target_extensible = self.is_extensible_value(context, &target_value)?;
                        if !target_extensible {
                            let target_proto =
                                self.ordinary_get_prototype_value(context, target_value, 0)?;
                            if !abstract_ops::same_value(proto, &target_proto) {
                                return Err(VmError::TypeError {
                                    message:
                                        "Proxy setPrototypeOf invariant violated: target is non-extensible and prototypes differ"
                                            .to_string(),
                                });
                            }
                        }
                        Ok(true)
                    }
                    None => self.set_prototype_value_proxy_aware(context, &proxy.target(), proto),
                }
            }
            Value::Object(obj) => {
                // §10.1.2 OrdinarySetPrototypeOf full algorithm.
                let obj = *obj;
                let current_proto =
                    object::prototype_value(obj, &self.gc_heap).unwrap_or(Value::Null);
                if abstract_ops::same_value(proto, &current_proto) {
                    return Ok(true);
                }
                if !object::is_extensible(obj, &self.gc_heap) {
                    return Ok(false);
                }
                // Step 8 cycle check — walk the candidate chain looking
                // for O itself. Only ordinary-object hops; the spec
                // stops when an exotic [[GetPrototypeOf]] is hit.
                let mut p = proto.clone();
                let hard_cap = object::PROTO_CHAIN_HARD_CAP;
                let mut hops = 0;
                loop {
                    match &p {
                        Value::Null => break,
                        Value::Object(candidate) => {
                            if abstract_ops::same_value(
                                &Value::Object(*candidate),
                                &Value::Object(obj),
                            ) {
                                return Ok(false);
                            }
                            if hops >= hard_cap {
                                break;
                            }
                            hops += 1;
                            p = object::prototype_value(*candidate, &self.gc_heap)
                                .unwrap_or(Value::Null);
                        }
                        // Non-ordinary prototype links short-circuit
                        // the cycle walk per §10.1.2 step 8.c.i.
                        _ => break,
                    }
                }
                let proto_opt = match proto {
                    Value::Null => None,
                    v => Some(v.clone()),
                };
                Ok(object::set_prototype_value(
                    obj,
                    &mut self.gc_heap,
                    proto_opt,
                ))
            }
            _ => Ok(true),
        }
    }

    /// §10.5.4 / §10.1.4 — value-level `[[PreventExtensions]]`.
    pub(crate) fn prevent_extensions_value(
        &mut self,
        context: &ExecutionContext,
        value: &Value,
    ) -> Result<bool, VmError> {
        match value {
            Value::Proxy(proxy) => {
                if proxy.is_revoked() {
                    return Err(VmError::TypeError {
                        message:
                            "Cannot perform 'preventExtensions' on a proxy that has been revoked"
                                .to_string(),
                    });
                }
                let trap_args: SmallVec<[Value; 8]> = smallvec::smallvec![proxy.target()];
                match self.invoke_proxy_trap(context, proxy, "preventExtensions", trap_args)? {
                    Some(result) => {
                        let ok = result.to_boolean();
                        if ok && self.is_extensible_value(context, &proxy.target())? {
                            return Err(VmError::TypeError {
                                message:
                                    "Proxy preventExtensions trap succeeded but target is still extensible"
                                        .to_string(),
                            });
                        }
                        Ok(ok)
                    }
                    None => self.prevent_extensions_value(context, &proxy.target()),
                }
            }
            Value::Object(obj) => {
                let heap = &mut self.gc_heap;
                object::prevent_extensions(*obj, heap);
                Ok(true)
            }
            _ => Ok(true),
        }
    }

    pub(crate) fn instanceof_target_prototype(
        &mut self,
        context: &ExecutionContext,
        rhs: &Value,
    ) -> Result<Option<Value>, VmError> {
        match rhs {
            Value::Object(_) | Value::Proxy(_) => {
                let key = VmPropertyKey::String("prototype");
                match self.ordinary_get_value(context, rhs.clone(), rhs.clone(), &key, 0)? {
                    VmGetOutcome::Value(Value::Undefined) => Ok(Some(rhs.clone())),
                    VmGetOutcome::Value(value @ (Value::Object(_) | Value::Proxy(_))) => {
                        Ok(Some(value))
                    }
                    VmGetOutcome::Value(_) => Err(VmError::TypeError {
                        message: "instanceof prototype is not an object".to_string(),
                    }),
                    VmGetOutcome::InvokeGetter { getter } => {
                        let args: SmallVec<[Value; 8]> = SmallVec::new();
                        let value = self.run_callable_sync(context, &getter, rhs.clone(), args)?;
                        if matches!(value, Value::Object(_) | Value::Proxy(_)) {
                            Ok(Some(value))
                        } else {
                            Err(VmError::TypeError {
                                message: "instanceof prototype is not an object".to_string(),
                            })
                        }
                    }
                }
            }
            Value::Function { function_id } | Value::Closure { function_id, .. } => {
                let value = self.function_property_get(context, *function_id, "prototype")?;
                if matches!(value, Value::Object(_) | Value::Proxy(_)) {
                    Ok(Some(value))
                } else {
                    Err(VmError::TypeError {
                        message: "instanceof prototype is not an object".to_string(),
                    })
                }
            }
            Value::ClassConstructor(class) => {
                Ok(Some(Value::Object(class.prototype(&self.gc_heap))))
            }
            Value::NativeFunction(native) => {
                let desc = native
                    .own_property_descriptor(&self.gc_heap, &self.string_heap, "prototype")
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
                            self.run_callable_sync(context, &getter, rhs.clone(), args)?
                        }
                        _ => Value::Undefined,
                    },
                    None => Value::Undefined,
                };
                if matches!(value, Value::Object(_) | Value::Proxy(_)) {
                    Ok(Some(value))
                } else {
                    Err(VmError::TypeError {
                        message: "instanceof prototype is not an object".to_string(),
                    })
                }
            }
            _ => Ok(None),
        }
    }

    pub(crate) fn instanceof_target_prototype_stack_rooted(
        &mut self,
        context: &ExecutionContext,
        stack: &SmallVec<[Frame; 8]>,
        rhs: &Value,
    ) -> Result<Option<Value>, VmError> {
        match rhs {
            Value::Object(_) | Value::Proxy(_) => {
                let key = VmPropertyKey::String("prototype");
                match self.ordinary_get_value(context, rhs.clone(), rhs.clone(), &key, 0)? {
                    VmGetOutcome::Value(Value::Undefined) => Ok(Some(rhs.clone())),
                    VmGetOutcome::Value(value @ (Value::Object(_) | Value::Proxy(_))) => {
                        Ok(Some(value))
                    }
                    VmGetOutcome::Value(_) => Err(VmError::TypeError {
                        message: "instanceof prototype is not an object".to_string(),
                    }),
                    VmGetOutcome::InvokeGetter { getter } => {
                        let args: SmallVec<[Value; 8]> = SmallVec::new();
                        let value = self.run_callable_sync(context, &getter, rhs.clone(), args)?;
                        if matches!(value, Value::Object(_) | Value::Proxy(_)) {
                            Ok(Some(value))
                        } else {
                            Err(VmError::TypeError {
                                message: "instanceof prototype is not an object".to_string(),
                            })
                        }
                    }
                }
            }
            Value::Function { function_id } | Value::Closure { function_id, .. } => {
                let value = self.function_property_get_stack_rooted(
                    context,
                    stack,
                    *function_id,
                    "prototype",
                )?;
                if matches!(value, Value::Object(_) | Value::Proxy(_)) {
                    Ok(Some(value))
                } else {
                    Err(VmError::TypeError {
                        message: "instanceof prototype is not an object".to_string(),
                    })
                }
            }
            Value::ClassConstructor(class) => {
                Ok(Some(Value::Object(class.prototype(&self.gc_heap))))
            }
            Value::NativeFunction(native) => {
                let desc = native
                    .own_property_descriptor(&self.gc_heap, &self.string_heap, "prototype")
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
                            self.run_callable_sync(context, &getter, rhs.clone(), args)?
                        }
                        _ => Value::Undefined,
                    },
                    None => Value::Undefined,
                };
                if matches!(value, Value::Object(_) | Value::Proxy(_)) {
                    Ok(Some(value))
                } else {
                    Err(VmError::TypeError {
                        message: "instanceof prototype is not an object".to_string(),
                    })
                }
            }
            _ => Ok(None),
        }
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
            if matches!(current, Value::Null) {
                return Ok(false);
            }
            if abstract_ops::same_value(&current, target_proto) {
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
            return Ok(VmGetOutcome::Value(Value::Undefined));
        }
        match base {
            Value::Object(obj) => {
                if let Some(value) = self.string_object_exotic_get(obj, key)? {
                    return Ok(VmGetOutcome::Value(value));
                }
                match self.lookup_own_vm_property_key(obj, key) {
                    object::PropertyLookup::Data { value, .. } => Ok(VmGetOutcome::Value(value)),
                    object::PropertyLookup::Accessor { getter, .. } => match getter {
                        Some(getter) if abstract_ops::is_callable(&getter) => {
                            Ok(VmGetOutcome::InvokeGetter { getter })
                        }
                        _ => Ok(VmGetOutcome::Value(Value::Undefined)),
                    },
                    object::PropertyLookup::Absent => {
                        match object::prototype_value(obj, &self.gc_heap) {
                            Some(proto) => {
                                self.ordinary_get_value(context, proto, receiver, key, hops + 1)
                            }
                            None => Ok(VmGetOutcome::Value(Value::Undefined)),
                        }
                    }
                }
            }
            Value::Proxy(proxy) => {
                let key_value = self.vm_property_key_to_value(key)?;
                let trap_args: SmallVec<[Value; 8]> =
                    smallvec::smallvec![proxy.target(), key_value, receiver.clone()];
                match self.invoke_proxy_trap(context, &proxy, "get", trap_args)? {
                    Some(value) => {
                        self.validate_proxy_get_invariants(&proxy.target(), key, &value)?;
                        Ok(VmGetOutcome::Value(value))
                    }
                    None => {
                        self.ordinary_get_value(context, proxy.target(), receiver, key, hops + 1)
                    }
                }
            }
            Value::Array(arr) => {
                let value = match key {
                    VmPropertyKey::Symbol(sym) => {
                        // §22.1 Array exotic — own symbol-keyed slot
                        // wins over the prototype walk, matching the
                        // `OrdinaryGet` ladder for ordinary objects.
                        if let Some(v) = crate::array::get_symbol_property(arr, &self.gc_heap, sym) {
                            v
                        } else if sym
                            .well_known_tag()
                            .is_some_and(|t| t == symbol::WellKnown::Iterator)
                        {
                            make_array_iterator_factory_runtime_rooted(self, arr)?
                        } else {
                            // §22.1.3 — walk Array.prototype for
                            // inherited symbol-keyed members
                            // (`@@toStringTag` accessor, etc.).
                            let proto = self.constructor_prototype_value("Array")?;
                            match proto {
                                Value::Object(p) => {
                                    return self.ordinary_get_value(
                                        context,
                                        Value::Object(p),
                                        receiver,
                                        key,
                                        hops + 1,
                                    );
                                }
                                _ => Value::Undefined,
                            }
                        }
                    }
                    _ => {
                        let key_str = key
                            .string_name()
                            .expect("non-symbol key has string spelling");
                        // §10.4.2 — installed accessor descriptors
                        // take precedence over dense / named data.
                        if let Some((getter, _)) =
                            crate::array::get_accessor(arr, &self.gc_heap, key_str)
                        {
                            match getter {
                                Some(callable) if abstract_ops::is_callable(&callable) => {
                                    return Ok(VmGetOutcome::InvokeGetter { getter: callable });
                                }
                                _ => return Ok(VmGetOutcome::Value(Value::Undefined)),
                            }
                        }
                        match crate::array::get_named_property(arr, &self.gc_heap, key_str) {
                            Some(v) => v,
                            None => {
                                // §22.1.3 — fall through to
                                // `Array.prototype` so inherited
                                // methods (`toString`, `join`,
                                // `map`, …) and user-installed
                                // overrides resolve through ordinary
                                // [[Get]]. Returning `Undefined`
                                // here previously broke the
                                // §7.1.1 ToPrimitive ladder for
                                // plain arrays.
                                // <https://tc39.es/ecma262/#sec-properties-of-the-array-prototype-object>
                                let proto = self.constructor_prototype_value("Array")?;
                                if let Value::Object(proto_obj) = proto {
                                    return self.ordinary_get_value(
                                        context,
                                        Value::Object(proto_obj),
                                        receiver,
                                        key,
                                        hops + 1,
                                    );
                                }
                                Value::Undefined
                            }
                        }
                    }
                };
                Ok(VmGetOutcome::Value(value))
            }
            Value::Function { function_id } | Value::Closure { function_id, .. } => {
                let value = match key {
                    VmPropertyKey::Symbol(sym) => {
                        // §10.2 ordinary function exotic — own
                        // symbol-keyed properties live in the lazy
                        // user-props bag (`f[Symbol.toStringTag] =
                        // "tag"`). Surface those before walking
                        // `Function.prototype` so reflective probes
                        // (`Object.prototype.toString.call(f)`)
                        // observe the override.
                        let own_symbol = self
                            .function_user_props
                            .get(&function_id)
                            .copied()
                            .and_then(|bag| object::get_symbol(bag, &self.gc_heap, sym));
                        match own_symbol {
                            Some(v) => v,
                            None => self
                                .function_prototype_object()
                                .ok()
                                .and_then(|p| object::get_symbol(p, &self.gc_heap, sym))
                                .unwrap_or(Value::Undefined),
                        }
                    }
                    _ => self.function_property_get(
                        context,
                        function_id,
                        key.string_name()
                            .expect("non-symbol key has string spelling"),
                    )?,
                };
                if let Some(outcome) =
                    self.callable_realm_prototype_accessor_outcome(&value, key)?
                {
                    return Ok(outcome);
                }
                Ok(VmGetOutcome::Value(value))
            }
            Value::NativeFunction(native) => {
                let value = match key {
                    VmPropertyKey::Symbol(sym) => {
                        match native.own_symbol_property_descriptor(&self.gc_heap, sym) {
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
                                    _ => VmGetOutcome::Value(Value::Undefined),
                                });
                            }
                            None => self
                                .function_prototype_object()
                                .ok()
                                .and_then(|p| object::get_symbol(p, &self.gc_heap, sym))
                                .unwrap_or(Value::Undefined),
                        }
                    }
                    _ if key
                        .string_name()
                        .is_some_and(|key| key == "name" || key == "length") =>
                    {
                        let key = key.string_name().expect("guard checked string key");
                        let ctx = function_metadata::FunctionMetadataContext::new(
                            context,
                            &self.gc_heap,
                            &self.string_heap,
                            &self.function_user_props,
                            &self.function_deleted_metadata,
                        );
                        function_metadata::callable_intrinsic_property(
                            &ctx,
                            &Value::NativeFunction(native),
                            key,
                        )?
                    }
                    _ => {
                        let key = key
                            .string_name()
                            .expect("non-symbol key has string spelling");
                        self.load_function_prototype_method(key)
                            .or_else(|| self.load_object_prototype_method(key))
                            .unwrap_or(Value::Undefined)
                    }
                };
                if let Some(outcome) =
                    self.callable_realm_prototype_accessor_outcome(&value, key)?
                {
                    return Ok(outcome);
                }
                Ok(VmGetOutcome::Value(value))
            }
            Value::BoundFunction(bound) => {
                let value = match key {
                    VmPropertyKey::Symbol(sym) => self
                        .function_prototype_object()
                        .ok()
                        .and_then(|p| object::get_symbol(p, &self.gc_heap, sym))
                        .unwrap_or(Value::Undefined),
                    _ => {
                        let key = key
                            .string_name()
                            .expect("non-symbol key has string spelling");
                        match function_metadata::bound_own_property_descriptor(
                            &bound,
                            &self.gc_heap,
                            &self.string_heap,
                            key,
                        )? {
                            Some(desc) => descriptor_value(&desc),
                            None => self
                                .load_function_prototype_method(key)
                                .or_else(|| self.load_object_prototype_method(key))
                                .unwrap_or(Value::Undefined),
                        }
                    }
                };
                if let Some(outcome) =
                    self.callable_realm_prototype_accessor_outcome(&value, key)?
                {
                    return Ok(outcome);
                }
                Ok(VmGetOutcome::Value(value))
            }
            Value::ClassConstructor(class) => {
                let value = match key {
                    VmPropertyKey::Symbol(sym) => {
                        object::get_symbol(class.statics(&self.gc_heap), &self.gc_heap, sym)
                            .unwrap_or(Value::Undefined)
                    }
                    _ if key.string_name().is_some_and(|key| key == "prototype") => {
                        Value::Object(class.prototype(&self.gc_heap))
                    }
                    _ => object::get(
                        class.statics(&self.gc_heap),
                        &self.gc_heap,
                        key.string_name()
                            .expect("non-symbol key has string spelling"),
                    )
                    .unwrap_or(Value::Undefined),
                };
                if let Some(outcome) =
                    self.callable_realm_prototype_accessor_outcome(&value, key)?
                {
                    return Ok(outcome);
                }
                Ok(VmGetOutcome::Value(value))
            }
            Value::RegExp(re) => {
                // §22.2.6 — user-installed own properties on a
                // `RegExp` instance live in the lazy expando bag and
                // shadow the spec-mandated accessors. Mirror the same
                // precedence the bytecode property-dispatch path uses
                // (property_dispatch::load_property RegExp arm) so
                // reflective entry points
                // (`ToPropertyDescriptor`, accessor-aware `[[Get]]`)
                // observe the same value the VM hands out.
                if let Some(bag) = re.expando(&self.gc_heap) {
                    let lookup = match key {
                        VmPropertyKey::Symbol(sym) => {
                            object::lookup_own_symbol(bag, &self.gc_heap, sym)
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
                                _ => VmGetOutcome::Value(Value::Undefined),
                            });
                        }
                        object::PropertyLookup::Absent => {}
                    }
                }
                let direct = match key {
                    VmPropertyKey::Symbol(_) => Value::Undefined,
                    _ => {
                        let key = key
                            .string_name()
                            .expect("non-symbol key has string spelling");
                        regexp_prototype::load_property(&re, &self.gc_heap, key, &self.string_heap)
                    }
                };
                match direct {
                    Value::Undefined => {
                        // §22.2.6 — walk `RegExp.prototype` so
                        // installed methods and accessors resolve.
                        let proto = self.constructor_prototype_value("RegExp")?;
                        if matches!(proto, Value::Null | Value::Undefined) {
                            return Ok(VmGetOutcome::Value(Value::Undefined));
                        }
                        self.ordinary_get_value(context, proto, receiver, key, hops + 1)
                    }
                    value => Ok(VmGetOutcome::Value(value)),
                }
            }
            // §24.* — collection instances have no own string keys
            // outside `size`-style accessors that live on the
            // prototype. Walk the realm prototype so user-installed
            // overrides on `Map.prototype` / `Set.prototype` / etc.
            // resolve through the same internal-method substrate that
            // Reflect/Proxy use.
            Value::Map(_) | Value::Set(_) | Value::WeakMap(_) | Value::WeakSet(_) => {
                let proto_name = match base {
                    Value::Map(_) => "Map",
                    Value::Set(_) => "Set",
                    Value::WeakMap(_) => "WeakMap",
                    Value::WeakSet(_) => "WeakSet",
                    _ => unreachable!(),
                };
                let proto = self.constructor_prototype_value(proto_name)?;
                if matches!(proto, Value::Null | Value::Undefined) {
                    return Ok(VmGetOutcome::Value(Value::Undefined));
                }
                self.ordinary_get_value(context, proto, receiver, key, hops + 1)
            }
            // §27.2.5 — Promise instances expose no own string keys.
            // Walk `Promise.prototype` so `then` / `catch` /
            // `finally` / `constructor` resolve through the same
            // internal-method substrate as other builtins.
            Value::Promise(_) => {
                let proto = self.constructor_prototype_value("Promise")?;
                if matches!(proto, Value::Null | Value::Undefined) {
                    return Ok(VmGetOutcome::Value(Value::Undefined));
                }
                self.ordinary_get_value(context, proto, receiver, key, hops + 1)
            }
            // §21.2.5 — BigInt primitive values walk
            // `BigInt.prototype` for `toString` / `valueOf` /
            // `constructor`.
            Value::BigInt(_) => {
                let proto = self.constructor_prototype_value("BigInt")?;
                if matches!(proto, Value::Null | Value::Undefined) {
                    return Ok(VmGetOutcome::Value(Value::Undefined));
                }
                self.ordinary_get_value(context, proto, receiver, key, hops + 1)
            }
            // §7.1.18 ToObject — primitive Boolean / Number / Symbol
            // receivers walk the matching wrapper prototype so
            // inherited `Object.prototype.*` methods surface for
            // direct property reads (`(true).toLocaleString`,
            // `(1).hasOwnProperty`, …). Strings have a richer custom
            // path (indexed chars + `length`) higher up.
            Value::Boolean(_) | Value::Number(_) | Value::Symbol(_) => {
                let proto_name = match base {
                    Value::Boolean(_) => "Boolean",
                    Value::Number(_) => "Number",
                    Value::Symbol(_) => "Symbol",
                    _ => unreachable!(),
                };
                let proto = self.constructor_prototype_value(proto_name)?;
                if matches!(proto, Value::Null | Value::Undefined) {
                    return Ok(VmGetOutcome::Value(Value::Undefined));
                }
                self.ordinary_get_value(context, proto, receiver, key, hops + 1)
            }
            // §26.1.4 / §26.2.4 — walk the realm prototype for
            // `WeakRef` / `FinalizationRegistry` instances.
            Value::WeakRef(_) | Value::FinalizationRegistry(_) => {
                let proto_name = match base {
                    Value::WeakRef(_) => "WeakRef",
                    Value::FinalizationRegistry(_) => "FinalizationRegistry",
                    _ => unreachable!(),
                };
                let proto = self.constructor_prototype_value(proto_name)?;
                if matches!(proto, Value::Null | Value::Undefined) {
                    return Ok(VmGetOutcome::Value(Value::Undefined));
                }
                self.ordinary_get_value(context, proto, receiver, key, hops + 1)
            }
            // ArrayBuffer / DataView — walk realm prototypes for
            // instance method lookups.
            Value::ArrayBuffer(_) | Value::DataView(_) => {
                let proto_name = match base {
                    Value::ArrayBuffer(buf) if buf.is_shared() => "SharedArrayBuffer",
                    Value::ArrayBuffer(_) => "ArrayBuffer",
                    Value::DataView(_) => "DataView",
                    _ => unreachable!(),
                };
                let proto = self.constructor_prototype_value(proto_name)?;
                if matches!(proto, Value::Null | Value::Undefined) {
                    return Ok(VmGetOutcome::Value(Value::Undefined));
                }
                self.ordinary_get_value(context, proto, receiver, key, hops + 1)
            }
            // §27.5 Generator / §27.1 Iterator — walk the realm
            // prototype so `next` / `return` / `throw` / `toString`
            // / `@@toStringTag` / `@@toPrimitive` resolve, and so
            // the value flows through `ToPrimitive` (and therefore
            // `ToPropertyKey`) without tripping `TypeMismatch` on
            // the catch-all arm. Generator instances expose no own
            // string keys.
            Value::Generator(_) | Value::Iterator(_) => {
                let proto = self.get_prototype_for_op(&base)?;
                if matches!(proto, Value::Null | Value::Undefined) {
                    return Ok(VmGetOutcome::Value(Value::Undefined));
                }
                self.ordinary_get_value(context, proto, receiver, key, hops + 1)
            }
            // §10.4.5.4 IntegerIndexedExoticObject [[Get]] —
            // canonical-numeric-index short-circuit, then the lazy
            // expando bag, then the per-kind prototype chain.
            Value::TypedArray(t) => {
                if let Some(name) = key.string_name() {
                    if let Some(n) = crate::property_dispatch::canonical_numeric_index_string(name)
                    {
                        if t.buffer().is_detached()
                            || !n.is_finite()
                            || n.fract() != 0.0
                            || n < 0.0
                            || (n as usize) >= t.length()
                        {
                            return Ok(VmGetOutcome::Value(Value::Undefined));
                        }
                        return Ok(VmGetOutcome::Value(t.get(n as usize)));
                    }
                    if let Some(bag) = t.expando()
                        && let Some(v) = crate::object::get(bag, &self.gc_heap, name)
                    {
                        return Ok(VmGetOutcome::Value(v));
                    }
                }
                if let VmPropertyKey::Symbol(sym) = key
                    && let Some(bag) = t.expando()
                    && let Some(v) = crate::object::get_symbol(bag, &self.gc_heap, sym)
                {
                    return Ok(VmGetOutcome::Value(v));
                }
                let proto = self.constructor_prototype_value(t.kind().name())?;
                if matches!(proto, Value::Null | Value::Undefined) {
                    return Ok(VmGetOutcome::Value(Value::Undefined));
                }
                self.ordinary_get_value(context, proto, receiver, key, hops + 1)
            }
            _ => Err(VmError::TypeMismatch),
        }
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
        match base {
            Value::Object(obj) => {
                if !matches!(
                    self.lookup_own_vm_property_key(obj, key),
                    object::PropertyLookup::Absent
                ) {
                    return Ok(true);
                }
                match object::prototype_value(obj, &self.gc_heap) {
                    Some(proto) => self.ordinary_has_property_value(context, proto, key, hops + 1),
                    None => Ok(false),
                }
            }
            Value::Proxy(proxy) => {
                let key_value = self.vm_property_key_to_value(key)?;
                let trap_args: SmallVec<[Value; 8]> =
                    smallvec::smallvec![proxy.target(), key_value];
                match self.invoke_proxy_trap(context, &proxy, "has", trap_args)? {
                    Some(value) => {
                        let result = value.to_boolean();
                        // §10.5.8 invariants — when the trap reports
                        // false, the target must not have the
                        // property as a non-configurable own property
                        // or be non-extensible while the property
                        // exists.
                        if !result {
                            let target_value = proxy.target();
                            let target_desc = self
                                .ordinary_get_own_property_descriptor_value_runtime_rooted(
                                    context,
                                    target_value.clone(),
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
                    None => {
                        self.ordinary_has_property_value(context, proxy.target(), key, hops + 1)
                    }
                }
            }
            // Other heap-allocated value kinds: probe own keys plus the
            // implied prototype so nested-Proxy fall-through reaches
            // the underlying spec behaviour.
            Value::Array(arr) => match key {
                VmPropertyKey::Symbol(sym)
                    if sym.well_known_tag() == Some(symbol::WellKnown::Iterator) =>
                {
                    Ok(true)
                }
                VmPropertyKey::Symbol(sym) => {
                    // Own symbol-keyed slot → present; otherwise
                    // walk Array.prototype so inherited symbol keys
                    // resolve (`@@toStringTag` accessor, etc.).
                    if array::get_symbol_property(arr, &self.gc_heap, sym).is_some() {
                        return Ok(true);
                    }
                    let proto = self.constructor_prototype_value("Array")?;
                    if matches!(proto, Value::Null) {
                        return Ok(false);
                    }
                    self.ordinary_has_property_value(context, proto, key, hops + 1)
                }
                _ if key.string_name().is_some_and(|k| k == "length") => Ok(true),
                _ => {
                    let k = key
                        .string_name()
                        .expect("non-symbol key has string spelling");
                    if let Ok(idx) = k.parse::<usize>()
                        && array::has_own_element(arr, &self.gc_heap, idx)
                    {
                        return Ok(true);
                    }
                    // §22.1.4 — Array exotic objects expose
                    // user-installed extra string-keyed properties
                    // through the named-properties side table.
                    // `HasProperty` must consult it before walking
                    // the prototype chain so e.g. `'value' in
                    // arr_with_value_named_prop` returns true.
                    if array::get_named_property(arr, &self.gc_heap, k).is_some() {
                        return Ok(true);
                    }
                    // Walk Array.prototype chain.
                    let proto = self.constructor_prototype_value("Array")?;
                    if matches!(proto, Value::Null) {
                        return Ok(false);
                    }
                    self.ordinary_has_property_value(context, proto, key, hops + 1)
                }
            },
            Value::Function { .. }
            | Value::Closure { .. }
            | Value::BoundFunction(_)
            | Value::NativeFunction(_)
            | Value::ClassConstructor(_)
            // §22.2.6 / §24.* / §27.2.5 — exotic objects whose own
            // string-keyed surface lives on the prototype. Probing
            // via Get keeps `HasProperty` consistent with the
            // `[[GetOwnProperty]] + walk-prototype` ladder these
            // value kinds expose elsewhere.
            | Value::RegExp(_)
            | Value::Map(_)
            | Value::Set(_)
            | Value::WeakMap(_)
            | Value::WeakSet(_)
            | Value::Promise(_)
            | Value::ArrayBuffer(_)
            | Value::DataView(_)
            | Value::TypedArray(_)
            | Value::WeakRef(_)
            | Value::FinalizationRegistry(_) => {
                match self.ordinary_get_value(context, base.clone(), base, key, hops + 1)? {
                    VmGetOutcome::Value(Value::Undefined) => Ok(false),
                    _ => Ok(true),
                }
            }
            _ => Err(VmError::TypeMismatch),
        }
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
        if matches!(method, M::DefineProperty)
            && matches!(
                target,
                Value::Object(_)
                    | Value::Proxy(_)
                    | Value::Array(_)
                    | Value::Function { .. }
                    | Value::Closure { .. }
                    | Value::BoundFunction(_)
            )
        {
            let key =
                self.evaluate_to_property_key(context, args.get(1).unwrap_or(&Value::Undefined))?;
            let attributes = args.get(2).cloned().unwrap_or(Value::Undefined);
            let descriptor = self.evaluate_to_property_descriptor(context, &attributes)?;
            let ok = self.define_own_property_value(context, target, &key, descriptor)?;
            if !ok {
                return Err(VmError::TypeError {
                    message: "Object.defineProperty failed".to_string(),
                });
            }
            return Ok(Some(target.clone()));
        }
        if !matches!(target, Value::Proxy(_)) {
            return Ok(None);
        }
        match method {
            M::IsExtensible => {
                let ext = self.is_extensible_value(context, target)?;
                Ok(Some(Value::Boolean(ext)))
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
                Ok(Some(target.clone()))
            }
            // §20.1.2.4 Object.defineProperty(O, P, Attributes) —
            // handled in the pre-Proxy block above.
            M::DefineProperty => {
                let key = self
                    .evaluate_to_property_key(context, args.get(1).unwrap_or(&Value::Undefined))?;
                let attributes = args.get(2).cloned().unwrap_or(Value::Undefined);
                let descriptor = self.evaluate_to_property_descriptor(context, &attributes)?;
                let ok = self.define_own_property_value(context, target, &key, descriptor)?;
                if !ok {
                    return Err(VmError::TypeError {
                        message: "Object.defineProperty failed".to_string(),
                    });
                }
                Ok(Some(target.clone()))
            }
            // §20.1.2.10 Object.getOwnPropertyNames(O) — full string
            // key set (enumerable + non-enumerable) for Proxy targets,
            // validated against §10.5.11 invariants.
            M::GetOwnPropertyNames => {
                let string_heap = self.string_heap.clone();
                let target_clone = target.clone();
                let trap_keys =
                    self.own_property_keys_value(context, &target_clone, &string_heap)?;
                let values: Vec<Value> = trap_keys
                    .into_iter()
                    .filter(|v| matches!(v, Value::String(_)))
                    .collect();
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
                Ok(Some(Value::Array(array)))
            }
            M::GetOwnPropertySymbols => {
                let string_heap = self.string_heap.clone();
                let target_clone = target.clone();
                let trap_keys =
                    self.own_property_keys_value(context, &target_clone, &string_heap)?;
                let values: Vec<Value> = trap_keys
                    .into_iter()
                    .filter(|v| matches!(v, Value::Symbol(_)))
                    .collect();
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
                Ok(Some(Value::Array(array)))
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
        let key = self.to_property_key_sync(context, key.cloned().unwrap_or(Value::Undefined))?;
        self.ordinary_get_own_property_descriptor_value_runtime_rooted(
            context,
            target.clone(),
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
    pub(crate) fn to_property_key_sync(
        &mut self,
        context: &ExecutionContext,
        value: Value,
    ) -> Result<VmPropertyKey<'static>, VmError> {
        if abstract_ops::is_primitive(&value) {
            return primitive_to_property_key(value);
        }
        let primitive =
            self.to_primitive_sync(context, value, abstract_ops::ToPrimitiveHint::String)?;
        primitive_to_property_key(primitive)
    }

    /// §7.1.1 `ToPrimitive(value, hint)` — synchronous variant. See
    /// [`Self::to_property_key_sync`] for the rationale.
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
        let to_prim = match &value {
            Value::Object(o) => crate::object::get_symbol(*o, &self.gc_heap, &to_prim_sym),
            _ => None,
        };
        if let Some(callee) = to_prim
            && self.is_callable_runtime(&callee)
        {
            let hint_str = JsString::from_str(hint.as_token(), &self.string_heap)?;
            let mut args: SmallVec<[Value; 8]> = SmallVec::new();
            args.push(Value::String(hint_str));
            let result = self.run_callable_sync(context, &callee, value.clone(), args)?;
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
            let callee = self.get_property_value_for_call(context, value.clone(), method)?;
            if !self.is_callable_runtime(&callee) {
                continue;
            }
            let result =
                self.run_callable_sync(context, &callee, value.clone(), SmallVec::new())?;
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
        match target {
            Value::Proxy(proxy) => {
                let trap_args: SmallVec<[Value; 8]> = smallvec::smallvec![proxy.target()];
                let keys = match self.invoke_proxy_trap(context, &proxy, "ownKeys", trap_args)? {
                    Some(Value::Array(arr)) => {
                        crate::array::with_elements(arr, &self.gc_heap, |elements| {
                            elements.to_vec()
                        })
                    }
                    Some(Value::Undefined) | Some(Value::Null) | None => {
                        return self.enumerable_own_string_keys_for_value(
                            context,
                            proxy.target(),
                            hops + 1,
                        );
                    }
                    Some(_) => {
                        return Err(VmError::TypeError {
                            message: "Proxy ownKeys trap returned non-array".to_string(),
                        });
                    }
                };
                let mut enumerable = Vec::new();
                for key in &keys {
                    let Value::String(name) = key else {
                        continue;
                    };
                    let name = name.to_lossy_string();
                    let proxy_root = Value::Proxy(proxy.clone());
                    let slice_roots: [&[Value]; 1] = [keys.as_slice()];
                    let desc = self.ordinary_get_own_property_descriptor_value_runtime_rooted(
                        context,
                        proxy_root.clone(),
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
                Ok(enumerable)
            }
            Value::Object(obj) => {
                let mut keys = Vec::new();
                if let Some(value) = object::string_data(obj, &self.gc_heap) {
                    keys.extend((0..value.len()).map(|idx| idx.to_string()));
                }
                keys.extend(crate::object::with_properties(obj, &self.gc_heap, |p| {
                    p.enumerable_keys().map(str::to_string).collect::<Vec<_>>()
                }));
                Ok(keys)
            }
            Value::Array(arr) => {
                let len = crate::array::len(arr, &self.gc_heap);
                let mut keys = Vec::new();
                for idx in 0..len {
                    if crate::array::has_own_element(arr, &self.gc_heap, idx) {
                        keys.push(idx.to_string());
                    }
                }
                Ok(keys)
            }
            Value::Function { function_id } | Value::Closure { function_id, .. } => {
                // §20.1.2.5 / §10.2.4 — enumerable own string keys in
                // spec creation order. Intrinsic metadata (`length`,
                // `name`, non-arrow `prototype`) is older than any
                // user-installed bag property; route through
                // `ordinary_function_own_property_keys` for the
                // canonical order, then filter by enumerability via
                // the descriptor reader (the default builtin attrs
                // are non-enumerable; `defineProperty` migrating one
                // into the user bag with `enumerable: true` lifts
                // it).
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
                Ok(out)
            }
            Value::NativeFunction(native) => Ok(native
                .enumerable_own_property_keys(&self.gc_heap)
                .into_iter()
                .collect()),
            Value::BoundFunction(bound) => Ok(
                function_metadata::bound_enumerable_own_property_keys(&bound, &self.gc_heap)
                    .into_iter()
                    .collect(),
            ),
            Value::RegExp(_) => Ok(Vec::new()),
            _ => Ok(Vec::new()),
        }
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
        match target {
            Value::Proxy(proxy) => {
                let key_value = self.vm_property_key_to_value(key)?;
                let trap_args: SmallVec<[Value; 8]> =
                    smallvec::smallvec![proxy.target(), key_value];
                match self.invoke_proxy_trap(context, &proxy, "deleteProperty", trap_args)? {
                    Some(value) => {
                        let result = value.to_boolean();
                        if !result {
                            return Ok(false);
                        }
                        // §10.5.10 invariants — when the trap reports
                        // success, the target must not retain a
                        // non-configurable own property at `P`, and
                        // configurable properties may only disappear
                        // from an extensible target.
                        let target_value = proxy.target();
                        let target_desc = self
                            .ordinary_get_own_property_descriptor_value_runtime_rooted(
                                context,
                                target_value.clone(),
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
                            let target_extensible =
                                self.is_extensible_value(context, &target_value)?;
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
                    None => self.ordinary_delete_value(context, proxy.target(), key, hops + 1),
                }
            }
            Value::Object(obj) => {
                if let Some(desc) = self.string_object_exotic_descriptor(obj, key)?
                    && !desc.configurable()
                {
                    return Ok(false);
                }
                Ok(if let Some(key) = key.string_name() {
                    object::delete(obj, &mut self.gc_heap, key)
                } else if let VmPropertyKey::Symbol(sym) = key {
                    object::delete_symbol(obj, &mut self.gc_heap, sym)
                } else {
                    true
                })
            }
            Value::Array(arr) => Ok(match key {
                VmPropertyKey::Symbol(sym) => {
                    array::delete_symbol_property(arr, &mut self.gc_heap, sym)
                }
                _ => match key.string_name() {
                    Some(k) => array::delete_named_property(arr, &mut self.gc_heap, k),
                    None => true,
                },
            }),
            Value::Function { function_id } | Value::Closure { function_id, .. } => {
                Ok(if let Some(key) = key.string_name() {
                    self.ordinary_function_delete_own_property(function_id, key)
                } else if let VmPropertyKey::Symbol(sym) = key {
                    self.function_user_props
                        .get(&function_id)
                        .copied()
                        .map(|bag| object::delete_symbol(bag, &mut self.gc_heap, sym))
                        .unwrap_or(true)
                } else {
                    true
                })
            }
            Value::NativeFunction(native) => Ok(match key.string_name() {
                Some(key) => native.delete_own_property(&mut self.gc_heap, key),
                None if let VmPropertyKey::Symbol(sym) = key => {
                    native.delete_own_symbol_property(&mut self.gc_heap, sym)
                }
                None => true,
            }),
            Value::BoundFunction(bound) => Ok(match key.string_name() {
                Some(key) => {
                    function_metadata::bound_delete_own_property(&bound, &mut self.gc_heap, key)
                }
                None => true,
            }),
            Value::RegExp(_) => Ok(!key.string_name().is_some_and(|key| key == "lastIndex")),
            _ => Ok(true),
        }
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
        match target {
            Value::Proxy(proxy) => {
                if proxy.is_revoked() {
                    return Err(VmError::TypeError {
                        message: "Cannot perform 'set' on a proxy that has been revoked"
                            .to_string(),
                    });
                }
                let key_value = self.vm_property_key_to_value(key)?;
                let trap_args: SmallVec<[Value; 8]> = smallvec::smallvec![
                    proxy.target(),
                    key_value,
                    value.clone(),
                    receiver.clone(),
                ];
                match self.invoke_proxy_trap(context, &proxy, "set", trap_args)? {
                    Some(result) => {
                        let ok = result.to_boolean();
                        if !ok {
                            return Ok(false);
                        }
                        // §10.5.9 invariants — when the trap reports
                        // success, verify the target descriptor admits
                        // the new value.
                        let target_value = proxy.target();
                        let target_desc = self
                            .ordinary_get_own_property_descriptor_value_runtime_rooted(
                                context,
                                target_value.clone(),
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
                        Ok(true)
                    }
                    None => self.ordinary_set_data_value(
                        context,
                        proxy.target(),
                        key,
                        value,
                        receiver,
                        hops + 1,
                    ),
                }
            }
            Value::Array(arr) => {
                if let Some(key) = key.string_name() {
                    array::set_named_property(arr, &mut self.gc_heap, key, value)
                        .map_err(|_| VmError::TypeMismatch)?;
                }
                Ok(true)
            }
            Value::Object(obj) => {
                if let Some(desc) = self.string_object_exotic_descriptor(obj, key)?
                    && !desc.writable()
                {
                    return Ok(false);
                }
                Ok(match key {
                    VmPropertyKey::Symbol(sym) => {
                        object::set_symbol(obj, &mut self.gc_heap, sym.clone(), value)
                    }
                    _ => self.ordinary_set_data_property(
                        obj,
                        key.string_name()
                            .expect("non-symbol key has string spelling"),
                        value,
                    )?,
                })
            }
            Value::RegExp(re) => match key {
                VmPropertyKey::String(key) if *key == "lastIndex" => {
                    regexp_prototype::store_property(&re, &mut self.gc_heap, key, value);
                    Ok(true)
                }
                _ => Ok(false),
            },
            Value::Function { function_id } | Value::Closure { function_id, .. } => match key {
                VmPropertyKey::Symbol(sym) => {
                    let bag = self.function_user_bag_runtime_rooted(function_id, &[&value], &[])?;
                    Ok(object::set_symbol(
                        bag,
                        &mut self.gc_heap,
                        sym.clone(),
                        value,
                    ))
                }
                _ => {
                    let key = key
                        .string_name()
                        .expect("non-symbol key has string spelling");
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
            },
            _ => Ok(false),
        }
    }
}

/// §6.2.5.7 IsCompatiblePropertyDescriptor specialised to a target
/// descriptor and a partial incoming descriptor — without mutation.
/// Returns `true` when applying `incoming` against `target_desc` on
/// an extensible object would succeed under §10.1.6.3.
fn is_compatible_partial_descriptor(
    target_desc: &object::PropertyDescriptor,
    incoming: &object::PartialPropertyDescriptor,
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
                && !abstract_ops::same_value(ex_v, in_v)
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
                let normalised = if matches!(g, Value::Undefined) {
                    None
                } else {
                    Some(g.clone())
                };
                if !optional_value_eq_pair(ex_get, &normalised) {
                    return false;
                }
            }
            if let Some(s) = &incoming.set {
                let normalised = if matches!(s, Value::Undefined) {
                    None
                } else {
                    Some(s.clone())
                };
                if !optional_value_eq_pair(ex_set, &normalised) {
                    return false;
                }
            }
        }
    }
    true
}

fn optional_value_eq_pair(a: &Option<Value>, b: &Option<Value>) -> bool {
    match (a, b) {
        (None, None) => true,
        (Some(x), Some(y)) => abstract_ops::same_value(x, y),
        _ => false,
    }
}

/// SameValue restricted to PropertyKey-typed values (Strings and
/// Symbols). Used by §10.5.11 Proxy `ownKeys` invariant validation.
fn same_property_key(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::String(x), Value::String(y)) => x.to_lossy_string() == y.to_lossy_string(),
        (Value::Symbol(x), Value::Symbol(y)) => x.ptr_eq(y),
        _ => false,
    }
}

/// Convert a PropertyKey-typed [`Value`] (String or Symbol) into a
/// [`VmPropertyKey`]. Caller is responsible for ensuring the value
/// actually holds a PropertyKey-typed entry; anything else is a
/// `TypeMismatch`.
fn property_key_from_value(value: &Value) -> Result<VmPropertyKey<'static>, VmError> {
    match value {
        Value::String(s) => Ok(VmPropertyKey::OwnedString(s.to_lossy_string())),
        Value::Symbol(sym) => Ok(VmPropertyKey::Symbol(sym.clone())),
        _ => Err(VmError::TypeMismatch),
    }
}
