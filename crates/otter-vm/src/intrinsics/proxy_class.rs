//! ES2024 §28.2 — Proxy Objects.
//! Spec: <https://tc39.es/ecma262/#sec-proxy-objects>

use crate::descriptors::{NativeFunctionDescriptor, VmNativeCallError};
use crate::object::{HeapValueKind, ObjectHandle, PropertyAttributes, PropertyValue};
use crate::value::RegisterValue;

use super::{
    IntrinsicsError, VmIntrinsics,
    install::{IntrinsicInstallContext, IntrinsicInstaller, install_function_length_name},
};

pub(super) static PROXY_INTRINSIC: ProxyIntrinsic = ProxyIntrinsic;

pub(super) struct ProxyIntrinsic;

impl IntrinsicInstaller for ProxyIntrinsic {
    fn init(
        &self,
        intrinsics: &mut VmIntrinsics,
        cx: &mut IntrinsicInstallContext<'_>,
    ) -> Result<(), IntrinsicsError> {
        // §28.2.1 — The Proxy Constructor
        let constructor_id = cx
            .native_functions
            .register(NativeFunctionDescriptor::constructor(
                "Proxy",
                2,
                proxy_constructor,
            ));
        let constructor =
            cx.alloc_intrinsic_host_function(constructor_id, intrinsics.function_prototype())?;
        install_function_length_name(constructor, 2, "Proxy", cx)?;

        // §28.2.2 — Proxy.revocable(target, handler)
        let revocable_id = cx
            .native_functions
            .register(NativeFunctionDescriptor::method(
                "revocable",
                2,
                proxy_revocable,
            ));
        let revocable_fn =
            cx.alloc_intrinsic_host_function(revocable_id, intrinsics.function_prototype())?;
        install_function_length_name(revocable_fn, 2, "revocable", cx)?;
        let revocable_prop = cx.property_names.intern("revocable");
        cx.heap.define_own_property(
            constructor,
            revocable_prop,
            PropertyValue::data_with_attrs(
                RegisterValue::from_object_handle(revocable_fn.0),
                PropertyAttributes::from_flags(true, false, true),
            ),
        )?;

        intrinsics.proxy_constructor = constructor;
        Ok(())
    }

    fn install_on_global(
        &self,
        intrinsics: &VmIntrinsics,
        cx: &mut IntrinsicInstallContext<'_>,
    ) -> Result<(), IntrinsicsError> {
        cx.install_global_value(
            intrinsics,
            "Proxy",
            RegisterValue::from_object_handle(intrinsics.proxy_constructor().0),
        )
    }
}

// ---------------------------------------------------------------------------
// §28.2.1 Proxy(target, handler)
// Spec: <https://tc39.es/ecma262/#sec-proxy-target-handler>
// ---------------------------------------------------------------------------

fn proxy_constructor(
    _this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    if !runtime.is_current_native_construct_call() {
        return Err(type_error(runtime, "Constructor Proxy requires 'new'")?);
    }
    proxy_create(args, runtime)
}

/// ProxyCreate(target, handler) — §10.5.14
/// Spec: <https://tc39.es/ecma262/#sec-proxycreate>
fn proxy_create(
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let target = require_object_like(
        args.first()
            .copied()
            .unwrap_or_else(RegisterValue::undefined),
        runtime,
        "Cannot create proxy with a non-object as target",
    )?;
    let handler = require_object_like(
        args.get(1)
            .copied()
            .unwrap_or_else(RegisterValue::undefined),
        runtime,
        "Cannot create proxy with a non-object as handler",
    )?;

    let proxy = runtime.objects_mut().alloc_proxy(target, handler);
    Ok(RegisterValue::from_object_handle(proxy.0))
}

// ---------------------------------------------------------------------------
// §28.2.2 Proxy.revocable(target, handler)
// Spec: <https://tc39.es/ecma262/#sec-proxy.revocable>
// ---------------------------------------------------------------------------

/// The revoke function for a revocable proxy. Stored as a native function
/// whose "proxy to revoke" handle is in a hidden `__proxy__` slot on itself.
fn proxy_revoke_function(
    _this: &RegisterValue,
    _args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    // The function object has a hidden `__proxy__` slot holding the proxy handle.
    // Use current_native_callee() to get the revoke function's own handle.
    let Some(fn_handle) = runtime.current_native_callee() else {
        return Ok(RegisterValue::undefined());
    };
    let slot = runtime.intern_property_name("__proxy__");
    let proxy_val = runtime
        .own_property_value(fn_handle, slot)
        .map_err(interp_err)?;
    // If the slot is null/undefined, the proxy was already revoked.
    let Some(proxy_handle) = proxy_val.as_object_handle().map(ObjectHandle) else {
        return Ok(RegisterValue::undefined());
    };

    // Revoke the proxy.
    runtime
        .objects_mut()
        .revoke_proxy(proxy_handle)
        .map_err(interp_err)?;

    // Clear the slot so future calls are no-ops.
    runtime
        .set_named_property(fn_handle, slot, RegisterValue::null())
        .map_err(interp_err)?;

    Ok(RegisterValue::undefined())
}

fn proxy_revocable(
    _this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    // 1. Let p be ? ProxyCreate(target, handler).
    let proxy_val = proxy_create(args, runtime)?;

    // 2. Create the revoke function.
    let revoke_desc = NativeFunctionDescriptor::method("", 0, proxy_revoke_function);
    let revoke_id = runtime.register_native_function(revoke_desc);
    let fn_proto = runtime.intrinsics().function_prototype();
    let revoke_fn = runtime.objects_mut().alloc_host_function(revoke_id);
    let _ = runtime
        .objects_mut()
        .set_prototype(revoke_fn, Some(fn_proto));

    // Store the proxy handle on the revoke function as a hidden slot.
    let slot = runtime.intern_property_name("__proxy__");
    runtime
        .objects_mut()
        .define_own_property(
            revoke_fn,
            slot,
            PropertyValue::data_with_attrs(
                proxy_val,
                PropertyAttributes::from_flags(true, false, false),
            ),
        )
        .map_err(interp_err)?;

    // 3. Return { proxy, revoke }.
    let result = runtime.alloc_object();
    let proxy_key = runtime.intern_property_name("proxy");
    let revoke_key = runtime.intern_property_name("revoke");
    runtime
        .set_named_property(result, proxy_key, proxy_val)
        .map_err(interp_err)?;
    runtime
        .set_named_property(
            result,
            revoke_key,
            RegisterValue::from_object_handle(revoke_fn.0),
        )
        .map_err(interp_err)?;

    Ok(RegisterValue::from_object_handle(result.0))
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn require_object_like(
    value: RegisterValue,
    runtime: &mut crate::interpreter::RuntimeState,
    message: &str,
) -> Result<ObjectHandle, VmNativeCallError> {
    let Some(handle) = value.as_object_handle().map(ObjectHandle) else {
        return Err(type_error(runtime, message)?);
    };
    if matches!(runtime.objects().kind(handle), Ok(HeapValueKind::String)) {
        return Err(type_error(runtime, message)?);
    }
    Ok(handle)
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

fn interp_err(error: impl std::fmt::Debug) -> VmNativeCallError {
    VmNativeCallError::Internal(format!("{error:?}").into())
}
