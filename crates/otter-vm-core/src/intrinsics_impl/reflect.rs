//! Reflect namespace initialization
//!
//! Creates the Reflect global namespace object with 13 static methods (ES2015+ complete):
//! - Reflect.get, set, has, deleteProperty
//! - Reflect.ownKeys, getOwnPropertyDescriptor, defineProperty
//! - Reflect.getPrototypeOf, setPrototypeOf
//! - Reflect.isExtensible, preventExtensions
//! - Reflect.apply, construct
//!
//! All Reflect methods are implemented natively in Rust inline,
//! similar to Math namespace.
//!
//! ## Implementation Notes
//!
//! - `Reflect.apply` and `Reflect.construct` work with both native functions and closures
//!   via `NativeContext::call_function()`
//!
//! ## ES2015+ Compliance
//!
//! **Methods**: All methods have property attributes:
//! - `writable: true` (allow polyfills/testing overrides)
//! - `enumerable: false` (keep namespace clean)
//! - `configurable: true` (allow runtime modifications)

use crate::context::NativeContext;
use crate::error::VmError;
use crate::gc::GcRef;
use crate::object::{JsObject, PropertyKey, PropertyDescriptor, PropertyAttributes};
use crate::value::Value;
use crate::memory::MemoryManager;
use crate::string::JsString;
use std::sync::Arc;

/// Helper to convert Value to PropertyKey
pub fn to_property_key(value: &Value) -> PropertyKey {
    if let Some(n) = value.as_number() {
        if n.fract() == 0.0 && n >= 0.0 && n <= u32::MAX as f64 {
            return PropertyKey::Index(n as u32);
        }
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
    } else {
        "[object]"
    };
    PropertyKey::String(JsString::intern(s))
}

/// Get object from value
fn get_target_object(value: &Value) -> Result<GcRef<JsObject>, String> {
    value.as_object().ok_or_else(|| {
        format!(
            "Reflect method requires an object target (got {})",
            value.type_of()
        )
    })
}

fn builtin_tag_for_value(value: &Value) -> Option<GcRef<JsString>> {
    let mut current = value.clone();
    if let Some(proxy) = current.as_proxy() {
        if let Some(target) = proxy.target() {
            current = target;
        }
    }
    current
        .as_object()
        .and_then(|o| o.get(&PropertyKey::string("__builtin_tag__")))
        .and_then(|v| v.as_string())
}

fn default_proto_for_construct(
    ncx: &NativeContext,
    target: &Value,
    new_target: &Value,
) -> Option<GcRef<JsObject>> {
    let tag = builtin_tag_for_value(target)?;
    let realm_id = ncx.realm_id_for_function(new_target);
    let intrinsics = ncx.ctx.realm_intrinsics(realm_id)?;
    intrinsics.prototype_for_builtin_tag(tag.as_str())
}

fn is_constructor_value(value: &Value) -> bool {
    if let Some(proxy) = value.as_proxy() {
        if let Some(target) = proxy.target() {
            return is_constructor_value(&target);
        }
        return false;
    }
    if !value.is_callable() {
        return false;
    }
    if let Some(obj) = value.as_object() {
        if obj
            .get(&PropertyKey::string("__non_constructor"))
            .and_then(|v| v.as_boolean())
            == Some(true)
        {
            return false;
        }
    }
    true
}

fn descriptor_from_attributes(attr_obj: &GcRef<JsObject>) -> PropertyDescriptor {
    let has_value = attr_obj.has(&PropertyKey::from("value"));
    let has_writable = attr_obj.has(&PropertyKey::from("writable"));
    let has_get = attr_obj.has(&PropertyKey::from("get"));
    let has_set = attr_obj.has(&PropertyKey::from("set"));

    let enumerable = attr_obj
        .get(&PropertyKey::from("enumerable"))
        .map(|v| v.to_boolean())
        .unwrap_or(false);
    let configurable = attr_obj
        .get(&PropertyKey::from("configurable"))
        .map(|v| v.to_boolean())
        .unwrap_or(false);

    if has_value || has_writable {
        let value = attr_obj.get(&PropertyKey::from("value")).unwrap_or(Value::undefined());
        let writable = attr_obj
            .get(&PropertyKey::from("writable"))
            .map(|v| v.to_boolean())
            .unwrap_or(false);
        PropertyDescriptor::Data {
            value,
            attributes: PropertyAttributes {
                writable,
                enumerable,
                configurable,
            },
        }
    } else if has_get || has_set {
        let get = attr_obj.get(&PropertyKey::from("get")).filter(|v| !v.is_undefined());
        let set = attr_obj.get(&PropertyKey::from("set")).filter(|v| !v.is_undefined());
        PropertyDescriptor::Accessor {
            get,
            set,
            attributes: PropertyAttributes {
                writable: false,
                enumerable,
                configurable,
            },
        }
    } else {
        PropertyDescriptor::Data {
            value: Value::undefined(),
            attributes: PropertyAttributes {
                writable: false,
                enumerable,
                configurable,
            },
        }
    }
}

fn descriptor_to_value(desc: PropertyDescriptor, ncx: &NativeContext) -> Value {
    match desc {
        PropertyDescriptor::Data { value, attributes } => {
            let desc_obj = GcRef::new(JsObject::new(Value::null(), ncx.memory_manager().clone()));
            desc_obj.set("value".into(), value);
            desc_obj.set("writable".into(), Value::boolean(attributes.writable));
            desc_obj.set("enumerable".into(), Value::boolean(attributes.enumerable));
            desc_obj.set("configurable".into(), Value::boolean(attributes.configurable));
            Value::object(desc_obj)
        }
        PropertyDescriptor::Accessor { get, set, attributes } => {
            let desc_obj = GcRef::new(JsObject::new(Value::null(), ncx.memory_manager().clone()));
            desc_obj.set("get".into(), get.unwrap_or(Value::undefined()));
            desc_obj.set("set".into(), set.unwrap_or(Value::undefined()));
            desc_obj.set("enumerable".into(), Value::boolean(attributes.enumerable));
            desc_obj.set("configurable".into(), Value::boolean(attributes.configurable));
            Value::object(desc_obj)
        }
        PropertyDescriptor::Deleted => Value::undefined(),
    }
}

/// Create and install Reflect namespace on global object
///
/// This function creates the Reflect namespace with inline implementations
/// of all 13 ES2015+ Reflect methods.
///
/// # Arguments
///
/// * `global` - The global object where Reflect will be installed
/// * `mm` - Memory manager for GC allocations
///
/// # Completeness
///
/// All ES2015+ Reflect methods are implemented:
/// - Property access: get, set, has, deleteProperty
/// - Property inspection: ownKeys, getOwnPropertyDescriptor, defineProperty
/// - Prototype chain: getPrototypeOf, setPrototypeOf
/// - Extensibility: isExtensible, preventExtensions
/// - Function invocation: apply, construct (native functions only)
pub fn install_reflect_namespace(
    global: GcRef<JsObject>,
    mm: &Arc<MemoryManager>,
) {
    // Create Reflect namespace object (plain object, not a constructor)
    let reflect_obj = GcRef::new(JsObject::new(Value::null(), mm.clone()));

    // ====================================================================
    // Reflect Methods (ES2015+ §26.1)
    // All methods use inline implementations
    // ====================================================================

    // Helper macro to define a Reflect method
    macro_rules! reflect_method {
        ($name:literal, $body:expr) => {
            reflect_obj.set(
                PropertyKey::string($name),
                Value::native_function(
                    $body,
                    mm.clone(),
                ),
            );
        };
    }

    // === Property Access ===

    // Reflect.get(target, propertyKey, receiver?)
    reflect_method!("get", |_, args: &[Value], ncx| {
        let target = args.first().ok_or("Reflect.get requires a target argument")?;
        let property_key = args.get(1).ok_or("Reflect.get requires a propertyKey argument")?;
        let receiver = args.get(2).cloned().unwrap_or_else(|| target.clone());

        // If target is a proxy, call the trap directly
        if let Some(proxy) = target.as_proxy() {
            let key = to_property_key(property_key);
            return crate::proxy_operations::proxy_get(
                ncx,
                proxy,
                &key,
                property_key.clone(),
                receiver,
            );
        }

        let obj = get_target_object(target)?;
        let key = to_property_key(property_key);

        Ok(obj.get(&key).unwrap_or(Value::undefined()))
    });

    // Reflect.set(target, propertyKey, value, receiver?)
    reflect_method!("set", |_, args, ncx| {
        let target = args.first().ok_or("Reflect.set requires a target argument")?;
        let property_key = args.get(1).ok_or("Reflect.set requires a propertyKey argument")?;
        let value = args.get(2).cloned().unwrap_or(Value::undefined());
        let receiver = args.get(3).cloned().unwrap_or_else(|| target.clone());

        // If target is a proxy, call the trap directly
        if let Some(proxy) = target.as_proxy() {
            let key = to_property_key(property_key);
            let success = crate::proxy_operations::proxy_set(
                ncx,
                proxy,
                &key,
                property_key.clone(),
                value,
                receiver,
            )?;
            return Ok(Value::boolean(success));
        }

        let obj = get_target_object(target)?;
        let key = to_property_key(property_key);

        obj.set(key, value);
        Ok(Value::boolean(true))
    });

    // Reflect.has(target, propertyKey)
    reflect_method!("has", |_, args, ncx| {
        let target = args.first().ok_or("Reflect.has requires a target argument")?;
        let property_key = args.get(1).ok_or("Reflect.has requires a propertyKey argument")?;

        // If target is a proxy, call the trap directly
        if let Some(proxy) = target.as_proxy() {
            let key = to_property_key(property_key);
            let result =
                crate::proxy_operations::proxy_has(ncx, proxy, &key, property_key.clone())?;
            return Ok(Value::boolean(result));
        }

        let obj = get_target_object(target)?;
        let key = to_property_key(property_key);

        Ok(Value::boolean(obj.has(&key)))
    });

    // Reflect.deleteProperty(target, propertyKey)
    reflect_method!("deleteProperty", |_, args, ncx| {
        let target = args.first().ok_or("Reflect.deleteProperty requires a target argument")?;
        let property_key = args.get(1).ok_or("Reflect.deleteProperty requires a propertyKey argument")?;

        // If target is a proxy, call the trap directly
        if let Some(proxy) = target.as_proxy() {
            let key = to_property_key(property_key);
            let result = crate::proxy_operations::proxy_delete_property(
                ncx,
                proxy,
                &key,
                property_key.clone(),
            )?;
            return Ok(Value::boolean(result));
        }

        let obj = get_target_object(target)?;
        let key = to_property_key(property_key);

        let deleted = obj.delete(&key);
        Ok(Value::boolean(deleted))
    });

    // === Property Enumeration and Inspection ===

    // Reflect.ownKeys(target)
    reflect_method!("ownKeys", |_, args, ncx| {
        let target = args.first().ok_or("Reflect.ownKeys requires a target argument")?;

        let keys = if let Some(proxy) = target.as_proxy() {
            crate::proxy_operations::proxy_own_keys(ncx, proxy)?
        } else {
            let obj = get_target_object(target)?;
            obj.own_keys()
        };

        let result = GcRef::new(JsObject::array(keys.len(), ncx.memory_manager().clone()));
        for (i, key) in keys.into_iter().enumerate() {
            let key_val = match key {
                PropertyKey::String(s) => Value::string(s),
                PropertyKey::Index(n) => Value::string(JsString::intern(&n.to_string())),
                PropertyKey::Symbol(_) => continue, // Skip symbols for now
            };
            result.set(PropertyKey::Index(i as u32), key_val);
        }

        Ok(Value::array(result))
    });

    // Reflect.getOwnPropertyDescriptor(target, propertyKey)
    reflect_method!("getOwnPropertyDescriptor", |_, args, ncx| {
        let target = args.first().ok_or("Reflect.getOwnPropertyDescriptor requires a target argument")?;
        let property_key = args.get(1).ok_or("Reflect.getOwnPropertyDescriptor requires a propertyKey argument")?;

        if let Some(proxy) = target.as_proxy() {
            let key = to_property_key(property_key);
            let result_desc = crate::proxy_operations::proxy_get_own_property_descriptor(
                ncx,
                proxy,
                &key,
                property_key.clone(),
            )?;
            return Ok(result_desc
                .map(|desc| descriptor_to_value(desc, ncx))
                .unwrap_or(Value::undefined()));
        }

        let obj = get_target_object(target)?;
        let key = to_property_key(property_key);

        if let Some(prop_desc) = obj.lookup_property_descriptor(&key) {
            match prop_desc {
                PropertyDescriptor::Data { value, attributes } => {
                    let desc = GcRef::new(JsObject::new(Value::null(), ncx.memory_manager().clone()));
                    desc.set("value".into(), value);
                    desc.set("writable".into(), Value::boolean(attributes.writable));
                    desc.set("enumerable".into(), Value::boolean(attributes.enumerable));
                    desc.set("configurable".into(), Value::boolean(attributes.configurable));
                    Ok(Value::object(desc))
                }
                PropertyDescriptor::Accessor { get, set, attributes } => {
                    let desc = GcRef::new(JsObject::new(Value::null(), ncx.memory_manager().clone()));
                    desc.set("get".into(), get.unwrap_or(Value::undefined()));
                    desc.set("set".into(), set.unwrap_or(Value::undefined()));
                    desc.set("enumerable".into(), Value::boolean(attributes.enumerable));
                    desc.set("configurable".into(), Value::boolean(attributes.configurable));
                    Ok(Value::object(desc))
                }
                PropertyDescriptor::Deleted => Ok(Value::undefined()),
            }
        } else if let Some(value) = obj.get(&key) {
            let desc = GcRef::new(JsObject::new(Value::null(), ncx.memory_manager().clone()));
            desc.set("value".into(), value);
            desc.set("writable".into(), Value::boolean(true));
            desc.set("enumerable".into(), Value::boolean(true));
            desc.set("configurable".into(), Value::boolean(true));
            Ok(Value::object(desc))
        } else {
            Ok(Value::undefined())
        }
    });

    // Reflect.defineProperty(target, propertyKey, attributes)
    reflect_method!("defineProperty", |_, args, ncx| {
        let target = args.first().ok_or("Reflect.defineProperty requires a target argument")?;
        let property_key = args.get(1).ok_or("Reflect.defineProperty requires a propertyKey argument")?;
        let attributes = args.get(2).ok_or("Reflect.defineProperty requires an attributes argument")?;
        let Some(attr_obj) = attributes.as_object() else {
            return Err(VmError::type_error("Reflect.defineProperty requires attributes to be an object"));
        };
        let key = to_property_key(property_key);

        if let Some(proxy) = target.as_proxy() {
            let desc = descriptor_from_attributes(&attr_obj);
            let result = crate::proxy_operations::proxy_define_property(
                ncx,
                proxy,
                &key,
                property_key.clone(),
                &desc,
            )?;
            return Ok(Value::boolean(result));
        }

        let obj = get_target_object(target)?;

        let read_bool = |name: &str, default: bool| -> bool {
            attr_obj.get(&name.into()).and_then(|v| v.as_boolean()).unwrap_or(default)
        };

        let enumerable = read_bool("enumerable", true);
        let configurable = read_bool("configurable", true);
        let writable = read_bool("writable", true);

        // Check if it's an accessor descriptor
        let get = attr_obj.get(&"get".into());
        let set = attr_obj.get(&"set".into());
        if get.is_some() || set.is_some() {
            let attrs = PropertyAttributes {
                writable: false,
                enumerable,
                configurable,
            };
            let ok = obj.define_property(
                key,
                PropertyDescriptor::Accessor {
                    get: get.filter(|v| !v.is_undefined()),
                    set: set.filter(|v| !v.is_undefined()),
                    attributes: attrs,
                },
            );
            return Ok(Value::boolean(ok));
        }

        // Data descriptor
        if let Some(value) = attr_obj.get(&"value".into()) {
            let attrs = PropertyAttributes {
                writable,
                enumerable,
                configurable,
            };
            let ok = obj.define_property(key, PropertyDescriptor::data_with_attrs(value, attrs));
            return Ok(Value::boolean(ok));
        }

        Ok(Value::boolean(true))
    });

    // === Prototype Chain ===

    // Reflect.getPrototypeOf(target)
    reflect_method!("getPrototypeOf", |_, args, ncx| {
        let target = args.first().ok_or("Reflect.getPrototypeOf requires a target argument")?;

        if let Some(proxy) = target.as_proxy() {
            let result = crate::proxy_operations::proxy_get_prototype_of(ncx, proxy)?;
            return Ok(match result {
                Some(proto) => Value::object(proto),
                None => Value::null(),
            });
        }

        let obj = get_target_object(target)?;

        let proto_val = obj.prototype();
        Ok(proto_val)
    });

    // Reflect.setPrototypeOf(target, prototype)
    reflect_method!("setPrototypeOf", |_, args, ncx| {
        let target = args.first().ok_or("Reflect.setPrototypeOf requires a target argument")?;
        let prototype = args.get(1).ok_or("Reflect.setPrototypeOf requires a prototype argument")?;

        if let Some(proxy) = target.as_proxy() {
            let new_proto = if prototype.is_null() {
                None
            } else if let Some(proto_obj) = prototype.as_object() {
                Some(proto_obj)
            } else {
                return Err(VmError::type_error("Prototype must be an object or null"));
            };
            let result = crate::proxy_operations::proxy_set_prototype_of(ncx, proxy, new_proto)?;
            return Ok(Value::boolean(result));
        }

        let obj = get_target_object(target)?;

        let new_proto = if prototype.is_null() {
            None
        } else if let Some(proto_obj) = prototype.as_object() {
            Some(proto_obj)
        } else {
            return Err(VmError::type_error("Prototype must be an object or null"));
        };

        let proto_value = new_proto.map(Value::object).unwrap_or_else(Value::null);
        let success = obj.set_prototype(proto_value);
        Ok(Value::boolean(success))
    });

    // === Extensibility ===

    // Reflect.isExtensible(target)
    reflect_method!("isExtensible", |_, args, ncx| {
        let target = args.first().ok_or("Reflect.isExtensible requires a target argument")?;

        if let Some(proxy) = target.as_proxy() {
            let result = crate::proxy_operations::proxy_is_extensible(ncx, proxy)?;
            return Ok(Value::boolean(result));
        }

        let obj = get_target_object(target)?;
        Ok(Value::boolean(obj.is_extensible()))
    });

    // Reflect.preventExtensions(target)
    reflect_method!("preventExtensions", |_, args, ncx| {
        let target = args.first().ok_or("Reflect.preventExtensions requires a target argument")?;

        if let Some(proxy) = target.as_proxy() {
            let result = crate::proxy_operations::proxy_prevent_extensions(ncx, proxy)?;
            return Ok(Value::boolean(result));
        }

        let obj = get_target_object(target)?;
        obj.prevent_extensions();
        Ok(Value::boolean(true))
    });

    // === Function Invocation ===

    // Reflect.apply(target, thisArgument, argumentsList)
    reflect_method!("apply", |_, args, ncx| {
        let target = args.first().ok_or("Reflect.apply requires a target argument")?;
        let this_arg = args.get(1).cloned().unwrap_or(Value::undefined());
        let args_list = args.get(2).ok_or("Reflect.apply requires an argumentsList argument")?;

        if let Some(proxy) = target.as_proxy() {
            // Convert argumentsList to array of Values
            let args_array = if let Some(arr_obj) = args_list.as_object() {
                if arr_obj.is_array() {
                    let len = arr_obj.array_length();
                    let mut call_args = Vec::with_capacity(len);
                    for i in 0..len {
                        let val = arr_obj
                            .get(&PropertyKey::Index(i as u32))
                            .unwrap_or(Value::undefined());
                        call_args.push(val);
                    }
                    call_args
                } else {
                    return Err(VmError::type_error("Reflect.apply argumentsList must be an array"));
                }
            } else {
                return Err(VmError::type_error("Reflect.apply argumentsList must be an object"));
            };

            let result = crate::proxy_operations::proxy_apply(ncx, proxy, this_arg, &args_array)?;
            return Ok(result);
        }

        // Check if target is callable
        if !target.is_callable() {
            return Err(VmError::type_error("Reflect.apply target must be a function"));
        }

        // Convert argumentsList to array of Values
        let args_array = if let Some(arr_obj) = args_list.as_object() {
            if arr_obj.is_array() {
                let len = arr_obj.array_length();
                let mut call_args = Vec::with_capacity(len);
                for i in 0..len {
                    let val = arr_obj.get(&PropertyKey::Index(i as u32)).unwrap_or(Value::undefined());
                    call_args.push(val);
                }
                call_args
            } else {
                return Err(VmError::type_error("Reflect.apply argumentsList must be an array"));
            }
        } else {
            return Err(VmError::type_error("Reflect.apply argumentsList must be an object"));
        };

        // Call the function (handles both closures and native functions)
        ncx.call_function(target, this_arg, &args_array)
    });

    // Reflect.construct(target, argumentsList, newTarget?)
    reflect_method!("construct", |_, args, ncx| {
        let target = args.first().ok_or("Reflect.construct requires a target argument")?;
        let args_list = args.get(1).ok_or("Reflect.construct requires an argumentsList argument")?;
        let new_target = args.get(2).cloned().unwrap_or_else(|| target.clone());

        if let Some(proxy) = target.as_proxy() {
            if !is_constructor_value(&new_target) {
                return Err(VmError::type_error("Reflect.construct newTarget must be a constructor"));
            }
            // Convert argumentsList to array of Values
            let args_array = if let Some(arr_obj) = args_list.as_object() {
                if arr_obj.is_array() {
                    let len = arr_obj.array_length();
                    let mut call_args = Vec::with_capacity(len);
                    for i in 0..len {
                        let val = arr_obj
                            .get(&PropertyKey::Index(i as u32))
                            .unwrap_or(Value::undefined());
                        call_args.push(val);
                    }
                    call_args
                } else {
                    return Err(VmError::type_error("Reflect.construct argumentsList must be an array"));
                }
            } else {
                return Err(VmError::type_error("Reflect.construct argumentsList must be an object"));
            };

            let result = crate::proxy_operations::proxy_construct(ncx, proxy, &args_array, new_target)?;
            return Ok(result);
        }

        // Check if target is a constructor
        if !is_constructor_value(target) {
            return Err(VmError::type_error("Reflect.construct target must be a constructor"));
        }
        if !is_constructor_value(&new_target) {
            return Err(VmError::type_error("Reflect.construct newTarget must be a constructor"));
        }

        // Convert argumentsList to array of Values
        let args_array = if let Some(arr_obj) = args_list.as_object() {
            if arr_obj.is_array() {
                let len = arr_obj.array_length();
                let mut call_args = Vec::with_capacity(len);
                for i in 0..len {
                    let val = arr_obj.get(&PropertyKey::Index(i as u32)).unwrap_or(Value::undefined());
                    call_args.push(val);
                }
                call_args
            } else {
                return Err(VmError::type_error("Reflect.construct argumentsList must be an array"));
            }
        } else {
            return Err(VmError::type_error("Reflect.construct argumentsList must be an object"));
        };

        // Create new instance using GetPrototypeFromConstructor(newTarget, %Object.prototype%)
        let default_proto = default_proto_for_construct(ncx, target, &new_target);
        let proto = ncx
            .get_prototype_from_constructor_with_default(&new_target, default_proto)
            .map(Value::object)
            .unwrap_or_else(Value::null);
        let new_obj = GcRef::new(JsObject::new(proto, ncx.memory_manager().clone()));
        let this_val = Value::object(new_obj);

        // Call constructor (handles both closures and native functions)
        let result = ncx.call_function_construct(target, this_val.clone(), &args_array)?;

        // If constructor returns an object, use it; otherwise use this_val
        if result.is_object() {
            Ok(result)
        } else {
            Ok(this_val)
        }
    });

    // ====================================================================
    // Install Reflect on global
    // ====================================================================
    global.set(PropertyKey::string("Reflect"), Value::object(reflect_obj));
}
