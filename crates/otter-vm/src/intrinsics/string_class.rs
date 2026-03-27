use crate::builders::ClassBuilder;
use crate::descriptors::{
    JsClassDescriptor, NativeBindingDescriptor, NativeBindingTarget, NativeFunctionDescriptor,
    VmNativeCallError,
};
use crate::object::{HeapValueKind, ObjectHandle, PropertyValue};
use crate::value::RegisterValue;

use super::{
    IntrinsicsError, VmIntrinsics,
    install::{IntrinsicInstallContext, IntrinsicInstaller, install_class_plan},
};

pub(super) static STRING_INTRINSIC: StringIntrinsic = StringIntrinsic;

const STRING_DATA_SLOT: &str = "__otter_string_data__";
const STRING_VALUE_OF_ERROR: &str = "String.prototype.valueOf requires a string receiver";

pub(super) struct StringIntrinsic;

impl IntrinsicInstaller for StringIntrinsic {
    fn init(
        &self,
        intrinsics: &mut VmIntrinsics,
        cx: &mut IntrinsicInstallContext<'_>,
    ) -> Result<(), IntrinsicsError> {
        let descriptor = string_class_descriptor();
        let plan = ClassBuilder::from_descriptor(&descriptor)
            .expect("String class descriptors should normalize")
            .build();

        let constructor = if let Some(descriptor) = plan.constructor() {
            let host_function = cx.native_functions.register(descriptor.clone());
            cx.alloc_intrinsic_host_function(host_function, intrinsics.function_prototype())?
        } else {
            cx.alloc_intrinsic_object(Some(intrinsics.object_prototype()))?
        };

        intrinsics.string_constructor = constructor;
        install_class_plan(
            intrinsics.string_prototype(),
            intrinsics.string_constructor(),
            &plan,
            intrinsics.function_prototype(),
            cx,
        )?;
        initialize_string_prototype(intrinsics, cx)?;

        Ok(())
    }

    fn install_on_global(
        &self,
        intrinsics: &VmIntrinsics,
        cx: &mut IntrinsicInstallContext<'_>,
    ) -> Result<(), IntrinsicsError> {
        cx.install_global_value(
            intrinsics,
            "String",
            RegisterValue::from_object_handle(intrinsics.string_constructor().0),
        )
    }
}

fn string_class_descriptor() -> JsClassDescriptor {
    JsClassDescriptor::new("String")
        .with_constructor(NativeFunctionDescriptor::constructor(
            "String",
            1,
            string_constructor,
        ))
        .with_binding(NativeBindingDescriptor::new(
            NativeBindingTarget::Prototype,
            NativeFunctionDescriptor::method("toString", 0, string_value_of),
        ))
        .with_binding(NativeBindingDescriptor::new(
            NativeBindingTarget::Prototype,
            NativeFunctionDescriptor::method("valueOf", 0, string_value_of),
        ))
}

fn string_constructor(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let coerced = coerce_to_string(
        args.first()
            .copied()
            .unwrap_or_else(RegisterValue::undefined),
        runtime,
    )?;
    let primitive = runtime.alloc_string(coerced);

    if let Some(receiver) = this.as_object_handle().map(ObjectHandle) {
        set_string_data(receiver, primitive, runtime)?;
        Ok(*this)
    } else {
        Ok(RegisterValue::from_object_handle(primitive.0))
    }
}

fn string_value_of(
    this: &RegisterValue,
    _args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    if let Some(handle) = this.as_object_handle().map(ObjectHandle) {
        if matches!(runtime.objects().kind(handle), Ok(HeapValueKind::String)) {
            return Ok(*this);
        }
        if let Some(primitive) = string_data(handle, runtime)? {
            return Ok(RegisterValue::from_object_handle(primitive.0));
        }
    }

    Err(VmNativeCallError::Internal(STRING_VALUE_OF_ERROR.into()))
}

fn coerce_to_string(
    value: RegisterValue,
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<Box<str>, VmNativeCallError> {
    if value == RegisterValue::undefined() {
        return Ok("undefined".into());
    }
    if value == RegisterValue::null() {
        return Ok("null".into());
    }
    if let Some(boolean) = value.as_bool() {
        return Ok(if boolean { "true" } else { "false" }.into());
    }
    if let Some(number) = value.as_number() {
        return Ok(number_to_string(number).into_boxed_str());
    }
    if let Some(handle) = value.as_object_handle().map(ObjectHandle) {
        if let Some(string) = runtime
            .objects()
            .string_value(handle)
            .map_err(|error| VmNativeCallError::Internal(format!("{error:?}").into()))?
        {
            return Ok(string.to_string().into_boxed_str());
        }
        if let Some(primitive) = string_data(handle, runtime)?
            && let Some(string) = runtime
                .objects()
                .string_value(primitive)
                .map_err(|error| VmNativeCallError::Internal(format!("{error:?}").into()))?
        {
            return Ok(string.to_string().into_boxed_str());
        }
        return Ok("[object Object]".into());
    }

    Ok(String::new().into_boxed_str())
}

fn initialize_string_prototype(
    intrinsics: &VmIntrinsics,
    cx: &mut IntrinsicInstallContext<'_>,
) -> Result<(), IntrinsicsError> {
    let primitive = cx.heap.alloc_string("");
    cx.heap
        .set_prototype(primitive, Some(intrinsics.string_prototype()))?;
    let backing = cx.property_names.intern(STRING_DATA_SLOT);
    cx.heap.set_property(
        intrinsics.string_prototype(),
        backing,
        RegisterValue::from_object_handle(primitive.0),
    )?;
    Ok(())
}

fn set_string_data(
    receiver: ObjectHandle,
    primitive: ObjectHandle,
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<(), VmNativeCallError> {
    let backing = runtime.intern_property_name(STRING_DATA_SLOT);
    runtime
        .objects_mut()
        .set_property(
            receiver,
            backing,
            RegisterValue::from_object_handle(primitive.0),
        )
        .map_err(|error| {
            VmNativeCallError::Internal(
                format!("String constructor backing store failed: {error:?}").into(),
            )
        })?;
    Ok(())
}

fn string_data(
    handle: ObjectHandle,
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<Option<ObjectHandle>, VmNativeCallError> {
    let backing = runtime.intern_property_name(STRING_DATA_SLOT);
    let Some(lookup) = runtime
        .objects()
        .get_property(handle, backing)
        .map_err(|error| {
            VmNativeCallError::Internal(format!("String data lookup failed: {error:?}").into())
        })?
    else {
        return Ok(None);
    };

    let PropertyValue::Data { value, .. } = lookup.value() else {
        return Ok(None);
    };

    Ok(value.as_object_handle().map(ObjectHandle))
}

pub(super) fn box_string_object(
    primitive: ObjectHandle,
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let wrapper =
        runtime.alloc_object_with_prototype(Some(runtime.intrinsics().string_prototype()));
    set_string_data(wrapper, primitive, runtime)?;
    Ok(RegisterValue::from_object_handle(wrapper.0))
}

fn number_to_string(number: f64) -> String {
    if number.is_nan() {
        "NaN".to_string()
    } else if number.is_infinite() {
        if number.is_sign_positive() {
            "Infinity".to_string()
        } else {
            "-Infinity".to_string()
        }
    } else if number == 0.0 {
        "0".to_string()
    } else if number.fract() == 0.0 {
        format!("{number:.0}")
    } else {
        number.to_string()
    }
}
