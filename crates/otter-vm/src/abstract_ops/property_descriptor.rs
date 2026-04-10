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

    // §6.2.6.5 ToPropertyDescriptor uses HasProperty (with proto chain),
    // not HasOwnProperty. Cache presence flags so we don't call into the
    // VM twice per field.
    let has_value = has_property_inherited(runtime, obj, "value")?;
    let value = if has_value {
        let key = runtime.intern_property_name("value");
        Some(runtime.ordinary_get(obj, key, RegisterValue::from_object_handle(obj.0))?)
    } else {
        None
    };
    let has_writable = has_property_inherited(runtime, obj, "writable")?;
    let writable = if has_writable {
        get_descriptor_bool_field(runtime, obj, "writable", false)?
    } else {
        false
    };
    let has_enumerable = has_property_inherited(runtime, obj, "enumerable")?;
    let enumerable = if has_enumerable {
        get_descriptor_bool_field(runtime, obj, "enumerable", false)?
    } else {
        false
    };
    let has_configurable = has_property_inherited(runtime, obj, "configurable")?;
    let configurable = if has_configurable {
        get_descriptor_bool_field(runtime, obj, "configurable", false)?
    } else {
        false
    };

    let (has_get, getter) = get_descriptor_callable_field(runtime, obj, "get")?;
    let (has_set, setter) = get_descriptor_callable_field(runtime, obj, "set")?;

    let is_accessor_descriptor = has_get || has_set;
    let is_data_descriptor = has_value || has_writable;

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
            has_enumerable.then_some(enumerable),
            has_configurable.then_some(configurable),
        ));
    }

    if is_data_descriptor {
        return Ok(PropertyDescriptor::data(
            value,
            has_writable.then_some(writable),
            has_enumerable.then_some(enumerable),
            has_configurable.then_some(configurable),
        ));
    }

    Ok(PropertyDescriptor::generic(
        has_enumerable.then_some(enumerable),
        has_configurable.then_some(configurable),
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

/// §6.2.6.5 ToPropertyDescriptor step 4-13: each `Has(Obj, name)` /
/// `Get(Obj, name)` pair must use the regular `[[HasProperty]]` /
/// `[[Get]]` paths so that:
///   1. Properties inherited from the prototype chain are visible.
///   2. Accessor descriptors are *invoked* (the previous implementation
///      treated an accessor-typed `value` field as "missing", which
///      broke `Object.defineProperties(o, { p: descObjWithGetterValue })`
///      style tests in test262).
///
/// `Get` invokes the accessor; the property's "presence" is then a
/// separate `HasProperty` check via `has_property_inherited`. Returns
/// `None` only when the property is genuinely absent — not when its
/// value is undefined or the slot is an accessor.
fn get_descriptor_data_field(
    runtime: &mut RuntimeState,
    obj: ObjectHandle,
    name: &str,
) -> Result<Option<RegisterValue>, VmNativeCallError> {
    if !has_property_inherited(runtime, obj, name)? {
        return Ok(None);
    }
    let key = runtime.intern_property_name(name);
    let value = runtime.ordinary_get(obj, key, RegisterValue::from_object_handle(obj.0))?;
    Ok(Some(value))
}

/// §7.3.11 HasProperty(O, P) — walks the [[Prototype]] chain.
///
/// `has_own_property` only checks the receiver's own slots, but
/// ToPropertyDescriptor needs to see inherited fields too: tests in
/// `built-ins/Object/defineProperties` create a descriptor object via
/// `Object.create(protoWithGetters)` and rely on the inherited
/// `enumerable`/`configurable`/`value` accessors firing.
fn has_property_inherited(
    runtime: &mut RuntimeState,
    obj: ObjectHandle,
    name: &str,
) -> Result<bool, VmNativeCallError> {
    let key = runtime.intern_property_name(name);
    runtime
        .property_lookup(obj, key)
        .map(|lookup| lookup.is_some())
        .map_err(|error| {
            VmNativeCallError::Internal(
                format!("descriptor field '{name}' presence check failed: {error:?}").into(),
            )
        })
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
    // §6.2.6.5 step 8/10: HasProperty (with proto chain) for the
    // accessor field, then Get if present.
    let present = has_property_inherited(runtime, obj, name)?;
    if !present {
        return Ok((false, None));
    }
    let key = runtime.intern_property_name(name);
    let value = runtime.ordinary_get(obj, key, RegisterValue::from_object_handle(obj.0))?;

    if value == RegisterValue::undefined() {
        return Ok((true, None));
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

    Ok((true, Some(handle)))
}
