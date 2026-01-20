//! Runtime Context wrapper with event loop and extension support
//!
//! Wraps jsc-core::JscContext and adds runtime-specific features
//! like event loop, timers, and extension registry.

// Arc is used intentionally here for shared ownership with global registries.
// The types aren't Send+Sync because JSC contexts are thread-bound, but Arc is
// still appropriate for reference counting within a single thread's context.
#![allow(clippy::arc_with_non_send_sync)]

use crate::bindings::*;
use crate::error::{JscError, JscResult};
use crate::event_loop::{EventLoop, register_context_event_loop, unregister_context_event_loop};
use crate::extension::{
    Extension, ExtensionRegistry, register_context_registry, unregister_context_registry,
};
use crate::value::JscValue;
use otter_jsc_core::extract_exception;
use parking_lot::Mutex;
use std::ffi::CString;
use std::ptr;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Global lock for JSC context creation.
/// JSC's initialization is not fully thread-safe, so we serialize context creation.
static CONTEXT_CREATION_LOCK: Mutex<()> = Mutex::new(());

/// A JavaScript execution context with runtime features
///
/// Wraps jsc-core::JscContext and adds event loop, timers, and extension support.
/// For basic JSC operations without runtime features, use jsc-core::JscContext directly.
pub struct JscContext {
    group: JSContextGroupRef,
    ctx: JSGlobalContextRef,
    extension_registry: Arc<ExtensionRegistry>,
    event_loop: Arc<EventLoop>,
}

impl JscContext {
    /// Create a new JavaScript context with event loop support
    ///
    /// This function is thread-safe - context creation is serialized
    /// to avoid race conditions in JSC's initialization.
    pub fn new() -> JscResult<Self> {
        // Serialize context creation to avoid JSC initialization race conditions
        let _guard = CONTEXT_CREATION_LOCK.lock();

        // SAFETY: JSGlobalContextCreate with null creates a default context
        unsafe {
            let group = JSContextGroupCreate();
            if group.is_null() {
                return Err(JscError::context_creation(
                    "JSContextGroupCreate returned null",
                ));
            }

            let ctx = JSGlobalContextCreateInGroup(group, ptr::null_mut());
            if ctx.is_null() {
                JSContextGroupRelease(group);
                return Err(JscError::context_creation(
                    "JSGlobalContextCreateInGroup returned null",
                ));
            }

            let registry = Arc::new(ExtensionRegistry::new());
            register_context_registry(ctx as JSContextRef, registry.clone());

            let event_loop = Arc::new(EventLoop::new(ctx as JSContextRef));
            register_context_event_loop(ctx as JSContextRef, event_loop.clone());

            Ok(Self {
                group,
                ctx,
                extension_registry: registry,
                event_loop,
            })
        }
    }

    /// Get the raw context pointer
    pub fn raw(&self) -> JSContextRef {
        self.ctx as JSContextRef
    }

    /// Get the global object
    pub fn global_object(&self) -> JSObjectRef {
        // SAFETY: self.ctx is valid
        unsafe { JSContextGetGlobalObject(self.ctx as JSContextRef) }
    }

    /// Register a safe extension on this context
    pub fn register_extension(&self, extension: Extension) -> JscResult<()> {
        self.extension_registry
            .register_extension(extension, self.ctx as JSContextRef)
    }

    /// Poll pending async ops and resolve Promises
    pub fn poll_promises(&self) -> JscResult<usize> {
        self.extension_registry
            .poll_promises(self.ctx as JSContextRef)
    }

    /// Run the full event loop tick (microtasks + timers + async ops + JS poll handlers)
    ///
    /// This function loops until no more work is done in a single iteration,
    /// ensuring that Promise continuations triggered by JS poll handlers
    /// (like child process close events) are properly processed.
    pub fn poll_event_loop(&self) -> JscResult<usize> {
        let mut total_handled = 0;
        loop {
            let mut handled = 0;
            handled += self.poll_promises()?;
            handled += self.event_loop.poll()?;
            handled += self.poll_js_handlers()?;
            total_handled += handled;

            // If no work was done this iteration, we're done
            if handled == 0 {
                break;
            }
            // Otherwise loop to process any newly scheduled work
            // (e.g., Promise continuations from resolved child process events)
        }

        // Final trigger: evaluate an empty expression to ensure JSC drains
        // any pending Promise jobs that were scheduled during the poll.
        // This is necessary because JSC's internal Promise job queue is
        // processed during script evaluation, and we need to ensure
        // async/await continuations are run even when no events are pending.
        if total_handled > 0 {
            let _ = self.eval("void 0");
        }

        Ok(total_handled)
    }

    /// Call JavaScript poll handlers (__otter_poll_all)
    fn poll_js_handlers(&self) -> JscResult<usize> {
        // Use eval to safely call the JS poll function without raw FFI
        // The function returns the count of handled events, or undefined if not defined
        let result = self.eval(
            "typeof __otter_poll_all === 'function' ? __otter_poll_all() : 0"
        );

        match result {
            Ok(value) => {
                if let Ok(count) = value.to_number() {
                    Ok(count as usize)
                } else {
                    Ok(0)
                }
            }
            Err(_) => {
                // Silently ignore poll errors to avoid breaking the event loop
                Ok(0)
            }
        }
    }

    pub fn has_pending_tasks(&self) -> bool {
        self.extension_registry.has_pending_async_ops()
            || self.event_loop.has_pending_tasks()
            || self.has_pending_js_refs()
            || self.is_main_promise_pending()
    }

    fn has_pending_js_refs(&self) -> bool {
        let result = self.eval(
            "typeof __otter_refed_count === 'function' ? __otter_refed_count() : 0",
        );

        match result {
            Ok(value) => value.to_number().map(|count| count > 0.0).unwrap_or(false),
            Err(_) => false,
        }
    }

    fn is_main_promise_pending(&self) -> bool {
        let result = self.eval(
            "typeof __otter_is_main_promise_pending === 'function' ? __otter_is_main_promise_pending() : false",
        );

        match result {
            Ok(value) => value.to_bool(),
            Err(_) => false,
        }
    }

    pub fn next_wake_delay(&self) -> Duration {
        if let Some(deadline) = self.event_loop.next_timer_deadline() {
            let now = Instant::now();
            if deadline > now {
                return deadline - now;
            }
        }
        Duration::from_millis(1)
    }

    /// Run the event loop until idle or timeout
    pub fn run_event_loop_until_idle(&self, timeout: Duration) -> JscResult<()> {
        let start = Instant::now();
        loop {
            let handled = self.poll_event_loop()?;
            if handled == 0 && !self.has_pending_tasks() {
                return Ok(());
            }

            if timeout != Duration::ZERO && start.elapsed() >= timeout {
                return Err(JscError::Timeout(timeout.as_millis() as u64));
            }

            let sleep_for = self.next_wake_delay();
            std::thread::sleep(sleep_for);
        }
    }

    /// Evaluate a JavaScript script and return the result
    pub fn eval(&self, script: &str) -> JscResult<JscValue> {
        self.eval_with_source(script, "<eval>")
    }

    /// Evaluate a JavaScript script with source URL (for better error messages)
    pub fn eval_with_source(&self, script: &str, source_url: &str) -> JscResult<JscValue> {
        let script_cstr = CString::new(script)
            .map_err(|e| JscError::internal(format!("Invalid script: {}", e)))?;
        let source_cstr = CString::new(source_url)
            .map_err(|e| JscError::internal(format!("Invalid source URL: {}", e)))?;

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
                return Err(extract_exception(self.ctx as JSContextRef, exception).into());
            }

            Ok(JscValue::new(self.ctx as JSContextRef, result))
        }
    }

    /// Set a property on the global object
    pub fn set_global(&self, name: &str, value: &JscValue) -> JscResult<()> {
        let name_cstr =
            CString::new(name).map_err(|e| JscError::internal(format!("Invalid name: {}", e)))?;

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
                return Err(extract_exception(self.ctx as JSContextRef, exception).into());
            }

            Ok(())
        }
    }

    /// Get a property from the global object
    pub fn get_global(&self, name: &str) -> JscResult<JscValue> {
        let name_cstr =
            CString::new(name).map_err(|e| JscError::internal(format!("Invalid name: {}", e)))?;

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
                return Err(extract_exception(self.ctx as JSContextRef, exception).into());
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
            CString::new(name).map_err(|e| JscError::internal(format!("Invalid name: {}", e)))?;

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
                return Err(extract_exception(self.ctx as JSContextRef, exception).into());
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
            CString::new(name).map_err(|e| JscError::internal(format!("Invalid name: {}", e)))?;

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
                return Err(extract_exception(self.ctx as JSContextRef, exception).into());
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
        Ok(JscValue::string(self.ctx as JSContextRef, s)?)
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
        unregister_context_event_loop(self.ctx as JSContextRef);
        unregister_context_registry(self.ctx as JSContextRef);
        // SAFETY: ctx was created by JSGlobalContextCreate
        unsafe {
            JSGlobalContextRelease(self.ctx);
            JSContextGroupRelease(self.group);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_create_context() {
        let ctx = JscContext::new().unwrap();
        drop(ctx);
    }

    #[test]
    fn test_eval_simple() {
        let ctx = JscContext::new().unwrap();
        let result = ctx.eval("2 + 2").unwrap();
        assert_eq!(result.to_number().unwrap(), 4.0);
    }

    #[test]
    fn test_eval_string() {
        let ctx = JscContext::new().unwrap();
        let result = ctx.eval("'hello' + ' ' + 'world'").unwrap();
        assert_eq!(result.to_string().unwrap(), "hello world");
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

    #[test]
    fn test_eval_error() {
        let ctx = JscContext::new().unwrap();
        let result = ctx.eval("throw new Error('test error')");
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.to_string().contains("test error"));
    }
}
