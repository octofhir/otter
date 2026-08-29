//! Property-key coercion and own-key enumeration.
//!
//! # Contents
//! - `ToPrimitive` / `ToPropertyDescriptor` / `ToPropertyKey` evaluation.
//! - `[[OwnPropertyKeys]]` and its proxy invariant checks.
//! - `[[SetPrototypeOf]]` in its proxy-aware form.
//!
//! # Invariants
//! - Coercion may call user code, so every entry here re-reads its receiver
//!   from a rooted slot after the call rather than caching it across.

use super::*;
use crate::activation_stack::ActivationStack;
use crate::{
    ExecutionContext, Interpreter, Local, Value, VmError, VmGetOutcome, VmPropertyKey,
    abstract_ops, array, function_metadata, object, proxy, symbol, to_length,
};
use smallvec::SmallVec;
use std::collections::BTreeSet;

impl Interpreter {
    /// §7.1.1 ToPrimitive synchronous helper. Used by sync callers
    /// (Reflect dispatcher, set / has / define paths) that need
    /// observable coercion outside the bytecode dispatch ladder.
    ///
    /// # See also
    /// - <https://tc39.es/ecma262/#sec-toprimitive>
    /// - <https://tc39.es/ecma262/#sec-ordinarytoprimitive>
    pub(crate) fn evaluate_to_primitive(
        &mut self,
        stack: &mut ActivationStack,
        context: &ExecutionContext,
        input: &Value,
        hint: abstract_ops::ToPrimitiveHint,
    ) -> Result<Value, VmError> {
        if abstract_ops::is_primitive(input) {
            return Ok(*input);
        }
        self.with_handle_scope(|interp, scope| {
            let input_handle = interp.scoped_value(scope, *input);
            // Step 1.a — try `@@toPrimitive` via OrdinaryGet on the
            // object's prototype chain. Falls back to ordinary toString /
            // valueOf when the exotic hook is absent.
            let to_prim_sym = interp
                .well_known_symbols
                .get(symbol::WellKnown::ToPrimitive);
            let current_input = interp.escape_scoped(input_handle);
            let exotic = match interp.ordinary_get_value(
                stack,
                context,
                current_input,
                current_input,
                &VmPropertyKey::Symbol(to_prim_sym),
                0,
            )? {
                VmGetOutcome::Value(value) => value,
                VmGetOutcome::InvokeGetter { getter } => interp.run_callable_sync_rooted(
                    stack,
                    context,
                    &getter,
                    interp.escape_scoped(input_handle),
                    SmallVec::new(),
                )?,
            };
            let exotic_handle = interp.scoped_value(scope, exotic);
            if !interp.escape_scoped(exotic_handle).is_nullish() {
                if !interp.is_callable_runtime(&interp.escape_scoped(exotic_handle)) {
                    return Err(interp.err_type(
                        ("Symbol.toPrimitive method is not callable".to_string()).into(),
                    ));
                }
                let hint_handle = interp.scoped_string(scope, hint.as_token())?;
                let args: SmallVec<[Value; 8]> =
                    smallvec::smallvec![interp.escape_scoped(hint_handle)];
                let result = interp.run_callable_sync_rooted(
                    stack,
                    context,
                    &interp.escape_scoped(exotic_handle),
                    interp.escape_scoped(input_handle),
                    args,
                )?;
                if abstract_ops::is_primitive(&result) {
                    return Ok(result);
                }
                return Err(interp
                    .err_type(("Symbol.toPrimitive returned a non-primitive".to_string()).into()));
            }
            let current_input = interp.escape_scoped(input_handle);
            interp.evaluate_ordinary_to_primitive(stack, context, &current_input, hint)
        })
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
        stack: &mut ActivationStack,
        context: &ExecutionContext,
        input: &Value,
        hint: abstract_ops::ToPrimitiveHint,
    ) -> Result<Value, VmError> {
        self.with_handle_scope(|interp, scope| {
            let input_handle = interp.scoped_value(scope, *input);
            let names: [&str; 2] = match hint {
                abstract_ops::ToPrimitiveHint::String => ["toString", "valueOf"],
                _ => ["valueOf", "toString"],
            };
            for name in names {
                let current_input = interp.escape_scoped(input_handle);
                let method = match interp.ordinary_get_value(
                    stack,
                    context,
                    current_input,
                    current_input,
                    &VmPropertyKey::String(name),
                    0,
                )? {
                    VmGetOutcome::Value(value) => value,
                    VmGetOutcome::InvokeGetter { getter } => interp.run_callable_sync_rooted(
                        stack,
                        context,
                        &getter,
                        interp.escape_scoped(input_handle),
                        SmallVec::new(),
                    )?,
                };
                let method_handle = interp.scoped_value(scope, method);
                if !interp.is_callable_runtime(&interp.escape_scoped(method_handle)) {
                    continue;
                }
                let result = interp.run_callable_sync_rooted(
                    stack,
                    context,
                    &interp.escape_scoped(method_handle),
                    interp.escape_scoped(input_handle),
                    SmallVec::new(),
                )?;
                if abstract_ops::is_primitive(&result) {
                    return Ok(result);
                }
            }
            Err(interp.err_type(
                ("OrdinaryToPrimitive could not convert object to primitive".to_string()).into(),
            ))
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
        stack: &mut ActivationStack,
        context: &ExecutionContext,
        attributes: &Value,
    ) -> Result<object::PartialPropertyDescriptor, VmError> {
        // Step 1 — `Type(Obj) is not Object → throw TypeError`. We
        // gate via the broader "type Object" check that includes
        // proxies / exotic value kinds.
        if !attributes.is_object_type() {
            return Err(self
                .err_type(("ToPropertyDescriptor argument must be an Object".to_string()).into()));
        }

        self.with_handle_scope(|interp, scope| {
            let attributes = interp.scoped_value(scope, *attributes);
            let read_field = |interp: &mut Self,
                              stack: &mut ActivationStack,
                              name: &str|
             -> Result<Option<Value>, VmError> {
                let key = VmPropertyKey::String(name);
                let current = interp.escape_scoped(attributes);
                if !interp.ordinary_has_property_value(stack, context, current, &key, 0)? {
                    return Ok(None);
                }
                let current = interp.escape_scoped(attributes);
                let value =
                    match interp.ordinary_get_value(stack, context, current, current, &key, 0)? {
                        VmGetOutcome::Value(v) => v,
                        VmGetOutcome::InvokeGetter { getter } => interp.run_callable_sync_rooted(
                            stack,
                            context,
                            &getter,
                            interp.escape_scoped(attributes),
                            SmallVec::new(),
                        )?,
                    };
                Ok(Some(value))
            };

            let enumerable =
                read_field(interp, stack, "enumerable")?.map(|v| v.to_boolean(&interp.gc_heap));
            let configurable =
                read_field(interp, stack, "configurable")?.map(|v| v.to_boolean(&interp.gc_heap));
            let value = read_field(interp, stack, "value")?.map(|v| interp.scoped_value(scope, v));
            let writable =
                read_field(interp, stack, "writable")?.map(|v| v.to_boolean(&interp.gc_heap));
            let get = read_field(interp, stack, "get")?.map(|v| interp.scoped_value(scope, v));
            if let Some(get) = get {
                let current = interp.escape_scoped(get);
                if !current.is_undefined() && !interp.is_callable_runtime(&current) {
                    return Err(interp.err_type(
                        ("Property descriptor `get` is not callable".to_string()).into(),
                    ));
                }
            }
            let set = read_field(interp, stack, "set")?.map(|v| interp.scoped_value(scope, v));
            if let Some(set) = set {
                let current = interp.escape_scoped(set);
                if !current.is_undefined() && !interp.is_callable_runtime(&current) {
                    return Err(interp.err_type(
                        ("Property descriptor `set` is not callable".to_string()).into(),
                    ));
                }
            }

            let descriptor = object::PartialPropertyDescriptor {
                value: value.map(|value| interp.escape_scoped(value)),
                writable,
                get: get.map(|get| interp.escape_scoped(get)),
                set: set.map(|set| interp.escape_scoped(set)),
                enumerable,
                configurable,
            };
            if descriptor.is_accessor() && descriptor.is_data() {
                return Err(interp.err_type(
                    ("Property descriptor mixes accessor + data fields".to_string()).into(),
                ));
            }
            Ok(descriptor)
        })
    }

    /// §7.1.19 ToPropertyKey synchronous helper. Used by Reflect /
    /// Object.defineProperty / Reflect.set / etc. for descriptor key
    /// coercion outside the dispatch ladder.
    ///
    /// # See also
    /// - <https://tc39.es/ecma262/#sec-topropertykey>
    pub(crate) fn evaluate_to_property_key(
        &mut self,
        stack: &mut ActivationStack,
        context: &ExecutionContext,
        input: &Value,
    ) -> Result<VmPropertyKey<'static>, VmError> {
        let primitive = self.evaluate_to_primitive(
            stack,
            context,
            input,
            abstract_ops::ToPrimitiveHint::String,
        )?;
        if let Some(sym) = primitive.as_symbol(&self.gc_heap) {
            return Ok(VmPropertyKey::Symbol(sym));
        }
        Ok(VmPropertyKey::OwnedString(
            primitive.display_string(&self.gc_heap),
        ))
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
    /// Allocate one own-property key string and append it to `keys`, building
    /// it inside a handle scope so the in-flight key list — plus the receiver
    /// `target` and any already-collected `symbols` — is rooted in the arena
    /// across the allocation.
    ///
    /// `[[OwnPropertyKeys]]` builds its result one `JsString` at a time; each
    /// allocation can drive a moving collection that would otherwise leave every
    /// previously collected young key (and the receiver) dangling. Parking the
    /// live values in the arena lets the collector rewrite them in place; they
    /// are read back out afterward so the caller's plain `Vec`/`Value` locals
    /// reflect any relocation.
    pub(crate) fn push_own_key_string(
        &mut self,
        keys: &mut Vec<Value>,
        target: &mut Value,
        symbols: &mut [Value],
        name: &str,
    ) -> Result<(), VmError> {
        self.push_own_key_strings(keys, target, symbols, std::iter::once(name))
    }

    /// Append several own string keys at once.
    ///
    /// The accumulated set is parked before the first allocation and read
    /// back after the last, so a collection during any of them rewrites it
    /// in place. Doing that per key instead is what made enumerating a
    /// large object quadratic.
    ///
    /// # Errors
    /// Returns the failure behind allocating a key string.
    pub(crate) fn push_own_key_strings<'a>(
        &mut self,
        keys: &mut Vec<Value>,
        target: &mut Value,
        symbols: &mut [Value],
        names: impl IntoIterator<Item = &'a str>,
    ) -> Result<(), VmError> {
        self.with_handle_scope(|interp, scope| {
            let key_handles: Vec<Local> = keys
                .iter()
                .map(|k| interp.scoped_value(scope, *k))
                .collect();
            let target_handle = interp.scoped_value(scope, *target);
            let symbol_handles: Vec<Local> = symbols
                .iter()
                .map(|s| interp.scoped_value(scope, *s))
                .collect();
            let mut fresh: Vec<Local> = Vec::new();
            for name in names {
                fresh.push(interp.scoped_string(scope, name)?);
            }
            // Every allocation above is a collection point; read the
            // (now collector-updated) arena slots back into the caller's
            // locals.
            for (slot, handle) in keys.iter_mut().zip(&key_handles) {
                *slot = interp.escape_scoped(*handle);
            }
            *target = interp.escape_scoped(target_handle);
            for (slot, handle) in symbols.iter_mut().zip(&symbol_handles) {
                *slot = interp.escape_scoped(*handle);
            }
            keys.reserve(fresh.len());
            for handle in fresh {
                keys.push(interp.escape_scoped(handle));
            }
            Ok(())
        })
    }

    pub(crate) fn own_property_keys_value(
        &mut self,
        stack: &mut ActivationStack,
        context: &ExecutionContext,
        target: &Value,
    ) -> Result<Vec<Value>, VmError> {
        let target = self.with_handle_scope(|interp, scope| -> Result<Value, VmError> {
            let target = interp.scoped_value(scope, *target);
            let current = interp.escape_scoped(target);
            interp.ensure_deferred_namespace_ready(stack, context, &current, true)?;
            Ok(interp.escape_scoped(target))
        })?;
        // Own a mutable copy of the receiver so `push_own_key_string` can refresh
        // it after each key allocation: a moving collection during key building
        // relocates the receiver, and the branches below re-read it to gather
        // symbol keys after the string keys are built.
        let mut target = target;
        // WeakRef / FinalizationRegistry are ordinary objects whose
        // observable own keys (no expando installed) are empty.
        if target.as_weak_ref().is_some() || target.as_finalization_registry().is_some() {
            return Ok(Vec::new());
        }
        // §10.4.6.13 [[OwnPropertyKeys]] — exported string names in
        // ascending code-unit order, followed by the namespace's own
        // symbol keys (`@@toStringTag`).
        if let Some(obj) = target.as_object()
            && object::module_namespace_env(obj, &self.gc_heap).is_some()
        {
            let mut keys: Vec<Value> = Vec::new();
            let names = self.module_namespace_export_names(obj);
            self.push_own_key_strings(
                &mut keys,
                &mut target,
                &mut [],
                names.iter().map(String::as_str),
            )?;
            // Re-read the receiver: the key allocations above may have moved
            // it; `target` is rewritten in place by the rooted visitor.
            let obj = target.as_object().ok_or(VmError::InvalidOperand)?;
            let symbols: Vec<Value> = object::with_properties(obj, &self.gc_heap, |p| {
                p.symbol_keys()
                    .filter(|k| !k.is_private_name())
                    .map(Value::symbol)
                    .collect()
            });
            keys.extend(symbols);
            return Ok(keys);
        }
        if let Some(proxy) = target.as_proxy() {
            if proxy.is_revoked(&self.gc_heap) {
                return Err(self.err_type(
                    ("Cannot perform 'ownKeys' on a proxy that has been revoked".to_string())
                        .into(),
                ));
            }
            let trap_args: SmallVec<[Value; 8]> = smallvec::smallvec![proxy.target(&self.gc_heap)];
            return match self.invoke_proxy_trap(stack, context, &proxy, "ownKeys", trap_args)? {
                crate::object_internal_ops::ProxyTrap::Trapped(trap_result) => {
                    let trap_keys = self.create_list_from_array_like_property_keys(
                        stack,
                        context,
                        trap_result,
                    )?;
                    self.validate_proxy_own_keys(stack, context, &proxy, trap_keys)
                }
                crate::object_internal_ops::ProxyTrap::NoTrap {
                    target: fallthrough_target,
                } => self.own_property_keys_value(stack, context, &fallthrough_target),
            };
        }
        // §10.4.5.11 TypedArray [[OwnPropertyKeys]] — integer indices
        // in ascending order, then expando string keys in insertion
        // order, then expando symbol keys.
        if let Some(t) = target.as_typed_array(&self.gc_heap) {
            let mut keys: Vec<Value> = Vec::new();
            if !t.buffer(&self.gc_heap).is_detached(&self.gc_heap) {
                let len = t.length(&self.gc_heap);
                keys.reserve(len);
                let names: Vec<String> = (0..len).map(|idx| idx.to_string()).collect();
                self.push_own_key_strings(
                    &mut keys,
                    &mut target,
                    &mut [],
                    names.iter().map(String::as_str),
                )?;
            }
            // Re-read the receiver after the index-key allocations above.
            let t = target
                .as_typed_array(&self.gc_heap)
                .ok_or(VmError::InvalidOperand)?;
            if let Some(bag) = t.expando(&self.gc_heap) {
                let (strings, mut symbols): (Vec<String>, Vec<Value>) =
                    object::with_properties(bag, &self.gc_heap, |p| {
                        (
                            p.keys().map(str::to_string).collect(),
                            p.symbol_keys()
                                .filter(|k| !k.is_private_name())
                                .map(Value::symbol)
                                .collect(),
                        )
                    });
                self.push_own_key_strings(
                    &mut keys,
                    &mut target,
                    &mut symbols,
                    strings.iter().map(String::as_str),
                )?;
                keys.extend(symbols);
            }
            return Ok(keys);
        }
        if let Some(obj) = target.as_object() {
            let mut keys: Vec<Value> = Vec::new();
            let string_data = object::string_data(obj, &self.gc_heap);
            if let Some(value) = &string_data {
                keys.reserve(value.len() as usize + 1);
                for idx in 0..value.len() {
                    let key = idx.to_string();
                    self.push_own_key_string(&mut keys, &mut target, &mut [], &key)?;
                }
            }
            let is_string_exotic = string_data.is_some();
            // Re-read the receiver after the index-key allocations above.
            let obj = target.as_object().ok_or(VmError::InvalidOperand)?;
            let (ordinary_strings, mut symbols): (Vec<String>, Vec<Value>) =
                object::with_properties(obj, &self.gc_heap, |p| {
                    (
                        p.keys().map(str::to_string).collect(),
                        p.symbol_keys()
                            .filter(|k| !k.is_private_name())
                            .map(Value::symbol)
                            .collect(),
                    )
                });
            if is_string_exotic {
                let string_len = string_data.as_ref().map_or(0, |value| value.len());
                let mut indexed = BTreeSet::new();
                let mut non_index_strings = Vec::new();
                for key in ordinary_strings {
                    if key == "length" {
                        continue;
                    }
                    match object::array_index_property_name(&key) {
                        Some(index) if index >= string_len => {
                            indexed.insert(index);
                        }
                        Some(_) => {}
                        None => non_index_strings.push(key),
                    }
                }
                let indexed: Vec<String> = indexed.into_iter().map(|i| i.to_string()).collect();
                self.push_own_key_strings(
                    &mut keys,
                    &mut target,
                    &mut symbols,
                    indexed
                        .iter()
                        .map(String::as_str)
                        .chain(std::iter::once("length"))
                        .chain(non_index_strings.iter().map(String::as_str)),
                )?;
            } else {
                self.push_own_key_strings(
                    &mut keys,
                    &mut target,
                    &mut symbols,
                    ordinary_strings.iter().map(String::as_str),
                )?;
            }
            keys.extend(symbols);
            return Ok(keys);
        }
        if let Some(arr) = target.as_array() {
            let (indices, string_keys) = array::own_index_and_string_keys(arr, &self.gc_heap);
            let mut keys: Vec<Value> = Vec::with_capacity(indices.len() + string_keys.len() + 2);
            let indices: Vec<String> = indices.into_iter().map(|idx| idx.to_string()).collect();
            // §10.4.2 Array exotic objects always expose `length`.
            self.push_own_key_strings(
                &mut keys,
                &mut target,
                &mut [],
                indices
                    .iter()
                    .map(String::as_str)
                    .chain(std::iter::once("length"))
                    .chain(string_keys.iter().map(String::as_str)),
            )?;
            // §10.4.2 — own symbol-keyed properties follow the
            // string keys per §7.3.22 OrdinaryOwnPropertyKeys
            // ordering. Re-read the receiver after the key
            // allocations above.
            let arr = target.as_array().ok_or(VmError::InvalidOperand)?;
            for sym in array::own_symbol_keys(arr, &self.gc_heap) {
                keys.push(Value::symbol(sym));
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
            let names = self.ordinary_function_own_property_keys(context, owner, function_id);
            let mut keys: Vec<Value> = Vec::with_capacity(names.len());
            self.push_own_key_strings(
                &mut keys,
                &mut target,
                &mut [],
                names.iter().map(String::as_str),
            )?;
            return Ok(keys);
        }
        if let Some(native) = target.as_native_function() {
            let names = native.own_property_keys(&self.gc_heap);
            let mut keys: Vec<Value> = Vec::with_capacity(names.len());
            self.push_own_key_strings(
                &mut keys,
                &mut target,
                &mut [],
                names.iter().map(String::as_str),
            )?;
            return Ok(keys);
        }
        if let Some(bound) = target.as_bound_function() {
            let names = function_metadata::bound_own_property_keys(&bound, &self.gc_heap);
            let mut keys: Vec<Value> = Vec::with_capacity(names.len());
            self.push_own_key_strings(
                &mut keys,
                &mut target,
                &mut [],
                names.iter().map(String::as_str),
            )?;
            return Ok(keys);
        }
        if let Some(class) = target.as_class_constructor() {
            let names = self.class_constructor_own_property_keys(Some(context), class)?;
            let mut keys: Vec<Value> = Vec::with_capacity(names.len());
            self.push_own_key_strings(
                &mut keys,
                &mut target,
                &mut [],
                names.iter().map(String::as_str),
            )?;
            // §10.1.11 — symbol keys follow the string keys. A class
            // constructor's own symbol-keyed properties (e.g. a static
            // `[sym]() {}` method) live on its statics object. Re-read
            // the constructor after the key allocations above.
            let class = target
                .as_class_constructor()
                .ok_or(VmError::InvalidOperand)?;
            let statics = class.statics(&self.gc_heap);
            let symbols: Vec<Value> = object::with_properties(statics, &self.gc_heap, |p| {
                p.symbol_keys()
                    .filter(|k| !k.is_private_name())
                    .map(Value::symbol)
                    .collect()
            });
            keys.extend(symbols);
            return Ok(keys);
        }
        if target.as_regexp().is_some() {
            let mut keys = Vec::new();
            self.push_own_key_string(&mut keys, &mut target, &mut [], "lastIndex")?;
            // Re-read the receiver after the key allocation above.
            let regexp = target.as_regexp().ok_or(VmError::InvalidOperand)?;
            if let Some(expando) = regexp.expando(&self.gc_heap) {
                let (strings, mut symbols): (Vec<String>, Vec<Value>) =
                    object::with_properties(expando, &self.gc_heap, |p| {
                        (
                            p.keys().map(str::to_string).collect(),
                            p.symbol_keys()
                                .filter(|k| !k.is_private_name())
                                .map(Value::symbol)
                                .collect(),
                        )
                    });
                for key in strings {
                    self.push_own_key_string(&mut keys, &mut target, &mut symbols, &key)?;
                }
                keys.extend(symbols);
            }
            return Ok(keys);
        }
        if let Some(dv) = target.as_data_view() {
            // §25.3 — own keys are exactly the ordinary expando entries
            // (byteLength / byteOffset / buffer are prototype getters).
            let mut keys = Vec::new();
            if let Some(expando) = dv.expando(&self.gc_heap) {
                let (strings, mut symbols): (Vec<String>, Vec<Value>) =
                    object::with_properties(expando, &self.gc_heap, |p| {
                        (
                            p.keys().map(str::to_string).collect(),
                            p.symbol_keys()
                                .filter(|k| !k.is_private_name())
                                .map(Value::symbol)
                                .collect(),
                        )
                    });
                for key in strings {
                    self.push_own_key_string(&mut keys, &mut target, &mut symbols, &key)?;
                }
                keys.extend(symbols);
            }
            return Ok(keys);
        }
        if target.is_map() || target.is_set() || target.is_generator() {
            // Own keys on a Map/Set/Generator are exactly the lazy expando entries;
            // size and the iterator methods are prototype properties.
            let mut keys = Vec::new();
            if let Some(expando) = self.collection_expando(&target) {
                let (strings, mut symbols): (Vec<String>, Vec<Value>) =
                    object::with_properties(expando, &self.gc_heap, |p| {
                        (
                            p.keys().map(str::to_string).collect(),
                            p.symbol_keys()
                                .filter(|k| !k.is_private_name())
                                .map(Value::symbol)
                                .collect(),
                        )
                    });
                for key in strings {
                    self.push_own_key_string(&mut keys, &mut target, &mut symbols, &key)?;
                }
                keys.extend(symbols);
            }
            return Ok(keys);
        }
        if let Some(t) = target.as_temporal(&self.gc_heap) {
            // Own keys are exactly the ordinary expando entries; the
            // year/month/… accessors are prototype properties.
            let mut keys = Vec::new();
            if let Some(expando) = t.expando(&self.gc_heap) {
                let (strings, mut symbols): (Vec<String>, Vec<Value>) =
                    object::with_properties(expando, &self.gc_heap, |p| {
                        (
                            p.keys().map(str::to_string).collect(),
                            p.symbol_keys()
                                .filter(|k| !k.is_private_name())
                                .map(Value::symbol)
                                .collect(),
                        )
                    });
                for key in strings {
                    self.push_own_key_string(&mut keys, &mut target, &mut symbols, &key)?;
                }
                keys.extend(symbols);
            }
            return Ok(keys);
        }
        if target.is_intl() || target.is_iterator() {
            // ECMA-402 service instances and builtin iterators are
            // ordinary objects; own keys are exactly the user-props
            // side-table entries.
            let mut keys = Vec::new();
            if let Some(bag) = self.non_gc_exotic_user_props(&target) {
                let (strings, mut symbols): (Vec<String>, Vec<Value>) =
                    object::with_properties(bag, &self.gc_heap, |p| {
                        (
                            p.keys().map(str::to_string).collect(),
                            p.symbol_keys()
                                .filter(|k| !k.is_private_name())
                                .map(Value::symbol)
                                .collect(),
                        )
                    });
                for key in strings {
                    self.push_own_key_string(&mut keys, &mut target, &mut symbols, &key)?;
                }
                keys.extend(symbols);
            }
            return Ok(keys);
        }
        Ok(Vec::new())
    }

    /// §7.3.18 CreateListFromArrayLike with elementTypes set to
    /// «String, Symbol» — used by Proxy `ownKeys` trap result
    /// validation per §10.5.11 step 8.
    pub(crate) fn create_list_from_array_like_property_keys(
        &mut self,
        stack: &mut ActivationStack,
        context: &ExecutionContext,
        list_value: Value,
    ) -> Result<Vec<Value>, VmError> {
        if !(list_value.is_object() || list_value.is_array() || list_value.is_proxy()) {
            return Err(
                self.err_type(("Proxy ownKeys trap result is not an Object".to_string()).into())
            );
        }
        let len_value = match self.ordinary_get_value(
            stack,
            context,
            list_value,
            list_value,
            &VmPropertyKey::String("length"),
            0,
        )? {
            VmGetOutcome::Value(v) => v,
            VmGetOutcome::InvokeGetter { getter } => {
                let args: SmallVec<[Value; 8]> = SmallVec::new();
                self.run_callable_sync_rooted(stack, context, &getter, list_value, args)?
            }
        };
        let len = to_length(&len_value, &self.gc_heap)?;
        let mut out: Vec<Value> = Vec::with_capacity(len);
        for i in 0..len {
            let key = VmPropertyKey::OwnedString(i.to_string());
            let element =
                match self.ordinary_get_value(stack, context, list_value, list_value, &key, 0)? {
                    VmGetOutcome::Value(v) => v,
                    VmGetOutcome::InvokeGetter { getter } => {
                        let args: SmallVec<[Value; 8]> = SmallVec::new();
                        self.run_callable_sync_rooted(stack, context, &getter, list_value, args)?
                    }
                };
            if !(element.is_string() || element.is_symbol()) {
                return Err(self.err_type(
                    ("Proxy ownKeys trap result contains a non-property-key entry".to_string())
                        .into(),
                ));
            }
            out.push(element);
        }
        Ok(out)
    }

    /// §10.5.11 steps 9–17 — validate a Proxy `ownKeys` trap result
    /// against the target's own keys.
    pub(crate) fn validate_proxy_own_keys(
        &mut self,
        stack: &mut ActivationStack,
        context: &ExecutionContext,
        proxy: &proxy::JsProxy,
        trap_result: Vec<Value>,
    ) -> Result<Vec<Value>, VmError> {
        // Step 9 — reject duplicates. String keys hash into a set
        // (the spec requires linear behaviour here — see V8's
        // ownKeys-linear regression); symbol keys are compared
        // pairwise, which stays cheap because real handler results
        // carry at most a handful of symbols.
        let trap_strs: Vec<Option<String>> = trap_result
            .iter()
            .map(|v| {
                v.as_string(&self.gc_heap)
                    .map(|s| s.to_lossy_string(&self.gc_heap))
            })
            .collect();
        {
            let mut seen: std::collections::HashSet<&str> =
                std::collections::HashSet::with_capacity(trap_result.len());
            let mut symbol_indices: Vec<usize> = Vec::new();
            for (i, snap) in trap_strs.iter().enumerate() {
                match snap {
                    Some(name) => {
                        if !seen.insert(name.as_str()) {
                            return Err(self.err_type(
                                ("Proxy ownKeys trap result contains duplicate entries"
                                    .to_string())
                                .into(),
                            ));
                        }
                    }
                    None => symbol_indices.push(i),
                }
            }
            for a in 0..symbol_indices.len() {
                for b in (a + 1)..symbol_indices.len() {
                    if same_property_key(
                        &trap_result[symbol_indices[a]],
                        &trap_result[symbol_indices[b]],
                        &self.gc_heap,
                    ) {
                        return Err(self.err_type(
                            ("Proxy ownKeys trap result contains duplicate entries".to_string())
                                .into(),
                        ));
                    }
                }
            }
        }
        let target_value = proxy.target(&self.gc_heap);
        let extensible_target = self.is_extensible_value(stack, context, &target_value)?;
        let target_keys = self.own_property_keys_value(stack, context, &target_value)?;
        let mut target_configurable: Vec<Value> = Vec::new();
        let mut target_nonconfigurable: Vec<Value> = Vec::new();
        for key in &target_keys {
            let vm_key = property_key_from_value(key, &self.gc_heap)?;
            let desc = self.ordinary_get_own_property_descriptor_value(
                stack,
                context,
                target_value,
                &vm_key,
                0,
            )?;
            match desc {
                Some(d) if !d.configurable() => target_nonconfigurable.push(*key),
                _ => target_configurable.push(*key),
            }
        }
        if extensible_target && target_nonconfigurable.is_empty() {
            return Ok(trap_result);
        }
        // Steps 17–21 — consume trap keys against the target key
        // sets through a hash index (strings) plus a short linear
        // walk (symbols), keeping the whole validation linear.
        let mut str_index: std::collections::HashMap<&str, usize> =
            std::collections::HashMap::with_capacity(trap_result.len());
        for (i, snap) in trap_strs.iter().enumerate() {
            if let Some(name) = snap {
                str_index.insert(name.as_str(), i);
            }
        }
        let mut consumed: Vec<bool> = vec![false; trap_result.len()];
        let mut remaining = trap_result.len();
        let consume = |key: &Value,
                       consumed: &mut Vec<bool>,
                       remaining: &mut usize,
                       heap: &otter_gc::GcHeap|
         -> bool {
            if let Some(name) = key.as_string(heap).map(|s| s.to_lossy_string(heap)) {
                if let Some(&i) = str_index.get(name.as_str())
                    && !consumed[i]
                {
                    consumed[i] = true;
                    *remaining -= 1;
                    return true;
                }
                return false;
            }
            for (i, v) in trap_result.iter().enumerate() {
                if !consumed[i] && same_property_key(v, key, heap) {
                    consumed[i] = true;
                    *remaining -= 1;
                    return true;
                }
            }
            false
        };
        for key in &target_nonconfigurable {
            if !consume(key, &mut consumed, &mut remaining, &self.gc_heap) {
                return Err(self.err_type(
                    ("Proxy ownKeys trap result omits a non-configurable target own key"
                        .to_string())
                    .into(),
                ));
            }
        }
        if extensible_target {
            return Ok(trap_result);
        }
        for key in &target_configurable {
            if !consume(key, &mut consumed, &mut remaining, &self.gc_heap) {
                return Err(self.err_type((
                        "Proxy ownKeys trap result omits a target own key while target is non-extensible"
                            .to_string()).into()));
            }
        }
        if remaining != 0 {
            return Err(self.err_type(
                ("Proxy ownKeys trap result includes extra keys while target is non-extensible"
                    .to_string())
                .into(),
            ));
        }
        Ok(trap_result)
    }

    /// §10.5.2 / §10.1.2 — value-level `[[SetPrototypeOf]]`.
    /// Proxies dispatch through `setPrototypeOf` trap and enforce the
    /// §10.5.7 invariant for non-extensible targets.
    pub(crate) fn set_prototype_value_proxy_aware(
        &mut self,
        stack: &mut ActivationStack,
        context: &ExecutionContext,
        target: &Value,
        proto: &Value,
    ) -> Result<bool, VmError> {
        // Deferred namespaces have an immutable null [[Prototype]]
        // (§28.3 [[SetPrototypeOf]] = SetImmutablePrototype): succeed
        // only when the requested prototype is also null.
        if let Some(obj) = target.as_object()
            && (object::deferred_namespace_target(obj, &self.gc_heap).is_some()
                || object::module_namespace_env(obj, &self.gc_heap).is_some())
        {
            return Ok(proto.is_null());
        }
        if let Some(proxy) = target.as_proxy() {
            if proxy.is_revoked(&self.gc_heap) {
                return Err(self.err_type(
                    ("Cannot perform 'setPrototypeOf' on a proxy that has been revoked"
                        .to_string())
                    .into(),
                ));
            }
            let trap_args: SmallVec<[Value; 8]> =
                smallvec::smallvec![proxy.target(&self.gc_heap), *proto];
            return match self.invoke_proxy_trap(
                stack,
                context,
                &proxy,
                "setPrototypeOf",
                trap_args,
            )? {
                crate::object_internal_ops::ProxyTrap::Trapped(result) => {
                    let ok = result.to_boolean(&self.gc_heap);
                    if !ok {
                        return Ok(false);
                    }
                    let target_value = proxy.target(&self.gc_heap);
                    let target_extensible =
                        self.is_extensible_value(stack, context, &target_value)?;
                    if !target_extensible {
                        let target_proto =
                            self.ordinary_get_prototype_value(stack, context, target_value, 0)?;
                        if !abstract_ops::same_value(proto, &target_proto, &self.gc_heap) {
                            return Err(self.err_type((
                                    "Proxy setPrototypeOf invariant violated: target is non-extensible and prototypes differ"
                                        .to_string()).into()));
                        }
                    }
                    Ok(true)
                }
                crate::object_internal_ops::ProxyTrap::NoTrap {
                    target: fallthrough_target,
                } => {
                    self.set_prototype_value_proxy_aware(stack, context, &fallthrough_target, proto)
                }
            };
        }
        // Class constructor [[SetPrototypeOf]] — record the identity
        // in the ctor_proto slot and mirror the walk-able chain on
        // the statics object (a class parent maps to its statics so
        // inherited statics keep resolving).
        if let Some(c) = target.as_class_constructor() {
            c.set_ctor_proto(&mut self.gc_heap, *proto);
            let statics_chain = if let Some(pc) = proto.as_class_constructor() {
                Value::object(pc.statics(&self.gc_heap))
            } else {
                *proto
            };
            let statics = Value::object(c.statics(&self.gc_heap));
            return self.set_prototype_value_proxy_aware(stack, context, &statics, &statics_chain);
        }
        if let Some(obj) = target.as_object() {
            // §10.1.2 OrdinarySetPrototypeOf full algorithm.
            let current_proto =
                object::prototype_value(obj, &self.gc_heap).unwrap_or(Value::null());
            // §20.1.3 — %Object.prototype% is an
            // immutable-prototype exotic object. It reports
            // success only when the requested prototype is
            // SameValue with its current [[Prototype]].
            if self.object_prototype_object_opt() == Some(obj) {
                return Ok(abstract_ops::same_value(
                    proto,
                    &current_proto,
                    &self.gc_heap,
                ));
            }
            if abstract_ops::same_value(proto, &current_proto, &self.gc_heap) {
                return Ok(true);
            }
            if !object::is_extensible(obj, &self.gc_heap) {
                return Ok(false);
            }
            // Step 8 cycle check — walk the candidate chain looking
            // for O itself. Only ordinary-object hops; the spec
            // stops when an exotic [[GetPrototypeOf]] is hit.
            let mut p = *proto;
            let hard_cap = object::PROTO_CHAIN_HARD_CAP;
            let mut hops = 0;
            loop {
                if p.is_null() {
                    break;
                }
                if let Some(candidate) = p.as_object() {
                    if abstract_ops::same_value(
                        &Value::object(candidate),
                        &Value::object(obj),
                        &self.gc_heap,
                    ) {
                        return Ok(false);
                    }
                    if hops >= hard_cap {
                        break;
                    }
                    hops += 1;
                    p = object::prototype_value(candidate, &self.gc_heap).unwrap_or(Value::null());
                } else {
                    // Non-ordinary prototype links short-circuit per
                    // §10.1.2 step 8.c.i.
                    break;
                }
            }
            let proto_opt = if proto.is_null() { None } else { Some(*proto) };
            let changed = object::set_prototype_value(obj, &mut self.gc_heap, proto_opt);
            if changed {
                self.bump_ordinary_object_prototype_shape_epoch();
            }
            return Ok(changed);
        }
        if let Some(arr) = target.as_array() {
            let current_proto = self.get_prototype_for_op(target)?;
            if abstract_ops::same_value(proto, &current_proto, &self.gc_heap) {
                return Ok(true);
            }
            if !array::is_extensible(arr, &self.gc_heap) {
                return Ok(false);
            }
            if abstract_ops::same_value(proto, target, &self.gc_heap) {
                return Ok(false);
            }
            let proto_opt = if proto.is_null() {
                None
            } else if proto.is_object_type() || proto.is_proxy() {
                Some(*proto)
            } else {
                return Ok(false);
            };
            array::set_prototype_override(arr, &mut self.gc_heap, proto_opt);
            return Ok(true);
        }
        // §10.1.2 OrdinarySetPrototypeOf for interned functions and
        // closures — the override rides the closure instance's body, or
        // the interned-template side table for a bare function value
        // (a stored `null` means an explicit null [[Prototype]],
        // distinct from "no override").
        let owner = target.as_closure(&self.gc_heap);
        let fid = target
            .as_function()
            .or_else(|| owner.map(|c| c.cached_function_id));
        if let Some(function_id) = fid {
            let current = self.get_prototype_for_op(target)?;
            if abstract_ops::same_value(proto, &current, &self.gc_heap) {
                return Ok(true);
            }
            if !self.ordinary_function_is_extensible(owner, function_id) {
                return Ok(false);
            }
            if abstract_ops::same_value(proto, target, &self.gc_heap) {
                return Ok(false);
            }
            self.set_function_prototype_override(target, Some(*proto));
            return Ok(true);
        }
        // §10.4.1 bound function exotic objects use OrdinarySetPrototypeOf;
        // the override rides a body slot every prototype walk consults.
        if let Some(bound) = target.as_bound_function() {
            let current = self.get_prototype_for_op(target)?;
            if abstract_ops::same_value(proto, &current, &self.gc_heap) {
                return Ok(true);
            }
            if abstract_ops::same_value(proto, target, &self.gc_heap) {
                return Ok(false);
            }
            bound.set_prototype_override(&mut self.gc_heap, *proto);
            return Ok(true);
        }
        // §10.1.2 OrdinarySetPrototypeOf for the remaining exotics —
        // TypedArray / ArrayBuffer / DataView / Map / Set / WeakMap /
        // WeakSet / RegExp / Promise / generator / iterator / Intl.
        // Their [[Prototype]] rides a body slot rather than an
        // `ObjectBody`, but the algorithm is the ordinary one: the
        // no-op case succeeds, a non-extensible object refuses, and a
        // chain that would run back into the target refuses.
        let current = self.get_prototype_for_op(target)?;
        if abstract_ops::same_value(proto, &current, &self.gc_heap) {
            return Ok(true);
        }
        if !self.is_extensible_value(stack, context, target)? {
            return Ok(false);
        }
        let mut p = *proto;
        let mut hops = 0;
        loop {
            if p.is_null() {
                break;
            }
            if abstract_ops::same_value(&p, target, &self.gc_heap) {
                return Ok(false);
            }
            let Some(candidate) = p.as_object() else {
                // Non-ordinary prototype links short-circuit per
                // §10.1.2 step 8.c.i.
                break;
            };
            if hops >= object::PROTO_CHAIN_HARD_CAP {
                break;
            }
            hops += 1;
            p = object::prototype_value(candidate, &self.gc_heap).unwrap_or(Value::null());
        }
        if self.set_exotic_prototype_override(target, *proto) {
            return Ok(true);
        }
        Ok(true)
    }
}
