use crate::builders::ClassBuilder;
use crate::descriptors::{
    JsClassDescriptor, NativeBindingDescriptor, NativeBindingTarget, NativeFunctionDescriptor,
    VmNativeCallError,
};
use crate::object::{HeapValueKind, ObjectHandle, PropertyValue};
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
            NativeFunctionDescriptor::method(
                "getOwnPropertyDescriptor",
                2,
                object_get_own_property_descriptor,
            ),
        ))
        .with_binding(NativeBindingDescriptor::new(
            NativeBindingTarget::Constructor,
            NativeFunctionDescriptor::method(
                "getOwnPropertyDescriptors",
                1,
                object_get_own_property_descriptors,
            ),
        ))
        .with_binding(NativeBindingDescriptor::new(
            NativeBindingTarget::Constructor,
            NativeFunctionDescriptor::method("defineProperty", 3, object_define_property),
        ))
        .with_binding(NativeBindingDescriptor::new(
            NativeBindingTarget::Constructor,
            NativeFunctionDescriptor::method("defineProperties", 2, object_define_properties),
        ))
        .with_binding(NativeBindingDescriptor::new(
            NativeBindingTarget::Constructor,
            NativeFunctionDescriptor::method("is", 2, object_is),
        ))
        .with_binding(NativeBindingDescriptor::new(
            NativeBindingTarget::Constructor,
            NativeFunctionDescriptor::method("hasOwn", 2, object_has_own),
        ))
        .with_binding(NativeBindingDescriptor::new(
            NativeBindingTarget::Constructor,
            NativeFunctionDescriptor::method(
                "getOwnPropertyNames",
                1,
                object_get_own_property_names,
            ),
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
            NativeFunctionDescriptor::method("isFrozen", 1, object_is_frozen),
        ))
        .with_binding(NativeBindingDescriptor::new(
            NativeBindingTarget::Constructor,
            NativeFunctionDescriptor::method("preventExtensions", 1, object_prevent_extensions),
        ))
        .with_binding(NativeBindingDescriptor::new(
            NativeBindingTarget::Constructor,
            NativeFunctionDescriptor::method("seal", 1, object_seal),
        ))
        .with_binding(NativeBindingDescriptor::new(
            NativeBindingTarget::Constructor,
            NativeFunctionDescriptor::method("isSealed", 1, object_is_sealed),
        ))
        .with_binding(NativeBindingDescriptor::new(
            NativeBindingTarget::Constructor,
            NativeFunctionDescriptor::method("isExtensible", 1, object_is_extensible),
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
            NativeFunctionDescriptor::method(
                "propertyIsEnumerable",
                1,
                object_property_is_enumerable,
            ),
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

fn to_object_for_introspection(
    value: RegisterValue,
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<ObjectHandle, VmNativeCallError> {
    if value == RegisterValue::undefined() || value == RegisterValue::null() {
        return Err(VmNativeCallError::Thrown(RegisterValue::undefined()));
    }

    if let Some(boolean) = value.as_bool() {
        let object = box_boolean_object(RegisterValue::from_bool(boolean), runtime)?;
        return Ok(ObjectHandle(
            object
                .as_object_handle()
                .expect("boxed boolean should return an object"),
        ));
    }

    if let Some(number) = value.as_number() {
        let object = box_number_object(RegisterValue::from_number(number), runtime)?;
        return Ok(ObjectHandle(
            object
                .as_object_handle()
                .expect("boxed number should return an object"),
        ));
    }

    value
        .as_object_handle()
        .map(ObjectHandle)
        .ok_or_else(|| VmNativeCallError::Thrown(RegisterValue::undefined()))
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
    let target = to_object_for_introspection(
        args.first()
            .copied()
            .unwrap_or_else(RegisterValue::undefined),
        runtime,
    )?;
    let key = args
        .get(1)
        .copied()
        .unwrap_or_else(RegisterValue::undefined);
    let property = runtime.property_name_from_value(key)?;

    let Some(descriptor) = runtime
        .own_property_descriptor(target, property)
        .map_err(|e| {
            VmNativeCallError::Internal(format!("getOwnPropertyDescriptor: {e:?}").into())
        })?
    else {
        return Ok(RegisterValue::undefined());
    };

    crate::abstract_ops::from_property_descriptor(descriptor, runtime)
}

// ---------------------------------------------------------------------------
// ES2024 §20.1.2.9  Object.getOwnPropertyDescriptors(O)
// ---------------------------------------------------------------------------
fn object_get_own_property_descriptors(
    _this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let target = to_object_for_introspection(
        args.first()
            .copied()
            .unwrap_or_else(RegisterValue::undefined),
        runtime,
    )?;

    let keys = runtime.own_property_keys(target).map_err(|e| {
        VmNativeCallError::Internal(format!("Object.getOwnPropertyDescriptors: {e:?}").into())
    })?;
    let result = runtime.alloc_object();

    for key in keys {
        let Some(descriptor) = runtime.own_property_descriptor(target, key).map_err(|e| {
            VmNativeCallError::Internal(
                format!("Object.getOwnPropertyDescriptors descriptor: {e:?}").into(),
            )
        })?
        else {
            continue;
        };
        let descriptor_object = crate::abstract_ops::from_property_descriptor(descriptor, runtime)?;
        runtime
            .objects_mut()
            .set_property(result, key, descriptor_object)
            .map_err(|e| {
                VmNativeCallError::Internal(
                    format!("Object.getOwnPropertyDescriptors result store: {e:?}").into(),
                )
            })?;
    }

    Ok(RegisterValue::from_object_handle(result.0))
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
    let key = args
        .get(1)
        .copied()
        .unwrap_or_else(RegisterValue::undefined);
    let property = runtime.property_name_from_value(key)?;
    let desc_obj = args
        .get(2)
        .copied()
        .and_then(RegisterValue::as_object_handle)
        .map(ObjectHandle);

    let desc = crate::abstract_ops::to_property_descriptor(desc_obj, runtime)?;
    let property_names = runtime.property_names().clone();
    let success = runtime
        .objects_mut()
        .define_own_property_from_descriptor_with_registry(target, property, desc, &property_names)
        .map_err(|e| VmNativeCallError::Internal(format!("Object.defineProperty: {e:?}").into()))?;

    if !success {
        return Err(VmNativeCallError::Thrown(RegisterValue::undefined()));
    }

    Ok(RegisterValue::from_object_handle(target.0))
}

// ES2024 §20.1.2.3  Object.defineProperties(O, Properties)
// ---------------------------------------------------------------------------
fn object_define_properties(
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
            VmNativeCallError::Internal("Object.defineProperties requires an object".into())
        })?;
    let properties_obj = args
        .get(1)
        .copied()
        .and_then(RegisterValue::as_object_handle)
        .map(ObjectHandle);

    let descriptors = crate::abstract_ops::collect_define_properties(properties_obj, runtime)?;
    let property_names = runtime.property_names().clone();
    for (property, descriptor) in descriptors {
        let success = runtime
            .objects_mut()
            .define_own_property_from_descriptor_with_registry(
                target,
                property,
                descriptor,
                &property_names,
            )
            .map_err(|e| {
                VmNativeCallError::Internal(format!("Object.defineProperties: {e:?}").into())
            })?;
        if !success {
            return Err(VmNativeCallError::Thrown(RegisterValue::undefined()));
        }
    }

    Ok(RegisterValue::from_object_handle(target.0))
}

/// ES2024 §20.1.2.14 Object.is(value1, value2)
fn object_is(
    _this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let lhs = args
        .first()
        .copied()
        .unwrap_or_else(RegisterValue::undefined);
    let rhs = args
        .get(1)
        .copied()
        .unwrap_or_else(RegisterValue::undefined);
    let same = runtime
        .objects()
        .same_value(lhs, rhs)
        .map_err(|error| VmNativeCallError::Internal(format!("Object.is: {error:?}").into()))?;
    Ok(RegisterValue::from_bool(same))
}

fn object_has_own(
    _this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let target = to_object_for_introspection(
        args.first()
            .copied()
            .unwrap_or_else(RegisterValue::undefined),
        runtime,
    )?;
    let property = runtime.property_name_from_value(
        args.get(1)
            .copied()
            .unwrap_or_else(RegisterValue::undefined),
    )?;
    let has = runtime
        .own_property_descriptor(target, property)
        .map_err(|error| VmNativeCallError::Internal(format!("Object.hasOwn: {error:?}").into()))?
        .is_some();
    Ok(RegisterValue::from_bool(has))
}

// ---------------------------------------------------------------------------
// ES2024 §20.1.2.17  Object.keys(O)
// ---------------------------------------------------------------------------
fn object_keys(
    _this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let target = to_object_for_introspection(
        args.first()
            .copied()
            .unwrap_or_else(RegisterValue::undefined),
        runtime,
    )?;

    let keys = runtime
        .own_property_keys(target)
        .map_err(|e| VmNativeCallError::Internal(format!("Object.keys: {e:?}").into()))?;

    let array = runtime.alloc_array();
    for key_id in &keys {
        let Some(descriptor) = runtime
            .own_property_descriptor(target, *key_id)
            .map_err(|e| {
                VmNativeCallError::Internal(format!("Object.keys descriptor: {e:?}").into())
            })?
        else {
            continue;
        };
        if !descriptor.attributes().enumerable() {
            continue;
        }
        let name = runtime
            .property_names()
            .get(*key_id)
            .unwrap_or("")
            .to_string();
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
    let target = to_object_for_introspection(
        args.first()
            .copied()
            .unwrap_or_else(RegisterValue::undefined),
        runtime,
    )?;

    let keys = runtime
        .own_property_keys(target)
        .map_err(|e| VmNativeCallError::Internal(format!("Object.values: {e:?}").into()))?;

    let array = runtime.alloc_array();
    for key_id in &keys {
        let Some(descriptor) = runtime
            .own_property_descriptor(target, *key_id)
            .map_err(|e| {
                VmNativeCallError::Internal(format!("Object.values descriptor: {e:?}").into())
            })?
        else {
            continue;
        };
        if !descriptor.attributes().enumerable() {
            continue;
        }
        let value = match descriptor {
            PropertyValue::Data { value, .. } => value,
            PropertyValue::Accessor { .. } => RegisterValue::undefined(),
        };
        runtime.objects_mut().push_element(array, value).ok();
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
    let target = to_object_for_introspection(
        args.first()
            .copied()
            .unwrap_or_else(RegisterValue::undefined),
        runtime,
    )?;

    let keys = runtime
        .own_property_keys(target)
        .map_err(|e| VmNativeCallError::Internal(format!("Object.entries: {e:?}").into()))?;

    let result = runtime.alloc_array();
    for key_id in &keys {
        let Some(descriptor) = runtime
            .own_property_descriptor(target, *key_id)
            .map_err(|e| {
                VmNativeCallError::Internal(format!("Object.entries descriptor: {e:?}").into())
            })?
        else {
            continue;
        };
        if !descriptor.attributes().enumerable() {
            continue;
        }
        let value = match descriptor {
            PropertyValue::Data { value, .. } => value,
            PropertyValue::Accessor { .. } => RegisterValue::undefined(),
        };
        let name = runtime
            .property_names()
            .get(*key_id)
            .unwrap_or("")
            .to_string();
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
    let target = args
        .first()
        .copied()
        .unwrap_or_else(RegisterValue::undefined);
    // Non-object arguments are returned as-is per spec.
    let Some(handle) = target.as_object_handle().map(ObjectHandle) else {
        return Ok(target);
    };
    runtime
        .objects_mut()
        .freeze(handle)
        .map_err(|e| VmNativeCallError::Internal(format!("Object.freeze: {e:?}").into()))?;
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
    let target = args
        .first()
        .copied()
        .unwrap_or_else(RegisterValue::undefined);
    let Some(handle) = target.as_object_handle().map(ObjectHandle) else {
        return Ok(target);
    };
    runtime
        .objects_mut()
        .seal(handle)
        .map_err(|e| VmNativeCallError::Internal(format!("Object.seal: {e:?}").into()))?;
    Ok(target)
}

// ---------------------------------------------------------------------------
// ES2024 §20.1.2.17 Object.preventExtensions(O)
// ---------------------------------------------------------------------------
fn object_prevent_extensions(
    _this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let target = args
        .first()
        .copied()
        .unwrap_or_else(RegisterValue::undefined);
    let Some(handle) = target.as_object_handle().map(ObjectHandle) else {
        return Ok(target);
    };
    runtime
        .objects_mut()
        .prevent_extensions(handle)
        .map_err(|e| {
            VmNativeCallError::Internal(format!("Object.preventExtensions: {e:?}").into())
        })?;
    Ok(target)
}

// ---------------------------------------------------------------------------
// ES2024 §20.1.2.15 Object.isExtensible(O)
// ---------------------------------------------------------------------------
fn object_is_extensible(
    _this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let target = args
        .first()
        .copied()
        .unwrap_or_else(RegisterValue::undefined);
    let Some(handle) = target.as_object_handle().map(ObjectHandle) else {
        return Ok(RegisterValue::from_bool(false));
    };
    let extensible = runtime
        .objects()
        .is_extensible(handle)
        .map_err(|e| VmNativeCallError::Internal(format!("Object.isExtensible: {e:?}").into()))?;
    Ok(RegisterValue::from_bool(extensible))
}

// ---------------------------------------------------------------------------
// ES2024 §20.1.2.x Object.isFrozen(O)
// ---------------------------------------------------------------------------
fn object_is_frozen(
    _this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let target = args
        .first()
        .copied()
        .unwrap_or_else(RegisterValue::undefined);
    let Some(handle) = target.as_object_handle().map(ObjectHandle) else {
        return Ok(RegisterValue::from_bool(true));
    };
    let frozen = runtime
        .objects()
        .is_frozen(handle)
        .map_err(|e| VmNativeCallError::Internal(format!("Object.isFrozen: {e:?}").into()))?;
    Ok(RegisterValue::from_bool(frozen))
}

// ---------------------------------------------------------------------------
// ES2024 §20.1.2.x Object.isSealed(O)
// ---------------------------------------------------------------------------
fn object_is_sealed(
    _this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let target = args
        .first()
        .copied()
        .unwrap_or_else(RegisterValue::undefined);
    let Some(handle) = target.as_object_handle().map(ObjectHandle) else {
        return Ok(RegisterValue::from_bool(true));
    };
    let sealed = runtime
        .objects()
        .is_sealed(handle)
        .map_err(|e| VmNativeCallError::Internal(format!("Object.isSealed: {e:?}").into()))?;
    Ok(RegisterValue::from_bool(sealed))
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
    let proto = runtime
        .objects()
        .get_prototype(target)
        .map_err(|e| VmNativeCallError::Internal(format!("Object.getPrototypeOf: {e:?}").into()))?;
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
    let proto_arg = args
        .get(1)
        .copied()
        .unwrap_or_else(RegisterValue::undefined);
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
    runtime
        .objects_mut()
        .set_prototype(target, proto)
        .map_err(|e| VmNativeCallError::Internal(format!("Object.setPrototypeOf: {e:?}").into()))?
        .then_some(())
        .ok_or_else(|| VmNativeCallError::Thrown(RegisterValue::undefined()))?;
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
        let keys = runtime.own_property_keys(source).unwrap_or_default();
        for key_id in keys {
            let Ok(Some(descriptor)) = runtime.own_property_descriptor(source, key_id) else {
                continue;
            };
            if !descriptor.attributes().enumerable() {
                continue;
            }
            let PropertyValue::Data { value, .. } = descriptor else {
                continue;
            };
            runtime
                .objects_mut()
                .set_property(target, key_id, value)
                .ok();
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
    let target = to_object_for_introspection(
        args.first()
            .copied()
            .unwrap_or_else(RegisterValue::undefined),
        runtime,
    )?;

    let keys = runtime.own_property_keys(target).map_err(|e| {
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
    let target = to_object_for_introspection(*this, runtime)?;
    let key = args
        .first()
        .copied()
        .unwrap_or_else(RegisterValue::undefined);
    let property = runtime.property_name_from_value(key)?;

    let has_own = runtime
        .own_property_descriptor(target, property)
        .map_err(|e| VmNativeCallError::Internal(format!("propertyIsEnumerable: {e:?}").into()))?;
    Ok(RegisterValue::from_bool(
        has_own
            .map(|descriptor| descriptor.attributes().enumerable())
            .unwrap_or(false),
    ))
}

// ---------------------------------------------------------------------------
// ES2024 §20.1.3.2  Object.prototype.hasOwnProperty(V)
// ---------------------------------------------------------------------------
fn object_has_own_property(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let target = to_object_for_introspection(*this, runtime)?;
    let key = args
        .first()
        .copied()
        .unwrap_or_else(RegisterValue::undefined);
    let property = runtime.property_name_from_value(key)?;
    let has = runtime
        .own_property_descriptor(target, property)
        .map_err(|e| VmNativeCallError::Internal(format!("hasOwnProperty: {e:?}").into()))?
        .is_some();
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
