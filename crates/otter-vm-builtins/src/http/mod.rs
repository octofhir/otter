//! HTTP server module for Otter.serve() API
//!
//! Provides Bun-compatible HTTP server functionality with high performance.
//!
//! # Security
//!
//! HTTP server requires network permission. The runtime must set capabilities
//! before creating servers using `CapabilitiesGuard`.

mod request;
mod server;
mod service;

use otter_vm_runtime::{ActiveServerCount, HttpEvent, Op, WsEvent, op_async, op_sync};
use serde_json::{Value as JsonValue, json};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;

use otter_vm_runtime::capabilities_context;

pub use request::{HttpRequest, get_request, insert_request, remove_request};
pub use server::{HttpServerManager, ServerOptions, TlsConfig, WebSocketServerConfig};

/// Create HTTP server operations
///
/// Returns ops and the channel sender for HTTP events.
/// The event loop should receive from this channel and call the dispatcher.
pub fn ops(
    event_tx: mpsc::UnboundedSender<HttpEvent>,
    ws_event_tx: mpsc::UnboundedSender<WsEvent>,
    active_count: ActiveServerCount,
) -> Vec<Op> {
    let manager = Arc::new(HttpServerManager::new(
        event_tx.clone(),
        ws_event_tx.clone(),
        active_count,
    ));
    let manager_create = Arc::clone(&manager);
    let manager_stop = Arc::clone(&manager);
    let manager_info = Arc::clone(&manager);
    let manager_pending = Arc::clone(&manager);
    let manager_ws_upgrade = Arc::clone(&manager);
    let manager_ws_send = Arc::clone(&manager);
    let manager_ws_close = Arc::clone(&manager);
    let manager_ws_terminate = Arc::clone(&manager);
    let manager_ws_ping = Arc::clone(&manager);
    let manager_ws_pong = Arc::clone(&manager);
    let manager_ws_subscribe = Arc::clone(&manager);
    let manager_ws_unsubscribe = Arc::clone(&manager);
    let manager_ws_publish = Arc::clone(&manager);
    let manager_ws_subscriber_count = Arc::clone(&manager);
    let manager_ws_buffered = Arc::clone(&manager);
    let manager_ws_ready = Arc::clone(&manager);

    vec![
        // __http_serve(options) -> { id, port, hostname, tls, unix }
        op_async("__http_serve", move |args| {
            let mgr = Arc::clone(&manager_create);
            let options = parse_server_options(args);
            let has_permission = match options.as_ref() {
                Ok(opts) => {
                    let check_host = if opts.unix.is_some() {
                        "localhost".to_string()
                    } else {
                        let host = opts
                            .hostname
                            .clone()
                            .unwrap_or_else(|| "0.0.0.0".to_string());
                        if host == "0.0.0.0" || host == "localhost" || host == "127.0.0.1" {
                            "localhost".to_string()
                        } else {
                            host
                        }
                    };
                    capabilities_context::can_net(&check_host)
                }
                Err(_) => true,
            };

            async move {
                let options = options?;
                if !has_permission {
                    return Err(
                        "PermissionDenied: Network access denied. Use --allow-net to enable HTTP server.".to_string()
                    );
                }

                let result = mgr.create_server(options).await?;
                Ok(json!({
                    "id": result.id,
                    "port": result.port,
                    "hostname": result.hostname,
                    "tls": result.is_tls,
                    "unix": result.unix,
                }))
            }
        }),
        // __http_server_stop(id) -> { success }
        op_sync("__http_server_stop", move |args| {
            let id = args.first().and_then(|v| v.as_u64()).unwrap_or(0);
            let success = manager_stop.stop_server(id);
            Ok(json!({ "success": success }))
        }),
        // __http_server_info(id) -> { port, hostname, tls, unix }
        op_sync("__http_server_info", move |args| {
            let id = args.first().and_then(|v| v.as_u64()).unwrap_or(0);
            match manager_info.server_info(id) {
                Some(info) => Ok(json!({
                    "port": info.port,
                    "hostname": info.hostname,
                    "tls": info.is_tls,
                    "unix": info.unix,
                })),
                None => Ok(JsonValue::Null),
            }
        }),
        // __http_server_pending(id) -> { pendingRequests, pendingWebSockets }
        op_sync("__http_server_pending", move |args| {
            let id = args.first().and_then(|v| v.as_u64()).unwrap_or(0);
            let pending_requests = manager_pending.pending_requests(id).unwrap_or(0);
            let pending_websockets = manager_pending.pending_websockets(id).unwrap_or(0);
            Ok(json!({
                "pendingRequests": pending_requests,
                "pendingWebSockets": pending_websockets,
            }))
        }),
        // __http_req_basic(req_id) -> { method, url }
        op_sync("__http_req_basic", |args| {
            let req_id = args.first().and_then(|v| v.as_u64()).unwrap_or(0);
            match request::get_basic_metadata(req_id) {
                Some(meta) => Ok(serde_json::to_value(meta).unwrap()),
                None => Err(format!("Request {} not found", req_id)),
            }
        }),
        // __http_req_headers(req_id) -> headers object
        op_sync("__http_req_headers", |args| {
            let req_id = args.first().and_then(|v| v.as_u64()).unwrap_or(0);
            let Some(headers) = request::get_request_headers(req_id) else {
                return Err(format!("Request {} not found", req_id));
            };
            Ok(JsonValue::Object(
                headers
                    .into_iter()
                    .map(|(k, v)| (k, JsonValue::String(v)))
                    .collect(),
            ))
        }),
        // __http_req_body(req_id) -> body bytes array
        op_async("__http_req_body", |args| {
            let req_id = args.first().and_then(|v| v.as_u64()).unwrap_or(0);

            async move {
                match request::read_request_body(req_id).await {
                    Some(body) => Ok(JsonValue::Array(
                        body.into_iter()
                            .map(|b| JsonValue::Number(b.into()))
                            .collect(),
                    )),
                    None => Err(format!("Request {} not found", req_id)),
                }
            }
        }),
        // __http_respond(req_id, status, headers, body) -> { success }
        op_sync("__http_respond", |args| {
            let req_id = args.first().and_then(|v| v.as_u64()).unwrap_or(0);
            let status = args.get(1).and_then(|v| v.as_u64()).unwrap_or(200) as u16;
            let headers = args
                .get(2)
                .cloned()
                .unwrap_or(JsonValue::Object(Default::default()));
            let body = args
                .get(3)
                .and_then(|v| {
                    v.as_array()
                        .map(|arr| {
                            arr.iter()
                                .filter_map(|n| n.as_u64().map(|n| n as u8))
                                .collect::<Vec<u8>>()
                        })
                        .or_else(|| v.as_str().map(|s| s.as_bytes().to_vec()))
                })
                .unwrap_or_default();

            let response_headers: std::collections::HashMap<String, String> = headers
                .as_object()
                .map(|obj| {
                    obj.iter()
                        .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                        .collect()
                })
                .unwrap_or_default();

            let success = request::send_response(req_id, status, response_headers, body);
            Ok(json!({ "success": success }))
        }),
        // __http_respond_text(req_id, status, text) -> { success } (fast path)
        op_sync("__http_respond_text", |args| {
            let req_id = args.first().and_then(|v| v.as_u64()).unwrap_or(0);
            let status = args.get(1).and_then(|v| v.as_u64()).unwrap_or(200) as u16;
            let text = args.get(2).and_then(|v| v.as_str()).unwrap_or("");

            let success = request::send_text_response(req_id, status, text);
            Ok(json!({ "success": success }))
        }),
        // __http_req_peer(req_id) -> { address, port, family } | null
        op_sync("__http_req_peer", |args| {
            let req_id = args.first().and_then(|v| v.as_u64()).unwrap_or(0);
            match request::get_request_peer_addr(req_id) {
                Some(peer) => Ok(json!({
                    "address": peer.ip().to_string(),
                    "port": peer.port(),
                    "family": if peer.is_ipv6() { "IPv6" } else { "IPv4" },
                })),
                None => Ok(JsonValue::Null),
            }
        }),
        // __http_ws_upgrade(serverId, reqId, headers, data) -> boolean
        op_sync("__http_ws_upgrade", move |args| {
            let server_id = args.get(0).and_then(|v| v.as_u64()).unwrap_or(0);
            let req_id = args.get(1).and_then(|v| v.as_u64()).unwrap_or(0);
            let headers = args
                .get(2)
                .and_then(|v| v.as_object())
                .map(json_headers_to_map)
                .unwrap_or_default();
            let data = args.get(3).filter(|v| !v.is_null()).cloned();

            let success = manager_ws_upgrade.upgrade_websocket(server_id, req_id, headers, data)?;
            Ok(json!(success))
        }),
        // __http_ws_send(socketId, data, isText) -> status
        op_sync("__http_ws_send", move |args| {
            let socket_id = args.get(0).and_then(|v| v.as_u64()).unwrap_or(0);
            let data = args.get(1).and_then(json_to_bytes).unwrap_or_default();
            let is_text = args.get(2).and_then(|v| v.as_bool()).unwrap_or(false);
            let status = manager_ws_send.ws_send(socket_id, data, is_text);
            Ok(json!(status))
        }),
        // __http_ws_close(socketId, code, reason) -> boolean
        op_sync("__http_ws_close", move |args| {
            let socket_id = args.get(0).and_then(|v| v.as_u64()).unwrap_or(0);
            let code = args.get(1).and_then(|v| v.as_u64()).map(|v| v as u16);
            let reason = args.get(2).and_then(|v| v.as_str()).map(|s| s.to_string());
            let success = manager_ws_close.ws_close(socket_id, code, reason);
            Ok(json!(success))
        }),
        // __http_ws_terminate(socketId) -> boolean
        op_sync("__http_ws_terminate", move |args| {
            let socket_id = args.get(0).and_then(|v| v.as_u64()).unwrap_or(0);
            let success = manager_ws_terminate.ws_terminate(socket_id);
            Ok(json!(success))
        }),
        // __http_ws_ping(socketId, data) -> status
        op_sync("__http_ws_ping", move |args| {
            let socket_id = args.get(0).and_then(|v| v.as_u64()).unwrap_or(0);
            let data = args.get(1).and_then(json_to_bytes).unwrap_or_default();
            let status = manager_ws_ping.ws_ping(socket_id, data);
            Ok(json!(status))
        }),
        // __http_ws_pong(socketId, data) -> status
        op_sync("__http_ws_pong", move |args| {
            let socket_id = args.get(0).and_then(|v| v.as_u64()).unwrap_or(0);
            let data = args.get(1).and_then(json_to_bytes).unwrap_or_default();
            let status = manager_ws_pong.ws_pong(socket_id, data);
            Ok(json!(status))
        }),
        // __http_ws_subscribe(socketId, topic) -> boolean
        op_sync("__http_ws_subscribe", move |args| {
            let socket_id = args.get(0).and_then(|v| v.as_u64()).unwrap_or(0);
            let topic = args.get(1).and_then(|v| v.as_str()).unwrap_or("");
            let success = manager_ws_subscribe.ws_subscribe(socket_id, topic);
            Ok(json!(success))
        }),
        // __http_ws_unsubscribe(socketId, topic) -> boolean
        op_sync("__http_ws_unsubscribe", move |args| {
            let socket_id = args.get(0).and_then(|v| v.as_u64()).unwrap_or(0);
            let topic = args.get(1).and_then(|v| v.as_str()).unwrap_or("");
            let success = manager_ws_unsubscribe.ws_unsubscribe(socket_id, topic);
            Ok(json!(success))
        }),
        // __http_ws_publish(serverId, topic, data, isText, senderId) -> status
        op_sync("__http_ws_publish", move |args| {
            let server_id = args.get(0).and_then(|v| v.as_u64()).unwrap_or(0);
            let topic = args.get(1).and_then(|v| v.as_str()).unwrap_or("");
            let data = args.get(2).and_then(json_to_bytes).unwrap_or_default();
            let is_text = args.get(3).and_then(|v| v.as_bool()).unwrap_or(false);
            let sender_id = args.get(4).and_then(|v| v.as_u64());
            let status = manager_ws_publish.ws_publish(server_id, topic, data, is_text, sender_id);
            Ok(json!(status))
        }),
        // __http_ws_subscriber_count(serverId, topic) -> number
        op_sync("__http_ws_subscriber_count", move |args| {
            let server_id = args.get(0).and_then(|v| v.as_u64()).unwrap_or(0);
            let topic = args.get(1).and_then(|v| v.as_str()).unwrap_or("");
            let count = manager_ws_subscriber_count.ws_subscriber_count(server_id, topic);
            Ok(json!(count))
        }),
        // __http_ws_buffered_amount(socketId) -> number
        op_sync("__http_ws_buffered_amount", move |args| {
            let socket_id = args.get(0).and_then(|v| v.as_u64()).unwrap_or(0);
            let amount = manager_ws_buffered.ws_buffered_amount(socket_id);
            Ok(json!(amount))
        }),
        // __http_ws_ready_state(socketId) -> number
        op_sync("__http_ws_ready_state", move |args| {
            let socket_id = args.get(0).and_then(|v| v.as_u64()).unwrap_or(0);
            let state = manager_ws_ready.ws_ready_state(socket_id);
            Ok(json!(state))
        }),
    ]
}

fn parse_server_options(args: &[JsonValue]) -> Result<ServerOptions, String> {
    if let Some(JsonValue::Object(obj)) = args.first() {
        let mut port = parse_port(obj.get("port"))?;
        let hostname = obj
            .get("hostname")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        let unix = obj
            .get("unix")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        if port.is_none() && unix.is_none() {
            port = Some(3000);
        }
        let tls = parse_tls_config(obj.get("tls"))?;
        let http2 = obj.get("http2").and_then(|v| v.as_bool()).unwrap_or(false);
        let h2c = obj.get("h2c").and_then(|v| v.as_bool()).unwrap_or(false);
        let reuse_port = obj
            .get("reusePort")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let ipv6_only = obj
            .get("ipv6Only")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let idle_timeout = obj
            .get("idleTimeout")
            .and_then(|v| v.as_f64())
            .map(Duration::from_secs_f64);
        let (ws_config, ws_enabled) = parse_ws_config(obj.get("websocket"));

        return Ok(ServerOptions {
            port,
            hostname,
            unix,
            tls,
            http2,
            h2c,
            reuse_port,
            ipv6_only,
            idle_timeout,
            ws_config,
            ws_enabled,
        });
    }

    let port = args.first().and_then(|v| v.as_u64()).unwrap_or(3000) as u16;
    let hostname = args
        .get(1)
        .and_then(|v| v.as_str())
        .unwrap_or("0.0.0.0")
        .to_string();

    Ok(ServerOptions {
        port: Some(port),
        hostname: Some(hostname),
        unix: None,
        tls: None,
        http2: false,
        h2c: false,
        reuse_port: false,
        ipv6_only: false,
        idle_timeout: None,
        ws_config: WebSocketServerConfig::default(),
        ws_enabled: false,
    })
}

fn parse_port(value: Option<&JsonValue>) -> Result<Option<u16>, String> {
    let Some(value) = value else {
        return Ok(None);
    };

    if let Some(port) = value.as_u64() {
        if port > u16::MAX as u64 {
            return Err("Invalid port value".to_string());
        }
        return Ok(Some(port as u16));
    }

    if let Some(port_str) = value.as_str() {
        let port = port_str
            .parse::<u16>()
            .map_err(|e| format!("Invalid port: {}", e))?;
        return Ok(Some(port));
    }

    Err("Invalid port value".to_string())
}

fn parse_tls_config(value: Option<&JsonValue>) -> Result<Option<TlsConfig>, String> {
    let Some(value) = value else {
        return Ok(None);
    };

    match value {
        JsonValue::Array(list) => {
            if let Some(first) = list.first() {
                parse_tls_config(Some(first))
            } else {
                Ok(None)
            }
        }
        JsonValue::Object(obj) => {
            let cert = obj
                .get("cert")
                .ok_or_else(|| "TLS cert is required".to_string())?;
            let key = obj
                .get("key")
                .ok_or_else(|| "TLS key is required".to_string())?;
            let cert_pem = parse_pem_value(cert)?;
            let key_pem = parse_pem_value(key)?;
            Ok(Some(TlsConfig::from_pem(
                cert_pem.as_bytes(),
                key_pem.as_bytes(),
            )?))
        }
        _ => Err("Invalid tls value".to_string()),
    }
}

fn parse_pem_value(value: &JsonValue) -> Result<String, String> {
    match value {
        JsonValue::String(s) => Ok(s.clone()),
        JsonValue::Array(values) => {
            if values.iter().all(|v| v.is_string()) {
                let joined = values
                    .iter()
                    .filter_map(|v| v.as_str())
                    .collect::<Vec<_>>()
                    .join("\n");
                return Ok(joined);
            }

            if values.iter().all(|v| v.as_u64().is_some()) {
                let bytes: Vec<u8> = values
                    .iter()
                    .filter_map(|v| v.as_u64().map(|b| b as u8))
                    .collect();
                let text = String::from_utf8(bytes).map_err(|_| "Invalid PEM bytes".to_string())?;
                return Ok(text);
            }

            Err("Invalid PEM value".to_string())
        }
        _ => Err("Invalid PEM value".to_string()),
    }
}

fn parse_ws_config(value: Option<&JsonValue>) -> (WebSocketServerConfig, bool) {
    let mut config = WebSocketServerConfig::default();
    let mut enabled = false;

    if let Some(value) = value {
        match value {
            JsonValue::Bool(true) => {
                enabled = true;
            }
            JsonValue::Object(obj) => {
                enabled = true;
                if let Some(max) = obj.get("maxPayloadLength").and_then(|v| v.as_u64()) {
                    config.max_payload_length = max as usize;
                }
                if let Some(limit) = obj.get("backpressureLimit").and_then(|v| v.as_u64()) {
                    config.backpressure_limit = limit as usize;
                }
                if let Some(close) = obj
                    .get("closeOnBackpressureLimit")
                    .and_then(|v| v.as_bool())
                {
                    config.close_on_backpressure_limit = close;
                }
                if let Some(timeout) = obj.get("idleTimeout").and_then(|v| v.as_f64()) {
                    config.idle_timeout = Duration::from_secs_f64(timeout);
                }
                if let Some(publish_to_self) = obj.get("publishToSelf").and_then(|v| v.as_bool()) {
                    config.publish_to_self = publish_to_self;
                }
                if let Some(send_pings) = obj.get("sendPings").and_then(|v| v.as_bool()) {
                    config.send_pings = send_pings;
                }
            }
            _ => {}
        }
    }

    (config, enabled)
}

fn json_headers_to_map(obj: &serde_json::Map<String, JsonValue>) -> HashMap<String, String> {
    obj.iter()
        .filter_map(|(key, val)| val.as_str().map(|v| (key.clone(), v.to_string())))
        .collect()
}

fn json_to_bytes(value: &JsonValue) -> Option<Vec<u8>> {
    if let Some(arr) = value.as_array() {
        return Some(
            arr.iter()
                .filter_map(|n| n.as_u64().map(|n| n as u8))
                .collect(),
        );
    }

    if let Some(text) = value.as_str() {
        return Some(text.as_bytes().to_vec());
    }

    None
}

/// JavaScript shim code for HTTP server
pub const JS_SHIM: &str = include_str!("serve.js");
