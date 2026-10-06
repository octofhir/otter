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

use crate::native_abi::CommittedValueError;
use smallvec::SmallVec;

use otter_gc::raw::RawGc;

use super::{
    MetadataProtoSet, canonical_numeric_index_string, regexp_ensure_expando,
    typed_array_valid_index,
};
use crate::activation_stack::ActivationStack;
use crate::{
    ExecutionContext, Interpreter, Value, VmGetOutcome, VmPropertyKey, abstract_ops, binary,
    function_metadata, object, property_atom::AtomizedPropertyKey, read_register, regexp_prototype,
    symbol_prototype, temporal, value_kind_name, write_register,
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
    ) -> Result<(), CommittedValueError> {
        let name = key.name();
        let receiver = *read_register(&stack[top_idx], obj_reg)
            .map_err(|error| CommittedValueError::Fatal(error.into()))?;
        let value = self.load_property_value(context, stack, receiver, name)?;
        let frame = &mut stack[top_idx];
        write_register(frame, dst, value)
            .map_err(|error| CommittedValueError::Fatal(error.into()))?;
        frame
            .advance_pc()
            .map_err(|error| CommittedValueError::Fatal(error.into()))?;
        Ok(())
    }

    /// `Op::HasNamedProperty` — `name in object` for a constant name
    /// (§13.10.1). An ordinary receiver asks the site's load inline cache,
    /// whose handler answers presence; a miss resolves the name without
    /// reading it and installs the handler a load would. A receiver no
    /// handler describes (exotic, proxy, accessor holder) takes the full
    /// `[[HasProperty]]`.
    pub(crate) fn run_has_named_property_reg(
        &mut self,
        context: &ExecutionContext,
        stack: &mut ActivationStack,
        frame_index: usize,
        dst: u16,
        obj_reg: u16,
        key: AtomizedPropertyKey<'_>,
        slot: Option<crate::feedback::PropertyFeedbackSlot<'_>>,
    ) -> Result<(), CommittedValueError> {
        let receiver = *read_register(&stack[frame_index], obj_reg)
            .map_err(|error| CommittedValueError::Fatal(error.into()))?;
        if !receiver.is_object_type() {
            return Err(CommittedValueError::JavaScript(
                crate::VmError::TypeMismatch,
            ));
        }
        let cached = match (receiver.as_object(), slot) {
            (Some(obj), Some(slot)) => {
                if let Some(found) = slot.probe_has(obj, &self.gc_heap) {
                    slot.record_hit();
                    Some(found)
                } else {
                    slot.record_miss();
                    let load = self.resolve_property_load(obj, key);
                    self.update_load_ic(slot, obj, &load);
                    match load {
                        crate::property_cache::PropertyLoad::Data(_) => Some(true),
                        crate::property_cache::PropertyLoad::Absent(_) => Some(false),
                        crate::property_cache::PropertyLoad::Other => None,
                    }
                }
            }
            _ => None,
        };
        let found = match cached {
            Some(found) => found,
            None => self.ordinary_has_property_value(
                stack,
                Some(context),
                receiver,
                &VmPropertyKey::String(key.name()),
                0,
            )?,
        };
        let frame = &mut stack[frame_index];
        write_register(frame, dst, Value::boolean(found))
            .map_err(|error| CommittedValueError::Fatal(error.into()))?;
        frame
            .advance_pc()
            .map_err(|error| CommittedValueError::Fatal(error.into()))
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
    ) -> Result<Value, CommittedValueError> {
        self.with_handle_scope(|interp, scope| {
            let receiver_root = interp.scoped_value(scope, receiver);

            if receiver.is_nullish() {
                return Err(CommittedValueError::JavaScript(
                    interp.nullish_read_error(&receiver, Some(name)),
                ));
            }
            let value = if receiver.as_object().is_some()
                || super::get_walks_prototype_chain(receiver)
            {
                let key = VmPropertyKey::String(name);
                match interp.ordinary_get_value(
                    stack,
                    Some(context),
                    receiver,
                    receiver,
                    &key,
                    0,
                )? {
                    VmGetOutcome::Value(value) => value,
                    VmGetOutcome::InvokeGetter { getter } => interp
                        .run_callable_sync_rooted(
                            stack,
                            Some(context),
                            &getter,
                            interp.escape_scoped(receiver_root),
                            SmallVec::new(),
                        )
                        .map_err(CommittedValueError::completed_call)?,
                }
            } else if let Some(c) = receiver.as_class_constructor() {
                if name == "prototype" {
                    Value::object(c.prototype(&interp.gc_heap))
                } else {
                    let statics = c.statics(&interp.gc_heap);
                    // §10.2.* — `name` / `length` are own properties of the
                    // class constructor (the class name and the constructor
                    // parameter count), supplied by the backing ctor
                    // function unless a static member shadows them. Resolve
                    // them from the ctor BEFORE the inherited
                    // %Function.prototype% walk, whose own `name`=""/
                    // `length`=0 would otherwise shadow the real values.
                    let ctor = c.ctor(&interp.gc_heap);
                    let metadata_deleted = if let Some(cl) = ctor.as_closure(&interp.gc_heap) {
                        function_metadata::ordinary_function_metadata_key(name)
                            .is_some_and(|k| cl.metadata_deleted(&interp.gc_heap, k))
                    } else if let Some(fid) = ctor.as_function() {
                        function_metadata::ordinary_function_metadata_key(name)
                            .is_some_and(|k| interp.function_deleted_metadata.contains(&(fid, k)))
                    } else {
                        false
                    };
                    if (name == "name" || name == "length")
                        && metadata_deleted
                        && object::get_own_descriptor(statics, &interp.gc_heap, name).is_none()
                    {
                        // Deleted virtual metadata — resolve through the
                        // class's own [[Prototype]] (the PARENT CLASS
                        // value), whose virtual name/length the
                        // statics-object walk cannot see.
                        let parent = interp
                            .get_prototype_for_op(&receiver)
                            .map_err(|error| CommittedValueError::JavaScript(error.into()))?;
                        if parent.is_null() || parent.is_undefined() {
                            Value::undefined()
                        } else {
                            let key = VmPropertyKey::String(name);
                            match interp.ordinary_get_value(
                                stack,
                                Some(context),
                                parent,
                                receiver,
                                &key,
                                1,
                            )? {
                                VmGetOutcome::Value(v) => v,
                                VmGetOutcome::InvokeGetter { getter } => interp
                                    .run_callable_sync_rooted(
                                        stack,
                                        Some(context),
                                        &getter,
                                        interp.escape_scoped(receiver_root),
                                        SmallVec::new(),
                                    )
                                    .map_err(CommittedValueError::completed_call)?,
                            }
                        }
                    } else if (name == "name" || name == "length")
                        && !metadata_deleted
                        && object::get_own_descriptor(statics, &interp.gc_heap, name).is_none()
                    {
                        if ctor.is_function()
                            || ctor.is_closure()
                            || ctor.is_native_function()
                            || ctor.is_bound_function()
                        {
                            let owner_bag = interp.callable_bag_for_value(&ctor);
                            let owner_deleted = interp.callable_deleted_flags_for_value(&ctor);
                            let mut ctx = function_metadata::FunctionMetadataContext::new(
                                context,
                                &mut interp.gc_heap,
                                owner_bag,
                                &interp.function_deleted_metadata,
                            )
                            .with_owner_deleted(owner_deleted);
                            function_metadata::callable_intrinsic_property(&mut ctx, &ctor, name)
                                .map_err(|error| CommittedValueError::JavaScript(error.into()))?
                        } else {
                            Value::undefined()
                        }
                    } else if let Some(v) = crate::object::get(statics, &interp.gc_heap, name) {
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
                        let parent = crate::object::prototype_value(statics, &interp.gc_heap);
                        let walked = match parent {
                            Some(p) if !(p.is_object() || p.is_null() || p.is_undefined()) => {
                                match interp.ordinary_get_value(
                                    stack,
                                    Some(context),
                                    p,
                                    receiver,
                                    &VmPropertyKey::String(name),
                                    0,
                                )? {
                                    VmGetOutcome::Value(v) => Some(v),
                                    VmGetOutcome::InvokeGetter { getter } => Some(
                                        interp
                                            .run_callable_sync_rooted(
                                                stack,
                                                Some(context),
                                                &getter,
                                                interp.escape_scoped(receiver_root),
                                                SmallVec::new(),
                                            )
                                            .map_err(CommittedValueError::completed_call)?,
                                    ),
                                }
                            }
                            _ => None,
                        };
                        walked
                            .filter(|v| !v.is_undefined())
                            .unwrap_or_else(Value::undefined)
                    }
                }
            } else if let Some(s) = receiver.as_string(&interp.gc_heap) {
                interp.load_string_primitive_property(stack, context, &receiver, s, name)?
            } else if receiver.is_array() {
                let v = &receiver;
                let a = v.as_array().unwrap();
                let direct = if let Some((getter, _setter)) =
                    crate::array::get_accessor(a, &interp.gc_heap, name)
                {
                    match getter {
                        Some(getter) if abstract_ops::is_callable(&getter) => {
                            let args: SmallVec<[Value; 8]> = SmallVec::new();
                            Some(
                                interp
                                    .run_callable_sync_rooted(
                                        stack,
                                        Some(context),
                                        &getter,
                                        interp.escape_scoped(receiver_root),
                                        args,
                                    )
                                    .map_err(CommittedValueError::completed_call)?,
                            )
                        }
                        _ => Some(Value::undefined()),
                    }
                } else {
                    crate::array::get_named_property(a, &interp.gc_heap, name)
                };
                match direct {
                    Some(value) => value,
                    // §10.4.2.4 — walk the array's *actual* [[Prototype]]: a
                    // `class X extends Array` instance carries a per-instance
                    // override (`X.prototype`), so inherited subclass
                    // accessors / data properties resolve, not only
                    // %Array.prototype%.
                    None => match crate::array::prototype_override(a, &interp.gc_heap) {
                        Some(proto) if proto.is_object_type() => {
                            match interp.ordinary_get_value(
                                stack,
                                Some(context),
                                proto,
                                *v,
                                &crate::VmPropertyKey::String(name),
                                0,
                            )? {
                                VmGetOutcome::Value(val) => val,
                                VmGetOutcome::InvokeGetter { getter } => interp
                                    .run_callable_sync_rooted(
                                        stack,
                                        Some(context),
                                        &getter,
                                        interp.escape_scoped(receiver_root),
                                        SmallVec::new(),
                                    )
                                    .map_err(CommittedValueError::completed_call)?,
                            }
                        }
                        Some(_) => Value::undefined(),
                        None => interp
                            .load_from_constructor_prototype(stack, context, "Array", v, name)?,
                    },
                }
            } else if let Some(fid) = receiver.as_function().or_else(|| {
                receiver
                    .as_closure(&interp.gc_heap)
                    .map(|c| c.cached_function_id)
            }) {
                let owner = receiver.as_closure(&interp.gc_heap);
                interp.function_property_get_with_receiver(
                    stack,
                    context,
                    owner,
                    fid,
                    Some(receiver),
                    name,
                )?
            } else if let Some(native) = receiver.as_native_function() {
                match native
                    .own_property_descriptor(&mut interp.gc_heap, name)
                    .map_err(|error| CommittedValueError::JavaScript(error.into()))?
                {
                    Some(desc) => match &desc.kind {
                        object::DescriptorKind::Data { value } => *value,
                        object::DescriptorKind::Accessor { getter, .. } => match getter {
                            Some(g) => {
                                let args: SmallVec<[Value; 8]> = SmallVec::new();
                                interp
                                    .run_callable_sync_rooted(
                                        stack,
                                        Some(context),
                                        g,
                                        receiver,
                                        args,
                                    )
                                    .map_err(CommittedValueError::completed_call)?
                            }
                            None => Value::undefined(),
                        },
                    },
                    // §10.1.8 — the walk continues at the callable's
                    // [[Prototype]]: an explicit one (Int8Array →
                    // %TypedArray%, or null), else %Function.prototype%.
                    None => {
                        let parent = interp
                            .get_prototype_for_op(&receiver)
                            .map_err(CommittedValueError::JavaScript)?;
                        if parent.is_null() {
                            Value::undefined()
                        } else {
                            let key = VmPropertyKey::String(name);
                            match interp.ordinary_get_value(
                                stack,
                                Some(context),
                                parent,
                                receiver,
                                &key,
                                0,
                            )? {
                                VmGetOutcome::Value(value) => value,
                                VmGetOutcome::InvokeGetter { getter } => interp
                                    .run_callable_sync_rooted(
                                        stack,
                                        Some(context),
                                        &getter,
                                        interp.escape_scoped(receiver_root),
                                        SmallVec::new(),
                                    )
                                    .map_err(CommittedValueError::completed_call)?,
                            }
                        }
                    }
                }
            } else if let Some(bound) = receiver.as_bound_function() {
                let bound = &bound;
                match function_metadata::bound_own_property_descriptor(
                    bound,
                    &mut interp.gc_heap,
                    name,
                )
                .map_err(|error| CommittedValueError::JavaScript(error.into()))?
                {
                    Some(desc) => match &desc.kind {
                        object::DescriptorKind::Data { value } => *value,
                        object::DescriptorKind::Accessor { getter, .. } => match getter {
                            Some(g) if abstract_ops::is_callable(g) => interp
                                .run_callable_sync_rooted(
                                    stack,
                                    Some(context),
                                    g,
                                    receiver,
                                    SmallVec::new(),
                                )
                                .map_err(CommittedValueError::completed_call)?,
                            _ => Value::undefined(),
                        },
                    },
                    None => {
                        // A [[Prototype]] override (bind copies the
                        // target's prototype; setPrototypeOf lands here
                        // too) replaces the %Function.prototype% walk.
                        if let Some(proto) = bound.prototype_override(&interp.gc_heap) {
                            if proto.is_nullish() {
                                Value::undefined()
                            } else {
                                let key = VmPropertyKey::String(name);
                                match interp.ordinary_get_value(
                                    stack,
                                    Some(context),
                                    proto,
                                    receiver,
                                    &key,
                                    0,
                                )? {
                                    VmGetOutcome::Value(value) => value,
                                    VmGetOutcome::InvokeGetter { getter } => interp
                                        .run_callable_sync_rooted(
                                            stack,
                                            Some(context),
                                            &getter,
                                            interp.escape_scoped(receiver_root),
                                            SmallVec::new(),
                                        )
                                        .map_err(CommittedValueError::completed_call)?,
                                }
                            }
                        } else {
                            interp
                                .load_function_prototype_method(name)
                                .or_else(|| interp.load_object_prototype_method(name))
                                .unwrap_or(Value::undefined())
                        }
                    }
                }
            } else if receiver.as_regexp().is_some() {
                // §10.1.8 [[Get]] on a RegExp: route through the shared
                // ladder so an own expando member installed with an
                // accessor (`Object.defineProperty(re, "global", {get})`)
                // fires its getter rather than reading as `undefined`. The
                // ladder checks the expando, the struct flag fast path, and
                // the prototype chain in spec order.
                let key = VmPropertyKey::String(name);
                match interp.ordinary_get_value(
                    stack,
                    Some(context),
                    receiver,
                    receiver,
                    &key,
                    0,
                )? {
                    VmGetOutcome::Value(value) => value,
                    VmGetOutcome::InvokeGetter { getter } => interp
                        .run_callable_sync_rooted(
                            stack,
                            Some(context),
                            &getter,
                            interp.escape_scoped(receiver_root),
                            SmallVec::new(),
                        )
                        .map_err(CommittedValueError::completed_call)?,
                }
            } else if let Some(s) = receiver.as_symbol(&interp.gc_heap) {
                symbol_prototype::load_property(s, name)
            } else if let Some(t) = receiver.as_temporal(&interp.gc_heap) {
                // An ordinary own property (installed via defineProperty /
                // assignment) lives in the expando and shadows the prototype
                // accessor that `load_property` resolves from internal slots.
                if let Some(bag) = t.expando(&interp.gc_heap)
                    && let Some(outcome) = Self::expando_own_get_outcome(bag, &interp.gc_heap, name)
                {
                    match outcome {
                        VmGetOutcome::Value(v) => v,
                        VmGetOutcome::InvokeGetter { getter } => {
                            let args: SmallVec<[Value; 8]> = SmallVec::new();
                            interp
                                .run_callable_sync_rooted(
                                    stack,
                                    Some(context),
                                    &getter,
                                    interp.escape_scoped(receiver_root),
                                    args,
                                )
                                .map_err(CommittedValueError::completed_call)?
                        }
                    }
                } else {
                    let direct = temporal::load_property(t, &mut interp.gc_heap, name);
                    if direct.is_undefined() {
                        // Prototype walk with the temporal as receiver: a
                        // method loaded as a value (`zdt.toString`), a
                        // subclass-prototype property, or a fallible
                        // accessor (`hoursInDay` out of range) that the
                        // internal-slot table cannot surface resolves —
                        // and can throw — through the real chain.
                        let proto = t
                            .prototype_override(&interp.gc_heap)
                            .or_else(|| interp.temporal_prototype_object(t.kind()));
                        if let Some(proto_obj) = proto {
                            let key = VmPropertyKey::String(name);
                            match interp.ordinary_get_value(
                                stack,
                                Some(context),
                                Value::object(proto_obj),
                                receiver,
                                &key,
                                0,
                            )? {
                                VmGetOutcome::Value(v) => v,
                                VmGetOutcome::InvokeGetter { getter } => {
                                    let args: SmallVec<[Value; 8]> = SmallVec::new();
                                    interp
                                        .run_callable_sync_rooted(
                                            stack,
                                            Some(context),
                                            &getter,
                                            interp.escape_scoped(receiver_root),
                                            args,
                                        )
                                        .map_err(CommittedValueError::completed_call)?
                                }
                            }
                        } else {
                            direct
                        }
                    } else {
                        direct
                    }
                }
            } else if let Some(t) = receiver.as_typed_array(&interp.gc_heap) {
                // §10.4.5.4 [[Get]] — a canonical numeric index never
                // consults the expando bag or the prototype chain: it is
                // the element value, or `undefined` when invalid
                // (out-of-bounds, fractional, `-0`, detached buffer).
                if let Some(n) = canonical_numeric_index_string(name) {
                    match typed_array_valid_index(&t, &interp.gc_heap, n) {
                        Some(idx) => t
                            .get(&mut interp.gc_heap, idx)
                            .map_err(crate::oom_to_vm)
                            .map_err(|error| CommittedValueError::JavaScript(error.into()))?,
                        None => Value::undefined(),
                    }
                } else if let Some(bag) = t.expando(&interp.gc_heap)
                    && let Some(outcome) = Self::expando_own_get_outcome(bag, &interp.gc_heap, name)
                {
                    match outcome {
                        VmGetOutcome::Value(v) => v,
                        VmGetOutcome::InvokeGetter { getter } => {
                            let args: SmallVec<[Value; 8]> = SmallVec::new();
                            interp
                                .run_callable_sync_rooted(
                                    stack,
                                    Some(context),
                                    &getter,
                                    interp.escape_scoped(receiver_root),
                                    args,
                                )
                                .map_err(CommittedValueError::completed_call)?
                        }
                    }
                } else {
                    let direct =
                        binary::typed_array_prototype::load_property(&t, &interp.gc_heap, name);
                    if direct.is_undefined() {
                        // §10.4.5.4 walks the instance's actual [[Prototype]]
                        // (a subclass `X.prototype`), not the kind default,
                        // so `O.constructor` / user prototype props resolve
                        // against the real chain.
                        let proto = interp
                            .get_prototype_for_op(&receiver)
                            .map_err(|error| CommittedValueError::JavaScript(error.into()))?;
                        match proto.as_object() {
                            Some(proto_obj) => {
                                let key = VmPropertyKey::String(name);
                                match interp.ordinary_get_value(
                                    stack,
                                    Some(context),
                                    Value::object(proto_obj),
                                    receiver,
                                    &key,
                                    0,
                                )? {
                                    VmGetOutcome::Value(v) => v,
                                    VmGetOutcome::InvokeGetter { getter } => interp
                                        .run_callable_sync_rooted(
                                            stack,
                                            Some(context),
                                            &getter,
                                            interp.escape_scoped(receiver_root),
                                            smallvec::SmallVec::new(),
                                        )
                                        .map_err(CommittedValueError::completed_call)?,
                                }
                            }
                            None => Value::undefined(),
                        }
                    } else {
                        direct
                    }
                }
            } else if receiver.is_big_int() {
                interp.load_from_constructor_prototype(stack, context, "BigInt", &receiver, name)?
            } else if receiver.is_intl() {
                // ECMA-402: methods resolve through `Intl.<Kind>.prototype`;
                // `ordinary_get_value` walks the kind prototype.
                let key = VmPropertyKey::String(name);
                match interp.ordinary_get_value(
                    stack,
                    Some(context),
                    receiver,
                    receiver,
                    &key,
                    0,
                )? {
                    VmGetOutcome::Value(v) => v,
                    VmGetOutcome::InvokeGetter { getter } => interp
                        .run_callable_sync_rooted(
                            stack,
                            Some(context),
                            &getter,
                            interp.escape_scoped(receiver_root),
                            smallvec::SmallVec::new(),
                        )
                        .map_err(CommittedValueError::completed_call)?,
                }
            } else {
                // Proxy and any other receiver not special-cased above resolve
                // through the generic, proxy-aware value-level `[[Get]]` funnel.
                // The interpreter opcode reaches this via `drive_load_property`'s
                // proxy pre-handling; compiled runtime operations call here
                // directly, so this fallback must cover proxies too.
                let key = VmPropertyKey::String(name);
                match interp.ordinary_get_value(
                    stack,
                    Some(context),
                    receiver,
                    receiver,
                    &key,
                    0,
                )? {
                    VmGetOutcome::Value(v) => v,
                    VmGetOutcome::InvokeGetter { getter } => interp
                        .run_callable_sync_rooted(
                            stack,
                            Some(context),
                            &getter,
                            interp.escape_scoped(receiver_root),
                            SmallVec::new(),
                        )
                        .map_err(CommittedValueError::completed_call)?,
                }
            };
            Ok(value)
        })
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
    ) -> Result<(), CommittedValueError> {
        if receiver.is_undefined() || receiver.is_null() || receiver.is_hole() {
            return Err(CommittedValueError::JavaScript(
                self.err_type(
                    (format!(
                        "Cannot set property '{name}' on {}",
                        value_kind_name(&receiver)
                    ))
                    .into(),
                ),
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
            let parent = self
                .primitive_wrapper_prototype(wrapper_name)
                .map_err(|error| CommittedValueError::JavaScript(error.into()))?;
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
            self.failed_set_result(strict, format!("Cannot assign to property '{name}'"))
                .map_err(|error| CommittedValueError::JavaScript(error.into()))?;
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
    ) -> Result<MetadataProtoSet, CommittedValueError> {
        let key = VmPropertyKey::String(name);
        let mut current = self
            .get_prototype_for_op(&receiver)
            .map_err(|error| CommittedValueError::JavaScript(error.into()))?;
        let mut hops = 0usize;
        while hops < object::PROTO_CHAIN_HARD_CAP {
            if current.is_null() || current.is_undefined() {
                return Ok(MetadataProtoSet::Create);
            }
            let desc = self.ordinary_get_own_property_descriptor_value(
                stack,
                Some(context),
                current,
                &key,
                0,
            )?;
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
                    current = self
                        .get_prototype_for_op(&current)
                        .map_err(|error| CommittedValueError::JavaScript(error.into()))?;
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
        force_strict: bool,
    ) -> Result<(), CommittedValueError> {
        let name = key.name();
        let frame = &stack[top_idx];
        let value =
            *read_register(frame, src).map_err(|error| CommittedValueError::Fatal(error.into()))?;
        // §15.7.1 — Op::StorePropertyStrict: class heritage / computed
        // keys are strict code even inside a sloppy function's frame.
        let strict = force_strict || context.function_is_strict(frame.function_id);
        let receiver = *read_register(frame, obj_reg)
            .map_err(|error| CommittedValueError::Fatal(error.into()))?;
        if let Some(o) = receiver.as_object()
            && object::deferred_namespace_target(o, &self.gc_heap).is_some()
        {
            self.ensure_deferred_namespace_ready(stack, context, &receiver, true)?;
            if !self
                .ordinary_set_data_property(o, name, value)
                .map_err(|error| CommittedValueError::JavaScript(error.into()))?
            {
                self.failed_set_result(
                    strict,
                    format!("Cannot assign to read-only property '{name}'"),
                )
                .map_err(|error| CommittedValueError::JavaScript(error.into()))?;
            }
            stack[top_idx]
                .advance_pc()
                .map_err(|error| CommittedValueError::Fatal(error.into()))?;
            return Ok(());
        }
        let target = if let Some(o) = receiver.as_object() {
            Some(o)
        } else if let Some(c) = receiver.as_class_constructor() {
            if self
                .class_store_hits_readonly_intrinsic(context, c, name)
                .map_err(|error| CommittedValueError::JavaScript(error.into()))?
            {
                self.failed_set_result(
                    strict,
                    format!("Cannot assign to read-only property '{name}' of class"),
                )
                .map_err(|error| CommittedValueError::JavaScript(error.into()))?;
                stack[top_idx]
                    .advance_pc()
                    .map_err(|error| CommittedValueError::Fatal(error.into()))?;
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
                    let proto = self
                        .get_prototype_for_op(&receiver)
                        .map_err(|error| CommittedValueError::JavaScript(error.into()))?;
                    if let Some(proto_obj) = proto.as_object() {
                        match object::resolve_set(proto_obj, &self.gc_heap, name) {
                            object::SetOutcome::InvokeSetter { setter } => {
                                let mut args: SmallVec<[Value; 8]> = SmallVec::new();
                                args.push(value);
                                self.run_callable_sync_rooted(
                                    stack,
                                    Some(context),
                                    &setter,
                                    receiver,
                                    args,
                                )
                                .map_err(CommittedValueError::completed_call)?;
                                stack[top_idx]
                                    .advance_pc()
                                    .map_err(|error| CommittedValueError::Fatal(error.into()))?;
                                return Ok(());
                            }
                            object::SetOutcome::Reject { .. } => {
                                self.failed_set_result(
                                    strict,
                                    format!("Cannot assign to read-only property '{name}'"),
                                )
                                .map_err(|error| CommittedValueError::JavaScript(error.into()))?;
                                stack[top_idx]
                                    .advance_pc()
                                    .map_err(|error| CommittedValueError::Fatal(error.into()))?;
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
                        )
                        .map_err(|error| CommittedValueError::JavaScript(error.into()))?;
                        None
                    } else {
                        let bag = regexp_ensure_expando(self, &r, &receiver)
                            .map_err(|error| CommittedValueError::JavaScript(error.into()))?;
                        // The expando ensure allocates; the stored value is
                        // re-read from its traced register slot.
                        let value = *read_register(&stack[top_idx], src)
                            .map_err(|error| CommittedValueError::Fatal(error.into()))?;
                        self.ordinary_set_data_property(bag, name, value)
                            .map_err(|error| CommittedValueError::JavaScript(error.into()))?;
                        None
                    }
                } else {
                    let bag = regexp_ensure_expando(self, &r, &receiver)
                        .map_err(|error| CommittedValueError::JavaScript(error.into()))?;
                    let value = *read_register(&stack[top_idx], src)
                        .map_err(|error| CommittedValueError::Fatal(error.into()))?;
                    if !self
                        .ordinary_set_data_property(bag, name, value)
                        .map_err(|error| CommittedValueError::JavaScript(error.into()))?
                    {
                        self.failed_set_result(
                            strict,
                            format!("Cannot assign to property '{name}'"),
                        )
                        .map_err(|error| CommittedValueError::JavaScript(error.into()))?;
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
                    )
                    .map_err(|error| CommittedValueError::JavaScript(error.into()))?;
                }
                stack[top_idx]
                    .advance_pc()
                    .map_err(|error| CommittedValueError::Fatal(error.into()))?;
                return Ok(());
            }
            if !self.store_array_accessor_property(stack, context, a, name, &value, strict)? {
                let has_own_named =
                    crate::array::get_named_property(a, &self.gc_heap, name).is_some();
                if !has_own_named {
                    let proto = self
                        .constructor_prototype_value("Array")
                        .map_err(|error| CommittedValueError::Fatal(error.into()))?;
                    if let Some(proto) = proto.as_object() {
                        match crate::object::resolve_set(proto, &self.gc_heap, name) {
                            object::SetOutcome::InvokeSetter { setter } => {
                                let mut args: SmallVec<[Value; 8]> = SmallVec::new();
                                args.push(value);
                                self.run_callable_sync_rooted(
                                    stack,
                                    Some(context),
                                    &setter,
                                    Value::array(a),
                                    args,
                                )
                                .map_err(CommittedValueError::completed_call)?;
                                stack[top_idx]
                                    .advance_pc()
                                    .map_err(|error| CommittedValueError::Fatal(error.into()))?;
                                return Ok(());
                            }
                            object::SetOutcome::Reject { .. } => {
                                self.failed_set_result(
                                    strict,
                                    format!("Cannot assign to property '{name}'"),
                                )
                                .map_err(|error| CommittedValueError::JavaScript(error.into()))?;
                                stack[top_idx]
                                    .advance_pc()
                                    .map_err(|error| CommittedValueError::Fatal(error.into()))?;
                                return Ok(());
                            }
                            object::SetOutcome::AssignData => {}
                            // %Array.prototype% chains stay ordinary.
                            object::SetOutcome::ExoticParent { .. } => {}
                        }
                    }
                }
                if !crate::array::set_named_property(a, &mut self.gc_heap, name, value)
                    .map_err(|error| CommittedValueError::JavaScript(error.into()))?
                {
                    self.failed_set_result(strict, format!("Cannot assign to property '{name}'"))
                        .map_err(|error| CommittedValueError::JavaScript(error.into()))?;
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
                    self.failed_set_result(strict, format!("Cannot assign to property '{name}'"))
                        .map_err(|error| CommittedValueError::JavaScript(error.into()))?;
                }
            }
            None
        } else if let Some(fid) = receiver.as_function().or_else(|| {
            receiver
                .as_closure(&self.gc_heap)
                .map(|c| c.cached_function_id)
        }) {
            let owner = receiver.as_closure(&self.gc_heap);
            let has_own = self
                .ordinary_function_has_own_string_property_for_extensibility(
                    context, owner, fid, name,
                )
                .map_err(|error| CommittedValueError::JavaScript(error.into()))?;

            // §10.2.5 — the implicit `prototype` is an own data property from
            // creation; an assignment writes its slot, never the bag.
            if name == "prototype"
                && let Some((_, writable)) = self.function_prototype_slot(context, owner, fid)
            {
                if writable {
                    let value = *read_register(&stack[top_idx], src)
                        .map_err(|error| CommittedValueError::Fatal(error.into()))?;
                    self.store_function_prototype(Some(stack), owner, fid, value)
                        .map_err(|error| CommittedValueError::JavaScript(error.into()))?;
                } else {
                    self.failed_set_result(
                        strict,
                        "Cannot assign to read-only property 'prototype' of function".to_string(),
                    )
                    .map_err(|error| CommittedValueError::JavaScript(error.into()))?;
                }
                stack[top_idx]
                    .advance_pc()
                    .map_err(|error| CommittedValueError::Fatal(error.into()))?;
                return Ok(());
            }
            if matches!(name, "name" | "length") {
                let own = self
                    .ordinary_function_own_property_descriptor(Some(context), owner, fid, name)
                    .map_err(|error| CommittedValueError::JavaScript(error.into()))?;
                match own {
                    Some(desc) if !desc.writable() => {
                        self.failed_set_result(
                            strict,
                            format!("Cannot assign to read-only property '{name}' of function"),
                        )
                        .map_err(|error| CommittedValueError::JavaScript(error.into()))?;
                        None
                    }
                    Some(_) => {
                        // Own writable metadata (made writable via
                        // defineProperty): overwrite in place.
                        let bag = self
                            .function_user_bag_for_register(stack, top_idx, obj_reg, src, fid)
                            .map_err(|error| CommittedValueError::JavaScript(error.into()))?;
                        Some(bag)
                    }
                    None => match self
                        .callable_metadata_proto_set(stack, context, receiver, name)?
                    {
                        MetadataProtoSet::Reject => {
                            self.failed_set_result(
                                strict,
                                format!("Cannot assign to read-only property '{name}' of function"),
                            )
                            .map_err(|error| CommittedValueError::JavaScript(error.into()))?;
                            None
                        }
                        MetadataProtoSet::InvokeSetter(setter) => {
                            self.run_callable_sync_rooted(
                                stack,
                                Some(context),
                                &setter,
                                receiver,
                                smallvec::smallvec![value],
                            )
                            .map_err(CommittedValueError::completed_call)?;
                            None
                        }
                        MetadataProtoSet::Create => {
                            let bag = self
                                .function_user_bag_for_register(stack, top_idx, obj_reg, src, fid)
                                .map_err(|error| CommittedValueError::JavaScript(error.into()))?;
                            if let Some(metadata_key) =
                                function_metadata::ordinary_function_metadata_key(name)
                            {
                                self.function_deleted_metadata.remove(&(fid, metadata_key));
                            }
                            Some(bag)
                        }
                    },
                }
            } else if !has_own && !self.ordinary_function_is_extensible(owner, fid) {
                self.failed_set_result(
                    strict,
                    format!("Cannot add property '{name}' to non-extensible function"),
                )
                .map_err(|error| CommittedValueError::JavaScript(error.into()))?;
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
                        )
                        .map_err(|error| CommittedValueError::JavaScript(error.into()))?;
                        None
                    }
                    MetadataProtoSet::InvokeSetter(setter) => {
                        self.run_callable_sync_rooted(
                            stack,
                            Some(context),
                            &setter,
                            receiver,
                            smallvec::smallvec![value],
                        )
                        .map_err(CommittedValueError::completed_call)?;
                        None
                    }
                    MetadataProtoSet::Create => {
                        let bag = self
                            .function_user_bag_for_register(stack, top_idx, obj_reg, src, fid)
                            .map_err(|error| CommittedValueError::JavaScript(error.into()))?;
                        Some(bag)
                    }
                }
            } else {
                let bag = self
                    .function_user_bag_for_register(stack, top_idx, obj_reg, src, fid)
                    .map_err(|error| CommittedValueError::JavaScript(error.into()))?;
                Some(bag)
            }
        } else if let Some(native) = receiver.as_native_function() {
            match native
                .own_property_descriptor(&mut self.gc_heap, name)
                .map_err(|error| CommittedValueError::JavaScript(error.into()))?
            {
                // §10.1.9.2 OrdinarySetWithOwnDescriptor step 3 — an own
                // accessor runs its setter with this receiver. Falling
                // into the `!writable()` arm below would report every
                // accessor as read-only, since an accessor descriptor has
                // no `[[Writable]]` at all.
                Some(desc) if desc.is_accessor() => {
                    match desc.kind {
                        object::DescriptorKind::Accessor {
                            setter: Some(setter),
                            ..
                        } if crate::abstract_ops::is_callable(&setter) => {
                            self.run_callable_sync_rooted(
                                stack,
                                Some(context),
                                &setter,
                                receiver,
                                smallvec::smallvec![value],
                            )
                            .map_err(CommittedValueError::completed_call)?;
                        }
                        _ => {
                            self.failed_set_result(
                                strict,
                                format!(
                                    "Cannot assign to read-only property '{name}' of function {}",
                                    native.name_string(&self.gc_heap)
                                ),
                            )
                            .map_err(|error| CommittedValueError::JavaScript(error.into()))?;
                        }
                    }
                    None
                }
                Some(desc) if !desc.writable() => {
                    self.failed_set_result(
                        strict,
                        format!(
                            "Cannot assign to read-only property '{name}' of function {}",
                            native.name_string(&self.gc_heap)
                        ),
                    )
                    .map_err(|error| CommittedValueError::JavaScript(error.into()))?;
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
                            )
                            .map_err(|error| CommittedValueError::JavaScript(error.into()))?;
                        }
                        MetadataProtoSet::InvokeSetter(setter) => {
                            self.run_callable_sync_rooted(
                                stack,
                                Some(context),
                                &setter,
                                receiver,
                                smallvec::smallvec![value],
                            )
                            .map_err(CommittedValueError::completed_call)?;
                        }
                        MetadataProtoSet::Create => {
                            let desc = object::PropertyDescriptor::data(value, true, false, true);
                            if !native
                                .define_own_property(&mut self.gc_heap, name, desc)
                                .map_err(|error| CommittedValueError::JavaScript(error.into()))?
                            {
                                self.failed_set_result(
                                    strict,
                                    format!(
                                        "Cannot define property '{name}' on function {}",
                                        native.name_string(&self.gc_heap)
                                    ),
                                )
                                .map_err(|error| CommittedValueError::JavaScript(error.into()))?;
                            }
                        }
                    }
                    None
                }
                _ => {
                    let enumerable =
                        function_metadata::ordinary_function_metadata_key(name).is_none();
                    let desc = object::PropertyDescriptor::data(value, true, enumerable, true);
                    if !native
                        .define_own_property(&mut self.gc_heap, name, desc)
                        .map_err(|error| CommittedValueError::JavaScript(error.into()))?
                    {
                        self.failed_set_result(
                            strict,
                            format!(
                                "Cannot define property '{name}' on function {}",
                                native.name_string(&self.gc_heap)
                            ),
                        )
                        .map_err(|error| CommittedValueError::JavaScript(error.into()))?;
                    }
                    None
                }
            }
        } else if let Some(bound) = receiver.as_bound_function() {
            let bound = &bound;
            match function_metadata::bound_own_property_descriptor(bound, &mut self.gc_heap, name)
                .map_err(|error| CommittedValueError::JavaScript(error.into()))?
            {
                Some(desc) if !desc.writable() => {
                    self.failed_set_result(
                        strict,
                        format!("Cannot assign to read-only property '{name}' of bound function"),
                    )
                    .map_err(|error| CommittedValueError::JavaScript(error.into()))?;
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
                            )
                            .map_err(|error| CommittedValueError::JavaScript(error.into()))?;
                        }
                        MetadataProtoSet::InvokeSetter(setter) => {
                            self.run_callable_sync_rooted(
                                stack,
                                Some(context),
                                &setter,
                                receiver,
                                smallvec::smallvec![value],
                            )
                            .map_err(CommittedValueError::completed_call)?;
                        }
                        MetadataProtoSet::Create => {
                            let desc = object::PropertyDescriptor::data(value, true, true, true);
                            if !function_metadata::bound_define_own_property(
                                bound,
                                &mut self.gc_heap,
                                name,
                                desc,
                            )
                            .map_err(|error| CommittedValueError::JavaScript(error.into()))?
                            {
                                self.failed_set_result(
                                    strict,
                                    format!("Cannot define property '{name}' on bound function"),
                                )
                                .map_err(|error| CommittedValueError::JavaScript(error.into()))?;
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
                    )
                    .map_err(|error| CommittedValueError::JavaScript(error.into()))?
                    {
                        self.failed_set_result(
                            strict,
                            format!("Cannot define property '{name}' on bound function"),
                        )
                        .map_err(|error| CommittedValueError::JavaScript(error.into()))?;
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
                let bag = crate::object::alloc_dictionary_object_with_roots(
                    &mut self.gc_heap,
                    &mut external_visit,
                )
                .map_err(|error| CommittedValueError::JavaScript(error.into()))?;
                p.set_expando(&mut self.gc_heap, bag);
                bag
            };
            Some(bag)
        } else if receiver.is_array_buffer() || receiver.is_data_view() {
            // §25.1 / §25.2 / §25.3 — ArrayBuffer, SharedArrayBuffer and
            // DataView are ordinary objects: §10.1.9 OrdinarySet walks
            // the prototype chain first (an inherited accessor such as
            // the §B.2.2.1 `__proto__` setter intercepts the write),
            // then defines into the lazy expando bag.
            let vm_key = VmPropertyKey::OwnedString(name.to_string());
            if !self
                .ordinary_set_data_value(stack, context, receiver, &vm_key, value, receiver, 0)?
            {
                self.failed_set_result(
                    strict,
                    format!("Cannot assign to read-only property '{name}'"),
                )
                .map_err(|error| CommittedValueError::JavaScript(error.into()))?;
            }
            None
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
                )
                .map_err(|error| CommittedValueError::JavaScript(error.into()))?;
            }
            None
        } else if receiver.is_map()
            || receiver.is_set()
            || receiver.is_weak_map()
            || receiver.is_weak_set()
            || receiver.is_generator()
        {
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
                )
                .map_err(|error| CommittedValueError::JavaScript(error.into()))?;
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
                )
                .map_err(|error| CommittedValueError::JavaScript(error.into()))?;
            }
            None
        } else if receiver.is_undefined() || receiver.is_null() || receiver.is_hole() {
            return Err(CommittedValueError::JavaScript(
                self.err_type(
                    (format!(
                        "Cannot set property '{name}' on {}",
                        value_kind_name(&receiver)
                    ))
                    .into(),
                ),
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
            )
            .map_err(|error| CommittedValueError::JavaScript(error.into()))?;
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
            )
            .map_err(|error| CommittedValueError::JavaScript(error.into()))?;
            None
        };
        if let Some(target) = target {
            // Branches above may have allocated (lazy expando bags); the
            // stored value is re-read from its traced register slot.
            let value = *read_register(&stack[top_idx], src)
                .map_err(|error| CommittedValueError::Fatal(error.into()))?;
            let receiver = *read_register(&stack[top_idx], obj_reg)
                .map_err(|error| CommittedValueError::Fatal(error.into()))?;
            let vm_key = VmPropertyKey::OwnedString(name.to_owned());
            if !self.ordinary_set_data_value(
                stack,
                context,
                Value::object(target),
                &vm_key,
                value,
                receiver,
                0,
            )? {
                self.failed_set_result(strict, format!("Cannot assign to property '{name}'"))
                    .map_err(|error| CommittedValueError::JavaScript(error.into()))?;
            }
        }
        stack[top_idx]
            .advance_pc()
            .map_err(|error| CommittedValueError::Fatal(error.into()))?;
        Ok(())
    }
}
