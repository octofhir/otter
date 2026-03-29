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
    symbol_class::box_symbol_object,
    string_class::box_string_object,
    WellKnownSymbol,
};

pub(super) static OBJECT_INTRINSIC: ObjectIntrinsic = ObjectIntrinsic;

const STRING_DATA_SLOT: &str = "__otter_string_data__";
const NUMBER_DATA_SLOT: &str = "__otter_number_data__";
const BOOLEAN_DATA_SLOT: &str = "__otter_boolean_data__";
const SYMBOL_DATA_SLOT: &str = "__otter_symbol_data__";
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

        if value.is_symbol() {
            return box_symbol_object(value, runtime);
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
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let object = to_object_for_prototype_method(
        *this,
        runtime,
        "Object.prototype.valueOf requires an object-coercible receiver",
    )?;
    Ok(RegisterValue::from_object_handle(object.0))
}

fn object_to_string(
    this: &RegisterValue,
    _args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let tag = if *this == RegisterValue::undefined() || *this == RegisterValue::null() {
        object_to_string_tag(*this, runtime)?.to_string()
    } else {
        let object = to_object_for_prototype_method(
            *this,
            runtime,
            "Object.prototype.toString requires an object-coercible receiver",
        )?;
        let builtin_tag =
            object_to_string_tag(RegisterValue::from_object_handle(object.0), runtime)?;
        let to_string_tag =
            runtime.intern_symbol_property_name(WellKnownSymbol::ToStringTag.stable_id());
        let symbol_tag = runtime.ordinary_get(
            object,
            to_string_tag,
            RegisterValue::from_object_handle(object.0),
        )?;
        if let Some(handle) = symbol_tag.as_object_handle().map(ObjectHandle)
            && let Some(tag) = runtime.objects().string_value(handle).map_err(|error| {
                VmNativeCallError::Internal(
                    format!("Object.prototype.toString tag lookup failed: {error:?}").into(),
                )
            })?
        {
            tag.to_string()
        } else {
            let legacy_tag = runtime.intern_property_name("@@toStringTag");
            let tag_value = runtime.ordinary_get(
                object,
                legacy_tag,
                RegisterValue::from_object_handle(object.0),
            )?;
            match tag_value.as_object_handle().map(ObjectHandle) {
                Some(handle) => match runtime.objects().string_value(handle).map_err(|error| {
                    VmNativeCallError::Internal(
                        format!("Object.prototype.toString tag lookup failed: {error:?}").into(),
                    )
                })? {
                    Some(tag) => tag.to_string(),
                    None => builtin_tag.to_string(),
                },
                None => builtin_tag.to_string(),
            }
        }
    };
    let string = runtime.alloc_string(format!("[object {tag}]"));
    Ok(RegisterValue::from_object_handle(string.0))
}

fn object_is_prototype_of(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let prototype = to_object_for_prototype_method(*this, runtime, OBJECT_IS_PROTOTYPE_OF_ERROR)?;
    let Some(mut candidate) = args
        .first()
        .copied()
        .and_then(RegisterValue::as_object_handle)
        .map(ObjectHandle)
    else {
        return Ok(RegisterValue::from_bool(false));
    };
    if matches!(runtime.objects().kind(candidate), Ok(HeapValueKind::String)) {
        return Ok(RegisterValue::from_bool(false));
    }

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
    let prototype_arg = args
        .first()
        .copied()
        .unwrap_or_else(RegisterValue::undefined);
    let prototype = if prototype_arg == RegisterValue::null() {
        None
    } else {
        let handle = prototype_arg
            .as_object_handle()
            .map(ObjectHandle)
            .ok_or_else(|| {
                throw_type_error(runtime, "Object.create prototype must be an object or null")
                    .unwrap_or_else(|error| error)
            })?;
        if matches!(runtime.objects().kind(handle), Ok(HeapValueKind::String)) {
            return Err(throw_type_error(
                runtime,
                "Object.create prototype must be an object or null",
            )?);
        }
        Some(handle)
    };
    let object = runtime.alloc_object_with_prototype(prototype);

    if let Some(properties_arg) = args.get(1).copied()
        && properties_arg != RegisterValue::undefined()
    {
        let properties = to_object_for_descriptor_map(
            properties_arg,
            runtime,
            "Object.create properties must be object-coercible",
        )?;
        let descriptors = crate::abstract_ops::collect_define_properties(properties, runtime)?;
        let property_names = runtime.property_names().clone();
        for (property, descriptor) in descriptors {
            let success = runtime
                .objects_mut()
                .define_own_property_from_descriptor_with_registry(
                    object,
                    property,
                    descriptor,
                    &property_names,
                )
                .map_err(|e| VmNativeCallError::Internal(format!("Object.create: {e:?}").into()))?;
            if !success {
                return Err(throw_type_error(
                    runtime,
                    "Object.create could not define property",
                )?);
            }
        }
    }

    Ok(RegisterValue::from_object_handle(object.0))
}

fn to_object_for_introspection(
    value: RegisterValue,
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<ObjectHandle, VmNativeCallError> {
    if value == RegisterValue::undefined() || value == RegisterValue::null() {
        return Err(throw_type_error(
            runtime,
            "Object operation requires an object-coercible value",
        )?);
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

    if value.is_symbol() {
        let object = box_symbol_object(value, runtime)?;
        return Ok(ObjectHandle(
            object
                .as_object_handle()
                .expect("boxed symbol should return an object"),
        ));
    }

    value.as_object_handle().map(ObjectHandle).ok_or_else(|| {
        throw_type_error(
            runtime,
            "Object operation requires an object-coercible value",
        )
        .unwrap_or_else(|error| error)
    })
}

fn non_string_object_target(
    value: RegisterValue,
    runtime: &crate::interpreter::RuntimeState,
) -> Option<ObjectHandle> {
    let handle = value.as_object_handle().map(ObjectHandle)?;
    if matches!(runtime.objects().kind(handle), Ok(HeapValueKind::String)) {
        return None;
    }
    Some(handle)
}

fn with_vm_context(error: VmNativeCallError, context: &str) -> VmNativeCallError {
    match error {
        VmNativeCallError::Thrown(value) => VmNativeCallError::Thrown(value),
        VmNativeCallError::Internal(message) => {
            VmNativeCallError::Internal(format!("{context}: {message}").into())
        }
    }
}

fn to_object_for_descriptor_map(
    value: RegisterValue,
    runtime: &mut crate::interpreter::RuntimeState,
    context: &str,
) -> Result<ObjectHandle, VmNativeCallError> {
    match to_object_for_introspection(value, runtime) {
        Ok(object) => Ok(object),
        Err(VmNativeCallError::Thrown(_)) => Err(throw_type_error(runtime, context)?),
        Err(error) => Err(with_vm_context(error, context)),
    }
}

fn to_object_for_prototype_method(
    value: RegisterValue,
    runtime: &mut crate::interpreter::RuntimeState,
    context: &str,
) -> Result<ObjectHandle, VmNativeCallError> {
    if value == RegisterValue::undefined() || value == RegisterValue::null() {
        return Err(throw_type_error(runtime, context)?);
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

    if value.is_symbol() {
        let object = box_symbol_object(value, runtime)?;
        return Ok(ObjectHandle(
            object
                .as_object_handle()
                .expect("boxed symbol should return an object"),
        ));
    }

    let Some(handle) = value.as_object_handle().map(ObjectHandle) else {
        return Err(throw_type_error(runtime, context)?);
    };

    match runtime.objects().kind(handle) {
        Ok(HeapValueKind::String) => {
            let object = box_string_object(handle, runtime)?;
            Ok(ObjectHandle(
                object
                    .as_object_handle()
                    .expect("boxed string should return an object"),
            ))
        }
        Ok(_) => Ok(handle),
        Err(error) => Err(VmNativeCallError::Internal(
            format!("ToObject receiver kind lookup failed: {error:?}").into(),
        )),
    }
}

fn throw_type_error(
    runtime: &mut crate::interpreter::RuntimeState,
    message: &str,
) -> Result<VmNativeCallError, VmNativeCallError> {
    let error = runtime.alloc_type_error(message).map_err(|error| {
        VmNativeCallError::Internal(format!("TypeError allocation failed: {error}").into())
    })?;
    Ok(VmNativeCallError::Thrown(
        RegisterValue::from_object_handle(error.0),
    ))
}

fn primitive_prototype(
    value: RegisterValue,
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<Option<ObjectHandle>, VmNativeCallError> {
    if value.as_bool().is_some() {
        return Ok(Some(runtime.intrinsics().boolean_prototype()));
    }

    if value.as_number().is_some() {
        return Ok(Some(runtime.intrinsics().number_prototype()));
    }

    if value.is_symbol() {
        return Ok(Some(runtime.intrinsics().symbol_prototype()));
    }

    let Some(handle) = value.as_object_handle().map(ObjectHandle) else {
        return Ok(None);
    };

    match runtime.objects().kind(handle) {
        Ok(HeapValueKind::String) => Ok(Some(runtime.intrinsics().string_prototype())),
        Ok(_) => Ok(None),
        Err(error) => Err(VmNativeCallError::Internal(
            format!("primitive prototype kind lookup failed: {error:?}").into(),
        )),
    }
}

fn prototype_argument(
    value: RegisterValue,
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<Option<ObjectHandle>, VmNativeCallError> {
    if value == RegisterValue::null() {
        return Ok(None);
    }

    non_string_object_target(value, runtime)
        .map(Some)
        .ok_or_else(|| {
            throw_type_error(
                runtime,
                "Object.setPrototypeOf proto must be object or null",
            )
            .unwrap_or_else(|error| error)
        })
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
    if value.is_symbol() {
        return Ok("Symbol");
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
    if has_own_data_slot(handle, SYMBOL_DATA_SLOT, runtime)? {
        return Ok("Symbol");
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
        .and_then(|value| non_string_object_target(value, runtime))
        .ok_or_else(|| {
            throw_type_error(runtime, "Object.defineProperty requires an object")
                .unwrap_or_else(|error| error)
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
        return Err(throw_type_error(
            runtime,
            "Object.defineProperty could not define property",
        )?);
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
        .and_then(|value| non_string_object_target(value, runtime))
        .ok_or_else(|| {
            throw_type_error(runtime, "Object.defineProperties requires an object")
                .unwrap_or_else(|error| error)
        })?;
    let properties_value = args
        .get(1)
        .copied()
        .unwrap_or_else(RegisterValue::undefined);

    let properties = to_object_for_descriptor_map(
        properties_value,
        runtime,
        "Object.defineProperties requires an object-coercible descriptor map",
    )?;
    let descriptors = crate::abstract_ops::collect_define_properties(properties, runtime)?;
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
            return Err(throw_type_error(
                runtime,
                "Object.defineProperties could not define property",
            )?);
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
        .enumerable_own_property_keys(target)
        .map_err(|e| VmNativeCallError::Internal(format!("Object.keys: {e}").into()))?;

    let array = runtime.alloc_array();
    for key_id in &keys {
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
        .enumerable_own_property_keys(target)
        .map_err(|e| VmNativeCallError::Internal(format!("Object.values: {e}").into()))?;

    let array = runtime.alloc_array();
    for key_id in &keys {
        let value = runtime
            .own_property_value(target, *key_id)
            .map_err(|e| with_vm_context(e, "Object.values get"))?;
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
        .enumerable_own_property_keys(target)
        .map_err(|e| VmNativeCallError::Internal(format!("Object.entries: {e}").into()))?;

    let result = runtime.alloc_array();
    for key_id in &keys {
        let value = runtime
            .own_property_value(target, *key_id)
            .map_err(|e| with_vm_context(e, "Object.entries get"))?;
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
    let Some(handle) = non_string_object_target(target, runtime) else {
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
    let Some(handle) = non_string_object_target(target, runtime) else {
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
    let Some(handle) = non_string_object_target(target, runtime) else {
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
    let Some(handle) = non_string_object_target(target, runtime) else {
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
    let Some(handle) = non_string_object_target(target, runtime) else {
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
    let Some(handle) = non_string_object_target(target, runtime) else {
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
        .unwrap_or_else(RegisterValue::undefined);
    if target == RegisterValue::undefined() || target == RegisterValue::null() {
        return Err(throw_type_error(
            runtime,
            "Object.getPrototypeOf requires an object-coercible value",
        )?);
    }

    if let Some(proto) = primitive_prototype(target, runtime)? {
        return Ok(RegisterValue::from_object_handle(proto.0));
    }

    let target = target.as_object_handle().map(ObjectHandle).ok_or_else(|| {
        VmNativeCallError::Internal("Object.getPrototypeOf target kind invalid".into())
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
        .unwrap_or_else(RegisterValue::undefined);
    let proto_arg = args
        .get(1)
        .copied()
        .unwrap_or_else(RegisterValue::undefined);
    let proto = prototype_argument(proto_arg, runtime)?;
    if target == RegisterValue::undefined() || target == RegisterValue::null() {
        return Err(throw_type_error(
            runtime,
            "Object.setPrototypeOf requires an object-coercible value",
        )?);
    }

    if primitive_prototype(target, runtime)?.is_some() {
        return Ok(target);
    }

    let target = target.as_object_handle().map(ObjectHandle).ok_or_else(|| {
        VmNativeCallError::Internal("Object.setPrototypeOf target kind invalid".into())
    })?;
    runtime
        .objects_mut()
        .set_prototype(target, proto)
        .map_err(|e| VmNativeCallError::Internal(format!("Object.setPrototypeOf: {e:?}").into()))?
        .then_some(())
        .ok_or_else(|| {
            throw_type_error(runtime, "Object.setPrototypeOf could not set prototype")
                .unwrap_or_else(|error| error)
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
    let target = to_object_for_introspection(
        args.first()
            .copied()
            .unwrap_or_else(RegisterValue::undefined),
        runtime,
    )?;

    for source_arg in args.iter().skip(1) {
        if *source_arg == RegisterValue::undefined() || *source_arg == RegisterValue::null() {
            continue;
        }

        let source = to_object_for_introspection(*source_arg, runtime)?;
        let keys = runtime
            .enumerable_own_property_keys(source)
            .map_err(|e| VmNativeCallError::Internal(format!("Object.assign keys: {e}").into()))?;

        for key_id in keys {
            let value = runtime
                .own_property_value(source, key_id)
                .map_err(|e| with_vm_context(e, "Object.assign get"))?;
            let success = runtime
                .ordinary_set(
                    target,
                    key_id,
                    RegisterValue::from_object_handle(target.0),
                    value,
                )
                .map_err(|e| with_vm_context(e, "Object.assign set"))?;
            if !success {
                let error = runtime
                    .alloc_type_error("Object.assign could not assign property")
                    .map_err(|e| {
                        VmNativeCallError::Internal(format!("Object.assign TypeError: {e}").into())
                    })?;
                return Err(VmNativeCallError::Thrown(
                    RegisterValue::from_object_handle(error.0),
                ));
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
        .filter(|key| !runtime.property_names().is_symbol(**key))
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
