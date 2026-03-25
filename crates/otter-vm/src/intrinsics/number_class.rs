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

pub(super) static NUMBER_INTRINSIC: NumberIntrinsic = NumberIntrinsic;

const NUMBER_DATA_SLOT: &str = "__otter_number_data__";
const NUMBER_VALUE_OF_ERROR: &str = "Number.prototype.valueOf requires a number receiver";

pub(super) struct NumberIntrinsic;

impl IntrinsicInstaller for NumberIntrinsic {
    fn init(
        &self,
        intrinsics: &mut VmIntrinsics,
        cx: &mut IntrinsicInstallContext<'_>,
    ) -> Result<(), IntrinsicsError> {
        let descriptor = number_class_descriptor();
        let plan = ClassBuilder::from_descriptor(&descriptor)
            .expect("Number class descriptors should normalize")
            .build();

        let constructor = if let Some(descriptor) = plan.constructor() {
            let host_function = cx.native_functions.register(descriptor.clone());
            cx.alloc_intrinsic_host_function(host_function, intrinsics.function_prototype())?
        } else {
            cx.alloc_intrinsic_object(Some(intrinsics.object_prototype()))?
        };

        intrinsics.number_constructor = constructor;
        install_class_plan(
            intrinsics.number_prototype(),
            intrinsics.number_constructor(),
            &plan,
            intrinsics.function_prototype(),
            cx,
        )?;
        initialize_number_prototype(intrinsics, cx)?;
        initialize_number_constructor(intrinsics, cx)?;

        Ok(())
    }

    fn install_on_global(
        &self,
        intrinsics: &VmIntrinsics,
        cx: &mut IntrinsicInstallContext<'_>,
    ) -> Result<(), IntrinsicsError> {
        cx.install_global_value(
            intrinsics,
            "Number",
            RegisterValue::from_object_handle(intrinsics.number_constructor().0),
        )
    }
}

fn number_class_descriptor() -> JsClassDescriptor {
    JsClassDescriptor::new("Number")
        .with_constructor(NativeFunctionDescriptor::constructor(
            "Number",
            1,
            number_constructor,
        ))
        .with_binding(NativeBindingDescriptor::new(
            NativeBindingTarget::Prototype,
            NativeFunctionDescriptor::method("valueOf", 0, number_value_of),
        ))
}

fn number_constructor(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let primitive = if args.is_empty() {
        RegisterValue::from_i32(0)
    } else {
        coerce_to_number(
            args.first()
                .copied()
                .unwrap_or_else(RegisterValue::undefined),
            runtime,
        )
    };

    if this.as_object_handle().is_some() {
        box_number_object(primitive, runtime)
    } else {
        Ok(primitive)
    }
}

fn number_value_of(
    this: &RegisterValue,
    _args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    if this.as_number().is_some() {
        return Ok(*this);
    }
    if let Some(handle) = this.as_object_handle().map(ObjectHandle)
        && let Some(value) = number_data(handle, runtime)?
    {
        return Ok(value);
    }

    Err(VmNativeCallError::Internal(NUMBER_VALUE_OF_ERROR.into()))
}

fn coerce_to_number(
    value: RegisterValue,
    runtime: &mut crate::interpreter::RuntimeState,
) -> RegisterValue {
    if value == RegisterValue::undefined() {
        return RegisterValue::from_number(f64::NAN);
    }
    if value == RegisterValue::null() {
        return RegisterValue::from_i32(0);
    }
    if let Some(number) = value.as_number() {
        return RegisterValue::from_number(number);
    }
    if let Some(boolean) = value.as_bool() {
        return RegisterValue::from_i32(if boolean { 1 } else { 0 });
    }
    if let Some(handle) = value.as_object_handle().map(ObjectHandle) {
        if let Some(string) = runtime.objects().string_value(handle).ok().flatten() {
            let trimmed = string.trim();
            if trimmed.is_empty() {
                return RegisterValue::from_i32(0);
            }
            if let Ok(parsed) = trimmed.parse::<f64>() {
                return RegisterValue::from_number(parsed);
            }
            return RegisterValue::from_number(f64::NAN);
        }
        if let Ok(Some(value)) = number_data(handle, runtime) {
            return value;
        }
    }

    RegisterValue::from_number(f64::NAN)
}

fn initialize_number_prototype(
    intrinsics: &VmIntrinsics,
    cx: &mut IntrinsicInstallContext<'_>,
) -> Result<(), IntrinsicsError> {
    let backing = cx.property_names.intern(NUMBER_DATA_SLOT);
    cx.heap.set_property(
        intrinsics.number_prototype(),
        backing,
        RegisterValue::from_i32(0),
    )?;
    Ok(())
}

fn initialize_number_constructor(
    intrinsics: &VmIntrinsics,
    cx: &mut IntrinsicInstallContext<'_>,
) -> Result<(), IntrinsicsError> {
    let nan = cx.property_names.intern("NaN");
    cx.heap.set_property(
        intrinsics.number_constructor(),
        nan,
        RegisterValue::from_number(f64::NAN),
    )?;

    let positive_infinity = cx.property_names.intern("POSITIVE_INFINITY");
    cx.heap.set_property(
        intrinsics.number_constructor(),
        positive_infinity,
        RegisterValue::from_number(f64::INFINITY),
    )?;

    let negative_infinity = cx.property_names.intern("NEGATIVE_INFINITY");
    cx.heap.set_property(
        intrinsics.number_constructor(),
        negative_infinity,
        RegisterValue::from_number(f64::NEG_INFINITY),
    )?;

    Ok(())
}

fn set_number_data(
    receiver: ObjectHandle,
    primitive: RegisterValue,
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<(), VmNativeCallError> {
    let backing = runtime.intern_property_name(NUMBER_DATA_SLOT);
    runtime
        .objects_mut()
        .set_property(receiver, backing, primitive)
        .map_err(|error| {
            VmNativeCallError::Internal(
                format!("Number constructor backing store failed: {error:?}").into(),
            )
        })?;
    Ok(())
}

fn number_data(
    handle: ObjectHandle,
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<Option<RegisterValue>, VmNativeCallError> {
    let backing = runtime.intern_property_name(NUMBER_DATA_SLOT);
    let Some(lookup) = runtime
        .objects()
        .get_property(handle, backing)
        .map_err(|error| {
            VmNativeCallError::Internal(format!("Number data lookup failed: {error:?}").into())
        })?
    else {
        return Ok(None);
    };

    let PropertyValue::Data(value) = lookup.value() else {
        return Ok(None);
    };

    Ok(Some(value))
}

pub(super) fn box_number_object(
    primitive: RegisterValue,
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let wrapper =
        runtime.alloc_object_with_prototype(Some(runtime.intrinsics().number_prototype()));
    set_number_data(wrapper, primitive, runtime)?;
    Ok(RegisterValue::from_object_handle(wrapper.0))
}
