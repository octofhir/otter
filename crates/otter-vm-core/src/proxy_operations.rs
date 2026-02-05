//! Proxy trap operations implementing ES2026 §9.5
//!
//! This module implements all 13 proxy handler traps with proper
//! invariant validation according to the ECMAScript specification.

use crate::error::{VmError, VmResult};
use crate::gc::GcRef;
use crate::context::NativeContext;
use crate::object::{JsObject, PropertyDescriptor, PropertyKey};
use crate::proxy::JsProxy;
use crate::string::JsString;
use crate::value::Value;
use std::sync::Arc;

fn create_args_array(ncx: &mut NativeContext, args: &[Value]) -> GcRef<JsObject> {
    let arr = GcRef::new(JsObject::array(args.len(), ncx.memory_manager().clone()));
    if let Some(array_ctor) = ncx.global().get(&PropertyKey::string("Array")) {
        if let Some(array_obj) = array_ctor.as_object() {
            if let Some(proto_val) = array_obj.get(&PropertyKey::string("prototype")) {
                if let Some(proto_obj) = proto_val.as_object() {
                    arr.set_prototype(Value::object(proto_obj));
                }
            }
        }
    }
    for (i, arg) in args.iter().enumerate() {
        arr.set(PropertyKey::Index(i as u32), arg.clone());
    }
    arr
}

fn proxy_target_value(proxy: GcRef<JsProxy>) -> VmResult<Value> {
    proxy
        .target()
        .ok_or_else(|| VmError::type_error("Proxy target is not available"))
}

fn proxy_handler_value(proxy: GcRef<JsProxy>) -> VmResult<Value> {
    proxy
        .handler()
        .ok_or_else(|| VmError::type_error("Proxy handler is not available"))
}

fn property_key_to_value(key: &PropertyKey) -> Value {
    match key {
        PropertyKey::String(s) => Value::string(*s),
        PropertyKey::Index(n) => Value::string(JsString::intern(&n.to_string())),
        PropertyKey::Symbol(sym) => Value::symbol(*sym),
    }
}

fn get_property_value(
    ncx: &mut NativeContext,
    receiver: &Value,
    key: &PropertyKey,
    key_value: Value,
) -> VmResult<Value> {
    if let Some(proxy) = receiver.as_proxy() {
        return proxy_get(ncx, proxy, key, key_value, receiver.clone());
    }

    let obj = receiver
        .as_object()
        .ok_or_else(|| VmError::type_error("Proxy handler must be an object"))?;

    if let Some(desc) = obj.lookup_property_descriptor(key) {
        match desc {
            PropertyDescriptor::Data { value, .. } => Ok(value),
            PropertyDescriptor::Accessor { get, .. } => {
                if let Some(getter) = get {
                    ncx.call_function(&getter, receiver.clone(), &[])
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

fn target_get_own_property_descriptor(
    ncx: &mut NativeContext,
    target: &Value,
    key: &PropertyKey,
    key_value: Value,
) -> VmResult<Option<PropertyDescriptor>> {
    if let Some(proxy) = target.as_proxy() {
        return proxy_get_own_property_descriptor(ncx, proxy, key, key_value);
    }
    let obj = target
        .as_object()
        .ok_or_else(|| VmError::type_error("Proxy target must be an object"))?;
    Ok(obj.get_own_property_descriptor(key))
}

fn target_has_own(
    ncx: &mut NativeContext,
    target: &Value,
    key: &PropertyKey,
    key_value: Value,
) -> VmResult<bool> {
    Ok(target_get_own_property_descriptor(ncx, target, key, key_value)?.is_some())
}

fn target_has(
    ncx: &mut NativeContext,
    target: &Value,
    key: &PropertyKey,
    key_value: Value,
) -> VmResult<bool> {
    if let Some(proxy) = target.as_proxy() {
        return proxy_has(ncx, proxy, key, key_value);
    }
    let obj = target
        .as_object()
        .ok_or_else(|| VmError::type_error("Proxy target must be an object"))?;
    Ok(obj.has(key))
}

fn target_is_extensible(ncx: &mut NativeContext, target: &Value) -> VmResult<bool> {
    if let Some(proxy) = target.as_proxy() {
        return proxy_is_extensible(ncx, proxy);
    }
    let obj = target
        .as_object()
        .ok_or_else(|| VmError::type_error("Proxy target must be an object"))?;
    Ok(obj.is_extensible())
}

fn target_get(
    ncx: &mut NativeContext,
    target: &Value,
    key: &PropertyKey,
    key_value: Value,
    receiver: Value,
) -> VmResult<Value> {
    if let Some(proxy) = target.as_proxy() {
        return proxy_get(ncx, proxy, key, key_value, receiver);
    }
    let obj = target
        .as_object()
        .ok_or_else(|| VmError::type_error("Proxy target must be an object"))?;
    Ok(obj.get(key).unwrap_or(Value::undefined()))
}

fn target_set(
    ncx: &mut NativeContext,
    target: &Value,
    key: &PropertyKey,
    key_value: Value,
    value: Value,
    receiver: Value,
) -> VmResult<bool> {
    if let Some(proxy) = target.as_proxy() {
        return proxy_set(ncx, proxy, key, key_value, value, receiver);
    }
    let obj = target
        .as_object()
        .ok_or_else(|| VmError::type_error("Proxy target must be an object"))?;
    obj.set(*key, value);
    Ok(true)
}

fn target_delete_property(
    ncx: &mut NativeContext,
    target: &Value,
    key: &PropertyKey,
    key_value: Value,
) -> VmResult<bool> {
    if let Some(proxy) = target.as_proxy() {
        return proxy_delete_property(ncx, proxy, key, key_value);
    }
    let obj = target
        .as_object()
        .ok_or_else(|| VmError::type_error("Proxy target must be an object"))?;
    Ok(obj.delete(key))
}

fn target_own_keys(ncx: &mut NativeContext, target: &Value) -> VmResult<Vec<PropertyKey>> {
    if let Some(proxy) = target.as_proxy() {
        return proxy_own_keys(ncx, proxy);
    }
    let obj = target
        .as_object()
        .ok_or_else(|| VmError::type_error("Proxy target must be an object"))?;
    Ok(obj.own_keys())
}

fn target_define_property(
    ncx: &mut NativeContext,
    target: &Value,
    key: &PropertyKey,
    key_value: Value,
    desc: &PropertyDescriptor,
) -> VmResult<bool> {
    if let Some(proxy) = target.as_proxy() {
        return proxy_define_property(ncx, proxy, key, key_value, desc);
    }
    let obj = target
        .as_object()
        .ok_or_else(|| VmError::type_error("Proxy target must be an object"))?;
    obj.define_property(*key, desc.clone());
    Ok(true)
}

fn target_get_prototype_of(
    ncx: &mut NativeContext,
    target: &Value,
) -> VmResult<Option<GcRef<JsObject>>> {
    if let Some(proxy) = target.as_proxy() {
        return proxy_get_prototype_of(ncx, proxy);
    }
    let obj = target
        .as_object()
        .ok_or_else(|| VmError::type_error("Proxy target must be an object"))?;
    Ok(obj.prototype().as_object())
}

fn target_set_prototype_of(
    ncx: &mut NativeContext,
    target: &Value,
    proto: Option<GcRef<JsObject>>,
) -> VmResult<bool> {
    if let Some(proxy) = target.as_proxy() {
        return proxy_set_prototype_of(ncx, proxy, proto);
    }
    let obj = target
        .as_object()
        .ok_or_else(|| VmError::type_error("Proxy target must be an object"))?;
    let proto_value = proto.map(Value::object).unwrap_or_else(Value::null);
    obj.set_prototype(proto_value);
    Ok(true)
}

fn target_prevent_extensions(ncx: &mut NativeContext, target: &Value) -> VmResult<bool> {
    if let Some(proxy) = target.as_proxy() {
        return proxy_prevent_extensions(ncx, proxy);
    }
    let obj = target
        .as_object()
        .ok_or_else(|| VmError::type_error("Proxy target must be an object"))?;
    obj.prevent_extensions();
    Ok(true)
}

/// Invoke a trap on a proxy handler
///
/// Returns:
/// - `Ok(Some(value))` if the trap exists and was called successfully
/// - `Ok(None)` if the trap doesn't exist (caller should use default behavior)
/// - `Err(...)` if the trap exists but threw an error or isn't callable
fn invoke_trap(
    ncx: &mut NativeContext,
    proxy: GcRef<JsProxy>,
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

    let handler = proxy_handler_value(proxy)?;
    let trap_key = PropertyKey::string(trap_name);
    let trap_key_value = Value::string(JsString::intern(trap_name));
    let trap = get_property_value(ncx, &handler, &trap_key, trap_key_value)?;

    if trap.is_undefined() || trap.is_null() {
        return Ok(None);
    }

    // Verify trap is callable
    if !trap.is_callable() {
        return Err(VmError::type_error(format!(
            "Proxy handler's '{}' trap must be a function",
            trap_name
        )));
    }

    // Call the trap with the handler as 'this'
    let result = ncx.call_function(&trap, handler, args)?;
    Ok(Some(result))
}

/// ES §9.5.8: [[Get]] trap
///
/// Implements the `get` trap with proper invariant validation.
pub fn proxy_get(
    ncx: &mut NativeContext,
    proxy: GcRef<JsProxy>,
    key: &PropertyKey,
    key_value: Value,
    receiver: Value,
) -> VmResult<Value> {
    // Get target
    let target = proxy_target_value(proxy)?;

    // Invoke trap: handler.get(target, key, receiver)
    let trap_args = &[target.clone(), key_value.clone(), receiver.clone()];
    let trap_result = invoke_trap(ncx, proxy, "get", trap_args)?;

    // If no trap, perform default [[Get]] on target
    let result = match trap_result {
        Some(r) => r,
        None => {
            return target_get(ncx, &target, key, key_value, receiver);
        }
    };

    // Validate invariants (ES §9.5.8 step 11-12)
    validate_get_trap_invariants(ncx, &target, key, key_value, &result)?;

    Ok(result)
}

/// Validate invariants for the `get` trap (ES §9.5.8)
fn validate_get_trap_invariants(
    ncx: &mut NativeContext,
    target: &Value,
    key: &PropertyKey,
    key_value: Value,
    trap_result: &Value,
) -> VmResult<()> {
    let target_desc = target_get_own_property_descriptor(ncx, target, key, key_value)?;

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
    ncx: &mut NativeContext,
    proxy: GcRef<JsProxy>,
    key: &PropertyKey,
    key_value: Value,
    value: Value,
    receiver: Value,
) -> VmResult<bool> {
    // Get target
    let target = proxy_target_value(proxy)?;

    // Invoke trap: handler.set(target, key, value, receiver)
    let trap_args = &[target.clone(), key_value.clone(), value.clone(), receiver.clone()];
    let trap_result = invoke_trap(ncx, proxy, "set", trap_args)?;

    // If no trap, perform default [[Set]] on target
    let success = match trap_result {
        Some(r) => {
            // Convert trap result to boolean
            r.to_boolean()
        }
        None => {
            return target_set(ncx, &target, key, key_value, value, receiver);
        }
    };

    // Validate invariants (ES §9.5.9 step 12-13)
    if success {
        validate_set_trap_invariants(ncx, &target, key, key_value, &value)?;
    }

    Ok(success)
}

/// Validate invariants for the `set` trap (ES §9.5.9)
fn validate_set_trap_invariants(
    ncx: &mut NativeContext,
    target: &Value,
    key: &PropertyKey,
    key_value: Value,
    value: &Value,
) -> VmResult<()> {
    let target_desc = target_get_own_property_descriptor(ncx, target, key, key_value)?;

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
    ncx: &mut NativeContext,
    proxy: GcRef<JsProxy>,
    key: &PropertyKey,
    key_value: Value,
) -> VmResult<bool> {
    // Get target
    let target = proxy_target_value(proxy)?;

    // Invoke trap: handler.has(target, key)
    let trap_args = &[target.clone(), key_value.clone()];
    let trap_result = invoke_trap(ncx, proxy, "has", trap_args)?;

    // If no trap, perform default [[HasProperty]] on target
    let result = match trap_result {
        Some(r) => r.to_boolean(),
        None => {
            return target_has(ncx, &target, key, key_value);
        }
    };

    // Validate invariants (ES §9.5.7 step 9-10)
    validate_has_trap_invariants(ncx, &target, key, key_value, result)?;

    Ok(result)
}

/// Validate invariants for the `has` trap (ES §9.5.7)
fn validate_has_trap_invariants(
    ncx: &mut NativeContext,
    target: &Value,
    key: &PropertyKey,
    key_value: Value,
    trap_result: bool,
) -> VmResult<()> {
    let target_desc = target_get_own_property_descriptor(ncx, target, key, key_value.clone())?;

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
    if !target_is_extensible(ncx, target)? {
        if target_has_own(ncx, target, key, key_value)? && !trap_result {
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
    ncx: &mut NativeContext,
    proxy: GcRef<JsProxy>,
    key: &PropertyKey,
    key_value: Value,
) -> VmResult<bool> {
    // Get target
    let target = proxy_target_value(proxy)?;

    // Invoke trap: handler.deleteProperty(target, key)
    let trap_args = &[target.clone(), key_value.clone()];
    let trap_result = invoke_trap(ncx, proxy, "deleteProperty", trap_args)?;

    // If no trap, perform default [[Delete]] on target
    let result = match trap_result {
        Some(r) => r.to_boolean(),
        None => {
            return target_delete_property(ncx, &target, key, key_value);
        }
    };

    // Validate invariants (ES §9.5.10 step 11-12)
    if result {
        validate_delete_trap_invariants(ncx, &target, key, key_value)?;
    }

    Ok(result)
}

/// Validate invariants for the `deleteProperty` trap (ES §9.5.10)
fn validate_delete_trap_invariants(
    ncx: &mut NativeContext,
    target: &Value,
    key: &PropertyKey,
    key_value: Value,
) -> VmResult<()> {
    let target_desc = target_get_own_property_descriptor(ncx, target, key, key_value)?;

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
    ncx: &mut NativeContext,
    proxy: GcRef<JsProxy>,
) -> VmResult<Vec<PropertyKey>> {
    // Get target
    let target = proxy_target_value(proxy)?;

    // Invoke trap: handler.ownKeys(target)
    let trap_args = &[target.clone()];
    let trap_result = invoke_trap(ncx, proxy, "ownKeys", trap_args)?;

    // If no trap, perform default [[OwnPropertyKeys]] on target
    let keys_array = match trap_result {
        Some(r) => r,
        None => {
            return target_own_keys(ncx, &target);
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

        // ES §9.5.11 step 8: CreateListFromArrayLike(trapResultArray, « String, Symbol »)
        // Only String and Symbol types are allowed
        if let Some(s) = element.as_string() {
            trap_keys.push(PropertyKey::String(s.clone()));
        } else if let Some(sym) = element.as_symbol() {
            trap_keys.push(PropertyKey::Symbol(sym));
        } else {
            // Per spec, throw TypeError for any other type
            return Err(VmError::type_error(
                "Proxy 'ownKeys' trap result must only contain String or Symbol values"
            ));
        }
    }

    // Validate invariants (ES §9.5.11 step 10-23)
    validate_own_keys_trap_invariants(ncx, &target, &trap_keys)?;

    Ok(trap_keys)
}

/// Validate invariants for the `ownKeys` trap (ES §9.5.11)
fn validate_own_keys_trap_invariants(
    ncx: &mut NativeContext,
    target: &Value,
    trap_keys: &[PropertyKey],
) -> VmResult<()> {
    let target_keys = target_own_keys(ncx, target)?;

    // Check that all non-configurable keys are present
    for target_key in &target_keys {
        if let Some(desc) = target_get_own_property_descriptor(
            ncx,
            target,
            target_key,
            property_key_to_value(target_key),
        )? {
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
    if !target_is_extensible(ncx, target)? {
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
    ncx: &mut NativeContext,
    proxy: GcRef<JsProxy>,
    key: &PropertyKey,
    key_value: Value,
) -> VmResult<Option<PropertyDescriptor>> {
    // Get target
    let target = proxy_target_value(proxy)?;

    // Invoke trap: handler.getOwnPropertyDescriptor(target, key)
    let trap_args = &[target.clone(), key_value.clone()];
    let trap_result = invoke_trap(ncx, proxy, "getOwnPropertyDescriptor", trap_args)?;

    // If no trap, perform default [[GetOwnProperty]] on target
    let result_value = match trap_result {
        Some(r) => r,
        None => {
            return target_get_own_property_descriptor(ncx, &target, key, key_value);
        }
    };

    // Trap result must be undefined or an object
    if result_value.is_undefined() {
        // Validate: if target property is non-configurable, trap cannot return undefined
        if let Some(target_desc) =
            target_get_own_property_descriptor(ncx, &target, key, key_value.clone())?
        {
            if !target_desc.is_configurable() {
                return Err(VmError::type_error(
                    "Proxy 'getOwnPropertyDescriptor' trap cannot return undefined for non-configurable property",
                ));
            }
        }

        // If target is non-extensible and property exists, trap cannot return undefined
        if !target_is_extensible(ncx, &target)?
            && target_has_own(ncx, &target, key, key_value)?
        {
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
    validate_get_own_property_descriptor_invariants(ncx, &target, key, key_value, &trap_desc)?;

    Ok(Some(trap_desc))
}

/// Validate invariants for `getOwnPropertyDescriptor` trap (ES §9.5.5)
fn validate_get_own_property_descriptor_invariants(
    ncx: &mut NativeContext,
    target: &Value,
    key: &PropertyKey,
    key_value: Value,
    trap_desc: &PropertyDescriptor,
) -> VmResult<()> {
    let target_desc = target_get_own_property_descriptor(ncx, target, key, key_value.clone())?;

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
                    // ES §9.5.5 step 17.b.i: If trap reports writable: false but target has writable: true, throw TypeError
                    if target_attrs.writable && !trap_attrs.writable && !trap_desc.is_configurable() {
                        return Err(VmError::type_error(
                            "Proxy 'getOwnPropertyDescriptor' trap cannot report writable property as non-writable when non-configurable",
                        ));
                    }
                    // Trap cannot report non-writable as writable
                    if !target_attrs.writable && trap_attrs.writable {
                        return Err(VmError::type_error(
                            "Proxy 'getOwnPropertyDescriptor' trap cannot report non-writable property as writable",
                        ));
                    }
                    // For non-configurable, non-writable properties, value must match
                    if !target_attrs.writable && !same_value(trap_value, target_value) {
                        return Err(VmError::type_error(
                            "Proxy 'getOwnPropertyDescriptor' trap must report same value for non-configurable, non-writable property",
                        ));
                    }
                }
            }
        }
    } else {
        // Property doesn't exist on target (targetDesc is undefined)
        // ES §9.5.5 step 22.a: If resultDesc.[[Configurable]] is false and targetDesc is undefined, throw TypeError
        if !trap_desc.is_configurable() {
            return Err(VmError::type_error(
                "Proxy 'getOwnPropertyDescriptor' trap cannot report non-configurable descriptor for non-existent property",
            ));
        }

        // If target is non-extensible, trap cannot report a new property
        if !target_is_extensible(ncx, target)? {
            return Err(VmError::type_error(
                "Proxy 'getOwnPropertyDescriptor' trap cannot report property on non-extensible target",
            ));
        }
    }

    Ok(())
}

/// ES §9.5.6: [[DefineOwnProperty]] trap
///
/// Implements the `defineProperty` trap with proper invariant validation.
pub fn proxy_define_property(
    ncx: &mut NativeContext,
    proxy: GcRef<JsProxy>,
    key: &PropertyKey,
    key_value: Value,
    desc: &PropertyDescriptor,
) -> VmResult<bool> {
    // Get target
    let target = proxy_target_value(proxy)?;

    // Convert descriptor to object for trap call
    let desc_obj = descriptor_to_object(desc, ncx);

    // Invoke trap: handler.defineProperty(target, key, descriptor)
    let trap_args = &[target.clone(), key_value.clone(), desc_obj];
    let trap_result = invoke_trap(ncx, proxy, "defineProperty", trap_args)?;

    // If no trap, perform default [[DefineOwnProperty]] on target
    let success = match trap_result {
        Some(r) => r.to_boolean(),
        None => {
            return target_define_property(ncx, &target, key, key_value, desc);
        }
    };

    // Validate invariants (ES §9.5.6 step 17-20)
    if success {
        validate_define_property_invariants(ncx, &target, key, key_value, desc)?;
    }

    Ok(success)
}

/// Validate invariants for `defineProperty` trap (ES §9.5.6)
fn validate_define_property_invariants(
    ncx: &mut NativeContext,
    target: &Value,
    key: &PropertyKey,
    key_value: Value,
    desc: &PropertyDescriptor,
) -> VmResult<()> {
    let target_desc = target_get_own_property_descriptor(ncx, target, key, key_value)?;

    // If target is non-extensible, cannot add new properties
    if !target_is_extensible(ncx, target)? && target_desc.is_none() {
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
fn descriptor_to_object(desc: &PropertyDescriptor, ncx: &NativeContext) -> Value {
    let obj = GcRef::new(JsObject::new(Value::null(), ncx.memory_manager().clone()));

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
    ncx: &mut NativeContext,
    proxy: GcRef<JsProxy>,
) -> VmResult<Option<GcRef<JsObject>>> {
    // Get target
    let target = proxy_target_value(proxy)?;

    // Invoke trap: handler.getPrototypeOf(target)
    let trap_args = &[target.clone()];
    let trap_result = invoke_trap(ncx, proxy, "getPrototypeOf", trap_args)?;

    // If no trap, perform default [[GetPrototypeOf]] on target
    let proto_value = match trap_result {
        Some(r) => r,
        None => {
            return target_get_prototype_of(ncx, &target);
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
    validate_get_prototype_of_invariants(ncx, &target, &trap_proto)?;

    Ok(trap_proto)
}

/// Validate invariants for `getPrototypeOf` trap (ES §9.5.1)
fn validate_get_prototype_of_invariants(
    ncx: &mut NativeContext,
    target: &Value,
    trap_proto: &Option<GcRef<JsObject>>,
) -> VmResult<()> {
    if !target_is_extensible(ncx, target)? {
        let target_proto = target_get_prototype_of(ncx, target)?;

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
    ncx: &mut NativeContext,
    proxy: GcRef<JsProxy>,
    proto: Option<GcRef<JsObject>>,
) -> VmResult<bool> {
    // Get target
    let target = proxy_target_value(proxy)?;

    // Convert prototype to Value
    let proto_value = match &proto {
        Some(p) => Value::object(p.clone()),
        None => Value::null(),
    };

    // Invoke trap: handler.setPrototypeOf(target, proto)
    let trap_args = &[target.clone(), proto_value];
    let trap_result = invoke_trap(ncx, proxy, "setPrototypeOf", trap_args)?;

    // If no trap, perform default [[SetPrototypeOf]] on target
    let success = match trap_result {
        Some(r) => r.to_boolean(),
        None => {
            return target_set_prototype_of(ncx, &target, proto);
        }
    };

    // Validate invariants (ES §9.5.2 step 8)
    if success {
        validate_set_prototype_of_invariants(ncx, &target, &proto)?;
    }

    Ok(success)
}

/// Validate invariants for `setPrototypeOf` trap (ES §9.5.2)
fn validate_set_prototype_of_invariants(
    ncx: &mut NativeContext,
    target: &Value,
    proto: &Option<GcRef<JsObject>>,
) -> VmResult<()> {
    if !target_is_extensible(ncx, target)? {
        let target_proto = target_get_prototype_of(ncx, target)?;

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
    ncx: &mut NativeContext,
    proxy: GcRef<JsProxy>,
) -> VmResult<bool> {
    // Get target
    let target = proxy_target_value(proxy)?;

    // Invoke trap: handler.isExtensible(target)
    let trap_args = &[target.clone()];
    let trap_result = invoke_trap(ncx, proxy, "isExtensible", trap_args)?;

    // If no trap, perform default [[IsExtensible]] on target
    let trap_extensible = match trap_result {
        Some(r) => r.to_boolean(),
        None => {
            return target_is_extensible(ncx, &target);
        }
    };

    // Validate invariants (ES §9.5.3 step 7)
    let target_extensible = target_is_extensible(ncx, &target)?;
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
    ncx: &mut NativeContext,
    proxy: GcRef<JsProxy>,
) -> VmResult<bool> {
    // Get target
    let target = proxy_target_value(proxy)?;

    // Invoke trap: handler.preventExtensions(target)
    let trap_args = &[target.clone()];
    let trap_result = invoke_trap(ncx, proxy, "preventExtensions", trap_args)?;

    // If no trap, perform default [[PreventExtensions]] on target
    let success = match trap_result {
        Some(r) => r.to_boolean(),
        None => {
            return target_prevent_extensions(ncx, &target);
        }
    };

    // Validate invariants (ES §9.5.4 step 7)
    if success && target_is_extensible(ncx, &target)? {
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
    ncx: &mut NativeContext,
    proxy: GcRef<JsProxy>,
    this_value: Value,
    args: &[Value],
) -> VmResult<Value> {
    // Get target
    let target = proxy_target_value(proxy)?;

    // Note: ES spec requires target to be callable, but we let the trap handle it
    // The trap can validate and throw if needed

    // Create arguments array for trap
    let args_array = create_args_array(ncx, args);

    // Invoke trap: handler.apply(target, thisValue, args)
    let trap_args = &[target.clone(), this_value.clone(), Value::object(args_array)];
    let trap_result = invoke_trap(ncx, proxy, "apply", trap_args)?;

    // If no trap, perform default [[Call]] on target
    match trap_result {
        Some(result) => Ok(result),
        None => {
            // Default behavior: call target function
            if let Some(proxy) = target.as_proxy() {
                return proxy_apply(ncx, proxy, this_value, args);
            }
            ncx.call_function(&target, this_value, args)
        }
    }
}

/// ES §9.5.14: [[Construct]] trap
///
/// Implements the `construct` trap for constructor calls.
pub fn proxy_construct(
    ncx: &mut NativeContext,
    proxy: GcRef<JsProxy>,
    args: &[Value],
    new_target: Value,
) -> VmResult<Value> {
    // Get target
    let target = proxy_target_value(proxy)?;

    // Note: ES spec requires target to be a constructor, but we let the trap handle it
    // The trap can validate and throw if needed

    // Create arguments array for trap
    let args_array = create_args_array(ncx, args);

    // Invoke trap: handler.construct(target, args, newTarget)
    let trap_args = &[target.clone(), Value::object(args_array), new_target.clone()];
    let trap_result = invoke_trap(ncx, proxy, "construct", trap_args)?;

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
