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
            NativeFunctionDescriptor::method("create", 0, object_create),
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
        HeapValueKind::HostFunction | HeapValueKind::Closure => Ok("Function"),
        HeapValueKind::Object
        | HeapValueKind::String
        | HeapValueKind::UpvalueCell
        | HeapValueKind::Iterator
        | HeapValueKind::Promise => Ok("Object"),
    }
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

    Ok(matches!(lookup.value(), PropertyValue::Data(_)))
}
