use crate::descriptors::VmNativeCallError;
use crate::interpreter::{InterpreterError, RuntimeState};
use crate::object::{ObjectHandle, PropertyDescriptor};
use crate::property::PropertyNameId;
use crate::value::RegisterValue;

fn excluded_property_names(
    runtime: &mut RuntimeState,
    excluded_keys: Option<RegisterValue>,
) -> Result<Vec<PropertyNameId>, VmNativeCallError> {
    let Some(excluded_keys) = excluded_keys else {
        return Ok(Vec::new());
    };

    let excluded_handle = runtime
        .property_base_object_handle(excluded_keys)
        .map_err(|error| match error {
            InterpreterError::UncaughtThrow(value) => VmNativeCallError::Thrown(value),
            InterpreterError::NativeCall(message) | InterpreterError::TypeError(message) => {
                VmNativeCallError::Internal(message)
            }
            other => VmNativeCallError::Internal(format!("{other}").into()),
        })?;
    let excluded_values = runtime.list_from_array_like(excluded_handle)?;
    let mut excluded = Vec::with_capacity(excluded_values.len());
    for value in excluded_values {
        runtime.check_interrupt()?;
        excluded.push(runtime.property_name_from_value(value)?);
    }
    Ok(excluded)
}

fn excluded_property_names_from_values(
    runtime: &mut RuntimeState,
    excluded_values: &[RegisterValue],
) -> Result<Vec<PropertyNameId>, VmNativeCallError> {
    let mut excluded = Vec::with_capacity(excluded_values.len());
    for value in excluded_values {
        runtime.check_interrupt()?;
        excluded.push(runtime.property_name_from_value(*value)?);
    }
    Ok(excluded)
}

pub(crate) fn copy_data_properties(
    runtime: &mut RuntimeState,
    target: ObjectHandle,
    source: RegisterValue,
    excluded_keys: Option<RegisterValue>,
) -> Result<(), VmNativeCallError> {
    if source == RegisterValue::undefined() || source == RegisterValue::null() {
        return Ok(());
    }

    let source_handle =
        runtime
            .property_base_object_handle(source)
            .map_err(|error| match error {
                InterpreterError::UncaughtThrow(value) => VmNativeCallError::Thrown(value),
                InterpreterError::NativeCall(message) | InterpreterError::TypeError(message) => {
                    VmNativeCallError::Internal(message)
                }
                other => VmNativeCallError::Internal(format!("{other}").into()),
            })?;
    let excluded = excluded_property_names(runtime, excluded_keys)?;
    copy_data_properties_with_excluded_names(runtime, target, source_handle, excluded)
}

pub(crate) fn copy_data_properties_except(
    runtime: &mut RuntimeState,
    target: ObjectHandle,
    source: RegisterValue,
    excluded_values: &[RegisterValue],
) -> Result<(), VmNativeCallError> {
    if source == RegisterValue::undefined() || source == RegisterValue::null() {
        return Ok(());
    }

    let source_handle =
        runtime
            .property_base_object_handle(source)
            .map_err(|error| match error {
                InterpreterError::UncaughtThrow(value) => VmNativeCallError::Thrown(value),
                InterpreterError::NativeCall(message) | InterpreterError::TypeError(message) => {
                    VmNativeCallError::Internal(message)
                }
                other => VmNativeCallError::Internal(format!("{other}").into()),
            })?;
    let excluded = excluded_property_names_from_values(runtime, excluded_values)?;
    copy_data_properties_with_excluded_names(runtime, target, source_handle, excluded)
}

fn copy_data_properties_with_excluded_names(
    runtime: &mut RuntimeState,
    target: ObjectHandle,
    source_handle: ObjectHandle,
    excluded: Vec<PropertyNameId>,
) -> Result<(), VmNativeCallError> {
    let keys = runtime.own_property_keys(source_handle).map_err(|error| {
        VmNativeCallError::Internal(
            format!("copy data properties ownKeys failed: {error:?}").into(),
        )
    })?;

    for property in keys {
        runtime.check_interrupt()?;
        if excluded.contains(&property) {
            continue;
        }

        let Some(descriptor) = runtime
            .own_property_descriptor(source_handle, property)
            .map_err(|error| {
                VmNativeCallError::Internal(
                    format!("copy data properties descriptor failed: {error:?}").into(),
                )
            })?
        else {
            continue;
        };
        if !descriptor.attributes().enumerable() {
            continue;
        }

        let value = runtime.own_property_value(source_handle, property)?;
        let defined = runtime
            .objects_mut()
            .define_own_property_from_descriptor(
                target,
                property,
                PropertyDescriptor::data(Some(value), Some(true), Some(true), Some(true)),
            )
            .map_err(|error| {
                VmNativeCallError::Internal(
                    format!("copy data properties define failed: {error:?}").into(),
                )
            })?;
        if !defined {
            return Err(VmNativeCallError::Internal(
                "copy data properties define returned false".into(),
            ));
        }
    }

    Ok(())
}
