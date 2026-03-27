//! ES2024 §28.1 The Reflect Object
//!
//! Implements the Reflect namespace with all 13 function properties.

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

pub(super) static REFLECT_INTRINSIC: ReflectIntrinsic = ReflectIntrinsic;

pub(super) struct ReflectIntrinsic;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn require_object(
    args: &[RegisterValue],
    index: usize,
    _method: &str,
) -> Result<ObjectHandle, VmNativeCallError> {
    args.get(index)
        .copied()
        .and_then(RegisterValue::as_object_handle)
        .map(ObjectHandle)
        .ok_or_else(|| {
            VmNativeCallError::Thrown(RegisterValue::undefined()) // TypeError
        })
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
    runtime.property_name_from_value(value)
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
        method("getOwnPropertyDescriptor", 2, reflect_get_own_property_descriptor),
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
        .ok_or_else(|| {
            VmNativeCallError::Internal("Reflect.apply requires target".into())
        })?;
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
    _runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let _target = require_object(args, 0, "Reflect.construct")?;
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
    let target = require_object(args, 0, "Reflect.defineProperty")?;
    let property = to_property_key(args, 1, runtime)?;
    let attrs = args
        .get(2)
        .copied()
        .and_then(RegisterValue::as_object_handle)
        .map(ObjectHandle);

    // Extract descriptor fields from attributes object.
    if let Some(attrs) = attrs {
        let value_key = runtime.intern_property_name("value");
        let value = runtime
            .objects()
            .get_property(attrs, value_key)
            .ok()
            .flatten()
            .and_then(|l| match l.value() {
                PropertyValue::Data { value: v, .. } => Some(v),
                _ => None,
            });

        let get_key = runtime.intern_property_name("get");
        let getter = runtime
            .objects()
            .get_property(attrs, get_key)
            .ok()
            .flatten()
            .and_then(|l| match l.value() {
                PropertyValue::Data { value: v, .. } => v.as_object_handle().map(ObjectHandle),
                _ => None,
            });

        let set_key = runtime.intern_property_name("set");
        let setter = runtime
            .objects()
            .get_property(attrs, set_key)
            .ok()
            .flatten()
            .and_then(|l| match l.value() {
                PropertyValue::Data { value: v, .. } => v.as_object_handle().map(ObjectHandle),
                _ => None,
            });

        if getter.is_some() || setter.is_some() {
            // Accessor descriptor.
            runtime
                .objects_mut()
                .define_accessor(target, property, getter, setter)
                .map_err(|e| {
                    VmNativeCallError::Internal(
                        format!("Reflect.defineProperty accessor failed: {e:?}").into(),
                    )
                })?;
        } else if let Some(value) = value {
            // Data descriptor.
            runtime
                .objects_mut()
                .set_property(target, property, value)
                .map_err(|e| {
                    VmNativeCallError::Internal(
                        format!("Reflect.defineProperty data failed: {e:?}").into(),
                    )
                })?;
        }
    }

    Ok(RegisterValue::from_bool(true))
}

// ---------------------------------------------------------------------------
// §28.1.4 Reflect.deleteProperty(target, propertyKey)
// ---------------------------------------------------------------------------
fn reflect_delete_property(
    _this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let target = require_object(args, 0, "Reflect.deleteProperty")?;
    let property = to_property_key(args, 1, runtime)?;
    let deleted = runtime
        .objects_mut()
        .delete_property(target, property)
        .map_err(|e| {
            VmNativeCallError::Internal(
                format!("Reflect.deleteProperty failed: {e:?}").into(),
            )
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
    let target = require_object(args, 0, "Reflect.get")?;
    let property = to_property_key(args, 1, runtime)?;
    let receiver = args
        .get(2)
        .copied()
        .unwrap_or_else(|| RegisterValue::from_object_handle(target.0))
        .as_object_handle()
        .map(ObjectHandle)
        .unwrap_or(target);
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
    let target = require_object(args, 0, "Reflect.getOwnPropertyDescriptor")?;
    let property = to_property_key(args, 1, runtime)?;

    // Check own property only (don't walk prototype chain).
    let has_own = runtime.objects().has_own_property(target, property).map_err(|e| {
        VmNativeCallError::Internal(format!("Reflect.getOwnPropertyDescriptor failed: {e:?}").into())
    })?;

    if !has_own {
        return Ok(RegisterValue::undefined());
    }

    // Get the property value (own only — use get_property which walks prototype, but we
    // already confirmed it's own).
    let lookup = runtime
        .objects()
        .get_property(target, property)
        .map_err(|e| {
            VmNativeCallError::Internal(format!("descriptor lookup failed: {e:?}").into())
        })?;

    let Some(lookup) = lookup else {
        return Ok(RegisterValue::undefined());
    };

    // Build descriptor object.
    let desc = runtime.alloc_object_with_prototype(
        Some(runtime.intrinsics().object_prototype()),
    );

    match lookup.value() {
        PropertyValue::Data { value, .. } => {
            let value_key = runtime.intern_property_name("value");
            runtime.objects_mut().set_property(desc, value_key, value).ok();

            let writable_key = runtime.intern_property_name("writable");
            runtime
                .objects_mut()
                .set_property(desc, writable_key, RegisterValue::from_bool(true))
                .ok();
        }
        PropertyValue::Accessor { getter, setter, .. } => {
            let get_key = runtime.intern_property_name("get");
            let get_val = getter
                .map(|h| RegisterValue::from_object_handle(h.0))
                .unwrap_or_else(RegisterValue::undefined);
            runtime.objects_mut().set_property(desc, get_key, get_val).ok();

            let set_key = runtime.intern_property_name("set");
            let set_val = setter
                .map(|h| RegisterValue::from_object_handle(h.0))
                .unwrap_or_else(RegisterValue::undefined);
            runtime.objects_mut().set_property(desc, set_key, set_val).ok();
        }
    }

    let enumerable_key = runtime.intern_property_name("enumerable");
    runtime
        .objects_mut()
        .set_property(desc, enumerable_key, RegisterValue::from_bool(true))
        .ok();

    let configurable_key = runtime.intern_property_name("configurable");
    runtime
        .objects_mut()
        .set_property(desc, configurable_key, RegisterValue::from_bool(true))
        .ok();

    Ok(RegisterValue::from_object_handle(desc.0))
}

// ---------------------------------------------------------------------------
// §28.1.7 Reflect.getPrototypeOf(target)
// ---------------------------------------------------------------------------
fn reflect_get_prototype_of(
    _this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let target = require_object(args, 0, "Reflect.getPrototypeOf")?;
    let proto = runtime
        .objects()
        .get_prototype(target)
        .map_err(|e| {
            VmNativeCallError::Internal(
                format!("Reflect.getPrototypeOf failed: {e:?}").into(),
            )
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
    let target = require_object(args, 0, "Reflect.has")?;
    let property = to_property_key(args, 1, runtime)?;
    // get_property walks prototype chain — if it finds anything, the property exists.
    let found = runtime
        .objects()
        .get_property(target, property)
        .map_err(|e| {
            VmNativeCallError::Internal(format!("Reflect.has failed: {e:?}").into())
        })?
        .is_some();
    Ok(RegisterValue::from_bool(found))
}

// ---------------------------------------------------------------------------
// §28.1.9 Reflect.isExtensible(target)
// ---------------------------------------------------------------------------
fn reflect_is_extensible(
    _this: &RegisterValue,
    args: &[RegisterValue],
    _runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let _target = require_object(args, 0, "Reflect.isExtensible")?;
    // All ordinary objects in the new VM are currently extensible.
    Ok(RegisterValue::from_bool(true))
}

// ---------------------------------------------------------------------------
// §28.1.10 Reflect.ownKeys(target)
// ---------------------------------------------------------------------------
fn reflect_own_keys(
    _this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let target = require_object(args, 0, "Reflect.ownKeys")?;
    let keys = runtime
        .objects()
        .own_keys(target)
        .map_err(|e| {
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
    _runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let _target = require_object(args, 0, "Reflect.preventExtensions")?;
    // Stub: always returns true. Full implementation needs extensibility flag.
    Ok(RegisterValue::from_bool(true))
}

// ---------------------------------------------------------------------------
// §28.1.12 Reflect.set(target, propertyKey, V [, receiver])
// ---------------------------------------------------------------------------
fn reflect_set(
    _this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let target = require_object(args, 0, "Reflect.set")?;
    let property = to_property_key(args, 1, runtime)?;
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
        .unwrap_or(target);
    runtime.ordinary_set(target, property, receiver, value)?;
    Ok(RegisterValue::from_bool(true))
}

// ---------------------------------------------------------------------------
// §28.1.13 Reflect.setPrototypeOf(target, proto)
// ---------------------------------------------------------------------------
fn reflect_set_prototype_of(
    _this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let target = require_object(args, 0, "Reflect.setPrototypeOf")?;
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
            VmNativeCallError::Internal(
                format!("Reflect.setPrototypeOf failed: {e:?}").into(),
            )
        })?;
    Ok(RegisterValue::from_bool(true))
}
