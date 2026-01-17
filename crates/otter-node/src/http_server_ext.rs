//! HTTP server extension module using the new architecture.
//!
//! This module provides the HTTP server extension for Otter.serve().
//!
//! ## Architecture
//!
//! - `http_server.rs` - Rust HTTP server implementation
//! - `http_request.rs` - Request/response handling
//! - `http_server_ext.rs` - Extension creation with ops
//! - `serve_shim.js` - JavaScript wrapper
//!
//! Note: This module uses shared state (HttpServerManager) and event channels,
//! which doesn't fit the #[dive] pattern.

use otter_runtime::extension::{op_async, op_sync};
use otter_runtime::Extension;
use serde_json::json;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::mpsc::UnboundedSender;

use crate::http_request;
use crate::http_server::{self, ActiveServerCount, HttpEvent};

/// Create the http_server extension.
///
/// This extension provides HTTP server functionality for Otter.serve().
///
/// Returns a tuple of (Extension, ActiveServerCount) so the runtime can track
/// when all servers have stopped.
pub fn extension(
    event_tx: UnboundedSender<HttpEvent>,
) -> (Extension, ActiveServerCount) {
    let manager = Arc::new(http_server::HttpServerManager::new());
    let active_count = manager.active_count();

    let manager_create = manager.clone();
    let manager_info = manager.clone();
    let manager_stop = manager.clone();

    let event_tx_create = event_tx.clone();

    let extension = Extension::new("http_server")
        .with_ops(vec![
            // Create a new HTTP server
            op_async("__otter_http_server_create", move |_ctx, args| {
                let mgr = manager_create.clone();
                let tx = event_tx_create.clone();

                async move {
                    let port = args
                        .first()
                        .and_then(|v| v.as_u64())
                        .unwrap_or(3000) as u16;

                    let hostname = args
                        .get(1)
                        .and_then(|v| v.as_str())
                        .unwrap_or("0.0.0.0");

                    // Parse TLS config if provided
                    let tls = if let Some(tls_obj) = args.get(2).and_then(|v| v.as_object()) {
                        let cert = tls_obj.get("cert").and_then(|v| {
                            if let Some(s) = v.as_str() {
                                Some(s.as_bytes().to_vec())
                            } else if let Some(arr) = v.as_array() {
                                Some(
                                    arr.iter()
                                        .filter_map(|v| v.as_u64().map(|n| n as u8))
                                        .collect(),
                                )
                            } else {
                                None
                            }
                        });

                        let key = tls_obj.get("key").and_then(|v| {
                            if let Some(s) = v.as_str() {
                                Some(s.as_bytes().to_vec())
                            } else if let Some(arr) = v.as_array() {
                                Some(
                                    arr.iter()
                                        .filter_map(|v| v.as_u64().map(|n| n as u8))
                                        .collect(),
                                )
                            } else {
                                None
                            }
                        });

                        if let (Some(cert), Some(key)) = (cert, key) {
                            match http_server::TlsConfig::from_pem(&cert, &key) {
                                Ok(config) => Some(config),
                                Err(e) => {
                                    return Ok(json!({ "error": e.to_string() }));
                                }
                            }
                        } else {
                            None
                        }
                    } else {
                        None
                    };

                    match mgr.create(port, hostname, tls, tx).await {
                        Ok(id) => {
                            let info = mgr.info(id).unwrap();
                            Ok(json!({
                                "id": id,
                                "port": info.port,
                                "hostname": info.hostname,
                                "tls": info.is_tls
                            }))
                        }
                        Err(e) => Ok(json!({ "error": e.to_string() })),
                    }
                }
            }),
            // Get request method
            op_sync("__otter_http_req_method", |_ctx, args| {
                let request_id = args.first().and_then(|v| v.as_u64()).unwrap_or(0);
                let method = http_request::get_request_method(request_id).unwrap_or_default();
                Ok(json!(method))
            }),
            // Get request URL
            op_sync("__otter_http_req_url", |_ctx, args| {
                let request_id = args.first().and_then(|v| v.as_u64()).unwrap_or(0);
                let url = http_request::get_request_url(request_id).unwrap_or_default();
                Ok(json!(url))
            }),
            // Get all request headers
            op_sync("__otter_http_req_headers", |_ctx, args| {
                let request_id = args.first().and_then(|v| v.as_u64()).unwrap_or(0);
                let headers = http_request::get_request_headers(request_id).unwrap_or_default();
                Ok(json!(headers))
            }),
            // Get all request metadata in single lock (batch optimization)
            op_sync("__otter_http_req_metadata", |_ctx, args| {
                let request_id = args.first().and_then(|v| v.as_u64()).unwrap_or(0);
                match http_request::get_request_metadata(request_id) {
                    Some(meta) => Ok(serde_json::to_value(meta).unwrap()),
                    None => Ok(json!(null)),
                }
            }),
            // Get basic metadata (method + url only) - for lazy headers optimization
            op_sync("__otter_http_req_basic", |_ctx, args| {
                let request_id = args.first().and_then(|v| v.as_u64()).unwrap_or(0);
                match http_request::get_basic_metadata(request_id) {
                    Some(meta) => Ok(serde_json::to_value(meta).unwrap()),
                    None => Ok(json!(null)),
                }
            }),
            // Read request body (async)
            op_async("__otter_http_req_body", |_ctx, args| async move {
                let request_id = args.first().and_then(|v| v.as_u64()).unwrap_or(0);
                match http_request::read_request_body(request_id).await {
                    Some(body) => Ok(json!({ "data": body })),
                    None => Ok(json!({ "data": [] })),
                }
            }),
            // Send response
            op_sync("__otter_http_respond", |_ctx, args| {
                let request_id = args.first().and_then(|v| v.as_u64()).unwrap_or(0);
                let status = args.get(1).and_then(|v| v.as_u64()).unwrap_or(200) as u16;

                let headers: HashMap<String, String> = args
                    .get(2)
                    .and_then(|v| v.as_object())
                    .map(|obj| {
                        obj.iter()
                            .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                            .collect()
                    })
                    .unwrap_or_default();

                let body: Vec<u8> = args
                    .get(3)
                    .map(|v| {
                        if let Some(s) = v.as_str() {
                            // Plain string body
                            s.as_bytes().to_vec()
                        } else if let Some(obj) = v.as_object() {
                            // Check for base64-encoded body (optimized path)
                            match obj.get("type").and_then(|t| t.as_str()) {
                                Some("base64") => {
                                    use base64::Engine;
                                    obj.get("data")
                                        .and_then(|d| d.as_str())
                                        .and_then(|s| {
                                            base64::engine::general_purpose::STANDARD
                                                .decode(s)
                                                .ok()
                                        })
                                        .unwrap_or_default()
                                }
                                _ => {
                                    // Legacy: Handle { data: [...] } format from Uint8Array
                                    obj.get("data")
                                        .and_then(|d| d.as_array())
                                        .map(|arr| {
                                            arr.iter()
                                                .filter_map(|v| v.as_u64().map(|n| n as u8))
                                                .collect()
                                        })
                                        .unwrap_or_default()
                                }
                            }
                        } else if let Some(arr) = v.as_array() {
                            // Legacy: array of numbers
                            arr.iter()
                                .filter_map(|v| v.as_u64().map(|n| n as u8))
                                .collect()
                        } else {
                            Vec::new()
                        }
                    })
                    .unwrap_or_default();

                let success = http_request::send_response(request_id, status, headers, body);
                Ok(json!({ "success": success }))
            }),
            // Send text response (fast path - avoids body serialization)
            op_sync("__otter_http_respond_text", |_ctx, args| {
                let request_id = args.first().and_then(|v| v.as_u64()).unwrap_or(0);
                let status = args.get(1).and_then(|v| v.as_u64()).unwrap_or(200) as u16;
                let body = args.get(2).and_then(|v| v.as_str()).unwrap_or("");

                let success = http_request::send_text_response(request_id, status, body);
                Ok(json!({ "success": success }))
            }),
            // Get server info
            op_sync("__otter_http_server_info", move |_ctx, args| {
                let server_id = args.first().and_then(|v| v.as_u64()).unwrap_or(0);
                match manager_info.info(server_id) {
                    Ok(info) => Ok(json!({
                        "port": info.port,
                        "hostname": info.hostname,
                        "tls": info.is_tls
                    })),
                    Err(e) => Ok(json!({ "error": e.to_string() })),
                }
            }),
            // Stop server
            op_sync("__otter_http_server_stop", move |_ctx, args| {
                let server_id = args.first().and_then(|v| v.as_u64()).unwrap_or(0);
                match manager_stop.stop(server_id) {
                    Ok(()) => Ok(json!({ "success": true })),
                    Err(e) => Ok(json!({ "error": e.to_string() })),
                }
            }),
        ])
        .with_js(include_str!("serve_shim.js"));

    (extension, active_count)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extension_creation() {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let (ext, _count) = extension(tx);
        assert_eq!(ext.name(), "http_server");
        assert!(ext.js_code().is_some());
    }
}
