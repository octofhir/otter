use crate::builders::ClassBuilder;
use crate::descriptors::{
    JsClassDescriptor, NativeBindingDescriptor, NativeBindingTarget, NativeFunctionDescriptor,
    VmNativeCallError,
};
use crate::value::RegisterValue;

use super::{
    IntrinsicsError, VmIntrinsics,
    install::{IntrinsicInstallContext, IntrinsicInstaller, install_class_plan},
};

pub(super) static OBJECT_INTRINSIC: ObjectIntrinsic = ObjectIntrinsic;

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
            cx.heap.alloc_host_function(host_function)
        } else {
            cx.heap.alloc_object()
        };

        intrinsics.object_constructor = constructor;
        install_class_plan(
            intrinsics.object_prototype(),
            intrinsics.object_constructor(),
            &plan,
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
            NativeFunctionDescriptor::method("valueOf", 0, object_value_of),
        ))
        .with_binding(NativeBindingDescriptor::new(
            NativeBindingTarget::Constructor,
            NativeFunctionDescriptor::method("create", 0, object_create),
        ))
}

fn object_constructor(
    _this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    if let Some(value) = args.first().copied()
        && value.as_object_handle().is_some()
    {
        return Ok(value);
    }

    let object = runtime.objects_mut().alloc_object();
    Ok(RegisterValue::from_object_handle(object.0))
}

fn object_value_of(
    this: &RegisterValue,
    _args: &[RegisterValue],
    _runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    Ok(*this)
}

fn object_create(
    _this: &RegisterValue,
    _args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let object = runtime.objects_mut().alloc_object();
    Ok(RegisterValue::from_object_handle(object.0))
}
