//! Map, Set, WeakMap, and WeakSet constructor and prototype implementations
//!
//! Complete ES2026 implementation:
//! - Map: constructor + 10 prototype methods + Symbol.toStringTag
//! - Set: constructor + 10 prototype methods + 7 ES2025 set methods + Symbol.toStringTag
//! - WeakMap: constructor + 4 prototype methods + Symbol.toStringTag
//! - WeakSet: constructor + 3 prototype methods + Symbol.toStringTag
//!
//! All methods use inline implementations for optimal performance.
//! Internal storage uses the same pattern as `otter-vm-builtins/src/map.rs` and `set.rs`.

use crate::error::VmError;
use crate::gc::GcRef;
use crate::memory::MemoryManager;
use crate::object::{JsObject, PropertyAttributes, PropertyDescriptor, PropertyKey};
use crate::string::JsString;
use crate::value::Value;
use std::sync::Arc;

// ============================================================================
// Internal storage keys (must match otter-vm-builtins/src/map.rs and set.rs)
// ============================================================================
const MAP_ENTRIES_KEY: &str = "__map_entries__";
const MAP_SIZE_KEY: &str = "__map_size__";
const IS_MAP_KEY: &str = "__is_map__";
const IS_WEAKMAP_KEY: &str = "__is_weakmap__";

const SET_VALUES_KEY: &str = "__set_values__";
const SET_SIZE_KEY: &str = "__set_size__";
const IS_SET_KEY: &str = "__is_set__";
const IS_WEAKSET_KEY: &str = "__is_weakset__";

// ============================================================================
// Helpers
// ============================================================================

/// Compute a hash key string for a value (for string-keyed internal storage).
/// Must match `otter-vm-builtins/src/map.rs::value_to_key`.
fn value_to_key(value: &Value) -> String {
    if value.is_undefined() {
        return "__undefined__".to_string();
    }
    if value.is_null() {
        return "__null__".to_string();
    }
    if let Some(b) = value.as_boolean() {
        return format!("__bool_{}__", b);
    }
    if let Some(n) = value.as_number() {
        if n.is_nan() {
            return "__nan__".to_string();
        }
        return format!("__num_{}__", n);
    }
    if let Some(s) = value.as_string() {
        return format!("__str_{}__", s.as_str());
    }
    if let Some(sym) = value.as_symbol() {
        return format!("__sym_{}__", sym.id);
    }
    if let Some(obj) = value.as_object() {
        return format!("__obj_{:p}__", obj.as_ptr());
    }
    if let Some(func) = value.as_function() {
        return format!("__func_{:p}__", Arc::as_ptr(func));
    }
    format!("__unknown_{:?}__", value)
}

/// Convert string to PropertyKey (intern for fast lookups).
fn pk(s: &str) -> PropertyKey {
    PropertyKey::String(JsString::intern(s))
}

/// Verify an object is a Map (has `__is_map__` = true).
fn is_map(obj: &GcRef<JsObject>) -> bool {
    obj.get(&pk(IS_MAP_KEY))
        .and_then(|v| v.as_boolean())
        == Some(true)
}

/// Verify an object is a Set (has `__is_set__` = true).
fn is_set(obj: &GcRef<JsObject>) -> bool {
    obj.get(&pk(IS_SET_KEY))
        .and_then(|v| v.as_boolean())
        == Some(true)
}

/// Verify an object is a WeakMap.
fn is_weakmap(obj: &GcRef<JsObject>) -> bool {
    obj.get(&pk(IS_WEAKMAP_KEY))
        .and_then(|v| v.as_boolean())
        == Some(true)
}

/// Verify an object is a WeakSet.
fn is_weakset(obj: &GcRef<JsObject>) -> bool {
    obj.get(&pk(IS_WEAKSET_KEY))
        .and_then(|v| v.as_boolean())
        == Some(true)
}

/// Get the entries object from a Map/WeakMap.
fn get_entries(obj: &GcRef<JsObject>) -> Option<GcRef<JsObject>> {
    obj.get(&pk(MAP_ENTRIES_KEY))
        .and_then(|v| v.as_object())
}

/// Get the values object from a Set/WeakSet.
fn get_set_values_obj(obj: &GcRef<JsObject>) -> Option<GcRef<JsObject>> {
    obj.get(&pk(SET_VALUES_KEY))
        .and_then(|v| v.as_object())
}

/// Get current size from a Map or Set.
fn get_size(obj: &GcRef<JsObject>, key: &str) -> i32 {
    obj.get(&pk(key))
        .and_then(|v| v.as_int32())
        .unwrap_or(0)
}

/// Check if a value is valid as a WeakMap/WeakSet key.
fn is_valid_weak_key(value: &Value) -> bool {
    value.is_object() || value.is_symbol() || value.is_function()
}

/// Initialize Map internal slots on an object.
fn init_map_slots(obj: &GcRef<JsObject>, mm: &Arc<MemoryManager>) {
    let entries = GcRef::new(JsObject::new(None, mm.clone()));
    obj.set(pk(MAP_ENTRIES_KEY), Value::object(entries));
    obj.set(pk(MAP_SIZE_KEY), Value::int32(0));
    obj.set(pk(IS_MAP_KEY), Value::boolean(true));
}

/// Initialize Set internal slots on an object.
fn init_set_slots(obj: &GcRef<JsObject>, mm: &Arc<MemoryManager>) {
    let values = GcRef::new(JsObject::new(None, mm.clone()));
    obj.set(pk(SET_VALUES_KEY), Value::object(values));
    obj.set(pk(SET_SIZE_KEY), Value::int32(0));
    obj.set(pk(IS_SET_KEY), Value::boolean(true));
}

// ============================================================================
// Map.prototype
// ============================================================================

// ============================================================================
// Map Iterator
// ============================================================================

/// Create a Map iterator object following the array iterator pattern.
/// Snapshots keys at creation for stable iteration.
fn make_map_iterator(
    this_val: &Value,
    kind: &str,
    mm: Arc<MemoryManager>,
    fn_proto: GcRef<JsObject>,
    iter_proto: GcRef<JsObject>,
) -> Result<Value, VmError> {
    let obj = this_val
        .as_object()
        .ok_or_else(|| VmError::type_error("Map iterator: this is not an object"))?;
    if !is_map(&obj) {
        return Err(VmError::type_error("Map iterator called on incompatible receiver"));
    }

    // Create iterator object with %IteratorPrototype% as prototype
    let iter = GcRef::new(JsObject::new(Some(iter_proto), mm.clone()));

    // Store the map reference, snapshot keys, current index, and kind
    iter.set(PropertyKey::string("__map_ref__"), Value::object(obj));
    iter.set(PropertyKey::string("__iter_index__"), Value::number(0.0));
    iter.set(
        PropertyKey::string("__iter_kind__"),
        Value::string(JsString::intern(kind)),
    );

    // Snapshot keys at iterator creation (for stable iteration)
    let entries = get_entries(&obj).ok_or("Internal error: missing entries")?;
    let keys_snapshot = GcRef::new(JsObject::array(0, mm.clone()));
    let props = entries.own_keys();
    let mut index = 0u32;
    for prop in props {
        if let Some(entry) = entries.get(&prop) {
            if let Some(entry_obj) = entry.as_object() {
                let key = entry_obj.get(&pk("k")).unwrap_or(Value::undefined());
                keys_snapshot.set(PropertyKey::Index(index), key);
                index += 1;
            }
        }
    }
    keys_snapshot.set(pk("length"), Value::int32(index as i32));
    iter.set(PropertyKey::string("__iter_keys__"), Value::array(keys_snapshot));

    // Define next() method
    let fn_proto_for_next = fn_proto;
    iter.define_property(
        PropertyKey::string("next"),
        PropertyDescriptor::builtin_method(Value::native_function_with_proto(
            |this_val, _args, ncx| {
                let iter_obj = this_val
                    .as_object()
                    .ok_or_else(|| "not an iterator object".to_string())?;
                let map = iter_obj
                    .get(&PropertyKey::string("__map_ref__"))
                    .and_then(|v| v.as_object())
                    .ok_or_else(|| "iterator: missing map ref".to_string())?;
                let keys_snapshot = iter_obj
                    .get(&PropertyKey::string("__iter_keys__"))
                    .and_then(|v| v.as_object())
                    .ok_or_else(|| "iterator: missing keys snapshot".to_string())?;
                let len = keys_snapshot
                    .get(&PropertyKey::string("length"))
                    .and_then(|v| v.as_number())
                    .unwrap_or(0.0) as usize;
                let kind = iter_obj
                    .get(&PropertyKey::string("__iter_kind__"))
                    .and_then(|v| v.as_string().map(|s| s.as_str().to_string()))
                    .unwrap_or_else(|| "entry".to_string());

                // Loop to skip deleted entries
                loop {
                    let idx = iter_obj
                        .get(&PropertyKey::string("__iter_index__"))
                        .and_then(|v| v.as_number())
                        .unwrap_or(0.0) as usize;

                    if idx >= len {
                        // Done
                        let result = GcRef::new(JsObject::new(None, ncx.memory_manager().clone()));
                        result.set(PropertyKey::string("value"), Value::undefined());
                        result.set(PropertyKey::string("done"), Value::boolean(true));
                        return Ok(Value::object(result));
                    }

                    // Get key from snapshot
                    let key = keys_snapshot
                        .get(&PropertyKey::Index(idx as u32))
                        .unwrap_or(Value::undefined());

                    // Advance index
                    iter_obj.set(
                        PropertyKey::string("__iter_index__"),
                        Value::number((idx + 1) as f64),
                    );

                    // Look up current value in live map (handle deleted entries)
                    let entries = get_entries(&map).ok_or("Internal error: missing entries")?;
                    let hash_key = value_to_key(&key);
                    let entry_opt = entries.get(&pk(&hash_key));

                    // If entry was deleted, continue to next iteration
                    if entry_opt.is_none() {
                        continue;
                    }

                    let entry_obj = entry_opt
                        .and_then(|v| v.as_object())
                        .ok_or_else(|| "invalid entry".to_string())?;
                    let value = entry_obj.get(&pk("v")).unwrap_or(Value::undefined());

                    let result = GcRef::new(JsObject::new(None, ncx.memory_manager().clone()));
                    match kind.as_str() {
                        "key" => {
                            result.set(PropertyKey::string("value"), key);
                        }
                        "entry" => {
                            let entry = GcRef::new(JsObject::array(2, ncx.memory_manager().clone()));
                            entry.set(PropertyKey::Index(0), key);
                            entry.set(PropertyKey::Index(1), value);
                            result.set(PropertyKey::string("value"), Value::array(entry));
                        }
                        _ => {
                            // "value"
                            result.set(PropertyKey::string("value"), value);
                        }
                    }
                    result.set(PropertyKey::string("done"), Value::boolean(false));
                    return Ok(Value::object(result));
                }
            },
            mm,
            fn_proto_for_next,
        )),
    );
    Ok(Value::object(iter))
}

/// Initialize Map.prototype with all ES2026 methods.
///
/// # Methods
/// - get, set, has, delete, clear
/// - size (getter-like), keys, values, entries, forEach
/// - Symbol.toStringTag = "Map"
pub fn init_map_prototype(
    map_proto: GcRef<JsObject>,
    fn_proto: GcRef<JsObject>,
    mm: &Arc<MemoryManager>,
    iterator_proto: GcRef<JsObject>,
    symbol_iterator_id: u64,
) {
    // Map.prototype.get(key)
    map_proto.define_property(
        PropertyKey::string("get"),
        PropertyDescriptor::builtin_method(Value::native_function_with_proto(
            |this_val, args, _ncx| {
                let obj = this_val
                    .as_object()
                    .ok_or_else(|| crate::error::VmError::type_error("Method Map.prototype.get called on incompatible receiver"))?;
                if !is_map(&obj) {
                    return Err(crate::error::VmError::type_error("Method Map.prototype.get called on incompatible receiver"));
                }
                let key = args.first().cloned().unwrap_or(Value::undefined());
                let entries = get_entries(&obj).ok_or("Internal error: missing entries")?;
                let hash_key = value_to_key(&key);
                if let Some(entry) = entries.get(&pk(&hash_key)) {
                    if let Some(entry_obj) = entry.as_object() {
                        return Ok(entry_obj.get(&pk("v")).unwrap_or(Value::undefined()));
                    }
                }
                Ok(Value::undefined())
            },
            mm.clone(),
            fn_proto,
        )),
    );

    // Map.prototype.set(key, value)
    map_proto.define_property(
        PropertyKey::string("set"),
        PropertyDescriptor::builtin_method(Value::native_function_with_proto(
            |this_val, args, ncx| {
                let obj = this_val
                    .as_object()
                    .ok_or_else(|| crate::error::VmError::type_error("Method Map.prototype.set called on incompatible receiver"))?;
                if !is_map(&obj) {
                    return Err(crate::error::VmError::type_error("Method Map.prototype.set called on incompatible receiver"));
                }
                let key = args.first().cloned().unwrap_or(Value::undefined());
                let value = args.get(1).cloned().unwrap_or(Value::undefined());
                let entries = get_entries(&obj).ok_or("Internal error: missing entries")?;
                let hash_key = value_to_key(&key);
                let is_new = entries.get(&pk(&hash_key)).is_none();

                // Create entry object with k/v
                let entry = GcRef::new(JsObject::new(None, ncx.memory_manager().clone()));
                entry.set(pk("k"), key);
                entry.set(pk("v"), value);
                entries.set(pk(&hash_key), Value::object(entry));

                if is_new {
                    let size = get_size(&obj, MAP_SIZE_KEY);
                    obj.set(pk(MAP_SIZE_KEY), Value::int32(size + 1));
                }
                Ok(this_val.clone())
            },
            mm.clone(),
            fn_proto,
        )),
    );

    // Map.prototype.has(key)
    map_proto.define_property(
        PropertyKey::string("has"),
        PropertyDescriptor::builtin_method(Value::native_function_with_proto(
            |this_val, args, _ncx| {
                let obj = this_val
                    .as_object()
                    .ok_or_else(|| crate::error::VmError::type_error("Method Map.prototype.has called on incompatible receiver"))?;
                if !is_map(&obj) {
                    return Err(crate::error::VmError::type_error("Method Map.prototype.has called on incompatible receiver"));
                }
                let key = args.first().cloned().unwrap_or(Value::undefined());
                let entries = get_entries(&obj).ok_or("Internal error: missing entries")?;
                let hash_key = value_to_key(&key);
                Ok(Value::boolean(entries.get(&pk(&hash_key)).is_some()))
            },
            mm.clone(),
            fn_proto,
        )),
    );

    // Map.prototype.delete(key)
    map_proto.define_property(
        PropertyKey::string("delete"),
        PropertyDescriptor::builtin_method(Value::native_function_with_proto(
            |this_val, args, _ncx| {
                let obj = this_val
                    .as_object()
                    .ok_or_else(|| crate::error::VmError::type_error("Method Map.prototype.delete called on incompatible receiver"))?;
                if !is_map(&obj) {
                    return Err(crate::error::VmError::type_error("Method Map.prototype.delete called on incompatible receiver"));
                }
                let key = args.first().cloned().unwrap_or(Value::undefined());
                let entries = get_entries(&obj).ok_or("Internal error: missing entries")?;
                let hash_key = value_to_key(&key);
                if entries.get(&pk(&hash_key)).is_none() {
                    return Ok(Value::boolean(false));
                }
                entries.delete(&pk(&hash_key));
                let size = get_size(&obj, MAP_SIZE_KEY);
                if size > 0 {
                    obj.set(pk(MAP_SIZE_KEY), Value::int32(size - 1));
                }
                Ok(Value::boolean(true))
            },
            mm.clone(),
            fn_proto,
        )),
    );

    // Map.prototype.clear()
    map_proto.define_property(
        PropertyKey::string("clear"),
        PropertyDescriptor::builtin_method(Value::native_function_with_proto(
            |this_val, _args, ncx| {
                let obj = this_val
                    .as_object()
                    .ok_or_else(|| crate::error::VmError::type_error("Method Map.prototype.clear called on incompatible receiver"))?;
                if !is_map(&obj) {
                    return Err(crate::error::VmError::type_error("Method Map.prototype.clear called on incompatible receiver"));
                }
                let new_entries = GcRef::new(JsObject::new(None, ncx.memory_manager().clone()));
                obj.set(pk(MAP_ENTRIES_KEY), Value::object(new_entries));
                obj.set(pk(MAP_SIZE_KEY), Value::int32(0));
                Ok(Value::undefined())
            },
            mm.clone(),
            fn_proto,
        )),
    );

    // Map.prototype.size (accessor getter per spec §24.1.3.10)
    map_proto.define_property(
        PropertyKey::string("size"),
        PropertyDescriptor::Accessor {
            get: Some(Value::native_function_with_proto(
                |this_val, _args, _ncx| {
                    let obj = this_val
                        .as_object()
                        .ok_or_else(|| crate::error::VmError::type_error("get Map.prototype.size called on incompatible receiver"))?;
                    if !is_map(&obj) {
                        return Err(crate::error::VmError::type_error("get Map.prototype.size called on incompatible receiver"));
                    }
                    Ok(Value::int32(get_size(&obj, MAP_SIZE_KEY)))
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

    // Map.prototype.keys() - returns iterator
    let iter_proto_for_keys = iterator_proto;
    let mm_for_keys = mm.clone();
    let fn_proto_for_keys = fn_proto;
    map_proto.define_property(
        PropertyKey::string("keys"),
        PropertyDescriptor::builtin_method(Value::native_function_with_proto(
            move |this_val, _args, ncx| {
                make_map_iterator(this_val, "key", ncx.memory_manager().clone(), fn_proto_for_keys, iter_proto_for_keys)
            },
            mm_for_keys,
            fn_proto,
        )),
    );

    // Map.prototype.values() - returns iterator
    let iter_proto_for_values = iterator_proto;
    let mm_for_values = mm.clone();
    let fn_proto_for_values = fn_proto;
    map_proto.define_property(
        PropertyKey::string("values"),
        PropertyDescriptor::builtin_method(Value::native_function_with_proto(
            move |this_val, _args, ncx| {
                make_map_iterator(this_val, "value", ncx.memory_manager().clone(), fn_proto_for_values, iter_proto_for_values)
            },
            mm_for_values,
            fn_proto,
        )),
    );

    // Map.prototype.entries() - returns iterator
    let iter_proto_for_entries = iterator_proto;
    let mm_for_entries = mm.clone();
    let fn_proto_for_entries = fn_proto;
    map_proto.define_property(
        PropertyKey::string("entries"),
        PropertyDescriptor::builtin_method(Value::native_function_with_proto(
            move |this_val, _args, ncx| {
                make_map_iterator(this_val, "entry", ncx.memory_manager().clone(), fn_proto_for_entries, iter_proto_for_entries)
            },
            mm_for_entries,
            fn_proto,
        )),
    );

    // Map.prototype.forEach(callback) — returns entries for JS-side iteration
    map_proto.define_property(
        PropertyKey::string("forEach"),
        PropertyDescriptor::builtin_method(Value::native_function_with_proto(
            |this_val, _args, ncx| {
                let obj = this_val
                    .as_object()
                    .ok_or_else(|| crate::error::VmError::type_error("Method Map.prototype.forEach called on incompatible receiver"))?;
                if !is_map(&obj) {
                    return Err(crate::error::VmError::type_error("Method Map.prototype.forEach called on incompatible receiver"));
                }
                // Return entries array for JS-side iteration (same pattern as builtins)
                let entries = get_entries(&obj).ok_or("Internal error: missing entries")?;
                let entries_array = GcRef::new(JsObject::array(0, ncx.memory_manager().clone()));
                let props = entries.own_keys();
                let mut index = 0i32;
                for prop in props {
                    if let Some(entry) = entries.get(&prop) {
                        if let Some(entry_obj) = entry.as_object() {
                            let key = entry_obj.get(&pk("k")).unwrap_or(Value::undefined());
                            let value = entry_obj.get(&pk("v")).unwrap_or(Value::undefined());
                            let pair = GcRef::new(JsObject::array(0, ncx.memory_manager().clone()));
                            pair.set(pk("0"), key);
                            pair.set(pk("1"), value);
                            pair.set(pk("length"), Value::int32(2));
                            entries_array.set(pk(&index.to_string()), Value::array(pair));
                            index += 1;
                        }
                    }
                }
                entries_array.set(pk("length"), Value::int32(index));
                Ok(Value::array(entries_array))
            },
            mm.clone(),
            fn_proto,
        )),
    );

    // Map.prototype[Symbol.iterator] - same as entries per ES spec
    let iter_proto_for_symbol = iterator_proto;
    let mm_for_symbol = mm.clone();
    let fn_proto_for_symbol = fn_proto;
    map_proto.define_property(
        PropertyKey::Symbol(symbol_iterator_id),
        PropertyDescriptor::builtin_method(Value::native_function_with_proto(
            move |this_val, _args, ncx| {
                make_map_iterator(this_val, "entry", ncx.memory_manager().clone(), fn_proto_for_symbol, iter_proto_for_symbol)
            },
            mm_for_symbol,
            fn_proto,
        )),
    );

    // Map.prototype[Symbol.toStringTag] = "Map"
    map_proto.define_property(
        PropertyKey::Symbol(crate::intrinsics::well_known::TO_STRING_TAG),
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
// Set.prototype
// ============================================================================

// ============================================================================
// Set Iterator
// ============================================================================

/// Create a Set iterator object following the array iterator pattern.
/// Snapshots values at creation for stable iteration.
fn make_set_iterator(
    this_val: &Value,
    kind: &str,
    mm: Arc<MemoryManager>,
    fn_proto: GcRef<JsObject>,
    iter_proto: GcRef<JsObject>,
) -> Result<Value, VmError> {
    let obj = this_val
        .as_object()
        .ok_or_else(|| VmError::type_error("Set iterator: this is not an object"))?;
    if !is_set(&obj) {
        return Err(VmError::type_error("Set iterator called on incompatible receiver"));
    }

    // Create iterator object with %IteratorPrototype% as prototype
    let iter = GcRef::new(JsObject::new(Some(iter_proto), mm.clone()));

    // Store the set reference, snapshot values, current index, and kind
    iter.set(PropertyKey::string("__set_ref__"), Value::object(obj));
    iter.set(PropertyKey::string("__iter_index__"), Value::number(0.0));
    iter.set(
        PropertyKey::string("__iter_kind__"),
        Value::string(JsString::intern(kind)),
    );

    // Snapshot values at iterator creation (for stable iteration)
    let values = get_set_values_obj(&obj).ok_or("Internal error: missing values")?;
    let values_snapshot = GcRef::new(JsObject::array(0, mm.clone()));
    let props = values.own_keys();
    let mut index = 0u32;
    for prop in props {
        if let Some(value) = values.get(&prop) {
            values_snapshot.set(PropertyKey::Index(index), value);
            index += 1;
        }
    }
    values_snapshot.set(pk("length"), Value::int32(index as i32));
    iter.set(PropertyKey::string("__iter_values__"), Value::array(values_snapshot));

    // Define next() method
    let fn_proto_for_next = fn_proto;
    iter.define_property(
        PropertyKey::string("next"),
        PropertyDescriptor::builtin_method(Value::native_function_with_proto(
            |this_val, _args, ncx| {
                let iter_obj = this_val
                    .as_object()
                    .ok_or_else(|| "not an iterator object".to_string())?;
                let set = iter_obj
                    .get(&PropertyKey::string("__set_ref__"))
                    .and_then(|v| v.as_object())
                    .ok_or_else(|| "iterator: missing set ref".to_string())?;
                let values_snapshot = iter_obj
                    .get(&PropertyKey::string("__iter_values__"))
                    .and_then(|v| v.as_object())
                    .ok_or_else(|| "iterator: missing values snapshot".to_string())?;
                let len = values_snapshot
                    .get(&PropertyKey::string("length"))
                    .and_then(|v| v.as_number())
                    .unwrap_or(0.0) as usize;
                let kind = iter_obj
                    .get(&PropertyKey::string("__iter_kind__"))
                    .and_then(|v| v.as_string().map(|s| s.as_str().to_string()))
                    .unwrap_or_else(|| "value".to_string());

                // Loop to skip deleted entries
                loop {
                    let idx = iter_obj
                        .get(&PropertyKey::string("__iter_index__"))
                        .and_then(|v| v.as_number())
                        .unwrap_or(0.0) as usize;

                    if idx >= len {
                        // Done
                        let result = GcRef::new(JsObject::new(None, ncx.memory_manager().clone()));
                        result.set(PropertyKey::string("value"), Value::undefined());
                        result.set(PropertyKey::string("done"), Value::boolean(true));
                        return Ok(Value::object(result));
                    }

                    // Get value from snapshot
                    let value = values_snapshot
                        .get(&PropertyKey::Index(idx as u32))
                        .unwrap_or(Value::undefined());

                    // Advance index
                    iter_obj.set(
                        PropertyKey::string("__iter_index__"),
                        Value::number((idx + 1) as f64),
                    );

                    // Check if value still exists in live set (handle deleted entries)
                    let values_obj = get_set_values_obj(&set).ok_or("Internal error: missing values")?;
                    let hash_key = value_to_key(&value);
                    let still_exists = values_obj.get(&pk(&hash_key)).is_some();

                    // If entry was deleted, continue to next iteration
                    if !still_exists {
                        continue;
                    }

                    let result = GcRef::new(JsObject::new(None, ncx.memory_manager().clone()));
                    match kind.as_str() {
                        "entry" => {
                            // For Sets, entries are [value, value] per ES spec
                            let entry = GcRef::new(JsObject::array(2, ncx.memory_manager().clone()));
                            entry.set(PropertyKey::Index(0), value.clone());
                            entry.set(PropertyKey::Index(1), value);
                            result.set(PropertyKey::string("value"), Value::array(entry));
                        }
                        _ => {
                            // "value" or "key" (both are the same for Sets)
                            result.set(PropertyKey::string("value"), value);
                        }
                    }
                    result.set(PropertyKey::string("done"), Value::boolean(false));
                    return Ok(Value::object(result));
                }
            },
            mm,
            fn_proto_for_next,
        )),
    );
    Ok(Value::object(iter))
}

/// Initialize Set.prototype with all ES2026 + ES2025 methods.
///
/// # Methods
/// - add, has, delete, clear
/// - size (getter-like), values, keys (= values), entries, forEach
/// - ES2025: union, intersection, difference, symmetricDifference,
///   isSubsetOf, isSupersetOf, isDisjointFrom
/// - Symbol.toStringTag = "Set"
pub fn init_set_prototype(
    set_proto: GcRef<JsObject>,
    fn_proto: GcRef<JsObject>,
    mm: &Arc<MemoryManager>,
    iterator_proto: GcRef<JsObject>,
    symbol_iterator_id: u64,
) {
    // Set.prototype.add(value)
    set_proto.define_property(
        PropertyKey::string("add"),
        PropertyDescriptor::builtin_method(Value::native_function_with_proto(
            |this_val, args, _ncx| {
                let obj = this_val
                    .as_object()
                    .ok_or_else(|| crate::error::VmError::type_error("Method Set.prototype.add called on incompatible receiver"))?;
                if !is_set(&obj) {
                    return Err(crate::error::VmError::type_error("Method Set.prototype.add called on incompatible receiver"));
                }
                let value = args.first().cloned().unwrap_or(Value::undefined());
                let values = get_set_values_obj(&obj).ok_or("Internal error: missing values")?;
                let hash_key = value_to_key(&value);
                let is_new = values.get(&pk(&hash_key)).is_none();
                values.set(pk(&hash_key), value);
                if is_new {
                    let size = get_size(&obj, SET_SIZE_KEY);
                    obj.set(pk(SET_SIZE_KEY), Value::int32(size + 1));
                }
                Ok(this_val.clone())
            },
            mm.clone(),
            fn_proto,
        )),
    );

    // Set.prototype.has(value)
    set_proto.define_property(
        PropertyKey::string("has"),
        PropertyDescriptor::builtin_method(Value::native_function_with_proto(
            |this_val, args, _ncx| {
                let obj = this_val
                    .as_object()
                    .ok_or_else(|| crate::error::VmError::type_error("Method Set.prototype.has called on incompatible receiver"))?;
                if !is_set(&obj) {
                    return Err(crate::error::VmError::type_error("Method Set.prototype.has called on incompatible receiver"));
                }
                let value = args.first().cloned().unwrap_or(Value::undefined());
                let values = get_set_values_obj(&obj).ok_or("Internal error: missing values")?;
                let hash_key = value_to_key(&value);
                Ok(Value::boolean(values.get(&pk(&hash_key)).is_some()))
            },
            mm.clone(),
            fn_proto,
        )),
    );

    // Set.prototype.delete(value)
    set_proto.define_property(
        PropertyKey::string("delete"),
        PropertyDescriptor::builtin_method(Value::native_function_with_proto(
            |this_val, args, _ncx| {
                let obj = this_val
                    .as_object()
                    .ok_or_else(|| crate::error::VmError::type_error("Method Set.prototype.delete called on incompatible receiver"))?;
                if !is_set(&obj) {
                    return Err(crate::error::VmError::type_error("Method Set.prototype.delete called on incompatible receiver"));
                }
                let value = args.first().cloned().unwrap_or(Value::undefined());
                let values = get_set_values_obj(&obj).ok_or("Internal error: missing values")?;
                let hash_key = value_to_key(&value);
                if values.get(&pk(&hash_key)).is_none() {
                    return Ok(Value::boolean(false));
                }
                values.delete(&pk(&hash_key));
                let size = get_size(&obj, SET_SIZE_KEY);
                if size > 0 {
                    obj.set(pk(SET_SIZE_KEY), Value::int32(size - 1));
                }
                Ok(Value::boolean(true))
            },
            mm.clone(),
            fn_proto,
        )),
    );

    // Set.prototype.clear()
    set_proto.define_property(
        PropertyKey::string("clear"),
        PropertyDescriptor::builtin_method(Value::native_function_with_proto(
            |this_val, _args, ncx| {
                let obj = this_val
                    .as_object()
                    .ok_or_else(|| crate::error::VmError::type_error("Method Set.prototype.clear called on incompatible receiver"))?;
                if !is_set(&obj) {
                    return Err(crate::error::VmError::type_error("Method Set.prototype.clear called on incompatible receiver"));
                }
                let new_values = GcRef::new(JsObject::new(None, ncx.memory_manager().clone()));
                obj.set(pk(SET_VALUES_KEY), Value::object(new_values));
                obj.set(pk(SET_SIZE_KEY), Value::int32(0));
                Ok(Value::undefined())
            },
            mm.clone(),
            fn_proto,
        )),
    );

    // Set.prototype.size (accessor getter per spec §24.2.3.9)
    set_proto.define_property(
        PropertyKey::string("size"),
        PropertyDescriptor::Accessor {
            get: Some(Value::native_function_with_proto(
                |this_val, _args, _ncx| {
                    let obj = this_val
                        .as_object()
                        .ok_or_else(|| crate::error::VmError::type_error("get Set.prototype.size called on incompatible receiver"))?;
                    if !is_set(&obj) {
                        return Err(crate::error::VmError::type_error("get Set.prototype.size called on incompatible receiver"));
                    }
                    Ok(Value::int32(get_size(&obj, SET_SIZE_KEY)))
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

    // Set.prototype.values() - returns iterator
    let iter_proto_for_values = iterator_proto;
    let mm_for_values = mm.clone();
    let fn_proto_for_values = fn_proto;
    set_proto.define_property(
        PropertyKey::string("values"),
        PropertyDescriptor::builtin_method(Value::native_function_with_proto(
            move |this_val, _args, ncx| {
                make_set_iterator(this_val, "value", ncx.memory_manager().clone(), fn_proto_for_values, iter_proto_for_values)
            },
            mm_for_values,
            fn_proto,
        )),
    );

    // Set.prototype.keys() - same as values() per spec, returns iterator
    let iter_proto_for_keys = iterator_proto;
    let mm_for_keys = mm.clone();
    let fn_proto_for_keys = fn_proto;
    set_proto.define_property(
        PropertyKey::string("keys"),
        PropertyDescriptor::builtin_method(Value::native_function_with_proto(
            move |this_val, _args, ncx| {
                make_set_iterator(this_val, "value", ncx.memory_manager().clone(), fn_proto_for_keys, iter_proto_for_keys)
            },
            mm_for_keys,
            fn_proto,
        )),
    );

    // Set.prototype.entries() - returns iterator
    let iter_proto_for_entries = iterator_proto;
    let mm_for_entries = mm.clone();
    let fn_proto_for_entries = fn_proto;
    set_proto.define_property(
        PropertyKey::string("entries"),
        PropertyDescriptor::builtin_method(Value::native_function_with_proto(
            move |this_val, _args, ncx| {
                make_set_iterator(this_val, "entry", ncx.memory_manager().clone(), fn_proto_for_entries, iter_proto_for_entries)
            },
            mm_for_entries,
            fn_proto,
        )),
    );

    // Set.prototype.forEach(callback) — returns values for JS-side iteration
    set_proto.define_property(
        PropertyKey::string("forEach"),
        PropertyDescriptor::builtin_method(Value::native_function_with_proto(
            |this_val, _args, ncx| {
                let obj = this_val
                    .as_object()
                    .ok_or_else(|| crate::error::VmError::type_error("Method Set.prototype.forEach called on incompatible receiver"))?;
                if !is_set(&obj) {
                    return Err(crate::error::VmError::type_error("Method Set.prototype.forEach called on incompatible receiver"));
                }
                let values = get_set_values_obj(&obj).ok_or("Internal error: missing values")?;
                let values_array = GcRef::new(JsObject::array(0, ncx.memory_manager().clone()));
                let props = values.own_keys();
                let mut index = 0i32;
                for prop in props {
                    if let Some(value) = values.get(&prop) {
                        values_array.set(pk(&index.to_string()), value);
                        index += 1;
                    }
                }
                values_array.set(pk("length"), Value::int32(index));
                Ok(Value::array(values_array))
            },
            mm.clone(),
            fn_proto,
        )),
    );

    // ========================================================================
    // ES2025 Set Methods (§24.2.3.x)
    // ========================================================================

    // Set.prototype.union(other)
    set_proto.define_property(
        PropertyKey::string("union"),
        PropertyDescriptor::builtin_method(Value::native_function_with_proto(
            |this_val, args, ncx| {
                let this_obj = this_val
                    .as_object()
                    .ok_or_else(|| crate::error::VmError::type_error("Method Set.prototype.union called on incompatible receiver"))?;
                if !is_set(&this_obj) {
                    return Err(crate::error::VmError::type_error("Method Set.prototype.union called on incompatible receiver"));
                }
                let other = args.first().ok_or_else(|| crate::error::VmError::type_error("Set.prototype.union requires argument"))?;
                let other_obj = other
                    .as_object()
                    .ok_or_else(|| crate::error::VmError::type_error("Set.prototype.union requires a Set-like argument"))?;
                if !is_set(&other_obj) {
                    return Err(crate::error::VmError::type_error("Set.prototype.union requires a Set-like argument"));
                }

                // Create new set
                let result = GcRef::new(JsObject::new(None, ncx.memory_manager().clone()));
                init_set_slots(&result, ncx.memory_manager());
                let result_values = get_set_values_obj(&result).unwrap();
                let mut count = 0i32;

                // Add all from this
                let this_vals = get_set_values_obj(&this_obj).ok_or("Internal error")?;
                for prop in this_vals.own_keys() {
                    if let Some(value) = this_vals.get(&prop) {
                        let hash = value_to_key(&value);
                        if result_values.get(&pk(&hash)).is_none() {
                            result_values.set(pk(&hash), value);
                            count += 1;
                        }
                    }
                }
                // Add all from other
                let other_vals = get_set_values_obj(&other_obj).ok_or("Internal error")?;
                for prop in other_vals.own_keys() {
                    if let Some(value) = other_vals.get(&prop) {
                        let hash = value_to_key(&value);
                        if result_values.get(&pk(&hash)).is_none() {
                            result_values.set(pk(&hash), value);
                            count += 1;
                        }
                    }
                }
                result.set(pk(SET_SIZE_KEY), Value::int32(count));
                Ok(Value::object(result))
            },
            mm.clone(),
            fn_proto,
        )),
    );

    // Set.prototype.intersection(other)
    set_proto.define_property(
        PropertyKey::string("intersection"),
        PropertyDescriptor::builtin_method(Value::native_function_with_proto(
            |this_val, args, ncx| {
                let this_obj = this_val
                    .as_object()
                    .ok_or_else(|| crate::error::VmError::type_error("Method Set.prototype.intersection called on incompatible receiver"))?;
                if !is_set(&this_obj) {
                    return Err(crate::error::VmError::type_error("Method Set.prototype.intersection called on incompatible receiver"));
                }
                let other = args.first().ok_or_else(|| crate::error::VmError::type_error("Set.prototype.intersection requires argument"))?;
                let other_obj = other
                    .as_object()
                    .ok_or_else(|| crate::error::VmError::type_error("Set.prototype.intersection requires a Set-like argument"))?;
                if !is_set(&other_obj) {
                    return Err(crate::error::VmError::type_error("Set.prototype.intersection requires a Set-like argument"));
                }

                let result = GcRef::new(JsObject::new(None, ncx.memory_manager().clone()));
                init_set_slots(&result, ncx.memory_manager());
                let result_values = get_set_values_obj(&result).unwrap();
                let mut count = 0i32;

                let this_vals = get_set_values_obj(&this_obj).ok_or("Internal error")?;
                let other_vals = get_set_values_obj(&other_obj).ok_or("Internal error")?;
                for prop in this_vals.own_keys() {
                    if let Some(value) = this_vals.get(&prop) {
                        let hash = value_to_key(&value);
                        if other_vals.get(&pk(&hash)).is_some() {
                            result_values.set(pk(&hash), value);
                            count += 1;
                        }
                    }
                }
                result.set(pk(SET_SIZE_KEY), Value::int32(count));
                Ok(Value::object(result))
            },
            mm.clone(),
            fn_proto,
        )),
    );

    // Set.prototype.difference(other)
    set_proto.define_property(
        PropertyKey::string("difference"),
        PropertyDescriptor::builtin_method(Value::native_function_with_proto(
            |this_val, args, ncx| {
                let this_obj = this_val
                    .as_object()
                    .ok_or_else(|| crate::error::VmError::type_error("Method Set.prototype.difference called on incompatible receiver"))?;
                if !is_set(&this_obj) {
                    return Err(crate::error::VmError::type_error("Method Set.prototype.difference called on incompatible receiver"));
                }
                let other = args.first().ok_or_else(|| crate::error::VmError::type_error("Set.prototype.difference requires argument"))?;
                let other_obj = other
                    .as_object()
                    .ok_or_else(|| crate::error::VmError::type_error("Set.prototype.difference requires a Set-like argument"))?;
                if !is_set(&other_obj) {
                    return Err(crate::error::VmError::type_error("Set.prototype.difference requires a Set-like argument"));
                }

                let result = GcRef::new(JsObject::new(None, ncx.memory_manager().clone()));
                init_set_slots(&result, ncx.memory_manager());
                let result_values = get_set_values_obj(&result).unwrap();
                let mut count = 0i32;

                let this_vals = get_set_values_obj(&this_obj).ok_or("Internal error")?;
                let other_vals = get_set_values_obj(&other_obj).ok_or("Internal error")?;
                for prop in this_vals.own_keys() {
                    if let Some(value) = this_vals.get(&prop) {
                        let hash = value_to_key(&value);
                        if other_vals.get(&pk(&hash)).is_none() {
                            result_values.set(pk(&hash), value);
                            count += 1;
                        }
                    }
                }
                result.set(pk(SET_SIZE_KEY), Value::int32(count));
                Ok(Value::object(result))
            },
            mm.clone(),
            fn_proto,
        )),
    );

    // Set.prototype.symmetricDifference(other)
    set_proto.define_property(
        PropertyKey::string("symmetricDifference"),
        PropertyDescriptor::builtin_method(Value::native_function_with_proto(
            |this_val, args, ncx| {
                let this_obj = this_val
                    .as_object()
                    .ok_or_else(|| crate::error::VmError::type_error("Method Set.prototype.symmetricDifference called on incompatible receiver"))?;
                if !is_set(&this_obj) {
                    return Err(crate::error::VmError::type_error("Method Set.prototype.symmetricDifference called on incompatible receiver"));
                }
                let other = args.first().ok_or_else(|| crate::error::VmError::type_error("Set.prototype.symmetricDifference requires argument"))?;
                let other_obj = other
                    .as_object()
                    .ok_or_else(|| crate::error::VmError::type_error("Set.prototype.symmetricDifference requires a Set-like argument"))?;
                if !is_set(&other_obj) {
                    return Err(crate::error::VmError::type_error("Set.prototype.symmetricDifference requires a Set-like argument"));
                }

                let result = GcRef::new(JsObject::new(None, ncx.memory_manager().clone()));
                init_set_slots(&result, ncx.memory_manager());
                let result_values = get_set_values_obj(&result).unwrap();
                let mut count = 0i32;

                let this_vals = get_set_values_obj(&this_obj).ok_or("Internal error")?;
                let other_vals = get_set_values_obj(&other_obj).ok_or("Internal error")?;

                // In this but not other
                for prop in this_vals.own_keys() {
                    if let Some(value) = this_vals.get(&prop) {
                        let hash = value_to_key(&value);
                        if other_vals.get(&pk(&hash)).is_none() {
                            result_values.set(pk(&hash), value);
                            count += 1;
                        }
                    }
                }
                // In other but not this
                for prop in other_vals.own_keys() {
                    if let Some(value) = other_vals.get(&prop) {
                        let hash = value_to_key(&value);
                        if this_vals.get(&pk(&hash)).is_none() {
                            result_values.set(pk(&hash), value);
                            count += 1;
                        }
                    }
                }
                result.set(pk(SET_SIZE_KEY), Value::int32(count));
                Ok(Value::object(result))
            },
            mm.clone(),
            fn_proto,
        )),
    );

    // Set.prototype.isSubsetOf(other)
    set_proto.define_property(
        PropertyKey::string("isSubsetOf"),
        PropertyDescriptor::builtin_method(Value::native_function_with_proto(
            |this_val, args, _ncx| {
                let this_obj = this_val
                    .as_object()
                    .ok_or_else(|| crate::error::VmError::type_error("Method Set.prototype.isSubsetOf called on incompatible receiver"))?;
                if !is_set(&this_obj) {
                    return Err(crate::error::VmError::type_error("Method Set.prototype.isSubsetOf called on incompatible receiver"));
                }
                let other = args.first().ok_or_else(|| crate::error::VmError::type_error("Set.prototype.isSubsetOf requires argument"))?;
                let other_obj = other
                    .as_object()
                    .ok_or_else(|| crate::error::VmError::type_error("Set.prototype.isSubsetOf requires a Set-like argument"))?;
                if !is_set(&other_obj) {
                    return Err(crate::error::VmError::type_error("Set.prototype.isSubsetOf requires a Set-like argument"));
                }

                let this_vals = get_set_values_obj(&this_obj).ok_or("Internal error")?;
                let other_vals = get_set_values_obj(&other_obj).ok_or("Internal error")?;
                for prop in this_vals.own_keys() {
                    if let Some(value) = this_vals.get(&prop) {
                        let hash = value_to_key(&value);
                        if other_vals.get(&pk(&hash)).is_none() {
                            return Ok(Value::boolean(false));
                        }
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
        PropertyDescriptor::builtin_method(Value::native_function_with_proto(
            |this_val, args, _ncx| {
                let this_obj = this_val
                    .as_object()
                    .ok_or_else(|| crate::error::VmError::type_error("Method Set.prototype.isSupersetOf called on incompatible receiver"))?;
                if !is_set(&this_obj) {
                    return Err(crate::error::VmError::type_error("Method Set.prototype.isSupersetOf called on incompatible receiver"));
                }
                let other = args.first().ok_or_else(|| crate::error::VmError::type_error("Set.prototype.isSupersetOf requires argument"))?;
                let other_obj = other
                    .as_object()
                    .ok_or_else(|| crate::error::VmError::type_error("Set.prototype.isSupersetOf requires a Set-like argument"))?;
                if !is_set(&other_obj) {
                    return Err(crate::error::VmError::type_error("Set.prototype.isSupersetOf requires a Set-like argument"));
                }

                let this_vals = get_set_values_obj(&this_obj).ok_or("Internal error")?;
                let other_vals = get_set_values_obj(&other_obj).ok_or("Internal error")?;
                for prop in other_vals.own_keys() {
                    if let Some(value) = other_vals.get(&prop) {
                        let hash = value_to_key(&value);
                        if this_vals.get(&pk(&hash)).is_none() {
                            return Ok(Value::boolean(false));
                        }
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
        PropertyDescriptor::builtin_method(Value::native_function_with_proto(
            |this_val, args, _ncx| {
                let this_obj = this_val
                    .as_object()
                    .ok_or_else(|| crate::error::VmError::type_error("Method Set.prototype.isDisjointFrom called on incompatible receiver"))?;
                if !is_set(&this_obj) {
                    return Err(crate::error::VmError::type_error("Method Set.prototype.isDisjointFrom called on incompatible receiver"));
                }
                let other = args.first().ok_or_else(|| crate::error::VmError::type_error("Set.prototype.isDisjointFrom requires argument"))?;
                let other_obj = other
                    .as_object()
                    .ok_or_else(|| crate::error::VmError::type_error("Set.prototype.isDisjointFrom requires a Set-like argument"))?;
                if !is_set(&other_obj) {
                    return Err(crate::error::VmError::type_error("Set.prototype.isDisjointFrom requires a Set-like argument"));
                }

                let this_vals = get_set_values_obj(&this_obj).ok_or("Internal error")?;
                let other_vals = get_set_values_obj(&other_obj).ok_or("Internal error")?;
                for prop in this_vals.own_keys() {
                    if let Some(value) = this_vals.get(&prop) {
                        let hash = value_to_key(&value);
                        if other_vals.get(&pk(&hash)).is_some() {
                            return Ok(Value::boolean(false));
                        }
                    }
                }
                Ok(Value::boolean(true))
            },
            mm.clone(),
            fn_proto,
        )),
    );

    // Set.prototype[Symbol.iterator] - same as values per ES spec
    let iter_proto_for_symbol = iterator_proto;
    let mm_for_symbol = mm.clone();
    let fn_proto_for_symbol = fn_proto;
    set_proto.define_property(
        PropertyKey::Symbol(symbol_iterator_id),
        PropertyDescriptor::builtin_method(Value::native_function_with_proto(
            move |this_val, _args, ncx| {
                make_set_iterator(this_val, "value", ncx.memory_manager().clone(), fn_proto_for_symbol, iter_proto_for_symbol)
            },
            mm_for_symbol,
            fn_proto,
        )),
    );

    // Set.prototype[Symbol.toStringTag] = "Set"
    set_proto.define_property(
        PropertyKey::Symbol(crate::intrinsics::well_known::TO_STRING_TAG),
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
// WeakMap.prototype
// ============================================================================

/// Initialize WeakMap.prototype with all ES2026 methods.
///
/// # Methods
/// - get, set, has, delete
/// - Symbol.toStringTag = "WeakMap"
pub fn init_weak_map_prototype(
    wm_proto: GcRef<JsObject>,
    fn_proto: GcRef<JsObject>,
    mm: &Arc<MemoryManager>,
) {
    // WeakMap.prototype.get(key)
    wm_proto.define_property(
        PropertyKey::string("get"),
        PropertyDescriptor::builtin_method(Value::native_function_with_proto(
            |this_val, args, _ncx| {
                let obj = this_val
                    .as_object()
                    .ok_or_else(|| crate::error::VmError::type_error("Method WeakMap.prototype.get called on incompatible receiver"))?;
                if !is_weakmap(&obj) {
                    return Err(crate::error::VmError::type_error("Method WeakMap.prototype.get called on incompatible receiver"));
                }
                let key = args.first().cloned().unwrap_or(Value::undefined());
                if !is_valid_weak_key(&key) {
                    return Ok(Value::undefined());
                }
                let entries = get_entries(&obj).ok_or("Internal error: missing entries")?;
                let hash_key = value_to_key(&key);
                if let Some(entry) = entries.get(&pk(&hash_key)) {
                    if let Some(entry_obj) = entry.as_object() {
                        return Ok(entry_obj.get(&pk("v")).unwrap_or(Value::undefined()));
                    }
                }
                Ok(Value::undefined())
            },
            mm.clone(),
            fn_proto,
        )),
    );

    // WeakMap.prototype.set(key, value)
    wm_proto.define_property(
        PropertyKey::string("set"),
        PropertyDescriptor::builtin_method(Value::native_function_with_proto(
            |this_val, args, ncx| {
                let obj = this_val
                    .as_object()
                    .ok_or_else(|| crate::error::VmError::type_error("Method WeakMap.prototype.set called on incompatible receiver"))?;
                if !is_weakmap(&obj) {
                    return Err(crate::error::VmError::type_error("Method WeakMap.prototype.set called on incompatible receiver"));
                }
                let key = args.first().cloned().unwrap_or(Value::undefined());
                if !is_valid_weak_key(&key) {
                    return Err(crate::error::VmError::type_error("Invalid value used as weak map key"));
                }
                let value = args.get(1).cloned().unwrap_or(Value::undefined());
                let entries = get_entries(&obj).ok_or("Internal error: missing entries")?;
                let hash_key = value_to_key(&key);

                let entry = GcRef::new(JsObject::new(None, ncx.memory_manager().clone()));
                entry.set(pk("k"), key);
                entry.set(pk("v"), value);
                entries.set(pk(&hash_key), Value::object(entry));

                Ok(this_val.clone())
            },
            mm.clone(),
            fn_proto,
        )),
    );

    // WeakMap.prototype.has(key)
    wm_proto.define_property(
        PropertyKey::string("has"),
        PropertyDescriptor::builtin_method(Value::native_function_with_proto(
            |this_val, args, _ncx| {
                let obj = this_val
                    .as_object()
                    .ok_or_else(|| crate::error::VmError::type_error("Method WeakMap.prototype.has called on incompatible receiver"))?;
                if !is_weakmap(&obj) {
                    return Err(crate::error::VmError::type_error("Method WeakMap.prototype.has called on incompatible receiver"));
                }
                let key = args.first().cloned().unwrap_or(Value::undefined());
                if !is_valid_weak_key(&key) {
                    return Ok(Value::boolean(false));
                }
                let entries = get_entries(&obj).ok_or("Internal error: missing entries")?;
                let hash_key = value_to_key(&key);
                Ok(Value::boolean(entries.get(&pk(&hash_key)).is_some()))
            },
            mm.clone(),
            fn_proto,
        )),
    );

    // WeakMap.prototype.delete(key)
    wm_proto.define_property(
        PropertyKey::string("delete"),
        PropertyDescriptor::builtin_method(Value::native_function_with_proto(
            |this_val, args, _ncx| {
                let obj = this_val
                    .as_object()
                    .ok_or_else(|| crate::error::VmError::type_error("Method WeakMap.prototype.delete called on incompatible receiver"))?;
                if !is_weakmap(&obj) {
                    return Err(crate::error::VmError::type_error("Method WeakMap.prototype.delete called on incompatible receiver"));
                }
                let key = args.first().cloned().unwrap_or(Value::undefined());
                if !is_valid_weak_key(&key) {
                    return Ok(Value::boolean(false));
                }
                let entries = get_entries(&obj).ok_or("Internal error: missing entries")?;
                let hash_key = value_to_key(&key);
                if entries.get(&pk(&hash_key)).is_none() {
                    return Ok(Value::boolean(false));
                }
                entries.delete(&pk(&hash_key));
                Ok(Value::boolean(true))
            },
            mm.clone(),
            fn_proto,
        )),
    );

    // WeakMap.prototype[Symbol.toStringTag] = "WeakMap"
    wm_proto.define_property(
        PropertyKey::Symbol(crate::intrinsics::well_known::TO_STRING_TAG),
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

/// Initialize WeakSet.prototype with all ES2026 methods.
///
/// # Methods
/// - add, has, delete
/// - Symbol.toStringTag = "WeakSet"
pub fn init_weak_set_prototype(
    ws_proto: GcRef<JsObject>,
    fn_proto: GcRef<JsObject>,
    mm: &Arc<MemoryManager>,
) {
    // WeakSet.prototype.add(value)
    ws_proto.define_property(
        PropertyKey::string("add"),
        PropertyDescriptor::builtin_method(Value::native_function_with_proto(
            |this_val, args, _ncx| {
                let obj = this_val
                    .as_object()
                    .ok_or_else(|| crate::error::VmError::type_error("Method WeakSet.prototype.add called on incompatible receiver"))?;
                if !is_weakset(&obj) {
                    return Err(crate::error::VmError::type_error("Method WeakSet.prototype.add called on incompatible receiver"));
                }
                let value = args.first().cloned().unwrap_or(Value::undefined());
                if !is_valid_weak_key(&value) {
                    return Err(crate::error::VmError::type_error("Invalid value used in weak set"));
                }
                let values = get_set_values_obj(&obj).ok_or("Internal error: missing values")?;
                let hash_key = value_to_key(&value);
                values.set(pk(&hash_key), value);
                Ok(this_val.clone())
            },
            mm.clone(),
            fn_proto,
        )),
    );

    // WeakSet.prototype.has(value)
    ws_proto.define_property(
        PropertyKey::string("has"),
        PropertyDescriptor::builtin_method(Value::native_function_with_proto(
            |this_val, args, _ncx| {
                let obj = this_val
                    .as_object()
                    .ok_or_else(|| crate::error::VmError::type_error("Method WeakSet.prototype.has called on incompatible receiver"))?;
                if !is_weakset(&obj) {
                    return Err(crate::error::VmError::type_error("Method WeakSet.prototype.has called on incompatible receiver"));
                }
                let value = args.first().cloned().unwrap_or(Value::undefined());
                if !is_valid_weak_key(&value) {
                    return Ok(Value::boolean(false));
                }
                let values = get_set_values_obj(&obj).ok_or("Internal error: missing values")?;
                let hash_key = value_to_key(&value);
                Ok(Value::boolean(values.get(&pk(&hash_key)).is_some()))
            },
            mm.clone(),
            fn_proto,
        )),
    );

    // WeakSet.prototype.delete(value)
    ws_proto.define_property(
        PropertyKey::string("delete"),
        PropertyDescriptor::builtin_method(Value::native_function_with_proto(
            |this_val, args, _ncx| {
                let obj = this_val
                    .as_object()
                    .ok_or_else(|| crate::error::VmError::type_error("Method WeakSet.prototype.delete called on incompatible receiver"))?;
                if !is_weakset(&obj) {
                    return Err(crate::error::VmError::type_error("Method WeakSet.prototype.delete called on incompatible receiver"));
                }
                let value = args.first().cloned().unwrap_or(Value::undefined());
                if !is_valid_weak_key(&value) {
                    return Ok(Value::boolean(false));
                }
                let values = get_set_values_obj(&obj).ok_or("Internal error: missing values")?;
                let hash_key = value_to_key(&value);
                if values.get(&pk(&hash_key)).is_none() {
                    return Ok(Value::boolean(false));
                }
                values.delete(&pk(&hash_key));
                Ok(Value::boolean(true))
            },
            mm.clone(),
            fn_proto,
        )),
    );

    // WeakSet.prototype[Symbol.toStringTag] = "WeakSet"
    ws_proto.define_property(
        PropertyKey::Symbol(crate::intrinsics::well_known::TO_STRING_TAG),
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
///
/// - **new Map()** — Creates a new empty Map
/// - **Map()** without new — TypeError (Map is not callable without new)
pub fn create_map_constructor() -> Box<
    dyn Fn(&Value, &[Value], &mut crate::context::NativeContext<'_>) -> Result<Value, crate::error::VmError>
        + Send
        + Sync,
> {
    Box::new(|this_val, _args, ncx| {
        if this_val.is_undefined() {
            return Err(crate::error::VmError::type_error(
                "Constructor Map requires 'new'",
            ));
        }
        if let Some(obj) = this_val.as_object() {
            init_map_slots(&obj, ncx.memory_manager());
            Ok(this_val.clone())
        } else {
            Err(crate::error::VmError::type_error(
                "Constructor Map requires 'new'",
            ))
        }
    })
}

/// Create Set constructor function.
///
/// - **new Set()** — Creates a new empty Set
/// - **Set()** without new — TypeError (Set is not callable without new)
pub fn create_set_constructor() -> Box<
    dyn Fn(&Value, &[Value], &mut crate::context::NativeContext<'_>) -> Result<Value, crate::error::VmError>
        + Send
        + Sync,
> {
    Box::new(|this_val, _args, ncx| {
        if this_val.is_undefined() {
            return Err(crate::error::VmError::type_error(
                "Constructor Set requires 'new'",
            ));
        }
        if let Some(obj) = this_val.as_object() {
            init_set_slots(&obj, ncx.memory_manager());
            Ok(this_val.clone())
        } else {
            Err(crate::error::VmError::type_error(
                "Constructor Set requires 'new'",
            ))
        }
    })
}

/// Create WeakMap constructor function.
///
/// - **new WeakMap()** — Creates a new empty WeakMap
/// - **WeakMap()** without new — TypeError
pub fn create_weak_map_constructor() -> Box<
    dyn Fn(&Value, &[Value], &mut crate::context::NativeContext<'_>) -> Result<Value, crate::error::VmError>
        + Send
        + Sync,
> {
    Box::new(|this_val, _args, ncx| {
        if this_val.is_undefined() {
            return Err(crate::error::VmError::type_error(
                "Constructor WeakMap requires 'new'",
            ));
        }
        if let Some(obj) = this_val.as_object() {
            let entries = GcRef::new(JsObject::new(None, ncx.memory_manager().clone()));
            obj.set(pk(MAP_ENTRIES_KEY), Value::object(entries));
            obj.set(pk(IS_WEAKMAP_KEY), Value::boolean(true));
            Ok(this_val.clone())
        } else {
            Err(crate::error::VmError::type_error(
                "Constructor WeakMap requires 'new'",
            ))
        }
    })
}

/// Create WeakSet constructor function.
///
/// - **new WeakSet()** — Creates a new empty WeakSet
/// - **WeakSet()** without new — TypeError
pub fn create_weak_set_constructor() -> Box<
    dyn Fn(&Value, &[Value], &mut crate::context::NativeContext<'_>) -> Result<Value, crate::error::VmError>
        + Send
        + Sync,
> {
    Box::new(|this_val, _args, ncx| {
        if this_val.is_undefined() {
            return Err(crate::error::VmError::type_error(
                "Constructor WeakSet requires 'new'",
            ));
        }
        if let Some(obj) = this_val.as_object() {
            let values = GcRef::new(JsObject::new(None, ncx.memory_manager().clone()));
            obj.set(pk(SET_VALUES_KEY), Value::object(values));
            obj.set(pk(IS_WEAKSET_KEY), Value::boolean(true));
            Ok(this_val.clone())
        } else {
            Err(crate::error::VmError::type_error(
                "Constructor WeakSet requires 'new'",
            ))
        }
    })
}
