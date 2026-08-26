//! Named-property load and store tails.
//!
//! # Contents
//! - The register-form `LoadProperty` / `StoreProperty` entries and the value
//!   forms every miss transition shares with them.
//! - The `OrdinarySet` resolution a callable's deleted `name` / `length`
//!   needs along its prototype chain.
//!
//! # Invariants
//! - Coercion order stays observable-identical to the spec: a receiver or key
//!   that can run user code does so before any cache is consulted.

use smallvec::SmallVec;

use otter_gc::raw::RawGc;

use super::{
    MetadataProtoSet, array_buffer_ensure_expando_pub, canonical_numeric_index_string,
    data_view_ensure_expando_pub, regexp_ensure_expando, typed_array_valid_index,
};
use crate::activation_stack::ActivationStack;
use crate::{
    ExecutionContext, Interpreter, Value, VmError, VmGetOutcome, VmPropertyKey, abstract_ops,
    binary, collections_prototype, function_metadata, object, property_atom::AtomizedPropertyKey,
    read_register, regexp_prototype, symbol_prototype, temporal, value_kind_name, write_register,
};

impl Interpreter {
    pub(crate) fn run_load_property_reg(
        &mut self,
        context: &ExecutionContext,
        stack: &mut ActivationStack,
        top_idx: usize,
        dst: u16,
        obj_reg: u16,
        key: AtomizedPropertyKey<'_>,
    ) -> Result<(), VmError> {
        let name = key.name();
        let receiver = *read_register(&stack[top_idx], obj_reg)?;
        let value = self.load_property_value(context, stack, receiver, name)?;
        let frame = &mut stack[top_idx];
        write_register(frame, dst, value)?;
        frame.advance_pc()?;
        Ok(())
    }

    /// Full string-keyed `[[Get]]` over any receiver value: the complete
    /// interpreter resolution cascade (ordinary objects, every exotic
    /// receiver family, primitives, and the proxy-aware fallback), including
    /// accessor invocation through the synchronous callable path. Shared by
    /// the interpreter opcode and the JIT property transitions, so both
    /// tiers resolve one property exactly one way.
    pub(crate) fn load_property_value(
        &mut self,
        context: &ExecutionContext,
        stack: &mut ActivationStack,
        receiver: Value,
        name: &str,
    ) -> Result<Value, VmError> {
        if receiver.is_nullish() {
            return Err(
                self.err_type(("Cannot read property of null or undefined".to_string()).into())
            );
        }
        let value = if receiver.as_object().is_some() {
            let key = VmPropertyKey::String(name);
            match self.ordinary_get_value(stack, context, receiver, receiver, &key, 0)? {
                VmGetOutcome::Value(value) => value,
                VmGetOutcome::InvokeGetter { getter } => self.run_callable_sync_rooted(
                    stack,
                    context,
                    &getter,
                    receiver,
                    SmallVec::new(),
                )?,
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
                let ctor = c.ctor(&self.gc_heap);
                let metadata_deleted = ctor
                    .as_function()
                    .or_else(|| {
                        ctor.as_closure(&self.gc_heap)
                            .map(|cl| cl.cached_function_id)
                    })
                    .is_some_and(|fid| {
                        function_metadata::ordinary_function_metadata_key(name)
                            .is_some_and(|k| self.function_deleted_metadata.contains(&(fid, k)))
                    });
                if (name == "name" || name == "length")
                    && metadata_deleted
                    && object::get_own_descriptor(statics, &self.gc_heap, name).is_none()
                {
                    // Deleted virtual metadata — resolve through the
                    // class's own [[Prototype]] (the PARENT CLASS
                    // value), whose virtual name/length the
                    // statics-object walk cannot see.
                    let parent = self.get_prototype_for_op(&receiver)?;
                    if parent.is_null() || parent.is_undefined() {
                        Value::undefined()
                    } else {
                        let key = VmPropertyKey::String(name);
                        match self.ordinary_get_value(stack, context, parent, receiver, &key, 1)? {
                            VmGetOutcome::Value(v) => v,
                            VmGetOutcome::InvokeGetter { getter } => self
                                .run_callable_sync_rooted(
                                    stack,
                                    context,
                                    &getter,
                                    receiver,
                                    SmallVec::new(),
                                )?,
                        }
                    }
                } else if (name == "name" || name == "length")
                    && !metadata_deleted
                    && object::get_own_descriptor(statics, &self.gc_heap, name).is_none()
                {
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
                                stack,
                                context,
                                p,
                                receiver,
                                &VmPropertyKey::String(name),
                                0,
                            )? {
                                VmGetOutcome::Value(v) => Some(v),
                                VmGetOutcome::InvokeGetter { getter } => {
                                    Some(self.run_callable_sync_rooted(
                                        stack,
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
            self.load_string_primitive_property(stack, context, &receiver, s, name)?
        } else if receiver.is_array() {
            let v = &receiver;
            let a = v.as_array().unwrap();
            let direct = if let Some((getter, _setter)) =
                crate::array::get_accessor(a, &self.gc_heap, name)
            {
                match getter {
                    Some(getter) if abstract_ops::is_callable(&getter) => {
                        let args: SmallVec<[Value; 8]> = SmallVec::new();
                        Some(self.run_callable_sync_rooted(stack, context, &getter, *v, args)?)
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
                            stack,
                            context,
                            proto,
                            *v,
                            &crate::VmPropertyKey::String(name),
                            0,
                        )? {
                            VmGetOutcome::Value(val) => val,
                            VmGetOutcome::InvokeGetter { getter } => self
                                .run_callable_sync_rooted(
                                    stack,
                                    context,
                                    &getter,
                                    *v,
                                    SmallVec::new(),
                                )?,
                        }
                    }
                    Some(_) => Value::undefined(),
                    None => {
                        self.load_from_constructor_prototype(stack, context, "Array", v, name)?
                    }
                },
            }
        } else if let Some(fid) = receiver.as_function().or_else(|| {
            receiver
                .as_closure(&self.gc_heap)
                .map(|c| c.cached_function_id)
        }) {
            let owner = receiver.as_closure(&self.gc_heap);
            self.function_property_get_with_receiver(
                stack,
                context,
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
                            self.run_callable_sync_rooted(stack, context, g, receiver, args)?
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
                        match self.ordinary_get_value(stack, context, parent, receiver, &key, 0)? {
                            VmGetOutcome::Value(value) => value,
                            VmGetOutcome::InvokeGetter { getter } => self
                                .run_callable_sync_rooted(
                                    stack,
                                    context,
                                    &getter,
                                    receiver,
                                    SmallVec::new(),
                                )?,
                        }
                    }
                    None => {
                        if let Ok(proto) = self.function_prototype_object() {
                            let key = VmPropertyKey::String(name);
                            match self.ordinary_get_value(
                                stack,
                                context,
                                Value::object(proto),
                                receiver,
                                &key,
                                0,
                            )? {
                                VmGetOutcome::Value(value) => value,
                                VmGetOutcome::InvokeGetter { getter } => self
                                    .run_callable_sync_rooted(
                                        stack,
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
                        Some(g) if abstract_ops::is_callable(g) => self.run_callable_sync_rooted(
                            stack,
                            context,
                            g,
                            receiver,
                            SmallVec::new(),
                        )?,
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
            match self.ordinary_get_value(stack, context, receiver, receiver, &key, 0)? {
                VmGetOutcome::Value(value) => value,
                VmGetOutcome::InvokeGetter { getter } => self.run_callable_sync_rooted(
                    stack,
                    context,
                    &getter,
                    receiver,
                    SmallVec::new(),
                )?,
            }
        } else if let Some(s) = receiver.as_symbol(&self.gc_heap) {
            symbol_prototype::load_property(s, name)
        } else if receiver.is_iterator() {
            // §27.1.5 — read string-keyed properties through
            // `Iterator.prototype` so the new spec-mandated
            // `next` / `return` / `throw` natives (and the helper
            // terminals like `map` / `forEach` / `toArray`) all
            // resolve uniformly via the realm prototype.
            self.load_from_constructor_prototype(stack, context, "Iterator", &receiver, name)?
        } else if receiver.is_weak_ref() || receiver.is_finalization_registry() {
            let proto_name = if receiver.is_weak_ref() {
                "WeakRef"
            } else {
                "FinalizationRegistry"
            };
            self.load_from_constructor_prototype(stack, context, proto_name, &receiver, name)?
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
                    match self.ordinary_get_value(stack, context, proto, receiver, &key, 0)? {
                        VmGetOutcome::Value(value) => value,
                        VmGetOutcome::InvokeGetter { getter } => self.run_callable_sync_rooted(
                            stack,
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
                        self.run_callable_sync_rooted(stack, context, &getter, receiver, args)?
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
                    self.load_from_constructor_prototype(
                        stack, context, proto_name, &receiver, name,
                    )?
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
                        self.run_callable_sync_rooted(stack, context, &getter, receiver, args)?
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
                        self.run_callable_sync_rooted(stack, context, &getter, receiver, args)?
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
                    self.load_from_constructor_prototype(
                        stack, context, proto_name, &receiver, name,
                    )?
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
                        self.run_callable_sync_rooted(stack, context, &getter, receiver, args)?
                    }
                }
            } else {
                let direct = binary::data_view_prototype::load_property(&dv, &self.gc_heap, name);
                if direct.is_undefined() {
                    self.load_from_constructor_prototype(
                        stack, context, "DataView", &receiver, name,
                    )?
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
                        self.run_callable_sync_rooted(stack, context, &getter, receiver, args)?
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
                                stack,
                                context,
                                Value::object(proto_obj),
                                receiver,
                                &key,
                                0,
                            )? {
                                VmGetOutcome::Value(v) => v,
                                VmGetOutcome::InvokeGetter { getter } => self
                                    .run_callable_sync_rooted(
                                        stack,
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
            self.load_from_constructor_prototype(stack, context, "BigInt", &receiver, name)?
        } else if receiver.is_intl() {
            // ECMA-402: methods resolve through `Intl.<Kind>.prototype`;
            // `ordinary_get_value` walks the kind prototype.
            let key = VmPropertyKey::String(name);
            match self.ordinary_get_value(stack, context, receiver, receiver, &key, 0)? {
                VmGetOutcome::Value(v) => v,
                VmGetOutcome::InvokeGetter { getter } => self.run_callable_sync_rooted(
                    stack,
                    context,
                    &getter,
                    receiver,
                    smallvec::SmallVec::new(),
                )?,
            }
        } else {
            // Proxy and any other receiver not special-cased above resolve
            // through the generic, proxy-aware value-level `[[Get]]` funnel.
            // The interpreter opcode reaches this via `drive_load_property`'s
            // proxy pre-handling; compiled runtime operations call here
            // directly, so this fallback must cover proxies too.
            let key = VmPropertyKey::String(name);
            match self.ordinary_get_value(stack, context, receiver, receiver, &key, 0)? {
                VmGetOutcome::Value(v) => v,
                VmGetOutcome::InvokeGetter { getter } => self.run_callable_sync_rooted(
                    stack,
                    context,
                    &getter,
                    receiver,
                    SmallVec::new(),
                )?,
            }
        };
        Ok(value)
    }

    /// Full string-keyed `[[Set]]` over any receiver value.
    ///
    /// Object-like receivers enter the single value-level internal-method
    /// funnel in [`Self::ordinary_set_data_value`]. Primitive bases enter at
    /// their wrapper prototype, matching interpreter dispatch without creating
    /// a temporary wrapper; inherited setters remain observable and the
    /// receiver phase rejects as required. Reentrant setters execute
    /// synchronously through the runtime callable boundary while the caller's
    /// published register window remains a moving-GC root.
    pub(crate) fn store_property_value(
        &mut self,
        context: &ExecutionContext,
        stack: &mut ActivationStack,
        receiver: Value,
        name: &str,
        value: Value,
        strict: bool,
    ) -> Result<(), VmError> {
        if receiver.is_undefined() || receiver.is_null() || receiver.is_hole() {
            return Err(self.err_type(
                (format!(
                    "Cannot set property '{name}' on {}",
                    value_kind_name(&receiver)
                ))
                .into(),
            ));
        }
        let key = VmPropertyKey::String(name);
        let wrapper_name = if receiver.is_boolean() {
            Some("Boolean")
        } else if receiver.is_number() {
            Some("Number")
        } else if receiver.is_string() {
            Some("String")
        } else if receiver.is_symbol() {
            Some("Symbol")
        } else if receiver.is_big_int() {
            Some("BigInt")
        } else {
            None
        };
        let accepted = if let Some(wrapper_name) = wrapper_name {
            let parent = self.primitive_wrapper_prototype(wrapper_name)?;
            self.ordinary_set_data_value(
                stack,
                context,
                Value::object(parent),
                &key,
                value,
                receiver,
                0,
            )?
        } else {
            self.ordinary_set_data_value(stack, context, receiver, &key, value, receiver, 0)?
        };
        if !accepted {
            self.failed_set_result(strict, format!("Cannot assign to property '{name}'"))?;
        }
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
        stack: &mut ActivationStack,
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
            let desc =
                self.ordinary_get_own_property_descriptor_value(stack, context, current, &key, 0)?;
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
        stack: &mut ActivationStack,
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
            self.ensure_deferred_namespace_ready(stack, context, &receiver, true)?;
            if !self.ordinary_set_data_property(o, name, value)? {
                self.failed_set_result(
                    strict,
                    format!("Cannot assign to read-only property '{name}'"),
                )?;
            }
            stack[top_idx].advance_pc()?;
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
                stack[top_idx].advance_pc()?;
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
                                self.run_callable_sync_rooted(
                                    stack, context, &setter, receiver, args,
                                )?;
                                stack[top_idx].advance_pc()?;
                                return Ok(());
                            }
                            object::SetOutcome::Reject { .. } => {
                                self.failed_set_result(
                                    strict,
                                    format!("Cannot assign to read-only property '{name}'"),
                                )?;
                                stack[top_idx].advance_pc()?;
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
                    stack,
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
                stack[top_idx].advance_pc()?;
                return Ok(());
            }
            if !self.store_array_accessor_property(stack, context, a, name, &value, strict)? {
                let has_own_named =
                    crate::array::get_named_property(a, &self.gc_heap, name).is_some();
                if !has_own_named {
                    let proto = self.constructor_prototype_value("Array")?;
                    if let Some(proto) = proto.as_object() {
                        match crate::object::resolve_set(proto, &self.gc_heap, name) {
                            object::SetOutcome::InvokeSetter { setter } => {
                                let mut args: SmallVec<[Value; 8]> = SmallVec::new();
                                args.push(value);
                                self.run_callable_sync_rooted(
                                    stack,
                                    context,
                                    &setter,
                                    Value::array(a),
                                    args,
                                )?;
                                stack[top_idx].advance_pc()?;
                                return Ok(());
                            }
                            object::SetOutcome::Reject { .. } => {
                                self.failed_set_result(
                                    strict,
                                    format!("Cannot assign to property '{name}'"),
                                )?;
                                stack[top_idx].advance_pc()?;
                                return Ok(());
                            }
                            object::SetOutcome::AssignData => {}
                            // %Array.prototype% chains stay ordinary.
                            object::SetOutcome::ExoticParent { .. } => {}
                        }
                    }
                }
                if !crate::array::set_named_property(a, &mut self.gc_heap, name, value)? {
                    self.failed_set_result(strict, format!("Cannot assign to property '{name}'"))?;
                }
            }
            None
        } else if let Some(t) = receiver.as_typed_array(&self.gc_heap) {
            if let Some(n) = canonical_numeric_index_string(name) {
                // §10.4.5.16 step 2 — convert the value (firing its
                // coercion, throwing for a Symbol / cross-type) before
                // the index check, so side effects run even for an
                // out-of-bounds write, which then discards the result.
                let converted = self.typed_array_coerce_element(stack, context, t.kind(), value)?;
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
                let ok = self.ordinary_set_data_value(
                    stack, context, receiver, &vm_key, value, receiver, 0,
                )?;
                if !ok {
                    self.failed_set_result(strict, format!("Cannot assign to property '{name}'"))?;
                }
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

            let prototype_in_bag = name == "prototype"
                && self.callable_bag_read(owner, fid).is_some_and(|bag| {
                    !matches!(
                        crate::object::lookup_own(bag, &self.gc_heap, name),
                        object::PropertyLookup::Absent
                    )
                });
            if name == "prototype"
                && !prototype_in_bag
                && context.function_has_prototype_property(fid)
            {
                // §10.2.5 OrdinaryFunctionCreate — the implicit
                // `prototype` slot is {writable, non-enumerable,
                // non-configurable}. Assigning before the slot
                // materializes must install those attributes, not the
                // generic {enumerable, configurable} create-on-set.
                let bag = self.function_user_bag_with_stack_roots(
                    stack,
                    owner,
                    fid,
                    &[&receiver, &value],
                )?;
                let desc = object::PropertyDescriptor::data(value, true, false, false);
                crate::object::define_own_property(bag, &mut self.gc_heap, name, desc);
                stack[top_idx].advance_pc()?;
                return Ok(());
            }
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
                    None => match self
                        .callable_metadata_proto_set(stack, context, receiver, name)?
                    {
                        MetadataProtoSet::Reject => {
                            self.failed_set_result(
                                strict,
                                format!("Cannot assign to read-only property '{name}' of function"),
                            )?;
                            None
                        }
                        MetadataProtoSet::InvokeSetter(setter) => {
                            self.run_callable_sync_rooted(
                                stack,
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
            } else if !has_own {
                // §10.1.9 OrdinarySet — an inherited accessor (e.g. the
                // §B.2.2.1 `__proto__` setter on %Object.prototype%) or
                // read-only data slot on the prototype chain intercepts
                // the write before an own property is created.
                match self.callable_metadata_proto_set(stack, context, receiver, name)? {
                    MetadataProtoSet::Reject => {
                        self.failed_set_result(
                            strict,
                            format!("Cannot assign to read-only property '{name}' of function"),
                        )?;
                        None
                    }
                    MetadataProtoSet::InvokeSetter(setter) => {
                        self.run_callable_sync_rooted(
                            stack,
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
                        Some(bag)
                    }
                }
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
                            native.name_string(&self.gc_heap)
                        ),
                    )?;
                    None
                }
                // No own slot for `name`/`length` means it was deleted; the
                // inherited %Function.prototype% slot is non-writable, so
                // resolve the write along the [[Prototype]] rather than
                // silently re-creating an own data property.
                None if matches!(name, "name" | "length") => {
                    match self.callable_metadata_proto_set(stack, context, receiver, name)? {
                        MetadataProtoSet::Reject => {
                            self.failed_set_result(
                                strict,
                                format!(
                                    "Cannot assign to read-only property '{name}' of function {}",
                                    native.name_string(&self.gc_heap)
                                ),
                            )?;
                        }
                        MetadataProtoSet::InvokeSetter(setter) => {
                            self.run_callable_sync_rooted(
                                stack,
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
                                        native.name_string(&self.gc_heap)
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
                                native.name_string(&self.gc_heap)
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
                    match self.callable_metadata_proto_set(stack, context, receiver, name)? {
                        MetadataProtoSet::Reject => {
                            self.failed_set_result(
                                strict,
                                format!(
                                    "Cannot assign to read-only property '{name}' of bound function"
                                ),
                            )?;
                        }
                        MetadataProtoSet::InvokeSetter(setter) => {
                            self.run_callable_sync_rooted(
                                stack,
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
            if !self
                .ordinary_set_data_value(stack, context, receiver, &vm_key, value, receiver, 0)?
            {
                self.failed_set_result(
                    strict,
                    format!("Cannot assign to read-only property '{name}'"),
                )?;
            }
            None
        } else if receiver.is_map() || receiver.is_set() || receiver.is_generator() {
            // §10.1.9 OrdinarySet — a user-assigned own property
            // (`m.x = 5`) lands in the lazy expando; the prototype
            // walk first lets a getter-only accessor (`size`) reject
            // the write.
            let vm_key = VmPropertyKey::OwnedString(name.to_string());
            if !self
                .ordinary_set_data_value(stack, context, receiver, &vm_key, value, receiver, 0)?
            {
                self.failed_set_result(
                    strict,
                    format!("Cannot assign to read-only property '{name}'"),
                )?;
            }
            None
        } else if receiver.is_intl() || receiver.is_iterator() {
            // ECMA-402 service instances and builtin iterators are
            // ordinary objects with internal slots. User writes land in
            // the non-GC exotic expando after the prototype chain has
            // had a chance to reject through accessors or read-only
            // descriptors.
            let vm_key = VmPropertyKey::OwnedString(name.to_string());
            if !self
                .ordinary_set_data_value(stack, context, receiver, &vm_key, value, receiver, 0)?
            {
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
        stack[top_idx].advance_pc()?;
        Ok(())
    }
}
