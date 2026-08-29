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
//! - The value-level `[[Set]]` funnel shared by interpreter and JIT property
//!   stores, including ordinary and exotic receiver phases.
//! - String exotic property reads/descriptors.
//! - Proxy invariant validation helpers.
//! - Realm constructor prototype lookup.
//! - Protector/shape epoch publication at the two Slice 11.8 mutation funnels.
//!
//! # Invariants
//! - Proxy traps are invoked through the normal callable path.
//! - `ordinary_set_data_value` is total for object-like values: specialised
//!   exotics run first and the generic internal-method fallback owns the rest.
//! - String exotic keys only synthesize `length` and index descriptors.
//! - Constructor prototype lookup preserves existing global-object semantics.
//! - The array-index accessor protector epoch advances only on the existing
//!   latch's `false -> true` transition.
//! - Proxy descriptor trap objects are assembled through canonical scoped
//!   handles; every optional field write re-reads collector-forwarded slots.
//! - The shape epoch covers only actual ordinary-`JsObject` prototype changes
//!   through `set_prototype_value_proxy_aware` (including proxy fallthrough and
//!   class-statics recursion). Array, TypedArray, function-side-table, direct
//!   low-level/bootstrap prototype writes, and property shape transitions do
//!   not advance it in Slice 11.8.
//!
//! # See also
//! - [`crate::property_dispatch`]
//! - [`crate::object`]

use crate::activation_stack::ActivationStack;
use std::collections::BTreeSet;

use smallvec::SmallVec;

use crate::{
    ExecutionContext, Interpreter, JsObject, JsString, Local, Value, VmError, VmGetOutcome,
    VmPropertyKey, abstract_ops, array, descriptor_value, function_metadata, object, string,
};

mod define;
mod descriptors;
mod get;
pub(crate) use get::byte_view_expando;
mod keys;
mod set_delete;

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
    interp: &Interpreter,
    value: &Value,
    heap: &otter_gc::GcHeap,
) -> Result<VmPropertyKey<'static>, VmError> {
    if let Some(s) = value.as_string(heap) {
        return Ok(VmPropertyKey::OwnedString(s.to_lossy_string(heap)));
    }
    if let Some(sym) = value.as_symbol(heap) {
        return Ok(VmPropertyKey::Symbol(sym));
    }
    Err(interp.err_type(("property key must be a string or symbol".to_string()).into()))
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

/// Result of a Proxy trap dispatch: either the trap's return value, or
/// the [[ProxyTarget]] snapshot taken BEFORE the observable trap lookup
/// (§10.5.* step "Let target be O.[[ProxyTarget]]") — the handler get
/// can revoke the proxy as a side effect, and the fallthrough must keep
/// operating on the pre-revocation target.
pub(crate) enum ProxyTrap {
    /// The trap ran; its result.
    Trapped(Value),
    /// No trap installed — fall through to `target`'s internal method.
    NoTrap {
        /// The pre-lookup [[ProxyTarget]] snapshot.
        target: Value,
    },
}

impl Interpreter {
    /// §28.2 — call a Proxy handler trap. When the trap is missing,
    /// returns [`ProxyTrap::NoTrap`] with the pre-lookup target so the
    /// caller can fall through to the target's behaviour. When the trap
    /// exists, invokes it with `(target, ...trap_args)` (per spec each
    /// trap takes the target as its first explicit argument; subsequent
    /// ones come from `args`) and returns the result.
    pub(crate) fn invoke_proxy_trap(
        &mut self,
        stack: &mut ActivationStack,
        context: &ExecutionContext,
        proxy: &crate::proxy::JsProxy,
        trap: &str,
        args: SmallVec<[Value; 8]>,
    ) -> Result<ProxyTrap, VmError> {
        self.with_handle_scope(|interp, scope| {
            let proxy_handle = interp.scoped_value(scope, Value::proxy(*proxy));
            let arg_handles: SmallVec<[Local<'_>; 8]> = args
                .into_iter()
                .map(|value| interp.scoped_value(scope, value))
                .collect();

            let proxy = interp
                .escape_scoped(proxy_handle)
                .as_proxy()
                .ok_or(VmError::TypeMismatch)?;
            if proxy.is_revoked(&interp.gc_heap) {
                return Err(VmError::TypeMismatch);
            }
            let target_handle = interp.scoped_value(scope, proxy.target(&interp.gc_heap));
            let handler_handle = interp.scoped_value(scope, proxy.handler(&interp.gc_heap));
            let trap_key = VmPropertyKey::String(trap);
            let handler = interp.escape_scoped(handler_handle);
            let trap_value =
                match interp.ordinary_get_value(stack, context, handler, handler, &trap_key, 0)? {
                    VmGetOutcome::Value(value) => value,
                    VmGetOutcome::InvokeGetter { getter } => interp.run_callable_sync_rooted(
                        stack,
                        context,
                        &getter,
                        interp.escape_scoped(handler_handle),
                        SmallVec::new(),
                    )?,
                };
            let trap_handle = interp.scoped_value(scope, trap_value);
            let trap_value = interp.escape_scoped(trap_handle);
            if trap_value.is_nullish() {
                return Ok(ProxyTrap::NoTrap {
                    target: interp.escape_scoped(target_handle),
                });
            }
            if !interp.is_callable_runtime(&trap_value) {
                return Err(VmError::TypeMismatch);
            }
            let current_args = arg_handles
                .into_iter()
                .map(|handle| interp.escape_scoped(handle))
                .collect();
            let result = interp.run_callable_sync_rooted(
                stack,
                context,
                &interp.escape_scoped(trap_handle),
                interp.escape_scoped(handler_handle),
                current_args,
            )?;
            Ok(ProxyTrap::Trapped(result))
        })
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
            return Ok(Some(Value::number_u32(value.len())));
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
                    return Err(self.err_type(
                        ("Proxy getOwnPropertyDescriptor trap cannot hide target property"
                            .to_string())
                        .into(),
                    ));
                }
            }
            (None, Some(trap_desc)) => {
                if self.target_is_non_extensible_object(target) || !trap_desc.configurable() {
                    return Err(self.err_type(
                        ("Proxy getOwnPropertyDescriptor trap reported incompatible property"
                            .to_string())
                        .into(),
                    ));
                }
            }
            (Some(target_desc), Some(trap_desc)) => {
                if !target_desc.configurable() && trap_desc.configurable() {
                    return Err(self.err_type(( "Proxy getOwnPropertyDescriptor trap reported configurable descriptor for non-configurable target property".to_string()).into()));
                }
                if !trap_desc.configurable() && target_desc.configurable() {
                    return Err(self.err_type(( "Proxy getOwnPropertyDescriptor trap reported non-configurable descriptor for configurable target property".to_string()).into()));
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
                    return Err(self.err_type(( "Proxy getOwnPropertyDescriptor trap reported non-writable descriptor for writable target property".to_string()).into()));
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
        // A class constructor's `prototype` is a non-writable
        // non-configurable own data property — the one function-shaped
        // descriptor the §10.5.8 get-trap invariant can trip over.
        if let Some(class) = target.as_class_constructor() {
            if key.string_name() == Some("prototype") {
                if let Some(desc) = object::get_own_descriptor(
                    class.statics(&self.gc_heap),
                    &self.gc_heap,
                    "prototype",
                ) {
                    return Some(desc);
                }
                return Some(object::PropertyDescriptor::data(
                    Value::object(class.prototype(&self.gc_heap)),
                    false,
                    false,
                    false,
                ));
            }
            return None;
        }
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
                return Err(self.err_type(( "Proxy get trap returned incompatible value for non-writable non-configurable property".to_string()).into()));
            }
            object::DescriptorKind::Accessor { getter: None, .. }
                if !desc.configurable() && !trap_result.is_undefined() =>
            {
                return Err(self.err_type(
                    ("Proxy get trap returned value for non-configurable accessor without getter"
                        .to_string())
                    .into(),
                ));
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
            "Object" => self.realm_intrinsics.object_prototype(),
            "Function" => self.realm_intrinsics.function_prototype(),
            "Array" => self.realm_intrinsics.array_prototype(),
            "Promise" => self.realm_intrinsics.promise_prototype(),
            "RegExp" => self.realm_intrinsics.regexp_prototype(),
            "String" => self.realm_intrinsics.string_prototype(),
            "Number" => self.realm_intrinsics.number_prototype(),
            "Map" => self.realm_intrinsics.map_prototype(),
            "Set" => self.realm_intrinsics.set_prototype(),
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

    /// §10.5.4 / §10.1.4 — value-level `[[PreventExtensions]]`.
    pub(crate) fn prevent_extensions_value(
        &mut self,
        stack: &mut ActivationStack,
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
                return Err(self.err_type(
                    ("Cannot perform 'preventExtensions' on a proxy that has been revoked"
                        .to_string())
                    .into(),
                ));
            }
            let trap_args: SmallVec<[Value; 8]> = smallvec::smallvec![proxy.target(&self.gc_heap)];
            return match self.invoke_proxy_trap(
                stack,
                context,
                &proxy,
                "preventExtensions",
                trap_args,
            )? {
                crate::object_internal_ops::ProxyTrap::Trapped(result) => {
                    let ok = result.to_boolean(&self.gc_heap);
                    if ok
                        && self.is_extensible_value(stack, context, &proxy.target(&self.gc_heap))?
                    {
                        return Err(self.err_type((
                                "Proxy preventExtensions trap succeeded but target is still extensible"
                                    .to_string()).into()));
                    }
                    Ok(ok)
                }
                crate::object_internal_ops::ProxyTrap::NoTrap {
                    target: fallthrough_target,
                } => self.prevent_extensions_value(stack, context, &fallthrough_target),
            };
        }
        self.prevent_extensions_non_proxy(value)
    }

    /// The `[[PreventExtensions]]` family dispatch for every receiver kind
    /// except Proxy, whose trap needs a reentrant call.
    ///
    /// Shared with `Object.preventExtensions`, which has no reentry state and
    /// would otherwise carry a second, silently diverging list of families.
    pub(crate) fn prevent_extensions_non_proxy(&mut self, value: &Value) -> Result<bool, VmError> {
        if let Some(obj) = value.as_object() {
            object::prevent_extensions(obj, &mut self.gc_heap);
            return Ok(true);
        }
        // §10.4.5.4 TypedArray [[PreventExtensions]] returns `false` when
        // `IsTypedArrayFixedLength(O)` is false, so the internal method —
        // and therefore `Object.preventExtensions`/`freeze`/`seal` —
        // throws. A view is *not* fixed-length when it length-tracks its
        // buffer, or when it has an explicit length over a non-shared
        // resizable buffer (which can shrink the view out of bounds). A
        // view over a fixed-length buffer, or a fixed-length view over a
        // growable SharedArrayBuffer (which only grows, never shrinks),
        // is fixed-length and succeeds; its extensibility lives on the
        // lazy expando bag (elements are exempt from [[Extensible]]).
        if let Some(t) = value.as_typed_array(&self.gc_heap) {
            let buffer = t.buffer(&self.gc_heap);
            let fixed_length = !t.is_length_tracking(&self.gc_heap)
                && (!buffer.is_resizable(&self.gc_heap) || buffer.is_shared());
            if !fixed_length {
                return Ok(false);
            }
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
        // A class constructor's own (static) properties live on its statics
        // object, so that is where [[Extensible]] has to live too — otherwise
        // `preventExtensions` on a class is silently a no-op and static fields
        // keep being installed.
        if let Some(class) = value.as_class_constructor() {
            let statics = class.statics(&self.gc_heap);
            object::prevent_extensions(statics, &mut self.gc_heap);
            return Ok(true);
        }
        let owner = value.as_closure(&self.gc_heap);
        let fid = value
            .as_function()
            .or_else(|| owner.map(|c| c.cached_function_id));
        if let Some(function_id) = fid {
            self.ordinary_function_prevent_extensions(owner, function_id);
            return Ok(true);
        }
        if let Some(regexp) = value.as_regexp() {
            regexp.prevent_extensions(&mut self.gc_heap);
            return Ok(true);
        }
        if value.is_map()
            || value.is_set()
            || value.is_weak_map()
            || value.is_weak_set()
            || value.is_generator()
        {
            // Materialise the expando and mark it non-extensible so the
            // collection reports [[IsExtensible]] = false and rejects
            // further own-property additions.
            let bag = self.collection_ensure_expando(value)?;
            object::prevent_extensions(bag, &mut self.gc_heap);
            return Ok(true);
        }
        if let Some(promise) = value.as_promise() {
            // Same shape as the collections above: a promise's own properties
            // live on a lazy bag, and [[IsExtensible]] reads that bag, so the
            // flag has nowhere to live until the bag exists.
            let bag =
                crate::property_dispatch::promise_ensure_expando_pub(&mut self.gc_heap, &promise)?;
            object::prevent_extensions(bag, &mut self.gc_heap);
            return Ok(true);
        }
        Ok(true)
    }

    pub(crate) fn instanceof_target_prototype(
        &mut self,
        stack: &mut ActivationStack,
        context: &ExecutionContext,
        rhs: &Value,
    ) -> Result<Option<Value>, VmError> {
        self.with_handle_scope(|interp, scope| {
            let rhs_handle = interp.scoped_value(scope, *rhs);
            interp.instanceof_target_prototype_scoped(stack, context, rhs_handle)
        })
    }

    fn instanceof_target_prototype_scoped(
        &mut self,
        stack: &mut ActivationStack,
        context: &ExecutionContext,
        rhs_handle: Local<'_>,
    ) -> Result<Option<Value>, VmError> {
        let rhs = self.escape_scoped(rhs_handle);
        if rhs.is_object() || rhs.is_proxy() {
            let key = VmPropertyKey::String("prototype");
            return match self.ordinary_get_value(stack, context, rhs, rhs, &key, 0)? {
                VmGetOutcome::Value(v) if v.is_undefined() => {
                    Ok(Some(self.escape_scoped(rhs_handle)))
                }
                VmGetOutcome::Value(value) if value.is_object_type() || value.is_proxy() => {
                    Ok(Some(value))
                }
                VmGetOutcome::Value(_) => {
                    Err(self.err_type(("instanceof prototype is not an object".to_string()).into()))
                }
                VmGetOutcome::InvokeGetter { getter } => {
                    let args: SmallVec<[Value; 8]> = SmallVec::new();
                    let receiver = self.escape_scoped(rhs_handle);
                    let value =
                        self.run_callable_sync_rooted(stack, context, &getter, receiver, args)?;
                    if value.is_object_type() || value.is_proxy() {
                        Ok(Some(value))
                    } else {
                        Err(self
                            .err_type(("instanceof prototype is not an object".to_string()).into()))
                    }
                }
            };
        }
        let rhs = self.escape_scoped(rhs_handle);
        let fid = rhs
            .as_function()
            .or_else(|| rhs.as_closure(&self.gc_heap).map(|c| c.cached_function_id));
        if let Some(function_id) = fid {
            let owner = rhs.as_closure(&self.gc_heap);
            let value = self.function_property_get_with_receiver(
                stack,
                context,
                owner,
                function_id,
                Some(rhs),
                "prototype",
            )?;
            return if value.is_object_type() || value.is_proxy() {
                Ok(Some(value))
            } else {
                Err(self.err_type(("instanceof prototype is not an object".to_string()).into()))
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
                        let receiver = self.escape_scoped(rhs_handle);
                        self.run_callable_sync_rooted(stack, context, &getter, receiver, args)?
                    }
                    _ => Value::undefined(),
                },
                None => Value::undefined(),
            };
            return if value.is_object_type() || value.is_proxy() {
                Ok(Some(value))
            } else {
                Err(self.err_type(("instanceof prototype is not an object".to_string()).into()))
            };
        }
        Ok(None)
    }

    pub(crate) fn value_has_proxy_aware_prototype(
        &mut self,
        stack: &mut ActivationStack,
        context: &ExecutionContext,
        lhs: Value,
        target_proto: &Value,
    ) -> Result<bool, VmError> {
        self.with_handle_scope(|interp, scope| {
            let current_handle = interp.scoped_value(scope, lhs);
            let target_handle = interp.scoped_value(scope, *target_proto);
            for hops in 0..object::PROTO_CHAIN_HARD_CAP {
                let current = interp.escape_scoped(current_handle);
                let next = interp.ordinary_get_prototype_value(stack, context, current, hops)?;
                interp.set_scoped(current_handle, next);
                let current = interp.escape_scoped(current_handle);
                if current.is_null() {
                    return Ok(false);
                }
                let target_proto = interp.escape_scoped(target_handle);
                if abstract_ops::same_value(&current, &target_proto, &interp.gc_heap) {
                    return Ok(true);
                }
            }
            Ok(false)
        })
    }

    /// The `[[Prototype]]` an Array exotic object inherits from: a
    /// per-instance override (a `class X extends Array` instance points
    /// at `X.prototype`) when present, otherwise the realm's
    /// %Array.prototype%. `null` for `extends null` is preserved.
    fn array_get_prototype_value(&mut self, arr: crate::array::JsArray) -> Result<Value, VmError> {
        match crate::array::prototype_override(arr, &self.gc_heap) {
            Some(proto) => Ok(proto),
            None => self.constructor_prototype_value("Array"),
        }
    }

    pub(crate) fn get_own_property_descriptor_for_value(
        &mut self,
        stack: &mut ActivationStack,
        context: &ExecutionContext,
        target: Value,
        key: Option<&Value>,
    ) -> Result<Option<object::PropertyDescriptor>, VmError> {
        let key =
            self.to_property_key_sync(stack, context, key.cloned().unwrap_or(Value::undefined()))?;
        self.ordinary_get_own_property_descriptor_value(stack, context, target, &key, 0)
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
        stack: &mut ActivationStack,
        context: &ExecutionContext,
        value: Value,
    ) -> Result<VmPropertyKey<'static>, VmError> {
        if abstract_ops::is_primitive(&value) {
            return primitive_to_property_key(value, &self.gc_heap);
        }
        let primitive =
            self.to_primitive_sync(stack, context, value, abstract_ops::ToPrimitiveHint::String)?;
        primitive_to_property_key(primitive, &self.gc_heap)
    }

    /// §7.1.1 `ToPrimitive(value, hint)` — synchronous variant. See
    /// [`Self::to_property_key_sync`] for the rationale.
    #[allow(clippy::wrong_self_convention)]
    pub(crate) fn to_primitive_sync(
        &mut self,
        stack: &mut ActivationStack,
        context: &ExecutionContext,
        value: Value,
        hint: abstract_ops::ToPrimitiveHint,
    ) -> Result<Value, VmError> {
        self.evaluate_to_primitive(stack, context, &value, hint)
    }

    pub(crate) fn enumerable_own_string_keys_for_value(
        &mut self,
        stack: &mut ActivationStack,
        context: &ExecutionContext,
        target: Value,
        hops: usize,
    ) -> Result<Vec<String>, VmError> {
        if hops >= object::PROTO_CHAIN_HARD_CAP {
            return Ok(Vec::new());
        }
        let target = self.with_handle_scope(|interp, scope| -> Result<Value, VmError> {
            let target = interp.scoped_value(scope, target);
            let current = interp.escape_scoped(target);
            interp.ensure_deferred_namespace_ready(stack, context, &current, true)?;
            Ok(interp.escape_scoped(target))
        })?;
        if let Some(proxy) = target.as_proxy() {
            let trap_args: SmallVec<[Value; 8]> = smallvec::smallvec![proxy.target(&self.gc_heap)];
            let trap_result =
                match self.invoke_proxy_trap(stack, context, &proxy, "ownKeys", trap_args)? {
                    ProxyTrap::Trapped(v) => Some(v),
                    ProxyTrap::NoTrap { .. } => None,
                };
            let keys = if let Some(arr) = trap_result.and_then(|v| v.as_array()) {
                crate::array::with_elements(arr, &self.gc_heap, |elements| elements.to_vec())
            } else if let Some(v) = trap_result {
                if v.is_nullish() {
                    return self.enumerable_own_string_keys_for_value(
                        stack,
                        context,
                        proxy.target(&self.gc_heap),
                        hops + 1,
                    );
                }
                return Err(
                    self.err_type(("Proxy ownKeys trap returned non-array".to_string()).into())
                );
            } else {
                return self.enumerable_own_string_keys_for_value(
                    stack,
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
                let desc = self.ordinary_get_own_property_descriptor_value(
                    stack,
                    context,
                    proxy_root,
                    &VmPropertyKey::OwnedString(name.clone()),
                    hops + 1,
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
                    let desc = self.ordinary_get_own_property_descriptor_value(
                        stack,
                        context,
                        target,
                        &VmPropertyKey::OwnedString(name.clone()),
                        hops + 1,
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
            let own_keys = self.own_property_keys_value(stack, context, &target)?;
            let mut out = Vec::new();
            for key_value in own_keys {
                let Some(name) = key_value.as_string(&self.gc_heap) else {
                    continue;
                };
                let key = name.to_lossy_string(&self.gc_heap);
                if let Some(desc) = self.ordinary_get_own_property_descriptor_value(
                    stack,
                    context,
                    target,
                    &VmPropertyKey::OwnedString(key.clone()),
                    hops + 1,
                )? && desc.enumerable()
                {
                    out.push(key);
                }
            }
            return Ok(out);
        }
        // §23.2.3.* — a TypedArray's enumerable own string keys are its
        // canonical integer indices (all enumerable) in ascending order,
        // followed by any enumerable string-keyed expando properties.
        if let Some(t) = target.as_typed_array(&self.gc_heap) {
            let mut keys = Vec::new();
            if !t.buffer(&self.gc_heap).is_detached(&self.gc_heap) {
                let len = t.length(&self.gc_heap);
                keys.extend((0..len).map(|idx| idx.to_string()));
            }
            if let Some(bag) = t.expando(&self.gc_heap) {
                keys.extend(object::with_properties(bag, &self.gc_heap, |p| {
                    p.enumerable_keys().map(str::to_string).collect::<Vec<_>>()
                }));
            }
            return Ok(keys);
        }
        // §22.2 — a RegExp's only intrinsic own property (`lastIndex`) is
        // non-enumerable, so its enumerable own string keys are exactly the
        // enumerable string-keyed expando properties.
        if let Some(re) = target.as_regexp() {
            let mut keys = Vec::new();
            if let Some(bag) = re.expando(&self.gc_heap) {
                keys.extend(object::with_properties(bag, &self.gc_heap, |p| {
                    p.enumerable_keys().map(str::to_string).collect::<Vec<_>>()
                }));
            }
            return Ok(keys);
        }
        if target.is_map()
            || target.is_set()
            || target.is_weak_map()
            || target.is_weak_set()
            || target.is_generator()
            || target.is_iterator()
        {
            let mut keys = Vec::new();
            let bag = self
                .collection_expando(&target)
                .or_else(|| self.non_gc_exotic_user_props(&target));
            if let Some(bag) = bag {
                keys.extend(object::with_properties(bag, &self.gc_heap, |p| {
                    p.enumerable_keys().map(str::to_string).collect::<Vec<_>>()
                }));
            }
            return Ok(keys);
        }
        let fid = target.as_function().or_else(|| {
            target
                .as_closure(&self.gc_heap)
                .map(|c| c.cached_function_id)
        });
        if let Some(function_id) = fid {
            let owner = target.as_closure(&self.gc_heap);
            let keys = self.ordinary_function_own_property_keys(context, owner, function_id);
            let mut out = Vec::with_capacity(keys.len());
            for key in keys {
                if let Some(desc) = self.ordinary_function_own_property_descriptor(
                    Some(context),
                    owner,
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
        if target.is_temporal() {
            // Enumerable own string keys are exactly the enumerable
            // entries of the lazy expando bag.
            let own_keys = self.own_property_keys_value(stack, context, &target)?;
            let mut out = Vec::new();
            for key_value in own_keys {
                let Some(name) = key_value.as_string(&self.gc_heap) else {
                    continue;
                };
                let key = name.to_lossy_string(&self.gc_heap);
                if let Some(desc) = self.ordinary_get_own_property_descriptor_value(
                    stack,
                    context,
                    target,
                    &VmPropertyKey::OwnedString(key.clone()),
                    hops + 1,
                )? && desc.enumerable()
                {
                    out.push(key);
                }
            }
            return Ok(out);
        }
        // §10.2 a class constructor keeps its static members on a separate
        // object, so its own keys come from the constructor-aware walk. Without
        // this, `Object.keys` on a class answers nothing at all, even for a
        // plainly enumerable static assigned after the declaration.
        if let Some(class) = target.as_class_constructor() {
            let keys = self.class_constructor_own_property_keys(Some(context), class)?;
            let mut out = Vec::with_capacity(keys.len());
            for key in keys {
                if let Some(descriptor) = self.ordinary_get_own_property_descriptor_value(
                    stack,
                    context,
                    target,
                    &VmPropertyKey::OwnedString(key.clone()),
                    hops + 1,
                )? && descriptor.enumerable()
                {
                    out.push(key);
                }
            }
            return Ok(out);
        }
        Ok(Vec::new())
    }

    pub(crate) fn enumerable_for_in_string_keys_for_value(
        &mut self,
        stack: &mut ActivationStack,
        context: &ExecutionContext,
        target: Value,
    ) -> Result<Vec<String>, VmError> {
        if target.is_nullish() {
            return Ok(Vec::new());
        }
        let target = self.with_handle_scope(|interp, scope| -> Result<Value, VmError> {
            let target = interp.scoped_value(scope, target);
            let current = interp.escape_scoped(target);
            interp.ensure_deferred_namespace_ready(stack, context, &current, true)?;
            Ok(interp.escape_scoped(target))
        })?;

        let mut current = target;
        let mut visited = BTreeSet::new();
        let mut out = Vec::new();

        // §14.7.5.9 ForIn/OfHeadEvaluation — `for (x in v)` enumerates
        // ToObject(v). A primitive has no enumerable own properties
        // except a String's index keys (its `length` is non-enumerable);
        // continue the prototype walk from the wrapper prototype rather
        // than letting [[GetPrototypeOf]] reject the primitive.
        if !current.is_object_type() {
            if let Some(s) = current.as_string(&self.gc_heap) {
                for idx in 0..s.len() {
                    let name = idx.to_string();
                    if visited.insert(name.clone()) {
                        out.push(name);
                    }
                }
            }
            match self.intrinsic_prototype_object_for(&current) {
                Some(proto) => current = Value::object(proto),
                None => return Ok(out),
            }
        }

        for hops in 0..object::PROTO_CHAIN_HARD_CAP {
            if current.is_null() {
                break;
            }

            let keys = self.own_property_keys_value(stack, context, &current)?;
            for key in &keys {
                let Some(name) = key.as_string(&self.gc_heap) else {
                    continue;
                };
                let name = name.to_lossy_string(&self.gc_heap);
                if !visited.insert(name.clone()) {
                    continue;
                }

                let key = VmPropertyKey::OwnedString(name.clone());
                let desc = self.ordinary_get_own_property_descriptor_value(
                    stack,
                    context,
                    current,
                    &key,
                    hops + 1,
                )?;
                if desc
                    .as_ref()
                    .is_some_and(object::PropertyDescriptor::enumerable)
                {
                    out.push(name);
                }
            }

            current = self.ordinary_get_prototype_value(stack, context, current, hops + 1)?;
        }

        Ok(out)
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
