//! Safe wrapper for JSC objects with property access and function support

use otter_jsc_sys::*;
use std::ffi::CString;
use std::marker::PhantomData;
use std::ptr;

use crate::error::{JscError, JscResult};
use crate::value::{JscValue, extract_exception};

/// A JavaScript object with automatic GC protection
///
/// Provides safe methods for property access, function calls, and array operations.
///
/// # Thread Safety
///
/// This type is `!Send` and `!Sync` because JavaScript objects are tied to
/// their context's thread. Cross-thread access causes undefined behavior.
pub struct JscObject {
    object: JSObjectRef,
    ctx: JSContextRef,
    /// Marker to make this type !Send + !Sync
    _not_send: PhantomData<*mut ()>,
}

impl JscObject {
    /// Create a new JscObject wrapper
    ///
    /// # Safety
    /// - `ctx` must be a valid JSContextRef
    /// - `object` must be a valid JSObjectRef from the same context
    pub unsafe fn new(ctx: JSContextRef, object: JSObjectRef) -> Self {
        if !object.is_null() {
            // SAFETY: ctx and object are valid per preconditions
            unsafe { JSValueProtect(ctx, object as JSValueRef) };
        }
        Self {
            object,
            ctx,
            _not_send: PhantomData,
        }
    }

    /// Create an empty JavaScript object
    pub fn empty(ctx: JSContextRef) -> Self {
        // SAFETY: JSObjectMake with null class creates a plain object
        unsafe {
            let object = JSObjectMake(ctx, ptr::null_mut(), ptr::null_mut());
            Self::new(ctx, object)
        }
    }

    /// Get the raw object reference
    pub fn raw(&self) -> JSObjectRef {
        self.object
    }

    /// Get the context
    pub fn context(&self) -> JSContextRef {
        self.ctx
    }

    /// Convert to JscValue
    pub fn to_value(&self) -> JscValue {
        // SAFETY: self.ctx and self.object are valid
        unsafe { JscValue::new(self.ctx, self.object as JSValueRef) }
    }

    /// Check if the object is a function
    pub fn is_function(&self) -> bool {
        // SAFETY: self.ctx and self.object are valid
        unsafe { JSObjectIsFunction(self.ctx, self.object) }
    }

    /// Check if the object is an array
    pub fn is_array(&self) -> bool {
        // SAFETY: self.ctx and self.object are valid
        unsafe { JSValueIsArray(self.ctx, self.object as JSValueRef) }
    }

    /// Get a property by name
    pub fn get(&self, name: &str) -> JscResult<JscValue> {
        let name_cstr =
            CString::new(name).map_err(|e| JscError::Internal(format!("Invalid name: {}", e)))?;

        // SAFETY: CString is valid, ctx and object are valid
        unsafe {
            let name_ref = JSStringCreateWithUTF8CString(name_cstr.as_ptr());
            let mut exception: JSValueRef = ptr::null_mut();

            let value = JSObjectGetProperty(self.ctx, self.object, name_ref, &mut exception);

            JSStringRelease(name_ref);

            if !exception.is_null() {
                return Err(extract_exception(self.ctx, exception));
            }

            Ok(JscValue::new(self.ctx, value))
        }
    }

    /// Set a property by name
    pub fn set(&self, name: &str, value: &JscValue) -> JscResult<()> {
        let name_cstr =
            CString::new(name).map_err(|e| JscError::Internal(format!("Invalid name: {}", e)))?;

        // SAFETY: CString is valid, ctx and object are valid
        unsafe {
            let name_ref = JSStringCreateWithUTF8CString(name_cstr.as_ptr());
            let mut exception: JSValueRef = ptr::null_mut();

            JSObjectSetProperty(
                self.ctx,
                self.object,
                name_ref,
                value.raw(),
                K_JS_PROPERTY_ATTRIBUTE_NONE,
                &mut exception,
            );

            JSStringRelease(name_ref);

            if !exception.is_null() {
                return Err(extract_exception(self.ctx, exception));
            }

            Ok(())
        }
    }

    /// Check if a property exists
    pub fn has(&self, name: &str) -> JscResult<bool> {
        let name_cstr =
            CString::new(name).map_err(|e| JscError::Internal(format!("Invalid name: {}", e)))?;

        // SAFETY: CString is valid, ctx and object are valid
        unsafe {
            let name_ref = JSStringCreateWithUTF8CString(name_cstr.as_ptr());
            let has = JSObjectHasProperty(self.ctx, self.object, name_ref);
            JSStringRelease(name_ref);
            Ok(has)
        }
    }

    /// Delete a property
    pub fn delete(&self, name: &str) -> JscResult<bool> {
        let name_cstr =
            CString::new(name).map_err(|e| JscError::Internal(format!("Invalid name: {}", e)))?;

        // SAFETY: CString is valid, ctx and object are valid
        unsafe {
            let name_ref = JSStringCreateWithUTF8CString(name_cstr.as_ptr());
            let mut exception: JSValueRef = ptr::null_mut();

            let deleted = JSObjectDeleteProperty(self.ctx, self.object, name_ref, &mut exception);

            JSStringRelease(name_ref);

            if !exception.is_null() {
                return Err(extract_exception(self.ctx, exception));
            }

            Ok(deleted)
        }
    }

    /// Get array element by index
    pub fn get_index(&self, index: u32) -> JscResult<JscValue> {
        // SAFETY: ctx and object are valid
        unsafe {
            let mut exception: JSValueRef = ptr::null_mut();
            let value = JSObjectGetPropertyAtIndex(self.ctx, self.object, index, &mut exception);

            if !exception.is_null() {
                return Err(extract_exception(self.ctx, exception));
            }

            Ok(JscValue::new(self.ctx, value))
        }
    }

    /// Set array element by index
    pub fn set_index(&self, index: u32, value: &JscValue) -> JscResult<()> {
        // SAFETY: ctx and object are valid
        unsafe {
            let mut exception: JSValueRef = ptr::null_mut();
            JSObjectSetPropertyAtIndex(self.ctx, self.object, index, value.raw(), &mut exception);

            if !exception.is_null() {
                return Err(extract_exception(self.ctx, exception));
            }

            Ok(())
        }
    }

    /// Call this object as a function with arguments
    pub fn call(&self, this: Option<&JscObject>, args: &[&JscValue]) -> JscResult<JscValue> {
        if !self.is_function() {
            return Err(JscError::type_error("function", "non-function"));
        }

        let this_obj = this.map(|o| o.object).unwrap_or(ptr::null_mut());
        let arg_values: Vec<JSValueRef> = args.iter().map(|v| v.raw()).collect();

        // SAFETY: ctx and object are valid, args are valid values
        unsafe {
            let mut exception: JSValueRef = ptr::null_mut();
            let result = JSObjectCallAsFunction(
                self.ctx,
                self.object,
                this_obj,
                arg_values.len(),
                if arg_values.is_empty() {
                    ptr::null()
                } else {
                    arg_values.as_ptr()
                },
                &mut exception,
            );

            if !exception.is_null() {
                return Err(extract_exception(self.ctx, exception));
            }

            Ok(JscValue::new(self.ctx, result))
        }
    }

    /// Get array length (for array objects)
    pub fn length(&self) -> JscResult<u32> {
        let len_value = self.get("length")?;
        let len = len_value.to_number()?;
        Ok(len as u32)
    }
}

impl Drop for JscObject {
    fn drop(&mut self) {
        if !self.object.is_null() {
            // SAFETY: object was protected in new(), now unprotecting
            unsafe {
                JSValueUnprotect(self.ctx, self.object as JSValueRef);
            }
        }
    }
}

impl std::fmt::Debug for JscObject {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "JscObject({:?})", self.object)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::JscContext;

    #[test]
    fn test_empty_object() {
        let ctx = JscContext::new().unwrap();
        let obj = JscObject::empty(ctx.raw());
        assert!(!obj.is_function());
        assert!(!obj.is_array());
    }

    #[test]
    fn test_property_access() {
        let ctx = JscContext::new().unwrap();
        let obj = JscObject::empty(ctx.raw());

        let value = ctx.number(42.0);
        obj.set("foo", &value).unwrap();

        assert!(obj.has("foo").unwrap());
        assert!(!obj.has("bar").unwrap());

        let got = obj.get("foo").unwrap();
        assert_eq!(got.to_number().unwrap(), 42.0);
    }

    #[test]
    fn test_delete_property() {
        let ctx = JscContext::new().unwrap();
        let obj = JscObject::empty(ctx.raw());

        let value = ctx.number(42.0);
        obj.set("foo", &value).unwrap();
        assert!(obj.has("foo").unwrap());

        obj.delete("foo").unwrap();
        assert!(!obj.has("foo").unwrap());
    }

    #[test]
    fn test_array_access() {
        let ctx = JscContext::new().unwrap();
        let result = ctx.eval("[1, 2, 3]").unwrap();

        // Convert value to object
        let arr = unsafe { JscObject::new(ctx.raw(), result.raw() as JSObjectRef) };

        assert!(arr.is_array());
        assert_eq!(arr.length().unwrap(), 3);
        assert_eq!(arr.get_index(0).unwrap().to_number().unwrap(), 1.0);
        assert_eq!(arr.get_index(1).unwrap().to_number().unwrap(), 2.0);
        assert_eq!(arr.get_index(2).unwrap().to_number().unwrap(), 3.0);
    }

    #[test]
    fn test_function_call() {
        let ctx = JscContext::new().unwrap();
        let result = ctx.eval("(function(a, b) { return a + b; })").unwrap();

        let func = unsafe { JscObject::new(ctx.raw(), result.raw() as JSObjectRef) };
        assert!(func.is_function());

        let arg1 = ctx.number(2.0);
        let arg2 = ctx.number(3.0);
        let sum = func.call(None, &[&arg1, &arg2]).unwrap();

        assert_eq!(sum.to_number().unwrap(), 5.0);
    }
}
