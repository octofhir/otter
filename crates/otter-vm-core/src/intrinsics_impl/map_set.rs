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
        let _ = obj.set(pk("__non_constructor"), Value::boolean(true));
    }
    val
}

fn is_map(obj: &GcRef<JsObject>) -> bool {
    obj.get(&pk(IS_MAP_KEY)).and_then(|v| v.as_boolean()) == Some(true)
}

fn is_set(obj: &GcRef<JsObject>) -> bool {
    obj.get(&pk(IS_SET_KEY)).and_then(|v| v.as_boolean()) == Some(true)
}

fn is_weakmap(obj: &GcRef<JsObject>) -> bool {
    obj.get(&pk(IS_WEAKMAP_KEY)).and_then(|v| v.as_boolean()) == Some(true)
}

fn is_weakset(obj: &GcRef<JsObject>) -> bool {
    obj.get(&pk(IS_WEAKSET_KEY)).and_then(|v| v.as_boolean()) == Some(true)
}

fn get_map_data(obj: &GcRef<JsObject>) -> Option<GcRef<MapData>> {
    obj.get(&pk(MAP_DATA_KEY)).and_then(|v| v.as_map_data())
}

fn get_set_data(obj: &GcRef<JsObject>) -> Option<GcRef<SetData>> {
    obj.get(&pk(SET_DATA_KEY)).and_then(|v| v.as_set_data())
}

fn is_valid_weak_key(
    value: &Value,
    symbol_registry: &crate::symbol_registry::SymbolRegistry,
) -> bool {
    if value.is_object() || value.is_function() {
        return true;
    }
    // Symbols are valid weak keys UNLESS they are registered (Symbol.for).
    // Registered symbols are globally shared and never garbage collected.
    if let Some(sym) = value.as_symbol() {
        return symbol_registry.key_for(&sym).is_none();
    }
    false
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

/// Get GC header pointer from a weak key value
fn weak_key_header(value: &Value) -> Option<*const otter_vm_gc::GcHeader> {
    value.gc_header()
}

/// Convert Value to bytes for ephemeron storage
fn value_to_bytes(value: &Value) -> Vec<u8> {
    // Value is 16 bytes (8 bytes for bits + 8 bytes for Option<HeapRef>)
    // We serialize it directly as bytes
    unsafe {
        let ptr = value as *const Value as *const u8;
        std::slice::from_raw_parts(ptr, std::mem::size_of::<Value>()).to_vec()
    }
}

/// Convert bytes back to Value from ephemeron storage
unsafe fn bytes_to_value(bytes: &[u8]) -> Value {
    assert_eq!(bytes.len(), std::mem::size_of::<Value>());
    unsafe { std::ptr::read(bytes.as_ptr() as *const Value) }
}

/// JavaScript-style property Get that triggers accessor getters.
/// Unlike `obj.get()`, this properly calls getter functions.
fn js_get(
    obj: &GcRef<JsObject>,
    key: &PropertyKey,
    this_val: &Value,
    ncx: &mut crate::context::NativeContext<'_>,
) -> Result<Value, VmError> {
    if let Some(desc) = obj.lookup_property_descriptor(key) {
        match desc {
            PropertyDescriptor::Data { value, .. } => Ok(value),
            PropertyDescriptor::Accessor { get, .. } => {
                if let Some(getter) = get {
                    if getter.is_callable() {
                        ncx.call_function(&getter, this_val.clone(), &[])
                    } else {
                        Ok(Value::undefined())
                    }
                } else {
                    Ok(Value::undefined())
                }
            }
            PropertyDescriptor::Deleted => Ok(Value::undefined()),
        }
    } else {
        Ok(Value::undefined())
    }
}

/// Get a property value from an object, properly triggering accessor getters.
/// Unlike `obj.get()`, this calls getter functions when the property is an accessor.
fn js_get_value(
    obj: &GcRef<JsObject>,
    key: &PropertyKey,
    receiver: &Value,
    ncx: &mut crate::context::NativeContext<'_>,
) -> Result<Value, VmError> {
    if let Some(desc) = obj.lookup_property_descriptor(key) {
        match desc {
            PropertyDescriptor::Data { value, .. } => Ok(value),
            PropertyDescriptor::Accessor { get, .. } => {
                if let Some(getter) = get {
                    if getter.is_callable() {
                        ncx.call_function(&getter, receiver.clone(), &[])
                    } else {
                        Ok(Value::undefined())
                    }
                } else {
                    Ok(Value::undefined())
                }
            }
            PropertyDescriptor::Deleted => Ok(Value::undefined()),
        }
    } else {
        Ok(Value::undefined())
    }
}

/// Close an iterator by calling its `return` method (IteratorClose).
/// Ignores errors from `return()` — the original error takes precedence.
fn iterator_close(
    iterator: &Value,
    iterator_obj: &GcRef<JsObject>,
    ncx: &mut crate::context::NativeContext<'_>,
) {
    if let Some(ret_fn) = iterator_obj.get(&pk("return")) {
        if ret_fn.is_callable() {
            let _ = ncx.call_function(&ret_fn, iterator.clone(), &[]);
        }
    }
}

/// Iterate an iterable using the Symbol.iterator protocol.
/// Calls `callback(value)` for each yielded value.
/// Implements IteratorClose: if callback returns an error, calls `iterator.return()` before propagating.
fn iterate_with_protocol(
    iterable: &Value,
    ncx: &mut crate::context::NativeContext<'_>,
    mut callback: impl FnMut(Value, &mut crate::context::NativeContext<'_>) -> Result<(), VmError>,
) -> Result<(), VmError> {
    let iter_sym = crate::intrinsics::well_known::iterator_symbol();
    let iter_key = PropertyKey::Symbol(iter_sym);

    // Get the @@iterator method
    // For strings, look up Symbol.iterator on the String prototype
    let iter_fn = if let Some(obj) = iterable.as_object().or_else(|| iterable.as_array()) {
        obj.get(&iter_key).unwrap_or(Value::undefined())
    } else if iterable.as_string().is_some() {
        // String primitive: get Symbol.iterator from String.prototype
        ncx.ctx
            .get_global("String")
            .and_then(|v| v.as_object())
            .and_then(|c| c.get(&pk("prototype")))
            .and_then(|v| v.as_object())
            .and_then(|proto| proto.get(&iter_key))
            .unwrap_or(Value::undefined())
    } else {
        Value::undefined()
    };

    if !iter_fn.is_callable() {
        return Err(VmError::type_error("object is not iterable"));
    }

    // Call @@iterator to get the iterator
    let iterator = ncx.call_function(&iter_fn, iterable.clone(), &[])?;
    let iterator_obj = iterator
        .as_object()
        .ok_or_else(|| VmError::type_error("Iterator result is not an object"))?;

    // Get the `next` method
    let next_fn = iterator_obj.get(&pk("next")).unwrap_or(Value::undefined());
    if !next_fn.is_callable() {
        return Err(VmError::type_error("Iterator .next is not a function"));
    }

    // Iterate
    loop {
        let result = ncx.call_function(&next_fn, iterator.clone(), &[])?;
        let result_obj = result
            .as_object()
            .ok_or_else(|| VmError::type_error("Iterator result is not an object"))?;

        // Check .done - must use js_get_value to trigger accessor getters
        let done = js_get_value(&result_obj, &pk("done"), &result, ncx)?;
        if done.to_boolean() {
            break;
        }

        // Get .value - must use js_get_value to trigger accessor getters (spec: IteratorValue calls Get)
        let value = js_get_value(&result_obj, &pk("value"), &result, ncx)?;

        // Call the callback; on error, close the iterator (IfAbruptCloseIterator)
        if let Err(err) = callback(value, ncx) {
            iterator_close(&iterator, &iterator_obj, ncx);
            return Err(err);
        }
    }

    Ok(())
}

fn init_map_slots(obj: &GcRef<JsObject>) {
    let data = GcRef::new(MapData::new());
    let _ = obj.set(pk(MAP_DATA_KEY), Value::map_data(data));
    let _ = obj.set(pk(IS_MAP_KEY), Value::boolean(true));
}

fn init_set_slots(obj: &GcRef<JsObject>) {
    let data = GcRef::new(SetData::new());
    let _ = obj.set(pk(SET_DATA_KEY), Value::set_data(data));
    let _ = obj.set(pk(IS_SET_KEY), Value::boolean(true));
}

/// Require `this` to be a Map and extract its data.
fn require_map(
    this_val: &Value,
    method: &str,
) -> Result<(GcRef<JsObject>, GcRef<MapData>), VmError> {
    let obj = this_val.as_object().ok_or_else(|| {
        VmError::type_error(format!(
            "Method Map.prototype.{} called on incompatible receiver",
            method
        ))
    })?;
    if !is_map(&obj) {
        return Err(VmError::type_error(format!(
            "Method Map.prototype.{} called on incompatible receiver",
            method
        )));
    }
    let data = get_map_data(&obj)
        .ok_or_else(|| VmError::type_error("Map object missing internal data"))?;
    Ok((obj, data))
}

/// Require `this` to be a Set and extract its data.
fn require_set(
    this_val: &Value,
    method: &str,
) -> Result<(GcRef<JsObject>, GcRef<SetData>), VmError> {
    let obj = this_val.as_object().ok_or_else(|| {
        VmError::type_error(format!(
            "Method Set.prototype.{} called on incompatible receiver",
            method
        ))
    })?;
    if !is_set(&obj) {
        return Err(VmError::type_error(format!(
            "Method Set.prototype.{} called on incompatible receiver",
            method
        )));
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
    let _ = iter.set(pk("__map_data_ref__"), Value::map_data(data));
    let _ = iter.set(pk("__iter_index__"), Value::number(0.0));
    let _ = iter.set(pk("__iter_kind__"), Value::string(JsString::intern(kind)));

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
                        let result =
                            GcRef::new(JsObject::new(Value::null(), ncx.memory_manager().clone()));
                        let _ = result.set(pk("value"), Value::undefined());
                        let _ = result.set(pk("done"), Value::boolean(true));
                        // Park index at end
                        let _ = iter_obj.set(pk("__iter_index__"), Value::number(idx as f64));
                        return Ok(Value::object(result));
                    }

                    if let Some((key, value)) = data.entry_at(idx) {
                        idx += 1;
                        let _ = iter_obj.set(pk("__iter_index__"), Value::number(idx as f64));

                        let result =
                            GcRef::new(JsObject::new(Value::null(), ncx.memory_manager().clone()));
                        match kind.as_str() {
                            "key" => {
                                let _ = result.set(pk("value"), key);
                            }
                            "entry" => {
                                let entry =
                                    GcRef::new(JsObject::array(2, ncx.memory_manager().clone()));
                                let _ = entry.set(PropertyKey::Index(0), key);
                                let _ = entry.set(PropertyKey::Index(1), value);
                                let _ = result.set(pk("value"), Value::array(entry));
                            }
                            _ => {
                                // "value"
                                let _ = result.set(pk("value"), value);
                            }
                        }
                        let _ = result.set(pk("done"), Value::boolean(false));
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
                let _ = data.set(MapKey(normalized_key), value);
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
                make_map_iterator(
                    this_val,
                    "key",
                    ncx.memory_manager().clone(),
                    fn_proto_for_keys,
                    iter_proto_for_keys,
                )
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
                make_map_iterator(
                    this_val,
                    "value",
                    ncx.memory_manager().clone(),
                    fn_proto_for_values,
                    iter_proto_for_values,
                )
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
                make_map_iterator(
                    this_val,
                    "entry",
                    ncx.memory_manager().clone(),
                    fn_proto_for_entries,
                    iter_proto_for_entries,
                )
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
                    return Err(VmError::type_error(
                        "Map.prototype.forEach: callback is not a function",
                    ));
                }

                // Live iteration: check entries_len each round to see new entries
                let mut pos = 0;
                loop {
                    let len = data.entries_len();
                    if pos >= len {
                        break;
                    }
                    if let Some((key, value)) = data.entry_at(pos) {
                        ncx.call_function(
                            &callback,
                            this_arg.clone(),
                            &[value, key, this_val.clone()],
                        )?;
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
                    return Err(VmError::type_error(
                        "Map.prototype.getOrInsertComputed: callbackfn is not a function",
                    ));
                }
                // Check if key exists first
                let mk = MapKey(key.clone());
                if let Some(existing) = data.get(&mk) {
                    return Ok(existing);
                }
                // Key not found: Call(callbackfn, undefined, « key »)
                let value = ncx.call_function(&callback, Value::undefined(), &[key.clone()])?;
                let _ = data.set(MapKey(key), value.clone());
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
                make_map_iterator(
                    this_val,
                    "entry",
                    ncx.memory_manager().clone(),
                    fn_proto_for_symbol,
                    iter_proto_for_symbol,
                )
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
    let _ = iter.set(pk("__set_data_ref__"), Value::set_data(data));
    let _ = iter.set(pk("__iter_index__"), Value::number(0.0));
    let _ = iter.set(pk("__iter_kind__"), Value::string(JsString::intern(kind)));

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
                        let result =
                            GcRef::new(JsObject::new(Value::null(), ncx.memory_manager().clone()));
                        let _ = result.set(pk("value"), Value::undefined());
                        let _ = result.set(pk("done"), Value::boolean(true));
                        let _ = iter_obj.set(pk("__iter_index__"), Value::number(idx as f64));
                        return Ok(Value::object(result));
                    }

                    if let Some(value) = data.entry_at(idx) {
                        idx += 1;
                        let _ = iter_obj.set(pk("__iter_index__"), Value::number(idx as f64));

                        let result =
                            GcRef::new(JsObject::new(Value::null(), ncx.memory_manager().clone()));
                        match kind.as_str() {
                            "entry" => {
                                let entry =
                                    GcRef::new(JsObject::array(2, ncx.memory_manager().clone()));
                                let _ = entry.set(PropertyKey::Index(0), value.clone());
                                let _ = entry.set(PropertyKey::Index(1), value);
                                let _ = result.set(pk("value"), Value::array(entry));
                            }
                            _ => {
                                // "value" or "key" (both same for Sets)
                                let _ = result.set(pk("value"), value);
                            }
                        }
                        let _ = result.set(pk("done"), Value::boolean(false));
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
                make_set_iterator(
                    this_val,
                    "value",
                    ncx.memory_manager().clone(),
                    fn_proto_for_values,
                    iter_proto_for_values,
                )
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
                make_set_iterator(
                    this_val,
                    "value",
                    ncx.memory_manager().clone(),
                    fn_proto_for_keys,
                    iter_proto_for_keys,
                )
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
                make_set_iterator(
                    this_val,
                    "entry",
                    ncx.memory_manager().clone(),
                    fn_proto_for_entries,
                    iter_proto_for_entries,
                )
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
                    return Err(VmError::type_error(
                        "Set.prototype.forEach: callback is not a function",
                    ));
                }

                // Live iteration: check entries_len each round to see new entries
                let mut pos = 0;
                loop {
                    let len = data.entries_len();
                    if pos >= len {
                        break;
                    }
                    if let Some(value) = data.entry_at(pos) {
                        ncx.call_function(
                            &callback,
                            this_arg.clone(),
                            &[value.clone(), value, this_val.clone()],
                        )?;
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
                let other = args
                    .first()
                    .ok_or_else(|| VmError::type_error("Set.prototype.union requires argument"))?;
                let other_obj = other.as_object().ok_or_else(|| {
                    VmError::type_error("Set.prototype.union requires a Set-like argument")
                })?;
                if !is_set(&other_obj) {
                    return Err(VmError::type_error(
                        "Set.prototype.union requires a Set-like argument",
                    ));
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
                let other = args.first().ok_or_else(|| {
                    VmError::type_error("Set.prototype.intersection requires argument")
                })?;
                let other_obj = other.as_object().ok_or_else(|| {
                    VmError::type_error("Set.prototype.intersection requires a Set-like argument")
                })?;
                if !is_set(&other_obj) {
                    return Err(VmError::type_error(
                        "Set.prototype.intersection requires a Set-like argument",
                    ));
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
                let other = args.first().ok_or_else(|| {
                    VmError::type_error("Set.prototype.difference requires argument")
                })?;
                let other_obj = other.as_object().ok_or_else(|| {
                    VmError::type_error("Set.prototype.difference requires a Set-like argument")
                })?;
                if !is_set(&other_obj) {
                    return Err(VmError::type_error(
                        "Set.prototype.difference requires a Set-like argument",
                    ));
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
                let other = args.first().ok_or_else(|| {
                    VmError::type_error("Set.prototype.symmetricDifference requires argument")
                })?;
                let other_obj = other.as_object().ok_or_else(|| {
                    VmError::type_error(
                        "Set.prototype.symmetricDifference requires a Set-like argument",
                    )
                })?;
                if !is_set(&other_obj) {
                    return Err(VmError::type_error(
                        "Set.prototype.symmetricDifference requires a Set-like argument",
                    ));
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
                let other = args.first().ok_or_else(|| {
                    VmError::type_error("Set.prototype.isSubsetOf requires argument")
                })?;
                let other_obj = other.as_object().ok_or_else(|| {
                    VmError::type_error("Set.prototype.isSubsetOf requires a Set-like argument")
                })?;
                if !is_set(&other_obj) {
                    return Err(VmError::type_error(
                        "Set.prototype.isSubsetOf requires a Set-like argument",
                    ));
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
                let other = args.first().ok_or_else(|| {
                    VmError::type_error("Set.prototype.isSupersetOf requires argument")
                })?;
                let other_obj = other.as_object().ok_or_else(|| {
                    VmError::type_error("Set.prototype.isSupersetOf requires a Set-like argument")
                })?;
                if !is_set(&other_obj) {
                    return Err(VmError::type_error(
                        "Set.prototype.isSupersetOf requires a Set-like argument",
                    ));
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
                let other = args.first().ok_or_else(|| {
                    VmError::type_error("Set.prototype.isDisjointFrom requires argument")
                })?;
                let other_obj = other.as_object().ok_or_else(|| {
                    VmError::type_error("Set.prototype.isDisjointFrom requires a Set-like argument")
                })?;
                if !is_set(&other_obj) {
                    return Err(VmError::type_error(
                        "Set.prototype.isDisjointFrom requires a Set-like argument",
                    ));
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
                make_set_iterator(
                    this_val,
                    "value",
                    ncx.memory_manager().clone(),
                    fn_proto_for_symbol,
                    iter_proto_for_symbol,
                )
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

/// WeakMap uses EphemeronTable for proper weak semantics with GC integration.
/// Keys are tracked by pointer identity and entries are automatically collected
/// when keys become unreachable.
fn get_weakmap_entries(obj: &GcRef<JsObject>) -> Option<GcRef<otter_vm_gc::EphemeronTable>> {
    obj.get(&pk(WEAKMAP_ENTRIES_KEY))
        .and_then(|v| v.as_ephemeron_table())
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
            |this_val, args, ncx| {
                let obj = this_val.as_object().ok_or_else(|| {
                    VmError::type_error(
                        "Method WeakMap.prototype.get called on incompatible receiver",
                    )
                })?;
                if !is_weakmap(&obj) {
                    return Err(VmError::type_error(
                        "Method WeakMap.prototype.get called on incompatible receiver",
                    ));
                }
                let key = args.first().cloned().unwrap_or(Value::undefined());
                // Return undefined if key cannot be held weakly (spec step 4)
                if !is_valid_weak_key(&key, ncx.ctx.symbol_registry()) {
                    return Ok(Value::undefined());
                }
                let Some(key_header) = weak_key_header(&key) else {
                    return Ok(Value::undefined());
                };
                let entries = get_weakmap_entries(&obj).ok_or("Internal error: missing entries")?;

                unsafe {
                    if let Some(value_bytes) = entries.get_raw(key_header) {
                        Ok(bytes_to_value(&value_bytes))
                    } else {
                        Ok(Value::undefined())
                    }
                }
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
            |this_val, args, ncx| {
                let obj = this_val.as_object().ok_or_else(|| {
                    VmError::type_error(
                        "Method WeakMap.prototype.set called on incompatible receiver",
                    )
                })?;
                if !is_weakmap(&obj) {
                    return Err(VmError::type_error(
                        "Method WeakMap.prototype.set called on incompatible receiver",
                    ));
                }
                let key = args.first().cloned().unwrap_or(Value::undefined());
                if !is_valid_weak_key(&key, ncx.ctx.symbol_registry()) {
                    return Err(VmError::type_error("Invalid value used as weak map key"));
                }
                let value = args.get(1).cloned().unwrap_or(Value::undefined());
                let key_header =
                    weak_key_header(&key).ok_or_else(|| VmError::type_error("Invalid weak key"))?;
                let entries = get_weakmap_entries(&obj).ok_or("Internal error: missing entries")?;

                let value_bytes = value_to_bytes(&value);
                unsafe {
                    entries.set_raw(key_header, value_bytes, None);
                }
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
            |this_val, args, ncx| {
                let obj = this_val.as_object().ok_or_else(|| {
                    VmError::type_error(
                        "Method WeakMap.prototype.has called on incompatible receiver",
                    )
                })?;
                if !is_weakmap(&obj) {
                    return Err(VmError::type_error(
                        "Method WeakMap.prototype.has called on incompatible receiver",
                    ));
                }
                let key = args.first().cloned().unwrap_or(Value::undefined());
                if !is_valid_weak_key(&key, ncx.ctx.symbol_registry()) {
                    return Ok(Value::boolean(false));
                }
                let key_header = match weak_key_header(&key) {
                    Some(h) => h,
                    None => return Ok(Value::boolean(false)),
                };
                let entries = get_weakmap_entries(&obj).ok_or("Internal error: missing entries")?;

                unsafe { Ok(Value::boolean(entries.has(key_header))) }
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
            |this_val, args, ncx| {
                let obj = this_val.as_object().ok_or_else(|| {
                    VmError::type_error(
                        "Method WeakMap.prototype.delete called on incompatible receiver",
                    )
                })?;
                if !is_weakmap(&obj) {
                    return Err(VmError::type_error(
                        "Method WeakMap.prototype.delete called on incompatible receiver",
                    ));
                }
                let key = args.first().cloned().unwrap_or(Value::undefined());
                if !is_valid_weak_key(&key, ncx.ctx.symbol_registry()) {
                    return Ok(Value::boolean(false));
                }
                let key_header = match weak_key_header(&key) {
                    Some(h) => h,
                    None => return Ok(Value::boolean(false)),
                };
                let entries = get_weakmap_entries(&obj).ok_or("Internal error: missing entries")?;

                unsafe { Ok(Value::boolean(entries.delete(key_header))) }
            },
            mm.clone(),
            fn_proto,
        )),
    );

    // WeakMap.prototype.getOrInsert(key, value)
    wm_proto.define_property(
        PropertyKey::string("getOrInsert"),
        PropertyDescriptor::builtin_method(make_builtin(
            "getOrInsert",
            2,
            |this_val, args, ncx| {
                let obj = this_val.as_object().ok_or_else(|| {
                    VmError::type_error(
                        "Method WeakMap.prototype.getOrInsert called on incompatible receiver",
                    )
                })?;
                if !is_weakmap(&obj) {
                    return Err(VmError::type_error(
                        "Method WeakMap.prototype.getOrInsert called on incompatible receiver",
                    ));
                }
                let key = args.first().cloned().unwrap_or(Value::undefined());
                if !is_valid_weak_key(&key, ncx.ctx.symbol_registry()) {
                    return Err(VmError::type_error("Invalid value used as weak map key"));
                }
                let default_value = args.get(1).cloned().unwrap_or(Value::undefined());
                let Some(key_header) = weak_key_header(&key) else {
                    return Err(VmError::type_error("Invalid weak key"));
                };
                let entries = get_weakmap_entries(&obj).ok_or("Internal error: missing entries")?;

                unsafe {
                    // If key exists, return existing value
                    if let Some(value_bytes) = entries.get_raw(key_header) {
                        return Ok(bytes_to_value(&value_bytes));
                    }
                    // Otherwise insert and return default_value
                    let value_bytes = value_to_bytes(&default_value);
                    entries.set_raw(key_header, value_bytes, None);
                    Ok(default_value)
                }
            },
            mm.clone(),
            fn_proto,
        )),
    );

    // WeakMap.prototype.getOrInsertComputed(key, callbackfn)
    wm_proto.define_property(
        PropertyKey::string("getOrInsertComputed"),
        PropertyDescriptor::builtin_method(make_builtin(
            "getOrInsertComputed",
            2,
            |this_val, args, ncx| {
                let obj = this_val
                    .as_object()
                    .ok_or_else(|| VmError::type_error("Method WeakMap.prototype.getOrInsertComputed called on incompatible receiver"))?;
                if !is_weakmap(&obj) {
                    return Err(VmError::type_error("Method WeakMap.prototype.getOrInsertComputed called on incompatible receiver"));
                }
                let key = args.first().cloned().unwrap_or(Value::undefined());
                if !is_valid_weak_key(&key, ncx.ctx.symbol_registry()) {
                    return Err(VmError::type_error("Invalid value used as weak map key"));
                }
                let callback = args.get(1).cloned().unwrap_or(Value::undefined());
                if !callback.is_callable() {
                    return Err(VmError::type_error("WeakMap.prototype.getOrInsertComputed: callback is not a function"));
                }
                let Some(key_header) = weak_key_header(&key) else {
                    return Err(VmError::type_error("Invalid weak key"));
                };
                let entries = get_weakmap_entries(&obj).ok_or("Internal error: missing entries")?;

                unsafe {
                    // If key exists, return existing value
                    if let Some(value_bytes) = entries.get_raw(key_header) {
                        return Ok(bytes_to_value(&value_bytes));
                    }
                    // Otherwise compute, insert, and return
                    let computed = ncx.call_function(&callback, Value::undefined(), &[key.clone()])?;
                    // Re-get entries in case callback triggered GC
                    let entries = get_weakmap_entries(&obj).ok_or("Internal error: missing entries")?;
                    let Some(key_header) = weak_key_header(&key) else {
                        return Err(VmError::type_error("Invalid weak key"));
                    };
                    let value_bytes = value_to_bytes(&computed);
                    entries.set_raw(key_header, value_bytes, None);
                    Ok(computed)
                }
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

fn get_weakset_entries(obj: &GcRef<JsObject>) -> Option<GcRef<otter_vm_gc::EphemeronTable>> {
    obj.get(&pk(WEAKSET_ENTRIES_KEY))
        .and_then(|v| v.as_ephemeron_table())
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
            |this_val, args, ncx| {
                let obj = this_val.as_object().ok_or_else(|| {
                    VmError::type_error(
                        "Method WeakSet.prototype.add called on incompatible receiver",
                    )
                })?;
                if !is_weakset(&obj) {
                    return Err(VmError::type_error(
                        "Method WeakSet.prototype.add called on incompatible receiver",
                    ));
                }
                let value = args.first().cloned().unwrap_or(Value::undefined());
                if !is_valid_weak_key(&value, ncx.ctx.symbol_registry()) {
                    return Err(VmError::type_error("Invalid value used in weak set"));
                }
                let key_header = weak_key_header(&value)
                    .ok_or_else(|| VmError::type_error("Invalid weak key"))?;
                let entries = get_weakset_entries(&obj).ok_or("Internal error: missing entries")?;

                // For WeakSet, we just store a boolean true marker
                let marker = Value::boolean(true);
                let value_bytes = value_to_bytes(&marker);
                unsafe {
                    entries.set_raw(key_header, value_bytes, None);
                }
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
            |this_val, args, ncx| {
                let obj = this_val.as_object().ok_or_else(|| {
                    VmError::type_error(
                        "Method WeakSet.prototype.has called on incompatible receiver",
                    )
                })?;
                if !is_weakset(&obj) {
                    return Err(VmError::type_error(
                        "Method WeakSet.prototype.has called on incompatible receiver",
                    ));
                }
                let value = args.first().cloned().unwrap_or(Value::undefined());
                if !is_valid_weak_key(&value, ncx.ctx.symbol_registry()) {
                    return Ok(Value::boolean(false));
                }
                let key_header = match weak_key_header(&value) {
                    Some(h) => h,
                    None => return Ok(Value::boolean(false)),
                };
                let entries = get_weakset_entries(&obj).ok_or("Internal error: missing entries")?;

                unsafe { Ok(Value::boolean(entries.has(key_header))) }
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
            |this_val, args, ncx| {
                let obj = this_val.as_object().ok_or_else(|| {
                    VmError::type_error(
                        "Method WeakSet.prototype.delete called on incompatible receiver",
                    )
                })?;
                if !is_weakset(&obj) {
                    return Err(VmError::type_error(
                        "Method WeakSet.prototype.delete called on incompatible receiver",
                    ));
                }
                let value = args.first().cloned().unwrap_or(Value::undefined());
                if !is_valid_weak_key(&value, ncx.ctx.symbol_registry()) {
                    return Ok(Value::boolean(false));
                }
                let key_header = match weak_key_header(&value) {
                    Some(h) => h,
                    None => return Ok(Value::boolean(false)),
                };
                let entries = get_weakset_entries(&obj).ok_or("Internal error: missing entries")?;

                unsafe { Ok(Value::boolean(entries.delete(key_header))) }
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
                    let len = arr
                        .get(&PropertyKey::string("length"))
                        .and_then(|v| v.as_number())
                        .unwrap_or(0.0) as usize;
                    for i in 0..len {
                        if let Some(entry) = arr.get(&PropertyKey::Index(i as u32))
                            && let Some(entry_obj) = entry.as_array().or_else(|| entry.as_object())
                        {
                            let key = entry_obj
                                .get(&PropertyKey::Index(0))
                                .unwrap_or(Value::undefined());
                            let value = entry_obj
                                .get(&PropertyKey::Index(1))
                                .unwrap_or(Value::undefined());
                            let _ = data.set(MapKey(key), value);
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
                    let len = arr
                        .get(&PropertyKey::string("length"))
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
/// Supports `new WeakMap()` and `new WeakMap(iterable)`.
pub fn create_weak_map_constructor() -> Box<
    dyn Fn(&Value, &[Value], &mut crate::context::NativeContext<'_>) -> Result<Value, VmError>
        + Send
        + Sync,
> {
    Box::new(|this_val, args, ncx| {
        if this_val.is_undefined() {
            return Err(VmError::type_error("Constructor WeakMap requires 'new'"));
        }
        if let Some(obj) = this_val.as_object() {
            let ephemeron_table = GcRef::new(otter_vm_gc::EphemeronTable::new());
            let _ = obj.set(
                pk(WEAKMAP_ENTRIES_KEY),
                Value::ephemeron_table(ephemeron_table),
            );
            let _ = obj.set(pk(IS_WEAKMAP_KEY), Value::boolean(true));

            // Handle iterable argument: new WeakMap([[k1, v1], [k2, v2], ...])
            let iterable = args.first().cloned().unwrap_or(Value::undefined());
            if !iterable.is_undefined() && !iterable.is_null() {
                // Get the "set" adder method via JS Get (triggers accessor getters) (spec step 5)
                let adder = js_get_value(&obj, &pk("set"), this_val, ncx)?;

                // Check if adder is callable (spec step 6)
                if !adder.is_callable() {
                    return Err(VmError::type_error("WeakMap.prototype.set is not callable"));
                }

                // Use proper iterator protocol (spec step 7: AddEntriesFromIterable)
                let this_clone = this_val.clone();
                iterate_with_protocol(&iterable, ncx, |entry, ncx| {
                    // Each entry must be an object [key, value] (spec step 9.f)
                    let entry_obj =
                        entry
                            .as_array()
                            .or_else(|| entry.as_object())
                            .ok_or_else(|| {
                                VmError::type_error("Iterator value is not an entry object")
                            })?;

                    // Use js_get_value to trigger accessor getters (spec: Get(nextItem, "0") / Get(nextItem, "1"))
                    let entry_val = entry.clone();
                    let key = js_get_value(&entry_obj, &PropertyKey::Index(0), &entry_val, ncx)?;
                    let value = js_get_value(&entry_obj, &PropertyKey::Index(1), &entry_val, ncx)?;

                    // Call adder (this.set(key, value))
                    ncx.call_function(&adder, this_clone.clone(), &[key, value])?;
                    Ok(())
                })?;
            }

            Ok(this_val.clone())
        } else {
            Err(VmError::type_error("Constructor WeakMap requires 'new'"))
        }
    })
}

/// Create WeakSet constructor function.
/// Supports `new WeakSet()` and `new WeakSet(iterable)`.
pub fn create_weak_set_constructor() -> Box<
    dyn Fn(&Value, &[Value], &mut crate::context::NativeContext<'_>) -> Result<Value, VmError>
        + Send
        + Sync,
> {
    Box::new(|this_val, args, ncx| {
        if this_val.is_undefined() {
            return Err(VmError::type_error("Constructor WeakSet requires 'new'"));
        }
        if let Some(obj) = this_val.as_object() {
            let ephemeron_table = GcRef::new(otter_vm_gc::EphemeronTable::new());
            let _ = obj.set(
                pk(WEAKSET_ENTRIES_KEY),
                Value::ephemeron_table(ephemeron_table),
            );
            let _ = obj.set(pk(IS_WEAKSET_KEY), Value::boolean(true));

            // Handle iterable argument: new WeakSet([v1, v2, ...])
            let iterable = args.first().cloned().unwrap_or(Value::undefined());
            if !iterable.is_undefined() && !iterable.is_null() {
                // Get the "add" adder method via JS Get (triggers accessor getters) (spec step 5)
                let adder = js_get_value(&obj, &pk("add"), this_val, ncx)?;

                // Check if adder is callable (spec step 6)
                if !adder.is_callable() {
                    return Err(VmError::type_error("WeakSet.prototype.add is not callable"));
                }

                // Use proper iterator protocol
                let this_clone = this_val.clone();
                iterate_with_protocol(&iterable, ncx, |value, ncx| {
                    // Call adder (this.add(value))
                    ncx.call_function(&adder, this_clone.clone(), &[value])?;
                    Ok(())
                })?;
            }

            Ok(this_val.clone())
        } else {
            Err(VmError::type_error("Constructor WeakSet requires 'new'"))
        }
    })
}

/// Install static methods on the Map constructor (e.g. Map.groupBy).
pub fn install_map_statics(
    map_ctor: GcRef<JsObject>,
    fn_proto: GcRef<JsObject>,
    mm: &Arc<MemoryManager>,
) {
    // Map.groupBy ( items, callbackfn ) — ES2024
    map_ctor.define_property(
        pk("groupBy"),
        PropertyDescriptor::builtin_method(make_builtin(
            "groupBy",
            2,
            |_this, args, ncx| {
                let items = args.first().cloned().unwrap_or(Value::undefined());
                let callback = args.get(1).cloned().unwrap_or(Value::undefined());

                if !callback.is_callable() {
                    return Err(VmError::type_error(
                        "Map.groupBy: callbackfn is not a function",
                    ));
                }

                // Create a new Map
                let mm = ncx.ctx.memory_manager().clone();
                let map_obj = GcRef::new(JsObject::new(Value::null(), mm.clone()));
                // Set Map prototype
                if let Some(map_proto) = ncx
                    .ctx
                    .get_global("Map")
                    .and_then(|v| v.as_object())
                    .and_then(|c| c.get(&pk("prototype")))
                    .and_then(|v| v.as_object())
                {
                    map_obj.set_prototype(Value::object(map_proto));
                }
                init_map_slots(&map_obj);
                let map_data = get_map_data(&map_obj).unwrap();

                // Iterate items
                let mut k: u32 = 0;
                iterate_with_protocol(&items, ncx, |value, ncx| {
                    let key =
                        ncx.call_function(&callback, Value::undefined(), &[value.clone(), Value::number(k as f64)])?;
                    k += 1;

                    // If key already exists in map, push to existing array; else create new array
                    let map_key = MapKey(key.clone());
                    if let Some(existing) = map_data.get(&map_key) {
                        // Push to existing array
                        if let Some(arr) = existing.as_array().or_else(|| existing.as_object()) {
                            let len = arr
                                .get(&pk("length"))
                                .and_then(|v| v.as_number())
                                .unwrap_or(0.0) as u32;
                            let _ = arr.set(PropertyKey::Index(len), value);
                            let _ = arr.set(pk("length"), Value::number((len + 1) as f64));
                        }
                    } else {
                        // Create new array with this value
                        let arr = JsObject::array(4, mm.clone());
                        // Set Array prototype
                        if let Some(array_proto) = ncx
                            .ctx
                            .get_global("Array")
                            .and_then(|v| v.as_object())
                            .and_then(|c| c.get(&pk("prototype")))
                            .and_then(|v| v.as_object())
                        {
                            arr.set_prototype(Value::object(array_proto));
                        }
                        let _ = arr.set(PropertyKey::Index(0), value);
                        let _ = arr.set(pk("length"), Value::number(1.0));
                        let _ = map_data.set(map_key, Value::array(GcRef::new(arr)));
                    }
                    Ok(())
                })?;

                Ok(Value::object(map_obj))
            },
            mm.clone(),
            fn_proto,
        )),
    );
}
