use crate::builders::ClassBuilder;
use crate::descriptors::{
    JsClassDescriptor, NativeBindingDescriptor, NativeBindingTarget, NativeFunctionDescriptor,
    VmNativeCallError,
};
use crate::object::{HeapValueKind, ObjectHandle};
use crate::value::RegisterValue;

use super::{
    IntrinsicsError, VmIntrinsics,
    install::{IntrinsicInstallContext, IntrinsicInstaller, install_class_plan},
};

pub(super) static ARRAY_INTRINSIC: ArrayIntrinsic = ArrayIntrinsic;

pub(super) struct ArrayIntrinsic;

impl IntrinsicInstaller for ArrayIntrinsic {
    fn init(
        &self,
        intrinsics: &mut VmIntrinsics,
        cx: &mut IntrinsicInstallContext<'_>,
    ) -> Result<(), IntrinsicsError> {
        let descriptor = array_class_descriptor();
        let plan = ClassBuilder::from_descriptor(&descriptor)
            .expect("Array class descriptors should normalize")
            .build();

        let constructor = if let Some(descriptor) = plan.constructor() {
            let host_function = cx.native_functions.register(descriptor.clone());
            cx.alloc_intrinsic_host_function(host_function, intrinsics.function_prototype())?
        } else {
            cx.alloc_intrinsic_object(Some(intrinsics.object_prototype()))?
        };

        intrinsics.array_constructor = constructor;
        install_class_plan(
            intrinsics.array_prototype(),
            intrinsics.array_constructor(),
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
            "Array",
            RegisterValue::from_object_handle(intrinsics.array_constructor().0),
        )
    }
}

fn array_class_descriptor() -> JsClassDescriptor {
    JsClassDescriptor::new("Array")
        .with_constructor(NativeFunctionDescriptor::constructor(
            "Array",
            1,
            array_constructor,
        ))
        .with_binding(NativeBindingDescriptor::new(
            NativeBindingTarget::Constructor,
            NativeFunctionDescriptor::method("isArray", 1, array_is_array),
        ))
        .with_binding(NativeBindingDescriptor::new(
            NativeBindingTarget::Prototype,
            NativeFunctionDescriptor::method("push", 1, array_push),
        ))
}

fn array_constructor(
    _this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let array = runtime.alloc_array();

    if let [length] = args
        && let Some(length) = length.as_i32()
        && length >= 0
    {
        if length > 0 {
            runtime
                .objects_mut()
                .set_index(
                    array,
                    usize::try_from(length - 1).unwrap_or(usize::MAX),
                    RegisterValue::undefined(),
                )
                .map_err(|error| {
                    VmNativeCallError::Internal(
                        format!("Array constructor length setup failed: {error:?}").into(),
                    )
                })?;
        }
        return Ok(RegisterValue::from_object_handle(array.0));
    }

    for (index, value) in args.iter().copied().enumerate() {
        runtime
            .objects_mut()
            .set_index(array, index, value)
            .map_err(|error| {
                VmNativeCallError::Internal(
                    format!("Array constructor element store failed: {error:?}").into(),
                )
            })?;
    }

    Ok(RegisterValue::from_object_handle(array.0))
}

fn array_is_array(
    _this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let is_array = args
        .first()
        .copied()
        .and_then(RegisterValue::as_object_handle)
        .map(ObjectHandle)
        .map(|handle| matches!(runtime.objects().kind(handle), Ok(HeapValueKind::Array)))
        .unwrap_or(false);
    Ok(RegisterValue::from_bool(is_array))
}

fn array_push(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let receiver = this.as_object_handle().map(ObjectHandle).ok_or_else(|| {
        VmNativeCallError::Internal("Array.prototype.push requires array receiver".into())
    })?;
    if !matches!(runtime.objects().kind(receiver), Ok(HeapValueKind::Array)) {
        return Err(VmNativeCallError::Internal(
            "Array.prototype.push requires array receiver".into(),
        ));
    }

    let start = runtime
        .objects()
        .array_length(receiver)
        .map_err(|error| {
            VmNativeCallError::Internal(
                format!("Array.prototype.push length lookup failed: {error:?}").into(),
            )
        })?
        .ok_or_else(|| {
            VmNativeCallError::Internal("Array.prototype.push requires array receiver".into())
        })?;

    for (offset, value) in args.iter().copied().enumerate() {
        runtime
            .objects_mut()
            .set_index(receiver, start.saturating_add(offset), value)
            .map_err(|error| {
                VmNativeCallError::Internal(
                    format!("Array.prototype.push element store failed: {error:?}").into(),
                )
            })?;
    }

    Ok(RegisterValue::from_i32(
        i32::try_from(start.saturating_add(args.len())).unwrap_or(i32::MAX),
    ))
}
