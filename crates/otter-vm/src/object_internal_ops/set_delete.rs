//! `[[Set]]` and `[[Delete]]`.
//!
//! # Contents
//! - The ordinary data-store entry and the value-level delete.
//! - The prototype-chain proxy probe both consult first.
//!
//! # Invariants
//! - A setter along the prototype chain can re-enter JavaScript; the receiver
//!   is re-read from its rooted slot after any such call.
//! - Proxy key materialization and trap reentry keep target, value, receiver,
//!   and symbol keys in one handle scope until invariant checks complete.

use crate::activation_stack::ActivationStack;
use crate::{
    ExecutionContext, Interpreter, Value, VmError, VmPropertyKey, abstract_ops, array,
    function_metadata, object, regexp_prototype,
};
use smallvec::SmallVec;

impl Interpreter {
    pub(crate) fn ordinary_delete_value(
        &mut self,
        stack: &mut ActivationStack,
        context: &ExecutionContext,
        target: Value,
        key: &VmPropertyKey,
        hops: usize,
    ) -> Result<bool, VmError> {
        if hops >= object::PROTO_CHAIN_HARD_CAP {
            return Ok(true);
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
            return match self.invoke_proxy_trap(
                stack,
                context,
                &proxy,
                "deleteProperty",
                trap_args,
            )? {
                crate::object_internal_ops::ProxyTrap::Trapped(value) => {
                    let result = value.to_boolean(&self.gc_heap);
                    if !result {
                        return Ok(false);
                    }
                    let target_value = proxy.target(&self.gc_heap);
                    let target_desc = self.ordinary_get_own_property_descriptor_value(
                        stack,
                        context,
                        target_value,
                        key,
                        hops + 1,
                    )?;
                    if let Some(desc) = target_desc {
                        if !desc.configurable() {
                            return Err(self.err_type((
                                    "Proxy deleteProperty trap returned true but target has the property as non-configurable"
                                        .to_string()).into()));
                        }
                        let target_extensible =
                            self.is_extensible_value(stack, context, &target_value)?;
                        if !target_extensible {
                            return Err(self.err_type((
                                    "Proxy deleteProperty trap returned true but target is non-extensible"
                                        .to_string()).into()));
                        }
                    }
                    Ok(true)
                }
                crate::object_internal_ops::ProxyTrap::NoTrap {
                    target: fallthrough_target,
                } => self.ordinary_delete_value(stack, context, fallthrough_target, key, hops + 1),
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
            let owner = target.as_closure(&self.gc_heap);
            return Ok(if let Some(key) = key.string_name() {
                let has_prototype = context.function_has_prototype_property(function_id);
                self.ordinary_function_delete_own_property(owner, function_id, key, has_prototype)
            } else if let VmPropertyKey::Symbol(sym) = key {
                self.callable_bag_read(owner, function_id)
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
        if let Some(t) = target.as_temporal(&self.gc_heap) {
            // Only ordinary expando entries are deletable; there are no
            // own non-configurable internal slots exposed as properties.
            if let Some(bag) = t.expando(&self.gc_heap) {
                return Ok(if let Some(name) = key.string_name() {
                    object::delete(bag, &mut self.gc_heap, name)
                } else if let VmPropertyKey::Symbol(sym) = key {
                    object::delete_symbol(bag, &mut self.gc_heap, *sym)
                } else {
                    true
                });
            }
            return Ok(true);
        }
        if target.is_map()
            || target.is_set()
            || target.is_weak_map()
            || target.is_weak_set()
            || target.is_generator()
        {
            // Only ordinary expando entries are deletable; size and the
            // iterator methods are non-own prototype properties.
            if let Some(bag) = self.collection_expando(&target) {
                return Ok(if let Some(name) = key.string_name() {
                    object::delete(bag, &mut self.gc_heap, name)
                } else if let VmPropertyKey::Symbol(sym) = key {
                    object::delete_symbol(bag, &mut self.gc_heap, *sym)
                } else {
                    true
                });
            }
            return Ok(true);
        }
        Ok(true)
    }

    /// Execute the value-level `[[Set]](key, value, receiver)` operation.
    ///
    /// Specialised exotic objects run their own internal methods above the
    /// generic tail. Every remaining object-like value resolves its own
    /// descriptor, prototype, and receiver phase through the shared internal
    /// method helpers, so interpreter opcodes and JIT slow transitions do not
    /// carry parallel property semantics.
    pub(crate) fn ordinary_set_data_value(
        &mut self,
        stack: &mut ActivationStack,
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
        let (target, value, receiver) =
            self.with_handle_scope(|interp, scope| -> Result<(Value, Value, Value), VmError> {
                let target = interp.scoped_value(scope, target);
                let value = interp.scoped_value(scope, value);
                let receiver = interp.scoped_value(scope, receiver);
                let current = interp.escape_scoped(target);
                interp.ensure_deferred_namespace_ready(
                    stack,
                    context,
                    &current,
                    !Self::deferred_key_is_symbol_like(key),
                )?;
                Ok((
                    interp.escape_scoped(target),
                    interp.escape_scoped(value),
                    interp.escape_scoped(receiver),
                ))
            })?;
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
        if target.as_typed_array(&self.gc_heap).is_some() {
            // The lazy expando ensure allocates, so the incoming value,
            // receiver, and target ride anchor slots and are re-read after
            // every ensure.
            let value_slot = self.push_iteration_anchor(value) - 1;
            let base = value_slot;
            let receiver_slot = self.push_iteration_anchor(receiver) - 1;
            let target_slot = self.push_iteration_anchor(target) - 1;
            let outcome = (|this: &mut Self| -> Result<bool, VmError> {
                let anchored_ta = |this: &Self| {
                    this.iteration_anchor(target_slot)
                        .as_typed_array(&this.gc_heap)
                        .expect("target stays a typed array across the anchored steps")
                };
                match key {
                    VmPropertyKey::Symbol(sym) => {
                        let t = anchored_ta(this);
                        let bag = crate::property_dispatch::typed_array_ensure_expando(this, &t)?;
                        let value = this.iteration_anchor(value_slot);
                        Ok(object::set_symbol(bag, &mut this.gc_heap, *sym, value))
                    }
                    _ => {
                        let name = key
                            .string_name()
                            .expect("non-symbol key has string spelling")
                            .to_string();
                        let t = anchored_ta(this);
                        if let Some(n) =
                            crate::property_dispatch::canonical_numeric_index_string(&name)
                        {
                            let receiver = this.iteration_anchor(receiver_slot);
                            let same_receiver = receiver
                                .as_typed_array(&this.gc_heap)
                                .is_some_and(|r| r == t);
                            if same_receiver {
                                let value = this.iteration_anchor(value_slot);
                                let coerced = this.typed_array_coerce_element(
                                    stack,
                                    context,
                                    t.kind(),
                                    value,
                                )?;
                                let t = anchored_ta(this);
                                if let Some(idx) = crate::property_dispatch::typed_array_valid_index(
                                    &t,
                                    &this.gc_heap,
                                    n,
                                ) {
                                    t.set(&mut this.gc_heap, idx, &coerced);
                                }
                                return Ok(true);
                            }
                            if crate::property_dispatch::typed_array_valid_index(
                                &t,
                                &this.gc_heap,
                                n,
                            )
                            .is_none()
                            {
                                return Ok(true);
                            }
                            // Valid target index + foreign receiver —
                            // §10.1.9.2 receiver phase (GetOwnProperty +
                            // DefineOwnProperty on the receiver, never its
                            // [[Set]]).
                            let value = this.iteration_anchor(value_slot);
                            let receiver = this.iteration_anchor(receiver_slot);
                            return this
                                .ordinary_set_on_receiver(stack, context, key, value, &receiver);
                        }
                        let mut bag =
                            crate::property_dispatch::typed_array_ensure_expando(this, &t)?;
                        // OrdinarySet on the expando: an own non-writable
                        // data property rejects, an own accessor invokes
                        // its setter (receiver = the typed array), and a
                        // fresh key requires the bag to be extensible.
                        let t = anchored_ta(this);
                        let receiver = this.iteration_anchor(receiver_slot);
                        let same_receiver = receiver
                            .as_typed_array(&this.gc_heap)
                            .is_some_and(|r| r == t);
                        match object::lookup_own(bag, &this.gc_heap, &name) {
                            object::PropertyLookup::Data { flags, .. } => {
                                if !flags.writable() {
                                    return Ok(false);
                                }
                                let value = this.iteration_anchor(value_slot);
                                if !same_receiver {
                                    // §10.1.9.2 — own writable data on the
                                    // chain: the write lands on the
                                    // RECEIVER, never the holder.
                                    return this.ordinary_set_on_receiver(
                                        stack, context, key, value, &receiver,
                                    );
                                }
                                object::set(&mut bag, &mut this.gc_heap, &name, value);
                                Ok(true)
                            }
                            object::PropertyLookup::Accessor { setter, .. } => {
                                let Some(setter) = setter else {
                                    return Ok(false);
                                };
                                let value = this.iteration_anchor(value_slot);
                                let argv: SmallVec<[Value; 8]> = smallvec::smallvec![value];
                                this.run_callable_sync_rooted(
                                    stack, context, &setter, receiver, argv,
                                )?;
                                Ok(true)
                            }
                            object::PropertyLookup::Absent => {
                                // §10.1.9 step 2 — own miss continues the
                                // walk through the typed array's
                                // [[Prototype]] (a setter on
                                // %TypedArray.prototype% must fire); only
                                // a fully-absent chain defines on the
                                // receiver.
                                let target = this.iteration_anchor(target_slot);
                                let parent = this.get_prototype_for_op(&target)?;
                                let value = this.iteration_anchor(value_slot);
                                let receiver = this.iteration_anchor(receiver_slot);
                                if parent.is_null() || parent.is_undefined() {
                                    return this.ordinary_set_on_receiver(
                                        stack, context, key, value, &receiver,
                                    );
                                }
                                this.ordinary_set_data_value(
                                    stack,
                                    context,
                                    parent,
                                    key,
                                    value,
                                    receiver,
                                    hops + 1,
                                )
                            }
                        }
                    }
                }
            })(self);
            self.pop_iteration_anchors_to(base);
            return outcome;
        }
        if target.as_proxy().is_some() {
            return self.with_handle_scope(|interp, scope| {
                let proxy_root = interp.scoped_value(scope, target);
                let value_root = interp.scoped_value(scope, value);
                let receiver_root = interp.scoped_value(scope, receiver);
                let proxy = interp
                    .escape_scoped(proxy_root)
                    .as_proxy()
                    .expect("proxy branch keeps a proxy target");
                if proxy.is_revoked(&interp.gc_heap) {
                    return Err(interp.err_type(
                        ("Cannot perform 'set' on a proxy that has been revoked".to_string())
                            .into(),
                    ));
                }

                // ToPropertyKey materialization allocates for named keys. Park
                // every operand before it so the trap receives relocated
                // target/value/receiver handles rather than pre-move copies.
                let key_value = interp.vm_property_key_to_value(key)?;
                let key_root = interp.scoped_value(scope, key_value);
                let proxy = interp
                    .escape_scoped(proxy_root)
                    .as_proxy()
                    .expect("rooted proxy remains a proxy");
                let trap_args: SmallVec<[Value; 8]> = smallvec::smallvec![
                    proxy.target(&interp.gc_heap),
                    interp.escape_scoped(key_root),
                    interp.escape_scoped(value_root),
                    interp.escape_scoped(receiver_root),
                ];
                match interp.invoke_proxy_trap(stack, context, &proxy, "set", trap_args)? {
                    crate::object_internal_ops::ProxyTrap::Trapped(result) => {
                        if !result.to_boolean(&interp.gc_heap) {
                            return Ok(false);
                        }
                        let proxy = interp
                            .escape_scoped(proxy_root)
                            .as_proxy()
                            .expect("rooted proxy remains a proxy after trap reentry");
                        let live_key = match key {
                            VmPropertyKey::Symbol(_) => VmPropertyKey::Symbol(
                                interp
                                    .escape_scoped(key_root)
                                    .as_symbol(&interp.gc_heap)
                                    .expect("rooted symbol key remains a symbol"),
                            ),
                            _ => VmPropertyKey::OwnedString(
                                key.string_name()
                                    .expect("non-symbol key has string spelling")
                                    .to_owned(),
                            ),
                        };
                        let target_desc = interp.ordinary_get_own_property_descriptor_value(
                            stack,
                            context,
                            proxy.target(&interp.gc_heap),
                            &live_key,
                            hops + 1,
                        )?;
                        if let Some(desc) = target_desc.as_ref()
                            && !desc.configurable()
                        {
                            match &desc.kind {
                                object::DescriptorKind::Data { value: target_v }
                                    if !desc.writable()
                                        && !abstract_ops::same_value(
                                            target_v,
                                            &interp.escape_scoped(value_root),
                                            &interp.gc_heap,
                                        ) =>
                                {
                                    return Err(interp.err_type((
                                            "Proxy set trap reported success but target is non-configurable non-writable with a different value"
                                                .to_string()).into()));
                                }
                                object::DescriptorKind::Accessor { setter: None, .. } => {
                                    return Err(interp.err_type((
                                            "Proxy set trap reported success but target is a non-configurable accessor without a setter"
                                                .to_string()).into()));
                                }
                                _ => {}
                            }
                        }
                        Ok(true)
                    }
                    crate::object_internal_ops::ProxyTrap::NoTrap {
                        target: fallthrough_target,
                    } => {
                        let live_key = match key {
                            VmPropertyKey::Symbol(_) => VmPropertyKey::Symbol(
                                interp
                                    .escape_scoped(key_root)
                                    .as_symbol(&interp.gc_heap)
                                    .expect("rooted symbol key remains a symbol"),
                            ),
                            _ => VmPropertyKey::OwnedString(
                                key.string_name()
                                    .expect("non-symbol key has string spelling")
                                    .to_owned(),
                            ),
                        };
                        interp.ordinary_set_data_value(
                            stack,
                            context,
                            fallthrough_target,
                            &live_key,
                            interp.escape_scoped(value_root),
                            interp.escape_scoped(receiver_root),
                            hops + 1,
                        )
                    }
                }
            });
        }
        if let Some(arr) = target.as_array() {
            // §10.4.2 arrays inherit OrdinarySet but their receiver
            // phase must route through Array [[DefineOwnProperty]].
            // A raw element/named store would skip extensibility,
            // length, accessor, prototype, and Proxy receiver rules.
            let target_value = Value::array(arr);
            let desc = self.ordinary_get_own_property_descriptor_value(
                stack,
                context,
                target_value,
                key,
                hops + 1,
            )?;
            if let Some(desc) = desc {
                return match desc.kind {
                    object::DescriptorKind::Accessor { setter, .. } => {
                        let Some(setter) = setter else {
                            return Ok(false);
                        };
                        let argv: SmallVec<[Value; 8]> = smallvec::smallvec![value];
                        self.run_callable_sync_rooted(stack, context, &setter, receiver, argv)?;
                        Ok(true)
                    }
                    object::DescriptorKind::Data { .. } => {
                        if !desc.writable() {
                            return Ok(false);
                        }
                        self.ordinary_set_on_receiver(stack, context, key, value, &receiver)
                    }
                };
            }
            let parent = self.get_prototype_for_op(&target_value)?;
            if parent.is_null() || parent.is_undefined() {
                return self.ordinary_set_on_receiver(stack, context, key, value, &receiver);
            }
            return self.ordinary_set_data_value(
                stack,
                context,
                parent,
                key,
                value,
                receiver,
                hops + 1,
            );
        }
        if let Some(obj) = target.as_object() {
            // §7.3.28 PrivateSet — installing a new private element is a hard
            // TypeError on a non-extensible object, independent of the
            // caller's strict-mode assignment policy. Keep this in the
            // value-level `[[Set]]` authority so interpreter and compiled
            // computed stores cannot diverge.
            if let VmPropertyKey::Symbol(sym) = key
                && sym.is_private_name()
                && object::get_own_symbol_descriptor(obj, &self.gc_heap, *sym).is_none()
                && !object::is_extensible(obj, &self.gc_heap)
            {
                return Err(self.err_type(
                    ("Cannot define private member on a non-extensible object".to_string()).into(),
                ));
            }
            if let Some(desc) = self.string_object_exotic_descriptor(obj, key)?
                && !desc.writable()
            {
                return Ok(false);
            }
            // §10.1.9 OrdinarySet — full chain walk: a setter
            // anywhere on the chain fires (receiver-bound), a
            // non-writable slot rejects, an exotic prototype link
            // re-enters this funnel, and a data outcome writes with
            // receiver-phase semantics.
            let outcome = if let VmPropertyKey::Symbol(sym) = key {
                object::resolve_symbol_set(obj, &self.gc_heap, *sym)
            } else {
                object::resolve_set(
                    obj,
                    &self.gc_heap,
                    key.string_name()
                        .expect("non-symbol key has string spelling"),
                )
            };
            return match outcome {
                object::SetOutcome::InvokeSetter { setter } => {
                    let argv: SmallVec<[Value; 8]> = smallvec::smallvec![value];
                    self.run_callable_sync_rooted(stack, context, &setter, receiver, argv)?;
                    Ok(true)
                }
                object::SetOutcome::Reject { .. } => Ok(false),
                object::SetOutcome::ExoticParent { parent } => self.ordinary_set_data_value(
                    stack,
                    context,
                    parent,
                    key,
                    value,
                    receiver,
                    hops + 1,
                ),
                object::SetOutcome::AssignData => {
                    let same_receiver = receiver.as_object().is_some_and(|r| r == obj);
                    if !same_receiver {
                        return self
                            .ordinary_set_on_receiver(stack, context, key, value, &receiver);
                    }
                    Ok(if let VmPropertyKey::Symbol(sym) = key {
                        object::set_symbol(obj, &mut self.gc_heap, *sym, value)
                    } else {
                        self.ordinary_set_data_property(
                            obj,
                            key.string_name()
                                .expect("non-symbol key has string spelling"),
                            value,
                        )?
                    })
                }
            };
        }
        if let Some(re) = target.as_regexp() {
            // Match `lastIndex` by its resolved name, not only the
            // `String` key variant — a write forwarded through a Proxy
            // (or any atomised store) arrives as an `Atom` and must still
            // hit the regex's only own data property.
            if key.string_name() == Some("lastIndex") {
                // A non-writable `lastIndex` rejects the write (returns
                // false → `Set` with Throw=true raises a TypeError);
                // otherwise store the new value.
                if !re.last_index_writable(&self.gc_heap) {
                    return Ok(false);
                }
                regexp_prototype::store_property(&re, &mut self.gc_heap, "lastIndex", value);
                return Ok(true);
            }
            if let Some(bag) = re.expando(&self.gc_heap) {
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
                let same_receiver = receiver
                    .as_regexp()
                    .is_some_and(|receiver| receiver.ptr_eq(&re));
                match lookup {
                    object::PropertyLookup::Data { flags, .. } => {
                        if !flags.writable() {
                            return Ok(false);
                        }
                        if !same_receiver {
                            return self
                                .ordinary_set_on_receiver(stack, context, key, value, &receiver);
                        }
                        return Ok(if let VmPropertyKey::Symbol(sym) = key {
                            object::set_symbol(bag, &mut self.gc_heap, *sym, value)
                        } else {
                            self.ordinary_set_data_property(
                                bag,
                                key.string_name()
                                    .expect("non-symbol key has string spelling"),
                                value,
                            )?
                        });
                    }
                    object::PropertyLookup::Accessor { setter, .. } => {
                        let Some(setter) = setter else {
                            return Ok(false);
                        };
                        return self.with_handle_scope(|interp, scope| {
                            let setter = interp.scoped_value(scope, setter);
                            let receiver = interp.scoped_value(scope, receiver);
                            let value = interp.scoped_value(scope, value);
                            let argv: SmallVec<[Value; 8]> =
                                smallvec::smallvec![interp.escape_scoped(value)];
                            interp.run_callable_sync_rooted(
                                stack,
                                context,
                                &interp.escape_scoped(setter),
                                interp.escape_scoped(receiver),
                                argv,
                            )?;
                            Ok(true)
                        });
                    }
                    object::PropertyLookup::Absent => {}
                }
            }
            let parent = self.get_prototype_for_op(&target)?;
            if parent.is_null() || parent.is_undefined() {
                return self.ordinary_set_on_receiver(stack, context, key, value, &receiver);
            }
            return self.ordinary_set_data_value(
                stack,
                context,
                parent,
                key,
                value,
                receiver,
                hops + 1,
            );
        }
        if target.is_map()
            || target.is_set()
            || target.is_weak_map()
            || target.is_weak_set()
            || target.is_generator()
        {
            // OrdinarySet over the lazy expando: an own writable data
            // slot stores (same receiver) or lands on the receiver; an
            // own accessor invokes its setter; an own miss continues the
            // walk through the collection's [[Prototype]] (Map.prototype
            // / Set.prototype, whose `size` accessor has no setter).
            let bag = self.collection_ensure_expando(&target)?;
            let same_receiver = if target.is_map() {
                receiver
                    .as_map()
                    .zip(target.as_map())
                    .is_some_and(|(r, t)| r == t)
            } else if target.is_generator() {
                receiver
                    .as_generator()
                    .zip(target.as_generator())
                    .is_some_and(|(r, t)| r == t)
            } else {
                receiver
                    .as_set()
                    .zip(target.as_set())
                    .is_some_and(|(r, t)| r == t)
            };
            let lookup = match key {
                VmPropertyKey::Symbol(sym) => object::lookup_own_symbol(bag, &self.gc_heap, *sym),
                _ => object::lookup_own(
                    bag,
                    &self.gc_heap,
                    key.string_name()
                        .expect("non-symbol key has string spelling"),
                ),
            };
            match lookup {
                object::PropertyLookup::Data { flags, .. } => {
                    if !flags.writable() {
                        return Ok(false);
                    }
                    if !same_receiver {
                        return self
                            .ordinary_set_on_receiver(stack, context, key, value, &receiver);
                    }
                    return Ok(if let VmPropertyKey::Symbol(sym) = key {
                        object::set_symbol(bag, &mut self.gc_heap, *sym, value)
                    } else {
                        self.ordinary_set_data_property(
                            bag,
                            key.string_name()
                                .expect("non-symbol key has string spelling"),
                            value,
                        )?
                    });
                }
                object::PropertyLookup::Accessor { setter, .. } => {
                    let Some(setter) = setter else {
                        return Ok(false);
                    };
                    return self.with_handle_scope(|interp, scope| {
                        let setter = interp.scoped_value(scope, setter);
                        let receiver = interp.scoped_value(scope, receiver);
                        let value = interp.scoped_value(scope, value);
                        let argv: SmallVec<[Value; 8]> =
                            smallvec::smallvec![interp.escape_scoped(value)];
                        interp.run_callable_sync_rooted(
                            stack,
                            context,
                            &interp.escape_scoped(setter),
                            interp.escape_scoped(receiver),
                            argv,
                        )?;
                        Ok(true)
                    });
                }
                object::PropertyLookup::Absent => {
                    let parent = self.get_prototype_for_op(&target)?;
                    if parent.is_null() || parent.is_undefined() {
                        return self
                            .ordinary_set_on_receiver(stack, context, key, value, &receiver);
                    }
                    return self.ordinary_set_data_value(
                        stack,
                        context,
                        parent,
                        key,
                        value,
                        receiver,
                        hops + 1,
                    );
                }
            }
        }
        if let Some(t) = target.as_temporal(&self.gc_heap) {
            // OrdinarySet over the expando: an own writable data slot
            // stores (same receiver) or lands on the receiver; an own
            // accessor invokes its setter; an own miss continues the
            // walk through the Temporal instance's real [[Prototype]]
            // (so `dt.year = x`, a getter-only accessor, rejects).
            let mut bag =
                crate::property_dispatch::temporal_ensure_expando_pub(&mut self.gc_heap, &t)?;
            let same_receiver = receiver
                .as_temporal(&self.gc_heap)
                .is_some_and(|r| r.ptr_eq(t));
            let lookup = match key {
                VmPropertyKey::Symbol(sym) => object::lookup_own_symbol(bag, &self.gc_heap, *sym),
                _ => object::lookup_own(
                    bag,
                    &self.gc_heap,
                    key.string_name()
                        .expect("non-symbol key has string spelling"),
                ),
            };
            match lookup {
                object::PropertyLookup::Data { flags, .. } => {
                    if !flags.writable() {
                        return Ok(false);
                    }
                    if !same_receiver {
                        return self
                            .ordinary_set_on_receiver(stack, context, key, value, &receiver);
                    }
                    if let VmPropertyKey::Symbol(sym) = key {
                        object::set_symbol(bag, &mut self.gc_heap, *sym, value);
                    } else {
                        object::set(
                            &mut bag,
                            &mut self.gc_heap,
                            key.string_name()
                                .expect("non-symbol key has string spelling"),
                            value,
                        );
                    }
                    return Ok(true);
                }
                object::PropertyLookup::Accessor { setter, .. } => {
                    let Some(setter) = setter else {
                        return Ok(false);
                    };
                    let argv: SmallVec<[Value; 8]> = smallvec::smallvec![value];
                    self.run_callable_sync_rooted(stack, context, &setter, receiver, argv)?;
                    return Ok(true);
                }
                object::PropertyLookup::Absent => {
                    let parent = self.get_prototype_for_op(&target)?;
                    if parent.is_null() || parent.is_undefined() {
                        return self
                            .ordinary_set_on_receiver(stack, context, key, value, &receiver);
                    }
                    return self.ordinary_set_data_value(
                        stack,
                        context,
                        parent,
                        key,
                        value,
                        receiver,
                        hops + 1,
                    );
                }
            }
        }
        if target.is_iterator() {
            // Builtin iterator objects — ordinary objects whose user
            // properties live in the non-GC side-table bag; the
            // prototype walk stays ordinary (§10.1.9).
            let Some(mut bag) = self.ensure_non_gc_exotic_user_props(&target)? else {
                return Ok(false);
            };
            let same_receiver = receiver
                .as_iterator()
                .zip(target.as_iterator())
                .is_some_and(|(r, t)| r.as_header_ptr() == t.as_header_ptr());
            let lookup = match key {
                VmPropertyKey::Symbol(sym) => object::lookup_own_symbol(bag, &self.gc_heap, *sym),
                _ => object::lookup_own(
                    bag,
                    &self.gc_heap,
                    key.string_name()
                        .expect("non-symbol key has string spelling"),
                ),
            };
            match lookup {
                object::PropertyLookup::Data { flags, .. } => {
                    if !flags.writable() {
                        return Ok(false);
                    }
                    if !same_receiver {
                        return self
                            .ordinary_set_on_receiver(stack, context, key, value, &receiver);
                    }
                    if let VmPropertyKey::Symbol(sym) = key {
                        object::set_symbol(bag, &mut self.gc_heap, *sym, value);
                    } else {
                        object::set(
                            &mut bag,
                            &mut self.gc_heap,
                            key.string_name()
                                .expect("non-symbol key has string spelling"),
                            value,
                        );
                    }
                    return Ok(true);
                }
                object::PropertyLookup::Accessor { setter, .. } => {
                    let Some(setter) = setter else {
                        return Ok(false);
                    };
                    let argv: SmallVec<[Value; 8]> = smallvec::smallvec![value];
                    self.run_callable_sync_rooted(stack, context, &setter, receiver, argv)?;
                    return Ok(true);
                }
                object::PropertyLookup::Absent => {
                    let parent = self.get_prototype_for_op(&target)?;
                    if parent.is_null() || parent.is_undefined() {
                        return self
                            .ordinary_set_on_receiver(stack, context, key, value, &receiver);
                    }
                    return self.ordinary_set_data_value(
                        stack,
                        context,
                        parent,
                        key,
                        value,
                        receiver,
                        hops + 1,
                    );
                }
            }
        }
        if let Some(target_intl) = target.as_intl(&self.gc_heap) {
            // ECMA-402 service objects have internal slots plus normal
            // ordinary own properties. The internal slots live in a
            // non-GC payload, so user properties are stored in a lazy
            // side-table bag and the prototype walk remains ordinary.
            let Some(mut bag) = self.ensure_non_gc_exotic_user_props(&target)? else {
                return Ok(false);
            };
            let same_receiver = receiver
                .as_intl(&self.gc_heap)
                .is_some_and(|receiver_intl| {
                    receiver_intl.identity_addr() == target_intl.identity_addr()
                });
            let lookup = match key {
                VmPropertyKey::Symbol(sym) => object::lookup_own_symbol(bag, &self.gc_heap, *sym),
                _ => object::lookup_own(
                    bag,
                    &self.gc_heap,
                    key.string_name()
                        .expect("non-symbol key has string spelling"),
                ),
            };
            match lookup {
                object::PropertyLookup::Data { flags, .. } => {
                    if !flags.writable() {
                        return Ok(false);
                    }
                    if !same_receiver {
                        return self
                            .ordinary_set_on_receiver(stack, context, key, value, &receiver);
                    }
                    if let VmPropertyKey::Symbol(sym) = key {
                        object::set_symbol(bag, &mut self.gc_heap, *sym, value);
                    } else {
                        object::set(
                            &mut bag,
                            &mut self.gc_heap,
                            key.string_name()
                                .expect("non-symbol key has string spelling"),
                            value,
                        );
                    }
                    return Ok(true);
                }
                object::PropertyLookup::Accessor { setter, .. } => {
                    let Some(setter) = setter else {
                        return Ok(false);
                    };
                    let argv: SmallVec<[Value; 8]> = smallvec::smallvec![value];
                    self.run_callable_sync_rooted(stack, context, &setter, receiver, argv)?;
                    return Ok(true);
                }
                object::PropertyLookup::Absent => {
                    let parent = self.get_prototype_for_op(&target)?;
                    if parent.is_null() || parent.is_undefined() {
                        return self
                            .ordinary_set_on_receiver(stack, context, key, value, &receiver);
                    }
                    return self.ordinary_set_data_value(
                        stack,
                        context,
                        parent,
                        key,
                        value,
                        receiver,
                        hops + 1,
                    );
                }
            }
        }
        if let Some(native) = target.as_native_function() {
            // §10.1.9.1 OrdinarySet over a built-in (native) function
            // object reached as a [[Prototype]] link or proxy target.
            // A miss continues up the function's [[Prototype]]; the
            // write lands on the RECEIVER (e.g. `%AsyncFunction%`
            // inheriting from `%Function%` must gain an own property
            // rather than retargeting `%Function%`).
            let own = match key {
                VmPropertyKey::Symbol(sym) => {
                    native.own_symbol_property_descriptor(&self.gc_heap, *sym)
                }
                _ => {
                    let name = key
                        .string_name()
                        .expect("non-symbol key has string spelling");
                    native.own_property_descriptor(&mut self.gc_heap, name)?
                }
            };
            return match own {
                Some(desc) => match desc.kind {
                    object::DescriptorKind::Accessor { setter, .. } => {
                        let Some(setter) = setter else {
                            return Ok(false);
                        };
                        let argv: SmallVec<[Value; 8]> = smallvec::smallvec![value];
                        self.run_callable_sync_rooted(stack, context, &setter, receiver, argv)?;
                        Ok(true)
                    }
                    object::DescriptorKind::Data { .. } => {
                        if !desc.flags.writable() {
                            return Ok(false);
                        }
                        self.ordinary_set_on_receiver(stack, context, key, value, &receiver)
                    }
                },
                None => {
                    let parent = self.get_prototype_for_op(&target)?;
                    if parent.is_null() || parent.is_undefined() {
                        return self
                            .ordinary_set_on_receiver(stack, context, key, value, &receiver);
                    }
                    self.ordinary_set_data_value(
                        stack,
                        context,
                        parent,
                        key,
                        value,
                        receiver,
                        hops + 1,
                    )
                }
            };
        }
        let fid = target.as_function().or_else(|| {
            target
                .as_closure(&self.gc_heap)
                .map(|c| c.cached_function_id)
        });
        if let Some(function_id) = fid {
            let owner = target.as_closure(&self.gc_heap);
            // §10.1.9.1 OrdinarySet over a function object. Locate the
            // own descriptor, then apply receiver-phase semantics: an
            // own miss continues the walk through the function's
            // [[Prototype]], and a data write always lands on the
            // RECEIVER — which differs from the function when the
            // function itself is a [[Prototype]] link (e.g.
            // `%AsyncFunction%` inheriting from `%Function%`, where the
            // write must create an own property on `%AsyncFunction%`,
            // not silently retarget `%Function%`).
            let own = match key {
                VmPropertyKey::Symbol(sym) => self
                    .callable_bag_read(owner, function_id)
                    .and_then(|bag| object::get_own_symbol_descriptor(bag, &self.gc_heap, *sym)),
                _ => {
                    let name = key
                        .string_name()
                        .expect("non-symbol key has string spelling");
                    self.ordinary_function_own_property_descriptor(
                        Some(context),
                        owner,
                        function_id,
                        name,
                    )?
                }
            };
            return match own {
                Some(desc) => match desc.kind {
                    object::DescriptorKind::Accessor { setter, .. } => {
                        let Some(setter) = setter else {
                            return Ok(false);
                        };
                        let argv: SmallVec<[Value; 8]> = smallvec::smallvec![value];
                        self.run_callable_sync_rooted(stack, context, &setter, receiver, argv)?;
                        Ok(true)
                    }
                    object::DescriptorKind::Data { .. } => {
                        if !desc.flags.writable() {
                            return Ok(false);
                        }
                        self.ordinary_set_on_receiver(stack, context, key, value, &receiver)
                    }
                },
                None => {
                    let parent = self.get_prototype_for_op(&target)?;
                    if parent.is_null() || parent.is_undefined() {
                        return self
                            .ordinary_set_on_receiver(stack, context, key, value, &receiver);
                    }
                    self.ordinary_set_data_value(
                        stack,
                        context,
                        parent,
                        key,
                        value,
                        receiver,
                        hops + 1,
                    )
                }
            };
        }
        // Generic OrdinarySet for the remaining object-like receiver families
        // (class/bound constructors, Promise/ArrayBuffer/DataView and future
        // hosted objects). Their value-level descriptor/prototype/define
        // internal methods are authoritative; duplicating one branch per
        // representation here would let interpreter and JIT semantics drift.
        if crate::reflect::is_type_object_value(&target) {
            return self.with_handle_scope(|interp, scope| {
                let target = interp.scoped_value(scope, target);
                let value = interp.scoped_value(scope, value);
                let receiver = interp.scoped_value(scope, receiver);
                // The handle arena owns every moving value here while the
                // shared activation stack remains the sole frame-root owner.
                let own = interp.ordinary_get_own_property_descriptor_value(
                    stack,
                    context,
                    interp.escape_scoped(target),
                    key,
                    hops + 1,
                )?;
                if let Some(desc) = own {
                    return match desc.kind {
                        object::DescriptorKind::Accessor { setter, .. } => {
                            let Some(setter) = setter else {
                                return Ok(false);
                            };
                            interp.run_callable_sync_rooted(
                                stack,
                                context,
                                &setter,
                                interp.escape_scoped(receiver),
                                smallvec::smallvec![interp.escape_scoped(value)],
                            )?;
                            Ok(true)
                        }
                        object::DescriptorKind::Data { .. } => {
                            if !desc.writable() {
                                return Ok(false);
                            }
                            interp.ordinary_set_on_receiver(
                                stack,
                                context,
                                key,
                                interp.escape_scoped(value),
                                &interp.escape_scoped(receiver),
                            )
                        }
                    };
                }
                let parent = interp.get_prototype_for_op(&interp.escape_scoped(target))?;
                if parent.is_null() || parent.is_undefined() {
                    return interp.ordinary_set_on_receiver(
                        stack,
                        context,
                        key,
                        interp.escape_scoped(value),
                        &interp.escape_scoped(receiver),
                    );
                }
                interp.ordinary_set_data_value(
                    stack,
                    context,
                    parent,
                    key,
                    interp.escape_scoped(value),
                    interp.escape_scoped(receiver),
                    hops + 1,
                )
            });
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
