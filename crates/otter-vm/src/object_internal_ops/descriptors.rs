//! Own-property descriptors, prototypes and extensibility.
//!
//! # Contents
//! - `[[GetOwnProperty]]` at value level and the exotic receivers it forks on.
//! - `[[GetPrototypeOf]]` / `[[IsExtensible]]` value forms.
//! - Expando resolution for collection receivers and the private-element table.
//! - The array-index accessor protector and the prototype shape epoch.
//!
//! # Invariants
//! - A proxy receiver never reaches these; the proxy-aware entries in [`super`]
//!   resolve the trap first.

use crate::activation_stack::ActivationStack;
use crate::{
    ExecutionContext, Interpreter, Value, VmError, VmPropertyKey, abstract_ops, array,
    function_metadata, object, string,
};
use smallvec::SmallVec;

impl Interpreter {
    pub(crate) fn ordinary_get_own_property_descriptor_value(
        &mut self,
        stack: &mut ActivationStack,
        context: &ExecutionContext,
        target: Value,
        key: &VmPropertyKey,
        hops: usize,
    ) -> Result<Option<object::PropertyDescriptor>, VmError> {
        if hops >= object::PROTO_CHAIN_HARD_CAP {
            return Ok(None);
        }
        let target = self.with_handle_scope(|interp, scope| -> Result<Value, VmError> {
            let target = interp.scoped_value(scope, target);
            let current = interp.escape_scoped(target);
            interp.ensure_deferred_namespace_ready(
                stack,
                context,
                &current,
                !Self::deferred_key_is_symbol_like(key),
            )?;
            Ok(interp.escape_scoped(target))
        })?;
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
                Some(v) if v.is_hole() => Err(self.err_this_uninit(
                    (format!("Cannot access '{name}' before initialization")).into(),
                )),
                Some(v) => Ok(Some(object::PropertyDescriptor::data(v, true, true, false))),
                None => Ok(None),
            };
        }
        if let Some(proxy) = target.as_proxy() {
            let key_value = self.vm_property_key_to_value(key)?;
            let trap_args: SmallVec<[Value; 8]> =
                smallvec::smallvec![proxy.target(&self.gc_heap), key_value];
            return match self.invoke_proxy_trap(
                stack,
                context,
                &proxy,
                "getOwnPropertyDescriptor",
                trap_args,
            )? {
                crate::object_internal_ops::ProxyTrap::Trapped(v) if v.is_nullish() => {
                    let target_desc = self.ordinary_get_own_property_descriptor_value(
                        stack,
                        context,
                        proxy.target(&self.gc_heap),
                        key,
                        hops + 1,
                    )?;
                    self.validate_proxy_get_own_property_descriptor(
                        &proxy.target(&self.gc_heap),
                        target_desc.as_ref(),
                        None,
                    )?;
                    Ok(None)
                }
                crate::object_internal_ops::ProxyTrap::Trapped(v)
                    if v.is_object() || v.is_proxy() =>
                {
                    // §10.5.5 step 9-ish ToPropertyDescriptor through
                    // ordinary [[Get]]s so a Proxy descriptor object
                    // dispatches its own traps.
                    let partial = self.evaluate_to_property_descriptor(stack, context, &v)?;
                    let desc = partial.complete_for_new_property();
                    let target_desc = self.ordinary_get_own_property_descriptor_value(
                        stack,
                        context,
                        proxy.target(&self.gc_heap),
                        key,
                        hops + 1,
                    )?;
                    self.validate_proxy_get_own_property_descriptor(
                        &proxy.target(&self.gc_heap),
                        target_desc.as_ref(),
                        Some(&desc),
                    )?;
                    Ok(Some(desc))
                }
                crate::object_internal_ops::ProxyTrap::Trapped(_) => Err(self.err_type(
                    ("Proxy getOwnPropertyDescriptor trap returned non-object descriptor"
                        .to_string())
                    .into(),
                )),
                crate::object_internal_ops::ProxyTrap::NoTrap {
                    target: fallthrough_target,
                } => self.ordinary_get_own_property_descriptor_value(
                    stack,
                    context,
                    fallthrough_target,
                    key,
                    hops + 1,
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
            if let Some(value) = array::get_own_named_data_property(arr, &self.gc_heap, key) {
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
        if target.is_map()
            || target.is_set()
            || target.is_weak_map()
            || target.is_weak_set()
            || target.is_generator()
        {
            // Ordinary own properties on a Map/Set/Generator live in the
            // lazy expando; size/keys/… are prototype accessors, not own.
            if let Some(bag) = self.collection_expando(&target) {
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
        if let Some(t) = target.as_temporal(&self.gc_heap) {
            // Ordinary own properties live in the lazy expando; the
            // year/month/… accessors are prototype properties, not own.
            if let Some(bag) = t.expando(&self.gc_heap) {
                if let Some(name) = key.string_name() {
                    if let Some(desc) = object::get_own_descriptor(bag, &self.gc_heap, name) {
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
        if target.is_intl() || target.is_iterator() {
            if let Some(bag) = self.non_gc_exotic_user_props(&target) {
                if let Some(name) = key.string_name() {
                    if let Some(desc) = object::get_own_descriptor(bag, &self.gc_heap, name) {
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
            let owner = target.as_closure(&self.gc_heap);
            if let VmPropertyKey::Symbol(sym) = key {
                let Some(bag) = self.callable_bag_read(owner, function_id) else {
                    return Ok(None);
                };
                return Ok(object::get_own_symbol_descriptor(bag, &self.gc_heap, *sym));
            }
            let key = key
                .string_name()
                .expect("non-symbol key has string spelling");
            if key == "prototype" {
                let _ = self.function_property_get_with_receiver(
                    stack,
                    context,
                    owner,
                    function_id,
                    Some(target),
                    "prototype",
                )?;
                let Some(bag) = self.callable_bag_read(owner, function_id) else {
                    return Ok(None);
                };
                return Ok(object::get_own_descriptor(bag, &self.gc_heap, key));
            }
            return self.ordinary_function_own_property_descriptor(
                Some(context),
                owner,
                function_id,
                key,
            );
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
                let owner = ctor.as_closure(&self.gc_heap);
                return self.ordinary_function_own_property_descriptor(
                    Some(context),
                    owner,
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

    pub(crate) fn proxy_get_prototype_invariant_target_proto(
        &mut self,
        stack: &mut ActivationStack,
        context: &ExecutionContext,
        target: &Value,
    ) -> Result<Option<Value>, VmError> {
        // §10.5.1 step 8 — IsExtensible(target) is the target's own
        // internal method: for a Proxy target the `isExtensible` trap
        // fires (observable, may throw) before any prototype read.
        if self.is_extensible_value(stack, context, target)? {
            return Ok(None);
        }
        Ok(Some(self.ordinary_get_prototype_value(
            stack, context, *target, 0,
        )?))
    }

    pub(crate) fn ordinary_get_prototype_value(
        &mut self,
        stack: &mut ActivationStack,
        context: &ExecutionContext,
        value: Value,
        hops: usize,
    ) -> Result<Value, VmError> {
        if hops >= object::PROTO_CHAIN_HARD_CAP {
            return Ok(Value::null());
        }
        if let Some(proxy) = value.as_proxy() {
            let trap_args: SmallVec<[Value; 8]> = smallvec::smallvec![proxy.target(&self.gc_heap)];
            return match self.invoke_proxy_trap(
                stack,
                context,
                &proxy,
                "getPrototypeOf",
                trap_args,
            )? {
                crate::object_internal_ops::ProxyTrap::Trapped(result) => {
                    if !Self::proxy_get_prototype_result_is_object_or_null(&result) {
                        return Err(self.err_type(
                            ("Proxy getPrototypeOf trap returned non-object".to_string()).into(),
                        ));
                    }
                    if let Some(target_proto) = self.proxy_get_prototype_invariant_target_proto(
                        stack,
                        context,
                        &proxy.target(&self.gc_heap),
                    )? && !abstract_ops::same_value(&result, &target_proto, &self.gc_heap)
                    {
                        return Err(self.err_type(
                            ("Proxy getPrototypeOf trap returned incompatible prototype"
                                .to_string())
                            .into(),
                        ));
                    }
                    Ok(result)
                }
                crate::object_internal_ops::ProxyTrap::NoTrap {
                    target: fallthrough_target,
                } => {
                    self.ordinary_get_prototype_value(stack, context, fallthrough_target, hops + 1)
                }
            };
        }
        if let Some(intl) = value.as_intl(&self.gc_heap) {
            if let Some(over) = self.non_gc_exotic_prototype_override(&value) {
                return Ok(over);
            }
            return Ok(self.intl_kind_prototype_value(intl.kind().class_name()));
        }
        if value.is_object_type() {
            return self.get_prototype_for_op(&value);
        }
        // §B.2.2.1.1 step 1 — ToObject(this value): a primitive
        // receiver reports its wrapper's [[Prototype]] without
        // allocating the wrapper.
        if !value.is_nullish() {
            return self.get_prototype_for_op(&value);
        }
        Err(VmError::TypeMismatch)
    }

    pub(crate) fn proxy_get_prototype_result_is_object_or_null(value: &Value) -> bool {
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
        stack: &mut ActivationStack,
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
                return Err(self.err_type(
                    ("Cannot perform 'isExtensible' on a proxy that has been revoked".to_string())
                        .into(),
                ));
            }
            let trap_args: SmallVec<[Value; 8]> = smallvec::smallvec![proxy.target(&self.gc_heap)];
            return match self.invoke_proxy_trap(
                stack,
                context,
                &proxy,
                "isExtensible",
                trap_args,
            )? {
                crate::object_internal_ops::ProxyTrap::Trapped(result) => {
                    let trap = result.to_boolean(&self.gc_heap);
                    let target_ext =
                        self.is_extensible_value(stack, context, &proxy.target(&self.gc_heap))?;
                    if trap != target_ext {
                        return Err(self.err_type(
                            ("Proxy isExtensible trap returned value inconsistent with target"
                                .to_string())
                            .into(),
                        ));
                    }
                    Ok(trap)
                }
                crate::object_internal_ops::ProxyTrap::NoTrap {
                    target: fallthrough_target,
                } => self.is_extensible_value(stack, context, &fallthrough_target),
            };
        }
        Ok(self.is_extensible_non_proxy(value))
    }

    /// The `[[IsExtensible]]` family dispatch for every receiver kind except
    /// Proxy, whose trap needs a reentrant call.
    ///
    /// Shared with `Object.isExtensible` for the same reason as
    /// [`Self::prevent_extensions_non_proxy`]: a second list here silently
    /// diverges from the kinds that keep the flag on a lazy expando bag.
    #[must_use]
    pub(crate) fn is_extensible_non_proxy(&self, value: &Value) -> bool {
        if let Some(obj) = value.as_object() {
            return object::is_extensible(obj, &self.gc_heap);
        }
        if let Some(t) = value.as_typed_array(&self.gc_heap) {
            return t
                .expando(&self.gc_heap)
                .is_none_or(|bag| object::is_extensible(bag, &self.gc_heap));
        }
        if let Some(arr) = value.as_array() {
            return array::is_extensible(arr, &self.gc_heap);
        }
        if let Some(native) = value.as_native_function() {
            return native.is_extensible(&self.gc_heap);
        }
        if let Some(class) = value.as_class_constructor() {
            return object::is_extensible(class.statics(&self.gc_heap), &self.gc_heap);
        }
        let owner = value.as_closure(&self.gc_heap);
        let fid = value
            .as_function()
            .or_else(|| owner.map(|c| c.cached_function_id));
        if let Some(function_id) = fid {
            return self.ordinary_function_is_extensible(owner, function_id);
        }
        if let Some(regexp) = value.as_regexp() {
            return regexp.is_extensible(&self.gc_heap);
        }
        if let Some(bag) = self.collection_expando(value) {
            return object::is_extensible(bag, &self.gc_heap);
        }
        if let Some(promise) = value.as_promise()
            && let Some(bag) = promise.expando(&self.gc_heap)
        {
            return object::is_extensible(bag, &self.gc_heap);
        }
        // Per §10.1.3 every other ordinary heap value is extensible
        // by default.
        true
    }

    /// Read the lazily-allocated expando bag carrying user-defined
    /// own properties on a Map, Set, or Generator instance, if one has
    /// been materialised. Returns `None` for other values or instances
    /// that have never had a property written.
    pub(crate) fn collection_expando(&self, value: &Value) -> Option<object::JsObject> {
        if let Some(m) = value.as_map() {
            return crate::collections::map_expando(m, &self.gc_heap);
        }
        if let Some(s) = value.as_set() {
            return crate::collections::set_expando(s, &self.gc_heap);
        }
        if let Some(m) = value.as_weak_map() {
            return crate::collections::weak_map_expando(m, &self.gc_heap);
        }
        if let Some(ws) = value.as_weak_set() {
            return crate::collections::weak_set_expando(ws, &self.gc_heap);
        }
        if let Some(g) = value.as_generator() {
            return g.expando(&self.gc_heap);
        }
        None
    }

    /// Materialise (or fetch) the expando bag for a Map, Set, or
    /// Generator so a user-defined own property can be stored on it.
    /// Caller must have already established the receiver kind.
    pub(crate) fn collection_ensure_expando(
        &mut self,
        value: &Value,
    ) -> Result<object::JsObject, VmError> {
        if let Some(m) = value.as_map() {
            return crate::property_dispatch::map_ensure_expando_pub(&mut self.gc_heap, m);
        }
        if let Some(s) = value.as_set() {
            return crate::property_dispatch::set_ensure_expando_pub(&mut self.gc_heap, s);
        }
        if let Some(m) = value.as_weak_map() {
            return crate::property_dispatch::weak_map_ensure_expando_pub(&mut self.gc_heap, m);
        }
        if let Some(ws) = value.as_weak_set() {
            return crate::property_dispatch::weak_set_ensure_expando_pub(&mut self.gc_heap, ws);
        }
        if let Some(g) = value.as_generator() {
            return crate::property_dispatch::generator_ensure_expando_pub(&mut self.gc_heap, &g);
        }
        Err(self.err_type(("collection_ensure_expando on non-collection value".to_string()).into()))
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
        stack: &mut ActivationStack,
        context: &ExecutionContext,
        key: &VmPropertyKey,
        value: Value,
        receiver: &Value,
    ) -> Result<bool, VmError> {
        if !crate::reflect::is_type_object_value(receiver) {
            return Ok(false);
        }
        let existing =
            self.ordinary_get_own_property_descriptor_value(stack, context, *receiver, key, 0)?;
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
                    self.define_own_property_value(stack, context, receiver, key, partial)
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
                self.define_own_property_value(stack, context, receiver, key, descriptor)
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
        let slots = self
            .gc_heap
            .read_payload(proxy.handle(), |body| body.private_elements);
        if slots.is_null() {
            return None;
        }
        self.gc_heap.read_payload(slots, |body| {
            body.slots()
                .iter()
                .find(|slot| slot.name.handle() == sym.handle())
                .map(|slot| slot.value)
        })
    }

    /// Insert or overwrite `sym` in a proxy's [[PrivateElements]].
    pub(crate) fn proxy_private_upsert(
        &mut self,
        proxy: &crate::proxy::JsProxy,
        sym: crate::symbol::JsSymbol,
        value: Value,
    ) {
        let slots = self
            .gc_heap
            .read_payload(proxy.handle(), |body| body.private_elements);
        // Overwriting an existing name needs no allocation.
        if !slots.is_null() {
            let overwritten = self.gc_heap.with_payload(slots, |body| {
                match body
                    .slots_mut()
                    .iter_mut()
                    .find(|slot| slot.name.handle() == sym.handle())
                {
                    Some(slot) => {
                        slot.value = value;
                        true
                    }
                    None => false,
                }
            });
            if overwritten {
                self.gc_heap.record_write(slots, &value);
                return;
            }
        }
        // A new name grows the list, which allocates. The proxy, the name
        // and the value are all live roots across it.
        let previous = self
            .gc_heap
            .read_payload(proxy.handle(), |body| body.private_elements);
        let existing = if previous.is_null() {
            Vec::new()
        } else {
            self.gc_heap
                .read_payload(previous, |body| body.slots().to_vec())
        };
        let mut proxy_handle = proxy.handle();
        let mut name_value = crate::Value::symbol(sym);
        let mut stored = value;
        let grown = {
            let proxy_slot = std::ptr::addr_of_mut!(proxy_handle);
            let mut visit = |visitor: &mut dyn FnMut(*mut otter_gc::raw::RawGc)| {
                visitor(proxy_slot.cast::<otter_gc::raw::RawGc>());
                name_value.trace_value_slot_mut(visitor);
                stored.trace_value_slot_mut(visitor);
                for slot in &existing {
                    slot.name.trace_value_slots(visitor);
                    slot.value.trace_value_slots(visitor);
                }
            };
            match crate::proxy::alloc_private_slots(
                &mut self.gc_heap,
                existing.len() + 1,
                &mut visit,
            ) {
                Ok(handle) => handle,
                // Out of memory installing a private field leaves the
                // proxy exactly as it was; the caller's operation fails
                // through the ordinary allocation path instead.
                Err(_) => return,
            }
        };
        self.gc_heap.with_payload(grown, |body| {
            let slots = body.slots_mut();
            for (target, source) in slots.iter_mut().zip(existing.iter()) {
                *target = *source;
            }
            let last = slots.len() - 1;
            slots[last] = crate::proxy::PrivateSlot {
                name: sym,
                value: stored,
            };
            true
        });
        self.gc_heap
            .with_payload(proxy_handle, |body| body.private_elements = grown);
        // The list handle and every pair were installed by raw payload
        // writes, so record the edges the barrier would have.
        self.gc_heap.record_write(proxy_handle, &grown);
        for slot in &existing {
            self.gc_heap
                .record_write(grown, &crate::Value::symbol(slot.name));
            self.gc_heap.record_write(grown, &slot.value);
        }
        self.gc_heap.record_write(grown, &name_value);
        self.gc_heap.record_write(grown, &stored);
    }

    pub(crate) fn private_element_lookup(
        &mut self,
        stack: &mut ActivationStack,
        context: &ExecutionContext,
        receiver: &Value,
        sym: crate::symbol::JsSymbol,
    ) -> Result<Option<(Value, object::PropertyDescriptor)>, VmError> {
        self.with_handle_scope(|interp, scope| {
            let receiver_handle = interp.scoped_value(scope, *receiver);
            let receiver = interp.escape_scoped(receiver_handle);
            // §6.2.12 — a Proxy carries its own [[PrivateElements]];
            // private names never consult traps or the target/prototype
            // chain.
            if sym.is_private_name()
                && let Some(p) = receiver.as_proxy()
            {
                if let Some(value) = interp.proxy_private_find(&p, sym) {
                    return Ok(Some((
                        interp.escape_scoped(receiver_handle),
                        object::PropertyDescriptor {
                            kind: object::DescriptorKind::Data { value },
                            flags: object::PropertyFlags::new(true, false, false),
                        },
                    )));
                }
                // Brand entries are copied out under a non-allocating heap
                // borrow, then immediately parked as handles before any
                // descriptor lookup can allocate or invoke user code.
                let private_slots = interp
                    .gc_heap
                    .read_payload(p.handle(), |body| body.private_elements);
                let brand_protos: Vec<Value> = if private_slots.is_null() {
                    Vec::new()
                } else {
                    interp.gc_heap.read_payload(private_slots, |body| {
                        body.slots()
                            .iter()
                            .filter(|slot| slot.value.is_object())
                            .map(|slot| slot.value)
                            .collect()
                    })
                };
                let brand_protos: Vec<_> = brand_protos
                    .into_iter()
                    .map(|proto| interp.scoped_value(scope, proto))
                    .collect();
                let key = VmPropertyKey::Symbol(sym);
                for proto in brand_protos {
                    let proto_value = interp.escape_scoped(proto);
                    if let Some(desc) = interp.ordinary_get_own_property_descriptor_value(
                        stack,
                        context,
                        proto_value,
                        &key,
                        0,
                    )? {
                        return Ok(Some((interp.escape_scoped(proto), desc)));
                    }
                }
                return Ok(None);
            }

            let key = VmPropertyKey::Symbol(sym);
            let current_handle = interp.scoped_value(scope, receiver);
            let mut hops = 0;
            loop {
                let current = interp.escape_scoped(current_handle);
                if let Some(desc) = interp
                    .ordinary_get_own_property_descriptor_value(stack, context, current, &key, 0)?
                {
                    return Ok(Some((interp.escape_scoped(current_handle), desc)));
                }
                if hops >= object::PROTO_CHAIN_HARD_CAP {
                    break;
                }
                let current = interp.escape_scoped(current_handle);
                // §7.3.30 — private elements never inherit across a class
                // boundary: a subclass constructor does not see the parent
                // constructor's static privates.
                if current.is_class_constructor() {
                    break;
                }
                let proto = interp.ordinary_get_prototype_value(stack, context, current, hops)?;
                if !proto.is_object() && !proto.is_object_type() {
                    break;
                }
                interp.set_scoped(current_handle, proto);
                hops += 1;
            }

            // Constructor-return override: a branded plain object whose
            // [[Prototype]] chain misses the method holder still resolves
            // private methods through its brand entries.
            let receiver = interp.escape_scoped(receiver_handle);
            if sym.is_private_name()
                && let Some(obj) = receiver.as_object()
            {
                let brand_protos: Vec<Value> =
                    crate::object::with_properties(obj, &interp.gc_heap, |props| {
                        props
                            .symbol_keys()
                            .filter(|k| k.is_private_name())
                            .filter_map(|k| crate::object::get_symbol(obj, &interp.gc_heap, k))
                            .filter(|v| v.is_object())
                            .collect()
                    });
                let brand_protos: Vec<_> = brand_protos
                    .into_iter()
                    .map(|proto| interp.scoped_value(scope, proto))
                    .collect();
                for proto in brand_protos {
                    let proto_value = interp.escape_scoped(proto);
                    if proto_value == interp.escape_scoped(receiver_handle) {
                        continue;
                    }
                    if let Some(desc) = interp.ordinary_get_own_property_descriptor_value(
                        stack,
                        context,
                        proto_value,
                        &key,
                        0,
                    )? {
                        return Ok(Some((interp.escape_scoped(proto), desc)));
                    }
                }
            }
            Ok(None)
        })
    }

    /// `true` when `target` is a realm prototype that an ordinary dense array
    /// inherits from, so an indexed property defined on it becomes visible
    /// through that array's holes.
    pub(crate) fn is_realm_element_prototype(&self, target: Value) -> bool {
        let Some(object) = target.as_object() else {
            return false;
        };
        [
            self.realm_intrinsics.array_prototype(),
            self.realm_intrinsics.object_prototype(),
        ]
        .into_iter()
        .flatten()
        .any(|prototype| prototype == object)
    }

    /// Flip the existing array-index accessor protector latch and publish its
    /// sole epoch transition. Redundant observations are strict no-ops.
    pub(crate) fn activate_array_index_accessor_protector(&mut self) {
        if self.array_index_accessor_protector {
            return;
        }
        self.array_index_accessor_protector = true;
        self.array_index_accessor_protector_epoch = self
            .array_index_accessor_protector_epoch
            .checked_add(1)
            .expect("array-index accessor protector epoch exhausted");
        let affected = self.jit_code_registry.invalidate_dependents(
            crate::native_abi::CodeDependencyKind::Protector,
            crate::native_abi::ARRAY_INDEX_ACCESSOR_PROTECTOR_IDENTITY,
            self.array_index_accessor_protector_epoch,
        );
        self.discard_invalidated_jit_state(&affected);
    }

    /// Publish one successful ordinary-object prototype mutation.
    pub(crate) fn bump_ordinary_object_prototype_shape_epoch(&mut self) {
        self.shape_epoch = self
            .shape_epoch
            .checked_add(1)
            .expect("ordinary-object prototype shape epoch exhausted");
        let affected = self.jit_code_registry.invalidate_dependents(
            crate::native_abi::CodeDependencyKind::ShapeEpoch,
            crate::native_abi::ORDINARY_OBJECT_PROTOTYPE_SHAPE_IDENTITY,
            self.shape_epoch,
        );
        self.discard_invalidated_jit_state(&affected);
    }
}
