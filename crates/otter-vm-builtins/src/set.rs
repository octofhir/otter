//! Set and WeakSet built-ins
//!
//! Provides Set and WeakSet collections with full ES2026 support.

use otter_vm_core::error::VmError;
use otter_vm_core::gc::GcRef;
use otter_vm_core::memory;
use otter_vm_core::object::{JsObject, PropertyKey};
use otter_vm_core::value::Value as VmValue;
use otter_vm_runtime::{Op, op_native_with_mm as op_native};
use std::sync::Arc;

/// Get Set ops for extension registration
pub fn ops() -> Vec<Op> {
    vec![
        // Set operations
        op_native("__Set_new", native_set_new),
        op_native("__Set_add", native_set_add),
        op_native("__Set_has", native_set_has),
        op_native("__Set_delete", native_set_delete),
        op_native("__Set_clear", native_set_clear),
        op_native("__Set_size", native_set_size),
        op_native("__Set_values", native_set_values),
        op_native("__Set_keys", native_set_keys),
        op_native("__Set_entries", native_set_entries),
        op_native("__Set_forEach", native_set_foreach),
        // Set methods (ES2025+)
        op_native("__Set_union", native_set_union),
        op_native("__Set_intersection", native_set_intersection),
        op_native("__Set_difference", native_set_difference),
        op_native("__Set_symmetricDifference", native_set_symmetric_difference),
        op_native("__Set_isSubsetOf", native_set_is_subset_of),
        op_native("__Set_isSupersetOf", native_set_is_superset_of),
        op_native("__Set_isDisjointFrom", native_set_is_disjoint_from),
        // WeakSet operations
        op_native("__WeakSet_new", native_weakset_new),
        op_native("__WeakSet_add", native_weakset_add),
        op_native("__WeakSet_has", native_weakset_has),
        op_native("__WeakSet_delete", native_weakset_delete),
    ]
}

// Internal storage keys
const SET_VALUES_KEY: &str = "__set_values__";
const SET_SIZE_KEY: &str = "__set_size__";
const IS_SET_KEY: &str = "__is_set__";
const IS_WEAKSET_KEY: &str = "__is_weakset__";

/// Helper to compute a hash key for a value
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
        return format!("__obj_{:p}__", obj.as_ptr());
    }
    if let Some(func) = value.as_function() {
        return format!("__func_{:p}__", func.as_ptr());
    }
    format!("__unknown_{:?}__", value)
}

/// Helper to convert string to PropertyKey
fn str_to_key(s: &str) -> PropertyKey {
    PropertyKey::String(otter_vm_core::string::JsString::intern(s))
}

// ============================================================================
// Set Operations
// ============================================================================

/// Create a new Set
fn native_set_new(_args: &[VmValue], mm: Arc<memory::MemoryManager>) -> Result<VmValue, VmError> {
    let set_obj = GcRef::new(JsObject::new(VmValue::null(), Arc::clone(&mm)));

    // Create internal values storage
    let values_obj = GcRef::new(JsObject::new(VmValue::null(), Arc::clone(&mm)));
    set_obj.set(str_to_key(SET_VALUES_KEY), VmValue::object(values_obj));
    set_obj.set(str_to_key(SET_SIZE_KEY), VmValue::int32(0));
    set_obj.set(str_to_key(IS_SET_KEY), VmValue::boolean(true));

    Ok(VmValue::object(set_obj))
}

/// Set.prototype.add(value)
fn native_set_add(args: &[VmValue], _mm: Arc<memory::MemoryManager>) -> Result<VmValue, VmError> {
    let set = args.first().ok_or("Set.add requires a Set")?;
    let value = args.get(1).cloned().unwrap_or_else(VmValue::undefined);

    let set_obj = set.as_object().ok_or("First argument must be a Set")?;

    // Verify it's a Set
    let is_set = set_obj
        .get(&str_to_key(IS_SET_KEY))
        .unwrap_or_else(VmValue::undefined);
    if is_set.as_boolean() != Some(true) {
        return Err(VmError::type_error("Method Set.prototype.add called on incompatible receiver"));
    }

    let values = set_obj
        .get(&str_to_key(SET_VALUES_KEY))
        .ok_or("Internal error: missing values")?;
    let values_obj = values
        .as_object()
        .ok_or("Internal error: values not an object")?;

    let hash_key = value_to_key(&value);

    // Check if value already exists
    let existing = values_obj.get(&str_to_key(&hash_key));
    let is_new = existing.is_none();

    // Store the value (using the original value as storage)
    values_obj.set(str_to_key(&hash_key), value);

    // Update size if new value
    if is_new {
        let size = set_obj
            .get(&str_to_key(SET_SIZE_KEY))
            .unwrap_or_else(VmValue::undefined);
        let current_size = size.as_int32().unwrap_or(0);
        set_obj.set(str_to_key(SET_SIZE_KEY), VmValue::int32(current_size + 1));
    }

    // Return the set for chaining
    Ok(set.clone())
}

/// Set.prototype.has(value)
fn native_set_has(args: &[VmValue], _mm: Arc<memory::MemoryManager>) -> Result<VmValue, VmError> {
    let set = args.first().ok_or("Set.has requires a Set")?;
    let value = args.get(1).cloned().unwrap_or_else(VmValue::undefined);

    let set_obj = set.as_object().ok_or("First argument must be a Set")?;

    let is_set = set_obj
        .get(&str_to_key(IS_SET_KEY))
        .unwrap_or_else(VmValue::undefined);
    if is_set.as_boolean() != Some(true) {
        return Err(VmError::type_error("Method Set.prototype.has called on incompatible receiver"));
    }

    let values = set_obj
        .get(&str_to_key(SET_VALUES_KEY))
        .ok_or("Internal error: missing values")?;
    let values_obj = values
        .as_object()
        .ok_or("Internal error: values not an object")?;

    let hash_key = value_to_key(&value);
    let entry = values_obj.get(&str_to_key(&hash_key));

    Ok(VmValue::boolean(entry.is_some()))
}

/// Set.prototype.delete(value)
fn native_set_delete(args: &[VmValue], _mm: Arc<memory::MemoryManager>) -> Result<VmValue, VmError> {
    let set = args.first().ok_or("Set.delete requires a Set")?;
    let value = args.get(1).cloned().unwrap_or_else(VmValue::undefined);

    let set_obj = set.as_object().ok_or("First argument must be a Set")?;

    let is_set = set_obj
        .get(&str_to_key(IS_SET_KEY))
        .unwrap_or_else(VmValue::undefined);
    if is_set.as_boolean() != Some(true) {
        return Err(VmError::type_error("Method Set.prototype.delete called on incompatible receiver"));
    }

    let values = set_obj
        .get(&str_to_key(SET_VALUES_KEY))
        .ok_or("Internal error: missing values")?;
    let values_obj = values
        .as_object()
        .ok_or("Internal error: values not an object")?;

    let hash_key = value_to_key(&value);
    let existing = values_obj.get(&str_to_key(&hash_key));

    if existing.is_none() {
        return Ok(VmValue::boolean(false));
    }

    // Delete the value
    values_obj.delete(&str_to_key(&hash_key));

    // Update size
    let size = set_obj
        .get(&str_to_key(SET_SIZE_KEY))
        .unwrap_or_else(VmValue::undefined);
    let current_size = size.as_int32().unwrap_or(0);
    if current_size > 0 {
        set_obj.set(str_to_key(SET_SIZE_KEY), VmValue::int32(current_size - 1));
    }

    Ok(VmValue::boolean(true))
}

/// Set.prototype.clear()
fn native_set_clear(args: &[VmValue], mm: Arc<memory::MemoryManager>) -> Result<VmValue, VmError> {
    let set = args.first().ok_or("Set.clear requires a Set")?;

    let set_obj = set.as_object().ok_or("First argument must be a Set")?;

    let is_set = set_obj
        .get(&str_to_key(IS_SET_KEY))
        .unwrap_or_else(VmValue::undefined);
    if is_set.as_boolean() != Some(true) {
        return Err(VmError::type_error("Method Set.prototype.clear called on incompatible receiver"));
    }

    // Replace values with new empty object
    let new_values = GcRef::new(JsObject::new(VmValue::null(), Arc::clone(&mm)));
    set_obj.set(str_to_key(SET_VALUES_KEY), VmValue::object(new_values));
    set_obj.set(str_to_key(SET_SIZE_KEY), VmValue::int32(0));

    Ok(VmValue::undefined())
}

/// Set.prototype.size getter
fn native_set_size(args: &[VmValue], _mm: Arc<memory::MemoryManager>) -> Result<VmValue, VmError> {
    let set = args.first().ok_or("Set.size requires a Set")?;

    let set_obj = set.as_object().ok_or("First argument must be a Set")?;

    let is_set = set_obj
        .get(&str_to_key(IS_SET_KEY))
        .unwrap_or_else(VmValue::undefined);
    if is_set.as_boolean() != Some(true) {
        return Err(VmError::type_error("get Set.prototype.size called on incompatible receiver"));
    }

    let size = set_obj
        .get(&str_to_key(SET_SIZE_KEY))
        .unwrap_or_else(VmValue::undefined);
    Ok(VmValue::int32(size.as_int32().unwrap_or(0)))
}

/// Set.prototype.values() - returns an iterator over values
fn native_set_values(args: &[VmValue], mm: Arc<memory::MemoryManager>) -> Result<VmValue, VmError> {
    let set = args.first().ok_or("Set.values requires a Set")?;

    let set_obj = set.as_object().ok_or("First argument must be a Set")?;

    let is_set = set_obj
        .get(&str_to_key(IS_SET_KEY))
        .unwrap_or_else(VmValue::undefined);
    if is_set.as_boolean() != Some(true) {
        return Err(VmError::type_error("Method Set.prototype.values called on incompatible receiver"));
    }

    let values = set_obj
        .get(&str_to_key(SET_VALUES_KEY))
        .ok_or("Internal error: missing values")?;
    let values_obj = values
        .as_object()
        .ok_or("Internal error: values not an object")?;

    // Collect all values into an array
    let values_array = GcRef::new(JsObject::array(0, Arc::clone(&mm)));
    let props = values_obj.own_keys();
    let mut index = 0;

    for prop in props {
        if let Some(value) = values_obj.get(&prop) {
            values_array.set(str_to_key(&index.to_string()), value);
            index += 1;
        }
    }

    values_array.set(str_to_key("length"), VmValue::int32(index));
    Ok(VmValue::array(values_array))
}

/// Set.prototype.keys() - same as values() per spec
fn native_set_keys(args: &[VmValue], mm: Arc<memory::MemoryManager>) -> Result<VmValue, VmError> {
    native_set_values(args, mm)
}

/// Set.prototype.entries() - returns an iterator over [value, value] pairs
fn native_set_entries(
    args: &[VmValue],
    mm: Arc<memory::MemoryManager>,
) -> Result<VmValue, VmError> {
    let set = args.first().ok_or("Set.entries requires a Set")?;

    let set_obj = set.as_object().ok_or("First argument must be a Set")?;

    let is_set = set_obj
        .get(&str_to_key(IS_SET_KEY))
        .unwrap_or_else(VmValue::undefined);
    if is_set.as_boolean() != Some(true) {
        return Err(VmError::type_error("Method Set.prototype.entries called on incompatible receiver"));
    }

    let values = set_obj
        .get(&str_to_key(SET_VALUES_KEY))
        .ok_or("Internal error: missing values")?;
    let values_obj = values
        .as_object()
        .ok_or("Internal error: values not an object")?;

    // Collect all [value, value] pairs into an array
    let entries_array = GcRef::new(JsObject::array(0, Arc::clone(&mm)));
    let props = values_obj.own_keys();
    let mut index = 0;

    for prop in props {
        if let Some(value) = values_obj.get(&prop) {
            // Create [value, value] pair as array
            let pair = GcRef::new(JsObject::array(0, Arc::clone(&mm)));
            pair.set(str_to_key("0"), value.clone());
            pair.set(str_to_key("1"), value);
            pair.set(str_to_key("length"), VmValue::int32(2));

            entries_array.set(str_to_key(&index.to_string()), VmValue::array(pair));
            index += 1;
        }
    }

    entries_array.set(str_to_key("length"), VmValue::int32(index));
    Ok(VmValue::array(entries_array))
}

/// Set.prototype.forEach - returns values for JS to iterate
fn native_set_foreach(args: &[VmValue], mm: Arc<memory::MemoryManager>) -> Result<VmValue, VmError> {
    native_set_values(args, mm)
}

// ============================================================================
// ES2025 Set Methods
// ============================================================================

/// Helper to get all values from a Set
fn get_set_values(set: &VmValue) -> Result<Vec<VmValue>, VmError> {
    let set_obj = set.as_object().ok_or("Expected a Set")?;

    let is_set = set_obj
        .get(&str_to_key(IS_SET_KEY))
        .unwrap_or_else(VmValue::undefined);
    if is_set.as_boolean() != Some(true) {
        return Err(VmError::type_error("Expected a Set"));
    }

    let values = set_obj
        .get(&str_to_key(SET_VALUES_KEY))
        .ok_or("Internal error")?;
    let values_obj = values.as_object().ok_or("Internal error")?;

    let props = values_obj.own_keys();
    let mut result = Vec::new();

    for prop in props {
        if let Some(value) = values_obj.get(&prop) {
            result.push(value);
        }
    }

    Ok(result)
}

/// Set.prototype.union(other)
fn native_set_union(args: &[VmValue], mm: Arc<memory::MemoryManager>) -> Result<VmValue, VmError> {
    let this_values = get_set_values(args.first().ok_or("Missing this")?)?;
    let other_values = get_set_values(args.get(1).ok_or("Missing other Set")?)?;

    // Create new set with values from both
    let result = native_set_new(&[], Arc::clone(&mm))?;

    for value in this_values {
        native_set_add(&[result.clone(), value], Arc::clone(&mm))?;
    }
    for value in other_values {
        native_set_add(&[result.clone(), value], Arc::clone(&mm))?;
    }

    Ok(result)
}

/// Set.prototype.intersection(other)
fn native_set_intersection(
    args: &[VmValue],
    mm: Arc<memory::MemoryManager>,
) -> Result<VmValue, VmError> {
    let set = args.first().ok_or("Missing this")?;
    let other = args.get(1).ok_or("Missing other Set")?;
    let this_values = get_set_values(set)?;

    let result = native_set_new(&[], Arc::clone(&mm))?;

    for value in this_values {
        let has = native_set_has(&[other.clone(), value.clone()], Arc::clone(&mm))?;
        if has.as_boolean() == Some(true) {
            native_set_add(&[result.clone(), value], Arc::clone(&mm))?;
        }
    }

    Ok(result)
}

/// Set.prototype.difference(other)
fn native_set_difference(
    args: &[VmValue],
    mm: Arc<memory::MemoryManager>,
) -> Result<VmValue, VmError> {
    let set = args.first().ok_or("Missing this")?;
    let other = args.get(1).ok_or("Missing other Set")?;
    let this_values = get_set_values(set)?;

    let result = native_set_new(&[], Arc::clone(&mm))?;

    for value in this_values {
        let has = native_set_has(&[other.clone(), value.clone()], Arc::clone(&mm))?;
        if has.as_boolean() != Some(true) {
            native_set_add(&[result.clone(), value], Arc::clone(&mm))?;
        }
    }

    Ok(result)
}

/// Set.prototype.symmetricDifference(other)
fn native_set_symmetric_difference(
    args: &[VmValue],
    mm: Arc<memory::MemoryManager>,
) -> Result<VmValue, VmError> {
    let set = args.first().ok_or("Missing this")?;
    let other = args.get(1).ok_or("Missing other Set")?;
    let this_values = get_set_values(set)?;
    let other_values = get_set_values(other)?;

    let result = native_set_new(&[], Arc::clone(&mm))?;

    // Add values in this but not in other
    for value in &this_values {
        let has = native_set_has(&[other.clone(), value.clone()], Arc::clone(&mm))?;
        if has.as_boolean() != Some(true) {
            native_set_add(&[result.clone(), value.clone()], Arc::clone(&mm))?;
        }
    }

    // Add values in other but not in this
    for value in other_values {
        let has = native_set_has(&[set.clone(), value.clone()], Arc::clone(&mm))?;
        if has.as_boolean() != Some(true) {
            native_set_add(&[result.clone(), value], Arc::clone(&mm))?;
        }
    }

    Ok(result)
}

/// Set.prototype.isSubsetOf(other)
fn native_set_is_subset_of(
    args: &[VmValue],
    mm: Arc<memory::MemoryManager>,
) -> Result<VmValue, VmError> {
    let set = args.first().ok_or("Missing this")?;
    let other = args.get(1).ok_or("Missing other Set")?;
    let this_values = get_set_values(set)?;

    for value in this_values {
        let has = native_set_has(&[other.clone(), value], Arc::clone(&mm))?;
        if has.as_boolean() != Some(true) {
            return Ok(VmValue::boolean(false));
        }
    }

    Ok(VmValue::boolean(true))
}

/// Set.prototype.isSupersetOf(other)
fn native_set_is_superset_of(
    args: &[VmValue],
    mm: Arc<memory::MemoryManager>,
) -> Result<VmValue, VmError> {
    let set = args.first().ok_or("Missing this")?;
    let other = args.get(1).ok_or("Missing other Set")?;
    let other_values = get_set_values(other)?;

    for value in other_values {
        let has = native_set_has(&[set.clone(), value], Arc::clone(&mm))?;
        if has.as_boolean() != Some(true) {
            return Ok(VmValue::boolean(false));
        }
    }

    Ok(VmValue::boolean(true))
}

/// Set.prototype.isDisjointFrom(other)
fn native_set_is_disjoint_from(
    args: &[VmValue],
    mm: Arc<memory::MemoryManager>,
) -> Result<VmValue, VmError> {
    let set = args.first().ok_or("Missing this")?;
    let other = args.get(1).ok_or("Missing other Set")?;
    let this_values = get_set_values(set)?;

    for value in this_values {
        let has = native_set_has(&[other.clone(), value], Arc::clone(&mm))?;
        if has.as_boolean() == Some(true) {
            return Ok(VmValue::boolean(false));
        }
    }

    Ok(VmValue::boolean(true))
}

// ============================================================================
// WeakSet Operations
// ============================================================================

/// Create a new WeakSet
fn native_weakset_new(
    _args: &[VmValue],
    mm: Arc<memory::MemoryManager>,
) -> Result<VmValue, VmError> {
    let set_obj = GcRef::new(JsObject::new(VmValue::null(), Arc::clone(&mm)));

    let values_obj = GcRef::new(JsObject::new(VmValue::null(), Arc::clone(&mm)));
    set_obj.set(str_to_key(SET_VALUES_KEY), VmValue::object(values_obj));
    set_obj.set(str_to_key(IS_WEAKSET_KEY), VmValue::boolean(true));

    Ok(VmValue::object(set_obj))
}

/// Helper to validate WeakSet value (must be object or symbol)
fn validate_weakset_value(value: &VmValue) -> Result<(), VmError> {
    if value.is_object() || value.is_symbol() || value.is_function() {
        Ok(())
    } else {
        Err(VmError::type_error("Invalid value used in weak set"))
    }
}

/// WeakSet.prototype.add(value)
fn native_weakset_add(
    args: &[VmValue],
    _mm: Arc<memory::MemoryManager>,
) -> Result<VmValue, VmError> {
    let set = args.first().ok_or("WeakSet.add requires a WeakSet")?;
    let value = args.get(1).cloned().unwrap_or_else(VmValue::undefined);

    let set_obj = set.as_object().ok_or("First argument must be a WeakSet")?;

    let is_weakset = set_obj
        .get(&str_to_key(IS_WEAKSET_KEY))
        .unwrap_or_else(VmValue::undefined);
    if is_weakset.as_boolean() != Some(true) {
        return Err(VmError::type_error("Method WeakSet.prototype.add called on incompatible receiver"));
    }

    validate_weakset_value(&value)?;

    let values = set_obj
        .get(&str_to_key(SET_VALUES_KEY))
        .ok_or("Internal error: missing values")?;
    let values_obj = values
        .as_object()
        .ok_or("Internal error: values not an object")?;

    let hash_key = value_to_key(&value);
    values_obj.set(str_to_key(&hash_key), value);

    Ok(set.clone())
}

/// WeakSet.prototype.has(value)
fn native_weakset_has(
    args: &[VmValue],
    _mm: Arc<memory::MemoryManager>,
) -> Result<VmValue, VmError> {
    let set = args.first().ok_or("WeakSet.has requires a WeakSet")?;
    let value = args.get(1).cloned().unwrap_or_else(VmValue::undefined);

    let set_obj = set.as_object().ok_or("First argument must be a WeakSet")?;

    let is_weakset = set_obj
        .get(&str_to_key(IS_WEAKSET_KEY))
        .unwrap_or_else(VmValue::undefined);
    if is_weakset.as_boolean() != Some(true) {
        return Err(VmError::type_error("Method WeakSet.prototype.has called on incompatible receiver"));
    }

    if !value.is_object() && !value.is_symbol() && !value.is_function() {
        return Ok(VmValue::boolean(false));
    }

    let values = set_obj
        .get(&str_to_key(SET_VALUES_KEY))
        .ok_or("Internal error: missing values")?;
    let values_obj = values
        .as_object()
        .ok_or("Internal error: values not an object")?;

    let hash_key = value_to_key(&value);
    let entry = values_obj.get(&str_to_key(&hash_key));

    Ok(VmValue::boolean(entry.is_some()))
}

/// WeakSet.prototype.delete(value)
fn native_weakset_delete(
    args: &[VmValue],
    _mm: Arc<memory::MemoryManager>,
) -> Result<VmValue, VmError> {
    let set = args.first().ok_or("WeakSet.delete requires a WeakSet")?;
    let value = args.get(1).cloned().unwrap_or_else(VmValue::undefined);

    let set_obj = set.as_object().ok_or("First argument must be a WeakSet")?;

    let is_weakset = set_obj
        .get(&str_to_key(IS_WEAKSET_KEY))
        .unwrap_or_else(VmValue::undefined);
    if is_weakset.as_boolean() != Some(true) {
        return Err(VmError::type_error("Method WeakSet.prototype.delete called on incompatible receiver"));
    }

    if !value.is_object() && !value.is_symbol() && !value.is_function() {
        return Ok(VmValue::boolean(false));
    }

    let values = set_obj
        .get(&str_to_key(SET_VALUES_KEY))
        .ok_or("Internal error: missing values")?;
    let values_obj = values
        .as_object()
        .ok_or("Internal error: values not an object")?;

    let hash_key = value_to_key(&value);
    let existing = values_obj.get(&str_to_key(&hash_key));

    if existing.is_none() {
        return Ok(VmValue::boolean(false));
    }

    values_obj.delete(&str_to_key(&hash_key));
    Ok(VmValue::boolean(true))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_set_new() {
        let mm = Arc::new(memory::MemoryManager::test());
        let result = native_set_new(&[], Arc::clone(&mm)).unwrap();
        assert!(result.is_object());
    }

    #[test]
    fn test_set_add_has() {
        let mm = Arc::new(memory::MemoryManager::test());
        let set = native_set_new(&[], Arc::clone(&mm)).unwrap();

        let value = VmValue::int32(42);
        let _ = native_set_add(&[set.clone(), value.clone()], Arc::clone(&mm)).unwrap();

        let has = native_set_has(&[set, value], Arc::clone(&mm)).unwrap();
        assert_eq!(has.as_boolean(), Some(true));
    }

    #[test]
    fn test_set_delete() {
        let mm = Arc::new(memory::MemoryManager::test());
        let set = native_set_new(&[], Arc::clone(&mm)).unwrap();
        let value = VmValue::int32(42);

        let _ = native_set_add(&[set.clone(), value.clone()], Arc::clone(&mm)).unwrap();

        let deleted =
            native_set_delete(&[set.clone(), value.clone()], Arc::clone(&mm)).unwrap();
        assert_eq!(deleted.as_boolean(), Some(true));

        let has = native_set_has(&[set, value], Arc::clone(&mm)).unwrap();
        assert_eq!(has.as_boolean(), Some(false));
    }

    #[test]
    fn test_set_size() {
        let mm = Arc::new(memory::MemoryManager::test());
        let set = native_set_new(&[], Arc::clone(&mm)).unwrap();

        let size = native_set_size(std::slice::from_ref(&set), Arc::clone(&mm)).unwrap();
        assert_eq!(size.as_int32(), Some(0));

        let _ = native_set_add(&[set.clone(), VmValue::int32(1)], Arc::clone(&mm)).unwrap();
        let _ = native_set_add(&[set.clone(), VmValue::int32(2)], Arc::clone(&mm)).unwrap();

        let size = native_set_size(&[set], Arc::clone(&mm)).unwrap();
        assert_eq!(size.as_int32(), Some(2));
    }

    #[test]
    fn test_set_clear() {
        let mm = Arc::new(memory::MemoryManager::test());
        let set = native_set_new(&[], Arc::clone(&mm)).unwrap();

        let _ = native_set_add(&[set.clone(), VmValue::int32(1)], Arc::clone(&mm)).unwrap();
        let _ = native_set_clear(std::slice::from_ref(&set), Arc::clone(&mm)).unwrap();

        let size = native_set_size(&[set], Arc::clone(&mm)).unwrap();
        assert_eq!(size.as_int32(), Some(0));
    }

    #[test]
    fn test_set_union() {
        let mm = Arc::new(memory::MemoryManager::test());
        let set1 = native_set_new(&[], Arc::clone(&mm)).unwrap();
        let _ = native_set_add(&[set1.clone(), VmValue::int32(1)], Arc::clone(&mm)).unwrap();
        let _ = native_set_add(&[set1.clone(), VmValue::int32(2)], Arc::clone(&mm)).unwrap();

        let set2 = native_set_new(&[], Arc::clone(&mm)).unwrap();
        let _ = native_set_add(&[set2.clone(), VmValue::int32(2)], Arc::clone(&mm)).unwrap();
        let _ = native_set_add(&[set2.clone(), VmValue::int32(3)], Arc::clone(&mm)).unwrap();

        let result = native_set_union(&[set1, set2], Arc::clone(&mm)).unwrap();
        let size = native_set_size(&[result], Arc::clone(&mm)).unwrap();
        assert_eq!(size.as_int32(), Some(3));
    }

    #[test]
    fn test_set_intersection() {
        let mm = Arc::new(memory::MemoryManager::test());
        let set1 = native_set_new(&[], Arc::clone(&mm)).unwrap();
        let _ = native_set_add(&[set1.clone(), VmValue::int32(1)], Arc::clone(&mm)).unwrap();
        let _ = native_set_add(&[set1.clone(), VmValue::int32(2)], Arc::clone(&mm)).unwrap();

        let set2 = native_set_new(&[], Arc::clone(&mm)).unwrap();
        let _ = native_set_add(&[set2.clone(), VmValue::int32(2)], Arc::clone(&mm)).unwrap();
        let _ = native_set_add(&[set2.clone(), VmValue::int32(3)], Arc::clone(&mm)).unwrap();

        let result = native_set_intersection(&[set1, set2], Arc::clone(&mm)).unwrap();
        let size = native_set_size(&[result], Arc::clone(&mm)).unwrap();
        assert_eq!(size.as_int32(), Some(1));
    }

    #[test]
    fn test_set_difference() {
        let mm = Arc::new(memory::MemoryManager::test());
        let set1 = native_set_new(&[], Arc::clone(&mm)).unwrap();
        let _ = native_set_add(&[set1.clone(), VmValue::int32(1)], Arc::clone(&mm)).unwrap();
        let _ = native_set_add(&[set1.clone(), VmValue::int32(2)], Arc::clone(&mm)).unwrap();

        let set2 = native_set_new(&[], Arc::clone(&mm)).unwrap();
        let _ = native_set_add(&[set2.clone(), VmValue::int32(2)], Arc::clone(&mm)).unwrap();
        let _ = native_set_add(&[set2.clone(), VmValue::int32(3)], Arc::clone(&mm)).unwrap();

        let result = native_set_difference(&[set1, set2], Arc::clone(&mm)).unwrap();
        let size = native_set_size(&[result], Arc::clone(&mm)).unwrap();
        assert_eq!(size.as_int32(), Some(1));
    }

    #[test]
    fn test_weakset_new() {
        let mm = Arc::new(memory::MemoryManager::test());
        let result = native_weakset_new(&[], Arc::clone(&mm)).unwrap();
        assert!(result.is_object());
    }

    #[test]
    fn test_weakset_requires_object() {
        let mm = Arc::new(memory::MemoryManager::test());
        let set = native_weakset_new(&[], Arc::clone(&mm)).unwrap();

        let value = VmValue::int32(42);
        let result = native_weakset_add(&[set, value], Arc::clone(&mm));
        assert!(result.is_err());
    }

    #[test]
    fn test_weakset_with_object() {
        let mm = Arc::new(memory::MemoryManager::test());
        let set = native_weakset_new(&[], Arc::clone(&mm)).unwrap();

        let obj = GcRef::new(JsObject::new(VmValue::null(), Arc::clone(&mm)));
        let value = VmValue::object(obj);

        let _ = native_weakset_add(&[set.clone(), value.clone()], Arc::clone(&mm)).unwrap();

        let has = native_weakset_has(&[set, value], Arc::clone(&mm)).unwrap();
        assert_eq!(has.as_boolean(), Some(true));
    }
}
