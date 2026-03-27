use crate::builders::ClassBuilder;
use crate::descriptors::{
    JsClassDescriptor, NativeBindingDescriptor, NativeBindingTarget, NativeFunctionDescriptor,
    VmNativeCallError,
};
use crate::object::{HeapValueKind, ObjectHandle, PropertyAttributes, PropertyValue};
use crate::value::RegisterValue;

use super::{
    IntrinsicsError, VmIntrinsics,
    boolean_class::box_boolean_object,
    install::{IntrinsicInstallContext, IntrinsicInstaller, install_class_plan},
    number_class::box_number_object,
    string_class::box_string_object,
};

pub(super) static OBJECT_INTRINSIC: ObjectIntrinsic = ObjectIntrinsic;

const STRING_DATA_SLOT: &str = "__otter_string_data__";
const NUMBER_DATA_SLOT: &str = "__otter_number_data__";
const BOOLEAN_DATA_SLOT: &str = "__otter_boolean_data__";
const OBJECT_IS_PROTOTYPE_OF_ERROR: &str =
    "Object.prototype.isPrototypeOf requires an object receiver";

pub(super) struct ObjectIntrinsic;

impl IntrinsicInstaller for ObjectIntrinsic {
    fn init(
        &self,
        intrinsics: &mut VmIntrinsics,
        cx: &mut IntrinsicInstallContext<'_>,
    ) -> Result<(), IntrinsicsError> {
        let descriptor = object_class_descriptor();
        let plan = ClassBuilder::from_descriptor(&descriptor)
            .expect("Object class descriptors should normalize")
            .build();

        let constructor = if let Some(descriptor) = plan.constructor() {
            let host_function = cx.native_functions.register(descriptor.clone());
            cx.alloc_intrinsic_host_function(host_function, intrinsics.function_prototype())?
        } else {
            cx.alloc_intrinsic_object(Some(intrinsics.object_prototype()))?
        };

        intrinsics.object_constructor = constructor;
        install_class_plan(
            intrinsics.object_prototype(),
            intrinsics.object_constructor(),
            &plan,
            intrinsics.function_prototype(),
            cx,
        )?;

        Ok(())
    }

    fn install_on_global(
        &self,
        intrinsics: &VmIntrinsics,
        cx: &mut IntrinsicInstallContext<'_>,
    ) -> Result<(), IntrinsicsError> {
        cx.install_global_value(
            intrinsics,
            "Object",
            RegisterValue::from_object_handle(intrinsics.object_constructor().0),
        )
    }
}

fn object_class_descriptor() -> JsClassDescriptor {
    JsClassDescriptor::new("Object")
        .with_constructor(NativeFunctionDescriptor::constructor(
            "Object",
            1,
            object_constructor,
        ))
        .with_binding(NativeBindingDescriptor::new(
            NativeBindingTarget::Prototype,
            NativeFunctionDescriptor::method("toString", 0, object_to_string),
        ))
        .with_binding(NativeBindingDescriptor::new(
            NativeBindingTarget::Prototype,
            NativeFunctionDescriptor::method("isPrototypeOf", 1, object_is_prototype_of),
        ))
        .with_binding(NativeBindingDescriptor::new(
            NativeBindingTarget::Prototype,
            NativeFunctionDescriptor::method("valueOf", 0, object_value_of),
        ))
        .with_binding(NativeBindingDescriptor::new(
            NativeBindingTarget::Constructor,
            NativeFunctionDescriptor::method("create", 2, object_create),
        ))
        .with_binding(NativeBindingDescriptor::new(
            NativeBindingTarget::Constructor,
            NativeFunctionDescriptor::method("getOwnPropertyDescriptor", 2, object_get_own_property_descriptor),
        ))
        .with_binding(NativeBindingDescriptor::new(
            NativeBindingTarget::Constructor,
            NativeFunctionDescriptor::method("defineProperty", 3, object_define_property),
        ))
        .with_binding(NativeBindingDescriptor::new(
            NativeBindingTarget::Constructor,
            NativeFunctionDescriptor::method("getOwnPropertyNames", 1, object_get_own_property_names),
        ))
        .with_binding(NativeBindingDescriptor::new(
            NativeBindingTarget::Constructor,
            NativeFunctionDescriptor::method("keys", 1, object_keys),
        ))
        .with_binding(NativeBindingDescriptor::new(
            NativeBindingTarget::Constructor,
            NativeFunctionDescriptor::method("values", 1, object_values),
        ))
        .with_binding(NativeBindingDescriptor::new(
            NativeBindingTarget::Constructor,
            NativeFunctionDescriptor::method("entries", 1, object_entries),
        ))
        .with_binding(NativeBindingDescriptor::new(
            NativeBindingTarget::Constructor,
            NativeFunctionDescriptor::method("freeze", 1, object_freeze),
        ))
        .with_binding(NativeBindingDescriptor::new(
            NativeBindingTarget::Constructor,
            NativeFunctionDescriptor::method("seal", 1, object_seal),
        ))
        .with_binding(NativeBindingDescriptor::new(
            NativeBindingTarget::Constructor,
            NativeFunctionDescriptor::method("getPrototypeOf", 1, object_get_prototype_of),
        ))
        .with_binding(NativeBindingDescriptor::new(
            NativeBindingTarget::Constructor,
            NativeFunctionDescriptor::method("setPrototypeOf", 2, object_set_prototype_of),
        ))
        .with_binding(NativeBindingDescriptor::new(
            NativeBindingTarget::Constructor,
            NativeFunctionDescriptor::method("assign", 2, object_assign),
        ))
        .with_binding(NativeBindingDescriptor::new(
            NativeBindingTarget::Prototype,
            NativeFunctionDescriptor::method("hasOwnProperty", 1, object_has_own_property),
        ))
        .with_binding(NativeBindingDescriptor::new(
            NativeBindingTarget::Prototype,
            NativeFunctionDescriptor::method("propertyIsEnumerable", 1, object_property_is_enumerable),
        ))
}

fn object_constructor(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    if let Some(value) = args.first().copied() {
        if value == RegisterValue::undefined() || value == RegisterValue::null() {
            if this.as_object_handle().is_some() {
                return Ok(*this);
            }
            let object = runtime.alloc_object();
            return Ok(RegisterValue::from_object_handle(object.0));
        }

        if let Some(boolean) = value.as_bool() {
            return box_boolean_object(RegisterValue::from_bool(boolean), runtime);
        }

        if let Some(number) = value.as_number() {
            return box_number_object(RegisterValue::from_number(number), runtime);
        }

        if let Some(handle) = value.as_object_handle().map(ObjectHandle) {
            return match runtime.objects().kind(handle) {
                Ok(HeapValueKind::String) => box_string_object(handle, runtime),
                Ok(_) => Ok(value),
                Err(error) => Err(VmNativeCallError::Internal(
                    format!("Object constructor kind lookup failed: {error:?}").into(),
                )),
            };
        }
    }

    if this.as_object_handle().is_some() {
        return Ok(*this);
    }

    let object = runtime.alloc_object();
    Ok(RegisterValue::from_object_handle(object.0))
}

fn object_value_of(
    this: &RegisterValue,
    _args: &[RegisterValue],
    _runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    Ok(*this)
}

fn object_to_string(
    this: &RegisterValue,
    _args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let tag = object_to_string_tag(*this, runtime)?;
    let string = runtime.alloc_string(format!("[object {tag}]"));
    Ok(RegisterValue::from_object_handle(string.0))
}

fn object_is_prototype_of(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let prototype = this
        .as_object_handle()
        .map(ObjectHandle)
        .ok_or_else(|| VmNativeCallError::Internal(OBJECT_IS_PROTOTYPE_OF_ERROR.into()))?;
    let Some(mut candidate) = args
        .first()
        .copied()
        .and_then(|value| value.as_object_handle().map(ObjectHandle))
    else {
        return Ok(RegisterValue::from_bool(false));
    };

    while let Some(current) = runtime
        .objects()
        .get_prototype(candidate)
        .map_err(|error| {
            VmNativeCallError::Internal(
                format!("isPrototypeOf prototype lookup failed: {error:?}").into(),
            )
        })?
    {
        if current == prototype {
            return Ok(RegisterValue::from_bool(true));
        }
        candidate = current;
    }

    Ok(RegisterValue::from_bool(false))
}

fn object_create(
    _this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let prototype = match args.first().copied() {
        None => Some(runtime.intrinsics().object_prototype()),
        Some(value) if value == RegisterValue::null() => None,
        Some(value) => value.as_object_handle().map(crate::object::ObjectHandle),
    };
    let object = runtime.alloc_object_with_prototype(prototype);
    Ok(RegisterValue::from_object_handle(object.0))
}

fn object_to_string_tag(
    value: RegisterValue,
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<&'static str, VmNativeCallError> {
    if value == RegisterValue::undefined() {
        return Ok("Undefined");
    }
    if value == RegisterValue::null() {
        return Ok("Null");
    }
    if value.as_bool().is_some() {
        return Ok("Boolean");
    }
    if value.as_number().is_some() {
        return Ok("Number");
    }

    let Some(handle) = value.as_object_handle().map(ObjectHandle) else {
        return Ok("Object");
    };

    if matches!(runtime.objects().kind(handle), Ok(HeapValueKind::String)) {
        return Ok("String");
    }
    if has_own_data_slot(handle, STRING_DATA_SLOT, runtime)? {
        return Ok("String");
    }
    if has_own_data_slot(handle, NUMBER_DATA_SLOT, runtime)? {
        return Ok("Number");
    }
    if has_own_data_slot(handle, BOOLEAN_DATA_SLOT, runtime)? {
        return Ok("Boolean");
    }

    match runtime.objects().kind(handle).map_err(|error| {
        VmNativeCallError::Internal(format!("toString kind lookup failed: {error:?}").into())
    })? {
        HeapValueKind::Array => Ok("Array"),
        HeapValueKind::HostFunction | HeapValueKind::Closure | HeapValueKind::BoundFunction => {
            Ok("Function")
        }
        HeapValueKind::Object
        | HeapValueKind::String
        | HeapValueKind::UpvalueCell
        | HeapValueKind::Iterator
        | HeapValueKind::Promise => Ok("Object"),
    }
}

// ---------------------------------------------------------------------------
// ES2024 §20.1.2.8  Object.getOwnPropertyDescriptor(O, P)
// ---------------------------------------------------------------------------
fn object_get_own_property_descriptor(
    _this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let target = args
        .first()
        .copied()
        .and_then(RegisterValue::as_object_handle)
        .map(ObjectHandle)
        .ok_or_else(|| {
            VmNativeCallError::Internal(
                "Object.getOwnPropertyDescriptor requires an object".into(),
            )
        })?;
    let key = args.get(1).copied().unwrap_or_else(RegisterValue::undefined);
    let property = runtime.property_name_from_value(key)?;

    if !runtime.objects().has_own_property(target, property).map_err(|e| {
        VmNativeCallError::Internal(format!("getOwnPropertyDescriptor: {e:?}").into())
    })? {
        return Ok(RegisterValue::undefined());
    }

    let lookup = runtime.objects().get_property(target, property).map_err(|e| {
        VmNativeCallError::Internal(format!("getOwnPropertyDescriptor lookup: {e:?}").into())
    })?;

    let Some(lookup) = lookup else {
        return Ok(RegisterValue::undefined());
    };

    // ES2024 §6.2.6.4 FromPropertyDescriptor — build descriptor object.
    from_property_descriptor(lookup.value(), runtime)
}

/// ES2024 §6.2.6.4 FromPropertyDescriptor(Desc)
fn from_property_descriptor(
    pv: PropertyValue,
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let desc = runtime.alloc_object_with_prototype(Some(runtime.intrinsics().object_prototype()));
    let attrs = pv.attributes();

    match pv {
        PropertyValue::Data { value, .. } => {
            let value_key = runtime.intern_property_name("value");
            runtime.objects_mut().set_property(desc, value_key, value).ok();

            let writable_key = runtime.intern_property_name("writable");
            runtime
                .objects_mut()
                .set_property(desc, writable_key, RegisterValue::from_bool(attrs.writable()))
                .ok();
        }
        PropertyValue::Accessor { getter, setter, .. } => {
            let get_key = runtime.intern_property_name("get");
            let get_val = getter
                .map(|h| RegisterValue::from_object_handle(h.0))
                .unwrap_or_else(RegisterValue::undefined);
            runtime.objects_mut().set_property(desc, get_key, get_val).ok();

            let set_key = runtime.intern_property_name("set");
            let set_val = setter
                .map(|h| RegisterValue::from_object_handle(h.0))
                .unwrap_or_else(RegisterValue::undefined);
            runtime.objects_mut().set_property(desc, set_key, set_val).ok();
        }
    }

    let enumerable_key = runtime.intern_property_name("enumerable");
    runtime
        .objects_mut()
        .set_property(desc, enumerable_key, RegisterValue::from_bool(attrs.enumerable()))
        .ok();

    let configurable_key = runtime.intern_property_name("configurable");
    runtime
        .objects_mut()
        .set_property(desc, configurable_key, RegisterValue::from_bool(attrs.configurable()))
        .ok();

    Ok(RegisterValue::from_object_handle(desc.0))
}

// ---------------------------------------------------------------------------
// ES2024 §20.1.2.4  Object.defineProperty(O, P, Attributes)
// ---------------------------------------------------------------------------
fn object_define_property(
    _this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let target = args
        .first()
        .copied()
        .and_then(RegisterValue::as_object_handle)
        .map(ObjectHandle)
        .ok_or_else(|| {
            VmNativeCallError::Internal("Object.defineProperty requires an object".into())
        })?;
    let key = args.get(1).copied().unwrap_or_else(RegisterValue::undefined);
    let property = runtime.property_name_from_value(key)?;
    let desc_obj = args
        .get(2)
        .copied()
        .and_then(RegisterValue::as_object_handle)
        .map(ObjectHandle);

    let desc = to_property_descriptor(desc_obj, runtime)?;
    runtime.objects_mut().define_own_property(target, property, desc).map_err(|e| {
        VmNativeCallError::Internal(format!("Object.defineProperty: {e:?}").into())
    })?;

    Ok(RegisterValue::from_object_handle(target.0))
}

/// ES2024 §6.2.6.5 ToPropertyDescriptor(Obj)
fn to_property_descriptor(
    desc_obj: Option<ObjectHandle>,
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<PropertyValue, VmNativeCallError> {
    let Some(obj) = desc_obj else {
        return Ok(PropertyValue::data(RegisterValue::undefined()));
    };

    // Check for accessor descriptor (get/set present).
    let get_key = runtime.intern_property_name("get");
    let getter = runtime
        .objects()
        .get_property(obj, get_key)
        .ok()
        .flatten()
        .and_then(|l| match l.value() {
            PropertyValue::Data { value, .. } if value != RegisterValue::undefined() => {
                value.as_object_handle().map(ObjectHandle)
            }
            _ => None,
        });

    let set_key = runtime.intern_property_name("set");
    let setter = runtime
        .objects()
        .get_property(obj, set_key)
        .ok()
        .flatten()
        .and_then(|l| match l.value() {
            PropertyValue::Data { value, .. } if value != RegisterValue::undefined() => {
                value.as_object_handle().map(ObjectHandle)
            }
            _ => None,
        });

    let enumerable = read_bool_attr(obj, "enumerable", true, runtime);
    let configurable = read_bool_attr(obj, "configurable", true, runtime);

    if getter.is_some() || setter.is_some() {
        return Ok(PropertyValue::Accessor {
            getter,
            setter,
            attributes: PropertyAttributes::from_flags(false, enumerable, configurable),
        });
    }

    // Data descriptor.
    let value_key = runtime.intern_property_name("value");
    let value = runtime
        .objects()
        .get_property(obj, value_key)
        .ok()
        .flatten()
        .map(|l| match l.value() {
            PropertyValue::Data { value, .. } => value,
            _ => RegisterValue::undefined(),
        })
        .unwrap_or_else(RegisterValue::undefined);

    let writable = read_bool_attr(obj, "writable", true, runtime);

    Ok(PropertyValue::Data {
        value,
        attributes: PropertyAttributes::from_flags(writable, enumerable, configurable),
    })
}

fn read_bool_attr(
    obj: ObjectHandle,
    name: &str,
    default: bool,
    runtime: &mut crate::interpreter::RuntimeState,
) -> bool {
    let key = runtime.intern_property_name(name);
    runtime
        .objects()
        .get_property(obj, key)
        .ok()
        .flatten()
        .map(|l| match l.value() {
            PropertyValue::Data { value, .. } => value.is_truthy(),
            _ => default,
        })
        .unwrap_or(default)
}

// ---------------------------------------------------------------------------
// ES2024 §20.1.2.17  Object.keys(O)
// ---------------------------------------------------------------------------
fn object_keys(
    _this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let target = args
        .first()
        .copied()
        .and_then(RegisterValue::as_object_handle)
        .map(ObjectHandle)
        .ok_or_else(|| VmNativeCallError::Internal("Object.keys requires an object".into()))?;

    let keys = runtime.objects().own_keys(target).map_err(|e| {
        VmNativeCallError::Internal(format!("Object.keys: {e:?}").into())
    })?;

    // Filter by enumerable.
    let array = runtime.alloc_array();
    for key_id in &keys {
        // Check if property is enumerable.
        if let Ok(Some(lookup)) = runtime.objects().get_property(target, *key_id) {
            if !lookup.value().attributes().enumerable() {
                continue;
            }
        }
        let name = runtime.property_names().get(*key_id).unwrap_or("").to_string();
        let str_handle = runtime.alloc_string(name);
        runtime
            .objects_mut()
            .push_element(array, RegisterValue::from_object_handle(str_handle.0))
            .ok();
    }

    Ok(RegisterValue::from_object_handle(array.0))
}

// ---------------------------------------------------------------------------
// ES2024 §20.1.2.22  Object.values(O)
// ---------------------------------------------------------------------------
fn object_values(
    _this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let target = args
        .first()
        .copied()
        .and_then(RegisterValue::as_object_handle)
        .map(ObjectHandle)
        .ok_or_else(|| VmNativeCallError::Internal("Object.values requires an object".into()))?;

    let keys = runtime.objects().own_keys(target).map_err(|e| {
        VmNativeCallError::Internal(format!("Object.values: {e:?}").into())
    })?;

    let array = runtime.alloc_array();
    for key_id in &keys {
        if let Ok(Some(lookup)) = runtime.objects().get_property(target, *key_id) {
            if !lookup.value().attributes().enumerable() {
                continue;
            }
            let value = match lookup.value() {
                PropertyValue::Data { value, .. } => value,
                _ => RegisterValue::undefined(),
            };
            runtime.objects_mut().push_element(array, value).ok();
        }
    }

    Ok(RegisterValue::from_object_handle(array.0))
}

// ---------------------------------------------------------------------------
// ES2024 §20.1.2.5  Object.entries(O)
// ---------------------------------------------------------------------------
fn object_entries(
    _this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let target = args
        .first()
        .copied()
        .and_then(RegisterValue::as_object_handle)
        .map(ObjectHandle)
        .ok_or_else(|| VmNativeCallError::Internal("Object.entries requires an object".into()))?;

    let keys = runtime.objects().own_keys(target).map_err(|e| {
        VmNativeCallError::Internal(format!("Object.entries: {e:?}").into())
    })?;

    let result = runtime.alloc_array();
    for key_id in &keys {
        if let Ok(Some(lookup)) = runtime.objects().get_property(target, *key_id) {
            if !lookup.value().attributes().enumerable() {
                continue;
            }
            let value = match lookup.value() {
                PropertyValue::Data { value, .. } => value,
                _ => RegisterValue::undefined(),
            };
            let name = runtime.property_names().get(*key_id).unwrap_or("").to_string();
            let key_str = runtime.alloc_string(name);
            let pair = runtime.alloc_array();
            runtime
                .objects_mut()
                .push_element(pair, RegisterValue::from_object_handle(key_str.0))
                .ok();
            runtime.objects_mut().push_element(pair, value).ok();
            runtime
                .objects_mut()
                .push_element(result, RegisterValue::from_object_handle(pair.0))
                .ok();
        }
    }

    Ok(RegisterValue::from_object_handle(result.0))
}

// ---------------------------------------------------------------------------
// ES2024 §20.1.2.6  Object.freeze(O)
// ---------------------------------------------------------------------------
fn object_freeze(
    _this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let target = args.first().copied().unwrap_or_else(RegisterValue::undefined);
    // Non-object arguments are returned as-is per spec.
    let Some(handle) = target.as_object_handle().map(ObjectHandle) else {
        return Ok(target);
    };
    runtime.objects_mut().freeze(handle).map_err(|e| {
        VmNativeCallError::Internal(format!("Object.freeze: {e:?}").into())
    })?;
    Ok(target)
}

// ---------------------------------------------------------------------------
// ES2024 §20.1.2.19  Object.seal(O)
// ---------------------------------------------------------------------------
fn object_seal(
    _this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let target = args.first().copied().unwrap_or_else(RegisterValue::undefined);
    let Some(handle) = target.as_object_handle().map(ObjectHandle) else {
        return Ok(target);
    };
    runtime.objects_mut().seal(handle).map_err(|e| {
        VmNativeCallError::Internal(format!("Object.seal: {e:?}").into())
    })?;
    Ok(target)
}

// ---------------------------------------------------------------------------
// ES2024 §20.1.2.14  Object.getPrototypeOf(O)
// ---------------------------------------------------------------------------
fn object_get_prototype_of(
    _this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let target = args
        .first()
        .copied()
        .and_then(RegisterValue::as_object_handle)
        .map(ObjectHandle)
        .ok_or_else(|| {
            VmNativeCallError::Internal("Object.getPrototypeOf requires an object".into())
        })?;
    let proto = runtime.objects().get_prototype(target).map_err(|e| {
        VmNativeCallError::Internal(format!("Object.getPrototypeOf: {e:?}").into())
    })?;
    Ok(proto
        .map(|h| RegisterValue::from_object_handle(h.0))
        .unwrap_or_else(RegisterValue::null))
}

// ---------------------------------------------------------------------------
// ES2024 §20.1.2.20  Object.setPrototypeOf(O, proto)
// ---------------------------------------------------------------------------
fn object_set_prototype_of(
    _this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let target = args
        .first()
        .copied()
        .and_then(RegisterValue::as_object_handle)
        .map(ObjectHandle)
        .ok_or_else(|| {
            VmNativeCallError::Internal("Object.setPrototypeOf requires an object".into())
        })?;
    let proto_arg = args.get(1).copied().unwrap_or_else(RegisterValue::undefined);
    let proto = if proto_arg == RegisterValue::null() {
        None
    } else {
        Some(
            proto_arg
                .as_object_handle()
                .map(ObjectHandle)
                .ok_or_else(|| {
                    VmNativeCallError::Internal(
                        "Object.setPrototypeOf proto must be object or null".into(),
                    )
                })?,
        )
    };
    runtime.objects_mut().set_prototype(target, proto).map_err(|e| {
        VmNativeCallError::Internal(format!("Object.setPrototypeOf: {e:?}").into())
    })?;
    Ok(RegisterValue::from_object_handle(target.0))
}

// ---------------------------------------------------------------------------
// ES2024 §20.1.2.1  Object.assign(target, ...sources)
// ---------------------------------------------------------------------------
fn object_assign(
    _this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let target = args
        .first()
        .copied()
        .and_then(RegisterValue::as_object_handle)
        .map(ObjectHandle)
        .ok_or_else(|| VmNativeCallError::Internal("Object.assign requires an object".into()))?;

    for source_arg in args.iter().skip(1) {
        let Some(source) = source_arg.as_object_handle().map(ObjectHandle) else {
            continue; // null/undefined sources are skipped
        };
        let keys = runtime.objects().own_keys(source).unwrap_or_default();
        for key_id in keys {
            if let Ok(Some(lookup)) = runtime.objects().get_property(source, key_id) {
                if !lookup.value().attributes().enumerable() {
                    continue;
                }
                let value = match lookup.value() {
                    PropertyValue::Data { value, .. } => value,
                    _ => continue,
                };
                runtime.objects_mut().set_property(target, key_id, value).ok();
            }
        }
    }

    Ok(RegisterValue::from_object_handle(target.0))
}

// ---------------------------------------------------------------------------
// ES2024 §20.1.2.11  Object.getOwnPropertyNames(O)
// ---------------------------------------------------------------------------
fn object_get_own_property_names(
    _this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let target = args
        .first()
        .copied()
        .and_then(RegisterValue::as_object_handle)
        .map(ObjectHandle)
        .ok_or_else(|| {
            VmNativeCallError::Internal("Object.getOwnPropertyNames requires an object".into())
        })?;

    let keys = runtime.objects().own_keys(target).map_err(|e| {
        VmNativeCallError::Internal(format!("Object.getOwnPropertyNames: {e:?}").into())
    })?;

    // All own string-keyed properties (no enumerable filter).
    let key_names: Vec<String> = keys
        .iter()
        .map(|k| runtime.property_names().get(*k).unwrap_or("").to_string())
        .collect();

    let array = runtime.alloc_array();
    for name in &key_names {
        let str_handle = runtime.alloc_string(name.as_str());
        runtime
            .objects_mut()
            .push_element(array, RegisterValue::from_object_handle(str_handle.0))
            .ok();
    }

    Ok(RegisterValue::from_object_handle(array.0))
}

// ---------------------------------------------------------------------------
// ES2024 §20.1.3.4  Object.prototype.propertyIsEnumerable(V)
// ---------------------------------------------------------------------------
fn object_property_is_enumerable(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let target = this
        .as_object_handle()
        .map(ObjectHandle)
        .ok_or_else(|| {
            VmNativeCallError::Internal("propertyIsEnumerable requires object receiver".into())
        })?;
    let key = args.first().copied().unwrap_or_else(RegisterValue::undefined);
    let property = runtime.property_name_from_value(key)?;

    let has_own = runtime.objects().has_own_property(target, property).unwrap_or(false);
    if !has_own {
        return Ok(RegisterValue::from_bool(false));
    }

    // Check enumerable attribute on own property.
    if let Ok(Some(lookup)) = runtime.objects().get_property(target, property) {
        if lookup.owner() == target {
            return Ok(RegisterValue::from_bool(lookup.value().attributes().enumerable()));
        }
    }

    Ok(RegisterValue::from_bool(false))
}

// ---------------------------------------------------------------------------
// ES2024 §20.1.3.2  Object.prototype.hasOwnProperty(V)
// ---------------------------------------------------------------------------
fn object_has_own_property(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let target = this
        .as_object_handle()
        .map(ObjectHandle)
        .ok_or_else(|| {
            VmNativeCallError::Internal("hasOwnProperty requires an object receiver".into())
        })?;
    let key = args.first().copied().unwrap_or_else(RegisterValue::undefined);
    let property = runtime.property_name_from_value(key)?;
    let has = runtime.objects().has_own_property(target, property).map_err(|e| {
        VmNativeCallError::Internal(format!("hasOwnProperty: {e:?}").into())
    })?;
    Ok(RegisterValue::from_bool(has))
}

fn has_own_data_slot(
    handle: ObjectHandle,
    slot_name: &str,
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<bool, VmNativeCallError> {
    let backing = runtime.intern_property_name(slot_name);
    let Some(lookup) = runtime
        .objects()
        .get_property(handle, backing)
        .map_err(|error| {
            VmNativeCallError::Internal(format!("data slot lookup failed: {error:?}").into())
        })?
    else {
        return Ok(false);
    };

    if lookup.owner() != handle {
        return Ok(false);
    }

    Ok(matches!(lookup.value(), PropertyValue::Data { .. }))
}
