//! Dgram extension module.
//!
//! Provides node:dgram compatible UDP socket APIs.

use otter_runtime::Extension;
use otter_runtime::extension::op_async;
use serde_json::json;
use std::sync::OnceLock;
use tokio::sync::mpsc;

use crate::dgram::{DgramEvent, DgramManager, SocketType};

/// Global dgram manager instance.
static DGRAM_MANAGER: OnceLock<DgramManager> = OnceLock::new();

/// Global event receiver for dgram events.
static EVENT_TX: OnceLock<mpsc::UnboundedSender<DgramEvent>> = OnceLock::new();

/// Initialize and get the dgram manager.
fn get_manager() -> &'static DgramManager {
    DGRAM_MANAGER.get_or_init(|| {
        let (tx, _rx) = mpsc::unbounded_channel();
        // Store tx in EVENT_TX for later use
        let _ = EVENT_TX.set(tx.clone());
        DgramManager::new(tx)
    })
}

/// Create the dgram extension.
pub fn extension() -> Extension {
    Extension::new("dgram")
        .with_ops(vec![
            // Create a UDP socket
            op_async("__otter_dgram_create_socket", |_ctx, args| async move {
                let socket_type_str = args.first().and_then(|v| v.as_str()).unwrap_or("udp4");

                let socket_type = match socket_type_str {
                    "udp6" => SocketType::Udp6,
                    _ => SocketType::Udp4,
                };

                let manager = get_manager();
                let socket_id = manager.create_socket(socket_type);

                Ok(json!({
                    "socketId": socket_id,
                    "type": socket_type_str
                }))
            }),
            // Bind socket to address and port
            op_async("__otter_dgram_bind", |_ctx, args| async move {
                let socket_id = args.first().and_then(|v| v.as_u64()).ok_or_else(|| {
                    otter_runtime::error::JscError::internal("socket_id is required")
                })? as u32;

                let port = args.get(1).and_then(|v| v.as_u64()).unwrap_or(0) as u16;

                let address = args.get(2).and_then(|v| v.as_str()).unwrap_or("0.0.0.0");

                let manager = get_manager();
                manager
                    .bind(socket_id, port, address)
                    .await
                    .map_err(|e| otter_runtime::error::JscError::internal(e.to_string()))?;

                // Get the bound address
                let addr_info = manager
                    .address(socket_id)
                    .map_err(|e| otter_runtime::error::JscError::internal(e.to_string()))?;

                match addr_info {
                    Some((addr, port, family)) => Ok(json!({
                        "address": addr,
                        "port": port,
                        "family": family
                    })),
                    None => Ok(json!({
                        "address": address,
                        "port": port,
                        "family": "IPv4"
                    })),
                }
            }),
            // Send data to address
            op_async("__otter_dgram_send", |_ctx, args| async move {
                let socket_id = args.first().and_then(|v| v.as_u64()).ok_or_else(|| {
                    otter_runtime::error::JscError::internal("socket_id is required")
                })? as u32;

                // Data can be a base64 string or array of bytes
                let data = if let Some(s) = args.get(1).and_then(|v| v.as_str()) {
                    use base64::{Engine, engine::general_purpose::STANDARD};
                    STANDARD.decode(s).unwrap_or_else(|_| s.as_bytes().to_vec())
                } else if let Some(arr) = args.get(1).and_then(|v| v.as_array()) {
                    arr.iter()
                        .filter_map(|v| v.as_u64().map(|n| n as u8))
                        .collect()
                } else {
                    return Err(otter_runtime::error::JscError::internal("data is required"));
                };

                let port =
                    args.get(2).and_then(|v| v.as_u64()).ok_or_else(|| {
                        otter_runtime::error::JscError::internal("port is required")
                    })? as u16;

                let address = args.get(3).and_then(|v| v.as_str()).ok_or_else(|| {
                    otter_runtime::error::JscError::internal("address is required")
                })?;

                let manager = get_manager();
                let bytes_sent = manager
                    .send(socket_id, data, port, address)
                    .await
                    .map_err(|e| otter_runtime::error::JscError::internal(e.to_string()))?;

                Ok(json!({ "bytesSent": bytes_sent }))
            }),
            // Close socket
            op_async("__otter_dgram_close", |_ctx, args| async move {
                let socket_id = args.first().and_then(|v| v.as_u64()).ok_or_else(|| {
                    otter_runtime::error::JscError::internal("socket_id is required")
                })? as u32;

                let manager = get_manager();
                manager
                    .close(socket_id)
                    .map_err(|e| otter_runtime::error::JscError::internal(e.to_string()))?;

                Ok(json!({ "closed": true }))
            }),
            // Get socket address
            op_async("__otter_dgram_address", |_ctx, args| async move {
                let socket_id = args.first().and_then(|v| v.as_u64()).ok_or_else(|| {
                    otter_runtime::error::JscError::internal("socket_id is required")
                })? as u32;

                let manager = get_manager();
                let addr_info = manager
                    .address(socket_id)
                    .map_err(|e| otter_runtime::error::JscError::internal(e.to_string()))?;

                match addr_info {
                    Some((addr, port, family)) => Ok(json!({
                        "address": addr,
                        "port": port,
                        "family": family
                    })),
                    None => Err(otter_runtime::error::JscError::internal("Socket not bound")),
                }
            }),
        ])
        .with_js(include_str!("dgram.js"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extension_creation() {
        let ext = extension();
        assert_eq!(ext.name(), "dgram");
        assert!(ext.js_code().is_some());
    }
}
