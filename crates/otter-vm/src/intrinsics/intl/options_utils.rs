//! Shared option extraction utilities for Intl constructors.
//!
//! These helpers read named properties from a JS options object and coerce
//! them to the expected Rust type. They abstract over the new-VM API
//! (`RuntimeState` + `ObjectHandle`).
//!
//! Spec: <https://tc39.es/ecma402/#sec-getoption>

use crate::descriptors::VmNativeCallError;
use crate::object::ObjectHandle;
use crate::value::RegisterValue;

/// Reads a string-valued option from `options[name]`.
///
/// Returns `Ok(None)` if `options` is undefined/null or `options[name]` is undefined.
///
/// §9.2.12 GetOption (type = "string")
/// Spec: <https://tc39.es/ecma402/#sec-getoption>
pub fn get_option_string(
    options: RegisterValue,
    name: &str,
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<Option<String>, VmNativeCallError> {
    if options == RegisterValue::undefined() || options == RegisterValue::null() {
        return Ok(None);
    }
    let handle = match options.as_object_handle() {
        Some(h) => ObjectHandle(h),
        None => return Ok(None),
    };
    let prop = runtime.intern_property_name(name);
    let receiver = RegisterValue::from_object_handle(handle.0);
    let value = runtime.ordinary_get(handle, prop, receiver)?;
    if value == RegisterValue::undefined() {
        return Ok(None);
    }
    let s = runtime
        .js_to_string(value)
        .map_err(|e| VmNativeCallError::Internal(format!("GetOption({name}): {e}").into()))?;
    Ok(Some(s.to_string()))
}

/// Reads a boolean-valued option from `options[name]`.
///
/// Returns `Ok(None)` if `options` is undefined/null or `options[name]` is undefined.
///
/// §9.2.12 GetOption (type = "boolean")
/// Spec: <https://tc39.es/ecma402/#sec-getoption>
#[allow(dead_code)]
pub fn get_option_bool(
    options: RegisterValue,
    name: &str,
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<Option<bool>, VmNativeCallError> {
    if options == RegisterValue::undefined() || options == RegisterValue::null() {
        return Ok(None);
    }
    let handle = match options.as_object_handle() {
        Some(h) => ObjectHandle(h),
        None => return Ok(None),
    };
    let prop = runtime.intern_property_name(name);
    let receiver = RegisterValue::from_object_handle(handle.0);
    let value = runtime.ordinary_get(handle, prop, receiver)?;
    if value == RegisterValue::undefined() {
        return Ok(None);
    }
    // §7.1.2 ToBoolean — simplified for option values (no BigInt options expected).
    Ok(Some(value.is_truthy()))
}

/// Reads a number-valued option from `options[name]`.
///
/// Returns `Ok(None)` if `options` is undefined/null or `options[name]` is undefined.
///
/// §9.2.12 GetOption (type = "number")
/// Spec: <https://tc39.es/ecma402/#sec-getoption>
pub fn get_option_number(
    options: RegisterValue,
    name: &str,
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<Option<f64>, VmNativeCallError> {
    if options == RegisterValue::undefined() || options == RegisterValue::null() {
        return Ok(None);
    }
    let handle = match options.as_object_handle() {
        Some(h) => ObjectHandle(h),
        None => return Ok(None),
    };
    let prop = runtime.intern_property_name(name);
    let receiver = RegisterValue::from_object_handle(handle.0);
    let value = runtime.ordinary_get(handle, prop, receiver)?;
    if value == RegisterValue::undefined() {
        return Ok(None);
    }
    let n = runtime
        .js_to_number(value)
        .map_err(|e| VmNativeCallError::Internal(format!("GetOption({name}): {e}").into()))?;
    Ok(Some(n))
}

/// Reads a raw `RegisterValue` from `options[name]`.
///
/// Returns `Ok(None)` if `options` is undefined/null or `options[name]` is undefined.
#[allow(dead_code)]
pub fn get_option_value(
    options: RegisterValue,
    name: &str,
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<Option<RegisterValue>, VmNativeCallError> {
    if options == RegisterValue::undefined() || options == RegisterValue::null() {
        return Ok(None);
    }
    let handle = match options.as_object_handle() {
        Some(h) => ObjectHandle(h),
        None => return Ok(None),
    };
    let prop = runtime.intern_property_name(name);
    let receiver = RegisterValue::from_object_handle(handle.0);
    let value = runtime.ordinary_get(handle, prop, receiver)?;
    if value == RegisterValue::undefined() {
        return Ok(None);
    }
    Ok(Some(value))
}
