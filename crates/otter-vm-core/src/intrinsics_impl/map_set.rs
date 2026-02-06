//! Map, Set, WeakMap, and WeakSet constructor and prototype implementations
//!
//! ES2023 compliant implementation using proper SameValueZero semantics
//! via `MapKey`, insertion-ordered storage with tombstone-based deletion
//! for live iteration support.
//!
//! - Map: constructor (with iterable) + 10 prototype methods + Symbol.toStringTag
//! - Set: constructor (with iterable) + 10 prototype methods + 7 ES2025 set methods + Symbol.toStringTag
//! - WeakMap: constructor + 4 prototype methods + Symbol.toStringTag
//! - WeakSet: constructor + 3 prototype methods + Symbol.toStringTag

use crate::error::VmError;
use crate::gc::GcRef;
use crate::intrinsics_impl::helpers::MapKey;
use crate::map_data::{MapData, SetData};
use crate::memory::MemoryManager;
use crate::object::{JsObject, PropertyAttributes, PropertyDescriptor, PropertyKey};
use crate::string::JsString;
use crate::value::Value;
use std::sync::Arc;

// ============================================================================
// Internal slot keys
// ============================================================================
const MAP_DATA_KEY: &str = "__map_data__";
const SET_DATA_KEY: &str = "__set_data__";
const IS_MAP_KEY: &str = "__is_map__";
const IS_SET_KEY: &str = "__is_set__";
const IS_WEAKMAP_KEY: &str = "__is_weakmap__";
const IS_WEAKSET_KEY: &str = "__is_weakset__";
const WEAKMAP_ENTRIES_KEY: &str = "__weakmap_entries__";
const WEAKSET_ENTRIES_KEY: &str = "__weakset_entries__";

// ============================================================================
// Helpers
// ============================================================================

fn pk(s: &str) -> PropertyKey {
    PropertyKey::String(JsString::intern(s))
}

/// Create a native function Value with proper `name`, `length`, and `__non_constructor` properties.
/// Per ES2023 §10.2.8, built-in function objects have:
///   - `length`: { [[Value]]: argCount, [[Writable]]: false, [[Enumerable]]: false, [[Configurable]]: true }
///   - `name`: { [[Value]]: name, [[Writable]]: false, [[Enumerable]]: false, [[Configurable]]: true }
/// Per ES2023 §17, built-in prototype methods do not have a [[Construct]] internal method.
fn make_builtin<F>(
    name: &str,
    length: i32,
    f: F,
    mm: Arc<MemoryManager>,
    fn_proto: GcRef<JsObject>,
) -> Value
where
    F: Fn(&Value, &[Value], &mut crate::context::NativeContext<'_>) -> Result<Value, VmError>
        + Send
        + Sync
        + 'static,
{
    let val = Value::native_function_with_proto(f, mm, fn_proto);
    if let Some(obj) = val.native_function_object() {
        obj.define_property(
            PropertyKey::string("length"),
            PropertyDescriptor::function_length(Value::int32(length)),
        );
        obj.define_property(
            PropertyKey::string("name"),
            PropertyDescriptor::function_length(Value::string(JsString::intern(name))),
        );
        // Built-in prototype methods are not constructors (ES2023 §17)
        obj.set(pk("__non_constructor"), Value::boolean(true));
    }
    val
}

fn is_map(obj: &GcRef<JsObject>) -> bool {
    obj.get(&pk(IS_MAP_KEY))
        .and_then(|v| v.as_boolean())
        == Some(true)
}

fn is_set(obj: &GcRef<JsObject>) -> bool {
    obj.get(&pk(IS_SET_KEY))
        .and_then(|v| v.as_boolean())
        == Some(true)
}

fn is_weakmap(obj: &GcRef<JsObject>) -> bool {
    obj.get(&pk(IS_WEAKMAP_KEY))
        .and_then(|v| v.as_boolean())
        == Some(true)
}

fn is_weakset(obj: &GcRef<JsObject>) -> bool {
    obj.get(&pk(IS_WEAKSET_KEY))
        .and_then(|v| v.as_boolean())
        == Some(true)
}

fn get_map_data(obj: &GcRef<JsObject>) -> Option<GcRef<MapData>> {
    obj.get(&pk(MAP_DATA_KEY)).and_then(|v| v.as_map_data())
}

fn get_set_data(obj: &GcRef<JsObject>) -> Option<GcRef<SetData>> {
    obj.get(&pk(SET_DATA_KEY)).and_then(|v| v.as_set_data())
}

fn is_valid_weak_key(value: &Value) -> bool {
    value.is_object() || value.is_symbol() || value.is_function()
}

/// Get a stable pointer key for WeakMap/WeakSet (object identity).
fn weak_key_id(value: &Value) -> Option<usize> {
    if let Some(obj) = value.as_object() {
        Some(obj.as_ptr() as usize)
    } else if let Some(arr) = value.as_array() {
        Some(arr.as_ptr() as usize)
    } else if let Some(f) = value.as_function() {
        Some(f.as_ptr() as usize)
    } else if let Some(sym) = value.as_symbol() {
        // Registered symbols can be weak keys
        Some(sym.as_ptr() as usize)
    } else if let Some(p) = value.as_proxy() {
        Some(p.as_ptr() as usize)
    } else {
        value.as_promise().map(|p| p.as_ptr() as usize)
    }
}

fn init_map_slots(obj: &GcRef<JsObject>) {
    let data = GcRef::new(MapData::new());
    obj.set(pk(MAP_DATA_KEY), Value::map_data(data));
    obj.set(pk(IS_MAP_KEY), Value::boolean(true));
}

fn init_set_slots(obj: &GcRef<JsObject>) {
    let data = GcRef::new(SetData::new());
    obj.set(pk(SET_DATA_KEY), Value::set_data(data));
    obj.set(pk(IS_SET_KEY), Value::boolean(true));
}

/// Require `this` to be a Map and extract its data.
fn require_map(this_val: &Value, method: &str) -> Result<(GcRef<JsObject>, GcRef<MapData>), VmError> {
    let obj = this_val
        .as_object()
        .ok_or_else(|| VmError::type_error(format!("Method Map.prototype.{} called on incompatible receiver", method)))?;
    if !is_map(&obj) {
        return Err(VmError::type_error(format!("Method Map.prototype.{} called on incompatible receiver", method)));
    }
    let data = get_map_data(&obj)
        .ok_or_else(|| VmError::type_error("Map object missing internal data"))?;
    Ok((obj, data))
}

/// Require `this` to be a Set and extract its data.
fn require_set(this_val: &Value, method: &str) -> Result<(GcRef<JsObject>, GcRef<SetData>), VmError> {
    let obj = this_val
        .as_object()
        .ok_or_else(|| VmError::type_error(format!("Method Set.prototype.{} called on incompatible receiver", method)))?;
    if !is_set(&obj) {
        return Err(VmError::type_error(format!("Method Set.prototype.{} called on incompatible receiver", method)));
    }
    let data = get_set_data(&obj)
        .ok_or_else(|| VmError::type_error("Set object missing internal data"))?;
    Ok((obj, data))
}

// ============================================================================
// Map Iterator (live iteration)
// ============================================================================

fn make_map_iterator(
    this_val: &Value,
    kind: &str,
    mm: Arc<MemoryManager>,
    fn_proto: GcRef<JsObject>,
    iter_proto: GcRef<JsObject>,
) -> Result<Value, VmError> {
    let (_obj, data) = require_map(this_val, "entries")?;

    let iter = GcRef::new(JsObject::new(Value::object(iter_proto), mm.clone()));
    // Store MapData reference for live iteration
    iter.set(pk("__map_data_ref__"), Value::map_data(data));
    iter.set(pk("__iter_index__"), Value::number(0.0));
    iter.set(pk("__iter_kind__"), Value::string(JsString::intern(kind)));

    let fn_proto_for_next = fn_proto;
    let mm_for_next = mm;
    iter.define_property(
        PropertyKey::string("next"),
        PropertyDescriptor::builtin_method(make_builtin(
            "next",
            0,
            |this_val, _args, ncx| {
                let iter_obj = this_val
                    .as_object()
                    .ok_or_else(|| VmError::type_error("not an iterator object"))?;
                let data = iter_obj
                    .get(&pk("__map_data_ref__"))
                    .and_then(|v| v.as_map_data())
                    .ok_or_else(|| VmError::type_error("iterator: missing map data ref"))?;
                let kind = iter_obj
                    .get(&pk("__iter_kind__"))
                    .and_then(|v| v.as_string().map(|s| s.as_str().to_string()))
                    .unwrap_or_else(|| "entry".to_string());

                let mut idx = iter_obj
                    .get(&pk("__iter_index__"))
                    .and_then(|v| v.as_number())
                    .unwrap_or(0.0) as usize;

                // Scan forward, skipping tombstones
                loop {
                    let entries_len = data.entries_len();
                    if idx >= entries_len {
                        let result = GcRef::new(JsObject::new(Value::null(), ncx.memory_manager().clone()));
                        result.set(pk("value"), Value::undefined());
                        result.set(pk("done"), Value::boolean(true));
                        // Park index at end
                        iter_obj.set(pk("__iter_index__"), Value::number(idx as f64));
                        return Ok(Value::object(result));
                    }

                    if let Some((key, value)) = data.entry_at(idx) {
                        idx += 1;
                        iter_obj.set(pk("__iter_index__"), Value::number(idx as f64));

                        let result = GcRef::new(JsObject::new(Value::null(), ncx.memory_manager().clone()));
                        match kind.as_str() {
                            "key" => {
                                result.set(pk("value"), key);
                            }
                            "entry" => {
                                let entry = GcRef::new(JsObject::array(2, ncx.memory_manager().clone()));
                                entry.set(PropertyKey::Index(0), key);
                                entry.set(PropertyKey::Index(1), value);
                                result.set(pk("value"), Value::array(entry));
                            }
                            _ => {
                                // "value"
                                result.set(pk("value"), value);
                            }
                        }
                        result.set(pk("done"), Value::boolean(false));
                        return Ok(Value::object(result));
                    } else {
                        // Tombstone, skip
                        idx += 1;
                    }
                }
            },
            mm_for_next,
            fn_proto_for_next,
        )),
    );
    Ok(Value::object(iter))
}

// ============================================================================
// Map.prototype
// ============================================================================

/// Initialize Map.prototype with all ES2023 methods.
pub fn init_map_prototype(
    map_proto: GcRef<JsObject>,
    fn_proto: GcRef<JsObject>,
    mm: &Arc<MemoryManager>,
    iterator_proto: GcRef<JsObject>,
    symbol_iterator: crate::gc::GcRef<crate::value::Symbol>,
) {
    // Map.prototype.get(key)
    map_proto.define_property(
        PropertyKey::string("get"),
        PropertyDescriptor::builtin_method(make_builtin(
            "get",
            1,
            |this_val, args, _ncx| {
                let (_, data) = require_map(this_val, "get")?;
                let key = args.first().cloned().unwrap_or(Value::undefined());
                Ok(data.get(&MapKey(key)).unwrap_or(Value::undefined()))
            },
            mm.clone(),
            fn_proto,
        )),
    );

    // Map.prototype.set(key, value)
    map_proto.define_property(
        PropertyKey::string("set"),
        PropertyDescriptor::builtin_method(make_builtin(
            "set",
            2,
            |this_val, args, _ncx| {
                let (_, data) = require_map(this_val, "set")?;
                let key = args.first().cloned().unwrap_or(Value::undefined());
                let value = args.get(1).cloned().unwrap_or(Value::undefined());
                // Normalize -0 to +0 for keys (SameValueZero)
                let normalized_key = if let Some(n) = key.as_number() {
                    if n == 0.0 { Value::number(0.0) } else { key }
                } else if let Some(i) = key.as_int32() {
                    if i == 0 { Value::number(0.0) } else { key }
                } else {
                    key
                };
                data.set(MapKey(normalized_key), value);
                Ok(this_val.clone())
            },
            mm.clone(),
            fn_proto,
        )),
    );

    // Map.prototype.has(key)
    map_proto.define_property(
        PropertyKey::string("has"),
        PropertyDescriptor::builtin_method(make_builtin(
            "has",
            1,
            |this_val, args, _ncx| {
                let (_, data) = require_map(this_val, "has")?;
                let key = args.first().cloned().unwrap_or(Value::undefined());
                Ok(Value::boolean(data.has(&MapKey(key))))
            },
            mm.clone(),
            fn_proto,
        )),
    );

    // Map.prototype.delete(key)
    map_proto.define_property(
        PropertyKey::string("delete"),
        PropertyDescriptor::builtin_method(make_builtin(
            "delete",
            1,
            |this_val, args, _ncx| {
                let (_, data) = require_map(this_val, "delete")?;
                let key = args.first().cloned().unwrap_or(Value::undefined());
                Ok(Value::boolean(data.delete(&MapKey(key))))
            },
            mm.clone(),
            fn_proto,
        )),
    );

    // Map.prototype.clear()
    map_proto.define_property(
        PropertyKey::string("clear"),
        PropertyDescriptor::builtin_method(make_builtin(
            "clear",
            0,
            |this_val, _args, _ncx| {
                let (_, data) = require_map(this_val, "clear")?;
                data.clear();
                Ok(Value::undefined())
            },
            mm.clone(),
            fn_proto,
        )),
    );

    // Map.prototype.size (accessor getter)
    map_proto.define_property(
        PropertyKey::string("size"),
        PropertyDescriptor::Accessor {
            get: Some(make_builtin(
                "get size",
                0,
                |this_val, _args, _ncx| {
                    let (_, data) = require_map(this_val, "size")?;
                    Ok(Value::int32(data.size() as i32))
                },
                mm.clone(),
                fn_proto,
            )),
            set: None,
            attributes: PropertyAttributes {
                writable: false,
                enumerable: false,
                configurable: true,
            },
        },
    );

    // Map.prototype.keys()
    let iter_proto_for_keys = iterator_proto;
    let mm_for_keys = mm.clone();
    let fn_proto_for_keys = fn_proto;
    map_proto.define_property(
        PropertyKey::string("keys"),
        PropertyDescriptor::builtin_method(make_builtin(
            "keys",
            0,
            move |this_val, _args, ncx| {
                make_map_iterator(this_val, "key", ncx.memory_manager().clone(), fn_proto_for_keys, iter_proto_for_keys)
            },
            mm_for_keys,
            fn_proto,
        )),
    );

    // Map.prototype.values()
    let iter_proto_for_values = iterator_proto;
    let mm_for_values = mm.clone();
    let fn_proto_for_values = fn_proto;
    map_proto.define_property(
        PropertyKey::string("values"),
        PropertyDescriptor::builtin_method(make_builtin(
            "values",
            0,
            move |this_val, _args, ncx| {
                make_map_iterator(this_val, "value", ncx.memory_manager().clone(), fn_proto_for_values, iter_proto_for_values)
            },
            mm_for_values,
            fn_proto,
        )),
    );

    // Map.prototype.entries()
    let iter_proto_for_entries = iterator_proto;
    let mm_for_entries = mm.clone();
    let fn_proto_for_entries = fn_proto;
    map_proto.define_property(
        PropertyKey::string("entries"),
        PropertyDescriptor::builtin_method(make_builtin(
            "entries",
            0,
            move |this_val, _args, ncx| {
                make_map_iterator(this_val, "entry", ncx.memory_manager().clone(), fn_proto_for_entries, iter_proto_for_entries)
            },
            mm_for_entries,
            fn_proto,
        )),
    );

    // Map.prototype.forEach(callback [, thisArg])
    map_proto.define_property(
        PropertyKey::string("forEach"),
        PropertyDescriptor::builtin_method(make_builtin(
            "forEach",
            1,
            |this_val, args, ncx| {
                let (_, data) = require_map(this_val, "forEach")?;
                let callback = args.first().cloned().unwrap_or(Value::undefined());
                let this_arg = args.get(1).cloned().unwrap_or(Value::undefined());

                if !callback.is_callable() {
                    return Err(VmError::type_error("Map.prototype.forEach: callback is not a function"));
                }

                // Live iteration: check entries_len each round to see new entries
                let mut pos = 0;
                loop {
                    let len = data.entries_len();
                    if pos >= len {
                        break;
                    }
                    if let Some((key, value)) = data.entry_at(pos) {
                        ncx.call_function(&callback, this_arg.clone(), &[value, key, this_val.clone()])?;
                    }
                    pos += 1;
                }
                Ok(Value::undefined())
            },
            mm.clone(),
            fn_proto,
        )),
    );

    // Map.prototype.getOrInsert(key, value) — ES2026 upsert proposal
    map_proto.define_property(
        PropertyKey::string("getOrInsert"),
        PropertyDescriptor::builtin_method(make_builtin(
            "getOrInsert",
            2,
            |this_val, args, _ncx| {
                let (_, data) = require_map(this_val, "getOrInsert")?;
                let key = args.first().cloned().unwrap_or(Value::undefined());
                let default_value = args.get(1).cloned().unwrap_or(Value::undefined());
                // Normalize -0 to +0
                let key = if let Some(n) = key.as_number() {
                    if n == 0.0 { Value::number(0.0) } else { key }
                } else {
                    key
                };
                Ok(data.get_or_insert(MapKey(key), default_value))
            },
            mm.clone(),
            fn_proto,
        )),
    );

    // Map.prototype.getOrInsertComputed(key, callbackfn) — ES2026 upsert proposal
    map_proto.define_property(
        PropertyKey::string("getOrInsertComputed"),
        PropertyDescriptor::builtin_method(make_builtin(
            "getOrInsertComputed",
            2,
            |this_val, args, ncx| {
                let (_, data) = require_map(this_val, "getOrInsertComputed")?;
                let key = args.first().cloned().unwrap_or(Value::undefined());
                let callback = args.get(1).cloned().unwrap_or(Value::undefined());
                // Normalize -0 to +0
                let key = if let Some(n) = key.as_number() {
                    if n == 0.0 { Value::number(0.0) } else { key }
                } else {
                    key
                };
                if !callback.is_callable() {
                    return Err(VmError::type_error("Map.prototype.getOrInsertComputed: callbackfn is not a function"));
                }
                // Check if key exists first
                let mk = MapKey(key.clone());
                if let Some(existing) = data.get(&mk) {
                    return Ok(existing);
                }
                // Key not found: Call(callbackfn, undefined, « key »)
                let value = ncx.call_function(&callback, Value::undefined(), &[key.clone()])?;
                data.set(MapKey(key), value.clone());
                Ok(value)
            },
            mm.clone(),
            fn_proto,
        )),
    );

    // Map.prototype[Symbol.iterator] = Map.prototype.entries
    let iter_proto_for_symbol = iterator_proto;
    let mm_for_symbol = mm.clone();
    let fn_proto_for_symbol = fn_proto;
    map_proto.define_property(
        PropertyKey::Symbol(symbol_iterator),
        PropertyDescriptor::builtin_method(make_builtin(
            "[Symbol.iterator]",
            0,
            move |this_val, _args, ncx| {
                make_map_iterator(this_val, "entry", ncx.memory_manager().clone(), fn_proto_for_symbol, iter_proto_for_symbol)
            },
            mm_for_symbol,
            fn_proto,
        )),
    );

    // Map.prototype[Symbol.toStringTag] = "Map"
    map_proto.define_property(
        PropertyKey::Symbol(crate::intrinsics::well_known::to_string_tag_symbol()),
        PropertyDescriptor::data_with_attrs(
            Value::string(JsString::intern("Map")),
            PropertyAttributes {
                writable: false,
                enumerable: false,
                configurable: true,
            },
        ),
    );
}

// ============================================================================
// Set Iterator (live iteration)
// ============================================================================

fn make_set_iterator(
    this_val: &Value,
    kind: &str,
    mm: Arc<MemoryManager>,
    fn_proto: GcRef<JsObject>,
    iter_proto: GcRef<JsObject>,
) -> Result<Value, VmError> {
    let (_obj, data) = require_set(this_val, "values")?;

    let iter = GcRef::new(JsObject::new(Value::object(iter_proto), mm.clone()));
    iter.set(pk("__set_data_ref__"), Value::set_data(data));
    iter.set(pk("__iter_index__"), Value::number(0.0));
    iter.set(pk("__iter_kind__"), Value::string(JsString::intern(kind)));

    let fn_proto_for_next = fn_proto;
    let mm_for_next = mm;
    iter.define_property(
        PropertyKey::string("next"),
        PropertyDescriptor::builtin_method(make_builtin(
            "next",
            0,
            |this_val, _args, ncx| {
                let iter_obj = this_val
                    .as_object()
                    .ok_or_else(|| VmError::type_error("not an iterator object"))?;
                let data = iter_obj
                    .get(&pk("__set_data_ref__"))
                    .and_then(|v| v.as_set_data())
                    .ok_or_else(|| VmError::type_error("iterator: missing set data ref"))?;
                let kind = iter_obj
                    .get(&pk("__iter_kind__"))
                    .and_then(|v| v.as_string().map(|s| s.as_str().to_string()))
                    .unwrap_or_else(|| "value".to_string());

                let mut idx = iter_obj
                    .get(&pk("__iter_index__"))
                    .and_then(|v| v.as_number())
                    .unwrap_or(0.0) as usize;

                loop {
                    let entries_len = data.entries_len();
                    if idx >= entries_len {
                        let result = GcRef::new(JsObject::new(Value::null(), ncx.memory_manager().clone()));
                        result.set(pk("value"), Value::undefined());
                        result.set(pk("done"), Value::boolean(true));
                        iter_obj.set(pk("__iter_index__"), Value::number(idx as f64));
                        return Ok(Value::object(result));
                    }

                    if let Some(value) = data.entry_at(idx) {
                        idx += 1;
                        iter_obj.set(pk("__iter_index__"), Value::number(idx as f64));

                        let result = GcRef::new(JsObject::new(Value::null(), ncx.memory_manager().clone()));
                        match kind.as_str() {
                            "entry" => {
                                let entry = GcRef::new(JsObject::array(2, ncx.memory_manager().clone()));
                                entry.set(PropertyKey::Index(0), value.clone());
                                entry.set(PropertyKey::Index(1), value);
                                result.set(pk("value"), Value::array(entry));
                            }
                            _ => {
                                // "value" or "key" (both same for Sets)
                                result.set(pk("value"), value);
                            }
                        }
                        result.set(pk("done"), Value::boolean(false));
                        return Ok(Value::object(result));
                    } else {
                        idx += 1;
                    }
                }
            },
            mm_for_next,
            fn_proto_for_next,
        )),
    );
    Ok(Value::object(iter))
}

// ============================================================================
// Set.prototype
// ============================================================================

/// Initialize Set.prototype with all ES2023 + ES2025 methods.
pub fn init_set_prototype(
    set_proto: GcRef<JsObject>,
    fn_proto: GcRef<JsObject>,
    mm: &Arc<MemoryManager>,
    iterator_proto: GcRef<JsObject>,
    symbol_iterator: crate::gc::GcRef<crate::value::Symbol>,
) {
    // Set.prototype.add(value)
    set_proto.define_property(
        PropertyKey::string("add"),
        PropertyDescriptor::builtin_method(make_builtin(
            "add",
            1,
            |this_val, args, _ncx| {
                let (_, data) = require_set(this_val, "add")?;
                let value = args.first().cloned().unwrap_or(Value::undefined());
                // Normalize -0 to +0 for SameValueZero
                let normalized = if let Some(n) = value.as_number() {
                    if n == 0.0 { Value::number(0.0) } else { value }
                } else if let Some(i) = value.as_int32() {
                    if i == 0 { Value::number(0.0) } else { value }
                } else {
                    value
                };
                data.add(MapKey(normalized));
                Ok(this_val.clone())
            },
            mm.clone(),
            fn_proto,
        )),
    );

    // Set.prototype.has(value)
    set_proto.define_property(
        PropertyKey::string("has"),
        PropertyDescriptor::builtin_method(make_builtin(
            "has",
            1,
            |this_val, args, _ncx| {
                let (_, data) = require_set(this_val, "has")?;
                let value = args.first().cloned().unwrap_or(Value::undefined());
                Ok(Value::boolean(data.has(&MapKey(value))))
            },
            mm.clone(),
            fn_proto,
        )),
    );

    // Set.prototype.delete(value)
    set_proto.define_property(
        PropertyKey::string("delete"),
        PropertyDescriptor::builtin_method(make_builtin(
            "delete",
            1,
            |this_val, args, _ncx| {
                let (_, data) = require_set(this_val, "delete")?;
                let value = args.first().cloned().unwrap_or(Value::undefined());
                Ok(Value::boolean(data.delete(&MapKey(value))))
            },
            mm.clone(),
            fn_proto,
        )),
    );

    // Set.prototype.clear()
    set_proto.define_property(
        PropertyKey::string("clear"),
        PropertyDescriptor::builtin_method(make_builtin(
            "clear",
            0,
            |this_val, _args, _ncx| {
                let (_, data) = require_set(this_val, "clear")?;
                data.clear();
                Ok(Value::undefined())
            },
            mm.clone(),
            fn_proto,
        )),
    );

    // Set.prototype.size (accessor getter)
    set_proto.define_property(
        PropertyKey::string("size"),
        PropertyDescriptor::Accessor {
            get: Some(make_builtin(
                "get size",
                0,
                |this_val, _args, _ncx| {
                    let (_, data) = require_set(this_val, "size")?;
                    Ok(Value::int32(data.size() as i32))
                },
                mm.clone(),
                fn_proto,
            )),
            set: None,
            attributes: PropertyAttributes {
                writable: false,
                enumerable: false,
                configurable: true,
            },
        },
    );

    // Set.prototype.values()
    let iter_proto_for_values = iterator_proto;
    let mm_for_values = mm.clone();
    let fn_proto_for_values = fn_proto;
    set_proto.define_property(
        PropertyKey::string("values"),
        PropertyDescriptor::builtin_method(make_builtin(
            "values",
            0,
            move |this_val, _args, ncx| {
                make_set_iterator(this_val, "value", ncx.memory_manager().clone(), fn_proto_for_values, iter_proto_for_values)
            },
            mm_for_values,
            fn_proto,
        )),
    );

    // Set.prototype.keys() - same as values() per spec
    let iter_proto_for_keys = iterator_proto;
    let mm_for_keys = mm.clone();
    let fn_proto_for_keys = fn_proto;
    set_proto.define_property(
        PropertyKey::string("keys"),
        PropertyDescriptor::builtin_method(make_builtin(
            "keys",
            0,
            move |this_val, _args, ncx| {
                make_set_iterator(this_val, "value", ncx.memory_manager().clone(), fn_proto_for_keys, iter_proto_for_keys)
            },
            mm_for_keys,
            fn_proto,
        )),
    );

    // Set.prototype.entries()
    let iter_proto_for_entries = iterator_proto;
    let mm_for_entries = mm.clone();
    let fn_proto_for_entries = fn_proto;
    set_proto.define_property(
        PropertyKey::string("entries"),
        PropertyDescriptor::builtin_method(make_builtin(
            "entries",
            0,
            move |this_val, _args, ncx| {
                make_set_iterator(this_val, "entry", ncx.memory_manager().clone(), fn_proto_for_entries, iter_proto_for_entries)
            },
            mm_for_entries,
            fn_proto,
        )),
    );

    // Set.prototype.forEach(callback [, thisArg])
    set_proto.define_property(
        PropertyKey::string("forEach"),
        PropertyDescriptor::builtin_method(make_builtin(
            "forEach",
            1,
            |this_val, args, ncx| {
                let (_, data) = require_set(this_val, "forEach")?;
                let callback = args.first().cloned().unwrap_or(Value::undefined());
                let this_arg = args.get(1).cloned().unwrap_or(Value::undefined());

                if !callback.is_callable() {
                    return Err(VmError::type_error("Set.prototype.forEach: callback is not a function"));
                }

                // Live iteration: check entries_len each round to see new entries
                let mut pos = 0;
                loop {
                    let len = data.entries_len();
                    if pos >= len {
                        break;
                    }
                    if let Some(value) = data.entry_at(pos) {
                        ncx.call_function(&callback, this_arg.clone(), &[value.clone(), value, this_val.clone()])?;
                    }
                    pos += 1;
                }
                Ok(Value::undefined())
            },
            mm.clone(),
            fn_proto,
        )),
    );

    // ========================================================================
    // ES2025 Set Methods
    // ========================================================================

    // Set.prototype.union(other)
    set_proto.define_property(
        PropertyKey::string("union"),
        PropertyDescriptor::builtin_method(make_builtin(
            "union",
            1,
            |this_val, args, ncx| {
                let (_, this_data) = require_set(this_val, "union")?;
                let other = args.first().ok_or_else(|| VmError::type_error("Set.prototype.union requires argument"))?;
                let other_obj = other.as_object()
                    .ok_or_else(|| VmError::type_error("Set.prototype.union requires a Set-like argument"))?;
                if !is_set(&other_obj) {
                    return Err(VmError::type_error("Set.prototype.union requires a Set-like argument"));
                }
                let other_data = get_set_data(&other_obj)
                    .ok_or_else(|| VmError::type_error("Set object missing internal data"))?;

                let result = GcRef::new(JsObject::new(Value::null(), ncx.memory_manager().clone()));
                init_set_slots(&result);
                let result_data = get_set_data(&result).unwrap();

                for val in this_data.for_each_entries() {
                    result_data.add(MapKey(val));
                }
                for val in other_data.for_each_entries() {
                    result_data.add(MapKey(val));
                }
                Ok(Value::object(result))
            },
            mm.clone(),
            fn_proto,
        )),
    );

    // Set.prototype.intersection(other)
    set_proto.define_property(
        PropertyKey::string("intersection"),
        PropertyDescriptor::builtin_method(make_builtin(
            "intersection",
            1,
            |this_val, args, ncx| {
                let (_, this_data) = require_set(this_val, "intersection")?;
                let other = args.first().ok_or_else(|| VmError::type_error("Set.prototype.intersection requires argument"))?;
                let other_obj = other.as_object()
                    .ok_or_else(|| VmError::type_error("Set.prototype.intersection requires a Set-like argument"))?;
                if !is_set(&other_obj) {
                    return Err(VmError::type_error("Set.prototype.intersection requires a Set-like argument"));
                }
                let other_data = get_set_data(&other_obj)
                    .ok_or_else(|| VmError::type_error("Set object missing internal data"))?;

                let result = GcRef::new(JsObject::new(Value::null(), ncx.memory_manager().clone()));
                init_set_slots(&result);
                let result_data = get_set_data(&result).unwrap();

                for val in this_data.for_each_entries() {
                    if other_data.has(&MapKey(val.clone())) {
                        result_data.add(MapKey(val));
                    }
                }
                Ok(Value::object(result))
            },
            mm.clone(),
            fn_proto,
        )),
    );

    // Set.prototype.difference(other)
    set_proto.define_property(
        PropertyKey::string("difference"),
        PropertyDescriptor::builtin_method(make_builtin(
            "difference",
            1,
            |this_val, args, ncx| {
                let (_, this_data) = require_set(this_val, "difference")?;
                let other = args.first().ok_or_else(|| VmError::type_error("Set.prototype.difference requires argument"))?;
                let other_obj = other.as_object()
                    .ok_or_else(|| VmError::type_error("Set.prototype.difference requires a Set-like argument"))?;
                if !is_set(&other_obj) {
                    return Err(VmError::type_error("Set.prototype.difference requires a Set-like argument"));
                }
                let other_data = get_set_data(&other_obj)
                    .ok_or_else(|| VmError::type_error("Set object missing internal data"))?;

                let result = GcRef::new(JsObject::new(Value::null(), ncx.memory_manager().clone()));
                init_set_slots(&result);
                let result_data = get_set_data(&result).unwrap();

                for val in this_data.for_each_entries() {
                    if !other_data.has(&MapKey(val.clone())) {
                        result_data.add(MapKey(val));
                    }
                }
                Ok(Value::object(result))
            },
            mm.clone(),
            fn_proto,
        )),
    );

    // Set.prototype.symmetricDifference(other)
    set_proto.define_property(
        PropertyKey::string("symmetricDifference"),
        PropertyDescriptor::builtin_method(make_builtin(
            "symmetricDifference",
            1,
            |this_val, args, ncx| {
                let (_, this_data) = require_set(this_val, "symmetricDifference")?;
                let other = args.first().ok_or_else(|| VmError::type_error("Set.prototype.symmetricDifference requires argument"))?;
                let other_obj = other.as_object()
                    .ok_or_else(|| VmError::type_error("Set.prototype.symmetricDifference requires a Set-like argument"))?;
                if !is_set(&other_obj) {
                    return Err(VmError::type_error("Set.prototype.symmetricDifference requires a Set-like argument"));
                }
                let other_data = get_set_data(&other_obj)
                    .ok_or_else(|| VmError::type_error("Set object missing internal data"))?;

                let result = GcRef::new(JsObject::new(Value::null(), ncx.memory_manager().clone()));
                init_set_slots(&result);
                let result_data = get_set_data(&result).unwrap();

                // In this but not other
                for val in this_data.for_each_entries() {
                    if !other_data.has(&MapKey(val.clone())) {
                        result_data.add(MapKey(val));
                    }
                }
                // In other but not this
                for val in other_data.for_each_entries() {
                    if !this_data.has(&MapKey(val.clone())) {
                        result_data.add(MapKey(val));
                    }
                }
                Ok(Value::object(result))
            },
            mm.clone(),
            fn_proto,
        )),
    );

    // Set.prototype.isSubsetOf(other)
    set_proto.define_property(
        PropertyKey::string("isSubsetOf"),
        PropertyDescriptor::builtin_method(make_builtin(
            "isSubsetOf",
            1,
            |this_val, args, _ncx| {
                let (_, this_data) = require_set(this_val, "isSubsetOf")?;
                let other = args.first().ok_or_else(|| VmError::type_error("Set.prototype.isSubsetOf requires argument"))?;
                let other_obj = other.as_object()
                    .ok_or_else(|| VmError::type_error("Set.prototype.isSubsetOf requires a Set-like argument"))?;
                if !is_set(&other_obj) {
                    return Err(VmError::type_error("Set.prototype.isSubsetOf requires a Set-like argument"));
                }
                let other_data = get_set_data(&other_obj)
                    .ok_or_else(|| VmError::type_error("Set object missing internal data"))?;

                for val in this_data.for_each_entries() {
                    if !other_data.has(&MapKey(val)) {
                        return Ok(Value::boolean(false));
                    }
                }
                Ok(Value::boolean(true))
            },
            mm.clone(),
            fn_proto,
        )),
    );

    // Set.prototype.isSupersetOf(other)
    set_proto.define_property(
        PropertyKey::string("isSupersetOf"),
        PropertyDescriptor::builtin_method(make_builtin(
            "isSupersetOf",
            1,
            |this_val, args, _ncx| {
                let (_, this_data) = require_set(this_val, "isSupersetOf")?;
                let other = args.first().ok_or_else(|| VmError::type_error("Set.prototype.isSupersetOf requires argument"))?;
                let other_obj = other.as_object()
                    .ok_or_else(|| VmError::type_error("Set.prototype.isSupersetOf requires a Set-like argument"))?;
                if !is_set(&other_obj) {
                    return Err(VmError::type_error("Set.prototype.isSupersetOf requires a Set-like argument"));
                }
                let other_data = get_set_data(&other_obj)
                    .ok_or_else(|| VmError::type_error("Set object missing internal data"))?;

                for val in other_data.for_each_entries() {
                    if !this_data.has(&MapKey(val)) {
                        return Ok(Value::boolean(false));
                    }
                }
                Ok(Value::boolean(true))
            },
            mm.clone(),
            fn_proto,
        )),
    );

    // Set.prototype.isDisjointFrom(other)
    set_proto.define_property(
        PropertyKey::string("isDisjointFrom"),
        PropertyDescriptor::builtin_method(make_builtin(
            "isDisjointFrom",
            1,
            |this_val, args, _ncx| {
                let (_, this_data) = require_set(this_val, "isDisjointFrom")?;
                let other = args.first().ok_or_else(|| VmError::type_error("Set.prototype.isDisjointFrom requires argument"))?;
                let other_obj = other.as_object()
                    .ok_or_else(|| VmError::type_error("Set.prototype.isDisjointFrom requires a Set-like argument"))?;
                if !is_set(&other_obj) {
                    return Err(VmError::type_error("Set.prototype.isDisjointFrom requires a Set-like argument"));
                }
                let other_data = get_set_data(&other_obj)
                    .ok_or_else(|| VmError::type_error("Set object missing internal data"))?;

                for val in this_data.for_each_entries() {
                    if other_data.has(&MapKey(val)) {
                        return Ok(Value::boolean(false));
                    }
                }
                Ok(Value::boolean(true))
            },
            mm.clone(),
            fn_proto,
        )),
    );

    // Set.prototype[Symbol.iterator] = Set.prototype.values
    let iter_proto_for_symbol = iterator_proto;
    let mm_for_symbol = mm.clone();
    let fn_proto_for_symbol = fn_proto;
    set_proto.define_property(
        PropertyKey::Symbol(symbol_iterator),
        PropertyDescriptor::builtin_method(make_builtin(
            "[Symbol.iterator]",
            0,
            move |this_val, _args, ncx| {
                make_set_iterator(this_val, "value", ncx.memory_manager().clone(), fn_proto_for_symbol, iter_proto_for_symbol)
            },
            mm_for_symbol,
            fn_proto,
        )),
    );

    // Set.prototype[Symbol.toStringTag] = "Set"
    set_proto.define_property(
        PropertyKey::Symbol(crate::intrinsics::well_known::to_string_tag_symbol()),
        PropertyDescriptor::data_with_attrs(
            Value::string(JsString::intern("Set")),
            PropertyAttributes {
                writable: false,
                enumerable: false,
                configurable: true,
            },
        ),
    );
}

// ============================================================================
// WeakMap.prototype (pointer-identity keys, no iteration)
// ============================================================================

/// WeakMap uses a HashMap<usize, Value> keyed by object pointer.
/// NOT actually weak yet (requires GC ephemeron support), but uses proper
/// pointer identity instead of string serialization.
fn get_weakmap_entries(obj: &GcRef<JsObject>) -> Option<GcRef<JsObject>> {
    obj.get(&pk(WEAKMAP_ENTRIES_KEY)).and_then(|v| v.as_object())
}

/// Initialize WeakMap.prototype with all ES2023 methods.
pub fn init_weak_map_prototype(
    wm_proto: GcRef<JsObject>,
    fn_proto: GcRef<JsObject>,
    mm: &Arc<MemoryManager>,
) {
    // WeakMap.prototype.get(key)
    wm_proto.define_property(
        PropertyKey::string("get"),
        PropertyDescriptor::builtin_method(make_builtin(
            "get",
            1,
            |this_val, args, _ncx| {
                let obj = this_val
                    .as_object()
                    .ok_or_else(|| VmError::type_error("Method WeakMap.prototype.get called on incompatible receiver"))?;
                if !is_weakmap(&obj) {
                    return Err(VmError::type_error("Method WeakMap.prototype.get called on incompatible receiver"));
                }
                let key = args.first().cloned().unwrap_or(Value::undefined());
                if !is_valid_weak_key(&key) {
                    return Ok(Value::undefined());
                }
                let id = weak_key_id(&key).ok_or_else(|| VmError::type_error("Invalid weak key"))?;
                let entries = get_weakmap_entries(&obj).ok_or("Internal error: missing entries")?;
                let pk_id = PropertyKey::string(&format!("__wk_{}", id));
                Ok(entries.get(&pk_id).unwrap_or(Value::undefined()))
            },
            mm.clone(),
            fn_proto,
        )),
    );

    // WeakMap.prototype.set(key, value)
    wm_proto.define_property(
        PropertyKey::string("set"),
        PropertyDescriptor::builtin_method(make_builtin(
            "set",
            2,
            |this_val, args, _ncx| {
                let obj = this_val
                    .as_object()
                    .ok_or_else(|| VmError::type_error("Method WeakMap.prototype.set called on incompatible receiver"))?;
                if !is_weakmap(&obj) {
                    return Err(VmError::type_error("Method WeakMap.prototype.set called on incompatible receiver"));
                }
                let key = args.first().cloned().unwrap_or(Value::undefined());
                if !is_valid_weak_key(&key) {
                    return Err(VmError::type_error("Invalid value used as weak map key"));
                }
                let value = args.get(1).cloned().unwrap_or(Value::undefined());
                let id = weak_key_id(&key).ok_or_else(|| VmError::type_error("Invalid weak key"))?;
                let entries = get_weakmap_entries(&obj).ok_or("Internal error: missing entries")?;
                let pk_id = PropertyKey::string(&format!("__wk_{}", id));
                entries.set(pk_id, value);
                Ok(this_val.clone())
            },
            mm.clone(),
            fn_proto,
        )),
    );

    // WeakMap.prototype.has(key)
    wm_proto.define_property(
        PropertyKey::string("has"),
        PropertyDescriptor::builtin_method(make_builtin(
            "has",
            1,
            |this_val, args, _ncx| {
                let obj = this_val
                    .as_object()
                    .ok_or_else(|| VmError::type_error("Method WeakMap.prototype.has called on incompatible receiver"))?;
                if !is_weakmap(&obj) {
                    return Err(VmError::type_error("Method WeakMap.prototype.has called on incompatible receiver"));
                }
                let key = args.first().cloned().unwrap_or(Value::undefined());
                if !is_valid_weak_key(&key) {
                    return Ok(Value::boolean(false));
                }
                let id = match weak_key_id(&key) {
                    Some(id) => id,
                    None => return Ok(Value::boolean(false)),
                };
                let entries = get_weakmap_entries(&obj).ok_or("Internal error: missing entries")?;
                let pk_id = PropertyKey::string(&format!("__wk_{}", id));
                Ok(Value::boolean(entries.get(&pk_id).is_some()))
            },
            mm.clone(),
            fn_proto,
        )),
    );

    // WeakMap.prototype.delete(key)
    wm_proto.define_property(
        PropertyKey::string("delete"),
        PropertyDescriptor::builtin_method(make_builtin(
            "delete",
            1,
            |this_val, args, _ncx| {
                let obj = this_val
                    .as_object()
                    .ok_or_else(|| VmError::type_error("Method WeakMap.prototype.delete called on incompatible receiver"))?;
                if !is_weakmap(&obj) {
                    return Err(VmError::type_error("Method WeakMap.prototype.delete called on incompatible receiver"));
                }
                let key = args.first().cloned().unwrap_or(Value::undefined());
                if !is_valid_weak_key(&key) {
                    return Ok(Value::boolean(false));
                }
                let id = match weak_key_id(&key) {
                    Some(id) => id,
                    None => return Ok(Value::boolean(false)),
                };
                let entries = get_weakmap_entries(&obj).ok_or("Internal error: missing entries")?;
                let pk_id = PropertyKey::string(&format!("__wk_{}", id));
                if entries.get(&pk_id).is_none() {
                    return Ok(Value::boolean(false));
                }
                entries.delete(&pk_id);
                Ok(Value::boolean(true))
            },
            mm.clone(),
            fn_proto,
        )),
    );

    // WeakMap.prototype[Symbol.toStringTag] = "WeakMap"
    wm_proto.define_property(
        PropertyKey::Symbol(crate::intrinsics::well_known::to_string_tag_symbol()),
        PropertyDescriptor::data_with_attrs(
            Value::string(JsString::intern("WeakMap")),
            PropertyAttributes {
                writable: false,
                enumerable: false,
                configurable: true,
            },
        ),
    );
}

// ============================================================================
// WeakSet.prototype
// ============================================================================

fn get_weakset_entries(obj: &GcRef<JsObject>) -> Option<GcRef<JsObject>> {
    obj.get(&pk(WEAKSET_ENTRIES_KEY)).and_then(|v| v.as_object())
}

/// Initialize WeakSet.prototype with all ES2023 methods.
pub fn init_weak_set_prototype(
    ws_proto: GcRef<JsObject>,
    fn_proto: GcRef<JsObject>,
    mm: &Arc<MemoryManager>,
) {
    // WeakSet.prototype.add(value)
    ws_proto.define_property(
        PropertyKey::string("add"),
        PropertyDescriptor::builtin_method(make_builtin(
            "add",
            1,
            |this_val, args, _ncx| {
                let obj = this_val
                    .as_object()
                    .ok_or_else(|| VmError::type_error("Method WeakSet.prototype.add called on incompatible receiver"))?;
                if !is_weakset(&obj) {
                    return Err(VmError::type_error("Method WeakSet.prototype.add called on incompatible receiver"));
                }
                let value = args.first().cloned().unwrap_or(Value::undefined());
                if !is_valid_weak_key(&value) {
                    return Err(VmError::type_error("Invalid value used in weak set"));
                }
                let id = weak_key_id(&value).ok_or_else(|| VmError::type_error("Invalid weak key"))?;
                let entries = get_weakset_entries(&obj).ok_or("Internal error: missing entries")?;
                let pk_id = PropertyKey::string(&format!("__wk_{}", id));
                entries.set(pk_id, Value::boolean(true));
                Ok(this_val.clone())
            },
            mm.clone(),
            fn_proto,
        )),
    );

    // WeakSet.prototype.has(value)
    ws_proto.define_property(
        PropertyKey::string("has"),
        PropertyDescriptor::builtin_method(make_builtin(
            "has",
            1,
            |this_val, args, _ncx| {
                let obj = this_val
                    .as_object()
                    .ok_or_else(|| VmError::type_error("Method WeakSet.prototype.has called on incompatible receiver"))?;
                if !is_weakset(&obj) {
                    return Err(VmError::type_error("Method WeakSet.prototype.has called on incompatible receiver"));
                }
                let value = args.first().cloned().unwrap_or(Value::undefined());
                if !is_valid_weak_key(&value) {
                    return Ok(Value::boolean(false));
                }
                let id = match weak_key_id(&value) {
                    Some(id) => id,
                    None => return Ok(Value::boolean(false)),
                };
                let entries = get_weakset_entries(&obj).ok_or("Internal error: missing entries")?;
                let pk_id = PropertyKey::string(&format!("__wk_{}", id));
                Ok(Value::boolean(entries.get(&pk_id).is_some()))
            },
            mm.clone(),
            fn_proto,
        )),
    );

    // WeakSet.prototype.delete(value)
    ws_proto.define_property(
        PropertyKey::string("delete"),
        PropertyDescriptor::builtin_method(make_builtin(
            "delete",
            1,
            |this_val, args, _ncx| {
                let obj = this_val
                    .as_object()
                    .ok_or_else(|| VmError::type_error("Method WeakSet.prototype.delete called on incompatible receiver"))?;
                if !is_weakset(&obj) {
                    return Err(VmError::type_error("Method WeakSet.prototype.delete called on incompatible receiver"));
                }
                let value = args.first().cloned().unwrap_or(Value::undefined());
                if !is_valid_weak_key(&value) {
                    return Ok(Value::boolean(false));
                }
                let id = match weak_key_id(&value) {
                    Some(id) => id,
                    None => return Ok(Value::boolean(false)),
                };
                let entries = get_weakset_entries(&obj).ok_or("Internal error: missing entries")?;
                let pk_id = PropertyKey::string(&format!("__wk_{}", id));
                if entries.get(&pk_id).is_none() {
                    return Ok(Value::boolean(false));
                }
                entries.delete(&pk_id);
                Ok(Value::boolean(true))
            },
            mm.clone(),
            fn_proto,
        )),
    );

    // WeakSet.prototype[Symbol.toStringTag] = "WeakSet"
    ws_proto.define_property(
        PropertyKey::Symbol(crate::intrinsics::well_known::to_string_tag_symbol()),
        PropertyDescriptor::data_with_attrs(
            Value::string(JsString::intern("WeakSet")),
            PropertyAttributes {
                writable: false,
                enumerable: false,
                configurable: true,
            },
        ),
    );
}

// ============================================================================
// Constructor functions
// ============================================================================

/// Create Map constructor function.
/// Supports `new Map()` and `new Map(iterable)`.
pub fn create_map_constructor() -> Box<
    dyn Fn(&Value, &[Value], &mut crate::context::NativeContext<'_>) -> Result<Value, VmError>
        + Send
        + Sync,
> {
    Box::new(|this_val, args, _ncx| {
        if this_val.is_undefined() {
            return Err(VmError::type_error("Constructor Map requires 'new'"));
        }
        if let Some(obj) = this_val.as_object() {
            init_map_slots(&obj);

            // Handle iterable argument: new Map([[k1, v1], [k2, v2], ...])
            let iterable = args.first().cloned().unwrap_or(Value::undefined());
            if !iterable.is_undefined() && !iterable.is_null() {
                let data = get_map_data(&obj).unwrap();
                // Try to iterate the argument as an array-like
                if let Some(arr) = iterable.as_array().or_else(|| iterable.as_object()) {
                    let len = arr.get(&PropertyKey::string("length"))
                        .and_then(|v| v.as_number())
                        .unwrap_or(0.0) as usize;
                    for i in 0..len {
                        if let Some(entry) = arr.get(&PropertyKey::Index(i as u32))
                            && let Some(entry_obj) = entry.as_array().or_else(|| entry.as_object())
                        {
                            let key = entry_obj.get(&PropertyKey::Index(0)).unwrap_or(Value::undefined());
                            let value = entry_obj.get(&PropertyKey::Index(1)).unwrap_or(Value::undefined());
                            data.set(MapKey(key), value);
                        }
                    }
                }
            }

            Ok(this_val.clone())
        } else {
            Err(VmError::type_error("Constructor Map requires 'new'"))
        }
    })
}

/// Create Set constructor function.
/// Supports `new Set()` and `new Set(iterable)`.
pub fn create_set_constructor() -> Box<
    dyn Fn(&Value, &[Value], &mut crate::context::NativeContext<'_>) -> Result<Value, VmError>
        + Send
        + Sync,
> {
    Box::new(|this_val, args, _ncx| {
        if this_val.is_undefined() {
            return Err(VmError::type_error("Constructor Set requires 'new'"));
        }
        if let Some(obj) = this_val.as_object() {
            init_set_slots(&obj);

            // Handle iterable argument: new Set([v1, v2, ...])
            let iterable = args.first().cloned().unwrap_or(Value::undefined());
            if !iterable.is_undefined() && !iterable.is_null() {
                let data = get_set_data(&obj).unwrap();
                if let Some(arr) = iterable.as_array().or_else(|| iterable.as_object()) {
                    let len = arr.get(&PropertyKey::string("length"))
                        .and_then(|v| v.as_number())
                        .unwrap_or(0.0) as usize;
                    for i in 0..len {
                        if let Some(value) = arr.get(&PropertyKey::Index(i as u32)) {
                            data.add(MapKey(value));
                        }
                    }
                }
            }

            Ok(this_val.clone())
        } else {
            Err(VmError::type_error("Constructor Set requires 'new'"))
        }
    })
}

/// Create WeakMap constructor function.
pub fn create_weak_map_constructor() -> Box<
    dyn Fn(&Value, &[Value], &mut crate::context::NativeContext<'_>) -> Result<Value, VmError>
        + Send
        + Sync,
> {
    Box::new(|this_val, _args, ncx| {
        if this_val.is_undefined() {
            return Err(VmError::type_error("Constructor WeakMap requires 'new'"));
        }
        if let Some(obj) = this_val.as_object() {
            let entries = GcRef::new(JsObject::new(Value::null(), ncx.memory_manager().clone()));
            obj.set(pk(WEAKMAP_ENTRIES_KEY), Value::object(entries));
            obj.set(pk(IS_WEAKMAP_KEY), Value::boolean(true));
            Ok(this_val.clone())
        } else {
            Err(VmError::type_error("Constructor WeakMap requires 'new'"))
        }
    })
}

/// Create WeakSet constructor function.
pub fn create_weak_set_constructor() -> Box<
    dyn Fn(&Value, &[Value], &mut crate::context::NativeContext<'_>) -> Result<Value, VmError>
        + Send
        + Sync,
> {
    Box::new(|this_val, _args, ncx| {
        if this_val.is_undefined() {
            return Err(VmError::type_error("Constructor WeakSet requires 'new'"));
        }
        if let Some(obj) = this_val.as_object() {
            let entries = GcRef::new(JsObject::new(Value::null(), ncx.memory_manager().clone()));
            obj.set(pk(WEAKSET_ENTRIES_KEY), Value::object(entries));
            obj.set(pk(IS_WEAKSET_KEY), Value::boolean(true));
            Ok(this_val.clone())
        } else {
            Err(VmError::type_error("Constructor WeakSet requires 'new'"))
        }
    })
}
