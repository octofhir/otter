//! Map and Set intrinsics — ES2024 §24.1 and §24.2.

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

pub(super) static MAP_SET_INTRINSIC: MapSetIntrinsic = MapSetIntrinsic;

pub(super) struct MapSetIntrinsic;

impl IntrinsicInstaller for MapSetIntrinsic {
    fn init(
        &self,
        intrinsics: &mut VmIntrinsics,
        cx: &mut IntrinsicInstallContext<'_>,
    ) -> Result<(), IntrinsicsError> {
        install_map(intrinsics, cx)?;
        install_set(intrinsics, cx)?;
        Ok(())
    }

    fn install_on_global(
        &self,
        intrinsics: &VmIntrinsics,
        cx: &mut IntrinsicInstallContext<'_>,
    ) -> Result<(), IntrinsicsError> {
        cx.install_global_value(
            intrinsics,
            "Map",
            RegisterValue::from_object_handle(intrinsics.map_constructor.0),
        )?;
        cx.install_global_value(
            intrinsics,
            "Set",
            RegisterValue::from_object_handle(intrinsics.set_constructor.0),
        )
    }
}

fn proto(
    name: &str,
    arity: u16,
    f: fn(
        &RegisterValue,
        &[RegisterValue],
        &mut crate::interpreter::RuntimeState,
    ) -> Result<RegisterValue, VmNativeCallError>,
) -> NativeBindingDescriptor {
    NativeBindingDescriptor::new(
        NativeBindingTarget::Prototype,
        NativeFunctionDescriptor::method(name, arity, f),
    )
}

// ═══════════════════════════════════════════════════════════════════════════
//  Map
// ═══════════════════════════════════════════════════════════════════════════

fn map_class_descriptor() -> JsClassDescriptor {
    JsClassDescriptor::new("Map")
        .with_constructor(NativeFunctionDescriptor::constructor("Map", 0, map_constructor))
        .with_binding(proto("get", 1, map_get))
        .with_binding(proto("set", 2, map_set))
        .with_binding(proto("has", 1, map_has))
        .with_binding(proto("delete", 1, map_delete))
        .with_binding(proto("clear", 0, map_clear))
        .with_binding(proto("forEach", 1, map_for_each))
        .with_binding(proto("keys", 0, map_keys))
        .with_binding(proto("values", 0, map_values))
        .with_binding(proto("entries", 0, map_entries))
}

fn install_map(
    intrinsics: &mut VmIntrinsics,
    cx: &mut IntrinsicInstallContext<'_>,
) -> Result<(), IntrinsicsError> {
    let descriptor = map_class_descriptor();
    let plan = ClassBuilder::from_descriptor(&descriptor)
        .expect("Map class descriptors should normalize")
        .build();

    let constructor = if let Some(desc) = plan.constructor() {
        let host_function = cx.native_functions.register(desc.clone());
        cx.alloc_intrinsic_host_function(host_function, intrinsics.function_prototype())?
    } else {
        cx.alloc_intrinsic_object(Some(intrinsics.object_prototype()))?
    };

    intrinsics.map_constructor = constructor;

    let map_prototype = cx.alloc_intrinsic_object(Some(intrinsics.object_prototype()))?;
    intrinsics.map_prototype = map_prototype;

    install_class_plan(
        map_prototype,
        constructor,
        &plan,
        intrinsics.function_prototype(),
        cx,
    )?;

    // Install size getter.
    let size_desc = NativeFunctionDescriptor::method("get size", 0, map_size_getter);
    let size_id = cx.native_functions.register(size_desc);
    let size_fn = cx.alloc_intrinsic_host_function(size_id, intrinsics.function_prototype())?;
    let size_prop = cx.property_names.intern("size");
    cx.heap.define_own_property(
        map_prototype,
        size_prop,
        crate::object::PropertyValue::Accessor {
            getter: Some(size_fn),
            setter: None,
            attributes: crate::object::PropertyAttributes::from_flags(false, true, true),
        },
    )?;

    Ok(())
}

fn map_constructor(
    _this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let prototype = Some(runtime.intrinsics().map_prototype);
    let handle = runtime.objects_mut().alloc_map(prototype);

    // If iterable argument provided, add entries.
    if let Some(iterable) = args.first().copied()
        && iterable != RegisterValue::undefined()
        && iterable != RegisterValue::null()
        && let Some(arr_handle) = iterable.as_object_handle().map(ObjectHandle)
        && matches!(runtime.objects().kind(arr_handle), Ok(HeapValueKind::Array))
    {
                    let length = runtime
                        .objects()
                        .array_length(arr_handle)
                        .map_err(|e| VmNativeCallError::Internal(format!("{e:?}").into()))?
                        .unwrap_or(0);
                    for i in 0..length {
                        let entry = runtime.get_array_index_value(arr_handle, i)?;
                        if let Some(entry_val) = entry
                            && let Some(eh) = entry_val.as_object_handle().map(ObjectHandle)
                            && matches!(runtime.objects().kind(eh), Ok(HeapValueKind::Array))
                        {
                                let key = runtime
                                    .get_array_index_value(eh, 0)?
                                    .unwrap_or_else(RegisterValue::undefined);
                                let value = runtime
                                    .get_array_index_value(eh, 1)?
                                    .unwrap_or_else(RegisterValue::undefined);
                                runtime.objects_mut().map_set(handle, key, value)
                                    .map_err(|e| VmNativeCallError::Internal(format!("{e:?}").into()))?;
                        }
                    }
    }

    Ok(RegisterValue::from_object_handle(handle.0))
}

fn map_get(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let handle = require_map(*this, runtime)?;
    let key = args.first().copied().unwrap_or_else(RegisterValue::undefined);
    runtime.objects().map_get(handle, key)
        .map_err(|e| VmNativeCallError::Internal(format!("{e:?}").into()))
}

fn map_set(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let handle = require_map(*this, runtime)?;
    let key = args.first().copied().unwrap_or_else(RegisterValue::undefined);
    let value = args.get(1).copied().unwrap_or_else(RegisterValue::undefined);
    runtime.objects_mut().map_set(handle, key, value)
        .map_err(|e| VmNativeCallError::Internal(format!("{e:?}").into()))?;
    Ok(*this)
}

fn map_has(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let handle = require_map(*this, runtime)?;
    let key = args.first().copied().unwrap_or_else(RegisterValue::undefined);
    let result = runtime.objects().map_has(handle, key)
        .map_err(|e| VmNativeCallError::Internal(format!("{e:?}").into()))?;
    Ok(RegisterValue::from_bool(result))
}

fn map_delete(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let handle = require_map(*this, runtime)?;
    let key = args.first().copied().unwrap_or_else(RegisterValue::undefined);
    let result = runtime.objects_mut().map_delete(handle, key)
        .map_err(|e| VmNativeCallError::Internal(format!("{e:?}").into()))?;
    Ok(RegisterValue::from_bool(result))
}

fn map_clear(
    this: &RegisterValue,
    _args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let handle = require_map(*this, runtime)?;
    runtime.objects_mut().map_clear(handle)
        .map_err(|e| VmNativeCallError::Internal(format!("{e:?}").into()))?;
    Ok(RegisterValue::undefined())
}

fn map_size_getter(
    this: &RegisterValue,
    _args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let handle = require_map(*this, runtime)?;
    let size = runtime.objects().map_size(handle)
        .map_err(|e| VmNativeCallError::Internal(format!("{e:?}").into()))?;
    Ok(RegisterValue::from_i32(size as i32))
}

fn map_for_each(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let handle = require_map(*this, runtime)?;
    let callback = args
        .first()
        .copied()
        .and_then(RegisterValue::as_object_handle)
        .map(ObjectHandle)
        .filter(|h| runtime.objects().is_callable(*h))
        .ok_or_else(|| VmNativeCallError::Internal("Map.prototype.forEach callback is not a function".into()))?;
    let this_arg = args.get(1).copied().unwrap_or_else(RegisterValue::undefined);

    let entries = runtime.objects().map_entries(handle)
        .map_err(|e| VmNativeCallError::Internal(format!("{e:?}").into()))?;
    for (key, value) in entries {
        runtime.call_callable(callback, this_arg, &[value, key, *this])?;
    }
    Ok(RegisterValue::undefined())
}

fn map_keys(
    this: &RegisterValue,
    _args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let handle = require_map(*this, runtime)?;
    let entries = runtime.objects().map_entries(handle)
        .map_err(|e| VmNativeCallError::Internal(format!("{e:?}").into()))?;
    let result = runtime.alloc_array();
    for (i, (key, _)) in entries.iter().enumerate() {
        runtime.objects_mut().set_index(result, i, *key).ok();
    }
    Ok(RegisterValue::from_object_handle(result.0))
}

fn map_values(
    this: &RegisterValue,
    _args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let handle = require_map(*this, runtime)?;
    let entries = runtime.objects().map_entries(handle)
        .map_err(|e| VmNativeCallError::Internal(format!("{e:?}").into()))?;
    let result = runtime.alloc_array();
    for (i, (_, value)) in entries.iter().enumerate() {
        runtime.objects_mut().set_index(result, i, *value).ok();
    }
    Ok(RegisterValue::from_object_handle(result.0))
}

fn map_entries(
    this: &RegisterValue,
    _args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let handle = require_map(*this, runtime)?;
    let entries = runtime.objects().map_entries(handle)
        .map_err(|e| VmNativeCallError::Internal(format!("{e:?}").into()))?;
    let result = runtime.alloc_array();
    for (i, (key, value)) in entries.iter().enumerate() {
        let pair = runtime.alloc_array_with_elements(&[*key, *value]);
        runtime.objects_mut().set_index(result, i, RegisterValue::from_object_handle(pair.0)).ok();
    }
    Ok(RegisterValue::from_object_handle(result.0))
}

fn require_map(
    this: RegisterValue,
    runtime: &crate::interpreter::RuntimeState,
) -> Result<ObjectHandle, VmNativeCallError> {
    this.as_object_handle()
        .map(ObjectHandle)
        .filter(|h| matches!(runtime.objects().kind(*h), Ok(HeapValueKind::Map)))
        .ok_or_else(|| VmNativeCallError::Internal("Method requires a Map receiver".into()))
}

// ═══════════════════════════════════════════════════════════════════════════
//  Set
// ═══════════════════════════════════════════════════════════════════════════

fn set_class_descriptor() -> JsClassDescriptor {
    JsClassDescriptor::new("Set")
        .with_constructor(NativeFunctionDescriptor::constructor("Set", 0, set_constructor))
        .with_binding(proto("add", 1, set_add))
        .with_binding(proto("has", 1, set_has))
        .with_binding(proto("delete", 1, set_delete))
        .with_binding(proto("clear", 0, set_clear))
        .with_binding(proto("forEach", 1, set_for_each))
        .with_binding(proto("values", 0, set_values))
        .with_binding(proto("keys", 0, set_values)) // keys === values for Set
        .with_binding(proto("entries", 0, set_entries))
}

fn install_set(
    intrinsics: &mut VmIntrinsics,
    cx: &mut IntrinsicInstallContext<'_>,
) -> Result<(), IntrinsicsError> {
    let descriptor = set_class_descriptor();
    let plan = ClassBuilder::from_descriptor(&descriptor)
        .expect("Set class descriptors should normalize")
        .build();

    let constructor = if let Some(desc) = plan.constructor() {
        let host_function = cx.native_functions.register(desc.clone());
        cx.alloc_intrinsic_host_function(host_function, intrinsics.function_prototype())?
    } else {
        cx.alloc_intrinsic_object(Some(intrinsics.object_prototype()))?
    };

    intrinsics.set_constructor = constructor;

    let set_prototype = cx.alloc_intrinsic_object(Some(intrinsics.object_prototype()))?;
    intrinsics.set_prototype = set_prototype;

    install_class_plan(
        set_prototype,
        constructor,
        &plan,
        intrinsics.function_prototype(),
        cx,
    )?;

    // Install size getter.
    let size_desc = NativeFunctionDescriptor::method("get size", 0, set_size_getter);
    let size_id = cx.native_functions.register(size_desc);
    let size_fn = cx.alloc_intrinsic_host_function(size_id, intrinsics.function_prototype())?;
    let size_prop = cx.property_names.intern("size");
    cx.heap.define_own_property(
        set_prototype,
        size_prop,
        crate::object::PropertyValue::Accessor {
            getter: Some(size_fn),
            setter: None,
            attributes: crate::object::PropertyAttributes::from_flags(false, true, true),
        },
    )?;

    Ok(())
}

fn set_constructor(
    _this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let prototype = Some(runtime.intrinsics().set_prototype);
    let handle = runtime.objects_mut().alloc_set(prototype);

    if let Some(iterable) = args.first().copied()
        && iterable != RegisterValue::undefined()
        && iterable != RegisterValue::null()
        && let Some(arr_handle) = iterable.as_object_handle().map(ObjectHandle)
        && matches!(runtime.objects().kind(arr_handle), Ok(HeapValueKind::Array))
    {
        let length = runtime
            .objects()
            .array_length(arr_handle)
            .map_err(|e| VmNativeCallError::Internal(format!("{e:?}").into()))?
            .unwrap_or(0);
        for i in 0..length {
            if let Some(value) = runtime.get_array_index_value(arr_handle, i)? {
                runtime.objects_mut().set_add(handle, value)
                    .map_err(|e| VmNativeCallError::Internal(format!("{e:?}").into()))?;
            }
        }
    }

    Ok(RegisterValue::from_object_handle(handle.0))
}

fn set_add(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let handle = require_set(*this, runtime)?;
    let value = args.first().copied().unwrap_or_else(RegisterValue::undefined);
    runtime.objects_mut().set_add(handle, value)
        .map_err(|e| VmNativeCallError::Internal(format!("{e:?}").into()))?;
    Ok(*this)
}

fn set_has(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let handle = require_set(*this, runtime)?;
    let value = args.first().copied().unwrap_or_else(RegisterValue::undefined);
    let result = runtime.objects().set_has(handle, value)
        .map_err(|e| VmNativeCallError::Internal(format!("{e:?}").into()))?;
    Ok(RegisterValue::from_bool(result))
}

fn set_delete(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let handle = require_set(*this, runtime)?;
    let value = args.first().copied().unwrap_or_else(RegisterValue::undefined);
    let result = runtime.objects_mut().set_delete(handle, value)
        .map_err(|e| VmNativeCallError::Internal(format!("{e:?}").into()))?;
    Ok(RegisterValue::from_bool(result))
}

fn set_clear(
    this: &RegisterValue,
    _args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let handle = require_set(*this, runtime)?;
    runtime.objects_mut().set_clear(handle)
        .map_err(|e| VmNativeCallError::Internal(format!("{e:?}").into()))?;
    Ok(RegisterValue::undefined())
}

fn set_size_getter(
    this: &RegisterValue,
    _args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let handle = require_set(*this, runtime)?;
    let size = runtime.objects().set_size(handle)
        .map_err(|e| VmNativeCallError::Internal(format!("{e:?}").into()))?;
    Ok(RegisterValue::from_i32(size as i32))
}

fn set_for_each(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let handle = require_set(*this, runtime)?;
    let callback = args
        .first()
        .copied()
        .and_then(RegisterValue::as_object_handle)
        .map(ObjectHandle)
        .filter(|h| runtime.objects().is_callable(*h))
        .ok_or_else(|| VmNativeCallError::Internal("Set.prototype.forEach callback is not a function".into()))?;
    let this_arg = args.get(1).copied().unwrap_or_else(RegisterValue::undefined);

    let values = runtime.objects().set_values(handle)
        .map_err(|e| VmNativeCallError::Internal(format!("{e:?}").into()))?;
    for value in values {
        runtime.call_callable(callback, this_arg, &[value, value, *this])?;
    }
    Ok(RegisterValue::undefined())
}

fn set_values(
    this: &RegisterValue,
    _args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let handle = require_set(*this, runtime)?;
    let values = runtime.objects().set_values(handle)
        .map_err(|e| VmNativeCallError::Internal(format!("{e:?}").into()))?;
    let result = runtime.alloc_array();
    for (i, value) in values.iter().enumerate() {
        runtime.objects_mut().set_index(result, i, *value).ok();
    }
    Ok(RegisterValue::from_object_handle(result.0))
}

fn set_entries(
    this: &RegisterValue,
    _args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let handle = require_set(*this, runtime)?;
    let values = runtime.objects().set_values(handle)
        .map_err(|e| VmNativeCallError::Internal(format!("{e:?}").into()))?;
    let result = runtime.alloc_array();
    for (i, value) in values.iter().enumerate() {
        let pair = runtime.alloc_array_with_elements(&[*value, *value]);
        runtime.objects_mut().set_index(result, i, RegisterValue::from_object_handle(pair.0)).ok();
    }
    Ok(RegisterValue::from_object_handle(result.0))
}

fn require_set(
    this: RegisterValue,
    runtime: &crate::interpreter::RuntimeState,
) -> Result<ObjectHandle, VmNativeCallError> {
    this.as_object_handle()
        .map(ObjectHandle)
        .filter(|h| matches!(runtime.objects().kind(*h), Ok(HeapValueKind::Set)))
        .ok_or_else(|| VmNativeCallError::Internal("Method requires a Set receiver".into()))
}
