use crate::builders::NamespaceBuilder;
use crate::descriptors::{
    NativeBindingDescriptor, NativeBindingTarget, NativeFunctionDescriptor, VmNativeCallError,
};
use crate::object::ObjectHandle;
use crate::value::RegisterValue;

use super::{
    IntrinsicsError, VmIntrinsics,
    install::{IntrinsicInstallContext, IntrinsicInstaller, install_object_plan},
};

pub(super) static REFLECT_INTRINSIC: ReflectIntrinsic = ReflectIntrinsic;

pub(super) struct ReflectIntrinsic;

impl IntrinsicInstaller for ReflectIntrinsic {
    fn init(
        &self,
        intrinsics: &mut VmIntrinsics,
        cx: &mut IntrinsicInstallContext<'_>,
    ) -> Result<(), IntrinsicsError> {
        let reflect_namespace = cx.alloc_intrinsic_object(Some(intrinsics.object_prototype()))?;
        let reflect_plan = NamespaceBuilder::from_bindings(&reflect_namespace_bindings())
            .expect("Reflect namespace descriptors should normalize")
            .build();
        install_object_plan(
            reflect_namespace,
            &reflect_plan,
            intrinsics.function_prototype(),
            cx,
        )?;
        intrinsics.set_reflect_namespace(reflect_namespace);
        Ok(())
    }

    fn install_on_global(
        &self,
        intrinsics: &VmIntrinsics,
        cx: &mut IntrinsicInstallContext<'_>,
    ) -> Result<(), IntrinsicsError> {
        let reflect_namespace = intrinsics
            .reflect_namespace()
            .expect("Reflect namespace should be installed during init_core");
        cx.install_global_value(
            intrinsics,
            "Reflect",
            RegisterValue::from_object_handle(reflect_namespace.0),
        )
    }
}

fn reflect_namespace_bindings() -> Vec<NativeBindingDescriptor> {
    vec![
        NativeBindingDescriptor::new(
            NativeBindingTarget::Namespace,
            NativeFunctionDescriptor::method("get", 2, reflect_get),
        ),
        NativeBindingDescriptor::new(
            NativeBindingTarget::Namespace,
            NativeFunctionDescriptor::method("set", 3, reflect_set),
        ),
    ]
}

fn reflect_get(
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
            VmNativeCallError::Internal("Reflect.get target must be an object".into())
        })?;
    let property = args
        .get(1)
        .copied()
        .ok_or_else(|| VmNativeCallError::Internal("Reflect.get requires property key".into()))?;
    let receiver = args
        .get(2)
        .copied()
        .unwrap_or_else(|| RegisterValue::from_object_handle(target.0))
        .as_object_handle()
        .map(ObjectHandle)
        .ok_or_else(|| {
            VmNativeCallError::Internal("Reflect.get receiver must be an object".into())
        })?;
    let property = runtime.property_name_from_value(property)?;
    runtime.ordinary_get(target, property, receiver)
}

fn reflect_set(
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
            VmNativeCallError::Internal("Reflect.set target must be an object".into())
        })?;
    let property = args
        .get(1)
        .copied()
        .ok_or_else(|| VmNativeCallError::Internal("Reflect.set requires property key".into()))?;
    let value = args
        .get(2)
        .copied()
        .unwrap_or_else(RegisterValue::undefined);
    let receiver = args
        .get(3)
        .copied()
        .unwrap_or_else(|| RegisterValue::from_object_handle(target.0))
        .as_object_handle()
        .map(ObjectHandle)
        .ok_or_else(|| {
            VmNativeCallError::Internal("Reflect.set receiver must be an object".into())
        })?;
    let property = runtime.property_name_from_value(property)?;
    runtime.ordinary_set(target, property, receiver, value)?;
    Ok(RegisterValue::from_bool(true))
}
