//! Process IPC extension module using the new architecture.
//!
//! This module provides the process IPC extension for inter-process communication.
//!
//! ## Architecture
//!
//! - `ipc.rs` - Rust IPC implementation
//! - `process_ipc_ext.rs` - Extension creation with ops
//! - `process_ipc.js` - JavaScript wrapper for process.send/recv

use otter_runtime::Extension;
use otter_runtime::extension::{OpDecl, op_async, op_sync};
use serde_json::json;

/// Create the process IPC extension.
///
/// This extension provides IPC functionality for communicating with parent process.
/// Only available on Unix platforms.
///
/// # Arguments
///
/// * `ipc_channel` - The IPC channel to use for communication
///
/// # Example
///
/// ```no_run
/// use otter_node::ipc::IpcChannel;
/// use otter_node::process_ipc_ext;
///
/// // In child process, after detecting OTTER_IPC_FD
/// let fd = std::env::var("OTTER_IPC_FD").unwrap().parse().unwrap();
/// let channel = unsafe { IpcChannel::from_raw_fd(fd).unwrap() };
/// let ext = process_ipc_ext::extension(channel);
/// ```
#[cfg(unix)]
pub fn extension(ipc_channel: crate::ipc::IpcChannel) -> Extension {
    use std::sync::Arc;
    use tokio::sync::Mutex;

    let channel = Arc::new(Mutex::new(ipc_channel));
    let channel_send = channel.clone();
    let channel_recv = channel.clone();
    let connected = Arc::new(std::sync::atomic::AtomicBool::new(true));
    let connected_check = connected.clone();
    let connected_disconnect = connected.clone();

    let mut ops: Vec<OpDecl> = Vec::new();

    // __otter_process_ipc_send(message) -> boolean
    ops.push(op_async("__otter_process_ipc_send", move |_ctx, args| {
        let channel = channel_send.clone();
        let msg = args.first().cloned().unwrap_or(serde_json::Value::Null);

        Box::pin(async move {
            let mut ch = channel.lock().await;
            match ch.send(&msg).await {
                Ok(_) => Ok(json!(true)),
                Err(_) => Ok(json!(false)),
            }
        })
    }));

    // __otter_process_ipc_recv() -> message | null
    ops.push(op_async("__otter_process_ipc_recv", move |_ctx, _args| {
        let channel = channel_recv.clone();

        Box::pin(async move {
            let mut ch = channel.lock().await;
            match ch.recv().await {
                Ok(Some(msg)) => Ok(msg),
                Ok(None) | Err(_) => Ok(json!(null)),
            }
        })
    }));

    // __otter_process_ipc_connected() -> boolean
    ops.push(op_sync(
        "__otter_process_ipc_connected",
        move |_ctx, _args| {
            Ok(json!(
                connected_check.load(std::sync::atomic::Ordering::Relaxed)
            ))
        },
    ));

    // __otter_process_ipc_disconnect() -> null
    ops.push(op_sync(
        "__otter_process_ipc_disconnect",
        move |_ctx, _args| {
            connected_disconnect.store(false, std::sync::atomic::Ordering::Relaxed);
            Ok(json!(null))
        },
    ));

    Extension::new("process_ipc")
        .with_ops(ops)
        .with_js(include_str!("process_ipc.js"))
}

/// Stub for non-Unix platforms
#[cfg(not(unix))]
pub fn extension(_ipc_channel: ()) -> Extension {
    Extension::new("process_ipc")
}

#[cfg(test)]
mod tests {
    #[test]
    fn test_extension_name() {
        // Can't easily test full extension creation without IPC channel
        // Just verify the module compiles
    }
}
