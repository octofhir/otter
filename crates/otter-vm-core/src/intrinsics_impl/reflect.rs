//! `Reflect` namespace object (ES2024 §28.1)
//!
//! The `Reflect` object provides methods for interceptable JavaScript operations.
//! It is not a constructor and has no `[[Call]]` or `[[Construct]]`.
//!
//! Spec: <https://tc39.es/ecma262/#sec-reflect-object>
//! MDN: <https://developer.mozilla.org/en-US/docs/Web/JavaScript/Reference/Global_Objects/Reflect>

use crate::builtin_builder::{IntrinsicContext, IntrinsicObject, NamespaceBuilder};
use crate::context::NativeContext;
use crate::error::VmError;
use crate::gc::GcRef;
use crate::object::{JsObject, PropertyAttributes, PropertyDescriptor, PropertyKey};
use crate::string::JsString;
use crate::value::Value;
use otter_macros::dive;

/// Helper to convert Value to PropertyKey
pub fn to_property_key(value: &Value) -> PropertyKey {
    if let Some(n) = value.as_number()
        && n.fract() == 0.0
        && n >= 0.0
        && n <= u32::MAX as f64
    {
        return PropertyKey::Index(n as u32);
    }
    if let Some(s) = value.as_string() {
        // Use PropertyKey::string() to canonicalize numeric strings like "0" → Index(0)
        return PropertyKey::string(s.as_str());
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
fn get_target_object(value: &Value) -> Result<GcRef<JsObject>, VmError> {
    value.as_object().ok_or_else(|| {
        VmError::type_error(format!(
            "Reflect method requires an object target (got {})",
            value.type_of()
        ))
    })
}

fn builtin_tag_for_value(value: &Value) -> Option<GcRef<JsString>> {
    let mut current = *value;
    if let Some(proxy) = current.as_proxy()
        && let Some(target) = proxy.target()
    {
        current = target;
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
    let realm_id = ncx.realm_id_for_function(new_target);
    if let Some(tag) = builtin_tag_for_value(target)
        && let Some(intrinsics) = ncx.ctx.realm_intrinsics(realm_id)
        && let Some(proto) = intrinsics.prototype_for_builtin_tag(tag.as_str())
    {
        return Some(proto);
    }

    // Intl constructors are not in Intrinsics yet; derive default prototype
    // from the newTarget realm's Intl namespace when available.
    let ctor_name = target
        .as_object()
        .and_then(|o| o.get(&PropertyKey::string("name")))
        .and_then(|v| v.as_string())
        .map(|s| s.as_str().to_string())?;
    let realm_global = ncx.ctx.realm_global(realm_id)?;
    realm_global
        .get(&PropertyKey::string("Intl"))
        .and_then(|v| v.as_object())
        .and_then(|intl| intl.get(&PropertyKey::string(&ctor_name)))
        .and_then(|v| v.as_object())
        .and_then(|ctor| ctor.get(&PropertyKey::string("prototype")))
        .and_then(|v| v.as_object())
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
    // Check OWN __non_constructor (not inherited), since abstract constructors
    // like %TypedArray% set this flag but their concrete subclasses should
    // still be constructable.
    if let Some(obj) = value.as_object()
        && let Some(desc) = obj.get_own_property_descriptor(&PropertyKey::string("__non_constructor"))
        && desc.value().and_then(|v| v.as_boolean()) == Some(true)
    {
        return false;
    }
    true
}

fn descriptor_to_value(desc: PropertyDescriptor, ncx: &NativeContext) -> Value {
    let obj_proto = ncx
        .global()
        .get(&crate::object::PropertyKey::string("Object"))
        .and_then(|v| v.as_object())
        .and_then(|o| o.get(&crate::object::PropertyKey::string("prototype")))
        .unwrap_or(Value::null());
    match desc {
        PropertyDescriptor::Data { value, attributes } => {
            let desc_obj = GcRef::new(JsObject::new(obj_proto));
            let _ = desc_obj.set("value".into(), value);
            let _ = desc_obj.set("writable".into(), Value::boolean(attributes.writable));
            let _ = desc_obj.set("enumerable".into(), Value::boolean(attributes.enumerable));
            let _ = desc_obj.set(
                "configurable".into(),
                Value::boolean(attributes.configurable),
            );
            Value::object(desc_obj)
        }
        PropertyDescriptor::Accessor {
            get,
            set,
            attributes,
        } => {
            let desc_obj = GcRef::new(JsObject::new(obj_proto));
            let _ = desc_obj.set("get".into(), get.unwrap_or(Value::undefined()));
            let _ = desc_obj.set("set".into(), set.unwrap_or(Value::undefined()));
            let _ = desc_obj.set("enumerable".into(), Value::boolean(attributes.enumerable));
            let _ = desc_obj.set(
                "configurable".into(),
                Value::boolean(attributes.configurable),
            );
            Value::object(desc_obj)
        }
        PropertyDescriptor::Deleted => Value::undefined(),
    }
}

/// CreateListFromArrayLike(obj) — ES2024 §7.3.18
/// Accepts any object with a numeric `length` property (not just arrays).
/// Throws TypeError for non-objects (null, undefined, primitives).
fn value_to_array_args(args_list: &Value) -> Result<Vec<Value>, VmError> {
    if args_list.is_null() || args_list.is_undefined() {
        return Err(VmError::type_error(
            "CreateListFromArrayLike called on null or undefined",
        ));
    }
    if let Some(arr_obj) = args_list.as_object() {
        let len = if arr_obj.is_array() {
            arr_obj.array_length()
        } else {
            arr_obj
                .get(&PropertyKey::string("length"))
                .and_then(|v| v.as_number())
                .map(|n| if n.is_nan() || n < 0.0 { 0 } else { n as usize })
                .unwrap_or(0)
        };
        let mut call_args = Vec::with_capacity(len.min(65536));
        for i in 0..len {
            let val = arr_obj
                .get(&PropertyKey::Index(i as u32))
                .unwrap_or(Value::undefined());
            call_args.push(val);
        }
        Ok(call_args)
    } else {
        Err(VmError::type_error(
            "CreateListFromArrayLike called on non-object",
        ))
    }
}

/// Spec: <https://tc39.es/ecma262/#sec-reflect.get>
#[dive(name = "get", length = 2)]
fn reflect_get(
    _this: &Value,
    args: &[Value],
    ncx: &mut NativeContext<'_>,
) -> Result<Value, VmError> {
    let target = args
        .first()
        .ok_or("Reflect.get requires a target argument")?;
    let property_key = args
        .get(1)
        .ok_or("Reflect.get requires a propertyKey argument")?;
    let receiver = args.get(2).cloned().unwrap_or(*target);

    if let Some(proxy) = target.as_proxy() {
        let key = to_property_key(property_key);
        return crate::proxy_operations::proxy_get(ncx, proxy, &key, *property_key, receiver);
    }

    let obj = get_target_object(target)?;
    let key = to_property_key(property_key);
    // Use get_value_full to invoke getters; pass receiver as `this` for accessor calls
    if let Some(desc) = obj.lookup_property_descriptor(&key) {
        match desc {
            PropertyDescriptor::Data { value, .. } => Ok(value),
            PropertyDescriptor::Accessor { get, .. } => {
                if let Some(getter) = get
                    && !getter.is_undefined()
                {
                    return ncx.call_function(&getter, receiver, &[]);
                }
                Ok(Value::undefined())
            }
            PropertyDescriptor::Deleted => Ok(Value::undefined()),
        }
    } else {
        Ok(Value::undefined())
    }
}

/// Spec: <https://tc39.es/ecma262/#sec-reflect.set>
#[dive(name = "set", length = 3)]
fn reflect_set(
    _this: &Value,
    args: &[Value],
    ncx: &mut NativeContext<'_>,
) -> Result<Value, VmError> {
    let target = args
        .first()
        .ok_or("Reflect.set requires a target argument")?;
    let property_key = args
        .get(1)
        .ok_or("Reflect.set requires a propertyKey argument")?;
    let value = args.get(2).cloned().unwrap_or(Value::undefined());
    let receiver = args.get(3).cloned().unwrap_or(*target);

    if let Some(proxy) = target.as_proxy() {
        let key = to_property_key(property_key);
        let success =
            crate::proxy_operations::proxy_set(ncx, proxy, &key, *property_key, value, receiver)?;
        return Ok(Value::boolean(success));
    }

    // TypedArray exotic [[Set]] — §10.4.5.5
    if let Some(ta) = target.as_typed_array() {
        let key = to_property_key(property_key);
        match crate::typed_array_ops::ta_set(&ta, &key, &value) {
            crate::typed_array_ops::TaSetResult::Written => return Ok(Value::boolean(true)),
            crate::typed_array_ops::TaSetResult::OutOfBounds => return Ok(Value::boolean(false)),
            crate::typed_array_ops::TaSetResult::Detached => return Ok(Value::boolean(false)),
            crate::typed_array_ops::TaSetResult::NotAnIndex => {
                // Fall through to ordinary set on ta.object
                let _ = ta.object.set(key, value);
                return Ok(Value::boolean(true));
            }
        }
    }

    let obj = get_target_object(target)?;
    let key = to_property_key(property_key);
    let _ = obj.set(key, value);
    Ok(Value::boolean(true))
}

/// Spec: <https://tc39.es/ecma262/#sec-reflect.has>
#[dive(name = "has", length = 2)]
fn reflect_has(
    _this: &Value,
    args: &[Value],
    ncx: &mut NativeContext<'_>,
) -> Result<Value, VmError> {
    let target = args
        .first()
        .ok_or("Reflect.has requires a target argument")?;
    let property_key = args
        .get(1)
        .ok_or("Reflect.has requires a propertyKey argument")?;

    if let Some(proxy) = target.as_proxy() {
        let key = to_property_key(property_key);
        let result = crate::proxy_operations::proxy_has(ncx, proxy, &key, *property_key)?;
        return Ok(Value::boolean(result));
    }

    // TypedArray exotic [[HasProperty]]
    if let Some(ta) = target.as_typed_array() {
        let key = to_property_key(property_key);
        match crate::typed_array_ops::ta_has(&ta, &key) {
            crate::typed_array_ops::TaHasResult::Present => return Ok(Value::boolean(true)),
            crate::typed_array_ops::TaHasResult::Absent => return Ok(Value::boolean(false)),
            crate::typed_array_ops::TaHasResult::NotAnIndex => {
                // Fall through to ordinary has on ta.object
                return Ok(Value::boolean(ta.object.has(&key)));
            }
        }
    }

    let obj = get_target_object(target)?;
    let key = to_property_key(property_key);
    Ok(Value::boolean(obj.has(&key)))
}

/// Spec: <https://tc39.es/ecma262/#sec-reflect.deleteproperty>
#[dive(name = "deleteProperty", length = 2)]
fn reflect_delete_property(
    _this: &Value,
    args: &[Value],
    ncx: &mut NativeContext<'_>,
) -> Result<Value, VmError> {
    let target = args
        .first()
        .ok_or("Reflect.deleteProperty requires a target argument")?;
    let property_key = args
        .get(1)
        .ok_or("Reflect.deleteProperty requires a propertyKey argument")?;

    if let Some(proxy) = target.as_proxy() {
        let key = to_property_key(property_key);
        let result =
            crate::proxy_operations::proxy_delete_property(ncx, proxy, &key, *property_key)?;
        return Ok(Value::boolean(result));
    }

    // TypedArray exotic [[Delete]] — §10.4.5.6
    if let Some(ta) = target.as_typed_array() {
        let key = to_property_key(property_key);
        if let Some(result) = crate::typed_array_ops::ta_delete(&ta, &key) {
            return Ok(Value::boolean(result));
        }
        // Not a numeric index — fall through to ordinary delete on ta.object
        let deleted = ta.object.delete(&key);
        return Ok(Value::boolean(deleted));
    }

    let obj = get_target_object(target)?;
    let key = to_property_key(property_key);
    let deleted = obj.delete(&key);
    Ok(Value::boolean(deleted))
}

/// Spec: <https://tc39.es/ecma262/#sec-reflect.ownkeys>
#[dive(name = "ownKeys", length = 1)]
fn reflect_own_keys(
    _this: &Value,
    args: &[Value],
    ncx: &mut NativeContext<'_>,
) -> Result<Value, VmError> {
    let target = args
        .first()
        .ok_or("Reflect.ownKeys requires a target argument")?;

    let keys = if let Some(proxy) = target.as_proxy() {
        crate::proxy_operations::proxy_own_keys(ncx, proxy)?
    } else if let Some(ta) = target.as_typed_array() {
        // TypedArray exotic [[OwnPropertyKeys]] — §10.4.5.7
        crate::typed_array_ops::ta_own_keys_full(&ta)
    } else {
        let obj = get_target_object(target)?;
        obj.own_keys()
    };

    let result = GcRef::new(JsObject::array(keys.len()));
    for (i, key) in keys.into_iter().enumerate() {
        let key_val = crate::proxy_operations::property_key_to_value_pub(&key);
        let _ = result.set(PropertyKey::Index(i as u32), key_val);
    }

    Ok(Value::array(result))
}

/// Spec: <https://tc39.es/ecma262/#sec-reflect.getownpropertydescriptor>
#[dive(name = "getOwnPropertyDescriptor", length = 2)]
fn reflect_get_own_property_descriptor(
    _this: &Value,
    args: &[Value],
    ncx: &mut NativeContext<'_>,
) -> Result<Value, VmError> {
    let target = args
        .first()
        .ok_or("Reflect.getOwnPropertyDescriptor requires a target argument")?;
    let property_key = args
        .get(1)
        .ok_or("Reflect.getOwnPropertyDescriptor requires a propertyKey argument")?;

    if let Some(proxy) = target.as_proxy() {
        let key = to_property_key(property_key);
        let result_desc = crate::proxy_operations::proxy_get_own_property_descriptor(
            ncx,
            proxy,
            &key,
            *property_key,
        )?;
        return Ok(result_desc
            .map(|desc| descriptor_to_value(desc, ncx))
            .unwrap_or(Value::undefined()));
    }

    // TypedArray exotic [[GetOwnProperty]] — §10.4.5.1
    if let Some(ta) = target.as_typed_array() {
        let key = to_property_key(property_key);
        // Check if it's a canonical numeric index first
        if let Some(ci) = crate::typed_array_ops::canonical_numeric_index(&key) {
            // It IS a canonical numeric index — TypedArray handles it exclusively.
            return match ci {
                crate::typed_array_ops::CanonicalIndex::Int(idx) => {
                    if !ta.is_detached() && idx < ta.length() {
                        if let Some(desc) =
                            crate::typed_array_ops::ta_get_own_property(&ta, &key)
                        {
                            return Ok(descriptor_to_value(desc, ncx));
                        }
                    }
                    // Out of bounds or detached → property absent
                    Ok(Value::undefined())
                }
                crate::typed_array_ops::CanonicalIndex::NonInt => Ok(Value::undefined()),
            };
        }
        // Not a numeric index — fall through to ta.object for named properties
        if let Some(prop_desc) = ta.object.get_own_property_descriptor(&key) {
            return Ok(descriptor_to_value(prop_desc, ncx));
        }
        return Ok(Value::undefined());
    }

    let obj = get_target_object(target)?;
    let key = to_property_key(property_key);

    if let Some(prop_desc) = obj.lookup_property_descriptor(&key) {
        Ok(descriptor_to_value(prop_desc, ncx))
    } else if let Some(value) = obj.get(&key) {
        let fallback_desc = PropertyDescriptor::data_with_attrs(
            value,
            PropertyAttributes {
                writable: true,
                enumerable: true,
                configurable: true,
            },
        );
        Ok(descriptor_to_value(fallback_desc, ncx))
    } else {
        Ok(Value::undefined())
    }
}

/// Spec: <https://tc39.es/ecma262/#sec-reflect.defineproperty>
#[dive(name = "defineProperty", length = 3)]
fn reflect_define_property(
    _this: &Value,
    args: &[Value],
    ncx: &mut NativeContext<'_>,
) -> Result<Value, VmError> {
    let target = args
        .first()
        .ok_or("Reflect.defineProperty requires a target argument")?;
    let property_key = args
        .get(1)
        .ok_or("Reflect.defineProperty requires a propertyKey argument")?;
    let attributes = args
        .get(2)
        .ok_or("Reflect.defineProperty requires an attributes argument")?;
    let Some(attr_obj) = attributes.as_object() else {
        return Err(VmError::type_error(
            "Reflect.defineProperty requires attributes to be an object",
        ));
    };
    let key = to_property_key(property_key);

    let desc = crate::object::to_property_descriptor(&attr_obj, ncx)
        .map_err(|e| VmError::type_error(&e))?;

    if let Some(proxy) = target.as_proxy() {
        let full_desc = if desc.is_accessor_descriptor() {
            let get_val = desc.get.unwrap_or(Value::undefined());
            let set_val = desc.set.unwrap_or(Value::undefined());
            PropertyDescriptor::Accessor {
                get: if get_val.is_undefined() {
                    None
                } else {
                    Some(get_val)
                },
                set: if set_val.is_undefined() {
                    None
                } else {
                    Some(set_val)
                },
                attributes: PropertyAttributes {
                    writable: false,
                    enumerable: desc.enumerable.unwrap_or(false),
                    configurable: desc.configurable.unwrap_or(false),
                },
            }
        } else {
            PropertyDescriptor::data_with_attrs(
                desc.value.unwrap_or(Value::undefined()),
                PropertyAttributes {
                    writable: desc.writable.unwrap_or(false),
                    enumerable: desc.enumerable.unwrap_or(false),
                    configurable: desc.configurable.unwrap_or(false),
                },
            )
        };
        let result = crate::proxy_operations::proxy_define_property(
            ncx,
            proxy,
            &key,
            *property_key,
            &full_desc,
        )?;
        return Ok(Value::boolean(result));
    }

    // TypedArray exotic [[DefineOwnProperty]] — §10.4.5.3
    if let Some(ta) = target.as_typed_array() {
        use crate::typed_array_ops::{CanonicalIndex, canonical_numeric_index};
        match canonical_numeric_index(&key) {
            Some(CanonicalIndex::Int(idx)) => {
                if desc.is_accessor_descriptor() {
                    return Ok(Value::boolean(false));
                }
                if desc.configurable == Some(false)
                    || desc.enumerable == Some(false)
                    || desc.writable == Some(false)
                {
                    return Ok(Value::boolean(false));
                }
                if ta.is_detached() || idx >= ta.length() {
                    return Ok(Value::boolean(false));
                }
                // §10.4.5.11 IntegerIndexedElementSet: call ToNumber/ToBigInt
                if let Some(val) = desc.value {
                    if ta.kind().is_bigint() {
                        let prim = if val.is_object() || val.as_object().is_some() {
                            ncx.to_primitive(&val, crate::interpreter::PreferredType::Number)?
                        } else {
                            val
                        };
                        let n = crate::intrinsics_impl::typed_array::to_bigint_i64(&prim)?;
                        if !ta.is_detached() {
                            ta.set_bigint(idx, n);
                        }
                    } else {
                        let n = ncx.to_number_value(&val)?;
                        if !ta.is_detached() {
                            ta.set(idx, n);
                        }
                    }
                }
                return Ok(Value::boolean(true));
            }
            Some(CanonicalIndex::NonInt) => return Ok(Value::boolean(false)),
            None => {
                let ok = ta.object.define_own_property(key, &desc);
                return Ok(Value::boolean(ok));
            }
        }
    }

    let obj = get_target_object(target)?;
    let ok = obj.define_own_property(key, &desc);
    Ok(Value::boolean(ok))
}

/// Spec: <https://tc39.es/ecma262/#sec-reflect.getprototypeof>
#[dive(name = "getPrototypeOf", length = 1)]
fn reflect_get_prototype_of(
    _this: &Value,
    args: &[Value],
    ncx: &mut NativeContext<'_>,
) -> Result<Value, VmError> {
    let target = args
        .first()
        .ok_or("Reflect.getPrototypeOf requires a target argument")?;

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
}

/// Spec: <https://tc39.es/ecma262/#sec-reflect.setprototypeof>
#[dive(name = "setPrototypeOf", length = 2)]
fn reflect_set_prototype_of(
    _this: &Value,
    args: &[Value],
    ncx: &mut NativeContext<'_>,
) -> Result<Value, VmError> {
    let target = args
        .first()
        .ok_or("Reflect.setPrototypeOf requires a target argument")?;
    let prototype = args
        .get(1)
        .ok_or("Reflect.setPrototypeOf requires a prototype argument")?;

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
}

/// Spec: <https://tc39.es/ecma262/#sec-reflect.isextensible>
#[dive(name = "isExtensible", length = 1)]
fn reflect_is_extensible(
    _this: &Value,
    args: &[Value],
    ncx: &mut NativeContext<'_>,
) -> Result<Value, VmError> {
    let target = args
        .first()
        .ok_or("Reflect.isExtensible requires a target argument")?;

    if let Some(proxy) = target.as_proxy() {
        let result = crate::proxy_operations::proxy_is_extensible(ncx, proxy)?;
        return Ok(Value::boolean(result));
    }

    let obj = get_target_object(target)?;
    Ok(Value::boolean(obj.is_extensible()))
}

/// Spec: <https://tc39.es/ecma262/#sec-reflect.preventextensions>
#[dive(name = "preventExtensions", length = 1)]
fn reflect_prevent_extensions(
    _this: &Value,
    args: &[Value],
    ncx: &mut NativeContext<'_>,
) -> Result<Value, VmError> {
    let target = args
        .first()
        .ok_or("Reflect.preventExtensions requires a target argument")?;

    if let Some(proxy) = target.as_proxy() {
        let result = crate::proxy_operations::proxy_prevent_extensions(ncx, proxy)?;
        return Ok(Value::boolean(result));
    }

    let obj = get_target_object(target)?;
    obj.prevent_extensions();
    Ok(Value::boolean(true))
}

/// Spec: <https://tc39.es/ecma262/#sec-reflect.apply>
#[dive(name = "apply", length = 3)]
fn reflect_apply(
    _this: &Value,
    args: &[Value],
    ncx: &mut NativeContext<'_>,
) -> Result<Value, VmError> {
    let target = args
        .first()
        .ok_or_else(|| VmError::type_error("Reflect.apply requires a target argument"))?;
    let this_arg = args.get(1).cloned().unwrap_or(Value::undefined());
    // argumentsList defaults to undefined if missing → CreateListFromArrayLike will throw TypeError
    let args_list = args.get(2).cloned().unwrap_or(Value::undefined());

    if let Some(proxy) = target.as_proxy() {
        let args_array = value_to_array_args(&args_list)?;
        let result = crate::proxy_operations::proxy_apply(ncx, proxy, this_arg, &args_array)?;
        return Ok(result);
    }

    if !target.is_callable() {
        return Err(VmError::type_error(
            "Reflect.apply target must be a function",
        ));
    }

    let args_array = value_to_array_args(&args_list)?;
    ncx.call_function(target, this_arg, &args_array)
}

/// Spec: <https://tc39.es/ecma262/#sec-reflect.construct>
#[dive(name = "construct", length = 2)]
fn reflect_construct(
    _this: &Value,
    args: &[Value],
    ncx: &mut NativeContext<'_>,
) -> Result<Value, VmError> {
    let target = args
        .first()
        .ok_or("Reflect.construct requires a target argument")?;
    let args_list = args
        .get(1)
        .ok_or("Reflect.construct requires an argumentsList argument")?;
    let new_target = args.get(2).cloned().unwrap_or(*target);

    if let Some(proxy) = target.as_proxy() {
        if !is_constructor_value(&new_target) {
            return Err(VmError::type_error(
                "Reflect.construct newTarget must be a constructor",
            ));
        }
        let args_array = value_to_array_args(args_list)?;
        let result = crate::proxy_operations::proxy_construct(ncx, proxy, &args_array, new_target)?;
        return Ok(result);
    }

    if !is_constructor_value(target) {
        return Err(VmError::type_error(
            "Reflect.construct target must be a constructor",
        ));
    }
    if !is_constructor_value(&new_target) {
        return Err(VmError::type_error(
            "Reflect.construct newTarget must be a constructor",
        ));
    }

    let args_array = value_to_array_args(args_list)?;

    // Per spec, native constructors (TypedArray etc.) must process arguments
    // BEFORE GetPrototypeFromConstructor(newTarget). So we set newTarget on
    // the NativeContext and let the constructor call get_prototype_from_new_target()
    // at the correct point. For closure constructors, we do eager prototype read
    // since the JS `new` machinery handles it.
    let is_native = target.as_native_function().is_some();

    if is_native {
        // Set newTarget on VmContext; call_native_fn_with_realm will propagate to NativeContext
        ncx.ctx.set_pending_new_target(new_target);
    }

    let this_val = if is_native {
        // Pass undefined — call_function_construct creates `this` from target's proto
        Value::undefined()
    } else {
        // Closure constructors: eager GetPrototypeFromConstructor
        let proto = if let Some(nt_obj) = new_target.as_object() {
            let proto_val =
                ncx.get_property(&nt_obj, &crate::object::PropertyKey::string("prototype"))?;
            if proto_val.is_object() {
                proto_val
            } else {
                let default_proto = default_proto_for_construct(ncx, target, &new_target);
                default_proto.map(Value::object).unwrap_or_else(Value::null)
            }
        } else {
            let default_proto = default_proto_for_construct(ncx, target, &new_target);
            default_proto.map(Value::object).unwrap_or_else(Value::null)
        };
        let new_obj = GcRef::new(JsObject::new(proto));
        Value::object(new_obj)
    };

    let result = ncx.call_function_construct(target, this_val, &args_array)?;
    if result.is_object() {
        Ok(result)
    } else {
        Ok(this_val)
    }
}

/// Create and install Reflect namespace on global object
/// `Reflect` namespace object.
///
/// Spec: <https://tc39.es/ecma262/#sec-reflect-object>
/// MDN: <https://developer.mozilla.org/en-US/docs/Web/JavaScript/Reference/Global_Objects/Reflect>
pub struct ReflectNamespace;

impl IntrinsicObject for ReflectNamespace {
    fn init(ctx: &IntrinsicContext) {
        let reflect_obj = ctx.alloc_object(ctx.obj_proto());

        NamespaceBuilder::new(ctx.mm(), ctx.fn_proto(), reflect_obj)
            .method_decl(reflect_apply_decl())
            .method_decl(reflect_construct_decl())
            .method_decl(reflect_define_property_decl())
            .method_decl(reflect_delete_property_decl())
            .method_decl(reflect_get_decl())
            .method_decl(reflect_get_own_property_descriptor_decl())
            .method_decl(reflect_get_prototype_of_decl())
            .method_decl(reflect_has_decl())
            .method_decl(reflect_is_extensible_decl())
            .method_decl(reflect_own_keys_decl())
            .method_decl(reflect_prevent_extensions_decl())
            .method_decl(reflect_set_decl())
            .method_decl(reflect_set_prototype_of_decl())
            .string_tag("Reflect")
            .install_on(&ctx.global(), "Reflect");
    }
}
