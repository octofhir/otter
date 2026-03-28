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
        .with_binding(NativeBindingDescriptor::new(
            NativeBindingTarget::Prototype,
            NativeFunctionDescriptor::method("join", 1, array_join),
        ))
        .with_binding(NativeBindingDescriptor::new(
            NativeBindingTarget::Prototype,
            NativeFunctionDescriptor::method("indexOf", 1, array_index_of),
        ))
        .with_binding(NativeBindingDescriptor::new(
            NativeBindingTarget::Prototype,
            NativeFunctionDescriptor::method("concat", 1, array_concat),
        ))
        .with_binding(NativeBindingDescriptor::new(
            NativeBindingTarget::Prototype,
            NativeFunctionDescriptor::method("slice", 2, array_slice),
        ))
}

fn array_constructor(
    _this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let array = runtime.alloc_array();

    if let [length] = args {
        if let Some(length) = length.as_i32() {
            if length < 0 {
                return Err(invalid_array_length_error(runtime));
            }
            runtime
                .objects_mut()
                .set_array_length(array, usize::try_from(length).unwrap_or(usize::MAX))
                .map_err(|error| {
                    VmNativeCallError::Internal(
                        format!("Array constructor length setup failed: {error:?}").into(),
                    )
                })?;
            return Ok(RegisterValue::from_object_handle(array.0));
        }

        if let Some(length) = length.as_number() {
            if !is_valid_array_length(length) {
                return Err(invalid_array_length_error(runtime));
            }
            runtime
                .objects_mut()
                .set_array_length(array, length as usize)
                .map_err(|error| {
                    VmNativeCallError::Internal(
                        format!("Array constructor length setup failed: {error:?}").into(),
                    )
                })?;
            return Ok(RegisterValue::from_object_handle(array.0));
        }
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

/// ES2024 §23.1.3.15 Array.prototype.join(separator)
fn array_join(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let receiver = this.as_object_handle().map(ObjectHandle).ok_or_else(|| {
        VmNativeCallError::Internal("Array.prototype.join requires array receiver".into())
    })?;
    let length = array_length(receiver, runtime, "Array.prototype.join")?;

    let separator = if let Some(sep_arg) = args.first().copied() {
        if sep_arg == RegisterValue::undefined() {
            ",".to_string()
        } else {
            runtime.js_to_string_infallible(sep_arg).to_string()
        }
    } else {
        ",".to_string()
    };

    let mut parts = Vec::with_capacity(length);
    for index in 0..length {
        let value = array_index_value(receiver, index, runtime, "Array.prototype.join")?;
        let part = match value {
            None => String::new(),
            Some(value)
                if value == RegisterValue::undefined() || value == RegisterValue::null() =>
            {
                String::new()
            }
            Some(value) => runtime.js_to_string_infallible(value).to_string(),
        };
        parts.push(part);
    }

    let result = parts.join(&separator);
    let handle = runtime.alloc_string(result);
    Ok(RegisterValue::from_object_handle(handle.0))
}

/// ES2024 §23.1.3.14 Array.prototype.indexOf(searchElement [, fromIndex])
fn array_index_of(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let receiver = this.as_object_handle().map(ObjectHandle).ok_or_else(|| {
        VmNativeCallError::Internal("Array.prototype.indexOf requires array receiver".into())
    })?;
    let length = array_length(receiver, runtime, "Array.prototype.indexOf")?;

    let search = args
        .first()
        .copied()
        .unwrap_or_else(RegisterValue::undefined);
    let from = args
        .get(1)
        .copied()
        .and_then(RegisterValue::as_i32)
        .unwrap_or(0);
    let start = if from < 0 {
        (length as i32 + from).max(0) as usize
    } else {
        from as usize
    };

    for index in start..length {
        let Some(elem) = array_index_value(receiver, index, runtime, "Array.prototype.indexOf")?
        else {
            continue;
        };
        if elem == search {
            return Ok(RegisterValue::from_i32(index as i32));
        }
    }
    Ok(RegisterValue::from_i32(-1))
}

/// ES2024 §23.1.3.1 Array.prototype.concat(...items)
fn array_concat(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let receiver = this.as_object_handle().map(ObjectHandle).ok_or_else(|| {
        VmNativeCallError::Internal("Array.prototype.concat requires array receiver".into())
    })?;
    let base_len = array_length(receiver, runtime, "Array.prototype.concat")?;
    let result = runtime.alloc_array();
    runtime
        .objects_mut()
        .set_array_length(result, base_len)
        .ok();
    for index in 0..base_len {
        if let Some(elem) = array_index_value(receiver, index, runtime, "Array.prototype.concat")? {
            runtime.objects_mut().set_index(result, index, elem).ok();
        }
    }
    let mut next_index = base_len;
    for arg in args {
        if let Some(handle) = arg.as_object_handle().map(ObjectHandle)
            && matches!(runtime.objects().kind(handle), Ok(HeapValueKind::Array))
        {
            let arg_len = array_length(handle, runtime, "Array.prototype.concat")?;
            runtime
                .objects_mut()
                .set_array_length(result, next_index.saturating_add(arg_len))
                .ok();
            for offset in 0..arg_len {
                if let Some(elem) =
                    array_index_value(handle, offset, runtime, "Array.prototype.concat")?
                {
                    runtime
                        .objects_mut()
                        .set_index(result, next_index.saturating_add(offset), elem)
                        .ok();
                }
            }
            next_index = next_index.saturating_add(arg_len);
            continue;
        }
        runtime.objects_mut().push_element(result, *arg).ok();
        next_index = next_index.saturating_add(1);
    }
    Ok(RegisterValue::from_object_handle(result.0))
}

/// ES2024 §23.1.3.26 Array.prototype.slice(start, end)
fn array_slice(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let receiver = this.as_object_handle().map(ObjectHandle).ok_or_else(|| {
        VmNativeCallError::Internal("Array.prototype.slice requires array receiver".into())
    })?;
    let len = array_length(receiver, runtime, "Array.prototype.slice")? as i32;

    let raw_start = args.first().and_then(|v| v.as_i32()).unwrap_or(0);
    let start = if raw_start < 0 {
        (len + raw_start).max(0) as usize
    } else {
        raw_start.min(len) as usize
    };

    let raw_end = args
        .get(1)
        .and_then(|v| {
            if *v == RegisterValue::undefined() {
                None
            } else {
                v.as_i32()
            }
        })
        .unwrap_or(len);
    let end = if raw_end < 0 {
        (len + raw_end).max(0) as usize
    } else {
        raw_end.min(len) as usize
    };

    let result = runtime.alloc_array();
    let count = end.saturating_sub(start);
    runtime.objects_mut().set_array_length(result, count).ok();
    for (offset, index) in (start..end).enumerate() {
        if let Some(elem) = array_index_value(receiver, index, runtime, "Array.prototype.slice")? {
            runtime.objects_mut().set_index(result, offset, elem).ok();
        }
    }
    Ok(RegisterValue::from_object_handle(result.0))
}

fn array_length(
    receiver: ObjectHandle,
    runtime: &mut crate::interpreter::RuntimeState,
    op: &str,
) -> Result<usize, VmNativeCallError> {
    runtime
        .objects()
        .array_length(receiver)
        .map_err(|error| VmNativeCallError::Internal(format!("{op}: {error:?}").into()))?
        .ok_or_else(|| VmNativeCallError::Internal(format!("{op} requires array receiver").into()))
}

fn array_index_value(
    receiver: ObjectHandle,
    index: usize,
    runtime: &mut crate::interpreter::RuntimeState,
    _op: &str,
) -> Result<Option<RegisterValue>, VmNativeCallError> {
    runtime.get_array_index_value(receiver, index)
}

fn invalid_array_length_error(runtime: &mut crate::interpreter::RuntimeState) -> VmNativeCallError {
    let prototype = runtime.intrinsics().range_error_prototype;
    let handle = runtime.alloc_object_with_prototype(Some(prototype));
    let message = runtime.alloc_string("Invalid array length");
    let message_prop = runtime.intern_property_name("message");
    runtime
        .objects_mut()
        .set_property(
            handle,
            message_prop,
            RegisterValue::from_object_handle(message.0),
        )
        .ok();
    VmNativeCallError::Thrown(RegisterValue::from_object_handle(handle.0))
}

fn is_valid_array_length(length: f64) -> bool {
    length.is_finite() && length >= 0.0 && length.fract() == 0.0 && length <= (u32::MAX - 1) as f64
}
