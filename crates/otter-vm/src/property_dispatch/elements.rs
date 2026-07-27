//! Computed element load tails.
//!
//! # Contents
//! - The receiver family an element site records.
//! - The register and value forms of `LoadElement`.
//! - Typed-array element coercion.
//!
//! # Invariants
//! - A dense hole bypasses prototype lookup only while the one-shot indexed
//!   accessor protector remains intact.

use smallvec::SmallVec;

use super::{canonical_numeric_index_string, typed_array_valid_index};
use crate::activation_stack::ActivationStack;
use crate::{
    ExecutionContext, Interpreter, NumberValue, Value, VmError, VmGetOutcome, VmPropertyKey,
    abstract_ops, binary, collections_prototype, descriptor_value, read_register, regexp_prototype,
    rooting::RootScopeExt, symbol, write_register,
};

impl Interpreter {
    /// Which element-bearing family a receiver belongs to, as an element site
    /// records it. The families are exactly those a `JitElementAccess` instance
    /// describes; anything else is generic and keeps the runtime path.
    pub(crate) fn element_family_of(&self, recv: Value) -> crate::jit::JitElementFamily {
        use crate::jit::JitElementFamily as Family;
        if recv.as_array().is_some() {
            return Family::Dense;
        }
        match recv.as_typed_array(&self.gc_heap).map(|view| view.kind()) {
            Some(crate::binary::TypedArrayKind::Int32) => Family::TypedInt32,
            Some(crate::binary::TypedArrayKind::Float64) => Family::TypedFloat64,
            _ => Family::Generic,
        }
    }

    pub(crate) fn run_load_element_regs(
        &mut self,
        context: &ExecutionContext,
        stack: &mut ActivationStack,
        top_idx: usize,
        dst: u16,
        recv_reg: u16,
        idx_reg: u16,
    ) -> Result<(), VmError> {
        let receiver = *read_register(&stack[top_idx], recv_reg)?;
        let key = *read_register(&stack[top_idx], idx_reg)?;
        let value = self.load_element_values(stack, context, receiver, key)?;
        write_register(&mut stack[top_idx], dst, value)?;
        stack[top_idx].advance_pc()
    }

    /// Complete computed element lookup against either physical activation.
    ///
    /// Coercion may allocate or invoke JavaScript, so the coerced key is first
    /// committed and the receiver is then re-read from the traced frame. This
    /// prevents a moving collection from leaving a stale receiver in a Rust
    /// local. Dispatch owns PC advancement.
    pub(crate) fn load_element_values(
        &mut self,
        stack: &mut ActivationStack,
        context: &ExecutionContext,
        mut recv: Value,
        mut idx_value_raw: Value,
    ) -> Result<Value, VmError> {
        // Generated dense loads enter this Rust completion only for a failed
        // guard. The common miss is an in-bounds hole. When the one-shot
        // indexed-accessor protector is still intact and the receiver has no
        // exotic sidecar, the prototype chain cannot turn that hole into a
        // getter result, so avoid `ToPropertyKey` and the full descriptor walk.
        if !self.array_index_accessor_protector
            && let Some(arr) = recv.as_array()
            && let Some(index) = idx_value_raw
                .as_i32()
                .and_then(|index| usize::try_from(index).ok())
            && crate::array::is_plain_dense_hole(arr, &self.gc_heap, index)
        {
            return Ok(Value::undefined());
        }
        let mut idx_value = Value::undefined();
        let mut result = Value::undefined();
        let mut roots = otter_gc::RootScope::new(&mut self.gc_heap);
        // SAFETY: all rooted locals precede `roots` and remain stationary until
        // the scope is explicitly dropped after the complete observable get.
        unsafe {
            roots.add_value(&mut recv);
            roots.add_value(&mut idx_value_raw);
            roots.add_value(&mut idx_value);
            roots.add_value(&mut result);
        }
        if recv.is_nullish() {
            return Err(
                self.err_type(("Cannot read property of null or undefined".to_string()).into())
            );
        }
        idx_value = self.coerce_property_key_value(stack, context, idx_value_raw)?;
        // Exotic receivers that keep their own-property bag on an expando —
        // RegExp, Map, Set, class constructors, boxed primitives — dispatch on
        // a string or symbol key only. A numeric index reaches them still
        // spelled as a Number, so give them the `ToPropertyKey` string here
        // instead of leaving the family match to fail. Arrays, typed arrays,
        // strings, and ordinary objects keep the raw numeric key for their
        // integer fast paths.
        if let Some(number) = idx_value.as_number()
            && recv.as_object().is_none()
            && recv.as_array().is_none()
            && recv.as_typed_array(&self.gc_heap).is_none()
            && recv.as_string(&self.gc_heap).is_none()
        {
            let spelled = number.to_display_string();
            idx_value = Value::string(
                crate::string::JsString::from_str(&spelled, &mut self.gc_heap)
                    .map_err(|_| VmError::TypeMismatch)?,
            );
        }
        let value = if let Some(obj) = recv.as_object() {
            let key = if let Some(sym) = idx_value.as_symbol(&self.gc_heap) {
                VmPropertyKey::Symbol(sym)
            } else if let Some(key) = idx_value.as_string(&self.gc_heap) {
                VmPropertyKey::OwnedString(key.to_lossy_string(&self.gc_heap))
            } else if let Some(n) = idx_value.as_number() {
                VmPropertyKey::OwnedString(n.to_display_string())
            } else {
                return Err(VmError::TypeMismatch);
            };
            match self.ordinary_get_value(stack, context, Value::object(obj), recv, &key, 0)? {
                VmGetOutcome::Value(value) => value,
                VmGetOutcome::InvokeGetter { getter } => {
                    self.run_callable_sync_rooted(stack, context, &getter, recv, SmallVec::new())?
                }
            }
        } else if let Some(arr) = recv.as_array() {
            if let Some(sym) = idx_value.as_symbol(&self.gc_heap) {
                if sym
                    .well_known_tag()
                    .is_some_and(|t| t == symbol::WellKnown::Iterator)
                {
                    if let Some(v) = crate::array::get_symbol_property(arr, &self.gc_heap, sym) {
                        v
                    } else {
                        let key = VmPropertyKey::Symbol(sym);
                        match self.ordinary_get_value(stack, context, recv, recv, &key, 0)? {
                            crate::VmGetOutcome::Value(v) => v,
                            crate::VmGetOutcome::InvokeGetter { getter } => {
                                let args: smallvec::SmallVec<[Value; 8]> =
                                    smallvec::SmallVec::new();
                                self.run_callable_sync_rooted(stack, context, &getter, recv, args)?
                            }
                        }
                    }
                } else {
                    // §22.1 Array exotic — symbol-keyed access reads
                    // array's own symbol table first; on miss walks
                    // `Array.prototype`.
                    match crate::array::get_symbol_property(arr, &self.gc_heap, sym) {
                        Some(v) => v,
                        None => {
                            let proto = self.constructor_prototype_value("Array")?;
                            if let Some(p) = proto.as_object() {
                                crate::object::get_symbol(p, &self.gc_heap, sym)
                                    .unwrap_or(Value::undefined())
                            } else {
                                Value::undefined()
                            }
                        }
                    }
                }
            } else if let Some(key) = idx_value.as_string(&self.gc_heap) {
                // Computed string-key access on Array exotic.
                let name = key.to_lossy_string(&self.gc_heap);
                if name == "length" {
                    Value::number(NumberValue::from_f64(
                        crate::array::len(arr, &self.gc_heap) as f64
                    ))
                } else if let Some((getter, _setter)) =
                    crate::array::get_accessor(arr, &self.gc_heap, &name)
                {
                    match getter {
                        Some(getter) if abstract_ops::is_callable(&getter) => {
                            let args: SmallVec<[Value; 8]> = SmallVec::new();
                            self.run_callable_sync_rooted(stack, context, &getter, recv, args)?
                        }
                        _ => Value::undefined(),
                    }
                } else if let Some(idx) = crate::object::array_index_property_name(&name) {
                    // §10.4.2.4 [[Get]] — an absent integer index is not a
                    // dead end; OrdinaryGet walks the Array.prototype chain
                    // (e.g. `Array.prototype[0]` inherited by a hole slot).
                    if crate::array::has_own_element(arr, &self.gc_heap, idx as usize) {
                        crate::array::get(arr, &self.gc_heap, idx as usize)
                    } else {
                        self.load_from_constructor_prototype(stack, context, "Array", &recv, &name)?
                    }
                } else {
                    match crate::array::get_named_property(arr, &self.gc_heap, &name) {
                        Some(v) => v,
                        None => self.load_from_constructor_prototype(
                            stack, context, "Array", &recv, &name,
                        )?,
                    }
                }
            } else if let Some(n) = idx_value.as_number() {
                match crate::array::index_from_number(n) {
                    Some(idx)
                        if !crate::array::has_accessors(arr, &self.gc_heap)
                            && crate::array::has_own_element(arr, &self.gc_heap, idx) =>
                    {
                        // Dense own element, no index accessors: read directly,
                        // skipping the per-element `idx.to_string()` + accessor
                        // lookup (the hot array element-load path).
                        crate::array::get(arr, &self.gc_heap, idx)
                    }
                    Some(idx) => {
                        let key = idx.to_string();
                        if let Some((getter, _setter)) =
                            crate::array::get_accessor(arr, &self.gc_heap, &key)
                        {
                            match getter {
                                Some(getter) if abstract_ops::is_callable(&getter) => {
                                    let args: smallvec::SmallVec<[Value; 8]> =
                                        smallvec::SmallVec::new();
                                    self.run_callable_sync_rooted(
                                        stack,
                                        context,
                                        &getter,
                                        Value::array(arr),
                                        args,
                                    )?
                                }
                                _ => Value::undefined(),
                            }
                        } else if crate::array::has_own_element(arr, &self.gc_heap, idx) {
                            crate::array::get(arr, &self.gc_heap, idx)
                        } else {
                            // §10.4.2.4 — an absent index falls to Array.prototype.
                            self.load_from_constructor_prototype(
                                stack,
                                context,
                                "Array",
                                &recv,
                                &idx.to_string(),
                            )?
                        }
                    }
                    None => {
                        crate::array::get_named_property(arr, &self.gc_heap, &n.to_display_string())
                            .unwrap_or(Value::undefined())
                    }
                }
            } else {
                return Err(VmError::TypeMismatch);
            }
        } else if let Some(fid) = recv
            .as_function()
            .or_else(|| recv.as_closure(&self.gc_heap).map(|c| c.cached_function_id))
        {
            let owner = recv.as_closure(&self.gc_heap);
            if let Some(key) = idx_value.as_string(&self.gc_heap) {
                match self.ordinary_function_own_property_descriptor(
                    Some(context),
                    owner,
                    fid,
                    &key.to_lossy_string(&self.gc_heap),
                )? {
                    Some(desc) => descriptor_value(&desc),
                    None => Value::undefined(),
                }
            } else if let Some(sym) = idx_value.as_symbol(&self.gc_heap) {
                let key = VmPropertyKey::Symbol(sym);
                match self.ordinary_get_value(stack, context, recv, recv, &key, 0)? {
                    crate::VmGetOutcome::Value(v) => v,
                    crate::VmGetOutcome::InvokeGetter { getter } => {
                        let args: smallvec::SmallVec<[Value; 8]> = smallvec::SmallVec::new();
                        self.run_callable_sync_rooted(stack, context, &getter, recv, args)?
                    }
                }
            } else {
                return Err(VmError::TypeMismatch);
            }
        } else if recv.as_native_function().is_some() {
            // Computed string keys walk the same ordinary [[Get]]
            // ladder as dotted access (own props, then the
            // prototype_override chain down to %Function.prototype%) —
            // the old own-descriptor read made Object['toString']
            // undefined while Object.toString resolved.
            if let Some(key) = idx_value.as_string(&self.gc_heap) {
                let key = key.to_lossy_string(&self.gc_heap);
                let key = VmPropertyKey::OwnedString(key);
                match self.ordinary_get_value(stack, context, recv, recv, &key, 0)? {
                    crate::VmGetOutcome::Value(v) => v,
                    crate::VmGetOutcome::InvokeGetter { getter } => {
                        let args: smallvec::SmallVec<[Value; 8]> = smallvec::SmallVec::new();
                        self.run_callable_sync_rooted(stack, context, &getter, recv, args)?
                    }
                }
            } else if let Some(sym) = idx_value.as_symbol(&self.gc_heap) {
                let key = VmPropertyKey::Symbol(sym);
                match self.ordinary_get_value(stack, context, recv, recv, &key, 0)? {
                    crate::VmGetOutcome::Value(v) => v,
                    crate::VmGetOutcome::InvokeGetter { getter } => {
                        let args: smallvec::SmallVec<[Value; 8]> = smallvec::SmallVec::new();
                        self.run_callable_sync_rooted(stack, context, &getter, recv, args)?
                    }
                }
            } else {
                return Err(VmError::TypeMismatch);
            }
        } else if recv.as_bound_function().is_some() {
            // Same ladder for bound functions — own descriptor first,
            // then %Function.prototype% via the shared resolver.
            if let Some(key) = idx_value.as_string(&self.gc_heap) {
                let key = key.to_lossy_string(&self.gc_heap);
                let key = VmPropertyKey::OwnedString(key);
                match self.ordinary_get_value(stack, context, recv, recv, &key, 0)? {
                    crate::VmGetOutcome::Value(v) => v,
                    crate::VmGetOutcome::InvokeGetter { getter } => {
                        let args: smallvec::SmallVec<[Value; 8]> = smallvec::SmallVec::new();
                        self.run_callable_sync_rooted(stack, context, &getter, recv, args)?
                    }
                }
            } else if let Some(sym) = idx_value.as_symbol(&self.gc_heap) {
                let key = VmPropertyKey::Symbol(sym);
                match self.ordinary_get_value(stack, context, recv, recv, &key, 0)? {
                    crate::VmGetOutcome::Value(v) => v,
                    crate::VmGetOutcome::InvokeGetter { getter } => {
                        let args: smallvec::SmallVec<[Value; 8]> = smallvec::SmallVec::new();
                        self.run_callable_sync_rooted(stack, context, &getter, recv, args)?
                    }
                }
            } else {
                return Err(VmError::TypeMismatch);
            }
        } else if let Some(t) = recv.as_typed_array(&self.gc_heap) {
            if let Some(key) = idx_value.as_string(&self.gc_heap) {
                let name = key.to_lossy_string(&self.gc_heap);
                if let Some(n) = canonical_numeric_index_string(&name) {
                    match typed_array_valid_index(&t, &self.gc_heap, n) {
                        Some(idx) => t.get(&mut self.gc_heap, idx).map_err(crate::oom_to_vm)?,
                        None => Value::undefined(),
                    }
                } else {
                    let mut value = Value::undefined();
                    let mut found = false;
                    if let Some(bag) = t.expando(&self.gc_heap)
                        && let Some(outcome) =
                            Self::expando_own_get_outcome(bag, &self.gc_heap, &name)
                    {
                        value = match outcome {
                            VmGetOutcome::Value(v) => v,
                            VmGetOutcome::InvokeGetter { getter } => {
                                let args: SmallVec<[Value; 8]> = SmallVec::new();
                                self.run_callable_sync_rooted(stack, context, &getter, recv, args)?
                            }
                        };
                        found = true;
                    }
                    if !found {
                        let direct =
                            binary::typed_array_prototype::load_property(&t, &self.gc_heap, &name);
                        value = if direct.is_undefined() {
                            let kind_name = t.kind().name();
                            self.load_from_constructor_prototype(
                                stack, context, kind_name, &recv, &name,
                            )?
                        } else {
                            direct
                        };
                    }
                    value
                }
            } else if let Some(n) = idx_value.as_number() {
                match crate::array::index_from_number(n) {
                    Some(idx) => match t.get_uint8_value(&self.gc_heap, idx) {
                        Some(value) => value,
                        None => t.get(&mut self.gc_heap, idx).map_err(crate::oom_to_vm)?,
                    },
                    None => Value::undefined(),
                }
            } else if let Some(sym) = idx_value.as_symbol(&self.gc_heap) {
                let key = VmPropertyKey::Symbol(sym);
                match self.ordinary_get_value(stack, context, recv, recv, &key, 0)? {
                    crate::VmGetOutcome::Value(v) => v,
                    crate::VmGetOutcome::InvokeGetter { getter } => {
                        let args: smallvec::SmallVec<[Value; 8]> = smallvec::SmallVec::new();
                        self.run_callable_sync_rooted(stack, context, &getter, recv, args)?
                    }
                }
            } else {
                return Err(VmError::TypeMismatch);
            }
        } else if let Some(s) = recv.as_string(&self.gc_heap) {
            // §10.4.3 String exotic [[GetOwnProperty]] — UTF-16 code
            // unit indexed access then String.prototype fallback.
            if let Some(key) = idx_value.as_string(&self.gc_heap) {
                let name = key.to_lossy_string(&self.gc_heap);
                self.load_string_primitive_property(stack, context, &recv, s, &name)?
            } else if let Some(n) = idx_value.as_number() {
                // §10.4.3.5 — every numeric index, in- or out-of-bounds,
                // goes through the one String [[Get]] funnel so OOB
                // reads return `undefined` (not the empty string) and
                // the prototype fallback stays consistent.
                let name = n.to_display_string();
                self.load_string_primitive_property(stack, context, &recv, s, &name)?
            } else if let Some(sym) = idx_value.as_symbol(&self.gc_heap) {
                let key = VmPropertyKey::Symbol(sym);
                let proto = self.constructor_prototype_value("String")?;
                if proto.is_nullish() {
                    Value::undefined()
                } else {
                    match self.ordinary_get_value(stack, context, proto, recv, &key, 0)? {
                        crate::VmGetOutcome::Value(v) => v,
                        crate::VmGetOutcome::InvokeGetter { getter } => {
                            let args: smallvec::SmallVec<[Value; 8]> = smallvec::SmallVec::new();
                            self.run_callable_sync_rooted(stack, context, &getter, recv, args)?
                        }
                    }
                }
            } else {
                return Err(VmError::TypeMismatch);
            }
        } else if let Some(r) = recv.as_regexp() {
            if let Some(key) = idx_value.as_string(&self.gc_heap) {
                // Computed string-key on RegExp.
                let name = key.to_lossy_string(&self.gc_heap);
                if let Some(bag) = r.expando(&self.gc_heap)
                    && let Some(value) = crate::object::get(bag, &self.gc_heap, &name)
                {
                    value
                } else {
                    let direct = regexp_prototype::load_property(&r, &mut self.gc_heap, &name);
                    if direct.is_undefined() {
                        self.load_from_constructor_prototype(
                            stack, context, "RegExp", &recv, &name,
                        )?
                    } else {
                        direct
                    }
                }
            } else if let Some(sym) = idx_value.as_symbol(&self.gc_heap) {
                let key = VmPropertyKey::Symbol(sym);
                match self.ordinary_get_value(stack, context, recv, recv, &key, 0)? {
                    crate::VmGetOutcome::Value(v) => v,
                    crate::VmGetOutcome::InvokeGetter { getter } => {
                        let args: smallvec::SmallVec<[Value; 8]> = smallvec::SmallVec::new();
                        self.run_callable_sync_rooted(stack, context, &getter, recv, args)?
                    }
                }
            } else {
                return Err(VmError::TypeMismatch);
            }
        } else if let Some(m) = recv.as_map() {
            if let Some(sym) = idx_value.as_symbol(&self.gc_heap) {
                if sym
                    .well_known_tag()
                    .is_some_and(|t| t == symbol::WellKnown::Iterator)
                {
                    collections_prototype::make_map_iterator_factory(m, &mut self.gc_heap)?
                } else {
                    let key = VmPropertyKey::Symbol(sym);
                    match self.ordinary_get_value(stack, context, recv, recv, &key, 0)? {
                        crate::VmGetOutcome::Value(v) => v,
                        crate::VmGetOutcome::InvokeGetter { getter } => {
                            let args: smallvec::SmallVec<[Value; 8]> = smallvec::SmallVec::new();
                            self.run_callable_sync_rooted(stack, context, &getter, recv, args)?
                        }
                    }
                }
            } else {
                return Err(VmError::TypeMismatch);
            }
        } else if let Some(set) = recv.as_set() {
            if let Some(sym) = idx_value.as_symbol(&self.gc_heap) {
                if sym
                    .well_known_tag()
                    .is_some_and(|t| t == symbol::WellKnown::Iterator)
                {
                    collections_prototype::make_set_iterator_factory(set, &mut self.gc_heap)?
                } else {
                    let key = VmPropertyKey::Symbol(sym);
                    match self.ordinary_get_value(stack, context, recv, recv, &key, 0)? {
                        crate::VmGetOutcome::Value(v) => v,
                        crate::VmGetOutcome::InvokeGetter { getter } => {
                            let args: smallvec::SmallVec<[Value; 8]> = smallvec::SmallVec::new();
                            self.run_callable_sync_rooted(stack, context, &getter, recv, args)?
                        }
                    }
                }
            } else {
                return Err(VmError::TypeMismatch);
            }
        } else if recv.is_class_constructor()
            || recv.is_weak_map()
            || recv.is_weak_set()
            || recv.is_weak_ref()
            || recv.is_finalization_registry()
            || recv.is_promise()
            || recv.is_array_buffer()
            || recv.is_data_view()
        {
            // §10.2 — symbol-keyed access on callable / class /
            // collection exotics walks via ordinary [[Get]].
            if let Some(sym) = idx_value.as_symbol(&self.gc_heap) {
                let key = VmPropertyKey::Symbol(sym);
                match self.ordinary_get_value(stack, context, recv, recv, &key, 0)? {
                    crate::VmGetOutcome::Value(v) => v,
                    crate::VmGetOutcome::InvokeGetter { getter } => {
                        let args: smallvec::SmallVec<[Value; 8]> = smallvec::SmallVec::new();
                        self.run_callable_sync_rooted(stack, context, &getter, recv, args)?
                    }
                }
            } else {
                return Err(VmError::TypeMismatch);
            }
        } else if recv.is_symbol() || recv.is_boolean() || recv.is_number() || recv.is_big_int() {
            // §7.1.18 ToObject — primitive receivers walk wrapper
            // prototype for string/symbol/number key access.
            let ctor_name = if recv.is_symbol() {
                "Symbol"
            } else if recv.is_boolean() {
                "Boolean"
            } else if recv.is_number() {
                "Number"
            } else {
                "BigInt"
            };
            let key = if let Some(sym) = idx_value.as_symbol(&self.gc_heap) {
                VmPropertyKey::Symbol(sym)
            } else if let Some(s) = idx_value.as_string(&self.gc_heap) {
                VmPropertyKey::OwnedString(s.to_lossy_string(&self.gc_heap))
            } else if let Some(n) = idx_value.as_number() {
                VmPropertyKey::OwnedString(n.to_display_string())
            } else {
                return Err(VmError::TypeMismatch);
            };
            let proto = self.constructor_prototype_value(ctor_name)?;
            if proto.is_nullish() {
                Value::undefined()
            } else {
                match self.ordinary_get_value(stack, context, proto, recv, &key, 0)? {
                    crate::VmGetOutcome::Value(v) => v,
                    crate::VmGetOutcome::InvokeGetter { getter } => {
                        let args: smallvec::SmallVec<[Value; 8]> = smallvec::SmallVec::new();
                        self.run_callable_sync_rooted(stack, context, &getter, recv, args)?
                    }
                }
            }
        } else {
            // Remaining object-typed receivers (class constructors,
            // module namespaces, ...) resolve through the generic
            // value-level [[Get]] funnel.
            let key = if let Some(sym) = idx_value.as_symbol(&self.gc_heap) {
                VmPropertyKey::Symbol(sym)
            } else if let Some(k) = idx_value.as_string(&self.gc_heap) {
                VmPropertyKey::OwnedString(k.to_lossy_string(&self.gc_heap))
            } else if let Some(n) = idx_value.as_number() {
                VmPropertyKey::OwnedString(n.to_display_string())
            } else {
                return Err(VmError::TypeMismatch);
            };
            match self.ordinary_get_value(stack, context, recv, recv, &key, 0)? {
                crate::VmGetOutcome::Value(v) => v,
                crate::VmGetOutcome::InvokeGetter { getter } => {
                    let args: smallvec::SmallVec<[Value; 8]> = smallvec::SmallVec::new();
                    self.run_callable_sync_rooted(stack, context, &getter, recv, args)?
                }
            }
        };
        result = value;
        drop(roots);
        Ok(result)
    }

    /// §10.4.5.16 step 2 — convert a value being stored into a typed
    /// array with `ToBigInt` for BigInt element kinds and `ToNumber`
    /// otherwise (firing the operand's coercion and throwing for a
    /// Symbol / cross-numeric type), then narrow it to the element
    /// representation. The conversion runs before the index check.
    pub(crate) fn typed_array_coerce_element(
        &mut self,
        stack: &mut ActivationStack,
        context: &ExecutionContext,
        kind: crate::binary::TypedArrayKind,
        value: Value,
    ) -> Result<Value, VmError> {
        let converted = if kind.is_bigint() {
            Value::big_int(crate::coerce::to_big_int_or_throw(
                self, stack, context, &value,
            )?)
        } else {
            Value::number(crate::coerce::to_number_or_throw(
                self, stack, context, &value,
            )?)
        };
        binary::dispatch::coerce_element_for_store(&mut self.gc_heap, kind, &converted)
    }
}
