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

use otter_vm_core::object::{JsObject, PropertyKey};
use otter_vm_core::string::JsString;
use otter_vm_core::value::Value as VmValue;
use otter_vm_runtime::{Op, op_native};
use std::sync::Arc;

/// Get Reflect ops for extension registration
pub fn ops() -> Vec<Op> {
    vec![
        op_native("__Reflect_get", native_reflect_get),
        op_native("__Reflect_set", native_reflect_set),
        op_native("__Reflect_has", native_reflect_has),
        op_native("__Reflect_deleteProperty", native_reflect_delete_property),
        op_native("__Reflect_ownKeys", native_reflect_own_keys),
        op_native("__Reflect_getOwnPropertyDescriptor", native_reflect_get_own_property_descriptor),
        op_native("__Reflect_defineProperty", native_reflect_define_property),
        op_native("__Reflect_getPrototypeOf", native_reflect_get_prototype_of),
        op_native("__Reflect_setPrototypeOf", native_reflect_set_prototype_of),
        op_native("__Reflect_isExtensible", native_reflect_is_extensible),
        op_native("__Reflect_preventExtensions", native_reflect_prevent_extensions),
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
        return PropertyKey::String(Arc::clone(s));
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
fn get_target_object(value: &VmValue) -> Result<Arc<JsObject>, String> {
    // Check if it's a proxy first
    if let Some(proxy) = value.as_proxy() {
        return proxy
            .target()
            .cloned()
            .ok_or_else(|| "Cannot perform operation on a revoked proxy".to_string());
    }

    value
        .as_object()
        .cloned()
        .ok_or_else(|| "Reflect method requires an object target".to_string())
}

// ============================================================================
// Native Operations
// ============================================================================

/// Reflect.get(target, propertyKey, receiver?)
/// Returns the value of the property
fn native_reflect_get(args: &[VmValue]) -> Result<VmValue, String> {
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
fn native_reflect_set(args: &[VmValue]) -> Result<VmValue, String> {
    let target = args
        .first()
        .ok_or("Reflect.set requires a target argument")?;
    let property_key = args
        .get(1)
        .ok_or("Reflect.set requires a propertyKey argument")?;
    let value = args
        .get(2)
        .cloned()
        .unwrap_or(VmValue::undefined());
    // receiver is optional (args[3])

    let obj = get_target_object(target)?;
    let key = to_property_key(property_key);

    obj.set(key, value);
    Ok(VmValue::boolean(true))
}

/// Reflect.has(target, propertyKey)
/// Returns true if the property exists
fn native_reflect_has(args: &[VmValue]) -> Result<VmValue, String> {
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
fn native_reflect_delete_property(args: &[VmValue]) -> Result<VmValue, String> {
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
fn native_reflect_own_keys(args: &[VmValue]) -> Result<VmValue, String> {
    let target = args
        .first()
        .ok_or("Reflect.ownKeys requires a target argument")?;

    let obj = get_target_object(target)?;
    let keys = obj.own_keys();

    // Filter to only string and index keys (symbols require registry lookup)
    let filtered_keys: Vec<_> = keys
        .into_iter()
        .filter(|k| !matches!(k, PropertyKey::Symbol(_)))
        .collect();

    let result = Arc::new(JsObject::array(filtered_keys.len()));
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
fn native_reflect_get_own_property_descriptor(args: &[VmValue]) -> Result<VmValue, String> {
    let target = args
        .first()
        .ok_or("Reflect.getOwnPropertyDescriptor requires a target argument")?;
    let property_key = args
        .get(1)
        .ok_or("Reflect.getOwnPropertyDescriptor requires a propertyKey argument")?;

    let obj = get_target_object(target)?;
    let key = to_property_key(property_key);

    // Check if property exists
    if let Some(value) = obj.get(&key) {
        let desc = Arc::new(JsObject::new(None));
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
fn native_reflect_define_property(args: &[VmValue]) -> Result<VmValue, String> {
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

    // Get value from descriptor
    if let Some(attr_obj) = attributes.as_object()
        && let Some(value) = attr_obj.get(&"value".into())
    {
        obj.set(key, value);
    }

    Ok(VmValue::boolean(true))
}

/// Reflect.getPrototypeOf(target)
/// Returns the prototype of the target
fn native_reflect_get_prototype_of(args: &[VmValue]) -> Result<VmValue, String> {
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
fn native_reflect_set_prototype_of(args: &[VmValue]) -> Result<VmValue, String> {
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
        Some(Arc::clone(proto_obj))
    } else {
        return Err("Prototype must be an object or null".to_string());
    };

    let success = obj.set_prototype(new_proto);
    Ok(VmValue::boolean(success))
}

/// Reflect.isExtensible(target)
/// Returns true if the target is extensible
fn native_reflect_is_extensible(args: &[VmValue]) -> Result<VmValue, String> {
    let target = args
        .first()
        .ok_or("Reflect.isExtensible requires a target argument")?;

    let obj = get_target_object(target)?;
    Ok(VmValue::boolean(obj.is_extensible()))
}

/// Reflect.preventExtensions(target)
/// Returns true if extensions were prevented
fn native_reflect_prevent_extensions(args: &[VmValue]) -> Result<VmValue, String> {
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
        let obj = Arc::new(JsObject::new(None));
        obj.set("x".into(), VmValue::number(42.0));

        let result = native_reflect_get(&[
            VmValue::object(obj),
            VmValue::string(JsString::intern("x")),
        ])
        .unwrap();

        assert_eq!(result.as_number(), Some(42.0));
    }

    #[test]
    fn test_reflect_get_missing() {
        let obj = Arc::new(JsObject::new(None));

        let result = native_reflect_get(&[
            VmValue::object(obj),
            VmValue::string(JsString::intern("missing")),
        ])
        .unwrap();

        assert!(result.is_undefined());
    }

    #[test]
    fn test_reflect_set() {
        let obj = Arc::new(JsObject::new(None));

        let result = native_reflect_set(&[
            VmValue::object(Arc::clone(&obj)),
            VmValue::string(JsString::intern("x")),
            VmValue::number(99.0),
        ])
        .unwrap();

        assert_eq!(result.as_boolean(), Some(true));
        assert_eq!(obj.get(&"x".into()).unwrap().as_number(), Some(99.0));
    }

    #[test]
    fn test_reflect_has() {
        let obj = Arc::new(JsObject::new(None));
        obj.set("x".into(), VmValue::number(1.0));

        let result = native_reflect_has(&[
            VmValue::object(Arc::clone(&obj)),
            VmValue::string(JsString::intern("x")),
        ])
        .unwrap();
        assert_eq!(result.as_boolean(), Some(true));

        let result = native_reflect_has(&[
            VmValue::object(obj),
            VmValue::string(JsString::intern("y")),
        ])
        .unwrap();
        assert_eq!(result.as_boolean(), Some(false));
    }

    #[test]
    fn test_reflect_delete_property() {
        let obj = Arc::new(JsObject::new(None));
        obj.set("x".into(), VmValue::number(1.0));

        let result = native_reflect_delete_property(&[
            VmValue::object(Arc::clone(&obj)),
            VmValue::string(JsString::intern("x")),
        ])
        .unwrap();

        assert_eq!(result.as_boolean(), Some(true));
        assert!(!obj.has(&"x".into()));
    }

    #[test]
    fn test_reflect_own_keys() {
        let obj = Arc::new(JsObject::new(None));
        obj.set("a".into(), VmValue::number(1.0));
        obj.set("b".into(), VmValue::number(2.0));

        let result = native_reflect_own_keys(&[VmValue::object(obj)]).unwrap();

        let arr = result.as_array().unwrap();
        assert!(arr.is_array());
        assert_eq!(arr.array_length(), 2);
    }

    #[test]
    fn test_reflect_get_prototype_of() {
        let proto = Arc::new(JsObject::new(None));
        let obj = Arc::new(JsObject::new(Some(Arc::clone(&proto))));

        let result = native_reflect_get_prototype_of(&[VmValue::object(obj)]).unwrap();

        assert!(result.is_object());
    }

    #[test]
    fn test_reflect_is_extensible() {
        let obj = Arc::new(JsObject::new(None));

        let result = native_reflect_is_extensible(&[VmValue::object(Arc::clone(&obj))]).unwrap();
        assert_eq!(result.as_boolean(), Some(true));

        obj.prevent_extensions();
        let result = native_reflect_is_extensible(&[VmValue::object(obj)]).unwrap();
        assert_eq!(result.as_boolean(), Some(false));
    }
}
