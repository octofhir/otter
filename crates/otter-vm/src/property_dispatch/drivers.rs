//! Per-opcode drivers for the cases dispatch cannot complete inline.
//!
//! # Contents
//! - `drive_*` ticks for property, element, prototype and proxy operations.
//! - The strictness and failed-`[[Set]]` helpers they share.
//!
//! # Invariants
//! - A driver returns `true` only when it fully completed the tick, including
//!   the PC advance; `false` leaves the operation to the caller.

use smallvec::SmallVec;

use crate::activation_stack::ActivationStack;
use crate::{
    ExecutionContext, Frame, Interpreter, JsString, Value, VmError, VmGetOutcome, VmPropertyKey,
    abstract_ops, cache_ir, function_metadata, is_restricted_function_property, object,
    operand_decode::{const_operand, register_operand},
    property_atom::AtomizedPropertyKey,
    property_ic::PropertyIcKind,
    read_register, write_register,
};

impl Interpreter {
    /// Drive one tick of [`Op::LoadProperty`] when the receiver is
    /// an object and the resolved property is an accessor descriptor.
    /// Returns `Ok(true)` when an accessor was dispatched (frame
    /// pushed or undefined written) and the outer loop should
    /// `continue`; `Ok(false)` when the in-frame fast path should
    /// run (data slot, non-object receiver, or absent property).
    ///
    /// # Algorithm — §10.1.8 OrdinaryGet
    /// 1. Read the receiver register from the operands decoded by dispatch.
    /// 2. Probe the receiver's own + prototype chain.
    ///    - Absent / data slot: hand off to the in-frame fast path.
    ///    - Accessor with no getter: write `undefined` to `dst`,
    ///      advance pc, signal handled.
    ///    - Accessor with a getter: advance pc, push a call to the
    ///      getter with `this = receiver` and dst = `dst`.
    /// 3. Class constructors and other special receiver kinds skip
    ///    accessor handling: their property tables are plain data
    ///    today, so the in-frame match is authoritative.
    ///
    /// # See also
    /// - <https://tc39.es/ecma262/#sec-ordinaryget>
    pub(crate) fn drive_load_property(
        &mut self,
        stack: &mut ActivationStack,
        context: &ExecutionContext,
        dst: u16,
        obj_reg: u16,
        atomized_key: AtomizedPropertyKey<'_>,
        site: usize,
    ) -> Result<bool, VmError> {
        let name = atomized_key.name();
        let top_idx = stack.len() - 1;
        let receiver = *read_register(&stack[top_idx], obj_reg)?;
        if let Some(obj) = receiver.as_object() {
            // Fast monomorphic own-data load: one shape-handle compare plus a
            // direct slab read, skipping the megamorphic query, the stub walk,
            // and its per-stub atom compare / receiver re-decompress. The shape
            // match fixes both the slot and the key. Falls through to the full
            // probe on a shape miss (polymorphic / prototype / dictionary).
            if let Some(hit) = self.feedback_directory.mono_load_own_data_hit(site)
                && let Some(value) =
                    crate::object::load_own_data_slot_by_shape(obj, &self.gc_heap, hit)
            {
                self.feedback_directory
                    .record_property_hit(PropertyIcKind::Load);
                Self::finish_property_fast_path_value(&mut stack[top_idx], dst, value)?;
                return Ok(true);
            }
            let mut site_disabled = self
                .feedback_directory
                .property_is_megamorphic(site, PropertyIcKind::Load)
                .unwrap_or(true);
            if let Some(value) =
                self.feedback_directory
                    .probe_load(site, obj, &self.gc_heap, atomized_key)
            {
                self.feedback_directory
                    .record_property_hit(PropertyIcKind::Load);
                Self::finish_property_fast_path_value(&mut stack[top_idx], dst, value)?;
                return Ok(true);
            }
            if self
                .feedback_directory
                .property_entry_count(site, PropertyIcKind::Load)
                .unwrap_or_default()
                > 0
            {
                site_disabled = self
                    .feedback_directory
                    .record_property_guard_miss(site, PropertyIcKind::Load)
                    .unwrap_or(true);
            } else {
                self.feedback_directory
                    .record_property_uncached_miss(site, PropertyIcKind::Load);
            }
            // The IC probing / miss bookkeeping above can materialise a rope
            // key and scavenge, relocating the receiver; re-read it from its
            // rooted register before the stub install and the slow-path get
            // both read its shape.
            let obj = read_register(&stack[top_idx], obj_reg)?
                .as_object()
                .unwrap_or(obj);
            // The shared table answers first, so a site re-learning after a
            // guard miss pays a probe instead of another chain walk, and a
            // saturated one — which will never build a program of its own —
            // walks at most once per receiver class and name before its answer,
            // positive or negative, is on record.
            if let Some(resolved) = self.resolve_property_data_slot(obj, atomized_key) {
                if !site_disabled {
                    let ic = cache_ir::CacheStub::from_resolved_load(
                        object::shape_id(obj, &self.gc_heap),
                        &resolved,
                    );
                    self.feedback_directory
                        .install_property_stub(site, PropertyIcKind::Load, ic);
                }
                Self::finish_property_fast_path_value(&mut stack[top_idx], dst, resolved.value)?;
                return Ok(true);
            }
            let key = VmPropertyKey::atom(atomized_key);
            stack[top_idx].advance_pc()?;
            match self.ordinary_get_value(
                stack,
                context,
                Value::object(obj),
                Value::object(obj),
                &key,
                0,
            )? {
                VmGetOutcome::Value(value) => write_register(&mut stack[top_idx], dst, value)?,
                VmGetOutcome::InvokeGetter { getter } => {
                    if abstract_ops::is_callable(&getter) {
                        let args: SmallVec<[Value; 8]> = SmallVec::new();
                        self.invoke(stack, context, &getter, Value::object(obj), args, dst)?;
                    } else {
                        write_register(&mut stack[top_idx], dst, Value::undefined())?;
                    }
                }
            }
            return Ok(true);
        }
        // Heap variants that walk a prototype chain in
        // `ordinary_get_value`. Symbol / atomized string keys on
        // Generator / Iterator / Map / Set / WeakRef / Promise /
        // ArrayBuffer / DataView previously fell to the slow
        // `run_load_property_regs` path whose per-type match had no
        // arms for these receivers and surfaced a bogus
        // `TypeMismatch`. Route through the same `[[Get]]` substrate
        // the Object / Proxy fast paths already use so static-key
        // reads (`iter.next`, `map.size`, `prom.then`, …) resolve
        // consistently.
        if receiver.is_proxy()
            || receiver.is_generator()
            || receiver.is_iterator()
            || receiver.is_map()
            || receiver.is_set()
            || receiver.is_weak_map()
            || receiver.is_weak_set()
            || receiver.is_weak_ref()
            || receiver.is_finalization_registry()
            || receiver.is_promise()
            || receiver.is_array_buffer()
            || receiver.is_data_view()
        {
            let key = VmPropertyKey::atom(atomized_key);
            stack[top_idx].advance_pc()?;
            match self.ordinary_get_value(stack, context, receiver, receiver, &key, 0)? {
                VmGetOutcome::Value(value) => write_register(&mut stack[top_idx], dst, value)?,
                VmGetOutcome::InvokeGetter { getter } => {
                    if abstract_ops::is_callable(&getter) {
                        let args: SmallVec<[Value; 8]> = SmallVec::new();
                        self.invoke(stack, context, &getter, receiver, args, dst)?;
                    } else {
                        write_register(&mut stack[top_idx], dst, Value::undefined())?;
                    }
                }
            }
            return Ok(true);
        }
        if receiver.is_boolean()
            || receiver.is_number()
            || receiver.is_string()
            || receiver.is_symbol()
            || receiver.is_big_int()
        {
            let boxed = self.box_sloppy_this_primitive_stack_rooted(stack, receiver, &[])?;
            let key = VmPropertyKey::atom(atomized_key);
            stack[top_idx].advance_pc()?;
            match self.ordinary_get_value(stack, context, boxed, receiver, &key, 0)? {
                VmGetOutcome::Value(value) => write_register(&mut stack[top_idx], dst, value)?,
                VmGetOutcome::InvokeGetter { getter } => {
                    if abstract_ops::is_callable(&getter) {
                        let args: SmallVec<[Value; 8]> = SmallVec::new();
                        self.invoke(stack, context, &getter, receiver, args, dst)?;
                    } else {
                        write_register(&mut stack[top_idx], dst, Value::undefined())?;
                    }
                }
            }
            return Ok(true);
        }
        if let Some(bound) = receiver.as_bound_function() {
            let bound = &bound;
            match function_metadata::bound_own_property_descriptor(bound, &mut self.gc_heap, name)?
            {
                Some(object::PropertyDescriptor {
                    kind: object::DescriptorKind::Accessor { getter, .. },
                    ..
                }) => {
                    stack[top_idx].advance_pc()?;
                    match getter {
                        Some(callee) if abstract_ops::is_callable(&callee) => {
                            let args: SmallVec<[Value; 8]> = SmallVec::new();
                            self.invoke(stack, context, &callee, receiver, args, dst)?;
                        }
                        _ => write_register(&mut stack[top_idx], dst, Value::undefined())?,
                    }
                    return Ok(true);
                }
                Some(_) => return Ok(false),
                None => {
                    // SpiderMonkey-legacy magic `caller` / `arguments`
                    // on a sloppy ordinary function resolves BEFORE the
                    // %Function.prototype% poison accessors.
                    if is_restricted_function_property(name)
                        && let Some(value) =
                            self.legacy_restricted_property(stack, context, receiver, name)?
                    {
                        write_register(&mut stack[top_idx], dst, value)?;
                        stack[top_idx].advance_pc()?;
                        return Ok(true);
                    }
                    if let Some(object::PropertyDescriptor {
                        kind: object::DescriptorKind::Accessor { getter, .. },
                        ..
                    }) = object::get_own_descriptor(
                        self.function_prototype_object()?,
                        &self.gc_heap,
                        name,
                    ) {
                        stack[top_idx].advance_pc()?;
                        match getter {
                            Some(callee) if abstract_ops::is_callable(&callee) => {
                                let args: SmallVec<[Value; 8]> = SmallVec::new();
                                self.invoke(stack, context, &callee, receiver, args, dst)?;
                            }
                            _ => write_register(&mut stack[top_idx], dst, Value::undefined())?,
                        }
                        return Ok(true);
                    }
                    if is_restricted_function_property(name) {
                        stack[top_idx].advance_pc()?;
                        let callee = self.restricted_throw_type_error()?;
                        let args: SmallVec<[Value; 8]> = SmallVec::new();
                        self.invoke(stack, context, &callee, receiver, args, dst)?;
                        return Ok(true);
                    }
                }
            }
        }
        // Function / Closure / NativeFunction / ClassConstructor —
        // probe `%Function.prototype%` for accessor descriptors so
        // §10.2.4 `AddRestrictedFunctionProperties` poison pills
        // (`caller`, `arguments`) and any user-installed accessor on
        // `Function.prototype` invoke their getter rather than
        // collapsing to `undefined` through the in-frame data path.
        if receiver.is_function()
            || receiver.is_closure()
            || receiver.is_native_function()
            || receiver.is_class_constructor()
        {
            let own_present = if let Some(fid) = receiver.as_function().or_else(|| {
                receiver
                    .as_closure(&self.gc_heap)
                    .map(|c| c.cached_function_id)
            }) {
                let owner = receiver.as_closure(&self.gc_heap);
                let bag_has = self.callable_bag_read(owner, fid).is_some_and(|bag| {
                    !matches!(
                        object::lookup_own(bag, &self.gc_heap, name),
                        object::PropertyLookup::Absent
                    )
                });
                // Virtual own properties (metadata-backed `name` /
                // `length`, lazily-materialized `prototype`) shadow
                // %Function.prototype% — §13.2 step 18: an accessor
                // named `prototype` installed there must never fire
                // for an ordinary function.
                let metadata_has = self
                    .ordinary_function_own_property_descriptor(None, owner, fid, name)
                    .ok()
                    .flatten()
                    .is_some();
                let prototype_implicit = name == "prototype"
                    && context.function_has_prototype_property(fid)
                    && !self.function_deleted_metadata.contains(&(fid, "prototype"));
                bag_has || metadata_has || prototype_implicit
            } else if let Some(c) = receiver.as_class_constructor() {
                // Class constructors expose `prototype` / `name` /
                // `length` virtually when no static shadows them.
                matches!(name, "prototype" | "name" | "length")
                    || !matches!(
                        object::lookup_own(c.statics(&self.gc_heap), &self.gc_heap, name),
                        object::PropertyLookup::Absent
                    )
            } else if let Some(native) = receiver.as_native_function() {
                native
                    .own_property_descriptor(&mut self.gc_heap, name)?
                    .is_some()
            } else {
                false
            };
            if !own_present {
                // SpiderMonkey-legacy magic `caller` / `arguments` on
                // a sloppy ordinary function resolves BEFORE the
                // %Function.prototype% poison accessors.
                if is_restricted_function_property(name)
                    && let Some(value) =
                        self.legacy_restricted_property(stack, context, receiver, name)?
                {
                    write_register(&mut stack[top_idx], dst, value)?;
                    stack[top_idx].advance_pc()?;
                    return Ok(true);
                }
                let proto = self.function_prototype_object()?;
                if let object::PropertyLookup::Accessor { getter, .. } =
                    object::lookup(proto, &self.gc_heap, name)
                {
                    stack[top_idx].advance_pc()?;
                    match getter {
                        Some(callee) if abstract_ops::is_callable(&callee) => {
                            let args: SmallVec<[Value; 8]> = SmallVec::new();
                            self.invoke(stack, context, &callee, receiver, args, dst)?;
                        }
                        _ => write_register(&mut stack[top_idx], dst, Value::undefined())?,
                    }
                    return Ok(true);
                }
            }
        }
        let obj = if let Some(o) = receiver.as_object() {
            o
        } else if let Some(c) = receiver.as_class_constructor() {
            c.statics(&self.gc_heap)
        } else if let Some(fid) = receiver.as_function().or_else(|| {
            receiver
                .as_closure(&self.gc_heap)
                .map(|c| c.cached_function_id)
        }) {
            let owner = receiver.as_closure(&self.gc_heap);
            match self.callable_bag_read(owner, fid) {
                Some(bag) => bag,
                None => self.function_user_bag_with_stack_roots(stack, owner, fid, &[&receiver])?,
            }
        } else {
            return Ok(false);
        };
        match crate::object::lookup(obj, &self.gc_heap, name) {
            object::PropertyLookup::Accessor { getter, .. } => {
                stack[top_idx].advance_pc()?;
                match getter {
                    Some(callee) if abstract_ops::is_callable(&callee) => {
                        let args: SmallVec<[Value; 8]> = SmallVec::new();
                        self.invoke(stack, context, &callee, receiver, args, dst)?;
                    }
                    _ => {
                        // §10.1.8.1 step 4.b — undefined.
                        write_register(&mut stack[top_idx], dst, Value::undefined())?;
                    }
                }
                Ok(true)
            }
            // Data or absent — fall through to the in-frame fast path.
            _ => Ok(false),
        }
    }

    /// Drive one tick of [`Op::LoadElement`] for computed ordinary
    /// object/proxy reads whose resolved descriptor is an accessor.
    pub(crate) fn drive_load_element(
        &mut self,
        stack: &mut ActivationStack,
        context: &ExecutionContext,
        operands: impl crate::executable::OperandSource,
    ) -> Result<bool, VmError> {
        let dst = register_operand(operands.first())?;
        let obj_reg = register_operand(operands.get(1))?;
        let key_reg = register_operand(operands.get(2))?;
        let top_idx = stack.len() - 1;
        let receiver = *read_register(&stack[top_idx], obj_reg)?;
        let key_value_raw = *read_register(&stack[top_idx], key_reg)?;
        // An own dense element of a plain array is the whole `[[Get]]` answer
        // and is the dominant computed-index shape. Everything below first
        // spells the index as a fresh heap string through `ToPropertyKey`,
        // then declines the array receiver to the element path, which coerces
        // it a second time.
        if let Some(arr) = receiver.as_array()
            && let Some(index) = key_value_raw
                .as_i32()
                .and_then(|index| usize::try_from(index).ok())
            && let Some(value) = crate::array::plain_dense_element(arr, &self.gc_heap, index)
        {
            let frame = &mut stack[top_idx];
            write_register(frame, dst, value)?;
            frame.advance_pc()?;
            return Ok(true);
        }
        if receiver.is_nullish() {
            return Err(
                self.err_type(("Cannot read property of null or undefined".to_string()).into())
            );
        }
        let key_value = self.coerce_property_key_value(stack, context, key_value_raw)?;
        write_register(&mut stack[top_idx], key_reg, key_value)?;
        let key = if let Some(s) = key_value.as_string(&self.gc_heap) {
            VmPropertyKey::OwnedString(s.to_lossy_string(&self.gc_heap))
        } else if let Some(n) = key_value.as_number() {
            VmPropertyKey::OwnedString(n.to_display_string())
        } else if let Some(sym) = key_value.as_symbol(&self.gc_heap) {
            VmPropertyKey::Symbol(sym)
        } else {
            return Ok(false);
        };

        // Heap values that walk a prototype chain via `ordinary_get_value`.
        let prototype_routed = receiver.is_object()
            || receiver.is_proxy()
            || receiver.is_generator()
            || receiver.is_iterator()
            || receiver.is_map()
            || receiver.is_set()
            || receiver.is_weak_map()
            || receiver.is_weak_set()
            || receiver.is_weak_ref()
            || receiver.is_finalization_registry()
            || receiver.is_promise()
            || receiver.is_array_buffer()
            || receiver.is_typed_array()
            || receiver.is_class_constructor()
            || receiver.as_native_function().is_some()
            || receiver.is_data_view();
        if prototype_routed {
            stack[top_idx].advance_pc()?;
            match self.ordinary_get_value(stack, context, receiver, receiver, &key, 0)? {
                VmGetOutcome::Value(value) => write_register(&mut stack[top_idx], dst, value)?,
                VmGetOutcome::InvokeGetter { getter } => {
                    if abstract_ops::is_callable(&getter) {
                        let args: SmallVec<[Value; 8]> = SmallVec::new();
                        self.invoke(stack, context, &getter, receiver, args, dst)?;
                    } else {
                        write_register(&mut stack[top_idx], dst, Value::undefined())?;
                    }
                }
            }
            return Ok(true);
        }

        if let (Some(bound), Some(key)) = (receiver.as_bound_function(), key.string_name()) {
            let bound = &bound;
            match function_metadata::bound_own_property_descriptor(bound, &mut self.gc_heap, key)? {
                Some(object::PropertyDescriptor {
                    kind: object::DescriptorKind::Accessor { getter, .. },
                    ..
                }) => {
                    stack[top_idx].advance_pc()?;
                    match getter {
                        Some(callee) if abstract_ops::is_callable(&callee) => {
                            let args: SmallVec<[Value; 8]> = SmallVec::new();
                            self.invoke(stack, context, &callee, receiver, args, dst)?;
                        }
                        _ => write_register(&mut stack[top_idx], dst, Value::undefined())?,
                    }
                    return Ok(true);
                }
                Some(_) => return Ok(false),
                None => {
                    // SpiderMonkey-legacy magic `caller` / `arguments`
                    // on a sloppy ordinary function resolves BEFORE the
                    // %Function.prototype% poison accessors.
                    if is_restricted_function_property(key)
                        && let Some(value) =
                            self.legacy_restricted_property(stack, context, receiver, key)?
                    {
                        write_register(&mut stack[top_idx], dst, value)?;
                        stack[top_idx].advance_pc()?;
                        return Ok(true);
                    }
                    if let Some(object::PropertyDescriptor {
                        kind: object::DescriptorKind::Accessor { getter, .. },
                        ..
                    }) = object::get_own_descriptor(
                        self.function_prototype_object()?,
                        &self.gc_heap,
                        key,
                    ) {
                        stack[top_idx].advance_pc()?;
                        match getter {
                            Some(callee) if abstract_ops::is_callable(&callee) => {
                                let args: SmallVec<[Value; 8]> = SmallVec::new();
                                self.invoke(stack, context, &callee, receiver, args, dst)?;
                            }
                            _ => write_register(&mut stack[top_idx], dst, Value::undefined())?,
                        }
                        return Ok(true);
                    }
                    if is_restricted_function_property(key) {
                        stack[top_idx].advance_pc()?;
                        let callee = self.restricted_throw_type_error()?;
                        let args: SmallVec<[Value; 8]> = SmallVec::new();
                        self.invoke(stack, context, &callee, receiver, args, dst)?;
                        return Ok(true);
                    }
                }
            }
        }

        let obj = if let Some(o) = receiver.as_object() {
            o
        } else if let Some(class) = receiver.as_class_constructor() {
            if key.string_name().is_some_and(|key| key == "prototype") {
                stack[top_idx].advance_pc()?;
                write_register(
                    &mut stack[top_idx],
                    dst,
                    Value::object(class.prototype(&self.gc_heap)),
                )?;
                return Ok(true);
            }
            class.statics(&self.gc_heap)
        } else if let Some(fid) = receiver.as_function().or_else(|| {
            receiver
                .as_closure(&self.gc_heap)
                .map(|c| c.cached_function_id)
        }) {
            let owner = receiver.as_closure(&self.gc_heap);
            let Some(bag) = self.callable_bag_read(owner, fid) else {
                return Ok(false);
            };
            bag
        } else {
            return Ok(false);
        };
        let lookup = match &key {
            VmPropertyKey::Symbol(sym) => crate::object::lookup_symbol(obj, &self.gc_heap, *sym),
            _ => crate::object::lookup(
                obj,
                &self.gc_heap,
                key.string_name()
                    .expect("non-symbol key has string spelling"),
            ),
        };
        match lookup {
            object::PropertyLookup::Data { value, .. } => {
                stack[top_idx].advance_pc()?;
                write_register(&mut stack[top_idx], dst, value)?;
                Ok(true)
            }
            object::PropertyLookup::Accessor { getter, .. } => {
                stack[top_idx].advance_pc()?;
                match getter {
                    Some(callee) if abstract_ops::is_callable(&callee) => {
                        let args: SmallVec<[Value; 8]> = SmallVec::new();
                        self.invoke(stack, context, &callee, receiver, args, dst)?;
                    }
                    _ => {
                        write_register(&mut stack[top_idx], dst, Value::undefined())?;
                    }
                }
                Ok(true)
            }
            _ => Ok(false),
        }
    }

    /// Apply descriptor-aware data assignment for computed ordinary-object
    /// writes (`obj[key] = value`).
    pub(crate) fn function_is_strict(context: &ExecutionContext, function_id: u32) -> bool {
        context.function_is_strict(function_id)
    }

    pub(crate) fn current_frame_is_strict(
        stack: &ActivationStack,
        context: &ExecutionContext,
    ) -> bool {
        stack
            .last()
            .is_some_and(|frame| Self::function_is_strict(context, frame.function_id))
    }

    /// §15.7.14 — `C.prototype`, `C.name`, and `C.length` are
    /// non-writable own properties of a class constructor. The class
    /// value keeps them virtually (delegated to the inner callable's
    /// metadata), so a plain write into the statics object would mint
    /// a shadowing own slot instead of rejecting. Returns `true` when
    /// the named store must fail; an own statics property (a static
    /// method shadow or a post-delete re-creation) falls back to the
    /// ordinary descriptor-aware path.
    pub(crate) fn class_store_hits_readonly_intrinsic(
        &mut self,
        context: &ExecutionContext,
        class: crate::class_constructor::ClassConstructor,
        name: &str,
    ) -> Result<bool, VmError> {
        if name == "prototype" {
            return Ok(true);
        }
        if function_metadata::ordinary_function_metadata_key(name).is_none() {
            return Ok(false);
        }
        let statics = class.statics(&self.gc_heap);
        if crate::object::get_own_descriptor(statics, &self.gc_heap, name).is_some() {
            return Ok(false);
        }
        let ctor = class.ctor(&self.gc_heap);
        if let Some(fid) = ctor
            .as_function()
            .or_else(|| ctor.as_closure(&self.gc_heap).map(|c| c.cached_function_id))
        {
            let owner = ctor.as_closure(&self.gc_heap);
            return Ok(self
                .ordinary_function_own_property_descriptor(Some(context), owner, fid, name)?
                .is_some_and(|desc| !desc.writable()));
        }
        Ok(true)
    }

    pub(crate) fn finish_failed_set(
        &self,
        stack: &mut ActivationStack,
        context: &ExecutionContext,
        force_strict: bool,
        message: impl Into<Box<str>>,
    ) -> Result<bool, VmError> {
        if force_strict || Self::current_frame_is_strict(stack, context) {
            return Err(self.err_type(message.into()));
        }
        let top_idx = stack.len() - 1;
        stack[top_idx].advance_pc()?;
        Ok(true)
    }

    pub(crate) fn failed_set_result(
        &self,
        strict: bool,
        message: impl Into<Box<str>>,
    ) -> Result<(), VmError> {
        if strict {
            Err(self.err_type(message.into()))
        } else {
            Ok(())
        }
    }

    pub(crate) fn advance_property_fast_path(frame: &mut Frame) -> Result<(), VmError> {
        frame.advance_pc()
    }

    pub(crate) fn finish_property_fast_path_value(
        frame: &mut Frame,
        dst: u16,
        value: Value,
    ) -> Result<(), VmError> {
        Self::advance_property_fast_path(frame)?;
        write_register(frame, dst, value)
    }

    pub(crate) fn store_to_primitive_base(
        &mut self,
        stack: &mut ActivationStack,
        context: &ExecutionContext,
        receiver: Value,
        key: VmPropertyKey,
        value: Value,
        scratch_reg: u16,
    ) -> Result<bool, VmError> {
        let Some(base_object) =
            self.object_for_primitive_property_base_stack_rooted(stack, &receiver)?
        else {
            return Ok(false);
        };
        let strict = Self::current_frame_is_strict(stack, context);
        let mut current = object::prototype_value(base_object, &self.gc_heap);
        let mut hops = 0;
        while let Some(proto) = current {
            if hops >= object::PROTO_CHAIN_HARD_CAP {
                break;
            }
            hops += 1;
            if let Some(obj) = proto.as_object() {
                {
                    let lookup = match &key {
                        VmPropertyKey::Symbol(sym) => {
                            object::lookup_own_symbol(obj, &self.gc_heap, *sym)
                        }
                        _ => object::lookup_own(
                            obj,
                            &self.gc_heap,
                            key.string_name()
                                .expect("non-symbol key has string spelling"),
                        ),
                    };
                    match lookup {
                        object::PropertyLookup::Data { flags, .. } => {
                            if !flags.writable() {
                                let name = key.string_name().unwrap_or("symbol");
                                self.failed_set_result(
                                    strict,
                                    format!("Cannot assign to read-only property '{name}'"),
                                )?;
                            } else {
                                let name = key.string_name().unwrap_or("symbol");
                                self.failed_set_result(
                                    strict,
                                    format!("Cannot assign to property '{name}' on primitive"),
                                )?;
                            }
                            let top_idx = stack.len() - 1;
                            stack[top_idx].advance_pc()?;
                            return Ok(true);
                        }
                        object::PropertyLookup::Accessor { setter, .. } => {
                            let Some(setter) = setter else {
                                self.failed_set_result(
                                    strict,
                                    "Cannot assign to accessor property without a setter",
                                )?;
                                let top_idx = stack.len() - 1;
                                let pc = stack[top_idx].pc;
                                stack[top_idx].pc =
                                    pc.checked_add(1).ok_or(VmError::InvalidOperand)?;
                                return Ok(true);
                            };
                            let top_idx = stack.len() - 1;
                            stack[top_idx].advance_pc()?;
                            let mut args: SmallVec<[Value; 8]> = SmallVec::new();
                            args.push(value);
                            self.invoke(stack, context, &setter, receiver, args, scratch_reg)?;
                            return Ok(true);
                        }
                        object::PropertyLookup::Absent => {
                            current = object::prototype_value(obj, &self.gc_heap);
                        }
                    }
                }
            } else if let Some(proxy) = proto.as_proxy() {
                {
                    let key_value = self.vm_property_key_to_value(&key)?;
                    let trap_args: SmallVec<[Value; 8]> = smallvec::smallvec![
                        proxy.target(&self.gc_heap),
                        key_value,
                        value,
                        receiver
                    ];
                    let top_idx = stack.len() - 1;
                    stack[top_idx].advance_pc()?;
                    match self.invoke_proxy_trap(stack, context, &proxy, "set", trap_args)? {
                        crate::object_internal_ops::ProxyTrap::Trapped(_) => {}
                        crate::object_internal_ops::ProxyTrap::NoTrap {
                            target: fallthrough_target,
                        } => {
                            let Some(target) = fallthrough_target.as_object() else {
                                return Err(VmError::TypeMismatch);
                            };
                            match &key {
                                VmPropertyKey::Symbol(sym) => {
                                    match object::resolve_symbol_set(target, &self.gc_heap, *sym) {
                                        object::SetOutcome::AssignData => {}
                                        object::SetOutcome::InvokeSetter { setter } => {
                                            let mut args: SmallVec<[Value; 8]> = SmallVec::new();
                                            args.push(value);
                                            self.invoke(
                                                stack,
                                                context,
                                                &setter,
                                                receiver,
                                                args,
                                                scratch_reg,
                                            )?;
                                        }
                                        object::SetOutcome::Reject { .. } => {
                                            self.failed_set_result(
                                                strict,
                                                "Cannot assign to symbol property",
                                            )?;
                                        }
                                        object::SetOutcome::ExoticParent { parent } => {
                                            if !self.ordinary_set_data_value(
                                                stack,
                                                context,
                                                parent,
                                                &VmPropertyKey::Symbol(*sym),
                                                value,
                                                receiver,
                                                1,
                                            )? {
                                                self.failed_set_result(
                                                    strict,
                                                    "Cannot assign to symbol property",
                                                )?;
                                            }
                                        }
                                    }
                                }
                                _ => {
                                    let key = key
                                        .string_name()
                                        .expect("non-symbol key has string spelling");
                                    match object::resolve_set(target, &self.gc_heap, key) {
                                        object::SetOutcome::AssignData => {}
                                        object::SetOutcome::InvokeSetter { setter } => {
                                            let mut args: SmallVec<[Value; 8]> = SmallVec::new();
                                            args.push(value);
                                            self.invoke(
                                                stack,
                                                context,
                                                &setter,
                                                receiver,
                                                args,
                                                scratch_reg,
                                            )?;
                                        }
                                        object::SetOutcome::Reject { .. } => {
                                            self.failed_set_result(
                                                strict,
                                                format!("Cannot assign to property '{key}'"),
                                            )?;
                                        }
                                        object::SetOutcome::ExoticParent { parent } => {
                                            if !self.ordinary_set_data_value(
                                                stack,
                                                context,
                                                parent,
                                                &VmPropertyKey::String(key),
                                                value,
                                                receiver,
                                                1,
                                            )? {
                                                self.failed_set_result(
                                                    strict,
                                                    format!("Cannot assign to property '{key}'"),
                                                )?;
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                    return Ok(true);
                }
            } else {
                break;
            }
        }

        let top_idx = stack.len() - 1;
        let name = key.string_name().unwrap_or("symbol");
        self.failed_set_result(
            strict,
            format!("Cannot assign to property '{name}' on primitive"),
        )?;
        stack[top_idx].advance_pc()?;
        Ok(true)
    }

    /// Drive one tick of [`Op::StoreProperty`] when §10.1.9
    /// OrdinarySet routes through an accessor setter, hits a
    /// non-writable shadow, or hits a non-extensible receiver.
    /// Returns `Ok(true)` when the dispatch path took over,
    /// `Ok(false)` when the in-frame data-write fast path should run.
    ///
    /// Non-writable / accessor-without-setter / non-extensible
    /// rejections follow the caller frame's compiled strict flag:
    /// strict callers throw `TypeError`, sloppy callers silently
    /// ignore the failed write after advancing the program counter.
    ///
    /// # See also
    /// - <https://tc39.es/ecma262/#sec-ordinaryset>
    /// - <https://tc39.es/ecma262/#sec-ordinarysetwithowndescriptor>
    pub(crate) fn drive_store_property(
        &mut self,
        stack: &mut ActivationStack,
        context: &ExecutionContext,
        operands: impl crate::executable::OperandSource,
        force_strict: bool,
    ) -> Result<bool, VmError> {
        let obj_reg = register_operand(operands.first())?;
        let name_idx = const_operand(operands.get(1))?;
        let src_reg = register_operand(operands.get(2))?;
        let scratch_reg = register_operand(operands.get(3))?;
        let atomized_key = context
            .property_atom(name_idx)
            .ok_or(VmError::InvalidOperand)?;
        let name = atomized_key.name();
        let top_idx = stack.len() - 1;
        let receiver = *read_register(&stack[top_idx], obj_reg)?;
        let value = *read_register(&stack[top_idx], src_reg)?;
        // §15.7.1 — Op::StorePropertyStrict forces strict PutValue
        // failure semantics for class parts lowered into sloppy frames.
        let strict = force_strict || Self::current_frame_is_strict(stack, context);
        if let Some(obj) = receiver.as_object()
            && object::supports_fast_property_ic(obj, &self.gc_heap)
        {
            let site = context
                .property_ic_site(stack[top_idx].function_id, stack[top_idx].pc)
                .ok_or(VmError::InvalidOperand)?;
            let entries_len = self
                .feedback_directory
                .property_entry_count(site, PropertyIcKind::Store)
                .unwrap_or_default();
            // The stub program is `&self`; only `gc_heap` is mutated by a store.
            // The feedback directory and `gc_heap` are disjoint fields, so the
            // stub slice and the `&mut gc_heap` a store writes through can be
            // held at once — no per-store clone of the whole bank is needed.
            if self.feedback_directory.probe_store(
                site,
                obj,
                &mut self.gc_heap,
                atomized_key,
                &value,
            ) {
                self.feedback_directory
                    .record_property_hit(PropertyIcKind::Store);
                Self::advance_property_fast_path(&mut stack[top_idx])?;
                return Ok(true);
            }
            if entries_len > 0 {
                self.feedback_directory
                    .record_property_guard_miss(site, PropertyIcKind::Store);
            } else {
                self.feedback_directory
                    .record_property_uncached_miss(site, PropertyIcKind::Store);
            }
        }
        // §28.2.4.5 / §10.5.9 Proxy.[[Set]] — invoke the `set` trap
        // when present; otherwise delegate to the target.
        if let Some(proxy) = receiver.as_proxy() {
            if proxy.is_revoked(&self.gc_heap) {
                return Err(self.err_type(
                    ("Cannot perform 'set' on a proxy that has been revoked".to_string()).into(),
                ));
            }
            let key_str = JsString::from_str(name, self.gc_heap_mut())?;
            let key_vm = VmPropertyKey::atom(atomized_key);
            let trap_args: SmallVec<[Value; 8]> = smallvec::smallvec![
                proxy.target(&self.gc_heap),
                Value::string(key_str),
                value,
                Value::proxy(proxy),
            ];
            stack[top_idx].advance_pc()?;
            match self.invoke_proxy_trap(stack, context, &proxy, "set", trap_args)? {
                crate::object_internal_ops::ProxyTrap::Trapped(result) => {
                    let ok = result.to_boolean(&self.gc_heap);
                    if !ok {
                        self.failed_set_result(
                            strict,
                            format!("Cannot assign to property '{name}'"),
                        )?;
                        return Ok(true);
                    }
                    // §10.5.9 step 13–14 invariants — when trap reports
                    // success, ensure target descriptor admits the
                    // value.
                    let target_value = proxy.target(&self.gc_heap);
                    let target_desc = self.ordinary_get_own_property_descriptor_value(
                        stack,
                        context,
                        target_value,
                        &key_vm,
                        0,
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
                                return Err(self.err_type((
                                            "Proxy set trap reported success but target is non-configurable non-writable with a different value"
                                                .to_string()).into()));
                            }
                            object::DescriptorKind::Accessor { setter: None, .. } => {
                                return Err(self.err_type((
                                        "Proxy set trap reported success but target is a non-configurable accessor without a setter"
                                            .to_string()).into()));
                            }
                            _ => {}
                        }
                    }
                }
                crate::object_internal_ops::ProxyTrap::NoTrap {
                    target: fallthrough_target,
                } => {
                    // §10.5.9 step 9 — a missing `set` trap forwards to
                    // `target.[[Set]](P, V, Receiver)` with the PROXY as
                    // Receiver. OrdinarySet's own-property steps then run
                    // against the receiver, so the proxy's
                    // getOwnPropertyDescriptor / defineProperty traps fire
                    // and an inherited setter sees `this === proxy`. Route
                    // every target (ordinary, exotic, or nested proxy)
                    // through the value-level funnel rather than a
                    // receiver-blind fast path.
                    let target_value = fallthrough_target;
                    if !self.ordinary_set_data_value(
                        stack,
                        context,
                        target_value,
                        &key_vm,
                        value,
                        Value::proxy(proxy),
                        0,
                    )? {
                        self.failed_set_result(
                            strict,
                            format!("Cannot assign to property '{name}'"),
                        )?;
                    }
                }
            }
            return Ok(true);
        }
        if let Some(bound) = receiver.as_bound_function() {
            let bound = &bound;
            match function_metadata::bound_own_property_descriptor(bound, &mut self.gc_heap, name)?
            {
                Some(object::PropertyDescriptor {
                    kind: object::DescriptorKind::Accessor { setter, .. },
                    ..
                }) => {
                    let setter = setter.ok_or(VmError::TypeMismatch)?;
                    if !abstract_ops::is_callable(&setter) {
                        return Err(VmError::TypeMismatch);
                    }
                    stack[top_idx].advance_pc()?;
                    let mut args: SmallVec<[Value; 8]> = SmallVec::new();
                    args.push(value);
                    self.invoke(stack, context, &setter, receiver, args, scratch_reg)?;
                    return Ok(true);
                }
                Some(_) => return Ok(false),
                None => {
                    if let Some(object::PropertyDescriptor {
                        kind: object::DescriptorKind::Accessor { setter, .. },
                        ..
                    }) = object::get_own_descriptor(
                        self.function_prototype_object()?,
                        &self.gc_heap,
                        name,
                    ) {
                        let setter = setter.ok_or(VmError::TypeMismatch)?;
                        if !abstract_ops::is_callable(&setter) {
                            return Err(VmError::TypeMismatch);
                        }
                        stack[top_idx].advance_pc()?;
                        let mut args: SmallVec<[Value; 8]> = SmallVec::new();
                        args.push(value);
                        self.invoke(stack, context, &setter, receiver, args, scratch_reg)?;
                        return Ok(true);
                    }
                    if is_restricted_function_property(name) {
                        stack[top_idx].advance_pc()?;
                        let callee = self.restricted_throw_type_error()?;
                        let mut args: SmallVec<[Value; 8]> = SmallVec::new();
                        args.push(value);
                        self.invoke(stack, context, &callee, receiver, args, scratch_reg)?;
                        return Ok(true);
                    }
                }
            }
        }
        if receiver.is_function()
            || receiver.is_closure()
            || receiver.is_native_function()
            || receiver.is_class_constructor()
        {
            let own_present = if let Some(fid) = receiver.as_function().or_else(|| {
                receiver
                    .as_closure(&self.gc_heap)
                    .map(|c| c.cached_function_id)
            }) {
                let owner = receiver.as_closure(&self.gc_heap);
                let bag_has = self.callable_bag_read(owner, fid).is_some_and(|bag| {
                    !matches!(
                        object::lookup_own(bag, &self.gc_heap, name),
                        object::PropertyLookup::Absent
                    )
                });
                // Virtual own properties (metadata-backed `name` /
                // `length`, lazily-materialized `prototype`) shadow
                // %Function.prototype% — §13.2 step 18: an accessor
                // named `prototype` installed there must never fire
                // for an ordinary function.
                let metadata_has = self
                    .ordinary_function_own_property_descriptor(None, owner, fid, name)
                    .ok()
                    .flatten()
                    .is_some();
                let prototype_implicit = name == "prototype"
                    && context.function_has_prototype_property(fid)
                    && !self.function_deleted_metadata.contains(&(fid, "prototype"));
                bag_has || metadata_has || prototype_implicit
            } else if let Some(c) = receiver.as_class_constructor() {
                // Class constructors expose `prototype` / `name` /
                // `length` virtually when no static shadows them.
                matches!(name, "prototype" | "name" | "length")
                    || !matches!(
                        object::lookup_own(c.statics(&self.gc_heap), &self.gc_heap, name),
                        object::PropertyLookup::Absent
                    )
            } else if let Some(native) = receiver.as_native_function() {
                native
                    .own_property_descriptor(&mut self.gc_heap, name)?
                    .is_some()
            } else {
                false
            };
            if !own_present && is_restricted_function_property(name) {
                stack[top_idx].advance_pc()?;
                let callee = self.restricted_throw_type_error()?;
                let mut args: SmallVec<[Value; 8]> = SmallVec::new();
                args.push(value);
                self.invoke(stack, context, &callee, receiver, args, scratch_reg)?;
                return Ok(true);
            }
        }
        if receiver.is_boolean()
            || receiver.is_number()
            || receiver.is_string()
            || receiver.is_symbol()
            || receiver.is_big_int()
        {
            return self.store_to_primitive_base(
                stack,
                context,
                receiver,
                VmPropertyKey::atom(atomized_key),
                value,
                scratch_reg,
            );
        }
        let obj = if let Some(o) = receiver.as_object() {
            o
        } else if let Some(c) = receiver.as_class_constructor() {
            if self.class_store_hits_readonly_intrinsic(context, c, name)? {
                return self.finish_failed_set(
                    stack,
                    context,
                    force_strict,
                    format!("Cannot assign to read-only property '{name}' of class"),
                );
            }
            c.statics(&self.gc_heap)
        } else if let Some(fid) = receiver.as_function().or_else(|| {
            receiver
                .as_closure(&self.gc_heap)
                .map(|c| c.cached_function_id)
        }) {
            let owner = receiver.as_closure(&self.gc_heap);
            if function_metadata::ordinary_function_metadata_key(name).is_some() {
                match self.ordinary_function_own_property_descriptor(
                    Some(context),
                    owner,
                    fid,
                    name,
                )? {
                    Some(desc) if !desc.writable() => {
                        return self.finish_failed_set(
                            stack,
                            context,
                            force_strict,
                            format!("Cannot assign to read-only property '{name}' of function"),
                        );
                    }
                    // The virtual `name`/`length` was deleted: defer to the
                    // slow store funnel, which resolves the write along the
                    // [[Prototype]] (the inherited %Function.prototype% slot
                    // is non-writable) instead of re-creating an own bag
                    // entry that would mask it.
                    None => return Ok(false),
                    Some(_) => {}
                }
            }
            // An own bag hit may overwrite in place; a miss falls to
            // the slow store funnel, whose §10.1.9 OrdinarySet walk
            // honours inherited accessors (the §B.2.2.1 `__proto__`
            // setter) before creating an own property.
            let own_in_bag = self.callable_bag_read(owner, fid).is_some_and(|bag| {
                !matches!(
                    object::lookup_own(bag, &self.gc_heap, name),
                    object::PropertyLookup::Absent
                )
            });
            if !own_in_bag {
                return Ok(false);
            }
            match self.callable_bag_read(owner, fid) {
                Some(bag) => bag,
                None => self.function_user_bag_with_stack_roots(
                    stack,
                    owner,
                    fid,
                    &[&receiver, &value],
                )?,
            }
        } else {
            return Ok(false);
        };
        // §10.4.3 String exotic object — own index slots and `length`
        // are non-writable; the write rejects before the ordinary
        // resolve (throwing under strict PutValue).
        if let Some(desc) =
            self.string_object_exotic_descriptor(obj, &VmPropertyKey::String(name))?
            && !desc.writable()
        {
            return self.finish_failed_set(
                stack,
                context,
                force_strict,
                format!("Cannot assign to read-only property '{name}' of string"),
            );
        }
        let outcome = crate::object::resolve_set_atomized(obj, &self.gc_heap, atomized_key);
        match outcome {
            // §10.1.9.2 step 2 — an exotic prototype owns [[Set]]:
            // continue through the value-level funnel (TypedArray
            // §10.4.5.5 etc. are observable from plain receivers).
            object::SetOutcome::ExoticParent { parent } => {
                if !self.ordinary_set_data_value(
                    stack,
                    context,
                    parent,
                    &VmPropertyKey::String(name),
                    value,
                    receiver,
                    1,
                )? {
                    return self.finish_failed_set(
                        stack,
                        context,
                        force_strict,
                        format!("Cannot assign to property '{name}'"),
                    );
                }
                stack[top_idx].advance_pc()?;
                Ok(true)
            }
            object::SetOutcome::AssignData => {
                let transition = if receiver.is_object()
                    && object::supports_fast_property_ic(obj, &self.gc_heap)
                {
                    self.capture_store_property_transition_with_stack_roots(
                        stack,
                        obj,
                        atomized_key,
                        &value,
                    )?
                } else {
                    None
                };
                if transition.is_none() {
                    // `capture_store_property_transition_with_stack_roots` above
                    // roots the stack but not this local `obj`; a scavenge there
                    // relocates the receiver, so re-read it from its rooted
                    // register before the ordinary (shape-advancing) store. Only
                    // plain-object receivers live in `obj_reg` — statics/function
                    // bags are derived from the receiver and keep their handle.
                    let store_obj = if receiver.is_object() {
                        read_register(&stack[top_idx], obj_reg)?
                            .as_object()
                            .unwrap_or(obj)
                    } else {
                        obj
                    };
                    if !self.ordinary_set_data_property(store_obj, name, value)? {
                        return self.finish_failed_set(
                            stack,
                            context,
                            force_strict,
                            format!("Cannot assign to property '{name}'"),
                        );
                    }
                }
                if receiver.is_object() {
                    // The store (or transition capture) may have scavenged,
                    // relocating the receiver object; re-read the live handle
                    // from its rooted register before the IC probe touches it.
                    let Some(obj) = read_register(&stack[top_idx], obj_reg)?.as_object() else {
                        stack[top_idx].advance_pc()?;
                        return Ok(true);
                    };
                    let site = context
                        .property_ic_site(stack[top_idx].function_id, stack[top_idx].pc)
                        .ok_or(VmError::InvalidOperand)?;
                    if self
                        .feedback_directory
                        .property_is_megamorphic(site, PropertyIcKind::Store)
                        == Some(false)
                        && object::supports_fast_property_ic(obj, &self.gc_heap)
                    {
                        if let Some(transition) = transition {
                            self.feedback_directory.install_property_stub(
                                site,
                                PropertyIcKind::Store,
                                cache_ir::CacheStub::store_transition(transition),
                            );
                        } else if let Some(ic) = cache_ir::CacheStub::install_store_existing(
                            obj,
                            &self.gc_heap,
                            atomized_key,
                        ) {
                            self.feedback_directory.install_property_stub(
                                site,
                                PropertyIcKind::Store,
                                ic,
                            );
                        }
                    }
                }
                stack[top_idx].advance_pc()?;
                Ok(true)
            }
            object::SetOutcome::InvokeSetter { setter } => {
                if !abstract_ops::is_callable(&setter) {
                    // Spec §10.1.9 step 5.b — accessor with non-
                    // callable setter rejects.
                    return self.finish_failed_set(
                        stack,
                        context,
                        force_strict,
                        format!("Cannot assign to accessor property '{name}' without a setter"),
                    );
                }
                stack[top_idx].advance_pc()?;
                let mut args: SmallVec<[Value; 8]> = SmallVec::new();
                args.push(value);
                self.invoke(stack, context, &setter, receiver, args, scratch_reg)?;
                Ok(true)
            }
            object::SetOutcome::Reject { .. } => self.finish_failed_set(
                stack,
                context,
                force_strict,
                format!("Cannot assign to property '{name}'"),
            ),
        }
    }

    /// §28.2.4.10 Proxy.[[Delete]] — invoke the `deleteProperty`
    /// trap when the receiver of `delete obj.x` is a Proxy.
    pub(crate) fn drive_delete_property_proxy(
        &mut self,
        stack: &mut ActivationStack,
        context: &ExecutionContext,
        operands: impl crate::executable::OperandSource,
    ) -> Result<bool, VmError> {
        let dst = register_operand(operands.first())?;
        let obj_reg = register_operand(operands.get(1))?;
        let name_idx = const_operand(operands.get(2))?;
        let atomized_key = context
            .property_atom(name_idx)
            .ok_or(VmError::InvalidOperand)?;
        let top_idx = stack.len() - 1;
        let receiver = *read_register(&stack[top_idx], obj_reg)?;
        let Some(proxy) = receiver.as_proxy() else {
            return Ok(false);
        };
        stack[top_idx].advance_pc()?;
        let removed = self.ordinary_delete_value(
            stack,
            context,
            Value::proxy(proxy),
            &VmPropertyKey::atom(atomized_key),
            0,
        )?;
        // §13.5.1.2 — strict-mode `delete` whose [[Delete]] returns false
        // throws a TypeError (the computed-key path already does this).
        let strict = context.function_is_strict(stack[top_idx].function_id);
        if !removed && strict {
            return Err(self.err_type(("Cannot delete property".to_string()).into()));
        }
        write_register(&mut stack[top_idx], dst, Value::boolean(removed))?;
        Ok(true)
    }

    /// §28.2.4.10 Proxy.[[Delete]] — computed delete uses the
    /// same trap-aware path as `delete obj.x`.
    pub(crate) fn drive_delete_element_proxy(
        &mut self,
        stack: &mut ActivationStack,
        context: &ExecutionContext,
        operands: impl crate::executable::OperandSource,
    ) -> Result<bool, VmError> {
        let dst = register_operand(operands.first())?;
        let obj_reg = register_operand(operands.get(1))?;
        let idx_reg = register_operand(operands.get(2))?;
        let top_idx = stack.len() - 1;
        let receiver = *read_register(&stack[top_idx], obj_reg)?;
        if !receiver.is_proxy() {
            return Ok(false);
        }
        let idx = *read_register(&stack[top_idx], idx_reg)?;
        let key = Self::coerce_vm_property_key(Some(&idx), &self.gc_heap)?;
        stack[top_idx].advance_pc()?;
        let removed = self.ordinary_delete_value(stack, context, receiver, &key, 0)?;
        let strict = context.function_is_strict(stack[top_idx].function_id);
        if !removed && strict {
            return Err(self.err_type(("Cannot delete property".to_string()).into()));
        }
        write_register(&mut stack[top_idx], dst, Value::boolean(removed))?;
        Ok(true)
    }
}
