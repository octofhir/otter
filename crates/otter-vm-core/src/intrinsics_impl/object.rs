//! Object.prototype methods and Object static methods implementation
//!
//! All Object methods for ES2026 standard.

use crate::error::VmError;
use crate::gc::GcRef;
use crate::intrinsics::well_known;
use crate::object::{JsObject, PropertyAttributes, PropertyDescriptor, PropertyKey};
use crate::string::JsString;
use crate::value::Value;
use crate::context::NativeContext;
use crate::memory::MemoryManager;
use crate::value::Symbol;
use std::sync::Arc;

fn get_builtin_proto(global: &GcRef<JsObject>, name: &str) -> Option<GcRef<JsObject>> {
    global
        .get(&PropertyKey::string(name))
        .and_then(|v| v.as_object())
        .and_then(|ctor| ctor.get(&PropertyKey::string("prototype")))
        .and_then(|v| v.as_object())
}

fn to_object_for_builtin(ncx: &mut NativeContext<'_>, value: &Value) -> Result<GcRef<JsObject>, VmError> {
    if let Some(obj) = value.as_object() {
        return Ok(obj);
    }
    if value.is_null() || value.is_undefined() {
        return Err(VmError::type_error("Cannot convert undefined or null to object"));
    }

    let global = ncx.ctx.global();
    let mm = ncx.memory_manager().clone();
    let (proto_name, slot_key, slot_value) = if let Some(s) = value.as_string() {
        ("String", "__primitiveValue__", Value::string(s))
    } else if let Some(n) = value.as_number() {
        ("Number", "__value__", Value::number(n))
    } else if let Some(i) = value.as_int32() {
        ("Number", "__value__", Value::number(i as f64))
    } else if let Some(b) = value.as_boolean() {
        ("Boolean", "__value__", Value::boolean(b))
    } else if value.is_symbol() {
        ("Symbol", "__primitiveValue__", value.clone())
    } else if value.is_bigint() {
        ("BigInt", "__value__", value.clone())
    } else {
        ("Object", "__value__", value.clone())
    };

    let proto = get_builtin_proto(&global, proto_name).or_else(|| get_builtin_proto(&global, "Object"));
    let obj = GcRef::new(JsObject::new(
        proto.map(Value::object).unwrap_or_else(Value::null),
        mm,
    ));
    obj.set(PropertyKey::string(slot_key), slot_value);
    Ok(obj)
}

/// Initialize Object.prototype methods
pub fn init_object_prototype(
    object_proto: GcRef<JsObject>,
    fn_proto: GcRef<JsObject>,
    mm: &Arc<MemoryManager>,
) {
    // Object.prototype.toString
    object_proto.define_property(
        PropertyKey::string("toString"),
        PropertyDescriptor::builtin_method(Value::native_function_with_proto(
            |this_val, _args, ncx| {
                if this_val.is_undefined() {
                    return Ok(Value::string(JsString::intern("[object Undefined]")));
                }
                if this_val.is_null() {
                    return Ok(Value::string(JsString::intern("[object Null]")));
                }

                if this_val.is_boolean() {
                    return Ok(Value::string(JsString::intern("[object Boolean]")));
                }
                if this_val.is_number() {
                    return Ok(Value::string(JsString::intern("[object Number]")));
                }
                if this_val.is_string() {
                    return Ok(Value::string(JsString::intern("[object String]")));
                }
                if this_val.is_symbol() {
                    return Ok(Value::string(JsString::intern("[object Symbol]")));
                }
                if this_val.is_bigint() {
                    return Ok(Value::string(JsString::intern("[object BigInt]")));
                }

                fn is_array_value(value: &Value) -> Result<bool, VmError> {
                    if let Some(proxy) = value.as_proxy() {
                        let target = proxy
                            .target()
                            .ok_or_else(|| VmError::type_error("Cannot perform 'get' on a proxy that has been revoked"))?;
                        return is_array_value(&target);
                    }
                    if let Some(obj) = value.as_object() {
                        return Ok(obj.is_array());
                    }
                    Ok(false)
                }

                let is_array = is_array_value(this_val)?;
                let builtin_tag = if is_array {
                    "Array"
                } else if this_val.is_callable() {
                    "Function"
                } else if this_val.is_promise() {
                    "Promise"
                } else if this_val.is_generator() {
                    "Generator"
                } else if this_val.is_array_buffer() {
                    "ArrayBuffer"
                } else if this_val.is_shared_array_buffer() {
                    "SharedArrayBuffer"
                } else if this_val.is_typed_array() {
                    "TypedArray"
                } else if this_val.is_data_view() {
                    "DataView"
                } else if this_val.as_regex().is_some() {
                    "RegExp"
                } else {
                    "Object"
                };

                let tag_value = if let Some(proxy) = this_val.as_proxy() {
                    let key = PropertyKey::Symbol(well_known::to_string_tag_symbol());
                    let key_value = Value::symbol(GcRef::new(Symbol {
                        description: None,
                        id: well_known::TO_STRING_TAG,
                    }));
                    crate::proxy_operations::proxy_get(ncx, proxy, &key, key_value, this_val.clone())?
                } else if let Some(obj) = this_val.as_object() {
                    obj.get(&PropertyKey::Symbol(well_known::to_string_tag_symbol()))
                        .unwrap_or(Value::undefined())
                } else {
                    Value::undefined()
                };

                let tag = tag_value
                    .as_string()
                    .map(|s| s.as_str().to_string())
                    .unwrap_or_else(|| builtin_tag.to_string());

                Ok(Value::string(JsString::intern(&format!("[object {}]", tag))))
            },
            mm.clone(),
            fn_proto.clone(),
        )),
    );

    // Object.prototype.valueOf
    object_proto.define_property(
        PropertyKey::string("valueOf"),
        PropertyDescriptor::builtin_method(Value::native_function_with_proto(
            |this_val, _args, _ncx| Ok(this_val.clone()),
            mm.clone(),
            fn_proto.clone(),
        )),
    );

    // Object.prototype.hasOwnProperty
    object_proto.define_property(
        PropertyKey::string("hasOwnProperty"),
        PropertyDescriptor::builtin_method(Value::native_function_with_proto(
            |this_val, args, ncx| {
                let key_val = args.first().cloned().unwrap_or(Value::undefined());
                use crate::intrinsics_impl::reflect::to_property_key;

                // Handle proxy
                if let Some(proxy) = this_val.as_proxy() {
                    let key = to_property_key(&key_val);

                    // Use proxy_has which goes through the 'has' trap
                    let result = crate::proxy_operations::proxy_has(ncx, proxy, &key, key_val)?;
                    return Ok(Value::boolean(result));
                }

                let obj = to_object_for_builtin(ncx, this_val)?;
                let key = to_property_key(&key_val);
                Ok(Value::boolean(obj.has_own(&key)))
            },
            mm.clone(),
            fn_proto.clone(),
        )),
    );

    // Object.prototype.isPrototypeOf
    object_proto.define_property(
        PropertyKey::string("isPrototypeOf"),
        PropertyDescriptor::builtin_method(Value::native_function_with_proto(
            |this_val, args, _ncx| {
                if let Some(target) = args.first().and_then(|v| v.as_object()) {
                    if let Some(this_obj) = this_val.as_object() {
                        let mut current = target.prototype().as_object();
                        while let Some(proto) = current {
                            if std::ptr::eq(
                                proto.as_ptr() as *const _,
                                this_obj.as_ptr() as *const _,
                            ) {
                                return Ok(Value::boolean(true));
                            }
                            current = proto.prototype().as_object();
                        }
                    }
                }
                Ok(Value::boolean(false))
            },
            mm.clone(),
            fn_proto.clone(),
        )),
    );

    // Object.prototype.propertyIsEnumerable
    object_proto.define_property(
        PropertyKey::string("propertyIsEnumerable"),
        PropertyDescriptor::builtin_method(Value::native_function_with_proto(
            |this_val, args, ncx| {
                let key_val = args.first().cloned().unwrap_or(Value::undefined());
                use crate::intrinsics_impl::reflect::to_property_key;

                if let Some(proxy) = this_val.as_proxy() {
                    let key = to_property_key(&key_val);
                    let desc = crate::proxy_operations::proxy_get_own_property_descriptor(
                        ncx,
                        proxy,
                        &key,
                        key_val,
                    )?;
                    return Ok(Value::boolean(desc.map(|d| d.enumerable()).unwrap_or(false)));
                }

                let obj = to_object_for_builtin(ncx, this_val)?;
                let key = to_property_key(&key_val);
                Ok(Value::boolean(
                    obj.get_own_property_descriptor(&key)
                        .map(|d| d.enumerable())
                        .unwrap_or(false),
                ))
            },
            mm.clone(),
            fn_proto,
        )),
    );
}

/// Initialize Object constructor static methods
pub fn init_object_constructor(
    object_ctor: GcRef<JsObject>,
    fn_proto: GcRef<JsObject>,
    mm: &Arc<MemoryManager>,
) {
    // Object.getPrototypeOf
    object_ctor.define_property(
        PropertyKey::string("getPrototypeOf"),
        PropertyDescriptor::builtin_method(Value::native_function_with_proto(
            |_this, args, ncx| {
                let target = args.first().cloned().unwrap_or(Value::undefined());
                if let Some(proxy) = target.as_proxy() {
                    let proto =
                        crate::proxy_operations::proxy_get_prototype_of(ncx, proxy)?;
                    return Ok(proto.map(Value::object).unwrap_or_else(Value::null));
                }
                if let Some(obj) = target.as_object() {
                    return Ok(obj.prototype());
                }
                if target.is_null() || target.is_undefined() {
                    return Err(VmError::type_error(
                        "Object.getPrototypeOf requires an object",
                    ));
                }

                let global = ncx.ctx.global();
                let proto = if target.is_string() {
                    get_builtin_proto(&global, "String")
                } else if target.is_number() {
                    get_builtin_proto(&global, "Number")
                } else if target.is_boolean() {
                    get_builtin_proto(&global, "Boolean")
                } else if target.is_symbol() {
                    get_builtin_proto(&global, "Symbol")
                } else if target.is_bigint() {
                    get_builtin_proto(&global, "BigInt")
                } else {
                    get_builtin_proto(&global, "Object")
                };
                Ok(proto.map(Value::object).unwrap_or_else(Value::null))
            },
            mm.clone(),
            fn_proto.clone(),
        )),
    );

    // Object.setPrototypeOf
    object_ctor.define_property(
        PropertyKey::string("setPrototypeOf"),
        PropertyDescriptor::builtin_method(Value::native_function_with_proto(
            |_this, args, _ncx| {
                let target = args.first().cloned().unwrap_or(Value::undefined());
                if let Some(obj) = target.as_object() {
                    let proto_val = args.get(1).cloned().unwrap_or(Value::undefined());
                    let proto = if proto_val.is_null() {
                        None
                    } else {
                        proto_val.as_object()
                    };
                    let proto_value = proto.map(Value::object).unwrap_or_else(Value::null);
                    if !obj.set_prototype(proto_value) {
                        return Err(
                            VmError::type_error("Object.setPrototypeOf failed")
                        );
                    }
                }
                Ok(target)
            },
            mm.clone(),
            fn_proto.clone(),
        )),
    );

    // Object.getOwnPropertyDescriptor
    object_ctor.define_property(
        PropertyKey::string("getOwnPropertyDescriptor"),
        PropertyDescriptor::builtin_method(Value::native_function_with_proto(
            |_this, args, ncx_inner| {
                // Handle proxy case first
                if let Some(target_val) = args.first() {
                    if let Some(proxy) = target_val.as_proxy() {
                        // Get propertyKey (defaults to undefined if not provided)
                        let key_val = args.get(1).cloned().unwrap_or(Value::undefined());
                        use crate::intrinsics_impl::reflect::to_property_key;
                        let key = to_property_key(&key_val);
                        let result_desc = crate::proxy_operations::proxy_get_own_property_descriptor(
                            ncx_inner,
                            proxy,
                            &key,
                            key_val.clone(),
                        )?;

                        if let Some(desc) = result_desc {
                            let desc_obj = GcRef::new(JsObject::new(Value::null(), ncx_inner.memory_manager().clone()));
                            match &desc {
                                PropertyDescriptor::Data { value, attributes } => {
                                    desc_obj.set(PropertyKey::string("value"), value.clone());
                                    desc_obj.set(PropertyKey::string("writable"), Value::boolean(attributes.writable));
                                    desc_obj.set(PropertyKey::string("enumerable"), Value::boolean(attributes.enumerable));
                                    desc_obj.set(PropertyKey::string("configurable"), Value::boolean(attributes.configurable));
                                }
                                PropertyDescriptor::Accessor { get, set, attributes } => {
                                    desc_obj.set(PropertyKey::string("get"), get.clone().unwrap_or(Value::undefined()));
                                    desc_obj.set(PropertyKey::string("set"), set.clone().unwrap_or(Value::undefined()));
                                    desc_obj.set(PropertyKey::string("enumerable"), Value::boolean(attributes.enumerable));
                                    desc_obj.set(PropertyKey::string("configurable"), Value::boolean(attributes.configurable));
                                }
                                PropertyDescriptor::Deleted => {}
                            }
                            return Ok(Value::object(desc_obj));
                        }
                        return Ok(Value::undefined());
                    }
                }

                // Handle regular object case (including primitives via ToObject)
                let target = args.first().cloned().unwrap_or(Value::undefined());
                let obj = to_object_for_builtin(ncx_inner, &target)?;
                let key_val = args.get(1).cloned().unwrap_or(Value::undefined());
                use crate::intrinsics_impl::reflect::to_property_key;
                let pk = to_property_key(&key_val);

                if let Some(desc) = obj.get_own_property_descriptor(&pk) {
                    // Build descriptor object
                    let desc_obj =
                        GcRef::new(JsObject::new(Value::null(), ncx_inner.memory_manager().clone()));
                    match &desc {
                        PropertyDescriptor::Data { value, attributes } => {
                            desc_obj.set(PropertyKey::string("value"), value.clone());
                            desc_obj.set(
                                PropertyKey::string("writable"),
                                Value::boolean(attributes.writable),
                            );
                            desc_obj.set(
                                PropertyKey::string("enumerable"),
                                Value::boolean(attributes.enumerable),
                            );
                            desc_obj.set(
                                PropertyKey::string("configurable"),
                                Value::boolean(attributes.configurable),
                            );
                        }
                        PropertyDescriptor::Accessor { get, set, attributes } => {
                            desc_obj.set(
                                PropertyKey::string("get"),
                                get.clone().unwrap_or(Value::undefined()),
                            );
                            desc_obj.set(
                                PropertyKey::string("set"),
                                set.clone().unwrap_or(Value::undefined()),
                            );
                            desc_obj.set(
                                PropertyKey::string("enumerable"),
                                Value::boolean(attributes.enumerable),
                            );
                            desc_obj.set(
                                PropertyKey::string("configurable"),
                                Value::boolean(attributes.configurable),
                            );
                        }
                        PropertyDescriptor::Deleted => {}
                    }
                    return Ok(Value::object(desc_obj));
                }
                Ok(Value::undefined())
            },
            mm.clone(),
            fn_proto.clone(),
        )),
    );

    // Object.keys
    object_ctor.define_property(
        PropertyKey::string("keys"),
        PropertyDescriptor::builtin_method(Value::native_function_with_proto(
            |_this, args, ncx| {
                let arg = args.first().cloned().unwrap_or(Value::undefined());
                let proxy = arg.as_proxy();
                let obj = if proxy.is_none() {
                    Some(to_object_for_builtin(ncx, &arg)?)
                } else {
                    None
                };

                let keys = if let Some(p) = proxy {
                    crate::proxy_operations::proxy_own_keys(ncx, p)?
                } else {
                    obj.unwrap().own_keys()
                };

                let mut names = Vec::new();
                for key in keys {
                    match &key {
                        PropertyKey::String(s) => {
                            let desc = if let Some(p) = proxy {
                                let key_value = Value::string(*s);
                                crate::proxy_operations::proxy_get_own_property_descriptor(
                                    ncx, p, &key, key_value,
                                )?
                            } else {
                                obj.unwrap().get_own_property_descriptor(&key)
                            };
                            if let Some(desc) = desc {
                                if desc.enumerable() {
                                    names.push(Value::string(*s));
                                }
                            }
                        }
                        PropertyKey::Index(i) => {
                            let desc = if let Some(p) = proxy {
                                let key_value = Value::string(JsString::intern(&i.to_string()));
                                crate::proxy_operations::proxy_get_own_property_descriptor(
                                    ncx, p, &key, key_value,
                                )?
                            } else {
                                obj.unwrap().get_own_property_descriptor(&key)
                            };
                            if let Some(desc) = desc {
                                if desc.enumerable() {
                                    names.push(Value::string(JsString::intern(&i.to_string())));
                                }
                            }
                        }
                        _ => {}
                    }
                }
                let result = GcRef::new(JsObject::array(names.len(), ncx.memory_manager().clone()));
                if let Some(array_ctor) = ncx.global().get(&PropertyKey::string("Array")) {
                    if let Some(array_obj) = array_ctor.as_object() {
                        if let Some(proto_val) = array_obj.get(&PropertyKey::string("prototype")) {
                            if let Some(proto_obj) = proto_val.as_object() {
                                result.set_prototype(Value::object(proto_obj));
                            }
                        }
                    }
                }
                for (i, name) in names.into_iter().enumerate() {
                    result.set(PropertyKey::Index(i as u32), name);
                }
                Ok(Value::array(result))
            },
            mm.clone(),
            fn_proto.clone(),
        )),
    );

    // Object.values
    object_ctor.define_property(
        PropertyKey::string("values"),
        PropertyDescriptor::builtin_method(Value::native_function_with_proto(
            |_this, args, ncx| {
                let arg = args.first().cloned().unwrap_or(Value::undefined());
                let proxy = arg.as_proxy();
                let obj = if proxy.is_none() {
                    Some(to_object_for_builtin(ncx, &arg)?)
                } else {
                    None
                };

                let keys = if let Some(p) = proxy {
                    crate::proxy_operations::proxy_own_keys(ncx, p)?
                } else {
                    obj.unwrap().own_keys()
                };
                let mut values = Vec::new();
                let receiver = arg.clone();
                for key in keys {
                    let key_value = match &key {
                        PropertyKey::String(s) => Value::string(*s),
                        PropertyKey::Index(i) => Value::string(JsString::intern(&i.to_string())),
                        PropertyKey::Symbol(sym) => Value::symbol(*sym),
                    };
                    let desc = if let Some(p) = proxy {
                        crate::proxy_operations::proxy_get_own_property_descriptor(
                            ncx, p, &key, key_value.clone(),
                        )?
                    } else {
                        obj.unwrap().get_own_property_descriptor(&key)
                    };
                    if let Some(desc) = desc {
                        if !desc.enumerable() {
                            continue;
                        }
                        let value = if let Some(p) = proxy {
                            crate::proxy_operations::proxy_get(
                                ncx, p, &key, key_value, receiver.clone(),
                            )?
                        } else {
                            obj.unwrap().get(&key).unwrap_or(Value::undefined())
                        };
                        values.push(value);
                    }
                }
                let result = GcRef::new(JsObject::array(values.len(), ncx.memory_manager().clone()));
                if let Some(array_ctor) = ncx.global().get(&PropertyKey::string("Array")) {
                    if let Some(array_obj) = array_ctor.as_object() {
                        if let Some(proto_val) = array_obj.get(&PropertyKey::string("prototype")) {
                            if let Some(proto_obj) = proto_val.as_object() {
                                result.set_prototype(Value::object(proto_obj));
                            }
                        }
                    }
                }
                for (i, value) in values.into_iter().enumerate() {
                    result.set(PropertyKey::Index(i as u32), value);
                }
                Ok(Value::array(result))
            },
            mm.clone(),
            fn_proto.clone(),
        )),
    );

    // Object.entries
    object_ctor.define_property(
        PropertyKey::string("entries"),
        PropertyDescriptor::builtin_method(Value::native_function_with_proto(
            |_this, args, ncx_inner| {
                let arg = args.first().cloned().unwrap_or(Value::undefined());
                let proxy = arg.as_proxy();
                let obj = if proxy.is_none() {
                    Some(to_object_for_builtin(ncx_inner, &arg)?)
                } else {
                    None
                };

                let keys = if let Some(p) = proxy {
                    crate::proxy_operations::proxy_own_keys(ncx_inner, p)?
                } else {
                    obj.unwrap().own_keys()
                };
                let mut entries = Vec::new();
                let receiver = arg.clone();
                for key in keys {
                    let key_value = match &key {
                        PropertyKey::String(s) => Value::string(*s),
                        PropertyKey::Index(i) => Value::string(JsString::intern(&i.to_string())),
                        PropertyKey::Symbol(sym) => Value::symbol(*sym),
                    };
                    let desc = if let Some(p) = proxy {
                        crate::proxy_operations::proxy_get_own_property_descriptor(
                            ncx_inner, p, &key, key_value.clone(),
                        )?
                    } else {
                        obj.unwrap().get_own_property_descriptor(&key)
                    };
                    if let Some(desc) = desc {
                        if !desc.enumerable() {
                            continue;
                        }
                        let value = if let Some(p) = proxy {
                            crate::proxy_operations::proxy_get(
                                ncx_inner, p, &key, key_value, receiver.clone(),
                            )?
                        } else {
                            obj.unwrap().get(&key).unwrap_or(Value::undefined())
                        };
                        let key_str = match &key {
                            PropertyKey::String(s) => Value::string(*s),
                            PropertyKey::Index(i) => {
                                Value::string(JsString::intern(&i.to_string()))
                            }
                            _ => continue,
                        };
                        let entry = GcRef::new(JsObject::array(
                            2,
                            ncx_inner.memory_manager().clone(),
                        ));
                        if let Some(array_ctor) =
                            ncx_inner.global().get(&PropertyKey::string("Array"))
                        {
                            if let Some(array_obj) = array_ctor.as_object() {
                                if let Some(proto_val) =
                                    array_obj.get(&PropertyKey::string("prototype"))
                                {
                                    if let Some(proto_obj) = proto_val.as_object() {
                                        entry.set_prototype(Value::object(proto_obj));
                                    }
                                }
                            }
                        }
                        entry.set(PropertyKey::Index(0), key_str);
                        entry.set(PropertyKey::Index(1), value);
                        entries.push(Value::array(entry));
                    }
                }
                let result = GcRef::new(JsObject::array(
                    entries.len(),
                    ncx_inner.memory_manager().clone(),
                ));
                if let Some(array_ctor) = ncx_inner.global().get(&PropertyKey::string("Array")) {
                    if let Some(array_obj) = array_ctor.as_object() {
                        if let Some(proto_val) = array_obj.get(&PropertyKey::string("prototype")) {
                            if let Some(proto_obj) = proto_val.as_object() {
                                result.set_prototype(Value::object(proto_obj));
                            }
                        }
                    }
                }
                for (i, entry) in entries.into_iter().enumerate() {
                    result.set(PropertyKey::Index(i as u32), entry);
                }
                Ok(Value::array(result))
            },
            mm.clone(),
            fn_proto.clone(),
        )),
    );

    // Object.assign
    object_ctor.define_property(
        PropertyKey::string("assign"),
        PropertyDescriptor::builtin_method(Value::native_function_with_proto(
            |_this, args, _ncx| {
                let target_val = args
                    .first()
                    .ok_or_else(|| {
                        "Object.assign requires at least one argument".to_string()
                    })?;
                let target = target_val
                    .as_object()
                    .ok_or_else(|| "Object.assign target must be an object".to_string())?;
                for source_val in &args[1..] {
                    if source_val.is_null() || source_val.is_undefined() {
                        continue;
                    }
                    if let Some(source) = source_val.as_object() {
                        for key in source.own_keys() {
                            if let Some(desc) = source.get_own_property_descriptor(&key) {
                                if desc.enumerable() {
                                    if let Some(value) = source.get(&key) {
                                        target.set(key, value);
                                    }
                                }
                            }
                        }
                    }
                }
                Ok(target_val.clone())
            },
            mm.clone(),
            fn_proto.clone(),
        )),
    );

    // Object.hasOwn
    object_ctor.define_property(
        PropertyKey::string("hasOwn"),
        PropertyDescriptor::builtin_method(Value::native_function_with_proto(
            |_this, args, _ncx| {
                let obj = args
                    .first()
                    .and_then(|v| v.as_object())
                    .ok_or_else(|| "Object.hasOwn requires an object".to_string())?;
                let prop = args.get(1).ok_or_else(|| {
                    "Object.hasOwn requires a property key".to_string()
                })?;
                let key = if let Some(s) = prop.as_string() {
                    PropertyKey::String(s)
                } else if let Some(sym) = prop.as_symbol() {
                    PropertyKey::Symbol(sym)
                } else if let Some(n) = prop.as_number() {
                    if n.fract() == 0.0 && n >= 0.0 && n <= u32::MAX as f64 {
                        PropertyKey::Index(n as u32)
                    } else {
                        PropertyKey::String(JsString::intern(&n.to_string()))
                    }
                } else {
                    PropertyKey::String(JsString::intern("undefined"))
                };
                Ok(Value::boolean(obj.has_own(&key)))
            },
            mm.clone(),
            fn_proto.clone(),
        )),
    );

    // Object.freeze
    object_ctor.define_property(
        PropertyKey::string("freeze"),
        PropertyDescriptor::builtin_method(Value::native_function_with_proto(
            |_this, args, _ncx| {
                let obj_val = args.first().cloned().unwrap_or(Value::undefined());
                if let Some(obj) = obj_val.as_object() {
                    obj.freeze();
                }
                Ok(obj_val)
            },
            mm.clone(),
            fn_proto.clone(),
        )),
    );

    // Object.isFrozen
    object_ctor.define_property(
        PropertyKey::string("isFrozen"),
        PropertyDescriptor::builtin_method(Value::native_function_with_proto(
            |_this, args, _ncx| {
                let is_frozen = args
                    .first()
                    .and_then(|v| v.as_object())
                    .map(|o| o.is_frozen())
                    .unwrap_or(true);
                Ok(Value::boolean(is_frozen))
            },
            mm.clone(),
            fn_proto.clone(),
        )),
    );

    // Object.seal
    object_ctor.define_property(
        PropertyKey::string("seal"),
        PropertyDescriptor::builtin_method(Value::native_function_with_proto(
            |_this, args, _ncx| {
                let obj_val = args.first().cloned().unwrap_or(Value::undefined());
                if let Some(obj) = obj_val.as_object() {
                    obj.seal();
                }
                Ok(obj_val)
            },
            mm.clone(),
            fn_proto.clone(),
        )),
    );

    // Object.isSealed
    object_ctor.define_property(
        PropertyKey::string("isSealed"),
        PropertyDescriptor::builtin_method(Value::native_function_with_proto(
            |_this, args, _ncx| {
                let is_sealed = args
                    .first()
                    .and_then(|v| v.as_object())
                    .map(|o| o.is_sealed())
                    .unwrap_or(true);
                Ok(Value::boolean(is_sealed))
            },
            mm.clone(),
            fn_proto.clone(),
        )),
    );

    // Object.preventExtensions
    object_ctor.define_property(
        PropertyKey::string("preventExtensions"),
        PropertyDescriptor::builtin_method(Value::native_function_with_proto(
            |_this, args, _ncx| {
                let obj_val = args.first().cloned().unwrap_or(Value::undefined());
                if let Some(obj) = obj_val.as_object() {
                    obj.prevent_extensions();
                }
                Ok(obj_val)
            },
            mm.clone(),
            fn_proto.clone(),
        )),
    );

    // Object.isExtensible
    object_ctor.define_property(
        PropertyKey::string("isExtensible"),
        PropertyDescriptor::builtin_method(Value::native_function_with_proto(
            |_this, args, _ncx| {
                let is_extensible = args
                    .first()
                    .and_then(|v| v.as_object())
                    .map(|o| o.is_extensible())
                    .unwrap_or(false);
                Ok(Value::boolean(is_extensible))
            },
            mm.clone(),
            fn_proto.clone(),
        )),
    );

    // Object.defineProperty
    object_ctor.define_property(
        PropertyKey::string("defineProperty"),
        PropertyDescriptor::builtin_method(Value::native_function_with_proto(
            |_this, args, _ncx| {
                let obj_val = args
                    .first()
                    .ok_or_else(|| "Object.defineProperty requires an object".to_string())?;
                let obj = obj_val.as_object().ok_or_else(|| {
                    "Object.defineProperty first argument must be an object".to_string()
                })?;
                let key_val = args
                    .get(1)
                    .ok_or_else(|| {
                        "Object.defineProperty requires a property key".to_string()
                    })?;
                let descriptor = args
                    .get(2)
                    .ok_or_else(|| {
                        "Object.defineProperty requires a descriptor".to_string()
                    })?;

                // Convert key
                let key = if let Some(s) = key_val.as_string() {
                    PropertyKey::String(s)
                } else if let Some(sym) = key_val.as_symbol() {
                    PropertyKey::Symbol(sym)
                } else if let Some(n) = key_val.as_number() {
                    if n.fract() == 0.0 && n >= 0.0 && n <= u32::MAX as f64 {
                        PropertyKey::Index(n as u32)
                    } else {
                        PropertyKey::String(JsString::intern(&n.to_string()))
                    }
                } else {
                    PropertyKey::String(JsString::intern("undefined"))
                };

                let attr_obj = descriptor.as_object().ok_or_else(|| {
                    "Property descriptor must be an object".to_string()
                })?;

                let read_bool = |name: &str, default: bool| -> bool {
                    attr_obj
                        .get(&PropertyKey::from(name))
                        .and_then(|v| v.as_boolean())
                        .unwrap_or(default)
                };

                let get = attr_obj.get(&PropertyKey::from("get"));
                let set = attr_obj.get(&PropertyKey::from("set"));

                let success = if get.is_some() || set.is_some() {
                    let enumerable = read_bool("enumerable", false);
                    let configurable = read_bool("configurable", false);

                    let existing = obj.get_own_property_descriptor(&key);
                    let (mut existing_get, mut existing_set) = match existing {
                        Some(PropertyDescriptor::Accessor { get, set, .. }) => {
                            (get, set)
                        }
                        _ => (None, None),
                    };
                    let get = get
                        .filter(|v| !v.is_undefined())
                        .or_else(|| existing_get.take());
                    let set = set
                        .filter(|v| !v.is_undefined())
                        .or_else(|| existing_set.take());

                    obj.define_property(
                        key,
                        PropertyDescriptor::Accessor {
                            get,
                            set,
                            attributes: PropertyAttributes {
                                writable: false,
                                enumerable,
                                configurable,
                            },
                        },
                    )
                } else {
                    let value = attr_obj
                        .get(&PropertyKey::from("value"))
                        .unwrap_or(Value::undefined());
                    let writable = read_bool("writable", false);
                    let enumerable = read_bool("enumerable", false);
                    let configurable = read_bool("configurable", false);

                    obj.define_property(
                        key,
                        PropertyDescriptor::data_with_attrs(
                            value,
                            PropertyAttributes {
                                writable,
                                enumerable,
                                configurable,
                            },
                        ),
                    )
                };

                if !success {
                    return Err(VmError::type_error(
                        "Cannot define property: object is not extensible or property is non-configurable",
                    ));
                }

                Ok(obj_val.clone())
            },
            mm.clone(),
            fn_proto.clone(),
        )),
    );

    // Object.create
    object_ctor.define_property(
        PropertyKey::string("create"),
        PropertyDescriptor::builtin_method(Value::native_function_with_proto(
            |_this, args, ncx_inner| {
                let proto_val = args.first().ok_or_else(|| {
                    "Object.create requires a prototype argument".to_string()
                })?;

                // Prototype can be null, object, or proxy
                let prototype = if proto_val.is_null() {
                    Value::null()
                } else if proto_val.as_object().is_some() || proto_val.as_proxy().is_some() {
                    proto_val.clone()
                } else {
                    return Err(
                        VmError::type_error("Object prototype may only be an Object or null")
                    );
                };

                let new_obj = GcRef::new(JsObject::new(prototype, ncx_inner.memory_manager().clone()));

                // Handle optional properties object (second argument)
                if let Some(props_val) = args.get(1) {
                    if !props_val.is_undefined() {
                        let props = props_val.as_object().ok_or_else(|| {
                            "Properties argument must be an object".to_string()
                        })?;
                        for key in props.own_keys() {
                            if let Some(descriptor) = props.get(&key) {
                                if let Some(attr_obj) = descriptor.as_object() {
                                    let read_bool =
                                        |name: &str, default: bool| -> bool {
                                            attr_obj
                                                .get(&PropertyKey::from(name))
                                                .and_then(|v| v.as_boolean())
                                                .unwrap_or(default)
                                        };
                                    let get =
                                        attr_obj.get(&PropertyKey::from("get"));
                                    let set =
                                        attr_obj.get(&PropertyKey::from("set"));

                                    if get.is_some() || set.is_some() {
                                        let enumerable =
                                            read_bool("enumerable", false);
                                        let configurable =
                                            read_bool("configurable", false);
                                        new_obj.define_property(
                                            key,
                                            PropertyDescriptor::Accessor {
                                                get: get
                                                    .filter(|v| !v.is_undefined()),
                                                set: set
                                                    .filter(|v| !v.is_undefined()),
                                                attributes: PropertyAttributes {
                                                    writable: false,
                                                    enumerable,
                                                    configurable,
                                                },
                                            },
                                        );
                                    } else {
                                        let value = attr_obj
                                            .get(&PropertyKey::from("value"))
                                            .unwrap_or(Value::undefined());
                                        let writable =
                                            read_bool("writable", false);
                                        let enumerable =
                                            read_bool("enumerable", false);
                                        let configurable =
                                            read_bool("configurable", false);
                                        new_obj.define_property(
                                            key,
                                            PropertyDescriptor::data_with_attrs(
                                                value,
                                                PropertyAttributes {
                                                    writable,
                                                    enumerable,
                                                    configurable,
                                                },
                                            ),
                                        );
                                    }
                                }
                            }
                        }
                    }
                }

                Ok(Value::object(new_obj))
            },
            mm.clone(),
            fn_proto.clone(),
        )),
    );

    // Object.is (SameValue algorithm)
    object_ctor.define_property(
        PropertyKey::string("is"),
        PropertyDescriptor::builtin_method(Value::native_function_with_proto(
            |_this, args, _ncx| {
                let v1 = args.first().cloned().unwrap_or(Value::undefined());
                let v2 = args.get(1).cloned().unwrap_or(Value::undefined());
                let result =
                    if let (Some(n1), Some(n2)) = (v1.as_number(), v2.as_number()) {
                        if n1.is_nan() && n2.is_nan() {
                            true
                        } else if n1 == 0.0 && n2 == 0.0 {
                            (1.0_f64 / n1).is_sign_positive()
                                == (1.0_f64 / n2).is_sign_positive()
                        } else {
                            n1 == n2
                        }
                    } else if v1.is_undefined() && v2.is_undefined() {
                        true
                    } else if v1.is_null() && v2.is_null() {
                        true
                    } else if let (Some(b1), Some(b2)) =
                        (v1.as_boolean(), v2.as_boolean())
                    {
                        b1 == b2
                    } else if let (Some(s1), Some(s2)) =
                        (v1.as_string(), v2.as_string())
                    {
                        s1.as_str() == s2.as_str()
                    } else if let (Some(sym1), Some(sym2)) =
                        (v1.as_symbol(), v2.as_symbol())
                    {
                        sym1.id == sym2.id
                    } else if let (Some(o1), Some(o2)) =
                        (v1.as_object(), v2.as_object())
                    {
                        o1.as_ptr() == o2.as_ptr()
                    } else {
                        false
                    };
                Ok(Value::boolean(result))
            },
            mm.clone(),
            fn_proto.clone(),
        )),
    );

    // Object.getOwnPropertyNames
    object_ctor.define_property(
        PropertyKey::string("getOwnPropertyNames"),
        PropertyDescriptor::builtin_method(Value::native_function_with_proto(
            |_this, args, ncx_inner| {
                let obj = match args.first().and_then(|v| v.as_object()) {
                    Some(o) => o,
                    None => {
                        let arr =
                            GcRef::new(JsObject::array(0, ncx_inner.memory_manager().clone()));
                        if let Some(array_ctor) = ncx_inner.global().get(&PropertyKey::string("Array")) {
                            if let Some(array_obj) = array_ctor.as_object() {
                                if let Some(proto_val) = array_obj.get(&PropertyKey::string("prototype")) {
                                    if let Some(proto_obj) = proto_val.as_object() {
                                        arr.set_prototype(Value::object(proto_obj));
                                    }
                                }
                            }
                        }
                        return Ok(Value::array(arr));
                    }
                };
                let keys = obj.own_keys();
                let mut names = Vec::new();
                for key in keys {
                    match key {
                        PropertyKey::String(s) => names.push(Value::string(s)),
                        PropertyKey::Index(i) => {
                            names.push(Value::string(JsString::intern(
                                &i.to_string(),
                            )));
                        }
                        _ => {} // skip symbols
                    }
                }
                let result =
                    GcRef::new(JsObject::array(names.len(), ncx_inner.memory_manager().clone()));
                if let Some(array_ctor) = ncx_inner.global().get(&PropertyKey::string("Array")) {
                    if let Some(array_obj) = array_ctor.as_object() {
                        if let Some(proto_val) = array_obj.get(&PropertyKey::string("prototype")) {
                            if let Some(proto_obj) = proto_val.as_object() {
                                result.set_prototype(Value::object(proto_obj));
                            }
                        }
                    }
                }
                for (i, name) in names.into_iter().enumerate() {
                    result.set(PropertyKey::Index(i as u32), name);
                }
                Ok(Value::array(result))
            },
            mm.clone(),
            fn_proto.clone(),
        )),
    );

    // Object.getOwnPropertySymbols
    object_ctor.define_property(
        PropertyKey::string("getOwnPropertySymbols"),
        PropertyDescriptor::builtin_method(Value::native_function_with_proto(
            |_this, args, ncx_inner| {
                let arg = args.first().cloned().unwrap_or(Value::undefined());

                // ToObject - throws TypeError for undefined/null
                if arg.is_undefined() || arg.is_null() {
                    return Err(VmError::type_error(
                        "Cannot convert undefined or null to object",
                    ));
                }

                let obj = match arg.as_object() {
                    Some(o) => o,
                    None => {
                        // Primitives (string, number, boolean, symbol) have no own symbol properties
                        let arr =
                            GcRef::new(JsObject::array(0, ncx_inner.memory_manager().clone()));
                        if let Some(array_ctor) = ncx_inner.global().get(&PropertyKey::string("Array")) {
                            if let Some(array_obj) = array_ctor.as_object() {
                                if let Some(proto_val) = array_obj.get(&PropertyKey::string("prototype")) {
                                    if let Some(proto_obj) = proto_val.as_object() {
                                        arr.set_prototype(Value::object(proto_obj));
                                    }
                                }
                            }
                        }
                        return Ok(Value::array(arr));
                    }
                };
                let keys = obj.own_keys();
                let mut symbols = Vec::new();
                for key in keys {
                    if let PropertyKey::Symbol(sym) = key {
                        symbols.push(Value::symbol(sym));
                    }
                }
                let result =
                    GcRef::new(JsObject::array(symbols.len(), ncx_inner.memory_manager().clone()));
                if let Some(array_ctor) = ncx_inner.global().get(&PropertyKey::string("Array")) {
                    if let Some(array_obj) = array_ctor.as_object() {
                        if let Some(proto_val) = array_obj.get(&PropertyKey::string("prototype")) {
                            if let Some(proto_obj) = proto_val.as_object() {
                                result.set_prototype(Value::object(proto_obj));
                            }
                        }
                    }
                }
                for (i, sym) in symbols.into_iter().enumerate() {
                    result.set(PropertyKey::Index(i as u32), sym);
                }
                Ok(Value::array(result))
            },
            mm.clone(),
            fn_proto.clone(),
        )),
    );

    // Object.getOwnPropertyDescriptors
    object_ctor.define_property(
        PropertyKey::string("getOwnPropertyDescriptors"),
        PropertyDescriptor::builtin_method(Value::native_function_with_proto(
            |_this, args, ncx_inner| {
                let obj = match args.first().and_then(|v| v.as_object()) {
                    Some(o) => o,
                    None => {
                        return Ok(Value::object(GcRef::new(JsObject::new(
                            Value::null(), ncx_inner.memory_manager().clone(),
                        ))));
                    }
                };
                let result = GcRef::new(JsObject::new(Value::null(), ncx_inner.memory_manager().clone()));
                for key in obj.own_keys() {
                    if let Some(desc) = obj.get_own_property_descriptor(&key) {
                        let desc_obj =
                            GcRef::new(JsObject::new(Value::null(), ncx_inner.memory_manager().clone()));
                        match &desc {
                            PropertyDescriptor::Data { value, attributes } => {
                                desc_obj.set(
                                    PropertyKey::string("value"),
                                    value.clone(),
                                );
                                desc_obj.set(
                                    PropertyKey::string("writable"),
                                    Value::boolean(attributes.writable),
                                );
                                desc_obj.set(
                                    PropertyKey::string("enumerable"),
                                    Value::boolean(attributes.enumerable),
                                );
                                desc_obj.set(
                                    PropertyKey::string("configurable"),
                                    Value::boolean(attributes.configurable),
                                );
                            }
                            PropertyDescriptor::Accessor {
                                get,
                                set,
                                attributes,
                            } => {
                                desc_obj.set(
                                    PropertyKey::string("get"),
                                    get.clone()
                                        .unwrap_or(Value::undefined()),
                                );
                                desc_obj.set(
                                    PropertyKey::string("set"),
                                    set.clone()
                                        .unwrap_or(Value::undefined()),
                                );
                                desc_obj.set(
                                    PropertyKey::string("enumerable"),
                                    Value::boolean(attributes.enumerable),
                                );
                                desc_obj.set(
                                    PropertyKey::string("configurable"),
                                    Value::boolean(attributes.configurable),
                                );
                            }
                            PropertyDescriptor::Deleted => {}
                        }
                        result.set(key, Value::object(desc_obj));
                    }
                }
                Ok(Value::object(result))
            },
            mm.clone(),
            fn_proto.clone(),
        )),
    );

    // Object.defineProperties
    object_ctor.define_property(
        PropertyKey::string("defineProperties"),
        PropertyDescriptor::builtin_method(Value::native_function_with_proto(
            |_this, args, _ncx| {
                let obj_val = args
                    .first()
                    .ok_or_else(|| {
                        "Object.defineProperties requires an object".to_string()
                    })?;
                let obj = obj_val.as_object().ok_or_else(|| {
                    "Object.defineProperties first argument must be an object"
                        .to_string()
                })?;
                let props_val = args.get(1).ok_or_else(|| {
                    "Object.defineProperties requires properties".to_string()
                })?;
                let props = props_val.as_object().ok_or_else(|| {
                    "Object.defineProperties second argument must be an object"
                        .to_string()
                })?;

                for key in props.own_keys() {
                    if let Some(descriptor) = props.get(&key) {
                        if let Some(attr_obj) = descriptor.as_object() {
                            let read_bool =
                                |name: &str, default: bool| -> bool {
                                    attr_obj
                                        .get(&PropertyKey::from(name))
                                        .and_then(|v| v.as_boolean())
                                        .unwrap_or(default)
                                };
                            let get = attr_obj.get(&PropertyKey::from("get"));
                            let set = attr_obj.get(&PropertyKey::from("set"));
                            if get.is_some() || set.is_some() {
                                let enumerable =
                                    read_bool("enumerable", false);
                                let configurable =
                                    read_bool("configurable", false);
                                obj.define_property(
                                    key,
                                    PropertyDescriptor::Accessor {
                                        get: get
                                            .filter(|v| !v.is_undefined()),
                                        set: set
                                            .filter(|v| !v.is_undefined()),
                                        attributes: PropertyAttributes {
                                            writable: false,
                                            enumerable,
                                            configurable,
                                        },
                                    },
                                );
                            } else {
                                let value = attr_obj
                                    .get(&PropertyKey::from("value"))
                                    .unwrap_or(Value::undefined());
                                let writable = read_bool("writable", false);
                                let enumerable =
                                    read_bool("enumerable", false);
                                let configurable =
                                    read_bool("configurable", false);
                                obj.define_property(
                                    key,
                                    PropertyDescriptor::data_with_attrs(
                                        value,
                                        PropertyAttributes {
                                            writable,
                                            enumerable,
                                            configurable,
                                        },
                                    ),
                                );
                            }
                        }
                    }
                }
                Ok(obj_val.clone())
            },
            mm.clone(),
            fn_proto.clone(),
        )),
    );

    // Object.fromEntries
    object_ctor.define_property(
        PropertyKey::string("fromEntries"),
        PropertyDescriptor::builtin_method(Value::native_function_with_proto(
            |_this, args, ncx_inner| {
                let iterable = args.first().ok_or_else(|| {
                    "Object.fromEntries requires an iterable".to_string()
                })?;
                let iter_obj = iterable.as_object().ok_or_else(|| {
                    "Object.fromEntries argument must be iterable".to_string()
                })?;
                let result = GcRef::new(JsObject::new(Value::null(), ncx_inner.memory_manager().clone()));

                // Support array-like iterables (check length property)
                if let Some(len_val) =
                    iter_obj.get(&PropertyKey::String(JsString::intern("length")))
                {
                    if let Some(len) = len_val.as_number() {
                        for i in 0..(len as u32) {
                            if let Some(entry) =
                                iter_obj.get(&PropertyKey::Index(i))
                            {
                                if let Some(entry_obj) = entry.as_object() {
                                    let key = entry_obj
                                        .get(&PropertyKey::Index(0))
                                        .unwrap_or(Value::undefined());
                                    let value = entry_obj
                                        .get(&PropertyKey::Index(1))
                                        .unwrap_or(Value::undefined());
                                    let pk = if let Some(s) = key.as_string() {
                                        PropertyKey::String(s)
                                    } else if let Some(n) = key.as_number() {
                                        PropertyKey::String(JsString::intern(
                                            &n.to_string(),
                                        ))
                                    } else {
                                        PropertyKey::String(JsString::intern(
                                            "undefined",
                                        ))
                                    };
                                    result.set(pk, value);
                                }
                            }
                        }
                    }
                }
                Ok(Value::object(result))
            },
            mm.clone(),
            fn_proto,
        )),
    );
}

/// Create Object constructor function
pub fn create_object_constructor() -> Box<
    dyn Fn(&Value, &[Value], &mut crate::context::NativeContext<'_>) -> Result<Value, VmError> + Send + Sync
> {
    Box::new(|_this, args, ncx| {
        let global = ncx.ctx.global();
        let mm = ncx.ctx.memory_manager().clone();
        let get_proto = |name: &str| -> Option<GcRef<JsObject>> {
            global
                .get(&PropertyKey::string(name))
                .and_then(|v| v.as_object())
                .and_then(|ctor| ctor.get(&PropertyKey::string("prototype")))
                .and_then(|v| v.as_object())
        };
        // When called with an object argument, return it directly
        if let Some(arg) = args.first() {
            if arg.is_object() {
                return Ok(arg.clone());
            }
            if arg.is_null() || arg.is_undefined() {
                let obj_proto = get_proto("Object");
                let obj = GcRef::new(JsObject::new(
                    obj_proto
                        .map(Value::object)
                        .unwrap_or_else(Value::null),
                    mm,
                ));
                return Ok(Value::object(obj));
            }

            let (proto_name, slot_key, slot_value) = if let Some(s) = arg.as_string() {
                ("String", "__primitiveValue__", Value::string(s))
            } else if let Some(n) = arg.as_number() {
                ("Number", "__value__", Value::number(n))
            } else if let Some(i) = arg.as_int32() {
                ("Number", "__value__", Value::number(i as f64))
            } else if let Some(b) = arg.as_boolean() {
                ("Boolean", "__value__", Value::boolean(b))
            } else if arg.is_symbol() {
                ("Symbol", "__primitiveValue__", arg.clone())
            } else if arg.is_bigint() {
                ("BigInt", "__value__", arg.clone())
            } else {
                ("Object", "__value__", arg.clone())
            };

            let proto = get_proto(proto_name).or_else(|| get_proto("Object"));
            let obj = GcRef::new(JsObject::new(
                proto.map(Value::object).unwrap_or_else(Value::null),
                mm,
            ));
            obj.set(PropertyKey::string(slot_key), slot_value);
            return Ok(Value::object(obj));
        }
        // Return undefined so Construct handler uses new_obj_value
        // (which has Object.prototype as [[Prototype]])
        Ok(Value::undefined())
    })
}
