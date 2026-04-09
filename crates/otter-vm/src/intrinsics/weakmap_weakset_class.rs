//! WeakMap and WeakSet intrinsics.
//!
//! Spec references (ECMAScript 2024 / ES15):
//! - WeakMap:           <https://tc39.es/ecma262/#sec-weakmap-objects>
//! - WeakMap.prototype: <https://tc39.es/ecma262/#sec-properties-of-the-weakmap-prototype-object>
//! - WeakSet:           <https://tc39.es/ecma262/#sec-weakset-objects>
//! - WeakSet.prototype: <https://tc39.es/ecma262/#sec-properties-of-the-weakset-prototype-object>

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

pub(super) static WEAKMAP_WEAKSET_INTRINSIC: WeakMapWeakSetIntrinsic = WeakMapWeakSetIntrinsic;

pub(super) struct WeakMapWeakSetIntrinsic;

impl IntrinsicInstaller for WeakMapWeakSetIntrinsic {
    fn init(
        &self,
        intrinsics: &mut VmIntrinsics,
        cx: &mut IntrinsicInstallContext<'_>,
    ) -> Result<(), IntrinsicsError> {
        install_weakmap(intrinsics, cx)?;
        install_weakset(intrinsics, cx)?;
        Ok(())
    }

    fn install_on_global(
        &self,
        intrinsics: &VmIntrinsics,
        cx: &mut IntrinsicInstallContext<'_>,
    ) -> Result<(), IntrinsicsError> {
        cx.install_global_value(
            intrinsics,
            "WeakMap",
            RegisterValue::from_object_handle(intrinsics.weakmap_constructor.0),
        )?;
        cx.install_global_value(
            intrinsics,
            "WeakSet",
            RegisterValue::from_object_handle(intrinsics.weakset_constructor.0),
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
//  WeakMap — §24.3
//  Spec: <https://tc39.es/ecma262/#sec-weakmap-objects>
// ═══════════════════════════════════════════════════════════════════════════

fn weakmap_class_descriptor() -> JsClassDescriptor {
    JsClassDescriptor::new("WeakMap")
        .with_constructor(
            NativeFunctionDescriptor::constructor("WeakMap", 0, weakmap_constructor)
                .with_default_intrinsic(crate::intrinsics::IntrinsicKey::WeakMapPrototype),
        )
        .with_binding(proto("get", 1, weakmap_get))
        .with_binding(proto("set", 2, weakmap_set))
        .with_binding(proto("has", 1, weakmap_has))
        .with_binding(proto("delete", 1, weakmap_delete))
}

fn install_weakmap(
    intrinsics: &mut VmIntrinsics,
    cx: &mut IntrinsicInstallContext<'_>,
) -> Result<(), IntrinsicsError> {
    let descriptor = weakmap_class_descriptor();
    let plan = ClassBuilder::from_descriptor(&descriptor)
        .expect("WeakMap class descriptors should normalize")
        .build();

    let constructor = if let Some(desc) = plan.constructor() {
        let host_fn = cx.native_functions.register(desc.clone());
        cx.alloc_intrinsic_host_function(host_fn, intrinsics.function_prototype())?
    } else {
        cx.alloc_intrinsic_object(Some(intrinsics.object_prototype()))?
    };
    intrinsics.weakmap_constructor = constructor;

    let weakmap_proto = cx.alloc_intrinsic_object(Some(intrinsics.object_prototype()))?;
    intrinsics.weakmap_prototype = weakmap_proto;

    install_class_plan(
        weakmap_proto,
        constructor,
        &plan,
        intrinsics.function_prototype(),
        cx,
    )?;

    // §24.3.3.6 WeakMap.prototype[@@toStringTag] = "WeakMap"
    // Spec: <https://tc39.es/ecma262/#sec-weakmap.prototype-@@tostringtag>
    let sym_tag = cx
        .property_names
        .intern_symbol(super::WellKnownSymbol::ToStringTag.stable_id());
    let tag_str = cx.heap.alloc_string("WeakMap");
    cx.heap.define_own_property(
        weakmap_proto,
        sym_tag,
        crate::object::PropertyValue::data_with_attrs(
            RegisterValue::from_object_handle(tag_str.0),
            crate::object::PropertyAttributes::from_flags(false, false, true),
        ),
    )?;

    Ok(())
}

/// WeakMap([ iterable ])
/// Spec: <https://tc39.es/ecma262/#sec-weakmap-iterable>
fn weakmap_constructor(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let prototype =
        runtime.subclass_prototype_or_default(*this, runtime.intrinsics().weakmap_prototype);
    let handle = runtime.objects_mut().alloc_weakmap(Some(prototype));

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
                    .unwrap_or(RegisterValue::undefined());
                let value = runtime
                    .get_array_index_value(eh, 1)?
                    .unwrap_or(RegisterValue::undefined());
                let key_handle = key.as_object_handle().ok_or_else(|| {
                    VmNativeCallError::Internal("Invalid value used as weak map key".into())
                })?;
                runtime
                    .objects_mut()
                    .weakmap_set(handle, key_handle, value)
                    .map_err(|e| VmNativeCallError::Internal(format!("{e:?}").into()))?;
            }
        }
    }

    Ok(RegisterValue::from_object_handle(handle.0))
}

/// WeakMap.prototype.get(key)
/// Spec: <https://tc39.es/ecma262/#sec-weakmap.prototype.get>
fn weakmap_get(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let handle = require_weakmap(*this, runtime)?;
    let key = args.first().copied().unwrap_or(RegisterValue::undefined());
    let key_handle = match key.as_object_handle() {
        Some(h) => h,
        None => return Ok(RegisterValue::undefined()),
    };
    let result = runtime
        .objects()
        .weakmap_get(handle, key_handle)
        .map_err(|e| VmNativeCallError::Internal(format!("{e:?}").into()))?;
    Ok(result.unwrap_or(RegisterValue::undefined()))
}

/// WeakMap.prototype.set(key, value)
/// Spec: <https://tc39.es/ecma262/#sec-weakmap.prototype.set>
fn weakmap_set(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let handle = require_weakmap(*this, runtime)?;
    let key = args.first().copied().unwrap_or(RegisterValue::undefined());
    let value = args.get(1).copied().unwrap_or(RegisterValue::undefined());
    let key_handle = key
        .as_object_handle()
        .ok_or_else(|| VmNativeCallError::Internal("Invalid value used as weak map key".into()))?;
    runtime
        .objects_mut()
        .weakmap_set(handle, key_handle, value)
        .map_err(|e| VmNativeCallError::Internal(format!("{e:?}").into()))?;
    Ok(*this)
}

/// WeakMap.prototype.has(key)
/// Spec: <https://tc39.es/ecma262/#sec-weakmap.prototype.has>
fn weakmap_has(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let handle = require_weakmap(*this, runtime)?;
    let key = args.first().copied().unwrap_or(RegisterValue::undefined());
    let key_handle = match key.as_object_handle() {
        Some(h) => h,
        None => return Ok(RegisterValue::from_bool(false)),
    };
    let result = runtime
        .objects()
        .weakmap_has(handle, key_handle)
        .map_err(|e| VmNativeCallError::Internal(format!("{e:?}").into()))?;
    Ok(RegisterValue::from_bool(result))
}

/// WeakMap.prototype.delete(key)
/// Spec: <https://tc39.es/ecma262/#sec-weakmap.prototype.delete>
fn weakmap_delete(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let handle = require_weakmap(*this, runtime)?;
    let key = args.first().copied().unwrap_or(RegisterValue::undefined());
    let key_handle = match key.as_object_handle() {
        Some(h) => h,
        None => return Ok(RegisterValue::from_bool(false)),
    };
    let result = runtime
        .objects_mut()
        .weakmap_delete(handle, key_handle)
        .map_err(|e| VmNativeCallError::Internal(format!("{e:?}").into()))?;
    Ok(RegisterValue::from_bool(result))
}

fn require_weakmap(
    this: RegisterValue,
    runtime: &crate::interpreter::RuntimeState,
) -> Result<ObjectHandle, VmNativeCallError> {
    this.as_object_handle()
        .map(ObjectHandle)
        .filter(|h| matches!(runtime.objects().kind(*h), Ok(HeapValueKind::WeakMap)))
        .ok_or_else(|| VmNativeCallError::Internal("Method requires a WeakMap receiver".into()))
}

// ═══════════════════════════════════════════════════════════════════════════
//  WeakSet — §24.4
//  Spec: <https://tc39.es/ecma262/#sec-weakset-objects>
// ═══════════════════════════════════════════════════════════════════════════

fn weakset_class_descriptor() -> JsClassDescriptor {
    JsClassDescriptor::new("WeakSet")
        .with_constructor(
            NativeFunctionDescriptor::constructor("WeakSet", 0, weakset_constructor)
                .with_default_intrinsic(crate::intrinsics::IntrinsicKey::WeakSetPrototype),
        )
        .with_binding(proto("add", 1, weakset_add))
        .with_binding(proto("has", 1, weakset_has))
        .with_binding(proto("delete", 1, weakset_delete))
}

fn install_weakset(
    intrinsics: &mut VmIntrinsics,
    cx: &mut IntrinsicInstallContext<'_>,
) -> Result<(), IntrinsicsError> {
    let descriptor = weakset_class_descriptor();
    let plan = ClassBuilder::from_descriptor(&descriptor)
        .expect("WeakSet class descriptors should normalize")
        .build();

    let constructor = if let Some(desc) = plan.constructor() {
        let host_fn = cx.native_functions.register(desc.clone());
        cx.alloc_intrinsic_host_function(host_fn, intrinsics.function_prototype())?
    } else {
        cx.alloc_intrinsic_object(Some(intrinsics.object_prototype()))?
    };
    intrinsics.weakset_constructor = constructor;

    let weakset_proto = cx.alloc_intrinsic_object(Some(intrinsics.object_prototype()))?;
    intrinsics.weakset_prototype = weakset_proto;

    install_class_plan(
        weakset_proto,
        constructor,
        &plan,
        intrinsics.function_prototype(),
        cx,
    )?;

    // §24.4.3.5 WeakSet.prototype[@@toStringTag] = "WeakSet"
    // Spec: <https://tc39.es/ecma262/#sec-weakset.prototype-@@tostringtag>
    let sym_tag = cx
        .property_names
        .intern_symbol(super::WellKnownSymbol::ToStringTag.stable_id());
    let tag_str = cx.heap.alloc_string("WeakSet");
    cx.heap.define_own_property(
        weakset_proto,
        sym_tag,
        crate::object::PropertyValue::data_with_attrs(
            RegisterValue::from_object_handle(tag_str.0),
            crate::object::PropertyAttributes::from_flags(false, false, true),
        ),
    )?;

    Ok(())
}

/// WeakSet([ iterable ])
/// Spec: <https://tc39.es/ecma262/#sec-weakset-iterable>
fn weakset_constructor(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let prototype =
        runtime.subclass_prototype_or_default(*this, runtime.intrinsics().weakset_prototype);
    let handle = runtime.objects_mut().alloc_weakset(Some(prototype));

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
                let key_handle = value.as_object_handle().ok_or_else(|| {
                    VmNativeCallError::Internal("Invalid value used in weak set".into())
                })?;
                runtime
                    .objects_mut()
                    .weakset_add(handle, key_handle)
                    .map_err(|e| VmNativeCallError::Internal(format!("{e:?}").into()))?;
            }
        }
    }

    Ok(RegisterValue::from_object_handle(handle.0))
}

/// WeakSet.prototype.add(value)
/// Spec: <https://tc39.es/ecma262/#sec-weakset.prototype.add>
fn weakset_add(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let handle = require_weakset(*this, runtime)?;
    let value = args.first().copied().unwrap_or(RegisterValue::undefined());
    let key_handle = value
        .as_object_handle()
        .ok_or_else(|| VmNativeCallError::Internal("Invalid value used in weak set".into()))?;
    runtime
        .objects_mut()
        .weakset_add(handle, key_handle)
        .map_err(|e| VmNativeCallError::Internal(format!("{e:?}").into()))?;
    Ok(*this)
}

/// WeakSet.prototype.has(value)
/// Spec: <https://tc39.es/ecma262/#sec-weakset.prototype.has>
fn weakset_has(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let handle = require_weakset(*this, runtime)?;
    let value = args.first().copied().unwrap_or(RegisterValue::undefined());
    let key_handle = match value.as_object_handle() {
        Some(h) => h,
        None => return Ok(RegisterValue::from_bool(false)),
    };
    let result = runtime
        .objects()
        .weakset_has(handle, key_handle)
        .map_err(|e| VmNativeCallError::Internal(format!("{e:?}").into()))?;
    Ok(RegisterValue::from_bool(result))
}

/// WeakSet.prototype.delete(value)
/// Spec: <https://tc39.es/ecma262/#sec-weakset.prototype.delete>
fn weakset_delete(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let handle = require_weakset(*this, runtime)?;
    let value = args.first().copied().unwrap_or(RegisterValue::undefined());
    let key_handle = match value.as_object_handle() {
        Some(h) => h,
        None => return Ok(RegisterValue::from_bool(false)),
    };
    let result = runtime
        .objects_mut()
        .weakset_delete(handle, key_handle)
        .map_err(|e| VmNativeCallError::Internal(format!("{e:?}").into()))?;
    Ok(RegisterValue::from_bool(result))
}

fn require_weakset(
    this: RegisterValue,
    runtime: &crate::interpreter::RuntimeState,
) -> Result<ObjectHandle, VmNativeCallError> {
    this.as_object_handle()
        .map(ObjectHandle)
        .filter(|h| matches!(runtime.objects().kind(*h), Ok(HeapValueKind::WeakSet)))
        .ok_or_else(|| VmNativeCallError::Internal("Method requires a WeakSet receiver".into()))
}
