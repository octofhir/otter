//! WeakRef and FinalizationRegistry intrinsics.
//!
//! Spec references (ECMAScript 2024 / ES15):
//! - WeakRef:                       <https://tc39.es/ecma262/#sec-weak-ref-objects>
//! - WeakRef.prototype:             <https://tc39.es/ecma262/#sec-properties-of-the-weak-ref-prototype-object>
//! - FinalizationRegistry:          <https://tc39.es/ecma262/#sec-finalization-registry-objects>
//! - FinalizationRegistry.prototype:<https://tc39.es/ecma262/#sec-properties-of-the-finalization-registry-prototype-object>

use crate::builders::ClassBuilder;
use crate::descriptors::{
    JsClassDescriptor, NativeBindingDescriptor, NativeBindingTarget, NativeFunctionDescriptor,
    VmNativeCallError,
};
use crate::object::ObjectHandle;
use crate::value::RegisterValue;

use super::{
    IntrinsicsError, VmIntrinsics,
    install::{IntrinsicInstallContext, IntrinsicInstaller, install_class_plan},
};

pub(super) static WEAKREF_INTRINSIC: WeakRefIntrinsic = WeakRefIntrinsic;

pub(super) struct WeakRefIntrinsic;

impl IntrinsicInstaller for WeakRefIntrinsic {
    fn init(
        &self,
        intrinsics: &mut VmIntrinsics,
        cx: &mut IntrinsicInstallContext<'_>,
    ) -> Result<(), IntrinsicsError> {
        install_weakref(intrinsics, cx)?;
        install_finalization_registry(intrinsics, cx)?;
        Ok(())
    }

    fn install_on_global(
        &self,
        intrinsics: &VmIntrinsics,
        cx: &mut IntrinsicInstallContext<'_>,
    ) -> Result<(), IntrinsicsError> {
        cx.install_global_value(
            intrinsics,
            "WeakRef",
            RegisterValue::from_object_handle(intrinsics.weakref_constructor.0),
        )?;
        cx.install_global_value(
            intrinsics,
            "FinalizationRegistry",
            RegisterValue::from_object_handle(intrinsics.finalization_registry_constructor.0),
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

fn type_error(
    runtime: &mut crate::interpreter::RuntimeState,
    message: &str,
) -> Result<VmNativeCallError, VmNativeCallError> {
    let error = runtime.alloc_type_error(message).map_err(|error| {
        VmNativeCallError::Internal(format!("TypeError allocation failed: {error}").into())
    })?;
    Ok(VmNativeCallError::Thrown(
        RegisterValue::from_object_handle(error.0),
    ))
}

// ═══════════════════════════════════════════════════════════════════════════
//  WeakRef — §26.1
//  Spec: <https://tc39.es/ecma262/#sec-weak-ref-objects>
// ═══════════════════════════════════════════════════════════════════════════

fn weakref_class_descriptor() -> JsClassDescriptor {
    JsClassDescriptor::new("WeakRef")
        .with_constructor(
            NativeFunctionDescriptor::constructor("WeakRef", 1, weakref_constructor)
                .with_default_intrinsic(crate::intrinsics::IntrinsicKey::WeakRefPrototype),
        )
        .with_binding(proto("deref", 0, weakref_deref))
}

fn install_weakref(
    intrinsics: &mut VmIntrinsics,
    cx: &mut IntrinsicInstallContext<'_>,
) -> Result<(), IntrinsicsError> {
    let descriptor = weakref_class_descriptor();
    let plan = ClassBuilder::from_descriptor(&descriptor)
        .expect("WeakRef class descriptors should normalize")
        .build();

    let constructor = if let Some(desc) = plan.constructor() {
        let host_fn = cx.native_functions.register(desc.clone());
        cx.alloc_intrinsic_host_function(host_fn, intrinsics.function_prototype())?
    } else {
        cx.alloc_intrinsic_object(Some(intrinsics.object_prototype()))?
    };
    intrinsics.weakref_constructor = constructor;

    let weakref_proto = cx.alloc_intrinsic_object(Some(intrinsics.object_prototype()))?;
    intrinsics.weakref_prototype = weakref_proto;

    install_class_plan(
        weakref_proto,
        constructor,
        &plan,
        intrinsics.function_prototype(),
        cx,
    )?;

    // §26.1.3.4 WeakRef.prototype[@@toStringTag] = "WeakRef"
    let sym_tag = cx
        .property_names
        .intern_symbol(super::WellKnownSymbol::ToStringTag.stable_id());
    let tag_str = cx.heap.alloc_string("WeakRef");
    cx.heap.define_own_property(
        weakref_proto,
        sym_tag,
        crate::object::PropertyValue::data_with_attrs(
            RegisterValue::from_object_handle(tag_str.0),
            crate::object::PropertyAttributes::from_flags(false, false, true),
        ),
    )?;

    Ok(())
}

/// WeakRef(target)
/// Spec: <https://tc39.es/ecma262/#sec-weak-ref-target>
///
/// 1. If NewTarget is undefined, throw a TypeError.
/// 2. If target is not an Object, throw a TypeError.
/// 3. Let weakRef be ? OrdinaryCreateFromConstructor(NewTarget, "%WeakRef.prototype%").
/// 4. Set weakRef.[[WeakRefTarget]] to target.
/// 5. Return weakRef.
fn weakref_constructor(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    if !runtime.is_current_native_construct_call() {
        return Err(type_error(runtime, "WeakRef constructor requires 'new'")?);
    }
    let target = args
        .first()
        .copied()
        .unwrap_or_else(RegisterValue::undefined);
    let target_handle = target.as_object_handle().map(ObjectHandle).ok_or_else(|| {
        type_error(runtime, "WeakRef target must be an object").unwrap_or(
            VmNativeCallError::Internal("WeakRef target must be an object".into()),
        )
    })?;
    // §10.1.13 OrdinaryCreateFromConstructor — honour `newTarget.prototype`.
    let prototype =
        Some(runtime.subclass_prototype_or_default(*this, runtime.intrinsics().weakref_prototype));
    let handle = runtime
        .objects_mut()
        .alloc_weakref(prototype, target_handle);
    Ok(RegisterValue::from_object_handle(handle.0))
}

/// WeakRef.prototype.deref()
/// Spec: <https://tc39.es/ecma262/#sec-weak-ref.prototype.deref>
///
/// 1. Let weakRef be the this value.
/// 2. Perform ? RequireInternalSlot(weakRef, [[WeakRefTarget]]).
/// 3. Return weakRef.[[WeakRefTarget]].
fn weakref_deref(
    this: &RegisterValue,
    _args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let handle = this.as_object_handle().map(ObjectHandle).ok_or_else(|| {
        type_error(runtime, "WeakRef.prototype.deref requires a WeakRef").unwrap_or(
            VmNativeCallError::Internal("WeakRef.prototype.deref requires a WeakRef".into()),
        )
    })?;
    match runtime.objects().weakref_deref(handle) {
        Ok(Some(target)) => Ok(RegisterValue::from_object_handle(target.0)),
        Ok(None) => Ok(RegisterValue::undefined()),
        Err(_) => Err(type_error(
            runtime,
            "WeakRef.prototype.deref requires a WeakRef",
        )?),
    }
}

// ═══════════════════════════════════════════════════════════════════════════
//  FinalizationRegistry — §26.2
//  Spec: <https://tc39.es/ecma262/#sec-finalization-registry-objects>
// ═══════════════════════════════════════════════════════════════════════════

fn finalization_registry_class_descriptor() -> JsClassDescriptor {
    JsClassDescriptor::new("FinalizationRegistry")
        .with_constructor(
            NativeFunctionDescriptor::constructor(
                "FinalizationRegistry",
                1,
                finalization_registry_constructor,
            )
            .with_default_intrinsic(crate::intrinsics::IntrinsicKey::FinalizationRegistryPrototype),
        )
        .with_binding(proto("register", 2, finalization_registry_register))
        .with_binding(proto("unregister", 1, finalization_registry_unregister))
}

fn install_finalization_registry(
    intrinsics: &mut VmIntrinsics,
    cx: &mut IntrinsicInstallContext<'_>,
) -> Result<(), IntrinsicsError> {
    let descriptor = finalization_registry_class_descriptor();
    let plan = ClassBuilder::from_descriptor(&descriptor)
        .expect("FinalizationRegistry class descriptors should normalize")
        .build();

    let constructor = if let Some(desc) = plan.constructor() {
        let host_fn = cx.native_functions.register(desc.clone());
        cx.alloc_intrinsic_host_function(host_fn, intrinsics.function_prototype())?
    } else {
        cx.alloc_intrinsic_object(Some(intrinsics.object_prototype()))?
    };
    intrinsics.finalization_registry_constructor = constructor;

    let fr_proto = cx.alloc_intrinsic_object(Some(intrinsics.object_prototype()))?;
    intrinsics.finalization_registry_prototype = fr_proto;

    install_class_plan(
        fr_proto,
        constructor,
        &plan,
        intrinsics.function_prototype(),
        cx,
    )?;

    // §26.2.3.5 FinalizationRegistry.prototype[@@toStringTag] = "FinalizationRegistry"
    let sym_tag = cx
        .property_names
        .intern_symbol(super::WellKnownSymbol::ToStringTag.stable_id());
    let tag_str = cx.heap.alloc_string("FinalizationRegistry");
    cx.heap.define_own_property(
        fr_proto,
        sym_tag,
        crate::object::PropertyValue::data_with_attrs(
            RegisterValue::from_object_handle(tag_str.0),
            crate::object::PropertyAttributes::from_flags(false, false, true),
        ),
    )?;

    Ok(())
}

/// FinalizationRegistry(cleanupCallback)
/// Spec: <https://tc39.es/ecma262/#sec-finalization-registry-cleanup-callback>
///
/// 1. If NewTarget is undefined, throw a TypeError.
/// 2. If IsCallable(cleanupCallback) is false, throw a TypeError.
/// 3. Let finalizationRegistry be ? OrdinaryCreateFromConstructor(NewTarget, ...).
/// 4. Set finalizationRegistry.[[CleanupCallback]] to cleanupCallback.
/// 5. Set finalizationRegistry.[[Cells]] to « ».
/// 6. Return finalizationRegistry.
fn finalization_registry_constructor(
    _this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    if !runtime.is_current_native_construct_call() {
        return Err(type_error(
            runtime,
            "FinalizationRegistry constructor requires 'new'",
        )?);
    }
    let callback = args
        .first()
        .copied()
        .unwrap_or_else(RegisterValue::undefined);
    let callback_handle = callback
        .as_object_handle()
        .map(ObjectHandle)
        .filter(|h| runtime.objects().is_callable(*h))
        .ok_or_else(|| {
            type_error(
                runtime,
                "FinalizationRegistry cleanup callback must be callable",
            )
            .unwrap_or(VmNativeCallError::Internal(
                "cleanup callback must be callable".into(),
            ))
        })?;
    let prototype = Some(runtime.intrinsics().finalization_registry_prototype);
    let handle = runtime
        .objects_mut()
        .alloc_finalization_registry(prototype, callback_handle);
    Ok(RegisterValue::from_object_handle(handle.0))
}

/// FinalizationRegistry.prototype.register(target, heldValue [, unregisterToken])
/// Spec: <https://tc39.es/ecma262/#sec-finalization-registry.prototype.register>
///
/// 1. Let finalizationRegistry be the this value.
/// 2. Perform ? RequireInternalSlot(finalizationRegistry, [[Cells]]).
/// 3. If target is not an Object, throw a TypeError.
/// 4. If SameValue(target, heldValue), throw a TypeError.
/// 5. If unregisterToken is not an Object and is not undefined, throw a TypeError.
/// 6. Append cell { target, heldValue, unregisterToken } to [[Cells]].
/// 7. Return undefined.
fn finalization_registry_register(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let handle = this.as_object_handle().map(ObjectHandle).ok_or_else(|| {
        type_error(
            runtime,
            "FinalizationRegistry.prototype.register requires a FinalizationRegistry",
        )
        .unwrap_or(VmNativeCallError::Internal(
            "register requires FinalizationRegistry".into(),
        ))
    })?;

    let target = args
        .first()
        .copied()
        .unwrap_or_else(RegisterValue::undefined);
    let target_handle = target.as_object_handle().map(ObjectHandle).ok_or_else(|| {
        type_error(runtime, "FinalizationRegistry target must be an object").unwrap_or(
            VmNativeCallError::Internal("target must be an object".into()),
        )
    })?;

    let held_value = args
        .get(1)
        .copied()
        .unwrap_or_else(RegisterValue::undefined);

    // §26.2.3.3 step 4: If SameValue(target, heldValue) is true, throw TypeError.
    if held_value == target {
        return Err(type_error(
            runtime,
            "FinalizationRegistry target and held value must not be the same",
        )?);
    }

    let unregister_token = args.get(2).copied().and_then(|v| {
        if v == RegisterValue::undefined() {
            None
        } else {
            v.as_object_handle().map(ObjectHandle)
        }
    });

    runtime
        .objects_mut()
        .finalization_registry_register(handle, target_handle, held_value, unregister_token)
        .map_err(|_| {
            type_error(
                runtime,
                "FinalizationRegistry.prototype.register requires a FinalizationRegistry",
            )
            .unwrap_or(VmNativeCallError::Internal(
                "register requires FinalizationRegistry".into(),
            ))
        })?;

    Ok(RegisterValue::undefined())
}

/// FinalizationRegistry.prototype.unregister(unregisterToken)
/// Spec: <https://tc39.es/ecma262/#sec-finalization-registry.prototype.unregister>
///
/// 1. Let finalizationRegistry be the this value.
/// 2. Perform ? RequireInternalSlot(finalizationRegistry, [[Cells]]).
/// 3. If unregisterToken is not an Object, throw a TypeError.
/// 4. Remove matching cells.
/// 5. Return a Boolean indicating whether cells were removed.
fn finalization_registry_unregister(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let handle = this.as_object_handle().map(ObjectHandle).ok_or_else(|| {
        type_error(
            runtime,
            "FinalizationRegistry.prototype.unregister requires a FinalizationRegistry",
        )
        .unwrap_or(VmNativeCallError::Internal(
            "unregister requires FinalizationRegistry".into(),
        ))
    })?;

    let token = args
        .first()
        .copied()
        .unwrap_or_else(RegisterValue::undefined);
    let token_handle = token.as_object_handle().map(ObjectHandle).ok_or_else(|| {
        type_error(
            runtime,
            "FinalizationRegistry unregister token must be an object",
        )
        .unwrap_or(VmNativeCallError::Internal(
            "unregister token must be an object".into(),
        ))
    })?;

    let removed = runtime
        .objects_mut()
        .finalization_registry_unregister(handle, token_handle)
        .map_err(|_| {
            type_error(
                runtime,
                "FinalizationRegistry.prototype.unregister requires a FinalizationRegistry",
            )
            .unwrap_or(VmNativeCallError::Internal(
                "unregister requires FinalizationRegistry".into(),
            ))
        })?;

    Ok(RegisterValue::from_bool(removed))
}
