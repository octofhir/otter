//! Core JSC Context wrapper with safe evaluation and object management

use jsc_sys::*;
use std::ffi::CString;
use std::marker::PhantomData;
use std::ptr;

use crate::error::{JscError, JscResult};
use crate::value::{extract_exception, JscValue};

/// A JavaScript execution context
///
/// Wraps a JSGlobalContext and provides safe methods for script evaluation
/// and object manipulation. This is the core context without event loop
/// or extension support - use otter-runtime for those features.
///
/// # Thread Safety
///
/// This type is `!Send` and `!Sync` because JavaScriptCore contexts are not
/// thread-safe. Accessing a context from multiple threads causes undefined behavior.
pub struct JscContext {
    ctx: JSGlobalContextRef,
    /// Marker to make this type !Send + !Sync
    _not_send: PhantomData<*mut ()>,
}

impl JscContext {
    /// Create a new JavaScript context
    pub fn new() -> JscResult<Self> {
        // SAFETY: JSGlobalContextCreate with null creates a default context
        unsafe {
            let ctx = JSGlobalContextCreate(ptr::null_mut());
            if ctx.is_null() {
                return Err(JscError::ContextCreation {
                    message: "JSGlobalContextCreate returned null".to_string(),
                });
            }

            Ok(Self {
                ctx,
                _not_send: PhantomData,
            })
        }
    }

    /// Get the raw context pointer
    pub fn raw(&self) -> JSContextRef {
        self.ctx as JSContextRef
    }

    /// Get the raw global context pointer
    pub fn raw_global(&self) -> JSGlobalContextRef {
        self.ctx
    }

    /// Get the global object
    pub fn global_object(&self) -> JSObjectRef {
        // SAFETY: self.ctx is valid
        unsafe { JSContextGetGlobalObject(self.ctx as JSContextRef) }
    }

    /// Evaluate a JavaScript script and return the result
    pub fn eval(&self, script: &str) -> JscResult<JscValue> {
        self.eval_with_source(script, "<eval>")
    }

    /// Evaluate a JavaScript script with source URL (for better error messages)
    pub fn eval_with_source(&self, script: &str, source_url: &str) -> JscResult<JscValue> {
        let script_cstr = CString::new(script)
            .map_err(|e| JscError::Internal(format!("Invalid script: {}", e)))?;
        let source_cstr = CString::new(source_url)
            .map_err(|e| JscError::Internal(format!("Invalid source URL: {}", e)))?;

        // SAFETY: CStrings are valid null-terminated, ctx is valid
        unsafe {
            let script_ref = JSStringCreateWithUTF8CString(script_cstr.as_ptr());
            let source_ref = JSStringCreateWithUTF8CString(source_cstr.as_ptr());
            let mut exception: JSValueRef = ptr::null_mut();

            let result = JSEvaluateScript(
                self.ctx as JSContextRef,
                script_ref,
                ptr::null_mut(),
                source_ref,
                1,
                &mut exception,
            );

            JSStringRelease(script_ref);
            JSStringRelease(source_ref);

            if !exception.is_null() {
                return Err(extract_exception(self.ctx as JSContextRef, exception));
            }

            Ok(JscValue::new(self.ctx as JSContextRef, result))
        }
    }

    /// Set a property on the global object
    pub fn set_global(&self, name: &str, value: &JscValue) -> JscResult<()> {
        let name_cstr =
            CString::new(name).map_err(|e| JscError::Internal(format!("Invalid name: {}", e)))?;

        // SAFETY: CString is valid, ctx is valid
        unsafe {
            let name_ref = JSStringCreateWithUTF8CString(name_cstr.as_ptr());
            let mut exception: JSValueRef = ptr::null_mut();

            JSObjectSetProperty(
                self.ctx as JSContextRef,
                self.global_object(),
                name_ref,
                value.raw(),
                K_JS_PROPERTY_ATTRIBUTE_NONE,
                &mut exception,
            );

            JSStringRelease(name_ref);

            if !exception.is_null() {
                return Err(extract_exception(self.ctx as JSContextRef, exception));
            }

            Ok(())
        }
    }

    /// Get a property from the global object
    pub fn get_global(&self, name: &str) -> JscResult<JscValue> {
        let name_cstr =
            CString::new(name).map_err(|e| JscError::Internal(format!("Invalid name: {}", e)))?;

        // SAFETY: CString is valid, ctx is valid
        unsafe {
            let name_ref = JSStringCreateWithUTF8CString(name_cstr.as_ptr());
            let mut exception: JSValueRef = ptr::null_mut();

            let value = JSObjectGetProperty(
                self.ctx as JSContextRef,
                self.global_object(),
                name_ref,
                &mut exception,
            );

            JSStringRelease(name_ref);

            if !exception.is_null() {
                return Err(extract_exception(self.ctx as JSContextRef, exception));
            }

            Ok(JscValue::new(self.ctx as JSContextRef, value))
        }
    }

    /// Create an empty JavaScript object
    pub fn create_object(&self) -> JscValue {
        // SAFETY: ctx is valid, null class creates plain object
        unsafe {
            let obj = JSObjectMake(self.ctx as JSContextRef, ptr::null_mut(), ptr::null_mut());
            JscValue::new(self.ctx as JSContextRef, obj as JSValueRef)
        }
    }

    /// Set a property on an object
    pub fn set_property(&self, object: JSObjectRef, name: &str, value: &JscValue) -> JscResult<()> {
        let name_cstr =
            CString::new(name).map_err(|e| JscError::Internal(format!("Invalid name: {}", e)))?;

        // SAFETY: CString is valid, ctx and object are valid
        unsafe {
            let name_ref = JSStringCreateWithUTF8CString(name_cstr.as_ptr());
            let mut exception: JSValueRef = ptr::null_mut();

            JSObjectSetProperty(
                self.ctx as JSContextRef,
                object,
                name_ref,
                value.raw(),
                K_JS_PROPERTY_ATTRIBUTE_NONE,
                &mut exception,
            );

            JSStringRelease(name_ref);

            if !exception.is_null() {
                return Err(extract_exception(self.ctx as JSContextRef, exception));
            }

            Ok(())
        }
    }

    /// Register a native function callback
    ///
    /// The callback will be exposed to JavaScript with the given name on the global object.
    pub fn register_function(
        &self,
        name: &str,
        callback: JSObjectCallAsFunctionCallback,
    ) -> JscResult<()> {
        let name_cstr =
            CString::new(name).map_err(|e| JscError::Internal(format!("Invalid name: {}", e)))?;

        // SAFETY: CString is valid, ctx is valid
        unsafe {
            let name_ref = JSStringCreateWithUTF8CString(name_cstr.as_ptr());
            let func =
                JSObjectMakeFunctionWithCallback(self.ctx as JSContextRef, name_ref, callback);

            let mut exception: JSValueRef = ptr::null_mut();
            JSObjectSetProperty(
                self.ctx as JSContextRef,
                self.global_object(),
                name_ref,
                func as JSValueRef,
                K_JS_PROPERTY_ATTRIBUTE_NONE,
                &mut exception,
            );

            JSStringRelease(name_ref);

            if !exception.is_null() {
                return Err(extract_exception(self.ctx as JSContextRef, exception));
            }

            Ok(())
        }
    }

    /// Force garbage collection
    pub fn gc(&self) {
        // SAFETY: ctx is valid
        unsafe {
            JSGarbageCollect(self.ctx as JSContextRef);
        }
    }

    /// Inject a JSON object as a global variable
    pub fn inject_json(&self, name: &str, json: &str) -> JscResult<()> {
        let value = JscValue::from_json(self.ctx as JSContextRef, json)?;
        self.set_global(name, &value)
    }

    /// Create a string value
    pub fn string(&self, s: &str) -> JscResult<JscValue> {
        JscValue::string(self.ctx as JSContextRef, s)
    }

    /// Create a number value
    pub fn number(&self, n: f64) -> JscValue {
        JscValue::number(self.ctx as JSContextRef, n)
    }

    /// Create a boolean value
    pub fn boolean(&self, b: bool) -> JscValue {
        JscValue::boolean(self.ctx as JSContextRef, b)
    }

    /// Create an undefined value
    pub fn undefined(&self) -> JscValue {
        JscValue::undefined(self.ctx as JSContextRef)
    }

    /// Create a null value
    pub fn null(&self) -> JscValue {
        JscValue::null(self.ctx as JSContextRef)
    }
}

impl Drop for JscContext {
    fn drop(&mut self) {
        // SAFETY: ctx was created by JSGlobalContextCreate
        unsafe {
            JSGlobalContextRelease(self.ctx);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_context_creation() {
        let ctx = JscContext::new().unwrap();
        drop(ctx);
    }

    #[test]
    fn test_eval_number() {
        let ctx = JscContext::new().unwrap();
        let result = ctx.eval("1 + 1").unwrap();
        assert_eq!(result.to_number().unwrap(), 2.0);
    }

    #[test]
    fn test_eval_string() {
        let ctx = JscContext::new().unwrap();
        let result = ctx.eval("'hello'").unwrap();
        assert_eq!(result.to_string().unwrap(), "hello");
    }

    #[test]
    fn test_eval_error() {
        let ctx = JscContext::new().unwrap();
        let result = ctx.eval("throw new Error('oops')");
        assert!(result.is_err());
    }

    #[test]
    fn test_set_get_global() {
        let ctx = JscContext::new().unwrap();
        let value = ctx.number(42.0);
        ctx.set_global("myVar", &value).unwrap();

        let result = ctx.eval("myVar * 2").unwrap();
        assert_eq!(result.to_number().unwrap(), 84.0);
    }

    #[test]
    fn test_inject_json() {
        let ctx = JscContext::new().unwrap();
        ctx.inject_json("config", r#"{"name": "test", "value": 123}"#)
            .unwrap();

        let name = ctx.eval("config.name").unwrap();
        assert_eq!(name.to_string().unwrap(), "test");

        let value = ctx.eval("config.value").unwrap();
        assert_eq!(value.to_number().unwrap(), 123.0);
    }
}
