//! Object.prototype methods and Object static methods implementation
//!
//! All Object methods for ES2026 standard.

use crate::context::NativeContext;
use crate::error::VmError;
use crate::gc::GcRef;
use crate::intrinsics::well_known;
use crate::memory::MemoryManager;
use crate::object::{JsObject, PropertyAttributes, PropertyDescriptor, PropertyKey};
use crate::string::JsString;
use crate::value::Symbol;
use crate::value::Value;
use std::sync::Arc;

fn get_builtin_proto(global: &GcRef<JsObject>, name: &str) -> Option<GcRef<JsObject>> {
    global
        .get(&PropertyKey::string(name))
        .and_then(|v| v.as_object())
        .and_then(|ctor| ctor.get(&PropertyKey::string("prototype")))
        .and_then(|v| v.as_object())
}

pub(crate) fn to_object_for_builtin(
    ncx: &mut NativeContext<'_>,
    value: &Value,
) -> Result<GcRef<JsObject>, VmError> {
    if let Some(obj) = value.as_object() {
        return Ok(obj);
    }
    if value.is_null() || value.is_undefined() {
        return Err(VmError::type_error(
            "Cannot convert undefined or null to object",
        ));
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

    let proto =
        get_builtin_proto(&global, proto_name).or_else(|| get_builtin_proto(&global, "Object"));
    let obj = GcRef::new(JsObject::new(
        proto.map(Value::object).unwrap_or_else(Value::null),
        mm,
    ));
    let _ = obj.set(PropertyKey::string(slot_key), slot_value);
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
                        let target = proxy.target().ok_or_else(|| {
                            VmError::type_error(
                                "Cannot perform 'get' on a proxy that has been revoked",
                            )
                        })?;
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
                } else if this_val
                    .as_object()
                    .and_then(|o| o.get(&PropertyKey::string("__primitiveValue__")))
                    .map_or(false, |v| v.is_string())
                {
                    "String"
                } else if this_val
                    .as_object()
                    .and_then(|o| o.get(&PropertyKey::string("__primitiveValue__")))
                    .map_or(false, |v| v.is_boolean())
                {
                    "Boolean"
                } else if this_val
                    .as_object()
                    .and_then(|o| o.get(&PropertyKey::string("__primitiveValue__")))
                    .map_or(false, |v| v.as_number().is_some())
                {
                    "Number"
                } else {
                    "Object"
                };

                let tag_value = if let Some(proxy) = this_val.as_proxy() {
                    let key = PropertyKey::Symbol(well_known::to_string_tag_symbol());
                    let key_value = Value::symbol(GcRef::new(Symbol {
                        description: None,
                        id: well_known::TO_STRING_TAG,
                    }));
                    crate::proxy_operations::proxy_get(
                        ncx,
                        proxy,
                        &key,
                        key_value,
                        this_val.clone(),
                    )?
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

                Ok(Value::string(JsString::intern(&format!(
                    "[object {}]",
                    tag
                ))))
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

                // Handle proxy: hasOwnProperty uses [[GetOwnProperty]], not [[HasProperty]]
                if let Some(proxy) = this_val.as_proxy() {
                    let key = to_property_key(&key_val);
                    let desc = crate::proxy_operations::proxy_get_own_property_descriptor(
                        ncx, proxy, &key, key_val,
                    )?;
                    return Ok(Value::boolean(desc.is_some()));
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
                        ncx, proxy, &key, key_val,
                    )?;
                    return Ok(Value::boolean(
                        desc.map(|d| d.enumerable()).unwrap_or(false),
                    ));
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
                    let proto = crate::proxy_operations::proxy_get_prototype_of(ncx, proxy)?;
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
            |_this, args, ncx| {
                let target = args.first().cloned().unwrap_or(Value::undefined());
                let proto_val = args.get(1).cloned().unwrap_or(Value::undefined());
                let proto_value = if proto_val.is_null() {
                    Value::null()
                } else if proto_val.is_object() {
                    // is_object() returns true for proxies too
                    proto_val
                } else {
                    return Err(VmError::type_error(
                        "Object prototype may only be an Object or null",
                    ));
                };

                // Handle proxy target
                if let Some(proxy) = target.as_proxy() {
                    let new_proto = proto_value.as_object();
                    crate::proxy_operations::proxy_set_prototype_of(ncx, proxy, new_proto)?;
                    return Ok(target);
                }

                if let Some(obj) = target.as_object() {
                    if !obj.set_prototype(proto_value) {
                        return Err(VmError::type_error("Object.setPrototypeOf failed"));
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
                        let result_desc =
                            crate::proxy_operations::proxy_get_own_property_descriptor(
                                ncx_inner,
                                proxy,
                                &key,
                                key_val.clone(),
                            )?;

                        if let Some(desc) = result_desc {
                            let obj_proto = get_builtin_proto(&ncx_inner.global(), "Object")
                                .map(Value::object)
                                .unwrap_or(Value::null());
                            let desc_obj = GcRef::new(JsObject::new(
                                obj_proto,
                                ncx_inner.memory_manager().clone(),
                            ));
                            match &desc {
                                PropertyDescriptor::Data { value, attributes } => {
                                    let _ =
                                        desc_obj.set(PropertyKey::string("value"), value.clone());
                                    let _ = desc_obj.set(
                                        PropertyKey::string("writable"),
                                        Value::boolean(attributes.writable),
                                    );
                                    let _ = desc_obj.set(
                                        PropertyKey::string("enumerable"),
                                        Value::boolean(attributes.enumerable),
                                    );
                                    let _ = desc_obj.set(
                                        PropertyKey::string("configurable"),
                                        Value::boolean(attributes.configurable),
                                    );
                                }
                                PropertyDescriptor::Accessor {
                                    get,
                                    set,
                                    attributes,
                                } => {
                                    let _ = desc_obj.set(
                                        PropertyKey::string("get"),
                                        get.clone().unwrap_or(Value::undefined()),
                                    );
                                    let _ = desc_obj.set(
                                        PropertyKey::string("set"),
                                        set.clone().unwrap_or(Value::undefined()),
                                    );
                                    let _ = desc_obj.set(
                                        PropertyKey::string("enumerable"),
                                        Value::boolean(attributes.enumerable),
                                    );
                                    let _ = desc_obj.set(
                                        PropertyKey::string("configurable"),
                                        Value::boolean(attributes.configurable),
                                    );
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
                    // Build descriptor object with Object.prototype
                    let obj_proto = get_builtin_proto(&ncx_inner.global(), "Object")
                        .map(Value::object)
                        .unwrap_or(Value::null());
                    let desc_obj =
                        GcRef::new(JsObject::new(obj_proto, ncx_inner.memory_manager().clone()));
                    match &desc {
                        PropertyDescriptor::Data { value, attributes } => {
                            let _ = desc_obj.set(PropertyKey::string("value"), value.clone());
                            let _ = desc_obj.set(
                                PropertyKey::string("writable"),
                                Value::boolean(attributes.writable),
                            );
                            let _ = desc_obj.set(
                                PropertyKey::string("enumerable"),
                                Value::boolean(attributes.enumerable),
                            );
                            let _ = desc_obj.set(
                                PropertyKey::string("configurable"),
                                Value::boolean(attributes.configurable),
                            );
                        }
                        PropertyDescriptor::Accessor {
                            get,
                            set,
                            attributes,
                        } => {
                            let _ = desc_obj.set(
                                PropertyKey::string("get"),
                                get.clone().unwrap_or(Value::undefined()),
                            );
                            let _ = desc_obj.set(
                                PropertyKey::string("set"),
                                set.clone().unwrap_or(Value::undefined()),
                            );
                            let _ = desc_obj.set(
                                PropertyKey::string("enumerable"),
                                Value::boolean(attributes.enumerable),
                            );
                            let _ = desc_obj.set(
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
                    let _ = result.set(PropertyKey::Index(i as u32), name);
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
                            ncx,
                            p,
                            &key,
                            key_value.clone(),
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
                                ncx,
                                p,
                                &key,
                                key_value,
                                receiver.clone(),
                            )?
                        } else {
                            crate::object::get_value_full(obj.as_ref().unwrap(), &key, ncx)?
                        };
                        values.push(value);
                    }
                }
                let result =
                    GcRef::new(JsObject::array(values.len(), ncx.memory_manager().clone()));
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
                    let _ = result.set(PropertyKey::Index(i as u32), value);
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
                            ncx_inner,
                            p,
                            &key,
                            key_value.clone(),
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
                                ncx_inner,
                                p,
                                &key,
                                key_value,
                                receiver.clone(),
                            )?
                        } else {
                            crate::object::get_value_full(obj.as_ref().unwrap(), &key, ncx_inner)?
                        };
                        let key_str = match &key {
                            PropertyKey::String(s) => Value::string(*s),
                            PropertyKey::Index(i) => {
                                Value::string(JsString::intern(&i.to_string()))
                            }
                            _ => continue,
                        };
                        let entry =
                            GcRef::new(JsObject::array(2, ncx_inner.memory_manager().clone()));
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
                        let _ = entry.set(PropertyKey::Index(0), key_str);
                        let _ = entry.set(PropertyKey::Index(1), value);
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
                    let _ = result.set(PropertyKey::Index(i as u32), entry);
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
            |_this, args, ncx| {
                let target_val = args
                    .first()
                    .ok_or_else(|| "Object.assign requires at least one argument".to_string())?;
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
                                    let value = crate::object::get_value_full(&source, &key, ncx)?;
                                    let _ = target.set(key, value);
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
                let prop = args
                    .get(1)
                    .ok_or_else(|| "Object.hasOwn requires a property key".to_string())?;
                let key = crate::intrinsics_impl::reflect::to_property_key(prop);
                Ok(Value::boolean(obj.has_own(&key)))
            },
            mm.clone(),
            fn_proto.clone(),
        )),
    );

    // Object.freeze
    let freeze_fn = Value::native_function_with_proto(
        |_this, args, ncx| {
            let obj_val = args.first().cloned().unwrap_or(Value::undefined());
            if let Some(proxy) = obj_val.as_proxy() {
                // Proxy path: preventExtensions + defineProperty for each key
                let _ = crate::proxy_operations::proxy_prevent_extensions(ncx, proxy)?;
                let keys = crate::proxy_operations::proxy_own_keys(ncx, proxy)?;
                for key in &keys {
                    let key_value = crate::proxy_operations::property_key_to_value_pub(key);
                    if let Some(desc) = crate::proxy_operations::proxy_get_own_property_descriptor(
                        ncx,
                        proxy,
                        key,
                        key_value.clone(),
                    )? {
                        let frozen_desc = match desc {
                            PropertyDescriptor::Data { value, attributes } => {
                                PropertyDescriptor::data_with_attrs(
                                    value,
                                    PropertyAttributes {
                                        writable: false,
                                        enumerable: attributes.enumerable,
                                        configurable: false,
                                    },
                                )
                            }
                            PropertyDescriptor::Accessor {
                                get,
                                set,
                                attributes,
                            } => PropertyDescriptor::Accessor {
                                get,
                                set,
                                attributes: PropertyAttributes {
                                    writable: false,
                                    enumerable: attributes.enumerable,
                                    configurable: false,
                                },
                            },
                            PropertyDescriptor::Deleted => continue,
                        };
                        let _ = crate::proxy_operations::proxy_define_property(
                            ncx,
                            proxy,
                            key,
                            key_value,
                            &frozen_desc,
                        )?;
                    }
                }
            } else if let Some(obj) = obj_val.as_object() {
                obj.freeze();
            }
            Ok(obj_val)
        },
        mm.clone(),
        fn_proto.clone(),
    );
    if let Some(obj) = freeze_fn.as_object() {
        obj.define_property(
            PropertyKey::string("length"),
            PropertyDescriptor::function_length(Value::int32(1)),
        );
        obj.define_property(
            PropertyKey::string("name"),
            PropertyDescriptor::function_length(Value::string(JsString::intern("freeze"))),
        );
    }
    object_ctor.define_property(
        PropertyKey::string("freeze"),
        PropertyDescriptor::builtin_method(freeze_fn),
    );

    // Object.isFrozen
    let is_frozen_fn = Value::native_function_with_proto(
        |_this, args, ncx| {
            let arg = args.first().cloned().unwrap_or(Value::undefined());
            if let Some(proxy) = arg.as_proxy() {
                if crate::proxy_operations::proxy_is_extensible(ncx, proxy)? {
                    return Ok(Value::boolean(false));
                }
                let keys = crate::proxy_operations::proxy_own_keys(ncx, proxy)?;
                for key in &keys {
                    let key_value = crate::proxy_operations::property_key_to_value_pub(key);
                    if let Some(desc) = crate::proxy_operations::proxy_get_own_property_descriptor(
                        ncx, proxy, key, key_value,
                    )? {
                        if desc.is_configurable() {
                            return Ok(Value::boolean(false));
                        }
                        if let PropertyDescriptor::Data { attributes, .. } = &desc {
                            if attributes.writable {
                                return Ok(Value::boolean(false));
                            }
                        }
                    }
                }
                return Ok(Value::boolean(true));
            }
            let is_frozen = arg.as_object().map(|o| o.is_frozen()).unwrap_or(true);
            Ok(Value::boolean(is_frozen))
        },
        mm.clone(),
        fn_proto.clone(),
    );
    if let Some(obj) = is_frozen_fn.as_object() {
        obj.define_property(
            PropertyKey::string("length"),
            PropertyDescriptor::function_length(Value::int32(1)),
        );
        obj.define_property(
            PropertyKey::string("name"),
            PropertyDescriptor::function_length(Value::string(JsString::intern("isFrozen"))),
        );
    }
    object_ctor.define_property(
        PropertyKey::string("isFrozen"),
        PropertyDescriptor::builtin_method(is_frozen_fn),
    );

    // Object.seal
    let seal_fn = Value::native_function_with_proto(
        |_this, args, ncx| {
            let obj_val = args.first().cloned().unwrap_or(Value::undefined());
            if let Some(proxy) = obj_val.as_proxy() {
                // Proxy path: preventExtensions + defineProperty for each key (configurable=false)
                let _ = crate::proxy_operations::proxy_prevent_extensions(ncx, proxy)?;
                let keys = crate::proxy_operations::proxy_own_keys(ncx, proxy)?;
                for key in &keys {
                    let key_value = crate::proxy_operations::property_key_to_value_pub(key);
                    if let Some(desc) = crate::proxy_operations::proxy_get_own_property_descriptor(
                        ncx,
                        proxy,
                        key,
                        key_value.clone(),
                    )? {
                        let sealed_desc = match desc {
                            PropertyDescriptor::Data { value, attributes } => {
                                PropertyDescriptor::data_with_attrs(
                                    value,
                                    PropertyAttributes {
                                        writable: attributes.writable,
                                        enumerable: attributes.enumerable,
                                        configurable: false,
                                    },
                                )
                            }
                            PropertyDescriptor::Accessor {
                                get,
                                set,
                                attributes,
                            } => PropertyDescriptor::Accessor {
                                get,
                                set,
                                attributes: PropertyAttributes {
                                    writable: false,
                                    enumerable: attributes.enumerable,
                                    configurable: false,
                                },
                            },
                            PropertyDescriptor::Deleted => continue,
                        };
                        let _ = crate::proxy_operations::proxy_define_property(
                            ncx,
                            proxy,
                            key,
                            key_value,
                            &sealed_desc,
                        )?;
                    }
                }
            } else if let Some(obj) = obj_val.as_object() {
                obj.seal();
            }
            Ok(obj_val)
        },
        mm.clone(),
        fn_proto.clone(),
    );
    if let Some(obj) = seal_fn.as_object() {
        obj.define_property(
            PropertyKey::string("length"),
            PropertyDescriptor::function_length(Value::int32(1)),
        );
        obj.define_property(
            PropertyKey::string("name"),
            PropertyDescriptor::function_length(Value::string(JsString::intern("seal"))),
        );
    }
    object_ctor.define_property(
        PropertyKey::string("seal"),
        PropertyDescriptor::builtin_method(seal_fn),
    );

    // Object.isSealed
    let is_sealed_fn = Value::native_function_with_proto(
        |_this, args, ncx| {
            let arg = args.first().cloned().unwrap_or(Value::undefined());
            if let Some(proxy) = arg.as_proxy() {
                if crate::proxy_operations::proxy_is_extensible(ncx, proxy)? {
                    return Ok(Value::boolean(false));
                }
                let keys = crate::proxy_operations::proxy_own_keys(ncx, proxy)?;
                for key in &keys {
                    let key_value = crate::proxy_operations::property_key_to_value_pub(key);
                    if let Some(desc) = crate::proxy_operations::proxy_get_own_property_descriptor(
                        ncx, proxy, key, key_value,
                    )? {
                        if desc.is_configurable() {
                            return Ok(Value::boolean(false));
                        }
                    }
                }
                return Ok(Value::boolean(true));
            }
            let is_sealed = arg.as_object().map(|o| o.is_sealed()).unwrap_or(true);
            Ok(Value::boolean(is_sealed))
        },
        mm.clone(),
        fn_proto.clone(),
    );
    if let Some(obj) = is_sealed_fn.as_object() {
        obj.define_property(
            PropertyKey::string("length"),
            PropertyDescriptor::function_length(Value::int32(1)),
        );
        obj.define_property(
            PropertyKey::string("name"),
            PropertyDescriptor::function_length(Value::string(JsString::intern("isSealed"))),
        );
    }
    object_ctor.define_property(
        PropertyKey::string("isSealed"),
        PropertyDescriptor::builtin_method(is_sealed_fn),
    );

    // Object.preventExtensions
    let prevent_extensions_fn = Value::native_function_with_proto(
        |_this, args, ncx| {
            let obj_val = args.first().cloned().unwrap_or(Value::undefined());
            if let Some(proxy) = obj_val.as_proxy() {
                let _ = crate::proxy_operations::proxy_prevent_extensions(ncx, proxy)?;
            } else if let Some(obj) = obj_val.as_object() {
                obj.prevent_extensions();
            }
            Ok(obj_val)
        },
        mm.clone(),
        fn_proto.clone(),
    );
    if let Some(obj) = prevent_extensions_fn.as_object() {
        obj.define_property(
            PropertyKey::string("length"),
            PropertyDescriptor::function_length(Value::int32(1)),
        );
        obj.define_property(
            PropertyKey::string("name"),
            PropertyDescriptor::function_length(Value::string(JsString::intern(
                "preventExtensions",
            ))),
        );
    }
    object_ctor.define_property(
        PropertyKey::string("preventExtensions"),
        PropertyDescriptor::builtin_method(prevent_extensions_fn),
    );

    // Object.isExtensible
    let is_extensible_fn = Value::native_function_with_proto(
        |_this, args, ncx| {
            let arg = args.first().cloned().unwrap_or(Value::undefined());
            if let Some(proxy) = arg.as_proxy() {
                let is_extensible = crate::proxy_operations::proxy_is_extensible(ncx, proxy)?;
                return Ok(Value::boolean(is_extensible));
            }
            let is_extensible = arg.as_object().map(|o| o.is_extensible()).unwrap_or(false);
            Ok(Value::boolean(is_extensible))
        },
        mm.clone(),
        fn_proto.clone(),
    );
    if let Some(obj) = is_extensible_fn.as_object() {
        obj.define_property(
            PropertyKey::string("length"),
            PropertyDescriptor::function_length(Value::int32(1)),
        );
        obj.define_property(
            PropertyKey::string("name"),
            PropertyDescriptor::function_length(Value::string(JsString::intern("isExtensible"))),
        );
    }
    object_ctor.define_property(
        PropertyKey::string("isExtensible"),
        PropertyDescriptor::builtin_method(is_extensible_fn),
    );

    // Object.defineProperty
    let define_property_fn = Value::native_function_with_proto(
        |_this, args, ncx| {
            let obj_val = args
                .first()
                .ok_or_else(|| "Object.defineProperty requires an object".to_string())?;

            // Per spec §20.1.2.4 step 1: TypeError if first argument is not an object
            if obj_val.as_object().is_none() && obj_val.as_proxy().is_none() {
                return Err(VmError::type_error(
                    "Object.defineProperty called on non-object",
                ));
            }

            let key_val = args
                .get(1)
                .ok_or_else(|| "Object.defineProperty requires a property key".to_string())?;
            let descriptor = args
                .get(2)
                .ok_or_else(|| "Object.defineProperty requires a descriptor".to_string())?;

            // Convert key via ToPropertyKey (ES2026 §7.1.14)
            let key = crate::intrinsics_impl::reflect::to_property_key(key_val);

            let attr_obj = descriptor
                .as_object()
                .ok_or_else(|| "Property descriptor must be an object".to_string())?;

            // Parse JS object into PartialDescriptor (ToPropertyDescriptor)
            let desc = crate::object::to_property_descriptor(&attr_obj, ncx)
                .map_err(|e| VmError::type_error(&e))?;

            // Proxy path
            if let Some(proxy) = obj_val.as_proxy() {
                // Convert PartialDescriptor to full for proxy trap
                let full_desc = if desc.is_accessor_descriptor() {
                    let get_val = desc.get.clone().unwrap_or(Value::undefined());
                    let set_val = desc.set.clone().unwrap_or(Value::undefined());
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
                        desc.value.clone().unwrap_or(Value::undefined()),
                        PropertyAttributes {
                            writable: desc.writable.unwrap_or(false),
                            enumerable: desc.enumerable.unwrap_or(false),
                            configurable: desc.configurable.unwrap_or(false),
                        },
                    )
                };
                let key_value = crate::proxy_operations::property_key_to_value_pub(&key);
                let success = crate::proxy_operations::proxy_define_property(
                    ncx, proxy, &key, key_value, &full_desc,
                )?;
                if !success {
                    return Err(VmError::type_error(
                        "Cannot define property: proxy rejected the operation",
                    ));
                }
                return Ok(obj_val.clone());
            }

            let obj = obj_val.as_object().ok_or_else(|| {
                "Object.defineProperty first argument must be an object".to_string()
            })?;

            let success = obj.define_own_property(key, &desc);

            if !success {
                return Err(VmError::type_error(
                    "Cannot define property: object is not extensible or property is non-configurable",
                ));
            }

            Ok(obj_val.clone())
        },
        mm.clone(),
        fn_proto.clone(),
    );
    if let Some(obj) = define_property_fn.as_object() {
        obj.define_property(
            PropertyKey::string("length"),
            PropertyDescriptor::function_length(Value::int32(3)),
        );
        obj.define_property(
            PropertyKey::string("name"),
            PropertyDescriptor::function_length(Value::string(JsString::intern("defineProperty"))),
        );
    }
    object_ctor.define_property(
        PropertyKey::string("defineProperty"),
        PropertyDescriptor::builtin_method(define_property_fn),
    );

    // Object.create
    object_ctor.define_property(
        PropertyKey::string("create"),
        PropertyDescriptor::builtin_method(Value::native_function_with_proto(
            |_this, args, ncx_inner| {
                let proto_val = args
                    .first()
                    .ok_or_else(|| "Object.create requires a prototype argument".to_string())?;

                // Prototype can be null, object, or proxy
                let prototype = if proto_val.is_null() {
                    Value::null()
                } else if proto_val.as_object().is_some() || proto_val.as_proxy().is_some() {
                    proto_val.clone()
                } else {
                    return Err(VmError::type_error(
                        "Object prototype may only be an Object or null",
                    ));
                };

                let new_obj =
                    GcRef::new(JsObject::new(prototype, ncx_inner.memory_manager().clone()));

                // Handle optional properties object (second argument)
                if let Some(props_val) = args.get(1) {
                    if !props_val.is_undefined() {
                        let props = props_val
                            .as_object()
                            .ok_or_else(|| "Properties argument must be an object".to_string())?;
                        // Per spec: collect descriptors first from enumerable own properties
                        let keys = props.own_keys();
                        let mut descriptors = Vec::new();
                        for key in keys {
                            if let Some(prop_desc) = props.get_own_property_descriptor(&key) {
                                if !prop_desc.enumerable() {
                                    continue;
                                }
                            } else {
                                continue;
                            }
                            let descriptor =
                                crate::object::get_value_full(&props, &key, ncx_inner)?;
                            let attr_obj = descriptor.as_object().ok_or_else(|| {
                                VmError::type_error("Property description must be an object")
                            })?;
                            let desc = crate::object::to_property_descriptor(&attr_obj, ncx_inner)
                                .map_err(|e| VmError::type_error(&e))?;
                            descriptors.push((key, desc));
                        }
                        for (key, desc) in descriptors {
                            let success = new_obj.define_own_property(key, &desc);
                            if !success {
                                return Err(VmError::type_error("Cannot define property"));
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
                let result = if let (Some(n1), Some(n2)) = (v1.as_number(), v2.as_number()) {
                    if n1.is_nan() && n2.is_nan() {
                        true
                    } else if n1 == 0.0 && n2 == 0.0 {
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
                let arg = args.first().cloned().unwrap_or(Value::undefined());

                // Proxy path: use ownKeys trap
                let keys = if let Some(proxy) = arg.as_proxy() {
                    crate::proxy_operations::proxy_own_keys(ncx_inner, proxy)?
                } else if let Some(obj) = arg.as_object() {
                    obj.own_keys()
                } else {
                    let arr = GcRef::new(JsObject::array(0, ncx_inner.memory_manager().clone()));
                    if let Some(array_ctor) = ncx_inner.global().get(&PropertyKey::string("Array"))
                    {
                        if let Some(array_obj) = array_ctor.as_object() {
                            if let Some(proto_val) =
                                array_obj.get(&PropertyKey::string("prototype"))
                            {
                                if let Some(proto_obj) = proto_val.as_object() {
                                    arr.set_prototype(Value::object(proto_obj));
                                }
                            }
                        }
                    }
                    return Ok(Value::array(arr));
                };
                let mut names = Vec::new();
                for key in keys {
                    match key {
                        PropertyKey::String(s) => {
                            let name = s.as_str();
                            if name == "__non_constructor" || name == "__realm_id__" {
                                continue;
                            }
                            names.push(Value::string(s));
                        }
                        PropertyKey::Index(i) => {
                            names.push(Value::string(JsString::intern(&i.to_string())));
                        }
                        _ => {} // skip symbols
                    }
                }
                let result = GcRef::new(JsObject::array(
                    names.len(),
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
                for (i, name) in names.into_iter().enumerate() {
                    let _ = result.set(PropertyKey::Index(i as u32), name);
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

                // Proxy path: use ownKeys trap
                let keys = if let Some(proxy) = arg.as_proxy() {
                    crate::proxy_operations::proxy_own_keys(ncx_inner, proxy)?
                } else if let Some(obj) = arg.as_object() {
                    obj.own_keys()
                } else {
                    // Primitives (string, number, boolean, symbol) have no own symbol properties
                    let arr = GcRef::new(JsObject::array(0, ncx_inner.memory_manager().clone()));
                    if let Some(array_ctor) = ncx_inner.global().get(&PropertyKey::string("Array"))
                    {
                        if let Some(array_obj) = array_ctor.as_object() {
                            if let Some(proto_val) =
                                array_obj.get(&PropertyKey::string("prototype"))
                            {
                                if let Some(proto_obj) = proto_val.as_object() {
                                    arr.set_prototype(Value::object(proto_obj));
                                }
                            }
                        }
                    }
                    return Ok(Value::array(arr));
                };
                let mut symbols = Vec::new();
                for key in keys {
                    if let PropertyKey::Symbol(sym) = key {
                        symbols.push(Value::symbol(sym));
                    }
                }
                let result = GcRef::new(JsObject::array(
                    symbols.len(),
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
                for (i, sym) in symbols.into_iter().enumerate() {
                    let _ = result.set(PropertyKey::Index(i as u32), sym);
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
                            Value::null(),
                            ncx_inner.memory_manager().clone(),
                        ))));
                    }
                };
                let obj_proto = get_builtin_proto(&ncx_inner.global(), "Object")
                    .map(Value::object)
                    .unwrap_or(Value::null());
                let result = GcRef::new(JsObject::new(
                    obj_proto.clone(),
                    ncx_inner.memory_manager().clone(),
                ));
                for key in obj.own_keys() {
                    if let Some(desc) = obj.get_own_property_descriptor(&key) {
                        let desc_obj = GcRef::new(JsObject::new(
                            obj_proto.clone(),
                            ncx_inner.memory_manager().clone(),
                        ));
                        match &desc {
                            PropertyDescriptor::Data { value, attributes } => {
                                let _ = desc_obj.set(PropertyKey::string("value"), value.clone());
                                let _ = desc_obj.set(
                                    PropertyKey::string("writable"),
                                    Value::boolean(attributes.writable),
                                );
                                let _ = desc_obj.set(
                                    PropertyKey::string("enumerable"),
                                    Value::boolean(attributes.enumerable),
                                );
                                let _ = desc_obj.set(
                                    PropertyKey::string("configurable"),
                                    Value::boolean(attributes.configurable),
                                );
                            }
                            PropertyDescriptor::Accessor {
                                get,
                                set,
                                attributes,
                            } => {
                                let _ = desc_obj.set(
                                    PropertyKey::string("get"),
                                    get.clone().unwrap_or(Value::undefined()),
                                );
                                let _ = desc_obj.set(
                                    PropertyKey::string("set"),
                                    set.clone().unwrap_or(Value::undefined()),
                                );
                                let _ = desc_obj.set(
                                    PropertyKey::string("enumerable"),
                                    Value::boolean(attributes.enumerable),
                                );
                                let _ = desc_obj.set(
                                    PropertyKey::string("configurable"),
                                    Value::boolean(attributes.configurable),
                                );
                            }
                            PropertyDescriptor::Deleted => {}
                        }
                        let _ = result.set(key, Value::object(desc_obj));
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
            |_this, args, ncx| {
                let obj_val = args
                    .first()
                    .ok_or_else(|| "Object.defineProperties requires an object".to_string())?;
                let is_proxy = obj_val.as_proxy().is_some();
                let obj = if is_proxy {
                    None
                } else {
                    Some(obj_val.as_object().ok_or_else(|| {
                        "Object.defineProperties first argument must be an object".to_string()
                    })?)
                };
                let props_val = args
                    .get(1)
                    .ok_or_else(|| "Object.defineProperties requires properties".to_string())?;
                let props = props_val.as_object().ok_or_else(|| {
                    "Object.defineProperties second argument must be an object".to_string()
                })?;

                // Per spec §20.1.2.3.1: collect descriptors first, then apply
                let keys = props.own_keys();
                let mut descriptors = Vec::new();
                for key in keys {
                    // Per spec: only process enumerable own properties of props
                    if let Some(prop_desc) = props.get_own_property_descriptor(&key) {
                        if !prop_desc.enumerable() {
                            continue;
                        }
                    } else {
                        continue;
                    }
                    let descriptor = crate::object::get_value_full(&props, &key, ncx)?;
                    // Per spec: ToPropertyDescriptor throws TypeError if not an object
                    let attr_obj = descriptor.as_object().ok_or_else(|| {
                        VmError::type_error("Property description must be an object")
                    })?;
                    let desc = crate::object::to_property_descriptor(&attr_obj, ncx)
                        .map_err(|e| VmError::type_error(&e))?;
                    descriptors.push((key, desc));
                }
                // Now apply all collected descriptors
                for (key, desc) in descriptors {
                    if let Some(_proxy) = obj_val.as_proxy() {
                        // Convert partial to full for proxy trap
                        let full_desc = if desc.is_accessor_descriptor() {
                            let get_val = desc.get.clone().unwrap_or(Value::undefined());
                            let set_val = desc.set.clone().unwrap_or(Value::undefined());
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
                                desc.value.clone().unwrap_or(Value::undefined()),
                                PropertyAttributes {
                                    writable: desc.writable.unwrap_or(false),
                                    enumerable: desc.enumerable.unwrap_or(false),
                                    configurable: desc.configurable.unwrap_or(false),
                                },
                            )
                        };
                        let key_value = crate::proxy_operations::property_key_to_value_pub(&key);
                        let _ = crate::proxy_operations::proxy_define_property(
                            ncx, _proxy, &key, key_value, &full_desc,
                        )?;
                    } else if let Some(ref obj) = obj {
                        let success = obj.define_own_property(key, &desc);
                        if !success {
                            return Err(VmError::type_error("Cannot redefine property"));
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
                let iterable = args
                    .first()
                    .ok_or_else(|| "Object.fromEntries requires an iterable".to_string())?;
                let iter_obj = iterable
                    .as_object()
                    .ok_or_else(|| "Object.fromEntries argument must be iterable".to_string())?;
                let result = GcRef::new(JsObject::new(
                    Value::null(),
                    ncx_inner.memory_manager().clone(),
                ));

                // Support array-like iterables (check length property)
                if let Some(len_val) =
                    iter_obj.get(&PropertyKey::String(JsString::intern("length")))
                {
                    if let Some(len) = len_val.as_number() {
                        for i in 0..(len as u32) {
                            if let Some(entry) = iter_obj.get(&PropertyKey::Index(i)) {
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
                                        PropertyKey::String(JsString::intern(&n.to_string()))
                                    } else {
                                        PropertyKey::String(JsString::intern("undefined"))
                                    };
                                    let _ = result.set(pk, value);
                                }
                            }
                        }
                    }
                }
                Ok(Value::object(result))
            },
            mm.clone(),
            fn_proto.clone(),
        )),
    );

    // Object.groupBy ( items, callbackfn ) — ES2024
    object_ctor.define_property(
        PropertyKey::string("groupBy"),
        PropertyDescriptor::builtin_method(Value::native_function_with_proto(
            |_this, args, ncx| {
                let items = args.first().cloned().unwrap_or(Value::undefined());
                let callback = args.get(1).cloned().unwrap_or(Value::undefined());

                if !callback.is_callable() {
                    return Err(VmError::type_error(
                        "Object.groupBy: callbackfn is not a function",
                    ));
                }

                let mm = ncx.ctx.memory_manager().clone();
                let result = GcRef::new(JsObject::new(Value::null(), mm.clone()));
                // Root result across call_function GC points
                ncx.ctx.push_root_slot(Value::object(result));

                // Iterate items using the iterable protocol
                let iter_sym = crate::intrinsics::well_known::iterator_symbol();
                let iter_key = PropertyKey::Symbol(iter_sym);

                let iter_fn = if let Some(obj) = items.as_object().or_else(|| items.as_array()) {
                    obj.get(&iter_key).unwrap_or(Value::undefined())
                } else if items.as_string().is_some() {
                    ncx.ctx
                        .get_global("String")
                        .and_then(|v| v.as_object())
                        .and_then(|c| c.get(&PropertyKey::string("prototype")))
                        .and_then(|v| v.as_object())
                        .and_then(|proto| proto.get(&iter_key))
                        .unwrap_or(Value::undefined())
                } else {
                    Value::undefined()
                };

                if !iter_fn.is_callable() {
                    ncx.ctx.pop_root_slots(1);
                    return Err(VmError::type_error("object is not iterable"));
                }

                let loop_result: Result<(), VmError> = (|| {
                    let iterator = ncx.call_function(&iter_fn, items.clone(), &[])?;
                    let iterator_obj = iterator
                        .as_object()
                        .ok_or_else(|| VmError::type_error("Iterator result is not an object"))?;
                    let next_fn = iterator_obj
                        .get(&PropertyKey::string("next"))
                        .unwrap_or(Value::undefined());

                    let mut k: u32 = 0;
                    loop {
                        let iter_result = ncx.call_function(&next_fn, iterator.clone(), &[])?;
                        let iter_obj = iter_result
                            .as_object()
                            .ok_or_else(|| {
                                VmError::type_error("Iterator result is not an object")
                            })?;
                        let done = iter_obj
                            .get(&PropertyKey::string("done"))
                            .unwrap_or(Value::undefined());
                        if done.to_boolean() {
                            break;
                        }
                        let value = iter_obj
                            .get(&PropertyKey::string("value"))
                            .unwrap_or(Value::undefined());

                        let group_key = ncx.call_function(
                            &callback,
                            Value::undefined(),
                            &[value.clone(), Value::number(k as f64)],
                        )?;
                        k += 1;

                        // Convert key to property key (string)
                        let prop_key = if let Some(s) = group_key.as_string() {
                            PropertyKey::String(s)
                        } else if let Some(n) = group_key.as_number() {
                            PropertyKey::String(JsString::intern(
                                &crate::globals::js_number_to_string(n),
                            ))
                        } else if group_key.is_undefined() {
                            PropertyKey::string("undefined")
                        } else if group_key.is_null() {
                            PropertyKey::string("null")
                        } else if let Some(b) = group_key.as_boolean() {
                            PropertyKey::string(if b { "true" } else { "false" })
                        } else if let Some(sym) = group_key.as_symbol() {
                            PropertyKey::Symbol(sym)
                        } else {
                            PropertyKey::string("[object Object]")
                        };

                        if let Some(existing) = result.get(&prop_key) {
                            if let Some(arr) =
                                existing.as_array().or_else(|| existing.as_object())
                            {
                                let len = arr
                                    .get(&PropertyKey::string("length"))
                                    .and_then(|v| v.as_number())
                                    .unwrap_or(0.0) as u32;
                                let _ = arr.set(PropertyKey::Index(len), value);
                                let _ = arr.set(
                                    PropertyKey::string("length"),
                                    Value::number((len + 1) as f64),
                                );
                            }
                        } else {
                            let arr = JsObject::array(4, mm.clone());
                            if let Some(array_proto) = ncx
                                .ctx
                                .get_global("Array")
                                .and_then(|v| v.as_object())
                                .and_then(|c| c.get(&PropertyKey::string("prototype")))
                                .and_then(|v| v.as_object())
                            {
                                arr.set_prototype(Value::object(array_proto));
                            }
                            let _ = arr.set(PropertyKey::Index(0), value);
                            let _ = arr.set(PropertyKey::string("length"), Value::number(1.0));
                            let _ = result.set(prop_key, Value::array(GcRef::new(arr)));
                        }
                    }
                    Ok(())
                })();

                ncx.ctx.pop_root_slots(1);
                loop_result?;
                Ok(Value::object(result))
            },
            mm.clone(),
            fn_proto,
        )),
    );
}

/// Create Object constructor function
pub fn create_object_constructor() -> Box<
    dyn Fn(&Value, &[Value], &mut crate::context::NativeContext<'_>) -> Result<Value, VmError>
        + Send
        + Sync,
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
                    obj_proto.map(Value::object).unwrap_or_else(Value::null),
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
            let _ = obj.set(PropertyKey::string(slot_key), slot_value);
            return Ok(Value::object(obj));
        }
        // Return undefined so Construct handler uses new_obj_value
        // (which has Object.prototype as [[Prototype]])
        Ok(Value::undefined())
    })
}

/// Create internal helper used by compiler lowering for object rest:
/// `__Object_rest(source, excludedKeysArray)`.
pub fn create_object_rest_helper(fn_proto: GcRef<JsObject>, mm: &Arc<MemoryManager>) -> Value {
    Value::native_function_with_proto(
        |_this, args, ncx| {
            let source = args.first().cloned().unwrap_or_else(Value::undefined);
            if source.is_null() || source.is_undefined() {
                return Err(VmError::type_error("Cannot destructure null or undefined"));
            }
            let source_obj = to_object_for_builtin(ncx, &source)?;

            let excluded_keys_arg = args.get(1).cloned().unwrap_or_else(Value::undefined);
            let mut excluded_keys = Vec::new();
            if let Some(excluded_obj) = excluded_keys_arg.as_object() {
                for key in excluded_obj.own_keys() {
                    if matches!(key, PropertyKey::Index(_))
                        && let Some(v) = excluded_obj.get(&key)
                    {
                        excluded_keys.push(crate::intrinsics_impl::reflect::to_property_key(&v));
                    }
                }
            }

            let proto = get_builtin_proto(&ncx.global(), "Object");
            let result = GcRef::new(JsObject::new(
                proto.map(Value::object).unwrap_or_else(Value::null),
                ncx.memory_manager().clone(),
            ));

            for key in source_obj.own_keys() {
                if excluded_keys.iter().any(|k| *k == key) {
                    continue;
                }
                if let Some(desc) = source_obj.get_own_property_descriptor(&key)
                    && desc.enumerable()
                    && let Some(value) = source_obj.get(&key)
                {
                    let _ = result.set(key, value);
                }
            }

            Ok(Value::object(result))
        },
        mm.clone(),
        fn_proto,
    )
}
