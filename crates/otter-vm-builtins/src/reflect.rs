//! Reflect built-in
//!
//! Provides Reflect static methods that mirror the internal operations:
//! - `Reflect.get(target, propertyKey, receiver?)`
//! - `Reflect.set(target, propertyKey, value, receiver?)`
//! - `Reflect.has(target, propertyKey)`
//! - `Reflect.deleteProperty(target, propertyKey)`
//! - `Reflect.ownKeys(target)`
//! - `Reflect.getOwnPropertyDescriptor(target, propertyKey)`
//! - `Reflect.defineProperty(target, propertyKey, attributes)`
//! - `Reflect.getPrototypeOf(target)`
//! - `Reflect.setPrototypeOf(target, prototype)`
//! - `Reflect.isExtensible(target)`
//! - `Reflect.preventExtensions(target)`
//! - `Reflect.apply(target, thisArgument, argumentsList)`
//! - `Reflect.construct(target, argumentsList, newTarget?)`

use otter_vm_core::gc::GcRef;
use otter_vm_core::object::{JsObject, PropertyKey};
use otter_vm_core::string::JsString;
use otter_vm_core::value::Value as VmValue;
use otter_vm_core::memory;
use otter_vm_runtime::{Op, op_native_with_mm as op_native};
use std::sync::Arc;

/// Get Reflect ops for extension registration
pub fn ops() -> Vec<Op> {
    vec![
        op_native("__Reflect_get", native_reflect_get),
        op_native("__Reflect_set", native_reflect_set),
        op_native("__Reflect_has", native_reflect_has),
        op_native("__Reflect_deleteProperty", native_reflect_delete_property),
        op_native("__Reflect_ownKeys", native_reflect_own_keys),
        op_native(
            "__Reflect_getOwnPropertyDescriptor",
            native_reflect_get_own_property_descriptor,
        ),
        op_native("__Reflect_defineProperty", native_reflect_define_property),
        op_native("__Reflect_getPrototypeOf", native_reflect_get_prototype_of),
        op_native("__Reflect_setPrototypeOf", native_reflect_set_prototype_of),
        op_native("__Reflect_isExtensible", native_reflect_is_extensible),
        op_native(
            "__Reflect_preventExtensions",
            native_reflect_prevent_extensions,
        ),
    ]
}

// ============================================================================
// Helper Functions
// ============================================================================

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
        return PropertyKey::Symbol(sym.id);
    }
    // Fallback: for primitives, create a string key
    // This is a simplified conversion - in full ES spec this would call ToString
    let s = if value.is_undefined() {
        "undefined"
    } else if value.is_null() {
        "null"
    } else if let Some(b) = value.as_boolean() {
        if b { "true" } else { "false" }
    } else {
        // For other types, use a placeholder
        "[object]"
    };
    PropertyKey::String(JsString::intern(s))
}

/// Get object from value, checking for proxy first
fn get_target_object(value: &VmValue) -> Result<GcRef<JsObject>, String> {
    // Check if it's a proxy first
    if let Some(proxy) = value.as_proxy() {
        return proxy
            .target()
            .ok_or_else(|| "Cannot perform operation on a revoked proxy".to_string());
    }

    value.as_object().ok_or_else(|| {
        format!(
            "Reflect method requires an object target (got {}: {:?})",
            value.type_of(),
            value
        )
    })
}

// ============================================================================
// Native Operations
// ============================================================================

/// Reflect.get(target, propertyKey, receiver?)
/// Returns the value of the property
fn native_reflect_get(
    args: &[VmValue],
    _mm: Arc<memory::MemoryManager>,
) -> Result<VmValue, String> {
    let target = args
        .first()
        .ok_or("Reflect.get requires a target argument")?;
    let property_key = args
        .get(1)
        .ok_or("Reflect.get requires a propertyKey argument")?;
    // receiver is optional (args[2]) - used for getter's this value

    let obj = get_target_object(target)?;
    let key = to_property_key(property_key);

    Ok(obj.get(&key).unwrap_or(VmValue::undefined()))
}

/// Reflect.set(target, propertyKey, value, receiver?)
/// Returns true if the property was set successfully
fn native_reflect_set(
    args: &[VmValue],
    _mm: Arc<memory::MemoryManager>,
) -> Result<VmValue, String> {
    let target = args
        .first()
        .ok_or("Reflect.set requires a target argument")?;
    let property_key = args
        .get(1)
        .ok_or("Reflect.set requires a propertyKey argument")?;
    let value = args.get(2).cloned().unwrap_or(VmValue::undefined());
    // receiver is optional (args[3])

    let obj = get_target_object(target)?;
    let key = to_property_key(property_key);

    obj.set(key, value);
    Ok(VmValue::boolean(true))
}

/// Reflect.has(target, propertyKey)
/// Returns true if the property exists
fn native_reflect_has(
    args: &[VmValue],
    _mm: Arc<memory::MemoryManager>,
) -> Result<VmValue, String> {
    let target = args
        .first()
        .ok_or("Reflect.has requires a target argument")?;
    let property_key = args
        .get(1)
        .ok_or("Reflect.has requires a propertyKey argument")?;

    let obj = get_target_object(target)?;
    let key = to_property_key(property_key);

    Ok(VmValue::boolean(obj.has(&key)))
}

/// Reflect.deleteProperty(target, propertyKey)
/// Returns true if the property was deleted
fn native_reflect_delete_property(
    args: &[VmValue],
    _mm: Arc<memory::MemoryManager>,
) -> Result<VmValue, String> {
    let target = args
        .first()
        .ok_or("Reflect.deleteProperty requires a target argument")?;
    let property_key = args
        .get(1)
        .ok_or("Reflect.deleteProperty requires a propertyKey argument")?;

    let obj = get_target_object(target)?;
    let key = to_property_key(property_key);

    let deleted = obj.delete(&key);
    Ok(VmValue::boolean(deleted))
}

/// Reflect.ownKeys(target)
/// Returns an array of the target's own property keys
fn native_reflect_own_keys(
    args: &[VmValue],
    mm: Arc<memory::MemoryManager>,
) -> Result<VmValue, String> {
    let target = args
        .first()
        .ok_or("Reflect.ownKeys requires a target argument")?;

    // Special handling for functions - they have virtual 'length', 'name', 'prototype' properties
    if let Some(closure) = target.as_function() {
        let mut keys = vec![
            JsString::intern("length"),
            JsString::intern("name"),
            JsString::intern("prototype"),
        ];
        // Also include any properties on the closure's object
        let obj_keys = closure.object.own_keys();
        for key in obj_keys {
            if let PropertyKey::String(s) = key {
                if s.as_str() != "length" && s.as_str() != "name" && s.as_str() != "prototype" {
                    keys.push(s);
                }
            }
        }
        let result = GcRef::new(JsObject::array(keys.len(), Arc::clone(&mm)));
        for (i, key) in keys.into_iter().enumerate() {
            result.set(PropertyKey::Index(i as u32), VmValue::string(key));
        }
        return Ok(VmValue::array(result));
    }

    // Handle native functions
    if target.as_native_function().is_some() {
        let keys = vec![
            JsString::intern("length"),
            JsString::intern("name"),
        ];
        let result = GcRef::new(JsObject::array(keys.len(), Arc::clone(&mm)));
        for (i, key) in keys.into_iter().enumerate() {
            result.set(PropertyKey::Index(i as u32), VmValue::string(key));
        }
        return Ok(VmValue::array(result));
    }

    let obj = get_target_object(target)?;
    let keys = obj.own_keys();

    // Filter to only string and index keys (symbols require registry lookup)
    let filtered_keys: Vec<_> = keys
        .into_iter()
        .filter(|k| !matches!(k, PropertyKey::Symbol(_)))
        .collect();

    let result = GcRef::new(JsObject::array(filtered_keys.len(), Arc::clone(&mm)));
    for (i, key) in filtered_keys.into_iter().enumerate() {
        let key_val = match key {
            PropertyKey::String(s) => VmValue::string(s),
            PropertyKey::Index(n) => VmValue::string(JsString::intern(&n.to_string())),
            PropertyKey::Symbol(_) => unreachable!(), // filtered out above
        };
        result.set(PropertyKey::Index(i as u32), key_val);
    }

    Ok(VmValue::array(result))
}

/// Reflect.getOwnPropertyDescriptor(target, propertyKey)
/// Returns the property descriptor or undefined
fn native_reflect_get_own_property_descriptor(
    args: &[VmValue],
    mm: Arc<memory::MemoryManager>,
) -> Result<VmValue, String> {
    let target = args
        .first()
        .ok_or("Reflect.getOwnPropertyDescriptor requires a target argument")?;
    let property_key = args
        .get(1)
        .ok_or("Reflect.getOwnPropertyDescriptor requires a propertyKey argument")?;

    let key = to_property_key(property_key);

    // Try to get as object (works for objects, functions, native functions, etc.)
    let obj = match target.as_object() {
        Some(o) => o,
        None => {
            // If not an object, use get_target_object which handles proxies
            get_target_object(target)?
        }
    };

    // Check property descriptor first (for proper attribute handling)
    use otter_vm_core::object::PropertyDescriptor;
    if let Some(prop_desc) = obj.lookup_property_descriptor(&key) {
        match prop_desc {
            PropertyDescriptor::Data { value, attributes } => {
                let desc = GcRef::new(JsObject::new(None, Arc::clone(&mm)));
                desc.set("value".into(), value);
                desc.set("writable".into(), VmValue::boolean(attributes.writable));
                desc.set("enumerable".into(), VmValue::boolean(attributes.enumerable));
                desc.set("configurable".into(), VmValue::boolean(attributes.configurable));
                return Ok(VmValue::object(desc));
            }
            PropertyDescriptor::Accessor { get, set, attributes } => {
                let desc = GcRef::new(JsObject::new(None, Arc::clone(&mm)));
                desc.set("get".into(), get.unwrap_or(VmValue::undefined()));
                desc.set("set".into(), set.unwrap_or(VmValue::undefined()));
                desc.set("enumerable".into(), VmValue::boolean(attributes.enumerable));
                desc.set("configurable".into(), VmValue::boolean(attributes.configurable));
                return Ok(VmValue::object(desc));
            }
            PropertyDescriptor::Deleted => {
                // Property was deleted
                return Ok(VmValue::undefined());
            }
        }
    }

    // Check if property exists
    if let Some(value) = obj.get(&key) {
        let desc = GcRef::new(JsObject::new(None, Arc::clone(&mm)));
        desc.set("value".into(), value);
        desc.set("writable".into(), VmValue::boolean(true));
        desc.set("enumerable".into(), VmValue::boolean(true));
        desc.set("configurable".into(), VmValue::boolean(true));
        Ok(VmValue::object(desc))
    } else {
        Ok(VmValue::undefined())
    }
}

/// Reflect.defineProperty(target, propertyKey, attributes)
/// Returns true if the property was defined successfully
fn native_reflect_define_property(
    args: &[VmValue],
    _mm: Arc<memory::MemoryManager>,
) -> Result<VmValue, String> {
    let target = args
        .first()
        .ok_or("Reflect.defineProperty requires a target argument")?;
    let property_key = args
        .get(1)
        .ok_or("Reflect.defineProperty requires a propertyKey argument")?;
    let attributes = args
        .get(2)
        .ok_or("Reflect.defineProperty requires an attributes argument")?;

    let obj = get_target_object(target)?;
    let key = to_property_key(property_key);

    let Some(attr_obj) = attributes.as_object() else {
        return Err("Reflect.defineProperty requires attributes to be an object".to_string());
    };

    // Helper to read boolean fields with default true (the common case for builtins)
    let read_bool = |name: &str, default: bool| -> bool {
        attr_obj
            .get(&name.into())
            .and_then(|v| v.as_boolean())
            .unwrap_or(default)
    };

    let enumerable = read_bool("enumerable", true);
    let configurable = read_bool("configurable", true);
    let writable = read_bool("writable", true);

    // Accessor descriptor: { get?, set? }
    let get = attr_obj.get(&"get".into());
    let set = attr_obj.get(&"set".into());
    if get.is_some() || set.is_some() {
        use otter_vm_core::object::{PropertyAttributes, PropertyDescriptor};

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

        let ok = obj.define_property(
            key,
            PropertyDescriptor::Accessor {
                get,
                set,
                attributes: attrs,
            },
        );
        return Ok(VmValue::boolean(ok));
    }

    // Data descriptor: { value, writable?, enumerable?, configurable? }
    if let Some(value) = attr_obj.get(&"value".into()) {
        use otter_vm_core::object::{PropertyAttributes, PropertyDescriptor};

        let attrs = PropertyAttributes {
            writable,
            enumerable,
            configurable,
        };
        let ok = obj.define_property(key, PropertyDescriptor::data_with_attrs(value, attrs));
        return Ok(VmValue::boolean(ok));
    }

    Ok(VmValue::boolean(true))
}

/// Reflect.getPrototypeOf(target)
/// Returns the prototype of the target
fn native_reflect_get_prototype_of(
    args: &[VmValue],
    _mm: Arc<memory::MemoryManager>,
) -> Result<VmValue, String> {
    let target = args
        .first()
        .ok_or("Reflect.getPrototypeOf requires a target argument")?;

    let obj = get_target_object(target)?;

    match obj.prototype() {
        Some(proto) => Ok(VmValue::object(proto)),
        None => Ok(VmValue::null()),
    }
}

/// Reflect.setPrototypeOf(target, prototype)
/// Returns true if the prototype was set successfully
fn native_reflect_set_prototype_of(
    args: &[VmValue],
    _mm: Arc<memory::MemoryManager>,
) -> Result<VmValue, String> {
    let target = args
        .first()
        .ok_or("Reflect.setPrototypeOf requires a target argument")?;
    let prototype = args
        .get(1)
        .ok_or("Reflect.setPrototypeOf requires a prototype argument")?;

    let obj = get_target_object(target)?;

    let new_proto = if prototype.is_null() {
        None
    } else if let Some(proto_obj) = prototype.as_object() {
        Some(proto_obj)
    } else {
        return Err("Prototype must be an object or null".to_string());
    };

    let success = obj.set_prototype(new_proto);
    Ok(VmValue::boolean(success))
}

/// Reflect.isExtensible(target)
/// Returns true if the target is extensible
fn native_reflect_is_extensible(
    args: &[VmValue],
    _mm: Arc<memory::MemoryManager>,
) -> Result<VmValue, String> {
    let target = args
        .first()
        .ok_or("Reflect.isExtensible requires a target argument")?;

    let obj = get_target_object(target)?;
    Ok(VmValue::boolean(obj.is_extensible()))
}

/// Reflect.preventExtensions(target)
/// Returns true if extensions were prevented
fn native_reflect_prevent_extensions(
    args: &[VmValue],
    _mm: Arc<memory::MemoryManager>,
) -> Result<VmValue, String> {
    let target = args
        .first()
        .ok_or("Reflect.preventExtensions requires a target argument")?;

    let obj = get_target_object(target)?;
    obj.prevent_extensions();
    Ok(VmValue::boolean(true))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_reflect_get() {
        let mm = Arc::new(memory::MemoryManager::test());
        let obj = GcRef::new(JsObject::new(None, Arc::clone(&mm)));
        obj.set("x".into(), VmValue::number(42.0));

        let result = native_reflect_get(
            &[VmValue::object(obj), VmValue::string(JsString::intern("x"))],
            Arc::clone(&mm),
        )
        .unwrap();

        assert_eq!(result.as_number(), Some(42.0));
    }

    #[test]
    fn test_reflect_get_missing() {
        let mm = Arc::new(memory::MemoryManager::test());
        let obj = GcRef::new(JsObject::new(None, Arc::clone(&mm)));

        let result = native_reflect_get(
            &[
                VmValue::object(obj),
                VmValue::string(JsString::intern("missing")),
            ],
            Arc::clone(&mm),
        )
        .unwrap();

        assert!(result.is_undefined());
    }

    #[test]
    fn test_reflect_set() {
        let mm = Arc::new(memory::MemoryManager::test());
        let obj = GcRef::new(JsObject::new(None, Arc::clone(&mm)));

        let result = native_reflect_set(
            &[
                VmValue::object(obj),
                VmValue::string(JsString::intern("x")),
                VmValue::number(99.0),
            ],
            Arc::clone(&mm),
        )
        .unwrap();

        assert_eq!(result.as_boolean(), Some(true));
        assert_eq!(obj.get(&"x".into()).unwrap().as_number(), Some(99.0));
    }

    #[test]
    fn test_reflect_has() {
        let mm = Arc::new(memory::MemoryManager::test());
        let obj = GcRef::new(JsObject::new(None, Arc::clone(&mm)));
        obj.set("x".into(), VmValue::number(1.0));

        let result = native_reflect_has(
            &[
                VmValue::object(obj),
                VmValue::string(JsString::intern("x")),
            ],
            Arc::clone(&mm),
        )
        .unwrap();
        assert_eq!(result.as_boolean(), Some(true));

        let result = native_reflect_has(
            &[VmValue::object(obj), VmValue::string(JsString::intern("y"))],
            Arc::clone(&mm),
        )
        .unwrap();
        assert_eq!(result.as_boolean(), Some(false));
    }

    #[test]
    fn test_reflect_delete_property() {
        let mm = Arc::new(memory::MemoryManager::test());
        let obj = GcRef::new(JsObject::new(None, Arc::clone(&mm)));
        obj.set("x".into(), VmValue::number(1.0));

        let result = native_reflect_delete_property(
            &[
                VmValue::object(obj),
                VmValue::string(JsString::intern("x")),
            ],
            Arc::clone(&mm),
        )
        .unwrap();

        assert_eq!(result.as_boolean(), Some(true));
        assert!(!obj.has(&"x".into()));
    }

    #[test]
    fn test_reflect_own_keys() {
        let mm = Arc::new(memory::MemoryManager::test());
        let obj = GcRef::new(JsObject::new(None, Arc::clone(&mm)));
        obj.set("a".into(), VmValue::number(1.0));
        obj.set("b".into(), VmValue::number(2.0));

        let result = native_reflect_own_keys(&[VmValue::object(obj)], Arc::clone(&mm)).unwrap();

        let arr = result.as_array().unwrap();
        assert!(arr.is_array());
        assert_eq!(arr.array_length(), 2);
    }

    #[test]
    fn test_reflect_get_prototype_of() {
        let mm = Arc::new(memory::MemoryManager::test());
        let proto = GcRef::new(JsObject::new(None, Arc::clone(&mm)));
        let obj = GcRef::new(JsObject::new(Some(proto), Arc::clone(&mm)));

        let result =
            native_reflect_get_prototype_of(&[VmValue::object(obj)], Arc::clone(&mm)).unwrap();

        assert!(result.is_object());
    }

    #[test]
    fn test_reflect_is_extensible() {
        let mm = Arc::new(memory::MemoryManager::test());
        let obj = GcRef::new(JsObject::new(None, Arc::clone(&mm)));

        let result =
            native_reflect_is_extensible(&[VmValue::object(obj)], Arc::clone(&mm))
                .unwrap();
        assert_eq!(result.as_boolean(), Some(true));

        obj.prevent_extensions();
        let result = native_reflect_is_extensible(&[VmValue::object(obj)], Arc::clone(&mm)).unwrap();
        assert_eq!(result.as_boolean(), Some(false));
    }
}
