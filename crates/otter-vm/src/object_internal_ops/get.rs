//! `[[Get]]` and `[[HasProperty]]`.
//!
//! # Contents
//! - The ordinary and scoped forms of both, plus their resumption entries.
//! - The proxy static-call fork.
//!
//! # Invariants
//! - A getter or trap can re-enter JavaScript, so the scoped forms suspend and
//!   resume through an explicit continuation rather than a Rust call stack.

use super::*;
use crate::activation_stack::ActivationStack;
use crate::{
    ExecutionContext, Interpreter, Local, Value, VmError, VmGetOutcome, VmPropertyKey,
    abstract_ops, array, descriptor_value, function_metadata, object, regexp_prototype, symbol,
};
use smallvec::SmallVec;

impl Interpreter {
    pub(crate) fn ordinary_get_value(
        &mut self,
        stack: &mut ActivationStack,
        context: &ExecutionContext,
        base: Value,
        receiver: Value,
        key: &VmPropertyKey,
        hops: usize,
    ) -> Result<VmGetOutcome, VmError> {
        // `VmPropertyKey::Symbol` carries the symbol identity body, which
        // `alloc_symbol` (including private names) allocates directly in old
        // space. That identity handle is immovable across a young collection;
        // only a symbol's cached description may move, and property lookup
        // never uses that cache for identity. String/Atom keys contain no
        // moving GC handle.
        self.with_handle_scope(|interp, scope| {
            let base_handle = interp.scoped_value(scope, base);
            let receiver_handle = interp.scoped_value(scope, receiver);
            interp.ordinary_get_value_scoped(
                stack,
                context,
                scope,
                base_handle,
                receiver_handle,
                key,
                hops,
            )
        })
    }

    pub(crate) fn ordinary_get_value_scoped(
        &mut self,
        stack: &mut ActivationStack,
        context: &ExecutionContext,
        scope: &crate::handles::HandleScope,
        base_handle: Local<'_>,
        receiver_handle: Local<'_>,
        key: &VmPropertyKey,
        hops: usize,
    ) -> Result<VmGetOutcome, VmError> {
        if hops >= object::PROTO_CHAIN_HARD_CAP {
            return Ok(VmGetOutcome::Value(Value::undefined()));
        }
        let base = self.escape_scoped(base_handle);
        // TC39 import defer — accessing a deferred namespace evaluates
        // its module, then reads delegate to the module environment.
        self.ensure_deferred_namespace_ready(
            stack,
            context,
            &base,
            !Self::deferred_key_is_symbol_like(key),
        )?;
        let base = self.escape_scoped(base_handle);
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
                    Some(value) if value.is_hole() => Err(self.err_this_uninit(
                        (format!("Cannot access '{name}' before initialization")).into(),
                    )),
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
                    Some(proto) => self.continue_ordinary_get_value_scoped(
                        stack,
                        context,
                        scope,
                        base_handle,
                        receiver_handle,
                        proto,
                        key,
                        hops + 1,
                    ),
                    None => Ok(VmGetOutcome::Value(Value::undefined())),
                },
            };
        }
        if base.as_proxy().is_some() {
            let key_value = self.vm_property_key_to_value(key)?;
            let key_handle = self.scoped_value(scope, key_value);
            let proxy = self
                .escape_scoped(base_handle)
                .as_proxy()
                .ok_or(VmError::TypeMismatch)?;
            let trap_args: SmallVec<[Value; 8]> = smallvec::smallvec![
                proxy.target(&self.gc_heap),
                self.escape_scoped(key_handle),
                self.escape_scoped(receiver_handle)
            ];
            return match self.invoke_proxy_trap(stack, context, &proxy, "get", trap_args)? {
                Some(value) => {
                    let proxy = self
                        .escape_scoped(base_handle)
                        .as_proxy()
                        .ok_or(VmError::TypeMismatch)?;
                    self.validate_proxy_get_invariants(&proxy.target(&self.gc_heap), key, &value)?;
                    Ok(VmGetOutcome::Value(value))
                }
                None => {
                    let proxy = self
                        .escape_scoped(base_handle)
                        .as_proxy()
                        .ok_or(VmError::TypeMismatch)?;
                    self.continue_ordinary_get_value_scoped(
                        stack,
                        context,
                        scope,
                        base_handle,
                        receiver_handle,
                        proxy.target(&self.gc_heap),
                        key,
                        hops + 1,
                    )
                }
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
                    } else {
                        let proto = self.array_get_prototype_value(arr)?;
                        if proto.is_object_type() {
                            return self.continue_ordinary_get_value_scoped(
                                stack,
                                context,
                                scope,
                                base_handle,
                                receiver_handle,
                                proto,
                                key,
                                hops + 1,
                            );
                        }
                        Value::undefined()
                    }
                }
                _ => {
                    let key_str = key
                        .string_name()
                        .expect("non-symbol key has string spelling");
                    if key_str == "length" {
                        return Ok(VmGetOutcome::Value(Value::number_f64(crate::array::len(
                            arr,
                            &self.gc_heap,
                        )
                            as f64)));
                    }
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
                            // §10.4.2.4 — walk the array's *actual*
                            // [[Prototype]] so a `class X extends Array`
                            // instance observes the subclass prototype's
                            // inherited accessors / data properties, not
                            // just %Array.prototype%.
                            let proto = self.array_get_prototype_value(arr)?;
                            if proto.is_object_type() {
                                return self.continue_ordinary_get_value_scoped(
                                    stack,
                                    context,
                                    scope,
                                    base_handle,
                                    receiver_handle,
                                    proto,
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
            let owner = base.as_closure(&self.gc_heap);
            // A user-mutated [[Prototype]] (`fn.__proto__ = obj` /
            // Object.setPrototypeOf) replaces the intrinsic chain: after
            // the own properties miss, continue the ordinary walk from
            // the override instead of %Function.prototype%.
            let proto_override = self.function_prototype_overrides.get(&function_id).copied();
            let own_lookup = match key {
                VmPropertyKey::Symbol(sym) => self
                    .callable_bag_read(owner, function_id)
                    .and_then(|bag| object::get_own_symbol_descriptor(bag, &self.gc_heap, *sym))
                    .map(descriptor_to_lookup),
                _ => {
                    let key_name = key
                        .string_name()
                        .expect("non-symbol key has string spelling");
                    // Ordinary constructable functions own a virtual
                    // `prototype` data property until its object is first
                    // observed. Materialize it through the shared activation
                    // stack before the descriptor lookup; otherwise the
                    // metadata-only descriptor path sees only `name`/`length`
                    // and incorrectly falls through to %Function.prototype%.
                    //
                    // Do not take this shortcut for arrows/methods/async
                    // functions: they have no own `prototype`, so an inherited
                    // user-installed property must still be found by the
                    // ordinary prototype walk below.
                    if key_name == "prototype"
                        && context.function_has_prototype_property(function_id)
                        && self
                            .callable_bag_read(owner, function_id)
                            .is_none_or(|bag| {
                                object::get_own_descriptor(bag, &self.gc_heap, key_name).is_none()
                            })
                    {
                        let callable = self.escape_scoped(base_handle);
                        let value = self.function_property_get_with_receiver(
                            stack,
                            context,
                            owner,
                            function_id,
                            Some(callable),
                            key_name,
                        )?;
                        return Ok(VmGetOutcome::Value(value));
                    }
                    self.ordinary_function_own_property_descriptor(
                        Some(context),
                        owner,
                        function_id,
                        key_name,
                    )?
                    .map(descriptor_to_lookup)
                }
            };
            if own_lookup.is_none()
                && let Some(over) = proto_override
            {
                if over.is_null() {
                    return Ok(VmGetOutcome::Value(Value::undefined()));
                }
                return self.continue_ordinary_get_value_scoped(
                    stack,
                    context,
                    scope,
                    base_handle,
                    receiver_handle,
                    over,
                    key,
                    hops + 1,
                );
            }
            let lookup = match own_lookup {
                Some(lookup) => lookup,
                None => match key {
                    VmPropertyKey::Symbol(sym) => self
                        .function_kind_prototype_for(context, function_id)
                        .and_then(
                            |proto| match object::lookup_symbol(proto, &self.gc_heap, *sym) {
                                object::PropertyLookup::Absent => None,
                                lookup => Some(lookup),
                            },
                        )
                        .or_else(|| {
                            self.function_prototype_object()
                                .ok()
                                .map(|proto| object::lookup_symbol(proto, &self.gc_heap, *sym))
                        })
                        .unwrap_or(object::PropertyLookup::Absent),
                    _ => {
                        let key_name = key
                            .string_name()
                            .expect("non-symbol key has string spelling");
                        self.function_kind_prototype_for(context, function_id)
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
                            .unwrap_or(object::PropertyLookup::Absent)
                    }
                },
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
                            // §10.1.8 — native callables walk their real
                            // [[Prototype]] chain. TypedArray constructors
                            // may override it; ordinary natives inherit from
                            // %Function.prototype%, whose prototype is
                            // %Object.prototype%.
                            let proto = native.prototype_override(&self.gc_heap).or_else(|| {
                                self.function_prototype_object().ok().map(Value::object)
                            });
                            if let Some(proto) = proto {
                                return self.continue_ordinary_get_value_scoped(
                                    stack,
                                    context,
                                    scope,
                                    base_handle,
                                    receiver_handle,
                                    proto,
                                    key,
                                    hops + 1,
                                );
                            }
                            Value::undefined()
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
                                return self.continue_ordinary_get_value_scoped(
                                    stack,
                                    context,
                                    scope,
                                    base_handle,
                                    receiver_handle,
                                    proto,
                                    key,
                                    hops + 1,
                                );
                            }
                            if let Ok(proto) = self.function_prototype_object() {
                                return self.continue_ordinary_get_value_scoped(
                                    stack,
                                    context,
                                    scope,
                                    base_handle,
                                    receiver_handle,
                                    Value::object(proto),
                                    key,
                                    hops + 1,
                                );
                            }
                            Value::undefined()
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
                return self.continue_ordinary_get_value_scoped(
                    stack,
                    context,
                    scope,
                    base_handle,
                    receiver_handle,
                    ctor,
                    key,
                    hops + 1,
                );
            }
            let outcome = self.continue_ordinary_get_value_scoped(
                stack,
                context,
                scope,
                base_handle,
                receiver_handle,
                Value::object(statics),
                key,
                hops + 1,
            )?;
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
                self.continue_ordinary_get_value_scoped(
                    stack,
                    context,
                    scope,
                    base_handle,
                    receiver_handle,
                    proto,
                    key,
                    hops + 1,
                )
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
                self.continue_ordinary_get_value_scoped(
                    stack,
                    context,
                    scope,
                    base_handle,
                    receiver_handle,
                    proto,
                    key,
                    hops + 1,
                )
            } else {
                Ok(VmGetOutcome::Value(direct))
            };
        }
        if base.is_map() || base.is_set() || base.is_weak_map() || base.is_weak_set() {
            // User-assigned own properties live in the lazy expando and
            // shadow the prototype methods (Map/Set only — Weak* never
            // grow an expando in the [[Set]] path).
            if let Some(bag) = self.collection_expando(&base) {
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
                            Some(getter) if abstract_ops::is_callable(&getter) => {
                                VmGetOutcome::InvokeGetter { getter }
                            }
                            _ => VmGetOutcome::Value(Value::undefined()),
                        });
                    }
                    object::PropertyLookup::Absent => {}
                }
            }
            // §10.1.8 — a subclass instance carries its constructor's
            // `prototype` as a per-instance override; walk *that*
            // chain, falling back to the canonical realm prototype.
            let proto = match self.collection_prototype_override_value(&base) {
                Some(proto) => proto,
                None => {
                    let proto_name = if base.is_map() {
                        "Map"
                    } else if base.is_set() {
                        "Set"
                    } else if base.is_weak_map() {
                        "WeakMap"
                    } else {
                        "WeakSet"
                    };
                    self.constructor_prototype_value(proto_name)?
                }
            };
            if proto.is_nullish() {
                return Ok(VmGetOutcome::Value(Value::undefined()));
            }
            return self.continue_ordinary_get_value_scoped(
                stack,
                context,
                scope,
                base_handle,
                receiver_handle,
                proto,
                key,
                hops + 1,
            );
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
            return self.continue_ordinary_get_value_scoped(
                stack,
                context,
                scope,
                base_handle,
                receiver_handle,
                proto,
                key,
                hops + 1,
            );
        }
        if base.is_big_int() {
            let proto = self.constructor_prototype_value("BigInt")?;
            if proto.is_nullish() {
                return Ok(VmGetOutcome::Value(Value::undefined()));
            }
            return self.continue_ordinary_get_value_scoped(
                stack,
                context,
                scope,
                base_handle,
                receiver_handle,
                proto,
                key,
                hops + 1,
            );
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
            return self.continue_ordinary_get_value_scoped(
                stack,
                context,
                scope,
                base_handle,
                receiver_handle,
                proto,
                key,
                hops + 1,
            );
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
                    return Ok(VmGetOutcome::Value(Value::number_u32(s.len())));
                }
            }
            let proto = self.constructor_prototype_value("String")?;
            if proto.is_nullish() {
                return Ok(VmGetOutcome::Value(Value::undefined()));
            }
            return self.continue_ordinary_get_value_scoped(
                stack,
                context,
                scope,
                base_handle,
                receiver_handle,
                proto,
                key,
                hops + 1,
            );
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
            return self.continue_ordinary_get_value_scoped(
                stack,
                context,
                scope,
                base_handle,
                receiver_handle,
                proto,
                key,
                hops + 1,
            );
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
            return self.continue_ordinary_get_value_scoped(
                stack,
                context,
                scope,
                base_handle,
                receiver_handle,
                proto,
                key,
                hops + 1,
            );
        }
        if let Some(b) = base.as_array_buffer() {
            // Own expando bag (a `constructor` override for the
            // §25.1.6.16 species protocol, or a cross-brand accessor
            // installed via defineProperty) wins over the prototype.
            // An own accessor fires with the buffer as receiver.
            if let Some(bag) = b.expando(&self.gc_heap) {
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
            return self.continue_ordinary_get_value_scoped(
                stack,
                context,
                scope,
                base_handle,
                receiver_handle,
                proto,
                key,
                hops + 1,
            );
        }
        if base.is_generator() || base.is_iterator() {
            // A user-defined own property on a generator/iterator lives
            // in its lazy expando (generator body slot / non-GC side
            // table) and shadows the prototype.
            let expando = base
                .as_generator()
                .and_then(|g| g.expando(&self.gc_heap))
                .or_else(|| {
                    if base.is_iterator() {
                        self.non_gc_exotic_user_props(&base)
                    } else {
                        None
                    }
                });
            if let Some(bag) = expando {
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
            return self.continue_ordinary_get_value_scoped(
                stack,
                context,
                scope,
                base_handle,
                receiver_handle,
                proto,
                key,
                hops + 1,
            );
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
            return self.continue_ordinary_get_value_scoped(
                stack,
                context,
                scope,
                base_handle,
                receiver_handle,
                proto,
                key,
                hops + 1,
            );
        }
        if let Some(t) = base.as_temporal(&self.gc_heap) {
            // An ordinary own property installed via defineProperty /
            // assignment lives in the expando bag and shadows the
            // prototype accessor.
            if let Some(bag) = t.expando(&self.gc_heap) {
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
                            Some(getter) if abstract_ops::is_callable(&getter) => {
                                VmGetOutcome::InvokeGetter { getter }
                            }
                            _ => VmGetOutcome::Value(Value::undefined()),
                        });
                    }
                    object::PropertyLookup::Absent => {}
                }
            }
            // Otherwise route through the per-class prototype installed
            // on `Temporal.<X>.prototype`.
            let proto = self.get_prototype_for_op(&base)?;
            if proto.is_nullish() {
                return Ok(VmGetOutcome::Value(Value::undefined()));
            }
            return self.continue_ordinary_get_value_scoped(
                stack,
                context,
                scope,
                base_handle,
                receiver_handle,
                proto,
                key,
                hops + 1,
            );
        }
        if base.as_intl(&self.gc_heap).is_some() {
            if let Some(bag) = self.non_gc_exotic_user_props(&base) {
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
                            Some(getter) if abstract_ops::is_callable(&getter) => {
                                VmGetOutcome::InvokeGetter { getter }
                            }
                            _ => VmGetOutcome::Value(Value::undefined()),
                        });
                    }
                    object::PropertyLookup::Absent => {}
                }
            }
            // ECMA-402: an `Intl.<Kind>` instance inherits its methods
            // from its actual `[[Prototype]]`, including subclass
            // prototype overrides selected by `new.target`.
            let proto = self.get_prototype_for_op(&base)?;
            if proto.is_nullish() {
                return Ok(VmGetOutcome::Value(Value::undefined()));
            }
            return self.continue_ordinary_get_value_scoped(
                stack,
                context,
                scope,
                base_handle,
                receiver_handle,
                proto,
                key,
                hops + 1,
            );
        }
        // V8-compatible diagnostic: name the base kind and the key being
        // read ("Cannot read properties of undefined (reading 'foo')").
        let shown_key = key.string_name().unwrap_or("property");
        Err(self.err_type(
            (format!(
                "Cannot read properties of {} (reading '{shown_key}')",
                crate::value_kind_name(&base)
            ))
            .into(),
        ))
    }

    pub(crate) fn continue_ordinary_get_value_scoped<'scope>(
        &mut self,
        stack: &mut ActivationStack,
        context: &ExecutionContext,
        scope: &'scope crate::handles::HandleScope,
        base_handle: Local<'scope>,
        receiver_handle: Local<'scope>,
        next_base: Value,
        key: &VmPropertyKey,
        hops: usize,
    ) -> Result<VmGetOutcome, VmError> {
        self.set_scoped(base_handle, next_base);
        self.ordinary_get_value_scoped(
            stack,
            context,
            scope,
            base_handle,
            receiver_handle,
            key,
            hops,
        )
    }

    /// Resolve `Intl.<class_name>.prototype` by walking
    /// `globalThis.Intl.<class_name>.prototype`. Returns `null` when
    /// the namespace or constructor is missing.
    pub(crate) fn intl_kind_prototype_value(&mut self, class_name: &str) -> Value {
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
        stack: &mut ActivationStack,
        context: &ExecutionContext,
        base: Value,
        key: &VmPropertyKey,
        hops: usize,
    ) -> Result<bool, VmError> {
        self.with_handle_scope(|interp, scope| {
            let base = interp.scoped_value(scope, base);
            interp.ordinary_has_property_value_scoped(stack, context, scope, base, key, hops)
        })
    }

    pub(crate) fn ordinary_has_property_value_scoped(
        &mut self,
        stack: &mut ActivationStack,
        context: &ExecutionContext,
        scope: &crate::handles::HandleScope,
        base_handle: Local<'_>,
        key: &VmPropertyKey,
        hops: usize,
    ) -> Result<bool, VmError> {
        if hops >= object::PROTO_CHAIN_HARD_CAP {
            return Ok(false);
        }
        let base = self.escape_scoped(base_handle);
        self.ensure_deferred_namespace_ready(
            stack,
            context,
            &base,
            !Self::deferred_key_is_symbol_like(key),
        )?;
        let base = self.escape_scoped(base_handle);
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
            // §10.4.3.1 — a String exotic object's own index / length
            // slots are not in the ordinary property table; consult the
            // exotic [[GetOwnProperty]] so `in`, for-in, and
            // getOwnPropertyDescriptor agree on one funnel.
            if self.string_object_exotic_descriptor(obj, key)?.is_some() {
                return Ok(true);
            }
            return match object::prototype_value(obj, &self.gc_heap) {
                Some(proto) => self.continue_ordinary_has_property_value_scoped(
                    stack,
                    context,
                    scope,
                    base_handle,
                    proto,
                    key,
                    hops + 1,
                ),
                None => Ok(false),
            };
        }
        if base.as_proxy().is_some() {
            let key_value = self.vm_property_key_to_value(key)?;
            let proxy = self
                .escape_scoped(base_handle)
                .as_proxy()
                .ok_or(VmError::TypeMismatch)?;
            let trap_args: SmallVec<[Value; 8]> =
                smallvec::smallvec![proxy.target(&self.gc_heap), key_value];
            return match self.invoke_proxy_trap(stack, context, &proxy, "has", trap_args)? {
                Some(value) => {
                    let result = value.to_boolean(&self.gc_heap);
                    if !result {
                        let proxy = self
                            .escape_scoped(base_handle)
                            .as_proxy()
                            .ok_or(VmError::TypeMismatch)?;
                        self.set_scoped(base_handle, proxy.target(&self.gc_heap));
                        let target_value = self.escape_scoped(base_handle);
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
                                        "Proxy has trap returned false but target has the property as non-configurable"
                                            .to_string()).into()));
                            }
                            let target_value = self.escape_scoped(base_handle);
                            let target_extensible =
                                self.is_extensible_value(stack, context, &target_value)?;
                            if !target_extensible {
                                return Err(self.err_type((
                                        "Proxy has trap returned false but target has the property and is non-extensible"
                                            .to_string()).into()));
                            }
                        }
                    }
                    Ok(result)
                }
                None => {
                    let proxy = self
                        .escape_scoped(base_handle)
                        .as_proxy()
                        .ok_or(VmError::TypeMismatch)?;
                    self.continue_ordinary_has_property_value_scoped(
                        stack,
                        context,
                        scope,
                        base_handle,
                        proxy.target(&self.gc_heap),
                        key,
                        hops + 1,
                    )
                }
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
                    let base_value = Value::array(arr);
                    let proto = self.get_prototype_for_op(&base_value)?;
                    if proto.is_null() || proto.is_undefined() {
                        return Ok(false);
                    }
                    self.continue_ordinary_has_property_value_scoped(
                        stack,
                        context,
                        scope,
                        base_handle,
                        proto,
                        key,
                        hops + 1,
                    )
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
                    let base_value = Value::array(arr);
                    let proto = self.get_prototype_for_op(&base_value)?;
                    if proto.is_null() || proto.is_undefined() {
                        return Ok(false);
                    }
                    self.continue_ordinary_has_property_value_scoped(
                        stack,
                        context,
                        scope,
                        base_handle,
                        proto,
                        key,
                        hops + 1,
                    )
                }
            };
        }
        if let Some(function_id) = base
            .as_function()
            .or_else(|| base.as_closure(&self.gc_heap).map(|c| c.cached_function_id))
        {
            let owner = base.as_closure(&self.gc_heap);
            if let Some(name) = key.string_name()
                && self
                    .ordinary_function_own_property_descriptor(
                        Some(context),
                        owner,
                        function_id,
                        name,
                    )?
                    .is_some()
            {
                return Ok(true);
            }
            let proto = self.get_prototype_for_op(&base)?;
            return if proto.is_null() || proto.is_undefined() {
                Ok(false)
            } else {
                self.continue_ordinary_has_property_value_scoped(
                    stack,
                    context,
                    scope,
                    base_handle,
                    proto,
                    key,
                    hops + 1,
                )
            };
        }
        if let Some(native) = base.as_native_function() {
            let has_own = match key {
                VmPropertyKey::Symbol(sym) => native
                    .own_symbol_property_descriptor(&self.gc_heap, *sym)
                    .is_some(),
                _ => {
                    let name = key
                        .string_name()
                        .expect("non-symbol key has string spelling");
                    native
                        .own_property_descriptor(&mut self.gc_heap, name)
                        .ok()
                        .flatten()
                        .is_some()
                }
            };
            if has_own {
                return Ok(true);
            }
            let proto = self.get_prototype_for_op(&base)?;
            return if proto.is_null() || proto.is_undefined() {
                Ok(false)
            } else {
                self.continue_ordinary_has_property_value_scoped(
                    stack,
                    context,
                    scope,
                    base_handle,
                    proto,
                    key,
                    hops + 1,
                )
            };
        }
        if let Some(bound) = base.as_bound_function() {
            if let Some(name) = key.string_name()
                && function_metadata::bound_own_property_descriptor(
                    &bound,
                    &mut self.gc_heap,
                    name,
                )?
                .is_some()
            {
                return Ok(true);
            }
            let proto = self.get_prototype_for_op(&base)?;
            return if proto.is_null() || proto.is_undefined() {
                Ok(false)
            } else {
                self.continue_ordinary_has_property_value_scoped(
                    stack,
                    context,
                    scope,
                    base_handle,
                    proto,
                    key,
                    hops + 1,
                )
            };
        }
        if base.is_class_constructor()
            || base.is_regexp()
            || base.is_map()
            || base.is_set()
            || base.is_weak_map()
            || base.is_weak_set()
            || base.is_iterator()
            || base.is_promise()
            || base.is_array_buffer()
            || base.is_data_view()
            || base.is_weak_ref()
            || base.is_finalization_registry()
            || base.is_temporal()
            || base.is_intl()
        {
            let own = self.ordinary_get_own_property_descriptor_value(
                stack,
                context,
                base,
                key,
                hops + 1,
            )?;
            if own.is_some() {
                return Ok(true);
            }
            let proto = self.get_prototype_for_op(&base)?;
            return if proto.is_null() || proto.is_undefined() {
                Ok(false)
            } else {
                self.continue_ordinary_has_property_value_scoped(
                    stack,
                    context,
                    scope,
                    base_handle,
                    proto,
                    key,
                    hops + 1,
                )
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
                return self.continue_ordinary_has_property_value_scoped(
                    stack,
                    context,
                    scope,
                    base_handle,
                    proto,
                    key,
                    hops + 1,
                );
            }
            return Ok(false);
        }
        Err(VmError::TypeMismatch)
    }

    pub(crate) fn continue_ordinary_has_property_value_scoped(
        &mut self,
        stack: &mut ActivationStack,
        context: &ExecutionContext,
        scope: &crate::handles::HandleScope,
        base_handle: Local<'_>,
        next_base: Value,
        key: &VmPropertyKey,
        hops: usize,
    ) -> Result<bool, VmError> {
        self.set_scoped(base_handle, next_base);
        self.ordinary_has_property_value_scoped(stack, context, scope, base_handle, key, hops)
    }

    pub(crate) fn try_proxy_object_static_call(
        &mut self,
        stack: &mut ActivationStack,
        context: &ExecutionContext,
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
            let key = self.evaluate_to_property_key(
                stack,
                context,
                args.get(1).unwrap_or(&Value::undefined()),
            )?;
            let attributes = args.get(2).cloned().unwrap_or(Value::undefined());
            let descriptor = self.evaluate_to_property_descriptor(stack, context, &attributes)?;
            let ok = self.define_own_property_value(stack, context, target, &key, descriptor)?;
            if !ok {
                return Err(self.err_type(("Cannot define property".to_string()).into()));
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
        // §10.4.5 TypedArrays have exotic [[PreventExtensions]] /
        // [[DefineOwnProperty]] / [[OwnPropertyKeys]], so integrity
        // operations must run the generic §7.3.15 SetIntegrityLevel over
        // those internal methods: a length-tracking TypedArray cannot be
        // made non-extensible, and a non-empty one cannot be frozen or
        // sealed (its integer-indexed elements stay writable and
        // configurable), so both throw rather than silently succeeding.
        let typed_array_integrity = matches!(
            method,
            M::Freeze
                | M::Seal
                | M::IsFrozen
                | M::IsSealed
                | M::IsExtensible
                | M::PreventExtensions
        ) && target.as_typed_array(&self.gc_heap).is_some();
        // Map, Set, and Generator objects keep ordinary own properties on a
        // lazy expando bag. Integrity operations must freeze/test that bag;
        // Set's internal [[SetData]] intentionally remains mutable unless a
        // host API explicitly marks its snapshot read-only.
        let collection_integrity = matches!(
            method,
            M::Freeze
                | M::Seal
                | M::IsFrozen
                | M::IsSealed
                | M::IsExtensible
                | M::PreventExtensions
        ) && (target.is_map()
            || target.is_set()
            || target.is_weak_map()
            || target.is_weak_set()
            || target.is_generator());
        if !target.is_proxy()
            && !namespace_integrity
            && !typed_array_integrity
            && !collection_integrity
        {
            return Ok(None);
        }
        match method {
            M::Freeze => {
                if !self.set_integrity_level_value(
                    stack,
                    context,
                    target,
                    ObjectIntegrityLevel::Frozen,
                )? {
                    return Err(self.err_type(("Object.freeze failed".to_string()).into()));
                }
                Ok(Some(*target))
            }
            M::Seal => {
                if !self.set_integrity_level_value(
                    stack,
                    context,
                    target,
                    ObjectIntegrityLevel::Sealed,
                )? {
                    return Err(self.err_type(("Object.seal failed".to_string()).into()));
                }
                Ok(Some(*target))
            }
            M::IsFrozen => {
                let frozen = self.test_integrity_level_value(
                    stack,
                    context,
                    target,
                    ObjectIntegrityLevel::Frozen,
                )?;
                Ok(Some(Value::boolean(frozen)))
            }
            M::IsSealed => {
                let sealed = self.test_integrity_level_value(
                    stack,
                    context,
                    target,
                    ObjectIntegrityLevel::Sealed,
                )?;
                Ok(Some(Value::boolean(sealed)))
            }
            M::IsExtensible => {
                let ext = self.is_extensible_value(stack, context, target)?;
                Ok(Some(Value::boolean(ext)))
            }
            M::PreventExtensions => {
                let ok = self.prevent_extensions_value(stack, context, target)?;
                // §20.1.2.10 — Object.preventExtensions throws when the
                // underlying `[[PreventExtensions]]` returns false.
                if !ok {
                    return Err(
                        self.err_type(("Object.preventExtensions failed".to_string()).into())
                    );
                }
                Ok(Some(*target))
            }
            // §20.1.2.4 Object.defineProperty(O, P, Attributes) —
            // handled in the pre-Proxy block above.
            M::DefineProperty => {
                let key = self.evaluate_to_property_key(
                    stack,
                    context,
                    args.get(1).unwrap_or(&Value::undefined()),
                )?;
                let attributes = args.get(2).cloned().unwrap_or(Value::undefined());
                let descriptor =
                    self.evaluate_to_property_descriptor(stack, context, &attributes)?;
                let ok =
                    self.define_own_property_value(stack, context, target, &key, descriptor)?;
                if !ok {
                    return Err(self.err_type(("Object.defineProperty failed".to_string()).into()));
                }
                Ok(Some(*target))
            }
            // §20.1.2.10 Object.getOwnPropertyNames(O) — full string
            // key set (enumerable + non-enumerable) for Proxy targets,
            // validated against §10.5.11 invariants.
            M::GetOwnPropertyNames => {
                let target_clone = *target;
                let trap_keys = self.own_property_keys_value(stack, context, &target_clone)?;
                let values: Vec<Value> = trap_keys.into_iter().filter(|v| v.is_string()).collect();
                let array = self.alloc_stack_rooted_array_from_values_with_root_slices(
                    stack,
                    values,
                    &[&target_clone],
                    &[args],
                )?;
                Ok(Some(Value::array(array)))
            }
            M::GetOwnPropertySymbols => {
                let target_clone = *target;
                // A byte view's own keys are its element indices plus its
                // expandos. Building the whole key list to keep the symbols
                // would spell out one string per element and discard every
                // one of them, which is quadratic on a large buffer.
                let values: Vec<Value> = if let Some(bag) =
                    byte_view_expando(&target_clone, &self.gc_heap)
                {
                    match bag {
                        Some(bag) => crate::object::with_properties(bag, &self.gc_heap, |p| {
                            p.symbol_keys().map(Value::symbol).collect()
                        }),
                        None => Vec::new(),
                    }
                } else {
                    let trap_keys = self.own_property_keys_value(stack, context, &target_clone)?;
                    trap_keys.into_iter().filter(|v| v.is_symbol()).collect()
                };
                let array = self.alloc_stack_rooted_array_from_values_with_root_slices(
                    stack,
                    values,
                    &[&target_clone],
                    &[args],
                )?;
                Ok(Some(Value::array(array)))
            }
            _ => Ok(None),
        }
    }
}

/// The expando bag of a byte view, wrapped so the caller can tell "not a
/// byte view" from "a byte view with no expandos".
pub(crate) fn byte_view_expando(
    value: &Value,
    heap: &otter_gc::GcHeap,
) -> Option<Option<crate::object::JsObject>> {
    if let Some(view) = value.as_typed_array(heap) {
        return Some(view.expando(heap));
    }
    value.as_data_view().map(|view| view.expando(heap))
}
