//! Fetch API implementation
//!
//! Provides the `fetch()` function for making HTTP requests.
//! Uses reqwest with rustls for TLS support.
//!
//! # Security
//!
//! Network access is controlled by capabilities. The runtime must set
//! capabilities before executing fetch ops using `CapabilitiesGuard`.
//! If capabilities are not set or network access is denied, fetch will fail.

use bytes::Bytes;
use otter_vm_runtime::{Op, op_async};
use reqwest::{Client, Method, header::HeaderMap};
use serde_json::{Value as JsonValue, json};
use std::sync::OnceLock;
use std::time::Duration;

use otter_vm_runtime::capabilities_context;

/// Global HTTP client - reused for connection pooling
static HTTP_CLIENT: OnceLock<Client> = OnceLock::new();

/// Get or create the global HTTP client
fn get_client() -> &'static Client {
    HTTP_CLIENT.get_or_init(|| {
        Client::builder()
            .pool_max_idle_per_host(32)
            .pool_idle_timeout(Duration::from_secs(90))
            .connect_timeout(Duration::from_secs(30))
            .timeout(Duration::from_secs(300))
            .build()
            .expect("Failed to create HTTP client")
    })
}

/// Convert HeaderMap to JSON object
fn headers_to_json(headers: &HeaderMap) -> JsonValue {
    let mut obj = serde_json::Map::new();
    for (name, value) in headers.iter() {
        if let Ok(v) = value.to_str() {
            obj.insert(name.as_str().to_string(), JsonValue::String(v.to_string()));
        }
    }
    JsonValue::Object(obj)
}

/// Parse method string to reqwest Method
fn parse_method(method: &str) -> Method {
    match method.to_uppercase().as_str() {
        "GET" => Method::GET,
        "POST" => Method::POST,
        "PUT" => Method::PUT,
        "DELETE" => Method::DELETE,
        "PATCH" => Method::PATCH,
        "HEAD" => Method::HEAD,
        "OPTIONS" => Method::OPTIONS,
        "CONNECT" => Method::CONNECT,
        "TRACE" => Method::TRACE,
        _ => Method::GET,
    }
}

/// Extract host from URL for permission checking
fn extract_host(url: &str) -> Option<String> {
    // Try parsing as full URL first
    if let Ok(parsed) = url::Url::parse(url) {
        return parsed.host_str().map(|s| s.to_string());
    }
    // URL might be relative or malformed
    None
}

/// Create fetch operations
pub fn ops() -> Vec<Op> {
    vec![
        // __fetch(url, method, headers, body) -> { status, statusText, headers, body }
        op_async("__fetch", |args| {
            // Parse arguments
            let url = args
                .first()
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();

            let method = args
                .get(1)
                .and_then(|v| v.as_str())
                .unwrap_or("GET")
                .to_string();

            let headers: Option<JsonValue> = args.get(2).cloned();

            let body: Option<Bytes> = args.get(3).and_then(|v| {
                if v.is_null() {
                    None
                } else if let Some(s) = v.as_str() {
                    Some(Bytes::from(s.to_string()))
                } else if let Some(arr) = v.as_array() {
                    // Convert array of numbers to bytes
                    let bytes: Vec<u8> = arr
                        .iter()
                        .filter_map(|n| n.as_u64().map(|n| n as u8))
                        .collect();
                    Some(Bytes::from(bytes))
                } else {
                    None
                }
            });

            // Check network permission BEFORE the async block
            // This captures the current thread's capabilities state
            let host = extract_host(&url);
            let permission_denied = host
                .as_ref()
                .is_some_and(|h| !capabilities_context::can_net(h));
            let denied_host = if permission_denied {
                host.clone()
            } else {
                None
            };

            async move {
                // Check permission (captured before async)
                if let Some(denied_host) = denied_host {
                    return Err(format!(
                        "PermissionDenied: Network access denied for host '{}'. Use --allow-net to grant access.",
                        denied_host
                    ));
                }

                let client = get_client();
                let method = parse_method(&method);

                let mut request = client.request(method, &url);

                // Add headers
                if let Some(headers_obj) = headers
                    && let Some(obj) = headers_obj.as_object()
                {
                    for (key, value) in obj {
                        if let Some(v) = value.as_str() {
                            request = request.header(key.as_str(), v);
                        }
                    }
                }

                // Add body
                if let Some(body_bytes) = body {
                    request = request.body(body_bytes);
                }

                // Send request
                let response = request.send().await.map_err(|e| e.to_string())?;

                let status = response.status().as_u16();
                let status_text = response
                    .status()
                    .canonical_reason()
                    .unwrap_or("")
                    .to_string();
                let response_headers = headers_to_json(response.headers());

                // Read body
                let body_bytes = response.bytes().await.map_err(|e| e.to_string())?;
                let body_array: Vec<JsonValue> = body_bytes
                    .iter()
                    .map(|&b| JsonValue::Number(b.into()))
                    .collect();

                Ok(json!({
                    "status": status,
                    "statusText": status_text,
                    "headers": response_headers,
                    "body": body_array
                }))
            }
        }),
    ]
}

/// JavaScript shim code for fetch API
pub const JS_SHIM: &str = include_str!("fetch.js");

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_method() {
        assert_eq!(parse_method("get"), Method::GET);
        assert_eq!(parse_method("POST"), Method::POST);
        assert_eq!(parse_method("Delete"), Method::DELETE);
    }

    #[test]
    fn test_headers_to_json() {
        let mut headers = HeaderMap::new();
        headers.insert("content-type", "application/json".parse().unwrap());
        headers.insert("x-custom", "value".parse().unwrap());

        let json = headers_to_json(&headers);
        assert_eq!(json["content-type"], "application/json");
        assert_eq!(json["x-custom"], "value");
    }
}
