//! Object built-in
//!
//! Provides Object.keys(), Object.values(), Object.entries(), Object.assign(), Object.hasOwn(),
//! Object.freeze(), Object.isFrozen(), Object.seal(), Object.isSealed(),
//! Object.preventExtensions(), Object.isExtensible()

use otter_macros::dive;
use otter_vm_core::value::Value as VmValue;
use otter_vm_runtime::{Op, op_native, op_sync};
use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;

/// Object constructor and methods
pub struct ObjectBuiltin;

/// Get Object ops for extension registration
pub fn ops() -> Vec<Op> {
    vec![
        // JSON-based ops (work with serialized data)
        op_sync("__Object_keys", __otter_dive_object_keys),
        op_sync("__Object_values", __otter_dive_object_values),
        op_sync("__Object_entries", __otter_dive_object_entries),
        op_sync("__Object_assign", __otter_dive_object_assign),
        op_sync("__Object_hasOwn", __otter_dive_object_has_own),
        // Native ops (require object identity)
        op_native("__Object_freeze", native_object_freeze),
        op_native("__Object_isFrozen", native_object_is_frozen),
        op_native("__Object_seal", native_object_seal),
        op_native("__Object_isSealed", native_object_is_sealed),
        op_native(
            "__Object_preventExtensions",
            native_object_prevent_extensions,
        ),
        op_native("__Object_isExtensible", native_object_is_extensible),
    ]
}

// ============================================================================
// Native Ops - Work with VM Value directly for object identity operations
// ============================================================================

/// Object.freeze() - native implementation
fn native_object_freeze(args: &[VmValue]) -> Result<VmValue, String> {
    let obj = args.first().ok_or("Object.freeze requires an argument")?;
    if let Some(obj_ref) = obj.as_object() {
        obj_ref.freeze();
    }
    // Return the same object (per spec, returns the frozen object)
    Ok(obj.clone())
}

/// Object.isFrozen() - native implementation
fn native_object_is_frozen(args: &[VmValue]) -> Result<VmValue, String> {
    let obj = args.first().ok_or("Object.isFrozen requires an argument")?;
    let is_frozen = obj.as_object().map(|o| o.is_frozen()).unwrap_or(true); // Non-objects are considered frozen
    Ok(VmValue::boolean(is_frozen))
}

/// Object.seal() - native implementation
fn native_object_seal(args: &[VmValue]) -> Result<VmValue, String> {
    let obj = args.first().ok_or("Object.seal requires an argument")?;
    if let Some(obj_ref) = obj.as_object() {
        obj_ref.seal();
    }
    Ok(obj.clone())
}

/// Object.isSealed() - native implementation
fn native_object_is_sealed(args: &[VmValue]) -> Result<VmValue, String> {
    let obj = args.first().ok_or("Object.isSealed requires an argument")?;
    let is_sealed = obj.as_object().map(|o| o.is_sealed()).unwrap_or(true); // Non-objects are considered sealed
    Ok(VmValue::boolean(is_sealed))
}

/// Object.preventExtensions() - native implementation
fn native_object_prevent_extensions(args: &[VmValue]) -> Result<VmValue, String> {
    let obj = args
        .first()
        .ok_or("Object.preventExtensions requires an argument")?;
    if let Some(obj_ref) = obj.as_object() {
        obj_ref.prevent_extensions();
    }
    Ok(obj.clone())
}

/// Object.isExtensible() - native implementation
fn native_object_is_extensible(args: &[VmValue]) -> Result<VmValue, String> {
    let obj = args
        .first()
        .ok_or("Object.isExtensible requires an argument")?;
    let is_extensible = obj.as_object().map(|o| o.is_extensible()).unwrap_or(false); // Non-objects are not extensible
    Ok(VmValue::boolean(is_extensible))
}

// ============================================================================
// Dive Functions - Each becomes a callable JS function via #[dive(swift)]
// Note: Original functions appear unused because #[dive] copies their body
// into generated wrappers. They are used in tests.
// ============================================================================

/// Object.keys() - returns array of object's own enumerable property names
#[dive(swift)]
#[allow(dead_code)]
fn object_keys(obj: JsonValue) -> Vec<String> {
    match obj {
        JsonValue::Object(map) => map.keys().cloned().collect(),
        _ => vec![],
    }
}

/// Object.values() - returns array of object's own enumerable property values
#[dive(swift)]
#[allow(dead_code)]
fn object_values(obj: JsonValue) -> Vec<JsonValue> {
    match obj {
        JsonValue::Object(map) => map.values().cloned().collect(),
        _ => vec![],
    }
}

/// Entry type for Object.entries()
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ObjectEntry(String, JsonValue);

/// Object.entries() - returns array of [key, value] pairs
#[dive(swift)]
#[allow(dead_code)]
fn object_entries(obj: JsonValue) -> Vec<(String, JsonValue)> {
    match obj {
        JsonValue::Object(map) => map.into_iter().collect(),
        _ => vec![],
    }
}

/// Arguments for Object.assign
#[derive(Debug, Clone, Deserialize)]
pub struct AssignArgs {
    pub target: JsonValue,
    pub sources: Vec<JsonValue>,
}

/// Object.assign() - copies properties from sources to target
#[dive(swift)]
#[allow(dead_code)]
fn object_assign(args: AssignArgs) -> Result<JsonValue, String> {
    let mut result = match args.target {
        JsonValue::Object(map) => map,
        _ => return Err("Target must be an object".to_string()),
    };

    for source in args.sources {
        if let JsonValue::Object(map) = source {
            for (k, v) in map {
                result.insert(k, v);
            }
        }
    }

    Ok(JsonValue::Object(result))
}

/// Object.hasOwn() - returns true if object has own property
#[dive(swift)]
#[allow(dead_code)]
fn object_has_own(obj: JsonValue, key: String) -> bool {
    match obj {
        JsonValue::Object(map) => map.contains_key(&key),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_object_keys() {
        let obj = json!({"a": 1, "b": 2});
        let keys = object_keys(obj);
        assert_eq!(keys.len(), 2);
    }

    #[test]
    fn test_object_values() {
        let obj = json!({"a": 1, "b": 2});
        let values = object_values(obj);
        assert_eq!(values.len(), 2);
    }

    #[test]
    fn test_object_entries() {
        let obj = json!({"a": 1});
        let entries = object_entries(obj);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].0, "a");
        assert_eq!(entries[0].1, json!(1));
    }

    #[test]
    fn test_object_assign() {
        let args = AssignArgs {
            target: json!({"a": 1}),
            sources: vec![json!({"b": 2})],
        };
        let result = object_assign(args).unwrap();
        let obj = result.as_object().unwrap();
        assert_eq!(obj.len(), 2);
        assert_eq!(obj.get("a"), Some(&json!(1)));
        assert_eq!(obj.get("b"), Some(&json!(2)));
    }

    #[test]
    fn test_object_assign_overwrite() {
        let args = AssignArgs {
            target: json!({"a": 1}),
            sources: vec![json!({"a": 2})],
        };
        let result = object_assign(args).unwrap();
        let obj = result.as_object().unwrap();
        assert_eq!(obj.get("a"), Some(&json!(2)));
    }

    #[test]
    fn test_object_has_own() {
        let obj = json!({"a": 1});
        assert!(object_has_own(obj.clone(), "a".to_string()));
        assert!(!object_has_own(obj, "b".to_string()));
    }

    #[test]
    fn test_native_object_freeze() {
        use otter_vm_core::object::JsObject;
        use std::sync::Arc;

        let obj = Arc::new(JsObject::new(None));
        obj.set("a".into(), VmValue::int32(1));

        let value = VmValue::object(obj.clone());
        let result = native_object_freeze(std::slice::from_ref(&value)).unwrap();

        assert!(obj.is_frozen());
        // Result should be the same value
        assert!(result.is_object());
    }

    #[test]
    fn test_native_object_is_frozen() {
        use otter_vm_core::object::JsObject;
        use std::sync::Arc;

        let obj = Arc::new(JsObject::new(None));
        let value = VmValue::object(obj.clone());

        // Initially not frozen
        let result = native_object_is_frozen(std::slice::from_ref(&value)).unwrap();
        assert_eq!(result.as_boolean(), Some(false));

        // After freeze
        obj.freeze();
        let result = native_object_is_frozen(std::slice::from_ref(&value)).unwrap();
        assert_eq!(result.as_boolean(), Some(true));
    }

    #[test]
    fn test_native_object_seal() {
        use otter_vm_core::object::JsObject;
        use std::sync::Arc;

        let obj = Arc::new(JsObject::new(None));
        obj.set("a".into(), VmValue::int32(1));

        let value = VmValue::object(obj.clone());
        let _ = native_object_seal(std::slice::from_ref(&value)).unwrap();

        assert!(obj.is_sealed());
    }

    #[test]
    fn test_native_object_prevent_extensions() {
        use otter_vm_core::object::JsObject;
        use std::sync::Arc;

        let obj = Arc::new(JsObject::new(None));
        let value = VmValue::object(obj.clone());

        // Initially extensible
        assert!(obj.is_extensible());

        let _ = native_object_prevent_extensions(std::slice::from_ref(&value)).unwrap();

        // Now not extensible
        assert!(!obj.is_extensible());
    }
}
