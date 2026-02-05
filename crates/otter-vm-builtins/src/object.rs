//! Object built-in
//!
//! Provides Object.keys(), Object.values(), Object.entries(), Object.assign(), Object.hasOwn(),
//! Object.freeze(), Object.isFrozen(), Object.seal(), Object.isSealed(),
//! Object.preventExtensions(), Object.isExtensible(), Object.defineProperty(), Object.create(),
//! Object.is()

use otter_vm_core::error::VmError;
use otter_vm_core::gc::GcRef;
use otter_vm_core::memory::{self, MemoryManager};
use otter_vm_core::object::{JsObject, PropertyAttributes, PropertyDescriptor, PropertyKey};
use otter_vm_core::string::JsString;
use otter_vm_core::value::Value as VmValue;
use otter_vm_runtime::{Op, op_native_with_mm as op_native};
use std::sync::Arc;

/// Object constructor and methods
pub struct ObjectBuiltin;

/// Get Object ops for extension registration
pub fn ops() -> Vec<Op> {
    vec![
        // All ops now use native Value (no JSON conversion)
        op_native("__Object_keys", native_object_keys),
        op_native("__Object_values", native_object_values),
        op_native("__Object_entries", native_object_entries),
        op_native("__Object_assign", native_object_assign),
        op_native("__Object_hasOwn", native_object_has_own),
        // Object identity ops
        op_native("__Object_freeze", native_object_freeze),
        op_native("__Object_isFrozen", native_object_is_frozen),
        op_native("__Object_seal", native_object_seal),
        op_native("__Object_isSealed", native_object_is_sealed),
        op_native(
            "__Object_preventExtensions",
            native_object_prevent_extensions,
        ),
        op_native("__Object_isExtensible", native_object_is_extensible),
        op_native("__Object_defineProperty", native_object_define_property),
        op_native("__Object_create", native_object_create),
        op_native("__Object_is", native_object_is),
        op_native("__Object_rest", native_object_rest),
        op_native(
            "__Object_getOwnPropertyNames",
            native_object_get_own_property_names,
        ),
        op_native(
            "__Object_getOwnPropertySymbols",
            native_object_get_own_property_symbols,
        ),
        op_native(
            "__Object_getOwnPropertyDescriptors",
            native_object_get_own_property_descriptors,
        ),
    ]
}

// ============================================================================
// Native Ops - Work with VM Value directly for object identity operations
// ============================================================================

/// Object.freeze() - native implementation
fn native_object_freeze(
    args: &[VmValue],
    _mm: Arc<memory::MemoryManager>,
) -> Result<VmValue, VmError> {
    let obj = args.first().ok_or("Object.freeze requires an argument")?;
    if let Some(obj_ref) = obj.as_object() {
        obj_ref.freeze();
    }
    // Return the same object (per spec, returns the frozen object)
    Ok(obj.clone())
}

/// Object.isFrozen() - native implementation
fn native_object_is_frozen(
    args: &[VmValue],
    _mm: Arc<memory::MemoryManager>,
) -> Result<VmValue, VmError> {
    let obj = args.first().ok_or("Object.isFrozen requires an argument")?;
    let is_frozen = obj.as_object().map(|o| o.is_frozen()).unwrap_or(true); // Non-objects are considered frozen
    Ok(VmValue::boolean(is_frozen))
}

/// Object.seal() - native implementation
fn native_object_seal(
    args: &[VmValue],
    _mm: Arc<memory::MemoryManager>,
) -> Result<VmValue, VmError> {
    let obj = args.first().ok_or("Object.seal requires an argument")?;
    if let Some(obj_ref) = obj.as_object() {
        obj_ref.seal();
    }
    Ok(obj.clone())
}

/// Object.isSealed() - native implementation
fn native_object_is_sealed(
    args: &[VmValue],
    _mm: Arc<memory::MemoryManager>,
) -> Result<VmValue, VmError> {
    let obj = args.first().ok_or("Object.isSealed requires an argument")?;
    let is_sealed = obj.as_object().map(|o| o.is_sealed()).unwrap_or(true); // Non-objects are considered sealed
    Ok(VmValue::boolean(is_sealed))
}

/// Object.preventExtensions() - native implementation
fn native_object_prevent_extensions(
    args: &[VmValue],
    _mm: Arc<memory::MemoryManager>,
) -> Result<VmValue, VmError> {
    let obj = args
        .first()
        .ok_or("Object.preventExtensions requires an argument")?;
    if let Some(obj_ref) = obj.as_object() {
        obj_ref.prevent_extensions();
    }
    Ok(obj.clone())
}

/// Object.isExtensible() - native implementation
fn native_object_is_extensible(
    args: &[VmValue],
    _mm: Arc<memory::MemoryManager>,
) -> Result<VmValue, VmError> {
    let obj = args
        .first()
        .ok_or("Object.isExtensible requires an argument")?;
    let is_extensible = obj.as_object().map(|o| o.is_extensible()).unwrap_or(false); // Non-objects are not extensible
    Ok(VmValue::boolean(is_extensible))
}

/// Convert a value to a PropertyKey
fn to_property_key(value: &VmValue) -> PropertyKey {
    if let Some(n) = value.as_number()
        && n.fract() == 0.0
        && n >= 0.0
        && n <= u32::MAX as f64
    {
        return PropertyKey::Index(n as u32);
    }
    if let Some(s) = value.as_string() {
        return PropertyKey::String(s);
    }
    if let Some(sym) = value.as_symbol() {
        return PropertyKey::Symbol(sym);
    }
    // Fallback: convert to string
    let s = if value.is_undefined() {
        "undefined"
    } else if value.is_null() {
        "null"
    } else if let Some(b) = value.as_boolean() {
        if b { "true" } else { "false" }
    } else if let Some(n) = value.as_number() {
        // Handle numeric strings
        return PropertyKey::String(JsString::intern(&n.to_string()));
    } else {
        "[object]"
    };
    PropertyKey::String(JsString::intern(s))
}

/// Object.defineProperty(obj, prop, descriptor) - native implementation
fn native_object_define_property(
    args: &[VmValue],
    _mm: Arc<memory::MemoryManager>,
) -> Result<VmValue, VmError> {
    let obj_val = args
        .first()
        .ok_or("Object.defineProperty requires an object")?;
    let key_val = args
        .get(1)
        .ok_or("Object.defineProperty requires a property key")?;
    let descriptor = args
        .get(2)
        .ok_or("Object.defineProperty requires a descriptor")?;

    // First argument must be an object
    let obj = obj_val
        .as_object()
        .ok_or("Object.defineProperty requires the first argument to be an object")?;

    let key = to_property_key(key_val);

    let Some(attr_obj) = descriptor.as_object() else {
        return Err(VmError::type_error("Property descriptor must be an object"));
    };

    // Helper to read boolean fields
    let read_bool = |name: &str, default: bool| -> bool {
        attr_obj
            .get(&name.into())
            .and_then(|v| v.as_boolean())
            .unwrap_or(default)
    };

    // Check for accessor descriptor (get/set)
    let get = attr_obj.get(&"get".into());
    let set = attr_obj.get(&"set".into());

    if get.is_some() || set.is_some() {
        // Accessor descriptor
        let enumerable = read_bool("enumerable", false);
        let configurable = read_bool("configurable", false);

        // Get existing accessor if any (for partial updates)
        let existing = obj.get_own_property_descriptor(&key);
        let (mut existing_get, mut existing_set) = match existing {
            Some(PropertyDescriptor::Accessor { get, set, .. }) => (get, set),
            _ => (None, None),
        };

        let get = get
            .filter(|v| !v.is_undefined())
            .or_else(|| existing_get.take());
        let set = set
            .filter(|v| !v.is_undefined())
            .or_else(|| existing_set.take());

        let attrs = PropertyAttributes {
            writable: false,
            enumerable,
            configurable,
        };

        obj.define_property(
            key,
            PropertyDescriptor::Accessor {
                get,
                set,
                attributes: attrs,
            },
        );
    } else {
        // Data descriptor
        let value = attr_obj
            .get(&"value".into())
            .unwrap_or(VmValue::undefined());
        let writable = read_bool("writable", false);
        let enumerable = read_bool("enumerable", false);
        let configurable = read_bool("configurable", false);

        let attrs = PropertyAttributes {
            writable,
            enumerable,
            configurable,
        };

        obj.define_property(key, PropertyDescriptor::data_with_attrs(value, attrs));
    }

    // Return the object (per spec)
    Ok(obj_val.clone())
}

/// Object.create(proto, propertiesObject?) - native implementation
fn native_object_create(
    args: &[VmValue],
    mm: Arc<memory::MemoryManager>,
) -> Result<VmValue, VmError> {
    let proto_val = args
        .first()
        .ok_or("Object.create requires a prototype argument")?;

    // Prototype must be object or null
    let prototype = if proto_val.is_null() || proto_val.as_object().is_some() {
        proto_val.clone()
    } else {
        return Err(VmError::type_error("Object prototype may only be an Object or null"));
    };

    let new_obj = GcRef::new(JsObject::new(prototype, mm));

    // Handle optional properties object (second argument)
    if let Some(props_val) = args.get(1) {
        if !props_val.is_undefined() {
            let props = props_val
                .as_object()
                .ok_or("Properties argument must be an object")?;

            // For each enumerable own property of props, call defineProperty
            for key in props.own_keys() {
                if let Some(descriptor) = props.get(&key) {
                    if let Some(attr_obj) = descriptor.as_object() {
                        // Helper to read boolean fields
                        let read_bool = |name: &str, default: bool| -> bool {
                            attr_obj
                                .get(&name.into())
                                .and_then(|v| v.as_boolean())
                                .unwrap_or(default)
                        };

                        let get = attr_obj.get(&"get".into());
                        let set = attr_obj.get(&"set".into());

                        if get.is_some() || set.is_some() {
                            let enumerable = read_bool("enumerable", false);
                            let configurable = read_bool("configurable", false);

                            let attrs = PropertyAttributes {
                                writable: false,
                                enumerable,
                                configurable,
                            };

                            new_obj.define_property(
                                key,
                                PropertyDescriptor::Accessor {
                                    get: get.filter(|v| !v.is_undefined()),
                                    set: set.filter(|v| !v.is_undefined()),
                                    attributes: attrs,
                                },
                            );
                        } else {
                            let value = attr_obj
                                .get(&"value".into())
                                .unwrap_or(VmValue::undefined());
                            let writable = read_bool("writable", false);
                            let enumerable = read_bool("enumerable", false);
                            let configurable = read_bool("configurable", false);

                            let attrs = PropertyAttributes {
                                writable,
                                enumerable,
                                configurable,
                            };

                            new_obj.define_property(
                                key,
                                PropertyDescriptor::data_with_attrs(value, attrs),
                            );
                        }
                    }
                }
            }
        }
    }

    Ok(VmValue::object(new_obj))
}

/// Object.is(value1, value2) - SameValue algorithm
fn native_object_is(args: &[VmValue], _mm: Arc<memory::MemoryManager>) -> Result<VmValue, VmError> {
    let v1 = args.first().cloned().unwrap_or(VmValue::undefined());
    let v2 = args.get(1).cloned().unwrap_or(VmValue::undefined());

    // SameValue algorithm:
    // 1. NaN === NaN is true
    // 2. +0 !== -0 (different from ===)
    // 3. Otherwise, same as ===

    let result = if let (Some(n1), Some(n2)) = (v1.as_number(), v2.as_number()) {
        if n1.is_nan() && n2.is_nan() {
            // NaN is same as NaN
            true
        } else if n1 == 0.0 && n2 == 0.0 {
            // Check sign: 1.0/+0.0 = +inf, 1.0/-0.0 = -inf
            (1.0_f64 / n1).is_sign_positive() == (1.0_f64 / n2).is_sign_positive()
        } else {
            n1 == n2
        }
    } else if v1.is_undefined() && v2.is_undefined() {
        true
    } else if v1.is_null() && v2.is_null() {
        true
    } else if let (Some(b1), Some(b2)) = (v1.as_boolean(), v2.as_boolean()) {
        b1 == b2
    } else if let (Some(s1), Some(s2)) = (v1.as_string(), v2.as_string()) {
        s1.as_str() == s2.as_str()
    } else if let (Some(sym1), Some(sym2)) = (v1.as_symbol(), v2.as_symbol()) {
        sym1.id == sym2.id
    } else if let (Some(o1), Some(o2)) = (v1.as_object(), v2.as_object()) {
        // Same reference check
        o1.as_ptr() == o2.as_ptr()
    } else if let (Some(f1), Some(f2)) = (v1.as_function(), v2.as_function()) {
        // Same closure check
        Arc::ptr_eq(&f1.module, &f2.module) && f1.function_index == f2.function_index
    } else {
        false
    };

    Ok(VmValue::boolean(result))
}

// ============================================================================
// All functions now use native Value directly (no JSON conversion)
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_native_object_freeze() {
        use otter_vm_core::object::JsObject;
        use std::sync::Arc;

        let memory_manager = Arc::new(memory::MemoryManager::test());
        let obj = GcRef::new(JsObject::new(VmValue::null(), memory_manager.clone()));
        obj.set("a".into(), VmValue::int32(1));

        let value = VmValue::object(obj.clone());
        let result = native_object_freeze(std::slice::from_ref(&value), memory_manager).unwrap();

        assert!(obj.is_frozen());
        // Result should be the same value
        assert!(result.is_object());
    }

    #[test]
    fn test_native_object_is_frozen() {
        let memory_manager = Arc::new(memory::MemoryManager::test());
        let obj = GcRef::new(JsObject::new(VmValue::null(), memory_manager.clone()));
        let value = VmValue::object(obj.clone());

        // Initially not frozen
        let result =
            native_object_is_frozen(std::slice::from_ref(&value), memory_manager.clone()).unwrap();
        assert_eq!(result.as_boolean(), Some(false));

        // After freeze
        obj.freeze();
        let result = native_object_is_frozen(std::slice::from_ref(&value), memory_manager).unwrap();
        assert_eq!(result.as_boolean(), Some(true));
    }

    #[test]
    fn test_native_object_seal() {
        use otter_vm_core::object::JsObject;
        use std::sync::Arc;

        let memory_manager = Arc::new(memory::MemoryManager::test());
        let obj = GcRef::new(JsObject::new(VmValue::null(), memory_manager.clone()));
        obj.set("a".into(), VmValue::int32(1));

        let value = VmValue::object(obj.clone());
        let _ = native_object_seal(std::slice::from_ref(&value), memory_manager).unwrap();

        assert!(obj.is_sealed());
    }

    #[test]
    fn test_native_object_prevent_extensions() {
        use otter_vm_core::object::JsObject;
        use std::sync::Arc;

        let memory_manager = Arc::new(memory::MemoryManager::test());
        let obj = GcRef::new(JsObject::new(VmValue::null(), memory_manager.clone()));
        let value = VmValue::object(obj.clone());

        // Initially extensible
        assert!(obj.is_extensible());

        let _ =
            native_object_prevent_extensions(std::slice::from_ref(&value), memory_manager).unwrap();

        // Now not extensible
        assert!(!obj.is_extensible());
    }

    #[test]
    fn test_native_object_rest() {
        let memory_manager = Arc::new(memory::MemoryManager::test());
        let obj = GcRef::new(JsObject::new(VmValue::null(), memory_manager.clone()));
        obj.set("a".into(), VmValue::int32(1));
        obj.set("b".into(), VmValue::int32(2));
        obj.set("c".into(), VmValue::int32(3));

        let excluded = GcRef::new(JsObject::array(2, memory_manager.clone()));
        excluded.set(
            PropertyKey::Index(0),
            VmValue::string(JsString::intern("a")),
        );
        excluded.set(
            PropertyKey::Index(1),
            VmValue::string(JsString::intern("b")),
        );

        let args = vec![VmValue::object(obj), VmValue::object(excluded)];
        let result = native_object_rest(&args, memory_manager).unwrap();
        let result_obj = result.as_object().unwrap();

        assert_eq!(result_obj.own_keys().len(), 1);
        assert_eq!(
            result_obj.get(&PropertyKey::String(JsString::intern("c"))),
            Some(VmValue::int32(3))
        );
    }
}

/// Object rest helper: copy own enumerable properties from source excluding keys in excluded_keys_array
fn native_object_rest(args: &[VmValue], mm: Arc<memory::MemoryManager>) -> Result<VmValue, VmError> {
    let source = args.first().ok_or("Object rest requires a source object")?;
    let excluded_keys_val = args
        .get(1)
        .ok_or("Object rest requires an excluded keys array")?;

    // If source is null or undefined, throw TypeError (spec requires ObjectCoerce)
    if source.is_nullish() {
        return Err(VmError::type_error("Cannot destructure null or undefined"));
    }

    // Coerce to object
    let source_obj = source
        .as_object()
        .or_else(|| {
            // Primitive coercion (simplified for now)
            None
        })
        .ok_or("Source must be an object")?;

    let excluded_keys_obj = excluded_keys_val
        .as_object()
        .ok_or("Excluded keys must be an array")?;

    // Build a set of excluded keys for efficient lookup
    let mut excluded = std::collections::HashSet::new();
    // Assuming excluded_keys_obj is an array (has length and integer keys)
    if let Some(len_val) = excluded_keys_obj.get(&PropertyKey::String(JsString::intern("length"))) {
        if let Some(len) = len_val.as_number() {
            for i in 0..(len as u32) {
                if let Some(key_val) = excluded_keys_obj.get(&PropertyKey::Index(i)) {
                    excluded.insert(to_property_key(&key_val));
                }
            }
        }
    }

    let new_obj = GcRef::new(JsObject::new(
        VmValue::object(GcRef::new(JsObject::new(VmValue::null(), mm.clone()))),
        mm,
    )); // Should probably use Object.prototype

    for key in source_obj.own_keys() {
        // Only enumerable own properties
        if let Some(desc) = source_obj.get_own_property_descriptor(&key) {
            if desc.enumerable() && !excluded.contains(&key) {
                if let Some(val) = source_obj.get(&key) {
                    new_obj.set(key, val);
                }
            }
        }
    }

    Ok(VmValue::object(new_obj))
}

/// Object.getOwnPropertyNames(obj) - native implementation
fn native_object_get_own_property_names(
    args: &[VmValue],
    mm: Arc<memory::MemoryManager>,
) -> Result<VmValue, VmError> {
    let obj_val = args
        .first()
        .ok_or("Object.getOwnPropertyNames requires an argument")?;

    // If not an object, return empty array for now (per builtins.js current behavior)
    // Spec says: ToObject(obj) -> GetOwnPropertyKeys(obj, string)
    let Some(obj) = obj_val.as_object() else {
        return Ok(VmValue::object(GcRef::new(JsObject::array(
            0,
            Arc::clone(&mm),
        ))));
    };

    let keys = obj.own_keys();
    let mut names = Vec::new();

    for key in keys {
        if let PropertyKey::String(s) = key {
            names.push(VmValue::string(s));
        } else if let PropertyKey::Index(i) = key {
            names.push(VmValue::string(JsString::intern(&i.to_string())));
        }
    }

    let result = GcRef::new(JsObject::array(names.len(), Arc::clone(&mm)));
    for (i, name) in names.into_iter().enumerate() {
        result.set(PropertyKey::Index(i as u32), name);
    }

    Ok(VmValue::array(result))
}

/// Object.keys(obj) - native implementation
fn native_object_keys(args: &[VmValue], mm: Arc<MemoryManager>) -> Result<VmValue, VmError> {
    let obj_val = args.get(0).ok_or("Object.keys requires an object")?;
    let obj = obj_val
        .as_object()
        .ok_or("Object.keys argument must be an object")?;

    let keys = obj.own_keys();
    let mut names = Vec::new();

    for key in keys {
        if let PropertyKey::String(s) = &key {
            if let Some(desc) = obj.get_own_property_descriptor(&PropertyKey::String(s.clone())) {
                if desc.enumerable() {
                    names.push(VmValue::string(s.clone()));
                }
            }
        }
    }

    let result = GcRef::new(JsObject::array(names.len(), Arc::clone(&mm)));
    for (i, name) in names.into_iter().enumerate() {
        result.set(PropertyKey::Index(i as u32), name);
    }

    Ok(VmValue::array(result))
}

/// Object.values(obj) - native implementation
fn native_object_values(args: &[VmValue], mm: Arc<MemoryManager>) -> Result<VmValue, VmError> {
    let obj_val = args.get(0).ok_or("Object.values requires an object")?;
    let obj = obj_val
        .as_object()
        .ok_or("Object.values argument must be an object")?;

    let keys = obj.own_keys();
    let mut values = Vec::new();

    for key in keys {
        if let PropertyKey::String(s) = &key {
            if let Some(desc) = obj.get_own_property_descriptor(&PropertyKey::String(s.clone())) {
                if desc.enumerable() {
                    if let Some(value) = obj.get(&PropertyKey::String(s.clone())) {
                        values.push(value);
                    }
                }
            }
        }
    }

    let result = GcRef::new(JsObject::array(values.len(), Arc::clone(&mm)));
    for (i, value) in values.into_iter().enumerate() {
        result.set(PropertyKey::Index(i as u32), value);
    }

    Ok(VmValue::array(result))
}

/// Object.entries(obj) - native implementation
fn native_object_entries(args: &[VmValue], mm: Arc<MemoryManager>) -> Result<VmValue, VmError> {
    let obj_val = args.get(0).ok_or("Object.entries requires an object")?;
    let obj = obj_val
        .as_object()
        .ok_or("Object.entries argument must be an object")?;

    let keys = obj.own_keys();
    let mut entries = Vec::new();

    for key in keys {
        if let PropertyKey::String(s) = &key {
            if let Some(desc) = obj.get_own_property_descriptor(&PropertyKey::String(s.clone())) {
                if desc.enumerable() {
                    if let Some(value) = obj.get(&PropertyKey::String(s.clone())) {
                        // Create [key, value] array
                        let entry = GcRef::new(JsObject::array(2, Arc::clone(&mm)));
                        entry.set(PropertyKey::Index(0), VmValue::string(s.clone()));
                        entry.set(PropertyKey::Index(1), value);
                        entries.push(VmValue::array(entry));
                    }
                }
            }
        }
    }

    let result = GcRef::new(JsObject::array(entries.len(), Arc::clone(&mm)));
    for (i, entry) in entries.into_iter().enumerate() {
        result.set(PropertyKey::Index(i as u32), entry);
    }

    Ok(VmValue::array(result))
}

/// Object.assign(target, ...sources) - native implementation
fn native_object_assign(args: &[VmValue], _mm: Arc<MemoryManager>) -> Result<VmValue, VmError> {
    let target_val = args.get(0).ok_or("Object.assign requires at least one argument")?;
    let target = target_val
        .as_object()
        .ok_or("Object.assign target must be an object")?;

    // Copy properties from each source to target
    for source_val in &args[1..] {
        if source_val.is_null() || source_val.is_undefined() {
            continue; // Skip null/undefined sources
        }

        if let Some(source) = source_val.as_object() {
            for key in source.own_keys() {
                if let Some(value) = source.get(&key) {
                    target.set(key, value);
                }
            }
        }
    }

    Ok(target_val.clone())
}

/// Object.hasOwn(obj, prop) - native implementation
fn native_object_has_own(args: &[VmValue], _mm: Arc<MemoryManager>) -> Result<VmValue, VmError> {
    let obj_val = args.get(0).ok_or("Object.hasOwn requires two arguments")?;
    let prop_val = args.get(1).ok_or("Object.hasOwn requires two arguments")?;

    let obj = obj_val
        .as_object()
        .ok_or("Object.hasOwn first argument must be an object")?;

    // Convert property to key
    let key = if let Some(s) = prop_val.as_string() {
        PropertyKey::String(s.clone())
    } else if let Some(sym) = prop_val.as_symbol() {
        PropertyKey::Symbol(sym)
    } else {
        // Convert to string
        let s = JsString::intern(&format!("{:?}", prop_val));
        PropertyKey::String(s)
    };

    let has_own = obj.has_own(&key);
    Ok(VmValue::boolean(has_own))
}

/// Object.getOwnPropertySymbols(obj) - native implementation
fn native_object_get_own_property_symbols(
    args: &[VmValue],
    mm: Arc<memory::MemoryManager>,
) -> Result<VmValue, VmError> {
    let obj_val = args
        .first()
        .ok_or("Object.getOwnPropertySymbols requires an argument")?;

    // If not an object, return empty array
    let Some(obj) = obj_val.as_object() else {
        return Ok(VmValue::object(GcRef::new(JsObject::array(
            0,
            Arc::clone(&mm),
        ))));
    };

    let keys = obj.own_keys();
    let mut symbols: Vec<VmValue> = Vec::new();

    // Collect all symbol keys
    for key in keys {
        if let PropertyKey::Symbol(sym) = key {
            symbols.push(VmValue::symbol(sym));
        }
    }

    let result = GcRef::new(JsObject::array(symbols.len(), Arc::clone(&mm)));
    for (i, sym) in symbols.into_iter().enumerate() {
        result.set(PropertyKey::Index(i as u32), sym);
    }

    Ok(VmValue::array(result))
}

/// Helper to convert a PropertyDescriptor to a JS object
fn descriptor_to_object(
    desc: PropertyDescriptor,
    mm: &Arc<memory::MemoryManager>,
) -> GcRef<JsObject> {
    let desc_obj = GcRef::new(JsObject::new(VmValue::null(), Arc::clone(mm)));
    match desc {
        PropertyDescriptor::Data { value, attributes } => {
            desc_obj.set("value".into(), value);
            desc_obj.set("writable".into(), VmValue::boolean(attributes.writable));
            desc_obj.set("enumerable".into(), VmValue::boolean(attributes.enumerable));
            desc_obj.set(
                "configurable".into(),
                VmValue::boolean(attributes.configurable),
            );
        }
        PropertyDescriptor::Accessor {
            get,
            set,
            attributes,
        } => {
            desc_obj.set("get".into(), get.unwrap_or(VmValue::undefined()));
            desc_obj.set("set".into(), set.unwrap_or(VmValue::undefined()));
            desc_obj.set("enumerable".into(), VmValue::boolean(attributes.enumerable));
            desc_obj.set(
                "configurable".into(),
                VmValue::boolean(attributes.configurable),
            );
        }
        PropertyDescriptor::Deleted => {}
    }
    desc_obj
}

/// Object.getOwnPropertyDescriptors(obj)
fn native_object_get_own_property_descriptors(
    args: &[VmValue],
    mm: Arc<memory::MemoryManager>,
) -> Result<VmValue, VmError> {
    let obj_val = args
        .first()
        .ok_or("Object.getOwnPropertyDescriptors requires a target")?;

    // If not an object, return empty object (shams/wrappers should handle this)
    let Some(obj) = obj_val.as_object() else {
        return Ok(VmValue::object(GcRef::new(JsObject::new(
            VmValue::null(),
            Arc::clone(&mm),
        ))));
    };

    let result = GcRef::new(JsObject::new(VmValue::null(), Arc::clone(&mm)));
    let keys = obj.own_keys();

    for key in keys {
        if let Some(prop_desc) = obj.get_own_property_descriptor(&key) {
            let desc_obj = descriptor_to_object(prop_desc, &mm);
            result.set(key, VmValue::object(desc_obj));
        }
    }

    Ok(VmValue::object(result))
}
