//! Spec-faithful `JSON.stringify` (§25.5.2) driven by a live
//! interpreter so user-observable hooks fire.
//!
//! The heap-only walker in [`super::stringify`] stays the fast path
//! for [`super::call`] callers that have no execution context. This
//! module implements the full abstract algorithm — `toJSON`
//! invocation, `ReplacerFunction` / `PropertyList`, wrapper-object
//! unwrapping, accessor-aware `[[Get]]`, and `IsArray` over proxy
//! chains — for the native `JSON.stringify` entry point.
//!
//! # Contents
//! - [`Interpreter::json_stringify_spec`] — §25.5.2.1 entry.
//! - `serialize_json_property` — §25.5.2.2 SerializeJSONProperty.
//! - `serialize_json_object` / `serialize_json_array` — §25.5.2.4/.5.
//!
//! # Invariants
//! - **Cycle / depth guard.** `state.stack` holds the identity
//!   pointer of every object/array currently being serialised.
//!   Revisiting one raises a `TypeError`; exceeding
//!   [`MAX_NESTING_DEPTH`] does too, which also bounds host-stack
//!   recursion.
//! - **Diagnostic parity.** Cyclic and BigInt failures reuse the
//!   exact messages the heap-only path emits so existing runtime
//!   assertions keep matching.
//!
//! # See also
//! - <https://tc39.es/ecma262/#sec-json.stringify>

use smallvec::{SmallVec, smallvec};

use super::MAX_NESTING_DEPTH;
use crate::number::NumberValue;
use crate::object;
use crate::string::JsString;
use crate::{ExecutionContext, Interpreter, Value, VmError};

const CYCLIC_MESSAGE: &str = "JSON.stringify cannot serialize cyclic structures.";
const BIGINT_MESSAGE: &str = "JSON.stringify cannot serialize BigInt values.";

/// §25.5.2.2 step 4 wrapper classification.
enum WrapperKind {
    None,
    Number,
    String,
    Boolean(bool),
    BigInt(crate::bigint::BigIntValue),
}

/// Mutable serialisation state threaded through the recursion.
#[derive(Default)]
struct JsonState {
    /// Identity pointers of in-progress containers (cycle guard).
    stack: Vec<*const ()>,
    /// `gap` — the indent unit (§25.5.2.1 steps 6–9).
    gap: String,
    /// Current cumulative indentation.
    indent: String,
    /// `PropertyList` from an array replacer (§25.5.2.1 step 5).
    property_list: Option<Vec<String>>,
    /// Scratch-root index of the `ReplacerFunction` (when the replacer
    /// is callable). Held on the interpreter's root stack rather than
    /// inline so a scavenge during traversal can't strand it.
    replacer_root: Option<usize>,
}

impl Interpreter {
    /// §25.5.2.1 `JSON.stringify(value, replacer, space)`.
    ///
    /// Returns `Value::undefined()` when the root serialises to
    /// nothing (e.g. a function or `undefined`).
    pub(crate) fn json_stringify_spec(
        &mut self,
        context: &ExecutionContext,
        args: &[Value],
    ) -> Result<Value, VmError> {
        let value = args.first().copied().unwrap_or_else(Value::undefined);
        let replacer = args.get(1).copied().unwrap_or_else(Value::undefined);
        let space = args.get(2).copied().unwrap_or_else(Value::undefined);

        let mut state = JsonState::default();

        // Park the input value across replacer classification and gap
        // derivation — both can allocate (and a property-list replacer
        // or a `space` wrapper's `valueOf` can scavenge). `value_root`
        // is the bottom of this call's scratch-root region.
        let value_root = self.json_root_push(value);

        // §25.5.2.1 steps 4–5 — classify the replacer argument.
        if replacer.is_object_type() {
            if replacer.is_callable() {
                state.replacer_root = Some(self.json_root_push(replacer));
            } else if self.json_is_array(&replacer)? {
                state.property_list = Some(self.json_build_property_list(context, &replacer)?);
            }
        }

        // §25.5.2.1 steps 6–9 — derive `gap` from `space`.
        state.gap = self.json_gap(context, &space)?;

        // §25.5.2.1 step 10 — wrapper = { "": value }.
        let rooted_value = self.json_root_get(value_root);
        let wrapper = self.json_make_wrapper(rooted_value)?;

        let mut buffer = String::new();
        let wrote = self.serialize_json_property_into(
            context,
            &mut state,
            "",
            Value::object(wrapper),
            &mut buffer,
        )?;
        // Release this call's scratch roots (value + any replacer).
        self.json_root_pop_to(value_root);

        if wrote {
            let s = JsString::from_str(&buffer, self.gc_heap_mut()).map_err(|_| {
                VmError::TypeError {
                    message: ("out of memory".to_string()).into(),
                }
            })?;
            Ok(Value::string(s))
        } else {
            Ok(Value::undefined())
        }
    }

    /// §25.5.1 steps 7–9 — apply a callable `reviver` to a freshly
    /// parsed value. Builds the `{ "": unfiltered }` root holder and
    /// runs InternalizeJSONProperty from there. `source` is the
    /// `context.source` span tree for the parsed text (the
    /// `json-parse-with-source` proposal); `None` disables it.
    pub(crate) fn json_internalize_root(
        &mut self,
        context: &ExecutionContext,
        unfiltered: Value,
        reviver: Value,
        source: Option<&crate::json::parse::SourceNode>,
    ) -> Result<Value, VmError> {
        let root = self.json_make_wrapper(unfiltered)?;
        self.internalize_json_property(context, Value::object(root), "", &reviver, source)
    }

    /// §25.5.1.1 InternalizeJSONProperty(holder, name, reviver).
    fn internalize_json_property(
        &mut self,
        context: &ExecutionContext,
        holder: Value,
        name: &str,
        reviver: &Value,
        source: Option<&crate::json::parse::SourceNode>,
    ) -> Result<Value, VmError> {
        let value = self.get_property_value_for_call(context, holder, name)?;

        if value.is_object_type() {
            if self.json_is_array(&value)? {
                let len = self.json_length(context, &value)?;
                for index in 0..len {
                    let key = index.to_string();
                    let child = source.and_then(|s| s.array_child(index));
                    self.internalize_one(context, value, &key, reviver, child)?;
                }
            } else {
                // EnumerableOwnPropertyNames is snapshotted up front so
                // a reviver mutating later siblings cannot perturb the
                // key set already chosen (§25.5.1.1 step 2.c.i).
                let keys = self.json_enumerable_string_keys(context, &value)?;
                for key in keys {
                    let child = source.and_then(|s| s.object_child(&key));
                    self.internalize_one(context, value, &key, reviver, child)?;
                }
            }
        }

        // §25.5.1.1 — the reviver receives a `context` object whose
        // own `source` property is present only for primitive leaves.
        let name_arg = self.json_key_value(name)?;
        let context_obj = self.json_make_reviver_context(value, source)?;
        let args: SmallVec<[Value; 8]> = smallvec![name_arg, value, context_obj];
        self.run_callable_sync(context, reviver, holder, args)
    }

    /// Build the reviver `context` object: a plain `%Object.prototype%`
    /// object carrying an own `source` data property when `value` is a
    /// primitive leaf with recorded source text.
    fn json_make_reviver_context(
        &mut self,
        value: Value,
        source: Option<&crate::json::parse::SourceNode>,
    ) -> Result<Value, VmError> {
        let obj = self.json_make_plain_object()?;
        if !value.is_object_type()
            && let Some(src) = source.and_then(|s| s.source())
        {
            // The `source` text applies only while the leaf still holds
            // its originally parsed value; a reviver that forward-
            // replaces a slot makes the new value source-less. Compare
            // against the re-parsed token via SameValue.
            let still_original = crate::json::parse::parse(src, self.gc_heap_mut())
                .ok()
                .is_some_and(|parsed| {
                    crate::abstract_ops::same_value(&value, &parsed, self.gc_heap())
                });
            if still_original {
                let js = JsString::from_str(src, self.gc_heap_mut()).map_err(|_| {
                    VmError::TypeError {
                        message: ("out of memory".to_string()).into(),
                    }
                })?;
                object::set(obj, self.gc_heap_mut(), "source", Value::string(js));
            }
        }
        Ok(Value::object(obj))
    }

    /// Allocate an empty `%Object.prototype%`-backed object.
    fn json_make_plain_object(&mut self) -> Result<object::JsObject, VmError> {
        let obj = object::alloc_object_with_roots(
            self.gc_heap_mut(),
            &mut |_: &mut dyn FnMut(*mut otter_gc::raw::RawGc)| {},
        )
        .map_err(|_| VmError::TypeError {
            message: ("out of memory".to_string()).into(),
        })?;
        let object_proto = self.object_prototype_object_opt();
        object::set_prototype_value(obj, self.gc_heap_mut(), object_proto.map(Value::object));
        Ok(obj)
    }

    /// One key of InternalizeJSONProperty: recurse, then Delete on
    /// `undefined` or CreateDataProperty otherwise (§25.5.1.1 step
    /// 2.b/2.c).
    fn internalize_one(
        &mut self,
        context: &ExecutionContext,
        holder: Value,
        key: &str,
        reviver: &Value,
        source: Option<&crate::json::parse::SourceNode>,
    ) -> Result<(), VmError> {
        let new_element = self.internalize_json_property(context, holder, key, reviver, source)?;
        let property_key = crate::VmPropertyKey::OwnedString(key.to_string());
        if new_element.is_undefined() {
            self.ordinary_delete_value(context, holder, &property_key, 0)?;
        } else {
            let descriptor = object::PartialPropertyDescriptor {
                value: Some(new_element),
                writable: Some(true),
                enumerable: Some(true),
                configurable: Some(true),
                ..object::PartialPropertyDescriptor::default()
            };
            self.define_own_property_value(context, &holder, &property_key, descriptor)?;
        }
        Ok(())
    }

    /// §25.5.2.2 SerializeJSONProperty(key, holder).
    /// SerializeJSONProperty, appending the rendered text directly into
    /// `out` and returning whether anything was written (`false` =
    /// undefined / function / symbol, i.e. the property is omitted). The
    /// single shared `out` buffer replaces the previous per-node `String`
    /// allocation + parent-level `join`, which dominated `JSON.stringify`.
    fn serialize_json_property_into(
        &mut self,
        context: &ExecutionContext,
        state: &mut JsonState,
        key: &str,
        holder: Value,
        out: &mut String,
    ) -> Result<bool, VmError> {
        // step 1 — value = ? Get(holder, key).
        // `holder` and the evolving `value` are parked on the scratch
        // root stack so the JS-string allocations below (key arguments,
        // `[[OwnPropertyKeys]]` names) — or a user getter / `toJSON` /
        // replacer — can trigger a scavenge without leaving us
        // dereferencing a moved object. Each step re-reads from the slot.
        let holder_root = self.json_root_push(holder);
        let value0 =
            self.get_property_value_for_call(context, self.json_root_get(holder_root), key)?;
        let value_root = self.json_root_push(value0);

        let rendered = self.serialize_json_property_rooted_into(
            context,
            state,
            key,
            holder_root,
            value_root,
            out,
        );
        // Restore the strict-stack invariant regardless of outcome.
        self.json_root_pop_to(holder_root);
        rendered
    }

    /// Fast-path SerializeJSONProperty when the step-1 value is already in
    /// hand (read directly from the holder's data slot under a validated
    /// shape), skipping the `Get(holder, key)`. `holder_root` is the holder's
    /// scratch-root index (used only if a replacer is active — never on this
    /// path, but threaded for parity). Behaviour is otherwise identical to
    /// [`Self::serialize_json_property_into`].
    fn serialize_json_known_value_into(
        &mut self,
        context: &ExecutionContext,
        state: &mut JsonState,
        key: &str,
        holder_root: usize,
        value: Value,
        out: &mut String,
    ) -> Result<bool, VmError> {
        let value_root = self.json_root_push(value);
        let rendered = self.serialize_json_property_rooted_into(
            context,
            state,
            key,
            holder_root,
            value_root,
            out,
        );
        self.json_root_pop_to(value_root);
        rendered
    }

    /// Body of SerializeJSONProperty once `holder` (`holder_root`) and
    /// the step-1 value (`value_root`) are parked on the scratch root
    /// stack. Reads both back through their slots after every call that
    /// can allocate, so a mid-serialization scavenge never strands a
    /// stale copy.
    fn serialize_json_property_rooted_into(
        &mut self,
        context: &ExecutionContext,
        state: &mut JsonState,
        key: &str,
        holder_root: usize,
        value_root: usize,
        out: &mut String,
    ) -> Result<bool, VmError> {
        // step 2 — invoke `toJSON` when present and callable.
        let value = self.json_root_get(value_root);
        if value.is_object_type() || value.is_big_int() {
            let to_json = self.get_property_value_for_call(context, value, "toJSON")?;
            if to_json.is_callable() {
                let key_arg = self.json_key_value(key)?;
                let args: SmallVec<[Value; 8]> = smallvec![key_arg];
                let receiver = self.json_root_get(value_root);
                let next = self.run_callable_sync(context, &to_json, receiver, args)?;
                self.json_root_set(value_root, next);
            }
        }

        // step 3 — apply the replacer function.
        if let Some(replacer_root) = state.replacer_root {
            let key_arg = self.json_key_value(key)?;
            let args: SmallVec<[Value; 8]> = smallvec![key_arg, self.json_root_get(value_root)];
            let holder = self.json_root_get(holder_root);
            let replacer = self.json_root_get(replacer_root);
            let next = self.run_callable_sync(context, &replacer, holder, args)?;
            self.json_root_set(value_root, next);
        }

        let mut value = self.json_root_get(value_root);

        // step 4 — unwrap Number / String / Boolean / BigInt wrappers.
        // [[NumberData]] / [[StringData]] coerce through ToNumber /
        // ToString so a user `valueOf` / `toString` / `@@toPrimitive`
        // fires; [[BooleanData]] / [[BigIntData]] take the raw slot.
        if let Some(obj) = value.as_object() {
            let kind = {
                let heap = self.gc_heap();
                if object::number_data(obj, heap).is_some() {
                    WrapperKind::Number
                } else if object::string_data(obj, heap).is_some() {
                    WrapperKind::String
                } else if let Some(b) = object::boolean_data(obj, heap) {
                    WrapperKind::Boolean(b)
                } else {
                    object::bigint_data(obj, heap).map_or(WrapperKind::None, WrapperKind::BigInt)
                }
            };
            match kind {
                WrapperKind::Number => {
                    value = Value::number(self.coerce_to_number(context, &value)?)
                }
                WrapperKind::String => {
                    let s = self.coerce_to_string(context, &value)?;
                    let js = JsString::from_str(&s, self.gc_heap_mut()).map_err(|_| {
                        VmError::TypeError {
                            message: ("out of memory".to_string()).into(),
                        }
                    })?;
                    value = Value::string(js);
                }
                WrapperKind::Boolean(b) => value = Value::boolean(b),
                WrapperKind::BigInt(bi) => value = Value::big_int(bi),
                WrapperKind::None => {}
            }
        }

        // §25.5.3 — an object carrying [[IsRawJSON]] serialises to its
        // own `"rawJSON"` text verbatim (no quoting, no recursion).
        if let Some(obj) = value.as_object()
            && object::is_raw_json(obj, self.gc_heap())
        {
            let raw = self.get_property_value_for_call(context, value, "rawJSON")?;
            if let Some(s) = raw.as_string(self.gc_heap()) {
                out.push_str(&s.to_lossy_string(self.gc_heap()));
                return Ok(true);
            }
        }

        // steps 5–12 — render by type.
        if value.is_null() {
            out.push_str("null");
            return Ok(true);
        }
        if let Some(b) = value.as_boolean() {
            out.push_str(if b { "true" } else { "false" });
            return Ok(true);
        }
        if let Some(s) = value.as_string(self.gc_heap()) {
            quote_json_string_into(s, self.gc_heap(), out);
            return Ok(true);
        }
        if let Some(n) = value.as_number() {
            out.push_str(&render_number(n));
            return Ok(true);
        }
        if value.is_big_int() {
            return Err(VmError::TypeError {
                message: (BIGINT_MESSAGE.to_string()).into(),
            });
        }
        // step 11 — Object that is not callable.
        if value.is_object_type() && !value.is_callable() {
            if self.json_is_array(&value)? {
                self.serialize_json_array_into(context, state, value, out)?;
            } else {
                self.serialize_json_object_into(context, state, value, out)?;
            }
            return Ok(true);
        }
        // undefined / function / symbol → omitted.
        Ok(false)
    }

    /// §25.5.2.4 SerializeJSONObject(value), appending into `out`.
    fn serialize_json_object_into(
        &mut self,
        context: &ExecutionContext,
        state: &mut JsonState,
        value: Value,
        out: &mut String,
    ) -> Result<(), VmError> {
        self.json_enter(state, &value)?;
        // Park the container: `json_enumerable_string_keys` mints a
        // `JsString` per key and each property may recurse, so a
        // scavenge can move `value` mid-loop. Re-read from the slot.
        let value_root = self.json_root_push(value);

        // Each entry is `(key, Some(slot))` for the fast path (read the value
        // straight from its flat data slot, re-validating the shape per key) or
        // `(key, None)` for the observable `[[Get]]` path. The fast path applies
        // only to an ordinary object with a replacer-free key list and no
        // enumerable accessors; everything else (replacer property list, proxy /
        // typed array / module namespace / String wrapper, any enumerable
        // getter) keeps the spec `[[Get]]` per key.
        let (entries, fast_shape): (Vec<(String, Option<u16>)>, Option<object::ShapeId>) =
            match &state.property_list {
                Some(list) => (list.iter().map(|k| (k.clone(), None)).collect(), None),
                None => {
                    let v = self.json_root_get(value_root);
                    let heap = self.gc_heap();
                    let fast = v.as_object().filter(|obj| {
                        v.as_proxy().is_none()
                            && v.as_typed_array(heap).is_none()
                            && object::module_namespace_env(*obj, heap).is_none()
                            && object::string_data(*obj, heap).is_none()
                    });
                    match fast.and_then(|obj| {
                        object::with_properties(obj, heap, |p| p.enumerable_string_data_offsets())
                            .map(|offs| (obj, offs))
                    }) {
                        Some((obj, offs)) => {
                            let sid = object::shape_id(obj, heap);
                            (
                                offs.into_iter().map(|(k, o)| (k, Some(o))).collect(),
                                Some(sid),
                            )
                        }
                        None => {
                            let keys = self.json_enumerable_string_keys(context, &v)?;
                            (keys.into_iter().map(|k| (k, None)).collect(), None)
                        }
                    }
                }
            };

        let stepback = state.indent.clone();
        state.indent.push_str(&state.gap);

        out.push('{');
        let mut any = false;
        for (key, fast_slot) in &entries {
            let holder = self.json_root_get(value_root);
            // Tentatively write this member's separator + key prefix, then
            // its value; if the property is omitted, rewind `out` to undo
            // the prefix so no stray comma/key survives.
            let mark = out.len();
            if any {
                out.push(',');
            }
            if !state.gap.is_empty() {
                out.push('\n');
                out.push_str(&state.indent);
            }
            quote_json_string_str_into(key, out);
            out.push(':');
            if !state.gap.is_empty() {
                out.push(' ');
            }
            // Fast path: read the value directly from its data slot, but only
            // while the holder's live shape still matches the one enumerated
            // above (a nested `toJSON` could have mutated the holder). On any
            // mismatch fall back to the observable `[[Get]]`, so behaviour is
            // identical to the spec path.
            let fast_value = match fast_slot {
                Some(slot) => holder
                    .as_object()
                    .filter(|o| Some(object::shape_id(*o, self.gc_heap())) == fast_shape)
                    .map(|o| object::data_value_at(o, self.gc_heap(), *slot)),
                None => None,
            };
            let rendered = match fast_value {
                Some(value) => self
                    .serialize_json_known_value_into(context, state, key, value_root, value, out)?,
                None => self.serialize_json_property_into(context, state, key, holder, out)?,
            };
            if rendered {
                any = true;
            } else {
                out.truncate(mark);
            }
        }
        if any && !state.gap.is_empty() {
            out.push('\n');
            out.push_str(&stepback);
        }
        out.push('}');

        state.indent = stepback;
        self.json_root_pop_to(value_root);
        self.json_leave(state);
        Ok(())
    }

    /// §25.5.2.5 SerializeJSONArray(value), appending into `out`.
    fn serialize_json_array_into(
        &mut self,
        context: &ExecutionContext,
        state: &mut JsonState,
        value: Value,
        out: &mut String,
    ) -> Result<(), VmError> {
        self.json_enter(state, &value)?;
        // Park the container so per-index recursion (which allocates
        // index-key strings and may scavenge) can't strand a stale copy.
        let value_root = self.json_root_push(value);

        let len = {
            let v = self.json_root_get(value_root);
            self.json_length(context, &v)?
        };
        let stepback = state.indent.clone();
        state.indent.push_str(&state.gap);

        out.push('[');
        for index in 0..len {
            if index > 0 {
                out.push(',');
            }
            if !state.gap.is_empty() {
                out.push('\n');
                out.push_str(&state.indent);
            }
            let key = index.to_string();
            let holder = self.json_root_get(value_root);
            // §25.5.2.5 — an omitted element serialises as `null`.
            if !self.serialize_json_property_into(context, state, &key, holder, out)? {
                out.push_str("null");
            }
        }
        if len > 0 && !state.gap.is_empty() {
            out.push('\n');
            out.push_str(&stepback);
        }
        out.push(']');

        state.indent = stepback;
        self.json_root_pop_to(value_root);
        self.json_leave(state);
        Ok(())
    }

    /// Push `value`'s identity onto the cycle stack, rejecting
    /// revisits (§25.5.2.4/.5 step 1) and over-deep nesting.
    fn json_enter(&self, state: &mut JsonState, value: &Value) -> Result<(), VmError> {
        if state.stack.len() >= MAX_NESTING_DEPTH {
            return Err(VmError::TypeError {
                message: (format!("JSON nesting exceeded {MAX_NESTING_DEPTH} levels.")).into(),
            });
        }
        let id = self.json_identity(value);
        if !id.is_null() && state.stack.contains(&id) {
            return Err(VmError::TypeError {
                message: (CYCLIC_MESSAGE.to_string()).into(),
            });
        }
        state.stack.push(id);
        Ok(())
    }

    fn json_leave(&self, state: &mut JsonState) {
        state.stack.pop();
    }

    /// Identity pointer for cycle detection across object/array/proxy.
    fn json_identity(&self, value: &Value) -> *const () {
        if let Some(obj) = value.as_object() {
            obj.as_header_ptr() as *const ()
        } else if let Some(arr) = value.as_array() {
            crate::array::identity_addr(arr)
        } else if let Some(proxy) = value.as_proxy_gc() {
            proxy.as_header_ptr() as *const ()
        } else {
            core::ptr::null()
        }
    }

    /// §7.2.2 IsArray, resolving proxy `[[ProxyTarget]]` chains. A
    /// revoked proxy raises a `TypeError`.
    fn json_is_array(&self, value: &Value) -> Result<bool, VmError> {
        let mut current = *value;
        let mut hops = 0usize;
        loop {
            if hops >= object::PROTO_CHAIN_HARD_CAP {
                return Ok(false);
            }
            hops += 1;
            if let Some(proxy) = current.as_proxy() {
                if proxy.is_revoked(self.gc_heap()) {
                    return Err(VmError::TypeError {
                        message: ("Cannot perform IsArray on a proxy that has been revoked"
                            .to_string())
                        .into(),
                    });
                }
                current = proxy.target(self.gc_heap());
                continue;
            }
            return Ok(current.is_array());
        }
    }

    /// §7.3.18 LengthOfArrayLike(value) for the array branch.
    fn json_length(&mut self, context: &ExecutionContext, value: &Value) -> Result<usize, VmError> {
        if let Some(arr) = value.as_array() {
            return Ok(crate::array::len(arr, self.gc_heap()));
        }
        let len_val = self.get_property_value_for_call(context, *value, "length")?;
        let len_val = if len_val.is_object_type() {
            self.evaluate_to_primitive(
                context,
                &len_val,
                crate::abstract_ops::ToPrimitiveHint::Number,
            )?
        } else {
            len_val
        };
        crate::to_length(&len_val, self.gc_heap())
    }

    /// EnumerableOwnPropertyNames(value, key) restricted to string
    /// keys (§25.5.2.4 step 5). Symbol keys are skipped; non-
    /// enumerable keys are filtered out.
    fn json_enumerable_string_keys(
        &mut self,
        context: &ExecutionContext,
        value: &Value,
    ) -> Result<Vec<String>, VmError> {
        // Fast path — an ordinary object's enumerable own string keys
        // are read straight from its property table as Rust strings, in
        // ordinary own-key order. This skips both the per-key `JsString`
        // allocation and the per-key `[[GetOwnProperty]]` lookup of the
        // spec path; crucially it allocates nothing, so it can't trigger
        // a scavenge that would strand the container mid-enumeration.
        // Exotics with a custom `[[OwnPropertyKeys]]` / key order
        // (proxy, typed array, module namespace, String wrapper) keep
        // the spec path below.
        if let Some(obj) = value.as_object()
            && value.as_proxy().is_none()
            && value.as_typed_array(self.gc_heap()).is_none()
            && object::module_namespace_env(obj, self.gc_heap()).is_none()
            && object::string_data(obj, self.gc_heap()).is_none()
        {
            let keys = object::with_properties(obj, self.gc_heap(), |p| {
                p.enumerable_keys()
                    .map(str::to_string)
                    .collect::<Vec<String>>()
            });
            return Ok(keys);
        }

        // Spec path for exotic objects. `own_property_keys_value` mints a
        // `JsString` per key (which can scavenge), so the container is
        // parked and re-read from its root slot before each descriptor
        // lookup instead of dereferencing a possibly-moved copy.
        let value_root = self.json_root_push(*value);
        let keys = {
            let v = self.json_root_get(value_root);
            self.own_property_keys_value(context, &v)?
        };
        let mut out = Vec::with_capacity(keys.len());
        for key in keys {
            if key.is_symbol() {
                continue;
            }
            let holder = self.json_root_get(value_root);
            let desc = self.get_own_property_descriptor_for_value(context, holder, Some(&key))?;
            if desc.is_some_and(|d| d.enumerable()) {
                let s = key
                    .as_string(self.gc_heap())
                    .map(|js| js.to_lossy_string(self.gc_heap()))
                    .unwrap_or_else(|| key.display_string(self.gc_heap()));
                out.push(s);
            }
        }
        self.json_root_pop_to(value_root);
        Ok(out)
    }

    /// Build the deduplicated `PropertyList` from an array replacer
    /// (§25.5.2.1 step 5.b.iii). Each item contributes a key when it
    /// is a String / Number (or their wrapper objects).
    fn json_build_property_list(
        &mut self,
        context: &ExecutionContext,
        replacer: &Value,
    ) -> Result<Vec<String>, VmError> {
        let len = self.json_length(context, replacer)?;
        let mut list: Vec<String> = Vec::new();
        for index in 0..len {
            let key = index.to_string();
            let item = self.get_property_value_for_call(context, *replacer, &key)?;
            if let Some(entry) = self.json_property_list_entry(context, &item)?
                && !list.contains(&entry)
            {
                list.push(entry);
            }
        }
        Ok(list)
    }

    /// Coerce one array-replacer item to its key string per §25.5.2.1
    /// step 5.b.iii, or `None` when the item is neither a String /
    /// Number nor a wrapper of one. String values pass through
    /// verbatim; Number values and String / Number wrappers run
    /// `ToString` (so a wrapper `toString` is observable).
    fn json_property_list_entry(
        &mut self,
        context: &ExecutionContext,
        item: &Value,
    ) -> Result<Option<String>, VmError> {
        if let Some(s) = item.as_string(self.gc_heap()) {
            return Ok(Some(s.to_lossy_string(self.gc_heap())));
        }
        if let Some(n) = item.as_number() {
            return Ok(Some(render_number_key(n)));
        }
        if let Some(obj) = item.as_object() {
            let is_wrapper = {
                let heap = self.gc_heap();
                object::number_data(obj, heap).is_some() || object::string_data(obj, heap).is_some()
            };
            if is_wrapper {
                return Ok(Some(self.coerce_to_string(context, item)?));
            }
        }
        Ok(None)
    }

    /// §25.5.2.1 steps 5–9 — translate `space` into the `gap` string.
    /// A Number / String wrapper coerces through ToNumber / ToString
    /// so a user `valueOf` / `toString` is observable.
    fn json_gap(&mut self, context: &ExecutionContext, space: &Value) -> Result<String, VmError> {
        let space = if let Some(obj) = space.as_object() {
            let heap = self.gc_heap();
            if object::number_data(obj, heap).is_some() {
                Value::number(self.coerce_to_number(context, space)?)
            } else if object::string_data(obj, heap).is_some() {
                let s = self.coerce_to_string(context, space)?;
                let js =
                    JsString::from_str(&s, self.gc_heap_mut()).map_err(|_| VmError::TypeError {
                        message: ("out of memory".to_string()).into(),
                    })?;
                Value::string(js)
            } else {
                *space
            }
        } else {
            *space
        };

        if let Some(n) = space.as_number() {
            let f = n.as_f64();
            let count = if f.is_nan() || f <= 0.0 {
                0
            } else {
                (f.trunc() as usize).min(10)
            };
            return Ok(" ".repeat(count));
        }
        if let Some(s) = space.as_string(self.gc_heap()) {
            let text = s.to_lossy_string(self.gc_heap());
            return Ok(text.chars().take(10).collect());
        }
        Ok(String::new())
    }

    /// Allocate the `{ "": value }` wrapper holder (§25.5.2.1 steps
    /// 9–10): a plain extensible object whose `[[Prototype]]` is the
    /// realm `%Object.prototype%`, carrying `""` as an own data
    /// property installed via CreateDataProperty (no `[[Set]]`).
    fn json_make_wrapper(&mut self, value: Value) -> Result<object::JsObject, VmError> {
        let mut roots = |visitor: &mut dyn FnMut(*mut otter_gc::raw::RawGc)| {
            value.trace_value_slots(visitor);
        };
        let obj =
            object::alloc_object_with_roots(self.gc_heap_mut(), &mut roots).map_err(|_| {
                VmError::TypeError {
                    message: ("out of memory".to_string()).into(),
                }
            })?;
        let object_proto = self.object_prototype_object_opt();
        object::set_prototype_value(obj, self.gc_heap_mut(), object_proto.map(Value::object));
        object::set(obj, self.gc_heap_mut(), "", value);
        Ok(obj)
    }

    /// Build the `key` argument passed to `toJSON` / the replacer.
    fn json_key_value(&mut self, key: &str) -> Result<Value, VmError> {
        let s = JsString::from_str(key, self.gc_heap_mut()).map_err(|_| VmError::TypeError {
            message: ("out of memory".to_string()).into(),
        })?;
        Ok(Value::string(s))
    }
}

/// §25.5.2.3 QuoteJSONString over UTF-16 code units so lone
/// surrogates are escaped as `\uXXXX` (well-formed `JSON.stringify`).
fn quote_json_string_into(s: JsString, heap: &otter_gc::GcHeap, out: &mut String) {
    let units = s.to_utf16_vec(heap);
    quote_units_into(&units, out);
}

/// QuoteJSONString for a Rust `&str` member name (object keys are
/// always well-formed UTF-8, so a code-unit round trip is moot).
fn quote_json_string_str_into(s: &str, out: &mut String) {
    let units: Vec<u16> = s.encode_utf16().collect();
    quote_units_into(&units, out);
}

fn quote_units_into(units: &[u16], out: &mut String) {
    use std::fmt::Write as _;
    out.reserve(units.len() + 2);
    out.push('"');
    let mut i = 0;
    while i < units.len() {
        let c = units[i];
        match c {
            0x22 => out.push_str("\\\""),
            0x5C => out.push_str("\\\\"),
            0x08 => out.push_str("\\b"),
            0x0C => out.push_str("\\f"),
            0x0A => out.push_str("\\n"),
            0x0D => out.push_str("\\r"),
            0x09 => out.push_str("\\t"),
            0x00..=0x1F => {
                let _ = write!(out, "\\u{c:04x}");
            }
            0xD800..=0xDBFF => {
                if let Some(&low) = units.get(i + 1)
                    && (0xDC00..=0xDFFF).contains(&low)
                {
                    let cp = 0x10000 + (((c as u32) - 0xD800) << 10) + ((low as u32) - 0xDC00);
                    if let Some(ch) = char::from_u32(cp) {
                        out.push(ch);
                    }
                    i += 2;
                    continue;
                }
                let _ = write!(out, "\\u{c:04x}");
            }
            0xDC00..=0xDFFF => {
                let _ = write!(out, "\\u{c:04x}");
            }
            _ => {
                if let Some(ch) = char::from_u32(c as u32) {
                    out.push(ch);
                }
            }
        }
        i += 1;
    }
    out.push('"');
}

/// ToString(Number) with the §25.5.2.2 non-finite → `null` rule.
fn render_number(n: NumberValue) -> String {
    let f = n.as_f64();
    if !f.is_finite() {
        return "null".to_string();
    }
    if f == 0.0 {
        return "0".to_string();
    }
    n.to_display_string()
}

/// ToString(Number) for a PropertyList key (no `null` substitution —
/// `Infinity`/`NaN` simply ToString as themselves there).
fn render_number_key(n: NumberValue) -> String {
    n.to_display_string()
}
