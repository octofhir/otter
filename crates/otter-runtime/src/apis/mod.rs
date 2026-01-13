//! Native API implementations for JavaScript execution.
//!
//! This module provides Rust implementations of APIs that are exposed to JavaScript:
//! - `console.*` - Logging (routed to tracing)
//! - `http.fetch` - HTTP client (via reqwest)

pub mod console;
pub mod http;
pub mod timers;

use crate::bindings::*;
use crate::error::JscResult;
use crate::value::js_string_to_rust;
use std::ffi::CString;
use std::ptr;

/// Helper to get argument as string
pub unsafe fn get_arg_as_string(
    ctx: JSContextRef,
    arguments: *const JSValueRef,
    index: usize,
    argument_count: usize,
) -> Option<String> {
    if index >= argument_count {
        return None;
    }

    let value = *arguments.add(index);
    if value.is_null() || JSValueIsUndefined(ctx, value) {
        return None;
    }

    let mut exception: JSValueRef = ptr::null_mut();
    let js_str = JSValueToStringCopy(ctx, value, &mut exception);
    if js_str.is_null() {
        return None;
    }

    let result = js_string_to_rust(js_str);
    JSStringRelease(js_str);
    Some(result)
}

/// Helper to get argument as JSON object
pub unsafe fn get_arg_as_json(
    ctx: JSContextRef,
    arguments: *const JSValueRef,
    index: usize,
    argument_count: usize,
) -> Option<serde_json::Value> {
    if index >= argument_count {
        return None;
    }

    let value = *arguments.add(index);
    if value.is_null() || JSValueIsUndefined(ctx, value) {
        return None;
    }

    let mut exception: JSValueRef = ptr::null_mut();
    let js_str = JSValueCreateJSONString(ctx, value, 0, &mut exception);
    if js_str.is_null() {
        return None;
    }

    let json_str = js_string_to_rust(js_str);
    JSStringRelease(js_str);

    serde_json::from_str(&json_str).ok()
}

/// Helper to create a JS exception from an error message
pub unsafe fn make_exception(ctx: JSContextRef, message: &str) -> JSValueRef {
    let script = format!("new Error({})", serde_json::to_string(message).unwrap());
    let script_cstr = CString::new(script).unwrap();
    let script_ref = JSStringCreateWithUTF8CString(script_cstr.as_ptr());
    let source_cstr = CString::new("<error>").unwrap();
    let source_ref = JSStringCreateWithUTF8CString(source_cstr.as_ptr());
    let mut exc: JSValueRef = ptr::null_mut();

    let result = JSEvaluateScript(ctx, script_ref, ptr::null_mut(), source_ref, 1, &mut exc);

    JSStringRelease(script_ref);
    JSStringRelease(source_ref);

    if result.is_null() {
        let msg_cstr =
            CString::new(message).unwrap_or_else(|_| CString::new("Unknown error").unwrap());
        let msg_ref = JSStringCreateWithUTF8CString(msg_cstr.as_ptr());
        let str_value = JSValueMakeString(ctx, msg_ref);
        JSStringRelease(msg_ref);
        str_value
    } else {
        result
    }
}

/// Helper to create a JS value from JSON
pub unsafe fn json_to_js_value(ctx: JSContextRef, json: &serde_json::Value) -> JSValueRef {
    let json_str = serde_json::to_string(json).unwrap_or_else(|_| "null".to_string());
    let json_cstr = CString::new(json_str).unwrap();
    let json_ref = JSStringCreateWithUTF8CString(json_cstr.as_ptr());
    let value = JSValueMakeFromJSONString(ctx, json_ref);
    JSStringRelease(json_ref);

    if value.is_null() {
        JSValueMakeNull(ctx)
    } else {
        value
    }
}

/// Register all native APIs on a context
pub fn register_all_apis(ctx: JSContextRef) -> JscResult<()> {
    console::register_console_api(ctx)?;
    http::register_http_api(ctx)?;
    timers::register_timers_api(ctx)?;
    Ok(())
}
