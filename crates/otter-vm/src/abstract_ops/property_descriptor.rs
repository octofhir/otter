use crate::descriptors::VmNativeCallError;
use crate::interpreter::RuntimeState;
use crate::object::{HeapValueKind, ObjectHandle, PropertyDescriptor, PropertyValue};
use crate::property::PropertyNameId;
use crate::value::RegisterValue;

fn type_error(
    runtime: &mut RuntimeState,
    message: &str,
) -> Result<VmNativeCallError, VmNativeCallError> {
    let error = runtime.alloc_type_error(message).map_err(|error| {
        VmNativeCallError::Internal(format!("TypeError allocation failed: {error}").into())
    })?;
    Ok(VmNativeCallError::Thrown(
        RegisterValue::from_object_handle(error.0),
    ))
}

/// ES2024 §6.2.6.4 FromPropertyDescriptor(Desc)
pub(crate) fn from_property_descriptor(
    pv: PropertyValue,
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let desc = runtime.alloc_object_with_prototype(Some(runtime.intrinsics().object_prototype()));
    let attrs = pv.attributes();

    match pv {
        PropertyValue::Data { value, .. } => {
            define_descriptor_field(runtime, desc, "value", value)?;
            define_descriptor_field(
                runtime,
                desc,
                "writable",
                RegisterValue::from_bool(attrs.writable()),
            )?;
        }
        PropertyValue::Accessor { getter, setter, .. } => {
            define_descriptor_field(
                runtime,
                desc,
                "get",
                getter
                    .map(|handle| RegisterValue::from_object_handle(handle.0))
                    .unwrap_or_else(RegisterValue::undefined),
            )?;
            define_descriptor_field(
                runtime,
                desc,
                "set",
                setter
                    .map(|handle| RegisterValue::from_object_handle(handle.0))
                    .unwrap_or_else(RegisterValue::undefined),
            )?;
        }
    }

    define_descriptor_field(
        runtime,
        desc,
        "enumerable",
        RegisterValue::from_bool(attrs.enumerable()),
    )?;
    define_descriptor_field(
        runtime,
        desc,
        "configurable",
        RegisterValue::from_bool(attrs.configurable()),
    )?;

    Ok(RegisterValue::from_object_handle(desc.0))
}

/// ES2024 §6.2.6.5 ToPropertyDescriptor(Obj)
pub(crate) fn to_property_descriptor(
    desc_obj: Option<ObjectHandle>,
    runtime: &mut RuntimeState,
) -> Result<PropertyDescriptor, VmNativeCallError> {
    let Some(obj) = desc_obj else {
        return Err(type_error(
            runtime,
            "property descriptor must be an object",
        )?);
    };
    if matches!(runtime.objects().kind(obj), Ok(HeapValueKind::String)) {
        return Err(type_error(
            runtime,
            "property descriptor must be an object",
        )?);
    }

    let value = get_descriptor_data_field(runtime, obj, "value")?;
    let writable = get_descriptor_bool_field(runtime, obj, "writable", false)?;
    let enumerable = get_descriptor_bool_field(runtime, obj, "enumerable", false)?;
    let configurable = get_descriptor_bool_field(runtime, obj, "configurable", false)?;

    let (has_get, getter) = get_descriptor_callable_field(runtime, obj, "get")?;
    let (has_set, setter) = get_descriptor_callable_field(runtime, obj, "set")?;

    let is_accessor_descriptor = has_get || has_set;
    let is_data_descriptor = value.is_some() || has_own_descriptor_field(runtime, obj, "writable")?;

    if is_accessor_descriptor && is_data_descriptor {
        return Err(type_error(
            runtime,
            "property descriptor cannot mix accessor and data fields",
        )?);
    }

    if is_accessor_descriptor {
        return Ok(PropertyDescriptor::accessor(
            has_get.then_some(getter),
            has_set.then_some(setter),
            has_own_descriptor_field(runtime, obj, "enumerable")?.then_some(enumerable),
            has_own_descriptor_field(runtime, obj, "configurable")?.then_some(configurable),
        ));
    }

    if is_data_descriptor {
        return Ok(PropertyDescriptor::data(
            value,
            has_own_descriptor_field(runtime, obj, "writable")?.then_some(writable),
            has_own_descriptor_field(runtime, obj, "enumerable")?.then_some(enumerable),
            has_own_descriptor_field(runtime, obj, "configurable")?.then_some(configurable),
        ));
    }

    Ok(PropertyDescriptor::generic(
        has_own_descriptor_field(runtime, obj, "enumerable")?.then_some(enumerable),
        has_own_descriptor_field(runtime, obj, "configurable")?.then_some(configurable),
    ))
}

pub(crate) fn collect_define_properties(
    properties: ObjectHandle,
    runtime: &mut RuntimeState,
) -> Result<Vec<(PropertyNameId, PropertyDescriptor)>, VmNativeCallError> {
    let keys = runtime
        .enumerable_own_property_keys(properties)
        .map_err(|error| {
            VmNativeCallError::Internal(
                format!("defineProperties key collection failed: {error}").into(),
            )
        })?;

    let mut descriptors = Vec::new();
    for key in keys {
        let descriptor_value = runtime.own_property_value(properties, key)?;
        let descriptor_object = descriptor_value.as_object_handle().map(ObjectHandle);
        let descriptor = to_property_descriptor(descriptor_object, runtime)?;
        descriptors.push((key, descriptor));
    }

    Ok(descriptors)
}

fn define_descriptor_field(
    runtime: &mut RuntimeState,
    target: ObjectHandle,
    name: &str,
    value: RegisterValue,
) -> Result<(), VmNativeCallError> {
    let key = runtime.intern_property_name(name);
    runtime
        .objects_mut()
        .set_property(target, key, value)
        .map(|_| ())
        .map_err(|error| {
            VmNativeCallError::Internal(
                format!("descriptor field '{name}' store failed: {error:?}").into(),
            )
        })
}

fn has_own_descriptor_field(
    runtime: &mut RuntimeState,
    obj: ObjectHandle,
    name: &str,
) -> Result<bool, VmNativeCallError> {
    let key = runtime.intern_property_name(name);
    runtime
        .objects()
        .has_own_property(obj, key)
        .map_err(|error| {
            VmNativeCallError::Internal(
                format!("descriptor field '{name}' lookup failed: {error:?}").into(),
            )
        })
}

fn get_descriptor_data_field(
    runtime: &mut RuntimeState,
    obj: ObjectHandle,
    name: &str,
) -> Result<Option<RegisterValue>, VmNativeCallError> {
    let key = runtime.intern_property_name(name);
    Ok(runtime
        .property_lookup(obj, key)
        .map_err(|error| {
            VmNativeCallError::Internal(
                format!("descriptor field '{name}' get failed: {error:?}").into(),
            )
        })?
        .and_then(|lookup| match lookup.value() {
            PropertyValue::Data { value, .. } => Some(value),
            PropertyValue::Accessor { .. } => None,
        }))
}

fn get_descriptor_bool_field(
    runtime: &mut RuntimeState,
    obj: ObjectHandle,
    name: &str,
    default: bool,
) -> Result<bool, VmNativeCallError> {
    let Some(value) = get_descriptor_data_field(runtime, obj, name)? else {
        return Ok(default);
    };

    runtime.js_to_boolean(value).map_err(|error| {
        VmNativeCallError::Internal(
            format!("descriptor boolean field '{name}' coercion failed: {error}").into(),
        )
    })
}

fn get_descriptor_callable_field(
    runtime: &mut RuntimeState,
    obj: ObjectHandle,
    name: &str,
) -> Result<(bool, Option<ObjectHandle>), VmNativeCallError> {
    let present = has_own_descriptor_field(runtime, obj, name)?;
    let Some(value) = get_descriptor_data_field(runtime, obj, name)? else {
        return Ok((present, None));
    };

    if value == RegisterValue::undefined() {
        return Ok((present, None));
    }

    let handle = value.as_object_handle().map(ObjectHandle).ok_or_else(|| {
        type_error(
            runtime,
            &format!("descriptor field '{name}' must be callable or undefined"),
        )
        .unwrap_or_else(|error| error)
    })?;

    if !runtime.objects().is_callable(handle) {
        return Err(type_error(
            runtime,
            &format!("descriptor field '{name}' must be callable or undefined"),
        )
        .unwrap_or_else(|error| error));
    }

    Ok((present, Some(handle)))
}
