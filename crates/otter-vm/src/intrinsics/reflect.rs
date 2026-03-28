//! ES2024 §28.1 The Reflect Object
//!
//! Implements the Reflect namespace with all 13 function properties.

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

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn require_object(
    args: &[RegisterValue],
    index: usize,
    runtime: &mut crate::interpreter::RuntimeState,
    _method: &str,
) -> Result<ObjectHandle, VmNativeCallError> {
    let Some(handle) = args
        .get(index)
        .copied()
        .and_then(RegisterValue::as_object_handle)
        .map(ObjectHandle)
    else {
        return Err(VmNativeCallError::Thrown(RegisterValue::undefined()));
    };

    if matches!(
        runtime.objects().kind(handle),
        Ok(crate::object::HeapValueKind::String)
    ) {
        return Err(VmNativeCallError::Thrown(RegisterValue::undefined()));
    }

    Ok(handle)
}

fn to_property_key(
    args: &[RegisterValue],
    index: usize,
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<crate::property::PropertyNameId, VmNativeCallError> {
    let value = args
        .get(index)
        .copied()
        .unwrap_or_else(RegisterValue::undefined);
    crate::abstract_ops::to_property_key(runtime, value)
}

// ---------------------------------------------------------------------------
// Installer
// ---------------------------------------------------------------------------

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

fn method(
    name: &str,
    length: u16,
    callback: crate::descriptors::VmNativeFunction,
) -> NativeBindingDescriptor {
    NativeBindingDescriptor::new(
        NativeBindingTarget::Namespace,
        NativeFunctionDescriptor::method(name, length, callback),
    )
}

fn reflect_namespace_bindings() -> Vec<NativeBindingDescriptor> {
    vec![
        method("apply", 3, reflect_apply),
        method("construct", 2, reflect_construct),
        method("defineProperty", 3, reflect_define_property),
        method("deleteProperty", 2, reflect_delete_property),
        method("get", 2, reflect_get),
        method(
            "getOwnPropertyDescriptor",
            2,
            reflect_get_own_property_descriptor,
        ),
        method("getPrototypeOf", 1, reflect_get_prototype_of),
        method("has", 2, reflect_has),
        method("isExtensible", 1, reflect_is_extensible),
        method("ownKeys", 1, reflect_own_keys),
        method("preventExtensions", 1, reflect_prevent_extensions),
        method("set", 3, reflect_set),
        method("setPrototypeOf", 2, reflect_set_prototype_of),
    ]
}

// ---------------------------------------------------------------------------
// §28.1.1 Reflect.apply(target, thisArgument, argumentsList)
// ---------------------------------------------------------------------------
fn reflect_apply(
    _this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let target = args
        .first()
        .copied()
        .ok_or_else(|| VmNativeCallError::Internal("Reflect.apply requires target".into()))?;
    let this_arg = args
        .get(1)
        .copied()
        .unwrap_or_else(RegisterValue::undefined);

    // Extract arguments from argumentsList (arg 2).
    let call_args = if let Some(args_list) = args.get(2).copied()
        && let Some(handle) = args_list.as_object_handle().map(ObjectHandle)
    {
        runtime.array_to_args(handle)?
    } else {
        Vec::new()
    };

    // Call via host function bridge (works for host functions; closures need interpreter).
    let target_handle = target.as_object_handle().map(ObjectHandle).ok_or_else(|| {
        VmNativeCallError::Internal("Reflect.apply target must be callable".into())
    })?;
    runtime.call_host_function(Some(target_handle), this_arg, &call_args)
}

// ---------------------------------------------------------------------------
// §28.1.2 Reflect.construct(target, argumentsList [, newTarget])
// ---------------------------------------------------------------------------
fn reflect_construct(
    _this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let _target = require_object(args, 0, runtime, "Reflect.construct")?;
    // Full construct requires interpreter-level call_function_construct.
    // For now, return a useful error instead of crashing.
    Err(VmNativeCallError::Internal(
        "Reflect.construct not yet fully implemented (requires interpreter call bridge)".into(),
    ))
}

// ---------------------------------------------------------------------------
// §28.1.3 Reflect.defineProperty(target, propertyKey, attributes)
// ---------------------------------------------------------------------------
fn reflect_define_property(
    _this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let target = require_object(args, 0, runtime, "Reflect.defineProperty")?;
    let property = to_property_key(args, 1, runtime)?;
    let attrs = args
        .get(2)
        .copied()
        .and_then(RegisterValue::as_object_handle)
        .map(ObjectHandle);

    let desc = crate::abstract_ops::to_property_descriptor(attrs, runtime)?;
    let property_names = runtime.property_names().clone();
    let success = runtime
        .objects_mut()
        .define_own_property_from_descriptor_with_registry(target, property, desc, &property_names)
        .map_err(|e| {
            VmNativeCallError::Internal(format!("Reflect.defineProperty failed: {e:?}").into())
        })?;

    Ok(RegisterValue::from_bool(success))
}

// ---------------------------------------------------------------------------
// §28.1.4 Reflect.deleteProperty(target, propertyKey)
// ---------------------------------------------------------------------------
fn reflect_delete_property(
    _this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let target = require_object(args, 0, runtime, "Reflect.deleteProperty")?;
    let property = to_property_key(args, 1, runtime)?;
    let property_names = runtime.property_names().clone();
    let deleted = runtime
        .objects_mut()
        .delete_property_with_registry(target, property, &property_names)
        .map_err(|e| {
            VmNativeCallError::Internal(format!("Reflect.deleteProperty failed: {e:?}").into())
        })?;
    Ok(RegisterValue::from_bool(deleted))
}

// ---------------------------------------------------------------------------
// §28.1.5 Reflect.get(target, propertyKey [, receiver])
// ---------------------------------------------------------------------------
fn reflect_get(
    _this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let target = require_object(args, 0, runtime, "Reflect.get")?;
    let property = to_property_key(args, 1, runtime)?;
    let receiver = args
        .get(2)
        .copied()
        .unwrap_or_else(|| RegisterValue::from_object_handle(target.0));
    runtime.ordinary_get(target, property, receiver)
}

// ---------------------------------------------------------------------------
// §28.1.6 Reflect.getOwnPropertyDescriptor(target, propertyKey)
// ---------------------------------------------------------------------------
fn reflect_get_own_property_descriptor(
    _this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let target = require_object(args, 0, runtime, "Reflect.getOwnPropertyDescriptor")?;
    let property = to_property_key(args, 1, runtime)?;

    let Some(descriptor) = runtime
        .own_property_descriptor(target, property)
        .map_err(|e| {
            VmNativeCallError::Internal(
                format!("Reflect.getOwnPropertyDescriptor failed: {e:?}").into(),
            )
        })?
    else {
        return Ok(RegisterValue::undefined());
    };

    crate::abstract_ops::from_property_descriptor(descriptor, runtime)
}

// ---------------------------------------------------------------------------
// §28.1.7 Reflect.getPrototypeOf(target)
// ---------------------------------------------------------------------------
fn reflect_get_prototype_of(
    _this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let target = require_object(args, 0, runtime, "Reflect.getPrototypeOf")?;
    let proto = runtime.objects().get_prototype(target).map_err(|e| {
        VmNativeCallError::Internal(format!("Reflect.getPrototypeOf failed: {e:?}").into())
    })?;
    Ok(proto
        .map(|h| RegisterValue::from_object_handle(h.0))
        .unwrap_or_else(RegisterValue::null))
}

// ---------------------------------------------------------------------------
// §28.1.8 Reflect.has(target, propertyKey)
// ---------------------------------------------------------------------------
fn reflect_has(
    _this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let target = require_object(args, 0, runtime, "Reflect.has")?;
    let property = to_property_key(args, 1, runtime)?;
    let found = runtime
        .has_property(target, property)
        .map_err(|e| VmNativeCallError::Internal(format!("Reflect.has failed: {e:?}").into()))?;
    Ok(RegisterValue::from_bool(found))
}

// ---------------------------------------------------------------------------
// §28.1.9 Reflect.isExtensible(target)
// ---------------------------------------------------------------------------
fn reflect_is_extensible(
    _this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let target = require_object(args, 0, runtime, "Reflect.isExtensible")?;
    let extensible = runtime.objects().is_extensible(target).map_err(|e| {
        VmNativeCallError::Internal(format!("Reflect.isExtensible failed: {e:?}").into())
    })?;
    Ok(RegisterValue::from_bool(extensible))
}

// ---------------------------------------------------------------------------
// §28.1.10 Reflect.ownKeys(target)
// ---------------------------------------------------------------------------
fn reflect_own_keys(
    _this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let target = require_object(args, 0, runtime, "Reflect.ownKeys")?;
    let keys = runtime.own_property_keys(target).map_err(|e| {
        VmNativeCallError::Internal(format!("Reflect.ownKeys failed: {e:?}").into())
    })?;

    // Collect key names first to avoid borrow conflict.
    let key_names: Vec<String> = keys
        .iter()
        .map(|key_id| {
            runtime
                .property_names()
                .get(*key_id)
                .unwrap_or("<unknown>")
                .to_string()
        })
        .collect();

    let array = runtime.alloc_array();
    for name in &key_names {
        let str_handle = runtime.alloc_string(name.as_str());
        runtime
            .objects_mut()
            .push_element(array, RegisterValue::from_object_handle(str_handle.0))
            .ok();
    }

    Ok(RegisterValue::from_object_handle(array.0))
}

// ---------------------------------------------------------------------------
// §28.1.11 Reflect.preventExtensions(target)
// ---------------------------------------------------------------------------
fn reflect_prevent_extensions(
    _this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let target = require_object(args, 0, runtime, "Reflect.preventExtensions")?;
    let success = runtime
        .objects_mut()
        .prevent_extensions(target)
        .map_err(|e| {
            VmNativeCallError::Internal(format!("Reflect.preventExtensions failed: {e:?}").into())
        })?;
    Ok(RegisterValue::from_bool(success))
}

// ---------------------------------------------------------------------------
// §28.1.12 Reflect.set(target, propertyKey, V [, receiver])
// ---------------------------------------------------------------------------
fn reflect_set(
    _this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let target = require_object(args, 0, runtime, "Reflect.set")?;
    let property = to_property_key(args, 1, runtime)?;
    let value = args
        .get(2)
        .copied()
        .unwrap_or_else(RegisterValue::undefined);
    let receiver = args
        .get(3)
        .copied()
        .unwrap_or_else(|| RegisterValue::from_object_handle(target.0));
    let success = runtime.ordinary_set(target, property, receiver, value)?;
    Ok(RegisterValue::from_bool(success))
}

// ---------------------------------------------------------------------------
// §28.1.13 Reflect.setPrototypeOf(target, proto)
// ---------------------------------------------------------------------------
fn reflect_set_prototype_of(
    _this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let target = require_object(args, 0, runtime, "Reflect.setPrototypeOf")?;
    let proto_arg = args
        .get(1)
        .copied()
        .unwrap_or_else(RegisterValue::undefined);
    let proto = if proto_arg == RegisterValue::null() {
        None
    } else {
        Some(
            proto_arg
                .as_object_handle()
                .map(ObjectHandle)
                .ok_or_else(|| {
                    VmNativeCallError::Internal(
                        "Reflect.setPrototypeOf proto must be object or null".into(),
                    )
                })?,
        )
    };
    runtime
        .objects_mut()
        .set_prototype(target, proto)
        .map_err(|e| {
            VmNativeCallError::Internal(format!("Reflect.setPrototypeOf failed: {e:?}").into())
        })
        .map(RegisterValue::from_bool)
}
