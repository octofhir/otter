//! WebSocket extension module using the new architecture.
//!
//! This module provides the WebSocket extension with Web-standard WebSocket API.
//!
//! ## Architecture
//!
//! - `websocket.rs` - Rust WebSocket implementation
//! - `websocket_ext.rs` - Extension creation with ops
//! - `websocket.js` - JavaScript WebSocket class wrapper
//!
//! Note: This module uses shared state (WebSocketManager) which doesn't fit the #[dive]
//! pattern, so we use traditional op_sync with closures.

use otter_runtime::extension::{op_sync, OpDecl};
use otter_runtime::Extension;
use serde_json::json;
use std::sync::Arc;

use crate::websocket;

/// Create the websocket extension.
///
/// This extension provides Web-standard WebSocket API for real-time communication.
pub fn extension() -> Extension {
    // Shared WebSocket manager
    let manager = Arc::new(websocket::WebSocketManager::new());

    let mut ops: Vec<OpDecl> = Vec::new();

    // wsConnect(url) -> connection_id
    let mgr_connect = manager.clone();
    ops.push(op_sync("wsConnect", move |_ctx, args| {
        let url = args
            .first()
            .and_then(|v| v.as_str())
            .ok_or_else(|| otter_runtime::error::JscError::internal("wsConnect requires url"))?;

        let id = mgr_connect
            .connect(url)
            .map_err(|e| otter_runtime::error::JscError::internal(e.to_string()))?;

        Ok(json!(id))
    }));

    // wsSend(id, data) -> null
    let mgr_send = manager.clone();
    ops.push(op_sync("wsSend", move |_ctx, args| {
        let id = args
            .first()
            .and_then(|v| v.as_u64())
            .ok_or_else(|| otter_runtime::error::JscError::internal("wsSend requires id"))?
            as u32;

        let data = args
            .get(1)
            .ok_or_else(|| otter_runtime::error::JscError::internal("wsSend requires data"))?;

        if let Some(text) = data.as_str() {
            mgr_send
                .send(id, text)
                .map_err(|e| otter_runtime::error::JscError::internal(e.to_string()))?;
        } else if let Some(arr) = data.as_array() {
            let bytes: Vec<u8> = arr
                .iter()
                .filter_map(|v| v.as_u64().map(|n| n as u8))
                .collect();
            mgr_send
                .send_binary(id, bytes)
                .map_err(|e| otter_runtime::error::JscError::internal(e.to_string()))?;
        } else if let Some(arr) = data
            .as_object()
            .and_then(|obj| obj.get("data"))
            .and_then(|v| v.as_array())
        {
            let bytes: Vec<u8> = arr
                .iter()
                .filter_map(|v| v.as_u64().map(|n| n as u8))
                .collect();
            mgr_send
                .send_binary(id, bytes)
                .map_err(|e| otter_runtime::error::JscError::internal(e.to_string()))?;
        }

        Ok(json!(null))
    }));

    // wsClose(id, code?, reason?) -> null
    let mgr_close = manager.clone();
    ops.push(op_sync("wsClose", move |_ctx, args| {
        let id = args
            .first()
            .and_then(|v| v.as_u64())
            .ok_or_else(|| otter_runtime::error::JscError::internal("wsClose requires id"))?
            as u32;

        let code = args.get(1).and_then(|v| v.as_u64()).map(|n| n as u16);
        let reason = args.get(2).and_then(|v| v.as_str()).map(|s| s.to_string());

        mgr_close
            .close(id, code, reason)
            .map_err(|e| otter_runtime::error::JscError::internal(e.to_string()))?;

        Ok(json!(null))
    }));

    // wsReadyState(id) -> number
    let mgr_state = manager.clone();
    ops.push(op_sync("wsReadyState", move |_ctx, args| {
        let id = args
            .first()
            .and_then(|v| v.as_u64())
            .ok_or_else(|| {
                otter_runtime::error::JscError::internal("wsReadyState requires id")
            })? as u32;

        let state = mgr_state
            .ready_state(id)
            .unwrap_or(websocket::ReadyState::Closed);
        Ok(json!(state as u8))
    }));

    // wsUrl(id) -> string
    let mgr_url = manager.clone();
    ops.push(op_sync("wsUrl", move |_ctx, args| {
        let id = args
            .first()
            .and_then(|v| v.as_u64())
            .ok_or_else(|| otter_runtime::error::JscError::internal("wsUrl requires id"))?
            as u32;

        let url = mgr_url.url(id).unwrap_or_default();
        Ok(json!(url))
    }));

    // wsPollEvents() -> array of events
    let mgr_poll = manager.clone();
    ops.push(op_sync("wsPollEvents", move |_ctx, _args| {
        let events = mgr_poll.poll_events();
        let json_events: Vec<serde_json::Value> = events
            .into_iter()
            .map(|(id, event)| match event {
                websocket::WebSocketEvent::Open => json!({
                    "id": id,
                    "type": "open"
                }),
                websocket::WebSocketEvent::Message(msg) => match msg {
                    websocket::WebSocketMessage::Text(text) => json!({
                        "id": id,
                        "type": "message",
                        "data": text
                    }),
                    websocket::WebSocketMessage::Binary(data) => json!({
                        "id": id,
                        "type": "message",
                        "data": { "type": "Buffer", "data": data }
                    }),
                },
                websocket::WebSocketEvent::Close { code, reason } => json!({
                    "id": id,
                    "type": "close",
                    "code": code,
                    "reason": reason
                }),
                websocket::WebSocketEvent::Error(msg) => json!({
                    "id": id,
                    "type": "error",
                    "message": msg
                }),
            })
            .collect();

        Ok(json!(json_events))
    }));

    Extension::new("WebSocket")
        .with_ops(ops)
        .with_js(include_str!("websocket.js"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extension_creation() {
        let ext = extension();
        assert_eq!(ext.name(), "WebSocket");
        assert!(ext.js_code().is_some());
    }
}
