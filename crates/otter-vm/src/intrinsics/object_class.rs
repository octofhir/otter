use crate::builders::ClassBuilder;
use crate::descriptors::{
    JsClassDescriptor, NativeBindingDescriptor, NativeBindingTarget, NativeFunctionDescriptor,
    VmNativeCallError,
};
use crate::object::{HeapValueKind, ObjectHandle};
use crate::value::RegisterValue;

use super::{
    IntrinsicsError, VmIntrinsics,
    boolean_class::box_boolean_object,
    install::{IntrinsicInstallContext, IntrinsicInstaller, install_class_plan},
    number_class::box_number_object,
    string_class::box_string_object,
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
