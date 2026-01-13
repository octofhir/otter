//! Console API implementation
//!
//! Provides `console.log`, `console.warn`, `console.error`, `console.debug`, and `console.info`
//! that route output to the tracing crate.

use crate::bindings::*;
use crate::error::JscResult;
use crate::value::js_string_to_rust;
use parking_lot::Mutex;
use std::ffi::CString;
use std::ptr;
use std::sync::{Arc, OnceLock};
use tracing::{debug, error, info, warn};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConsoleLevel {
    Log,
    Info,
    Debug,
    Warn,
    Error,
}

type ConsoleHandler = dyn Fn(ConsoleLevel, &str) + Send + Sync + 'static;

static CONSOLE_HANDLER: OnceLock<Mutex<Arc<ConsoleHandler>>> = OnceLock::new();

pub fn set_console_handler(handler: impl Fn(ConsoleLevel, &str) + Send + Sync + 'static) {
    let lock = CONSOLE_HANDLER.get_or_init(|| Mutex::new(Arc::new(default_console_handler)));
    *lock.lock() = Arc::new(handler);
}

fn default_console_handler(level: ConsoleLevel, message: &str) {
    match level {
        ConsoleLevel::Log | ConsoleLevel::Info => info!(target: "otter", "{}", message),
        ConsoleLevel::Debug => debug!(target: "otter", "{}", message),
        ConsoleLevel::Warn => warn!(target: "otter", "{}", message),
        ConsoleLevel::Error => error!(target: "otter", "{}", message),
    }
}

fn dispatch_console(level: ConsoleLevel, message: &str) {
    let lock = CONSOLE_HANDLER.get_or_init(|| Mutex::new(Arc::new(default_console_handler)));
    let handler = lock.lock().clone();
    handler(level, message);
}

/// Register the console API on the global object
pub fn register_console_api(ctx: JSContextRef) -> JscResult<()> {
    unsafe {
        let console_obj = JSObjectMake(ctx, ptr::null_mut(), ptr::null_mut());

        register_console_method(ctx, console_obj, "log", Some(js_console_log))?;
        register_console_method(ctx, console_obj, "info", Some(js_console_info))?;
        register_console_method(ctx, console_obj, "debug", Some(js_console_debug))?;
        register_console_method(ctx, console_obj, "warn", Some(js_console_warn))?;
        register_console_method(ctx, console_obj, "error", Some(js_console_error))?;

        let console_name = CString::new("console").unwrap();
        let console_name_ref = JSStringCreateWithUTF8CString(console_name.as_ptr());
        let global = JSContextGetGlobalObject(ctx);
        let mut exception: JSValueRef = ptr::null_mut();

        JSObjectSetProperty(
            ctx,
            global,
            console_name_ref,
            console_obj as JSValueRef,
            K_JS_PROPERTY_ATTRIBUTE_NONE,
            &mut exception,
        );

        JSStringRelease(console_name_ref);
    }

    Ok(())
}

unsafe fn register_console_method(
    ctx: JSContextRef,
    console_obj: JSObjectRef,
    name: &str,
    callback: JSObjectCallAsFunctionCallback,
) -> JscResult<()> {
    let name_cstr = CString::new(name).unwrap();
    let name_ref = JSStringCreateWithUTF8CString(name_cstr.as_ptr());

    let func = JSObjectMakeFunctionWithCallback(ctx, name_ref, callback);

    let mut exception: JSValueRef = ptr::null_mut();
    JSObjectSetProperty(
        ctx,
        console_obj,
        name_ref,
        func as JSValueRef,
        K_JS_PROPERTY_ATTRIBUTE_NONE,
        &mut exception,
    );

    JSStringRelease(name_ref);
    Ok(())
}

unsafe fn format_console_args(
    ctx: JSContextRef,
    argument_count: usize,
    arguments: *const JSValueRef,
) -> String {
    let mut parts = Vec::with_capacity(argument_count);

    for i in 0..argument_count {
        let value = *arguments.add(i);
        if value.is_null() {
            parts.push("null".to_string());
            continue;
        }

        let mut exception: JSValueRef = ptr::null_mut();

        if JSValueIsObject(ctx, value) && !JSValueIsNull(ctx, value) {
            let json_str = JSValueCreateJSONString(ctx, value, 0, &mut exception);
            if !json_str.is_null() {
                let s = js_string_to_rust(json_str);
                JSStringRelease(json_str);
                parts.push(s);
                continue;
            }
        }

        let js_str = JSValueToStringCopy(ctx, value, &mut exception);
        if !js_str.is_null() {
            let s = js_string_to_rust(js_str);
            JSStringRelease(js_str);
            parts.push(s);
        } else {
            parts.push("[object]".to_string());
        }
    }

    parts.join(" ")
}

unsafe extern "C" fn js_console_log(
    ctx: JSContextRef,
    _function: JSObjectRef,
    _this_object: JSObjectRef,
    argument_count: usize,
    arguments: *const JSValueRef,
    _exception: *mut JSValueRef,
) -> JSValueRef {
    let message = format_console_args(ctx, argument_count, arguments);
    dispatch_console(ConsoleLevel::Log, &message);
    JSValueMakeUndefined(ctx)
}

unsafe extern "C" fn js_console_info(
    ctx: JSContextRef,
    _function: JSObjectRef,
    _this_object: JSObjectRef,
    argument_count: usize,
    arguments: *const JSValueRef,
    _exception: *mut JSValueRef,
) -> JSValueRef {
    let message = format_console_args(ctx, argument_count, arguments);
    dispatch_console(ConsoleLevel::Info, &message);
    JSValueMakeUndefined(ctx)
}

unsafe extern "C" fn js_console_debug(
    ctx: JSContextRef,
    _function: JSObjectRef,
    _this_object: JSObjectRef,
    argument_count: usize,
    arguments: *const JSValueRef,
    _exception: *mut JSValueRef,
) -> JSValueRef {
    let message = format_console_args(ctx, argument_count, arguments);
    dispatch_console(ConsoleLevel::Debug, &message);
    JSValueMakeUndefined(ctx)
}

unsafe extern "C" fn js_console_warn(
    ctx: JSContextRef,
    _function: JSObjectRef,
    _this_object: JSObjectRef,
    argument_count: usize,
    arguments: *const JSValueRef,
    _exception: *mut JSValueRef,
) -> JSValueRef {
    let message = format_console_args(ctx, argument_count, arguments);
    dispatch_console(ConsoleLevel::Warn, &message);
    JSValueMakeUndefined(ctx)
}

unsafe extern "C" fn js_console_error(
    ctx: JSContextRef,
    _function: JSObjectRef,
    _this_object: JSObjectRef,
    argument_count: usize,
    arguments: *const JSValueRef,
    _exception: *mut JSValueRef,
) -> JSValueRef {
    let message = format_console_args(ctx, argument_count, arguments);
    dispatch_console(ConsoleLevel::Error, &message);
    JSValueMakeUndefined(ctx)
}
