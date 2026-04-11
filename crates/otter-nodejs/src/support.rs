use otter_runtime::{ObjectHandle, RegisterValue, RuntimeState, VmNativeCallError};
use otter_vm::descriptors::NativeFunctionDescriptor;
use otter_vm::object::PropertyDescriptor;

pub(crate) fn string_value(runtime: &mut RuntimeState, value: impl AsRef<str>) -> RegisterValue {
    RegisterValue::from_object_handle(runtime.alloc_string(value.as_ref()).0)
}

pub(crate) fn install_value(
    runtime: &mut RuntimeState,
    target: ObjectHandle,
    name: &str,
    value: RegisterValue,
) -> Result<(), String> {
    let property = runtime.intern_property_name(name);
    runtime
        .objects_mut()
        .set_property(target, property, value)
        .map(|_| ())
        .map_err(|error| format!("failed to install {name}: {error:?}"))
}

pub(crate) fn install_readonly_value(
    runtime: &mut RuntimeState,
    target: ObjectHandle,
    name: &str,
    value: RegisterValue,
) -> Result<(), String> {
    let property = runtime.intern_property_name(name);
    runtime
        .objects_mut()
        .define_own_property(
            target,
            property,
            PropertyDescriptor::data(Some(value), Some(false), Some(true), Some(true))
                .to_property_value(),
        )
        .map(|_| ())
        .map_err(|error| format!("failed to install readonly {name}: {error:?}"))
}

pub(crate) fn install_method(
    runtime: &mut RuntimeState,
    target: ObjectHandle,
    name: &str,
    arity: u16,
    callback: fn(
        &RegisterValue,
        &[RegisterValue],
        &mut RuntimeState,
    ) -> Result<RegisterValue, VmNativeCallError>,
    context: &str,
) -> Result<(), String> {
    let descriptor = NativeFunctionDescriptor::method(name, arity, callback);
    let id = runtime.register_native_function(descriptor);
    let function = runtime.alloc_host_function(id);
    let property = runtime.intern_property_name(name);
    runtime
        .objects_mut()
        .set_property(
            target,
            property,
            RegisterValue::from_object_handle(function.0),
        )
        .map(|_| ())
        .map_err(|error| format!("failed to install {context}: {error:?}"))
}

pub(crate) fn own_property(
    runtime: &mut RuntimeState,
    target: ObjectHandle,
    name: &str,
) -> Option<RegisterValue> {
    let property = runtime.intern_property_name(name);
    runtime.own_property_value(target, property).ok()
}

pub(crate) fn value_to_string(runtime: &mut RuntimeState, value: RegisterValue) -> String {
    runtime.js_to_string_infallible(value).into_string()
}

pub(crate) fn type_error(runtime: &mut RuntimeState, message: &str) -> VmNativeCallError {
    match runtime.alloc_type_error(message) {
        Ok(error) => VmNativeCallError::Thrown(RegisterValue::from_object_handle(error.0)),
        Err(_) => VmNativeCallError::Internal(message.into()),
    }
}

pub(crate) fn throw_type_error_with_code(
    runtime: &mut RuntimeState,
    message: &str,
    code: &str,
) -> VmNativeCallError {
    match runtime.alloc_type_error(message) {
        Ok(error) => {
            let property = runtime.intern_property_name("code");
            let code_value = string_value(runtime, code);
            let _ = runtime
                .objects_mut()
                .set_property(error, property, code_value);
            VmNativeCallError::Thrown(RegisterValue::from_object_handle(error.0))
        }
        Err(_) => VmNativeCallError::Internal(message.into()),
    }
}
