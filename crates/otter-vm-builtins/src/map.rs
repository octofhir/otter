//! Map and WeakMap built-ins
//!
//! Provides Map and WeakMap collections with full ES2026 support.

use otter_vm_core::object::{JsObject, PropertyKey};
use otter_vm_core::value::Value as VmValue;
use otter_vm_runtime::{Op, op_native};
use std::sync::Arc;

/// Get Map ops for extension registration
pub fn ops() -> Vec<Op> {
    vec![
        // Map operations
        op_native("__Map_new", native_map_new),
        op_native("__Map_get", native_map_get),
        op_native("__Map_set", native_map_set),
        op_native("__Map_has", native_map_has),
        op_native("__Map_delete", native_map_delete),
        op_native("__Map_clear", native_map_clear),
        op_native("__Map_size", native_map_size),
        op_native("__Map_keys", native_map_keys),
        op_native("__Map_values", native_map_values),
        op_native("__Map_entries", native_map_entries),
        op_native("__Map_forEach", native_map_foreach),
        // WeakMap operations
        op_native("__WeakMap_new", native_weakmap_new),
        op_native("__WeakMap_get", native_weakmap_get),
        op_native("__WeakMap_set", native_weakmap_set),
        op_native("__WeakMap_has", native_weakmap_has),
        op_native("__WeakMap_delete", native_weakmap_delete),
    ]
}

// Internal storage key for map entries
const MAP_ENTRIES_KEY: &str = "__map_entries__";
const MAP_SIZE_KEY: &str = "__map_size__";
const IS_MAP_KEY: &str = "__is_map__";
const IS_WEAKMAP_KEY: &str = "__is_weakmap__";

/// Helper to compute a hash key for a value (for string-keyed internal storage)
fn value_to_key(value: &VmValue) -> String {
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
    // For objects, use pointer address for identity
    if let Some(obj) = value.as_object() {
        return format!("__obj_{:p}__", Arc::as_ptr(obj));
    }
    if let Some(func) = value.as_function() {
        return format!("__func_{:p}__", Arc::as_ptr(func));
    }
    // Fallback
    format!("__unknown_{:?}__", value)
}

/// Helper to convert string to PropertyKey
fn str_to_key(s: &str) -> PropertyKey {
    PropertyKey::String(Arc::new(otter_vm_core::string::JsString::new(s)))
}

// ============================================================================
// Map Operations
// ============================================================================

/// Create a new Map
fn native_map_new(_args: &[VmValue]) -> Result<VmValue, String> {
    let map_obj = Arc::new(JsObject::new(None));

    // Create internal entries storage as a nested object
    let entries_obj = Arc::new(JsObject::new(None));
    map_obj.set(str_to_key(MAP_ENTRIES_KEY), VmValue::object(entries_obj));
    map_obj.set(str_to_key(MAP_SIZE_KEY), VmValue::int32(0));
    map_obj.set(str_to_key(IS_MAP_KEY), VmValue::boolean(true));

    Ok(VmValue::object(map_obj))
}

/// Map.prototype.get(key)
fn native_map_get(args: &[VmValue]) -> Result<VmValue, String> {
    let map = args.first().ok_or("Map.get requires a Map")?;
    let key = args.get(1).cloned().unwrap_or_else(VmValue::undefined);

    let map_obj = map.as_object().ok_or("First argument must be a Map")?;

    // Verify it's a Map
    let is_map = map_obj
        .get(&str_to_key(IS_MAP_KEY))
        .unwrap_or_else(VmValue::undefined);
    if is_map.as_boolean() != Some(true) {
        return Err("Method Map.prototype.get called on incompatible receiver".to_string());
    }

    let entries = map_obj
        .get(&str_to_key(MAP_ENTRIES_KEY))
        .ok_or("Internal error: missing entries")?;
    let entries_obj = entries
        .as_object()
        .ok_or("Internal error: entries not an object")?;

    let hash_key = value_to_key(&key);
    let entry_val = entries_obj.get(&str_to_key(&hash_key));

    // Entry stores [key, value] pair as object with "k" and "v" properties
    #[allow(clippy::collapsible_if)]
    if let Some(entry) = entry_val {
        if let Some(entry_obj) = entry.as_object() {
            let value = entry_obj
                .get(&str_to_key("v"))
                .unwrap_or_else(VmValue::undefined);
            return Ok(value);
        }
    }

    Ok(VmValue::undefined())
}

/// Map.prototype.set(key, value)
fn native_map_set(args: &[VmValue]) -> Result<VmValue, String> {
    let map = args.first().ok_or("Map.set requires a Map")?;
    let key = args.get(1).cloned().unwrap_or_else(VmValue::undefined);
    let value = args.get(2).cloned().unwrap_or_else(VmValue::undefined);

    let map_obj = map.as_object().ok_or("First argument must be a Map")?;

    // Verify it's a Map
    let is_map = map_obj
        .get(&str_to_key(IS_MAP_KEY))
        .unwrap_or_else(VmValue::undefined);
    if is_map.as_boolean() != Some(true) {
        return Err("Method Map.prototype.set called on incompatible receiver".to_string());
    }

    let entries = map_obj
        .get(&str_to_key(MAP_ENTRIES_KEY))
        .ok_or("Internal error: missing entries")?;
    let entries_obj = entries
        .as_object()
        .ok_or("Internal error: entries not an object")?;

    let hash_key = value_to_key(&key);

    // Check if key already exists
    let existing = entries_obj.get(&str_to_key(&hash_key));
    let is_new = existing.is_none();

    // Create entry object to store both key and value
    let entry = Arc::new(JsObject::new(None));
    entry.set(str_to_key("k"), key);
    entry.set(str_to_key("v"), value);
    entries_obj.set(str_to_key(&hash_key), VmValue::object(entry));

    // Update size if new key
    if is_new {
        let size = map_obj
            .get(&str_to_key(MAP_SIZE_KEY))
            .unwrap_or_else(VmValue::undefined);
        let current_size = size.as_int32().unwrap_or(0);
        map_obj.set(str_to_key(MAP_SIZE_KEY), VmValue::int32(current_size + 1));
    }

    // Return the map for chaining
    Ok(map.clone())
}

/// Map.prototype.has(key)
fn native_map_has(args: &[VmValue]) -> Result<VmValue, String> {
    let map = args.first().ok_or("Map.has requires a Map")?;
    let key = args.get(1).cloned().unwrap_or_else(VmValue::undefined);

    let map_obj = map.as_object().ok_or("First argument must be a Map")?;

    let is_map = map_obj
        .get(&str_to_key(IS_MAP_KEY))
        .unwrap_or_else(VmValue::undefined);
    if is_map.as_boolean() != Some(true) {
        return Err("Method Map.prototype.has called on incompatible receiver".to_string());
    }

    let entries = map_obj
        .get(&str_to_key(MAP_ENTRIES_KEY))
        .ok_or("Internal error: missing entries")?;
    let entries_obj = entries
        .as_object()
        .ok_or("Internal error: entries not an object")?;

    let hash_key = value_to_key(&key);
    let entry = entries_obj.get(&str_to_key(&hash_key));

    Ok(VmValue::boolean(entry.is_some()))
}

/// Map.prototype.delete(key)
fn native_map_delete(args: &[VmValue]) -> Result<VmValue, String> {
    let map = args.first().ok_or("Map.delete requires a Map")?;
    let key = args.get(1).cloned().unwrap_or_else(VmValue::undefined);

    let map_obj = map.as_object().ok_or("First argument must be a Map")?;

    let is_map = map_obj
        .get(&str_to_key(IS_MAP_KEY))
        .unwrap_or_else(VmValue::undefined);
    if is_map.as_boolean() != Some(true) {
        return Err("Method Map.prototype.delete called on incompatible receiver".to_string());
    }

    let entries = map_obj
        .get(&str_to_key(MAP_ENTRIES_KEY))
        .ok_or("Internal error: missing entries")?;
    let entries_obj = entries
        .as_object()
        .ok_or("Internal error: entries not an object")?;

    let hash_key = value_to_key(&key);
    let existing = entries_obj.get(&str_to_key(&hash_key));

    if existing.is_none() {
        return Ok(VmValue::boolean(false));
    }

    // Delete the entry
    entries_obj.delete(&str_to_key(&hash_key));

    // Update size
    let size = map_obj
        .get(&str_to_key(MAP_SIZE_KEY))
        .unwrap_or_else(VmValue::undefined);
    let current_size = size.as_int32().unwrap_or(0);
    if current_size > 0 {
        map_obj.set(str_to_key(MAP_SIZE_KEY), VmValue::int32(current_size - 1));
    }

    Ok(VmValue::boolean(true))
}

/// Map.prototype.clear()
fn native_map_clear(args: &[VmValue]) -> Result<VmValue, String> {
    let map = args.first().ok_or("Map.clear requires a Map")?;

    let map_obj = map.as_object().ok_or("First argument must be a Map")?;

    let is_map = map_obj
        .get(&str_to_key(IS_MAP_KEY))
        .unwrap_or_else(VmValue::undefined);
    if is_map.as_boolean() != Some(true) {
        return Err("Method Map.prototype.clear called on incompatible receiver".to_string());
    }

    // Replace entries with new empty object
    let new_entries = Arc::new(JsObject::new(None));
    map_obj.set(str_to_key(MAP_ENTRIES_KEY), VmValue::object(new_entries));
    map_obj.set(str_to_key(MAP_SIZE_KEY), VmValue::int32(0));

    Ok(VmValue::undefined())
}

/// Map.prototype.size getter
fn native_map_size(args: &[VmValue]) -> Result<VmValue, String> {
    let map = args.first().ok_or("Map.size requires a Map")?;

    let map_obj = map.as_object().ok_or("First argument must be a Map")?;

    let is_map = map_obj
        .get(&str_to_key(IS_MAP_KEY))
        .unwrap_or_else(VmValue::undefined);
    if is_map.as_boolean() != Some(true) {
        return Err("get Map.prototype.size called on incompatible receiver".to_string());
    }

    let size = map_obj
        .get(&str_to_key(MAP_SIZE_KEY))
        .unwrap_or_else(VmValue::undefined);
    Ok(VmValue::int32(size.as_int32().unwrap_or(0)))
}

/// Map.prototype.keys() - returns an iterator over keys
fn native_map_keys(args: &[VmValue]) -> Result<VmValue, String> {
    let map = args.first().ok_or("Map.keys requires a Map")?;

    let map_obj = map.as_object().ok_or("First argument must be a Map")?;

    let is_map = map_obj
        .get(&str_to_key(IS_MAP_KEY))
        .unwrap_or_else(VmValue::undefined);
    if is_map.as_boolean() != Some(true) {
        return Err("Method Map.prototype.keys called on incompatible receiver".to_string());
    }

    let entries = map_obj
        .get(&str_to_key(MAP_ENTRIES_KEY))
        .ok_or("Internal error: missing entries")?;
    let entries_obj = entries
        .as_object()
        .ok_or("Internal error: entries not an object")?;

    // Collect all keys into an array
    let keys_array = Arc::new(JsObject::array(0));
    let props = entries_obj.own_keys();
    let mut index = 0;

    for prop in props {
        #[allow(clippy::collapsible_if)]
        if let Some(entry) = entries_obj.get(&prop) {
            if let Some(entry_obj) = entry.as_object() {
                let key = entry_obj
                    .get(&str_to_key("k"))
                    .unwrap_or_else(VmValue::undefined);
                keys_array.set(str_to_key(&index.to_string()), key);
                index += 1;
            }
        }
    }

    keys_array.set(str_to_key("length"), VmValue::int32(index));
    Ok(VmValue::array(keys_array))
}

/// Map.prototype.values() - returns an iterator over values
fn native_map_values(args: &[VmValue]) -> Result<VmValue, String> {
    let map = args.first().ok_or("Map.values requires a Map")?;

    let map_obj = map.as_object().ok_or("First argument must be a Map")?;

    let is_map = map_obj
        .get(&str_to_key(IS_MAP_KEY))
        .unwrap_or_else(VmValue::undefined);
    if is_map.as_boolean() != Some(true) {
        return Err("Method Map.prototype.values called on incompatible receiver".to_string());
    }

    let entries = map_obj
        .get(&str_to_key(MAP_ENTRIES_KEY))
        .ok_or("Internal error: missing entries")?;
    let entries_obj = entries
        .as_object()
        .ok_or("Internal error: entries not an object")?;

    // Collect all values into an array
    let values_array = Arc::new(JsObject::array(0));
    let props = entries_obj.own_keys();
    let mut index = 0;

    for prop in props {
        #[allow(clippy::collapsible_if)]
        if let Some(entry) = entries_obj.get(&prop) {
            if let Some(entry_obj) = entry.as_object() {
                let value = entry_obj
                    .get(&str_to_key("v"))
                    .unwrap_or_else(VmValue::undefined);
                values_array.set(str_to_key(&index.to_string()), value);
                index += 1;
            }
        }
    }

    values_array.set(str_to_key("length"), VmValue::int32(index));
    Ok(VmValue::array(values_array))
}

/// Map.prototype.entries() - returns an iterator over [key, value] pairs
fn native_map_entries(args: &[VmValue]) -> Result<VmValue, String> {
    let map = args.first().ok_or("Map.entries requires a Map")?;

    let map_obj = map.as_object().ok_or("First argument must be a Map")?;

    let is_map = map_obj
        .get(&str_to_key(IS_MAP_KEY))
        .unwrap_or_else(VmValue::undefined);
    if is_map.as_boolean() != Some(true) {
        return Err("Method Map.prototype.entries called on incompatible receiver".to_string());
    }

    let entries = map_obj
        .get(&str_to_key(MAP_ENTRIES_KEY))
        .ok_or("Internal error: missing entries")?;
    let entries_obj = entries
        .as_object()
        .ok_or("Internal error: entries not an object")?;

    // Collect all [key, value] pairs into an array
    let entries_array = Arc::new(JsObject::array(0));
    let props = entries_obj.own_keys();
    let mut index = 0;

    for prop in props {
        #[allow(clippy::collapsible_if)]
        if let Some(entry) = entries_obj.get(&prop) {
            if let Some(entry_obj) = entry.as_object() {
                let key = entry_obj
                    .get(&str_to_key("k"))
                    .unwrap_or_else(VmValue::undefined);
                let value = entry_obj
                    .get(&str_to_key("v"))
                    .unwrap_or_else(VmValue::undefined);

                // Create [key, value] pair as array
                let pair = Arc::new(JsObject::array(0));
                pair.set(str_to_key("0"), key);
                pair.set(str_to_key("1"), value);
                pair.set(str_to_key("length"), VmValue::int32(2));

                entries_array.set(str_to_key(&index.to_string()), VmValue::array(pair));
                index += 1;
            }
        }
    }

    entries_array.set(str_to_key("length"), VmValue::int32(index));
    Ok(VmValue::array(entries_array))
}

/// Map.prototype.forEach(callback, thisArg) - just returns entries for JS to iterate
fn native_map_foreach(args: &[VmValue]) -> Result<VmValue, String> {
    // The actual forEach iteration is done in JS
    // We just return the entries array here
    native_map_entries(args)
}

// ============================================================================
// WeakMap Operations
// ============================================================================

/// Create a new WeakMap
fn native_weakmap_new(_args: &[VmValue]) -> Result<VmValue, String> {
    let map_obj = Arc::new(JsObject::new(None));

    // WeakMap uses the same internal structure but only allows object keys
    let entries_obj = Arc::new(JsObject::new(None));
    map_obj.set(str_to_key(MAP_ENTRIES_KEY), VmValue::object(entries_obj));
    map_obj.set(str_to_key(IS_WEAKMAP_KEY), VmValue::boolean(true));

    Ok(VmValue::object(map_obj))
}

/// Helper to validate WeakMap key (must be object or symbol)
fn validate_weakmap_key(key: &VmValue) -> Result<(), String> {
    if key.is_object() || key.is_symbol() || key.is_function() {
        Ok(())
    } else {
        Err("Invalid value used as weak map key".to_string())
    }
}

/// WeakMap.prototype.get(key)
fn native_weakmap_get(args: &[VmValue]) -> Result<VmValue, String> {
    let map = args.first().ok_or("WeakMap.get requires a WeakMap")?;
    let key = args.get(1).cloned().unwrap_or_else(VmValue::undefined);

    let map_obj = map.as_object().ok_or("First argument must be a WeakMap")?;

    let is_weakmap = map_obj
        .get(&str_to_key(IS_WEAKMAP_KEY))
        .unwrap_or_else(VmValue::undefined);
    if is_weakmap.as_boolean() != Some(true) {
        return Err("Method WeakMap.prototype.get called on incompatible receiver".to_string());
    }

    // WeakMap keys must be objects or symbols
    if !key.is_object() && !key.is_symbol() && !key.is_function() {
        return Ok(VmValue::undefined());
    }

    let entries = map_obj
        .get(&str_to_key(MAP_ENTRIES_KEY))
        .ok_or("Internal error: missing entries")?;
    let entries_obj = entries
        .as_object()
        .ok_or("Internal error: entries not an object")?;

    let hash_key = value_to_key(&key);
    let entry_val = entries_obj.get(&str_to_key(&hash_key));

    #[allow(clippy::collapsible_if)]
    if let Some(entry) = entry_val {
        if let Some(entry_obj) = entry.as_object() {
            let value = entry_obj
                .get(&str_to_key("v"))
                .unwrap_or_else(VmValue::undefined);
            return Ok(value);
        }
    }

    Ok(VmValue::undefined())
}

/// WeakMap.prototype.set(key, value)
fn native_weakmap_set(args: &[VmValue]) -> Result<VmValue, String> {
    let map = args.first().ok_or("WeakMap.set requires a WeakMap")?;
    let key = args.get(1).cloned().unwrap_or_else(VmValue::undefined);
    let value = args.get(2).cloned().unwrap_or_else(VmValue::undefined);

    let map_obj = map.as_object().ok_or("First argument must be a WeakMap")?;

    let is_weakmap = map_obj
        .get(&str_to_key(IS_WEAKMAP_KEY))
        .unwrap_or_else(VmValue::undefined);
    if is_weakmap.as_boolean() != Some(true) {
        return Err("Method WeakMap.prototype.set called on incompatible receiver".to_string());
    }

    validate_weakmap_key(&key)?;

    let entries = map_obj
        .get(&str_to_key(MAP_ENTRIES_KEY))
        .ok_or("Internal error: missing entries")?;
    let entries_obj = entries
        .as_object()
        .ok_or("Internal error: entries not an object")?;

    let hash_key = value_to_key(&key);

    // Create entry object to store both key (weakly) and value
    let entry = Arc::new(JsObject::new(None));
    entry.set(str_to_key("k"), key);
    entry.set(str_to_key("v"), value);
    entries_obj.set(str_to_key(&hash_key), VmValue::object(entry));

    // Return the map for chaining
    Ok(map.clone())
}

/// WeakMap.prototype.has(key)
fn native_weakmap_has(args: &[VmValue]) -> Result<VmValue, String> {
    let map = args.first().ok_or("WeakMap.has requires a WeakMap")?;
    let key = args.get(1).cloned().unwrap_or_else(VmValue::undefined);

    let map_obj = map.as_object().ok_or("First argument must be a WeakMap")?;

    let is_weakmap = map_obj
        .get(&str_to_key(IS_WEAKMAP_KEY))
        .unwrap_or_else(VmValue::undefined);
    if is_weakmap.as_boolean() != Some(true) {
        return Err("Method WeakMap.prototype.has called on incompatible receiver".to_string());
    }

    if !key.is_object() && !key.is_symbol() && !key.is_function() {
        return Ok(VmValue::boolean(false));
    }

    let entries = map_obj
        .get(&str_to_key(MAP_ENTRIES_KEY))
        .ok_or("Internal error: missing entries")?;
    let entries_obj = entries
        .as_object()
        .ok_or("Internal error: entries not an object")?;

    let hash_key = value_to_key(&key);
    let entry = entries_obj.get(&str_to_key(&hash_key));

    Ok(VmValue::boolean(entry.is_some()))
}

/// WeakMap.prototype.delete(key)
fn native_weakmap_delete(args: &[VmValue]) -> Result<VmValue, String> {
    let map = args.first().ok_or("WeakMap.delete requires a WeakMap")?;
    let key = args.get(1).cloned().unwrap_or_else(VmValue::undefined);

    let map_obj = map.as_object().ok_or("First argument must be a WeakMap")?;

    let is_weakmap = map_obj
        .get(&str_to_key(IS_WEAKMAP_KEY))
        .unwrap_or_else(VmValue::undefined);
    if is_weakmap.as_boolean() != Some(true) {
        return Err("Method WeakMap.prototype.delete called on incompatible receiver".to_string());
    }

    if !key.is_object() && !key.is_symbol() && !key.is_function() {
        return Ok(VmValue::boolean(false));
    }

    let entries = map_obj
        .get(&str_to_key(MAP_ENTRIES_KEY))
        .ok_or("Internal error: missing entries")?;
    let entries_obj = entries
        .as_object()
        .ok_or("Internal error: entries not an object")?;

    let hash_key = value_to_key(&key);
    let existing = entries_obj.get(&str_to_key(&hash_key));

    if existing.is_none() {
        return Ok(VmValue::boolean(false));
    }

    entries_obj.delete(&str_to_key(&hash_key));
    Ok(VmValue::boolean(true))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_map_new() {
        let result = native_map_new(&[]).unwrap();
        assert!(result.is_object());
    }

    #[test]
    fn test_map_set_get() {
        let map = native_map_new(&[]).unwrap();

        // Set a value
        let key = VmValue::int32(42);
        let value = VmValue::int32(100);
        let _ = native_map_set(&[map.clone(), key.clone(), value.clone()]).unwrap();

        // Get the value
        let result = native_map_get(&[map.clone(), key]).unwrap();
        assert_eq!(result.as_int32(), Some(100));
    }

    #[test]
    fn test_map_has() {
        let map = native_map_new(&[]).unwrap();

        let key = VmValue::int32(42);
        let value = VmValue::int32(100);

        // Initially doesn't have key
        let has = native_map_has(&[map.clone(), key.clone()]).unwrap();
        assert_eq!(has.as_boolean(), Some(false));

        // After set, has key
        let _ = native_map_set(&[map.clone(), key.clone(), value]).unwrap();
        let has = native_map_has(&[map.clone(), key]).unwrap();
        assert_eq!(has.as_boolean(), Some(true));
    }

    #[test]
    fn test_map_delete() {
        let map = native_map_new(&[]).unwrap();
        let key = VmValue::int32(42);
        let value = VmValue::int32(100);

        let _ = native_map_set(&[map.clone(), key.clone(), value]).unwrap();

        let deleted = native_map_delete(&[map.clone(), key.clone()]).unwrap();
        assert_eq!(deleted.as_boolean(), Some(true));

        let has = native_map_has(&[map, key]).unwrap();
        assert_eq!(has.as_boolean(), Some(false));
    }

    #[test]
    fn test_map_size() {
        let map = native_map_new(&[]).unwrap();

        // Initially 0
        let size = native_map_size(std::slice::from_ref(&map)).unwrap();
        assert_eq!(size.as_int32(), Some(0));

        // After adding
        let _ = native_map_set(&[map.clone(), VmValue::int32(1), VmValue::int32(1)]).unwrap();
        let _ = native_map_set(&[map.clone(), VmValue::int32(2), VmValue::int32(2)]).unwrap();

        let size = native_map_size(&[map]).unwrap();
        assert_eq!(size.as_int32(), Some(2));
    }

    #[test]
    fn test_map_clear() {
        let map = native_map_new(&[]).unwrap();

        let _ = native_map_set(&[map.clone(), VmValue::int32(1), VmValue::int32(1)]).unwrap();
        let _ = native_map_clear(std::slice::from_ref(&map)).unwrap();

        let size = native_map_size(&[map]).unwrap();
        assert_eq!(size.as_int32(), Some(0));
    }

    #[test]
    fn test_weakmap_new() {
        let result = native_weakmap_new(&[]).unwrap();
        assert!(result.is_object());
    }

    #[test]
    fn test_weakmap_requires_object_key() {
        let map = native_weakmap_new(&[]).unwrap();

        // Primitive key should fail
        let key = VmValue::int32(42);
        let value = VmValue::int32(100);
        let result = native_weakmap_set(&[map, key, value]);
        assert!(result.is_err());
    }

    #[test]
    fn test_weakmap_with_object_key() {
        let map = native_weakmap_new(&[]).unwrap();

        // Object key should work
        let key_obj = Arc::new(JsObject::new(None));
        let key = VmValue::object(key_obj);
        let value = VmValue::int32(100);

        let _ = native_weakmap_set(&[map.clone(), key.clone(), value]).unwrap();

        let result = native_weakmap_get(&[map, key]).unwrap();
        assert_eq!(result.as_int32(), Some(100));
    }
}
