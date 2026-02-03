//! Reflect namespace initialization
//!
//! Creates the Reflect global namespace object with 13 static methods (ES2015+ complete):
//! - Reflect.get, set, has, deleteProperty
//! - Reflect.ownKeys, getOwnPropertyDescriptor, defineProperty
//! - Reflect.getPrototypeOf, setPrototypeOf
//! - Reflect.isExtensible, preventExtensions
//! - Reflect.apply, construct (with native function support)
//!
//! All Reflect methods are implemented natively in Rust inline,
//! similar to Math namespace.
//!
//! ## Implementation Notes
//!
//! - `Reflect.apply` and `Reflect.construct` work with native functions
//! - Closure/bytecode function calls require VmContext and are not fully supported in intrinsics
//!
//! ## ES2015+ Compliance
//!
//! **Methods**: All methods have property attributes:
//! - `writable: true` (allow polyfills/testing overrides)
//! - `enumerable: false` (keep namespace clean)
//! - `configurable: true` (allow runtime modifications)
//!
//! ## Limitations
//!
//! - **Reflect.apply** and **Reflect.construct**: Work with native functions but not with closures/bytecode functions (requires VmContext)

use crate::error::VmError;
use crate::gc::GcRef;
use crate::object::{JsObject, PropertyKey, PropertyDescriptor, PropertyAttributes};
use crate::value::Value;
use crate::memory::MemoryManager;
use crate::string::JsString;
use std::sync::Arc;

/// Helper to convert Value to PropertyKey
fn to_property_key(value: &Value) -> PropertyKey {
    if let Some(n) = value.as_number() {
        if n.fract() == 0.0 && n >= 0.0 && n <= u32::MAX as f64 {
            return PropertyKey::Index(n as u32);
        }
    }
    if let Some(s) = value.as_string() {
        return PropertyKey::String(s);
    }
    if let Some(sym) = value.as_symbol() {
        return PropertyKey::Symbol(sym.id);
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
    let reflect_obj = GcRef::new(JsObject::new(None, mm.clone()));

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
    reflect_method!("get", |_, args: &[Value], _ncx| {
        let target = args.first().ok_or("Reflect.get requires a target argument")?;
        let property_key = args.get(1).ok_or("Reflect.get requires a propertyKey argument")?;

        // If target is a proxy, signal the interpreter to handle it
        if target.as_proxy().is_some() {
            return Err(VmError::interception(
                crate::error::InterceptionSignal::ReflectGetProxy,
            ));
        }

        let obj = get_target_object(target)?;
        let key = to_property_key(property_key);

        Ok(obj.get(&key).unwrap_or(Value::undefined()))
    });

    // Reflect.set(target, propertyKey, value, receiver?)
    reflect_method!("set", |_, args, _ncx| {
        let target = args.first().ok_or("Reflect.set requires a target argument")?;
        let property_key = args.get(1).ok_or("Reflect.set requires a propertyKey argument")?;
        let value = args.get(2).cloned().unwrap_or(Value::undefined());

        // If target is a proxy, signal the interpreter to handle it
        if target.as_proxy().is_some() {
            return Err(VmError::interception(
                crate::error::InterceptionSignal::ReflectSetProxy,
            ));
        }

        let obj = get_target_object(target)?;
        let key = to_property_key(property_key);

        obj.set(key, value);
        Ok(Value::boolean(true))
    });

    // Reflect.has(target, propertyKey)
    reflect_method!("has", |_, args, _ncx| {
        let target = args.first().ok_or("Reflect.has requires a target argument")?;
        let property_key = args.get(1).ok_or("Reflect.has requires a propertyKey argument")?;

        // If target is a proxy, signal the interpreter to handle it
        if target.as_proxy().is_some() {
            return Err(VmError::interception(
                crate::error::InterceptionSignal::ReflectHasProxy,
            ));
        }

        let obj = get_target_object(target)?;
        let key = to_property_key(property_key);

        Ok(Value::boolean(obj.has(&key)))
    });

    // Reflect.deleteProperty(target, propertyKey)
    reflect_method!("deleteProperty", |_, args, _ncx| {
        let target = args.first().ok_or("Reflect.deleteProperty requires a target argument")?;
        let property_key = args.get(1).ok_or("Reflect.deleteProperty requires a propertyKey argument")?;

        // If target is a proxy, signal the interpreter to handle it
        if target.as_proxy().is_some() {
            return Err(VmError::interception(
                crate::error::InterceptionSignal::ReflectDeletePropertyProxy,
            ));
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

        // Check if target is a proxy
        if target.as_proxy().is_some() {
            return Err(VmError::interception(crate::error::InterceptionSignal::ReflectOwnKeysProxy));
        }

        let obj = get_target_object(target)?;
        let keys = obj.own_keys();

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

        // Check if target is a proxy
        if target.as_proxy().is_some() {
            return Err(VmError::interception(crate::error::InterceptionSignal::ReflectGetOwnPropertyDescriptorProxy));
        }

        let obj = get_target_object(target)?;
        let key = to_property_key(property_key);

        if let Some(prop_desc) = obj.lookup_property_descriptor(&key) {
            match prop_desc {
                PropertyDescriptor::Data { value, attributes } => {
                    let desc = GcRef::new(JsObject::new(None, ncx.memory_manager().clone()));
                    desc.set("value".into(), value);
                    desc.set("writable".into(), Value::boolean(attributes.writable));
                    desc.set("enumerable".into(), Value::boolean(attributes.enumerable));
                    desc.set("configurable".into(), Value::boolean(attributes.configurable));
                    Ok(Value::object(desc))
                }
                PropertyDescriptor::Accessor { get, set, attributes } => {
                    let desc = GcRef::new(JsObject::new(None, ncx.memory_manager().clone()));
                    desc.set("get".into(), get.unwrap_or(Value::undefined()));
                    desc.set("set".into(), set.unwrap_or(Value::undefined()));
                    desc.set("enumerable".into(), Value::boolean(attributes.enumerable));
                    desc.set("configurable".into(), Value::boolean(attributes.configurable));
                    Ok(Value::object(desc))
                }
                PropertyDescriptor::Deleted => Ok(Value::undefined()),
            }
        } else if let Some(value) = obj.get(&key) {
            let desc = GcRef::new(JsObject::new(None, ncx.memory_manager().clone()));
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
    reflect_method!("defineProperty", |_, args, _ncx| {
        let target = args.first().ok_or("Reflect.defineProperty requires a target argument")?;
        let property_key = args.get(1).ok_or("Reflect.defineProperty requires a propertyKey argument")?;
        let attributes = args.get(2).ok_or("Reflect.defineProperty requires an attributes argument")?;

        // Check if target is a proxy
        if target.as_proxy().is_some() {
            return Err(VmError::interception(crate::error::InterceptionSignal::ReflectDefinePropertyProxy));
        }

        let obj = get_target_object(target)?;
        let key = to_property_key(property_key);

        let Some(attr_obj) = attributes.as_object() else {
            return Err(VmError::type_error("Reflect.defineProperty requires attributes to be an object"));
        };

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
    reflect_method!("getPrototypeOf", |_, args, _ncx| {
        let target = args.first().ok_or("Reflect.getPrototypeOf requires a target argument")?;

        // Check if target is a proxy
        if target.as_proxy().is_some() {
            return Err(VmError::interception(crate::error::InterceptionSignal::ReflectGetPrototypeOfProxy));
        }

        let obj = get_target_object(target)?;

        match obj.prototype() {
            Some(proto) => Ok(Value::object(proto)),
            None => Ok(Value::null()),
        }
    });

    // Reflect.setPrototypeOf(target, prototype)
    reflect_method!("setPrototypeOf", |_, args, _ncx| {
        let target = args.first().ok_or("Reflect.setPrototypeOf requires a target argument")?;
        let prototype = args.get(1).ok_or("Reflect.setPrototypeOf requires a prototype argument")?;

        // Check if target is a proxy
        if target.as_proxy().is_some() {
            return Err(VmError::interception(crate::error::InterceptionSignal::ReflectSetPrototypeOfProxy));
        }

        let obj = get_target_object(target)?;

        let new_proto = if prototype.is_null() {
            None
        } else if let Some(proto_obj) = prototype.as_object() {
            Some(proto_obj)
        } else {
            return Err(VmError::type_error("Prototype must be an object or null"));
        };

        let success = obj.set_prototype(new_proto);
        Ok(Value::boolean(success))
    });

    // === Extensibility ===

    // Reflect.isExtensible(target)
    reflect_method!("isExtensible", |_, args, _ncx| {
        let target = args.first().ok_or("Reflect.isExtensible requires a target argument")?;

        // Check if target is a proxy
        if target.as_proxy().is_some() {
            return Err(VmError::interception(crate::error::InterceptionSignal::ReflectIsExtensibleProxy));
        }

        let obj = get_target_object(target)?;
        Ok(Value::boolean(obj.is_extensible()))
    });

    // Reflect.preventExtensions(target)
    reflect_method!("preventExtensions", |_, args, _ncx| {
        let target = args.first().ok_or("Reflect.preventExtensions requires a target argument")?;

        // Check if target is a proxy
        if target.as_proxy().is_some() {
            return Err(VmError::interception(crate::error::InterceptionSignal::ReflectPreventExtensionsProxy));
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

        // Check if target is a proxy
        if target.as_proxy().is_some() {
            return Err(VmError::interception(crate::error::InterceptionSignal::ReflectApplyProxy));
        }

        // Check if target is a function
        if !target.is_function() {
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

        // Call the function
        if let Some(native_fn) = target.as_native_function() {
            // Native function call
            native_fn(&this_arg, &args_array, ncx)
        } else if let Some(_closure) = target.as_function() {
            // Closure call - not fully supported yet in intrinsics
            // Would require VmContext to execute bytecode
            Err(VmError::type_error("Reflect.apply with closures not yet supported in intrinsics context"))
        } else {
            Err(VmError::type_error("Reflect.apply target is not callable"))
        }
    });

    // Reflect.construct(target, argumentsList, newTarget?)
    reflect_method!("construct", |_, args, ncx| {
        let target = args.first().ok_or("Reflect.construct requires a target argument")?;
        let args_list = args.get(1).ok_or("Reflect.construct requires an argumentsList argument")?;
        let _new_target = args.get(2); // Optional, for advanced use

        // Check if target is a proxy
        if target.as_proxy().is_some() {
            return Err(VmError::interception(crate::error::InterceptionSignal::ReflectConstructProxy));
        }

        // Check if target is a function
        if !target.is_function() {
            return Err(VmError::type_error("Reflect.construct target must be a constructor"));
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

        // Create new instance
        let new_obj = GcRef::new(JsObject::new(None, ncx.memory_manager().clone()));
        let this_val = Value::object(new_obj);

        // Call constructor
        if let Some(native_fn) = target.as_native_function() {
            // Call native constructor
            let result = native_fn(&this_val, &args_array, ncx)?;

            // If constructor returns an object, use it; otherwise use this_val
            if result.is_object() {
                Ok(result)
            } else {
                Ok(this_val)
            }
        } else if let Some(_closure) = target.as_function() {
            // Closure constructor - not fully supported yet
            Err(VmError::type_error("Reflect.construct with closures not yet supported in intrinsics context"))
        } else {
            Err(VmError::type_error("Reflect.construct target is not a constructor"))
        }
    });

    // ====================================================================
    // Install Reflect on global
    // ====================================================================
    global.set(PropertyKey::string("Reflect"), Value::object(reflect_obj));
}
