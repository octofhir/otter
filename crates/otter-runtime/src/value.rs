//! Safe wrapper around JSC values with automatic GC protection

use crate::bindings::*;
use crate::error::{JscError, JscResult};
use std::ffi::CString;
use std::ptr;

/// A JavaScript value with automatic GC protection
///
/// When created, the value is protected from garbage collection.
/// When dropped, the protection is removed.
pub struct JscValue {
    value: JSValueRef,
    ctx: JSContextRef,
}

impl std::fmt::Debug for JscValue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.to_string() {
            Ok(s) => write!(f, "JscValue({})", s),
            Err(_) => write!(f, "JscValue(<opaque>)"),
        }
    }
}

impl JscValue {
    /// Create a new protected value
    ///
    /// # Safety
    /// The value must be valid for the given context
    pub unsafe fn new(ctx: JSContextRef, value: JSValueRef) -> Self {
        if !value.is_null() {
            JSValueProtect(ctx, value);
        }
        Self { value, ctx }
    }

    /// Create an undefined value
    pub fn undefined(ctx: JSContextRef) -> Self {
        unsafe {
            let value = JSValueMakeUndefined(ctx);
            Self::new(ctx, value)
        }
    }

    /// Create a null value
    pub fn null(ctx: JSContextRef) -> Self {
        unsafe {
            let value = JSValueMakeNull(ctx);
            Self::new(ctx, value)
        }
    }

    /// Create a boolean value
    pub fn boolean(ctx: JSContextRef, b: bool) -> Self {
        unsafe {
            let value = JSValueMakeBoolean(ctx, b);
            Self::new(ctx, value)
        }
    }

    /// Create a number value
    pub fn number(ctx: JSContextRef, n: f64) -> Self {
        unsafe {
            let value = JSValueMakeNumber(ctx, n);
            Self::new(ctx, value)
        }
    }

    /// Create a string value
    pub fn string(ctx: JSContextRef, s: &str) -> JscResult<Self> {
        let c_str = CString::new(s).map_err(|e| JscError::Internal(e.to_string()))?;
        unsafe {
            let js_str = JSStringCreateWithUTF8CString(c_str.as_ptr());
            let value = JSValueMakeString(ctx, js_str);
            JSStringRelease(js_str);
            Ok(Self::new(ctx, value))
        }
    }

    /// Create a value from JSON string
    pub fn from_json(ctx: JSContextRef, json: &str) -> JscResult<Self> {
        let c_str = CString::new(json).map_err(|e| JscError::Internal(e.to_string()))?;
        unsafe {
            let js_str = JSStringCreateWithUTF8CString(c_str.as_ptr());
            let value = JSValueMakeFromJSONString(ctx, js_str);
            JSStringRelease(js_str);

            if value.is_null() {
                return Err(JscError::JsonError(serde_json::Error::io(
                    std::io::Error::new(std::io::ErrorKind::InvalidData, "Invalid JSON"),
                )));
            }

            Ok(Self::new(ctx, value))
        }
    }

    /// Get the raw value reference
    pub fn raw(&self) -> JSValueRef {
        self.value
    }

    /// Get the context
    pub fn context(&self) -> JSContextRef {
        self.ctx
    }

    /// Check if the value is undefined
    pub fn is_undefined(&self) -> bool {
        unsafe { JSValueIsUndefined(self.ctx, self.value) }
    }

    /// Check if the value is null
    pub fn is_null(&self) -> bool {
        unsafe { JSValueIsNull(self.ctx, self.value) }
    }

    /// Check if the value is a boolean
    pub fn is_boolean(&self) -> bool {
        unsafe { JSValueIsBoolean(self.ctx, self.value) }
    }

    /// Check if the value is a number
    pub fn is_number(&self) -> bool {
        unsafe { JSValueIsNumber(self.ctx, self.value) }
    }

    /// Check if the value is a string
    pub fn is_string(&self) -> bool {
        unsafe { JSValueIsString(self.ctx, self.value) }
    }

    /// Check if the value is an object
    pub fn is_object(&self) -> bool {
        unsafe { JSValueIsObject(self.ctx, self.value) }
    }

    /// Check if the value is an array
    pub fn is_array(&self) -> bool {
        unsafe { JSValueIsArray(self.ctx, self.value) }
    }

    /// Convert to boolean
    pub fn to_bool(&self) -> bool {
        unsafe { JSValueToBoolean(self.ctx, self.value) }
    }

    /// Convert to number
    pub fn to_number(&self) -> JscResult<f64> {
        unsafe {
            let mut exception: JSValueRef = ptr::null_mut();
            let result = JSValueToNumber(self.ctx, self.value, &mut exception);

            if !exception.is_null() {
                return Err(extract_exception(self.ctx, exception));
            }

            Ok(result)
        }
    }

    /// Convert to string
    pub fn to_string(&self) -> JscResult<String> {
        unsafe {
            let mut exception: JSValueRef = ptr::null_mut();
            let js_str = JSValueToStringCopy(self.ctx, self.value, &mut exception);

            if !exception.is_null() || js_str.is_null() {
                return Err(extract_exception(self.ctx, exception));
            }

            let result = js_string_to_rust(js_str);
            JSStringRelease(js_str);
            Ok(result)
        }
    }

    /// Convert to JSON string
    pub fn to_json(&self) -> JscResult<String> {
        unsafe {
            let mut exception: JSValueRef = ptr::null_mut();
            let js_str = JSValueCreateJSONString(self.ctx, self.value, 0, &mut exception);

            if !exception.is_null() || js_str.is_null() {
                return Err(extract_exception(self.ctx, exception));
            }

            let result = js_string_to_rust(js_str);
            JSStringRelease(js_str);
            Ok(result)
        }
    }

    /// Deserialize from JSON to Rust type
    pub fn deserialize<T: serde::de::DeserializeOwned>(&self) -> JscResult<T> {
        let json = self.to_json()?;
        serde_json::from_str(&json).map_err(JscError::JsonError)
    }
}

impl Drop for JscValue {
    fn drop(&mut self) {
        if !self.value.is_null() {
            unsafe {
                JSValueUnprotect(self.ctx, self.value);
            }
        }
    }
}

/// Convert JSStringRef to Rust String
pub(crate) unsafe fn js_string_to_rust(js_str: JSStringRef) -> String {
    if js_str.is_null() {
        return String::new();
    }

    let max_size = JSStringGetMaximumUTF8CStringSize(js_str);
    let mut buffer = vec![0u8; max_size];
    let actual_size = JSStringGetUTF8CString(js_str, buffer.as_mut_ptr() as *mut i8, max_size);

    if actual_size > 0 {
        buffer.truncate(actual_size - 1);
        String::from_utf8_lossy(&buffer).into_owned()
    } else {
        String::new()
    }
}

/// Extract error message from JSC exception
pub(crate) unsafe fn extract_exception(ctx: JSContextRef, exception: JSValueRef) -> JscError {
    if exception.is_null() {
        return JscError::script_error("Unknown error");
    }

    let mut ex: JSValueRef = ptr::null_mut();
    let js_str = JSValueToStringCopy(ctx, exception, &mut ex);

    if js_str.is_null() {
        return JscError::script_error("Unknown error (failed to stringify)");
    }

    let message = js_string_to_rust(js_str);
    JSStringRelease(js_str);

    JscError::script_error(message)
}
