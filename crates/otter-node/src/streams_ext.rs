//! Streams extension module using the new architecture.
//!
//! This module provides the Web Streams API (ReadableStream, WritableStream, TransformStream).
//!
//! ## Architecture
//!
//! - `stream.rs` - Rust stream implementation
//! - `streams_ext.rs` - Extension creation with ops
//! - `streams.js` - JavaScript Streams API wrapper
//!
//! Note: This module uses shared state (StreamManager) which doesn't fit the #[dive]
//! pattern, so we use traditional op_sync with closures.

use otter_runtime::Extension;
use otter_runtime::extension::{OpDecl, op_sync};
use serde_json::json;
use std::sync::Arc;

use crate::stream::{StreamChunk, StreamManager, StreamState};

/// Create the streams extension.
///
/// This extension provides Web Streams API for handling streaming data.
pub fn extension() -> Extension {
    // Shared stream manager
    let manager = Arc::new(StreamManager::new());

    let mut ops: Vec<OpDecl> = Vec::new();

    // createReadableStream(highWaterMark?) -> stream_id
    let mgr_create_readable = manager.clone();
    ops.push(op_sync("createReadableStream", move |_ctx, args| {
        let high_water_mark = args.first().and_then(|v| v.as_u64()).map(|n| n as usize);
        let id = mgr_create_readable.create_readable(high_water_mark);
        Ok(json!(id))
    }));

    // createWritableStream() -> stream_id
    let mgr_create_writable = manager.clone();
    ops.push(op_sync("createWritableStream", move |_ctx, _args| {
        let id = mgr_create_writable.create_writable();
        Ok(json!(id))
    }));

    // readableEnqueue(id, chunk) -> null
    let mgr_enqueue = manager.clone();
    ops.push(op_sync("readableEnqueue", move |_ctx, args| {
        let id = args.first().and_then(|v| v.as_u64()).ok_or_else(|| {
            otter_runtime::error::JscError::internal("readableEnqueue requires id")
        })? as u32;

        let chunk_value = args.get(1).cloned().unwrap_or(json!(null));
        let chunk = StreamChunk::from_json(chunk_value);

        mgr_enqueue
            .enqueue(id, chunk)
            .map_err(|e| otter_runtime::error::JscError::internal(e.to_string()))?;

        Ok(json!(null))
    }));

    // readableRead(id) -> { value, done }
    let mgr_read = manager.clone();
    ops.push(op_sync("readableRead", move |_ctx, args| {
        let id =
            args.first().and_then(|v| v.as_u64()).ok_or_else(|| {
                otter_runtime::error::JscError::internal("readableRead requires id")
            })? as u32;

        match mgr_read.read(id) {
            Ok(Some(chunk)) => Ok(json!({
                "value": chunk.to_json(),
                "done": false
            })),
            Ok(None) => {
                // Check if stream is closed
                let is_closed = mgr_read.readable_state(id) == Some(StreamState::Closed);
                Ok(json!({
                    "value": null,
                    "done": is_closed
                }))
            }
            Err(e) => Err(otter_runtime::error::JscError::internal(e.to_string())),
        }
    }));

    // readableClose(id) -> null
    let mgr_close_readable = manager.clone();
    ops.push(op_sync("readableClose", move |_ctx, args| {
        let id =
            args.first().and_then(|v| v.as_u64()).ok_or_else(|| {
                otter_runtime::error::JscError::internal("readableClose requires id")
            })? as u32;

        mgr_close_readable
            .close_readable(id)
            .map_err(|e| otter_runtime::error::JscError::internal(e.to_string()))?;

        Ok(json!(null))
    }));

    // readableError(id, message) -> null
    let mgr_error_readable = manager.clone();
    ops.push(op_sync("readableError", move |_ctx, args| {
        let id =
            args.first().and_then(|v| v.as_u64()).ok_or_else(|| {
                otter_runtime::error::JscError::internal("readableError requires id")
            })? as u32;

        let message = args
            .get(1)
            .and_then(|v| v.as_str())
            .unwrap_or("Unknown error")
            .to_string();

        mgr_error_readable
            .error_readable(id, message)
            .map_err(|e| otter_runtime::error::JscError::internal(e.to_string()))?;

        Ok(json!(null))
    }));

    // readableLock(id) -> null
    let mgr_lock_readable = manager.clone();
    ops.push(op_sync("readableLock", move |_ctx, args| {
        let id =
            args.first().and_then(|v| v.as_u64()).ok_or_else(|| {
                otter_runtime::error::JscError::internal("readableLock requires id")
            })? as u32;

        mgr_lock_readable
            .lock_readable(id)
            .map_err(|e| otter_runtime::error::JscError::internal(e.to_string()))?;

        Ok(json!(null))
    }));

    // readableUnlock(id) -> null
    let mgr_unlock_readable = manager.clone();
    ops.push(op_sync("readableUnlock", move |_ctx, args| {
        let id =
            args.first().and_then(|v| v.as_u64()).ok_or_else(|| {
                otter_runtime::error::JscError::internal("readableUnlock requires id")
            })? as u32;

        mgr_unlock_readable
            .unlock_readable(id)
            .map_err(|e| otter_runtime::error::JscError::internal(e.to_string()))?;

        Ok(json!(null))
    }));

    // readableIsLocked(id) -> boolean
    let mgr_is_locked_readable = manager.clone();
    ops.push(op_sync("readableIsLocked", move |_ctx, args| {
        let id = args.first().and_then(|v| v.as_u64()).ok_or_else(|| {
            otter_runtime::error::JscError::internal("readableIsLocked requires id")
        })? as u32;

        Ok(json!(mgr_is_locked_readable.is_readable_locked(id)))
    }));

    // writableWrite(id, chunk) -> null
    let mgr_write = manager.clone();
    ops.push(op_sync("writableWrite", move |_ctx, args| {
        let id =
            args.first().and_then(|v| v.as_u64()).ok_or_else(|| {
                otter_runtime::error::JscError::internal("writableWrite requires id")
            })? as u32;

        let chunk_value = args.get(1).cloned().unwrap_or(json!(null));
        let chunk = StreamChunk::from_json(chunk_value);

        mgr_write
            .write(id, chunk)
            .map_err(|e| otter_runtime::error::JscError::internal(e.to_string()))?;

        Ok(json!(null))
    }));

    // writableClose(id) -> null
    let mgr_close_writable = manager.clone();
    ops.push(op_sync("writableClose", move |_ctx, args| {
        let id =
            args.first().and_then(|v| v.as_u64()).ok_or_else(|| {
                otter_runtime::error::JscError::internal("writableClose requires id")
            })? as u32;

        mgr_close_writable
            .close_writable(id)
            .map_err(|e| otter_runtime::error::JscError::internal(e.to_string()))?;

        Ok(json!(null))
    }));

    // writableError(id, message) -> null
    let mgr_error_writable = manager.clone();
    ops.push(op_sync("writableError", move |_ctx, args| {
        let id =
            args.first().and_then(|v| v.as_u64()).ok_or_else(|| {
                otter_runtime::error::JscError::internal("writableError requires id")
            })? as u32;

        let message = args
            .get(1)
            .and_then(|v| v.as_str())
            .unwrap_or("Unknown error")
            .to_string();

        mgr_error_writable
            .error_writable(id, message)
            .map_err(|e| otter_runtime::error::JscError::internal(e.to_string()))?;

        Ok(json!(null))
    }));

    // writableLock(id) -> null
    let mgr_lock_writable = manager.clone();
    ops.push(op_sync("writableLock", move |_ctx, args| {
        let id =
            args.first().and_then(|v| v.as_u64()).ok_or_else(|| {
                otter_runtime::error::JscError::internal("writableLock requires id")
            })? as u32;

        mgr_lock_writable
            .lock_writable(id)
            .map_err(|e| otter_runtime::error::JscError::internal(e.to_string()))?;

        Ok(json!(null))
    }));

    // writableUnlock(id) -> null
    let mgr_unlock_writable = manager.clone();
    ops.push(op_sync("writableUnlock", move |_ctx, args| {
        let id =
            args.first().and_then(|v| v.as_u64()).ok_or_else(|| {
                otter_runtime::error::JscError::internal("writableUnlock requires id")
            })? as u32;

        mgr_unlock_writable
            .unlock_writable(id)
            .map_err(|e| otter_runtime::error::JscError::internal(e.to_string()))?;

        Ok(json!(null))
    }));

    // writableIsLocked(id) -> boolean
    let mgr_is_locked_writable = manager.clone();
    ops.push(op_sync("writableIsLocked", move |_ctx, args| {
        let id = args.first().and_then(|v| v.as_u64()).ok_or_else(|| {
            otter_runtime::error::JscError::internal("writableIsLocked requires id")
        })? as u32;

        Ok(json!(mgr_is_locked_writable.is_writable_locked(id)))
    }));

    Extension::new("Streams")
        .with_ops(ops)
        .with_js(include_str!("streams.js"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extension_creation() {
        let ext = extension();
        assert_eq!(ext.name(), "Streams");
        assert!(ext.js_code().is_some());
    }
}
