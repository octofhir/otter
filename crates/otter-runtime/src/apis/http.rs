//! HTTP API implementation (fetch)
//!
//! Provides the Web standard `fetch(url, options)` for making HTTP requests from scripts.
//! Uses reqwest under the hood with async calls.
//!
//! # Security
//!
//! Network access is controlled by a permission checker that can be configured via
//! `set_net_permission_checker`. By default, all network access is denied unless
//! a checker is set and approves the request.
//!
//! # Example
//!
//! ```typescript
//! // Requires: --allow-net=api.example.com
//! const response = await fetch("https://api.example.com/data");
//! const json = await response.json();
//! ```

use crate::apis::{get_arg_as_json, get_arg_as_string, make_exception};
use crate::bindings::*;
use crate::error::{JscError, JscResult};
use crate::extension::schedule_promise;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::ffi::CString;
use std::ptr;
use std::str::FromStr;
use std::time::Duration;
use tracing::{debug, warn};
use url::Url;

/// Type for network permission checker function.
///
/// Takes a host string and returns true if access is allowed.
pub type NetPermissionChecker = Box<dyn Fn(&str) -> bool + Send + Sync>;

use std::sync::OnceLock;

// Global (process-wide) storage for network permission checker.
// This allows the permission checker to be set once and used by all worker threads.
static NET_PERMISSION_CHECKER: OnceLock<NetPermissionChecker> = OnceLock::new();

/// Set the network permission checker globally (process-wide).
///
/// This should be called once before starting the engine. The checker function
/// takes a host string and returns true if access is allowed. Once set, the
/// checker applies to all worker threads.
///
/// # Example
///
/// ```ignore
/// use otter_runtime::set_net_permission_checker;
///
/// // Allow all network access
/// set_net_permission_checker(Box::new(|_host| true));
///
/// // Allow only specific hosts
/// set_net_permission_checker(Box::new(|host| {
///     host == "api.example.com" || host.ends_with(".example.com")
/// }));
/// ```
///
/// # Note
///
/// This function can only be called once. Subsequent calls will be ignored.
pub fn set_net_permission_checker(checker: NetPermissionChecker) {
    let _ = NET_PERMISSION_CHECKER.set(checker);
}

/// Clear the network permission checker.
///
/// Note: This is a no-op since OnceLock cannot be reset. The checker persists
/// for the lifetime of the process.
pub fn clear_net_permission_checker() {
    // No-op: OnceLock cannot be reset
}

/// Check if network access is allowed for a URL.
fn check_net_permission(url: &str) -> Result<(), JscError> {
    // Parse URL to extract host
    let parsed = Url::parse(url).map_err(|e| JscError::HttpError(format!("Invalid URL: {}", e)))?;

    let host = parsed
        .host_str()
        .ok_or_else(|| JscError::HttpError("URL has no host".to_string()))?;

    // Check with the permission checker
    let allowed = match NET_PERMISSION_CHECKER.get() {
        Some(checker) => checker(host),
        None => {
            // No checker set - deny by default (secure default)
            false
        }
    };

    if !allowed {
        return Err(JscError::PermissionDenied(format!(
            "Network access denied for '{}'. Use --allow-net={} to grant access.",
            url, host
        )));
    }

    Ok(())
}

const FETCH_SHIM: &str = include_str!("fetch_shim.js");

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default)]
struct FetchOptions {
    method: Option<String>,
    headers: Option<HashMap<String, String>>,
    body: Option<serde_json::Value>,
    timeout: Option<u64>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct FetchResponse {
    status: u16,
    status_text: String,
    headers: HashMap<String, String>,
    body_text: String,
    url: String,
}

/// Register the http API on the global object
pub fn register_http_api(ctx: JSContextRef) -> JscResult<()> {
    unsafe {
        let http_obj = JSObjectMake(ctx, ptr::null_mut(), ptr::null_mut());

        let fetch_name = CString::new("__otter_fetch_raw").unwrap();
        let fetch_name_ref = JSStringCreateWithUTF8CString(fetch_name.as_ptr());
        let fetch_func = JSObjectMakeFunctionWithCallback(ctx, fetch_name_ref, Some(js_http_fetch));

        let mut exception: JSValueRef = ptr::null_mut();
        JSObjectSetProperty(
            ctx,
            http_obj,
            fetch_name_ref,
            fetch_func as JSValueRef,
            K_JS_PROPERTY_ATTRIBUTE_NONE,
            &mut exception,
        );
        JSStringRelease(fetch_name_ref);

        let http_name = CString::new("http").unwrap();
        let http_name_ref = JSStringCreateWithUTF8CString(http_name.as_ptr());
        let global = JSContextGetGlobalObject(ctx);

        JSObjectSetProperty(
            ctx,
            global,
            http_name_ref,
            http_obj as JSValueRef,
            K_JS_PROPERTY_ATTRIBUTE_NONE,
            &mut exception,
        );
        JSStringRelease(http_name_ref);

        let fetch_name = CString::new("__otter_fetch_raw").unwrap();
        let fetch_name_ref = JSStringCreateWithUTF8CString(fetch_name.as_ptr());
        JSObjectSetProperty(
            ctx,
            global,
            fetch_name_ref,
            fetch_func as JSValueRef,
            K_JS_PROPERTY_ATTRIBUTE_NONE,
            &mut exception,
        );
        JSStringRelease(fetch_name_ref);

        let shim_cstr = CString::new(FETCH_SHIM).unwrap();
        let shim_ref = JSStringCreateWithUTF8CString(shim_cstr.as_ptr());
        let source_cstr = CString::new("<otter_fetch_shim>").unwrap();
        let source_ref = JSStringCreateWithUTF8CString(source_cstr.as_ptr());
        let mut shim_exception: JSValueRef = ptr::null_mut();
        JSEvaluateScript(
            ctx,
            shim_ref,
            ptr::null_mut(),
            source_ref,
            1,
            &mut shim_exception,
        );
        JSStringRelease(shim_ref);
        JSStringRelease(source_ref);

        if !shim_exception.is_null() {
            return Err(crate::value::extract_exception(ctx, shim_exception).into());
        }
    }

    Ok(())
}

unsafe extern "C" fn js_http_fetch(
    ctx: JSContextRef,
    _function: JSObjectRef,
    _this_object: JSObjectRef,
    argument_count: usize,
    arguments: *const JSValueRef,
    exception: *mut JSValueRef,
) -> JSValueRef {
    let url = match get_arg_as_string(ctx, arguments, 0, argument_count) {
        Some(u) => u,
        None => {
            *exception = make_exception(ctx, "http.fetch requires a URL as first argument");
            return JSValueMakeUndefined(ctx);
        }
    };

    let options: FetchOptions = get_arg_as_json(ctx, arguments, 1, argument_count)
        .and_then(|v| serde_json::from_value(v).ok())
        .unwrap_or_default();

    debug!(
        url = %url,
        method = ?options.method,
        "http.fetch called"
    );

    let url_for_future = url.clone();
    let future = async move { execute_fetch(&url_for_future, options).await };
    match schedule_promise(ctx, future) {
        Ok(promise) => promise,
        Err(err) => {
            warn!(error = %err, url = %url, "http.fetch failed");
            *exception = make_exception(ctx, &err.to_string());
            JSValueMakeUndefined(ctx)
        }
    }
}

async fn execute_fetch(url: &str, options: FetchOptions) -> JscResult<serde_json::Value> {
    // Check network permissions before making the request
    check_net_permission(url)?;

    let client = reqwest::Client::builder()
        .timeout(Duration::from_millis(options.timeout.unwrap_or(30000)))
        .build()
        .map_err(|e| JscError::HttpError(format!("Failed to create HTTP client: {}", e)))?;

    let method = options.method.as_deref().unwrap_or("GET").to_uppercase();

    let mut request = match method.as_str() {
        "GET" => client.get(url),
        "POST" => client.post(url),
        "PUT" => client.put(url),
        "DELETE" => client.delete(url),
        "PATCH" => client.patch(url),
        "HEAD" => client.head(url),
        _ => {
            return Err(JscError::HttpError(format!(
                "Unsupported HTTP method: {}",
                method
            )));
        }
    };

    if let Some(headers) = options.headers {
        let mut header_map = HeaderMap::new();
        for (key, value) in headers {
            if let (Ok(name), Ok(val)) = (HeaderName::from_str(&key), HeaderValue::from_str(&value))
            {
                header_map.insert(name, val);
            }
        }
        request = request.headers(header_map);
    }

    if let Some(body) = options.body {
        match body {
            serde_json::Value::String(s) => {
                request = request.body(s);
            }
            _ => {
                let json_str = serde_json::to_string(&body)
                    .map_err(|e| JscError::HttpError(format!("Failed to serialize body: {}", e)))?;
                request = request
                    .header("Content-Type", "application/json")
                    .body(json_str);
            }
        }
    }

    let response = request
        .send()
        .await
        .map_err(|e| JscError::HttpError(format!("HTTP request failed: {}", e)))?;

    let status = response.status();
    let status_code = status.as_u16();
    let status_text = status.canonical_reason().unwrap_or("Unknown").to_string();
    let url = response.url().to_string();

    let mut headers = HashMap::new();
    for (name, value) in response.headers() {
        if let Ok(v) = value.to_str() {
            headers.insert(name.to_string(), v.to_string());
        }
    }

    let body_text = response
        .text()
        .await
        .map_err(|e| JscError::HttpError(format!("Failed to read response body: {}", e)))?;

    let response = FetchResponse {
        status: status_code,
        status_text,
        headers,
        body_text,
        url,
    };

    serde_json::to_value(&response)
        .map_err(|e| JscError::HttpError(format!("Failed to serialize response: {}", e)))
}
