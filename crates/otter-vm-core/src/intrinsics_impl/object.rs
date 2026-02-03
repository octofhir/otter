//! Object.prototype methods and Object static methods implementation
//!
//! All Object methods for ES2026 standard.

use crate::error::VmError;
use crate::gc::GcRef;
use crate::intrinsics::well_known;
use crate::object::{JsObject, PropertyAttributes, PropertyDescriptor, PropertyKey};
use crate::string::JsString;
use crate::value::Value;
use crate::memory::MemoryManager;
use crate::value::Symbol;
use std::sync::Arc;

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
                    let key = PropertyKey::Symbol(well_known::TO_STRING_TAG);
                    let key_value = Value::symbol(Arc::new(Symbol {
                        description: None,
                        id: well_known::TO_STRING_TAG,
                    }));
                    crate::proxy_operations::proxy_get(ncx, proxy, &key, key_value, this_val.clone())?
                } else if let Some(obj) = this_val.as_object() {
                    obj.get(&PropertyKey::Symbol(well_known::TO_STRING_TAG))
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
            |this_val, args, _ncx| {
                if let Some(obj) = this_val.as_object() {
                    if let Some(key) = args.first() {
                        if let Some(s) = key.as_string() {
                            return Ok(Value::boolean(
                                obj.has_own(&PropertyKey::string(s.as_str())),
                            ));
                        }
                    }
                }
                Ok(Value::boolean(false))
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
                        let mut current = target.prototype();
                        while let Some(proto) = current {
                            if std::ptr::eq(
                                proto.as_ptr() as *const _,
                                this_obj.as_ptr() as *const _,
                            ) {
                                return Ok(Value::boolean(true));
                            }
                            current = proto.prototype();
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
            |this_val, args, _ncx| {
                if let Some(obj) = this_val.as_object() {
                    if let Some(key) = args.first() {
                        if let Some(s) = key.as_string() {
                            let pk = PropertyKey::string(s.as_str());
                            if let Some(desc) = obj.get_own_property_descriptor(&pk) {
                                return Ok(Value::boolean(desc.enumerable()));
                            }
                        }
                    }
                }
                Ok(Value::boolean(false))
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
            |_this, args, _ncx| {
                if let Some(obj) = args.first().and_then(|v| v.as_object()) {
                    match obj.prototype() {
                        Some(proto) => Ok(Value::object(proto)),
                        None => Ok(Value::null()),
                    }
                } else {
                    Ok(Value::null())
                }
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
                    if !obj.set_prototype(proto) {
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
                let target = args.first().and_then(|v| v.as_object());
                let key = args.get(1).and_then(|v| v.as_string());
                if let (Some(obj), Some(key_str)) = (target, key) {
                    let pk = PropertyKey::string(key_str.as_str());
                    if let Some(desc) = obj.get_own_property_descriptor(&pk) {
                        // Build descriptor object
                        let desc_obj =
                            GcRef::new(JsObject::new(None, ncx_inner.memory_manager().clone()));
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
            |_this, args, _ncx| {
                let obj = args
                    .first()
                    .and_then(|v| v.as_object())
                    .ok_or_else(|| "Object.keys requires an object".to_string())?;
                let keys = obj.own_keys();
                let mut names = Vec::new();
                for key in keys {
                    match &key {
                        PropertyKey::String(s) => {
                            if let Some(desc) = obj.get_own_property_descriptor(&key) {
                                if desc.enumerable() {
                                    names.push(Value::string(s.clone()));
                                }
                            }
                        }
                        PropertyKey::Index(i) => {
                            if let Some(desc) = obj.get_own_property_descriptor(&key) {
                                if desc.enumerable() {
                                    names.push(Value::string(JsString::intern(
                                        &i.to_string(),
                                    )));
                                }
                            }
                        }
                        _ => {}
                    }
                }
                let result = GcRef::new(JsObject::array(names.len(), _ncx.memory_manager().clone()));
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
            |_this, args, _ncx| {
                let obj = args
                    .first()
                    .and_then(|v| v.as_object())
                    .ok_or_else(|| "Object.values requires an object".to_string())?;
                let keys = obj.own_keys();
                let mut values = Vec::new();
                for key in keys {
                    if let Some(desc) = obj.get_own_property_descriptor(&key) {
                        if desc.enumerable() {
                            if let Some(value) = obj.get(&key) {
                                values.push(value);
                            }
                        }
                    }
                }
                let result = GcRef::new(JsObject::array(values.len(), _ncx.memory_manager().clone()));
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
                let obj = args
                    .first()
                    .and_then(|v| v.as_object())
                    .ok_or_else(|| "Object.entries requires an object".to_string())?;
                let keys = obj.own_keys();
                let mut entries = Vec::new();
                for key in keys {
                    if let Some(desc) = obj.get_own_property_descriptor(&key) {
                        if desc.enumerable() {
                            if let Some(value) = obj.get(&key) {
                                let key_str = match &key {
                                    PropertyKey::String(s) => Value::string(s.clone()),
                                    PropertyKey::Index(i) => {
                                        Value::string(JsString::intern(&i.to_string()))
                                    }
                                    _ => continue,
                                };
                                let entry = GcRef::new(JsObject::array(
                                    2,
                                    ncx_inner.memory_manager().clone(),
                                ));
                                entry.set(PropertyKey::Index(0), key_str);
                                entry.set(PropertyKey::Index(1), value);
                                entries.push(Value::array(entry));
                            }
                        }
                    }
                }
                let result =
                    GcRef::new(JsObject::array(entries.len(), ncx_inner.memory_manager().clone()));
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
                    PropertyKey::Symbol(sym.id)
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
                    PropertyKey::Symbol(sym.id)
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

                if get.is_some() || set.is_some() {
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
                    );
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
                    );
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
                let prototype = if proto_val.is_null() {
                    None
                } else if let Some(proto_obj) = proto_val.as_object() {
                    Some(proto_obj)
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
                                        arr.set_prototype(Some(proto_obj));
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
                                result.set_prototype(Some(proto_obj));
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

    // Object.getOwnPropertyDescriptors
    object_ctor.define_property(
        PropertyKey::string("getOwnPropertyDescriptors"),
        PropertyDescriptor::builtin_method(Value::native_function_with_proto(
            |_this, args, ncx_inner| {
                let obj = match args.first().and_then(|v| v.as_object()) {
                    Some(o) => o,
                    None => {
                        return Ok(Value::object(GcRef::new(JsObject::new(
                            None, ncx_inner.memory_manager().clone(),
                        ))));
                    }
                };
                let result = GcRef::new(JsObject::new(None, ncx_inner.memory_manager().clone()));
                for key in obj.own_keys() {
                    if let Some(desc) = obj.get_own_property_descriptor(&key) {
                        let desc_obj =
                            GcRef::new(JsObject::new(None, ncx_inner.memory_manager().clone()));
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
                let result = GcRef::new(JsObject::new(None, ncx_inner.memory_manager().clone()));

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
    Box::new(|_this, args, _ncx_inner| {
        // When called with an object argument, return it directly
        if let Some(arg) = args.first() {
            if arg.is_object() {
                return Ok(arg.clone());
            }
        }
        // Return undefined so Construct handler uses new_obj_value
        // (which has Object.prototype as [[Prototype]])
        Ok(Value::undefined())
    })
}
