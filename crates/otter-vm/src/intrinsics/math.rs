use crate::builders::NamespaceBuilder;
use crate::descriptors::{
    NativeBindingDescriptor, NativeBindingTarget, NativeFunctionDescriptor, VmNativeCallError,
};
use crate::object::{ObjectHandle, PropertyValue};
use crate::value::RegisterValue;

use super::{
    IntrinsicsError, VmIntrinsics,
    install::{IntrinsicInstallContext, IntrinsicInstaller, install_object_plan},
};

pub(super) static MATH_INTRINSIC: MathIntrinsic = MathIntrinsic;

pub(super) struct MathIntrinsic;

impl IntrinsicInstaller for MathIntrinsic {
    fn init(
        &self,
        intrinsics: &mut VmIntrinsics,
        cx: &mut IntrinsicInstallContext<'_>,
    ) -> Result<(), IntrinsicsError> {
        let math_namespace = cx.alloc_intrinsic_object(Some(intrinsics.object_prototype()))?;
        let math_plan = NamespaceBuilder::from_bindings(&math_namespace_bindings())
            .expect("Math namespace descriptors should normalize")
            .build();
        install_object_plan(
            math_namespace,
            &math_plan,
            intrinsics.function_prototype(),
            cx,
        )?;
        intrinsics.set_math_namespace(math_namespace);
        Ok(())
    }

    fn install_on_global(
        &self,
        intrinsics: &VmIntrinsics,
        cx: &mut IntrinsicInstallContext<'_>,
    ) -> Result<(), IntrinsicsError> {
        let math_namespace = intrinsics
            .math_namespace()
            .expect("Math namespace should be installed during init_core");
        cx.install_global_value(
            intrinsics,
            "Math",
            RegisterValue::from_object_handle(math_namespace.0),
        )
    }
}

fn math_namespace_bindings() -> Vec<NativeBindingDescriptor> {
    vec![
        NativeBindingDescriptor::new(
            NativeBindingTarget::Namespace,
            NativeFunctionDescriptor::method("abs", 1, math_abs),
        ),
        NativeBindingDescriptor::new(
            NativeBindingTarget::Namespace,
            NativeFunctionDescriptor::getter("memory", math_memory_getter),
        ),
        NativeBindingDescriptor::new(
            NativeBindingTarget::Namespace,
            NativeFunctionDescriptor::setter("memory", math_memory_setter),
        ),
    ]
}

fn math_abs(
    _this: &RegisterValue,
    args: &[RegisterValue],
    _runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let value = args
        .first()
        .copied()
        .and_then(RegisterValue::as_number)
        .map(f64::abs)
        .unwrap_or(f64::NAN);
    Ok(RegisterValue::from_number(value))
}

fn math_memory_getter(
    this: &RegisterValue,
    _args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let receiver = this.as_object_handle().map(ObjectHandle).ok_or_else(|| {
        VmNativeCallError::Internal("Math.memory getter requires object receiver".into())
    })?;
    let backing = runtime.intern_property_name("__memory");
    match runtime.objects().get_property(receiver, backing) {
        Ok(Some(lookup)) => match lookup.value() {
            PropertyValue::Data(value) => Ok(value),
            PropertyValue::Accessor { .. } => Ok(RegisterValue::undefined()),
        },
        Ok(None) => Ok(RegisterValue::undefined()),
        Err(error) => Err(VmNativeCallError::Internal(
            format!("Math.memory getter failed: {error:?}").into(),
        )),
    }
}

fn math_memory_setter(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let receiver = this.as_object_handle().map(ObjectHandle).ok_or_else(|| {
        VmNativeCallError::Internal("Math.memory setter requires object receiver".into())
    })?;
    let backing = runtime.intern_property_name("__memory");
    let value = args
        .first()
        .copied()
        .unwrap_or_else(RegisterValue::undefined);
    runtime
        .objects_mut()
        .set_property(receiver, backing, value)
        .map_err(|error| {
            VmNativeCallError::Internal(format!("Math.memory setter failed: {error:?}").into())
        })?;
    Ok(RegisterValue::undefined())
}
