//! Base64 encoding/decoding APIs (atob, btoa)
//!
//! Web standard functions for base64 encoding and decoding.

use crate::bindings::*;
use crate::error::JscResult;
use crate::value::js_string_to_rust;
use std::ffi::CString;
use std::ptr;

const BASE64_ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// Encode bytes to base64
fn base64_encode(data: &[u8]) -> String {
    let mut result = String::with_capacity((data.len() + 2) / 3 * 4);

    for chunk in data.chunks(3) {
        let mut n = 0u32;
        for (i, &byte) in chunk.iter().enumerate() {
            n |= (byte as u32) << (16 - i * 8);
        }

        let chars = match chunk.len() {
            3 => 4,
            2 => 3,
            1 => 2,
            _ => unreachable!(),
        };

        for i in 0..chars {
            let idx = ((n >> (18 - i * 6)) & 0x3f) as usize;
            result.push(BASE64_ALPHABET[idx] as char);
        }

        for _ in chars..4 {
            result.push('=');
        }
    }

    result
}

/// Decode base64 to bytes
fn base64_decode(input: &str) -> Result<Vec<u8>, String> {
    let input = input.trim_end_matches('=');
    let mut result = Vec::with_capacity(input.len() * 3 / 4);

    let mut buffer = 0u32;
    let mut bits = 0;

    for c in input.chars() {
        let value = match c {
            'A'..='Z' => c as u32 - 'A' as u32,
            'a'..='z' => c as u32 - 'a' as u32 + 26,
            '0'..='9' => c as u32 - '0' as u32 + 52,
            '+' => 62,
            '/' => 63,
            ' ' | '\t' | '\n' | '\r' => continue, // Skip whitespace
            _ => return Err(format!("Invalid base64 character: {}", c)),
        };

        buffer = (buffer << 6) | value;
        bits += 6;

        if bits >= 8 {
            bits -= 8;
            result.push((buffer >> bits) as u8);
            buffer &= (1 << bits) - 1;
        }
    }

    Ok(result)
}

/// btoa - encode a string to base64
///
/// # Safety
/// JSC FFI requirements
unsafe extern "C" fn btoa_callback(
    ctx: JSContextRef,
    _function: JSObjectRef,
    _this_object: JSObjectRef,
    argument_count: usize,
    arguments: *const JSValueRef,
    exception: *mut JSValueRef,
) -> JSValueRef {
    if argument_count == 0 {
        let err_msg = CString::new("btoa requires 1 argument").unwrap();
        let err_str = JSStringCreateWithUTF8CString(err_msg.as_ptr());
        *exception = JSValueMakeString(ctx, err_str);
        JSStringRelease(err_str);
        return JSValueMakeUndefined(ctx);
    }

    let arg = *arguments;
    let mut exc: JSValueRef = ptr::null_mut();
    let js_str = JSValueToStringCopy(ctx, arg, &mut exc);

    if js_str.is_null() {
        *exception = exc;
        return JSValueMakeUndefined(ctx);
    }

    let input = js_string_to_rust(js_str);
    JSStringRelease(js_str);

    // Check for non-Latin1 characters
    for c in input.chars() {
        if c as u32 > 255 {
            let err_msg =
                CString::new("btoa: The string contains characters outside of the Latin1 range")
                    .unwrap();
            let err_str = JSStringCreateWithUTF8CString(err_msg.as_ptr());
            *exception = JSValueMakeString(ctx, err_str);
            JSStringRelease(err_str);
            return JSValueMakeUndefined(ctx);
        }
    }

    let bytes: Vec<u8> = input.chars().map(|c| c as u8).collect();
    let encoded = base64_encode(&bytes);

    let result_cstr = CString::new(encoded).unwrap();
    let result_str = JSStringCreateWithUTF8CString(result_cstr.as_ptr());
    let result = JSValueMakeString(ctx, result_str);
    JSStringRelease(result_str);

    result
}

/// atob - decode a base64 string
///
/// # Safety
/// JSC FFI requirements
unsafe extern "C" fn atob_callback(
    ctx: JSContextRef,
    _function: JSObjectRef,
    _this_object: JSObjectRef,
    argument_count: usize,
    arguments: *const JSValueRef,
    exception: *mut JSValueRef,
) -> JSValueRef {
    if argument_count == 0 {
        let err_msg = CString::new("atob requires 1 argument").unwrap();
        let err_str = JSStringCreateWithUTF8CString(err_msg.as_ptr());
        *exception = JSValueMakeString(ctx, err_str);
        JSStringRelease(err_str);
        return JSValueMakeUndefined(ctx);
    }

    let arg = *arguments;
    let mut exc: JSValueRef = ptr::null_mut();
    let js_str = JSValueToStringCopy(ctx, arg, &mut exc);

    if js_str.is_null() {
        *exception = exc;
        return JSValueMakeUndefined(ctx);
    }

    let input = js_string_to_rust(js_str);
    JSStringRelease(js_str);

    match base64_decode(&input) {
        Ok(bytes) => {
            // Convert bytes to string (Latin1 encoding)
            let decoded: String = bytes.iter().map(|&b| b as char).collect();
            let result_cstr = CString::new(decoded).unwrap();
            let result_str = JSStringCreateWithUTF8CString(result_cstr.as_ptr());
            let result = JSValueMakeString(ctx, result_str);
            JSStringRelease(result_str);
            result
        }
        Err(e) => {
            let err_msg = CString::new(format!("atob: {}", e)).unwrap();
            let err_str = JSStringCreateWithUTF8CString(err_msg.as_ptr());
            *exception = JSValueMakeString(ctx, err_str);
            JSStringRelease(err_str);
            JSValueMakeUndefined(ctx)
        }
    }
}

/// Register atob and btoa on globalThis
pub fn register_encoding_api(ctx: JSContextRef) -> JscResult<()> {
    unsafe {
        let global = JSContextGetGlobalObject(ctx);

        // Register btoa
        let btoa_name = CString::new("btoa").unwrap();
        let btoa_name_ref = JSStringCreateWithUTF8CString(btoa_name.as_ptr());
        let btoa_fn = JSObjectMakeFunctionWithCallback(ctx, btoa_name_ref, Some(btoa_callback));
        JSObjectSetProperty(
            ctx,
            global,
            btoa_name_ref,
            btoa_fn,
            K_JS_PROPERTY_ATTRIBUTE_NONE,
            ptr::null_mut(),
        );
        JSStringRelease(btoa_name_ref);

        // Register atob
        let atob_name = CString::new("atob").unwrap();
        let atob_name_ref = JSStringCreateWithUTF8CString(atob_name.as_ptr());
        let atob_fn = JSObjectMakeFunctionWithCallback(ctx, atob_name_ref, Some(atob_callback));
        JSObjectSetProperty(
            ctx,
            global,
            atob_name_ref,
            atob_fn,
            K_JS_PROPERTY_ATTRIBUTE_NONE,
            ptr::null_mut(),
        );
        JSStringRelease(atob_name_ref);
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_base64_encode() {
        assert_eq!(base64_encode(b"hello"), "aGVsbG8=");
        assert_eq!(base64_encode(b"Hello, World!"), "SGVsbG8sIFdvcmxkIQ==");
        assert_eq!(base64_encode(b""), "");
        assert_eq!(base64_encode(b"a"), "YQ==");
        assert_eq!(base64_encode(b"ab"), "YWI=");
        assert_eq!(base64_encode(b"abc"), "YWJj");
    }

    #[test]
    fn test_base64_decode() {
        assert_eq!(base64_decode("aGVsbG8=").unwrap(), b"hello");
        assert_eq!(
            base64_decode("SGVsbG8sIFdvcmxkIQ==").unwrap(),
            b"Hello, World!"
        );
        assert_eq!(base64_decode("").unwrap(), b"");
        assert_eq!(base64_decode("YQ==").unwrap(), b"a");
        assert_eq!(base64_decode("YWI=").unwrap(), b"ab");
        assert_eq!(base64_decode("YWJj").unwrap(), b"abc");
    }
}
