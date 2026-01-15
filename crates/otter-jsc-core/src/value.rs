//! Safe wrapper around JSC values with automatic GC protection

use otter_jsc_sys::*;
use std::ffi::CString;
use std::marker::PhantomData;
use std::ptr;

use crate::error::{JscError, JscResult};
use crate::string::js_string_to_rust;

/// A JavaScript value with automatic GC protection
///
/// When created, the value is protected from garbage collection.
/// When dropped, the protection is removed.
///
/// # Thread Safety
///
/// This type is `!Send` and `!Sync` because JavaScript values are tied to
/// their context's thread. Cross-thread access causes undefined behavior.
pub struct JscValue {
    value: JSValueRef,
    ctx: JSContextRef,
    /// Marker to make this type !Send + !Sync
    _not_send: PhantomData<*mut ()>,
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
            // SAFETY: ctx and value are valid per caller contract
            unsafe { JSValueProtect(ctx, value) };
        }
        Self {
            value,
            ctx,
            _not_send: PhantomData,
        }
    }

    /// Create an undefined value
    pub fn undefined(ctx: JSContextRef) -> Self {
        // SAFETY: JSValueMakeUndefined always returns a valid value
        unsafe {
            let value = JSValueMakeUndefined(ctx);
            Self::new(ctx, value)
        }
    }

    /// Create a null value
    pub fn null(ctx: JSContextRef) -> Self {
        // SAFETY: JSValueMakeNull always returns a valid value
        unsafe {
            let value = JSValueMakeNull(ctx);
            Self::new(ctx, value)
        }
    }

    /// Create a boolean value
    pub fn boolean(ctx: JSContextRef, b: bool) -> Self {
        // SAFETY: JSValueMakeBoolean always returns a valid value
        unsafe {
            let value = JSValueMakeBoolean(ctx, b);
            Self::new(ctx, value)
        }
    }

    /// Create a number value
    pub fn number(ctx: JSContextRef, n: f64) -> Self {
        // SAFETY: JSValueMakeNumber always returns a valid value
        unsafe {
            let value = JSValueMakeNumber(ctx, n);
            Self::new(ctx, value)
        }
    }

    /// Create a string value
    pub fn string(ctx: JSContextRef, s: &str) -> JscResult<Self> {
        let c_str = CString::new(s).map_err(|e| JscError::Internal(e.to_string()))?;
        // SAFETY: c_str is valid null-terminated string
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
        // SAFETY: c_str is valid null-terminated string
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
        // SAFETY: self.ctx and self.value are valid
        unsafe { JSValueIsUndefined(self.ctx, self.value) }
    }

    /// Check if the value is null
    pub fn is_null(&self) -> bool {
        // SAFETY: self.ctx and self.value are valid
        unsafe { JSValueIsNull(self.ctx, self.value) }
    }

    /// Check if the value is a boolean
    pub fn is_boolean(&self) -> bool {
        // SAFETY: self.ctx and self.value are valid
        unsafe { JSValueIsBoolean(self.ctx, self.value) }
    }

    /// Check if the value is a number
    pub fn is_number(&self) -> bool {
        // SAFETY: self.ctx and self.value are valid
        unsafe { JSValueIsNumber(self.ctx, self.value) }
    }

    /// Check if the value is a string
    pub fn is_string(&self) -> bool {
        // SAFETY: self.ctx and self.value are valid
        unsafe { JSValueIsString(self.ctx, self.value) }
    }

    /// Check if the value is an object
    pub fn is_object(&self) -> bool {
        // SAFETY: self.ctx and self.value are valid
        unsafe { JSValueIsObject(self.ctx, self.value) }
    }

    /// Check if the value is an array
    pub fn is_array(&self) -> bool {
        // SAFETY: self.ctx and self.value are valid
        unsafe { JSValueIsArray(self.ctx, self.value) }
    }

    /// Check if the value is a Promise
    ///
    /// Checks if the object is an instance of the Promise constructor.
    pub fn is_promise(&self) -> bool {
        if !self.is_object() {
            return false;
        }

        // SAFETY: self.ctx and self.value are valid
        unsafe {
            // Get the Promise constructor
            let promise_name = CString::new("Promise").unwrap();
            let promise_str = JSStringCreateWithUTF8CString(promise_name.as_ptr());

            let global = JSContextGetGlobalObject(self.ctx);
            let mut exception: JSValueRef = ptr::null_mut();
            let promise_ctor = JSObjectGetProperty(self.ctx, global, promise_str, &mut exception);
            JSStringRelease(promise_str);

            if exception.is_null()
                && !promise_ctor.is_null()
                && JSValueIsObject(self.ctx, promise_ctor)
            {
                let mut check_exception: JSValueRef = ptr::null_mut();
                let is_instance = JSValueIsInstanceOfConstructor(
                    self.ctx,
                    self.value,
                    promise_ctor as JSObjectRef,
                    &mut check_exception,
                );
                return check_exception.is_null() && is_instance;
            }

            false
        }
    }

    /// Convert to boolean
    pub fn to_bool(&self) -> bool {
        // SAFETY: self.ctx and self.value are valid
        unsafe { JSValueToBoolean(self.ctx, self.value) }
    }

    /// Convert to number
    pub fn to_number(&self) -> JscResult<f64> {
        // SAFETY: self.ctx and self.value are valid
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
        // SAFETY: self.ctx and self.value are valid
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
        // SAFETY: self.ctx and self.value are valid
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
            // SAFETY: value was protected in new(), now unprotecting
            unsafe {
                JSValueUnprotect(self.ctx, self.value);
            }
        }
    }
}

/// Extract a structured error from a JS exception value
///
/// This function extracts as much information as possible from a JavaScript
/// exception, including error type, message, stack trace, and source location.
///
/// # Safety
/// - `ctx` must be a valid JSContextRef
/// - `exception` can be null (returns Internal error in that case)
pub unsafe fn extract_exception(ctx: JSContextRef, exception: JSValueRef) -> JscError {
    if exception.is_null() {
        return JscError::Internal("Null exception".into());
    }

    // SAFETY: ctx and exception are valid per caller contract
    unsafe {
        // Check if the exception is an object (Error objects have properties)
        let is_object = JSValueIsObject(ctx, exception);

        if is_object {
            extract_error_object(ctx, exception)
        } else {
            // Primitive exception (throw "string" or throw 42)
            let message =
                value_to_string(ctx, exception).unwrap_or_else(|| "Unknown error".to_string());
            JscError::script_error("Error", message)
        }
    }
}

/// Extract details from an Error object
unsafe fn extract_error_object(ctx: JSContextRef, exception: JSValueRef) -> JscError {
    // SAFETY: All operations in this block require ctx and exception to be valid,
    // which is guaranteed by the caller
    unsafe {
        let obj = exception as JSObjectRef;

        // Extract error type (constructor name)
        let error_type =
            get_string_property(ctx, obj, "name").unwrap_or_else(|| "Error".to_string());

        // Extract message
        let message = get_string_property(ctx, obj, "message").unwrap_or_else(|| {
            value_to_string(ctx, exception).unwrap_or_else(|| "Unknown error".to_string())
        });

        // Extract stack trace
        let stack = get_string_property(ctx, obj, "stack");

        // Extract source location (JSC uses different property names)
        let file = get_string_property(ctx, obj, "sourceURL")
            .or_else(|| get_string_property(ctx, obj, "fileName"));
        let line = get_number_property(ctx, obj, "line")
            .or_else(|| get_number_property(ctx, obj, "lineNumber"))
            .map(|n| n as u32);
        let column = get_number_property(ctx, obj, "column")
            .or_else(|| get_number_property(ctx, obj, "columnNumber"))
            .map(|n| n as u32);

        JscError::script_error_with_location(error_type, message, file, line, column, stack)
    }
}

/// Get a string property from a JS object
unsafe fn get_string_property(ctx: JSContextRef, obj: JSObjectRef, name: &str) -> Option<String> {
    let prop_name = CString::new(name).ok()?;
    // SAFETY: ctx and obj are valid per caller contract
    unsafe {
        let js_name = JSStringCreateWithUTF8CString(prop_name.as_ptr());
        if js_name.is_null() {
            return None;
        }

        let mut exception: JSValueRef = ptr::null_mut();
        let value = JSObjectGetProperty(ctx, obj, js_name, &mut exception);
        JSStringRelease(js_name);

        if exception.is_null() && !value.is_null() && !JSValueIsUndefined(ctx, value) {
            value_to_string(ctx, value)
        } else {
            None
        }
    }
}

/// Get a number property from a JS object
unsafe fn get_number_property(ctx: JSContextRef, obj: JSObjectRef, name: &str) -> Option<f64> {
    let prop_name = CString::new(name).ok()?;
    // SAFETY: ctx and obj are valid per caller contract
    unsafe {
        let js_name = JSStringCreateWithUTF8CString(prop_name.as_ptr());
        if js_name.is_null() {
            return None;
        }

        let mut exception: JSValueRef = ptr::null_mut();
        let value = JSObjectGetProperty(ctx, obj, js_name, &mut exception);
        JSStringRelease(js_name);

        if exception.is_null() && !value.is_null() && JSValueIsNumber(ctx, value) {
            let mut ex: JSValueRef = ptr::null_mut();
            let num = JSValueToNumber(ctx, value, &mut ex);
            if ex.is_null() && !num.is_nan() {
                Some(num)
            } else {
                None
            }
        } else {
            None
        }
    }
}

/// Convert a JS value to a Rust string
unsafe fn value_to_string(ctx: JSContextRef, value: JSValueRef) -> Option<String> {
    // SAFETY: ctx and value are valid per caller contract
    unsafe {
        let mut exception: JSValueRef = ptr::null_mut();
        let js_str = JSValueToStringCopy(ctx, value, &mut exception);

        if js_str.is_null() || !exception.is_null() {
            return None;
        }

        let result = js_string_to_rust(js_str);
        JSStringRelease(js_str);
        Some(result)
    }
}
