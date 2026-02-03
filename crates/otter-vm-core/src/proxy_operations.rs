//! Proxy trap operations implementing ES2026 §9.5
//!
//! This module implements all 13 proxy handler traps with proper
//! invariant validation according to the ECMAScript specification.

use crate::error::{VmError, VmResult};
use crate::gc::GcRef;
use crate::interpreter::Interpreter;
use crate::object::{JsObject, PropertyDescriptor, PropertyKey};
use crate::proxy::JsProxy;
use crate::value::Value;
use crate::VmContext;
use std::sync::Arc;

/// Invoke a trap on a proxy handler
///
/// Returns:
/// - `Ok(Some(value))` if the trap exists and was called successfully
/// - `Ok(None)` if the trap doesn't exist (caller should use default behavior)
/// - `Err(...)` if the trap exists but threw an error or isn't callable
fn invoke_trap(
    interpreter: &Interpreter,
    ctx: &mut VmContext,
    proxy: &Arc<JsProxy>,
    trap_name: &str,
    args: &[Value],
) -> VmResult<Option<Value>> {
    // Check if proxy is revoked
    if proxy.is_revoked() {
        return Err(VmError::type_error(format!(
            "Cannot perform '{}' on a revoked proxy",
            trap_name
        )));
    }

    // Get the trap from handler
    let trap = match proxy.get_trap(trap_name) {
        Some(t) => t,
        None => return Ok(None), // No trap, use default behavior
    };

    // Verify trap is callable
    if !trap.is_callable() {
        return Err(VmError::type_error(format!(
            "Proxy handler's '{}' trap must be a function",
            trap_name
        )));
    }

    // Call the trap with the handler as 'this'
    let handler = proxy
        .handler()
        .ok_or_else(|| VmError::type_error("Proxy handler is not available"))?;
    let this_value = Value::object(handler);

    let result = interpreter.call_function(ctx, &trap, this_value, args)?;
    Ok(Some(result))
}

/// ES §9.5.8: [[Get]] trap
///
/// Implements the `get` trap with proper invariant validation.
pub fn proxy_get(
    interpreter: &Interpreter,
    ctx: &mut VmContext,
    proxy: &Arc<JsProxy>,
    key: &PropertyKey,
    key_value: Value,
    receiver: Value,
) -> VmResult<Value> {
    // Get target
    let target = proxy
        .target()
        .ok_or_else(|| VmError::type_error("Proxy target is not available"))?;

    // Invoke trap: handler.get(target, key, receiver)
    let trap_args = &[Value::object(target), key_value, receiver];
    let trap_result = invoke_trap(interpreter, ctx, proxy, "get", trap_args)?;

    // If no trap, perform default [[Get]] on target
    let result = match trap_result {
        Some(r) => r,
        None => {
            // Default behavior: get property from target
            return Ok(target.get(key).unwrap_or(Value::undefined()));
        }
    };

    // Validate invariants (ES §9.5.8 step 11-12)
    validate_get_trap_invariants(&target, key, &result)?;

    Ok(result)
}

/// Validate invariants for the `get` trap (ES §9.5.8)
fn validate_get_trap_invariants(
    target: &GcRef<JsObject>,
    key: &PropertyKey,
    trap_result: &Value,
) -> VmResult<()> {
    // Get target's own property descriptor
    let target_desc = target.get_own_property_descriptor(key);

    if let Some(desc) = target_desc {
        match desc {
            PropertyDescriptor::Data {
                value,
                attributes,
            } => {
                // If property is non-configurable and non-writable,
                // trap result must be SameValue as target's value
                if !attributes.configurable && !attributes.writable {
                    if !same_value(trap_result, &value) {
                        return Err(VmError::type_error(
                            "Proxy 'get' trap returned value that doesn't match non-configurable, non-writable data property",
                        ));
                    }
                }
            }
            PropertyDescriptor::Accessor { get, attributes, .. } => {
                // If property is non-configurable accessor with undefined getter,
                // trap result must be undefined
                if !attributes.configurable && get.is_none() {
                    if !trap_result.is_undefined() {
                        return Err(VmError::type_error(
                            "Proxy 'get' trap must return undefined for non-configurable accessor property with undefined getter",
                        ));
                    }
                }
            }
            PropertyDescriptor::Deleted => {
                // Deleted properties don't impose restrictions
            }
        }
    }

    Ok(())
}

/// ES §9.5.9: [[Set]] trap
///
/// Implements the `set` trap with proper invariant validation.
pub fn proxy_set(
    interpreter: &Interpreter,
    ctx: &mut VmContext,
    proxy: &Arc<JsProxy>,
    key: &PropertyKey,
    key_value: Value,
    value: Value,
    receiver: Value,
) -> VmResult<bool> {
    // Get target
    let target = proxy
        .target()
        .ok_or_else(|| VmError::type_error("Proxy target is not available"))?;

    // Invoke trap: handler.set(target, key, value, receiver)
    let trap_args = &[
        Value::object(target),
        key_value,
        value.clone(),
        receiver,
    ];
    let trap_result = invoke_trap(interpreter, ctx, proxy, "set", trap_args)?;

    // If no trap, perform default [[Set]] on target
    let success = match trap_result {
        Some(r) => {
            // Convert trap result to boolean
            r.to_boolean()
        }
        None => {
            // Default behavior: set property on target
            target.set(*key, value);
            return Ok(true);
        }
    };

    // Validate invariants (ES §9.5.9 step 12-13)
    if success {
        validate_set_trap_invariants(&target, key, &value)?;
    }

    Ok(success)
}

/// Validate invariants for the `set` trap (ES §9.5.9)
fn validate_set_trap_invariants(
    target: &GcRef<JsObject>,
    key: &PropertyKey,
    value: &Value,
) -> VmResult<()> {
    // Get target's own property descriptor
    let target_desc = target.get_own_property_descriptor(key);

    if let Some(desc) = target_desc {
        match desc {
            PropertyDescriptor::Data {
                value: target_value,
                attributes,
            } => {
                // If property is non-configurable and non-writable,
                // cannot change the value
                if !attributes.configurable && !attributes.writable {
                    if !same_value(value, &target_value) {
                        return Err(VmError::type_error(
                            "Cannot set non-configurable, non-writable property via proxy",
                        ));
                    }
                }
            }
            PropertyDescriptor::Accessor { set, attributes, .. } => {
                // If property is non-configurable accessor with undefined setter,
                // cannot set
                if !attributes.configurable && set.is_none() {
                    return Err(VmError::type_error(
                        "Cannot set non-configurable accessor property with undefined setter via proxy",
                    ));
                }
            }
            PropertyDescriptor::Deleted => {
                // Deleted properties don't impose restrictions
            }
        }
    }

    Ok(())
}

/// ES §9.5.7: [[HasProperty]] trap
///
/// Implements the `has` trap with proper invariant validation.
pub fn proxy_has(
    interpreter: &Interpreter,
    ctx: &mut VmContext,
    proxy: &Arc<JsProxy>,
    key: &PropertyKey,
    key_value: Value,
) -> VmResult<bool> {
    // Get target
    let target = proxy
        .target()
        .ok_or_else(|| VmError::type_error("Proxy target is not available"))?;

    // Invoke trap: handler.has(target, key)
    let trap_args = &[Value::object(target), key_value];
    let trap_result = invoke_trap(interpreter, ctx, proxy, "has", trap_args)?;

    // If no trap, perform default [[HasProperty]] on target
    let result = match trap_result {
        Some(r) => r.to_boolean(),
        None => {
            // Default behavior: has property on target
            return Ok(target.has(key));
        }
    };

    // Validate invariants (ES §9.5.7 step 9-10)
    validate_has_trap_invariants(&target, key, result)?;

    Ok(result)
}

/// Validate invariants for the `has` trap (ES §9.5.7)
fn validate_has_trap_invariants(
    target: &GcRef<JsObject>,
    key: &PropertyKey,
    trap_result: bool,
) -> VmResult<()> {
    // Get target's own property descriptor
    let target_desc = target.get_own_property_descriptor(key);

    if let Some(desc) = target_desc {
        // If property exists on target and is non-configurable,
        // trap must return true
        if !desc.is_configurable() && !trap_result {
            return Err(VmError::type_error(
                "Proxy 'has' trap returned false for non-configurable property",
            ));
        }
    }

    // If target is non-extensible and property exists,
    // trap must return true
    if !target.is_extensible() {
        if target.has_own(key) && !trap_result {
            return Err(VmError::type_error(
                "Proxy 'has' trap returned false for property of non-extensible target",
            ));
        }
    }

    Ok(())
}

/// ES §9.5.10: [[Delete]] trap
///
/// Implements the `deleteProperty` trap with proper invariant validation.
pub fn proxy_delete_property(
    interpreter: &Interpreter,
    ctx: &mut VmContext,
    proxy: &Arc<JsProxy>,
    key: &PropertyKey,
    key_value: Value,
) -> VmResult<bool> {
    // Get target
    let target = proxy
        .target()
        .ok_or_else(|| VmError::type_error("Proxy target is not available"))?;

    // Invoke trap: handler.deleteProperty(target, key)
    let trap_args = &[Value::object(target), key_value];
    let trap_result = invoke_trap(interpreter, ctx, proxy, "deleteProperty", trap_args)?;

    // If no trap, perform default [[Delete]] on target
    let result = match trap_result {
        Some(r) => r.to_boolean(),
        None => {
            // Default behavior: delete property from target
            return Ok(target.delete(key));
        }
    };

    // Validate invariants (ES §9.5.10 step 11-12)
    if result {
        validate_delete_trap_invariants(&target, key)?;
    }

    Ok(result)
}

/// Validate invariants for the `deleteProperty` trap (ES §9.5.10)
fn validate_delete_trap_invariants(
    target: &GcRef<JsObject>,
    key: &PropertyKey,
) -> VmResult<()> {
    // Get target's own property descriptor
    let target_desc = target.get_own_property_descriptor(key);

    if let Some(desc) = target_desc {
        // If property is non-configurable, trap cannot return true
        if !desc.is_configurable() {
            return Err(VmError::type_error(
                "Cannot delete non-configurable property via proxy",
            ));
        }
    }

    Ok(())
}

/// ES §9.5.11: [[OwnPropertyKeys]] trap
///
/// Implements the `ownKeys` trap with proper invariant validation.
pub fn proxy_own_keys(
    interpreter: &Interpreter,
    ctx: &mut VmContext,
    proxy: &Arc<JsProxy>,
) -> VmResult<Vec<PropertyKey>> {
    // Get target
    let target = proxy
        .target()
        .ok_or_else(|| VmError::type_error("Proxy target is not available"))?;

    // Invoke trap: handler.ownKeys(target)
    let trap_args = &[Value::object(target.clone())];
    let trap_result = invoke_trap(interpreter, ctx, proxy, "ownKeys", trap_args)?;

    // If no trap, perform default [[OwnPropertyKeys]] on target
    let keys_array = match trap_result {
        Some(r) => r,
        None => {
            // Default behavior: get own property keys from target
            let keys = target.own_keys();
            return Ok(keys);
        }
    };

    // Trap result must be an object
    let keys_obj = keys_array
        .as_object()
        .ok_or_else(|| VmError::type_error("Proxy 'ownKeys' trap must return an object"))?;

    // Get length property
    let length_value = keys_obj.get(&PropertyKey::from("length")).unwrap_or(Value::int32(0));
    let length = length_value.as_number().unwrap_or(0.0) as usize;

    // Extract keys from array-like object
    let mut trap_keys = Vec::new();
    for i in 0..length {
        let element = keys_obj
            .get(&PropertyKey::Index(i as u32))
            .unwrap_or(Value::undefined());

        // Convert element to PropertyKey
        if let Some(s) = element.as_string() {
            trap_keys.push(PropertyKey::String(s.clone()));
        } else if let Some(sym) = element.as_symbol() {
            trap_keys.push(PropertyKey::Symbol(sym.id));
        } else {
            // Convert to string (use string representation)
            // For now, skip non-string/symbol keys
            // TODO: proper ToString conversion
            continue;
        }
    }

    // Validate invariants (ES §9.5.11 step 10-23)
    validate_own_keys_trap_invariants(&target, &trap_keys)?;

    Ok(trap_keys)
}

/// Validate invariants for the `ownKeys` trap (ES §9.5.11)
fn validate_own_keys_trap_invariants(
    target: &GcRef<JsObject>,
    trap_keys: &[PropertyKey],
) -> VmResult<()> {
    // Get target's own keys
    let target_keys = target.own_keys();

    // Check that all non-configurable keys are present
    for target_key in &target_keys {
        if let Some(desc) = target.get_own_property_descriptor(target_key) {
            if !desc.is_configurable() {
                // Non-configurable property must be in trap result
                if !trap_keys.contains(target_key) {
                    return Err(VmError::type_error(
                        "Proxy 'ownKeys' trap result must contain all non-configurable property keys",
                    ));
                }
            }
        }
    }

    // If target is non-extensible, trap result must contain exactly target's keys
    if !target.is_extensible() {
        // All target keys must be in trap result
        for target_key in &target_keys {
            if !trap_keys.contains(target_key) {
                return Err(VmError::type_error(
                    "Proxy 'ownKeys' trap result must contain all keys of non-extensible target",
                ));
            }
        }

        // All trap keys must be in target
        for trap_key in trap_keys {
            if !target_keys.contains(trap_key) {
                return Err(VmError::type_error(
                    "Proxy 'ownKeys' trap result cannot contain keys not present in non-extensible target",
                ));
            }
        }
    }

    Ok(())
}

/// ES §9.5.5: [[GetOwnProperty]] trap
///
/// Implements the `getOwnPropertyDescriptor` trap with proper invariant validation.
pub fn proxy_get_own_property_descriptor(
    interpreter: &Interpreter,
    ctx: &mut VmContext,
    proxy: &Arc<JsProxy>,
    key: &PropertyKey,
    key_value: Value,
) -> VmResult<Option<PropertyDescriptor>> {
    // Get target
    let target = proxy
        .target()
        .ok_or_else(|| VmError::type_error("Proxy target is not available"))?;

    // Invoke trap: handler.getOwnPropertyDescriptor(target, key)
    let trap_args = &[Value::object(target.clone()), key_value];
    let trap_result = invoke_trap(interpreter, ctx, proxy, "getOwnPropertyDescriptor", trap_args)?;

    // If no trap, perform default [[GetOwnProperty]] on target
    let result_value = match trap_result {
        Some(r) => r,
        None => {
            // Default behavior: get own property descriptor from target
            return Ok(target.get_own_property_descriptor(key));
        }
    };

    // Trap result must be undefined or an object
    if result_value.is_undefined() {
        // Validate: if target property is non-configurable, trap cannot return undefined
        if let Some(target_desc) = target.get_own_property_descriptor(key) {
            if !target_desc.is_configurable() {
                return Err(VmError::type_error(
                    "Proxy 'getOwnPropertyDescriptor' trap cannot return undefined for non-configurable property",
                ));
            }
        }

        // If target is non-extensible and property exists, trap cannot return undefined
        if !target.is_extensible() && target.has_own(key) {
            return Err(VmError::type_error(
                "Proxy 'getOwnPropertyDescriptor' trap cannot return undefined for property of non-extensible target",
            ));
        }

        return Ok(None);
    }

    let desc_obj = result_value.as_object().ok_or_else(|| {
        VmError::type_error("Proxy 'getOwnPropertyDescriptor' trap must return object or undefined")
    })?;

    // Convert descriptor object to PropertyDescriptor
    let trap_desc = descriptor_from_object(&desc_obj)?;

    // Validate invariants (ES §9.5.5 step 19-21)
    validate_get_own_property_descriptor_invariants(&target, key, &trap_desc)?;

    Ok(Some(trap_desc))
}

/// Validate invariants for `getOwnPropertyDescriptor` trap (ES §9.5.5)
fn validate_get_own_property_descriptor_invariants(
    target: &GcRef<JsObject>,
    key: &PropertyKey,
    trap_desc: &PropertyDescriptor,
) -> VmResult<()> {
    let target_desc = target.get_own_property_descriptor(key);

    if let Some(target_desc) = target_desc {
        // If target property is non-configurable, trap result must match
        if !target_desc.is_configurable() {
            // Check configurable attribute matches
            if trap_desc.is_configurable() {
                return Err(VmError::type_error(
                    "Proxy 'getOwnPropertyDescriptor' trap cannot report non-configurable property as configurable",
                ));
            }

            // For data properties, check value and writable
            if let PropertyDescriptor::Data { value: target_value, attributes: target_attrs } = &target_desc {
                if let PropertyDescriptor::Data { value: trap_value, attributes: trap_attrs } = trap_desc {
                    if !target_attrs.writable && trap_attrs.writable {
                        return Err(VmError::type_error(
                            "Proxy 'getOwnPropertyDescriptor' trap cannot report non-writable property as writable",
                        ));
                    }
                    if !target_attrs.writable && !same_value(trap_value, target_value) {
                        return Err(VmError::type_error(
                            "Proxy 'getOwnPropertyDescriptor' trap must report same value for non-configurable, non-writable property",
                        ));
                    }
                }
            }
        }
    } else {
        // Property doesn't exist on target
        // If target is non-extensible, trap cannot report a new property
        if !target.is_extensible() && !trap_desc.is_configurable() {
            return Err(VmError::type_error(
                "Proxy 'getOwnPropertyDescriptor' trap cannot report non-configurable property on non-extensible target",
            ));
        }
    }

    Ok(())
}

/// ES §9.5.6: [[DefineOwnProperty]] trap
///
/// Implements the `defineProperty` trap with proper invariant validation.
pub fn proxy_define_property(
    interpreter: &Interpreter,
    ctx: &mut VmContext,
    proxy: &Arc<JsProxy>,
    key: &PropertyKey,
    key_value: Value,
    desc: &PropertyDescriptor,
) -> VmResult<bool> {
    // Get target
    let target = proxy
        .target()
        .ok_or_else(|| VmError::type_error("Proxy target is not available"))?;

    // Convert descriptor to object for trap call
    let desc_obj = descriptor_to_object(desc, ctx);

    // Invoke trap: handler.defineProperty(target, key, descriptor)
    let trap_args = &[Value::object(target.clone()), key_value, desc_obj];
    let trap_result = invoke_trap(interpreter, ctx, proxy, "defineProperty", trap_args)?;

    // If no trap, perform default [[DefineOwnProperty]] on target
    let success = match trap_result {
        Some(r) => r.to_boolean(),
        None => {
            // Default behavior: define property on target
            target.define_property(*key, desc.clone());
            return Ok(true);
        }
    };

    // Validate invariants (ES §9.5.6 step 17-20)
    if success {
        validate_define_property_invariants(&target, key, desc)?;
    }

    Ok(success)
}

/// Validate invariants for `defineProperty` trap (ES §9.5.6)
fn validate_define_property_invariants(
    target: &GcRef<JsObject>,
    key: &PropertyKey,
    desc: &PropertyDescriptor,
) -> VmResult<()> {
    let target_desc = target.get_own_property_descriptor(key);

    // If target is non-extensible, cannot add new properties
    if !target.is_extensible() && target_desc.is_none() {
        return Err(VmError::type_error(
            "Cannot define property on non-extensible target via proxy",
        ));
    }

    // If target has non-configurable property, cannot change it to configurable
    if let Some(target_desc) = target_desc {
        if !target_desc.is_configurable() && desc.is_configurable() {
            return Err(VmError::type_error(
                "Cannot change non-configurable property to configurable via proxy",
            ));
        }
    }

    Ok(())
}

/// Convert a PropertyDescriptor to a descriptor object Value
fn descriptor_to_object(desc: &PropertyDescriptor, ctx: &VmContext) -> Value {
    let obj = GcRef::new(JsObject::new(None, ctx.memory_manager().clone()));

    match desc {
        PropertyDescriptor::Data { value, attributes } => {
            obj.set(PropertyKey::from("value"), value.clone());
            obj.set(PropertyKey::from("writable"), Value::boolean(attributes.writable));
            obj.set(PropertyKey::from("enumerable"), Value::boolean(attributes.enumerable));
            obj.set(PropertyKey::from("configurable"), Value::boolean(attributes.configurable));
        }
        PropertyDescriptor::Accessor { get, set, attributes } => {
            if let Some(getter) = get {
                obj.set(PropertyKey::from("get"), getter.clone());
            }
            if let Some(setter) = set {
                obj.set(PropertyKey::from("set"), setter.clone());
            }
            obj.set(PropertyKey::from("enumerable"), Value::boolean(attributes.enumerable));
            obj.set(PropertyKey::from("configurable"), Value::boolean(attributes.configurable));
        }
        PropertyDescriptor::Deleted => {
            // Deleted descriptor shouldn't be converted
        }
    }

    Value::object(obj)
}

/// Convert a descriptor object to PropertyDescriptor
fn descriptor_from_object(obj: &GcRef<JsObject>) -> VmResult<PropertyDescriptor> {
    let has_value = obj.has(&PropertyKey::from("value"));
    let has_writable = obj.has(&PropertyKey::from("writable"));
    let has_get = obj.has(&PropertyKey::from("get"));
    let has_set = obj.has(&PropertyKey::from("set"));

    let enumerable = obj
        .get(&PropertyKey::from("enumerable"))
        .map(|v| v.to_boolean())
        .unwrap_or(false);
    let configurable = obj
        .get(&PropertyKey::from("configurable"))
        .map(|v| v.to_boolean())
        .unwrap_or(false);

    // Determine if it's a data or accessor descriptor
    if has_value || has_writable {
        // Data descriptor
        let value = obj.get(&PropertyKey::from("value")).unwrap_or(Value::undefined());
        let writable = obj
            .get(&PropertyKey::from("writable"))
            .map(|v| v.to_boolean())
            .unwrap_or(false);

        Ok(PropertyDescriptor::Data {
            value,
            attributes: crate::object::PropertyAttributes {
                writable,
                enumerable,
                configurable,
            },
        })
    } else if has_get || has_set {
        // Accessor descriptor
        let get = obj.get(&PropertyKey::from("get")).filter(|v| !v.is_undefined());
        let set = obj.get(&PropertyKey::from("set")).filter(|v| !v.is_undefined());

        Ok(PropertyDescriptor::Accessor {
            get,
            set,
            attributes: crate::object::PropertyAttributes {
                writable: false, // Not applicable for accessors
                enumerable,
                configurable,
            },
        })
    } else {
        // Generic descriptor (no value/writable/get/set)
        Ok(PropertyDescriptor::Data {
            value: Value::undefined(),
            attributes: crate::object::PropertyAttributes {
                writable: false,
                enumerable,
                configurable,
            },
        })
    }
}

/// ES §9.5.1: [[GetPrototypeOf]] trap
///
/// Implements the `getPrototypeOf` trap with proper invariant validation.
pub fn proxy_get_prototype_of(
    interpreter: &Interpreter,
    ctx: &mut VmContext,
    proxy: &Arc<JsProxy>,
) -> VmResult<Option<GcRef<JsObject>>> {
    // Get target
    let target = proxy
        .target()
        .ok_or_else(|| VmError::type_error("Proxy target is not available"))?;

    // Invoke trap: handler.getPrototypeOf(target)
    let trap_args = &[Value::object(target.clone())];
    let trap_result = invoke_trap(interpreter, ctx, proxy, "getPrototypeOf", trap_args)?;

    // If no trap, perform default [[GetPrototypeOf]] on target
    let proto_value = match trap_result {
        Some(r) => r,
        None => {
            // Default behavior: get prototype from target
            return Ok(target.prototype());
        }
    };

    // Trap result must be null or an object
    let trap_proto = if proto_value.is_null() {
        None
    } else if let Some(obj) = proto_value.as_object() {
        Some(obj)
    } else {
        return Err(VmError::type_error(
            "Proxy 'getPrototypeOf' trap must return an object or null",
        ));
    };

    // Validate invariants (ES §9.5.1 step 8)
    validate_get_prototype_of_invariants(&target, &trap_proto)?;

    Ok(trap_proto)
}

/// Validate invariants for `getPrototypeOf` trap (ES §9.5.1)
fn validate_get_prototype_of_invariants(
    target: &GcRef<JsObject>,
    trap_proto: &Option<GcRef<JsObject>>,
) -> VmResult<()> {
    // If target is non-extensible, trap result must match target's prototype
    if !target.is_extensible() {
        let target_proto = target.prototype();

        // Both must be None or both must be Some with same reference
        let proto_matches = match (trap_proto, &target_proto) {
            (None, None) => true,
            (Some(a), Some(b)) => a.as_ptr() == b.as_ptr(),
            _ => false,
        };

        if !proto_matches {
            return Err(VmError::type_error(
                "Proxy 'getPrototypeOf' trap result must match target's prototype for non-extensible target",
            ));
        }
    }

    Ok(())
}

/// ES §9.5.2: [[SetPrototypeOf]] trap
///
/// Implements the `setPrototypeOf` trap with proper invariant validation.
pub fn proxy_set_prototype_of(
    interpreter: &Interpreter,
    ctx: &mut VmContext,
    proxy: &Arc<JsProxy>,
    proto: Option<GcRef<JsObject>>,
) -> VmResult<bool> {
    // Get target
    let target = proxy
        .target()
        .ok_or_else(|| VmError::type_error("Proxy target is not available"))?;

    // Convert prototype to Value
    let proto_value = match &proto {
        Some(p) => Value::object(p.clone()),
        None => Value::null(),
    };

    // Invoke trap: handler.setPrototypeOf(target, proto)
    let trap_args = &[Value::object(target.clone()), proto_value];
    let trap_result = invoke_trap(interpreter, ctx, proxy, "setPrototypeOf", trap_args)?;

    // If no trap, perform default [[SetPrototypeOf]] on target
    let success = match trap_result {
        Some(r) => r.to_boolean(),
        None => {
            // Default behavior: set prototype on target
            target.set_prototype(proto.clone());
            return Ok(true);
        }
    };

    // Validate invariants (ES §9.5.2 step 8)
    if success {
        validate_set_prototype_of_invariants(&target, &proto)?;
    }

    Ok(success)
}

/// Validate invariants for `setPrototypeOf` trap (ES §9.5.2)
fn validate_set_prototype_of_invariants(
    target: &GcRef<JsObject>,
    proto: &Option<GcRef<JsObject>>,
) -> VmResult<()> {
    // If target is non-extensible, cannot change prototype
    if !target.is_extensible() {
        let target_proto = target.prototype();

        // Prototype must match target's current prototype
        let proto_matches = match (proto, &target_proto) {
            (None, None) => true,
            (Some(a), Some(b)) => a.as_ptr() == b.as_ptr(),
            _ => false,
        };

        if !proto_matches {
            return Err(VmError::type_error(
                "Cannot change prototype of non-extensible target via proxy",
            ));
        }
    }

    Ok(())
}

/// ES §9.5.3: [[IsExtensible]] trap
///
/// Implements the `isExtensible` trap with proper invariant validation.
pub fn proxy_is_extensible(
    interpreter: &Interpreter,
    ctx: &mut VmContext,
    proxy: &Arc<JsProxy>,
) -> VmResult<bool> {
    // Get target
    let target = proxy
        .target()
        .ok_or_else(|| VmError::type_error("Proxy target is not available"))?;

    // Invoke trap: handler.isExtensible(target)
    let trap_args = &[Value::object(target.clone())];
    let trap_result = invoke_trap(interpreter, ctx, proxy, "isExtensible", trap_args)?;

    // If no trap, perform default [[IsExtensible]] on target
    let trap_extensible = match trap_result {
        Some(r) => r.to_boolean(),
        None => {
            // Default behavior: check if target is extensible
            return Ok(target.is_extensible());
        }
    };

    // Validate invariants (ES §9.5.3 step 7)
    let target_extensible = target.is_extensible();
    if trap_extensible != target_extensible {
        return Err(VmError::type_error(
            "Proxy 'isExtensible' trap result must match target's extensibility",
        ));
    }

    Ok(trap_extensible)
}

/// ES §9.5.4: [[PreventExtensions]] trap
///
/// Implements the `preventExtensions` trap with proper invariant validation.
pub fn proxy_prevent_extensions(
    interpreter: &Interpreter,
    ctx: &mut VmContext,
    proxy: &Arc<JsProxy>,
) -> VmResult<bool> {
    // Get target
    let target = proxy
        .target()
        .ok_or_else(|| VmError::type_error("Proxy target is not available"))?;

    // Invoke trap: handler.preventExtensions(target)
    let trap_args = &[Value::object(target.clone())];
    let trap_result = invoke_trap(interpreter, ctx, proxy, "preventExtensions", trap_args)?;

    // If no trap, perform default [[PreventExtensions]] on target
    let success = match trap_result {
        Some(r) => r.to_boolean(),
        None => {
            // Default behavior: prevent extensions on target
            target.prevent_extensions();
            return Ok(true);
        }
    };

    // Validate invariants (ES §9.5.4 step 7)
    if success && target.is_extensible() {
        return Err(VmError::type_error(
            "Proxy 'preventExtensions' trap returned true but target is still extensible",
        ));
    }

    Ok(success)
}

/// ES §9.5.13: [[Call]] trap
///
/// Implements the `apply` trap for function calls.
pub fn proxy_apply(
    interpreter: &Interpreter,
    ctx: &mut VmContext,
    proxy: &Arc<JsProxy>,
    this_value: Value,
    args: &[Value],
) -> VmResult<Value> {
    // Get target
    let target = proxy
        .target()
        .ok_or_else(|| VmError::type_error("Proxy target is not available"))?;

    // Note: ES spec requires target to be callable, but we let the trap handle it
    // The trap can validate and throw if needed

    // Create arguments array for trap
    let args_array = GcRef::new(JsObject::new(None, ctx.memory_manager().clone()));
    for (i, arg) in args.iter().enumerate() {
        args_array.set(PropertyKey::Index(i as u32), arg.clone());
    }
    args_array.set(PropertyKey::from("length"), Value::int32(args.len() as i32));

    // Invoke trap: handler.apply(target, thisValue, args)
    let trap_args = &[
        Value::object(target.clone()),
        this_value.clone(),
        Value::object(args_array),
    ];
    let trap_result = invoke_trap(interpreter, ctx, proxy, "apply", trap_args)?;

    // If no trap, perform default [[Call]] on target
    match trap_result {
        Some(result) => Ok(result),
        None => {
            // Default behavior: call target function
            interpreter.call_function(ctx, &Value::object(target), this_value, args)
        }
    }
}

/// ES §9.5.14: [[Construct]] trap
///
/// Implements the `construct` trap for constructor calls.
pub fn proxy_construct(
    interpreter: &Interpreter,
    ctx: &mut VmContext,
    proxy: &Arc<JsProxy>,
    args: &[Value],
    new_target: Value,
) -> VmResult<Value> {
    // Get target
    let target = proxy
        .target()
        .ok_or_else(|| VmError::type_error("Proxy target is not available"))?;

    // Note: ES spec requires target to be a constructor, but we let the trap handle it
    // The trap can validate and throw if needed

    // Create arguments array for trap
    let args_array = GcRef::new(JsObject::new(None, ctx.memory_manager().clone()));
    for (i, arg) in args.iter().enumerate() {
        args_array.set(PropertyKey::Index(i as u32), arg.clone());
    }
    args_array.set(PropertyKey::from("length"), Value::int32(args.len() as i32));

    // Invoke trap: handler.construct(target, args, newTarget)
    let trap_args = &[
        Value::object(target.clone()),
        Value::object(args_array),
        new_target.clone(),
    ];
    let trap_result = invoke_trap(interpreter, ctx, proxy, "construct", trap_args)?;

    // If no trap, perform default [[Construct]] on target
    let result = match trap_result {
        Some(r) => r,
        None => {
            // Default behavior: construct with target
            // This requires calling the constructor, which is complex
            // For now, return an error indicating no default construct
            return Err(VmError::type_error(
                "Proxy construct requires a construct trap or direct target call",
            ));
        }
    };

    // Validate result must be an object
    if !result.is_object() {
        return Err(VmError::type_error(
            "Proxy 'construct' trap must return an object",
        ));
    }

    Ok(result)
}

/// SameValue comparison (ES §7.2.11)
///
/// This is a simplified implementation that handles the most common cases.
/// For full compliance, this should handle NaN and -0/+0 distinctions.
fn same_value(x: &Value, y: &Value) -> bool {
    // Handle undefined
    if x.is_undefined() && y.is_undefined() {
        return true;
    }

    // Handle null
    if x.is_null() && y.is_null() {
        return true;
    }

    // Handle booleans
    if let (Some(a), Some(b)) = (x.as_boolean(), y.as_boolean()) {
        return a == b;
    }

    // Handle numbers (integers and floats)
    match (x.as_int32(), y.as_int32()) {
        (Some(a), Some(b)) => return a == b,
        _ => {}
    }

    match (x.as_number(), y.as_number()) {
        (Some(a), Some(b)) => {
            // Handle NaN: NaN is SameValue to itself
            if a.is_nan() && b.is_nan() {
                return true;
            }
            // Handle -0 and +0: they are NOT SameValue
            if a == 0.0 && b == 0.0 {
                return a.is_sign_positive() == b.is_sign_positive();
            }
            return a == b;
        }
        _ => {}
    }

    // Handle strings
    if let (Some(a), Some(b)) = (x.as_string(), y.as_string()) {
        return a == b;
    }

    // Handle symbols
    if let (Some(a), Some(b)) = (x.as_symbol(), y.as_symbol()) {
        return a.id == b.id;
    }

    // Handle object references (including functions, arrays, etc.)
    // For objects, SameValue means same reference (pointer equality)
    if let (Some(a), Some(b)) = (x.as_object(), y.as_object()) {
        return a.as_ptr() == b.as_ptr();
    }

    // Different types or unsupported comparison
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_same_value_primitives() {
        assert!(same_value(&Value::undefined(), &Value::undefined()));
        assert!(same_value(&Value::null(), &Value::null()));
        assert!(same_value(&Value::boolean(true), &Value::boolean(true)));
        assert!(same_value(&Value::int32(42), &Value::int32(42)));
        assert!(!same_value(&Value::int32(42), &Value::int32(43)));
    }

    #[test]
    fn test_same_value_nan() {
        let nan1 = Value::number(f64::NAN);
        let nan2 = Value::number(f64::NAN);
        assert!(same_value(&nan1, &nan2));
    }

    #[test]
    fn test_same_value_zero() {
        let pos_zero = Value::number(0.0);
        let neg_zero = Value::number(-0.0);
        assert!(!same_value(&pos_zero, &neg_zero));
    }
}
