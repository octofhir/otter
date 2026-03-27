use crate::builders::ClassBuilder;
use crate::descriptors::{
    JsClassDescriptor, NativeBindingDescriptor, NativeBindingTarget, NativeFunctionDescriptor,
    VmNativeCallError,
};
use crate::object::{ObjectHandle, PropertyValue};
use crate::value::RegisterValue;

use super::{
    IntrinsicsError, VmIntrinsics,
    install::{IntrinsicInstallContext, IntrinsicInstaller, install_class_plan},
};

pub(super) static BOOLEAN_INTRINSIC: BooleanIntrinsic = BooleanIntrinsic;

const BOOLEAN_DATA_SLOT: &str = "__otter_boolean_data__";
const BOOLEAN_VALUE_OF_ERROR: &str = "Boolean.prototype.valueOf requires a boolean receiver";
const BOOLEAN_TO_STRING_ERROR: &str = "Boolean.prototype.toString requires a boolean receiver";

pub(super) struct BooleanIntrinsic;

impl IntrinsicInstaller for BooleanIntrinsic {
    fn init(
        &self,
        intrinsics: &mut VmIntrinsics,
        cx: &mut IntrinsicInstallContext<'_>,
    ) -> Result<(), IntrinsicsError> {
        let descriptor = boolean_class_descriptor();
        let plan = ClassBuilder::from_descriptor(&descriptor)
            .expect("Boolean class descriptors should normalize")
            .build();

        let constructor = if let Some(descriptor) = plan.constructor() {
            let host_function = cx.native_functions.register(descriptor.clone());
            cx.alloc_intrinsic_host_function(host_function, intrinsics.function_prototype())?
        } else {
            cx.alloc_intrinsic_object(Some(intrinsics.object_prototype()))?
        };

        intrinsics.boolean_constructor = constructor;
        install_class_plan(
            intrinsics.boolean_prototype(),
            intrinsics.boolean_constructor(),
            &plan,
            intrinsics.function_prototype(),
            cx,
        )?;
        initialize_boolean_prototype(intrinsics, cx)?;

        Ok(())
    }

    fn install_on_global(
        &self,
        intrinsics: &VmIntrinsics,
        cx: &mut IntrinsicInstallContext<'_>,
    ) -> Result<(), IntrinsicsError> {
        cx.install_global_value(
            intrinsics,
            "Boolean",
            RegisterValue::from_object_handle(intrinsics.boolean_constructor().0),
        )
    }
}

fn boolean_class_descriptor() -> JsClassDescriptor {
    JsClassDescriptor::new("Boolean")
        .with_constructor(NativeFunctionDescriptor::constructor(
            "Boolean",
            1,
            boolean_constructor,
        ))
        .with_binding(NativeBindingDescriptor::new(
            NativeBindingTarget::Prototype,
            NativeFunctionDescriptor::method("toString", 0, boolean_to_string),
        ))
        .with_binding(NativeBindingDescriptor::new(
            NativeBindingTarget::Prototype,
            NativeFunctionDescriptor::method("valueOf", 0, boolean_value_of),
        ))
}

fn boolean_constructor(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let primitive = RegisterValue::from_bool(to_boolean(
        args.first()
            .copied()
            .unwrap_or_else(RegisterValue::undefined),
        runtime,
    ));

    if this.as_object_handle().is_some() {
        box_boolean_object(primitive, runtime)
    } else {
        Ok(primitive)
    }
}

fn boolean_value_of(
    this: &RegisterValue,
    _args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    if this.as_bool().is_some() {
        return Ok(*this);
    }
    if let Some(handle) = this.as_object_handle().map(ObjectHandle)
        && let Some(value) = boolean_data(handle, runtime)?
    {
        return Ok(value);
    }

    Err(VmNativeCallError::Internal(BOOLEAN_VALUE_OF_ERROR.into()))
}

fn boolean_to_string(
    this: &RegisterValue,
    _args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let value = if let Some(boolean) = this.as_bool() {
        boolean
    } else if let Some(handle) = this.as_object_handle().map(ObjectHandle) {
        boolean_data(handle, runtime)?
            .and_then(RegisterValue::as_bool)
            .ok_or_else(|| VmNativeCallError::Internal(BOOLEAN_TO_STRING_ERROR.into()))?
    } else {
        return Err(VmNativeCallError::Internal(BOOLEAN_TO_STRING_ERROR.into()));
    };

    let string = runtime.alloc_string(if value { "true" } else { "false" });
    Ok(RegisterValue::from_object_handle(string.0))
}

fn to_boolean(value: RegisterValue, runtime: &crate::interpreter::RuntimeState) -> bool {
    if value == RegisterValue::undefined() || value == RegisterValue::null() {
        return false;
    }
    if let Some(boolean) = value.as_bool() {
        return boolean;
    }
    if let Some(number) = value.as_number() {
        return !number.is_nan() && number != 0.0;
    }
    if let Some(handle) = value.as_object_handle().map(ObjectHandle) {
        if let Some(string) = runtime.objects().string_value(handle).ok().flatten() {
            return !string.is_empty();
        }
        return true;
    }

    true
}

fn initialize_boolean_prototype(
    intrinsics: &VmIntrinsics,
    cx: &mut IntrinsicInstallContext<'_>,
) -> Result<(), IntrinsicsError> {
    let backing = cx.property_names.intern(BOOLEAN_DATA_SLOT);
    cx.heap.set_property(
        intrinsics.boolean_prototype(),
        backing,
        RegisterValue::from_bool(false),
    )?;
    Ok(())
}

fn set_boolean_data(
    receiver: ObjectHandle,
    primitive: RegisterValue,
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<(), VmNativeCallError> {
    let backing = runtime.intern_property_name(BOOLEAN_DATA_SLOT);
    runtime
        .objects_mut()
        .set_property(receiver, backing, primitive)
        .map_err(|error| {
            VmNativeCallError::Internal(
                format!("Boolean constructor backing store failed: {error:?}").into(),
            )
        })?;
    Ok(())
}

fn boolean_data(
    handle: ObjectHandle,
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<Option<RegisterValue>, VmNativeCallError> {
    let backing = runtime.intern_property_name(BOOLEAN_DATA_SLOT);
    let Some(lookup) = runtime
        .objects()
        .get_property(handle, backing)
        .map_err(|error| {
            VmNativeCallError::Internal(format!("Boolean data lookup failed: {error:?}").into())
        })?
    else {
        return Ok(None);
    };

    let PropertyValue::Data { value, .. } = lookup.value() else {
        return Ok(None);
    };

    Ok(Some(value))
}

pub(super) fn box_boolean_object(
    primitive: RegisterValue,
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let wrapper =
        runtime.alloc_object_with_prototype(Some(runtime.intrinsics().boolean_prototype()));
    set_boolean_data(wrapper, primitive, runtime)?;
    Ok(RegisterValue::from_object_handle(wrapper.0))
}
