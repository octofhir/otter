//! `[[DefineOwnProperty]]` and integrity levels.
//!
//! # Contents
//! - The value-level define entry and its array-exotic index and named forms.
//! - `SetIntegrityLevel` / `TestIntegrityLevel`.
//! - Descriptor round-tripping to and from ordinary objects.
//!
//! # Invariants
//! - An array define keeps `length` and the dense range consistent before it
//!   reports success, so no partially applied descriptor is observable.
//! - Proxy defines keep the proxy, target, key value, and both descriptors
//!   rooted across trap argument construction, reentry, and invariant checks.

use super::*;
use crate::activation_stack::ActivationStack;
use crate::{
    ExecutionContext, Interpreter, Value, VmError, VmPropertyKey, abstract_ops, array, object,
};
use smallvec::SmallVec;

impl Interpreter {
    /// Run an allocating step (typically a lazy expando-bag ensure) with the
    /// descriptor's payload values parked on the iteration-anchor stack, and
    /// hand back the step's result together with the relocated descriptor.
    /// A raw descriptor held across the allocation would carry pre-move
    /// handles into the define that follows.
    pub(crate) fn with_descriptor_anchored<T>(
        &mut self,
        descriptor: object::PartialPropertyDescriptor,
        step: impl FnOnce(&mut Self) -> Result<T, VmError>,
    ) -> Result<(T, object::PartialPropertyDescriptor), VmError> {
        let mut descriptor = descriptor;
        let slots = [
            descriptor.value.map(|v| self.push_iteration_anchor(v) - 1),
            descriptor.get.map(|v| self.push_iteration_anchor(v) - 1),
            descriptor.set.map(|v| self.push_iteration_anchor(v) - 1),
        ];
        let base = slots.iter().flatten().min().copied();
        let outcome = step(self);
        if let Some(slot) = slots[0] {
            descriptor.value = Some(self.iteration_anchor(slot));
        }
        if let Some(slot) = slots[1] {
            descriptor.get = Some(self.iteration_anchor(slot));
        }
        if let Some(slot) = slots[2] {
            descriptor.set = Some(self.iteration_anchor(slot));
        }
        if let Some(base) = base {
            self.pop_iteration_anchors_to(base);
        }
        Ok((outcome?, descriptor))
    }

    pub(crate) fn define_own_property_value(
        &mut self,
        stack: &mut ActivationStack,
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
                return Err(self.err_type(
                    ("Cannot define private field on a non-extensible object".to_string()).into(),
                ));
            }
        }
        // Array index protector, tripped by either of two observations and
        // never reset:
        //
        // 1. An accessor landing on an array-index key *anywhere* (most
        //    relevantly %Array.prototype% / %Object.prototype%) forces array
        //    element writes onto the OrdinarySet prototype-walk slow path.
        // 2. Any indexed property — data included — landing on %Array.prototype%
        //    or %Object.prototype%. An ordinary dense array inherits from
        //    exactly those two objects, so an in-bounds hole can then resolve
        //    to an inherited value instead of `undefined`, and the fast
        //    hole-read shortcut must stop taking it.
        //
        // The second condition is deliberately restricted to the two realm
        // prototypes: an indexed data property on some unrelated plain object
        // cannot be reached from a dense array with no per-instance prototype
        // override, and tripping on those would latch the protector for
        // ordinary object use.
        if key
            .string_name()
            .is_some_and(|name| crate::object::array_index_property_name(name).is_some())
            && (descriptor.get.is_some()
                || descriptor.set.is_some()
                || self.is_realm_element_prototype(*target))
        {
            self.activate_array_index_accessor_protector();
        }
        let (target, descriptor) = self.with_handle_scope(
            |interp, scope| -> Result<(Value, object::PartialPropertyDescriptor), VmError> {
                let target_handle = interp.scoped_value(scope, *target);
                let value_handle = descriptor
                    .value
                    .map(|value| interp.scoped_value(scope, value));
                let get_handle = descriptor
                    .get
                    .map(|value| interp.scoped_value(scope, value));
                let set_handle = descriptor
                    .set
                    .map(|value| interp.scoped_value(scope, value));
                let current = interp.escape_scoped(target_handle);
                interp.ensure_deferred_namespace_ready(
                    stack,
                    context,
                    &current,
                    !Self::deferred_key_is_symbol_like(key),
                )?;
                let mut descriptor = descriptor;
                descriptor.value = value_handle.map(|value| interp.escape_scoped(value));
                descriptor.get = get_handle.map(|value| interp.escape_scoped(value));
                descriptor.set = set_handle.map(|value| interp.escape_scoped(value));
                Ok((interp.escape_scoped(target_handle), descriptor))
            },
        )?;
        let target = &target;
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
                return Err(self.err_type(
                    ("Cannot perform 'defineProperty' on a proxy that has been revoked"
                        .to_string())
                    .into(),
                ));
            }
            let scope_frame = crate::handles::HandleScopeFrame::enter(self);
            let scope = scope_frame.token();
            let proxy_root = self.scoped_value(&scope, *target);
            let target_root = self.scoped_value(&scope, proxy.target(&self.gc_heap));
            let value_root = descriptor
                .value
                .map(|value| self.scoped_value(&scope, value));
            let get_root = descriptor.get.map(|value| self.scoped_value(&scope, value));
            let set_root = descriptor.set.map(|value| self.scoped_value(&scope, value));
            let current_descriptor = |interp: &Self| {
                let mut current = descriptor.clone();
                current.value = value_root.map(|value| interp.escape_scoped(value));
                current.get = get_root.map(|value| interp.escape_scoped(value));
                current.set = set_root.map(|value| interp.escape_scoped(value));
                current
            };
            let key_value = self.vm_property_key_to_value(key)?;
            let key_root = self.scoped_value(&scope, key_value);
            let descriptor_object =
                self.partial_descriptor_to_object(&current_descriptor(self), &[])?;
            let trap_args: SmallVec<[Value; 8]> = smallvec::smallvec![
                self.escape_scoped(target_root),
                self.escape_scoped(key_root),
                Value::object(descriptor_object),
            ];
            let proxy = self
                .escape_scoped(proxy_root)
                .as_proxy()
                .ok_or(VmError::InvalidOperand)?;
            return match self.invoke_proxy_trap(
                stack,
                context,
                &proxy,
                "defineProperty",
                trap_args,
            )? {
                crate::object_internal_ops::ProxyTrap::Trapped(result) => {
                    let ok = result.to_boolean(&self.gc_heap);
                    if !ok {
                        return Ok(false);
                    }
                    let mut target_desc = self.ordinary_get_own_property_descriptor_value(
                        stack,
                        context,
                        self.escape_scoped(target_root),
                        key,
                        0,
                    )?;
                    let target_payloads = target_desc.as_ref().map(|desc| match desc.kind {
                        object::DescriptorKind::Data { value } => {
                            [Some(self.scoped_value(&scope, value)), None]
                        }
                        object::DescriptorKind::Accessor { getter, setter } => [
                            getter.map(|v| self.scoped_value(&scope, v)),
                            setter.map(|v| self.scoped_value(&scope, v)),
                        ],
                    });
                    let extensible =
                        self.is_extensible_value(stack, context, &self.escape_scoped(target_root))?;
                    if let (Some(desc), Some([first, second])) = (&mut target_desc, target_payloads)
                    {
                        desc.kind = match desc.kind {
                            object::DescriptorKind::Accessor { .. } => {
                                object::DescriptorKind::Accessor {
                                    getter: first.map(|v| self.escape_scoped(v)),
                                    setter: second.map(|v| self.escape_scoped(v)),
                                }
                            }
                            object::DescriptorKind::Data { .. } => object::DescriptorKind::Data {
                                value: self.escape_scoped(first.expect("data descriptor value")),
                            },
                        };
                    }
                    let descriptor = current_descriptor(self);
                    // §10.5.6 step 16 — settingConfigFalse is true only
                    // when the descriptor EXPLICITLY carries
                    // [[Configurable]]: false; an absent field never
                    // counts (Reflect.set's receiver-define passes
                    // partial descriptors).
                    let setting_config_false = matches!(descriptor.configurable, Some(false));
                    match target_desc.as_ref() {
                        None => {
                            if !extensible {
                                return Err(self.err_type((
                                            "Proxy defineProperty trap added a property on a non-extensible target"
                                                .to_string()).into()));
                            }
                            if setting_config_false {
                                return Err(self.err_type((
                                            "Proxy defineProperty trap added a non-configurable property absent on the target"
                                                .to_string()).into()));
                            }
                        }
                        Some(target_desc) => {
                            let target_configurable = target_desc.configurable();
                            if !target_configurable && matches!(descriptor.configurable, Some(true))
                            {
                                return Err(self.err_type((
                                            "Proxy defineProperty trap relaxed a non-configurable target descriptor"
                                                .to_string()).into()));
                            }
                            if target_configurable && matches!(descriptor.configurable, Some(false))
                            {
                                return Err(self.err_type((
                                            "Proxy defineProperty trap demoted a configurable target descriptor"
                                                .to_string()).into()));
                            }
                            if !target_configurable
                                && target_desc.is_data()
                                && target_desc.writable()
                                && matches!(descriptor.writable, Some(false))
                            {
                                return Err(self.err_type((
                                            "Proxy defineProperty trap narrowed writable on a non-configurable data target"
                                                .to_string()).into()));
                            }
                            if !is_compatible_partial_descriptor(
                                target_desc,
                                &descriptor,
                                &self.gc_heap,
                            ) {
                                return Err(self.err_type(
                                    ("Proxy defineProperty trap returned incompatible descriptor"
                                        .to_string())
                                    .into(),
                                ));
                            }
                        }
                    }
                    Ok(true)
                }
                crate::object_internal_ops::ProxyTrap::NoTrap {
                    target: fallthrough_target,
                } => {
                    // Trap missing — fall through to target.
                    self.define_own_property_value(
                        stack,
                        context,
                        &fallthrough_target,
                        key,
                        current_descriptor(self),
                    )
                }
            };
        }
        if let Some(mut obj) = target.as_object() {
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
                object::define_own_symbol_property_partial(
                    &mut obj,
                    &mut self.gc_heap,
                    *sym,
                    descriptor,
                )
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
                self.define_own_property_partial(&mut obj, k, descriptor)?
            });
        }
        if let Some(native) = target.as_native_function() {
            return Ok(if let VmPropertyKey::Symbol(sym) = key {
                native.define_own_symbol_property(&mut self.gc_heap, *sym, descriptor)
            } else {
                let k = key
                    .string_name()
                    .expect("non-symbol key has string spelling");
                native.define_own_property_partial(&mut self.gc_heap, k, descriptor)?
            });
        }
        if let Some(class) = target.as_class_constructor() {
            let mut statics = class.statics(&self.gc_heap);
            return Ok(if let VmPropertyKey::Symbol(sym) = key {
                object::define_own_symbol_property_partial(
                    &mut statics,
                    &mut self.gc_heap,
                    *sym,
                    descriptor,
                )
            } else {
                let k = key
                    .string_name()
                    .expect("non-symbol key has string spelling");
                self.define_own_property_partial(&mut statics, k, descriptor)?
            });
        }
        let fid = target.as_function().or_else(|| {
            target
                .as_closure(&self.gc_heap)
                .map(|c| c.cached_function_id)
        });
        if let Some(function_id) = fid {
            let owner = target.as_closure(&self.gc_heap);
            if let VmPropertyKey::Symbol(sym) = key {
                let (mut bag, descriptor) = self.with_descriptor_anchored(descriptor, |this| {
                    this.function_user_bag(stack, owner, function_id, &[])
                })?;
                return Ok(object::define_own_symbol_property_partial(
                    &mut bag,
                    &mut self.gc_heap,
                    *sym,
                    descriptor,
                ));
            }
            let Some(k) = key.string_name() else {
                return Ok(false);
            };
            // Materialize a virtual `prototype` into the function's own
            // property bag first, so the redefinition below validates
            // against its real descriptor (writable per the function
            // kind, configurable:false). Without this the lookup returns
            // `None` and the non-configurable invariant check is skipped,
            // wrongly letting `defineProperty(fn, "prototype", {set})` (a
            // data→accessor change) or a `configurable:true` flip succeed.
            if k == "prototype" {
                let _ = self.function_property_get_with_receiver(
                    stack,
                    context,
                    owner,
                    function_id,
                    None,
                    "prototype",
                )?;
            }
            let completed = match self.ordinary_function_own_property_descriptor(
                Some(context),
                owner,
                function_id,
                k,
            )? {
                Some(current) => descriptor.complete_against_current(&current),
                None => descriptor.complete_for_new_property(),
            };
            return self.ordinary_function_define_own_property(
                stack,
                Some(context),
                owner,
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
                let completed = descriptor.complete_against_current(&current);
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
            let (mut bag, descriptor) = self.with_descriptor_anchored(descriptor, |this| {
                crate::property_dispatch::regexp_ensure_expando_pub(&mut this.gc_heap, &regexp)
            })?;
            return Ok(if let VmPropertyKey::Symbol(sym) = key {
                object::define_own_symbol_property_partial(
                    &mut bag,
                    &mut self.gc_heap,
                    *sym,
                    descriptor,
                )
            } else {
                let k = key
                    .string_name()
                    .expect("non-symbol key has string spelling");
                self.define_own_property_partial(&mut bag, k, descriptor)?
            });
        }
        if target.is_map()
            || target.is_set()
            || target.is_weak_map()
            || target.is_weak_set()
            || target.is_generator()
        {
            let (mut bag, descriptor) = self.with_descriptor_anchored(descriptor, |this| {
                this.collection_ensure_expando(target)
            })?;
            return Ok(if let VmPropertyKey::Symbol(sym) = key {
                object::define_own_symbol_property_partial(
                    &mut bag,
                    &mut self.gc_heap,
                    *sym,
                    descriptor,
                )
            } else {
                let k = key
                    .string_name()
                    .expect("non-symbol key has string spelling");
                self.define_own_property_partial(&mut bag, k, descriptor)?
            });
        }
        if let Some(promise) = target.as_promise() {
            // Promise instances are ordinary objects whose user-defined
            // properties (e.g. a shadowing `then` accessor the combinator
            // resolve path observes) live on a lazily-allocated expando.
            let (mut bag, descriptor) = self.with_descriptor_anchored(descriptor, |this| {
                crate::property_dispatch::promise_ensure_expando_pub(&mut this.gc_heap, &promise)
            })?;
            return Ok(if let VmPropertyKey::Symbol(sym) = key {
                object::define_own_symbol_property_partial(
                    &mut bag,
                    &mut self.gc_heap,
                    *sym,
                    descriptor,
                )
            } else {
                let k = key
                    .string_name()
                    .expect("non-symbol key has string spelling");
                self.define_own_property_partial(&mut bag, k, descriptor)?
            });
        }
        if let Some(dv) = target.as_data_view() {
            // §25.3 — a `DataView` is an ordinary extensible object;
            // `Object.defineProperty(dv, …)` installs onto the expando.
            let (mut bag, descriptor) = self.with_descriptor_anchored(descriptor, |this| {
                crate::property_dispatch::data_view_ensure_expando_pub(&mut this.gc_heap, &dv)
            })?;
            return Ok(if let VmPropertyKey::Symbol(sym) = key {
                object::define_own_symbol_property_partial(
                    &mut bag,
                    &mut self.gc_heap,
                    *sym,
                    descriptor,
                )
            } else {
                let k = key
                    .string_name()
                    .expect("non-symbol key has string spelling");
                self.define_own_property_partial(&mut bag, k, descriptor)?
            });
        }
        if let Some(t) = target.as_temporal(&self.gc_heap) {
            // Temporal instances are ordinary extensible objects; an
            // own property (commonly an accessor shadowing a prototype
            // getter in the spec's conversion-fast-path tests) lands on
            // the lazy expando bag.
            let (mut bag, descriptor) = self.with_descriptor_anchored(descriptor, |this| {
                crate::property_dispatch::temporal_ensure_expando_pub(&mut this.gc_heap, &t)
            })?;
            return Ok(if let VmPropertyKey::Symbol(sym) = key {
                object::define_own_symbol_property_partial(
                    &mut bag,
                    &mut self.gc_heap,
                    *sym,
                    descriptor,
                )
            } else {
                let k = key
                    .string_name()
                    .expect("non-symbol key has string spelling");
                self.define_own_property_partial(&mut bag, k, descriptor)?
            });
        }
        if target.is_intl() || target.is_iterator() {
            let (bag, descriptor) = self.with_descriptor_anchored(descriptor, |this| {
                this.ensure_non_gc_exotic_user_props(target)
            })?;
            let Some(mut bag) = bag else {
                return Ok(false);
            };
            return Ok(if let VmPropertyKey::Symbol(sym) = key {
                object::define_own_symbol_property_partial(
                    &mut bag,
                    &mut self.gc_heap,
                    *sym,
                    descriptor,
                )
            } else {
                let k = key
                    .string_name()
                    .expect("non-symbol key has string spelling");
                self.define_own_property_partial(&mut bag, k, descriptor)?
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
                    // Both coercions can run user code; the candidate value
                    // rides an anchor slot so the second read is not stale.
                    let v_slot = self.push_iteration_anchor(v) - 1;
                    let outcome = (|this: &mut Self| {
                        let v = this.iteration_anchor(v_slot);
                        let number_for_uint =
                            crate::coerce::to_number_or_throw(this, stack, context, &v)?;
                        let new_len = crate::number::bitwise::to_uint32(number_for_uint);
                        let v = this.iteration_anchor(v_slot);
                        let number_len =
                            crate::coerce::to_number_or_throw(this, stack, context, &v)?;
                        if (new_len as f64) != number_len.as_f64() {
                            return Err(this.err_range(("Invalid array length".to_string()).into()));
                        }
                        Ok(new_len as usize)
                    })(self);
                    self.pop_iteration_anchors_to(v_slot);
                    Some(outcome?)
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
                let (mut bag, descriptor) = self.with_descriptor_anchored(descriptor, |this| {
                    crate::property_dispatch::typed_array_ensure_expando_pub(&mut this.gc_heap, &t)
                })?;
                return Ok(object::define_own_symbol_property_partial(
                    &mut bag,
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
                    let coerced =
                        self.typed_array_coerce_element(stack, context, t.kind(), value)?;
                    t.set(&mut self.gc_heap, idx, &coerced);
                }
                return Ok(true);
            }
            let (mut bag, descriptor) = self.with_descriptor_anchored(descriptor, |this| {
                crate::property_dispatch::typed_array_ensure_expando_pub(&mut this.gc_heap, &t)
            })?;
            return self.define_own_property_partial(&mut bag, name, descriptor);
        }
        // ArrayBuffer / SharedArrayBuffer and DataView are ordinary
        // objects (no exotic [[DefineOwnProperty]]); own properties live
        // on a lazily-allocated expando bag, mirroring the set/get path.
        if let Some(b) = target.as_array_buffer() {
            let (mut bag, descriptor) = self.with_descriptor_anchored(descriptor, |this| {
                crate::property_dispatch::array_buffer_ensure_expando_pub(&mut this.gc_heap, &b)
            })?;
            return match key {
                VmPropertyKey::Symbol(sym) => Ok(object::define_own_symbol_property_partial(
                    &mut bag,
                    &mut self.gc_heap,
                    *sym,
                    descriptor,
                )),
                _ => match key.string_name() {
                    Some(name) => self.define_own_property_partial(&mut bag, name, descriptor),
                    None => Ok(false),
                },
            };
        }
        if let Some(dv) = target.as_data_view() {
            let (mut bag, descriptor) = self.with_descriptor_anchored(descriptor, |this| {
                crate::property_dispatch::data_view_ensure_expando_pub(&mut this.gc_heap, &dv)
            })?;
            return match key {
                VmPropertyKey::Symbol(sym) => Ok(object::define_own_symbol_property_partial(
                    &mut bag,
                    &mut self.gc_heap,
                    *sym,
                    descriptor,
                )),
                _ => match key.string_name() {
                    Some(name) => self.define_own_property_partial(&mut bag, name, descriptor),
                    None => Ok(false),
                },
            };
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
        stack: &mut ActivationStack,
        context: &ExecutionContext,
        target: &Value,
        level: ObjectIntegrityLevel,
    ) -> Result<bool, VmError> {
        // §7.3.15 steps 3-4 — `[[PreventExtensions]]` runs *before*
        // `[[OwnPropertyKeys]]` (observable through Proxy trap order).
        if !self.prevent_extensions_value(stack, context, target)? {
            return Ok(false);
        }
        let keys = self.own_property_keys_value(stack, context, target)?;
        for key_value in &keys {
            let key = property_key_value_to_vm_key(self, key_value, &self.gc_heap)?;
            let descriptor = match level {
                ObjectIntegrityLevel::Sealed => object::PartialPropertyDescriptor {
                    configurable: Some(false),
                    ..Default::default()
                },
                ObjectIntegrityLevel::Frozen => {
                    let current = self.ordinary_get_own_property_descriptor_value(
                        stack, context, *target, &key, 0,
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
            // §7.3.15 step 5.b / 6.b use DefinePropertyOrThrow, so a
            // rejected redefinition throws a TypeError rather than making
            // `SetIntegrityLevel` report `false`. This is what makes
            // `Object.freeze`/`seal` throw on a non-empty TypedArray: its
            // integer-indexed elements cannot be made non-configurable /
            // non-writable, so `[[DefineOwnProperty]]` returns false.
            if !self.define_own_property_value(stack, context, target, &key, descriptor)? {
                return Err(self.err_type(
                    ("Cannot redefine property during SetIntegrityLevel".to_string()).into(),
                ));
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
        stack: &mut ActivationStack,
        context: &ExecutionContext,
        target: &Value,
        level: ObjectIntegrityLevel,
    ) -> Result<bool, VmError> {
        if self.is_extensible_value(stack, context, target)? {
            return Ok(false);
        }
        let keys = self.own_property_keys_value(stack, context, target)?;
        for key_value in &keys {
            let key = property_key_value_to_vm_key(self, key_value, &self.gc_heap)?;
            let desc =
                self.ordinary_get_own_property_descriptor_value(stack, context, *target, &key, 0)?;
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

    pub(crate) fn define_array_named_property(
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

    pub(crate) fn define_array_index_property(
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

    pub(crate) fn store_array_index_descriptor(
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

    pub(crate) fn store_array_named_descriptor(
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
    pub(crate) fn partial_descriptor_to_object(
        &mut self,
        descriptor: &object::PartialPropertyDescriptor,
        value_roots: &[&Value],
    ) -> Result<object::JsObject, VmError> {
        self.with_handle_scope(|interp, scope| {
            for value in value_roots {
                let _ = interp.scoped_value(scope, **value);
            }
            let value = descriptor
                .value
                .map(|value| interp.scoped_value(scope, value));
            let get = descriptor
                .get
                .map(|value| interp.scoped_value(scope, value));
            let set = descriptor
                .set
                .map(|value| interp.scoped_value(scope, value));
            let obj = interp.scoped_object(scope)?;
            if let Some(value) = value {
                interp.scoped_set(scope, obj, "value", value)?;
            }
            if let Some(writable) = descriptor.writable {
                let writable = interp.scoped_boolean(scope, writable);
                interp.scoped_set(scope, obj, "writable", writable)?;
            }
            if let Some(get) = get {
                interp.scoped_set(scope, obj, "get", get)?;
            }
            if let Some(set) = set {
                interp.scoped_set(scope, obj, "set", set)?;
            }
            if let Some(enumerable) = descriptor.enumerable {
                let enumerable = interp.scoped_boolean(scope, enumerable);
                interp.scoped_set(scope, obj, "enumerable", enumerable)?;
            }
            if let Some(configurable) = descriptor.configurable {
                let configurable = interp.scoped_boolean(scope, configurable);
                interp.scoped_set(scope, obj, "configurable", configurable)?;
            }
            interp
                .escape_scoped(obj)
                .as_object()
                .ok_or(VmError::TypeMismatch)
        })
    }
}
