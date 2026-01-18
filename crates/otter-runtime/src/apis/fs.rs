use crate::apis::{get_arg_as_string, make_exception};
use crate::bindings::*;
use crate::error::{JscError, JscResult};
use crate::extension::schedule_promise;
use crate::value::js_string_to_rust;
use std::ffi::CString;
use std::ptr;
use tokio::io::AsyncWriteExt;

pub fn register_fs_api(ctx: JSContextRef) -> JscResult<()> {
    unsafe {
        // 1. Get or Create 'Otter' global object
        let global = JSContextGetGlobalObject(ctx);
        let otter_key = CString::new("Otter").unwrap();
        let otter_key_ref = JSStringCreateWithUTF8CString(otter_key.as_ptr());

        let mut exception: JSValueRef = ptr::null_mut();
        let mut otter_val = JSObjectGetProperty(ctx, global, otter_key_ref, &mut exception);

        // Handle undefined or null
        if !exception.is_null() || JSValueIsUndefined(ctx, otter_val) || otter_val.is_null() {
            // Create new Otter object
            let obj = JSObjectMake(ctx, ptr::null_mut(), ptr::null_mut());
            let mut set_exc: JSValueRef = ptr::null_mut();
            JSObjectSetProperty(
                ctx,
                global,
                otter_key_ref,
                obj as JSValueRef,
                K_JS_PROPERTY_ATTRIBUTE_NONE,
                &mut set_exc,
            );
            otter_val = obj as JSValueRef;
        }

        let otter_obj = JSValueToObject(ctx, otter_val, &mut exception);
        JSStringRelease(otter_key_ref);

        if otter_obj.is_null() {
            return Err(JscError::internal("Failed to get/create Otter object"));
        }

        // 2. Register readFile
        register_method(ctx, otter_obj, "readFile", Some(js_read_file))?;

        // 3. Register writeFile
        register_method(ctx, otter_obj, "writeFile", Some(js_write_file))?;
    }
    Ok(())
}

fn register_method(
    ctx: JSContextRef,
    object: JSObjectRef,
    name: &str,
    callback: JSObjectCallAsFunctionCallback,
) -> JscResult<()> {
    unsafe {
        let name_cstr = CString::new(name).unwrap();
        let name_ref = JSStringCreateWithUTF8CString(name_cstr.as_ptr());
        let func = JSObjectMakeFunctionWithCallback(ctx, name_ref, callback);

        let mut exception: JSValueRef = ptr::null_mut();
        JSObjectSetProperty(
            ctx,
            object,
            name_ref,
            func as JSValueRef,
            K_JS_PROPERTY_ATTRIBUTE_NONE,
            &mut exception,
        );
        JSStringRelease(name_ref);
    }
    Ok(())
}

unsafe extern "C" fn js_read_file(
    ctx: JSContextRef,
    _function: JSObjectRef,
    _this_object: JSObjectRef,
    argument_count: usize,
    arguments: *const JSValueRef,
    exception: *mut JSValueRef,
) -> JSValueRef {
    let path = match get_arg_as_string(ctx, arguments, 0, argument_count) {
        Some(s) => s,
        None => {
            *exception = make_exception(ctx, "TypeError: Path must be a string");
            return JSValueMakeUndefined(ctx);
        }
    };

    let future = async move {
        match tokio::fs::read_to_string(&path).await {
            Ok(content) => Ok(serde_json::Value::String(content)),
            Err(e) => Err(JscError::script_error("IOError", &e.to_string())),
        }
    };

    match schedule_promise(ctx, future) {
        Ok(promise) => promise,
        Err(e) => {
            *exception = make_exception(ctx, &e.to_string());
            JSValueMakeUndefined(ctx)
        }
    }
}

unsafe extern "C" fn js_write_file(
    ctx: JSContextRef,
    _function: JSObjectRef,
    _this_object: JSObjectRef,
    argument_count: usize,
    arguments: *const JSValueRef,
    exception: *mut JSValueRef,
) -> JSValueRef {
    let path = match get_arg_as_string(ctx, arguments, 0, argument_count) {
        Some(s) => s,
        None => {
            *exception = make_exception(ctx, "TypeError: Path must be a string");
            return JSValueMakeUndefined(ctx);
        }
    };

    let data = match get_arg_as_string(ctx, arguments, 1, argument_count) {
        Some(s) => s,
        None => {
            *exception = make_exception(ctx, "TypeError: Data must be a string");
            return JSValueMakeUndefined(ctx);
        }
    };

    let future = async move {
        match tokio::fs::write(&path, data).await {
            Ok(_) => Ok(serde_json::Value::Null),
            Err(e) => Err(JscError::script_error("IOError", &e.to_string())),
        }
    };

    match schedule_promise(ctx, future) {
        Ok(promise) => promise,
        Err(e) => {
            *exception = make_exception(ctx, &e.to_string());
            JSValueMakeUndefined(ctx)
        }
    }
}
