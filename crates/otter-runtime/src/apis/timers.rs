//! Timers API implementation (setTimeout, setInterval, clearTimeout, clearInterval, queueMicrotask)

use crate::bindings::*;
use crate::error::JscResult;
use crate::event_loop::{
    collect_args, create_id_value, event_loop_for_context, get_delay_arg, get_function_arg,
    parse_id_arg,
};
use std::ffi::CString;
use std::ptr;

pub fn register_timers_api(ctx: JSContextRef) -> JscResult<()> {
    unsafe {
        register_timer_fn(ctx, "setTimeout", Some(js_set_timeout))?;
        register_timer_fn(ctx, "setInterval", Some(js_set_interval))?;
        register_timer_fn(ctx, "clearTimeout", Some(js_clear_timeout))?;
        register_timer_fn(ctx, "clearInterval", Some(js_clear_timeout))?;
        register_timer_fn(ctx, "queueMicrotask", Some(js_queue_microtask))?;
    }

    Ok(())
}

unsafe fn register_timer_fn(
    ctx: JSContextRef,
    name: &str,
    callback: JSObjectCallAsFunctionCallback,
) -> JscResult<()> {
    let name_cstr = CString::new(name).unwrap();
    let name_ref = JSStringCreateWithUTF8CString(name_cstr.as_ptr());
    let func = JSObjectMakeFunctionWithCallback(ctx, name_ref, callback);

    let mut exception: JSValueRef = ptr::null_mut();
    JSObjectSetProperty(
        ctx,
        JSContextGetGlobalObject(ctx),
        name_ref,
        func as JSValueRef,
        K_JS_PROPERTY_ATTRIBUTE_NONE,
        &mut exception,
    );

    JSStringRelease(name_ref);
    Ok(())
}

unsafe extern "C" fn js_set_timeout(
    ctx: JSContextRef,
    _function: JSObjectRef,
    _this_object: JSObjectRef,
    argument_count: usize,
    arguments: *const JSValueRef,
    exception: *mut JSValueRef,
) -> JSValueRef {
    let callback = match get_function_arg(ctx, arguments, 0, argument_count) {
        Ok(value) => value,
        Err(err) => {
            *exception = crate::apis::make_exception(ctx, &err.to_string());
            return JSValueMakeUndefined(ctx);
        }
    };

    let delay = match get_delay_arg(ctx, arguments, 1, argument_count) {
        Ok(value) => value,
        Err(err) => {
            *exception = crate::apis::make_exception(ctx, &err.to_string());
            return JSValueMakeUndefined(ctx);
        }
    };

    let args = collect_args(arguments, 2, argument_count);
    let Some(loop_ref) = event_loop_for_context(ctx) else {
        *exception = crate::apis::make_exception(ctx, "Event loop not available");
        return JSValueMakeUndefined(ctx);
    };

    match loop_ref.schedule_timer(callback, delay, None, args) {
        Ok(id) => create_id_value(ctx, id),
        Err(err) => {
            *exception = crate::apis::make_exception(ctx, &err.to_string());
            JSValueMakeUndefined(ctx)
        }
    }
}

unsafe extern "C" fn js_set_interval(
    ctx: JSContextRef,
    _function: JSObjectRef,
    _this_object: JSObjectRef,
    argument_count: usize,
    arguments: *const JSValueRef,
    exception: *mut JSValueRef,
) -> JSValueRef {
    let callback = match get_function_arg(ctx, arguments, 0, argument_count) {
        Ok(value) => value,
        Err(err) => {
            *exception = crate::apis::make_exception(ctx, &err.to_string());
            return JSValueMakeUndefined(ctx);
        }
    };

    let delay = match get_delay_arg(ctx, arguments, 1, argument_count) {
        Ok(value) => value,
        Err(err) => {
            *exception = crate::apis::make_exception(ctx, &err.to_string());
            return JSValueMakeUndefined(ctx);
        }
    };

    let args = collect_args(arguments, 2, argument_count);
    let Some(loop_ref) = event_loop_for_context(ctx) else {
        *exception = crate::apis::make_exception(ctx, "Event loop not available");
        return JSValueMakeUndefined(ctx);
    };

    match loop_ref.schedule_timer(callback, delay, Some(delay), args) {
        Ok(id) => create_id_value(ctx, id),
        Err(err) => {
            *exception = crate::apis::make_exception(ctx, &err.to_string());
            JSValueMakeUndefined(ctx)
        }
    }
}

unsafe extern "C" fn js_clear_timeout(
    ctx: JSContextRef,
    _function: JSObjectRef,
    _this_object: JSObjectRef,
    argument_count: usize,
    arguments: *const JSValueRef,
    exception: *mut JSValueRef,
) -> JSValueRef {
    let id = match parse_id_arg(ctx, arguments, 0, argument_count) {
        Ok(value) => value,
        Err(err) => {
            *exception = crate::apis::make_exception(ctx, &err.to_string());
            return JSValueMakeUndefined(ctx);
        }
    };

    let Some(loop_ref) = event_loop_for_context(ctx) else {
        *exception = crate::apis::make_exception(ctx, "Event loop not available");
        return JSValueMakeUndefined(ctx);
    };

    loop_ref.clear_timer(id);
    JSValueMakeUndefined(ctx)
}

unsafe extern "C" fn js_queue_microtask(
    ctx: JSContextRef,
    _function: JSObjectRef,
    _this_object: JSObjectRef,
    argument_count: usize,
    arguments: *const JSValueRef,
    exception: *mut JSValueRef,
) -> JSValueRef {
    let callback = match get_function_arg(ctx, arguments, 0, argument_count) {
        Ok(value) => value,
        Err(err) => {
            *exception = crate::apis::make_exception(ctx, &err.to_string());
            return JSValueMakeUndefined(ctx);
        }
    };

    let Some(loop_ref) = event_loop_for_context(ctx) else {
        *exception = crate::apis::make_exception(ctx, "Event loop not available");
        return JSValueMakeUndefined(ctx);
    };

    if let Err(err) = loop_ref.queue_microtask(callback) {
        *exception = crate::apis::make_exception(ctx, &err.to_string());
        return JSValueMakeUndefined(ctx);
    }

    JSValueMakeUndefined(ctx)
}
