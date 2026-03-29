use crate::descriptors::{NativeFunctionDescriptor, VmNativeCallError};
use crate::object::{HeapValueKind, ObjectHandle, PropertyAttributes, PropertyValue};
use crate::value::RegisterValue;

use super::{
    IntrinsicsError, VmIntrinsics,
    install::{IntrinsicInstallContext, IntrinsicInstaller, install_function_length_name},
};

pub(super) static PROXY_INTRINSIC: ProxyIntrinsic = ProxyIntrinsic;

const PROXY_TARGET_SLOT: &str = "__otter_proxy_target__";
const PROXY_HANDLER_SLOT: &str = "__otter_proxy_handler__";

pub(super) struct ProxyIntrinsic;

impl IntrinsicInstaller for ProxyIntrinsic {
    fn init(
        &self,
        intrinsics: &mut VmIntrinsics,
        cx: &mut IntrinsicInstallContext<'_>,
    ) -> Result<(), IntrinsicsError> {
        let constructor_id = cx.native_functions.register(NativeFunctionDescriptor::constructor(
            "Proxy",
            2,
            proxy_constructor,
        ));
        let constructor =
            cx.alloc_intrinsic_host_function(constructor_id, intrinsics.function_prototype())?;
        install_function_length_name(constructor, 2, "Proxy", cx)?;
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

fn proxy_constructor(
    _this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    if !runtime.is_current_native_construct_call() {
        return Err(type_error(runtime, "Constructor Proxy requires 'new'")?);
    }

    let target = require_object_like(
        args.first()
            .copied()
            .unwrap_or_else(RegisterValue::undefined),
        runtime,
        "Proxy target must be an object",
    )?;
    let handler = require_object_like(
        args.get(1)
            .copied()
            .unwrap_or_else(RegisterValue::undefined),
        runtime,
        "Proxy handler must be an object",
    )?;

    let prototype = runtime.objects().get_prototype(target).map_err(|error| {
        VmNativeCallError::Internal(format!("Proxy target prototype lookup failed: {error:?}").into())
    })?;
    let proxy = runtime.alloc_object_with_prototype(prototype);
    define_hidden_slot(proxy, PROXY_TARGET_SLOT, target, runtime)?;
    define_hidden_slot(proxy, PROXY_HANDLER_SLOT, handler, runtime)?;
    Ok(RegisterValue::from_object_handle(proxy.0))
}

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

fn define_hidden_slot(
    proxy: ObjectHandle,
    slot_name: &str,
    value: ObjectHandle,
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<(), VmNativeCallError> {
    let slot = runtime.intern_property_name(slot_name);
    runtime
        .objects_mut()
        .define_own_property(
            proxy,
            slot,
            PropertyValue::data_with_attrs(
                RegisterValue::from_object_handle(value.0),
                PropertyAttributes::from_flags(true, false, true),
            ),
        )
        .map_err(|error| {
            VmNativeCallError::Internal(format!("Proxy internal slot install failed: {error:?}").into())
        })?;
    Ok(())
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
